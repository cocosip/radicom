//! Config-driven server hosting helper (HOST-01; docs/01-architecture.md §11).
//!
//! A thin, framework-agnostic lifecycle wrapper over [`crate::server::DicomServer`]:
//! it turns a [`HostedServerConfig`] into a bound, running SCP that an application
//! embeds in its own runtime (a tokio task, systemd unit, container). No web
//! framework coupling — [`start`](HostedDicomServer::start) spawns the accept
//! loop on the caller's tokio runtime and [`stop`](HostedDicomServer::stop) drains
//! it.
//!
//! Divergence from the doc sketch: `connected_clients` is not reported, because
//! the SCP accept loop does not currently expose a per-connection hook; a fake
//! zero would be worse than an honest omission. `ae_title` is validated (a real
//! trust-boundary check) but the SCP negotiates by SOP class rather than filtering
//! the called AE.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

use crate::error::{NetworkError, Result};
use crate::server::{DicomServer, ServerOptions};
use crate::services::ServiceProvider;
use crate::store::StoreSink;

/// Configuration for a hosted DICOM server (docs/01-architecture.md §11).
#[derive(Debug, Clone)]
pub struct HostedServerConfig {
    /// TCP port to listen on (0 = any free port).
    pub port: u16,
    /// This SCP's AE title; validated at start (16 chars max, no padding spaces).
    pub ae_title: String,
    /// Upper bound on concurrent connections (0 = unlimited).
    pub max_clients: usize,
    /// Where received instances are spooled before the provider sees them; a
    /// path keeps the files, `None` spools to a temporary file.
    pub store_sink: Option<PathBuf>,
    /// TLS material; `None` serves plain TCP (`tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
}

impl Default for HostedServerConfig {
    fn default() -> Self {
        HostedServerConfig {
            port: 0,
            ae_title: "RADICOM_SCP".to_string(),
            max_clients: 0,
            store_sink: None,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

impl HostedServerConfig {
    /// The `ServerOptions` this config maps onto.
    fn server_options(&self) -> ServerOptions {
        ServerOptions {
            max_clients: self.max_clients,
            store_sink: match &self.store_sink {
                Some(dir) => StoreSink::SpoolToDir(dir.clone()),
                None => StoreSink::SpoolToTemp,
            },
            #[cfg(feature = "tls")]
            tls: self.tls.clone(),
            ..Default::default()
        }
    }

    /// Validates the AE title (PS3.5: ≤16 printable chars, no leading/trailing
    /// space). Returns the offending reason on failure.
    fn validate(&self) -> Result<()> {
        let ae = &self.ae_title;
        if ae.is_empty() || ae.len() > 16 {
            return Err(NetworkError::protocol("AE title must be 1..=16 characters"));
        }
        if ae.starts_with(' ') || ae.ends_with(' ') {
            return Err(NetworkError::protocol("AE title must not have leading/trailing spaces"));
        }
        if !ae.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
            return Err(NetworkError::protocol("AE title must be printable ASCII"));
        }
        Ok(())
    }
}

/// The observable health of a hosted server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHealth {
    /// True while the accept loop is bound and running.
    pub listening: bool,
    /// The most recent fatal listener error, if the loop exited.
    pub last_error: Option<String>,
}

struct State {
    addr: Option<SocketAddr>,
    task: Option<JoinHandle<()>>,
}

/// A lifecycle wrapper around a running DICOM server (fo-dicom AspNetCore package
/// equivalent).
pub struct HostedDicomServer<P: ServiceProvider + 'static> {
    config: HostedServerConfig,
    provider: Arc<P>,
    state: Mutex<State>,
    listening: AtomicBool,
    last_error: Arc<Mutex<Option<String>>>,
}

