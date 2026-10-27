//! End-to-end association and C-ECHO over a real TCP socket
//! (SRV-01/02; docs/07-network.md §5, §8).
//!
//! The engine here is intentionally small: it drives the association
//! handshake, routes one command/response exchange, and closes gracefully.
//! Larger service behaviour (streaming C-FIND, C-GET sub-operations) builds on
//! these same primitives in M6.

use crate::assembler::{PduSplitter, PdvAssembler, Stream};
use crate::association::{AcceptedContexts, NegotiationPolicy};
use crate::dimse::{Command, CommandField};
use crate::error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
use crate::pdu::{
    Abort, Associate, AssociateRj, Pdu, PresentationContext, UserInformation, APPLICATION_CONTEXT,
};
use crate::services::{CEchoRequest, CEchoResponse, CStoreResponse, ServiceProvider};
use crate::transport::{read_pdu, write_pdu, TcpOptions};
use radicom::model::dataset::Dataset;
use radicom::model::transfer_syntax::syntaxes;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// Implementation identity written into user information (§3.2).
pub const IMPLEMENTATION_CLASS_UID: &str = "1.2.826.0.1.3680043.10.1330.2";
pub const IMPLEMENTATION_VERSION_NAME: &str = "RADICOM_0_1";

/// Server options (§8).
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// 0 = unlimited.
    pub max_clients: usize,
    /// Peer maximum PDU length advertised in the association accept.
    pub max_pdu_length: u32,
    /// How long to wait for the peer's A-ASSOCIATE-RQ.
    pub association_timeout: Duration,
    /// Where incoming C-STORE instances are put before the provider sees them.
    pub store_sink: crate::store::StoreSink,
    /// TLS material; `None` serves plain TCP (NET-01, `tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            max_clients: 0,
            max_pdu_length: 16384,
            association_timeout: Duration::from_secs(10),
            store_sink: crate::store::StoreSink::default(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

/// A DIMSE server bound to one address (§8).
pub struct DicomServer<P: ServiceProvider> {
    listener: TcpListener,
    provider: Arc<P>,
    options: ServerOptions,
    local_addr: std::net::SocketAddr,
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
        Ok(DicomServer { listener, provider, options, local_addr })
    }

    /// The bound address, useful when binding to port 0.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Serves connections until the listener errors (§8).
    ///
    /// Each connection gets its own task; `max_clients` bounds concurrency.
    pub async fn run(self) -> Result<()> {
        let provider = self.provider.clone();
        let options = self.options.clone();
        loop {
            let (stream, _peer) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(e) => return Err(NetworkError::Io(e)),
            };
            let provider = provider.clone();
            let options = options.clone();
            tokio::spawn(async move {
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
    mut stream: S,
    provider: Arc<P>,
    options: ServerOptions,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // -- association handshake ------------------------------------------
    let Some(Pdu::AssociateRq(request)) = read_pdu(&mut stream).await? else {
        return Err(NetworkError::protocol("expected A-ASSOCIATE-RQ"));
    };

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
    let mut user = UserInformation::new(options.max_pdu_length);
    user.implementation_class_uid = Some(IMPLEMENTATION_CLASS_UID.to_string());
    user.implementation_version_name = Some(IMPLEMENTATION_VERSION_NAME.to_string());
    accept.user_information = user;
    write_pdu(&mut stream, &Pdu::AssociateAc(accept)).await?;

    let accepted = AcceptedContexts::from_negotiated(negotiated);
    let peer_max_pdu = if request.user_information.max_pdu_length == 0 {
        16384
    } else {
        request.user_information.max_pdu_length
    };
    let splitter = PduSplitter::new(peer_max_pdu, 16);
    let mut assembler = PdvAssembler::new();
    // a command awaiting its dataset (§5.2)
    let mut awaiting_dataset: Option<(u8, Command)> = None;

    // -- message loop ----------------------------------------------------
    loop {
        let Some(pdu) = read_pdu(&mut stream).await? else {
            return Ok(()); // peer closed
        };
        let mut serving = Serving { stream: &mut stream, splitter: &splitter, provider: &provider, options: &options };
        match pdu {            Pdu::PData(pdata) => {
                for unit in assembler.push_pdata(&pdata)? {
                    match unit.stream {
                        Stream::Command => {
                            let command = command_from_bytes(&unit.bytes)?;
                            // Only C-STORE and C-FIND carry a dataset (§7.2,
                            // §7.3). Any other request is answered at once:
                            // trusting the peer's (0000,0800) flag would hang
                            // the engine on a peer that sets it wrongly.
                            let expects_dataset = !command.no_dataset
                                && matches!(command.field, CommandField::CStoreRq | CommandField::CFindRq);
                            if expects_dataset {
                                // the dataset arrives in later PDVs
                                awaiting_dataset = Some((unit.context_id, command));
                            } else {
                                handle_command(&mut serving, unit.context_id, command).await?;
                            }
                        }
                        Stream::Data => {
                            let Some((context_id, command)) = awaiting_dataset.take() else {
                                return Err(NetworkError::protocol("dataset PDV without a command"));
                            };
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
            Pdu::Abort(_) => return Ok(()),
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
            let response = serving.provider.on_c_echo(&request).await?;
            serving.send(context_id, &response.to_command()).await
        }
        CommandField::CCancelRq => Ok(()),
        other => {
            // unsupported request: answer ProcessingFailure, keep the connection
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
        _ => serving.fail(context_id, &command, "this request does not carry a dataset").await,
    }
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

    // parse failures become a failure status; the connection survives (§5.2)
    let instance = match serving.options.store_sink.materialize(dataset_bytes, syntax) {
        Ok(instance) => instance,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
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
        let command_bytes = command_bytes(command)?;
        let payload = match dataset {
            Some(dataset) => Some(crate::message::encode_dataset(dataset, syntax)?),
            None => None,
        };
        for pdata in self.splitter.split_message(context_id, &command_bytes, payload.as_deref())? {
            write_pdu(self.stream, &Pdu::PData(pdata)).await?;
        }
        Ok(())
    }

    /// Answers with ProcessingFailure and a truncated comment.
    async fn fail(&mut self, context_id: u8, request: &Command, comment: &str) -> Result<()> {
        let mut response = Command::response(request, crate::dimse::status::PROCESSING_FAILURE);
        response.error_comment = Some(crate::dimse::truncate_lo(comment));
        self.send(context_id, &response).await
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
    let bytes = command_bytes(command)?;
    for pdata in splitter.split(context_id, true, &bytes)? {
        write_pdu(stream, &Pdu::PData(pdata)).await?;
    }
    Ok(())
}

/// Client options (§9.1, reduced to what M5 needs).
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub max_pdu_length: u32,
    pub association_timeout: Duration,
    /// TLS material; `None` connects over plain TCP (NET-01, `tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            max_pdu_length: 16384,
            association_timeout: Duration::from_secs(5),
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
    next_message_id: u16,
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
        let tcp = TcpStream::connect(addr).await.map_err(NetworkError::Io)?;
        crate::transport::apply_options(&tcp, &TcpOptions::default())?;

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
        let mut user = UserInformation::new(options.max_pdu_length);
        user.implementation_class_uid = Some(IMPLEMENTATION_CLASS_UID.to_string());
        user.implementation_version_name = Some(IMPLEMENTATION_VERSION_NAME.to_string());
        request.user_information = user;

        write_pdu(&mut stream, &Pdu::AssociateRq(request)).await?;

        let response = tokio::time::timeout(options.association_timeout, read_pdu(&mut stream))
            .await
            .map_err(|_| NetworkError::AssociationRequestTimedOut { attempts: 1 })??;
        let (accepted, peer_max_pdu) = match response {
            Some(Pdu::AssociateAc(accept)) => {
                let negotiated = accept
                    .presentation_contexts
                    .iter()
                    .map(|pc| crate::association::NegotiatedContext {
                        id: pc.id,
                        abstract_syntax: pc.abstract_syntax.clone(),
                        transfer_syntax: pc.transfer_syntaxes.first().cloned().unwrap_or_default(),
                        state: crate::association::ContextState::from_result_code(pc.result),
                        roles: Vec::new(),
                    })
                    .collect();
                // the peer's advertised maximum caps what we may send (§5.3);
                // 0 means "unlimited", so fall back to our own value
                let peer = accept.user_information.max_pdu_length;
                let cap = if peer == 0 { options.max_pdu_length } else { peer.min(options.max_pdu_length) };
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

        Ok(Association {
            stream,
            contexts: accepted,
            splitter: PduSplitter::new(peer_max_pdu, 16),
            assembler: PdvAssembler::new(),
            next_message_id: 1,
        })
    }

    /// The negotiated presentation contexts.
    pub fn contexts(&self) -> &AcceptedContexts {
        &self.contexts
    }

    /// Sends a C-ECHO and waits for its response (§7.1).
    pub async fn send_c_echo(&mut self, abstract_syntax: &str) -> Result<CEchoResponse> {
        let context = self.contexts.pick(abstract_syntax, "")?;
        let context_id = context.id;
        let message_id = self.allocate_message_id();
        let request = CEchoRequest {
            message_id,
            affected_sop_class_uid: abstract_syntax.to_string(),
        };

        let bytes = command_bytes(&request.to_command())?;
        for pdata in self.splitter.split(context_id, true, &bytes)? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }

        let command = self.await_response(message_id, CommandField::CEchoRsp).await?;
        CEchoResponse::from_command(&command)
    }

    /// Sends a C-STORE and waits for its response (§7.2).
    ///
    /// The dataset travels under the presentation context's accepted syntax;
    /// when that differs from the file's own syntax the dataset is transcoded
    /// through the global codec registry (§5.3).
    pub async fn send_c_store(&mut self, file: &radicom::io::file::DicomFile) -> Result<CStoreResponse> {
        let dataset = &file.dataset;
        let sop_class = dataset
            .try_get_str(radicom::model::tag::Tag::new(0x0008, 0x0016))
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let sop_instance = dataset
            .try_get_str(radicom::model::tag::Tag::new(0x0008, 0x0018))
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?;

        let context = self.contexts.pick(&sop_class, dataset.transfer_syntax().uid())?;
        let context_id = context.id;
        let target = transfer_syntax_of(&context.transfer_syntax);
        let message_id = self.allocate_message_id();

        let mut request = Command::request(CommandField::CStoreRq, message_id);
        request.affected_sop_class_uid = Some(sop_class);
        request.affected_sop_instance_uid = Some(sop_instance);
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

        let command = crate::message::encode_command(&request.to_dataset())?;
        for pdata in self.splitter.split_message(context_id, &command, Some(&payload))? {
            write_pdu(&mut self.stream, &Pdu::PData(pdata)).await?;
        }

        self.await_response(message_id, CommandField::CStoreRsp).await.map(|command| {
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

            let command = command_bytes(&request.to_command())?;
            let identifier = crate::message::encode_dataset(&request.identifier, &syntax)?;
            for pdata in self.splitter.split_message(context_id, &command, Some(&identifier))? {
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
    pub async fn release(mut self) -> Result<()> {
        write_pdu(&mut self.stream, &Pdu::ReleaseRq).await?;
        // wait for A-RELEASE-RP, tolerating a peer that just closes
        let _ = read_pdu(&mut self.stream).await;
        Ok(())
    }

    /// Aborts the association (§3.3).
    pub async fn abort(mut self, reason: u8) -> Result<()> {
        let abort = Abort { source: AbortSource::User, reason };
        write_pdu(&mut self.stream, &Pdu::Abort(abort)).await
    }

    fn allocate_message_id(&mut self) -> u16 {
        let id = self.next_message_id;
        self.next_message_id = if self.next_message_id == u16::MAX { 1 } else { self.next_message_id + 1 };
        id
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
        handle.await.unwrap().unwrap();
    }    /// A provider that records every stored instance.
    struct RecordingProvider {
        stored: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            RecordingProvider { stored: std::sync::Mutex::new(Vec::new()) }
        }

        fn instance_uids(&self) -> Vec<String> {
            self.stored.lock().unwrap().clone()
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
            Ok(CStoreResponse::success(request))
        }
    }

    fn store_context() -> PresentationContext {
        PresentationContext::new(
            1,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        )
    }

    fn instance_file() -> radicom::io::file::DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0008, 0x0016), Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::new(0x0008, 0x0018), Vr::Ui, "1.2.3.4.5.6");
        dataset.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        dataset.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
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
    }    /// A provider answering C-FIND with fixed study matches.
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
                    item.insert_str(Tag::new(0x0020, 0x000D), Vr::Ui, study_uid.clone());
                    item.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, patient_name.clone());
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
        assert_eq!(first.get_str(Tag::new(0x0020, 0x000D)).unwrap(), "1.2.3.1");
        assert_eq!(first.get_str(Tag::new(0x0010, 0x0010)).unwrap(), "Doe^John");
        let second = responses[1].identifier.as_ref().unwrap();
        assert_eq!(second.get_str(Tag::new(0x0010, 0x0010)).unwrap(), "Roe^Jane");

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
    #[tokio::test]
    async fn loopback_honours_a_small_peer_max_pdu() {
        // a 64-byte maximum forces the command set into several PDVs
        let options = ServerOptions { max_pdu_length: 64, ..Default::default() };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), options).await.unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let client_options = ClientOptions { max_pdu_length: 64, ..Default::default() };
        let mut association = Association::open(addr, "SCU", "SCP", vec![echo_context()], client_options)
            .await
            .unwrap();
        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS, "fragmented command still round-trips");
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
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
}