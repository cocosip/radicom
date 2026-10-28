//! TLS transport (NET-01; docs/01-architecture.md §11).
//!
//! TLS wraps the same async byte stream the plain path uses, so the protocol
//! engine is unchanged: only the connection setup differs. The feature is
//! optional (`tls`), and the module compiles to nothing when it is off.

#![cfg(feature = "tls")]

use crate::error::{NetworkError, Result};
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::version::{TLS12, TLS13};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig, SupportedProtocolVersion};

/// The lowest TLS version this endpoint negotiates (§2.13).
///
/// fo-dicom pins `Tls12 | Tls13` explicitly; rustls only implements those two,
/// so a TLS 1.0/1.1 peer cannot be negotiated with at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsVersion {
    #[default]
    Tls12,
    Tls13,
}

impl TlsVersion {
    /// The protocol versions to offer, most preferred first.
    fn versions(self) -> &'static [&'static SupportedProtocolVersion] {
        const TLS12_AND_13: &[&SupportedProtocolVersion] = &[&TLS13, &TLS12];
        const TLS13_ONLY: &[&SupportedProtocolVersion] = &[&TLS13];
        match self {
            TlsVersion::Tls12 => TLS12_AND_13,
            TlsVersion::Tls13 => TLS13_ONLY,
        }
    }
}

/// TLS material and policy for one endpoint (§2.4).
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// PEM certificate chain to present (server) or to trust (client CA bundle).
    pub cert_file: Option<std::path::PathBuf>,
    /// PEM private key matching `cert_file` (server only).
    pub key_file: Option<std::path::PathBuf>,
    /// PEM certificate chain the client presents for mutual TLS.
    pub client_cert_file: Option<std::path::PathBuf>,
    /// PEM private key matching `client_cert_file`.
    pub client_key_file: Option<std::path::PathBuf>,
    /// PEM CA bundle the *server* verifies client certificates against.
    /// Setting it makes client certificates mandatory (mutual TLS, §2.13).
    pub client_ca_file: Option<std::path::PathBuf>,
    /// Skip certificate verification (client only, testing).
    pub accept_any: bool,
    /// Override the SNI/verification name (client only). When unset the peer's
    /// address is used, never a hardcoded name (§2.14).
    pub server_name: Option<String>,
    /// Lowest protocol version to negotiate (§2.13).
    pub min_version: TlsVersion,
}

impl TlsOptions {
    /// Server material from a PEM certificate chain and private key.
    pub fn server(cert_file: impl Into<std::path::PathBuf>, key_file: impl Into<std::path::PathBuf>) -> Self {
        TlsOptions { cert_file: Some(cert_file.into()), key_file: Some(key_file.into()), ..Default::default() }
    }

    /// Client material trusting the given CA bundle, or anything when `None`.
    pub fn client(ca_file: Option<impl Into<std::path::PathBuf>>) -> Self {
        TlsOptions { cert_file: ca_file.map(Into::into), accept_any: false, ..Default::default() }
    }

    /// Builds the server configuration (§2.4).
    pub fn server_config(&self) -> Result<Arc<ServerConfig>> {
        let cert_path = self
            .cert_file
            .as_ref()
            .ok_or(NetworkError::protocol("TLS server needs a certificate file"))?;
        let key_path = self
            .key_file
            .as_ref()
            .ok_or(NetworkError::protocol("TLS server needs a private key file"))?;

        let chain = load_certs(cert_path)?;
        let key = load_key(key_path)?;
        let builder = ServerConfig::builder_with_protocol_versions(self.min_version.versions());
        let config = match &self.client_ca_file {
            // a CA bundle for peers turns the association into mutual TLS
            Some(ca) => {
                let mut roots = RootCertStore::empty();
                for cert in load_certs(ca)? {
                    roots.add(cert).map_err(|e| NetworkError::protocol_tls(e.to_string()))?;
                }
                let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(|e| NetworkError::protocol_tls(format!("bad client CA bundle: {e}")))?;
                builder.with_client_cert_verifier(verifier).with_single_cert(chain, key)
            }
            None => builder.with_no_client_auth().with_single_cert(chain, key),
        }
        .map_err(|e| NetworkError::protocol_tls(e.to_string()))?;
        Ok(Arc::new(config))
    }

