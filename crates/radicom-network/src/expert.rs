//! The expert (advanced) client layer (CLI-02; docs/01-architecture.md §11).
//!
//! One [`Connection`] holds one TCP/TLS connection and at most one open
//! association — the rule the wire itself imposes: a second A-ASSOCIATE-RQ on
//! the same connection is a protocol violation. An association sends requests
//! and hands back a stream of responses, so a caller that wants incremental
//! delivery never has to wait for the whole answer set.
//!
//! The high-level [`crate::client::DicomClient`] is built on the same
//! primitives; this module exists for callers that need to drive associations
//! by hand (custom negotiation, per-connection event handling).

use crate::cfind::CFindResponse;
use crate::cget::CGetResponse;
use crate::cmove::CMoveResponse;
use crate::dimse::CommandField;
use crate::error::{NetworkError, Result};
use crate::pdu::PresentationContext;
use crate::server::{Association as WireAssociation, ClientOptions};

/// One connection-level event for the expert API (§11.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected {
        remote: String,
    },
    AssociationOpened,
    AssociationClosed,
    /// The association was aborted; the connection is finished.
    Aborted {
        reason: u8,
    },
}

/// The expert connection: one socket, one association at a time (§11.4).
pub struct Connection {
    remote: String,
    calling_ae: String,
    called_ae: String,
    options: ClientOptions,
    /// The open association, if any. Holding it here enforces the invariant.
    association: Option<WireAssociation>,
    events: Vec<ConnectionEvent>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("remote", &self.remote)
            .field("open", &self.association.is_some())
            .finish()
    }
}

impl Connection {
    /// Describes a connection; nothing is opened until `open_association`.
    pub fn new(
        calling_ae: impl Into<String>,
        called_ae: impl Into<String>,
        remote: impl Into<String>,
    ) -> Self {
        Connection {
            remote: remote.into(),
            calling_ae: calling_ae.into(),
            called_ae: called_ae.into(),
            options: ClientOptions::default(),
            association: None,
            events: Vec::new(),
        }
    }

    /// Overrides the transport options (max PDU, TLS, identity, extended
    /// negotiation).
    pub fn with_options(mut self, options: ClientOptions) -> Self {
        self.options = options;
        self
    }

    /// True when an association is open.
    pub fn is_associated(&self) -> bool {
        self.association.is_some()
    }

    /// The remote address this connection targets.
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// The events observed so far, oldest first (§11.4 event stream).
    pub fn events(&self) -> &[ConnectionEvent] {
        &self.events
    }

    /// Opens the association (§11.4).
    ///
    /// Returns `AlreadyAssociated` when one is still open: each connection
    /// carries exactly one association at a time.
    pub async fn open_association(
        &mut self,
        presentation_contexts: Vec<PresentationContext>,
    ) -> Result<()> {
        if self.association.is_some() {
            return Err(NetworkError::protocol(
                "this connection already has an open association",
            ));
        }
        let association = WireAssociation::open(
            self.remote.as_str(),
            &self.calling_ae,
            &self.called_ae,
            presentation_contexts,
            self.options.clone(),
        )
        .await?;
        self.association = Some(association);
        self.events.push(ConnectionEvent::Connected {
            remote: self.remote.clone(),
        });
        self.events.push(ConnectionEvent::AssociationOpened);
        Ok(())
    }

    /// The open association, or `NoOpenAssociation`.
    pub fn association(&mut self) -> Result<&mut WireAssociation> {
        self.association.as_mut().ok_or(NetworkError::protocol(
            "no open association on this connection",
        ))
    }

    /// Releases the association gracefully (§11.4).
    pub async fn release(&mut self) -> Result<()> {
        let association = self.association.take().ok_or(NetworkError::protocol(
            "no open association on this connection",
        ))?;
        let result = association.release().await;
        self.events.push(ConnectionEvent::AssociationClosed);
        result
    }

    /// Aborts the association; the connection cannot be reused afterwards.
    pub async fn abort(&mut self, reason: u8) -> Result<()> {
        let association = self.association.take().ok_or(NetworkError::protocol(
            "no open association on this connection",
        ))?;
        let result = association.abort(reason).await;
        self.events.push(ConnectionEvent::Aborted { reason });
        result
    }
}

/// A response stream handle (§11.4).
///
/// The expert layer returns responses one at a time; this small cursor keeps
/// the "stream" shape without pulling every response into memory twice. It is
/// a plain iterator, so `for` and adapters work on it directly.
#[derive(Debug, Clone)]
pub struct ResponseStream<T> {
    items: std::vec::IntoIter<T>,
}

impl<T> ResponseStream<T> {
    pub fn from(items: Vec<T>) -> Self {
        ResponseStream {
            items: items.into_iter(),
        }
    }

    /// Responses not yet handed out.
    pub fn remaining(&self) -> usize {
        self.items.len()
    }

