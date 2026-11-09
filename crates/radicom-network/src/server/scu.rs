//! The DIMSE SCU: Association and its operations.

use super::scp::{
    connect_with_timeout, transcoder_for, transfer_syntax_of, validate_proposed_contexts,
    write_pdata_batch,
};
use super::*;

/// The client-side stream: plain TCP or TLS over TCP.
///
/// An enum keeps [`Association`] non-generic while still letting TLS share the
/// whole protocol path.
pub enum ClientStream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl tokio::io::AsyncRead for ClientStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "tls")]
            ClientStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for ClientStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "tls")]
            ClientStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            #[cfg(feature = "tls")]
            ClientStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ClientStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "tls")]
            ClientStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// An open association from the SCU side (§11.4 expert layer).
pub struct Association {
    pub(crate) stream: crate::observability::CountingStream<ClientStream>,
    contexts: AcceptedContexts,
    pub(crate) splitter: PduSplitter,
    assembler: PdvAssembler,
    /// Requests this side sent and whose response is still outstanding; the
    /// allocator skips their message IDs (§11.2).
    pub(crate) pending: crate::engine::PendingTable,
    request_cancellation: Option<(u8, tokio::sync::watch::Receiver<Option<u16>>)>,
    /// How long [`Self::release`] waits for the peer's A-RELEASE-RP.
    release_timeout: Duration,
    /// Maximum quiet time while waiting for a DIMSE response.
    request_timeout: Option<Duration>,
    /// Answers reverse C-STORE sub-operations; required for C-GET (§11.4).
    reverse_store: Option<ReverseStoreHandler>,
    /// Answers an incoming N-EVENT-REPORT-RQ (H2); `None` leaves it to the
    /// caller, which ignores it as before.
    n_event: Option<NEventReportHandler>,
    /// An incoming N-EVENT-REPORT whose dataset has not arrived yet (H2).
    pending_incoming: Option<(u8, Command)>,
    /// What the peer granted in SOP Class Extended Negotiation (§11.2).
    peer_extended: Vec<crate::pdu::ExtendedNegotiation>,
    /// The peer's user identity response (§11.2).
    peer_identity_response: Option<Vec<u8>>,
    /// Send an untranscodable dataset in its own syntax instead of failing (L3).
    ignore_unsupported_syntax_change: bool,
    /// Fallback charset for received text without (0008,0005) (item 2).
    fallback_encoding: Option<&'static str>,
    max_materialized_length: u64,
    /// Reports this association's connection open/close to the client metrics.
    _connection: crate::observability::ConnectionGuard,
    /// Reports this association's open/close to the client metrics.
    _association: crate::observability::AssociationGuard,
    /// The peer's implementation class UID from its user information (L8).
    peer_implementation_class_uid: Option<String>,
    /// The peer's implementation version name from its user information (L8).
    peer_implementation_version_name: Option<String>,
    async_operations: crate::pdu::AsyncOperations,
    /// Cancel-safe framing for every association read (NET-14): a timed-out or
    /// dropped read keeps its half-frame here instead of resynchronising.
    pdu_reader: crate::transport::PduReader,
    /// Transcoder per (source, target) syntax pair, reused across the many
    /// instances of one C-MOVE batch instead of rebuilt per instance (NET-16).
    transcoder_cache:
        std::collections::HashMap<(String, String), radicom::pixel::transcoder::Transcoder>,
}

/// The SCU-side answer to a reverse C-STORE (§11.4).
pub type ReverseStoreHandler = Arc<
    dyn Fn(&crate::services::CStoreRequest) -> Result<crate::services::CStoreResponse>
        + Send
        + Sync,
>;

/// Answers an incoming N-EVENT-REPORT-RQ on an SCU association (H2).
///
/// Mirrors fo-dicom `DicomClient.OnNEventReportRequest`: the callback returns
/// the response the SCP receives, so a service-class SCP can push events over
/// the SCU's own association. `Err` answers ProcessingFailure with the error as
/// the comment instead of dropping the request.
pub type NEventReportHandler = Arc<
    dyn Fn(&crate::nservice::NServiceRequest) -> Result<crate::nservice::NServiceResponse>
        + Send
        + Sync,
>;

#[derive(Clone)]
struct MixedPending {
    request_index: usize,
    message_id: u16,
    context_id: u8,
    kind: crate::batch::RequestKind,
    expected_field: CommandField,
    expected_sop_class_uid: Option<String>,
    expected_sop_instance_uid: Option<String>,
    syntax: Arc<radicom::model::transfer_syntax::TransferSyntax>,
}

enum MixedDataset {
    Response { message_id: u16, command: Command },
    ReverseStore { command: Command },
}

impl std::fmt::Debug for Association {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Association")
            .field("contexts", &self.contexts.all().len())
            .field("reverse_store", &self.reverse_store.is_some())
            .finish()
    }
}

impl Association {
    /// Feeds a P-DATA-TF to the assembler, moving any spooling work onto the
    /// blocking pool so a reverse C-STORE instance never blocks a runtime worker
    /// (§6, §11.1). Messages that stay in memory are assembled inline.
    async fn push_pdata_offloaded(
        &mut self,
        pdata: &crate::pdu::PData,
    ) -> Result<Vec<crate::assembler::MessageUnit>> {
        if !self.assembler.would_spill_pdata(pdata) {
            return self.assembler.push_pdata(pdata);
        }
        let mut assembler = std::mem::take(&mut self.assembler);
        let owned = pdata.clone();
        match tokio::task::spawn_blocking(move || {
            let result = assembler.push_pdata(&owned);
            (assembler, result)
        })
        .await
        {
            Ok((assembler, result)) => {
                self.assembler = assembler;
                result
            }
            Err(join) => {
                tracing::error!(target: "radicom::network", %join, "assembler blocking task failed");
                Err(NetworkError::protocol("assembler blocking task failed"))
            }
        }
    }

