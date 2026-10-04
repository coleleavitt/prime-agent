//! Ported from the TS `test/trace-command.test.ts`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;

const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const OTHER_TRACE: &str = "1bf7651916cd43dd8448eb211c80319d";
const TURN: &str = "b7ad6b7169203331";
const LLM: &str = "c8be7c8270314442";
const TOOL: &str = "d9cf8d9381425553";

/// agent.turn (ends last) with two children: llm.request, then tool.execute.
/// A stray info line sits under llm.request, one line is bound to the trace
/// but to no span, and an unrelated trace must be filtered out entirely.
fn fixture() -> Vec<String> {
    let rows: Vec<Value> = vec![
        json!({"ts": "2026-09-07T10:00:00.100Z", "level": "info", "component": "session", "msg": "turn started", "traceId": TRACE, "spanId": TURN, "sessionId": "abc"}),
        json!({"ts": "2026-09-07T10:00:00.200Z", "level": "info", "component": "ai.provider", "msg": "request", "traceId": TRACE, "spanId": LLM, "parentSpanId": TURN, "baseUrl": "https://api.example.test/v1"}),
        json!({"ts": "2026-09-07T10:00:00.250Z", "level": "warn", "component": "other", "msg": "unrelated", "traceId": OTHER_TRACE, "spanId": "eeeeeeeeeeeeeeee"}),
        Value::String("this line is not json".to_string()),
        json!({"ts": "2026-09-07T10:00:00.900Z", "level": "warn", "component": "trace", "msg": "span_end", "name": "llm.request", "traceId": TRACE, "spanId": LLM, "parentSpanId": TURN, "durationMs": 750, "status": "error", "attrs": {"llm.provider": "openai", "llm.base_url": "https://api.example.test/v1"}, "error": "401 archived"}),
        json!({"ts": "2026-09-07T10:00:01.000Z", "level": "info", "component": "trace", "msg": "span_end", "name": "tool.execute", "traceId": TRACE, "spanId": TOOL, "parentSpanId": TURN, "durationMs": 50, "status": "ok", "attrs": {"tool.name": "bash", "tool.call_id": "call_1"}}),
        json!({"ts": "2026-09-07T10:00:01.100Z", "level": "info", "component": "trace", "msg": "span_end", "name": "agent.turn", "traceId": TRACE, "spanId": TURN, "durationMs": 1050, "status": "ok", "attrs": {"session.id": "abc", "turn.index": 1}}),
        json!({"ts": "2026-09-07T10:00:01.200Z", "level": "debug", "component": "daemon", "msg": "context only", "traceId": TRACE}),
        json!({"ts": "2026-09-07T10:00:01.300Z", "level": "info", "component": "trace", "msg": "span_end", "name": "other.span", "traceId": OTHER_TRACE, "spanId": "ffffffffffffffff", "durationMs": 1, "status": "ok", "attrs": {}}),
    ];
    rows.into_iter()
        .map(|row| match row {
            Value::String(text) => text,
            other => other.to_string(),
        })
        .collect()
}

