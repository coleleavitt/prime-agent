//! Recorder behaviour (TS `logging-trace.test.ts`, the forwarding rules of
//! `repl-manager.ts`, and the span start/end contract).

use std::path::Path;
use std::time::Duration;

use pa_types::trace_context::TraceContext;
use serde_json::{json, Map, Value};
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::log_file::RotatingLog;

const INBOUND: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

/// Run `body` under a recorder writing to `<dir>/agent.jsonl` and return
/// every entry, with `ts` dropped and `durationMs` replaced by `"<ms>"`.
fn record(
    dir: &Path,
    inbound: Option<TraceContext>,
    body: impl FnOnce(),
) -> Vec<Map<String, Value>> {
    let path = dir.join("agent.jsonl");
    let writer = Arc::new(LogWriter::new(RotatingLog::new(path.clone())));
    let layer = TraceLayer::new(Arc::clone(&writer), inbound, None);
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), body);
    assert!(writer.flush(Duration::from_secs(30)));
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let Ok(Value::Object(mut entry)) = serde_json::from_str::<Value>(line) else {
                panic!("not an object: {line}");
            };
            assert!(entry.remove("ts").is_some_and(|ts| ts.is_string()));
            assert_eq!(entry.remove("pid"), Some(Value::from(std::process::id())));
            if entry.contains_key("durationMs") {
                entry.insert("durationMs".to_string(), Value::from("<ms>"));
            }
            entry
        })
        .collect()
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn ids(entry: &Map<String, Value>) -> (String, String) {
    (
        entry["traceId"].as_str().unwrap_or_default().to_string(),
        entry["spanId"].as_str().unwrap_or_default().to_string(),
    )
}

#[test]
fn stamps_lines_with_the_active_span_and_its_scoped_session_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = record(dir.path(), None, || {
        tracing::info_span!("agent.prompt", session.id = "session-1").in_scope(|| {
            tracing::info!(detail = 1, msg = "spoofed", "inside");
            tracing::info_span!("child.work").in_scope(|| tracing::warn!("nested"));
        });
        tracing::info!("outside");
        tracing::debug!("below the recorded level");
    });
    let (trace, prompt) = ids(&entries[0]);
    let (_, child) = ids(&entries[2]);
    assert_eq!(
        entries,
        vec![
            object(
                json!({"sessionId": "session-1", "traceId": trace, "spanId": prompt, "name": "agent.prompt", "attrs": {"session.id": "session-1"}, "level": "info", "component": "trace", "msg": "span_start"})
            ),
            object(
                json!({"sessionId": "session-1", "traceId": trace, "spanId": prompt, "detail": 1, "level": "info", "component": "pa_trace::layer::tests", "msg": "inside"})
            ),
            object(
                json!({"sessionId": "session-1", "traceId": trace, "spanId": child, "parentSpanId": prompt, "level": "warn", "component": "pa_trace::layer::tests", "msg": "nested"})
            ),
            object(
                json!({"sessionId": "session-1", "traceId": trace, "spanId": child, "parentSpanId": prompt, "name": "child.work", "durationMs": "<ms>", "status": "ok", "attrs": {}, "level": "info", "component": "trace", "msg": "span_end"})
            ),
            object(
                json!({"sessionId": "session-1", "traceId": trace, "spanId": prompt, "name": "agent.prompt", "durationMs": "<ms>", "status": "ok", "attrs": {"session.id": "session-1"}, "level": "info", "component": "trace", "msg": "span_end"})
            ),
            object(
                json!({"level": "info", "component": "pa_trace::layer::tests", "msg": "outside"})
            ),
        ]
    );
}

#[test]
fn parents_root_spans_to_the_inbound_traceparent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let inbound = TraceContext::parse(INBOUND);
    let entries = record(dir.path(), inbound, || {
        tracing::info_span!("child").in_scope(|| {});
    });
    assert_eq!(
        (
            entries[0]["traceId"].clone(),
            entries[0]["parentSpanId"].clone()
        ),
        (
            Value::from("0af7651916cd43dd8448eb211c80319c"),
            Value::from("b7ad6b7169203331")
        )
    );
}

#[test]
fn a_traceparent_field_names_a_remote_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = record(dir.path(), None, || {
        tracing::info_span!("outer").in_scope(|| {
            tracing::info_span!("daemon.command", traceparent = INBOUND).in_scope(|| {});
        });
    });
    let command = &entries[0];
    assert_eq!(
        (
            command["name"].clone(),
            command["traceId"].clone(),
            command["parentSpanId"].clone(),
            command["attrs"].clone()
        ),
        (
            Value::from("daemon.command"),
            Value::from("0af7651916cd43dd8448eb211c80319c"),
            Value::from("b7ad6b7169203331"),
            json!({})
        )
    );
}

#[test]
fn recorded_errors_fail_the_span_and_instrument_err_reports_on_its_span() {
    #[tracing::instrument(level = "info", name = "kernel.start", err(Display))]
    fn failing_start() -> Result<(), String> {
        Err("spawn error".to_string())
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = record(dir.path(), None, || {
        let span = tracing::info_span!(
            "tool.execute",
            tool.name = "bash",
            error = tracing::field::Empty
        );
        span.in_scope(|| {});
        span.record("error", "exit 1");
        drop(span);
        let _ = failing_start();
    });
    let summary: Vec<(Value, Value, Value, Value)> = entries
        .iter()
        .map(|entry| {
            let field = |key: &str| entry.get(key).cloned().unwrap_or(Value::Null);
            (field("msg"), field("name"), field("status"), field("error"))
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                json!("span_end"),
                json!("tool.execute"),
                json!("error"),
                json!("exit 1")
            ),
            (
                json!("span_start"),
                json!("kernel.start"),
                Value::Null,
                Value::Null
            ),
            (
                json!("span_end"),
                json!("kernel.start"),
                json!("error"),
                json!("spawn error")
            ),
        ]
    );
    assert_eq!(entries[0]["level"], "warn");
}

