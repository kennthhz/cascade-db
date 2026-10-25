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
/// v1: stores nothing; future phases wire an exporter. Callers publish by
/// canonical name (e.g. `reactor_stall_soft_total`) so the exporter contract
/// is fixed before any exporter exists.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    _placeholder: (),
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a counter delta under a canonical name. v1: a no-op — the
    /// name and call shape are the reservation.
    pub fn add(&self, _name: &'static str, _delta: u64) {}
}

/// Structured-log level for [`log`]. v1 carries the levels the runtime
/// actually emits (spec §7.2: INFO for soft stalls, WARN for hard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
}

type LogSink = dyn Fn(Level, &str, &[(&str, &str)]) + Send + Sync;

static LOG_SINK: std::sync::RwLock<Option<std::sync::Arc<LogSink>>> = std::sync::RwLock::new(None);

/// Emit one structured log record: an event name plus string fields.
///
/// v1: logfmt line to stderr. `install_log_sink` swaps in a capturing sink —
/// that is the §6.2 hook exporters replace, without touching call sites.
pub fn log(level: Level, event: &str, fields: &[(&str, &str)]) {
    let sink = LOG_SINK.read().expect("log sink lock").clone();
    match sink {
        Some(sink) => sink(level, event, fields),
        None => {
            let mut line = format!("level={level:?} event={event}");
            for (k, v) in fields {
                line.push_str(&format!(" {k}={v}"));
            }
            eprintln!("{line}");
        }
    }
}

/// Replace the log destination (tests capture records; exporters reformat).
/// `None` restores the default stderr logfmt output. Returns the previous
/// sink so a caller can put it back.
pub fn set_log_sink(sink: Option<std::sync::Arc<LogSink>>) -> Option<std::sync::Arc<LogSink>> {
    std::mem::replace(&mut *LOG_SINK.write().expect("log sink lock"), sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type CapturedRecords = Arc<Mutex<Vec<(Level, String, Vec<(String, String)>)>>>;

    #[test]
    fn log_records_land_in_the_installed_sink() {
        let records: CapturedRecords = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&records);
        let prev = set_log_sink(Some(Arc::new(move |level, event, fields| {
            capture.lock().unwrap().push((
                level,
                event.to_owned(),
                fields
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ));
        })));

        log(Level::Info, "reactor_stall", &[("gap_us", "1500")]);
        log(Level::Warn, "reactor_stall", &[("gap_us", "11000")]);

        // Restore before asserting so a panic here doesn't leak the sink.
        set_log_sink(prev);

        let got = records.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, Level::Info);
        assert_eq!(got[0].1, "reactor_stall");
        assert_eq!(got[1].0, Level::Warn);
        assert_eq!(got[1].2, vec![("gap_us".to_owned(), "11000".to_owned())]);
    }

    #[test]
    fn counter_publication_accepts_canonical_names() {
        let registry = MetricsRegistry::new();
        registry.add("reactor_stall_soft_total", 1);
        registry.add("reactor_stall_hard_total", 1);
    }
}