fn write_log(dir: &Path, lines: &[String]) -> PathBuf {
    let path = dir.join("agent.jsonl");
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write fixture");
    path
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn run(values: &[&str]) -> CommandOutcome {
    run_trace_command(&args(values), Path::new("/nonexistent/default/agent.jsonl"))
}

#[test]
fn accepts_a_bare_trace_id_upper_case_hex_and_a_full_traceparent() {
    let accepted: Vec<Result<String, String>> = [
        TRACE.to_string(),
        TRACE.to_ascii_uppercase(),
        format!("00-{TRACE}-{TURN}-01"),
    ]
    .iter()
    .map(|value| normalize_trace_id(value))
    .collect();
    assert_eq!(accepted, vec![Ok(TRACE.to_string()); 3]);
}

#[test]
fn rejects_malformed_ids() {
    for value in [
        "nope".to_string(),
        "0".repeat(32),
        format!("01-{TRACE}-{TURN}-01"),
    ] {
        assert!(normalize_trace_id(&value).is_err(), "{value}");
    }
}

#[test]
fn parses_log_and_json_in_any_position() {
    assert_eq!(
        parse_trace_command_args(&args(&["--json", TRACE, "--log", "/tmp/x.jsonl"])),
        Ok(TraceCommandOptions {
            trace_id: TRACE.to_string(),
            log_path: Some(PathBuf::from("/tmp/x.jsonl")),
            json: true,
        })
    );
    assert_eq!(
        parse_trace_command_args(&args(&[TRACE, "--log=/tmp/y.jsonl"])),
        Ok(TraceCommandOptions {
            trace_id: TRACE.to_string(),
            log_path: Some(PathBuf::from("/tmp/y.jsonl")),
            json: false,
        })
    );
    let errors: Vec<Result<TraceCommandOptions, String>> = [
        args(&[]),
        args(&[TRACE, "--log"]),
        args(&[TRACE, "--bogus"]),
        args(&[TRACE, OTHER_TRACE]),
    ]
    .iter()
    .map(|values| parse_trace_command_args(values))
    .collect();
    assert_eq!(
        errors,
        vec![
            Err("Missing trace id.".to_string()),
            Err("--log requires a path.".to_string()),
            Err("Unknown option for trace: --bogus".to_string()),
            Err("trace accepts exactly one trace id or traceparent.".to_string()),
        ]
    );
}

#[test]
fn renders_nested_spans_attributed_log_lines_and_orphans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_log(dir.path(), &fixture());
    let path_text = path.display().to_string();
    let outcome = run(&[TRACE, "--log", &path_text]);
    let expected = [
        format!("trace {TRACE}  (3 spans, 3 log lines, {path_text})"),
        format!("├─ agent.turn  1050ms  ok  session.id=abc turn.index=1  [{TURN}]"),
        "│  ├─ 10:00:00.100  info   session  turn started  sessionId=abc".to_string(),
        format!("│  ├─ llm.request  750ms  error  llm.provider=openai llm.base_url=https://api.example.test/v1  error=401 archived  [{LLM}]"),
        "│  │  └─ 10:00:00.200  info   ai.provider  request  baseUrl=https://api.example.test/v1".to_string(),
        format!("│  └─ tool.execute  50ms  ok  tool.name=bash tool.call_id=call_1  [{TOOL}]"),
        "└─ (no span)".to_string(),
        "   └─ 10:00:01.200  debug  daemon  context only".to_string(),
    ]
    .join("\n");
    assert_eq!(outcome, CommandOutcome::success(vec![expected]));
}

#[test]
fn accepts_a_traceparent_on_the_command_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_log(dir.path(), &fixture());
    let outcome = run(&[
        &format!("00-{TRACE}-{TURN}-01"),
        "--log",
        &path.display().to_string(),
    ]);
    assert_eq!(outcome.code, 0);
    assert!(outcome.stdout[0].starts_with(&format!("trace {TRACE}")));
    assert!(outcome.stdout[0].contains("agent.turn"));
}

