//! The session feature end to end, driven through the `SessionFeature`
//! hooks the engine calls: turn-boundary observation into the local and
//! global harness states, the observer seam RAVO plugs into, the
//! `failure.fingerprint` span annotation, and the `ipython` resolution hint.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use pa_agent::types::AgentMessage;
use pa_core::features::{SessionFeature, SessionFeatureContext, ToolResultObservation};
use pa_ledger::{
    fingerprint_failure, FailureKind, FailureLedger, FailureLedgerFeature, FailureRecord,
    HarnessDocument, LedgerBoundary, LedgerFlush, LedgerObserver, LedgerOptions, LedgerScope,
};
use serde_json::{json, Value};

const AT: &str = "2026-01-01T00:00:00.000Z";

fn context(root: &Path, session_id: &str, artifacts: bool) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: root.join("agent"),
        cwd: root.join("work"),
        session_id: session_id.to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: artifacts.then(|| root.join("artifacts").join(session_id)),
    })
}

fn message(value: Value) -> AgentMessage {
    serde_json::from_value(value).unwrap()
}

fn user() -> AgentMessage {
    message(json!({ "role": "user", "content": "go", "timestamp": 1 }))
}

fn assistant(stop_reason: &str, error: Option<&str>) -> AgentMessage {
    let mut value = json!({
        "role": "assistant", "content": [], "api": "test", "provider": "p1", "model": "m1",
        "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0 },
        "stopReason": stop_reason, "timestamp": 1
    });
    if let Some(error) = error {
        value["errorMessage"] = Value::from(error);
    }
    message(value)
}

fn tool_result(tool: &str, text: &str, is_error: bool) -> AgentMessage {
    message(json!({
        "role": "toolResult", "toolCallId": "c", "toolName": tool,
        "content": [{ "type": "text", "text": text }], "isError": is_error, "timestamp": 1
    }))
}

fn local_dir(root: &Path, session_id: &str) -> PathBuf {
    root.join("artifacts").join(session_id).join("harness")
}

fn global_dir(root: &Path) -> PathBuf {
    root.join("agent").join("harness")
}

fn record(
    kind: FailureKind,
    source: &str,
    raw: &str,
    count: u64,
    turns: (u64, u64),
    excerpt: &str,
    non_actionable: Option<u64>,
) -> FailureRecord {
    FailureRecord {
        fingerprint: fingerprint_failure(kind, Some(source), None, raw),
        count,
        first_seen_turn: turns.0,
        last_seen_turn: turns.1,
        first_seen_at: AT.to_string(),
        last_seen_at: AT.to_string(),
        excerpt: excerpt.to_string(),
        addressed_by_proposal_ids: Vec::new(),
        replay_cases: Vec::new(),
        non_actionable_count: non_actionable,
    }
}

fn ledger(records: Vec<FailureRecord>, cursor: u64) -> FailureLedger {
    FailureLedger {
        schema: 1,
        failures: records
            .into_iter()
            .map(|record| (record.fingerprint.id.clone(), record))
            .collect::<IndexMap<_, _>>(),
        last_scanned_entry_index: cursor,
    }
}

/// Feed `messages` through the message hook and wait for the worker.
fn run(
    feature: &FailureLedgerFeature,
    context: &Arc<SessionFeatureContext>,
    messages: &[AgentMessage],
) {
    for message in messages {
        feature.on_message_end(context, message);
    }
    feature.on_agent_end(context);
    assert!(feature.handle().wait_idle(Duration::from_secs(30)));
}

fn one_failing_turn() -> Vec<AgentMessage> {
    vec![
        user(),
        assistant("toolUse", None),
        tool_result("bash", "boom: exit 1", true),
        assistant("stop", None),
    ]
}

/// One boundary as the observer saw it: turn, newly recurring ids,
/// recurred ids, global ordinal.
type SeenBoundary = (u64, Vec<String>, Vec<String>, Option<u64>);

