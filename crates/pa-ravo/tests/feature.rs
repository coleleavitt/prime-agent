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
use pa_core::refinement::{load_harness_state, HarnessScope, RefinementKind, RefinementResult};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::refine::{
    execute_refinement_gated, RefineOptions, RefinementSource, RefinementTranscript,
};
use pa_ledger::{
    fingerprint_tool_result_text, FailureLedgerFeature, HarnessDocument, LedgerOptions, ReplayCase,
};
use pa_ravo::{
    RavoFeature, RavoOptions, ReplayEnvironment, ReplayOutcome, ReplayRunner,
    RAVO_BASELINE_CHANGED_RATIONALE, RAVO_GATE_DECISION_EVENT,
};
use pa_telemetry::Properties;
use pa_types::ai::{AssistantContentBlock, AssistantMessage, Model, StopReason, TextContent};
use serde_json::{json, Value};

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
        runner: Arc::new(NeverRuns),
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
        let gate = self
            .ravo
            .refinement_gate(&self.context)
            .expect("the feature offers a gate");
        let messages = vec![serde_json::from_value(
            json!({ "role": "user", "content": "do it twice", "timestamp": 1 }),
        )
        .unwrap()];
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

    fn harness_dir(&self) -> PathBuf {
        self.manager.get_session_dir().join("harness")
    }

    fn stored(&self) -> HarnessDocument {
        HarnessDocument::load(&self.harness_dir())
    }

    /// One turn whose `bash` call fails, through the ledger's hooks.
    fn failing_turn(&self) {
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
            message(json!({
                "role": "toolResult", "toolCallId": "c", "toolName": "bash",
                "content": [{ "type": "text", "text": "boom: exit 1" }], "isError": true,
                "timestamp": 1
            })),
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
