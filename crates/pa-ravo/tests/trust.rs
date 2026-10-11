//! Trust windows end to end through the seams `pa-cli` wires RAVO into: a
//! gated commit that claims a recurring failure opens a window over the
//! entries it wrote, the failure recurring inside it is recorded on it at
//! the next ledger flush, a post-commit replay of the skill's own import
//! debits that skill, a window whose claim did not recur credits what it
//! wrote, and a dormant entry leaves the rendered harness. Scripted
//! planner, judge and replays; never a real provider or interpreter.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::refinement::RefinementResult;
use pa_core::refinement::executor::RefinerFn;
use pa_core::refinement::gate::RefinementGating;
use pa_core::refinement::ranking::{HarnessStatePromptOptions, format_harness_state_for_prompt};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::refine::{
    RefineOptions,
    RefinementSource,
    RefinementTranscript,
    execute_refinement_gated,
};
use pa_ledger::{
    FailureLedgerFeature,
    HarnessDocument,
    LedgerOptions,
    ReplayCase,
    fingerprint_tool_result_text,
};
use pa_ravo::{RavoFeature, RavoOptions, ReplayEnvironment, ReplayOutcome, ReplayRunner};
use pa_types::ai::{AssistantContentBlock, AssistantMessage, StopReason, TextContent};
use serde_json::{Value, json};

/// Self-checks always reproduce; the referee's skill-import replays run
/// clean while the skill is `fixed`, and raise once it is not.
#[derive(Default)]
struct Probe {
    broken: AtomicBool,
    skill_runs: Mutex<Vec<String>>,
}

impl ReplayRunner for Probe {
    fn run<'a>(
        &'a self,
        case: &'a ReplayCase,
        environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        let raised = ReplayOutcome::Raised {
            exception_class: "ModuleNotFoundError".to_string(),
            detail: "ModuleNotFoundError: No module named 'prime_probe'".to_string(),
        };
        let outcome = match environment {
            ReplayEnvironment::Sanitized => raised,
            ReplayEnvironment::SkillImport => {
                self.skill_runs.lock().unwrap().push(case.source.clone());
                if self.broken.load(Ordering::SeqCst) {
                    raised
                } else {
                    ReplayOutcome::Clean {
                        detail: String::new(),
                    }
                }
            }
        };
        Box::pin(async move { outcome })
    }
}

struct Session {
    _root: tempfile::TempDir,
    manager: SessionManager,
    context: Arc<SessionFeatureContext>,
    ravo: RavoFeature,
    ledger: FailureLedgerFeature,
    runner: Arc<Probe>,
    global_dir: std::path::PathBuf,
}

fn session(global_ledger: bool) -> Session {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut manager = SessionManager::in_memory(root.path());
    manager.materialize_session_file(Some(session_dir));
    let context = Arc::new(SessionFeatureContext {
        agent_dir: root.path().join("agent"),
        cwd: root.path().join("work"),
        session_id: "s1".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(serde_json::to_value(model()).unwrap()).unwrap(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: Some(manager.get_session_dir().to_path_buf()),
    });
    let runner = Arc::new(Probe::default());
    let ravo = RavoFeature::new(RavoOptions {
        enabled: Some(true),
        runner: Arc::clone(&runner) as Arc<dyn ReplayRunner>,
        replay_sys_path: Vec::new(),
    });
    let ledger = FailureLedgerFeature::with_observers(
        LedgerOptions {
            global_ledger: Some(global_ledger),
            ..LedgerOptions::default()
        },
        vec![ravo.ledger_observer()],
    );
    ravo.attach_ledger(ledger.handle());
    ledger.on_session_start(&context, &[]);
    let global_dir = root.path().join("agent").join("harness");
    Session {
        _root: root,
        manager,
        context,
        ravo,
        ledger,
        runner,
        global_dir,
    }
}

fn model() -> pa_types::ai::Model {
    serde_json::from_value(json!({
        "id": "m1", "name": "M1", "api": "test", "provider": "p1",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 8000
    }))
    .unwrap()
}

fn scripted(text: &str) -> RefinerFn {
    let text = text.to_string();
    Box::new(move |_model, _system, _prompt| {
        let reply = AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text,
                text_signature: None,
                rest: serde_json::Map::default(),
            })],
            api: "test".to_string(),
            provider: "p1".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_types::ai::Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: serde_json::Map::default(),
            discarded_usage: None,
        };
        Box::pin(async move { Ok(reply) })
    })
}