impl<P: ServiceProvider + 'static> HostedDicomServer<P> {
    /// Creates a server from a config and a service provider. It does not bind
    /// until [`start`](Self::start) is called.
    pub fn new(config: HostedServerConfig, provider: P) -> Self {
        HostedDicomServer {
            config,
            provider: Arc::new(provider),
            state: Mutex::new(State { addr: None, task: None }),
            listening: AtomicBool::new(false),
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    /// The configuration this server was built with.
    pub fn config(&self) -> &HostedServerConfig {
        &self.config
    }

    /// The bound address, or `None` when not started.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.state.lock().expect("state lock").addr
    }

    /// Binds and starts serving. Idempotent: calling it while already running is
    /// a no-op. The accept loop runs on the current tokio runtime.
    pub async fn start(&self) -> Result<()> {
        if self.listening.load(Ordering::SeqCst) {
            return Ok(()); // idempotent guard
        }
        self.config.validate()?;

        let server = DicomServer::bind(
            ("0.0.0.0", self.config.port),
            self.provider.clone(),
            self.config.server_options(),
        )
        .await?;
        let addr = server.local_addr();

        let error_slot = self.last_error.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = server.run().await {
                *error_slot.lock().expect("error lock") = Some(e.to_string());
            }
        });

        let mut state = self.state.lock().expect("state lock");
        state.addr = Some(addr);
        state.task = Some(task);
        *self.last_error.lock().expect("error lock") = None;
        self.listening.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stops the server: aborts the accept loop and clears the listening state.
    pub async fn stop(&self) -> Result<()> {
        let task = {
            let mut state = self.state.lock().expect("state lock");
            state.addr = None;
            state.task.take()
        };
        if let Some(task) = task {
            task.abort();
            // wait for the abort to settle; an aborted task yields a JoinError
            let _ = task.await;
        }
        self.listening.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Current health snapshot.
    pub fn health(&self) -> ServerHealth {
        ServerHealth {
            listening: self.listening.load(Ordering::SeqCst),
            last_error: self.last_error.lock().expect("error lock").clone(),
        }
    }
}

impl<P: ServiceProvider + 'static> Drop for HostedDicomServer<P> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(task) = state.task.take() {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;
    use crate::services::{CEchoRequest, CEchoResponse};

    struct EchoOnly;

    #[async_trait::async_trait]
    impl ServiceProvider for EchoOnly {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::VERIFICATION.to_string()]
        }
        async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
            Ok(CEchoResponse::success(request))
        }
    }

    #[tokio::test]
    async fn starts_listens_and_stops() {
        let config = HostedServerConfig { port: 0, ..Default::default() };
        let server = HostedDicomServer::new(config, EchoOnly);
        assert!(!server.health().listening);

        server.start().await.unwrap();
        assert!(server.health().listening);
        let addr = server.local_addr().expect("bound address");

        // a real TCP connection proves the listener is up (bind is 0.0.0.0, so
        // dial loopback on the bound port)
        let dial = SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let stream = tokio::net::TcpStream::connect(dial).await;
        assert!(stream.is_ok(), "the server accepts TCP connections");

        server.stop().await.unwrap();
        assert!(!server.health().listening);
        assert!(server.local_addr().is_none());
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let server = HostedDicomServer::new(HostedServerConfig::default(), EchoOnly);
        server.start().await.unwrap();
        let addr = server.local_addr();
        server.start().await.unwrap(); // second call is a no-op
        assert_eq!(server.local_addr(), addr, "the same listener is reused");
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn an_invalid_ae_title_is_rejected() {
        let config = HostedServerConfig { ae_title: "WAY_TOO_LONG_AE_TITLE_X".into(), ..Default::default() };
        let server = HostedDicomServer::new(config, EchoOnly);
        assert!(server.start().await.is_err(), "over-long AE title is rejected");
        assert!(!server.health().listening);
    }

    #[tokio::test]
    async fn stop_then_start_serves_again() {
        // §4.7: stop must clear the listening/addr pair, and a later start must
        // bind and accept connections again (there was no restart test).
        let server = HostedDicomServer::new(HostedServerConfig { port: 0, ..Default::default() }, EchoOnly);
        server.start().await.unwrap();
        let first = server.local_addr().expect("bound");
        let dial = SocketAddr::from(([127, 0, 0, 1], first.port()));
        assert!(tokio::net::TcpStream::connect(dial).await.is_ok(), "the first listener is up");

        server.stop().await.unwrap();
        assert!(!server.health().listening);
        assert!(server.local_addr().is_none(), "stop clears the bound address");

        server.start().await.unwrap();
        assert!(server.health().listening);
        let second = server.local_addr().expect("bound again after stop");
        let dial = SocketAddr::from(([127, 0, 0, 1], second.port()));
        assert!(tokio::net::TcpStream::connect(dial).await.is_ok(), "the restarted listener accepts connections");
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn health_reports_a_clean_state() {
        let server = HostedDicomServer::new(HostedServerConfig::default(), EchoOnly);
        let health = server.health();
        assert!(!health.listening);
        assert!(health.last_error.is_none());
    }
}