#[test]
fn forwards_runtime_records_with_the_ts_rules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let forward = |record: Value| {
        tracing::debug!(
            target: pa_types::trace_context::FORWARDED_RECORD_TARGET,
            record = %record,
            "kernel trace record"
        );
    };
    let entries = record(dir.path(), None, || {
        forward(
            json!({"event": "trace", "id": "r1", "msg": "span_end", "name": "kernel.cell", "traceId": "t1", "spanId": "s1", "parentSpanId": "p1", "durationMs": 1.5, "status": "error", "attrs": {"error": "ValueError: x"}}),
        );
        forward(
            json!({"event": "trace", "id": "r1", "msg": "span_start", "name": "bash.command", "traceId": "t1", "spanId": "s2", "attrs": {}}),
        );
        forward(
            json!({"event": "trace", "msg": "command_no_output", "traceId": "t1", "spanId": "s2", "seconds": 30}),
        );
        forward(
            json!({"event": "trace", "msg": "command_progress", "traceId": "t1", "spanId": "s2"}),
        );
        forward(
            json!({"event": "trace", "msg": "something_else", "traceId": "t1", "spanId": "s2"}),
        );
        forward(json!({"event": "trace", "msg": "span_end", "name": "no.ids"}));
        forward(json!({"event": "trace", "msg": "command_progress"}));
        forward(json!("not an object"));
    });
    assert_eq!(
        entries,
        vec![
            object(
                json!({"name": "kernel.cell", "traceId": "t1", "spanId": "s1", "parentSpanId": "p1", "durationMs": "<ms>", "status": "error", "attrs": {"error": "ValueError: x"}, "error": "ValueError: x", "level": "warn", "component": "trace", "msg": "span_end"})
            ),
            object(
                json!({"name": "bash.command", "traceId": "t1", "spanId": "s2", "attrs": {}, "level": "info", "component": "trace", "msg": "span_start"})
            ),
            object(
                json!({"traceId": "t1", "spanId": "s2", "seconds": 30, "level": "warn", "component": "trace", "msg": "command_no_output"})
            ),
            object(
                json!({"traceId": "t1", "spanId": "s2", "level": "info", "component": "trace", "msg": "command_progress"})
            ),
        ]
    );
}

#[test]
fn the_context_source_answers_the_current_span_or_the_inbound_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut seen = Vec::new();
    let entries = record(dir.path(), TraceContext::parse(INBOUND), || {
        seen.push(current_context());
        tracing::info_span!("kernel.execute").in_scope(|| seen.push(current_context()));
    });
    let (trace, span) = ids(&entries[0]);
    assert_eq!(
        seen.iter()
            .map(|context| context.map(|context| context.to_string()))
            .collect::<Vec<_>>(),
        vec![
            Some(INBOUND.to_string()),
            Some(format!("00-{trace}-{span}-01"))
        ]
    );
}

#[test]
fn spans_and_events_outside_the_workspace_are_not_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = record(dir.path(), None, || {
        tracing::info_span!(target: "hyper::client", "connect").in_scope(|| {
            tracing::info!(target: "hyper::client", "dependency event");
        });
    });
    assert_eq!(entries, Vec::<Map<String, Value>>::new());
    assert!(
        !dir.path().join("agent.jsonl").exists(),
        "no record, no file"
    );
}

#[test]
fn a_span_attributes_event_annotates_its_span_and_logs_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = record(dir.path(), None, || {
        // Outside every span: nothing to annotate, nothing written.
        tracing::event!(
            target: pa_types::trace_context::SPAN_ATTRIBUTES_TARGET,
            tracing::Level::INFO,
            stray = 1
        );
        let span = tracing::info_span!("tool.execute", tool.name = "ipython");
        span.in_scope(|| {
            tracing::event!(
                target: pa_types::trace_context::SPAN_ATTRIBUTES_TARGET,
                tracing::Level::INFO,
                failure.fingerprint = "0123456789abcdef"
            );
        });
    });
    let (trace, span) = ids(&entries[0]);
    assert_eq!(
        entries,
        vec![object(
            json!({"traceId": trace, "spanId": span, "name": "tool.execute", "durationMs": "<ms>", "status": "ok", "attrs": {"tool.name": "ipython", "failure.fingerprint": "0123456789abcdef"}, "level": "info", "component": "trace", "msg": "span_end"})
        )]
    );
}

/// A field named `<key>.json` carries JSON text: the record holds the
/// parsed value under `<key>` (how a feature writes the array or object a
/// TS log record held); text that is not JSON stays under the full name.
#[test]
fn a_json_field_records_its_parsed_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entries = record(dir.path(), None, || {
        tracing::info!(
            target: "pa_trace_test",
            proposalId = "p1",
            addressed.json = r#"["a","b"]"#,
            broken.json = "[not json",
            "refinement.committed"
        );
    });
    assert_eq!(
        entries,
        vec![object(json!({
            "proposalId": "p1",
            "addressed": ["a", "b"],
            "broken.json": "[not json",
            "level": "info",
            "component": "pa_trace_test",
            "msg": "refinement.committed"
        }))]
    );
}