const TRACEBACK: &str = "Traceback (most recent call last):\n  File \"<cell>\", line 1, in <module>\nModuleNotFoundError: No module named 'prime_probe'";

const SKILL_PLAN: &str = r#"{"summary":"probe skill","rationale":"seen twice","expectedOutcome":"no repeat","edits":[{"action":"create","kind":"skill","id":"probe","title":"Probe","content":"Call await run()","reference":{"type":"python","import":"prime_probe","callable":"run","call_pattern":"await run()"},"arguments":{}}]}"#;
const MEMORY_PLAN: &str = r#"{"summary":"note it","rationale":"seen twice","expectedOutcome":"no repeat","edits":[{"action":"create","kind":"memory","id":"note","title":"Note","content":"Use tactic A"}]}"#;

fn ipython_failure() -> Value {
    json!({
        "role": "toolResult", "toolCallId": "c", "toolName": "ipython",
        "content": [{ "type": "text", "text": TRACEBACK }], "isError": true,
        "details": { "status": "error", "error": {
            "ename": "ModuleNotFoundError",
            "evalue": "No module named 'prime_probe'",
            "traceback": TRACEBACK.split('\n').collect::<Vec<_>>()
        } },
        "timestamp": 1
    })
}

fn bash_failure(text: &str) -> Value {
    json!({
        "role": "toolResult", "toolCallId": "c", "toolName": "bash",
        "content": [{ "type": "text", "text": text }], "isError": true, "timestamp": 1
    })
}

fn judge_claiming(fingerprint: &str) -> RefinerFn {
    scripted(&format!(
        r#"{{"verdict":"pass","score":80,"failedCriteria":[],"addressedFingerprints":["{fingerprint}"],"rationale":"fixes it"}}"#
    ))
}

impl Session {
    /// One turn whose tool call ends in `result`, then every check and
    /// replay it started, then the flush that lands them.
    fn turn(&self, result: Value) {
        let message = |value: Value| -> pa_agent::types::AgentMessage {
            serde_json::from_value(value).unwrap()
        };
        let assistant = |stop: &str| {
            message(json!({
                "role": "assistant", "content": [], "api": "test", "provider": "p1", "model": "m1",
                "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0 },
                "stopReason": stop, "timestamp": 1
            }))
        };
        for turn in [
            message(json!({ "role": "user", "content": "go", "timestamp": 1 })),
            assistant("toolUse"),
            message(result),
            assistant("stop"),
        ] {
            self.ledger.on_message_end(&self.context, &turn);
        }
        self.settle();
    }

    /// Wait for the ledger, the self-checks and the trust replays, then
    /// flush what they left (TS: the next `agent_end` flush).
    fn settle(&self) {
        let handle = self.ledger.handle();
        assert!(handle.wait_idle(Duration::from_secs(30)));
        assert!(self.ravo.wait_replay_checks(Duration::from_secs(30)));
        handle.request_flush(&self.context.session_id);
        assert!(handle.wait_idle(Duration::from_secs(30)));
    }

    async fn refine(&mut self, plan: &str, judge: RefinerFn) -> RefinementResult {
        let gate = self.ravo.refinement_gate(&self.context).unwrap();
        let messages = vec![
            serde_json::from_value(
                json!({ "role": "user", "content": "do it twice", "timestamp": 1 }),
            )
            .unwrap(),
        ];
        execute_refinement_gated(
            &mut self.manager,
            RefinementTranscript {
                messages: &messages,
                refinement_history: &[],
            },
            &self.global_dir,
            &model(),
            &RefineOptions::default(),
            RefinementSource::User,
            scripted(plan),
            None,
            Some(RefinementGating {
                gate,
                model_call: judge,
            }),
        )
        .await
        .unwrap()
        .0
    }

    fn local(&self) -> HarnessDocument {
        HarnessDocument::load(&self.manager.get_session_dir().join("harness"))
    }

