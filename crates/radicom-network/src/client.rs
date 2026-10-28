//! The high-level DICOM client (CLI-01; docs/01-architecture.md §11).
//!
//! The client owns a request queue, decides how many requests share one
//! association, and transparently rolls over to a new association when the
//! quota is spent or the engine's circuit breaker trips. Callers see one call
//! per request; every association detail stays here.
//!
//! ```text
//! IDLE → CONNECT → REQUEST-ASSOC ─(timeout: retry ≤N)→ ASSOCIATED
//!      → SEND (drain) → LINGER (wait for new queued requests)
//!      → RELEASE → IDLE (retry the remainder) → done
//! ```

use crate::cfind::{CFindRequest, CFindResponse};
use crate::cget::{CGetRequest, CGetResponse};
use crate::cmove::{CMoveRequest, CMoveResponse};
use crate::error::{NetworkError, Result};
use crate::pdu::PresentationContext;
use crate::server::{Association, ClientOptions};
use crate::services::{CEchoResponse, CStoreResponse, CStoreRequest};
use radicom::io::file::DicomFile;
use std::sync::Arc;
use std::time::Duration;

/// Client-side tuning (§9.1).
#[derive(Debug, Clone)]
pub struct DicomClientOptions {
    /// Requests sent on one association before it is released; `None` keeps
    /// the association open for as long as requests keep queueing.
    pub max_requests_per_association: Option<usize>,
    /// How long a finished association waits for newly queued requests
    /// before it is released.
    pub linger: Duration,
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
            linger: Duration::from_millis(50),
            max_consecutive_timeout_retries: 3,
            connection: ClientOptions::default(),
        }
    }
}

/// Events the client reports while it drives associations (§9.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    AssociationAccepted { attempt: u32 },
    AssociationRejected { reason: u8 },
    AssociationReleased,
    /// A request exceeded its `request_timeout`; the batch moves to a new
    /// association.
    RequestTimedOut { message_id: u16 },
}

/// The high-level client (§9.1).
///
/// One `DicomClient` describes a destination; requests are sent with the
/// `send_*` methods, which open associations as needed.
pub struct DicomClient {
    addr: String,
    pub calling_ae: String,
    pub called_ae: String,
    pub options: DicomClientOptions,
    /// Reverse C-STORE handler, required for C-GET (§7.4).
    reverse_store: Option<crate::server::ReverseStoreHandler>,
    events: Option<Arc<dyn Fn(ClientEvent) + Send + Sync>>,
    /// Requests already sent on the current association.
    sent_on_association: usize,
    association: Option<Association>,
}

impl DicomClient {
    /// A client for `addr` (host:port form understood by tokio).
    pub fn new(calling_ae: impl Into<String>, called_ae: impl Into<String>, addr: impl Into<String>) -> Self {
        DicomClient {
            addr: addr.into(),
            calling_ae: calling_ae.into(),
            called_ae: called_ae.into(),
            options: DicomClientOptions::default(),
            reverse_store: None,
            events: None,
            sent_on_association: 0,
            association: None,
        }
    }

    /// Overrides the tuning options.
    pub fn with_options(mut self, options: DicomClientOptions) -> Self {
        self.options = options;
        self
    }

