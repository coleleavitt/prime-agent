//! The span recorder: a `tracing_subscriber` layer that gives every recorded
//! span a W3C context and writes the TS `agent.jsonl` records.
//!
//! What it records (everything else is disabled at the callsite, so it costs
//! nothing):
//! - spans and events from the workspace crates (`pa_*` targets) at INFO and
//!   above: a finished span is one `span_end` entry, an event one log line
//!   stamped with the ids of the span it happened in;
//! - records another process produced in the trace shape, forwarded under
//!   [`FORWARDED_RECORD_TARGET`] (the kernel runtime's spans).
//!
//! Field conventions: span fields become `attrs` (dotted names kept); an
//! `error` field (or an ERROR event carrying only `error`, as
//! `#[instrument(err)]` emits) marks the span failed with that text; an
//! event under [`SPAN_ATTRIBUTES_TARGET`] adds its fields to the attrs of
//! the span it happens in and writes nothing itself; a
//! `traceparent` field names a remote parent; `session.id` scopes the
//! `sessionId` of every entry written inside the span.

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use pa_types::trace_context::{
    TraceContext, FORWARDED_RECORD_FIELD, FORWARDED_RECORD_TARGET, REMOTE_PARENT_FIELD,
    SPAN_ATTRIBUTES_TARGET,
};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::otlp::{FinishedSpan, OtlpExporter};
use crate::record::{
    child_context, duration_ms, js_number, Entry, Level, ProcessContext, SpanIds, SPAN_END_MSG,
    SPAN_START_MSG, TRACE_COMPONENT,
};
use crate::writer::LogWriter;

/// Long-running operations whose start is written too, so a crash or hang
/// before `span_end` stays visible (TS `ACTIVE_OPERATION_SPANS`).
const ACTIVE_OPERATION_SPANS: [&str; 11] = [
    "client.turn",
    "agent.prompt",
    "historian.run",
    "kernel.start",
    "session.compact",
    "cron.job",
    "ravo.run",
    "update.self",
    "child.passivate",
    "child.delete",
    "rlm.child.run",
];

/// Forwarded non-span diagnostics that are kept, with their level (TS
/// `forwardKernelTraceEvent`).
const FORWARDED_DIAGNOSTICS: [(&str, Level); 3] = [
    ("command_no_output", Level::Warn),
    ("cargo_lock_wait", Level::Warn),
    ("command_progress", Level::Info),
];

/// The span field that scopes `sessionId`.
const SESSION_ID_FIELD: &str = "session.id";

/// Per-span state kept in the registry's extensions.
struct SpanState {
    ids: SpanIds,
    name: &'static str,
    started: Instant,
    attrs: Map<String, Value>,
    error: Option<String>,
    session_id: Option<String>,
}

/// The recorder layer. Cheap to clone; clones share the writer.
#[derive(Clone)]
pub struct TraceLayer {
    process: ProcessContext,
    inbound: Option<TraceContext>,
    writer: Arc<LogWriter>,
    otlp: Option<Arc<OtlpExporter>>,
}

impl TraceLayer {
    pub(crate) fn new(
        writer: Arc<LogWriter>,
        inbound: Option<TraceContext>,
        otlp: Option<Arc<OtlpExporter>>,
    ) -> Self {
        TraceLayer {
            process: ProcessContext {
                pid: std::process::id(),
            },
            inbound,
            writer,
            otlp,
        }
    }

    /// The context handed to this process through `TRACEPARENT`, which every
    /// root span becomes a child of.
    pub(crate) fn inbound(&self) -> Option<TraceContext> {
        self.inbound
    }

    fn write(&self, line: String) {
        self.writer.write(line);
    }

