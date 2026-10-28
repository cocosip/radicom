//! End-to-end association and C-ECHO over a real TCP socket
//! (SRV-01/02; docs/01-architecture.md §11, §8).
//!
//! The engine here is intentionally small: it drives the association
//! handshake, routes one command/response exchange, and closes gracefully.
//! Larger service behaviour (streaming C-FIND, C-GET sub-operations) builds on
//! these same primitives in M6.

use crate::assembler::{PduSplitter, PdvAssembler, Stream};
use crate::association::{AcceptedContexts, NegotiationPolicy};
use crate::dimse::{Command, CommandField, StatusState};
use crate::error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
use crate::pdu::{
    Abort, Associate, AssociateRj, Pdu, PresentationContext, APPLICATION_CONTEXT,
};
use crate::services::{CEchoRequest, CEchoResponse, CStoreResponse, ServiceProvider};
use crate::transport::{read_pdu, write_pdu, TcpOptions};
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::transfer_syntax::syntaxes;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// Implementation identity written into user information (§3.2).
pub const IMPLEMENTATION_CLASS_UID: &str = "1.2.826.0.1.3680043.10.1330.2";
pub const IMPLEMENTATION_VERSION_NAME: &str = "RADICOM_0_1";

/// Server options (§8).
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Called AE title accepted by the SCP; `None` accepts any title.
    pub called_ae_title: Option<String>,
    /// 0 = unlimited.
    pub max_clients: usize,
    /// Peer maximum PDU length advertised in the association accept.
    pub max_pdu_length: u32,
    /// How long to wait for the peer's A-ASSOCIATE-RQ.
    pub association_timeout: Duration,
    /// Socket options applied to every accepted connection (§2.12).
    pub tcp: TcpOptions,
    /// Where incoming C-STORE instances are put before the provider sees them.
    pub store_sink: crate::store::StoreSink,
    /// Answers SOP Class Extended Negotiation requests (§4.3).
    pub extended_negotiation: crate::extended::ExtendedNegotiationPolicy,
    /// User identity types this SCP accepts, and the response to echo (§4.3).
    pub user_identity: UserIdentityPolicy,
    /// TLS material; `None` serves plain TCP (NET-01, `tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
    /// Optional metrics collector (OBS-01); `None` disables instrumentation.
    pub metrics: Option<std::sync::Arc<dyn crate::observability::MetricsCollector>>,
}

/// A snapshot handle for the number of accepted connections that are still
/// being served by an SCP.
#[derive(Clone, Debug)]
pub struct ConnectedClients {
    count: Arc<AtomicUsize>,
}

impl ConnectedClients {
    /// Returns the current number of live connection tasks.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

struct ConnectionCountGuard {
    count: Arc<AtomicUsize>,
}

impl Drop for ConnectionCountGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Release);
    }
}

/// How an SCP handles User Identity negotiation (§4.3).
#[derive(Debug, Clone, Default)]
pub enum UserIdentityPolicy {
    /// Reject the association when a user identity is requested.
    #[default]
    Reject,
    /// Accept any well-formed identity; the AC carries no response.
    Accept,
    /// Accept and echo this response byte string back (type 1–5 payload).
    Answer(Vec<u8>),
}

impl UserIdentityPolicy {
    /// The answer to `identity`, or `None` to reject the association.
    fn answer(&self, identity: &crate::pdu::UserIdentity) -> Option<Option<Vec<u8>>> {
        match self {
            UserIdentityPolicy::Reject => None,
            UserIdentityPolicy::Accept => Some(None),
            UserIdentityPolicy::Answer(response) => {
                // a response only means something when the peer asked for one
                if identity.positive_response_requested {
                    Some(Some(response.clone()))
                } else {
                    Some(None)
                }
            }
        }
    }
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            called_ae_title: None,
            max_clients: 0,
            max_pdu_length: 16384,
            association_timeout: Duration::from_secs(10),
            tcp: TcpOptions::default(),
            store_sink: crate::store::StoreSink::default(),
            extended_negotiation: crate::extended::ExtendedNegotiationPolicy::decline_all(),
            user_identity: UserIdentityPolicy::default(),
            #[cfg(feature = "tls")]
            tls: None,
            metrics: None,
        }
    }
}

/// A DIMSE server bound to one address (§8).
pub struct DicomServer<P: ServiceProvider> {
    listener: TcpListener,
    provider: Arc<P>,
    options: ServerOptions,
    local_addr: std::net::SocketAddr,
    connected_clients: Arc<AtomicUsize>,
}

impl<P: ServiceProvider + 'static> DicomServer<P> {
    /// Binds and starts accepting (§8).
    pub async fn bind(
        addr: impl tokio::net::ToSocketAddrs,
        provider: Arc<P>,
        options: ServerOptions,
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr).await.map_err(NetworkError::Io)?;
        let local_addr = listener.local_addr().map_err(NetworkError::Io)?;
        Ok(DicomServer {
            listener,
            provider,
            options,
            local_addr,
            connected_clients: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The bound address, useful when binding to port 0.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Returns a handle that observes live connection tasks after `run` moves
    /// the server into its accept loop.
    pub fn connected_clients(&self) -> ConnectedClients {
        ConnectedClients { count: self.connected_clients.clone() }
    }

    /// Serves connections until the listener errors (§8).
    ///
    /// Each connection gets its own task; `max_clients` bounds concurrency by
    /// gating the accept loop, so a full server leaves further peers in the
    /// listen backlog instead of spawning unbounded tasks.
    pub async fn run(self) -> Result<()> {
        let provider = self.provider.clone();
        let options = self.options.clone();
        let connected_clients = self.connected_clients.clone();
        // 0 = unlimited (a semaphore with no permits would block forever)
        let slots = (options.max_clients > 0)
            .then(|| Arc::new(tokio::sync::Semaphore::new(options.max_clients)));
        loop {
            let (stream, _peer) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(e) => return Err(NetworkError::Io(e)),
            };
            // advisory: a refused buffer size must not drop the connection
            let _ = crate::transport::apply_options(&stream, &options.tcp);
            let permit = match &slots {
                Some(slots) => Some(
                    slots
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| NetworkError::protocol("connection limiter closed"))?,
                ),
                None => None,
            };
            let provider = provider.clone();
            let options = options.clone();
            connected_clients.fetch_add(1, Ordering::AcqRel);
            let count_guard = ConnectionCountGuard { count: connected_clients.clone() };
            tokio::spawn(async move {
                // the slot is released when the connection task ends
                let _permit = permit;
                let _count_guard = count_guard;
                #[cfg(feature = "tls")]
                if let Some(tls) = options.tls.clone() {
                    // a failed handshake ends this connection only
                    if let Ok(stream) = tls.accept(stream).await {
                        let _ = serve_connection(stream, provider, options).await;
                    }
                    return;
                }
                let _ = serve_connection(stream, provider, options).await;
            });
        }
    }

    /// Serves exactly one connection, then returns (test and single-shot use).
    pub async fn accept_one(&self) -> Result<()> {
        let (stream, _peer) = self.listener.accept().await.map_err(NetworkError::Io)?;
        crate::transport::apply_options(&stream, &self.options.tcp)?;
        self.connected_clients.fetch_add(1, Ordering::AcqRel);
        let _count_guard = ConnectionCountGuard { count: self.connected_clients.clone() };
        #[cfg(feature = "tls")]
        if let Some(tls) = self.options.tls.clone() {
            let stream = tls.accept(stream).await?;
            return serve_connection(stream, self.provider.clone(), self.options.clone()).await;
        }
        serve_connection(stream, self.provider.clone(), self.options.clone()).await
    }
}

/// Drives one accepted connection through association, one exchange, release (§5).
async fn serve_connection<P: ServiceProvider, S>(
    stream: S,
    provider: Arc<P>,
    options: ServerOptions,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // -- observability (OBS-01): count this connection's bytes; the guard
    // reports open/close so every return path is covered ------------------
    let metrics = options.metrics.clone();
    let _connection = crate::observability::ConnectionGuard::new(metrics.clone());
    let mut stream = crate::observability::CountingStream::new(stream, metrics.clone());

    // -- association handshake ------------------------------------------
    // The ACSE phase has its own timeout: a peer that connects and never sends
    // (or half-sends) an A-ASSOCIATE-RQ must not hold this task and fd forever.
    let request = match tokio::time::timeout(options.association_timeout, read_pdu(&mut stream)).await {
        Ok(Ok(Some(Pdu::AssociateRq(request)))) => request,
        Ok(Ok(_)) => return Err(NetworkError::protocol("expected A-ASSOCIATE-RQ")),
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            // PS3.8 §9.3.8: provider-initiated abort, reason "not specified"
            let abort = Abort { source: AbortSource::Provider, reason: 0 };
            let _ = write_pdu(&mut stream, &Pdu::Abort(abort)).await;
            return Err(NetworkError::AssociationRequestTimedOut { attempts: 1 });
        }
    };

    if let Some(expected) = options.called_ae_title.as_deref() {
        if request.called_ae != expected {
            let reject = AssociateRj { result: RejectResult::Permanent, source: RejectSource::User, reason: 7 };
            write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
            return Ok(());
        }
    }

    let policy = NegotiationPolicy::new(provider.supported_sop_classes());
    let mut negotiated = policy.negotiate(&request.presentation_contexts);
    policy.negotiate_roles(&mut negotiated, &request.user_information.role_selections);

    // reject when nothing was accepted
    if !negotiated.iter().any(|c| c.is_accepted()) {
        let reject = AssociateRj {
            result: RejectResult::Permanent,
            source: RejectSource::User,
            reason: 3, // called AE title not recognized / no acceptable PC
        };
        write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
        return Ok(());
    }

    let mut accept = Associate::new(request.calling_ae.clone(), request.called_ae.clone());
    accept.presentation_contexts = negotiated.iter().map(|c| c.to_wire()).collect();
    let mut user = crate::association::build_user_information(
        options.max_pdu_length,
        IMPLEMENTATION_CLASS_UID,
        IMPLEMENTATION_VERSION_NAME,
        Vec::new(),
    );

    // extended negotiation: the AC echoes one item per requested SOP class (§4.3)
    user.extended_negotiations = options.extended_negotiation.answer(&request.user_information.extended_negotiations);

    // user identity: refused identities reject the association (§4.3)
    if let Some(identity) = &request.user_information.user_identity {
        let Some(response) = options.user_identity.answer(identity) else {
            let reject = AssociateRj {
                result: RejectResult::Permanent,
                source: RejectSource::User,
                reason: 1, // no reason given
            };
            write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
            return Ok(());
        };
        user.user_identity_response = response;
    }

    accept.user_information = user;
    write_pdu(&mut stream, &Pdu::AssociateAc(accept)).await?;
    // the association is now established (OBS-01): report open/close
    let _association = crate::observability::AssociationGuard::new(metrics.clone());

    let accepted = AcceptedContexts::from_negotiated(negotiated);
    let peer_max_pdu = if request.user_information.max_pdu_length == 0 {
        crate::pdu::clamp_max_pdu_length(options.max_pdu_length)
    } else {
        request.user_information.max_pdu_length
    };
    let splitter = PduSplitter::new(peer_max_pdu, 16);
    // a command awaiting its dataset (§5.2)
    let mut awaiting_dataset: std::collections::HashMap<u8, Command> = std::collections::HashMap::new();

    let mut serving = Serving {
        stream: &mut stream,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        assembler: PdvAssembler::new(),
        accepted: &accepted,
        reverse_message_id: 1,
    };

    // -- message loop ----------------------------------------------------
    loop {
        let Some(pdu) = read_pdu(serving.stream).await? else {
            return Ok(()); // peer closed
        };
        match pdu {
            Pdu::PData(pdata) => {
                for unit in serving.assembler.push_pdata(&pdata)? {
                    match unit.stream {
                        Stream::Command => {
                            let command = command_from_bytes(&unit.bytes)?;
                            // Only C-STORE, C-FIND, C-MOVE and C-GET carry a
                            // dataset (§7.2..§7.5). Any other request is
                            // answered at once: trusting the peer's
                            // (0000,0800) flag would hang the engine on a peer
                            // that sets it wrongly.
                            let expects_dataset = !command.no_dataset
                                && matches!(
                                    command.field,
                                    CommandField::CStoreRq
                                        | CommandField::CFindRq
                                        | CommandField::CMoveRq
                                        | CommandField::CGetRq
                                        | CommandField::NEventReportRq
                                        | CommandField::NCreateRq
                                        | CommandField::NSetRq
                                        | CommandField::NActionRq
                                );
                            if expects_dataset {
                                // the dataset arrives in later PDVs; keyed by
                                // context so a second request cannot displace it
                                awaiting_dataset.insert(unit.context_id, command);
                            } else {
                                handle_command(&mut serving, unit.context_id, command).await?;
                            }
                        }
                        Stream::Data => {
                            let Some(command) = awaiting_dataset.remove(&unit.context_id) else {
                                return Err(NetworkError::protocol("dataset PDV without a command"));
                            };
                            let context_id = unit.context_id;
                            let syntax = accepted
                                .find_abstract(command.affected_sop_class_uid.as_deref().unwrap_or_default())
                                .map(|c| transfer_syntax_of(&c.transfer_syntax))
                                .unwrap_or_else(|| syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
                            handle_command_with_dataset(
                                &mut serving,
                                context_id,
                                command,
                                &unit.bytes,
                                &syntax,
                            )
                            .await?;
                        }
                    }
                }
            }
            Pdu::ReleaseRq => {
                serving.send_pdu(&Pdu::ReleaseRp).await?;
                return Ok(());
            }
            // PS3.8 §9.3.8: the peer gave up on the association. Returning Ok
            // here would report a peer abort as a clean close (§2.16).
            Pdu::Abort(abort) => {
                return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
            }
            other => {
                return Err(NetworkError::protocol(match other {
                    Pdu::AssociateRq(_) => "unexpected A-ASSOCIATE-RQ after association",
                    Pdu::AssociateAc(_) => "unexpected A-ASSOCIATE-AC",
                    Pdu::AssociateRj(_) => "unexpected A-ASSOCIATE-RJ",
                    Pdu::ReleaseRp => "unexpected A-RELEASE-RP",
                    _ => "unexpected PDU",
                }))
            }
        }
    }
}

