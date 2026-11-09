//! The high-level DICOM client (CLI-01; docs/01-architecture.md §11).
//!
//! The client owns a request queue, decides how many requests share one
//! association, and transparently rolls over to a new association when the
//! quota is spent or the engine's circuit breaker trips. Callers see one call
//! per request; every association detail stays here.
//!
//! ```text
//! IDLE → CONNECT → REQUEST-ASSOC ─(timeout: retry ≤N)→ ASSOCIATED
//!      → SEND → (quota spent: cooldown → RELEASE) → IDLE → done
//! ```

use crate::cfind::{CFindRequest, CFindResponse};
use crate::cget::{CGetRequest, CGetResponse};
use crate::cmove::{CMoveRequest, CMoveResponse};
use crate::engine::ConnectionState;
use crate::error::{NetworkError, Result};
use crate::pdu::PresentationContext;
use crate::server::{Association, ClientOptions};
use crate::services::{CEchoResponse, CStoreRequest, CStoreResponse};
use radicom::io::file::DicomFile;
use std::sync::Arc;
use std::time::Duration;

/// Client-side tuning (§11.4).
#[derive(Debug, Clone)]
pub struct DicomClientOptions {
    /// Requests sent on one association before it is released; `None` keeps
    /// the association open until the caller releases it (no per-association
    /// quota).
    pub max_requests_per_association: Option<usize>,
    /// How long a spent association waits before it is released. This is a
    /// cooldown after the request quota is reached, not fo-dicom's
    /// listen-style linger: the synchronous API queues nothing while it sleeps
    pub quota_cooldown: Duration,
    /// Association attempts per batch before giving up (consecutive cap).
    pub max_consecutive_timeout_retries: u32,
    /// The transport-level options (max PDU, TLS, user identity, extended
    /// negotiation).
    pub connection: ClientOptions,
}

impl Default for DicomClientOptions {
    fn default() -> Self {
        DicomClientOptions {
            max_requests_per_association: None,
            quota_cooldown: Duration::from_millis(50),
            max_consecutive_timeout_retries: 3,
            connection: ClientOptions::default(),
        }
    }
}

/// How the client tears down an open association when the caller cancels
/// outstanding work (fo-dicom `DicomClientCancellationMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DicomClientCancellationMode {
    /// Gracefully release the association (the fo-dicom default).
    #[default]
    ImmediatelyReleaseAssociation,
    /// Abort the association for the fastest shutdown.
    ImmediatelyAbortAssociation,
}

/// Events the client reports while it drives associations (§11.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// The association lifecycle changed state.
    StateChanged {
        state: ConnectionState,
    },
    AssociationAccepted {
        attempt: u32,
    },
    AssociationRejected {
        reason: u8,
    },
    AssociationReleased,
    /// An association attempt timed out before the peer answered.
    AssociationRequestTimedOut {
        attempts: u32,
    },
    /// A request exceeded its `request_timeout`; the batch moves to a new
    /// association.
    RequestTimedOut {
        message_id: u16,
    },
}

/// A cloneable signal for cancelling a currently active FIND, GET or MOVE.
/// Obtain it before calling `send_*`; it can be used from another task.
#[derive(Clone, Debug)]
pub struct CancellationHandle {
    signal: tokio::sync::watch::Sender<Option<u16>>,
    active: Arc<std::sync::Mutex<std::collections::HashMap<u16, bool>>>,
}

impl Default for CancellationHandle {
    fn default() -> Self {
        Self {
            signal: tokio::sync::watch::channel(None).0,
            active: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

impl CancellationHandle {
    /// Signals the matching live request. Inactive IDs return false.
    pub fn cancel(&self, message_id: u16) -> bool {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(requested) = active.get_mut(&message_id) else {
            return false;
        };
        *requested = true;
        self.signal.send_replace(Some(message_id));
        true
    }

    pub(crate) fn register(&self, message_id: u16) -> ActiveRequest {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.is_empty() {
            self.signal.send_replace(None);
        }
        active.insert(message_id, false);
        ActiveRequest(self.clone(), message_id)
    }

    pub(crate) fn take_cancelled(&self) -> Vec<u16> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter_mut()
            .filter_map(|(&id, requested)| std::mem::take(requested).then_some(id))
            .collect()
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<u16>> {
        let mut receiver = self.signal.subscribe();
        if receiver.borrow().is_some() {
            receiver.mark_changed();
        }
        receiver
    }
}

pub(crate) struct ActiveRequest(CancellationHandle, u16);
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.1);
    }
}

/// The high-level client (§11.4).
///
/// One `DicomClient` describes a destination; requests are sent with the
/// `send_*` methods, which open associations as needed.
pub struct DicomClient {
    addr: String,
    pub calling_ae: String,
    pub called_ae: String,
    pub options: DicomClientOptions,
    /// Reverse C-STORE handler, required for C-GET (§11.4).
    reverse_store: Option<crate::server::ReverseStoreHandler>,
    /// Storage SOP classes the reverse C-STORE handler can receive; proposed
    /// automatically by [`Self::send_c_get_registered`].
    reverse_store_classes: Vec<String>,
    /// Answers an SCP-initiated N-EVENT-REPORT-RQ on this client (H2).
    n_event: Option<crate::server::NEventReportHandler>,
    events: Option<Arc<dyn Fn(ClientEvent) + Send + Sync>>,
    /// Requests already sent on the current association.
    sent_on_association: usize,
    /// The message ID and presentation-context ID of the last request, so a
    /// C-CANCEL travels on the context that request used.
    last_request: Option<(u16, u8)>,
    association: Option<Association>,
    cancellation: CancellationHandle,
}

impl DicomClient {
    /// A client for `addr` (host:port form understood by tokio).
    pub fn new(
        calling_ae: impl Into<String>,
        called_ae: impl Into<String>,
        addr: impl Into<String>,
    ) -> Self {
        DicomClient {
            addr: addr.into(),
            calling_ae: calling_ae.into(),
            called_ae: called_ae.into(),
            options: DicomClientOptions::default(),
            reverse_store: None,
            reverse_store_classes: Vec::new(),
            n_event: None,
            events: None,
            sent_on_association: 0,
            last_request: None,
            association: None,
            cancellation: CancellationHandle::default(),
        }
    }

    /// Independent cancellation control for a live FIND, GET or MOVE request.
    pub fn cancellation_handle(&self) -> CancellationHandle {
        self.cancellation.clone()
    }

    /// Overrides the tuning options.
    pub fn with_options(mut self, options: DicomClientOptions) -> Self {
        self.options = options;
        self
    }

    /// Registers the reverse C-STORE handler; without one C-GET fails fast.
    ///
    /// Pair it with [`Self::with_reverse_store_classes`] so
    /// [`Self::send_c_get_registered`] can propose the storage presentation
    /// contexts automatically.
    pub fn with_reverse_store_handler(
        mut self,
        handler: crate::server::ReverseStoreHandler,
    ) -> Self {
        self.reverse_store = Some(handler);
        self
    }

    /// Declares the storage SOP classes the reverse C-STORE handler can receive.
    ///
    /// This is a client capability, not a per-request argument: register it once
    /// and [`Self::send_c_get_registered`] proposes a presentation context (with
    /// the SCP role) for each (§11.4). The explicit
    /// [`Self::send_c_get`] still takes the classes directly.
    pub fn with_reverse_store_classes(mut self, classes: impl IntoIterator<Item = String>) -> Self {
        self.reverse_store_classes = classes.into_iter().collect();
        self
    }

    /// Registers the handler for SCP-initiated N-EVENT-REPORT-RQ (H2).
    ///
    /// Without one an incoming N-EVENT-REPORT stays unhandled; with one the
    /// peer's reports are answered on the association the client opened
    /// (fo-dicom `DicomClient.OnNEventReportRequest`). The associate accept must
    /// grant this side the SCP role for the service class
    /// (`ClientOptions::role_selections`); a client that also sends requests on
    /// that class proposes both roles, because a role selection replaces the
    /// default one for that class (PS3.7 §10.1).
    pub fn with_n_event_report_handler(
        mut self,
        handler: crate::server::NEventReportHandler,
    ) -> Self {
        self.n_event = Some(handler);
        self
    }