#[test]
fn keeps_a_running_parent_visible_as_an_open_span_placeholder() {
    // The turn has not ended yet: only its children and its log line are on disk.
    let dir = tempfile::tempdir().expect("tempdir");
    let lines: Vec<String> = fixture()
        .into_iter()
        .filter(|raw| !raw.contains(r#""name":"agent.turn""#))
        .collect();
    let path = write_log(dir.path(), &lines);
    let tree = build_trace_tree(TRACE, &read_trace_log_lines(&[path], TRACE).expect("read"));
    assert_eq!(tree.span_count, 2);
    let roots: Vec<(&str, &str)> = tree
        .roots
        .iter()
        .map(|root| {
            (
                tree.nodes[*root].name.as_str(),
                tree.nodes[*root].span_id.as_str(),
            )
        })
        .collect();
    assert_eq!(roots, vec![("(open span)", TURN)]);
    let root = &tree.nodes[tree.roots[0]];
    let children: Vec<&str> = root
        .children
        .iter()
        .map(|child| tree.nodes[*child].name.as_str())
        .collect();
    assert_eq!(children, vec!["llm.request", "tool.execute"]);
    let logs: Vec<&str> = root
        .logs
        .iter()
        .map(|line| line.entry["msg"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(logs, vec!["turn started"]);
}

#[test]
fn uses_span_start_metadata_for_a_silent_active_operation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let row = json!({"ts": "2026-09-08T11:00:00.000Z", "level": "info", "component": "trace", "msg": "span_start", "name": "agent.prompt", "traceId": TRACE, "spanId": TURN, "attrs": {"session.id": "active"}});
    let path = write_log(dir.path(), &[row.to_string()]);
    let tree = build_trace_tree(TRACE, &read_trace_log_lines(&[path], TRACE).expect("read"));
    assert_eq!((tree.span_count, tree.log_count), (0, 0));
    let roots: Vec<(&str, &str)> = tree
        .roots
        .iter()
        .map(|root| {
            (
                tree.nodes[*root].name.as_str(),
                tree.nodes[*root].span_id.as_str(),
            )
        })
        .collect();
    assert_eq!(roots, vec![("(open) agent.prompt", TURN)]);
}

#[test]
fn reads_the_rotated_old_sibling_before_the_live_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lines = fixture();
    let path = write_log(dir.path(), &lines[4..]);
    let old = dir.path().join("agent.jsonl.old");
    std::fs::write(&old, format!("{}\n", lines[..4].join("\n"))).expect("write old");
    assert_eq!(retained_log_files(&path), vec![old.clone(), path.clone()]);
    let outcome = run(&[TRACE, "--log", &path.display().to_string()]);
    assert_eq!(outcome.code, 0);
    assert!(outcome.stdout[0].contains(&format!(
        "3 spans, 3 log lines, {}, {}",
        old.display(),
        path.display()
    )));
    assert!(outcome.stdout[0].contains("turn started"));
}

#[test]
fn reads_gzip_compressed_retained_generations_oldest_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lines = fixture();
    let path = write_log(dir.path(), &lines[4..]);
    let gz = dir.path().join("agent.jsonl.old.1.gz");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(format!("{}\n", lines[..4].join("\n")).as_bytes())
        .expect("gzip");
    std::fs::write(&gz, encoder.finish().expect("gzip")).expect("write gz");
    assert_eq!(retained_log_files(&path), vec![gz.clone(), path.clone()]);
    let outcome = run(&[TRACE, "--log", &path.display().to_string()]);
    assert_eq!(outcome.code, 0);
    assert!(outcome.stdout[0].contains(&format!("{}, {}", gz.display(), path.display())));
    assert!(outcome.stdout[0].contains("turn started"));
}

#[test]
fn json_prints_the_raw_matching_lines_in_file_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lines = fixture();
    let path = write_log(dir.path(), &lines);
    let outcome = run(&[TRACE, "--json", "--log", &path.display().to_string()]);
    let expected: Vec<String> = lines
        .into_iter()
        .filter(|raw| raw.contains(TRACE))
        .collect();
    assert_eq!(expected.len(), 6);
    assert_eq!(outcome, CommandOutcome::success(expected));
}

#[test]
fn exits_1_with_a_clear_message_when_the_trace_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_log(dir.path(), &fixture());
    let missing = "abcdefabcdefabcdefabcdefabcdefab";
    let outcome = run(&[missing, "--log", &path.display().to_string()]);
    assert_eq!(
        outcome,
        CommandOutcome::failure(vec![format!(
            "Error: no entries for trace {missing} in {}",
            path.display()
        )])
    );
}

#[test]
fn exits_1_when_the_log_file_does_not_exist() {
    assert_eq!(
        run(&[TRACE, "--log", "/nonexistent/agent.jsonl"]),
        CommandOutcome::failure(vec![
            "Error: no log file at /nonexistent/agent.jsonl".to_string()
        ])
    );
}

#[test]
fn exits_1_with_usage_on_bad_arguments() {
    assert_eq!(
        run(&["not-a-trace"]),
        CommandOutcome::failure(vec![
            r#"Error: Not a trace id or traceparent: "not-a-trace" (expected 32 hex chars or 00-<traceId>-<spanId>-<flags>)."#.to_string(),
            "Usage: prime-agent trace <traceId|traceparent> [--log <path>] [--json]".to_string(),
        ])
    );
}

#[test]
fn long_and_control_character_values_are_capped_and_neutralized() {
    let dir = tempfile::tempdir().expect("tempdir");
    let row = json!({"ts": "2026-09-07T10:00:00.100Z", "level": "info", "component": "x", "msg": "m\u{1b}[2J", "traceId": TRACE, "detail": "a".repeat(200)});
    let path = write_log(dir.path(), &[row.to_string()]);
    let outcome = run(&[TRACE, "--log", &path.display().to_string()]);
    let expected_detail = format!("{}…", "a".repeat(119));
    assert_eq!(
        outcome.stdout[0].lines().last(),
        Some(format!("   └─ 10:00:00.100  info   x  m?[2J  detail={expected_detail}").as_str())
    );
}
