//! RAVO end to end through the seams `pa-cli` wires it into: a session's
//! refine meets the gate (a scripted planner and judge, never a real
//! provider), the failure ledger feeds it the recurring failures, a
//! measured commit opens a provisional window, and a recurrence inside the
//! window is recorded on the champion when the ledger flushes.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::refinement::executor::RefinerFn;
use pa_core::refinement::gate::RefinementGating;
use pa_core::refinement::{HarnessScope, RefinementKind, RefinementResult, load_harness_state};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::refine::{
    RefineOptions,
    RefinementSource,
    RefinementTranscript,
    execute_refinement_gated,
};
use pa_core::session_engine::turn_boundary::{
    PendingRefine,
    RefineRequester,
    RefineTrigger,
    TurnBoundaryRequests,
};
use pa_ledger::{
    FailureLedgerFeature,
    HarnessDocument,
    LedgerOptions,
    ReplayCase,
    fingerprint_tool_result_text,
    format_recurrence_refine_instructions,
    recurring_failures,
};
use pa_ravo::{
    RAVO_BASELINE_CHANGED_RATIONALE,
    RAVO_GATE_DECISION_EVENT,
    RavoFeature,
    RavoOptions,
    ReplayEnvironment,
    ReplayOutcome,
    ReplayRunner,
};
use pa_telemetry::Properties;
use pa_types::ai::{AssistantContentBlock, AssistantMessage, Model, StopReason, TextContent};
use serde_json::{Value, json};

/// No replay ever runs in these sessions' proposals (memory edits only).
struct NeverRuns;

impl ReplayRunner for NeverRuns {
    fn run<'a>(
        &'a self,
        _case: &'a ReplayCase,
        _environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        panic!("a memory edit never meets a replay");
    }
}

type Events = Arc<Mutex<Vec<(String, Properties)>>>;

struct Session {
    _root: tempfile::TempDir,
    manager: SessionManager,
    context: Arc<SessionFeatureContext>,
    ravo: RavoFeature,
    ledger: FailureLedgerFeature,
    events: Events,
    global_dir: PathBuf,
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "m1", "name": "M1", "api": "test", "provider": "p1",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 8000
    }))
    .unwrap()
}

fn session() -> Session {
    session_with(Arc::new(NeverRuns))
}