    /// Collects reverse C-STORE instances into `sink`.
    pub fn collect_reverse_stores(mut self, sink: Arc<std::sync::Mutex<Vec<DicomFile>>>) -> Self {
        self.reverse_store = Some(Arc::new(move |request: &CStoreRequest| {
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request.file.clone());
            Ok(crate::services::CStoreResponse::success(request))
        }));
        self
    }

    /// Receives every client event.
    pub fn with_event_handler(mut self, handler: Arc<dyn Fn(ClientEvent) + Send + Sync>) -> Self {
        self.events = Some(handler);
        self
    }

    fn emit(&self, event: ClientEvent) {
        if let Some(handler) = &self.events {
            handler(event);
        }
    }

    async fn finish_request<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            match error {
                NetworkError::RequestTimedOut { message_id } => {
                    self.emit(ClientEvent::RequestTimedOut {
                        message_id: *message_id,
                    });
                    self.abort().await;
                }
                // A peer close/abort, IO or protocol error leaves the
                // association unusable. Keep it and every later request fails
                // on the same dead socket; drop it so the next request opens a
                // fresh one (NET-19). Errors that describe the *operation*
                // rather than the connection keep the association.
                NetworkError::Reject { .. }
                | NetworkError::SopClassNotSupported { .. }
                | NetworkError::ScuRoleNotNegotiated { .. } => {}
                _ => self.forget_association(),
            }
        }
        result
    }

    /// Drops the cached association without any network IO, for failures where
    /// the stream itself is gone or unusable.
    fn forget_association(&mut self) {
        if self.association.take().is_some() {
            self.emit(ClientEvent::StateChanged {
                state: ConnectionState::Closed,
            });
        }
        self.sent_on_association = 0;
        self.last_request = None;
    }

    /// True when the current association has reached its request quota (§11.4).
    fn quota_spent(&self) -> bool {
        matches!(self.options.max_requests_per_association, Some(max) if self.sent_on_association >= max)
    }

    /// Waits the quota cooldown, then releases the association if the quota is
    /// spent.
    ///
    /// The synchronous API queues nothing while it sleeps, so this is a fixed
    /// delay before the release, not a wait for new work.
    async fn retire_if_spent(&mut self) {
        if self.quota_spent() {
            self.cooldown().await;
            self.close_association().await;
        }
    }

    /// Makes sure an association is open that carries every context this
    /// request needs, releasing and retrying as needed.
    ///
    /// An association is negotiated once, so a request whose SOP class was
    /// never proposed would fail for as long as that association lives: the
    /// association is released and reopened with the contexts at hand.
    /// Retries only when the peer does not answer at all; a rejection is
    /// permanent and is reported immediately.
    async fn ensure_association(
        &mut self,
        contexts: Vec<PresentationContext>,
    ) -> Result<&mut Association> {
        self.ensure_association_with_roles(contexts, Vec::new())
            .await
    }

    /// [`Self::ensure_association`] plus the role selections to propose.
    async fn ensure_association_with_roles(
        &mut self,
        contexts: Vec<PresentationContext>,
        roles: Vec<crate::pdu::RoleSelection>,
    ) -> Result<&mut Association> {
        self.retire_if_spent().await;
        if self
            .association
            .as_ref()
            .is_some_and(|open| !covers(open, &contexts) || !covers_roles(open, &roles))
        {
            self.close_association().await;
        }
        if self.association.is_none() {
            self.open_association(contexts, roles).await?;
        }
        Ok(self.association.as_mut().expect("open_association sets it"))
    }

    /// Opens an association, retrying peers that do not answer at all.
    async fn open_association(
        &mut self,
        contexts: Vec<PresentationContext>,
        roles: Vec<crate::pdu::RoleSelection>,
    ) -> Result<()> {
        let mut options = self.options.connection.clone();
        // the caller's configured roles plus the ones this request needs
        for role in roles {
            if !options.role_selections.contains(&role) {
                options.role_selections.push(role);
            }
        }
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let result = Association::open(
                self.addr.as_str(),
                &self.calling_ae,
                &self.called_ae,
                contexts.clone(),
                options.clone(),
            )
            .await;
            match result {
                Ok(mut association) => {
                    if let Some(handler) = self.reverse_store.clone() {
                        association.set_reverse_store_handler(handler);
                    }
                    if let Some(handler) = self.n_event.clone() {
                        association.set_n_event_report_handler(handler);
                    }
                    self.emit(ClientEvent::StateChanged {
                        state: ConnectionState::Open,
                    });
                    self.emit(ClientEvent::AssociationAccepted { attempt });
                    self.sent_on_association = 0;
                    self.association = Some(association);
                    return Ok(());
                }
                // a rejection is permanent: retrying it just burns time (§11.4)
                Err(other @ NetworkError::Reject { .. }) => {
                    if let NetworkError::Reject { reason, .. } = &other {
                        self.emit(ClientEvent::AssociationRejected { reason: *reason });
                    }
                    return Err(other);
                }
                Err(other) => {
                    if matches!(other, NetworkError::AssociationRequestTimedOut { .. })
                        || matches!(&other, NetworkError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
                    {
                        self.emit(ClientEvent::AssociationRequestTimedOut { attempts: attempt });
                    }
                    // only a peer that never answered is worth another attempt:
                    // a refusal, a TLS failure or a protocol error will not
                    // improve, and 0 retries means exactly that
                    if !is_retryable(&other)
                        || attempt >= self.options.max_consecutive_timeout_retries
                    {
                        return Err(other);
                    }
                }
            }
        }
    }

    /// Releases the current association, tolerating a peer that already left.
    async fn close_association(&mut self) {
        if let Some(association) = self.association.take() {
            self.emit(ClientEvent::StateChanged {
                state: ConnectionState::Closing,
            });
            let _ = association.release().await;
            self.emit(ClientEvent::StateChanged {
                state: ConnectionState::Closed,
            });
            self.emit(ClientEvent::AssociationReleased);
        }
        self.sent_on_association = 0;
        self.last_request = None;
    }

    /// Sleeps the quota cooldown (a post-quota delay, not a queue wait).
    async fn cooldown(&self) {
        tokio::time::sleep(self.options.quota_cooldown).await;
    }

    /// Sends concrete DIMSE request objects as one heterogeneous batch on one
    /// association, following the fo-dicom request collection model.
    ///
    /// Presentation contexts and reverse C-STORE role selections are derived
    /// from the complete request list before the association is opened. Each
    /// response is forwarded as soon as the association scheduler receives it.
    pub async fn send_requests<I, R, F>(&mut self, requests: I, on_response: F) -> Result<()>
    where
        I: IntoIterator<Item = R>,
        R: Into<crate::batch::DicomRequest>,
        F: FnMut(crate::batch::DicomResponseEvent) -> std::io::Result<()>,
    {
        let requests = requests
            .into_iter()
            .map(Into::into)
            .map(|request| match request {
                crate::batch::DicomRequest::Get {
                    request,
                    storage_sop_classes,
                } if storage_sop_classes.is_empty() => crate::batch::DicomRequest::Get {
                    request,
                    storage_sop_classes: self.reverse_store_classes.clone(),
                },
                request => request,
            })
            .collect::<Vec<_>>();
        if requests.is_empty() {
            return Ok(());
        }
        if requests
            .iter()
            .any(|request| matches!(request, crate::batch::DicomRequest::Get { .. }))
            && self.reverse_store.is_none()
        {
            return Err(NetworkError::ReverseStoreHandlerMissing);
        }
        if requests.iter().any(|request| {
            matches!(
                request,
                crate::batch::DicomRequest::Get {
                    storage_sop_classes,
                    ..
                } if storage_sop_classes.is_empty()
            )
        }) {
            return Err(NetworkError::protocol(
                "C-GET needs storage SOP classes: call with_reverse_store_classes first",
            ));
        }
        let (contexts, roles) = mixed_contexts(&requests)?;
        let request_count = requests.len();
        let cancellation = self.cancellation.clone();
        let result = {
            let association = self.ensure_association_with_roles(contexts, roles).await?;
            association
                .send_mixed_batch_cancellable(requests, on_response, Some(cancellation))
                .await
        };
        let result = self.finish_request(result).await;
        if result.is_ok() {
            self.sent_on_association += request_count;
        }
        result
    }

    /// Sends a queued set of heterogeneous DIMSE requests on one association.
    ///
    /// This builder-based entry point is kept for callers that prefer an
    /// explicit queue; [`Self::send_requests`] is the primary collection API.
    pub async fn send_queue<F>(
        &mut self,
        queue: crate::batch::DicomRequestQueue,
        on_response: F,
    ) -> Result<()>
    where
        F: FnMut(crate::batch::DicomResponseEvent) -> std::io::Result<()>,
    {
        self.send_requests(queue.into_requests(), on_response).await
    }

    /// Sends heterogeneous DIMSE requests on one association.
    ///
    /// This compatibility entry point delegates to [`Self::send_requests`].
    pub async fn send_batch<F>(
        &mut self,
        requests: Vec<crate::batch::DicomRequest>,
        on_response: F,
    ) -> Result<()>
    where
        F: FnMut(crate::batch::DicomResponseEvent) -> std::io::Result<()>,
    {
        self.send_requests(requests, on_response).await
    }

    /// Sends a C-ECHO (§11.4).
    pub async fn send_c_echo(&mut self, sop_class: impl Into<String>) -> Result<CEchoResponse> {
        let sop_class = sop_class.into();
        let context = PresentationContext::new(
            1,
            sop_class.clone(),
            vec![
                crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
                crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
            ],
        );
        let (context_id, response) = {
            let association = self.ensure_association(vec![context]).await?;
            let context_id = association
                .contexts()
                .pick(&sop_class, "")
                .map(|c| c.id)
                .ok();
            let response = association.send_c_echo(&sop_class).await;
            (context_id, response)
        };
        let response = self.finish_request(response).await;
        if let (Ok(response), Some(context_id)) = (&response, context_id) {
            self.last_request = Some((response.message_id_being_responded_to, context_id));
            self.sent_on_association += 1;
        }
        response
    }

    /// Sends a C-STORE (§11.4).
    pub async fn send_c_store(&mut self, file: &DicomFile) -> Result<CStoreResponse> {
        let sop_class = file
            .dataset
            .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let syntax = file.dataset.transfer_syntax().uid().to_string();
        let context = PresentationContext::new(
            1,
            sop_class.clone(),
            crate::services::c_store_transfer_syntaxes(&syntax, &[], false),
        );
        let (context_id, response) = {
            let association = self.ensure_association(vec![context]).await?;
            let context_id = association
                .contexts()
                .pick(&sop_class, &syntax)
                .map(|c| c.id)
                .ok();
            let response = association.send_c_store(file).await;
            (context_id, response)
        };
        let response = self.finish_request(response).await;
        if let (Ok(response), Some(context_id)) = (&response, context_id) {
            self.last_request = Some((response.message_id_being_responded_to, context_id));
            self.sent_on_association += 1;
        }
        response
    }

    /// Sends several C-STOREs without waiting for each response (M6).
    ///
    /// The association is negotiated once for every distinct SOP class in
    /// `files`; up to the peer's async-operations window is in flight at a time
    /// (see [`Association::send_c_store_batch`]). Results keep the request order.
    pub async fn send_c_store_batch(
        &mut self,
        files: &[&DicomFile],
    ) -> Result<Vec<CStoreResponse>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        // One context per distinct (SOP class, syntax); the builder allocates
        // unique odd IDs, so a batch of two classes does not propose two
        // contexts with the same ID (PS3.8 §9.3.2).
        let mut context_set = crate::association::ContextSetBuilder::new();
        for file in files {
            let sop_class = file
                .dataset
                .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
                .map(|s| s.trim().to_string())
                .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
            let syntax = file.dataset.transfer_syntax().uid().to_string();
            context_set.add(
                sop_class,
                crate::services::c_store_transfer_syntaxes(&syntax, &[], false),
            )?;
        }
        let contexts = context_set.to_wire();
        let result = {
            let association = self.ensure_association(contexts).await?;
            association.send_c_store_batch(files).await
        };
        let result = self.finish_request(result).await;
        if let Ok(responses) = &result {
            self.sent_on_association += responses.len();
        }
        result
    }

    /// Runs several C-FINDs within the async-operations window (M6).
    ///
    /// Returns each request's responses in request order; see
    /// [`Association::send_c_find_batch`] for the demultiplexing rules.
    pub async fn send_c_find_batch(
        &mut self,
        requests: &[CFindRequest],
    ) -> Result<Vec<Vec<CFindResponse>>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        // The builder hands out unique odd IDs; hard-coding id 1 for every
        // class would repeat a presentation-context ID (PS3.8 §9.3.2).
        let mut context_set = crate::association::ContextSetBuilder::new();
        for request in requests {
            context_set.add(
                request.sop_class_uid.clone(),
                vec![
                    crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
                    crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
                ],
            )?;
        }
        let contexts = context_set.to_wire();
        let result = {
            let association = self.ensure_association(contexts).await?;
            association.send_c_find_batch(requests).await
        };
        let result = self.finish_request(result).await;
        // Count every query group like the C-STORE batch does, so the
        // per-association quota sees these requests (NW-E-04).
        if let Ok(responses) = &result {
            self.sent_on_association += responses.len();
        }
        result
    }
    /// Runs a C-FIND and collects every response (§11.4).
    pub async fn send_c_find(&mut self, request: &CFindRequest) -> Result<Vec<CFindResponse>> {
        let context = PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec![
                crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
                crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
            ],
        );
        let cancellation = self.cancellation.clone();
        let (context_id, message_id, result) = {
            let association = self.ensure_association(vec![context]).await?;
            let mut request = request.clone();
            if request.message_id == 0 {
                request.message_id = association.next_message_id()?;
            }
            let message_id = request.message_id;
            let _active = cancellation.register(message_id);
            let context_id = association
                .contexts()
                .pick(&request.sop_class_uid, "")
                .map(|c| c.id)
                .ok();
            if let Some(context_id) = context_id {
                association.set_request_cancellation(context_id, cancellation.subscribe());
            }
            let result = association.send_c_find(&request).await;
            (context_id, message_id, result)
        };
        let responses = self.finish_request(result).await?;
        self.sent_on_association += 1;
        if let Some(context_id) = context_id {
            self.last_request = Some((message_id, context_id));
        }
        Ok(responses)
    }

    /// Runs a C-MOVE and collects every response (§11.4).
    pub async fn send_c_move(&mut self, request: &CMoveRequest) -> Result<Vec<CMoveResponse>> {
        let context = PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec![
                crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
                crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
            ],
        );
        let cancellation = self.cancellation.clone();
        let (context_id, message_id, result) = {
            let association = self.ensure_association(vec![context]).await?;
            let mut request = request.clone();
            if request.message_id == 0 {
                request.message_id = association.next_message_id()?;
            }
            let message_id = request.message_id;
            let _active = cancellation.register(message_id);
            let context_id = association
                .contexts()
                .pick(&request.sop_class_uid, "")
                .map(|c| c.id)
                .ok();
            if let Some(context_id) = context_id {
                association.set_request_cancellation(context_id, cancellation.subscribe());
            }
            let result = association.send_c_move(&request).await;
            (context_id, message_id, result)
        };
        let responses = self.finish_request(result).await?;
        self.sent_on_association += 1;
        if let Some(context_id) = context_id {
            self.last_request = Some((message_id, context_id));
        }
        Ok(responses)
    }

    /// Runs a C-GET and collects every response (§11.4).
    ///
    /// Instances arrive as reverse C-STORE sub-operations, so a reverse-store
    /// handler must have been registered and the storage classes the handler
    /// can receive must be proposed up front — otherwise the SCP has no
    /// presentation context for them and every sub-operation fails.
    pub async fn send_c_get(
        &mut self,
        request: &CGetRequest,
        storage_sop_classes: &[String],
    ) -> Result<Vec<CGetResponse>> {
        if self.reverse_store.is_none() {
            return Err(NetworkError::ReverseStoreHandlerMissing);
        }
        // The retrieve context plus one per storage class: the builder hands
        // out odd IDs through one allocator, so a peer asking for more than the
        // 128 available contexts gets an error instead of a wrapped ID that
        // collides with an existing context (NET-22).
        let syntaxes = vec![
            crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
            crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
        ];
        let mut context_set = crate::association::ContextSetBuilder::new();
        context_set.add(request.sop_class_uid.clone(), syntaxes.clone())?;
        // The instances come back as reverse C-STORE requests, so this side
        // must be granted the SCP role for every storage class it will
        // receive: a peer that honours role selection refuses to send them
        // otherwise (PS3.7 §7.4, review #16).
        let mut roles: Vec<crate::pdu::RoleSelection> = storage_sop_classes
            .iter()
            .map(|sop_class| crate::pdu::RoleSelection {
                sop_class: sop_class.clone(),
                scu_role: false,
                scp_role: true,
            })
            .collect();
        roles.sort_by(|a, b| a.sop_class.cmp(&b.sop_class));
        roles.dedup_by(|a, b| a.sop_class == b.sop_class);

        for sop_class in storage_sop_classes {
            context_set.add(sop_class.clone(), syntaxes.clone())?;
        }
        let contexts = context_set.to_wire();
        let cancellation = self.cancellation.clone();
        let (context_id, message_id, result) = {
            let association = self.ensure_association_with_roles(contexts, roles).await?;
            let mut request = request.clone();
            if request.message_id == 0 {
                request.message_id = association.next_message_id()?;
            }
            let message_id = request.message_id;
            let _active = cancellation.register(message_id);
            let context_id = association
                .contexts()
                .pick(&request.sop_class_uid, "")
                .map(|c| c.id)
                .ok();
            if let Some(context_id) = context_id {
                association.set_request_cancellation(context_id, cancellation.subscribe());
            }
            let result = association.send_c_get(&request).await;
            (context_id, message_id, result)
        };
        let responses = self.finish_request(result).await?;
        self.sent_on_association += 1;
        if let Some(context_id) = context_id {
            self.last_request = Some((message_id, context_id));
        }
        Ok(responses)
    }

    /// Runs a C-GET using the storage SOP classes declared with
    /// [`Self::with_reverse_store_classes`], so the caller does not repeat them
    /// on every request. Errors when none were declared.
    pub async fn send_c_get_registered(
        &mut self,
        request: &CGetRequest,
    ) -> Result<Vec<CGetResponse>> {
        let classes = self.reverse_store_classes.clone();
        if classes.is_empty() {
            return Err(NetworkError::protocol(
                "C-GET needs storage SOP classes: call with_reverse_store_classes first",
            ));
        }
        self.send_c_get(request, &classes).await
    }

    /// Sends one N-service request and returns its response (H2, §11.4).
    ///
    /// One call site covers all six services: the presentation context comes
    /// from the request's SOP class, and the dataset (if any) travels in its own
    /// transfer syntax. A `message_id` of 0 is allocated by the association, so
    /// a caller does not have to track IDs; the response carries the one used in
    /// `message_id_being_responded_to`.
    pub async fn send_n_service(
        &mut self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        if !crate::nservice::is_n_request(request.field) {
            return Err(NetworkError::protocol(
                "N-service request has a non-N command field",
            ));
        }
        let sop_class = request
            .sop_class_uid
            .clone()
            .ok_or_else(|| NetworkError::protocol("an N-service request has no SOP class UID"))?;
        let proposed = match &request.dataset {
            Some(dataset) => dataset.transfer_syntax().uid().to_string(),
            None => crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
        };
        let context = PresentationContext::new(1, sop_class.clone(), vec![proposed]);
        let (context_id, response) = {
            let association = self.ensure_association(vec![context]).await?;
            let context_id = association
                .contexts()
                .pick(&sop_class, "")
                .map(|c| c.id)
                .ok();
            let mut request = request.clone();
            if request.message_id == 0 {
                request.message_id = association.next_message_id()?;
            }
            let response = association.send_n_service(&request).await;
            (context_id, response)
        };
        let response = self.finish_request(response).await;
        if let (Ok(response), Some(context_id)) = (&response, context_id) {
            self.sent_on_association += 1;
            self.last_request = Some((response.message_id_being_responded_to, context_id));
        }
        response
    }

    /// Sends an N-ACTION and returns the response (H2, §11.4).
    ///
    /// The common case of [`Self::send_n_service`]: a service-class SCP expects
    /// an action type ID and usually a dataset.
    pub async fn send_n_action(
        &mut self,
        sop_class_uid: impl Into<String>,
        sop_instance_uid: impl Into<String>,
        action_type_id: u16,
        dataset: Option<radicom::model::dataset::Dataset>,
    ) -> Result<crate::nservice::NServiceResponse> {
        let mut request =
            crate::nservice::NServiceRequest::new(crate::dimse::CommandField::NActionRq, 0);
        request.sop_class_uid = Some(sop_class_uid.into());
        request.sop_instance_uid = Some(sop_instance_uid.into());
        request.action_type_id = Some(action_type_id);
        request.dataset = dataset;
        self.send_n_service(&request).await
    }

    /// Sends C-CANCEL explicitly on the most recently used request's context.
    /// For cancellation during `send_*`, use [`Self::cancellation_handle`]
    /// from another task instead (§11.4, SRV-08).
    ///
    /// `message_id` names the request to cancel, and the cancel travels on the
    /// presentation context that request used — a C-CANCEL sent on another
    /// context is a protocol error at the SCP. Returns `Ok(false)` when
    /// no association is open or the ID is not a request this client sent.
    /// As with fo-dicom, cancel is only meaningful while the request is still
    /// producing responses; the SCP's streaming C-FIND path returns a terminal
    /// `CANCEL` response and drops the provider stream.
    pub async fn cancel(&mut self, message_id: u16) -> Result<bool> {
        let Some(association) = self.association.as_mut() else {
            return Ok(false);
        };
        let Some((last_id, context_id)) = self.last_request else {
            return Ok(false);
        };
        if last_id != message_id {
            return Ok(false);
        }
        association.send_c_cancel(context_id, message_id).await?;
        Ok(true)
    }

    /// Releases the association; the client can be reused afterwards.
    pub async fn release(&mut self) {
        self.close_association().await;
    }

    /// Aborts the association (§11.4 cancellation mode).
    pub async fn abort(&mut self) {
        if let Some(association) = self.association.take() {
            self.emit(ClientEvent::StateChanged {
                state: ConnectionState::Closing,
            });
            let _ = association.abort(0).await;
            self.emit(ClientEvent::StateChanged {
                state: ConnectionState::Closed,
            });
        }
        self.sent_on_association = 0;
        self.last_request = None;
    }

    /// Tears down the current association according to `mode`
    /// (fo-dicom `DicomClientCancellationMode`). A client with no open
    /// association is unaffected.
    pub async fn shutdown(&mut self, mode: DicomClientCancellationMode) {
        match mode {
            DicomClientCancellationMode::ImmediatelyReleaseAssociation => self.release().await,
            DicomClientCancellationMode::ImmediatelyAbortAssociation => self.abort().await,
        }
    }
}