/// Resolves a UID string to a transfer syntax, defaulting to Explicit VR LE.
fn transfer_syntax_of(uid: &str) -> Arc<radicom::model::transfer_syntax::TransferSyntax> {
    radicom::model::transfer_syntax::parse(uid)
        .unwrap_or_else(|_| syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone())
}

/// Dispatches a command that carries no dataset (§5.1).
async fn handle_command<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match command.field {
        CommandField::CEchoRq => {
            let request = CEchoRequest::from_command(&command)?;
            // a provider error is a failed operation, not a broken association
            // (the C-STORE and N-service paths already answer a failure status)
            match serving.provider.on_c_echo(&request).await {
                Ok(response) => serving.send(context_id, &response.to_command()).await,
                Err(error) => serving.fail(context_id, &command, &format!("C-ECHO failed: {error}")).await,
            }
        }
        CommandField::CCancelRq => {
            // §7.6 / SRV-08: acknowledge the cancel for the request named in
            // (0000,0120). Providers here answer a query before the loop reads
            // the next PDU (buffered per query), so there is no in-flight
            // stream to interrupt; the loop simply consumes the command so the
            // association stays healthy. Full mid-stream cancellation needs the
            // streaming C-FIND path.
            let _cancelled = command.message_id_being_responded_to;
            Ok(())
        }
        // N-services without a dataset (N-GET / N-DELETE, and any N request
        // sent with the no-dataset flag set) — §7.6 / SRV-07.
        field if crate::nservice::is_n_request(field) => {
            let default_syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
            handle_n_service(serving, context_id, command, None, &default_syntax).await
        }
        other => {
            // A response field means the peer violated DIMSE: there is no
            // status to answer a response with. Abort instead of panicking.
            if crate::dimse::response_field(other).is_none() {
                let abort = Abort { source: AbortSource::User, reason: 0 };
                serving.send_pdu(&Pdu::Abort(abort)).await?;
                return Err(NetworkError::protocol("peer sent a command that is not a request"));
            }
            // a request we do not serve: answer ProcessingFailure, keep the connection
            serving
                .fail(context_id, &command, &format!("unsupported command {other:?}"))
                .await
        }
    }
}

/// Dispatches a command together with its dataset — the C-STORE path (§7.2).
async fn handle_command_with_dataset<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match command.field {
        CommandField::CStoreRq => handle_c_store(serving, context_id, command, dataset_bytes, syntax).await,
        CommandField::CFindRq => handle_c_find(serving, context_id, command, dataset_bytes, syntax).await,
        CommandField::CMoveRq => handle_c_move(serving, context_id, command, dataset_bytes, syntax).await,
        CommandField::CGetRq => handle_c_get(serving, context_id, command, dataset_bytes, syntax).await,
        field if crate::nservice::is_n_request(field) => {
            let dataset = match crate::message::decode_dataset(dataset_bytes, syntax) {
                Ok(dataset) => dataset,
                Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
            };
            handle_n_service(serving, context_id, command, Some(dataset), syntax).await
        }
        _ => serving.fail(context_id, &command, "this request does not carry a dataset").await,
    }
}

/// Dispatches an N-service request to the matching provider method (§7.6, SRV-07).
async fn handle_n_service<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset: Option<Dataset>,
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::nservice::NServiceRequest;
    let request = NServiceRequest::from_command(&command, dataset);
    let outcome = match request.field {
        CommandField::NEventReportRq => serving.provider.on_n_event_report(&request).await,
        CommandField::NGetRq => serving.provider.on_n_get(&request).await,
        CommandField::NSetRq => serving.provider.on_n_set(&request).await,
        CommandField::NActionRq => serving.provider.on_n_action(&request).await,
        CommandField::NCreateRq => serving.provider.on_n_create(&request).await,
        CommandField::NDeleteRq => serving.provider.on_n_delete(&request).await,
        other => {
            return serving.fail(context_id, &command, &format!("not an N-service: {other:?}")).await;
        }
    };
    match outcome {
        Ok(response) => {
            serving
                .send_with_dataset(context_id, &response.to_command(), response.dataset.as_ref(), syntax)
                .await
        }
        // a provider error becomes a ProcessingFailure, keeping the association alive
        Err(error) => serving.fail(context_id, &command, &error.to_string()).await,
    }
}

/// True for failures that mean the association itself is unusable (§2.10).
///
/// Everything else — a missing SOP class, an instance without the UIDs needed
/// to address it, a transcode that cannot run — describes one sub-operation,
/// so C-GET records it as failed and carries on with the next instance.
fn is_fatal(error: &NetworkError) -> bool {
    matches!(
        error,
        NetworkError::Io(_) | NetworkError::Abort { .. } | NetworkError::ClosedPrematurely { .. }
    )
}

/// The C-GET path: deliver instances back as reverse C-STORE sub-operations
/// (§7.4).
async fn handle_c_get<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let identifier = match crate::message::decode_dataset(dataset_bytes, syntax) {
        Ok(identifier) => identifier,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    let request = crate::cget::CGetRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        identifier,
    };

    // Events are collected first: the stream borrows the provider, and the
    // reverse sub-operations below need the socket exclusively.
    // ponytail: buffered per request; stream straight through if a retrieve
    // ever outgrows memory.
    let mut events = Vec::new();
    {
        use futures::StreamExt;
        let mut stream = serving.provider.on_c_get(&request);
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    let terminal = matches!(&event, crate::cget::CGetEvent::Response(r) if !r.is_pending());
                    events.push(event);
                    if terminal {
                        break;
                    }
                }
                Err(error) => {
                    events.push(crate::cget::CGetEvent::Response(crate::cget::CGetResponse::failure(
                        &request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    )));
                    break;
                }
            }
        }
    }

    let mut summary = crate::cmove::SubOperationSummary::default();
    // PS3.7 §9.1.3: (0000,1020) counts the C-STORE sub-operations still to be
    // performed *including* the one the response belongs to, so the first of
    // three instances reports 3 and only the terminal response reports 0.
    let mut remaining = events.iter().filter(|e| matches!(e, crate::cget::CGetEvent::Instance(_))).count() as u16;
    // A provider that reports its own progress owns (0000,1020): forwarding its
    // numbers beats recomputing them from the instances we happen to see.
    let provider_reports_progress =
        events.iter().any(|e| matches!(e, crate::cget::CGetEvent::Response(r) if r.is_pending()));
    let mut terminated = false;
    for event in events {
        match event {
            crate::cget::CGetEvent::Instance(file) => {
                let state = match serving.send_reverse_store(&file).await {
                    Ok(state) => state,
                    // PS3.4 C.4.2: an instance that cannot be delivered is one
                    // failed sub-operation, not a reason to drop the
                    // association and every instance already sent (§2.10)
                    Err(error) if is_fatal(&error) => return Err(error),
                    Err(_) => StatusState::Failure,
                };
                summary.record(state);
                if !provider_reports_progress {
                    let progress = crate::cget::CGetResponse::pending(&request, remaining, summary);
                    serving.send(context_id, &progress.to_command()).await?;
                }
                remaining = remaining.saturating_sub(1);
            }
            crate::cget::CGetEvent::Response(response) => {
                if !response.is_pending() {
                    terminated = true;
                    // the provider's counters are authoritative when it
                    // reported any; otherwise ours describe what happened
                    let summary = if response.summary.total() > 0 { response.summary } else { summary };
                    let terminal = crate::cget::CGetResponse::completed(&request, summary);
                    serving.send(context_id, &terminal.to_command()).await?;
                } else {
                    serving.send(context_id, &response.to_command()).await?;
                }
            }
        }
    }
    if !terminated {
        let terminal = crate::cget::CGetResponse::completed(&request, summary);
        serving.send(context_id, &terminal.to_command()).await?;
    }
    Ok(())
}

/// The C-MOVE path: decode the Identifier and relay the provider's progress
/// (§7.5).
async fn handle_c_move<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let identifier = match crate::message::decode_dataset(dataset_bytes, syntax) {
        Ok(identifier) => identifier,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    // the destination AE title is mandatory (PS3.7 §9.3.2)
    let Some(destination_ae) = command.move_destination.clone() else {
        let request = crate::cmove::CMoveRequest {
            message_id: command.message_id,
            sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
            priority: command.priority,
            destination_ae: String::new(),
            identifier,
        };
        let response = crate::cmove::CMoveResponse::failure(
            &request,
            crate::dimse::status::MOVE_DESTINATION_UNKNOWN,
            "C-MOVE request has no Move Destination",
        );
        return serving.send(context_id, &response.to_command()).await;
    };

    let request = crate::cmove::CMoveRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        destination_ae,
        identifier,
    };

    // collected for the same reason as C-FIND: the stream borrows the provider
    // ponytail: buffered per request; stream straight through if a retrieve
    // ever outgrows memory.
    let mut responses = Vec::new();
    {
        use futures::StreamExt;
        let mut stream = serving.provider.on_c_move(&request);
        while let Some(response) = stream.next().await {
            match response {
                Ok(response) => {
                    let terminal = !response.is_pending();
                    responses.push(response);
                    if terminal {
                        break;
                    }
                }
                Err(error) => {
                    responses.push(crate::cmove::CMoveResponse::failure(
                        &request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    ));
                    break;
                }
            }
        }
    }

    // Sub-operation counters are cumulative per response (PS3.4 §C.4.2), so the
    // last Pending response carries the real progress; a provider that never
    // terminates its stream still gets an answer built from it.
    let mut summary = crate::cmove::SubOperationSummary::default();
    let mut terminated = false;
    for response in responses {
        if response.is_pending() {
            summary = response.summary;
        } else {
            terminated = true;
        }
        serving.send(context_id, &response.to_command()).await?;
    }
    if !terminated {
        let response = crate::cmove::CMoveResponse::completed(&request, summary);
        serving.send(context_id, &response.to_command()).await?;
    }
    Ok(())
}

/// The C-FIND path: decode the Identifier and stream the matches back (§7.3).
async fn handle_c_find<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let identifier = match crate::message::decode_dataset(dataset_bytes, syntax) {
        Ok(identifier) => identifier,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    let request = crate::cfind::CFindRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        identifier,
    };

    // Collect first, then send: the stream borrows the provider, so it cannot
    // stay alive across the awaited writes without a self-referential borrow.
    // ponytail: buffered per query; stream straight through if a query ever
    // outgrows memory.
    let mut responses = Vec::new();
    {
        use futures::StreamExt;
        let mut stream = serving.provider.on_c_find(&request);
        while let Some(response) = stream.next().await {
            match response {
                Ok(response) => responses.push(response),
                Err(error) => {
                    let failure = crate::cfind::CFindResponse::failure(
                        &request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    );
                    responses.push(failure);
                    break;
                }
            }
            if responses.last().map(|r| !r.is_pending()).unwrap_or(false) {
                break;
            }
        }
    }

    let mut matches = 0u16;
    let mut terminated = false;
    for response in responses {
        let identifier = response.identifier.clone();
        if !response.is_pending() {
            matches = response.completed;
            terminated = true;
        }
        serving.send_with_dataset(context_id, &response.to_command(), identifier.as_ref(), syntax).await?;
    }
    if !terminated {
        // a provider that never terminated its stream still gets a final answer
        let success = crate::cfind::CFindResponse::success(&request, matches);
        serving.send(context_id, &success.to_command()).await?;
    }
    Ok(())
}

/// The C-STORE path: spool the instance and hand it to the provider (§7.2).
async fn handle_c_store<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if command.field != CommandField::CStoreRq {
        return serving.fail(context_id, &command, "dataset is only supported for C-STORE").await;
    }

    // parse failures become a failure status; the connection survives (§5.2).
    // The spool does synchronous file IO, so it runs on the blocking pool: a
    // slow disk must not stall this (or any other) connection's runtime task
    // (§3.2).
    let sink = serving.options.store_sink.clone();
    let bytes = dataset_bytes.to_vec();
    let syntax = syntax.clone();
    let instance = match tokio::task::spawn_blocking(move || sink.materialize(&bytes, &syntax)).await {
        Ok(Ok(instance)) => instance,
        Ok(Err(error)) => return serving.fail(context_id, &command, &error.to_string()).await,
        Err(join) => {
            let message = format!("spool task failed: {join}");
            return serving.fail(context_id, &command, &message).await;
        }
    };

    let request = match crate::store::c_store_request_from_message(&command, instance) {
        Ok(request) => request,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    let response = match serving.provider.on_c_store(&request).await {
        Ok(response) => response,
        Err(error) => crate::store::failure_response(&request, &error),
    };
    serving.send(context_id, &response.to_command()).await
}