    /// Opens a connection and negotiates an association (§11.4).
    pub async fn open(
        addr: impl tokio::net::ToSocketAddrs,
        calling_ae: &str,
        called_ae: &str,
        presentation_contexts: Vec<PresentationContext>,
        options: ClientOptions,
    ) -> Result<Self> {
        let mut options = options;
        options.max_pdu_length = crate::pdu::clamp_max_pdu_length(options.max_pdu_length);
        crate::pdu::validate_ae_title(calling_ae)?;
        crate::pdu::validate_ae_title(called_ae)?;
        // validate before touching the network: a bad identity must not
        // depend on whether the peer happens to be reachable (§11.2)
        if let Some(identity) = &options.user_identity {
            identity.validate()?;
        }
        validate_proposed_contexts(&presentation_contexts)?;

        let tcp = connect_with_timeout(TcpStream::connect(addr), options.connect_timeout).await?;
        crate::transport::apply_options(&tcp, &options.tcp)?;

        #[cfg(feature = "tls")]
        let stream = match &options.tls {
            // a peer that accepts TCP but never answers the handshake must not
            // hang the connect (review #14)
            Some(tls) => ClientStream::Tls(Box::new(
                tokio::time::timeout(options.tls_handshake_timeout, tls.connect(tcp))
                    .await
                    .map_err(|_| NetworkError::TlsHandshakeTimedOut {
                        timeout_ms: options.tls_handshake_timeout.as_millis() as u64,
                    })??,
            )),
            None => ClientStream::Plain(tcp),
        };
        #[cfg(not(feature = "tls"))]
        let stream = ClientStream::Plain(tcp);

        // L6: count this SCU's bytes the way the SCP already does (fo-dicom
        // instruments `DicomService`, not just the server); the open/close
        // guards live in the association, so they fire exactly once.
        let mut stream = crate::observability::CountingStream::new(stream, options.metrics.clone());

        let mut request = Associate::new(called_ae, calling_ae);
        request.application_context = APPLICATION_CONTEXT.to_string();
        request.presentation_contexts = presentation_contexts;
        let mut user = crate::association::build_user_information(
            options.max_pdu_length,
            IMPLEMENTATION_CLASS_UID,
            IMPLEMENTATION_VERSION_NAME,
            options.role_selections.clone(),
        );
        user.async_operations = Some(options.async_operations);
        user.extended_negotiations = options.extended_negotiations.clone();
        // L4: the item is optional and encoded only when configured; without
        // this the codec's common-extended support was unreachable
        user.common_extended = options.common_extended.clone();
        user.user_identity = options.user_identity.clone();
        request.user_information = user;

        // the AC does not repeat the abstract syntax; keep the RQ around so we
        // can recover it by presentation-context id (§9.3.3)
        let requested_contexts = request.presentation_contexts.clone();
        write_pdu(&mut stream, &Pdu::AssociateRq(request)).await?;

        let response = tokio::time::timeout(options.association_timeout, read_pdu(&mut stream))
            .await
            .map_err(|_| NetworkError::AssociationRequestTimedOut { attempts: 1 })??;
        // filled from the AC; both stay empty on a rejection
        let peer_extended;
        let peer_identity_response;
        let peer_implementation_class_uid;
        let peer_implementation_version_name;
        let async_operations;
        let (accepted, peer_max_pdu) = match response {
            Some(Pdu::AssociateAc(accept)) => {
                if accept.application_context != APPLICATION_CONTEXT {
                    return Err(NetworkError::protocol(
                        "association accept used an unsupported application context",
                    ));
                }
                if accept.protocol_version & 1 == 0 {
                    return Err(NetworkError::protocol(
                        "association accept used an unsupported protocol version",
                    ));
                }
                async_operations = accept.user_information.async_operations.unwrap_or_default();
                options.async_operations.validate_answer(async_operations)?;
                // the AC echoes the roles it granted; they apply to whichever
                // context carries that abstract syntax (review #16)
                let granted_roles = accept.user_information.role_selections;
                if accept.presentation_contexts.len() != requested_contexts.len() {
                    return Err(NetworkError::protocol(
                        "association accept must answer every proposed presentation context",
                    ));
                }
                let mut accepted_ids = std::collections::HashSet::new();
                let negotiated = accept
                    .presentation_contexts
                    .iter()
                    .map(|pc| {
                        // PS3.8 §9.3.3: the AC answers exactly the proposed IDs
                        // and may only select a proposed transfer syntax.
                        if pc.result > 4 {
                            return Err(NetworkError::protocol(
                                "invalid presentation-context result code",
                            ));
                        }
                        let rq = requested_contexts
                            .iter()
                            .find(|rq| rq.id == pc.id)
                            .ok_or_else(|| {
                                NetworkError::protocol(
                                    "association accept names a context that was not proposed",
                                )
                            })?;
                        if !accepted_ids.insert(pc.id) {
                            return Err(NetworkError::protocol(
                                "association accept repeats a presentation context ID",
                            ));
                        }
                        if pc.result == 0 && pc.transfer_syntaxes.len() != 1 {
                            return Err(NetworkError::protocol(
                                "accepted presentation context must carry exactly one transfer syntax",
                            ));
                        }
                        let transfer_syntax =
                            pc.transfer_syntaxes.first().cloned().unwrap_or_default();
                        if pc.result == 0 && !rq.transfer_syntaxes.contains(&transfer_syntax) {
                            return Err(NetworkError::protocol(
                                "association accept chose a transfer syntax that was not proposed",
                            ));
                        }
                        // an A-ASSOCIATE-AC omits the abstract syntax (it is
                        // implied by the presentation-context id), so reject
                        // peers that put one on the wire and recover it from
                        // the request we just sent.
                        if !pc.abstract_syntax.is_empty() {
                            return Err(NetworkError::protocol(
                                "association accept must not carry an abstract syntax",
                            ));
                        }
                        let abstract_syntax = rq.abstract_syntax.clone();
                        let roles = granted_roles
                            .iter()
                            .filter(|role| role.sop_class == abstract_syntax)
                            .cloned()
                            .collect();
                        Ok(crate::association::NegotiatedContext {
                            id: pc.id,
                            abstract_syntax,
                            transfer_syntax,
                            state: crate::association::ContextState::from_result_code(pc.result),
                            roles,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                // the peer's advertised maximum caps what we may send (§11.1);
                // 0 means "unlimited", so fall back to our own value
                let peer = accept.user_information.max_pdu_length;
                let cap = if peer == 0 {
                    options.max_pdu_length
                } else {
                    peer.min(options.max_pdu_length)
                };
                peer_extended = accept.user_information.extended_negotiations;
                peer_identity_response = accept.user_information.user_identity_response;
                // L8: decoded by the codec but previously dropped
                peer_implementation_class_uid = accept.user_information.implementation_class_uid;
                peer_implementation_version_name =
                    accept.user_information.implementation_version_name;
                (AcceptedContexts::from_negotiated(negotiated), cap)
            }
            Some(Pdu::AssociateRj(reject)) => {
                return Err(NetworkError::Reject {
                    result: reject.result.code(),
                    source_code: reject.source.code(),
                    reason: reject.reason,
                })
            }
            _ => {
                return Err(NetworkError::protocol(
                    "expected A-ASSOCIATE-AC or A-ASSOCIATE-RJ",
                ))
            }
        };

        // A positive identity response was required but the peer sent none:
        // silently continuing would authenticate nobody (§11.4).
        if let Some(identity) = &options.user_identity {
            if identity.positive_response_requested && peer_identity_response.is_none() {
                return Err(NetworkError::protocol(
                    "user identity response was required but not returned",
                ));
            }
        }

        Ok(Association {
            stream,
            contexts: accepted,
            splitter: PduSplitter::with_limits(
                peer_max_pdu,
                options.max_pdvs_per_pdu,
                options.max_command_buffer.map(|limit| limit as usize),
                options.max_data_buffer.map(|limit| limit as usize),
            ),
            assembler: match options.reverse_spool.clone() {
                Some(spool) => PdvAssembler::with_spool(spool),
                None => PdvAssembler::new(),
            },
            pending: crate::engine::PendingTable::new(),
            request_cancellation: None,
            release_timeout: options.release_timeout,
            request_timeout: options.request_timeout,
            reverse_store: None,
            n_event: None,
            pending_incoming: None,
            peer_extended,
            peer_identity_response,
            ignore_unsupported_syntax_change: options.ignore_unsupported_syntax_change,
            fallback_encoding: options.fallback_encoding,
            max_materialized_length: options
                .reverse_spool
                .as_ref()
                .map_or(PdvAssembler::DEFAULT_MAX_MESSAGE_LENGTH as u64, |spool| {
                    spool.max_message_length
                }),
            _connection: crate::observability::ConnectionGuard::new(options.metrics.clone()),
            _association: crate::observability::AssociationGuard::new(options.metrics.clone()),
            peer_implementation_class_uid,
            peer_implementation_version_name,
            async_operations,
            // E: accept exactly the Maximum-length-received this side advertised
            pdu_reader: crate::transport::PduReader::with_max_pdu(
                crate::pdu::clamp_max_pdu_length(options.max_pdu_length),
            ),
            transcoder_cache: std::collections::HashMap::new(),
        })
    }

    pub fn set_request_cancellation(
        &mut self,
        context_id: u8,
        receiver: tokio::sync::watch::Receiver<Option<u16>>,
    ) {
        self.request_cancellation = Some((context_id, receiver));
    }

    /// Accepted asynchronous-operation limits, in the requestor's direction.
    pub fn async_operations(&self) -> crate::pdu::AsyncOperations {
        self.async_operations
    }

    /// Sends a batch of C-ECHO requests; responses are returned in request order.
    /// At most 64 requests are outstanding even for an unlimited window.
    /// Other DIMSE services remain sequential on this association.
    pub async fn send_c_echo_batch(
        &mut self,
        requests: &[CEchoRequest],
    ) -> Result<Vec<CEchoResponse>> {
        let mut reserved_ids =
            self.reserve_explicit_ids(requests.iter().map(|request| request.message_id))?;
        let prepared_result = (|| -> Result<_> {
            let mut prepared = Vec::with_capacity(requests.len());
            let mut context_ids = Vec::with_capacity(requests.len());
            for request in requests {
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                    self.pending.insert(request.message_id);
                    reserved_ids.push(request.message_id);
                }
                let context = self.contexts.pick(&request.affected_sop_class_uid, "")?;
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                context_ids.push(context.id);
                prepared.push(request);
            }
            Ok((prepared, context_ids))
        })();
        let (prepared, context_ids) = match prepared_result {
            Ok(value) => value,
            Err(error) => {
                for id in &reserved_ids {
                    self.pending.remove(*id);
                }
                return Err(error);
            }
        };
        // Even a peer granting unlimited operations cannot grow this sender's
        // outstanding table without bound. Other DIMSE service kinds stay serial.
        let window = match self.async_operations.invoked {
            0 => 64,
            finite => usize::from(finite).min(64),
        };
        let mut inflight = std::collections::HashMap::new();
        let mut responses = vec![None; requests.len()];
        let mut next = 0;
        let result = async {
            while next < requests.len() || !inflight.is_empty() {
                let mut outgoing = Vec::new();
                while next < requests.len() && inflight.len() < window {
                    let request = &prepared[next];
                    outgoing.extend(self.splitter.split_bytes(
                        context_ids[next],
                        true,
                        bytes::Bytes::from(command_bytes(&request.to_command())?),
                    )?);
                    inflight.insert(request.message_id, next);
                    self.pending.insert(request.message_id);
                    next += 1;
                }
                if !outgoing.is_empty() {
                    write_pdata_batch(&mut self.stream, outgoing).await?;
                }
                let message_id = *inflight
                    .keys()
                    .next()
                    .expect("at least one request is pending");
                let Some(pdu) = self.read_request_pdu(message_id).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                            // dropping it, before the response filtering below
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            if unit.stream != Stream::Command {
                                return Err(NetworkError::protocol(
                                    "C-ECHO response carries a dataset",
                                ));
                            }
                            let command = command_from_bytes(&unit.bytes)?;
                            let response = CEchoResponse::from_command(&command)?;
                            let id = response.message_id_being_responded_to;
                            let index = inflight.remove(&id).ok_or_else(|| {
                                NetworkError::protocol(
                                    "duplicate or unexpected C-ECHO batch response ID",
                                )
                            })?;
                            if unit.context_id != context_ids[index]
                                || !command.no_dataset
                                || response.state() == StatusState::Pending
                            {
                                return Err(NetworkError::protocol(
                                    "invalid C-ECHO batch response",
                                ));
                            }
                            Self::validate_response_sop_class(
                                &requests[index].affected_sop_class_uid,
                                command.affected_sop_class_uid.as_deref(),
                            )?;
                            self.pending.remove(id);
                            responses[index] = Some(response);
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => return Err(NetworkError::protocol("unexpected PDU during C-ECHO batch")),
                }
            }
            Ok(responses
                .into_iter()
                .map(|response| response.expect("every request completed"))
                .collect())
        }
        .await;
        for id in inflight.keys().copied().chain(reserved_ids.iter().copied()) {
            self.pending.remove(id);
        }
        result
    }

    /// The extended negotiation items the peer accepted (§11.2).
    pub fn accepted_extended_negotiations(&self) -> &[crate::pdu::ExtendedNegotiation] {
        &self.peer_extended
    }

    /// The peer's user identity response, when it sent one (§11.2).
    pub fn user_identity_response(&self) -> Option<&[u8]> {
        self.peer_identity_response.as_deref()
    }

    /// The peer's implementation class UID from its user information (L8).
    ///
    /// fo-dicom `DicomAssociation.RemoteImplementationClassUid`; `None` when the
    /// peer sent no implementation identification item.
    pub fn peer_implementation_class_uid(&self) -> Option<&str> {
        self.peer_implementation_class_uid.as_deref()
    }

    /// The peer's implementation version name from its user information (L8).
    pub fn peer_implementation_version_name(&self) -> Option<&str> {
        self.peer_implementation_version_name.as_deref()
    }

    /// True when the peer granted `field` of `sop_class` (§11.2).
    ///
    /// A field the peer did not answer, or answered with 0, is not granted.
    pub fn extended_field(&self, sop_class: &str, field: u8) -> Option<&[u8]> {
        self.peer_extended
            .iter()
            .find(|ext| ext.sop_class == sop_class)
            .and_then(|ext| {
                let index = usize::from(field.checked_sub(1)?);
                (ext.accepted.get(index).copied()? != 0).then(|| &ext.accepted[index..index + 1])
            })
    }

    /// Registers the reverse C-STORE handler (§11.4).
    ///
    /// C-GET is impossible without one: the instances come back as C-STORE
    /// sub-operations that this side must answer, so [`Self::send_c_get`]
    /// returns `ReverseStoreHandlerMissing` when none is set.
    pub fn set_reverse_store_handler(&mut self, handler: ReverseStoreHandler) {
        self.reverse_store = Some(handler);
    }

    /// Registers the handler for incoming N-EVENT-REPORT-RQ (H2).
    ///
    /// The peer may report events only when this side was granted the SCP role
    /// for the service class; the request is answered on the presentation
    /// context it arrived on. Without a handler such a request stays unhandled,
    /// which is the previous behaviour.
    pub fn set_n_event_report_handler(&mut self, handler: NEventReportHandler) {
        self.n_event = Some(handler);
    }

    /// Sends one message (command plus optional dataset) on `context_id` (H2).
    async fn send_message(
        &mut self,
        context_id: u8,
        command: &Command,
        dataset: Option<&Dataset>,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    ) -> Result<()> {
        let command_bytes =
            bytes::Bytes::from(crate::message::encode_command(&command.to_dataset())?);
        let payload = match dataset {
            Some(dataset) => Some(bytes::Bytes::from(crate::message::encode_dataset(
                dataset, syntax,
            )?)),
            None => None,
        };
        let pdus = self
            .splitter
            .split_message_bytes(context_id, command_bytes, payload)?;
        write_pdata_batch(&mut self.stream, pdus).await
    }

    /// Consumes `unit` when it belongs to an incoming N-EVENT-REPORT-RQ (H2).
    ///
    /// Returns true when the unit was handled: the report goes to the registered
    /// handler and its response is written back on the same presentation
    /// context. A report with no handler is left to the caller, so a client
    /// that never registered one behaves exactly as before.
    async fn intercept_event_report(&mut self, unit: &MessageUnit) -> Result<bool> {
        let Some(handler) = self.n_event.clone() else {
            return Ok(false);
        };
        match unit.stream {
            Stream::Command => {
                let command = command_from_bytes(&unit.bytes)?;
                if command.field != CommandField::NEventReportRq {
                    return Ok(false);
                }
                if self.contexts.by_id(unit.context_id).is_none() {
                    return Err(NetworkError::protocol(
                        "N-EVENT-REPORT on an unaccepted presentation context",
                    ));
                }
                if command.no_dataset {
                    self.answer_event_report(unit.context_id, &handler, &command, None)
                        .await?;
                } else {
                    self.pending_incoming = Some((unit.context_id, command));
                }
                Ok(true)
            }
            Stream::Data => {
                let Some((context_id, command)) = self.pending_incoming.take() else {
                    return Ok(false);
                };
                if context_id != unit.context_id {
                    return Err(NetworkError::protocol(
                        "the N-EVENT-REPORT dataset used another presentation context",
                    ));
                }
                let syntax = transfer_syntax_of(&self.syntax_of(context_id))?;
                let dataset = self.decode_response_body(unit, &syntax).await?;
                self.answer_event_report(context_id, &handler, &command, Some(dataset))
                    .await?;
                Ok(true)
            }
        }
    }

    async fn decode_response_body(
        &self,
        unit: &MessageUnit,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    ) -> Result<Dataset> {
        let body = unit.body();
        let syntax = syntax.clone();
        let fallback = self.fallback_encoding;
        let limit = self.max_materialized_length;
        tokio::task::spawn_blocking(move || {
            crate::store::decode_body_with_fallback_limited(&body, &syntax, fallback, limit)
        })
        .await
        .map_err(|join| {
            NetworkError::Io(std::io::Error::other(format!(
                "response decode task failed: {join}"
            )))
        })?
    }

    /// The accepted transfer syntax of `context_id`, Explicit VR LE as a fallback.
    fn syntax_of(&self, context_id: u8) -> String {
        self.contexts
            .by_id(context_id)
            .map(|context| context.transfer_syntax.clone())
            .unwrap_or_else(|| syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid().to_string())
    }

    /// Writes the handler's answer to an N-EVENT-REPORT-RQ (H2).
    async fn answer_event_report(
        &mut self,
        context_id: u8,
        handler: &NEventReportHandler,
        command: &Command,
        dataset: Option<Dataset>,
    ) -> Result<()> {
        let request = crate::nservice::NServiceRequest::from_command(command, dataset);
        let syntax = transfer_syntax_of(&self.syntax_of(context_id))?;
        let (response_command, response_dataset) = match handler(&request) {
            Ok(response) => (response.to_command(), response.dataset),
            Err(error) => {
                let mut failure =
                    Command::response(command, crate::dimse::status::PROCESSING_FAILURE)
                        .ok_or_else(|| {
                            NetworkError::protocol("N-EVENT-REPORT has no response field")
                        })?;
                failure.error_comment = Some(crate::dimse::truncate_lo(&error.to_string()));
                (failure, None)
            }
        };
        self.send_message(
            context_id,
            &response_command,
            response_dataset.as_ref(),
            &syntax,
        )
        .await
    }

    /// Registers a reverse C-STORE handler that keeps every received instance.
    pub fn collect_reverse_stores(
        &mut self,
        sink: Arc<std::sync::Mutex<Vec<radicom::io::file::DicomFile>>>,
    ) {
        self.reverse_store = Some(Arc::new(move |request: &crate::services::CStoreRequest| {
            sink.lock().unwrap().push(request.file.clone());
            Ok(crate::services::CStoreResponse::success(request))
        }));
    }

    /// Sends heterogeneous DIMSE requests on one association.
    ///
    /// Requests are admitted up to the negotiated asynchronous-operation
    /// window. Responses may arrive in any order and are routed by Message ID;
    /// Pending responses remain associated with their original request until a
    /// terminal status is received.
    pub async fn send_mixed_batch<F>(
        &mut self,
        requests: Vec<crate::batch::DicomRequest>,
        on_response: F,
    ) -> Result<()>
    where
        F: FnMut(crate::batch::DicomResponseEvent) -> std::io::Result<()>,
    {
        self.send_mixed_batch_cancellable(requests, on_response, None)
            .await
    }

    pub(crate) async fn send_mixed_batch_cancellable<F>(
        &mut self,
        requests: Vec<crate::batch::DicomRequest>,
        mut on_response: F,
        cancellation: Option<crate::client::CancellationHandle>,
    ) -> Result<()>
    where
        F: FnMut(crate::batch::DicomResponseEvent) -> std::io::Result<()>,
    {
        if requests.is_empty() {
            return Ok(());
        }
        self.request_cancellation = None;
        if requests
            .iter()
            .any(|request| matches!(request, crate::batch::DicomRequest::Get { .. }))
            && self.reverse_store.is_none()
        {
            return Err(NetworkError::ReverseStoreHandlerMissing);
        }

        let mut explicit_ids = std::collections::HashSet::new();
        for request in &requests {
            let id = request.message_id().unwrap_or(0);
            if id != 0 && !explicit_ids.insert(id) {
                return Err(NetworkError::protocol(
                    "mixed batch message IDs must be unique",
                ));
            }
            if id != 0 && self.pending.contains(id) {
                return Err(NetworkError::protocol(
                    "mixed batch message ID is already in flight",
                ));
            }
        }

        // Reserve caller-supplied IDs before allocating automatic IDs.
        for id in &explicit_ids {
            self.pending.insert(*id);
        }
        let reserved_ids: Vec<u16> = explicit_ids.into_iter().collect();
        let mut inflight: std::collections::HashMap<u16, MixedPending> =
            std::collections::HashMap::new();
        let mut datasets: std::collections::HashMap<u8, std::collections::VecDeque<MixedDataset>> =
            std::collections::HashMap::new();
        let mut next = 0usize;
        let window = self.window().max(1);
        let mut cancellation_receiver = cancellation.as_ref().map(|handle| handle.subscribe());
        let mut active = std::collections::HashMap::new();
        let mut cancel_sent = std::collections::HashSet::new();

        let result = async {
            while next < requests.len() || !inflight.is_empty() {
                active.retain(|id, _| inflight.contains_key(id));
                cancel_sent.retain(|id| inflight.contains_key(id));
                while next < requests.len() && inflight.len() < window {
                    // Reverse C-STORE needs the peer to read: drain earlier work and keep GET alone.
                    if inflight.values().any(|pending| pending.kind == crate::batch::RequestKind::Get)
                        || (!inflight.is_empty() && matches!(requests[next], crate::batch::DicomRequest::Get { .. }))
                    {
                        break;
                    }
                    let pending = self.send_mixed_request(next, &requests[next]).await?;
                    if matches!(pending.kind, crate::batch::RequestKind::Find | crate::batch::RequestKind::Get | crate::batch::RequestKind::Move) {
                        if let Some(handle) = &cancellation {
                            active.insert(pending.message_id, handle.register(pending.message_id));
                        }
                    }
                    self.pending.insert(pending.message_id);
                    inflight.insert(pending.message_id, pending);
                    next += 1;
                }
                if let Some(handle) = &cancellation {
                    for id in handle.take_cancelled() {
                        if let Some(pending) = inflight.get(&id) {
                            if cancel_sent.insert(id) {
                                self.send_c_cancel(pending.context_id, id).await?;
                            }
                        }
                    }
                }

                let message_id = *inflight
                    .keys()
                    .next()
                    .expect("a mixed request is in flight");
                let pdu = if let Some(receiver) = cancellation_receiver.as_mut() {
                    tokio::select! {
                        pdu = self.read_request_pdu(message_id) => pdu?,
                        changed = receiver.changed() => {
                            if changed.is_err() { cancellation_receiver = None; }
                            continue;
                        }
                    }
                } else {
                    self.read_request_pdu(message_id).await?
                };
                let Some(pdu) = pdu else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            match unit.stream {
                                Stream::Command => {
                                    let command = command_from_bytes(&unit.bytes)?;
                                    if command.field == CommandField::CStoreRq {
                                        self.queue_mixed_reverse_store(
                                            unit.context_id,
                                            command,
                                            &mut datasets,
                                        )?;
                                        continue;
                                    }
                                    let id = command
                                        .message_id_being_responded_to
                                        .ok_or(NetworkError::protocol(
                                            "mixed response has no Message ID Being Responded To",
                                        ))?;
                                    let pending = inflight.get(&id).cloned().ok_or_else(|| {
                                        NetworkError::protocol(
                                            "duplicate or unexpected mixed response ID",
                                        )
                                    })?;
                                    Self::validate_response_context(
                                        pending.context_id,
                                        unit.context_id,
                                        "mixed batch",
                                    )?;
                                    if command.field != pending.expected_field {
                                        return Err(NetworkError::protocol(
                                            "mixed response field does not match its request",
                                        ));
                                    }
                                    if command.no_dataset {
                                        Self::emit_mixed_response(
                                            &mut self.pending,
                                            pending,
                                            command,
                                            None,
                                            &mut inflight,
                                            &mut on_response,
                                        )?;
                                    } else {
                                        datasets
                                            .entry(unit.context_id)
                                            .or_default()
                                            .push_back(MixedDataset::Response {
                                                message_id: id,
                                                command,
                                            });
                                    }
                                }
                                Stream::Data => {
                                    let queued = datasets
                                        .get_mut(&unit.context_id)
                                        .and_then(std::collections::VecDeque::pop_front)
                                        .ok_or(NetworkError::protocol(
                                            "mixed Dataset arrived without a command",
                                        ))?;
                                    match queued {
                                        MixedDataset::Response {
                                            message_id,
                                            command,
                                        } => {
                                            let pending = inflight
                                                .get(&message_id)
                                                .cloned()
                                                .ok_or_else(|| {
                                                    NetworkError::protocol(
                                                        "mixed Dataset belongs to an unknown response",
                                                    )
                                                })?;
                                            Self::validate_response_context(
                                                pending.context_id,
                                                unit.context_id,
                                                "mixed batch",
                                            )?;
                                            let dataset =
                                                self.decode_response_body(&unit, &pending.syntax).await?;
                                            Self::emit_mixed_response(
                                                &mut self.pending,
                                                pending,
                                                command,
                                                Some(dataset),
                                                &mut inflight,
                                                &mut on_response,
                                            )?;
                                        }
                                        MixedDataset::ReverseStore { command } => {
                                            self.handle_mixed_reverse_store(
                                                unit.context_id,
                                                command,
                                                unit.body(),
                                            )
                                            .await?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        });
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {
                        return Err(NetworkError::protocol(
                            "unexpected PDU during a mixed DIMSE batch",
                        ));
                    }
                }
            }
            Ok(())
        }
        .await;

        for id in inflight.keys().copied().chain(reserved_ids.iter().copied()) {
            self.pending.remove(id);
        }
        result
    }

    async fn send_mixed_request(
        &mut self,
        request_index: usize,
        request: &crate::batch::DicomRequest,
    ) -> Result<MixedPending> {
        use crate::batch::{DicomRequest, RequestKind};
        match request {
            DicomRequest::Echo(request) => {
                let context = self
                    .contexts
                    .pick(&request.affected_sop_class_uid, "")?
                    .clone();
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                }
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                self.write_mixed_message(
                    context.id,
                    &request.to_command(),
                    None,
                    &context.transfer_syntax,
                )
                .await?;
                Ok(MixedPending {
                    request_index,
                    message_id: request.message_id,
                    context_id: context.id,
                    kind: RequestKind::Echo,
                    expected_field: CommandField::CEchoRsp,
                    expected_sop_class_uid: Some(request.affected_sop_class_uid),
                    expected_sop_instance_uid: None,
                    syntax: transfer_syntax_of(&context.transfer_syntax)?,
                })
            }
            DicomRequest::Store { file } => {
                let (message_id, context_id) = self.send_c_store_unawaited(file).await?;
                let class = file
                    .dataset
                    .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
                    .map(|value| value.trim().to_owned())
                    .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
                let instance = file
                    .dataset
                    .try_get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
                    .map(|value| value.trim().to_owned())
                    .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?;
                let syntax = transfer_syntax_of(&self.syntax_of(context_id))?;
                Ok(MixedPending {
                    request_index,
                    message_id,
                    context_id,
                    kind: RequestKind::Store,
                    expected_field: CommandField::CStoreRsp,
                    expected_sop_class_uid: Some(class),
                    expected_sop_instance_uid: Some(instance),
                    syntax,
                })
            }
            DicomRequest::StoreRequest(request) => {
                let class = if request.affected_sop_class_uid.trim().is_empty() {
                    request
                        .file
                        .dataset
                        .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
                        .map(|value| value.trim().to_owned())
                        .ok_or(NetworkError::protocol("instance has no SOP class UID"))?
                } else {
                    request.affected_sop_class_uid.trim().to_owned()
                };
                let instance = if request.affected_sop_instance_uid.trim().is_empty() {
                    request
                        .file
                        .dataset
                        .try_get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
                        .map(|value| value.trim().to_owned())
                        .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?
                } else {
                    request.affected_sop_instance_uid.trim().to_owned()
                };
                let (message_id, context_id) = self
                    .send_c_store_request_unawaited(request, &class, &instance)
                    .await?;
                let syntax = transfer_syntax_of(&self.syntax_of(context_id))?;
                Ok(MixedPending {
                    request_index,
                    message_id,
                    context_id,
                    kind: RequestKind::Store,
                    expected_field: CommandField::CStoreRsp,
                    expected_sop_class_uid: Some(class),
                    expected_sop_instance_uid: Some(instance),
                    syntax,
                })
            }
            DicomRequest::Find(request) => {
                let context = self.contexts.pick(&request.sop_class_uid, "")?.clone();
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                }
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                self.write_mixed_message(
                    context.id,
                    &request.to_command(),
                    Some(&request.identifier),
                    &context.transfer_syntax,
                )
                .await?;
                Ok(MixedPending {
                    request_index,
                    message_id: request.message_id,
                    context_id: context.id,
                    kind: RequestKind::Find,
                    expected_field: CommandField::CFindRsp,
                    expected_sop_class_uid: Some(request.sop_class_uid),
                    expected_sop_instance_uid: None,
                    syntax: transfer_syntax_of(&context.transfer_syntax)?,
                })
            }
            DicomRequest::Get { request, .. } => {
                let context = self.contexts.pick(&request.sop_class_uid, "")?.clone();
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                }
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                self.write_mixed_message(
                    context.id,
                    &request.to_command(),
                    Some(&request.identifier),
                    &context.transfer_syntax,
                )
                .await?;
                Ok(MixedPending {
                    request_index,
                    message_id: request.message_id,
                    context_id: context.id,
                    kind: RequestKind::Get,
                    expected_field: CommandField::CGetRsp,
                    expected_sop_class_uid: Some(request.sop_class_uid),
                    expected_sop_instance_uid: None,
                    syntax: transfer_syntax_of(&context.transfer_syntax)?,
                })
            }
            DicomRequest::Move(request) => {
                let context = self.contexts.pick(&request.sop_class_uid, "")?.clone();
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                }
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                self.write_mixed_message(
                    context.id,
                    &request.to_command(),
                    Some(&request.identifier),
                    &context.transfer_syntax,
                )
                .await?;
                Ok(MixedPending {
                    request_index,
                    message_id: request.message_id,
                    context_id: context.id,
                    kind: RequestKind::Move,
                    expected_field: CommandField::CMoveRsp,
                    expected_sop_class_uid: Some(request.sop_class_uid),
                    expected_sop_instance_uid: None,
                    syntax: transfer_syntax_of(&context.transfer_syntax)?,
                })
            }
            DicomRequest::NService(request) => {
                let sop_class = request
                    .sop_class_uid
                    .as_deref()
                    .ok_or(NetworkError::protocol(
                        "N-service request has no SOP class UID",
                    ))?;
                let preferred = request
                    .dataset
                    .as_ref()
                    .map(|dataset| dataset.transfer_syntax().uid().to_owned())
                    .unwrap_or_else(|| {
                        crate::association::transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_owned()
                    });
                let context = self.contexts.pick(sop_class, &preferred)?.clone();
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                }
                let expected_field =
                    crate::dimse::response_field(request.field).ok_or_else(|| {
                        NetworkError::protocol("N-service request has no response field")
                    })?;
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                self.write_mixed_message(
                    context.id,
                    &request.to_command(),
                    request.dataset.as_ref(),
                    &context.transfer_syntax,
                )
                .await?;
                Ok(MixedPending {
                    request_index,
                    message_id: request.message_id,
                    context_id: context.id,
                    kind: RequestKind::NService,
                    expected_field,
                    expected_sop_class_uid: request.sop_class_uid,
                    expected_sop_instance_uid: request.sop_instance_uid,
                    syntax: transfer_syntax_of(&context.transfer_syntax)?,
                })
            }
        }
    }

    async fn write_mixed_message(
        &mut self,
        context_id: u8,
        command: &Command,
        dataset: Option<&radicom::model::dataset::Dataset>,
        transfer_syntax: &str,
    ) -> Result<()> {
        let syntax = transfer_syntax_of(transfer_syntax)?;
        let command = bytes::Bytes::from(command_bytes(command)?);
        let dataset = dataset
            .map(|dataset| crate::message::encode_dataset(dataset, &syntax))
            .transpose()?
            .map(bytes::Bytes::from);
        let pdus = self
            .splitter
            .split_message_bytes(context_id, command, dataset)?;
        write_pdata_batch(&mut self.stream, pdus).await
    }

    fn queue_mixed_reverse_store(
        &mut self,
        context_id: u8,
        command: Command,
        datasets: &mut std::collections::HashMap<u8, std::collections::VecDeque<MixedDataset>>,
    ) -> Result<()> {
        self.contexts
            .validate_request(context_id, &command, false)?;
        if command.no_dataset {
            return Err(NetworkError::protocol(
                "reverse C-STORE request has no Dataset",
            ));
        }
        self.assembler.arm_spill(context_id);
        datasets
            .entry(context_id)
            .or_default()
            .push_back(MixedDataset::ReverseStore { command });
        Ok(())
    }

    async fn handle_mixed_reverse_store(
        &mut self,
        context_id: u8,
        command: Command,
        body: crate::assembler::MessageBody,
    ) -> Result<()> {
        let Some(handler) = self.reverse_store.clone() else {
            return Err(NetworkError::ReverseStoreHandlerMissing);
        };
        let context = self
            .contexts
            .by_id(context_id)
            .ok_or_else(|| NetworkError::protocol("reverse C-STORE on an unaccepted context"))?;
        let syntax = transfer_syntax_of(&context.transfer_syntax)?;
        let fallback = self.fallback_encoding;
        let sink = if body.spool().is_some() {
            crate::store::StoreSink::SpoolToTemp
        } else {
            crate::store::StoreSink::Memory
        };
        let decode_syntax = syntax.clone();
        let limit = self.max_materialized_length;
        let dataset = tokio::task::spawn_blocking(move || {
            sink.materialize_body_with_fallback_limited(&body, &decode_syntax, fallback, limit)
        })
        .await
        .map_err(|join| {
            NetworkError::Io(std::io::Error::other(format!(
                "reverse C-STORE decode task failed: {join}"
            )))
        })??;
        let mut file = dataset.file;
        file.dataset.set_transfer_syntax(syntax);
        let request = crate::store::c_store_request_from_message(
            &command,
            crate::store::ReceivedInstance::from_file(file),
        )?;
        let response = match crate::store::dataset_mismatch(&request) {
            Some(comment) => crate::services::CStoreResponse::failure(
                &request,
                crate::store::DATASET_MISMATCH,
                comment,
            ),
            None => match handler(&request) {
                Ok(response) => response,
                Err(error) => crate::store::failure_response(&request, &error),
            },
        };
        let bytes = bytes::Bytes::from(command_bytes(&response.to_command())?);
        for pdata in self.splitter.split_bytes(context_id, true, bytes)? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }
        Ok(())
    }

    fn emit_mixed_response<F>(
        pending_table: &mut crate::engine::PendingTable,
        pending: MixedPending,
        command: Command,
        dataset: Option<radicom::model::dataset::Dataset>,
        inflight: &mut std::collections::HashMap<u16, MixedPending>,
        on_response: &mut F,
    ) -> Result<()>
    where
        F: FnMut(crate::batch::DicomResponseEvent) -> std::io::Result<()>,
    {
        let (response, is_final) = match pending.kind {
            crate::batch::RequestKind::Echo => {
                if dataset.is_some() {
                    return Err(NetworkError::protocol("C-ECHO response carries a Dataset"));
                }
                let response = crate::services::CEchoResponse::from_command(&command)?;
                (crate::batch::DicomResponse::CEcho(response), true)
            }
            crate::batch::RequestKind::Store => {
                if dataset.is_some() {
                    return Err(NetworkError::protocol("C-STORE response carries a Dataset"));
                }
                let response = crate::services::CStoreResponse::from_command(&command)?;
                (crate::batch::DicomResponse::CStore(response), true)
            }
            crate::batch::RequestKind::Find => {
                let response = crate::cfind::CFindResponse::from_command(&command, dataset)?;
                let is_final = !response.is_pending();
                (crate::batch::DicomResponse::CFind(response), is_final)
            }
            crate::batch::RequestKind::Get => {
                let mut response = crate::cget::CGetResponse::from_command(&command)?;
                let is_final = !response.is_pending();
                if let Some(identifier) = dataset {
                    if !is_final {
                        return Err(NetworkError::protocol(
                            "Pending C-GET response has an Identifier",
                        ));
                    }
                    response.set_identifier(&identifier);
                }
                (crate::batch::DicomResponse::CGet(response), is_final)
            }
            crate::batch::RequestKind::Move => {
                let mut response = crate::cmove::CMoveResponse::from_command(&command)?;
                let is_final = !response.is_pending();
                if let Some(identifier) = dataset {
                    if !is_final {
                        return Err(NetworkError::protocol(
                            "Pending C-MOVE response has an Identifier",
                        ));
                    }
                    response.set_identifier(&identifier);
                }
                (crate::batch::DicomResponse::CMove(response), is_final)
            }
            crate::batch::RequestKind::NService => {
                let response = crate::nservice::NServiceResponse::from_command(&command, dataset);
                (crate::batch::DicomResponse::NService(response), true)
            }
        };
        if pending
            .expected_sop_class_uid
            .as_deref()
            .is_some_and(|expected| {
                command
                    .affected_sop_class_uid
                    .as_deref()
                    .is_some_and(|actual| actual != expected)
            })
        {
            return Err(NetworkError::protocol(
                "mixed response used the wrong SOP Class UID",
            ));
        }
        if pending
            .expected_sop_instance_uid
            .as_deref()
            .is_some_and(|expected| {
                command
                    .affected_sop_instance_uid
                    .as_deref()
                    .is_some_and(|actual| actual != expected)
            })
        {
            return Err(NetworkError::protocol(
                "mixed C-STORE response did not echo the requested SOP Instance UID",
            ));
        }
        on_response(crate::batch::DicomResponseEvent {
            request_index: pending.request_index,
            message_id: pending.message_id,
            response,
            is_final,
        })
        .map_err(NetworkError::Io)?;
        if is_final {
            inflight.remove(&pending.message_id);
            pending_table.remove(pending.message_id);
        }
        Ok(())
    }

    /// The negotiated presentation contexts.
    pub fn contexts(&self) -> &AcceptedContexts {
        &self.contexts
    }

    /// Sends a C-ECHO and waits for its response (§11.4).
    pub async fn send_c_echo(&mut self, abstract_syntax: &str) -> Result<CEchoResponse> {
        let context = self.contexts.pick(abstract_syntax, "")?;
        let context_id = context.id;
        let message_id = self.allocate_message_id()?;
        let request = CEchoRequest {
            message_id,
            affected_sop_class_uid: abstract_syntax.to_string(),
        };

        let command = request.to_command();
        let bytes = bytes::Bytes::from(command_bytes(&command)?);
        let pdus = self.splitter.split_bytes(context_id, true, bytes)?;
        write_pdata_batch(&mut self.stream, pdus).await?;

        self.pending.insert(command.message_id);
        let response = self
            .await_response_for_context(message_id, CommandField::CEchoRsp, context_id)
            .await;
        self.pending.remove(message_id);
        let command = response?;
        Self::validate_response_sop_class(
            abstract_syntax,
            command.affected_sop_class_uid.as_deref(),
        )?;
        let response = CEchoResponse::from_command(&command)?;
        Ok(response)
    }

    /// Sends a C-STORE and waits for its response (§11.4).
    ///
    /// The dataset travels under the presentation context's accepted syntax;
    /// when that differs from the file's own syntax the dataset is transcoded
    /// through the global codec registry (§11.1).
    ///
    /// **Ceiling (L3):** a syntax pair that no registered codec handles fails
    /// the request instead of falling back to an uncompressed syntax. Offer the
    /// peer a syntax this side can produce, or register a transcoder first.
    pub async fn send_c_store(
        &mut self,
        file: &radicom::io::file::DicomFile,
    ) -> Result<CStoreResponse> {
        self.send_c_store_with_originator(file, None, None).await
    }

    /// Sends a C-STORE carrying Move Originator information (§11.4).
    ///
    /// The SCP of a C-MOVE uses this when pushing instances to the destination
    /// so the destination can attribute the sub-operation (divergence D5: the
    /// fields are always written when supplied).
    pub async fn send_c_store_with_originator(
        &mut self,
        file: &radicom::io::file::DicomFile,
        originator_ae: Option<&str>,
        originator_message_id: Option<u16>,
    ) -> Result<CStoreResponse> {
        let (message_id, context_id, target, command_bytes) =
            self.plan_c_store(file, originator_ae, originator_message_id)?;
        let dataset = &file.dataset;
        let expected_sop_class_uid = dataset
            .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let expected_sop_instance_uid = dataset
            .try_get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Stream the dataset when no transcode is needed (NET-13): the writer
        // frames PDUs through a bounded channel, so a multi-GB instance never
        // sits in memory as one Vec. A transcode materialises its output by
        // nature, so that branch keeps the buffered path.
        let source = dataset.transfer_syntax();
        // L3: a dataset no codec can convert still goes out, in its own syntax,
        // when the caller opted in; PixelData is never stripped
        let send_syntax = if source.uid() == target.uid() {
            target.clone()
        } else {
            let transcoder = transcoder_for(&mut self.transcoder_cache, &source, &target);
            match transcoder.transcode_dataset(dataset) {
                Ok(transcoded) => {
                    let payload =
                        bytes::Bytes::from(crate::message::encode_dataset(&transcoded, &target)?);
                    let pdus = self.splitter.split_message_bytes(
                        context_id,
                        command_bytes,
                        Some(payload),
                    )?;
                    write_pdata_batch(&mut self.stream, pdus).await?;
                    return self
                        .finish_c_store(
                            message_id,
                            context_id,
                            &expected_sop_class_uid,
                            &expected_sop_instance_uid,
                        )
                        .await;
                }
                Err(_) if self.ignore_unsupported_syntax_change => source.clone(),
                Err(error) => {
                    return Err(NetworkError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error.to_string(),
                    )))
                }
            }
        };
        let max_pdu = self.splitter.max_pdu();
        let max_pdvs = self.splitter.max_pdvs_per_pdu();
        crate::message::write_message_streamed(
            &mut self.stream,
            context_id,
            command_bytes,
            dataset.clone(),
            send_syntax,
            max_pdu,
            max_pdvs,
            self.splitter.max_command_pdv(),
            self.splitter.max_data_pdv(),
        )
        .await?;
        self.finish_c_store(
            message_id,
            context_id,
            &expected_sop_class_uid,
            &expected_sop_instance_uid,
        )
        .await
    }

    /// Sends a C-STORE message without waiting for its response. The mixed
    /// request scheduler uses this to keep several DIMSE operations in flight.
    async fn send_c_store_unawaited(
        &mut self,
        file: &radicom::io::file::DicomFile,
    ) -> Result<(u16, u8)> {
        let (message_id, context_id, target, command_bytes) =
            self.plan_c_store(file, None, None)?;
        self.send_c_store_unawaited_with_plan(file, message_id, context_id, target, command_bytes)
            .await
    }

    /// Sends a complete C-STORE request object without waiting for its response.
    /// The request owns the command metadata while the dataset uses the same
    /// bounded streaming path as the file convenience API.
    async fn send_c_store_request_unawaited(
        &mut self,
        request: &crate::services::CStoreRequest,
        sop_class: &str,
        sop_instance: &str,
    ) -> Result<(u16, u8)> {
        let context = self
            .contexts
            .pick(sop_class, request.file.dataset.transfer_syntax().uid())?
            .clone();
        let message_id = if request.message_id == 0 {
            self.allocate_message_id()?
        } else {
            request.message_id
        };
        let mut command = request.to_command();
        command.message_id = message_id;
        command.affected_sop_class_uid = Some(sop_class.to_owned());
        command.affected_sop_instance_uid = Some(sop_instance.to_owned());
        command.no_dataset = false;
        self.contexts.validate_request(context.id, &command, true)?;
        let target = transfer_syntax_of(&context.transfer_syntax)?;
        let command_bytes = bytes::Bytes::from(command_bytes(&command)?);
        self.send_c_store_unawaited_with_plan(
            &request.file,
            message_id,
            context.id,
            target,
            command_bytes,
        )
        .await
    }

    async fn send_c_store_unawaited_with_plan(
        &mut self,
        file: &radicom::io::file::DicomFile,
        message_id: u16,
        context_id: u8,
        target: Arc<radicom::model::transfer_syntax::TransferSyntax>,
        command_bytes: bytes::Bytes,
    ) -> Result<(u16, u8)> {
        let dataset = &file.dataset;
        let source = dataset.transfer_syntax();
        let send_syntax = if source.uid() == target.uid() {
            target.clone()
        } else {
            let transcoder = transcoder_for(&mut self.transcoder_cache, &source, &target);
            match transcoder.transcode_dataset(dataset) {
                Ok(transcoded) => {
                    let payload =
                        bytes::Bytes::from(crate::message::encode_dataset(&transcoded, &target)?);
                    let pdus = self.splitter.split_message_bytes(
                        context_id,
                        command_bytes,
                        Some(payload),
                    )?;
                    write_pdata_batch(&mut self.stream, pdus).await?;
                    return Ok((message_id, context_id));
                }
                Err(_) if self.ignore_unsupported_syntax_change => source.clone(),
                Err(error) => {
                    return Err(NetworkError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error.to_string(),
                    )))
                }
            }
        };
        crate::message::write_message_streamed(
            &mut self.stream,
            context_id,
            command_bytes,
            dataset.clone(),
            send_syntax,
            self.splitter.max_pdu(),
            self.splitter.max_pdvs_per_pdu(),
            self.splitter.max_command_pdv(),
            self.splitter.max_data_pdv(),
        )
        .await?;
        Ok((message_id, context_id))
    }

    /// Waits for the response of the C-STORE sent as `message_id` (§11.4).
    async fn finish_c_store(
        &mut self,
        message_id: u16,
        context_id: u8,
        expected_sop_class_uid: &str,
        expected_sop_instance_uid: &str,
    ) -> Result<CStoreResponse> {
        self.pending.insert(message_id);
        let response = self
            .await_response_with_context(message_id, CommandField::CStoreRsp, true, None)
            .await;
        self.pending.remove(message_id);
        let (response_context, command) = response?;
        if response_context != context_id {
            return Err(NetworkError::protocol(
                "C-STORE response used the wrong presentation context",
            ));
        }
        let response = CStoreResponse::from_command(&command)?;
        if command
            .affected_sop_class_uid
            .as_deref()
            .is_some_and(|actual| actual != expected_sop_class_uid)
            || command
                .affected_sop_instance_uid
                .as_deref()
                .is_some_and(|actual| actual != expected_sop_instance_uid)
        {
            return Err(NetworkError::protocol(
                "C-STORE response did not echo the requested SOP UIDs",
            ));
        }
        Ok(response)
    }

    /// Validates a C-STORE and builds its command, context and target syntax.
    ///
    /// Shared by the single-request path, which then streams or transcodes the
    /// dataset, and by [`Self::send_c_store_batch`], which must encode every
    /// queued request before the first response arrives (M6).
    #[allow(clippy::type_complexity)]
    fn plan_c_store(
        &mut self,
        file: &radicom::io::file::DicomFile,
        originator_ae: Option<&str>,
        originator_message_id: Option<u16>,
    ) -> Result<(
        u16,
        u8,
        Arc<radicom::model::transfer_syntax::TransferSyntax>,
        bytes::Bytes,
    )> {
        let dataset = &file.dataset;
        let sop_class = dataset
            .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let sop_instance = dataset
            .try_get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?;

        let context = self
            .contexts
            .pick(&sop_class, dataset.transfer_syntax().uid())?;
        let context_id = context.id;
        let target = transfer_syntax_of(&context.transfer_syntax)?;
        let message_id = self.allocate_message_id()?;

        let mut request = Command::request(CommandField::CStoreRq, message_id);
        request.affected_sop_class_uid = Some(sop_class);
        request.affected_sop_instance_uid = Some(sop_instance);
        request.move_originator_ae_title = originator_ae.map(|ae| ae.to_string());
        request.move_originator_message_id = originator_message_id;
        request.no_dataset = false;
        self.contexts.validate_request(context_id, &request, true)?;

        Ok((
            message_id,
            context_id,
            target,
            bytes::Bytes::from(command_bytes(&request)?),
        ))
    }

    /// Sends several C-STORE requests before waiting for any response (M6).
    ///
    /// fo-dicom gates its send loop on the negotiated `MaxAsyncOpsInvoked`
    /// (`Network/DicomService.cs:1145-1146`); this is the same window, with at
    /// most 64 requests outstanding. Datasets are encoded lazily, at most one
    /// window ahead of the send, so peak memory stays bounded; responses are
    /// matched by message ID, so a peer may answer out of order.
    ///
    /// The peer controls provider concurrency through its negotiated window.
    /// One malformed response fails the batch.
    pub async fn send_c_store_batch(
        &mut self,
        files: &[&radicom::io::file::DicomFile],
    ) -> Result<Vec<CStoreResponse>> {
        let window = self.window().max(1);
        let mut inflight: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
        let mut responses: Vec<Option<CStoreResponse>> = vec![None; files.len()];
        // Encode at most a window's worth ahead of the send, then send it and
        // free the window for the next file: peak memory is O(window * max_pdu
        // + one instance), not O(Σ instances) as when the whole batch was
        // materialised up front (A). Responses match by message id.
        let mut ready: std::collections::VecDeque<(u16, usize, Vec<crate::pdu::PData>)> =
            std::collections::VecDeque::new();
        let mut next = 0usize;
        let result = async {
            while next < files.len() || !ready.is_empty() || !inflight.is_empty() {
                // encode only enough files to fill the async-operations window
                while next < files.len() && ready.len() + inflight.len() < window {
                    let (message_id, context_id, target, command) =
                        self.plan_c_store(files[next], None, None)?;
                    let dataset = &files[next].dataset;
                    let source = dataset.transfer_syntax();
                    let payload = if source.uid() == target.uid() {
                        bytes::Bytes::from(crate::message::encode_dataset(dataset, &target)?)
                    } else {
                        let transcoder =
                            transcoder_for(&mut self.transcoder_cache, &source, &target);
                        match transcoder.transcode_dataset(dataset) {
                            Ok(transcoded) => bytes::Bytes::from(crate::message::encode_dataset(
                                &transcoded,
                                &target,
                            )?),
                            // L3: as-is in the instance's own syntax
                            Err(_) if self.ignore_unsupported_syntax_change => bytes::Bytes::from(
                                crate::message::encode_dataset(dataset, &source)?,
                            ),
                            Err(error) => {
                                return Err(NetworkError::Io(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    error.to_string(),
                                )))
                            }
                        }
                    };
                    let pdus =
                        self.splitter
                            .split_message_bytes(context_id, command, Some(payload))?;
                    ready.push_back((message_id, next, pdus));
                    next += 1;
                }
                // send every entry encoded so far; they all fit the window
                while let Some((message_id, index, pdus)) = ready.pop_front() {
                    write_pdata_batch(&mut self.stream, pdus).await?;
                    inflight.insert(message_id, index);
                    self.pending.insert(message_id);
                }
                let message_id = *inflight.keys().next().expect("a request is pending");
                let Some(pdu) = self.read_request_pdu(message_id).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                            // mistaking it for a C-STORE response
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            if unit.stream != Stream::Command {
                                return Err(NetworkError::protocol(
                                    "C-STORE response carries a dataset",
                                ));
                            }
                            let command = command_from_bytes(&unit.bytes)?;
                            let response = CStoreResponse::from_command(&command)?;
                            let id = response.message_id_being_responded_to;
                            let index = inflight.remove(&id).ok_or_else(|| {
                                NetworkError::protocol(
                                    "duplicate or unexpected C-STORE batch response ID",
                                )
                            })?;
                            let request = files[index];
                            let expected_class = request
                                .dataset
                                .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
                                .map(|s| s.trim().to_string())
                                .unwrap_or_default();
                            let expected_context = self
                                .contexts
                                .pick(&expected_class, request.dataset.transfer_syntax().uid())?;
                            if unit.context_id != expected_context.id {
                                return Err(NetworkError::protocol(
                                    "C-STORE batch response used the wrong presentation context",
                                ));
                            }
                            let expected_instance = request
                                .dataset
                                .try_get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
                                .map(|s| s.trim().to_string())
                                .unwrap_or_default();
                            if command
                                .affected_sop_class_uid
                                .as_deref()
                                .is_some_and(|actual| actual != expected_class)
                                || command
                                    .affected_sop_instance_uid
                                    .as_deref()
                                    .is_some_and(|actual| actual != expected_instance)
                            {
                                return Err(NetworkError::protocol(
                                    "C-STORE batch response did not echo the requested SOP UIDs",
                                ));
                            }
                            self.pending.remove(id);
                            responses[index] = Some(response);
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {
                        return Err(NetworkError::protocol(
                            "unexpected PDU during a C-STORE batch",
                        ))
                    }
                }
            }
            Ok(responses
                .into_iter()
                .map(|response| response.expect("every request completed"))
                .collect())
        }
        .await;
        for id in inflight.keys() {
            self.pending.remove(*id);
        }
        result
    }

    /// Runs several C-FIND requests within the async-operations window (M6).
    ///
    /// Results are returned per request, in request order. Responses are
    /// demultiplexed by message ID, so match rows from different requests may
    /// interleave; each request's stream ends at its non-pending response.
    ///
    /// **Ceiling:** a request in a batch cannot be cancelled (C-CANCEL needs the
    /// single-request path's registration); use [`Self::send_c_find_stream`].
    pub async fn send_c_find_batch(
        &mut self,
        requests: &[crate::cfind::CFindRequest],
    ) -> Result<Vec<Vec<crate::cfind::CFindResponse>>> {
        let mut reserved_ids =
            self.reserve_explicit_ids(requests.iter().map(|request| request.message_id))?;
        let prepared_result = (|| -> Result<_> {
            let mut prepared = Vec::with_capacity(requests.len());
            for request in requests {
                let mut request = request.clone();
                if request.message_id == 0 {
                    request.message_id = self.allocate_message_id()?;
                    self.pending.insert(request.message_id);
                    reserved_ids.push(request.message_id);
                }
                let context = self.contexts.pick(&request.sop_class_uid, "")?;
                self.contexts
                    .validate_request(context.id, &request.to_command(), true)?;
                let syntax = transfer_syntax_of(&context.transfer_syntax)?;
                prepared.push((request, syntax, context.id));
            }
            Ok(prepared)
        })();
        let prepared = match prepared_result {
            Ok(value) => value,
            Err(error) => {
                for id in &reserved_ids {
                    self.pending.remove(*id);
                }
                return Err(error);
            }
        };

        let window = self.window();
        let mut inflight: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
        let mut responses: Vec<Vec<crate::cfind::CFindResponse>> =
            requests.iter().map(|_| Vec::new()).collect();
        let mut awaiting: Option<(Command, usize)> = None;
        let mut next = 0usize;
        let result = async {
            while next < prepared.len() || !inflight.is_empty() {
                while next < prepared.len() && inflight.len() < window {
                    let (request, syntax, context_id) = &prepared[next];
                    let payload = bytes::Bytes::from(crate::message::encode_dataset(
                        &request.identifier,
                        syntax,
                    )?);
                    let pdus = self.splitter.split_message_bytes(
                        *context_id,
                        bytes::Bytes::from(command_bytes(&request.to_command())?),
                        Some(payload),
                    )?;
                    write_pdata_batch(&mut self.stream, pdus).await?;
                    inflight.insert(request.message_id, next);
                    self.pending.insert(request.message_id);
                    next += 1;
                }
                let message_id = *inflight.keys().next().expect("a request is pending");
                let Some(pdu) = self.read_request_pdu(message_id).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            match unit.stream {
                                Stream::Command => {
                                    let command = command_from_bytes(&unit.bytes)?;
                                    let Some(id) = command.message_id_being_responded_to
                                    else {
                                        return Err(NetworkError::protocol(
                                            "C-FIND batch response has no message ID being responded to",
                                        ));
                                    };
                                    let index = *inflight.get(&id).ok_or_else(|| {
                                        NetworkError::protocol(
                                            "duplicate or unexpected C-FIND batch response ID",
                                        )
                                    })?;
                                    Self::validate_response_sop_class(&requests[index].sop_class_uid, command.affected_sop_class_uid.as_deref())?;
                                    let expected_context_id = prepared[index].2;
                                    Self::validate_response_context(
                                        expected_context_id,
                                        unit.context_id,
                                        "C-FIND batch",
                                    )?;
                                    if command.no_dataset {
                                        if awaiting.is_some() {
                                            return Err(NetworkError::protocol(
                                                "C-FIND batch response arrived before its pending Identifier",
                                            ));
                                        }
                                        self.record_find_response(
                                            index,
                                            command,
                                            None,
                                            &mut inflight,
                                            &mut responses,
                                        )?;
                                        continue;
                                    }
                                    Self::put_pending_dataset(
                                        &mut awaiting,
                                        (command, index),
                                        "C-FIND batch response carried overlapping datasets",
                                    )?;
                                }
                                Stream::Data => {
                                    let (command, index) = Self::take_pending_dataset(
                                        &mut awaiting,
                                        "C-FIND batch Identifier without its response",
                                    )?;
                                    Self::validate_response_context(
                                        prepared[index].2,
                                        unit.context_id,
                                        "C-FIND batch",
                                    )?;
                                    let (_, syntax, _) = &prepared[index];
                                    let identifier = self.decode_response_body(&unit, syntax).await?;
                                    self.record_find_response(
                                        index,
                                        command,
                                        Some(identifier),
                                        &mut inflight,
                                        &mut responses,
                                    )?;
                                }
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {
                        return Err(NetworkError::protocol(
                            "unexpected PDU during a C-FIND batch",
                        ))
                    }
                }
            }
            Ok(responses)
        }
        .await;
        for id in inflight.keys().copied().chain(reserved_ids.iter().copied()) {
            self.pending.remove(id);
        }
        result
    }

    /// Records one C-FIND batch response and closes its request when terminal.
    fn record_find_response(
        &mut self,
        index: usize,
        command: Command,
        identifier: Option<Dataset>,
        inflight: &mut std::collections::HashMap<u16, usize>,
        responses: &mut [Vec<crate::cfind::CFindResponse>],
    ) -> Result<()> {
        let response = crate::cfind::CFindResponse::from_command(&command, identifier)?;
        let terminal = !response.is_pending();
        let id = response.message_id_being_responded_to;
        responses[index].push(response);
        if terminal {
            inflight.remove(&id);
            self.pending.remove(id);
        }
        Ok(())
    }

    /// The async-operations send window: the negotiated invoked count, 64 max.
    fn window(&self) -> usize {
        match self.async_operations.invoked {
            0 => 64,
            finite => usize::from(finite).min(64),
        }
    }
    /// Runs a C-FIND and returns every match plus the terminal response (§11.4).
    ///
    /// Matches are collected because each response carries a full Identifier;
    /// a caller that needs incremental delivery can drive [`Self::send_c_find_stream`].
    pub async fn send_c_find(
        &mut self,
        request: &crate::cfind::CFindRequest,
    ) -> Result<Vec<crate::cfind::CFindResponse>> {
        let mut all = Vec::new();
        self.send_c_find_stream(request, |response| {
            all.push(response);
            Ok(())
        })
        .await?;
        Ok(all)
    }

    /// Runs a C-FIND, handing each response to `on_response` as it arrives.
    pub async fn send_c_find_stream<F>(
        &mut self,
        request: &crate::cfind::CFindRequest,
        mut on_response: F,
    ) -> Result<()>
    where
        F: FnMut(crate::cfind::CFindResponse) -> std::io::Result<()>,
    {
        let context = self.contexts.pick(&request.sop_class_uid, "")?;
        let context_id = context.id;
        let syntax = transfer_syntax_of(&context.transfer_syntax)?;
        let mut request = request.clone();
        request.message_id = self.message_id_or_allocate(request.message_id)?;

        let message_id = request.message_id;
        let command = bytes::Bytes::from(command_bytes(&request.to_command())?);
        let identifier = bytes::Bytes::from(crate::message::encode_dataset(
            &request.identifier,
            &syntax,
        )?);
        let pdus = self
            .splitter
            .split_message_bytes(context_id, command, Some(identifier))?;
        write_pdata_batch(&mut self.stream, pdus).await?;
        self.pending.insert(message_id);

        // the Identifier of a Pending response follows its command
        let result = async {
            let mut awaiting_command: Option<(u8, Command)> = None;
            loop {
                let Some(pdu) = self.read_request_pdu(message_id).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                            // dropping it, before the response filtering below
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            match unit.stream {
                                Stream::Command => {
                                    let command = command_from_bytes(&unit.bytes)?;
                                    if command.field != CommandField::CFindRsp
                                        || command.message_id_being_responded_to != Some(message_id)
                                    {
                                        continue;
                                    }
                                    if unit.context_id != context_id {
                                        return Err(NetworkError::protocol(
                                            "C-FIND response used the wrong presentation context",
                                        ));
                                    }
                                    Self::validate_response_sop_class(
                                        &request.sop_class_uid,
                                        command.affected_sop_class_uid.as_deref(),
                                    )?;
                                    if command.no_dataset {
                                        if awaiting_command.is_some() {
                                            return Err(NetworkError::protocol(
                                            "C-FIND response arrived before its pending Identifier",
                                        ));
                                        }
                                        let response = crate::cfind::CFindResponse::from_command(
                                            &command, None,
                                        )?;
                                        let terminal = !response.is_pending();
                                        on_response(response)?;
                                        if terminal {
                                            return Ok(());
                                        }
                                    } else {
                                        // the match dataset follows
                                        Self::put_pending_dataset(
                                            &mut awaiting_command,
                                            (unit.context_id, command),
                                            "C-FIND response carried overlapping datasets",
                                        )?;
                                    }
                                }
                                Stream::Data => {
                                    let (command_context, command) = Self::take_pending_dataset(
                                        &mut awaiting_command,
                                        "C-FIND Identifier without its response",
                                    )?;
                                    if unit.context_id != command_context
                                        || unit.context_id != context_id
                                    {
                                        return Err(NetworkError::protocol(
                                            "C-FIND Identifier used the wrong presentation context",
                                        ));
                                    }
                                    let identifier =
                                        self.decode_response_body(&unit, &syntax).await?;
                                    let response = crate::cfind::CFindResponse::from_command(
                                        &command,
                                        Some(identifier),
                                    )?;
                                    let terminal = !response.is_pending();
                                    on_response(response)?;
                                    if terminal {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {}
                }
            }
        }
        .await;
        self.pending.remove(message_id);
        result
    }

    /// Runs a C-MOVE and collects every progress plus the terminal response
    /// (§11.4).
    pub async fn send_c_move(
        &mut self,
        request: &crate::cmove::CMoveRequest,
    ) -> Result<Vec<crate::cmove::CMoveResponse>> {
        let mut all = Vec::new();
        self.send_c_move_stream(request, |response| {
            all.push(response);
            Ok(())
        })
        .await?;
        Ok(all)
    }

    /// Runs a C-MOVE, handing each response to `on_response` as it arrives.
    ///
    /// Instances travel on the SCP's outbound association; a terminal response
    /// may carry the Failed SOP Instance UID List Identifier.
    pub async fn send_c_move_stream<F>(
        &mut self,
        request: &crate::cmove::CMoveRequest,
        mut on_response: F,
    ) -> Result<()>
    where
        F: FnMut(crate::cmove::CMoveResponse) -> std::io::Result<()>,
    {
        let context = self.contexts.pick(&request.sop_class_uid, "")?;
        let context_id = context.id;
        let syntax = transfer_syntax_of(&context.transfer_syntax)?;
        let mut request = request.clone();
        request.message_id = self.message_id_or_allocate(request.message_id)?;

        let message_id = request.message_id;
        let command = bytes::Bytes::from(command_bytes(&request.to_command())?);
        let identifier = bytes::Bytes::from(crate::message::encode_dataset(
            &request.identifier,
            &syntax,
        )?);
        let pdus = self
            .splitter
            .split_message_bytes(context_id, command, Some(identifier))?;
        write_pdata_batch(&mut self.stream, pdus).await?;
        self.pending.insert(message_id);

        let result = async {
            let mut awaiting_identifier: Option<crate::cmove::CMoveResponse> = None;
            loop {
                let Some(pdu) = self.read_request_pdu(message_id).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                            // dropping it, before the response filtering below
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            if unit.context_id != context_id {
                                return Err(NetworkError::protocol(
                                    "C-MOVE response on the wrong context",
                                ));
                            }
                            if unit.stream == Stream::Data {
                                let mut response = awaiting_identifier.take().ok_or_else(|| {
                                    NetworkError::protocol("C-MOVE Identifier without its response")
                                })?;
                                response.set_identifier(
                                    &self.decode_response_body(&unit, &syntax).await?,
                                );
                                on_response(response)?;
                                return Ok(());
                            }
                            let command = command_from_bytes(&unit.bytes)?;
                            if command.field != CommandField::CMoveRsp
                                || command.message_id_being_responded_to != Some(message_id)
                            {
                                continue;
                            }
                            let response = crate::cmove::CMoveResponse::from_command(&command)?;
                            Self::validate_response_sop_class(
                                &request.sop_class_uid,
                                command.affected_sop_class_uid.as_deref(),
                            )?;
                            let terminal = !response.is_pending();
                            if !command.no_dataset {
                                if !terminal {
                                    return Err(NetworkError::protocol(
                                        "Pending C-MOVE response has an Identifier",
                                    ));
                                }
                                awaiting_identifier = Some(response);
                                continue;
                            }
                            on_response(response)?;
                            if terminal {
                                return Ok(());
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {}
                }
            }
        }
        .await;
        self.pending.remove(message_id);
        result
    }

    /// Runs a C-GET, collecting every response (§11.4).
    ///
    /// Instances arrive as reverse C-STORE sub-operations, so a handler must be
    /// registered first; see [`Self::set_reverse_store_handler`].
    pub async fn send_c_get(
        &mut self,
        request: &crate::cget::CGetRequest,
    ) -> Result<Vec<crate::cget::CGetResponse>> {
        let mut all = Vec::new();
        self.send_c_get_stream(request, |response| {
            all.push(response);
            Ok(())
        })
        .await?;
        Ok(all)
    }

    /// Runs a C-GET, handing each response to `on_response` as it arrives.
    pub async fn send_c_get_stream<F>(
        &mut self,
        request: &crate::cget::CGetRequest,
        mut on_response: F,
    ) -> Result<()>
    where
        F: FnMut(crate::cget::CGetResponse) -> std::io::Result<()>,
    {
        let Some(handler) = self.reverse_store.clone() else {
            return Err(NetworkError::ReverseStoreHandlerMissing);
        };
        let context = self.contexts.pick(&request.sop_class_uid, "")?;
        let context_id = context.id;
        let syntax = transfer_syntax_of(&context.transfer_syntax)?;
        let mut request = request.clone();
        request.message_id = self.message_id_or_allocate(request.message_id)?;

        let message_id = request.message_id;
        let command = bytes::Bytes::from(command_bytes(&request.to_command())?);
        let identifier = bytes::Bytes::from(crate::message::encode_dataset(
            &request.identifier,
            &syntax,
        )?);
        let pdus = self
            .splitter
            .split_message_bytes(context_id, command, Some(identifier))?;
        write_pdata_batch(&mut self.stream, pdus).await?;
        self.pending.insert(message_id);

        // a reverse C-STORE request awaiting its dataset
        let result = async {
            let mut awaiting_store: Option<(u8, Command)> = None;
            let mut awaiting_identifier: Option<crate::cget::CGetResponse> = None;
            loop {
                let Some(pdu) = self.read_request_pdu(message_id).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.push_pdata_offloaded(&pdata).await? {
                            // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                            // dropping it, before the response filtering below
                            if self.intercept_event_report(&unit).await? {
                                continue;
                            }
                            match unit.stream {
                                Stream::Command => {
                                    let command = command_from_bytes(&unit.bytes)?;
                                    match command.field {
                                        CommandField::CStoreRq => {
                                            self.contexts.validate_request(
                                                unit.context_id,
                                                &command,
                                                false,
                                            )?;
                                            if command.no_dataset {
                                                // an instance without a dataset is unusable
                                                let Some(response) = Command::response(
                                                    &command,
                                                    crate::dimse::status::PROCESSING_FAILURE,
                                                ) else {
                                                    continue;
                                                };
                                                for pdata in self.splitter.split_bytes(
                                                    unit.context_id,
                                                    true,
                                                    bytes::Bytes::from(command_bytes(&response)?),
                                                )? {
                                                    write_pdu(&mut self.stream, &Pdu::PData(pdata))
                                                        .await?;
                                                }
                                            } else {
                                                if awaiting_store.is_some() {
                                                    return Err(NetworkError::protocol(
                                                        "overlapping reverse C-STORE requests",
                                                    ));
                                                }
                                                // Spool the instance so a large reverse
                                                // C-STORE does not sit in memory or block
                                                // the runtime worker.
                                                self.assembler.arm_spill(unit.context_id);
                                                awaiting_store = Some((unit.context_id, command));
                                            }
                                        }
                                        CommandField::CGetRsp => {
                                            if command.message_id_being_responded_to
                                                != Some(message_id)
                                            {
                                                continue;
                                            }
                                            let response =
                                                crate::cget::CGetResponse::from_command(&command)?;
                                            Self::validate_response_sop_class(
                                                &request.sop_class_uid,
                                                command.affected_sop_class_uid.as_deref(),
                                            )?;
                                            if unit.context_id != context_id {
                                                return Err(NetworkError::protocol(
                                                    "C-GET response on the wrong context",
                                                ));
                                            }
                                            let terminal = !response.is_pending();
                                            if !command.no_dataset {
                                                if !terminal {
                                                    return Err(NetworkError::protocol(
                                                        "Pending C-GET response has an Identifier",
                                                    ));
                                                }
                                                awaiting_identifier = Some(response);
                                                continue;
                                            }
                                            on_response(response)?;
                                            if terminal {
                                                return Ok(());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                Stream::Data => {
                                    if unit.context_id == context_id {
                                        let mut response =
                                            awaiting_identifier.take().ok_or_else(|| {
                                                NetworkError::protocol(
                                                    "C-GET Identifier without its response",
                                                )
                                            })?;
                                        response.set_identifier(
                                            &self.decode_response_body(&unit, &syntax).await?,
                                        );
                                        on_response(response)?;
                                        return Ok(());
                                    }
                                    let (store_context_id, store) =
                                        awaiting_store.take().ok_or_else(|| {
                                            NetworkError::protocol(
                                                "reverse C-STORE dataset without a command",
                                            )
                                        })?;
                                    if unit.context_id != store_context_id {
                                        return Err(NetworkError::protocol(
                                            "reverse C-STORE dataset on the wrong context",
                                        ));
                                    }
                                    // the reverse C-STORE dataset travels on the
                                    // storage context the SCP chose, which is not
                                    // the C-GET request's context: resolve the
                                    // syntax from the PDV's own context id (#15)
                                    let store_context =
                                        self.contexts.by_id(unit.context_id).ok_or_else(|| {
                                            NetworkError::protocol(
                                                "reverse C-STORE on an unaccepted context",
                                            )
                                        })?;
                                    let store_syntax =
                                        transfer_syntax_of(&store_context.transfer_syntax)?;
                                    let body = unit.body();
                                    let fallback = self.fallback_encoding;
                                    let sink = if body.spool().is_some() {
                                        crate::store::StoreSink::SpoolToTemp
                                    } else {
                                        crate::store::StoreSink::Memory
                                    };
                                    let decode_syntax = store_syntax.clone();
                                    let limit = self.max_materialized_length;
                                    let dataset = tokio::task::spawn_blocking(move || {
                                        sink.materialize_body_with_fallback_limited(
                                            &body,
                                            &decode_syntax,
                                            fallback,
                                            limit,
                                        )
                                    })
                                    .await
                                    .map_err(|join| {
                                        NetworkError::Io(std::io::Error::other(format!(
                                            "reverse C-STORE decode task failed: {join}"
                                        )))
                                    })??;
                                    let mut file = dataset.file;
                                    file.dataset.set_transfer_syntax(store_syntax.clone());
                                    let request = crate::store::c_store_request_from_message(
                                        &store,
                                        crate::store::ReceivedInstance::from_file(file),
                                    )?;
                                    // the handler's verdict goes straight back
                                    let response = match crate::store::dataset_mismatch(&request) {
                                        Some(comment) => crate::services::CStoreResponse::failure(
                                            &request,
                                            crate::store::DATASET_MISMATCH,
                                            comment,
                                        ),
                                        None => match handler(&request) {
                                            Ok(response) => response,
                                            Err(error) => {
                                                crate::store::failure_response(&request, &error)
                                            }
                                        },
                                    };
                                    let bytes =
                                        bytes::Bytes::from(command_bytes(&response.to_command())?);
                                    for pdata in
                                        self.splitter.split_bytes(unit.context_id, true, bytes)?
                                    {
                                        write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
                                    }
                                }
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort {
                            source_code: abort.source.code(),
                            reason: abort.reason,
                        })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {}
                }
            }
        }
        .await;
        self.pending.remove(message_id);
        result
    }

    /// Reads PDUs until the response for `message_id` arrives (§11.2).
    ///
    /// Reads go through the cancel-safe framer: a request timeout drops the
    /// read future, and without the retained half-frame the next call would
    /// start in the middle of a PDU (NET-14).
    pub(crate) async fn read_request_pdu(&mut self, message_id: u16) -> Result<Option<Pdu>> {
        let deadline = self
            .request_timeout
            .map(|timeout| tokio::time::Instant::now() + timeout);
        let read = async {
            loop {
                let Some((context_id, cancellation)) = self.request_cancellation.as_mut() else {
                    return self.pdu_reader.read_pdu(&mut self.stream).await;
                };
                let context_id = *context_id;
                let cancelled = tokio::select! {
                    pdu = self.pdu_reader.read_pdu(&mut self.stream) => return pdu,
                    changed = cancellation.changed() => changed.is_ok() && *cancellation.borrow_and_update() == Some(message_id),
                };
                if cancelled {
                    // Remove the signal after sending so duplicate cancellation cannot flood the peer.
                    self.request_cancellation = None;
                    self.send_c_cancel(context_id, message_id).await?;
                } else {
                    self.request_cancellation = None;
                }
            }
        };
        match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, read)
                .await
                .map_err(|_| NetworkError::RequestTimedOut { message_id })?,
            None => read.await,
        }
    }

    /// Reads PDUs until the response for `message_id` arrives (§11.2).
    #[allow(dead_code)]
    pub(crate) async fn await_response(
        &mut self,
        message_id: u16,
        expected: CommandField,
    ) -> Result<Command> {
        Ok(self
            .await_response_with_context(message_id, expected, false, None)
            .await?
            .1)
    }

    async fn await_response_for_context(
        &mut self,
        message_id: u16,
        expected: CommandField,
        expected_context_id: u8,
    ) -> Result<Command> {
        Ok(self
            .await_response_with_context(message_id, expected, false, Some(expected_context_id))
            .await?
            .1)
    }

    async fn await_response_with_context(
        &mut self,
        message_id: u16,
        expected: CommandField,
        reject_dataset: bool,
        expected_context_id: Option<u8>,
    ) -> Result<(u8, Command)> {
        loop {
            let Some(pdu) = self.read_request_pdu(message_id).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    for unit in self.push_pdata_offloaded(&pdata).await? {
                        // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                        // dropping it, before the response filtering below
                        if self.intercept_event_report(&unit).await? {
                            continue;
                        }
                        if unit.stream != Stream::Command {
                            if !reject_dataset {
                                continue;
                            }
                            return Err(NetworkError::protocol(
                                "response carried an unexpected dataset",
                            ));
                        }
                        let command = command_from_bytes(&unit.bytes)?;
                        if command.field == expected
                            && command.message_id_being_responded_to == Some(message_id)
                        {
                            if let Some(expected_context_id) = expected_context_id {
                                Self::validate_response_context(
                                    expected_context_id,
                                    unit.context_id,
                                    "C-ECHO",
                                )?;
                            }
                            return Ok((unit.context_id, command));
                        }
                    }
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort {
                        source_code: abort.source.code(),
                        reason: abort.reason,
                    })
                }
                Pdu::ReleaseRq => {
                    write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                _ => {}
            }
        }
    }

    pub(crate) fn validate_response_context(
        expected: u8,
        actual: u8,
        operation: &str,
    ) -> Result<()> {
        if expected != actual {
            let message = match operation {
                "C-ECHO" => "C-ECHO response used the wrong presentation context",
                "N-service" => "N-service response used the wrong presentation context",
                "C-FIND batch" => "C-FIND batch response used the wrong presentation context",
                _ => "response used the wrong presentation context",
            };
            return Err(NetworkError::protocol(message));
        }
        Ok(())
    }

    pub(crate) fn validate_response_sop_class(expected: &str, actual: Option<&str>) -> Result<()> {
        if actual.is_some_and(|actual| expected != actual) {
            return Err(NetworkError::protocol(
                "response used the wrong SOP Class UID",
            ));
        }
        Ok(())
    }

    pub(crate) fn take_pending_dataset<T>(
        pending: &mut Option<T>,
        message: &'static str,
    ) -> Result<T> {
        pending
            .take()
            .ok_or_else(|| NetworkError::protocol(message))
    }

    pub(crate) fn put_pending_dataset<T>(
        pending: &mut Option<T>,
        value: T,
        message: &'static str,
    ) -> Result<()> {
        if pending.is_some() {
            return Err(NetworkError::protocol(message));
        }
        *pending = Some(value);
        Ok(())
    }

    /// Sends a C-CANCEL-RQ (0x0FFF) for an in-flight request (§11.4, SRV-08).
    ///
    /// The command carries the target `message_id` in
    /// `(0000,0120) MessageIDBeingRespondedTo`, matching fo-dicom's
    /// `CCancelRequest = 0x0FFF`. It is sent on the accepted `context_id`
    /// used by the request being cancelled.
    /// The streaming FIND/MOVE/GET handlers return a terminal `CANCEL`
    /// response when this command names their active request.
    pub async fn send_c_cancel(&mut self, context_id: u8, message_id: u16) -> Result<()> {
        let mut command = Command::request(CommandField::CCancelRq, message_id);
        // a C-CANCEL identifies the cancelled request in (0000,0120)
        command.message_id_being_responded_to = Some(message_id);
        let bytes = bytes::Bytes::from(command_bytes(&command)?);
        for pdata in self.splitter.split_message_bytes(context_id, bytes, None)? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }
        Ok(())
    }

    /// Sends an N-service request and returns its response (§11.4, SRV-07).
    ///
    /// The presentation context is chosen from the request's SOP class. The
    /// Requested/Affected tags follow the command field (PS3.7 §10.1). A
    /// request/response dataset is transferred in the negotiated syntax.
    pub async fn send_n_service(
        &mut self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
        let mut request = request.clone();
        request.message_id = self.message_id_or_allocate(request.message_id)?;
        let message_id = request.message_id;
        let sop_class = request
            .sop_class_uid
            .as_deref()
            .ok_or_else(|| NetworkError::protocol("N-service request has no SOP class UID"))?;
        let request_syntax = request
            .dataset
            .as_ref()
            .map(|d| d.transfer_syntax())
            .unwrap_or_else(|| syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        let context = self.contexts.pick(sop_class, request_syntax.uid())?;
        let context_id = context.id;
        let target = transfer_syntax_of(&context.transfer_syntax)?;
        let expected = crate::dimse::response_field(request.field)
            .ok_or_else(|| NetworkError::protocol("N-service request has no response field"))?;

        let command = request.to_command();
        let command_bytes =
            bytes::Bytes::from(crate::message::encode_command(&command.to_dataset())?);
        let payload = match &request.dataset {
            Some(dataset) => Some(bytes::Bytes::from(crate::message::encode_dataset(
                dataset, &target,
            )?)),
            None => None,
        };
        for pdata in self
            .splitter
            .split_message_bytes(context_id, command_bytes, payload)?
        {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }
        self.pending.insert(message_id);

        let response = self
            .await_n_response(message_id, expected, &target, context_id)
            .await;
        self.pending.remove(message_id);
        let (response_command, response_dataset) = response?;
        Self::validate_response_sop_class(
            sop_class,
            response_command.affected_sop_class_uid.as_deref(),
        )?;
        if let (Some(expected), Some(actual)) = (
            &request.sop_instance_uid,
            &response_command.affected_sop_instance_uid,
        ) {
            if expected != actual {
                return Err(NetworkError::protocol(
                    "N-service response used the wrong SOP Instance UID",
                ));
            }
        }
        Ok(crate::nservice::NServiceResponse::from_command(
            &response_command,
            response_dataset,
        ))
    }

    /// Reads the N-service response command (plus any dataset) for `message_id`.
    async fn await_n_response(
        &mut self,
        message_id: u16,
        expected: CommandField,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
        expected_context_id: u8,
    ) -> Result<(Command, Option<Dataset>)> {
        let mut pending_command: Option<Command> = None;
        loop {
            let Some(pdu) = self.read_request_pdu(message_id).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    for unit in self.push_pdata_offloaded(&pdata).await? {
                        // H2: answer an incoming N-EVENT-REPORT-RQ instead of
                        // dropping it, before the response filtering below
                        if self.intercept_event_report(&unit).await? {
                            continue;
                        }
                        match unit.stream {
                            Stream::Command => {
                                let command = command_from_bytes(&unit.bytes)?;
                                if command.field == expected
                                    && command.message_id_being_responded_to == Some(message_id)
                                {
                                    Self::validate_response_context(
                                        expected_context_id,
                                        unit.context_id,
                                        "N-service",
                                    )?;
                                    if command.no_dataset {
                                        if pending_command.is_some() {
                                            return Err(NetworkError::protocol(
                                                "N-service response arrived before its pending dataset",
                                            ));
                                        }
                                        return Ok((command, None));
                                    }
                                    Self::put_pending_dataset(
                                        &mut pending_command,
                                        command,
                                        "N-service response carried overlapping datasets",
                                    )?;
                                }
                            }
                            Stream::Data => {
                                let command = Self::take_pending_dataset(
                                    &mut pending_command,
                                    "N-service dataset without its response",
                                )?;
                                Self::validate_response_context(
                                    expected_context_id,
                                    unit.context_id,
                                    "N-service",
                                )?;
                                let dataset = self.decode_response_body(&unit, syntax).await?;
                                return Ok((command, Some(dataset)));
                            }
                        }
                    }
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort {
                        source_code: abort.source.code(),
                        reason: abort.reason,
                    })
                }
                Pdu::ReleaseRq => {
                    write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                _ => {}
            }
        }
    }

    /// Releases the association and waits for the peer's A-RELEASE-RP (§9.3.6).
    ///
    /// A peer that answers with an A-ABORT, with something else, or not at all
    /// within `release_timeout` did not release: reporting success there would
    /// hide a broken peer, so each case is an error.
    pub async fn release(mut self) -> Result<()> {
        write_pdu(&mut self.stream, &Pdu::ReleaseRq).await?;
        let release_timeout = self.release_timeout;
        let response =
            tokio::time::timeout(release_timeout, self.pdu_reader.read_pdu(&mut self.stream))
                .await
                .map_err(|_| NetworkError::ReleaseTimedOut {
                    timeout_ms: release_timeout.as_millis() as u64,
                })??;
        match response {
            Some(Pdu::ReleaseRp) => Ok(()),
            Some(Pdu::Abort(abort)) => Err(NetworkError::Abort {
                source_code: abort.source.code(),
                reason: abort.reason,
            }),
            Some(_) => Err(NetworkError::protocol("expected A-RELEASE-RP")),
            None => Err(NetworkError::ClosedPrematurely { source: None }),
        }
    }

    /// Aborts the association (§11.2).
    pub async fn abort(mut self, reason: u8) -> Result<()> {
        let abort = Abort {
            source: AbortSource::User,
            reason,
        };
        write_pdu(&mut self.stream, &Pdu::Abort(abort)).await
    }

    /// Allocates a message ID for a request this side sends (§11.2).
    ///
    /// IDs still awaiting a response are skipped, so the wrap at 65535 can
    /// never hand an in-flight ID to a new request. Public because a caller
    /// building its own N-service request has to stamp one (H2).
    pub fn next_message_id(&mut self) -> Result<u16> {
        self.allocate_message_id()
    }

    /// Allocates a message ID for a request this side sends (§11.2).
    ///
    /// IDs still awaiting a response are skipped, so the wrap at 65535 can
    /// never hand an in-flight ID to a new request.
    pub(crate) fn allocate_message_id(&mut self) -> Result<u16> {
        self.pending.allocate_message_id()
    }

    fn message_id_or_allocate(&mut self, message_id: u16) -> Result<u16> {
        let message_id = if message_id == 0 {
            self.allocate_message_id()
        } else {
            Ok(message_id)
        }?;
        if self.pending.contains(message_id) {
            return Err(NetworkError::protocol("message ID is already in use"));
        }
        Ok(message_id)
    }

    fn reserve_explicit_ids<I>(&mut self, ids: I) -> Result<Vec<u16>>
    where
        I: IntoIterator<Item = u16>,
    {
        let mut explicit = std::collections::HashSet::new();
        for id in ids {
            if id != 0 && (!explicit.insert(id) || self.pending.contains(id)) {
                return Err(NetworkError::protocol(
                    "batch message ID is already in use or repeated",
                ));
            }
        }
        let reserved: Vec<_> = explicit.into_iter().collect();
        for id in &reserved {
            self.pending.insert(*id);
        }
        Ok(reserved)
    }
}

