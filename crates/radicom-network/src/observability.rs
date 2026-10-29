//! Observability instrumentation (OBS-01; docs/01-architecture.md §11).
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
        self.connections_opened
            .saturating_sub(self.connections_closed)
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

/// OBS-02: emits the metrics through `tracing` using the `monotonic_counter.*` /
/// `gauge.*` field conventions that downstream `tracing-opentelemetry` (or any
/// metrics layer) turns into OTel instruments — with **no** `opentelemetry`
/// dependency in this crate. This mirrors fo-dicom's `FO-DICOM.Instrumentation`
/// package, which emits via .NET's built-in `System.Diagnostics.Metrics` and
/// only references OpenTelemetry in optional `AddMeter`/`AddSource` glue.
///
/// Metric-name mapping (ours ↔ fo-dicom):
/// - `network_sent_bytes`     ↔ `fellowoakdicom.network.sent`
/// - `network_received_bytes` ↔ `fellowoakdicom.network.received`
/// - `network_connections`    ↔ `fellowoakdicom.network.connections`
/// - `network_concurrent_connections` (gauge) ↔ `fellowoakdicom.network.concurrentconnections`
#[cfg(feature = "otel")]
mod tracing_metrics {
    use super::MetricsCollector;
    use std::sync::atomic::{AtomicI64, Ordering};

    const TARGET: &str = "radicom::network";

    #[derive(Debug, Default)]
    pub struct TracingMetrics {
        concurrent: AtomicI64,
    }

    impl TracingMetrics {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl MetricsCollector for TracingMetrics {
        fn on_bytes_sent(&self, bytes: u64) {
            tracing::debug!(target: TARGET, { monotonic_counter.network_sent_bytes = bytes }, "network bytes sent");
        }
        fn on_bytes_received(&self, bytes: u64) {
            tracing::debug!(target: TARGET, { monotonic_counter.network_received_bytes = bytes }, "network bytes received");
        }
        fn on_connection_opened(&self) {
            tracing::debug!(target: TARGET, { monotonic_counter.network_connections = 1_i64 }, "connection opened");
            let current = self.concurrent.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::debug!(target: TARGET, { gauge.network_concurrent_connections = current }, "concurrent connections");
        }
        fn on_connection_closed(&self) {
            let current = self.concurrent.fetch_sub(1, Ordering::Relaxed) - 1;
            tracing::debug!(target: TARGET, { gauge.network_concurrent_connections = current }, "concurrent connections");
        }
        fn on_association_opened(&self) {
            tracing::debug!(target: TARGET, { monotonic_counter.network_associations = 1_i64 }, "association opened");
        }
    }
}

#[cfg(feature = "otel")]
pub use tracing_metrics::TracingMetrics;

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
        f.debug_struct("CountingStream")
            .field("inner", &self.inner)
            .finish()
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

    #[cfg(feature = "otel")]
    #[test]
    fn tracing_metrics_emit_metric_events() {
        use std::sync::atomic::AtomicUsize;

        #[derive(Clone)]
        struct Counter(Arc<AtomicUsize>);
        impl tracing::Subscriber for Counter {
            fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {
            }
            fn event(&self, _event: &tracing::Event<'_>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
        }

        let count = Arc::new(AtomicUsize::new(0));
        let metrics = TracingMetrics::new();
        tracing::subscriber::with_default(Counter(count.clone()), || {
            metrics.on_bytes_sent(10);
            metrics.on_bytes_received(20);
            metrics.on_connection_opened(); // counter + gauge
            metrics.on_connection_closed(); // gauge
            metrics.on_association_opened(); // counter
        });
        // 5 calls; connection_open contributes two events => 6
        assert!(
            count.load(Ordering::Relaxed) >= 6,
            "expected metric events, got {}",
            count.load(Ordering::Relaxed)
        );
    }
}