    /// Builds the client configuration (§2.4).
    pub fn client_config(&self) -> Result<Arc<ClientConfig>> {
        let mut roots = RootCertStore::empty();
        if let Some(ca) = &self.cert_file {
            for cert in load_certs(ca)? {
                roots.add(cert).map_err(|e| NetworkError::protocol_tls(e.to_string()))?;
            }
        }

        let builder = ClientConfig::builder_with_protocol_versions(self.min_version.versions());
        let builder = if self.accept_any {
            // testing only: accept any server certificate
            builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerification))
        } else {
            builder.with_root_certificates(roots)
        };
        let config = match (&self.client_cert_file, &self.client_key_file) {
            (Some(cert), Some(key)) => builder
                .with_client_auth_cert(load_certs(cert)?, load_key(key)?)
                .map_err(|e| NetworkError::protocol_tls(e.to_string()))?,
            (None, None) => builder.with_no_client_auth(),
            _ => {
                return Err(NetworkError::protocol_tls(
                    "a client certificate needs its private key and vice versa",
                ))
            }
        };
        Ok(Arc::new(config))
    }

    /// The configured verification name, when one was set (§2.14).
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    /// The name to verify the server certificate against.
    ///
    /// A DICOM TLS client knows the peer's address but not always its DNS name:
    /// an explicit `server_name` wins, otherwise the connected peer's IP is
    /// verified. Defaulting to `"localhost"` would accept any certificate
    /// issued for `localhost` (§2.14).
    fn verify_name(&self, peer: std::net::IpAddr) -> Result<ServerName<'static>> {
        match &self.server_name {
            Some(name) => ServerName::try_from(name.clone())
                .map_err(|e| NetworkError::protocol_tls(format!("invalid server name: {e}"))),
            None => Ok(ServerName::from(peer)),
        }
    }

    /// Performs the server handshake over an accepted TCP stream (§2.4).
    pub async fn accept(&self, stream: tokio::net::TcpStream) -> Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>> {
        let config = self.server_config()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        acceptor.accept(stream).await.map_err(tls_err)
    }

    /// Performs the client handshake over a connected TCP stream (§2.4).
    pub async fn connect(
        &self,
        stream: tokio::net::TcpStream,
    ) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
        let config = self.client_config()?;
        let connector = tokio_rustls::TlsConnector::from(config);
        let peer = stream.peer_addr().map_err(NetworkError::Io)?.ip();
        let name = self.verify_name(peer)?;
        connector.connect(name, stream).await.map_err(tls_err)
    }
}

/// Maps a handshake failure onto a TLS error.
fn tls_err(e: std::io::Error) -> NetworkError {
    NetworkError::Tls(e.to_string())
}

/// Loads a PEM certificate chain.
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path).map_err(NetworkError::Io)?;
    let parsed: std::result::Result<Vec<_>, _> = certs(&mut BufReader::new(data.as_slice())).collect();
    let chain = parsed.map_err(|e| NetworkError::protocol_tls(format!("bad certificate PEM: {e}")))?;
    if chain.is_empty() {
        return Err(NetworkError::protocol_tls("certificate file holds no certificate"));
    }
    Ok(chain)
}

/// Loads a PEM private key (PKCS#8 or PKCS#1).
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path).map_err(NetworkError::Io)?;
    if let Ok(Some(key)) = pkcs8_private_keys(&mut BufReader::new(data.as_slice())).next().transpose() {
        return Ok(PrivateKeyDer::Pkcs8(key));
    }
    if let Ok(Some(key)) = rsa_private_keys(&mut BufReader::new(data.as_slice())).next().transpose() {
        return Ok(PrivateKeyDer::Pkcs1(key));
    }
    Err(NetworkError::protocol_tls("private key file holds no usable key"))
}

/// Test-only verifier that accepts any certificate (§2.4).
#[derive(Debug)]
struct NoVerification;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> std::result::Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<tokio_rustls::rustls::client::danger::HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_requires_both_files() {
        let options = TlsOptions { cert_file: Some("cert.pem".into()), ..Default::default() };
        assert!(options.server_config().is_err(), "a key is mandatory");

