//! The `agent.jsonl` record shape (the TS logging contract): every entry is
//! one JSON object with the process context first, the trace ids, the
//! entry's own fields, and the reserved `ts`/`level`/`component`/`msg` last,
//! so they always win over a same-named field.

use std::time::{SystemTime, UNIX_EPOCH};

use pa_types::trace_context::TraceContext;
use serde_json::{Map, Value};

/// `component` of span records.
pub(crate) const TRACE_COMPONENT: &str = "trace";
/// `msg` of a finished span.
pub(crate) const SPAN_END_MSG: &str = "span_end";
/// `msg` of a long-running span's start.
pub(crate) const SPAN_START_MSG: &str = "span_start";

/// Keys the record writer owns; a field with one of these names is dropped.
const RESERVED_KEYS: [&str; 4] = ["ts", "level", "component", "msg"];

/// Entry severity, written as the TS level names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// The trace ids an entry is stamped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpanIds {
    pub(crate) context: TraceContext,
    pub(crate) parent_span_id: Option<u64>,
}

/// Fields every entry of this process starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessContext {
    pub(crate) pid: u32,
}

/// One entry under construction, serialized in the contract's key order.
pub(crate) struct Entry {
    fields: Map<String, Value>,
}

impl Entry {
    /// Process context, then the scoped session id, then the trace ids.
    pub(crate) fn new(
        process: &ProcessContext,
        session_id: Option<&str>,
        ids: Option<&SpanIds>,
    ) -> Self {
        let mut fields = Map::new();
        fields.insert("pid".to_string(), Value::from(process.pid));
        if let Some(session_id) = session_id {
            fields.insert("sessionId".to_string(), Value::from(session_id));
        }
        if let Some(ids) = ids {
            fields.insert(
                "traceId".to_string(),
                Value::from(ids.context.trace_id_hex()),
            );
            fields.insert("spanId".to_string(), Value::from(ids.context.span_id_hex()));
            if let Some(parent) = ids.parent_span_id {
                fields.insert(
                    "parentSpanId".to_string(),
                    Value::from(pa_types::trace_context::format_span_id(parent)),
                );
            }
        }
        Entry { fields }
    }

    /// Add one field; the reserved keys stay the writer's.
    pub(crate) fn field(&mut self, key: &str, value: Value) -> &mut Self {
        if !RESERVED_KEYS.contains(&key) {
            self.fields.insert(key.to_string(), value);
        }
        self
    }

    /// Close the entry with the reserved keys and render it as one line.
    pub(crate) fn finish(
        mut self,
        now: SystemTime,
        level: Level,
        component: &str,
        msg: &str,
    ) -> String {
        self.fields
            .insert("ts".to_string(), Value::from(format_iso_millis(now)));
        self.fields
            .insert("level".to_string(), Value::from(level.as_str()));
        self.fields
            .insert("component".to_string(), Value::from(component));
        self.fields.insert("msg".to_string(), Value::from(msg));
        Value::Object(self.fields).to_string()
    }
}

/// A JSON number with JavaScript `JSON.stringify` parity: integral values
/// print without a fractional part.
pub(crate) fn js_number(value: f64) -> Value {
    serde_json::to_value(pa_types::JsNumber(value)).unwrap_or(Value::Null)
}

/// Milliseconds rounded to three decimals (the TS `durationMs` precision).
pub(crate) fn duration_ms(elapsed: std::time::Duration) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "diagnostic span durations are far below 2^53 nanoseconds"
    )]
    let micros = elapsed.as_nanos() as f64 / 1000.0;
    micros.round() / 1000.0
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (JavaScript `Date#toISOString`).
pub(crate) fn format_iso_millis(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let millis = i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX);
    format_iso_from_ms(millis)
}

/// [`format_iso_millis`] for epoch milliseconds.
pub(crate) fn format_iso_from_ms(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(86_400_000);
    let in_day = epoch_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hours = in_day / 3_600_000;
    let minutes = in_day / 60_000 % 60;
    let seconds = in_day / 1000 % 60;
    let millis = in_day % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// `(year, month, day)` for days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// A fresh random context: a new trace when `parent` is `None`, else a child
/// span of the parent's trace with the parent's flags.
pub(crate) fn child_context(parent: Option<&TraceContext>) -> TraceContext {
    match parent {
        Some(parent) => TraceContext {
            trace_id: parent.trace_id,
            span_id: new_span_id(),
            flags: parent.flags,
        },
        None => TraceContext {
            trace_id: new_trace_id(),
            span_id: new_span_id(),
            flags: TraceContext::SAMPLED,
        },
    }
}

fn new_trace_id() -> u128 {
    loop {
        let id = uuid::Uuid::new_v4().as_u128();
        if id != 0 {
            return id;
        }
    }
}

fn new_span_id() -> u64 {
    loop {
        let (high, low) = uuid::Uuid::new_v4().as_u64_pair();
        let id = high ^ low;
        if id != 0 {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn formats_iso_timestamps_like_javascript() {
        assert_eq!(format_iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_iso_from_ms(1_788_782_400_123),
            "2026-09-07T12:00:00.123Z"
        );
        assert_eq!(
            format_iso_from_ms(951_782_400_000),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            format_iso_millis(UNIX_EPOCH + Duration::from_millis(1_788_782_400_123)),
            "2026-09-07T12:00:00.123Z"
        );
    }

    #[test]
    fn numbers_and_durations_print_like_javascript() {
        assert_eq!(js_number(1050.0).to_string(), "1050");
        assert_eq!(js_number(812.345).to_string(), "812.345");
        let durations: Vec<String> = [
            Duration::from_micros(812_345),
            Duration::from_nanos(1_500_600),
        ]
        .into_iter()
        .map(|elapsed| js_number(duration_ms(elapsed)).to_string())
        .collect();
        assert_eq!(durations, ["812.345", "1.501"]);
    }

    #[test]
    fn entries_keep_the_contract_key_order_and_reserved_keys() {
        let ids = SpanIds {
            context: TraceContext {
                trace_id: 0x0af7_6519_16cd_43dd_8448_eb21_1c80_319c,
                span_id: 0xb7ad_6b71_6920_3331,
                flags: 1,
            },
            parent_span_id: Some(0xc8be_7c82_7031_4442),
        };
        let mut entry = Entry::new(&ProcessContext { pid: 7 }, Some("s1"), Some(&ids));
        entry
            .field("detail", Value::from(1))
            .field("msg", Value::from("spoofed"))
            .field("level", Value::from("error"));
        let line = entry.finish(
            UNIX_EPOCH + Duration::from_millis(1_788_782_400_123),
            Level::Info,
            "test",
            "real",
        );
        assert_eq!(
            line,
            r#"{"pid":7,"sessionId":"s1","traceId":"0af7651916cd43dd8448eb211c80319c","spanId":"b7ad6b7169203331","parentSpanId":"c8be7c8270314442","detail":1,"ts":"2026-09-07T12:00:00.123Z","level":"info","component":"test","msg":"real"}"#
        );
    }

    #[test]
    fn minted_contexts_inherit_the_parent_trace() {
        let root = child_context(None);
        assert_ne!(root.trace_id, 0);
        assert_ne!(root.span_id, 0);
        assert_eq!(root.flags, TraceContext::SAMPLED);
        let child = child_context(Some(&TraceContext { flags: 0, ..root }));
        assert_eq!((child.trace_id, child.flags), (root.trace_id, 0));
        assert_ne!(child.span_id, root.span_id);
    }
}
