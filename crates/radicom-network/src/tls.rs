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
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};

/// TLS material and policy for one endpoint (§2.4).
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// PEM certificate chain to present (server) or to trust (client CA bundle).
    pub cert_file: Option<std::path::PathBuf>,
    /// PEM private key matching `cert_file` (server only).
    pub key_file: Option<std::path::PathBuf>,
    /// Skip certificate verification (client only, testing).
    pub accept_any: bool,
    /// Override the SNI/verification name (client only).
    pub server_name: Option<String>,
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
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
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

        let builder = ClientConfig::builder();
        let config = if self.accept_any {
            // testing only: accept any server certificate
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification))
                .with_no_client_auth()
        } else {
            builder.with_root_certificates(roots).with_no_client_auth()
        };
        Ok(Arc::new(config))
    }

    /// The name to verify the server certificate against.
    pub fn server_name(&self) -> String {
        self.server_name.clone().unwrap_or_else(|| "localhost".to_string())
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
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(self.server_name())
            .map_err(|e| NetworkError::protocol_tls(format!("invalid server name: {e}")))?;
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
    fn server_name_defaults_to_localhost() {
        assert_eq!(TlsOptions::default().server_name(), "localhost");
        let named = TlsOptions { server_name: Some("pacs.example".into()), ..Default::default() };
        assert_eq!(named.server_name(), "pacs.example");
    }
}