    /// Forward a record another process produced: span records need their
    /// ids, a failed span's `attrs.error` is hoisted to `error`, and only the
    /// known diagnostics are kept.
    fn forward(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let Some(Value::String(text)) = visitor.fields.remove(FORWARDED_RECORD_FIELD) else {
            return;
        };
        let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(Value::String(msg)) = fields.remove("msg") else {
            return;
        };
        fields.remove("event");
        fields.remove("id");
        let has_ids = ["traceId", "spanId"]
            .iter()
            .all(|key| fields.get(*key).is_some_and(Value::is_string));
        let level = if msg == SPAN_END_MSG || msg == SPAN_START_MSG {
            if !has_ids || !fields.get("name").is_some_and(Value::is_string) {
                return;
            }
            let failed = msg == SPAN_END_MSG
                && fields.get("status").and_then(Value::as_str) == Some("error");
            if failed && !fields.get("error").is_some_and(Value::is_string) {
                let hoisted = fields
                    .get("attrs")
                    .and_then(|attrs| attrs.get("error"))
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty())
                    .map(str::to_string);
                if let Some(error) = hoisted {
                    fields.insert("error".to_string(), Value::String(error));
                }
            }
            if failed {
                Level::Warn
            } else {
                Level::Info
            }
        } else {
            let known = FORWARDED_DIAGNOSTICS
                .iter()
                .find(|(name, _)| *name == msg)
                .map(|(_, level)| *level);
            match known {
                Some(level) if has_ids => level,
                Some(_) | None => return,
            }
        };
        let mut entry = Entry::new(&self.process, None, None);
        for (key, value) in fields {
            entry.field(&key, value);
        }
        self.write(entry.finish(SystemTime::now(), level, TRACE_COMPONENT, &msg));
    }
}

/// Whether a callsite reaches the recorder at all.
fn recorded(metadata: &Metadata<'_>) -> bool {
    metadata.target() == FORWARDED_RECORD_TARGET
        || metadata.target() == SPAN_ATTRIBUTES_TARGET
        || (metadata.target().starts_with("pa_") && *metadata.level() <= tracing::Level::INFO)
}