        let options = TlsOptions { key_file: Some("key.pem".into()), ..Default::default() };
        assert!(options.server_config().is_err(), "a certificate is mandatory");
    }

    #[test]
    fn missing_files_are_io_errors() {
        let options = TlsOptions::server("does-not-exist.pem", "does-not-exist.key");
        match options.server_config() {
            Err(NetworkError::Io(_)) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn client_config_without_a_ca_builds_an_empty_root_store() {
        let options = TlsOptions::default();
        assert!(options.client_config().is_ok(), "an empty root store is valid");
    }

    #[test]
    fn client_config_accepts_any_when_asked() {
        let options = TlsOptions { accept_any: true, ..Default::default() };
        assert!(options.client_config().is_ok());
    }
    /// Writes a self-signed certificate and key, returning their paths.
    fn self_signed(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir().join(format!("radicom-tls-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (cert_path, key_path)
    }

    #[tokio::test]
    async fn tls_loopback_c_echo_succeeds() {
        use crate::association::sop_class;
        use crate::pdu::PresentationContext;
        use crate::server::{Association, ClientOptions, DicomServer, ServerOptions};
        use crate::services::VerificationProvider;

        let (cert, key) = self_signed("echo");
        let server_options = ServerOptions {
            tls: Some(TlsOptions::server(cert.clone(), key)),
            ..Default::default()
        };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), server_options)
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // trust the self-signed certificate through the CA bundle
        let client_options = ClientOptions {
            tls: Some(TlsOptions { cert_file: Some(cert), server_name: Some("localhost".into()), ..Default::default() }),
            ..Default::default()
        };
        let contexts =
            vec![PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2".to_string()])];
        let mut association = Association::open(addr, "SCU", "SCP", contexts, client_options).await.unwrap();

        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn tls_handshake_fails_without_a_trusted_certificate() {
        use crate::association::sop_class;
        use crate::pdu::PresentationContext;
        use crate::server::{Association, ClientOptions, DicomServer, ServerOptions};
        use crate::services::VerificationProvider;

        let (cert, key) = self_signed("untrusted");
        let server_options = ServerOptions { tls: Some(TlsOptions::server(cert, key)), ..Default::default() };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), server_options)
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        // no CA bundle and no accept_any: verification must fail
        let client_options = ClientOptions { tls: Some(TlsOptions::default()), ..Default::default() };
        let contexts =
            vec![PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2".to_string()])];
        let result = Association::open(addr, "SCU", "SCP", contexts, client_options).await;
        assert!(result.is_err(), "an untrusted certificate is rejected");
        let _ = handle.await;
    }

    #[tokio::test]
    async fn accept_any_skips_verification() {
        use crate::association::sop_class;
        use crate::pdu::PresentationContext;
        use crate::server::{Association, ClientOptions, DicomServer, ServerOptions};
        use crate::services::VerificationProvider;

        let (cert, key) = self_signed("any");
        let server_options = ServerOptions { tls: Some(TlsOptions::server(cert, key)), ..Default::default() };
        let server = DicomServer::bind("127.0.0.1:0", Arc::new(VerificationProvider), server_options)
            .await
            .unwrap();
        let addr = server.local_addr();
        let handle = tokio::spawn(async move { server.accept_one().await });

        let client_options =
            ClientOptions { tls: Some(TlsOptions { accept_any: true, ..Default::default() }), ..Default::default() };
        let contexts =
            vec![PresentationContext::new(1, sop_class::VERIFICATION, vec!["1.2.840.10008.1.2".to_string()])];
        let mut association = Association::open(addr, "SCU", "SCP", contexts, client_options).await.unwrap();
        let response = association.send_c_echo(sop_class::VERIFICATION).await.unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }
    #[test]
    fn there_is_no_hardcoded_server_name() {
        // §2.14: defaulting to "localhost" verified the wrong name; an unset
        // name means "verify the peer address we actually connected to"
        assert_eq!(TlsOptions::default().server_name(), None);
        let named = TlsOptions { server_name: Some("pacs.example".into()), ..Default::default() };
        assert_eq!(named.server_name(), Some("pacs.example"));
    }

    #[test]
    fn the_default_floor_is_tls12() {
        assert_eq!(TlsOptions::default().min_version, TlsVersion::Tls12);
    }

    #[test]
    fn a_client_certificate_needs_its_key() {
        let (cert, _key) = self_signed("client-half");
        let options = TlsOptions { client_cert_file: Some(cert), ..Default::default() };
        match options.client_config() {
            Err(NetworkError::Tls(message)) => assert!(message.contains("private key"), "got {message}"),
            other => panic!("expected a TLS error, got {other:?}"),
        }
    }

    /// Spins a TLS server and returns `(addr, join handle)`.
    async fn tls_server(
        options: TlsOptions,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<crate::error::Result<()>>) {
        use crate::server::{DicomServer, ServerOptions};
        use crate::services::VerificationProvider;

        let server = DicomServer::bind(
            "127.0.0.1:0",
            Arc::new(VerificationProvider),
            ServerOptions { tls: Some(options), ..Default::default() },
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        (addr, tokio::spawn(async move { server.accept_one().await }))
    }

    fn echo_contexts() -> Vec<crate::pdu::PresentationContext> {
        vec![crate::pdu::PresentationContext::new(
            1,
            crate::association::sop_class::VERIFICATION,
            vec!["1.2.840.10008.1.2".to_string()],
        )]
    }

    #[tokio::test]
    async fn a_mismatched_server_name_fails_verification() {
        use crate::server::{Association, ClientOptions};

        let (cert, key) = self_signed("mismatch");
        let (addr, handle) = tls_server(TlsOptions::server(cert.clone(), key)).await;
        let client_options = ClientOptions {
            tls: Some(TlsOptions {
                cert_file: Some(cert),
                // the certificate is issued for localhost, not for this name
                server_name: Some("pacs.example".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = Association::open(addr, "SCU", "SCP", echo_contexts(), client_options).await;
        match result {
            Err(NetworkError::Tls(_)) => {}
            other => panic!("expected a certificate name mismatch, got {other:?}"),
        }
        let _ = handle.await;
    }

    #[tokio::test]
    async fn an_unset_server_name_verifies_the_peer_address() {
        use crate::server::{Association, ClientOptions};

        // §2.14: without a configured name the client used to verify
        // "localhost", which this certificate satisfies — even though the peer
        // is 127.0.0.1 and the certificate says nothing about it.
        let (cert, key) = self_signed("bare-ip");
        let (addr, handle) = tls_server(TlsOptions::server(cert.clone(), key)).await;
        let client_options = ClientOptions {
            tls: Some(TlsOptions { cert_file: Some(cert), ..Default::default() }),
            ..Default::default()
        };
        let result = Association::open(addr, "SCU", "SCP", echo_contexts(), client_options).await;
        match result {
            Err(NetworkError::Tls(_)) => {}
            other => panic!("a localhost certificate must not satisfy 127.0.0.1, got {other:?}"),
        }
        let _ = handle.await;
    }

    #[tokio::test]
    async fn a_tls13_floor_refuses_a_tls12_only_client() {
        // §2.13: the floor is explicit, so a peer that can only speak 1.2 must
        // be turned away by a 1.3-only endpoint
        let (cert, key) = self_signed("floor");
        let (addr, handle) = tls_server(TlsOptions {
            min_version: TlsVersion::Tls13,
            ..TlsOptions::server(cert.clone(), key)
        })
        .await;

        let mut roots = RootCertStore::empty();
        for cert in load_certs(&cert).unwrap() {
            roots.add(cert).unwrap();
        }
        let config = ClientConfig::builder_with_protocol_versions(&[&TLS12])
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from("localhost".to_string()).unwrap();
        assert!(
            connector.connect(name, tcp).await.is_err(),
            "a TLS 1.3 floor must refuse a TLS 1.2-only peer"
        );
        let _ = handle.await;
    }

    #[tokio::test]
    async fn a_tls13_floor_still_serves_a_tls13_client() {
        use crate::server::{Association, ClientOptions};

        let (cert, key) = self_signed("floor-ok");
        let (addr, handle) = tls_server(TlsOptions {
            min_version: TlsVersion::Tls13,
            ..TlsOptions::server(cert.clone(), key)
        })
        .await;
        let client_options = ClientOptions {
            tls: Some(TlsOptions {
                cert_file: Some(cert),
                server_name: Some("localhost".into()),
                min_version: TlsVersion::Tls13,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut association = Association::open(addr, "SCU", "SCP", echo_contexts(), client_options).await.unwrap();
        let response = association
            .send_c_echo(crate::association::sop_class::VERIFICATION)
            .await
            .unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_tls11_client_hello_is_refused() {
        // rustls implements no version below TLS 1.2, so a peer that only
        // offers TLS 1.1 cannot be negotiated with. This drives the handshake
        // with a hand-built TLS 1.1 ClientHello and checks that the server
        // answers with a fatal alert instead of a ServerHello.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (cert, key) = self_signed("legacy");
        let (addr, handle) = tls_server(TlsOptions::server(cert, key)).await;

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(&legacy_client_hello()).await.unwrap();

        let mut reply = [0u8; 5];
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.read_exact(&mut reply)).await;
        // 0x15 = alert: the handshake was refused. An immediate close (or no
        // answer at all) is a refusal too, so only a real reply is inspected.
        if let Ok(Ok(_)) = read {
            assert_eq!(reply[0], 0x15, "expected a TLS alert, got {reply:?}");
        }
        match handle.await.unwrap() {
            Err(NetworkError::Tls(_)) => {}
            other => panic!("expected the server to refuse the handshake, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mutual_tls_accepts_a_configured_client_certificate() {
        use crate::server::{Association, ClientOptions};

        let (server_cert, server_key) = self_signed("mtls-server");
        let (client_cert, client_key) = self_signed("mtls-client");
        let (addr, handle) = tls_server(TlsOptions {
            client_ca_file: Some(client_cert.clone()),
            ..TlsOptions::server(server_cert.clone(), server_key)
        })
        .await;

        let client_options = ClientOptions {
            tls: Some(TlsOptions {
                cert_file: Some(server_cert),
                server_name: Some("localhost".into()),
                client_cert_file: Some(client_cert),
                client_key_file: Some(client_key),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut association = Association::open(addr, "SCU", "SCP", echo_contexts(), client_options).await.unwrap();
        let response = association
            .send_c_echo(crate::association::sop_class::VERIFICATION)
            .await
            .unwrap();
        assert_eq!(response.status, crate::dimse::status::SUCCESS);
        association.release().await.unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn mutual_tls_refuses_a_client_without_a_certificate() {
        use crate::server::{Association, ClientOptions};

        let (server_cert, server_key) = self_signed("mtls-server-bare");
        let (client_cert, _client_key) = self_signed("mtls-client-bare");
        let (addr, handle) = tls_server(TlsOptions {
            client_ca_file: Some(client_cert),
            ..TlsOptions::server(server_cert.clone(), server_key)
        })
        .await;
        let client_options = ClientOptions {
            tls: Some(TlsOptions {
                cert_file: Some(server_cert),
                server_name: Some("localhost".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = Association::open(addr, "SCU", "SCP", echo_contexts(), client_options).await;
        assert!(result.is_err(), "the server requires a client certificate");
        let _ = handle.await;
    }

    /// A TLS 1.1 ClientHello: record version 0x0301, client_version 0x0302 and
    /// no `supported_versions` extension.
    fn legacy_client_hello() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x02]); // client_version: TLS 1.1
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session id length
        body.extend_from_slice(&[0x00, 0x02, 0x00, 0x2f]); // TLS_RSA_WITH_AES_128_CBC_SHA
        body.extend_from_slice(&[0x01, 0x00]); // one compression method: null
        body.extend_from_slice(&[0x00, 0x00]); // no extensions

        let mut handshake = vec![0x01];
        handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        handshake.extend_from_slice(&body);

        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }
}