    fn fingerprint(&self) -> String {
        self.ledger
            .handle()
            .session_ledger("s1")
            .unwrap()
            .failures
            .keys()
            .next()
            .unwrap()
            .clone()
    }
}

/// The M6 path: a skill commit claiming a recurring failure opens a window
/// over the skill with the import it wrote; the failure recurring inside
/// the window queues a replay of that import, which raises again (upheld),
/// and the next local flush faults the window and debits the skill alone.
#[tokio::test]
async fn an_upheld_post_commit_replay_debits_the_skill_it_ran_for() {
    let mut session = session(true);
    session.turn(ipython_failure());
    session.turn(ipython_failure());
    let fingerprint = session.fingerprint();
    let result = session
        .refine(SKILL_PLAN, judge_claiming(&fingerprint))
        .await;
    assert_eq!(result.extensions["ravo"]["decision"], json!("commit"));
    assert_eq!(
        result.applied_edits[0].after.as_ref().unwrap().extensions["trust"]["score"],
        json!(50)
    );
    let opened = session.local().get("trustWindows").cloned().unwrap();
    assert_eq!(
        opened[&result.id],
        json!({
            "proposalId": result.id,
            "touched": ["skill:probe"],
            "claimedFingerprints": [fingerprint],
            "committedTurn": 2,
            "untilTurn": 22,
            "outcome": "open",
            "skillImports": { "skill:probe": ["prime_probe"] }
        })
    );
    let skill =
        |session: &Session| session.local().get("entries").unwrap()["skill"]["probe"].clone();
    let created_at = skill(&session)["updated_at"].clone();
    assert_eq!(
        skill(&session)["trust"],
        json!({ "score": 50, "updated_at": created_at, "events": [] })
    );

    // The fix did not hold: the claimed failure recurs at ordinal 3.
    session.runner.broken.store(true, Ordering::SeqCst);
    session.turn(ipython_failure());
    let window = session.local().get("trustWindows").unwrap()[&result.id].clone();
    assert_eq!(window["outcome"], json!("faulted"));
    assert_eq!(window["recurrences"], json!({ fingerprint.clone(): 3 }));
    assert_eq!(window["faultedFingerprints"], json!([fingerprint]));
    assert_eq!(window["faultedEntries"], json!(["skill:probe"]));
    assert_eq!(window["settledTurn"], json!(3));
    assert_eq!(window["adjudications"][0]["status"], json!("upheld"));
    let trust = skill(&session)["trust"].clone();
    assert_eq!(trust["score"], json!(35));
    assert_eq!(
        (
            &trust["events"][0]["reason"],
            &trust["events"][0]["delta"],
            &trust["events"][0]["fingerprintId"]
        ),
        (&json!("measured_fault"), &json!(-15), &json!(fingerprint))
    );
    // The gate's referee ran once (clean), the trust replay once (raised).
    assert_eq!(
        *session.runner.skill_runs.lock().unwrap(),
        ["import prime_probe", "import prime_probe"]
    );
}

/// A memory window cannot be replayed: its claim recurring closes it
/// contested (no credit, no debit); a window whose claim stays quiet past
/// its end closes clean and credits what it wrote.
#[tokio::test]
async fn windows_close_contested_on_a_recurrence_and_clean_otherwise() {
    let mut session = session(true);
    session.turn(bash_failure("boom: exit 1"));
    session.turn(bash_failure("boom: exit 1"));
    let fingerprint = fingerprint_tool_result_text(Some("bash"), "boom: exit 1", true)
        .unwrap()
        .id;
    let result = session
        .refine(MEMORY_PLAN, judge_claiming(&fingerprint))
        .await;
    assert_eq!(result.extensions["ravo"]["decision"], json!("commit"));
    session.turn(bash_failure("boom: exit 1"));
    // Ordinals 4..=23 are other failures: the window (2..=22) ends.
    for index in 0..20 {
        session.turn(bash_failure(&format!("other {index}: exit 1")));
    }
    let window = session.local().get("trustWindows").unwrap()[&result.id].clone();
    assert_eq!(
        (
            &window["outcome"],
            &window["settledTurn"],
            &window["recurrences"]
        ),
        (
            &json!("contested"),
            &json!(23),
            &json!({ fingerprint.clone(): 3 })
        )
    );
    let note = |session: &Session| {
        session.local().get("entries").unwrap()["memory"]["note"]["trust"].clone()
    };
    assert_eq!(note(&session)["score"], json!(50));
    assert_eq!(note(&session)["events"], json!([]));

    // A second commit claiming a failure that never recurs again.
    let quiet = fingerprint_tool_result_text(Some("bash"), "other 0: exit 1", true)
        .unwrap()
        .id;
    let plan = MEMORY_PLAN
        .replace("\"note\"", "\"quiet\"")
        .replace("Note", "Quiet");
    session.turn(bash_failure("other 0: exit 1"));
    let second = session.refine(&plan, judge_claiming(&quiet)).await;
    assert_eq!(second.extensions["ravo"]["decision"], json!("commit"));
    for index in 0..21 {
        session.turn(bash_failure(&format!("later {index}: exit 1")));
    }
    let window = session.local().get("trustWindows").unwrap()[&second.id].clone();
    assert_eq!(window["outcome"], json!("clean"));
    let trust = session.local().get("entries").unwrap()["memory"]["quiet"]["trust"].clone();
    assert_eq!(
        (&trust["score"], &trust["events"][0]["reason"]),
        (&json!(55), &json!("clean_window"))
    );
}