    /// True when every response has been handed out.
    pub fn is_drained(&self) -> bool {
        self.remaining() == 0
    }
}

impl<T> Iterator for ResponseStream<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.items.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.items.size_hint()
    }
}

/// The request kinds the expert layer can send on an open association (§11.4).
#[derive(Debug, Clone)]
pub enum ExpertRequest {
    CFind(crate::cfind::CFindRequest),
    CMove(crate::cmove::CMoveRequest),
    CGet(crate::cget::CGetRequest),
}

/// What one expert request produced (§11.4).
#[derive(Debug, Clone)]
pub enum ExpertResponse {
    CFind(Box<CFindResponse>),
    CMove(Box<CMoveResponse>),
    CGet(Box<CGetResponse>),
}

impl ExpertResponse {
    /// True when the peer will send more responses for this request.
    pub fn is_pending(&self) -> bool {
        match self {
            ExpertResponse::CFind(r) => r.is_pending(),
            ExpertResponse::CMove(r) => r.is_pending(),
            ExpertResponse::CGet(r) => r.is_pending(),
        }
    }
}

/// Sends one request on an open association and returns its response stream
/// (§11.4).
pub async fn send(
    association: &mut WireAssociation,
    request: ExpertRequest,
) -> Result<ResponseStream<ExpertResponse>> {
    match request {
        ExpertRequest::CFind(request) => {
            let responses = association.send_c_find(&request).await?;
            Ok(ResponseStream::from(
                responses
                    .into_iter()
                    .map(|r| ExpertResponse::CFind(Box::new(r)))
                    .collect(),
            ))
        }
        ExpertRequest::CMove(request) => {
            let responses = association.send_c_move(&request).await?;
            Ok(ResponseStream::from(
                responses
                    .into_iter()
                    .map(|r| ExpertResponse::CMove(Box::new(r)))
                    .collect(),
            ))
        }
        ExpertRequest::CGet(request) => {
            let responses = association.send_c_get(&request).await?;
            Ok(ResponseStream::from(
                responses
                    .into_iter()
                    .map(|r| ExpertResponse::CGet(Box::new(r)))
                    .collect(),
            ))
        }
    }
}

/// Sends one request and forwards each response to `on_response` as it arrives,
/// without collecting the whole answer set (§11.4). Use [`send`] when you want
/// the responses buffered into an iterator instead.
pub async fn send_streaming<F>(
    association: &mut WireAssociation,
    request: ExpertRequest,
    mut on_response: F,
) -> Result<()>
where
    F: FnMut(ExpertResponse) -> std::io::Result<()>,
{
    match request {
        ExpertRequest::CFind(request) => {
            association
                .send_c_find_stream(&request, |response| {
                    on_response(ExpertResponse::CFind(Box::new(response)))
                })
                .await
        }
        ExpertRequest::CMove(request) => {
            association
                .send_c_move_stream(&request, |response| {
                    on_response(ExpertResponse::CMove(Box::new(response)))
                })
                .await
        }
        ExpertRequest::CGet(request) => {
            association
                .send_c_get_stream(&request, |response| {
                    on_response(ExpertResponse::CGet(Box::new(response)))
                })
                .await
        }
    }
}