fn mixed_contexts(
    requests: &[crate::batch::DicomRequest],
) -> Result<(Vec<PresentationContext>, Vec<crate::pdu::RoleSelection>)> {
    use std::collections::BTreeMap;

    let mut proposals: Vec<(String, Vec<String>)> = Vec::new();
    let mut roles: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    let mut add_role = |sop_class: &str, scu: bool, scp: bool| {
        let role = roles.entry(sop_class.to_owned()).or_default();
        role.0 |= scu;
        role.1 |= scp;
    };
    let mut add_syntaxes = |sop_class: &str, syntaxes: Vec<String>| {
        proposals.push((sop_class.to_owned(), syntaxes));
    };

    for request in requests {
        match request {
            crate::batch::DicomRequest::Echo(request) => {
                add_role(&request.affected_sop_class_uid, true, false);
                add_syntaxes(
                    &request.affected_sop_class_uid,
                    vec![
                        crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.into(),
                        crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.into(),
                    ],
                );
            }
            crate::batch::DicomRequest::Store { file } => {
                let sop_class = file
                    .dataset
                    .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
                    .map(|value| value.trim().to_owned())
                    .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
                let syntax = file.dataset.transfer_syntax().uid().to_owned();
                add_role(&sop_class, true, false);
                add_syntaxes(
                    &sop_class,
                    crate::services::c_store_transfer_syntaxes(&syntax, &[], false),
                );
            }
            crate::batch::DicomRequest::StoreRequest(request) => {
                let sop_class = if request.affected_sop_class_uid.trim().is_empty() {
                    request
                        .file
                        .dataset
                        .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
                        .map(|value| value.trim().to_owned())
                        .ok_or(NetworkError::protocol("instance has no SOP class UID"))?
                } else {
                    request.affected_sop_class_uid.trim().to_owned()
                };
                let syntax = request.file.dataset.transfer_syntax().uid().to_owned();
                add_role(&sop_class, true, false);
                add_syntaxes(
                    &sop_class,
                    crate::services::c_store_transfer_syntaxes(&syntax, &[], false),
                );
            }
            crate::batch::DicomRequest::Find(request) => {
                add_role(&request.sop_class_uid, true, false);
                add_syntaxes(
                    &request.sop_class_uid,
                    vec![
                        crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.into(),
                        crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.into(),
                    ],
                );
            }
            crate::batch::DicomRequest::Get {
                request,
                storage_sop_classes,
            } => {
                add_role(&request.sop_class_uid, true, false);
                add_syntaxes(
                    &request.sop_class_uid,
                    vec![
                        crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.into(),
                        crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.into(),
                    ],
                );
                for storage_sop_class in storage_sop_classes {
                    add_role(storage_sop_class, false, true);
                    add_syntaxes(
                        storage_sop_class,
                        vec![
                            crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.into(),
                            crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.into(),
                        ],
                    );
                }
            }
            crate::batch::DicomRequest::Move(request) => {
                add_role(&request.sop_class_uid, true, false);
                add_syntaxes(
                    &request.sop_class_uid,
                    vec![
                        crate::association::transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.into(),
                        crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.into(),
                    ],
                );
            }
            crate::batch::DicomRequest::NService(request) => {
                if !crate::nservice::is_n_request(request.field) {
                    return Err(NetworkError::protocol(
                        "N-service request has a non-N command field",
                    ));
                }
                let sop_class = request.sop_class_uid.as_deref().ok_or_else(|| {
                    NetworkError::protocol("N-service request has no SOP class UID")
                })?;
                let syntax = request
                    .dataset
                    .as_ref()
                    .map(|dataset| dataset.transfer_syntax().uid().to_owned())
                    .unwrap_or_else(|| {
                        crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_owned()
                    });
                let (scu_role, scp_role) =
                    if request.field == crate::dimse::CommandField::NEventReportRq {
                        (false, true)
                    } else {
                        (true, false)
                    };
                add_role(sop_class, scu_role, scp_role);
                add_syntaxes(sop_class, vec![syntax]);
            }
        }
    }

    let mut builder = crate::association::ContextSetBuilder::new();
    for (sop_class, syntaxes) in proposals {
        builder.add(sop_class, syntaxes)?;
    }
    let roles = roles
        .into_iter()
        .filter_map(|(sop_class, (scu_role, scp_role))| {
            scp_role.then_some(crate::pdu::RoleSelection {
                sop_class,
                scu_role,
                scp_role,
            })
        })
        .collect();
    Ok((builder.to_wire(), roles))
}

