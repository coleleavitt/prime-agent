//! Span sinks (TS `addSpanSink`): in-process consumers of every finished
//! span, beside the log and the optional OTLP export. A sink runs on the
//! thread that closed the span, so it must stay quick and never block; a
//! panicking sink is dropped from the set.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{Map, Value};

/// A finished span as a sink sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRecord {
    pub name: String,
    /// 32 lowercase hex digits.
    pub trace_id: String,
    /// 16 lowercase hex digits.
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub duration_ms: f64,
    pub attrs: Map<String, Value>,
    /// `Some` when the span failed.
    pub error: Option<String>,
}

/// One registered sink.
pub type SpanSink = Arc<dyn Fn(&SpanRecord) + Send + Sync>;

/// A registered sink's handle: [`remove_span_sink`] takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanSinkId(u64);

type Sinks = RwLock<Vec<(SpanSinkId, SpanSink)>>;

fn sinks() -> &'static Sinks {
    static SINKS: std::sync::OnceLock<Sinks> = std::sync::OnceLock::new();
    SINKS.get_or_init(Sinks::default)
}

/// Deliver every finished span to `sink` until [`remove_span_sink`].
pub fn add_span_sink(sink: SpanSink) -> SpanSinkId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = SpanSinkId(NEXT.fetch_add(1, Ordering::Relaxed));
    sinks()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((id, sink));
    id
}

/// Stop delivering to the sink `id`; `false` when it was not registered.
pub fn remove_span_sink(id: SpanSinkId) -> bool {
    let mut sinks = sinks()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = sinks.len();
    sinks.retain(|(registered, _)| *registered != id);
    sinks.len() != before
}

/// Whether any sink is registered (the layer skips building records
/// otherwise).
pub(crate) fn any() -> bool {
    !sinks()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty()
}

/// Deliver one finished span to every sink.
pub(crate) fn deliver(record: &SpanRecord) {
    let current: Vec<(SpanSinkId, SpanSink)> = sinks()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for (id, sink) in current {
        let delivered =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(record))).is_ok();
        if !delivered {
            remove_span_sink(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn record(name: &str) -> SpanRecord {
        SpanRecord {
            name: name.to_string(),
            trace_id: "0".repeat(32),
            span_id: "1".repeat(16),
            parent_span_id: None,
            duration_ms: 1.0,
            attrs: Map::new(),
            error: None,
        }
    }

    /// The sinks are process-global and the lib's other tests close spans
    /// concurrently: these sinks react only to this test's span names.
    const OWN: &str = "sink-test.";

    #[test]
    fn sinks_receive_spans_until_removed_and_a_panicking_one_is_dropped() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let id = add_span_sink({
            let seen = Arc::clone(&seen);
            Arc::new(move |span: &SpanRecord| {
                if span.name.starts_with(OWN) {
                    seen.lock().unwrap().push(span.name.clone());
                }
            })
        });
        let panicking = add_span_sink(Arc::new(|span: &SpanRecord| {
            assert!(!span.name.starts_with(OWN), "sink failure");
        }));
        deliver(&record("sink-test.a"));
        assert!(
            !remove_span_sink(panicking),
            "the panicking sink was dropped"
        );
        assert!(remove_span_sink(id));
        deliver(&record("sink-test.b"));
        assert_eq!(*seen.lock().unwrap(), vec!["sink-test.a".to_string()]);
    }
}