#[cfg(test)]
mod mixed_response_tests {
    use super::*;

    fn pending(kind: crate::batch::RequestKind) -> MixedPending {
        MixedPending {
            request_index: 0,
            message_id: 7,
            context_id: 1,
            kind,
            expected_field: CommandField::CEchoRsp,
            expected_sop_class_uid: None,
            expected_sop_instance_uid: None,
            syntax: syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
        }
    }

    fn emit_error(
        pending: MixedPending,
        command: Command,
        dataset: Option<radicom::Dataset>,
    ) -> NetworkError {
        let mut table = crate::engine::PendingTable::new();
        let mut inflight = std::collections::HashMap::from([(7, pending.clone())]);
        Association::emit_mixed_response(
            &mut table,
            pending,
            command,
            dataset,
            &mut inflight,
            &mut |_| Ok(()),
        )
        .expect_err("malformed response must be rejected")
    }

    fn emit_ok(pending: MixedPending, command: Command, dataset: Option<radicom::Dataset>) {
        let message_id = pending.message_id;
        let mut table = crate::engine::PendingTable::new();
        let mut inflight = std::collections::HashMap::from([(message_id, pending.clone())]);
        Association::emit_mixed_response(
            &mut table,
            pending,
            command,
            dataset,
            &mut inflight,
            &mut |_| Ok(()),
        )
        .expect("valid response");
    }