/// With the global ledger off nothing advances the clock windows are
/// measured on and no replay is planned: no trust moves.
#[tokio::test]
async fn with_the_global_ledger_off_no_trust_moves() {
    let mut session = session(false);
    session.turn(ipython_failure());
    session.turn(ipython_failure());
    let fingerprint = session.fingerprint();
    let result = session
        .refine(SKILL_PLAN, judge_claiming(&fingerprint))
        .await;
    assert_eq!(result.extensions["ravo"]["decision"], json!("commit"));
    session.runner.broken.store(true, Ordering::SeqCst);
    for _ in 0..3 {
        session.turn(ipython_failure());
    }
    let window = session.local().get("trustWindows").unwrap()[&result.id].clone();
    assert_eq!(
        (
            &window["outcome"],
            &window["committedTurn"],
            window.get("recurrences")
        ),
        (&json!("open"), &json!(0), None)
    );
    assert_eq!(
        session.local().get("entries").unwrap()["skill"]["probe"]["trust"]["score"],
        json!(50)
    );
    assert_eq!(session.runner.skill_runs.lock().unwrap().len(), 1);
}

/// A dormant entry leaves the rendered harness but not the store; the
/// line announcing it is the TS text.
#[test]
fn a_dormant_entry_leaves_the_rendered_harness() {
    let session = session(true);
    let hook = session
        .ravo
        .harness_prompt_hook(&session.context)
        .expect("the feature adjusts the harness digest");
    let mut state = pa_core::refinement::empty_harness_state();
    for (id, score) in [("kept", 30), ("dormant", 29), ("untrusted", 50)] {
        let mut entry: pa_core::refinement::HarnessEntry = serde_json::from_value(json!({
            "id": id, "kind": "memory", "title": id, "content": id, "path": "general",
            "scope": "local", "reference": {}, "arguments": {}, "metadata": {},
            "source": "refine", "created_at": "t", "updated_at": "t", "version": 1
        }))
        .unwrap();
        if id != "untrusted" {
            entry.extensions.insert(
                "trust".to_string(),
                json!({ "score": score, "updated_at": "t", "events": [] }),
            );
        }
        state
            .entries
            .get_mut(&pa_core::refinement::RefinementKind::Memory)
            .unwrap()
            .insert(id.to_string(), entry);
    }
    let rendered = format_harness_state_for_prompt(
        &state,
        &HarnessStatePromptOptions {
            include_ipython_examples: Some(false),
            adjustment: Some(hook.adjust(&state)),
            ..HarnessStatePromptOptions::default()
        },
    );
    let memory: Vec<&str> = rendered
        .lines()
        .skip_while(|line| *line != "memory: 2")
        .take(5)
        .collect();
    assert_eq!(
        memory,
        [
            "memory: 2",
            "- [local:kept] kept (general, v1): kept",
            "- [local:untrusted] untrusted (general, v1): untrusted",
            "- +1 dormant memory entries (below trust threshold; still readable and editable)",
            "",
        ]
    );
}