fn session_with(runner: Arc<dyn ReplayRunner>) -> Session {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut manager = SessionManager::in_memory(root.path());
    manager.materialize_session_file(Some(session_dir));
    let events: Events = Arc::default();
    let recorded = Arc::clone(&events);
    let context = Arc::new(SessionFeatureContext {
        agent_dir: root.path().join("agent"),
        cwd: root.path().join("work"),
        session_id: "s1".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        telemetry: Some(FeatureTelemetry::new(move |name, properties| {
            recorded
                .lock()
                .unwrap()
                .push((name.to_string(), properties));
        })),
        rlm_depth: 0,
        session_artifact_dir: Some(manager.get_session_dir().to_path_buf()),
    });
    let ravo = RavoFeature::new(RavoOptions {
        enabled: Some(true),
        runner,
        replay_sys_path: Vec::new(),
    });
    let ledger = FailureLedgerFeature::with_observers(
        LedgerOptions {
            global_ledger: Some(false),
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
        events,
        global_dir,
    }
}

fn reply(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContentBlock::Text(TextContent {
            text: text.to_string(),
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
    }
}

fn scripted(text: &str) -> RefinerFn {
    let text = text.to_string();
    Box::new(move |_model, _system, _prompt| Box::pin(async move { Ok(reply(&text)) }))
}

/// A judge that rewrites the local harness store while it is consulted
/// (another writer landing while the refine plans).
fn judge_moving_the_baseline(text: &str, harness_dir: &Path) -> RefinerFn {
    let text = text.to_string();
    let harness_dir = harness_dir.to_path_buf();
    Box::new(move |_model, _system, _prompt| {
        let mut state = load_harness_state(&harness_dir, HarnessScope::Local);
        let mut moved = state.entries[&RefinementKind::Memory]
            .values()
            .next()
            .cloned()
            .expect("a memory entry to move");
        moved.id = "other".to_string();
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("other".to_string(), moved);
        pa_core::refinement::save_harness_state(&harness_dir, &state).unwrap();
        Box::pin(async move { Ok(reply(&text)) })
    })
}

const MEMORY_PLAN: &str = r#"{"summary":"note the tactic","rationale":"seen twice","expectedOutcome":"no repeat","edits":[{"action":"create","kind":"memory","title":"Tactic","content":"Use tactic A"}]}"#;
const SECOND_PLAN: &str = r#"{"summary":"note another","rationale":"seen","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","title":"Second","content":"Use tactic B"}]}"#;

impl Session {
    async fn refine(&mut self, plan: &str, judge: RefinerFn) -> RefinementResult {
        self.refine_with(
            &RefineOptions::default(),
            RefinementSource::User,
            plan,
            judge,
        )
        .await
    }

    async fn refine_with(
        &mut self,
        options: &RefineOptions,
        source: RefinementSource,
        plan: &str,
        judge: RefinerFn,
    ) -> RefinementResult {
        let gate = self
            .ravo
            .refinement_gate(&self.context)
            .expect("the feature offers a gate");
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
            options,
            source,
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

    fn harness_dir(&self) -> PathBuf {
        self.manager.get_session_dir().join("harness")
    }

    fn stored(&self) -> HarnessDocument {
        HarnessDocument::load(&self.harness_dir())
    }

    /// One turn whose `bash` call fails, through the ledger's hooks.
    fn failing_turn(&self) {
        self.turn_failing_with(json!({
            "role": "toolResult", "toolCallId": "c", "toolName": "bash",
            "content": [{ "type": "text", "text": "boom: exit 1" }], "isError": true,
            "timestamp": 1
        }));
    }

    /// One turn whose tool call ends in `result`, through the ledger's hooks.
    fn turn_failing_with(&self, result: Value) {
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
        self.ledger.on_agent_end(&self.context);
        assert!(self.ledger.handle().wait_idle(Duration::from_secs(30)));
    }

    /// Every adoption event the session tracked, as JSON.
    fn decisions(&self) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == RAVO_GATE_DECISION_EVENT)
            .map(|(_, properties)| {
                Value::Object(
                    properties
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                )
            })
            .collect()
    }
}

fn decision(decision: &str, cause: &str, recurring: u64, claimed: u64) -> Value {
    json!({
        "decision": decision,
        "scope": "local",
        "reason": "manual",
        "cause": cause,
        "recurring": recurring,
        "claimed": claimed,
    })
}

/// A directed refine that claims nothing applies unmeasured: the edits land,
/// the report rides the result, and the stored RAVO state is the untouched
/// starting state.
#[tokio::test]
async fn a_claimless_commit_applies_unmeasured() {
    let mut session = session();
    let result = session
        .refine(
            MEMORY_PLAN,
            scripted(r#"{"verdict":"pass","score":72,"failedCriteria":[],"rationale":"fine"}"#),
        )
        .await;
    assert!(result.applied_edits.iter().all(|edit| edit.applied));
    assert_eq!(result.extensions["ravo"]["decision"], json!("commit"));
    assert_eq!(result.extensions["ravo"]["measurable"], json!(false));
    assert_eq!(
        session.stored().get("ravo"),
        Some(&json!({
            "lineage": [],
            "championId": null,
            "opponents": { "criteria": [
                { "id": "evidence", "seedWeight": 1, "currentWeight": 1 },
                { "id": "scope", "seedWeight": 1, "currentWeight": 1 },
                { "id": "minimality", "seedWeight": 1, "currentWeight": 1 },
                { "id": "contracts", "seedWeight": 1, "currentWeight": 1 },
                { "id": "novelty", "seedWeight": 1, "currentWeight": 1 }
            ] },
            "evaluatedProposalIds": []
        }))
    );
    assert_eq!(
        session.decisions(),
        [decision("commit_unmeasured", "none", 0, 0)]
    );
}

/// A judge that fails the candidate rejects it: nothing applies, the
/// rejected result carries the report and cause, and the consumed
/// evaluation is saved so the id cannot be evaluated twice.
#[tokio::test]
async fn a_failing_verdict_rejects_and_consumes_the_evaluation() {
    let mut session = session();
    let result = session
        .refine(
            MEMORY_PLAN,
            scripted(r#"{"verdict":"fail","score":90,"failedCriteria":[],"rationale":"worse"}"#),
        )
        .await;
    assert_eq!(
        (
            result.summary.as_str(),
            result.applied_edits.iter().any(|edit| edit.applied),
            result.applied_edits[0].error.as_deref(),
            &result.extensions["ravo"]["decision"],
            &result.extensions["rejectionCause"],
        ),
        (
            "RAVO gate rejected: note the tactic",
            false,
            Some("ravo gate rejected (reject_deep): worse"),
            &json!("reject_deep"),
            &json!("gate"),
        )
    );
    let state = load_harness_state(&session.harness_dir(), HarnessScope::Local);
    assert!(state.entries[&RefinementKind::Memory].is_empty());
    assert_eq!(
        session.stored().get("ravo").unwrap()["evaluatedProposalIds"],
        json!([result.id])
    );
    assert_eq!(session.decisions(), [decision("reject_deep", "gate", 0, 0)]);
}

/// An approval the harness moved out from under is lost at apply time: the
/// edits do not land and the rejection says why.
#[tokio::test]
async fn an_approval_is_lost_when_the_baseline_moves() {
    let mut session = session();
    session
        .refine(
            MEMORY_PLAN,
            scripted(r#"{"verdict":"pass","score":72,"failedCriteria":[],"rationale":"fine"}"#),
        )
        .await;
    let harness_dir = session.harness_dir();
    let result = session
        .refine(
            SECOND_PLAN,
            judge_moving_the_baseline(
                r#"{"verdict":"pass","score":75,"failedCriteria":[],"rationale":"fine"}"#,
                &harness_dir,
            ),
        )
        .await;
    assert_eq!(
        (
            &result.extensions["ravo"]["decision"],
            &result.extensions["ravo"]["rationale"],
            &result.extensions["rejectionCause"],
            result.harness_state_path.as_str(),
        ),
        (
            &json!("reject_deep"),
            &json!(RAVO_BASELINE_CHANGED_RATIONALE),
            &json!("baseline_changed"),
            "",
        )
    );
    let state = load_harness_state(&session.harness_dir(), HarnessScope::Local);
    assert!(!state.entries[&RefinementKind::Memory].contains_key("second"));
}

/// The vertical slice: a failure recurs in the session's ledger, the refine
/// is charged it, the judge credits the fix, the commit is measured and
/// opens a window on the session ledger's ordinal, and the same failure
/// recurring inside the window is recorded on the champion at the next
/// ledger flush.
#[tokio::test]
async fn a_measured_commit_regresses_when_its_claim_recurs_in_the_window() {
    let mut session = session();
    session.failing_turn();
    session.failing_turn();
    let fingerprint = fingerprint_tool_result_text(Some("bash"), "boom: exit 1", true)
        .unwrap()
        .id;
    let judge = format!(
        r#"{{"verdict":"pass","score":80,"failedCriteria":[],"addressedFingerprints":["{fingerprint}"],"rationale":"fixes it"}}"#
    );
    let result = session.refine(MEMORY_PLAN, scripted(&judge)).await;
    assert_eq!(
        (
            &result.extensions["ravo"]["decision"],
            &result.extensions["ravo"]["measurable"],
            &result.extensions["ravo"]["failureOpponents"],
        ),
        (
            &json!("commit"),
            &json!(true),
            &json!([format!("failure:{fingerprint}")]),
        )
    );
    let champion = |session: &Session| session.stored().get("ravo").unwrap()["lineage"][0].clone();
    assert_eq!(
        champion(&session)["provisional"],
        json!({ "committedTurn": 2, "untilTurn": 22, "clock": "local-ordinal" })
    );
    assert_eq!(
        champion(&session)["claimedFingerprints"],
        json!([fingerprint])
    );
    // The ledger's own key survived the refine's save.
    assert_eq!(session.stored().failures().failures[&fingerprint].count, 2);

    session.failing_turn();
    assert_eq!(
        champion(&session)["provisional"],
        json!({
            "committedTurn": 2,
            "untilTurn": 22,
            "clock": "local-ordinal",
            "observedRecurrence": { "turn": 3, "fingerprints": [fingerprint] }
        })
    );
    assert_eq!(session.decisions(), [decision("commit", "none", 1, 1)]);
}

/// While a refine runs, the session's ledger flushes wait (the certificate
/// binds the state); when it ends, what they skipped lands.
#[tokio::test]
async fn ledger_flushes_wait_for_a_running_refine() {
    let session = session();
    let gate = session.ravo.refinement_gate(&session.context).unwrap();
    let hold = gate.begin_refine();
    session.failing_turn();
    assert_eq!(session.stored().get("failures"), None);
    drop(hold);
    assert!(session.ledger.handle().wait_idle(Duration::from_secs(30)));
    assert_eq!(session.stored().failures().failures.len(), 1);
}

/// `PRIME_AGENT_RAVO=0` (here the option) leaves refinements ungated.
#[tokio::test]
async fn a_disabled_gate_lets_the_refine_apply_ungated() {
    let mut session = session();
    session.ravo = RavoFeature::new(RavoOptions {
        enabled: Some(false),
        runner: Arc::new(NeverRuns),
        replay_sys_path: Vec::new(),
    });
    let result = session
        .refine(MEMORY_PLAN, scripted("the judge is never asked"))
        .await;
    assert!(result.applied_edits.iter().all(|edit| edit.applied));
    assert!(result.extensions.is_empty());
    assert_eq!(session.stored().get("ravo"), None);
}

/// Answers every run with the recorded exception, counting the runs.
struct Reproduces(Mutex<Vec<String>>);

impl ReplayRunner for Reproduces {
    fn run<'a>(
        &'a self,
        case: &'a ReplayCase,
        environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        assert_eq!(environment, ReplayEnvironment::Sanitized);
        self.0.lock().unwrap().push(case.source.clone());
        Box::pin(async {
            ReplayOutcome::Raised {
                exception_class: "ModuleNotFoundError".to_string(),
                detail: "ModuleNotFoundError: No module named 'foo'".to_string(),
            }
        })
    }
}

/// A replay case derived from a cell's own traceback is self-checked off
/// the turn path, once per session, and the ledger's next flush stores it
/// verified: only then is it evidence the referee can run.
#[tokio::test]
async fn derived_replay_cases_are_self_checked_once_and_stored_verified() {
    let runner = Arc::new(Reproduces(Mutex::default()));
    let session = session_with(Arc::clone(&runner) as Arc<dyn ReplayRunner>);
    let traceback = "Traceback (most recent call last):\n  File \"<cell>\", line 1, in <module>\nModuleNotFoundError: No module named 'foo'";
    let cell = json!({
        "role": "toolResult", "toolCallId": "c", "toolName": "ipython",
        "content": [{ "type": "text", "text": traceback }], "isError": true,
        "details": { "status": "error", "error": {
            "ename": "ModuleNotFoundError",
            "evalue": "No module named 'foo'",
            "traceback": traceback.split('\n').collect::<Vec<_>>()
        } },
        "timestamp": 1
    });
    session.turn_failing_with(cell.clone());
    assert!(session.ravo.wait_replay_checks(Duration::from_secs(30)));
    session.turn_failing_with(cell);
    assert!(session.ravo.wait_replay_checks(Duration::from_secs(30)));
    assert_eq!(runner.0.lock().unwrap().clone(), ["import foo"]);
    let ledger = session.stored().failures();
    let record = ledger.failures.values().next().unwrap();
    assert_eq!(
        record
            .replay_cases
            .iter()
            .map(|case| (case.source.as_str(), case.verified_at.is_some()))
            .collect::<Vec<_>>(),
        [("import foo", true)]
    );
}

/// A global refine writes under the harness state lock every ledger flush
/// takes: while another writer holds it, the refine waits for that writer
/// and re-reads, so it keeps what the writer saved instead of overwriting
/// it, and once it lands the lock is released.
#[tokio::test]
async fn a_global_refine_takes_the_harness_state_lock() {
    let mut session = session();
    let gate = session.ravo.refinement_gate(&session.context).unwrap();
    let messages = vec![
        serde_json::from_value(json!({ "role": "user", "content": "do it twice", "timestamp": 1 }))
            .unwrap(),
    ];
    let gating = || RefinementGating {
        gate: Arc::clone(&gate),
        model_call: scripted(
            r#"{"verdict":"pass","score":72,"failedCriteria":[],"rationale":"fine"}"#,
        ),
    };
    let options = RefineOptions {
        global: true,
        ..RefineOptions::default()
    };
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = {
        let dir = session.global_dir.clone();
        std::thread::spawn(move || {
            let held = pa_ledger::acquire_harness_state_lock(&dir).unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let mut document = pa_ledger::HarnessDocument::load(&dir);
            document.set("holderMark", json!(true));
            document.save(&dir).unwrap();
            drop(held);
        })
    };
    held_rx.recv().unwrap();
    let (result, _) = execute_refinement_gated(
        &mut session.manager,
        RefinementTranscript {
            messages: &messages,
            refinement_history: &[],
        },
        &session.global_dir,
        &model(),
        &options,
        RefinementSource::User,
        scripted(MEMORY_PLAN),
        None,
        Some(gating()),
    )
    .await
    .unwrap();
    holder.join().unwrap();
    assert!(result.applied_edits.iter().all(|edit| edit.applied));
    let saved = std::fs::read_to_string(session.global_dir.join("harness_state.json")).unwrap();
    assert!(
        saved.contains("holderMark"),
        "the refine re-read after the holder's save and kept it: {saved}"
    );
    // Released once the save landed.
    assert!(pa_ledger::acquire_harness_state_lock(&session.global_dir).is_ok());
}

/// The loop RAVO closes on its own: a failure entering the recurring set
/// queues a recurrence refine (once per fingerprint per session) on the
/// session's pending refine, the refine is held to its trigger, a repair
/// the judge credits commits with a window, the claimed failure recurring
/// inside it queues a regression repair, and a repair that claims nothing
/// is refused as unclaimed.
// One scenario end to end: the steps depend on each other's state.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn failures_queue_their_own_refines_and_repairs() {
    let mut session = session();
    let requests = Arc::new(TurnBoundaryRequests::new());
    let gate = session.ravo.refinement_gate(&session.context).unwrap();
    gate.attach_refine_requester(RefineRequester::new(&requests));
    let fingerprint = fingerprint_tool_result_text(Some("bash"), "boom: exit 1", true)
        .unwrap()
        .id;

    session.failing_turn();
    assert_eq!(requests.take_refine().await, None);
    session.failing_turn();
    let recurrence = requests.take_refine().await.expect("a recurrence refine");
    let records = recurring_failures(&session.stored().failures(), None);
    assert_eq!(
        recurrence,
        PendingRefine {
            instructions: Some(format_recurrence_refine_instructions(&records)),
            global: false,
            trigger: Some(RefineTrigger {
                data: json!({
                    "reason": "recurrence",
                    "kind": "failure",
                    "triggerFingerprintIds": [fingerprint]
                }),
                joined_by_agent: false,
            }),
            plan_id: None,
        }
    );
    // Once per fingerprint per session.
    session.failing_turn();
    assert_eq!(requests.take_refine().await, None);

    let options = RefineOptions {
        global: recurrence.global,
        instructions: recurrence.instructions.clone(),
        rollback_id: None,
        trigger: recurrence.trigger.clone(),
        pinned_plan: None,
        package_state: None,
    };
    let claim = format!(
        r#"{{"verdict":"pass","score":80,"failedCriteria":[],"addressedFingerprints":["{fingerprint}"],"rationale":"fixes it"}}"#
    );
    let result = session
        .refine_with(
            &options,
            RefinementSource::SelfRefine,
            MEMORY_PLAN,
            scripted(&claim),
        )
        .await;
    assert_eq!(
        (
            &result.extensions["ravo"]["decision"],
            &result.extensions["triggerFingerprintIds"],
        ),
        (&json!("commit"), &json!([fingerprint]))
    );

    session.failing_turn();
    let repair = requests.take_refine().await.expect("a regression repair");
    let repair_request = repair.trigger.as_ref().unwrap().data.clone();
    assert_eq!(
        repair_request,
        json!({
            "reason": "regression",
            "kind": "failure",
            "triggerFingerprintIds": [fingerprint]
        })
    );
    assert!(
        repair
            .instructions
            .as_deref()
            .unwrap()
            .starts_with("Automatic refine triggered by regression")
    );
    let options = RefineOptions {
        trigger: repair.trigger.clone(),
        instructions: repair.instructions.clone(),
        ..RefineOptions::default()
    };
    let unclaimed = session
        .refine_with(
            &options,
            RefinementSource::SelfRefine,
            SECOND_PLAN,
            scripted(r#"{"verdict":"pass","score":90,"failedCriteria":[],"addressedFingerprints":[],"rationale":"unrelated"}"#),
        )
        .await;
    assert_eq!(
        (
            &unclaimed.extensions["ravo"]["decision"],
            &unclaimed.extensions["rejectionCause"],
            &unclaimed.extensions["ravo"]["failureOpponents"],
        ),
        (
            &json!("reject_unclaimed"),
            &json!("gate"),
            &json!([format!("failure:{fingerprint}")]),
        )
    );
    assert_eq!(
        session.decisions(),
        [
            json!({ "decision": "commit", "scope": "local", "reason": "recurrence", "cause": "none", "recurring": 1, "claimed": 1 }),
            json!({ "decision": "reject_unclaimed", "scope": "local", "reason": "regression", "cause": "gate", "recurring": 1, "claimed": 0 }),
        ]
    );
}

/// With the gate on, an approved automatic review refines the global
/// harness unless it asked for a local refine; with it off the native
/// policy (no policy offered) stays.
#[test]
fn the_gate_offers_the_global_default_auto_refine_policy() {
    let session = session();
    let policy = session
        .ravo
        .auto_refine_policy(&session.context)
        .expect("a policy while the gate is on");
    let review = pa_core::refinement::executor::AutoRefineReview {
        should_refine: true,
        rationale: "a standing rule".to_string(),
        instructions: None,
        reply: serde_json::Map::new(),
    };
    assert!(policy.approved_refine("compact", &review).global);
    let disabled = RavoFeature::new(RavoOptions {
        enabled: Some(false),
        runner: Arc::new(NeverRuns),
        replay_sys_path: Vec::new(),
    });
    assert!(disabled.auto_refine_policy(&session.context).is_none());
}

/// An aborted turn drops the pending repair unserviced: it repaired
/// nothing, so the failure that queued it may queue a repair again at the
/// next boundary (TS `refine_failed` releasing a cancelled request's
/// triggers); a repair that ran keeps it triggered for the session.
#[tokio::test]
async fn a_dropped_repair_releases_its_failure_to_queue_again() {
    let mut session = session();
    let requests = Arc::new(TurnBoundaryRequests::new());
    let gate = session.ravo.refinement_gate(&session.context).unwrap();
    gate.attach_refine_requester(RefineRequester::new(&requests));
    let fingerprint = fingerprint_tool_result_text(Some("bash"), "boom: exit 1", true)
        .unwrap()
        .id;
    session.failing_turn();
    session.failing_turn();
    let recurrence = requests.take_refine().await.expect("a recurrence refine");
    let options = RefineOptions {
        trigger: recurrence.trigger.clone(),
        instructions: recurrence.instructions.clone(),
        ..RefineOptions::default()
    };
    let claim = format!(
        r#"{{"verdict":"pass","score":80,"failedCriteria":[],"addressedFingerprints":["{fingerprint}"],"rationale":"fixes it"}}"#
    );
    session
        .refine_with(
            &options,
            RefinementSource::SelfRefine,
            MEMORY_PLAN,
            scripted(&claim),
        )
        .await;

    session.failing_turn();
    assert!(requests.refine_pending().await);
    requests.clear_pending().await;
    session.failing_turn();
    let repair = requests
        .take_refine()
        .await
        .expect("the released failure queues its repair again");
    assert_eq!(
        repair.trigger.unwrap().data,
        json!({
            "reason": "regression",
            "kind": "failure",
            "triggerFingerprintIds": [fingerprint]
        })
    );
    // Taken to run, not dropped: still triggered.
    session.failing_turn();
    assert_eq!(requests.take_refine().await, None);
}