    #[test]
    fn typed_response_distinguishes_omitted_and_supplied_uids() {
        assert!(Association::validate_response_sop_class("1.2.3", None).is_ok());
        assert!(Association::validate_response_sop_class("1.2.3", Some("1.2.3")).is_ok());
        assert!(Association::validate_response_sop_class("1.2.3", Some("")).is_err());
        assert!(Association::validate_response_sop_class("1.2.3", Some("1.2.4")).is_err());
    }

    #[test]
    fn mixed_responses_accept_omitted_optional_uids() {
        for (kind, field) in [
            (crate::batch::RequestKind::Echo, CommandField::CEchoRq),
            (crate::batch::RequestKind::Store, CommandField::CStoreRq),
            (crate::batch::RequestKind::Find, CommandField::CFindRq),
            (crate::batch::RequestKind::Get, CommandField::CGetRq),
            (crate::batch::RequestKind::Move, CommandField::CMoveRq),
            (crate::batch::RequestKind::NService, CommandField::NSetRq),
        ] {
            let request = Command::request(field, 7);
            let response = Command::response(&request, crate::dimse::status::SUCCESS).unwrap();
            let mut pending = pending(kind);
            pending.expected_sop_class_uid = Some("1.2.3".into());
            pending.expected_sop_instance_uid = Some("1.2.3.4".into());
            emit_ok(pending, response, None);
        }
    }

