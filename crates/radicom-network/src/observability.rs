//! Observability instrumentation (OBS-01; docs/07-network.md §10).
//!
//! A pluggable [`MetricsCollector`] receives byte and lifecycle events. The
//! server wires one in through [`crate::server::ServerOptions::metrics`]; a
//! user supplies any implementation. [`CountingMetrics`] is a ready-made
//! in-process counter implementation, and [`NoopMetrics`] discards everything.
//!
//! The collector is the seam OBS-02 plugs an OpenTelemetry-backed implementation
//! into: the metric names below mirror fo-dicom's Instrumentation package.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Metric names mirrored from fo-dicom's Instrumentation package.
pub mod names {
    pub const BYTES_SENT: &str = "dicom.bytes.sent";
    pub const BYTES_RECEIVED: &str = "dicom.bytes.received";
    pub const CONNECTIONS_OPENED: &str = "dicom.connections.opened";
    pub const CONNECTIONS_CLOSED: &str = "dicom.connections.closed";
    pub const ASSOCIATIONS_OPENED: &str = "dicom.associations.opened";
    pub const ASSOCIATIONS_CLOSED: &str = "dicom.associations.closed";
}

/// A metrics sink. Every method defaults to a no-op so implementations override
/// only what they care about (fo-dicom `MetricsCollector`).
pub trait MetricsCollector: Send + Sync + std::fmt::Debug {
    fn on_bytes_sent(&self, _bytes: u64) {}
    fn on_bytes_received(&self, _bytes: u64) {}
    fn on_connection_opened(&self) {}
    fn on_connection_closed(&self) {}
    fn on_association_opened(&self) {}
    fn on_association_closed(&self) {}
}

/// Discards every event.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl MetricsCollector for NoopMetrics {}

/// A snapshot of [`CountingMetrics`] counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub connections_opened: u64,
    pub connections_closed: u64,
    pub associations_opened: u64,
    pub associations_closed: u64,
}

impl MetricsSnapshot {
    /// Connections currently open (opened − closed, saturating).
    pub fn active_connections(&self) -> u64 {
        self.connections_opened.saturating_sub(self.connections_closed)
    }
}

/// An in-process counter implementation of [`MetricsCollector`].
#[derive(Debug, Default)]
pub struct CountingMetrics {
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    connections_opened: AtomicU64,
    connections_closed: AtomicU64,
    associations_opened: AtomicU64,
    associations_closed: AtomicU64,
}

impl CountingMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the current counters.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            connections_opened: self.connections_opened.load(Ordering::Relaxed),
            connections_closed: self.connections_closed.load(Ordering::Relaxed),
            associations_opened: self.associations_opened.load(Ordering::Relaxed),
            associations_closed: self.associations_closed.load(Ordering::Relaxed),
        }
    }
}

impl MetricsCollector for CountingMetrics {
    fn on_bytes_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }
    fn on_bytes_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }
    fn on_connection_opened(&self) {
        self.connections_opened.fetch_add(1, Ordering::Relaxed);
    }
    fn on_connection_closed(&self) {
        self.connections_closed.fetch_add(1, Ordering::Relaxed);
    }
    fn on_association_opened(&self) {
        self.associations_opened.fetch_add(1, Ordering::Relaxed);
    }
    fn on_association_closed(&self) {
        self.associations_closed.fetch_add(1, Ordering::Relaxed);
    }
}

/// An `AsyncRead`/`AsyncWrite` wrapper that reports transferred bytes to a
/// collector (one insertion point measures the whole connection).
pub struct CountingStream<S> {
    inner: S,
    metrics: Option<Arc<dyn MetricsCollector>>,
}

impl<S> CountingStream<S> {
    pub fn new(inner: S, metrics: Option<Arc<dyn MetricsCollector>>) -> Self {
        CountingStream { inner, metrics }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountingStream<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &result {
            let read = buf.filled().len().saturating_sub(before) as u64;
            if read > 0 {
                if let Some(metrics) = &this.metrics {
                    metrics.on_bytes_received(read);
                }
            }
        }
        result
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountingStream<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &result {
            if let (Some(metrics), true) = (&this.metrics, *n > 0) {
                metrics.on_bytes_sent(*n as u64);
            }
        }
        result
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl<S: std::fmt::Debug> std::fmt::Debug for CountingStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingStream").field("inner", &self.inner).finish()
    }
}

/// A drop guard that reports a connection open/close pair.
pub struct ConnectionGuard(Option<Arc<dyn MetricsCollector>>);

impl ConnectionGuard {
    pub fn new(metrics: Option<Arc<dyn MetricsCollector>>) -> Self {
        if let Some(m) = &metrics {
            m.on_connection_opened();
        }
        ConnectionGuard(metrics)
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(m) = &self.0 {
            m.on_connection_closed();
        }
    }
}

/// A drop guard that reports an association open/close pair.
pub struct AssociationGuard(Option<Arc<dyn MetricsCollector>>);

impl AssociationGuard {
    pub fn new(metrics: Option<Arc<dyn MetricsCollector>>) -> Self {
        if let Some(m) = &metrics {
            m.on_association_opened();
        }
        AssociationGuard(metrics)
    }
}

impl Drop for AssociationGuard {
    fn drop(&mut self) {
        if let Some(m) = &self.0 {
            m.on_association_closed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_metrics_accumulate_every_event() {
        let m = CountingMetrics::new();
        m.on_bytes_sent(100);
        m.on_bytes_sent(50);
        m.on_bytes_received(30);
        m.on_connection_opened();
        m.on_connection_opened();
        m.on_connection_closed();
        m.on_association_opened();
        m.on_association_closed();
        let s = m.snapshot();
        assert_eq!(s.bytes_sent, 150);
        assert_eq!(s.bytes_received, 30);
        assert_eq!(s.connections_opened, 2);
        assert_eq!(s.connections_closed, 1);
        assert_eq!(s.active_connections(), 1);
        assert_eq!(s.associations_opened, 1);
        assert_eq!(s.associations_closed, 1);
    }

    #[tokio::test]
    async fn a_counting_stream_reports_transferred_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let m = Arc::new(CountingMetrics::new());
        // an in-memory duplex: write into one side, read from the wrapped side
        let (a, b) = tokio::io::duplex(64);
        let mut stream = CountingStream::new(a, Some(m.clone() as Arc<dyn MetricsCollector>));

        // write 5 bytes through the wrapper
        stream.write_all(b"hello").await.unwrap();
        stream.flush().await.unwrap();
        let mut reader = b;
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).await.unwrap();
        let s = m.snapshot();
        assert_eq!(s.bytes_sent, 5, "writes are counted");
    }

    #[test]
    fn guards_report_open_and_close() {
        let m = Arc::new(CountingMetrics::new());
        {
            let _c = ConnectionGuard::new(Some(m.clone() as Arc<dyn MetricsCollector>));
            let _a = AssociationGuard::new(Some(m.clone() as Arc<dyn MetricsCollector>));
            assert_eq!(m.snapshot().active_connections(), 1);
            assert_eq!(m.snapshot().associations_opened, 1);
        }
        let s = m.snapshot();
        assert_eq!(s.connections_closed, 1);
        assert_eq!(s.associations_closed, 1);
        assert_eq!(s.active_connections(), 0);
    }

    #[test]
    fn the_noop_collector_is_a_safe_default() {
        let m: Arc<dyn MetricsCollector> = Arc::new(NoopMetrics);
        m.on_bytes_sent(1);
        m.on_connection_opened();
        // nothing to observe; the point is it must not panic
    }
}