#[derive(Default)]
struct Recorder {
    boundaries: Mutex<Vec<SeenBoundary>>,
    flushes: Mutex<Vec<(LedgerScope, bool)>>,
    hold: std::sync::atomic::AtomicBool,
}

impl LedgerObserver for Recorder {
    fn on_boundary(&self, _context: &Arc<SessionFeatureContext>, boundary: &LedgerBoundary<'_>) {
        self.boundaries.lock().unwrap().push((
            boundary.turn,
            boundary
                .newly_recurring
                .iter()
                .map(|record| record.fingerprint.id.clone())
                .collect(),
            boundary.recurred_ids.to_vec(),
            boundary.global_ordinal,
        ));
    }

    fn hold_flush(&self, _session_id: &str) -> bool {
        self.hold.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn on_flush(&self, flush: &mut LedgerFlush<'_>) {
        if flush.scope == LedgerScope::Global {
            flush.document.set("ravo", json!({ "lineage": [] }));
        }
    }

    fn on_flush_result(&self, scope: LedgerScope, _session_id: &str, landed: bool) {
        self.flushes.lock().unwrap().push((scope, landed));
    }
}

#[test]
fn a_turn_boundary_counts_failures_in_the_local_and_global_ledgers() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let feature = FailureLedgerFeature::with_observers(
        LedgerOptions {
            global_ledger: Some(true),
            ..LedgerOptions::default()
        },
        vec![Arc::clone(&recorder) as Arc<dyn LedgerObserver>],
    )
    .with_clock(|| AT.to_string());

    // Session A resumes with one earlier exchange: two branch messages, one turn.
    let a = context(root.path(), "a", true);
    feature.on_session_start(&a, &[user(), assistant("stop", None)]);
    let mut messages = one_failing_turn();
    messages.push(assistant("error", Some("Request was aborted")));
    run(&feature, &a, &messages);

    let bash = record(
        FailureKind::ToolError,
        "bash",
        "boom: exit 1",
        1,
        (3, 3),
        "boom: exit 1",
        None,
    );
    let aborted = record(
        FailureKind::ProviderError,
        "p1",
        "Request was aborted",
        1,
        (4, 4),
        "Request was aborted",
        Some(1),
    );
    assert_eq!(
        HarnessDocument::load(&local_dir(root.path(), "a")).failures(),
        ledger(vec![bash.clone(), aborted.clone()], 7)
    );
    assert_eq!(
        HarnessDocument::load(&global_dir(root.path())).failures(),
        ledger(vec![bash.clone(), aborted.clone()], 0)
    );

    // Session B: the same tool failure recurs across sessions (count 2 globally).
    let b = context(root.path(), "b", true);
    feature.on_session_start(&b, &[]);
    run(&feature, &b, &one_failing_turn());
    assert_eq!(
        HarnessDocument::load(&local_dir(root.path(), "b")).failures(),
        ledger(
            vec![record(
                FailureKind::ToolError,
                "bash",
                "boom: exit 1",
                1,
                (2, 2),
                "boom: exit 1",
                None
            )],
            4
        )
    );
    let global = HarnessDocument::load(&global_dir(root.path()));
    assert_eq!(
        global.failures(),
        ledger(
            vec![
                FailureRecord {
                    count: 2,
                    last_seen_turn: 3,
                    ..bash.clone()
                },
                aborted
            ],
            0
        )
    );
    // The observer folded its own key into the same locked write.
    assert_eq!(global.get("ravo"), Some(&json!({ "lineage": [] })));
    assert_eq!(
        *recorder.boundaries.lock().unwrap(),
        vec![
            (2, Vec::new(), Vec::new(), None),
            (3, Vec::new(), vec![bash.fingerprint.id.clone()], Some(1)),
            // The aborted request is counted but never recurs.
            (4, Vec::new(), Vec::new(), Some(2)),
            (1, Vec::new(), Vec::new(), None),
            (
                2,
                vec![bash.fingerprint.id.clone()],
                vec![bash.fingerprint.id],
                Some(3)
            ),
        ]
    );
}

