//! A `tracing` layer that records every span's name, parent and fields, so a
//! test can assert on the span tree a run emits (the TS `addSpanSink`).

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// One recorded span.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRecord {
    pub id: u64,
    pub name: &'static str,
    /// The parent span's id; `None` for a root.
    pub parent: Option<u64>,
    pub attrs: Map<String, Value>,
}

impl SpanRecord {
    /// One attribute.
    pub fn attr(&self, key: &str) -> Option<&Value> {
        self.attrs.get(key)
    }
}

struct Fields<'a>(&'a mut Map<String, Value>);

impl Visit for Fields<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().into(), serde_json::json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), Value::from(value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), Value::from(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), Value::from(format!("{value:?}")));
    }
}

type Records = Arc<Mutex<Vec<SpanRecord>>>;

thread_local! {
    /// The recording the current thread's `capture` collects into.
    static ACTIVE: std::cell::RefCell<Option<Records>> = const { std::cell::RefCell::new(None) };
}

fn active() -> Option<Records> {
    ACTIVE.with(|active| active.borrow().clone())
}

/// One process-wide subscriber that records into the capturing thread's
/// buffer. A global default (rather than a scoped one per test) keeps every
/// callsite's cached interest stable while tests run in parallel.
struct Sink;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Sink {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(records) = active() else {
            return;
        };
        let mut fields = Map::new();
        attrs.record(&mut Fields(&mut fields));
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .map(|parent| parent.id().into_u64());
        records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(SpanRecord {
                id: id.into_u64(),
                name: attrs.metadata().name(),
                parent,
                attrs: fields,
            });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let Some(records) = active() else {
            return;
        };
        let mut spans = records.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(span) = spans.iter_mut().rev().find(|span| span.id == id.into_u64()) {
            values.record(&mut Fields(&mut span.attrs));
        }
    }
}

static INSTALL: std::sync::Once = std::sync::Once::new();

/// Run `body` recording every span this thread opens; return its value and the spans.
pub fn capture<T>(body: impl FnOnce() -> T) -> (T, Vec<SpanRecord>) {
    INSTALL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Sink));
    });
    // A callsite first hit by another test while the global default was being
    // installed can cache "never" interest and stay silent for the binary's
    // life; recompute every callsite's interest against the installed sink.
    tracing::callsite::rebuild_interest_cache();
    let records = Records::default();
    ACTIVE.with(|active| *active.borrow_mut() = Some(Arc::clone(&records)));
    let value = body();
    ACTIVE.with(|active| *active.borrow_mut() = None);
    let spans = records
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (value, spans)
}

/// The spans named `name`, in creation order.
pub fn named<'a>(spans: &'a [SpanRecord], name: &str) -> Vec<&'a SpanRecord> {
    spans.iter().filter(|span| span.name == name).collect()
}