    #[test]
    fn mixed_n_responses_reject_supplied_wrong_uids() {
        for wrong_class in [false, true] {
            let request = Command::request(CommandField::NSetRq, 7);
            let mut response = Command::response(&request, crate::dimse::status::SUCCESS).unwrap();
            if wrong_class {
                response.affected_sop_class_uid = Some("1.2.5".into());
            } else {
                response.affected_sop_instance_uid = Some("1.2.3.5".into());
            }
            let mut pending = pending(crate::batch::RequestKind::NService);
            pending.expected_sop_class_uid = Some("1.2.3".into());
            pending.expected_sop_instance_uid = Some("1.2.3.4".into());
            assert!(matches!(
                emit_error(pending, response, None),
                NetworkError::Protocol(_)
            ));
        }
    }

    #[test]
    fn mixed_response_rejects_datasets_for_echo_and_store() {
        let echo_request = Command::request(CommandField::CEchoRq, 7);
        let echo_response =
            Command::response(&echo_request, crate::dimse::status::SUCCESS).unwrap();
        assert!(matches!(
            emit_error(
                pending(crate::batch::RequestKind::Echo),
                echo_response,
                Some(radicom::Dataset::new()),
            ),
            NetworkError::Protocol(_)
        ));

        let store_request = Command::request(CommandField::CStoreRq, 7);
        let store_response =
            Command::response(&store_request, crate::dimse::status::SUCCESS).unwrap();
        assert!(matches!(
            emit_error(
                pending(crate::batch::RequestKind::Store),
                store_response,
                Some(radicom::Dataset::new()),
            ),
            NetworkError::Protocol(_)
        ));
    }