/// True when the open association accepted every context in `wanted` with the
/// transfer syntax we intend to use.
///
/// An association that accepted the abstract syntax under a *different* transfer
/// syntax does not cover the request: sending in the preferred syntax would then
/// silently fall back to the accepted one (and transcode or fail). The caller
/// closes the association and re-negotiates with the syntax it actually needs.
fn covers(association: &Association, wanted: &[PresentationContext]) -> bool {
    wanted.iter().all(|context| {
        match context.transfer_syntaxes.first() {
            Some(preferred) => association
                .contexts()
                .find_exact(&context.abstract_syntax, preferred)
                .is_some(),
            // A proposal without a syntax can only be checked by abstract syntax.
            None => association
                .contexts()
                .find_abstract(&context.abstract_syntax)
                .is_some(),
        }
    })
}

/// True when the open association granted every role in `wanted`.
///
/// An association that negotiated no roles at all covers nothing here: the
/// C-GET storage SCP role must be granted explicitly (PS3.7 §7.4).
fn covers_roles(association: &Association, wanted: &[crate::pdu::RoleSelection]) -> bool {
    wanted.iter().all(|role| {
        association
            .contexts()
            .find_abstract(&role.sop_class)
            .is_some_and(|context| context.roles.iter().any(|granted| granted == role))
    })
}

