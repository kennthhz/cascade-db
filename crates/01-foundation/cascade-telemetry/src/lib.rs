//! Telemetry primitives — `Span`, `MetricsRegistry`, structured log helpers.
//!
//! v1 implementations are zero-cost no-ops. The §6.2 reservation is that the
//! *shape* of the API is fixed early so OTLP / Prometheus / log exporters can
//! plug in later without touching call sites.

/// A telemetry span handle. v1: zero-cost no-op.
///
/// Every hot-path function should accept `&Span` and forward it to children.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    _placeholder: (),
}

impl Span {
    /// A do-nothing span. The default; zero allocations, zero atomics.
    pub const fn noop() -> Self {
        Self { _placeholder: () }
    }
}

/// Central registry for counters, histograms, gauges.
///
/// v1: stores nothing; future phases wire an exporter.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    _placeholder: (),
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}