    #[test]
    fn mixed_response_rejects_pending_get_and_move_identifiers() {
        let mut get_request = Command::request(CommandField::CGetRq, 7);
        get_request.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.2".to_string());
        let mut get_response =
            Command::response(&get_request, crate::dimse::status::PENDING).unwrap();
        get_response.remaining_suboperations = Some(1);
        get_response.completed_suboperations = Some(0);
        get_response.failed_suboperations = Some(0);
        get_response.warning_suboperations = Some(0);
        let mut get_pending = pending(crate::batch::RequestKind::Get);
        get_pending.expected_field = CommandField::CGetRsp;
        assert!(matches!(
            emit_error(get_pending, get_response, Some(radicom::Dataset::new())),
            NetworkError::Protocol(_)
        ));

        let mut move_request = Command::request(CommandField::CMoveRq, 7);
        move_request.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.2".to_string());
        let mut move_response =
            Command::response(&move_request, crate::dimse::status::PENDING).unwrap();
        move_response.remaining_suboperations = Some(1);
        move_response.completed_suboperations = Some(0);
        move_response.failed_suboperations = Some(0);
        move_response.warning_suboperations = Some(0);
        let mut move_pending = pending(crate::batch::RequestKind::Move);
        move_pending.expected_field = CommandField::CMoveRsp;
        assert!(matches!(
            emit_error(move_pending, move_response, Some(radicom::Dataset::new())),
            NetworkError::Protocol(_)
        ));
    }

