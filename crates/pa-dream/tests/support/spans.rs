//! A `tracing` layer that records every span's name, parent and fields, so a
//! test can assert on the span tree a run emits (the TS `addSpanSink`).

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::Subscriber;
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

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<SpanRecord>>>);

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Sink {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Map::new();
        attrs.record(&mut Fields(&mut fields));
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .map(|parent| parent.id().into_u64());
        self.0
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
        let mut spans = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(span) = spans.iter_mut().rev().find(|span| span.id == id.into_u64()) {
            values.record(&mut Fields(&mut span.attrs));
        }
    }
}

/// Run `body` with a recording subscriber; return its value and the spans.
pub fn capture<T>(body: impl FnOnce() -> T) -> (T, Vec<SpanRecord>) {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::registry().with(sink.clone());
    let value = tracing::subscriber::with_default(subscriber, body);
    let spans = sink
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (value, spans)
}

/// The spans named `name`, in creation order.
pub fn named<'a>(spans: &'a [SpanRecord], name: &str) -> Vec<&'a SpanRecord> {
    spans.iter().filter(|span| span.name == name).collect()
}