/// True for the transport-level failures a retry can plausibly fix.
///
/// A peer that never answered at all is worth one more attempt; a refusal, a
/// TLS failure or a protocol error describes a peer that will answer the same
/// way next time.
fn is_retryable(error: &NetworkError) -> bool {
    match error {
        NetworkError::AssociationRequestTimedOut { .. } => true,
        NetworkError::Io(error) => {
            matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;
    use crate::server::{DicomServer, ServerOptions};
    use crate::services::{
        CEchoRequest, CEchoResponse, CStoreRequest, ServiceProvider, VerificationProvider,
    };
    use crate::store::StoreSink;

    #[tokio::test]
    async fn independent_handle_cancels_a_find_with_an_automatic_message_id() {
        struct PendingFind {
            started: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl ServiceProvider for PendingFind {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![sop_class::STUDY_ROOT_QR_FIND.into()]
            }
            fn on_c_find<'a>(
                &'a self,
                request: &'a CFindRequest,
            ) -> futures::stream::BoxStream<'a, Result<CFindResponse>> {
                assert_eq!(request.message_id, 1);
                self.started.notify_one();
                Box::pin(futures::stream::pending())
            }
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(PendingFind {
                started: started.clone(),
            }),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let serving = tokio::spawn(async move { server.accept_one().await });
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let cancel = client.cancellation_handle();
        assert!(!cancel.cancel(42), "no inactive request is cancelled");
        let sending = tokio::spawn(async move {
            let responses = client
                .send_c_find(&CFindRequest::study_query(0))
                .await
                .unwrap();
            client.release().await;
            responses
        });
        started.notified().await;
        assert!(
            !cancel.cancel(0),
            "the allocation sentinel is not a live ID"
        );
        assert!(cancel.cancel(1));
        let responses = tokio::time::timeout(Duration::from_secs(2), sending)
            .await
            .expect("live C-CANCEL is sent without borrowing the client")
            .unwrap();
        assert_eq!(
            responses.last().unwrap().status,
            crate::dimse::status::CANCEL
        );
        assert!(!cancel.cancel(1), "completed requests are no longer active");
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn independent_handle_cancels_get_and_move_with_automatic_message_ids() {
        struct PendingRetrieve {
            started: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl ServiceProvider for PendingRetrieve {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    sop_class::STUDY_ROOT_QR_GET.into(),
                    sop_class::STUDY_ROOT_QR_MOVE.into(),
                ]
            }
            fn on_c_get<'a>(
                &'a self,
                _: &'a CGetRequest,
            ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
                self.started.notify_one();
                Box::pin(futures::stream::pending())
            }
            fn on_c_move<'a>(
                &'a self,
                _: &'a CMoveRequest,
            ) -> futures::stream::BoxStream<'a, Result<CMoveResponse>> {
                self.started.notify_one();
                Box::pin(futures::stream::pending())
            }
        }
        for get in [true, false] {
            let started = Arc::new(tokio::sync::Notify::new());
            let server = DicomServer::bind(
                "127.0.0.1:0",
                Arc::new(PendingRetrieve {
                    started: started.clone(),
                }),
                ServerOptions::default(),
            )
            .await
            .unwrap();
            let addr = server.local_addr();
            let serving = tokio::spawn(async move { server.accept_one().await });
            let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
                .with_reverse_store_handler(Arc::new(|request| {
                    Ok(CStoreResponse::success(request))
                }));
            let cancel = client.cancellation_handle();
            let sending = tokio::spawn(async move {
                let terminal = if get {
                    client
                        .send_c_get(&CGetRequest::study("1.2.3", 0), &[])
                        .await
                        .unwrap()
                        .last()
                        .unwrap()
                        .status
                } else {
                    client
                        .send_c_move(&CMoveRequest::study("1.2.3", "DEST", 0))
                        .await
                        .unwrap()
                        .last()
                        .unwrap()
                        .status
                };
                client.release().await;
                terminal
            });
            started.notified().await;
            assert!(!cancel.cancel(0));
            assert!(cancel.cancel(1));
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), sending)
                    .await
                    .unwrap()
                    .unwrap(),
                crate::dimse::status::CANCEL
            );
            assert!(!cancel.cancel(1));
            serving.await.unwrap().unwrap();
        }
    }

    #[test]
    fn default_options_match_section_9_1() {
        let options = DicomClientOptions::default();
        assert_eq!(options.max_requests_per_association, None);
        assert_eq!(options.quota_cooldown, Duration::from_millis(50));
        assert_eq!(options.max_consecutive_timeout_retries, 3);
    }

    #[test]
    fn quota_forces_a_new_association() {
        // one request per association: the second call must reopen
        let options = DicomClientOptions {
            max_requests_per_association: Some(1),
            quota_cooldown: Duration::from_millis(0),
            ..Default::default()
        };
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:0").with_options(options);
        client.sent_on_association = 1;
        assert!(client.quota_spent());
        client.sent_on_association = 0;
        assert!(!client.quota_spent());
    }

    #[tokio::test]
    async fn empty_send_requests_does_not_open_an_association() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        let mut responses = 0;
        client
            .send_requests(std::iter::empty::<crate::batch::DicomRequest>(), |_| {
                responses += 1;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(responses, 0);
    }

    async fn mixed_peer_error(mode: u8) -> NetworkError {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let offered = crate::test_wire::accept_association(&mut stream).await;
            let Some(crate::pdu::Pdu::PData(pdata)) =
                crate::transport::read_pdu(&mut stream).await.unwrap()
            else {
                panic!("expected the queued request")
            };
            let request_pdv = pdata.pdvs.first().unwrap();
            if mode == 6 {
                return;
            }
            if mode == 8 {
                crate::transport::write_pdu(
                    &mut stream,
                    &crate::pdu::Pdu::Abort(crate::pdu::Abort {
                        source: crate::error::AbortSource::User,
                        reason: 1,
                    }),
                )
                .await
                .unwrap();
                return;
            }
            if mode == 9 {
                crate::transport::write_pdu(&mut stream, &crate::pdu::Pdu::AssociateAc(offered))
                    .await
                    .unwrap();
                return;
            }
            if mode == 4 {
                crate::transport::write_pdu(&mut stream, &crate::pdu::Pdu::ReleaseRq)
                    .await
                    .unwrap();
                return;
            }
            let request = crate::server::command_from_bytes(&request_pdv.data).unwrap();
            let mut response =
                crate::dimse::Command::response(&request, crate::dimse::status::SUCCESS).unwrap();
            if mode == 5 {
                response.no_dataset = false;
            }
            let context_id = match mode {
                0 => {
                    response.field = crate::dimse::CommandField::CFindRsp;
                    request_pdv.context_id
                }
                1 => {
                    response.message_id_being_responded_to = None;
                    request_pdv.context_id
                }
                2 => 3,
                7 => {
                    response.message_id_being_responded_to = Some(999);
                    request_pdv.context_id
                }
                3 => {
                    crate::transport::write_pdu(
                        &mut stream,
                        &crate::pdu::Pdu::PData(crate::pdu::PData {
                            pdvs: vec![crate::pdu::Pdv {
                                context_id: request_pdv.context_id,
                                is_command: false,
                                is_last: true,
                                data: bytes::Bytes::from_static(&[0]),
                            }],
                        }),
                    )
                    .await
                    .unwrap();
                    return;
                }
                5 => request_pdv.context_id,
                _ => unreachable!(),
            };
            crate::transport::write_pdu(
                &mut stream,
                &crate::pdu::Pdu::PData(crate::pdu::PData {
                    pdvs: vec![crate::pdu::Pdv {
                        context_id,
                        is_command: true,
                        is_last: true,
                        data: bytes::Bytes::from(crate::server::command_bytes(&response).unwrap()),
                    }],
                }),
            )
            .await
            .unwrap();
            if mode == 5 {
                let dataset = crate::message::encode_dataset(
                    &radicom::Dataset::new(),
                    &radicom::model::transfer_syntax::syntaxes::IMPLICIT_VR_LITTLE_ENDIAN,
                )
                .unwrap();
                crate::transport::write_pdu(
                    &mut stream,
                    &crate::pdu::Pdu::PData(crate::pdu::PData {
                        pdvs: vec![crate::pdu::Pdv {
                            context_id,
                            is_command: false,
                            is_last: true,
                            data: bytes::Bytes::from(dataset),
                        }],
                    }),
                )
                .await
                .unwrap();
            }
        });

        let mut queue = crate::batch::DicomRequestQueue::new();
        queue.push(CEchoRequest {
            message_id: 0,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        });
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let error = client.send_queue(queue, |_| Ok(())).await.unwrap_err();
        peer.await.unwrap();
        error
    }

    #[tokio::test]
    async fn mixed_queue_rejects_malformed_peer_responses() {
        for mode in 0..=9 {
            let error = mixed_peer_error(mode).await;
            assert!(!matches!(error, NetworkError::RequestTimedOut { .. }));
        }
    }

    #[tokio::test]
    async fn send_requests_mixes_echo_and_n_service_requests() {
        struct MixedProvider;

        #[async_trait::async_trait]
        impl ServiceProvider for MixedProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    sop_class::VERIFICATION.to_string(),
                    sop_class::STORAGE_COMMITMENT_PUSH.to_string(),
                ]
            }

            async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
                Ok(CEchoResponse::success(request))
            }

            async fn on_n_get(
                &self,
                request: &crate::nservice::NServiceRequest,
            ) -> Result<crate::nservice::NServiceResponse> {
                Ok(crate::nservice::NServiceResponse::success(request, None))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(MixedProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let serving = tokio::spawn(async move { server.accept_one().await });

        let mut nget = crate::nservice::NServiceRequest::new(crate::dimse::CommandField::NGetRq, 9);
        nget.sop_class_uid = Some(sop_class::STORAGE_COMMITMENT_PUSH.to_string());
        nget.sop_instance_uid = Some("1.2.3.4".to_string());
        let requests = vec![
            crate::batch::DicomRequest::from(CEchoRequest {
                message_id: 0,
                affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
            }),
            crate::batch::DicomRequest::from(nget),
        ];
        let mut events = Vec::new();

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        client
            .send_requests(requests, |event| {
                events.push((event.request_handle(), event.response));
                Ok(())
            })
            .await
            .unwrap();
        client.release().await;

        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|(handle, response)| {
            handle.index() == 0 && matches!(response, crate::batch::DicomResponse::CEcho(_))
        }));
        assert!(events.iter().any(|(handle, response)| {
            handle.index() == 1 && matches!(response, crate::batch::DicomResponse::NService(_))
        }));
        serving.await.unwrap().unwrap();
    }

    #[test]
    fn mixed_contexts_union_all_request_kinds_and_roles() {
        let mut dataset = radicom::Dataset::new();
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_CLASS_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.840.10008.5.1.4.1.1.7",
        );
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_INSTANCE_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.3.4",
        );
        let file = DicomFile::from_dataset(dataset);
        let store_request = CStoreRequest {
            message_id: 17,
            affected_sop_class_uid: "1.2.840.10008.5.1.4.1.1.7".into(),
            affected_sop_instance_uid: "1.2.3.4".into(),
            priority: crate::dimse::Priority::Medium,
            file: file.clone(),
            move_originator_ae_title: Some("MOVE-SCP".into()),
            move_originator_message_id: Some(9),
        };
        let mut inferred_store_request = store_request.clone();
        inferred_store_request.affected_sop_class_uid.clear();
        let mut nget = crate::nservice::NServiceRequest::new(crate::dimse::CommandField::NGetRq, 0);
        nget.sop_class_uid = Some(sop_class::STORAGE_COMMITMENT_PUSH.to_string());
        nget.sop_instance_uid = Some("1.2.3.4".to_string());
        let requests = vec![
            crate::batch::DicomRequest::Echo(CEchoRequest {
                message_id: 0,
                affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
            }),
            crate::batch::DicomRequest::store(Arc::new(file.clone())),
            crate::batch::DicomRequest::from(store_request),
            crate::batch::DicomRequest::from(inferred_store_request),
            crate::batch::DicomRequest::Find(CFindRequest::study_query(0)),
            crate::batch::DicomRequest::Get {
                request: CGetRequest::study("1.2.3", 0),
                storage_sop_classes: vec!["1.2.840.10008.5.1.4.1.1.7".to_string()],
            },
            crate::batch::DicomRequest::Move(CMoveRequest::study("1.2.3", "DEST", 0)),
            crate::batch::DicomRequest::NService(nget),
        ];

        let (contexts, roles) = mixed_contexts(&requests).unwrap();
        assert!(contexts
            .iter()
            .any(|context| context.abstract_syntax == sop_class::VERIFICATION));
        assert!(contexts
            .iter()
            .any(|context| { context.abstract_syntax == "1.2.840.10008.5.1.4.1.1.7" }));
        assert!(contexts
            .iter()
            .any(|context| context.abstract_syntax == sop_class::STUDY_ROOT_QR_FIND));
        assert!(contexts
            .iter()
            .any(|context| context.abstract_syntax == sop_class::STUDY_ROOT_QR_GET));
        assert!(contexts
            .iter()
            .any(|context| context.abstract_syntax == sop_class::STUDY_ROOT_QR_MOVE));
        assert!(contexts
            .iter()
            .any(|context| context.abstract_syntax == sop_class::STORAGE_COMMITMENT_PUSH));
        assert!(roles
            .iter()
            .any(|role| { role.sop_class == "1.2.840.10008.5.1.4.1.1.7" && role.scp_role }));
    }

    #[test]
    fn mixed_contexts_rejects_missing_sop_class_information() {
        let missing_store = crate::batch::DicomRequest::Store {
            file: Arc::new(DicomFile::from_dataset(radicom::Dataset::new())),
        };
        assert!(mixed_contexts(&[missing_store]).is_err());

        let missing_n_service = crate::batch::DicomRequest::NService(
            crate::nservice::NServiceRequest::new(crate::dimse::CommandField::NGetRq, 1),
        );
        assert!(mixed_contexts(&[missing_n_service]).is_err());
    }

    #[test]
    fn mixed_contexts_uses_scp_role_for_n_event_report() {
        let mut event =
            crate::nservice::NServiceRequest::new(crate::dimse::CommandField::NEventReportRq, 1);
        event.sop_class_uid = Some("1.2.840.10008.5.1.1.16".to_string());
        let (_, roles) = mixed_contexts(&[crate::batch::DicomRequest::from(event)]).unwrap();
        assert!(roles.iter().any(|role| {
            role.sop_class == "1.2.840.10008.5.1.1.16" && !role.scu_role && role.scp_role
        }));
    }

    #[test]
    fn mixed_contexts_rejects_non_n_service_command_fields() {
        let mut invalid =
            crate::nservice::NServiceRequest::new(crate::dimse::CommandField::CEchoRq, 1);
        invalid.sop_class_uid = Some(sop_class::VERIFICATION.to_string());
        let error = mixed_contexts(&[crate::batch::DicomRequest::from(invalid)]).unwrap_err();
        assert!(error.to_string().contains("non-N command field"));
    }

    #[tokio::test]
    async fn c_get_without_a_handler_fails_before_connecting() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        let request = CGetRequest::study("1.2.3", 1);
        match client.send_c_get(&request, &[]).await {
            Err(NetworkError::ReverseStoreHandlerMissing) => {}
            other => panic!("expected ReverseStoreHandlerMissing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queued_c_get_without_a_handler_fails_before_connecting() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        let mut queue = crate::batch::DicomRequestQueue::new();
        queue.push(crate::batch::DicomRequest::Get {
            request: CGetRequest::study("1.2.3", 1),
            storage_sop_classes: Vec::new(),
        });
        assert!(matches!(
            client.send_queue(queue, |_| Ok(())).await,
            Err(NetworkError::ReverseStoreHandlerMissing)
        ));
    }

    #[tokio::test]
    async fn queued_c_get_with_handler_but_without_storage_classes_fails_before_connecting() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1").with_reverse_store_handler(
            Arc::new(|request| Ok(crate::services::CStoreResponse::success(request))),
        );
        let error = client
            .send_requests(
                [crate::batch::DicomRequest::from(CGetRequest::study(
                    "1.2.3", 0,
                ))],
                |_| Ok(()),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("storage SOP classes"), "{error}");
    }

    #[tokio::test]
    async fn send_n_service_rejects_non_n_command_fields_before_connecting() {
        let mut request =
            crate::nservice::NServiceRequest::new(crate::dimse::CommandField::CEchoRq, 1);
        request.sop_class_uid = Some(sop_class::VERIFICATION.to_string());
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        let error = client.send_n_service(&request).await.unwrap_err();
        assert!(error.to_string().contains("non-N command field"), "{error}");
    }

    #[tokio::test]
    async fn queued_c_get_uses_registered_reverse_store_classes() {
        struct RegisteredQueueGetProvider;

        #[async_trait::async_trait]
        impl ServiceProvider for RegisteredQueueGetProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    sop_class::STUDY_ROOT_QR_GET.to_string(),
                    "1.2.840.10008.5.1.4.1.1.7".to_string(),
                ]
            }

            fn on_c_get<'a>(
                &'a self,
                _request: &'a CGetRequest,
            ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
                let mut dataset = radicom::Dataset::new();
                dataset.insert_str(
                    radicom::model::tag::Tag::SOP_CLASS_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.840.10008.5.1.4.1.1.7",
                );
                dataset.insert_str(
                    radicom::model::tag::Tag::SOP_INSTANCE_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.3.4.5",
                );
                Box::pin(futures::stream::once(async move {
                    Ok(crate::cget::CGetEvent::instance(DicomFile::from_dataset(
                        dataset,
                    )))
                }))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(RegisteredQueueGetProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let serving = tokio::spawn(async move { server.accept_one().await });
        let received = Arc::new(std::sync::Mutex::new(Vec::<DicomFile>::new()));
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .collect_reverse_stores(received.clone())
            .with_reverse_store_classes(vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]);
        let mut queue = crate::batch::DicomRequestQueue::new();
        queue.push(CGetRequest::study("1.2.3", 0));

        client.send_queue(queue, |_| Ok(())).await.unwrap();
        assert_eq!(received.lock().unwrap().len(), 1);
        // Reuse the negotiated association so role coverage is checked on a
        // second mixed-request admission as fo-dicom does for a request list.
        client
            .send_requests(
                [crate::batch::DicomRequest::from(CGetRequest::study(
                    "1.2.4", 0,
                ))],
                |_| Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(received.lock().unwrap().len(), 2);
        client.release().await;
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_peer_is_retried_then_reported() {
        let options = DicomClientOptions {
            max_consecutive_timeout_retries: 2,
            ..Default::default()
        };
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1").with_options(options);
        // nothing listens on port 1
        let result = client.send_c_echo(sop_class::VERIFICATION).await;
        assert!(result.is_err(), "the connection is refused");
    }

    #[tokio::test]
    async fn echo_through_the_high_level_client() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let response = client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn mixed_batch_uses_one_association_and_forwards_pending_events() {
        struct Mixed;
        #[async_trait::async_trait]
        impl ServiceProvider for Mixed {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    sop_class::VERIFICATION.into(),
                    sop_class::STUDY_ROOT_QR_FIND.into(),
                ]
            }

            async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
                Ok(CEchoResponse::success(request))
            }

            fn on_c_find<'a>(
                &'a self,
                request: &'a CFindRequest,
            ) -> futures::stream::BoxStream<'a, Result<CFindResponse>> {
                let mut item = radicom::Dataset::new();
                item.insert_str(
                    radicom::model::tag::Tag::STUDY_INSTANCE_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.3",
                );
                Box::pin(futures::stream::iter([
                    Ok(CFindResponse::pending(request, item)),
                    Ok(CFindResponse::success(request, 1)),
                ]))
            }
        }
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(Mixed), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let mut events = Vec::new();
        client
            .send_batch(
                vec![
                    crate::batch::DicomRequest::Echo(CEchoRequest {
                        message_id: 0,
                        affected_sop_class_uid: sop_class::VERIFICATION.into(),
                    }),
                    crate::batch::DicomRequest::Find(CFindRequest::study_query(7)),
                ],
                |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].request_index, 0);
        assert_eq!(events[1].request_index, 1);
        assert!(!events[1].is_final);
        assert!(events[2].is_final);
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[test]
    fn the_default_cancellation_mode_releases_the_association() {
        assert_eq!(
            DicomClientCancellationMode::default(),
            DicomClientCancellationMode::ImmediatelyReleaseAssociation
        );
    }

    #[tokio::test]
    async fn shutdown_releases_the_association_in_release_mode() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        client
            .shutdown(DicomClientCancellationMode::ImmediatelyReleaseAssociation)
            .await;
        // a clean release is what lets the server's accept_one observe a
        // graceful close rather than a dropped connection
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn events_report_the_association_lifecycle() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string()).with_event_handler(
            Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event)
            }),
        );
        client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        client.release().await;

        let events = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            events,
            vec![
                ClientEvent::StateChanged {
                    state: ConnectionState::Open
                },
                ClientEvent::AssociationAccepted { attempt: 1 },
                ClientEvent::StateChanged {
                    state: ConnectionState::Closing
                },
                ClientEvent::StateChanged {
                    state: ConnectionState::Closed
                },
                ClientEvent::AssociationReleased,
            ]
        );
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn store_through_the_high_level_client() {
        struct StoreProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for StoreProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
            }
            async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
                Ok(CStoreResponse::success(request))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(StoreProvider),
            ServerOptions {
                store_sink: StoreSink::Memory,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut dataset = radicom::model::dataset::Dataset::new();
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_CLASS_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.840.10008.5.1.4.1.1.7",
        );
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_INSTANCE_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.3",
        );
        dataset.set_transfer_syntax(
            radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        let file = DicomFile::from_dataset(dataset);

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let response = client.send_c_store(&file).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn c_get_through_the_high_level_client_receives_instances() {
        struct GetProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for GetProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    crate::association::sop_class::STUDY_ROOT_QR_GET.to_string(),
                    "1.2.840.10008.5.1.4.1.1.7".to_string(),
                ]
            }
            fn on_c_get<'a>(
                &'a self,
                request: &'a crate::cget::CGetRequest,
            ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
                let _ = request;
                let mut dataset = radicom::model::dataset::Dataset::new();
                dataset.insert_str(
                    radicom::model::tag::Tag::SOP_CLASS_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.840.10008.5.1.4.1.1.7",
                );
                dataset.insert_str(
                    radicom::model::tag::Tag::SOP_INSTANCE_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.3.4.5",
                );
                dataset.set_transfer_syntax(
                    radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
                );
                Box::pin(futures::stream::once(async move {
                    Ok(crate::cget::CGetEvent::instance(DicomFile::from_dataset(
                        dataset,
                    )))
                }))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(GetProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let received: Arc<std::sync::Mutex<Vec<DicomFile>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .collect_reverse_stores(received.clone());
        let storage = vec!["1.2.840.10008.5.1.4.1.1.7".to_string()];
        let responses = client
            .send_c_get(&CGetRequest::study("1.2.3", 1), &storage)
            .await
            .unwrap();

        assert_eq!(
            received
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "the instance arrived through the reverse handler"
        );
        assert_eq!(
            responses.last().unwrap().status,
            crate::dimse::status::SUCCESS,
            "one instance completed"
        );
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn c_get_through_registered_storage_classes() {
        struct RegisteredGetProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for RegisteredGetProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    crate::association::sop_class::STUDY_ROOT_QR_GET.to_string(),
                    "1.2.840.10008.5.1.4.1.1.7".to_string(),
                ]
            }
            fn on_c_get<'a>(
                &'a self,
                request: &'a crate::cget::CGetRequest,
            ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
                let _ = request;
                let mut dataset = radicom::model::dataset::Dataset::new();
                dataset.insert_str(
                    radicom::model::tag::Tag::SOP_CLASS_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.840.10008.5.1.4.1.1.7",
                );
                dataset.insert_str(
                    radicom::model::tag::Tag::SOP_INSTANCE_UID,
                    radicom::model::vr::Vr::Ui,
                    "1.2.3.4.5",
                );
                dataset.set_transfer_syntax(
                    radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
                );
                Box::pin(futures::stream::once(async move {
                    Ok(crate::cget::CGetEvent::instance(DicomFile::from_dataset(
                        dataset,
                    )))
                }))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(RegisteredGetProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let received: Arc<std::sync::Mutex<Vec<DicomFile>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .collect_reverse_stores(received.clone())
            .with_reverse_store_classes(vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]);
        let responses = client
            .send_c_get_registered(&CGetRequest::study("1.2.3", 1))
            .await
            .unwrap();

        assert_eq!(
            received
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "the instance arrived through the registered storage classes"
        );
        assert_eq!(
            responses.last().unwrap().status,
            crate::dimse::status::SUCCESS,
            "one instance completed"
        );
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn c_get_registered_without_declared_classes_is_a_clear_error() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1").with_reverse_store_handler(
            Arc::new(|request: &crate::services::CStoreRequest| {
                Ok(crate::services::CStoreResponse::success(request))
            }),
        );
        match client
            .send_c_get_registered(&CGetRequest::study("1.2.3", 1))
            .await
        {
            Ok(_) => panic!("expected an error without declared classes"),
            Err(error) => assert!(error.to_string().contains("storage SOP classes"), "{error}"),
        }
    }

    #[tokio::test]
    async fn c_find_through_the_high_level_client() {
        struct FindProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for FindProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![crate::association::sop_class::STUDY_ROOT_QR_FIND.to_string()]
            }
            fn on_c_find<'a>(
                &'a self,
                request: &'a CFindRequest,
            ) -> futures::stream::BoxStream<'a, Result<CFindResponse>> {
                Box::pin(futures::stream::once(async move {
                    Ok(CFindResponse::success(request, 0))
                }))
            }
        }
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(FindProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let responses = client
            .send_c_find(&CFindRequest::study_query(1))
            .await
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].status, crate::dimse::status::SUCCESS);
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancel_reports_whether_it_named_a_request_that_was_sent() {
        // §11.2: the previous test cancelled an id whose response had already
        // arrived and then only asserted "the association still works", which
        // passes for any implementation, including one that ignores C-CANCEL
        // entirely. The observable contract here is the return value — true only
        // for a request this client actually sent — plus a healthy association.
        struct FindProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for FindProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![crate::association::sop_class::STUDY_ROOT_QR_FIND.to_string()]
            }
            fn on_c_find<'a>(
                &'a self,
                request: &'a CFindRequest,
            ) -> futures::stream::BoxStream<'a, Result<CFindResponse>> {
                Box::pin(futures::stream::once(async move {
                    Ok(CFindResponse::success(request, 0))
                }))
            }
        }
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(FindProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        client
            .send_c_find(&CFindRequest::study_query(1))
            .await
            .unwrap();

        assert!(
            client.cancel(1).await.unwrap(),
            "the request we just sent is cancellable"
        );
        assert!(
            !client.cancel(999).await.unwrap(),
            "an id this client never sent is not reported as a sent cancel"
        );

        let responses = client
            .send_c_find(&CFindRequest::study_query(2))
            .await
            .unwrap();
        assert_eq!(
            responses.len(),
            1,
            "the association stayed healthy after C-CANCEL"
        );

        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancel_without_an_association_is_a_no_op() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        assert!(
            !client.cancel(1).await.unwrap(),
            "no association yet means nothing to cancel"
        );
    }

    #[tokio::test]
    async fn a_rejection_is_not_retried() {
        // the SCP serves nothing, so the association is rejected
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string()).with_event_handler(
            Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event)
            }),
        );
        // C-MOVE is not served by the Verification provider
        let request = CMoveRequest::study("1.2.3", "DEST", 1);
        match client.send_c_move(&request).await {
            Err(NetworkError::Reject { .. }) => {}
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(matches!(
            seen.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .first(),
            Some(ClientEvent::AssociationRejected { .. })
        ));
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_second_sop_class_gets_its_own_association() {
        // an association is negotiated once, so a C-STORE after a C-ECHO
        // used to fail with "SOP class ... is not supported by the association"
        struct EchoAndStore;
        #[async_trait::async_trait]
        impl ServiceProvider for EchoAndStore {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    sop_class::VERIFICATION.to_string(),
                    "1.2.840.10008.5.1.4.1.1.7".to_string(),
                ]
            }
            async fn on_c_echo(
                &self,
                request: &crate::services::CEchoRequest,
            ) -> Result<CEchoResponse> {
                Ok(CEchoResponse::success(request))
            }
            async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
                Ok(CStoreResponse::success(request))
            }
        }
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(EchoAndStore),
            ServerOptions {
                store_sink: StoreSink::Memory,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string()).with_event_handler(
            Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event)
            }),
        );

        client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        let response = client.send_c_store(&instance()).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);

        let events = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let accepted = events
            .iter()
            .filter(|event| matches!(event, ClientEvent::AssociationAccepted { .. }))
            .count();
        assert_eq!(
            accepted, 2,
            "the storage class needs an association of its own"
        );
        assert!(
            events.contains(&ClientEvent::AssociationReleased),
            "the echo association is released before the second is opened"
        );
        client.release().await;
    }

    fn instance() -> DicomFile {
        let mut dataset = radicom::model::dataset::Dataset::new();
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_CLASS_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.840.10008.5.1.4.1.1.7",
        );
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_INSTANCE_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.3",
        );
        dataset.set_transfer_syntax(
            radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        DicomFile::from_dataset(dataset)
    }

    /// A stored instance of `sop_class` with the given SOP Instance UID.
    fn store_instance(sop_class: &str, sop_instance_uid: &str) -> DicomFile {
        let mut dataset = radicom::model::dataset::Dataset::new();
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_CLASS_UID,
            radicom::model::vr::Vr::Ui,
            sop_class,
        );
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_INSTANCE_UID,
            radicom::model::vr::Vr::Ui,
            sop_instance_uid,
        );
        dataset.set_transfer_syntax(
            radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        DicomFile::from_dataset(dataset)
    }

    #[tokio::test]
    async fn a_store_batch_proposes_a_unique_context_per_sop_class() {
        // NW-E-01: two classes in one batch used to propose two contexts with
        // ID 1, which the SCP rejects (PS3.8 §9.3.2); the association then
        // never came up. The builder now hands out unique odd IDs.
        struct CtAndMr;
        #[async_trait::async_trait]
        impl ServiceProvider for CtAndMr {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![
                    "1.2.840.10008.5.1.4.1.1.2".to_string(),
                    "1.2.840.10008.5.1.4.1.1.4".to_string(),
                ]
            }
            async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
                Ok(CStoreResponse::success(request))
            }
        }
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(CtAndMr),
            ServerOptions {
                store_sink: StoreSink::Memory,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let ct = store_instance("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.6");
        let mr = store_instance("1.2.840.10008.5.1.4.1.1.4", "1.2.3.4.5.7");
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let responses = client.send_c_store_batch(&[&ct, &mr]).await.unwrap();
        assert_eq!(responses.len(), 2, "one response per instance");
        assert!(
            responses
                .iter()
                .all(|response| response.status == crate::dimse::status::SUCCESS),
            "both classes were stored on the one association",
        );
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_find_batch_counts_against_the_association_quota() {
        // NW-E-04: the Ok path returned without bumping `sent_on_association`,
        // so a batch of queries never triggered the quota. With a quota of one,
        // two calls must reopen the association (two accepted events).
        struct FindProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for FindProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
            }
            fn on_c_find<'a>(
                &'a self,
                request: &'a CFindRequest,
            ) -> futures::stream::BoxStream<'a, Result<CFindResponse>> {
                Box::pin(futures::stream::once(async move {
                    Ok(CFindResponse::success(request, 0))
                }))
            }
        }
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(FindProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_options(DicomClientOptions {
                max_requests_per_association: Some(1),
                quota_cooldown: Duration::from_millis(0),
                ..Default::default()
            })
            .with_event_handler(Arc::new(move |event| {
                if matches!(event, ClientEvent::AssociationAccepted { .. }) {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));

        for round in 0..2 {
            client
                .send_c_find_batch(&[CFindRequest::study_query(round + 1)])
                .await
                .unwrap();
            assert_eq!(
                client.sent_on_association, 1,
                "the batch is counted against the quota"
            );
        }
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "each quota-exhausted batch opens a fresh association"
        );
        client.release().await;
    }

    /// A peer that accepts connections and then does nothing, so the client
    /// can only fail by timing out. Returns the address and the attempt count.
    async fn silent_peer() -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // hold the socket open: EOF would be a different failure
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(stream);
                });
            }
        });
        (addr, attempts)
    }

    /// A peer that accepts the association and then never answers DIMSE.
    async fn silent_associated_peer() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::test_wire::accept_association(&mut stream).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        (addr, peer)
    }

    fn impatient_options(max_retries: u32) -> DicomClientOptions {
        DicomClientOptions {
            max_consecutive_timeout_retries: max_retries,
            connection: crate::server::ClientOptions {
                association_timeout: Duration::from_millis(100),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_silent_peer_is_retried_up_to_the_cap() {
        // the option counts attempts, not extra tries: 2 means two connections
        let (addr, attempts) = silent_peer().await;
        let mut client =
            DicomClient::new("SCU", "SCP", addr.to_string()).with_options(impatient_options(2));
        match client.send_c_echo(sop_class::VERIFICATION).await {
            Err(NetworkError::AssociationRequestTimedOut { .. }) => {}
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn zero_retries_means_one_attempt() {
        // max_consecutive_timeout_retries = 0 means "do not retry"
        let (addr, attempts) = silent_peer().await;
        let mut client =
            DicomClient::new("SCU", "SCP", addr.to_string()).with_options(impatient_options(0));
        assert!(client.send_c_echo(sop_class::VERIFICATION).await.is_err());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn association_timeout_is_reported_to_event_handlers() {
        let (addr, _attempts) = silent_peer().await;
        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_options(impatient_options(2))
            .with_event_handler(Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event)
            }));

        let error = client
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NetworkError::AssociationRequestTimedOut { .. }
        ));
        let events = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(events.contains(&ClientEvent::AssociationRequestTimedOut { attempts: 1 }));
        assert!(events.contains(&ClientEvent::AssociationRequestTimedOut { attempts: 2 }));
    }

    #[tokio::test]
    async fn request_timeout_reports_event_and_returns_without_waiting_forever() {
        let (addr, peer) = silent_associated_peer().await;
        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let options = DicomClientOptions {
            max_consecutive_timeout_retries: 1,
            connection: crate::server::ClientOptions {
                association_timeout: Duration::from_millis(100),
                request_timeout: Some(Duration::from_millis(50)),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_options(options)
            .with_event_handler(Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event)
            }));

        let error = client
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NetworkError::RequestTimedOut { message_id: 1 }
        ));
        assert!(seen
            .lock()
            .unwrap()
            .contains(&ClientEvent::RequestTimedOut { message_id: 1 }));
        peer.abort();
    }

    #[tokio::test]
    async fn a_protocol_error_is_not_retried() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // not a PDU at all: retrying would only repeat the same answer
                let _ = stream
                    .write_all(&[0xEE, 0x00, 0x00, 0x00, 0x00, 0x00])
                    .await;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(stream);
                });
            }
        });

        let mut client =
            DicomClient::new("SCU", "SCP", addr.to_string()).with_options(impatient_options(5));
        let error = client
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap_err();
        assert!(
            !matches!(error, NetworkError::AssociationRequestTimedOut { .. }),
            "the peer answered, so this is not a timeout: {error:?}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry"
        );
    }

    #[tokio::test]
    async fn a_cancel_travels_on_the_context_the_request_used() {
        // C-CANCEL names the cancelled request, and the SCP resolves it
        // within the presentation context it arrives on — so it must be the
        // context the request used, not context 1.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::test_wire::accept_association(&mut stream).await;
            loop {
                match crate::transport::read_pdu(&mut stream).await.unwrap() {
                    Some(crate::pdu::Pdu::PData(pdata)) => {
                        let pdv = pdata.pdvs.first().expect("a P-DATA carries a PDV");
                        assert_eq!(pdv.context_id, 3, "the C-FIND's own context");
                        let command = crate::server::command_from_bytes(&pdv.data).unwrap();
                        assert_eq!(command.field, crate::dimse::CommandField::CCancelRq);
                        assert_eq!(command.message_id_being_responded_to, Some(7));
                        return;
                    }
                    Some(_) => continue,
                    None => panic!("the association closed before the cancel"),
                }
            }
        });

        let contexts = vec![
            PresentationContext::new(
                1,
                "1.2.840.10008.5.1.4.1.1.7",
                vec!["1.2.840.10008.1.2".to_string()],
            ),
            PresentationContext::new(
                3,
                "1.2.840.10008.5.1.4.1.2.1.1",
                vec!["1.2.840.10008.1.2".to_string()],
            ),
        ];
        let association = Association::open(
            addr,
            "SCU",
            "SCP",
            contexts,
            crate::server::ClientOptions::default(),
        )
        .await
        .unwrap();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        client.association = Some(association);
        // the C-FIND (message ID 7) was sent on the second context
        client.last_request = Some((7, 3));

        assert!(client.cancel(7).await.unwrap());
        assert!(
            !client.cancel(8).await.unwrap(),
            "an ID this client never sent"
        );
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn an_implicit_le_association_does_not_cover_a_jpeg_context() {
        // D: reusing an association that only accepted Implicit VR LE for a
        // JPEG instance would silently transcode (or fail). `covers` must look
        // at the transfer syntax, not just the abstract syntax.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::test_wire::accept_association(&mut stream).await;
            // hold the association until the test drops it
            let _ = crate::transport::read_pdu(&mut stream).await;
        });
        const CT: &str = "1.2.840.10008.5.1.4.1.1.2";
        const IMPLICIT: &str = "1.2.840.10008.1.2";
        const JPEG: &str = "1.2.840.10008.1.2.4.50";
        let association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![PresentationContext::new(1, CT, vec![IMPLICIT.to_string()])],
            crate::server::ClientOptions::default(),
        )
        .await
        .unwrap();

        assert!(
            covers(
                &association,
                &[PresentationContext::new(1, CT, vec![IMPLICIT.to_string()])]
            ),
            "the accepted Implicit VR LE context is covered"
        );
        assert!(
            covers(&association, &[PresentationContext::new(1, CT, Vec::new())]),
            "a proposal without a preferred syntax falls back to the abstract syntax"
        );
        assert!(
            !covers(
                &association,
                &[PresentationContext::new(1, CT, vec![JPEG.to_string()])]
            ),
            "a JPEG context is not covered by an Implicit-VR-only association"
        );
        // even with the implicit fallback in the proposal, the preferred JPEG
        // syntax must be accepted exactly
        assert!(
            !covers(
                &association,
                &[PresentationContext::new(
                    1,
                    CT,
                    crate::services::c_store_transfer_syntaxes(JPEG, &[], false)
                )]
            ),
            "the preferred JPEG syntax is not accepted"
        );

        drop(association);
        let _ = peer.await;
    }

    async fn echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(crate::services::VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move {
            let _ = server.run().await;
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn the_reverse_store_handler_and_abort_shutdown_are_wired_up() {
        let (addr, server) = echo_server().await;
        let supplied = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = supplied.clone();
        let handler: crate::server::ReverseStoreHandler =
            Arc::new(move |request: &crate::services::CStoreRequest| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CStoreResponse::success(request))
            });
        let events: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_reverse_store_handler(handler)
            .with_event_handler(Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            }));

        client
            .send_c_echo(crate::association::sop_class::VERIFICATION)
            .await
            .unwrap();
        client
            .shutdown(DicomClientCancellationMode::ImmediatelyAbortAssociation)
            .await;
        assert_eq!(supplied.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&ClientEvent::StateChanged {
                state: ConnectionState::Closed
            }));
        server.abort();
    }

    #[tokio::test]
    async fn a_c_get_with_too_many_storage_classes_is_refused() {
        // NET-22: the retrieve context plus one per storage class must come
        // out of the odd-ID allocator; 128 is the whole available range
        let mut client =
            DicomClient::new("SCU", "SCP", "127.0.0.1:1").with_reverse_store_handler(Arc::new(
                |request: &crate::services::CStoreRequest| Ok(CStoreResponse::success(request)),
            ));
        let request = crate::cget::CGetRequest::study("1.2.3", 1);
        let classes: Vec<String> = (0..130)
            .map(|n| format!("1.2.840.10008.5.1.4.1.1.{}", 200 + n))
            .collect();
        let error = client
            .send_c_get(&request, &classes)
            .await
            .expect_err("129 contexts do not fit");
        assert!(
            error.to_string().contains("presentation context"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_spent_quota_retires_the_association() {
        let (addr, server) = echo_server().await;
        let options = DicomClientOptions {
            max_requests_per_association: Some(1),
            quota_cooldown: Duration::from_millis(0),
            ..DicomClientOptions::default()
        };
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string()).with_options(options);
        for _ in 0..2 {
            let response = client
                .send_c_echo(crate::association::sop_class::VERIFICATION)
                .await
                .unwrap();
            assert_eq!(response.status, crate::dimse::status::SUCCESS);
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_peer_that_closes_mid_request_is_forgotten() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::test_wire::accept_association(&mut stream).await;
            // the peer leaves instead of answering the C-ECHO
            drop(stream);
        });

        let events: Arc<std::sync::Mutex<Vec<ClientEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string()).with_event_handler(
            Arc::new(move |event| {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            }),
        );
        let error = client
            .send_c_echo(crate::association::sop_class::VERIFICATION)
            .await
            .expect_err("the peer left");
        assert!(matches!(
            error,
            NetworkError::ClosedPrematurely { .. } | NetworkError::Io(_)
        ));
        assert!(events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&ClientEvent::StateChanged {
                state: ConnectionState::Closed
            }));
        assert!(
            client.association.is_none(),
            "the dead association is dropped"
        );
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn c_move_through_the_high_level_client() {
        struct MoveProvider;
        #[async_trait::async_trait]
        impl ServiceProvider for MoveProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![crate::association::sop_class::STUDY_ROOT_QR_MOVE.to_string()]
            }
            fn on_c_move<'a>(
                &'a self,
                request: &'a crate::cmove::CMoveRequest,
            ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
                Box::pin(futures::stream::iter(vec![
                    Ok(crate::cmove::CMoveResponse::pending(
                        request,
                        1,
                        crate::cmove::SubOperationSummary::default(),
                    )),
                    Ok(crate::cmove::CMoveResponse::completed(
                        request,
                        crate::cmove::SubOperationSummary::default(),
                    )),
                ]))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(MoveProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let responses = client
            .send_c_move(&crate::cmove::CMoveRequest::study("1.2.3", "DEST", 1))
            .await
            .unwrap();
        assert_eq!(responses.len(), 2, "one pending and one terminal");
        assert!(responses[0].is_pending());
        assert!(!responses.last().unwrap().is_pending());
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    const PRINTER_SOP_CLASS: &str = "1.2.840.10008.5.1.1.16";

    /// An SCP that answers N-ACTION and reports the event afterwards (H2).
    struct PrinterScp {
        actions: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ServiceProvider for PrinterScp {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![PRINTER_SOP_CLASS.to_string()]
        }

        async fn on_n_action(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            self.actions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::nservice::NServiceResponse::success(request, None))
        }

        async fn on_send_n_event_report(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<Vec<crate::nservice::NServiceRequest>> {
            // only the first action reports, so the test's second request is
            // free to read and answer the pushed report
            if self.actions.load(std::sync::atomic::Ordering::SeqCst) > 1 {
                return Ok(Vec::new());
            }
            let mut event = crate::nservice::NServiceRequest::new(
                crate::dimse::CommandField::NEventReportRq,
                0,
            );
            event.sop_class_uid = request.sop_class_uid.clone();
            event.sop_instance_uid = request.sop_instance_uid.clone();
            event.event_type_id = Some(1);
            let mut dataset = radicom::model::dataset::Dataset::new();
            dataset.insert_str(
                radicom::model::tag::Tag::PATIENT_NAME,
                radicom::model::vr::Vr::Pn,
                "Report",
            );
            dataset.set_transfer_syntax(
                radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            );
            event.dataset = Some(dataset);
            Ok(vec![event])
        }
    }

    #[tokio::test]
    async fn an_n_action_round_trip_and_the_scp_pushed_event_report() {
        // H2: the high-level client sends an N-service request, and the SCP of
        // the service class reports the event back on the same association
        // (fo-dicom `DicomClient.OnNEventReportRequest`).
        let actions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(PrinterScp {
                actions: actions.clone(),
            }),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let seen: Arc<std::sync::Mutex<Vec<(u16, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_n_event_report_handler(Arc::new(move |request| {
                recorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((
                        request.event_type_id.unwrap_or(0),
                        request.sop_class_uid.clone().unwrap_or_default(),
                    ));
                Ok(crate::nservice::NServiceResponse::success(request, None))
            }));
        // SCP so the peer may report the event here, SCU so this side may still
        // send the N-ACTION on the same class (PS3.7 §10.1 role selection)
        client.options.connection.role_selections = vec![crate::pdu::RoleSelection {
            sop_class: PRINTER_SOP_CLASS.to_string(),
            scu_role: true,
            scp_role: true,
        }];

        let first = client
            .send_n_action(PRINTER_SOP_CLASS, "1.2.3.4", 1, None)
            .await
            .unwrap();
        assert_eq!(first.status, crate::dimse::status::SUCCESS);
        assert!(
            seen.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "an idle client reads the report with its next request"
        );

        let second = client
            .send_n_action(PRINTER_SOP_CLASS, "1.2.3.4", 1, None)
            .await
            .unwrap();
        assert_eq!(second.status, crate::dimse::status::SUCCESS);
        assert_eq!(
            *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![(1u16, PRINTER_SOP_CLASS.to_string())],
            "the client answered exactly one pushed report"
        );
        assert_eq!(actions.load(std::sync::atomic::Ordering::SeqCst), 2);

        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_large_store_reaches_the_sink_through_a_temp_file() {
        // H3: the dataset PDVs stream to a temporary file while they arrive, so
        // the sink sees an instance far larger than one PDU without the
        // receiver holding it.
        struct StoreProvider {
            seen: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl ServiceProvider for StoreProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
            }
            async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
                assert_eq!(
                    request
                        .file
                        .dataset
                        .get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
                        .unwrap(),
                    "1.2.3"
                );
                self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CStoreResponse::success(request))
            }
        }

        let receive_dir = std::env::temp_dir().join(format!("radicom-recv-{}", std::process::id()));
        let sink_dir = std::env::temp_dir().join(format!("radicom-sink-{}", std::process::id()));
        std::fs::create_dir_all(&receive_dir).unwrap();
        std::fs::create_dir_all(&sink_dir).unwrap();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(StoreProvider { seen: seen.clone() }),
            ServerOptions {
                store_sink: StoreSink::SpoolToDir(sink_dir.clone()),
                store_spool: crate::assembler::SpoolConfig {
                    memory_threshold: 0,
                    max_message_length: 64 * 1024 * 1024,
                    dir: Some(receive_dir.clone()),
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut dataset = radicom::model::dataset::Dataset::new();
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_CLASS_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.840.10008.5.1.4.1.1.7",
        );
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_INSTANCE_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.3",
        );
        // 2 MiB: many PDVs at the default 16 KiB peer maximum
        dataset.insert_or_update(radicom::model::item::DicomItem::Element(
            radicom::model::element::DicomElement::new_bytes(
                radicom::model::tag::Tag::PIXEL_DATA,
                radicom::model::vr::Vr::Ob,
                Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
                    0x5Au8;
                    2 * 1024
                        * 1024
                ])),
            ),
        ));
        dataset.set_transfer_syntax(
            radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        let file = DicomFile::from_dataset(dataset);

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let response = client.send_c_store(&file).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);

        let stored: Vec<std::path::PathBuf> = std::fs::read_dir(&sink_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(stored.len(), 1, "the sink kept exactly one instance");
        assert!(
            std::fs::metadata(&stored[0]).unwrap().len() >= 2 * 1024 * 1024,
            "the whole dataset arrived"
        );
        assert_eq!(
            std::fs::read_dir(&receive_dir).unwrap().count(),
            0,
            "the receive spool is removed once the sink copied it"
        );

        client.release().await;
        handle.await.unwrap().unwrap();
        std::fs::remove_dir_all(&sink_dir).ok();
        std::fs::remove_dir_all(&receive_dir).ok();
    }

    /// Sends `file` to a fresh loopback SCP configured with `fallback`, and
    /// returns the PatientName the provider decoded (H1).
    async fn store_and_read_name(fallback: Option<&'static str>, file: &DicomFile) -> String {
        struct StoreProvider {
            seen: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl ServiceProvider for StoreProvider {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
            }
            async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
                assert!(
                    !request
                        .file
                        .dataset
                        .contains(&radicom::model::tag::Tag::SPECIFIC_CHARACTER_SET),
                    "the premise is a dataset without (0008,0005)"
                );
                let name = request
                    .file
                    .dataset
                    .get_str(radicom::model::tag::Tag::PATIENT_NAME)
                    .unwrap_or_default();
                self.seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(name);
                Ok(CStoreResponse::success(request))
            }
        }

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(StoreProvider { seen: seen.clone() }),
            ServerOptions {
                fallback_encoding: fallback,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let response = client.send_c_store(file).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        client.release().await;
        handle.await.unwrap().unwrap();
        let name = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .expect("the instance arrived");
        name
    }

    /// An instance whose PatientName is "Müller" encoded with `term`, carrying
    /// no (0008,0005): the sender writes the text in the dataset's own fallback
    /// encoding, so the wire bytes are the declared repertoire (H1).
    fn instance_without_charset(term: &str) -> DicomFile {
        let mut dataset = radicom::model::dataset::Dataset::new();
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_CLASS_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.840.10008.5.1.4.1.1.7",
        );
        dataset.insert_str(
            radicom::model::tag::Tag::SOP_INSTANCE_UID,
            radicom::model::vr::Vr::Ui,
            "1.2.3",
        );
        dataset.insert_str(
            radicom::model::tag::Tag::PATIENT_NAME,
            radicom::model::vr::Vr::Pn,
            "Müller",
        );
        dataset.set_transfer_syntax(
            radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        dataset.set_fallback_encodings(vec![radicom::encoding::charset::charset_encoding(term)]);
        DicomFile::from_dataset(dataset)
    }

    #[tokio::test]
    async fn a_store_without_a_charset_decodes_with_the_servers_fallback() {
        // H1: a peer that omits (0008,0005) declares ISO-IR 6, but a legacy
        // Latin-1 instance is common. `ServerOptions::fallback_encoding` makes
        // the SCP decode that text as Latin-1. Nothing is re-decoded on the send
        // path: the writer takes its encoding from the dataset's own fallback
        // list, so the bytes on the wire are the instance's repertoire.
        let latin1 = instance_without_charset("ISO_IR 100");
        assert_eq!(
            store_and_read_name(Some("ISO_IR 100"), &latin1).await,
            "Müller",
            "configured ISO_IR 100 must decode the Latin-1 bytes"
        );
        // radicom resolves the ISO_IR 6 label to windows-1252 (the WHATWG label
        // table maps the ASCII and Latin-1 labels there), so the default
        // repertoire already preserves these bytes: `None` must not change that.
        assert_eq!(
            store_and_read_name(None, &latin1).await,
            "Müller",
            "the default repertoire stays the default"
        );

        // A repertoire that is *not* the default proves the option decides the
        // decode: the same text as UTF-8, still without (0008,0005).
        let utf8 = instance_without_charset("ISO_IR 192");
        assert_eq!(
            store_and_read_name(Some("ISO_IR 192"), &utf8).await,
            "Müller",
            "configured ISO_IR 192 must decode the UTF-8 bytes"
        );
        assert_eq!(
            store_and_read_name(None, &utf8).await,
            "MÃ¼ller",
            "without a fallback the default repertoire reads UTF-8 as single bytes"
        );
    }

    #[tokio::test]
    async fn empty_batches_short_circuit_without_connecting() {
        // An empty batch never opens an association and returns no responses.
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        assert!(client.send_c_store_batch(&[]).await.unwrap().is_empty());
        assert!(client.send_c_find_batch(&[]).await.unwrap().is_empty());
    }
}