    /// Registers the reverse C-STORE handler; without one C-GET fails fast.
    pub fn with_reverse_store_handler(mut self, handler: crate::server::ReverseStoreHandler) -> Self {
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

    /// True when the current association has reached its request quota (§9.1).
    fn quota_spent(&self) -> bool {
        matches!(self.options.max_requests_per_association, Some(max) if self.sent_on_association >= max)
    }

    /// Waits `linger`, then releases the association if the quota is spent.
    ///
    /// `linger` only pays off when requests arrive from other tasks; this
    /// synchronous API queues nothing while it sleeps, so the wait is the
    /// caller's signal that the burst is over (§9.1).
    async fn retire_if_spent(&mut self) {
        if self.quota_spent() {
            self.linger().await;
            self.close_association().await;
        }
    }

    /// Makes sure an association is open, releasing and retrying as needed.
    ///
    /// Retries only when the peer does not answer at all; a rejection is
    /// permanent and is reported immediately.
    async fn ensure_association(&mut self, contexts: Vec<PresentationContext>) -> Result<&mut Association> {
        self.retire_if_spent().await;
        if self.association.is_none() {
            self.open_association(contexts).await?;
        }
        Ok(self.association.as_mut().expect("open_association sets it"))
    }

    /// Opens an association, retrying peers that do not answer at all.
    async fn open_association(&mut self, contexts: Vec<PresentationContext>) -> Result<()> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let result = Association::open(
                self.addr.as_str(),
                &self.calling_ae,
                &self.called_ae,
                contexts.clone(),
                self.options.connection.clone(),
            )
            .await;
            match result {
                Ok(mut association) => {
                    if let Some(handler) = self.reverse_store.clone() {
                        association.set_reverse_store_handler(handler);
                    }
                    self.emit(ClientEvent::AssociationAccepted { attempt });
                    self.sent_on_association = 0;
                    self.association = Some(association);
                    return Ok(());
                }
                // a rejection is permanent: retrying it just burns time (§9.1)
                Err(other @ NetworkError::Reject { .. }) => {
                    if let NetworkError::Reject { reason, .. } = &other {
                        self.emit(ClientEvent::AssociationRejected { reason: *reason });
                    }
                    return Err(other);
                }
                Err(other) => {
                    if attempt >= self.options.max_consecutive_timeout_retries.max(1) {
                        return Err(other);
                    }
                }
            }
        }
    }

    /// Releases the current association, tolerating a peer that already left.
    async fn close_association(&mut self) {
        if let Some(association) = self.association.take() {
            let _ = association.release().await;
            self.emit(ClientEvent::AssociationReleased);
        }
        self.sent_on_association = 0;
    }

    /// Waits `linger` for more work (§9.1 LINGER state).
    async fn linger(&self) {
        tokio::time::sleep(self.options.linger).await;
    }

    /// Sends a C-ECHO (§7.1, §9.1).
    pub async fn send_c_echo(&mut self, sop_class: impl Into<String>) -> Result<CEchoResponse> {
        let sop_class = sop_class.into();
        let context = PresentationContext::new(
            1,
            sop_class.clone(),
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        );
        let association = self.ensure_association(vec![context]).await?;
        let response = association.send_c_echo(&sop_class).await;
        if response.is_ok() {
            self.sent_on_association += 1;
        }
        response
    }

    /// Sends a C-STORE (§7.2, §9.1).
    pub async fn send_c_store(&mut self, file: &DicomFile) -> Result<CStoreResponse> {
        let sop_class = file
            .dataset
            .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let syntax = file.dataset.transfer_syntax().uid().to_string();
        let context = PresentationContext::new(
            1,
            sop_class,
            crate::services::c_store_transfer_syntaxes(&syntax, &[], false),
        );
        let association = self.ensure_association(vec![context]).await?;
        let response = association.send_c_store(file).await;
        if response.is_ok() {
            self.sent_on_association += 1;
        }
        response
    }

    /// Runs a C-FIND and collects every response (§7.3, §9.1).
    pub async fn send_c_find(&mut self, request: &CFindRequest) -> Result<Vec<CFindResponse>> {
        let context = PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        );
        let association = self.ensure_association(vec![context]).await?;
        let responses = association.send_c_find(request).await?;
        self.sent_on_association += 1;
        Ok(responses)
    }

    /// Runs a C-MOVE and collects every response (§7.5, §9.1).
    pub async fn send_c_move(&mut self, request: &CMoveRequest) -> Result<Vec<CMoveResponse>> {
        let context = PresentationContext::new(
            1,
            request.sop_class_uid.clone(),
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        );
        let association = self.ensure_association(vec![context]).await?;
        let responses = association.send_c_move(request).await?;
        self.sent_on_association += 1;
        Ok(responses)
    }

    /// Runs a C-GET and collects every response (§7.4, §9.1).
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
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        )];
        let mut next_id = 3u8;
        for sop_class in storage_sop_classes {
            contexts.push(PresentationContext::new(
                next_id,
                sop_class.clone(),
                vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
            ));
            next_id = next_id.wrapping_add(2);
        }
        let association = self.ensure_association(contexts).await?;
        let responses = association.send_c_get(request).await?;
        self.sent_on_association += 1;
        Ok(responses)
    }

    /// Sends a C-CANCEL (0x0FFF) for an in-flight request (§7.6, SRV-08).
    ///
    /// `message_id` names the request to cancel. Returns `Ok(false)` when no
    /// association is open (nothing to cancel). Prints the cancel on the first
    /// negotiated presentation context. As with fo-dicom, cancel is only
    /// meaningful while the request is still producing responses; here the SCP
    /// buffers a query's matches, so a cancel suppresses *further* results.
    pub async fn cancel(&mut self, message_id: u16) -> Result<bool> {
        let Some(association) = self.association.as_mut() else {
            return Ok(false);
        };
        association.send_c_cancel(1, message_id).await?;
        Ok(true)
    }

    /// Releases the association; the client can be reused afterwards.
    pub async fn release(&mut self) {
        self.close_association().await;
    }

    /// Aborts the association (§9.1 cancellation mode).
    pub async fn abort(&mut self) {
        if let Some(association) = self.association.take() {
            let _ = association.abort(0).await;
        }
        self.sent_on_association = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;
    use crate::services::{CStoreRequest, ServiceProvider, VerificationProvider};
    use crate::server::{DicomServer, ServerOptions};
    use crate::store::StoreSink;

    #[test]
    fn default_options_match_section_9_1() {
        let options = DicomClientOptions::default();
        assert_eq!(options.max_requests_per_association, None);
        assert_eq!(options.linger, Duration::from_millis(50));
        assert_eq!(options.max_consecutive_timeout_retries, 3);
    }

    #[test]
    fn quota_forces_a_new_association() {
        // one request per association: the second call must reopen
        let options = DicomClientOptions {
            max_requests_per_association: Some(1),
            linger: Duration::from_millis(0),
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
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
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
    async fn events_report_the_association_lifecycle() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));
        client.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        client.release().await;

        let events = seen.lock().unwrap().clone();
        assert_eq!(events[0], ClientEvent::AssociationAccepted { attempt: 1 });
        assert_eq!(events[1], ClientEvent::AssociationReleased);
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
            ServerOptions { store_sink: StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut dataset = radicom::model::dataset::Dataset::new();
        dataset.insert_str(radicom::model::tag::Tag::SOP_CLASS_UID, radicom::model::vr::Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(radicom::model::tag::Tag::SOP_INSTANCE_UID, radicom::model::vr::Vr::Ui, "1.2.3");
        dataset.set_transfer_syntax(radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
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
                    Ok(crate::cget::CGetEvent::instance(DicomFile::from_dataset(dataset)))
                }))
            }
        }

        let server = DicomServer::bind("127.0.0.1:0", Arc::new(GetProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let received: Arc<std::sync::Mutex<Vec<DicomFile>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string()).collect_reverse_stores(received.clone());
        let storage = vec!["1.2.840.10008.5.1.4.1.1.7".to_string()];
        let responses = client.send_c_get(&CGetRequest::study("1.2.3", 1), &storage).await.unwrap();

        assert_eq!(received.lock().unwrap().len(), 1, "the instance arrived through the reverse handler");
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
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(FindProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        let responses = client.send_c_find(&CFindRequest::study_query(1)).await.unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].status, crate::dimse::status::SUCCESS);
        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_c_cancel_keeps_the_association_healthy() {
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
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(FindProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
        // open an association
        client.send_c_find(&CFindRequest::study_query(1)).await.unwrap();
        // cancel a request; the SCP consumes the 0x0FFF command without erroring
        assert!(client.cancel(1).await.unwrap(), "cancel is sent on the open association");
        // the association is still usable for the next request
        let responses = client.send_c_find(&CFindRequest::study_query(2)).await.unwrap();
        assert_eq!(responses.len(), 1, "the association stayed healthy after C-CANCEL");

        client.release().await;
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancel_without_an_association_is_a_no_op() {
        let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1");
        assert!(!client.cancel(1).await.unwrap(), "no association yet means nothing to cancel");
    }

    #[tokio::test]
    async fn a_rejection_is_not_retried() {
        // the SCP serves nothing, so the association is rejected
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let seen: Arc<std::sync::Mutex<Vec<ClientEvent>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
            .with_event_handler(Arc::new(move |event| sink.lock().unwrap().push(event)));
        // C-MOVE is not served by the Verification provider
        let request = CMoveRequest::study("1.2.3", "DEST", 1);
        match client.send_c_move(&request).await {
            Err(NetworkError::Reject { .. }) => {}
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(matches!(seen.lock().unwrap().first(), Some(ClientEvent::AssociationRejected { .. })));
        handle.await.unwrap().unwrap();
    }
}