    #[test]
    fn mixed_response_rejects_wrong_sop_uids() {
        let mut echo_request = Command::request(CommandField::CEchoRq, 7);
        echo_request.affected_sop_class_uid = Some("1.2.3".to_string());
        let echo_response =
            Command::response(&echo_request, crate::dimse::status::SUCCESS).unwrap();
        let mut echo_pending = pending(crate::batch::RequestKind::Echo);
        echo_pending.expected_sop_class_uid = Some("1.2.4".to_string());
        assert!(matches!(
            emit_error(echo_pending, echo_response, None),
            NetworkError::Protocol(_)
        ));

        let mut store_request = Command::request(CommandField::CStoreRq, 7);
        store_request.affected_sop_class_uid = Some("1.2.3".to_string());
        store_request.affected_sop_instance_uid = Some("1.2.3.4".to_string());
        let mut store_response =
            Command::response(&store_request, crate::dimse::status::SUCCESS).unwrap();
        store_response.affected_sop_instance_uid = Some("1.2.3.5".to_string());
        let mut store_pending = pending(crate::batch::RequestKind::Store);
        store_pending.expected_field = CommandField::CStoreRsp;
        store_pending.expected_sop_class_uid = Some("1.2.3".to_string());
        store_pending.expected_sop_instance_uid = Some("1.2.3.4".to_string());
        assert!(matches!(
            emit_error(store_pending, store_response, None),
            NetworkError::Protocol(_)
        ));
    }

