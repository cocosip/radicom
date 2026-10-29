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
    events: Option<Arc<dyn Fn(ClientEvent) + Send + Sync>>,
    /// Requests already sent on the current association.
    sent_on_association: usize,
    /// The message ID and presentation-context ID of the last request, so a
    /// C-CANCEL travels on the context that request used.
    last_request: Option<(u16, u8)>,
    association: Option<Association>,
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
            events: None,
            sent_on_association: 0,
            last_request: None,
            association: None,
        }
    }

    /// Overrides the tuning options.
    pub fn with_options(mut self, options: DicomClientOptions) -> Self {
        self.options = options;
        self
    }

    /// Registers the reverse C-STORE handler; without one C-GET fails fast.
    pub fn with_reverse_store_handler(
        mut self,
        handler: crate::server::ReverseStoreHandler,
    ) -> Self {
        self.reverse_store = Some(handler);
        self
    }

    /// Collects reverse C-STORE instances into `sink`.
    pub fn collect_reverse_stores(mut self, sink: Arc<std::sync::Mutex<Vec<DicomFile>>>) -> Self {
        self.reverse_store = Some(Arc::new(move |request: &CStoreRequest| {
            sink.lock().unwrap().push(request.file.clone());
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
        if let Err(NetworkError::RequestTimedOut { message_id }) = &result {
            self.emit(ClientEvent::RequestTimedOut {
                message_id: *message_id,
            });
            self.abort().await;
        }
        result
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

    /// Sends a C-ECHO (§11.4).
    pub async fn send_c_echo(&mut self, sop_class: impl Into<String>) -> Result<CEchoResponse> {
        let sop_class = sop_class.into();
        let context = PresentationContext::new(
            1,
            sop_class.clone(),
            vec![
                "1.2.840.10008.1.2".to_string(),
                "1.2.840.10008.1.2.1".to_string(),
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

    /// Runs a C-FIND and collects every response (§11.4).
    pub async fn send_c_find(&mut self, request: &CFindRequest) -> Result<Vec<CFindResponse>> {
        let context = PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec![
                "1.2.840.10008.1.2".to_string(),
                "1.2.840.10008.1.2.1".to_string(),
            ],
        );
        let (context_id, result) = {
            let association = self.ensure_association(vec![context]).await?;
            let context_id = association
                .contexts()
                .pick(&request.sop_class_uid, "")
                .map(|c| c.id)
                .ok();
            let result = association.send_c_find(request).await;
            (context_id, result)
        };
        let responses = self.finish_request(result).await?;
        self.sent_on_association += 1;
        if let Some(context_id) = context_id {
            self.last_request = Some((request.message_id, context_id));
        }
        Ok(responses)
    }

    /// Runs a C-MOVE and collects every response (§11.4).
    pub async fn send_c_move(&mut self, request: &CMoveRequest) -> Result<Vec<CMoveResponse>> {
        let context = PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec![
                "1.2.840.10008.1.2".to_string(),
                "1.2.840.10008.1.2.1".to_string(),
            ],
        );
        let (context_id, result) = {
            let association = self.ensure_association(vec![context]).await?;
            let context_id = association
                .contexts()
                .pick(&request.sop_class_uid, "")
                .map(|c| c.id)
                .ok();
            let result = association.send_c_move(request).await;
            (context_id, result)
        };
        let responses = self.finish_request(result).await?;
        self.sent_on_association += 1;
        if let Some(context_id) = context_id {
            self.last_request = Some((request.message_id, context_id));
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
        let mut contexts = vec![PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec![
                "1.2.840.10008.1.2".to_string(),
                "1.2.840.10008.1.2.1".to_string(),
            ],
        )];
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

        let mut next_id = 3u8;
        for sop_class in storage_sop_classes {
            contexts.push(PresentationContext::new(
                next_id,
                sop_class.clone(),
                vec![
                    "1.2.840.10008.1.2".to_string(),
                    "1.2.840.10008.1.2.1".to_string(),
                ],
            ));
            next_id = next_id.wrapping_add(2);
        }
        let (context_id, result) = {
            let association = self.ensure_association_with_roles(contexts, roles).await?;
            let context_id = association
                .contexts()
                .pick(&request.sop_class_uid, "")
                .map(|c| c.id)
                .ok();
            let result = association.send_c_get(request).await;
            (context_id, result)
        };
        let responses = self.finish_request(result).await?;
        self.sent_on_association += 1;
        if let Some(context_id) = context_id {
            self.last_request = Some((request.message_id, context_id));
        }
        Ok(responses)
    }

    /// Sends a C-CANCEL (0x0FFF) for an in-flight request (§11.4, SRV-08).
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

/// True when the open association accepted every abstract syntax in `wanted`.
fn covers(association: &Association, wanted: &[PresentationContext]) -> bool {
    wanted.iter().all(|context| {
        association
            .contexts()
            .find_abstract(&context.abstract_syntax)
            .is_some()
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
    use crate::services::{CStoreRequest, ServiceProvider, VerificationProvider};
    use crate::store::StoreSink;

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
    async fn c_get_without_a_handler_fails_before_connecting() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        let request = CGetRequest::study("1.2.3", 1);
        match client.send_c_get(&request, &[]).await {
            Err(NetworkError::ReverseStoreHandlerMissing) => {}
            other => panic!("expected ReverseStoreHandlerMissing, got {other:?}"),
        }
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
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));
        client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        client.release().await;

        let events = seen.lock().unwrap().clone();
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
            received.lock().unwrap().len(),
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
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));
        // C-MOVE is not served by the Verification provider
        let request = CMoveRequest::study("1.2.3", "DEST", 1);
        match client.send_c_move(&request).await {
            Err(NetworkError::Reject { .. }) => {}
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(matches!(
            seen.lock().unwrap().first(),
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
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));

        client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        let response = client.send_c_store(&instance()).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);

        let events = seen.lock().unwrap().clone();
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
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));

        let error = client
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NetworkError::AssociationRequestTimedOut { .. }
        ));
        let events = seen.lock().unwrap();
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
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));

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
}