/// The response command field a request kind expects (§11.3).
pub fn expected_response_field(kind: &ExpertRequest) -> CommandField {
    match kind {
        ExpertRequest::CFind(_) => CommandField::CFindRsp,
        ExpertRequest::CMove(_) => CommandField::CMoveRsp,
        ExpertRequest::CGet(_) => CommandField::CGetRsp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;
    use crate::server::{DicomServer, ServerOptions};
    use crate::services::VerificationProvider;
    use std::sync::Arc;

    /// A C-FIND SCP that returns two pending matches and a success.
    struct TwoStudyProvider;

    #[async_trait::async_trait]
    impl crate::services::ServiceProvider for TwoStudyProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
        }

        fn on_c_find<'a>(
            &'a self,
            request: &'a crate::cfind::CFindRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
            use futures::StreamExt;
            let make = |uid: &str| {
                let mut item = radicom::model::dataset::Dataset::new();
                item.insert_str(
                    radicom::model::tag::Tag::STUDY_INSTANCE_UID,
                    radicom::model::vr::Vr::Ui,
                    uid,
                );
                Ok(crate::cfind::CFindResponse::pending(request, item))
            };
            let items = vec![make("1.2.3"), make("1.2.4")];
            futures::stream::iter(items)
                .chain(futures::stream::once(async move {
                    Ok(crate::cfind::CFindResponse::success(request, 2))
                }))
                .boxed()
        }
    }

    fn find_context() -> PresentationContext {
        PresentationContext::new(
            1,
            sop_class::STUDY_ROOT_QR_FIND,
            vec!["1.2.840.10008.1.2".to_string()],
        )
    }

    fn echo_context() -> PresentationContext {
        PresentationContext::new(
            1,
            sop_class::VERIFICATION,
            vec!["1.2.840.10008.1.2".to_string()],
        )
    }

    #[test]
    fn a_fresh_connection_has_no_association() {
        let connection = Connection::new("SCU", "SCP", "127.0.0.1:1");
        assert!(!connection.is_associated());
        assert!(connection.events().is_empty());
        assert_eq!(connection.remote(), "127.0.0.1:1");
    }

    #[tokio::test]
    async fn sending_without_an_association_is_an_error() {
        let mut connection = Connection::new("SCU", "SCP", "127.0.0.1:1");
        assert!(connection.association().is_err());
        assert!(connection.release().await.is_err());
        assert!(connection.abort(0).await.is_err());
    }

    #[test]
    fn response_stream_hands_out_items_in_order() {
        let mut stream = ResponseStream::from(vec![1, 2, 3]);
        assert_eq!(stream.remaining(), 3);
        assert_eq!(stream.next(), Some(1));
        assert_eq!(stream.next(), Some(2));
        assert_eq!(stream.next(), Some(3));
        assert_eq!(stream.next(), None);
        assert!(stream.is_drained());
        assert_eq!(
            ResponseStream::from(vec![1, 2]).count(),
            2,
            "usable as an iterator"
        );
    }

    #[tokio::test]
    async fn send_streaming_forwards_each_response_without_buffering() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(TwoStudyProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection
            .open_association(vec![find_context()])
            .await
            .unwrap();
        let association = connection.association().unwrap();
        let request = ExpertRequest::CFind(crate::cfind::CFindRequest::study_query(1));

        let (mut total, mut pending) = (0, 0);
        send_streaming(association, request, |response| {
            total += 1;
            if response.is_pending() {
                pending += 1;
            }
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(total, 3, "two pending matches plus the final success");
        assert_eq!(pending, 2);
        connection.release().await.unwrap();
    }

    #[test]
    fn expected_response_fields_match_the_request() {
        let find = ExpertRequest::CFind(crate::cfind::CFindRequest::study_query(1));
        assert_eq!(expected_response_field(&find), CommandField::CFindRsp);
        let mv = ExpertRequest::CMove(crate::cmove::CMoveRequest::study("1.2.3", "DEST", 1));
        assert_eq!(expected_response_field(&mv), CommandField::CMoveRsp);
        let get = ExpertRequest::CGet(crate::cget::CGetRequest::study("1.2.3", 1));
        assert_eq!(expected_response_field(&get), CommandField::CGetRsp);
    }

    #[tokio::test]
    async fn one_association_per_connection_is_enforced() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection
            .open_association(vec![echo_context()])
            .await
            .unwrap();
        assert!(connection.is_associated());

        // a second association on the same connection is refused locally
        match connection.open_association(vec![echo_context()]).await {
            Err(NetworkError::Protocol(message)) => {
                assert!(message.contains("already"), "got {message}")
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }

        connection.release().await.unwrap();
        assert!(!connection.is_associated());
        assert_eq!(
            connection.events(),
            &[
                ConnectionEvent::Connected {
                    remote: addr.to_string()
                },
                ConnectionEvent::AssociationOpened,
                ConnectionEvent::AssociationClosed
            ]
        );
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_connection_can_be_reused_after_release() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection
            .open_association(vec![echo_context()])
            .await
            .unwrap();
        connection.release().await.unwrap();
        // the rule is one association *at a time*, not one per lifetime
        connection
            .open_association(vec![echo_context()])
            .await
            .unwrap();
        let association = connection.association().unwrap();
        let response = association
            .send_c_echo(sop_class::VERIFICATION)
            .await
            .unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        connection.release().await.unwrap();
    }

    #[tokio::test]
    async fn aborting_is_recorded_and_ends_the_association() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection
            .open_association(vec![echo_context()])
            .await
            .unwrap();
        connection.abort(3).await.unwrap();
        assert!(!connection.is_associated());
        assert_eq!(
            connection.events().last(),
            Some(&ConnectionEvent::Aborted { reason: 3 })
        );
        // the server reports the peer's abort instead of a clean close
        match handle.await.unwrap() {
            Err(NetworkError::Abort {
                source_code,
                reason,
            }) => assert_eq!((source_code, reason), (0, 3)),
            other => panic!("expected an abort error, got {other:?}"),
        }
    }

    #[test]
    fn debug_and_options_describe_the_connection() {
        let plain = Connection::new("SCU", "SCP", "10.0.0.1:104");
        assert!(format!("{plain:?}").contains("10.0.0.1:104"));
        assert!(format!("{plain:?}").contains("open: false"));

        let options = ClientOptions {
            max_pdu_length: 8192,
            ..ClientOptions::default()
        };
        let tuned = Connection::new("SCU", "SCP", "10.0.0.2:104").with_options(options);
        assert_eq!(tuned.options.max_pdu_length, 8192);
    }

    /// A C-MOVE SCP that completes immediately, and a C-GET SCP that finishes
    /// with a failure status: enough to drive both non-C-FIND expert arms.
    struct MoveProvider;
    struct GetProvider;

    #[async_trait::async_trait]
    impl crate::services::ServiceProvider for MoveProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
        }

        fn on_c_move<'a>(
            &'a self,
            request: &'a crate::cmove::CMoveRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
            Box::pin(futures::stream::iter(vec![Ok(
                crate::cmove::CMoveResponse::completed(
                    request,
                    crate::cmove::SubOperationSummary::default(),
                ),
            )]))
        }
    }

    #[async_trait::async_trait]
    impl crate::services::ServiceProvider for GetProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_GET.to_string()]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            Box::pin(futures::stream::iter(vec![Ok(
                crate::cget::CGetEvent::Response(crate::cget::CGetResponse::completed(
                    request,
                    crate::cmove::SubOperationSummary::default(),
                )),
            )]))
        }
    }

    async fn expert_round_trip<P: crate::services::ServiceProvider + 'static>(
        provider: P,
        context: PresentationContext,
        request: ExpertRequest,
    ) -> Vec<ExpertResponse> {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(provider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection.open_association(vec![context]).await.unwrap();
        let association = connection.association().unwrap();
        // C-MOVE and C-GET deliver their instances as reverse C-STOREs
        association.collect_reverse_stores(Arc::new(std::sync::Mutex::new(Vec::new())));
        let responses = send(association, request).await.unwrap().collect();
        connection.release().await.unwrap();
        handle.await.unwrap().unwrap();
        responses
    }

    #[tokio::test]
    async fn send_returns_a_stream_for_move_and_get() {
        let move_responses = expert_round_trip(
            MoveProvider,
            PresentationContext::new(
                1,
                sop_class::STUDY_ROOT_QR_MOVE,
                vec!["1.2.840.10008.1.2".to_string()],
            ),
            ExpertRequest::CMove(crate::cmove::CMoveRequest::study("1.2.3", "DEST", 1)),
        )
        .await;
        assert_eq!(move_responses.len(), 1);
        assert!(!move_responses[0].is_pending());
        assert!(matches!(move_responses[0], ExpertResponse::CMove(_)));

        let get_responses = expert_round_trip(
            GetProvider,
            PresentationContext::new(
                1,
                sop_class::STUDY_ROOT_QR_GET,
                vec!["1.2.840.10008.1.2".to_string()],
            ),
            ExpertRequest::CGet(crate::cget::CGetRequest::study("1.2.3", 1)),
        )
        .await;
        assert_eq!(get_responses.len(), 1);
        assert!(matches!(get_responses[0], ExpertResponse::CGet(_)));
        assert!(!get_responses[0].is_pending());

        // the C-FIND arm of `send` collects the whole stream
        let find_responses = expert_round_trip(
            TwoStudyProvider,
            find_context(),
            ExpertRequest::CFind(crate::cfind::CFindRequest::study_query(1)),
        )
        .await;
        assert_eq!(find_responses.len(), 3);
        assert!(matches!(find_responses[0], ExpertResponse::CFind(_)));
        assert!(find_responses[0].is_pending());
        assert!(!find_responses.last().unwrap().is_pending());
    }

    #[tokio::test]
    async fn send_streaming_handles_move_and_get() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(MoveProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection
            .open_association(vec![PresentationContext::new(
                1,
                sop_class::STUDY_ROOT_QR_MOVE,
                vec!["1.2.840.10008.1.2".to_string()],
            )])
            .await
            .unwrap();
        let mut total = 0;
        let association = connection.association().unwrap();
        association.collect_reverse_stores(Arc::new(std::sync::Mutex::new(Vec::new())));
        send_streaming(
            association,
            ExpertRequest::CMove(crate::cmove::CMoveRequest::study("1.2.3", "DEST", 1)),
            |_| {
                total += 1;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        connection.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn send_streaming_handles_get() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(GetProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut connection = Connection::new("SCU", "SCP", addr.to_string());
        connection
            .open_association(vec![PresentationContext::new(
                1,
                sop_class::STUDY_ROOT_QR_GET,
                vec!["1.2.840.10008.1.2".to_string()],
            )])
            .await
            .unwrap();
        let association = connection.association().unwrap();
        association.collect_reverse_stores(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut total = 0;
        send_streaming(
            association,
            ExpertRequest::CGet(crate::cget::CGetRequest::study("1.2.3", 1)),
            |_| {
                total += 1;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        connection.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }
}