fn level_of(level: tracing::Level) -> Level {
    match level {
        tracing::Level::ERROR => Level::Error,
        tracing::Level::WARN => Level::Warn,
        tracing::Level::INFO => Level::Info,
        tracing::Level::DEBUG | tracing::Level::TRACE => Level::Debug,
    }
}

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if recorded(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        recorded(metadata)
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        let parent = span.parent().and_then(|parent| {
            parent
                .extensions()
                .get::<SpanState>()
                .map(|state| (state.ids.context, state.session_id.clone()))
        });
        let remote = visitor
            .remote_parent
            .as_deref()
            .and_then(TraceContext::parse);
        let parent_context = remote
            .or(parent.as_ref().map(|(context, _)| *context))
            .or(self.inbound);
        let ids = SpanIds {
            context: child_context(parent_context.as_ref()),
            parent_span_id: parent_context.map(|context| context.span_id),
        };
        let session_id = visitor
            .fields
            .get(SESSION_ID_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| parent.and_then(|(_, session_id)| session_id));
        let name = span.name();
        if ACTIVE_OPERATION_SPANS.contains(&name) {
            let mut entry = Entry::new(&self.process, session_id.as_deref(), Some(&ids));
            entry
                .field("name", Value::from(name))
                .field("attrs", Value::Object(visitor.fields.clone()));
            self.write(entry.finish(
                SystemTime::now(),
                Level::Info,
                TRACE_COMPONENT,
                SPAN_START_MSG,
            ));
        }
        span.extensions_mut().insert(SpanState {
            ids,
            name,
            started: Instant::now(),
            attrs: visitor.fields,
            error: visitor.error,
            session_id,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        let Some(state) = extensions.get_mut::<SpanState>() else {
            return;
        };
        if let Some(Value::String(session_id)) = visitor.fields.get(SESSION_ID_FIELD) {
            state.session_id = Some(session_id.clone());
        }
        state.attrs.extend(visitor.fields);
        if visitor.error.is_some() {
            state.error = visitor.error;
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() == FORWARDED_RECORD_TARGET {
            self.forward(event);
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let span = ctx.event_span(event);
        if metadata.target() == SPAN_ATTRIBUTES_TARGET {
            if let Some(span) = &span {
                if let Some(state) = span.extensions_mut().get_mut::<SpanState>() {
                    state.attrs.extend(visitor.fields);
                }
            }
            return;
        }
        let Some(message) = visitor.message else {
            // `#[instrument(err)]`: the error ends the span that returned it.
            if *metadata.level() == tracing::Level::ERROR && visitor.error.is_some() {
                if let Some(span) = &span {
                    if let Some(state) = span.extensions_mut().get_mut::<SpanState>() {
                        state.error = visitor.error;
                    }
                }
            }
            return;
        };
        let scope = span.as_ref().and_then(|span| {
            span.extensions()
                .get::<SpanState>()
                .map(|state| (state.ids, state.session_id.clone()))
        });
        let (ids, session_id) = match scope {
            Some((ids, session_id)) => (Some(ids), session_id),
            None => (None, None),
        };
        let mut entry = Entry::new(&self.process, session_id.as_deref(), ids.as_ref());
        for (key, value) in visitor.fields {
            entry.field(&key, value);
        }
        if let Some(error) = visitor.error {
            entry.field("error", Value::String(error));
        }
        self.write(entry.finish(
            SystemTime::now(),
            level_of(*metadata.level()),
            metadata.target(),
            &message,
        ));
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(state) = span.extensions_mut().remove::<SpanState>() else {
            return;
        };
        let duration = duration_ms(state.started.elapsed());
        let failed = state.error.is_some();
        let mut entry = Entry::new(&self.process, state.session_id.as_deref(), Some(&state.ids));
        entry
            .field("name", Value::from(state.name))
            .field("durationMs", js_number(duration))
            .field("status", Value::from(if failed { "error" } else { "ok" }))
            .field("attrs", Value::Object(state.attrs.clone()));
        if let Some(error) = &state.error {
            entry.field("error", Value::String(error.clone()));
        }
        let level = if failed { Level::Warn } else { Level::Info };
        self.write(entry.finish(SystemTime::now(), level, TRACE_COMPONENT, SPAN_END_MSG));
        if let Some(otlp) = &self.otlp {
            otlp.export(FinishedSpan {
                name: state.name.to_string(),
                ids: state.ids,
                duration_ms: duration,
                attrs: state.attrs,
                error: state.error,
            });
        }
    }
}

/// Collects span/event fields as JSON values, splitting out the message,
/// the error, and a remote parent.
#[derive(Default)]
struct FieldVisitor {
    fields: Map<String, Value>,
    message: Option<String>,
    error: Option<String>,
    remote_parent: Option<String>,
}

impl FieldVisitor {
    fn put(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => self.message = Some(value_text(value)),
            "error" => self.error = Some(value_text(value)),
            name if name == REMOTE_PARENT_FIELD => self.remote_parent = Some(value_text(value)),
            name => {
                self.fields.insert(name.to_string(), value);
            }
        }
    }
}

fn value_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

impl Visit for FieldVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, js_number(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::from(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, Value::from(format!("{value:?}")));
    }
}

/// The context of the innermost recorded span on the calling thread, or the
/// inbound `TRACEPARENT` context outside every span (the answer behind
/// [`pa_types::trace_context::current`]).
pub(crate) fn current_context() -> Option<TraceContext> {
    tracing::dispatcher::get_default(|dispatch| {
        let from_span = dispatch.current_span().id().and_then(|id| {
            let registry = dispatch.downcast_ref::<tracing_subscriber::Registry>()?;
            let span = registry.span(id)?;
            let extensions = span.extensions();
            extensions.get::<SpanState>().map(|state| state.ids.context)
        });
        from_span.or_else(|| {
            dispatch
                .downcast_ref::<TraceLayer>()
                .and_then(TraceLayer::inbound)
        })
    })
}

#[cfg(test)]
mod tests;