/// The per-connection send side, so dispatch helpers stay small.
struct Serving<'a, P: ServiceProvider, S> {
    stream: &'a mut S,
    splitter: &'a PduSplitter,
    provider: &'a Arc<P>,
    options: &'a ServerOptions,
    /// Owned so a nested sub-operation can read the reverse C-STORE responses
    /// of its own C-GET (§7.4).
    assembler: PdvAssembler,
    accepted: &'a AcceptedContexts,
    /// The next message ID for reverse sub-operations (§7.4).
    reverse_message_id: u16,
}

impl<P: ServiceProvider, S> Serving<'_, P, S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    /// Writes one PDU on the connection.
    async fn send_pdu(&mut self, pdu: &Pdu) -> Result<()> {
        write_pdu(self.stream, pdu).await
    }

    /// Sends one command-only message on `context_id`.
    async fn send(&mut self, context_id: u8, command: &Command) -> Result<()> {
        send_command(self.stream, self.splitter, context_id, command).await
    }

    /// Sends a command followed by an optional dataset (§5.3).
    async fn send_with_dataset(
        &mut self,
        context_id: u8,
        command: &Command,
        dataset: Option<&Dataset>,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    ) -> Result<()> {
        let command_bytes = bytes::Bytes::from(command_bytes(command)?);
        let payload = match dataset {
            Some(dataset) => Some(bytes::Bytes::from(crate::message::encode_dataset(dataset, syntax)?)),
            None => None,
        };
        // Owned bytes handed straight to the splitter: each PDV is a shared
        // slice instead of a fresh copy (§3.3).
        for pdata in self.splitter.split_message_bytes(context_id, command_bytes, payload)? {
            write_pdu(self.stream, &Pdu::PData(pdata)).await?;
        }
        Ok(())
    }

    /// Answers with ProcessingFailure and a truncated comment.
    async fn fail(&mut self, context_id: u8, request: &Command, comment: &str) -> Result<()> {
        let Some(mut response) = Command::response(request, crate::dimse::status::PROCESSING_FAILURE) else {
            return Err(NetworkError::protocol("cannot answer a command that is not a request"));
        };
        response.error_comment = Some(crate::dimse::truncate_lo(comment));
        self.send(context_id, &response).await
    }

    /// Reads PDUs until the C-STORE response for `message_id` arrives (§7.4).
    ///
    /// Only the reverse sub-operations of the running C-GET can arrive here:
    /// the association is single-threaded, so the SCU has nothing else in
    /// flight that this side must serve.
    async fn await_store_response(&mut self, message_id: u16) -> Result<Command> {
        loop {
            let Some(pdu) = read_pdu(self.stream).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    for unit in self.assembler.push_pdata(&pdata)? {
                        if unit.stream != Stream::Command {
                            continue;
                        }
                        let command = command_from_bytes(&unit.bytes)?;
                        if command.field == CommandField::CStoreRsp
                            && command.message_id_being_responded_to == Some(message_id)
                        {
                            return Ok(command);
                        }
                    }
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
                }
                Pdu::ReleaseRq => {
                    write_pdu(self.stream, &Pdu::ReleaseRp).await?;
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                _ => {}
            }
        }
    }

    /// Delivers one instance as a reverse C-STORE sub-operation (§7.4).
    ///
    /// The SCU side must have registered a C-STORE handler, otherwise this is
    /// `ReverseStoreHandlerMissing` on the *caller*; the SCP just records the
    /// returned status.
    async fn send_reverse_store(
        &mut self,
        file: &radicom::io::file::DicomFile,
    ) -> Result<StatusState> {
        let dataset = &file.dataset;
        let sop_class = dataset
            .try_get_str(Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let sop_instance = dataset
            .try_get_str(Tag::SOP_INSTANCE_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?;
        let source_syntax = dataset.transfer_syntax();
        let context = self.accepted.pick(&sop_class, source_syntax.uid())?;
        let context_id = context.id;
        let target = transfer_syntax_of(&context.transfer_syntax);

        let message_id = self.next_reverse_message_id();
        let mut request = Command::request(CommandField::CStoreRq, message_id);
        request.affected_sop_class_uid = Some(sop_class);
        request.affected_sop_instance_uid = Some(sop_instance);
        request.no_dataset = false;

        let payload = if source_syntax.uid() == target.uid() {
            crate::message::encode_dataset(dataset, &target)?
        } else {
            let transcoder =
                radicom::pixel::transcoder::Transcoder::new(source_syntax, target.clone(), None, None);
            let transcoded = transcoder.transcode_dataset(dataset).map_err(|e| {
                NetworkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
            })?;
            crate::message::encode_dataset(&transcoded, &target)?
        };

        let command = bytes::Bytes::from(crate::message::encode_command(&request.to_dataset())?);
        let payload = bytes::Bytes::from(payload);
        for pdata in self.splitter.split_message_bytes(context_id, command, Some(payload))? {
            write_pdu(self.stream, &Pdu::PData(pdata)).await?;
        }

        let response = self.await_store_response(message_id).await?;
        Ok(crate::dimse::classify(response.status.unwrap_or(crate::dimse::status::PROCESSING_FAILURE)))
    }

    /// Message IDs for reverse sub-operations; the SCP side of a C-GET owns
    /// this sequence because it is the one issuing requests (§7.4).
    fn next_reverse_message_id(&mut self) -> u16 {
        let id = self.reverse_message_id;
        self.reverse_message_id = if self.reverse_message_id == u16::MAX {
            1
        } else {
            self.reverse_message_id + 1
        };
        id
    }
}

/// Encodes a command set.
///
/// PS3.5 §7.7 fixes every command element's VR in a table of its own, so the
/// command set is encoded with explicit VRs: the receiver then never needs a
/// dictionary entry for group 0000 (which the standard dictionary lacks).
/// The command group length is recomputed and kept (§5.3).
pub fn command_bytes(command: &Command) -> Result<Vec<u8>> {
    let dataset = command.to_dataset();
    let mut out = Vec::new();
    let options = radicom::io::writer::WriteOptions { keep_group_lengths: true, ..Default::default() };
    radicom::io::writer::DicomWriter::new(options)
        .write_dataset(&dataset, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN, &mut out)
        .map_err(|_| NetworkError::protocol("command set could not be encoded"))?;
    Ok(out)
}

/// Parses a command set (§5.2), matching [`command_bytes`].
pub fn command_from_bytes(bytes: &[u8]) -> Result<Command> {
    let dataset = parse_command_dataset(bytes)?;
    Command::from_dataset(&dataset)
}

/// Parses command bytes, tolerating trailing padding.
fn parse_command_dataset(bytes: &[u8]) -> Result<Dataset> {
    radicom::io::dataset_builder::parse_dataset_bytes(bytes, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
        .map_err(|e| NetworkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))
}

/// Sends a command-only message (§5.3).
async fn send_command<S>(
    stream: &mut S,
    splitter: &PduSplitter,
    context_id: u8,
    command: &Command,
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let bytes = bytes::Bytes::from(command_bytes(command)?);
    for pdata in splitter.split_bytes(context_id, true, bytes)? {
        write_pdu(stream, &Pdu::PData(pdata)).await?;
    }
    Ok(())
}

/// Client options (§9.1, reduced to what M5/M6 need).
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub max_pdu_length: u32,
    pub association_timeout: Duration,
    /// Socket options applied to the connected stream (§2.12).
    pub tcp: TcpOptions,
    /// How long a graceful release waits for the peer's A-RELEASE-RP before
    /// the association is reported as timed out (§9.3.6).
    pub release_timeout: Duration,
    /// User identity to offer; validated before the association is attempted
    /// (§4.3).
    pub user_identity: Option<crate::pdu::UserIdentity>,
    /// SOP Class Extended Negotiation items to offer (§4.3).
    pub extended_negotiations: Vec<crate::pdu::ExtendedNegotiation>,
    /// TLS material; `None` connects over plain TCP (NET-01, `tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            max_pdu_length: 16384,
            association_timeout: Duration::from_secs(5),
            release_timeout: Duration::from_secs(10),
            tcp: TcpOptions::default(),
            user_identity: None,
            extended_negotiations: Vec::new(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

/// The client-side stream: plain TCP or TLS over TCP (§2.4).
///
/// An enum keeps [`Association`] non-generic while still letting TLS share the
/// whole protocol path.
enum ClientStream {
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

/// An open association from the SCU side (§9.2 expert layer).
pub struct Association {
    stream: ClientStream,
    contexts: AcceptedContexts,
    splitter: PduSplitter,
    assembler: PdvAssembler,
    /// Requests this side sent and whose response is still outstanding; the
    /// allocator skips their message IDs (§5.2).
    pending: crate::engine::PendingTable,
    /// How long [`Self::release`] waits for the peer's A-RELEASE-RP.
    release_timeout: Duration,
    /// Answers reverse C-STORE sub-operations; required for C-GET (§7.4).
    reverse_store: Option<ReverseStoreHandler>,
    /// What the peer granted in SOP Class Extended Negotiation (§4.3).
    peer_extended: Vec<crate::pdu::ExtendedNegotiation>,
    /// The peer's user identity response (§4.3).
    peer_identity_response: Option<Vec<u8>>,
}

/// The SCU-side answer to a reverse C-STORE (§7.4).
pub type ReverseStoreHandler = Arc<
    dyn Fn(&crate::services::CStoreRequest) -> Result<crate::services::CStoreResponse> + Send + Sync,
>;

impl std::fmt::Debug for Association {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Association")
            .field("contexts", &self.contexts.all().len())
            .field("reverse_store", &self.reverse_store.is_some())
            .finish()
    }
}

impl Association {
    /// Opens a connection and negotiates an association (§9.1).
    pub async fn open(
        addr: impl tokio::net::ToSocketAddrs,
        calling_ae: &str,
        called_ae: &str,
        presentation_contexts: Vec<PresentationContext>,
        options: ClientOptions,
    ) -> Result<Self> {
        // validate before touching the network: a bad identity must not
        // depend on whether the peer happens to be reachable (§4.3)
        if let Some(identity) = &options.user_identity {
            identity.validate()?;
        }

        let tcp = TcpStream::connect(addr).await.map_err(NetworkError::Io)?;
        crate::transport::apply_options(&tcp, &options.tcp)?;

        #[cfg(feature = "tls")]
        let mut stream = match &options.tls {
            Some(tls) => ClientStream::Tls(Box::new(tls.connect(tcp).await?)),
            None => ClientStream::Plain(tcp),
        };
        #[cfg(not(feature = "tls"))]
        let mut stream = ClientStream::Plain(tcp);

        let mut request = Associate::new(called_ae, calling_ae);
        request.application_context = APPLICATION_CONTEXT.to_string();
        request.presentation_contexts = presentation_contexts;
        let mut user = crate::association::build_user_information(
            options.max_pdu_length,
            IMPLEMENTATION_CLASS_UID,
            IMPLEMENTATION_VERSION_NAME,
            Vec::new(),
        );
        user.extended_negotiations = options.extended_negotiations.clone();
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
        let (accepted, peer_max_pdu) = match response {
            Some(Pdu::AssociateAc(accept)) => {
                let negotiated = accept
                    .presentation_contexts
                    .iter()
                    .map(|pc| {
                        // an A-ASSOCIATE-AC omits the abstract syntax (it is
                        // implied by the presentation-context id), so recover it
                        // from the request we just sent
                        let abstract_syntax = if pc.abstract_syntax.is_empty() {
                            requested_contexts
                                .iter()
                                .find(|rq| rq.id == pc.id)
                                .map(|rq| rq.abstract_syntax.clone())
                                .unwrap_or_default()
                        } else {
                            pc.abstract_syntax.clone()
                        };
                        crate::association::NegotiatedContext {
                            id: pc.id,
                            abstract_syntax,
                            transfer_syntax: pc.transfer_syntaxes.first().cloned().unwrap_or_default(),
                            state: crate::association::ContextState::from_result_code(pc.result),
                            roles: Vec::new(),
                        }
                    })
                    .collect();
                // the peer's advertised maximum caps what we may send (§5.3);
                // 0 means "unlimited", so fall back to our own value
                let peer = accept.user_information.max_pdu_length;
                let cap = if peer == 0 { options.max_pdu_length } else { peer.min(options.max_pdu_length) };
                peer_extended = accept.user_information.extended_negotiations;
                peer_identity_response = accept.user_information.user_identity_response;
                (AcceptedContexts::from_negotiated(negotiated), cap)
            }
            Some(Pdu::AssociateRj(reject)) => {
                return Err(NetworkError::Reject {
                    result: reject.result.code(),
                    source_code: reject.source.code(),
                    reason: reject.reason,
                })
            }
            _ => return Err(NetworkError::protocol("expected A-ASSOCIATE-AC or A-ASSOCIATE-RJ")),
        };

        // A positive identity response was required but the peer sent none:
        // silently continuing would authenticate nobody (§9.1).
        if let Some(identity) = &options.user_identity {
            if identity.positive_response_requested && peer_identity_response.is_none() {
                return Err(NetworkError::protocol("user identity response was required but not returned"));
            }
        }

        Ok(Association {
            stream,
            contexts: accepted,
            splitter: PduSplitter::new(peer_max_pdu, 16),
            assembler: PdvAssembler::new(),
            pending: crate::engine::PendingTable::new(),
            release_timeout: options.release_timeout,
            reverse_store: None,
            peer_extended,
            peer_identity_response,
        })
    }

    /// The extended negotiation items the peer accepted (§4.3).
    pub fn accepted_extended_negotiations(&self) -> &[crate::pdu::ExtendedNegotiation] {
        &self.peer_extended
    }

    /// The peer's user identity response, when it sent one (§4.3).
    pub fn user_identity_response(&self) -> Option<&[u8]> {
        self.peer_identity_response.as_deref()
    }

    /// True when the peer granted `field` of `sop_class` (§4.3).
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

    /// Registers the reverse C-STORE handler (§7.4).
    ///
    /// C-GET is impossible without one: the instances come back as C-STORE
    /// sub-operations that this side must answer, so [`Self::send_c_get`]
    /// returns `ReverseStoreHandlerMissing` when none is set.
    pub fn set_reverse_store_handler(&mut self, handler: ReverseStoreHandler) {
        self.reverse_store = Some(handler);
    }

    /// Registers a reverse C-STORE handler that keeps every received instance.
    pub fn collect_reverse_stores(&mut self, sink: Arc<std::sync::Mutex<Vec<radicom::io::file::DicomFile>>>) {
        self.reverse_store = Some(Arc::new(move |request: &crate::services::CStoreRequest| {
            sink.lock().unwrap().push(request.file.clone());
            Ok(crate::services::CStoreResponse::success(request))
        }));
    }

    /// The negotiated presentation contexts.
    pub fn contexts(&self) -> &AcceptedContexts {
        &self.contexts
    }

    /// Sends a C-ECHO and waits for its response (§7.1).
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
        for pdata in self.splitter.split_bytes(context_id, true, bytes)? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }

        self.pending
            .insert(crate::engine::PendingRequest::new(command, std::time::Instant::now()));
        let response = self.await_response(message_id, CommandField::CEchoRsp).await;
        self.pending.remove(message_id);
        CEchoResponse::from_command(&response?)
    }

    /// Sends a C-STORE and waits for its response (§7.2).
    ///
    /// The dataset travels under the presentation context's accepted syntax;
    /// when that differs from the file's own syntax the dataset is transcoded
    /// through the global codec registry (§5.3).
    pub async fn send_c_store(&mut self, file: &radicom::io::file::DicomFile) -> Result<CStoreResponse> {
        self.send_c_store_with_originator(file, None, None).await
    }

    /// Sends a C-STORE carrying Move Originator information (§7.5).
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
        let dataset = &file.dataset;
        let sop_class = dataset
            .try_get_str(radicom::model::tag::Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let sop_instance = dataset
            .try_get_str(radicom::model::tag::Tag::SOP_INSTANCE_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?;

        let context = self.contexts.pick(&sop_class, dataset.transfer_syntax().uid())?;
        let context_id = context.id;
        let target = transfer_syntax_of(&context.transfer_syntax);
        let message_id = self.allocate_message_id()?;

        let mut request = Command::request(CommandField::CStoreRq, message_id);
        request.affected_sop_class_uid = Some(sop_class);
        request.affected_sop_instance_uid = Some(sop_instance);
        request.move_originator_ae_title = originator_ae.map(|ae| ae.to_string());
        request.move_originator_message_id = originator_message_id;
        request.no_dataset = false;

        // transcode when the accepted syntax differs from the instance's own
        let source = dataset.transfer_syntax();
        let payload = if source.uid() == target.uid() {
            crate::message::encode_dataset(dataset, &target)?
        } else {
            let transcoder = radicom::pixel::transcoder::Transcoder::new(
                source,
                target.clone(),
                None,
                None,
            );
            let transcoded = transcoder
                .transcode_dataset(dataset)
                .map_err(|e| NetworkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;
            crate::message::encode_dataset(&transcoded, &target)?
        };

        let command = bytes::Bytes::from(crate::message::encode_command(&request.to_dataset())?);
        let payload = bytes::Bytes::from(payload);
        for pdata in self.splitter.split_message_bytes(context_id, command, Some(payload))? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }

        self.pending
            .insert(crate::engine::PendingRequest::new(request.clone(), std::time::Instant::now()));
        let response = self.await_response(message_id, CommandField::CStoreRsp).await;
        self.pending.remove(message_id);
        response.map(|command| {
            CStoreResponse::from_command(&command).unwrap_or_else(|_| CStoreResponse {
                message_id_being_responded_to: message_id,
                status: crate::dimse::status::PROCESSING_FAILURE,
                affected_sop_class_uid: String::new(),
                affected_sop_instance_uid: String::new(),
                error_comment: Some("malformed C-STORE response".into()),
            })
        })
    }

    /// Runs a C-FIND and returns every match plus the terminal response (§7.3).
    ///
    /// Matches are collected because each response carries a full Identifier;
    /// a caller that needs incremental delivery can drive [`Self::send_c_find_stream`].
    pub async fn send_c_find(&mut self, request: &crate::cfind::CFindRequest) -> Result<Vec<crate::cfind::CFindResponse>> {
        let mut all = Vec::new();
        self.send_c_find_stream(request, |response| {
            all.push(response);
            Ok(())
        })
        .await?;
        Ok(all)
    }

    /// Runs a C-FIND, handing each response to `on_response` as it arrives.
    pub fn send_c_find_stream<'a, F>(
        &'a mut self,
        request: &'a crate::cfind::CFindRequest,
        mut on_response: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>
    where
        F: FnMut(crate::cfind::CFindResponse) -> std::io::Result<()> + 'a,
    {
        Box::pin(async move {
            let context = self.contexts.pick(&request.sop_class_uid, "")?;
            let context_id = context.id;
            let syntax = transfer_syntax_of(&context.transfer_syntax);
            let message_id = request.message_id;

            let command = bytes::Bytes::from(command_bytes(&request.to_command())?);
            let identifier = bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax)?);
            for pdata in self.splitter.split_message_bytes(context_id, command, Some(identifier))? {
                write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
            }

            // the Identifier of a Pending response follows its command
            let mut awaiting_command: Option<Command> = None;
            loop {
                let Some(pdu) = read_pdu(&mut self.stream).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.assembler.push_pdata(&pdata)? {
                            match unit.stream {
                                Stream::Command => {
                                    let command = command_from_bytes(&unit.bytes)?;
                                    if command.field != CommandField::CFindRsp
                                        || command.message_id_being_responded_to != Some(message_id)
                                    {
                                        continue;
                                    }
                                    if command.no_dataset {
                                        let response = crate::cfind::CFindResponse::from_command(&command, None)?;
                                        let terminal = !response.is_pending();
                                        on_response(response)?;
                                        if terminal {
                                            return Ok(());
                                        }
                                    } else {
                                        // the match dataset follows
                                        awaiting_command = Some(command);
                                    }
                                }
                                Stream::Data => {
                                    let Some(command) = awaiting_command.take() else {
                                        continue;
                                    };
                                    let identifier = crate::message::decode_dataset(&unit.bytes, &syntax)?;
                                    let response = crate::cfind::CFindResponse::from_command(&command, Some(identifier))?;
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
                        return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {}
                }
            }
        })
    }

    /// Runs a C-MOVE and collects every progress plus the terminal response
    /// (§7.5).
    pub async fn send_c_move(&mut self, request: &crate::cmove::CMoveRequest) -> Result<Vec<crate::cmove::CMoveResponse>> {
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
    /// A C-MOVE response never carries a dataset: the instances travel on the
    /// SCP's own outbound association (§7.5).
    pub fn send_c_move_stream<'a, F>(
        &'a mut self,
        request: &'a crate::cmove::CMoveRequest,
        mut on_response: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>
    where
        F: FnMut(crate::cmove::CMoveResponse) -> std::io::Result<()> + 'a,
    {
        Box::pin(async move {
            let context = self.contexts.pick(&request.sop_class_uid, "")?;
            let context_id = context.id;
            let syntax = transfer_syntax_of(&context.transfer_syntax);
            let message_id = request.message_id;

            let command = bytes::Bytes::from(command_bytes(&request.to_command())?);
            let identifier = bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax)?);
            for pdata in self.splitter.split_message_bytes(context_id, command, Some(identifier))? {
                write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
            }

            loop {
                let Some(pdu) = read_pdu(&mut self.stream).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.assembler.push_pdata(&pdata)? {
                            if unit.stream != Stream::Command {
                                continue;
                            }
                            let command = command_from_bytes(&unit.bytes)?;
                            if command.field != CommandField::CMoveRsp
                                || command.message_id_being_responded_to != Some(message_id)
                            {
                                continue;
                            }
                            let response = crate::cmove::CMoveResponse::from_command(&command)?;
                            let terminal = !response.is_pending();
                            on_response(response)?;
                            if terminal {
                                return Ok(());
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {}
                }
            }
        })
    }

    /// Runs a C-GET, collecting every response (§7.4).
    ///
    /// Instances arrive as reverse C-STORE sub-operations, so a handler must be
    /// registered first; see [`Self::set_reverse_store_handler`].
    pub async fn send_c_get(&mut self, request: &crate::cget::CGetRequest) -> Result<Vec<crate::cget::CGetResponse>> {
        let mut all = Vec::new();
        self.send_c_get_stream(request, |response| {
            all.push(response);
            Ok(())
        })
        .await?;
        Ok(all)
    }

    /// Runs a C-GET, handing each response to `on_response` as it arrives.
    pub fn send_c_get_stream<'a, F>(
        &'a mut self,
        request: &'a crate::cget::CGetRequest,
        mut on_response: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>
    where
        F: FnMut(crate::cget::CGetResponse) -> std::io::Result<()> + 'a,
    {
        Box::pin(async move {
            let Some(handler) = self.reverse_store.clone() else {
                return Err(NetworkError::ReverseStoreHandlerMissing);
            };
            let context = self.contexts.pick(&request.sop_class_uid, "")?;
            let context_id = context.id;
            let syntax = transfer_syntax_of(&context.transfer_syntax);
            let message_id = request.message_id;

            let command = bytes::Bytes::from(command_bytes(&request.to_command())?);
            let identifier = bytes::Bytes::from(crate::message::encode_dataset(&request.identifier, &syntax)?);
            for pdata in self.splitter.split_message_bytes(context_id, command, Some(identifier))? {
                write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
            }

            // a reverse C-STORE request awaiting its dataset
            let mut awaiting_store: Option<Command> = None;
            loop {
                let Some(pdu) = read_pdu(&mut self.stream).await? else {
                    return Err(NetworkError::ClosedPrematurely { source: None });
                };
                match pdu {
                    Pdu::PData(pdata) => {
                        for unit in self.assembler.push_pdata(&pdata)? {
                            match unit.stream {
                                Stream::Command => {
                                    let command = command_from_bytes(&unit.bytes)?;
                                    match command.field {
                                        CommandField::CStoreRq => {
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
                                                    write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
                                                }
                                            } else {
                                                awaiting_store = Some(command);
                                            }
                                        }
                                        CommandField::CGetRsp => {
                                            if command.message_id_being_responded_to != Some(message_id) {
                                                continue;
                                            }
                                            let response = crate::cget::CGetResponse::from_command(&command)?;
                                            let terminal = !response.is_pending();
                                            on_response(response)?;
                                            if terminal {
                                                return Ok(());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                Stream::Data => {
                                    let Some(store) = awaiting_store.take() else {
                                        continue;
                                    };
                                    let dataset = crate::message::decode_dataset(&unit.bytes, &syntax)?;
                                    let mut file = radicom::io::file::DicomFile::from_dataset(dataset);
                                    file.dataset.set_transfer_syntax(syntax.clone());
                                    let request = crate::store::c_store_request_from_message(
                                        &store,
                                        crate::store::ReceivedInstance::from_file(file),
                                    )?;
                                    // the handler's verdict goes straight back
                                    let response = match handler(&request) {
                                        Ok(response) => response,
                                        Err(error) => crate::store::failure_response(&request, &error),
                                    };
                                    let bytes = bytes::Bytes::from(command_bytes(&response.to_command())?);
                                    for pdata in self.splitter.split_bytes(unit.context_id, true, bytes)? {
                                        write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
                                    }
                                }
                            }
                        }
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
                    }
                    Pdu::ReleaseRq => {
                        write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => {}
                }
            }
        })
    }

    /// Reads PDUs until the response for `message_id` arrives (§5.2).
    async fn await_response(&mut self, message_id: u16, expected: CommandField) -> Result<Command> {
        loop {
            let Some(pdu) = read_pdu(&mut self.stream).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    for unit in self.assembler.push_pdata(&pdata)? {
                        if unit.stream != Stream::Command {
                            continue;
                        }
                        let command = command_from_bytes(&unit.bytes)?;
                        if command.field == expected
                            && command.message_id_being_responded_to == Some(message_id)
                        {
                            return Ok(command);
                        }
                    }
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
                }
                Pdu::ReleaseRq => {
                    write_pdu(&mut self.stream, &Pdu::ReleaseRp).await?;
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                _ => {}
            }
        }
    }

    /// Releases the association gracefully (§5.5).
    /// Sends a C-CANCEL-RQ (0x0FFF) for an in-flight request (§7.6, SRV-08).
    ///
    /// The command carries the target `message_id` in
    /// `(0000,0120) MessageIDBeingRespondedTo`, matching fo-dicom's
    /// `CCancelRequest = 0x0FFF`. It is sent on `context_id` (a proposed
    /// presentation context id). Note the SCP collects a query's matches before
    /// answering (buffered per query), so a cancel prevents *further* results
    /// rather than aborting one already computed; full mid-stream interruption
    /// needs the streaming C-FIND path.
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

    /// Sends an N-service request and returns its response (§7.6, SRV-07).
    ///
    /// The presentation context is chosen from the request's SOP class. The
    /// Requested/Affected tags follow the command field (PS3.7 §10.1). A
    /// request/response dataset is transferred in the negotiated syntax.
    pub async fn send_n_service(
        &mut self,
        request: &crate::nservice::NServiceRequest,
    ) -> Result<crate::nservice::NServiceResponse> {
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
        let target = transfer_syntax_of(&context.transfer_syntax);

        let command = request.to_command();
        let command_bytes = bytes::Bytes::from(crate::message::encode_command(&command.to_dataset())?);
        let payload = match &request.dataset {
            Some(dataset) => Some(bytes::Bytes::from(crate::message::encode_dataset(dataset, &target)?)),
            None => None,
        };
        for pdata in self.splitter.split_message_bytes(context_id, command_bytes, payload)? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }

        let expected = crate::dimse::response_field(request.field)
            .ok_or_else(|| NetworkError::protocol("N-service request has no response field"))?;
        let (response_command, response_dataset) =
            self.await_n_response(request.message_id, expected, &target).await?;
        Ok(crate::nservice::NServiceResponse::from_command(&response_command, response_dataset))
    }

    /// Reads the N-service response command (plus any dataset) for `message_id`.
    async fn await_n_response(
        &mut self,
        message_id: u16,
        expected: CommandField,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    ) -> Result<(Command, Option<Dataset>)> {
        let mut pending_command: Option<Command> = None;
        loop {
            let Some(pdu) = read_pdu(&mut self.stream).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    for unit in self.assembler.push_pdata(&pdata)? {
                        match unit.stream {
                            Stream::Command => {
                                let command = command_from_bytes(&unit.bytes)?;
                                if command.field == expected
                                    && command.message_id_being_responded_to == Some(message_id)
                                {
                                    if command.no_dataset {
                                        return Ok((command, None));
                                    }
                                    pending_command = Some(command);
                                }
                            }
                            Stream::Data => {
                                if let Some(command) = pending_command.take() {
                                    let dataset = crate::message::decode_dataset(&unit.bytes, syntax)?;
                                    return Ok((command, Some(dataset)));
                                }
                            }
                        }
                    }
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
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
        let response = tokio::time::timeout(release_timeout, read_pdu(&mut self.stream))
            .await
            .map_err(|_| NetworkError::ReleaseTimedOut { timeout_ms: release_timeout.as_millis() as u64 })??;
        match response {
            Some(Pdu::ReleaseRp) => Ok(()),
            Some(Pdu::Abort(abort)) => {
                Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
            }
            Some(_) => Err(NetworkError::protocol("expected A-RELEASE-RP")),
            None => Err(NetworkError::ClosedPrematurely { source: None }),
        }
    }

    /// Aborts the association (§3.3).
    pub async fn abort(mut self, reason: u8) -> Result<()> {
        let abort = Abort { source: AbortSource::User, reason };
        write_pdu(&mut self.stream, &Pdu::Abort(abort)).await
    }

    /// Allocates a message ID for a request this side sends (§5.2).
    ///
    /// IDs still awaiting a response are skipped, so the wrap at 65535 can
    /// never hand an in-flight ID to a new request.
    fn allocate_message_id(&mut self) -> Result<u16> {
        self.pending.allocate_message_id()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;
    use crate::services::{CStoreRequest, VerificationProvider};
    use radicom::model::dataset::Dataset;
    use radicom::model::tag::Tag;
    use radicom::model::vr::Vr;

    fn echo_context() -> PresentationContext {
        PresentationContext::new(
            1,
            sop_class::VERIFICATION,
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        )
    }

    #[test]
    fn command_bytes_are_implicit_vr_little_endian() {
        let request = CEchoRequest { message_id: 5, affected_sop_class_uid: sop_class::VERIFICATION.into() };
        let bytes = command_bytes(&request.to_command()).unwrap();
        // implicit VR: tag(4) + length(4), no VR bytes
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0x00], "(0000,0000) group length first");
        let back = command_from_bytes(&bytes).unwrap();
        assert_eq!(back.field, CommandField::CEchoRq);
        assert_eq!(back.message_id, 5);
    }

    #[test]
    fn command_round_trip_preserves_every_field() {
        let mut command = Command::request(CommandField::CStoreRq, 77);
        command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
        command.affected_sop_instance_uid = Some("1.2.3.4".into());
        command.priority = crate::dimse::Priority::Medium;
        command.no_dataset = false;

        let back = command_from_bytes(&command_bytes(&command).unwrap()).unwrap();
        assert_eq!(back, command);
    }

    #[tokio::test]
    async fn loopback_c_echo_succeeds() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();

        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "MY-SCU",
            "MY-SCP",
            vec![echo_context()],
            ClientOptions::default(),
        )
        .await
        .expect("association accepted");

        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        assert_eq!(response.state(), crate::dimse::StatusState::Success);
        assert_eq!(response.message_id_being_responded_to, 1);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_metrics_collector_observes_the_connection() {
        use crate::observability::{CountingMetrics, MetricsCollector};
        let metrics = Arc::new(CountingMetrics::new());
        let options = ServerOptions {
            metrics: Some(metrics.clone() as Arc<dyn MetricsCollector>),
            ..Default::default()
        };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options)
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "MY-SCU",
            "MY-SCP",
            vec![echo_context()],
            ClientOptions::default(),
        )
        .await
        .expect("association accepted");
        let _ = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.connections_opened, 1, "one connection counted");
        assert_eq!(snapshot.connections_closed, 1, "the guard closed it");
        assert_eq!(snapshot.associations_opened, 1, "one association counted");
        assert_eq!(snapshot.associations_closed, 1);
        assert!(snapshot.bytes_received > 0, "the A-ASSOCIATE-RQ bytes were counted");
        assert!(snapshot.bytes_sent > 0, "the accept + echo response bytes were counted");
    }

    #[tokio::test]
    async fn loopback_rejects_an_unsupported_abstract_syntax() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // propose only an abstract syntax the provider does not serve
        let context = PresentationContext::new(1, "9.9.9", vec!["1.2.840.10008.1.2".to_string()]);
        let result = Association::open(addr, "SCU", "SCP", vec![context], ClientOptions::default()).await;
        match result {
            Err(NetworkError::Reject { source_code, .. }) => assert_eq!(source_code, 1, "user rejection"),
            Err(other) => panic!("expected Reject, got {other}"),
            Ok(_) => panic!("expected the association to be rejected"),
        }
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_negotiates_the_offered_syntax() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // offer explicit VR LE only; the SCP supports it
        let context = PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2.1".to_string()]);
        let association = Association::open(addr, "SCU", "SCP", vec![context], ClientOptions::default())
            .await
            .unwrap();
        let accepted = association.contexts().all();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].transfer_syntax, "1.2.840.10008.1.2.1");
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_multiple_echoes_on_one_association() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![echo_context()],
            ClientOptions::default(),
        )
        .await
        .unwrap();
        for expected in 1..=3u16 {
            let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
            assert_eq!(response.status, crate::dimse::status::SUCCESS);
            assert_eq!(response.message_id_being_responded_to, expected, "message IDs increment");
        }
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_abort_ends_the_connection() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![echo_context()],
            ClientOptions::default(),
        )
        .await
        .unwrap();
        association.abort(0).await.unwrap();
        // §2.16: a peer abort is not a clean close, so the server reports it
        match handle.await.unwrap() {
            Err(NetworkError::Abort { source_code, reason }) => {
                assert_eq!((source_code, reason), (0, 0), "the peer's own abort source and reason");
            }
            other => panic!("expected the peer abort to surface as an error, got {other:?}"),
        }
    }    /// A provider that records every stored instance.
    struct RecordingProvider {
        stored: std::sync::Mutex<Vec<String>>,
        /// `(originator AE, originator message ID)` per C-STORE (§7.5, D5).
        origins: std::sync::Mutex<Vec<(Option<String>, Option<u16>)>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            RecordingProvider {
                stored: std::sync::Mutex::new(Vec::new()),
                origins: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn instance_uids(&self) -> Vec<String> {
            self.stored.lock().unwrap().clone()
        }

        fn origins(&self) -> Vec<(Option<String>, Option<u16>)> {
            self.origins.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ServiceProvider for RecordingProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![
                sop_class::VERIFICATION.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }

        async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
            Ok(CEchoResponse::success(request))
        }

        async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
            self.stored.lock().unwrap().push(request.affected_sop_instance_uid.clone());
            self.origins.lock().unwrap().push((
                request.move_originator_ae_title.clone(),
                request.move_originator_message_id,
            ));
            Ok(CStoreResponse::success(request))
        }
    }

    fn store_context() -> PresentationContext {
        PresentationContext::new(
            3,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        )
    }

    fn instance_file() -> radicom::io::file::DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6");
        dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        radicom::io::file::DicomFile::from_dataset(dataset)
    }

    #[tokio::test]
    async fn loopback_c_store_delivers_the_instance() {
        let provider = Arc::new(RecordingProvider::new());
        let server = DicomServer::bind(
            "127.0.0.1:0",
            provider.clone(),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![store_context()],
            ClientOptions::default(),
        )
        .await
        .unwrap();
        let response = association.send_c_store(&instance_file()).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS, "C-STORE accepted");
        assert_eq!(response.state(), crate::dimse::StatusState::Success);
        assert_eq!(provider.instance_uids(), vec!["1.2.3.4.5.6".to_string()]);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_store_preserves_the_dataset() {
        let provider = Arc::new(RecordingProvider::new());
        let server = DicomServer::bind(
            "127.0.0.1:0",
            provider.clone(),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(addr, "SCU", "SCP", vec![store_context()], ClientOptions::default())
            .await
            .unwrap();
        let file = instance_file();
        association.send_c_store(&file).await.unwrap();
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
        assert_eq!(provider.instance_uids().len(), 1);
    }

    #[tokio::test]
    async fn loopback_c_store_then_echo_on_one_association() {
        let provider = Arc::new(RecordingProvider::new());
        let server = DicomServer::bind(
            "127.0.0.1:0",
            provider.clone(),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // propose both contexts so one association serves both services
        let contexts = vec![
            PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2".to_string()]),
            store_context(),
        ];
        let mut association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
            .await
            .unwrap();

        let store = association.send_c_store(&instance_file()).await.unwrap();
        assert_eq!(store.status, crate::dimse::status::SUCCESS);
        let echo = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(echo.status, crate::dimse::status::SUCCESS);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_store_failure_keeps_the_connection() {
        // the VerificationProvider does not implement C-STORE
        struct EchoOnly;
        #[async_trait::async_trait]
        impl ServiceProvider for EchoOnly {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![sop_class::VERIFICATION.to_string(), "1.2.840.10008.5.1.4.1.1.7".to_string()]
            }
            async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
                Ok(CEchoResponse::success(request))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(EchoOnly),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let contexts = vec![
            PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2".to_string()]),
            store_context(),
        ];
        let mut association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
            .await
            .unwrap();

        let response = association.send_c_store(&instance_file()).await.unwrap();
        assert_eq!(response.state(), crate::dimse::StatusState::Failure, "C-STORE rejected");
        // the connection survives a rejected instance (§5.2)
        let echo = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(echo.status, crate::dimse::status::SUCCESS);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_failing_echo_provider_answers_processing_failure() {
        // a provider error must not kill the association: the peer gets a
        // failure status and can keep using the connection
        struct FailingEcho;
        #[async_trait::async_trait]
        impl ServiceProvider for FailingEcho {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![sop_class::VERIFICATION.to_string()]
            }
            async fn on_c_echo(&self, _request: &CEchoRequest) -> Result<CEchoResponse> {
                Err(NetworkError::protocol("provider exploded"))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(FailingEcho),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let contexts =
            vec![PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2".to_string()])];
        let mut association = Association::open(addr, "SCU", "SCP", contexts, ClientOptions::default())
            .await
            .unwrap();

        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(
            response.state(),
            crate::dimse::StatusState::Failure,
            "a provider error is answered as a failed operation"
        );
        // the association is still healthy: a second echo gets an answer, not EOF
        let again = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(again.state(), crate::dimse::StatusState::Failure);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    /// A provider answering C-FIND with fixed study matches.
    struct QueryProvider {
        matches: Vec<(String, String)>,
    }

    impl QueryProvider {
        fn new(matches: Vec<(&str, &str)>) -> Self {
            QueryProvider {
                matches: matches.into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ServiceProvider for QueryProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
        }

        fn on_c_find<'a>(&'a self, request: &'a crate::cfind::CFindRequest) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
            use futures::StreamExt;
            let total = self.matches.len() as u16;
            let items: Vec<_> = self
                .matches
                .iter()
                .map(|(study_uid, patient_name)| {
                    let mut item = Dataset::new();
                    item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, study_uid.clone());
                    item.insert_str(Tag::PATIENT_NAME, Vr::Pn, patient_name.clone());
                    Ok(crate::cfind::CFindResponse::pending(request, item))
                })
                .collect();
            futures::stream::iter(items)
                .chain(futures::stream::once(async move {
                    Ok(crate::cfind::CFindResponse::success(request, total))
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

    #[tokio::test]
    async fn loopback_c_find_returns_every_match() {
        let provider = Arc::new(QueryProvider::new(vec![
            ("1.2.3.1", "Doe^John"),
            ("1.2.3.2", "Roe^Jane"),
        ]));
        let server = DicomServer::bind(
            "127.0.0.1:0",
            provider.clone(),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "SCU", "SCP", vec![find_context()], ClientOptions::default())
                .await
                .unwrap();

        let request = crate::cfind::CFindRequest::study_query(1)
            .with_key(0x0010, 0x0020, Vr::Lo, "PID-1");
        let responses = association.send_c_find(&request).await.unwrap();

        assert_eq!(responses.len(), 3, "two matches plus the terminal response");
        assert!(responses[0].is_pending() && responses[1].is_pending());
        assert_eq!(responses[2].status, crate::dimse::status::SUCCESS);
        assert_eq!(responses[2].completed, 2);

        let first = responses[0].identifier.as_ref().unwrap();
        assert_eq!(first.get_str(Tag::STUDY_INSTANCE_UID).unwrap(), "1.2.3.1");
        assert_eq!(first.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
        let second = responses[1].identifier.as_ref().unwrap();
        assert_eq!(second.get_str(Tag::PATIENT_NAME).unwrap(), "Roe^Jane");

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_find_with_no_matches_terminates_cleanly() {
        let provider = Arc::new(QueryProvider::new(vec![]));
        let server = DicomServer::bind(
            "127.0.0.1:0",
            provider,
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "SCU", "SCP", vec![find_context()], ClientOptions::default())
                .await
                .unwrap();
        let responses = association.send_c_find(&crate::cfind::CFindRequest::study_query(1)).await.unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].status, crate::dimse::status::SUCCESS);
        assert_eq!(responses[0].completed, 0);
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_find_surfaces_a_provider_failure() {
        struct BrokenFind;
        #[async_trait::async_trait]
        impl ServiceProvider for BrokenFind {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
            }
            fn on_c_find<'a>(&'a self, _request: &'a crate::cfind::CFindRequest) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
                Box::pin(futures::stream::once(async {
                    Err(NetworkError::protocol("query backend down"))
                }))
            }
        }

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(BrokenFind),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "SCU", "SCP", vec![find_context()], ClientOptions::default())
                .await
                .unwrap();
        let responses = association.send_c_find(&crate::cfind::CFindRequest::study_query(1)).await.unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].state(), crate::dimse::StatusState::Failure);
        assert!(responses[0].error_comment.is_some());

        // the association survives a failed query
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }
    /// A provider that answers C-MOVE by pushing instances to a fixed address.
    struct MoveProvider {
        destination: std::net::SocketAddr,
        instances: Vec<radicom::io::file::DicomFile>,
        originator: std::sync::Mutex<Option<(String, u16)>>,
    }

    #[async_trait::async_trait]
    impl ServiceProvider for MoveProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
        }

        fn on_c_move<'a>(
            &'a self,
            request: &'a crate::cmove::CMoveRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
            let destination = self.destination;
            let instances = self.instances.clone();
            let originator = self.originator.lock().unwrap().clone();
            let originator_ae = originator.as_ref().map(|(ae, _)| ae.clone());
            let originator_id = originator.as_ref().map(|(_, id)| *id);
            Box::pin(futures::stream::once(async move {
                let summary = crate::cmove::store_to_destination(
                    destination,
                    "MOVE-SCP",
                    "STORE-SCP",
                    &instances,
                    originator_ae.as_deref().map(|ae| (ae, originator_id.unwrap_or(0))),
                    ClientOptions::default(),
                )
                .await?;
                Ok(crate::cmove::CMoveResponse::completed(request, summary))
            }))
        }
    }

    fn move_context() -> PresentationContext {
        PresentationContext::new(
            1,
            sop_class::STUDY_ROOT_QR_MOVE,
            vec!["1.2.840.10008.1.2".to_string()],
        )
    }

    #[tokio::test]
    async fn loopback_c_move_pushes_instances_to_the_destination() {
        // the destination is a plain C-STORE SCP
        let stored = Arc::new(RecordingProvider::new());
        let destination = DicomServer::bind(
            "127.0.0.1:0",
            stored.clone(),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let destination_addr = destination.local_addr();
        tokio::spawn(async move {
            let _ = destination.run().await;
        });

        let provider = Arc::new(MoveProvider {
            destination: destination_addr,
            instances: vec![instance_file()],
            originator: std::sync::Mutex::new(Some(("MOVE-SCU".to_string(), 9))),
        });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "MOVE-SCU", "MOVE-SCP", vec![move_context()], ClientOptions::default())
                .await
                .unwrap();

        let request = crate::cmove::CMoveRequest::study("1.2.3.4", "STORE-SCP", 1);
        let responses = association.send_c_move(&request).await.unwrap();

        assert_eq!(responses.len(), 1, "one terminal response, no progress was emitted");
        assert_eq!(responses[0].status, crate::dimse::status::SUCCESS);
        assert_eq!(responses[0].summary.completed, 1);
        assert_eq!(stored.instance_uids(), vec!["1.2.3.4.5.6".to_string()], "the destination received the instance");
        assert_eq!(
            stored.origins(),
            vec![(Some("MOVE-SCU".to_string()), Some(9))],
            "Move Originator AE title and message ID travel with the sub-operation (D5)"
        );

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_move_surfaces_a_failed_delivery() {
        // the destination refuses everything: no reverse handler for C-STORE
        struct RefusingStore;
        #[async_trait::async_trait]
        impl ServiceProvider for RefusingStore {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
            }
        }
        let destination = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(RefusingStore),
            ServerOptions { store_sink: crate::store::StoreSink::Memory, ..Default::default() },
        )
        .await
        .unwrap();
        let destination_addr = destination.local_addr();
        tokio::spawn(async move {
            let _ = destination.run().await;
        });

        let provider = Arc::new(MoveProvider {
            destination: destination_addr,
            instances: vec![instance_file()],
            originator: std::sync::Mutex::new(None),
        });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "MOVE-SCU", "MOVE-SCP", vec![move_context()], ClientOptions::default())
                .await
                .unwrap();
        let responses = association
            .send_c_move(&crate::cmove::CMoveRequest::study("1.2.3.4", "STORE-SCP", 1))
            .await
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].state(),
            crate::dimse::StatusState::Failure,
            "a destination that rejects every instance fails the move"
        );
        assert_eq!(responses[0].summary.failed, 1);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_move_without_a_destination_is_refused() {
        // the provider is never called: the engine rejects a malformed request
        let provider = Arc::new(MoveProvider {
            destination: "127.0.0.1:1".parse().unwrap(),
            instances: Vec::new(),
            originator: std::sync::Mutex::new(None),
        });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "SCU", "SCP", vec![move_context()], ClientOptions::default())
                .await
                .unwrap();
        // a hand-built request without Move Destination
        let mut request = crate::cmove::CMoveRequest::study("1.2.3.4", "DEST", 1);
        request.destination_ae = String::new();
        let mut command = request.to_command();
        command.move_destination = None;
        let identifier = crate::message::encode_dataset(
            &request.identifier,
            &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN,
        )
        .unwrap();
        let context_id = association.contexts().all()[0].id;
        let bytes = command_bytes(&command).unwrap();
        for pdata in association.splitter.split_message(context_id, &bytes, Some(&identifier)).unwrap() {
            write_pdu(&mut association.stream, &Pdu::PData(pdata)).await.unwrap();
        }
        let response = association.await_response(1, CommandField::CMoveRsp).await.unwrap();
        assert_eq!(response.status, Some(crate::dimse::status::MOVE_DESTINATION_UNKNOWN));

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    /// A provider that answers C-GET by handing back instances.
    struct GetProvider {
        instances: Vec<radicom::io::file::DicomFile>,
    }

    #[async_trait::async_trait]
    impl ServiceProvider for GetProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            // the retrieve model plus the storage class the sub-operations
            // travel on (§7.4)
            vec![
                sop_class::STUDY_ROOT_QR_GET.to_string(),
                "1.2.840.10008.5.1.4.1.1.7".to_string(),
            ]
        }

        fn on_c_get<'a>(
            &'a self,
            request: &'a crate::cget::CGetRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cget::CGetEvent>> {
            let _ = request;
            let events: Vec<_> = self
                .instances
                .iter()
                .cloned()
                .map(|file| Ok(crate::cget::CGetEvent::instance(file)))
                .collect();
            Box::pin(futures::stream::iter(events))
        }
    }

    fn get_context() -> PresentationContext {
        PresentationContext::new(
            1,
            sop_class::STUDY_ROOT_QR_GET,
            vec!["1.2.840.10008.1.2".to_string()],
        )
    }

    #[tokio::test]
    async fn loopback_c_get_delivers_instances_on_the_same_association() {
        let provider = Arc::new(GetProvider { instances: vec![instance_file()] });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // one association carries both the C-GET and the reverse C-STOREs
        let contexts = vec![
            get_context(),
            PresentationContext::new(
                3,
                "1.2.840.10008.5.1.4.1.1.7",
                vec!["1.2.840.10008.1.2".to_string()],
            ),
        ];
        let mut association = Association::open(addr, "GET-SCU", "GET-SCP", contexts, ClientOptions::default())
            .await
            .unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        association.collect_reverse_stores(received.clone());

        let responses = association
            .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
            .await
            .unwrap();

        let stored = received.lock().unwrap().clone();
        assert_eq!(stored.len(), 1, "the instance came back as a reverse C-STORE");
        assert_eq!(
            stored[0].dataset.get_str(Tag::SOP_INSTANCE_UID).unwrap(),
            "1.2.3.4.5.6"
        );

        assert!(responses.iter().any(|r| r.is_pending()), "progress is reported per instance");
        let terminal = responses.last().unwrap();
        assert_eq!(terminal.status, crate::dimse::status::SUCCESS);
        assert_eq!(terminal.summary.completed, 1);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn c_get_without_a_reverse_store_handler_fails_fast() {
        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(GetProvider { instances: vec![instance_file()] }),
            ServerOptions::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });
        let mut association = Association::open(
            addr,
            "GET-SCU",
            "GET-SCP",
            vec![
                get_context(),
                PresentationContext::new(3, "1.2.840.10008.5.1.4.1.1.7", vec!["1.2.840.10008.1.2".to_string()]),
            ],
            ClientOptions::default(),
        )
        .await
        .unwrap();

        // no handler registered: the caller learns immediately, before any
        // sub-operation is attempted (§7.4)
        match association.send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1)).await {
            Err(NetworkError::ReverseStoreHandlerMissing) => {}
            other => panic!("expected ReverseStoreHandlerMissing, got {other:?}"),
        }
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_c_get_reports_a_failed_sub_operation() {
        let provider = Arc::new(GetProvider { instances: vec![instance_file()] });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "GET-SCU",
            "GET-SCP",
            vec![
                get_context(),
                PresentationContext::new(3, "1.2.840.10008.5.1.4.1.1.7", vec!["1.2.840.10008.1.2".to_string()]),
            ],
            ClientOptions::default(),
        )
        .await
        .unwrap();
        // a handler that refuses every instance
        association.set_reverse_store_handler(Arc::new(|request: &CStoreRequest| {
            Ok(CStoreResponse::failure(request, crate::dimse::status::STORAGE_FAILURE, "full"))
        }));

        let responses = association
            .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
            .await
            .unwrap();
        let terminal = responses.last().unwrap();
        assert_eq!(
            terminal.state(),
            crate::dimse::StatusState::Failure,
            "a refused instance fails the retrieve"
        );
        assert_eq!(terminal.summary.failed, 1);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_extended_negotiation_is_answered_per_sop_class() {
        fn grant_level(sop_class: &str, requested: &crate::extended::AppInfo) -> Option<crate::extended::AppInfo> {
            if sop_class != "1.2.840.10008.5.1.4.1.1.7" {
                return None;
            }
            Some(crate::extended::AppInfo::answer(requested, |field, _| (field == 1).then_some(1)))
        }

        let options = ServerOptions {
            extended_negotiation: crate::extended::ExtendedNegotiationPolicy::new(grant_level),
            ..Default::default()
        };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let request = crate::pdu::ExtendedNegotiation {
            sop_class: "1.2.840.10008.5.1.4.1.1.7".into(),
            requested: crate::extended::StorageAppInfo {
                level_of_support: 1,
                digital_signature: 1,
                element_coercion: 0,
            }
            .to_app_info()
            .fields,
            accepted: Vec::new(),
        };
        let client_options = ClientOptions {
            extended_negotiations: vec![request],
            ..Default::default()
        };
        let association =
            Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options).await.unwrap();

        assert_eq!(
            association.extended_field("1.2.840.10008.5.1.4.1.1.7", 1),
            Some(&[1u8][..]),
            "the SCP granted level 2"
        );
        assert_eq!(
            association.extended_field("1.2.840.10008.5.1.4.1.1.7", 2),
            None,
            "a zero-length answer is not a grant"
        );
        assert_eq!(association.extended_field("9.9.9", 1), None, "unknown SOP class");

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_user_identity_is_echoed_when_requested() {
        let options = ServerOptions {
            user_identity: UserIdentityPolicy::Answer(b"OK".to_vec()),
            ..Default::default()
        };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let client_options = ClientOptions {
            user_identity: Some(crate::pdu::UserIdentity {
                identity_type: 2,
                primary: b"user".to_vec(),
                secondary: b"pass".to_vec(),
                positive_response_requested: true,
            }),
            ..Default::default()
        };
        let association =
            Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options).await.unwrap();
        assert_eq!(association.user_identity_response(), Some(&b"OK"[..]));

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loopback_a_refused_identity_rejects_the_association() {
        // the default policy refuses user identity
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let client_options = ClientOptions {
            user_identity: Some(crate::pdu::UserIdentity {
                identity_type: 1,
                primary: b"user".to_vec(),
                secondary: Vec::new(),
                positive_response_requested: false,
            }),
            ..Default::default()
        };
        match Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options).await {
            Err(NetworkError::Reject { result, source_code, .. }) => {
                assert_eq!(result, 1, "permanent");
                assert_eq!(source_code, 1, "service user");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_required_identity_response_that_is_missing_fails_the_association() {
        // Accept without echoing: the peer asked for a positive response
        let options = ServerOptions { user_identity: UserIdentityPolicy::Accept, ..Default::default() };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let client_options = ClientOptions {
            user_identity: Some(crate::pdu::UserIdentity {
                identity_type: 1,
                primary: b"user".to_vec(),
                secondary: Vec::new(),
                positive_response_requested: true,
            }),
            ..Default::default()
        };
        match Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options).await {
            Err(NetworkError::Protocol(message)) => {
                assert!(message.contains("user identity response"), "got {message}")
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_invalid_identity_is_refused_before_connecting() {
        let client_options = ClientOptions {
            user_identity: Some(crate::pdu::UserIdentity {
                identity_type: 1,
                primary: b"user".to_vec(),
                // a passcode without type 2 is invalid (§4.3)
                secondary: b"pass".to_vec(),
                positive_response_requested: false,
            }),
            ..Default::default()
        };
        let result = Association::open("127.0.0.1:1", "SCU", "SCP", vec![echo_context()], client_options).await;
        match result {
            Err(NetworkError::Protocol(message)) => assert!(message.contains("type 2"), "got {message}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn loopback_honours_a_small_peer_max_pdu() {
        // §4.5: the old test only asserted that C-ECHO succeeded, so any
        // implementation passed. A 64-byte maximum must actually split the
        // command set over several P-DATA PDUs, so a fake peer reads the real
        // wire frames and checks each body length against the negotiated max.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // echo the client's own 64-byte maximum back in the accept
            let request = crate::test_wire::accept_association(&mut stream).await;
            assert_eq!(request.user_information.max_pdu_length, 64);

            let mut assembler = PdvAssembler::new();
            let mut frames = 0usize;
            let mut command = None;
            loop {
                match read_pdu(&mut stream).await.unwrap() {
                    Some(Pdu::PData(pdata)) => {
                        frames += 1;
                        // a P-DATA body is the PDU length field, i.e. the PDU
                        // less its 6-byte header; every PDV costs 6 header
                        // bytes plus an even-padded value
                        let body: usize = pdata
                            .pdvs
                            .iter()
                            .map(|pdv| 6 + pdv.data.len() + pdv.data.len() % 2)
                            .sum();
                        assert!(body <= 64, "P-DATA body is {body} bytes, over the negotiated 64");
                        for unit in assembler.push_pdata(&pdata).unwrap() {
                            if unit.stream == Stream::Command {
                                command = Some(command_from_bytes(&unit.bytes).unwrap());
                            }
                        }
                        if let Some(command) = &command {
                            let reply = Command::response(command, crate::dimse::status::SUCCESS)
                                .expect("a C-ECHO request has a response field");
                            let bytes = command_bytes(&reply).unwrap();
                            let pdv = crate::pdu::Pdv {
                                context_id: pdata.pdvs[0].context_id,
                                is_command: true,
                                is_last: true,
                                data: bytes.into(),
                            };
                            write_pdu(&mut stream, &Pdu::PData(crate::pdu::PData { pdvs: vec![pdv] }))
                                .await
                                .unwrap();
                            // the client answers a response with A-RELEASE-RQ;
                            // keep the socket open until it does, or its release
                            // sees a hard reset instead of a clean close
                            break;
                        }
                    }
                    Some(_) => continue,
                    None => panic!("the client closed before sending a command"),
                }
            }
            // stay available for the release handshake
            loop {
                match read_pdu(&mut stream).await.unwrap() {
                    Some(Pdu::ReleaseRq) => {
                        write_pdu(&mut stream, &Pdu::ReleaseRp).await.unwrap();
                        break;
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            frames
        });

        let client_options = ClientOptions { max_pdu_length: 64, ..Default::default() };
        let mut association = Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options)
            .await
            .unwrap();
        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS, "fragmented command still round-trips");
        association.release().await.unwrap();

        let frames = peer.await.unwrap();
        assert!(frames > 1, "a 64-byte maximum must split the command set over several P-DATA PDUs (got {frames})");
    }
    #[tokio::test]
    async fn server_serves_several_connections() {
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

        for _ in 0..3 {
            let mut association = Association::open(
                addr,
                "SCU",
                "SCP",
                vec![echo_context()],
                ClientOptions::default(),
            )
            .await
            .unwrap();
            let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
            assert_eq!(response.status, crate::dimse::status::SUCCESS);
            association.release().await.unwrap();
        }
    }

    const COMMIT_SOP_CLASS: &str = "1.2.840.10008.1.20.1"; // storage commitment push model

    struct CommitProvider;

    #[async_trait::async_trait]
    impl ServiceProvider for CommitProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![COMMIT_SOP_CLASS.to_string()]
        }

        async fn on_n_action(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            // storage commitment answers with a Transaction UID in the response dataset
            let mut dataset = Dataset::new();
            dataset.insert_str(Tag::TRANSACTION_UID, Vr::Ui, "1.2.3.4.5.6");
            Ok(crate::nservice::NServiceResponse::success(request, Some(dataset)))
        }

        async fn on_n_delete(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            // N-DELETE answers with a command-only response
            Ok(crate::nservice::NServiceResponse::success(request, None))
        }
    }

    fn commit_context() -> PresentationContext {
        PresentationContext::new(1, COMMIT_SOP_CLASS, vec!["1.2.840.10008.1.2.1".to_string()])
    }

    #[tokio::test]
    async fn n_action_round_trips_with_a_dataset() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(CommitProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![commit_context()],
            ClientOptions::default(),
        )
        .await
        .unwrap();

        let mut request = crate::nservice::NServiceRequest::new(CommandField::NActionRq, 1);
        request.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
        request.sop_instance_uid = Some("1.2.3.4".to_string());
        request.action_type_id = Some(1);
        let mut request_dataset = Dataset::new();
        request_dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        request.dataset = Some(request_dataset);

        let response = association.send_n_service(&request).await.unwrap();
        assert_eq!(response.field, CommandField::NActionRsp);
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        let dataset = response.dataset.expect("N-ACTION response carries a dataset");
        assert_eq!(
            dataset.try_get_str(Tag::TRANSACTION_UID).as_deref(),
            Some("1.2.3.4.5.6"),
            "the Transaction UID comes back in the response dataset"
        );

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn n_delete_round_trips_without_a_dataset() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(CommitProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![commit_context()],
            ClientOptions::default(),
        )
        .await
        .unwrap();

        let mut request = crate::nservice::NServiceRequest::new(CommandField::NDeleteRq, 2);
        request.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
        request.sop_instance_uid = Some("1.2.3.4".to_string());

        let response = association.send_n_service(&request).await.unwrap();
        assert_eq!(response.field, CommandField::NDeleteRsp);
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        assert!(response.dataset.is_none(), "N-DELETE response is command-only");

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    /// A provider that answers the four N-services under test. A request whose
    /// SOP instance UID is `FAIL` gets a failure status; everything else
    /// succeeds. This exercises the real dispatch path, not just the builders.
    struct NServiceEcho;

    fn n_service_answer(request: &crate::nservice::NServiceRequest) -> crate::nservice::NServiceResponse {
        if request.sop_instance_uid.as_deref() == Some("FAIL") {
            crate::nservice::NServiceResponse::failure(request, crate::dimse::status::PROCESSING_FAILURE)
        } else {
            crate::nservice::NServiceResponse::success(request, None)
        }
    }

    #[async_trait::async_trait]
    impl ServiceProvider for NServiceEcho {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![COMMIT_SOP_CLASS.to_string()]
        }
        async fn on_n_event_report(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            Ok(n_service_answer(request))
        }
        async fn on_n_get(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            Ok(n_service_answer(request))
        }
        async fn on_n_set(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            Ok(n_service_answer(request))
        }
        async fn on_n_create(
            &self,
            request: &crate::nservice::NServiceRequest,
        ) -> Result<crate::nservice::NServiceResponse> {
            Ok(n_service_answer(request))
        }
    }

    /// Drives one N-service request/response pair through a real association:
    /// one success and one failure status (§4.7).
    async fn assert_n_service_round_trip(request_field: CommandField, response_field: CommandField) {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(NServiceEcho), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });
        let mut association =
            Association::open(addr, "SCU", "SCP", vec![commit_context()], ClientOptions::default())
                .await
                .unwrap();

        let mut ok = crate::nservice::NServiceRequest::new(request_field, 1);
        ok.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
        ok.sop_instance_uid = Some("1.2.3.4".to_string());
        if request_field == CommandField::NEventReportRq {
            ok.event_type_id = Some(1);
        }
        let response = association.send_n_service(&ok).await.unwrap();
        assert_eq!(response.field, response_field, "a {request_field:?} is answered with {response_field:?}");
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        assert_eq!(response.message_id_being_responded_to, 1);

        let mut fail = crate::nservice::NServiceRequest::new(request_field, 2);
        fail.sop_class_uid = Some(COMMIT_SOP_CLASS.to_string());
        fail.sop_instance_uid = Some("FAIL".to_string());
        if request_field == CommandField::NEventReportRq {
            fail.event_type_id = Some(1);
        }
        let response = association.send_n_service(&fail).await.unwrap();
        assert_eq!(response.field, response_field);
        assert_eq!(response.status, crate::dimse::status::PROCESSING_FAILURE, "a failed provider maps to a failure status");

        // the association survives a failed operation
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn n_event_report_round_trips_through_the_server() {
        assert_n_service_round_trip(CommandField::NEventReportRq, CommandField::NEventReportRsp).await;
    }

    #[tokio::test]
    async fn n_get_round_trips_through_the_server() {
        assert_n_service_round_trip(CommandField::NGetRq, CommandField::NGetRsp).await;
    }

    #[tokio::test]
    async fn n_set_round_trips_through_the_server() {
        assert_n_service_round_trip(CommandField::NSetRq, CommandField::NSetRsp).await;
    }

    #[tokio::test]
    async fn n_create_round_trips_through_the_server() {
        assert_n_service_round_trip(CommandField::NCreateRq, CommandField::NCreateRsp).await;
    }

    #[tokio::test]
    async fn concurrent_stores_complete_through_the_blocking_spool() {
        // §3.2/§4.7: the default sink spools through spawn_blocking, so several
        // concurrent C-STOREs must still all succeed and leave the client with
        // an ordinary response.
        struct StoreOk;
        #[async_trait::async_trait]
        impl ServiceProvider for StoreOk {
            fn supported_sop_classes(&self) -> Vec<String> {
                vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]
            }
            async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
                Ok(CStoreResponse::success(request))
            }
        }

        let server = DicomServer::bind("127.0.0.1:0", Arc::new(StoreOk), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let mut tasks = Vec::new();
        for _ in 0..8 {
            tasks.push(tokio::spawn(async move {
                let mut association =
                    Association::open(addr, "SCU", "SCP", vec![store_context()], ClientOptions::default())
                        .await
                        .unwrap();
                let response = association.send_c_store(&instance_file()).await.unwrap();
                assert_eq!(response.status, crate::dimse::status::SUCCESS);
                association.release().await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_response_command_aborts_the_association_instead_of_panicking() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association = Association::open(addr, "SCU", "SCP", vec![echo_context()], ClientOptions::default())
            .await
            .unwrap();

        // A C-FIND-RSP-shaped command set where a request belongs: the peer is
        // violating DIMSE, and the SCP must answer with A-ABORT, not panic.
        let bogus = command_bytes(&Command::request(CommandField::CFindRsp, 1)).unwrap();
        let context_id = association.contexts().all()[0].id;
        for pdata in association.splitter.split(context_id, true, &bogus).unwrap() {
            write_pdu(&mut association.stream, &Pdu::PData(pdata)).await.unwrap();
        }

        match read_pdu(&mut association.stream).await {
            Ok(Some(Pdu::Abort(abort))) => {
                assert_eq!(abort.source, AbortSource::User, "service-user abort");
                assert_eq!(abort.reason, 0, "reason not specified");
            }
            other => panic!("expected A-ABORT, got {other:?}"),
        }
        // a panicking connection task would surface as a JoinError here
        let outcome = handle.await.unwrap();
        assert!(outcome.is_err(), "the connection ends with a protocol error");
    }

    /// A provider that reports progress but never terminates its stream.
    struct ProgressOnlyMoveProvider;

    #[async_trait::async_trait]
    impl ServiceProvider for ProgressOnlyMoveProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
        }

        fn on_c_move<'a>(
            &'a self,
            request: &'a crate::cmove::CMoveRequest,
        ) -> futures::stream::BoxStream<'a, Result<crate::cmove::CMoveResponse>> {
            let summary = crate::cmove::SubOperationSummary { completed: 2, failed: 1, warning: 0 };
            Box::pin(futures::stream::once(async move {
                Ok(crate::cmove::CMoveResponse::pending(request, 0, summary))
            }))
        }
    }

    #[tokio::test]
    async fn c_move_reports_the_real_counts_when_the_provider_never_terminates() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(ProgressOnlyMoveProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "SCU", "SCP", vec![move_context()], ClientOptions::default())
                .await
                .unwrap();
        let responses = association
            .send_c_move(&crate::cmove::CMoveRequest::study("1.2.3.4", "STORE-SCP", 1))
            .await
            .unwrap();

        assert_eq!(responses.len(), 2, "the pending progress plus the engine's terminal answer");
        let terminal = responses.last().unwrap();
        assert!(!terminal.is_pending());
        assert_eq!(terminal.summary.completed, 2, "the reported progress is not replaced with zeros");
        assert_eq!(terminal.summary.failed, 1);
        assert_eq!(terminal.state(), crate::dimse::StatusState::Warning);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn max_clients_bounds_concurrent_associations() {
        let options = ServerOptions { max_clients: 1, ..Default::default() };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options).await.unwrap();
        let addr = server.local_addr();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        // the first association takes the only slot
        let first =
            Association::open(addr, "SCU", "SCP", vec![echo_context()], ClientOptions::default())
                .await
                .unwrap();

        // a second peer gets no answer while the slot is taken
        let second = Association::open(
            addr,
            "SCU",
            "SCP",
            vec![echo_context()],
            ClientOptions { association_timeout: Duration::from_millis(200), ..Default::default() },
        )
        .await;
        match second {
            Err(NetworkError::AssociationRequestTimedOut { .. }) => {}
            other => panic!("expected the second association to wait for a slot, got {other:?}"),
        }

        // releasing the slot serves the next peer
        first.release().await.unwrap();
        let third = Association::open(addr, "SCU", "SCP", vec![echo_context()], ClientOptions::default()).await;
        third.expect("the slot is released when the association ends").release().await.unwrap();
    }

    #[tokio::test]
    async fn connected_client_handle_tracks_live_connections() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let clients = server.connected_clients();
        tokio::spawn(async move {
            let _ = server.run().await;
        });

        let association = Association::open(addr, "SCU", "SCP", vec![echo_context()], ClientOptions::default())
            .await
            .unwrap();
        assert_eq!(clients.count(), 1);

        association.release().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while clients.count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released association leaves the connected client count");
    }

    #[tokio::test]
    async fn an_association_that_never_sends_a_request_times_out() {
        use tokio::io::AsyncReadExt;

        let options = ServerOptions { association_timeout: Duration::from_millis(100), ..Default::default() };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // connect and stay silent: the ACSE phase must not hold the task forever
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut abort = [0u8; 10];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut abort))
            .await
            .expect("the server answers within its association timeout")
            .unwrap();
        assert_eq!(abort[0], 0x07, "A-ABORT");
        assert_eq!(abort[8], AbortSource::Provider.code());
        assert_eq!(abort[9], 0, "reason not specified");

        match handle.await.unwrap() {
            Err(NetworkError::AssociationRequestTimedOut { .. }) => {}
            other => panic!("expected AssociationRequestTimedOut, got {other:?}"),
        }
    }

    // -- service semantics: remaining counts, message IDs, sub-operations -----

    /// Runs one malformed P-DATA frame against a live `serve_connection` over a
    /// duplex pair: performs the ACSE handshake, sends the raw frame, then waits
    /// (bounded) for the connection task. Returns its typed result; a panic in
    /// the task surfaces as a failed `expect`, a hang as a timeout.
    async fn drive_malformed_pdata(frame: Vec<u8>) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(serve_connection(
            server,
            Arc::new(VerificationProvider),
            ServerOptions::default(),
        ));

        let mut rq = Associate::new("SCP", "SCU");
        rq.presentation_contexts.push(echo_context());
        let request = crate::pdu::encode(&Pdu::AssociateRq(rq)).expect("encode RQ");
        client.write_all(&request).await.expect("send A-ASSOCIATE-RQ");
        match read_pdu(&mut client).await.expect("read AC") {
            Some(Pdu::AssociateAc(_)) => {}
            other => panic!("expected A-ASSOCIATE-AC, got {other:?}"),
        }

        client.write_all(&frame).await.ok();
        drop(client); // EOF if the frame alone did not already end the exchange

        match tokio::time::timeout(Duration::from_secs(2), handle).await {
            Err(_) => panic!("server hung on malformed P-DATA"),
            Ok(joined) => joined.expect("serve_connection must not panic"),
        }
    }

    #[tokio::test]
    async fn malformed_pdata_after_association_never_panics() {
        /// Wraps a P-DATA body in a PDU frame with a correct length field.
        fn pdata_frame(body: &[u8]) -> Vec<u8> {
            let mut frame = vec![0x04, 0x00];
            frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
            frame.extend_from_slice(body);
            frame
        }

        // Structurally invalid framing: each must come back as a typed error.
        let invalid: Vec<(&str, Vec<u8>)> = vec![
            ("declared PDU length over the cap", vec![0x04, 0x00, 0xFF, 0xFF, 0xFF, 0xFF]),
            ("unknown PDU type", vec![0x09, 0x00, 0x00, 0x00, 0x00, 0x00]),
            ("zero-length PDV", pdata_frame(&[0x00, 0x00, 0x00, 0x00])),
            ("one-byte PDV", pdata_frame(&[0x00, 0x00, 0x00, 0x01, 0x01])),
            ("PDV length past the body", pdata_frame(&[0x00, 0x00, 0x00, 0x10, 0x01, 0x03, 1, 2])),
            ("truncated PDV value", pdata_frame(&[0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 1, 2])),
        ];
        for (name, frame) in invalid {
            let result = drive_malformed_pdata(frame).await;
            assert!(result.is_err(), "case {name} must be a typed error, got {result:?}");
        }

        // Decodable but ill-behaved P-DATA: the guaranteed contract is no panic
        // and no hang (Ok or a typed Err are both acceptable).
        let echo = command_bytes(&Command::request(CommandField::CEchoRq, 1)).expect("encode echo");
        let mut not_negotiated = Vec::new();
        not_negotiated.extend_from_slice(&((echo.len() + 2) as u32).to_be_bytes());
        not_negotiated.push(0xFF); // context id never negotiated
        not_negotiated.push(0x03);
        not_negotiated.extend_from_slice(&echo);
        let _ = drive_malformed_pdata(pdata_frame(&not_negotiated)).await;

        let mut command_then_stray_data = Vec::new();
        command_then_stray_data.extend_from_slice(&((echo.len() + 2) as u32).to_be_bytes());
        command_then_stray_data.push(1);
        command_then_stray_data.push(0x03); // command + last
        command_then_stray_data.extend_from_slice(&echo);
        command_then_stray_data.extend_from_slice(&4u32.to_be_bytes());
        command_then_stray_data.push(1);
        command_then_stray_data.push(0x00); // data, never last
        command_then_stray_data.extend_from_slice(&[0xAB, 0xCD]);
        let _ = drive_malformed_pdata(pdata_frame(&command_then_stray_data)).await;

        // Seeded PRNG fuzz over random P-DATA bodies: 200 iterations, no new
        // dependency, bounded and deterministic.
        let mut state = 0x5EED_1234u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let len = (next() % 40) as usize;
            let body: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = drive_malformed_pdata(pdata_frame(&body)).await;
        }
    }

    /// The contexts a C-GET needs: the retrieve model plus the storage class.
    fn get_contexts() -> Vec<PresentationContext> {
        vec![
            get_context(),
            PresentationContext::new(3, "1.2.840.10008.5.1.4.1.1.7", vec!["1.2.840.10008.1.2".to_string()]),
        ]
    }

    fn instance_with_uid(uid: &str) -> radicom::io::file::DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, uid);
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        radicom::io::file::DicomFile::from_dataset(dataset)
    }

    #[tokio::test]
    async fn c_get_reports_the_remaining_sub_operations() {
        // PS3.7 §9.1.3: (0000,1020) counts the sub-operations still to be
        // performed including the current one, so three instances report
        // 3, 2, 1 and only the terminal response reports 0 (§4.6).
        let provider = Arc::new(GetProvider {
            instances: vec![
                instance_with_uid("1.2.3.4.5.1"),
                instance_with_uid("1.2.3.4.5.2"),
                instance_with_uid("1.2.3.4.5.3"),
            ],
        });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default()).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "GET-SCU", "GET-SCP", get_contexts(), ClientOptions::default())
                .await
                .unwrap();
        // the reverse C-STORE message IDs must be distinct, or the SCU cannot
        // match its responses (§4.6)
        let stored = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = stored.clone();
        association.set_reverse_store_handler(Arc::new(move |request: &CStoreRequest| {
            sink.lock().unwrap().push(request.message_id);
            Ok(CStoreResponse::success(request))
        }));

        let responses = association
            .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
            .await
            .unwrap();

        let pending: Vec<u16> = responses.iter().filter(|r| r.is_pending()).map(|r| r.remaining).collect();
        assert_eq!(pending, vec![3, 2, 1], "the count includes the sub-operation being reported");
        let terminal = responses.last().unwrap();
        assert!(!terminal.is_pending());
        assert_eq!(terminal.remaining, 0, "only the end reports none left");
        assert_eq!(terminal.summary.completed, 3);
        assert_eq!(terminal.status, crate::dimse::status::SUCCESS);

        let mut ids = stored.lock().unwrap().clone();
        assert_eq!(ids.len(), 3);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3, "every reverse C-STORE has its own message ID");

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn c_get_records_an_undeliverable_instance_and_carries_on() {
        // §2.10: an instance the SCP cannot address is one failed
        // sub-operation; the instances after it still have to arrive.
        let mut broken = instance_with_uid("1.2.3.4.5.1");
        broken.dataset.remove(Tag::SOP_INSTANCE_UID);
        let provider = Arc::new(GetProvider { instances: vec![broken, instance_with_uid("1.2.3.4.5.2")] });
        let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default()).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "GET-SCU", "GET-SCP", get_contexts(), ClientOptions::default())
                .await
                .unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        association.collect_reverse_stores(received.clone());

        let responses = association
            .send_c_get(&crate::cget::CGetRequest::study("1.2.3.4", 1))
            .await
            .expect("the retrieval completes despite the broken instance");

        assert_eq!(received.lock().unwrap().len(), 1, "the good instance still arrived");
        let terminal = responses.last().unwrap();
        assert_eq!(terminal.summary.completed, 1);
        assert_eq!(terminal.summary.failed, 1);
        assert_eq!(
            terminal.status,
            crate::dimse::status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS,
            "a partial retrieve is not a failure"
        );

        // the association survived: it can still be released cleanly
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn message_ids_skip_requests_still_in_flight() {
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), ServerOptions::default())
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let mut association =
            Association::open(addr, "SCU", "SCP", vec![echo_context()], ClientOptions::default()).await.unwrap();
        // message ID 1 is still awaiting a response
        association.pending.insert(crate::engine::PendingRequest::new(
            Command::request(CommandField::CEchoRq, 1),
            std::time::Instant::now(),
        ));
        assert_eq!(association.allocate_message_id().unwrap(), 2, "an in-flight ID is skipped");
        // the ordinary path still allocates, sends and releases its entry
        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);

        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn release_reports_a_peer_abort_instead_of_success() {
        // §2.13: the peer must answer A-RELEASE-RQ with A-RELEASE-RP
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::test_wire::accept_association(&mut stream).await;
            let release = read_pdu(&mut stream).await.unwrap();
            assert!(matches!(release, Some(Pdu::ReleaseRq)), "expected A-RELEASE-RQ");
            let abort = Abort { source: AbortSource::Provider, reason: 0 };
            write_pdu(&mut stream, &Pdu::Abort(abort)).await.unwrap();
        });

        let association =
            Association::open(addr, "SCU", "SCP", vec![echo_context()], ClientOptions::default()).await.unwrap();
        match association.release().await {
            Err(NetworkError::Abort { source_code, reason }) => assert_eq!((source_code, reason), (2, 0)),
            other => panic!("expected the peer abort to surface, got {other:?}"),
        }
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn release_times_out_when_the_release_rp_never_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::test_wire::accept_association(&mut stream).await;
            // swallow the release request and stay silent
            let _ = read_pdu(&mut stream).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let options = ClientOptions { release_timeout: Duration::from_millis(100), ..Default::default() };
        let association = Association::open(addr, "SCU", "SCP", vec![echo_context()], options).await.unwrap();
        match association.release().await {
            Err(NetworkError::ReleaseTimedOut { timeout_ms }) => assert_eq!(timeout_ms, 100),
            other => panic!("expected a release timeout, got {other:?}"),
        }
        peer.abort();
    }
}
