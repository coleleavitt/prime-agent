//! Ported from the TS `test/health-command.test.ts`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;

/// 2026-09-08T12:00:00.000Z
const NOW: i64 = 1_788_868_800_000;
const TRACE_PROVIDER: &str = "0af7651916cd43dd8448eb211c80319c";
const TRACE_STUCK: &str = "1bf7651916cd43dd8448eb211c80319d";

fn fixture() -> Vec<String> {
    let mut rows: Vec<String> = [
        json!({"ts": "2026-09-07T10:00:00.000Z", "component": "ai.provider", "msg": "provider stream failure", "provider": "old", "message": "outside window"}),
        json!({"ts": "2026-09-08T10:00:00.000Z", "component": "trace", "msg": "span_end", "name": "historian.validate", "status": "error", "error": "invalid summary", "traceId": "2cf7651916cd43dd8448eb211c80319e", "attrs": {"historian.session_id": "hist"}}),
        json!({"ts": "2026-09-08T10:10:00.000Z", "component": "ai.provider", "msg": "provider stream failure", "provider": "openai", "message": "rate limited", "traceId": TRACE_PROVIDER, "spanId": "provider-span", "sessionId": "session-1"}),
        json!({"ts": "2026-09-08T10:10:00.001Z", "component": "trace", "msg": "span_end", "name": "llm.request", "status": "error", "error": "rate limited", "traceId": TRACE_PROVIDER, "spanId": "provider-span", "attrs": {"llm.provider": "openai"}}),
        json!({"ts": "2026-09-08T10:59:00.000Z", "component": "trace", "msg": "span_start", "name": "agent.prompt", "spanId": "open-turn", "traceId": TRACE_STUCK, "attrs": {"session.id": "session-stuck"}}),
        json!({"ts": "2026-09-08T11:00:00.000Z", "component": "trace", "msg": "span_end", "name": "llm.request", "status": "ok", "spanId": "child", "parentSpanId": "open-turn", "traceId": TRACE_STUCK, "attrs": {"llm.provider": "anthropic"}}),
        json!({"ts": "2026-09-08T11:50:00.000Z", "component": "coding-agent.daemon-supervisor", "msg": "Worker worker-1 recovery failed: socket closed"}),
        json!({"ts": "2026-09-08T11:55:00.000Z", "component": "coding-agent.daemon-supervisor", "msg": "Worker worker-2 recovered successfully"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect();
    rows.push("not json".to_string());
    rows
}

struct LogDir {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn write_log(lines: &[String]) -> LogDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("agent.jsonl");
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write log");
    LogDir { _dir: dir, path }
}

fn rows(values: &[Value]) -> Vec<String> {
    values.iter().map(Value::to_string).collect()
}

fn run(values: &[&str]) -> CommandOutcome {
    let args: Vec<String> = values.iter().map(|value| (*value).to_string()).collect();
    run_health_command(&args, Path::new("/missing/default/agent.jsonl"), NOW)
}

fn run_json(path: &Path, extra: &[&str]) -> (i32, Value) {
    let path = path.display().to_string();
    let mut values = vec!["--log", path.as_str(), "--json"];
    values.extend_from_slice(extra);
    let outcome = run(&values);
    assert_eq!(outcome.stderr, Vec::<String>::new());
    let summary = serde_json::from_str(&outcome.stdout[0]).expect("summary JSON");
    (outcome.code, summary)
}

fn count(summary: &Value, category: &str) -> u64 {
    summary["counts"][category].as_u64().expect("count")
}

/// `--limit` is JS `Number(text)`: hex, JS white space (U+FEFF) and an
/// exponent all name an integer; `inf` is `NaN`, not infinity.
#[test]
fn parses_the_limit_as_js_number() {
    let limit =
        |value: &str| parse_health_args(&[format!("--limit={value}")]).map(|options| options.limit);
    let range = Err("--limit must be an integer from 1 to 200.".to_string());
    assert_eq!(
        ["0x32", "\u{feff}50", "5e1", "inf", ""].map(limit),
        [Ok(50), Ok(50), Ok(50), range.clone(), range]
    );
}

#[test]
fn parses_bounded_duration_and_output_options() {
    let args =
        |values: &[&str]| -> Vec<String> { values.iter().map(|v| (*v).to_string()).collect() };
    assert_eq!(
        parse_health_args(&args(&[
            "--since=6h",
            "--stuck-after",
            "30m",
            "--limit",
            "50",
            "--json"
        ])),
        Ok(HealthOptions {
            log_path: None,
            since_ms: 21_600_000,
            stuck_after_ms: 1_800_000,
            limit: 50,
            json: true,
        })
    );
    let errors: Vec<Result<HealthOptions, String>> = [
        args(&["--since", "soon"]),
        args(&["--limit", "201"]),
        args(&["extra"]),
        args(&["--log="]),
        args(&["--since", "0m"]),
    ]
    .iter()
    .map(|values| parse_health_args(values))
    .collect();
    assert_eq!(
        errors,
        vec![
            Err("--since requires a duration such as 30m, 6h, or 2d.".to_string()),
            Err("--limit must be an integer from 1 to 200.".to_string()),
            Err("Unknown option for health: extra".to_string()),
            Err("--log requires a path.".to_string()),
            Err("--since must be positive.".to_string()),
        ]
    );
}

#[test]
fn summarizes_the_incident_classes_and_deduplicates_provider_span_log_pairs() {
    let log = write_log(&fixture());
    let outcome = run(&["--log", &log.path.display().to_string()]);
    let path = log.path.display();
    let expected = [
        format!("health since 2026-09-07T12:00:00.000Z  (5 incidents; {path})"),
        "Historian failures: 1".to_string(),
        "  2026-09-08T10:00:00.000Z  historian.validate: invalid summary  session=hist trace=2cf7651916cd43dd8448eb211c80319e".to_string(),
        "Provider errors: 1".to_string(),
        format!("  2026-09-08T10:10:00.000Z  openai: rate limited  session=session-1 trace={TRACE_PROVIDER}"),
        "Stuck turns: 1".to_string(),
        format!("  2026-09-08T10:59:00.000Z  agent.prompt span open-turn has no completion after 1h  session=session-stuck trace={TRACE_STUCK}"),
        "Daemon recovery failures: 1".to_string(),
        "  2026-09-08T11:50:00.000Z  Worker worker-1 recovery failed: socket closed".to_string(),
        "Process failures: 0".to_string(),
        "Kernel failures: 0".to_string(),
        "Child failures: 0".to_string(),
        "Agent message delivery failures: 0".to_string(),
        "Lock failures: 0".to_string(),
        "Orphan cleanup failures: 0".to_string(),
        "Diagnostic uncertainty: 1".to_string(),
        "UNKNOWN: 1 malformed log line(s) could not be evaluated.".to_string(),
    ]
    .join("\n");
    assert_eq!(
        outcome,
        CommandOutcome {
            code: 2,
            stdout: vec![expected],
            stderr: Vec::new(),
        }
    );
}

#[test]
fn prints_stable_json_and_bounds_incident_details_without_losing_counts() {
    let log = write_log(&fixture());
    let (code, summary) = run_json(&log.path, &["--limit", "2"]);
    assert_eq!(code, 2);
    assert_eq!(
        summary,
        json!({
            "status": "unhealthy",
            "generatedAt": "2026-09-08T12:00:00.000Z",
            "since": "2026-09-07T12:00:00.000Z",
            "files": [log.path.display().to_string()],
            "counts": {"historian": 1, "provider": 1, "stuck_turn": 1, "daemon_recovery": 1, "process": 0, "kernel": 0, "child": 0, "message_delivery": 0, "lock": 0, "orphan": 0, "diagnostic": 1},
            "toolErrors": {"historian": 0, "provider": 0, "stuck_turn": 0, "daemon_recovery": 0, "process": 0, "kernel": 0, "child": 0, "message_delivery": 0, "lock": 0, "orphan": 0, "diagnostic": 0},
            "incidents": [
                {"category": "daemon_recovery", "ts": "2026-09-08T11:50:00.000Z", "summary": "Worker worker-1 recovery failed: socket closed"},
                {"category": "stuck_turn", "ts": "2026-09-08T10:59:00.000Z", "summary": "agent.prompt span open-turn has no completion after 1h", "traceId": TRACE_STUCK, "sessionId": "session-stuck"},
            ],
            "truncated": true,
            "parseErrors": 1,
            "stale": false,
            "latestEntryAt": "2026-09-08T11:55:00.000Z",
        })
    );
}

#[test]
fn flags_an_active_operation_span_start_with_no_matching_end() {
    let mut lines = fixture();
    lines.push(json!({"ts": "2026-09-08T11:00:00.000Z", "component": "trace", "msg": "span_start", "name": "agent.prompt", "spanId": "silent-open", "traceId": TRACE_STUCK, "attrs": {"session.id": "session-open"}}).to_string());
    let log = write_log(&lines);
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(count(&summary, "stuck_turn"), 2);
    assert!(summary["incidents"]
        .as_array()
        .expect("incidents")
        .iter()
        .any(|item| item["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("agent.prompt span silent-open")));
}

#[test]
fn detects_a_still_open_turn_that_started_before_the_window() {
    let mut lines = fixture();
    lines.push(json!({"ts": "2026-09-06T11:00:00.000Z", "component": "trace", "msg": "span_start", "name": "client.turn", "spanId": "old-open", "traceId": TRACE_STUCK}).to_string());
    let log = write_log(&lines);
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(count(&summary, "stuck_turn"), 2);
}

#[test]
fn does_not_flag_a_turn_whose_span_ended() {
    let mut lines = fixture();
    lines.push(json!({"ts": "2026-09-08T11:01:00.000Z", "component": "trace", "msg": "span_end", "name": "agent.turn", "status": "ok", "spanId": "open-turn", "traceId": TRACE_STUCK}).to_string());
    let log = write_log(&lines);
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(count(&summary, "stuck_turn"), 0);
}

#[test]
fn reads_gzip_compressed_retained_generations() {
    let log = write_log(&fixture());
    let gz = PathBuf::from(format!("{}.old.1.gz", log.path.display()));
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let row = json!({"ts": "2026-09-08T09:00:00.000Z", "component": "ai.provider", "msg": "provider stream failure", "provider": "retained", "message": "failed"});
    encoder
        .write_all(format!("{row}\n").as_bytes())
        .expect("gzip");
    std::fs::write(&gz, encoder.finish().expect("gzip")).expect("write gz");
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(count(&summary, "provider"), 2);
    assert_eq!(summary["files"][0], Value::from(gz.display().to_string()));
}

#[test]
fn expands_process_kernel_child_lock_and_orphan_evidence_and_fails_closed() {
    let log = write_log(&rows(&[
        json!({"ts": "2026-09-08T10:00:00.000Z", "component": "trace", "msg": "span_start", "name": "bash.command", "traceId": "p", "spanId": "p1", "attrs": {"bash.command": "sleep 99"}}),
        json!({"ts": "2026-09-08T10:01:00.000Z", "component": "trace", "msg": "span_start", "name": "kernel.cell", "traceId": "k", "spanId": "k1", "attrs": {"kernel.cell": "print(1)"}}),
        json!({"ts": "2026-09-08T10:02:00.000Z", "component": "trace", "msg": "span_end", "name": "rlm.child", "status": "error", "error": "child timed out", "traceId": "c", "spanId": "c1"}),
        json!({"ts": "2026-09-08T10:03:00.000Z", "component": "trace", "msg": "span_end", "name": "cargo_lock_wait", "status": "error", "error": "lock timed out", "traceId": "l", "spanId": "l1"}),
        json!({"ts": "2026-09-08T10:04:00.000Z", "component": "orphan-process", "msg": "Could not reap orphaned worker resources"}),
        json!({"ts": "2026-09-08T10:05:00.000Z", "component": "kernel", "msg": "kernel_exit", "message": "unexpected exit"}),
    ]));
    let (code, summary) = run_json(&log.path, &[]);
    assert_eq!(
        (code, summary["status"].clone()),
        (2, Value::from("unhealthy"))
    );
    let counts: Vec<u64> = ["process", "kernel", "child", "lock", "orphan"]
        .iter()
        .map(|category| count(&summary, category))
        .collect();
    assert_eq!(counts, vec![1, 2, 1, 1, 1]);
}

#[test]
fn counts_an_unfinished_child_run_and_a_delivered_completion_notice_as_child_incidents() {
    let log = write_log(&rows(&[
        json!({"ts": "2026-09-08T10:00:00.000Z", "component": "trace", "msg": "span_start", "name": "rlm.child.run", "traceId": "3df7651916cd43dd8448eb211c80319f", "spanId": "run1", "attrs": {"rlm.child_id": "child-hung", "rlm.depth": 1}}),
        json!({"ts": "2026-09-08T11:30:00.000Z", "component": "coding-agent.rlm-child", "msg": "rlm_child_terminal_notice_delivered", "kind": "completed_without_reply", "rlm.child_id": "child-silent", "sessionId": "child-session-1", "traceId": "4ef7651916cd43dd8448eb211c803190", "spanId": "run2"}),
        json!({"ts": "2026-09-08T11:31:00.000Z", "component": "coding-agent.rlm-child", "msg": "rlm_child_terminal_notice_delivered", "kind": "cancelled", "rlm.child_id": "child-cancelled", "sessionId": "child-session-2"}),
    ]));
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(count(&summary, "child"), 2);
    let incidents = summary["incidents"].as_array().expect("incidents");
    assert!(incidents.iter().any(|item| item["summary"]
        .as_str()
        .unwrap_or_default()
        .contains("rlm.child.run span run1")));
    assert!(incidents.contains(&json!({
        "category": "child",
        "ts": "2026-09-08T11:30:00.000Z",
        "summary": "rlm child child-silent: completed_without_reply notice delivered to parent",
        "traceId": "4ef7651916cd43dd8448eb211c803190",
        "sessionId": "child-session-1",
    })));
    assert!(!incidents.iter().any(|item| item["summary"]
        .as_str()
        .unwrap_or_default()
        .contains("child-cancelled")));
}

#[test]
fn separates_agent_visible_tool_errors_from_incidents_and_reports_rejected_agent_messages() {
    let log = write_log(&rows(&[
        json!({"ts": "2026-09-08T11:00:00.000Z", "component": "trace", "msg": "span_end", "name": "bash.command", "status": "error", "traceId": "5ff7651916cd43dd8448eb211c803191", "attrs": {"bash.command": "rg missing", "bash.exit_code": 1, "error": "exit code 1"}}),
        json!({"ts": "2026-09-08T11:01:00.000Z", "component": "trace", "msg": "span_end", "name": "bash.command", "status": "error", "traceId": "5ff7651916cd43dd8448eb211c803192", "attrs": {"bash.command": "missing-binary", "error": "spawn ENOENT"}}),
        json!({"ts": "2026-09-08T11:02:00.000Z", "component": "trace", "msg": "span_end", "name": "kernel.cell", "status": "error", "attrs": {"error": "interrupted"}}),
        json!({"ts": "2026-09-08T11:03:00.000Z", "component": "trace", "msg": "span_end", "name": "kernel.cell", "status": "error", "attrs": {"error": "TypeError: 'str' object is not callable"}}),
        json!({"ts": "2026-09-08T11:04:00.000Z", "component": "trace", "msg": "span_end", "name": "kernel.cell", "status": "error", "traceId": "5ff7651916cd43dd8448eb211c803193", "attrs": {"error": "write failed"}}),
        json!({"ts": "2026-09-08T11:05:00.000Z", "component": "trace", "msg": "span_end", "name": "kernel.host_request", "status": "error", "error": "Target session has too many pending messages: 24 unfinished, limit is 20", "sessionId": "child-session-9", "traceId": "5ff7651916cd43dd8448eb211c803194", "attrs": {"host_request.type": "agent_message.send"}}),
        json!({"ts": "2026-09-08T11:06:00.000Z", "component": "trace", "msg": "span_end", "name": "kernel.host_request", "status": "error", "error": "agent_observe max_chars must be between 80 and 2000", "attrs": {"host_request.type": "agent_observe.recent"}}),
    ]));
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(
        (
            summary["toolErrors"]["process"].clone(),
            summary["toolErrors"]["kernel"].clone(),
        ),
        (Value::from(1), Value::from(3))
    );
    let counts: Vec<u64> = ["process", "kernel", "message_delivery"]
        .iter()
        .map(|category| count(&summary, category))
        .collect();
    assert_eq!(counts, vec![1, 1, 1]);
    let incidents = summary["incidents"].as_array().expect("incidents");
    let summary_of = |category: &str| {
        incidents
            .iter()
            .find(|item| item["category"] == category)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(
        summary_of("process")["summary"],
        "bash.command: spawn ENOENT"
    );
    assert_eq!(summary_of("kernel")["summary"], "kernel.cell: write failed");
    assert_eq!(
        summary_of("message_delivery"),
        json!({
            "category": "message_delivery",
            "ts": "2026-09-08T11:05:00.000Z",
            "summary": "agent_message.send: Target session has too many pending messages: 24 unfinished, limit is 20",
            "traceId": "5ff7651916cd43dd8448eb211c803194",
            "sessionId": "child-session-9",
        })
    );
    let text = run(&["--log", &log.path.display().to_string()]).stdout[0].clone();
    assert!(text.contains("Agent message delivery failures: 1"));
    assert!(text.contains("Agent-visible tool errors (not incidents): process=1 kernel=3"));
}

#[test]
fn returns_unknown_for_malformed_empty_or_stale_evidence() {
    let log = write_log(&["not json".to_string()]);
    let (code, summary) = run_json(&log.path, &[]);
    assert_eq!(
        (
            code,
            summary["status"].clone(),
            summary["parseErrors"].clone(),
            summary["stale"].clone()
        ),
        (2, Value::from("unknown"), Value::from(1), Value::from(true))
    );
    let log = write_log(&rows(&[
        json!({"ts": "2026-09-01T00:00:00.000Z", "component": "session", "msg": "ok"}),
    ]));
    let (code, summary) = run_json(&log.path, &[]);
    assert_eq!(
        (
            code,
            summary["status"].clone(),
            summary["parseErrors"].clone(),
            summary["stale"].clone()
        ),
        (2, Value::from("unknown"), Value::from(0), Value::from(true))
    );
}

#[test]
fn returns_healthy_only_with_valid_recent_evidence_and_no_incidents() {
    let log = write_log(&rows(&[
        json!({"ts": "2026-09-08T11:59:00.000Z", "component": "session", "msg": "heartbeat"}),
    ]));
    let (code, summary) = run_json(&log.path, &[]);
    assert_eq!(
        (
            code,
            summary["status"].clone(),
            summary["parseErrors"].clone(),
            summary["stale"].clone()
        ),
        (
            0,
            Value::from("healthy"),
            Value::from(0),
            Value::from(false)
        )
    );
}

#[test]
fn keeps_unmatched_starts_visible_after_the_bounded_entry_buffer_evicts_them() {
    let mut lines = vec![json!({"ts": "2026-09-08T10:00:00.000Z", "component": "trace", "msg": "span_start", "name": "bash.command", "traceId": "large", "spanId": "open"}).to_string()];
    for index in 0..100_001 {
        lines.push(json!({"ts": "2026-09-08T10:01:00.000Z", "component": "noise", "msg": format!("line {index}")}).to_string());
    }
    let log = write_log(&lines);
    let (_, summary) = run_json(&log.path, &[]);
    assert_eq!(count(&summary, "process"), 1);
}

#[test]
fn reports_missing_logs_and_usage_errors() {
    assert_eq!(
        run(&["--log", "/missing/agent.jsonl"]),
        CommandOutcome::failure(vec![
            "Error: no log file at /missing/agent.jsonl".to_string()
        ])
    );
    assert_eq!(
        run(&["--limit", "0"]),
        CommandOutcome::failure(vec![
            "Error: --limit must be an integer from 1 to 200.".to_string(),
            USAGE.to_string(),
        ])
    );
}