#[test]
fn with_the_global_ledger_off_only_the_local_ledger_is_written() {
    let root = tempfile::tempdir().unwrap();
    let feature = FailureLedgerFeature::new(LedgerOptions {
        global_ledger: Some(false),
        ..LedgerOptions::default()
    })
    .with_clock(|| AT.to_string());
    for id in ["a", "b"] {
        let session = context(root.path(), id, true);
        feature.on_session_start(&session, &[]);
        run(&feature, &session, &one_failing_turn());
        assert_eq!(
            HarnessDocument::load(&local_dir(root.path(), id))
                .failures()
                .failures
                .len(),
            1
        );
    }
    assert!(!global_dir(root.path()).exists());
}

#[test]
fn a_session_without_an_artifact_dir_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let feature = FailureLedgerFeature::new(LedgerOptions {
        global_ledger: Some(true),
        ..LedgerOptions::default()
    });
    let session = context(root.path(), "a", false);
    feature.on_session_start(&session, &[]);
    run(&feature, &session, &one_failing_turn());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn a_held_flush_waits_and_the_next_flush_writes_everything() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    recorder
        .hold
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let feature = FailureLedgerFeature::with_observers(
        LedgerOptions {
            global_ledger: Some(true),
            ..LedgerOptions::default()
        },
        vec![Arc::clone(&recorder) as Arc<dyn LedgerObserver>],
    )
    .with_clock(|| AT.to_string());
    let session = context(root.path(), "a", true);
    feature.on_session_start(&session, &[]);
    run(&feature, &session, &one_failing_turn());
    assert!(!local_dir(root.path(), "a").exists());
    assert!(!global_dir(root.path()).exists());
    // While held, the unflushed ledger is what a consumer reads.
    assert_eq!(
        feature
            .handle()
            .session_ledger("a")
            .map(|ledger| ledger.failures.len()),
        Some(1)
    );
    assert_eq!(
        feature
            .handle()
            .fresh_global_ledger(&root.path().join("agent"), Some("a"))
            .failures
            .len(),
        1
    );

    recorder
        .hold
        .store(false, std::sync::atomic::Ordering::SeqCst);
    feature.flush(std::time::Instant::now() + Duration::from_secs(30));
    assert_eq!(
        HarnessDocument::load(&local_dir(root.path(), "a"))
            .failures()
            .failures
            .len(),
        1
    );
    assert_eq!(
        HarnessDocument::load(&global_dir(root.path()))
            .failures()
            .failures
            .len(),
        1
    );
    assert_eq!(
        *recorder.flushes.lock().unwrap(),
        vec![(LedgerScope::Global, true), (LedgerScope::Local, true)]
    );
}

#[test]
fn concurrent_global_flushes_never_lose_each_others_records() {
    let root = tempfile::tempdir().unwrap();
    let flushers: Vec<_> = ["alpha", "beta"]
        .into_iter()
        .map(|name| {
            let root = root.path().to_path_buf();
            std::thread::spawn(move || {
                let feature = FailureLedgerFeature::new(LedgerOptions {
                    global_ledger: Some(true),
                    ..LedgerOptions::default()
                });
                for round in 0..10 {
                    let session = context(&root, &format!("{name}-{round}"), true);
                    feature.on_session_start(&session, &[]);
                    run(
                        &feature,
                        &session,
                        &[
                            user(),
                            tool_result(name, "concurrent flush", true),
                            assistant("stop", None),
                        ],
                    );
                }
            })
        })
        .collect();
    for flusher in flushers {
        flusher.join().unwrap();
    }
    let global = HarnessDocument::load(&global_dir(root.path())).failures();
    for name in ["alpha", "beta"] {
        let id =
            fingerprint_failure(FailureKind::ToolError, Some(name), None, "concurrent flush").id;
        assert_eq!(global.failures[&id].count, 10, "{name}");
    }
}

