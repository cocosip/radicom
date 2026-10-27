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
use crate::services::{CEchoRequest, CEchoResponse, ServiceProvider};
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
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions { max_clients: 0, max_pdu_length: 16384, association_timeout: Duration::from_secs(10) }
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
                let _ = serve_connection(stream, provider, options).await;
            });
        }
    }

    /// Serves exactly one connection, then returns (test and single-shot use).
    pub async fn accept_one(&self) -> Result<()> {
        let (stream, _peer) = self.listener.accept().await.map_err(NetworkError::Io)?;
        serve_connection(stream, self.provider.clone(), self.options.clone()).await
    }
}

/// Drives one accepted connection through association, one exchange, release (§5).
async fn serve_connection<P: ServiceProvider>(
    mut stream: TcpStream,
    provider: Arc<P>,
    options: ServerOptions,
) -> Result<()> {
    crate::transport::apply_options(&stream, &TcpOptions::default())?;

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

    let _accepted = AcceptedContexts::from_negotiated(negotiated);
    let peer_max_pdu = if request.user_information.max_pdu_length == 0 {
        16384
    } else {
        request.user_information.max_pdu_length
    };
    let splitter = PduSplitter::new(peer_max_pdu, 16);
    let mut assembler = PdvAssembler::new();

    // -- message loop ----------------------------------------------------
    loop {
        let Some(pdu) = read_pdu(&mut stream).await? else {
            return Ok(()); // peer closed
        };
        match pdu {
            Pdu::PData(pdata) => {
                for unit in assembler.push_pdata(&pdata)? {
                    if unit.stream != Stream::Command {
                        continue;
                    }
                    let command = command_from_bytes(&unit.bytes)?;
                    match command.field {
                        CommandField::CEchoRq => {
                            let request = CEchoRequest::from_command(&command)?;
                            let response = provider.on_c_echo(&request).await?;
                            send_command(&mut stream, &splitter, unit.context_id, &response.to_command()).await?;
                        }
                        CommandField::CCancelRq => {
                            // nothing to cancel in this exchange
                        }
                        other => {
                            // unsupported request: answer ProcessingFailure
                            let mut response = Command::response(&command, crate::dimse::status::PROCESSING_FAILURE);
                            response.error_comment = Some(format!("unsupported command {other:?}"));
                            send_command(&mut stream, &splitter, unit.context_id, &response).await?;
                        }
                    }
                }
            }
            Pdu::ReleaseRq => {
                write_pdu(&mut stream, &Pdu::ReleaseRp).await?;
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
async fn send_command(
    stream: &mut TcpStream,
    splitter: &PduSplitter,
    context_id: u8,
    command: &Command,
) -> Result<()> {
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
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions { max_pdu_length: 16384, association_timeout: Duration::from_secs(5) }
    }
}

/// An open association from the SCU side (§9.2 expert layer).
pub struct Association {
    stream: TcpStream,
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
        let mut stream = TcpStream::connect(addr).await.map_err(NetworkError::Io)?;
        crate::transport::apply_options(&stream, &TcpOptions::default())?;

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

        // wait for the matching response
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
                        if command.field == CommandField::CEchoRsp
                            && command.message_id_being_responded_to == Some(message_id)
                        {
                            return CEchoResponse::from_command(&command);
                        }
                    }
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason })
                }
                Pdu::ReleaseRq => {
                    // the peer wants to close; answer and report
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
    use crate::services::VerificationProvider;

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