    #[test]
    fn mixed_response_attaches_final_get_and_move_identifiers() {
        let mut get_request = Command::request(CommandField::CGetRq, 7);
        get_request.affected_sop_class_uid = Some("1.2.3".to_string());
        let mut get_response =
            Command::response(&get_request, crate::dimse::status::SUCCESS).unwrap();
        get_response.remaining_suboperations = Some(0);
        get_response.completed_suboperations = Some(1);
        get_response.failed_suboperations = Some(0);
        get_response.warning_suboperations = Some(0);
        let mut get_pending = pending(crate::batch::RequestKind::Get);
        get_pending.expected_field = CommandField::CGetRsp;
        get_pending.expected_sop_class_uid = Some("1.2.3".to_string());
        emit_ok(get_pending, get_response, Some(radicom::Dataset::new()));

        let mut move_request = Command::request(CommandField::CMoveRq, 7);
        move_request.affected_sop_class_uid = Some("1.2.3".to_string());
        let mut move_response =
            Command::response(&move_request, crate::dimse::status::SUCCESS).unwrap();
        move_response.remaining_suboperations = Some(0);
        move_response.completed_suboperations = Some(1);
        move_response.failed_suboperations = Some(0);
        move_response.warning_suboperations = Some(0);
        let mut move_pending = pending(crate::batch::RequestKind::Move);
        move_pending.expected_field = CommandField::CMoveRsp;
        move_pending.expected_sop_class_uid = Some("1.2.3".to_string());
        emit_ok(move_pending, move_response, Some(radicom::Dataset::new()));
    }
}