/// An `ipython` result as the loop reports it: a cell that raised returns
/// normally (the loop's `is_error` is false); only its details say `error`.
fn ipython_result(code: &str, text: &str, is_error: bool) -> ToolResultObservation {
    ToolResultObservation {
        tool_call_id: "c".to_string(),
        tool_name: "ipython".to_string(),
        args: json!({ "code": code }),
        is_error: false,
        content: vec![pa_agent::types::ToolResultContent::text(text)],
        details: json!({ "status": if is_error { "error" } else { "ok" } }),
        host_facts: Value::Null,
        earlier_results_of_tool: 0,
    }
}

#[tokio::test]
async fn a_recurrence_gets_the_cell_that_fixed_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("work").join(".git")).unwrap();
    std::fs::write(
        root.path().join("work").join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();
    let tracked: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let sink = Arc::clone(&tracked);
    let session = Arc::new(SessionFeatureContext {
        telemetry: Some(pa_core::features::FeatureTelemetry::new(
            move |name, properties| {
                sink.lock().unwrap().push((
                    name.to_string(),
                    properties.get("origin").cloned().unwrap_or(Value::Null),
                ));
            },
        )),
        ..(*context(root.path(), "a", true)).clone()
    });
    let feature = FailureLedgerFeature::default();
    let failure = "Traceback (most recent call last):\n  File \"<ipython-input-1>\", line 1, in <module>\n    agents = client.list_agents()\nAttributeError: 'AgentClient' object has no attribute 'list_agents'";
    let type_error = "Traceback (most recent call last):\n  File \"<ipython-input-2>\", line 1, in <module>\n    agents = client.agents(1)\nTypeError: agents() takes 0 positional arguments but 1 was given";
    let calls = [
        ipython_result("agents = client.list_agents()", failure, true),
        // A cell that itself raised is never the fix, whatever the loop's flag says.
        ipython_result("agents = client.agents(1)", type_error, true),
        ipython_result("agents = client.agents()", "3 agents", false),
        ipython_result("for a in client.list_agents(): pass", failure, true),
    ];
    let mut appended = Vec::new();
    for call in &calls {
        appended.push(feature.after_tool_call(&session, call).await);
    }
    let id = fingerprint_failure(
        FailureKind::PythonException,
        Some("ipython"),
        Some("AttributeError"),
        "'AgentClient' object has no attribute 'list_agents'",
    )
    .id;
    assert_eq!(
        appended,
        vec![
            None,
            None,
            None,
            Some(format!(
                "<ipython_resolution_hint>\nYou hit this before; this fixed it:\n```python\nagents = client.agents()\n```\n(failure:{id}, AttributeError; first hit at cell 1, fixed at cell 3 of this session)\n</ipython_resolution_hint>"
            )),
        ]
    );
    assert_eq!(
        *tracked.lock().unwrap(),
        vec![(
            "failure_resolution_hint".to_string(),
            Value::from("session")
        )]
    );
    // The fix was mirrored to the repo's durable store.
    assert_eq!(
        std::fs::read_dir(root.path().join("agent").join("resolution"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn a_crashed_kernel_or_another_tool_never_feeds_the_index() {
    let root = tempfile::tempdir().unwrap();
    let session = context(root.path(), "a", true);
    let feature = FailureLedgerFeature::default();
    let mut crashed = ipython_result("x = 1", "kernel died", true);
    crashed.details = json!({ "status": "error", "kernelCrashed": { "exitCode": 1 } });
    let mut other = ipython_result("x = 1", "boom", true);
    other.tool_name = "bash".to_string();
    for call in [&crashed, &crashed, &other, &other] {
        assert_eq!(feature.after_tool_call(&session, call).await, None);
    }
}
