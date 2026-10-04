//! `ravo.run` (TS `ravo/run-service.ts`): the full RAVO controller loop
//! over one continual-harness refinement proposal, one run per session at
//! a time, in the background.
//!
//! Children (inspect, plan, implement, repair, judge, supervisor) are
//! JSON-only prompts to the session's model, one provider call each (TS
//! ran them as tool-less `RunAgent` children); the fast screen is the
//! structural screen; the deep gate and the five hygiene opponents share
//! ONE memoized judge call per proposal; recurring failure fingerprints are
//! opponents that pass iff the proposal claims them and the referee fails
//! to refute the claim by re-running the recorded replay case. The commit
//! gate applies the proposal to the target store and persists the stepped
//! reducer state into its `ravo` key (a global run under the harness state
//! lock), refusing, and stopping the run as `stale_cas`, when the stored
//! RAVO state changed since the run read it.

pub mod archive;
pub mod context;
pub mod controller;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use pa_core::refinement::planner::{
    apply_refinement_proposal, count_valid_refinement_edits, normalize_refinement_proposal,
    ApplyOptions, RefinementProposal,
};
use pa_core::refinement::ranking::{format_harness_state_for_prompt, HarnessStatePromptOptions};
use pa_core::refinement::{
    load_harness_state, save_harness_state, HarnessScope, HarnessState, RefinementAction,
};
use pa_ledger::{
    failure_opponent_id, format_failure_ledger_for_prompt, recurring_failures, FailureRecord,
};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use self::context::{
    build_bounded_context_view, render_context, BoundedContextView, ContextArchive, ContextAtom,
    ContextAtomKind, ContextViewLimits,
};
use self::controller::{
    empty_error_budget, run_ravo_controller, stop_name, ChildCallOptions, ChildFuture, ChildResult,
    ControllerCheckpoint, ControllerHooks, ControllerOptions, ControllerProposal,
    DiagnosticFeedback, EvaluatorKind, EvaluatorSpec, GateOutcome, InspectionFindings, Observation,
    ProgressEvent, RavoPhase, RavoPlan, RavoStopReason, SupervisorAdvice, SupervisorSignal,
};
use crate::authority::{
    failure_opponent_fingerprint, is_failure_opponent_id, normalize_assisted_ravo_state,
};
use crate::gate::{
    carry_observed_recurrences, parse_judge_verdict, ravo_fast_screen, set_stored_ravo_state,
    stored_ravo_state, without_observed_recurrences, RAVO_DEFAULT_CONFIG, RAVO_KEY,
    RAVO_SEED_CRITERIA,
};
use crate::js::{canonical_json, js_round, sha256_hex};
use crate::reducer::{
    ravo_extend_opponents, ravo_mark_provisional, ravo_step, GateStatus, RavoCriterionObservation,
    RavoEvaluation, RavoGateCertificate, RavoOpponentPool, RavoProposal, RavoState,
};
use crate::referee::{
    adjudicate_failure_claims, failure_opponent_passed, is_referee_opponent_id, referee_detail,
    referee_opponent_fingerprint, referee_opponent_id, referee_opponent_passed,
    referee_verdict_is_evidence, skill_imports_of, RefereeVerdict, RefereeVerdictStatus,
    ReplayRunner,
};
use crate::trust::{empty_entry_trust, TRUST_KEY};

/// The run's defaults (TS `RAVO_RUN_DEFAULTS`).
pub const RAVO_RUN_MAX_ROUNDS: u64 = 4;
pub const RAVO_RUN_MAX_REPAIRS: u64 = 3;
pub const RAVO_RUN_DEADLINE: Duration = Duration::from_mins(20);
pub const RAVO_RUN_TOKEN_BUDGET: u64 = 1_500_000;
const RESERVATION_PER_CALL: u64 = 60_000;

const CONTEXT_LIMITS: ContextViewLimits = ContextViewLimits {
    max_tokens: 24_000,
    max_bytes: 96_000,
    max_items: 16,
    lineage_depth: 3,
    max_artifact_bytes_per_item: 32_000,
};

const CHILD_RETRIES: u32 = 1;
const JSON_ONLY: &str =
    "Return exactly one JSON object and nothing else: no prose, no code fences.";

/// What a run is asked to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RavoRunRequest {
    pub task: String,
    pub instructions: Option<String>,
    pub global: bool,
    pub max_rounds: Option<u64>,
    pub max_repairs: Option<u64>,
    pub deadline_ms: Option<u64>,
    pub token_budget: Option<u64>,
}

/// One model reply to a child prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelReply {
    pub text: String,
    pub tokens: u64,
}

/// A child's model failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFailure {
    /// `error`, `aborted`, `budget_exceeded`.
    pub status: String,
    pub tokens: u64,
    pub error: Option<String>,
}

/// The model a run's children talk to: one provider call per prompt.
pub trait RavoModel: Send + Sync {
    /// One call; `token_budget` is what the child may still spend.
    fn complete(
        &self,
        prompt: String,
        token_budget: u64,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send>>;
}

/// A harness store a run reads and commits into.
#[derive(Debug, Clone)]
pub struct RunStores {
    pub local_dir: PathBuf,
    pub global_dir: PathBuf,
    /// The settings root the factory opt-in is read from.
    pub agent_dir: PathBuf,
}

impl RunStores {
    fn dir(&self, scope: HarnessScope) -> &PathBuf {
        match scope {
            HarnessScope::Local => &self.local_dir,
            HarnessScope::Global => &self.global_dir,
        }
    }
}

/// Told every status change.
pub type StatusListener = Arc<dyn Fn(&Value) + Send + Sync>;

/// The run service's collaborators.
#[derive(Clone)]
pub struct RunServiceDeps {
    pub model: Arc<dyn RavoModel>,
    pub runner: Arc<dyn ReplayRunner>,
    pub replay_sys_path: Vec<String>,
    pub stores: RunStores,
    pub on_update: StatusListener,
}

/// The status a run reports (TS `RavoRunStatus`), as its JSON.
#[derive(Debug, Clone, PartialEq)]
struct Status {
    run_id: String,
    phase: RavoPhase,
    round: u64,
    repairs: u64,
    last_event: Option<Value>,
    stop_reason: Option<RavoStopReason>,
    started_at: u64,
    updated_at: u64,
    candidate_id: Option<String>,
    last_certificate: Option<Value>,
    error: Option<String>,
}

impl Status {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("runId".into(), json!(self.run_id));
        map.insert("phase".into(), json!(self.phase));
        map.insert("round".into(), json!(self.round));
        map.insert("repairs".into(), json!(self.repairs));
        if let Some(event) = &self.last_event {
            map.insert("lastEvent".into(), event.clone());
        }
        if let Some(reason) = self.stop_reason {
            map.insert("stopReason".into(), json!(stop_name(reason)));
        }
        map.insert("startedAt".into(), json!(self.started_at));
        map.insert("updatedAt".into(), json!(self.updated_at));
        if let Some(id) = &self.candidate_id {
            map.insert("candidateId".into(), json!(id));
        }
        if let Some(certificate) = &self.last_certificate {
            map.insert("lastCertificate".into(), certificate.clone());
        }
        if let Some(error) = &self.error {
            map.insert("error".into(), json!(error));
        }
        Value::Object(map)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct Slot {
    status: Option<Status>,
    cancel: Option<CancellationToken>,
    running: bool,
}

/// One session's RAVO runs: a single slot.
#[derive(Clone)]
pub struct RavoRunService {
    deps: RunServiceDeps,
    slot: Arc<Mutex<Slot>>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

/// Why a run did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotStarted(pub String);

/// The certificate as the status reports it (TS `certificateSummary`).
#[must_use]
pub fn certificate_summary(certificate: &RavoGateCertificate) -> Value {
    let status = if certificate.committed {
        "commit"
    } else {
        match certificate.rejection {
            Some(crate::reducer::RavoRejection::Screen) => "reject_screen",
            Some(crate::reducer::RavoRejection::Deep) => "reject_deep",
            _ => "reject_criteria",
        }
    };
    let mut map = Map::new();
    map.insert("proposalId".into(), json!(certificate.proposal_id));
    map.insert("status".into(), json!(status));
    map.insert(
        "screenScore".into(),
        json!(certificate.screen.score.unwrap_or(0)),
    );
    if let Some(deep) = certificate.deep.score {
        map.insert("deepScore".into(), json!(deep));
    }
    map.insert("missed".into(), json!(certificate.missed_criterion_ids));
    Value::Object(map)
}

fn event_value(event: &ProgressEvent) -> Value {
    match event {
        ProgressEvent::Phase { phase, round } => {
            json!({ "type": "phase", "phase": phase, "round": round })
        }
        ProgressEvent::Proposal {
            proposal_id,
            parent_id,
            repair_of,
        } => json!({
            "type": "proposal", "proposalId": proposal_id, "parentId": parent_id, "repairOf": repair_of
        }),
        ProgressEvent::Evaluation {
            proposal_id,
            certificate,
        } => json!({ "type": "evaluation", "proposalId": proposal_id, "certificate": certificate }),
        ProgressEvent::Supervisor { intervened, detail } => {
            let mut value = json!({ "type": "supervisor", "intervened": intervened });
            if let Some(detail) = detail {
                value["detail"] = json!(detail);
            }
            value
        }
        ProgressEvent::Stopped { reason } => {
            json!({ "type": "stopped", "reason": stop_name(*reason) })
        }
    }
}

impl RavoRunService {
    /// A service with no run yet.
    #[must_use]
    pub fn new(deps: RunServiceDeps) -> Self {
        Self {
            deps,
            slot: Arc::default(),
            now: Arc::new(pa_ledger::now_millis),
        }
    }

    /// Whether a run is in progress.
    #[must_use]
    pub fn running(&self) -> bool {
        lock(&self.slot).running
    }

    /// The latest run's status (`None` before the first run).
    #[must_use]
    pub fn status(&self) -> Option<Value> {
        lock(&self.slot).status.as_ref().map(Status::to_value)
    }

    /// Ask the running run to stop; `false` when none runs.
    #[must_use]
    pub fn cancel(&self) -> bool {
        let slot = lock(&self.slot);
        match (&slot.cancel, slot.running) {
            (Some(cancel), true) => {
                cancel.cancel();
                true
            }
            _ => false,
        }
    }

    fn update(&self, patch: impl FnOnce(&mut Status), emit: bool) {
        let value = {
            let mut slot = lock(&self.slot);
            let Some(status) = slot.status.as_mut() else {
                return;
            };
            patch(status);
            status.updated_at = (self.now)();
            status.to_value()
        };
        if emit {
            (self.deps.on_update)(&value);
        }
    }

    /// Start a run in the background (on the current tokio runtime);
    /// answers its id at once. The returned handle settles with the final
    /// status.
    ///
    /// # Errors
    ///
    /// [`NotStarted`] when a run is already in progress or the task is
    /// empty.
    pub fn start(
        &self,
        request: RavoRunRequest,
    ) -> Result<(String, tokio::task::JoinHandle<Value>), NotStarted> {
        if request.task.trim().is_empty() {
            return Err(NotStarted("RAVO run requires a task".to_string()));
        }
        let now = (self.now)();
        let run_id = new_run_id(now);
        let cancel = CancellationToken::new();
        {
            let mut slot = lock(&self.slot);
            if slot.running {
                let current = slot.status.as_ref().map(|status| status.run_id.clone());
                return Err(NotStarted(current.map_or_else(
                    || "a RAVO run is already in progress".to_string(),
                    |id| format!("RAVO run {id} is already in progress"),
                )));
            }
            slot.running = true;
            slot.cancel = Some(cancel.clone());
            slot.status = Some(Status {
                run_id: run_id.clone(),
                phase: RavoPhase::Idle,
                round: 0,
                repairs: 0,
                last_event: None,
                stop_reason: None,
                started_at: now,
                updated_at: now,
                candidate_id: None,
                last_certificate: None,
                error: None,
            });
        }
        let service = self.clone();
        let id = run_id.clone();
        let handle = tokio::spawn(async move {
            let outcome = service.run(&id, &request, cancel).await;
            if let Err(error) = outcome {
                service.update(
                    |status| {
                        status.phase = RavoPhase::Stopped;
                        status.error = Some(error);
                    },
                    true,
                );
            }
            {
                let mut slot = lock(&service.slot);
                slot.running = false;
                slot.cancel = None;
            }
            service.status().unwrap_or(Value::Null)
        });
        Ok((run_id, handle))
    }

    #[allow(clippy::too_many_lines)]
    async fn run(
        &self,
        run_id: &str,
        request: &RavoRunRequest,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let scope = if request.global {
            HarnessScope::Global
        } else {
            HarnessScope::Local
        };
        let base_dir = self.deps.stores.dir(scope).clone();
        let runs_dir = base_dir.join("ravo").join("runs");
        let checkpoint_path = runs_dir.join(format!("{run_id}.json"));
        std::fs::create_dir_all(&runs_dir).map_err(|error| error.to_string())?;
        let state = load_harness_state(self.deps.stores.dir(scope), scope);
        let failures = state
            .extensions
            .get(pa_ledger::FAILURES_KEY)
            .map(pa_ledger::normalize_failure_ledger)
            .unwrap_or_default();
        let recurring = recurring_failures(&failures, None);
        let mut active_failure_ids: Vec<String> = Vec::new();
        let mut referee_ids: Vec<String> = Vec::new();
        for record in &recurring {
            let failure = failure_opponent_id(&record.fingerprint.id);
            if !active_failure_ids.contains(&failure) {
                active_failure_ids.push(failure);
            }
            if record.verified_replay_cases().last().is_some() {
                let referee = referee_opponent_id(&record.fingerprint.id);
                if !referee_ids.contains(&referee) {
                    referee_ids.push(referee);
                }
            }
        }
        let base_ravo = normalize_assisted_ravo_state(state.extensions.get(RAVO_KEY));
        let baseline_of = |ravo: &RavoState| {
            canonical_json(
                &serde_json::to_value(without_observed_recurrences(ravo)).unwrap_or(Value::Null),
            )
        };
        let base_baseline = baseline_of(&base_ravo);
        let mut extended = active_failure_ids.clone();
        extended.extend(referee_ids.iter().cloned());
        let initial_state = RavoState {
            opponents: ravo_extend_opponents(&base_ravo.opponents, &extended),
            ..base_ravo.clone()
        };
        let context_archive = build_context_archive(request, &state, &recurring, &initial_state);
        let context = build_bounded_context_view(&context_archive, &CONTEXT_LIMITS);
        let archive = archive::RavoArchive::new(&base_dir, "ravo/archive");
        let hooks = RunHooks {
            service: self.clone(),
            run_id: run_id.to_string(),
            scope,
            request: request.clone(),
            state: state.clone(),
            recurring: recurring.clone(),
            active_failure_ids,
            base_pool: base_ravo.opponents.clone(),
            base_baseline,
            initial_pool: initial_state.opponents.clone(),
            mirror: Mutex::new(initial_state.clone()),
            judged: Mutex::new(HashMap::new()),
            claims: Mutex::new(HashMap::new()),
            refereed: Mutex::new(HashMap::new()),
            counters: Mutex::new((0, 0)),
            rejected_since_commit: Mutex::new(0),
            checkpoint_path: checkpoint_path.clone(),
            cancel: cancel.clone(),
        };
        let token_budget = request.token_budget.unwrap_or(RAVO_RUN_TOKEN_BUDGET);
        let options = ControllerOptions {
            run_id: run_id.to_string(),
            context,
            initial_state,
            reducer_config: RAVO_DEFAULT_CONFIG,
            archive: &archive,
            error_budget: empty_error_budget(),
            max_rounds: request.max_rounds.unwrap_or(RAVO_RUN_MAX_ROUNDS),
            max_repairs: request.max_repairs.unwrap_or(RAVO_RUN_MAX_REPAIRS),
            deadline: request
                .deadline_ms
                .map_or(RAVO_RUN_DEADLINE, Duration::from_millis),
            token_budget,
            reservation_per_call: RESERVATION_PER_CALL.min(token_budget / 8).max(1),
            cancel: cancel.clone(),
        };
        let result = match run_ravo_controller(&options, &hooks).await {
            Ok(result) => result,
            Err(error) if !cancel.is_cancelled() => return Err(error),
            Err(_) => {
                self.update(
                    |status| {
                        status.phase = RavoPhase::Stopped;
                        status.stop_reason = Some(RavoStopReason::Cancelled);
                        status.last_event =
                            Some(json!({ "type": "stopped", "reason": "cancelled" }));
                    },
                    true,
                );
                return Ok(());
            }
        };
        if result.reason == RavoStopReason::Accepted {
            let _ = std::fs::remove_file(&checkpoint_path);
        }
        let reason = result.reason;
        let checkpoint = result.checkpoint;
        self.update(
            |status| {
                status.phase = if reason == RavoStopReason::Accepted {
                    RavoPhase::Accepted
                } else {
                    RavoPhase::Stopped
                };
                status.stop_reason = Some(reason);
                status.round = checkpoint.round;
                status.repairs = checkpoint.repairs;
                status.last_event = Some(json!({ "type": "stopped", "reason": stop_name(reason) }));
                if let Some(candidate) = &checkpoint.candidate {
                    status.candidate_id = Some(candidate.id.clone());
                }
            },
            true,
        );
        Ok(())
    }
}

fn new_run_id(now_millis: u64) -> String {
    let stamp: String = pa_ledger::iso_from_millis(now_millis)
        .chars()
        .filter(char::is_ascii_digit)
        .take(14)
        .collect();
    let nonce = sha256_hex(&format!(
        "{now_millis}:{}:{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    format!("ravo_{stamp}_{}", &nonce[..8])
}

/// The dormant entries leave the run's harness overview too (TS
/// `formatHarnessStateForPrompt`).
fn overview(state: &HarnessState) -> String {
    format_harness_state_for_prompt(
        state,
        &HarnessStatePromptOptions {
            include_ipython_examples: Some(false),
            include_shell_examples: false,
            adjustment: Some(crate::feature::dormant_adjustment(state)),
            ..HarnessStatePromptOptions::default()
        },
    )
}

fn summary_of(artifact: &Value) -> String {
    artifact
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("(no summary)")
        .to_string()
}

fn build_context_archive(
    request: &RavoRunRequest,
    state: &HarnessState,
    recurring: &[FailureRecord],
    ravo: &RavoState,
) -> ContextArchive {
    let scope_policy = if request.global {
        "Scope: global. Only stable cross-session lessons, durable user preferences, reusable skills/subagents, or project-qualified facts."
    } else {
        "Scope: local. Session progress and coordination facts for this session only. Record a transient condition (an open blocker, a pending rename, a service not yet registered) only together with how to re-check it, and update or delete it once it changes."
    };
    let constraint = |id: &str, text: String| ContextAtom {
        id: id.to_string(),
        kind: ContextAtomKind::Constraint,
        text,
    };
    let mut task = vec![request.task.trim().to_string()];
    if let Some(instructions) = &request.instructions {
        task.push(format!("Instructions: {}", instructions.trim()));
    }
    ContextArchive {
        current_task: ContextAtom {
            id: "task".to_string(),
            kind: ContextAtomKind::CurrentTask,
            text: task.join("\n"),
        },
        champion: ravo.lineage.last().map(|champion| ContextAtom {
            id: format!("champion:{}", champion.proposal_id),
            kind: ContextAtomKind::Champion,
            text: format!(
                "Current champion {} (score {}): {}",
                champion.proposal_id,
                champion.score,
                summary_of(&champion.artifact)
            ),
        }),
        constraints: vec![
            constraint("scope-policy", scope_policy.to_string()),
            constraint("harness-overview", overview(state)),
            constraint(
                "failure-ledger",
                format!(
                    "Recurring failures (opponent criteria failure:<fingerprint>):\n{}",
                    format_failure_ledger_for_prompt(recurring, 12)
                ),
            ),
        ],
    }
}

const PROPOSAL_SHAPE: &str = r#"{
  "summary": "one line",
  "rationale": "why, citing concrete evidence",
  "expectedOutcome": "one line",
  "addressedFingerprints": ["recurring failure fingerprint ids this proposal genuinely fixes"],
  "edits": [{ "action": "create|update|delete", "kind": "prompt|memory|skill|subagent", "id": "existing id (update/delete)", "title": "...", "content": "...", "path": "area/topic", "reference": { "type": "python", "import": "module", "callable": "fn" }, "arguments": {}, "reason": "..." }]
}
Rules: cite evidence for every edit; keep edits minimal and non-duplicating; skill edits need a python reference and arguments; never edit base_system_prompt; an empty edits list is a non-candidate."#;

const JUDGE_HEADER: &str = r#"# RAVO judge
You are the RAVO deep evaluator for Prime Agent's /refine subsystem. Score a proposed continual-harness refinement against the evidence. Judge the QUALITY OF THE RESULTING HARNESS STATE, not prose style.
Each criterion below is an opponent; list the ids the proposal fails in "failedCriteria". A recurring failure counts as addressed only if the edits would plausibly prevent that exact failure from recurring; never list a fingerprint the proposal merely mentions, and never one whose failure is outside the harness's control (a provider outage, a user denial, a flaky network). A fingerprint marked replay=verified is re-executed after you answer when a skill the proposal writes imports the module its replay case probes, so a claim on one whose failure has not actually stopped loses the gate.
"verdict" is your decision on the deep gate: "pass" if this candidate is at least as good a harness state as the current champion, "fail" if it is worse, "abstain" if the evidence given cannot decide. A non-pass verdict rejects the candidate whatever it scored."#;

fn format_feedback(feedback: &DiagnosticFeedback) -> String {
    let mut lines = vec![format!("rejection: {}", feedback.rejection)];
    for finding in &feedback.findings {
        let status = serde_json::to_value(finding.status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        lines.push(format!(
            "- {} [{status}]: {}",
            finding.source, finding.detail
        ));
    }
    lines.join("\n")
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// A judge call's memoized outcome: the verdict, or (status, error).
type JudgeOutcome = Result<JudgeVerdict, (String, String)>;

/// The judge's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JudgeVerdict {
    verdict: GateStatus,
    score: u64,
    failed_criteria: Vec<String>,
    addressed_fingerprints: Vec<String>,
    rationale: String,
}

fn validate_judge(value: &Value) -> Result<JudgeVerdict, String> {
    let record = object(value);
    let raw = match record.get("score") {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|score| score.is_finite())
    .ok_or_else(|| "judge output requires a numeric score".to_string())?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let score = js_round(raw).clamp(0.0, 100.0) as u64;
    let verdict_value = match record.get("verdict") {
        None | Some(Value::Null) => record.get("status"),
        some => some,
    };
    Ok(JudgeVerdict {
        verdict: parse_judge_verdict(verdict_value),
        score,
        failed_criteria: string_list(record.get("failedCriteria")),
        addressed_fingerprints: string_list(record.get("addressedFingerprints")),
        rationale: record
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// The implement/repair output as the run's artifact: the normalized
/// proposal plus `addressedFingerprints` (TS `validateArtifact`).
fn validate_artifact(value: &Value) -> Result<Value, String> {
    let record = object(value);
    if !record.get("edits").is_some_and(Value::is_array) {
        return Err("proposal output requires an edits array".to_string());
    }
    let proposal = normalize_refinement_proposal(value);
    let mut artifact = crate::gate::proposal_artifact(&proposal);
    if let Some(map) = artifact.as_object_mut() {
        map.insert(
            "addressedFingerprints".into(),
            json!(string_list(record.get("addressedFingerprints"))),
        );
    }
    Ok(artifact)
}

fn proposal_of(artifact: &Value) -> RefinementProposal {
    normalize_refinement_proposal(artifact)
}

fn addressed_fingerprints_of(artifact: &Value) -> Vec<String> {
    string_list(artifact.get("addressedFingerprints"))
}

/// The run's hooks into the controller.
struct RunHooks {
    service: RavoRunService,
    run_id: String,
    scope: HarnessScope,
    request: RavoRunRequest,
    /// The target store as the run read it.
    state: HarnessState,
    recurring: Vec<FailureRecord>,
    active_failure_ids: Vec<String>,
    base_pool: RavoOpponentPool,
    base_baseline: String,
    initial_pool: RavoOpponentPool,
    /// The controller's reducer state, replayed by the commit gate.
    mirror: Mutex<RavoState>,
    /// One judge call per proposal.
    judged: Mutex<HashMap<String, JudgeOutcome>>,
    /// The fingerprints the judge credited, per proposal.
    claims: Mutex<HashMap<String, Vec<String>>>,
    /// One referee pass per proposal.
    refereed: Mutex<HashMap<String, HashMap<String, RefereeVerdict>>>,
    /// (proposals, plans) minted.
    counters: Mutex<(u64, u64)>,
    rejected_since_commit: Mutex<u64>,
    checkpoint_path: PathBuf,
    cancel: CancellationToken,
}

impl RunHooks {
    /// Prompt the model and validate its strict-JSON reply, retrying once
    /// on an invalid reply; tokens are summed across attempts.
    async fn structured<T>(
        &self,
        prompt: String,
        validate: impl Fn(&Value) -> Result<T, String>,
        options: &ChildCallOptions,
    ) -> ChildResult<T> {
        let mut spent = 0u64;
        let mut attempt = 0;
        loop {
            let reply = self
                .service
                .deps
                .model
                .complete(prompt.clone(), options.token_budget, options.cancel.clone())
                .await;
            let (status, tokens, error) = match reply {
                Ok(reply) if spent + reply.tokens > options.token_budget => {
                    spent += reply.tokens;
                    ("budget_exceeded".to_string(), 0, None)
                }
                Ok(reply) => {
                    spent += reply.tokens;
                    let parsed = serde_json::from_str::<Value>(reply.text.trim())
                        .map_err(|error| format!("child output is not valid JSON: {error}"))
                        .and_then(|value| validate(&value));
                    match parsed {
                        Ok(value) => {
                            return ChildResult::Completed {
                                value,
                                tokens: spent,
                            }
                        }
                        Err(error) => ("error".to_string(), 0, Some(error)),
                    }
                }
                Err(failure) => {
                    spent += failure.tokens;
                    (failure.status, failure.tokens, failure.error)
                }
            };
            let _ = tokens;
            if status != "error" || attempt >= CHILD_RETRIES || options.cancel.is_cancelled() {
                return ChildResult::Failed {
                    status,
                    tokens: spent,
                    error,
                };
            }
            attempt += 1;
        }
    }

    fn next_proposal_id(&self) -> String {
        let mut counters = lock(&self.counters);
        counters.0 += 1;
        format!("{}-p{}", self.run_id, counters.0)
    }

    fn next_plan_id(&self) -> String {
        let mut counters = lock(&self.counters);
        counters.1 += 1;
        format!("{}-plan{}", self.run_id, counters.1)
    }

    fn proposal_prompt(
        &self,
        kind: &str,
        context: &BoundedContextView,
        inspection: Option<&InspectionFindings>,
        plan: &RavoPlan,
        candidate: Option<&ControllerProposal>,
        feedback: Option<&DiagnosticFeedback>,
    ) -> String {
        let mut parts = vec![
            format!("# RAVO {kind}"),
            "Everything you need is in this message. Do not search, browse, or call tools; write the answer directly.".to_string(),
            if kind == "implement" {
                "Produce the refinement proposal that executes the plan.".to_string()
            } else {
                "Repair the rejected proposal so every finding below is resolved. Keep what was right; change only what the findings require.".to_string()
            },
            render_context(context),
        ];
        if let Some(inspection) = inspection {
            parts.push(format!(
                "<inspection>\n{}\n</inspection>",
                inspection.summary
            ));
        }
        let mut steps: Vec<String> = plan.steps.iter().map(|step| format!("- {step}")).collect();
        if let Some(advice) = &plan.supervisor_advice {
            steps.push(format!("Supervisor: {advice}"));
        }
        parts.push(format!("<plan>\n{}\n</plan>", steps.join("\n")));
        if let Some(candidate) = candidate {
            parts.push(format!(
                "<rejected_proposal>\n{}\n</rejected_proposal>",
                serde_json::to_string(&candidate.artifact).unwrap_or_default()
            ));
        }
        if let Some(feedback) = feedback {
            parts.push(format!(
                "<rejection>\n{}\n</rejection>",
                format_feedback(feedback)
            ));
        }
        let fingerprints: Vec<&str> = self
            .recurring
            .iter()
            .map(|record| record.fingerprint.id.as_str())
            .collect();
        if !fingerprints.is_empty() {
            parts.push(format!(
                "Recurring failure fingerprints that must be addressed (list the ones you fix in addressedFingerprints): {}",
                fingerprints.join(", ")
            ));
        }
        parts.push(format!(
            "Return JSON with this shape:\n{PROPOSAL_SHAPE}\n{JSON_ONLY}"
        ));
        parts.join("\n\n")
    }

    fn judge_prompt(&self, proposal: &ControllerProposal, context: &BoundedContextView) -> String {
        let criteria: Vec<String> = RAVO_SEED_CRITERIA
            .iter()
            .map(|(id, description)| format!("- {id}: {description}"))
            .collect();
        let mut parts = vec![
            JUDGE_HEADER.to_string(),
            format!("<criteria>\n{}\n</criteria>", criteria.join("\n")),
        ];
        if !self.recurring.is_empty() {
            parts.push(format!(
                "<recurring_failures>\n{}\n</recurring_failures>",
                format_failure_ledger_for_prompt(&self.recurring, 12)
            ));
        }
        parts.push(format!(
            "<current_harness_state>\n{}\n</current_harness_state>",
            overview(&self.state)
        ));
        parts.push(render_context(context));
        parts.push(format!(
            "<proposal>\n{}\n</proposal>",
            serde_json::to_string_pretty(&proposal.artifact).unwrap_or_default()
        ));
        parts.push(format!("Return JSON: {{ \"verdict\": \"pass\"|\"fail\"|\"abstain\", \"score\": 0-100, \"failedCriteria\": [\"id\"], \"addressedFingerprints\": [\"fingerprint id\"], \"rationale\": \"one or two sentences\" }}. {JSON_ONLY}"));
        parts.join("\n\n")
    }

    /// The judge's verdict on `proposal`, called once; later readers spend
    /// no tokens.
    async fn judge(
        &self,
        proposal: &ControllerProposal,
        context: &BoundedContextView,
        options: &ChildCallOptions,
    ) -> ChildResult<JudgeVerdict> {
        let cached = lock(&self.judged).get(&proposal.id).cloned();
        if let Some(cached) = cached {
            return match cached {
                Ok(value) => ChildResult::Completed { value, tokens: 0 },
                Err((status, error)) => ChildResult::Failed {
                    status,
                    tokens: 0,
                    error: Some(error),
                },
            };
        }
        let result = self
            .structured(
                self.judge_prompt(proposal, context),
                validate_judge,
                options,
            )
            .await;
        let stored = match &result {
            ChildResult::Completed { value, .. } => {
                let claims = judged_claims(
                    &proposal.artifact,
                    &self
                        .recurring
                        .iter()
                        .map(|record| record.fingerprint.id.clone())
                        .collect::<Vec<_>>(),
                    &value.addressed_fingerprints,
                );
                lock(&self.claims).insert(proposal.id.clone(), claims);
                Ok(value.clone())
            }
            ChildResult::Failed { status, error, .. } => {
                Err((status.clone(), error.clone().unwrap_or_default()))
            }
        };
        lock(&self.judged).insert(proposal.id.clone(), stored);
        result
    }

    /// One referee pass per proposal, shared by the paired opponents and
    /// the commit gate.
    async fn referee(&self, proposal: &ControllerProposal) -> HashMap<String, RefereeVerdict> {
        if let Some(cached) = lock(&self.refereed).get(&proposal.id).cloned() {
            return cached;
        }
        let refinement = proposal_of(&proposal.artifact);
        let verdicts = adjudicate_failure_claims(
            &self.recurring,
            &addressed_fingerprints_of(&proposal.artifact),
            &skill_imports_of(&refinement.edits),
            &self.service.deps.replay_sys_path,
            self.service.deps.runner.as_ref(),
        )
        .await;
        let verdicts: HashMap<String, RefereeVerdict> = verdicts
            .into_iter()
            .map(|verdict| (verdict.fingerprint_id.clone(), verdict))
            .collect();
        lock(&self.refereed).insert(proposal.id.clone(), verdicts.clone());
        verdicts
    }

    fn log_outcome(
        &self,
        proposal_id: &str,
        decision: &str,
        certificate: &RavoGateCertificate,
        cause: &str,
    ) {
        let claimed = lock(&self.claims)
            .get(proposal_id)
            .cloned()
            .unwrap_or_default();
        let addressed = if decision == "commit" {
            credited_claims(&claimed, certificate)
        } else {
            claimed.clone()
        };
        let scope = crate::gate::scope_name(self.scope);
        let deep_score = certificate.deep.score.unwrap_or(0);
        let missed = certificate.missed_criterion_ids.len();
        let addressed = addressed.join(",");
        if decision == "commit" {
            tracing::info!(target: crate::feature::REFINEMENT_LOG_TARGET, proposal_id, addressed, deep_score, missed, reason = "ravo_run", scope, "refinement.committed");
        } else {
            tracing::info!(target: crate::feature::REFINEMENT_LOG_TARGET, proposal_id, decision, deep_score, missed, claimed = claimed.len(), reason = "ravo_run", scope, cause, "refinement.rejected");
        }
    }

    fn commit_inputs(
        &self,
        proposal: &ControllerProposal,
        stepped: &RavoState,
        verdicts: HashMap<String, RefereeVerdict>,
    ) -> CommitInputs {
        CommitInputs {
            stores: self.service.deps.stores.clone(),
            scope: self.scope,
            state: self.state.clone(),
            base_pool: self.base_pool.clone(),
            base_baseline: self.base_baseline.clone(),
            active_failure_ids: self.active_failure_ids.clone(),
            proposal: proposal.clone(),
            stepped: stepped.clone(),
            verdicts,
        }
    }
}

/// What the commit gate's blocking write needs.
struct CommitInputs {
    stores: RunStores,
    scope: HarnessScope,
    state: HarnessState,
    base_pool: RavoOpponentPool,
    base_baseline: String,
    active_failure_ids: Vec<String>,
    proposal: ControllerProposal,
    stepped: RavoState,
    verdicts: HashMap<String, RefereeVerdict>,
}

/// Apply the committed proposal and persist the stepped state, under
/// the target store's lock (blocking).
// One read-modify-write under one lock: splitting it would spread the
// window between the read and the save.
#[allow(clippy::too_many_lines)]
fn commit(inputs: &CommitInputs) -> Result<CommitOutcome, String> {
    let CommitInputs {
        stores,
        proposal,
        stepped,
        verdicts,
        ..
    } = inputs;
    let dir = stores.dir(inputs.scope).clone();
    let _guard = match inputs.scope {
        HarnessScope::Global => {
            Some(pa_ledger::acquire_harness_state_lock(&dir).map_err(|error| error.to_string())?)
        }
        HarnessScope::Local => None,
    };
    let mut current = load_harness_state(&dir, inputs.scope);
    let stored = normalize_assisted_ravo_state(current.extensions.get(RAVO_KEY));
    let baseline = canonical_json(
        &serde_json::to_value(without_observed_recurrences(&stored)).unwrap_or(Value::Null),
    );
    if baseline != inputs.base_baseline {
        return Ok(CommitOutcome::Stale);
    }
    let refinement = proposal_of(&proposal.artifact);
    let mut result = apply_refinement_proposal(
        &mut current,
        &refinement,
        ApplyOptions {
            id: proposal.id.clone(),
            rollback_of: None,
            scope: Some(inputs.scope),
            baseline_state: Some(inputs.state.clone()),
            factory_enabled: pa_core::refinement::factory_enabled(&stores.agent_dir),
        },
    );
    let failed: Vec<String> = result
        .applied_edits
        .iter()
        .filter(|edit| !edit.applied)
        .map(|edit| {
            format!(
                "{} {}:{}: {}",
                serde_json::to_value(edit.action)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
                serde_json::to_value(edit.kind)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default(),
                edit.id,
                edit.error.as_deref().unwrap_or("not applied")
            )
        })
        .collect();
    if !failed.is_empty() {
        return Ok(CommitOutcome::Failed(failed.join("; ")));
    }
    // Every entry an apply writes carries a trust record (TS
    // `applyRefinementProposal`).
    for edit in &mut result.applied_edits {
        if edit.action == RefinementAction::Delete {
            continue;
        }
        if let Some(entry) = current
            .entries
            .get_mut(&edit.kind)
            .and_then(|records| records.get_mut(&edit.id))
        {
            let trust =
                serde_json::to_value(empty_entry_trust(&entry.updated_at)).unwrap_or(Value::Null);
            entry
                .extensions
                .entry(TRUST_KEY.to_string())
                .or_insert(trust);
        }
    }
    // A referee opponent this run added is persisted only when a
    // replay adjudicated it.
    let persisted: Vec<&str> = inputs
        .base_pool
        .criteria
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    let pool = RavoOpponentPool {
        criteria: stepped
            .opponents
            .criteria
            .iter()
            .filter(|criterion| {
                !is_referee_opponent_id(&criterion.id)
                    || persisted.contains(&criterion.id.as_str())
                    || referee_verdict_is_evidence(
                        referee_opponent_fingerprint(&criterion.id)
                            .and_then(|fingerprint| verdicts.get(fingerprint)),
                    )
            })
            .cloned()
            .collect(),
    };
    let claimed: Vec<String> = addressed_fingerprints_of(&proposal.artifact)
        .into_iter()
        .filter(|fingerprint| {
            inputs
                .active_failure_ids
                .contains(&failure_opponent_id(fingerprint))
        })
        .collect();
    let marked = ravo_mark_provisional(
        &RavoState {
            opponents: pool,
            ..stepped.clone()
        },
        &proposal.id,
        &claimed,
        None,
    );
    let next = carry_observed_recurrences(&marked, stored_ravo_state(&current).as_ref());
    set_stored_ravo_state(&mut current, &next);
    save_harness_state(&dir, &current).map_err(|error| error.to_string())?;
    Ok(CommitOutcome::Applied(result.applied_edits.len()))
}

enum CommitOutcome {
    Stale,
    Failed(String),
    Applied(usize),
}

/// The fingerprints a `ravo.run` proposal is logged as claiming: claimed by
/// the proposal, recurring in this run's ledger, and named by the judge.
fn judged_claims(
    artifact: &Value,
    recurring: &[String],
    judge_addressed: &[String],
) -> Vec<String> {
    let judged: Vec<&str> = judge_addressed
        .iter()
        .map(|id| failure_opponent_fingerprint(id).unwrap_or(id))
        .collect();
    crate::js::sorted_unique(
        addressed_fingerprints_of(artifact)
            .into_iter()
            .filter(|id| recurring.contains(id) && judged.contains(&id.as_str())),
    )
}

/// The claims a certificate credited.
fn credited_claims(claimed: &[String], certificate: &RavoGateCertificate) -> Vec<String> {
    claimed
        .iter()
        .filter(|id| {
            !certificate
                .missed_criterion_ids
                .contains(&failure_opponent_id(id))
                && !certificate
                    .missed_criterion_ids
                    .contains(&referee_opponent_id(id))
        })
        .cloned()
        .collect()
}

fn evaluation_from_certificate(certificate: &RavoGateCertificate) -> RavoEvaluation {
    RavoEvaluation {
        proposal_id: certificate.proposal_id.clone(),
        screen: certificate.screen.clone(),
        deep: certificate.deep.clone(),
        criteria: certificate
            .criteria
            .iter()
            .map(|item| RavoCriterionObservation {
                criterion_id: item.criterion_id.clone(),
                status: item.status,
                detail: item.detail.clone(),
            })
            .collect(),
    }
}

fn decision_name(certificate: &RavoGateCertificate) -> &'static str {
    certificate_summary(certificate)
        .get("status")
        .and_then(Value::as_str)
        .map_or("reject_criteria", |status| match status {
            "commit" => "commit",
            "reject_screen" => "reject_screen",
            "reject_deep" => "reject_deep",
            _ => "reject_criteria",
        })
}

fn rejection_cause(decision: &str, certificate: &RavoGateCertificate) -> &'static str {
    if decision == "reject_screen" {
        "screen"
    } else if certificate.deep.status == GateStatus::Error {
        "judge_unavailable"
    } else {
        "gate"
    }
}

fn completed<T>(value: T) -> ChildResult<T> {
    ChildResult::Completed { value, tokens: 0 }
}

fn pass_or_fail(passed: bool) -> GateStatus {
    if passed {
        GateStatus::Pass
    } else {
        GateStatus::Fail
    }
}

impl ControllerHooks for RunHooks {
    fn inspect<'a>(
        &'a self,
        context: &'a BoundedContextView,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, InspectionFindings> {
        let prompt = [
            "# RAVO inspect".to_string(),
            "Inspect the continual harness state and failure ledger below. Report what relates to the task: relevant entries, gaps, duplicates, and recurring failures.".to_string(),
            render_context(context),
            format!("Return JSON: {{ \"summary\": \"one paragraph\", \"facts\": [\"short concrete fact\", ...] }}. {JSON_ONLY}"),
        ]
        .join("\n\n");
        Box::pin(async move {
            self.structured(
                prompt,
                |value| {
                    let record = object(value);
                    let summary = record
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or_default()
                        .to_string();
                    if summary.is_empty() {
                        return Err("inspect output requires a summary".to_string());
                    }
                    Ok(InspectionFindings {
                        summary,
                        facts: string_list(record.get("facts")),
                    })
                },
                &options,
            )
            .await
        })
    }

    fn plan<'a>(
        &'a self,
        context: &'a BoundedContextView,
        inspection: &'a InspectionFindings,
        feedback: Option<&'a DiagnosticFeedback>,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, RavoPlan> {
        let facts: Vec<String> = inspection
            .facts
            .iter()
            .map(|fact| format!("- {fact}"))
            .collect();
        let mut parts = vec![
            "# RAVO plan".to_string(),
            "Plan a minimal continual-harness refinement for the task. Each step names the edit (action, kind, id) and the evidence for it.".to_string(),
            render_context(context),
            format!("<inspection>\n{}\n{}\n</inspection>", inspection.summary, facts.join("\n")),
        ];
        if let Some(feedback) = feedback {
            parts.push(format!(
                "<rejection>\n{}\n</rejection>\nThe plan must fix every finding above.",
                format_feedback(feedback)
            ));
        }
        parts.push(format!(
            "Return JSON: {{ \"steps\": [\"step\", ...] }}. {JSON_ONLY}"
        ));
        let prompt = parts.join("\n\n");
        Box::pin(async move {
            self.structured(
                prompt,
                |value| {
                    Ok(RavoPlan {
                        id: String::new(),
                        steps: string_list(object(value).get("steps")),
                        supervisor_advice: None,
                    })
                },
                &options,
            )
            .await
            .map_plan(|plan| RavoPlan {
                id: self.next_plan_id(),
                ..plan
            })
        })
    }

    fn implement<'a>(
        &'a self,
        context: &'a BoundedContextView,
        inspection: &'a InspectionFindings,
        plan: &'a RavoPlan,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, ControllerProposal> {
        let prompt = self.proposal_prompt("implement", context, Some(inspection), plan, None, None);
        Box::pin(async move {
            self.structured(prompt, validate_artifact, &options)
                .await
                .map_plan(|artifact| ControllerProposal {
                    id: self.next_proposal_id(),
                    parent_id: None,
                    repair_of: None,
                    artifact,
                })
        })
    }

    fn repair<'a>(
        &'a self,
        context: &'a BoundedContextView,
        candidate: &'a ControllerProposal,
        feedback: &'a DiagnosticFeedback,
        plan: &'a RavoPlan,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, ControllerProposal> {
        let prompt = self.proposal_prompt(
            "repair",
            context,
            None,
            plan,
            Some(candidate),
            Some(feedback),
        );
        Box::pin(async move {
            self.structured(prompt, validate_artifact, &options)
                .await
                .map_plan(|artifact| ControllerProposal {
                    id: self.next_proposal_id(),
                    parent_id: Some(candidate.id.clone()),
                    repair_of: Some(candidate.id.clone()),
                    artifact,
                })
        })
    }

    fn evaluators(&self) -> Vec<EvaluatorSpec> {
        let spec = |id: String, kind, criterion: Option<String>| EvaluatorSpec {
            id,
            kind,
            criterion_id: criterion,
        };
        let mut evaluators = vec![
            spec(
                "fast:structural-dry-run".to_string(),
                EvaluatorKind::Fast,
                None,
            ),
            spec("deep:judge".to_string(), EvaluatorKind::Deep, None),
        ];
        let mut observed: Vec<String> = Vec::new();
        for (id, _) in RAVO_SEED_CRITERIA {
            evaluators.push(spec(
                format!("opponent:{id}"),
                EvaluatorKind::Opponent,
                Some(id.to_string()),
            ));
            observed.push(id.to_string());
        }
        for criterion in &self.initial_pool.criteria {
            if is_failure_opponent_id(&criterion.id) || is_referee_opponent_id(&criterion.id) {
                evaluators.push(spec(
                    format!("opponent:{}", criterion.id),
                    EvaluatorKind::Opponent,
                    Some(criterion.id.clone()),
                ));
                observed.push(criterion.id.clone());
            }
        }
        for criterion in &self.initial_pool.criteria {
            if !observed.contains(&criterion.id) {
                evaluators.push(spec(
                    format!("opponent:{}", criterion.id),
                    EvaluatorKind::Opponent,
                    Some(criterion.id.clone()),
                ));
            }
        }
        evaluators
    }

    fn evaluate<'a>(
        &'a self,
        evaluator: &'a EvaluatorSpec,
        proposal: &'a ControllerProposal,
        context: &'a BoundedContextView,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, Observation> {
        Box::pin(async move {
            match evaluator.kind {
                EvaluatorKind::Fast => {
                    let refinement = proposal_of(&proposal.artifact);
                    let valid = count_valid_refinement_edits(&refinement);
                    let score = ravo_fast_screen(&refinement, valid);
                    let invalid = refinement.edits.len() - valid;
                    let mut detail = format!(
                        "{valid}/{} edits pass the structural screen and dry-run",
                        refinement.edits.len()
                    );
                    if invalid > 0 {
                        detail = format!("{detail}; {invalid} structurally invalid");
                    }
                    completed(Observation {
                        status: pass_or_fail(score >= RAVO_DEFAULT_CONFIG.screen_threshold),
                        score: Some(score),
                        detail: Some(detail),
                    })
                }
                EvaluatorKind::Deep => {
                    self.judge(proposal, context, &options)
                        .await
                        .map_plan(|judged| Observation {
                            status: judged.verdict,
                            score: Some(judged.score),
                            detail: Some(judged.rationale),
                        })
                }
                EvaluatorKind::Opponent => {
                    let criterion = evaluator.criterion_id.clone().unwrap_or_default();
                    if RAVO_SEED_CRITERIA.iter().any(|(id, _)| *id == criterion) {
                        return self
                            .judge(proposal, context, &options)
                            .await
                            .map_plan(|judged| Observation {
                                status: pass_or_fail(!judged.failed_criteria.contains(&criterion)),
                                score: None,
                                detail: Some(judged.rationale),
                            });
                    }
                    if let Some(fingerprint) = failure_opponent_fingerprint(&criterion) {
                        if !self.active_failure_ids.contains(&criterion) {
                            return completed(Observation {
                                status: GateStatus::Pass,
                                score: None,
                                detail: Some(
                                    "dormant: fingerprint is not currently recurring".to_string(),
                                ),
                            });
                        }
                        let claimed = addressed_fingerprints_of(&proposal.artifact)
                            .iter()
                            .any(|id| id == fingerprint);
                        let verdicts = self.referee(proposal).await;
                        let verdict = verdicts.get(fingerprint);
                        let detail = match verdict {
                            Some(verdict) if verdict.status != RefereeVerdictStatus::NotApplicable => {
                                format!("{fingerprint}: {}", verdict.detail)
                            }
                            _ if claimed => format!("proposal claims to address {fingerprint}"),
                            _ => format!("recurring failure {fingerprint} is not addressed (set addressedFingerprints)"),
                        };
                        return completed(Observation {
                            status: pass_or_fail(failure_opponent_passed(claimed, verdict)),
                            score: None,
                            detail: Some(detail),
                        });
                    }
                    if let Some(fingerprint) = referee_opponent_fingerprint(&criterion) {
                        let claimed = addressed_fingerprints_of(&proposal.artifact)
                            .iter()
                            .any(|id| id == fingerprint);
                        let verdicts = self.referee(proposal).await;
                        let verdict = verdicts.get(fingerprint);
                        return completed(Observation {
                            status: pass_or_fail(referee_opponent_passed(claimed, verdict)),
                            score: None,
                            detail: Some(referee_detail(verdict, claimed)),
                        });
                    }
                    completed(Observation {
                        status: GateStatus::Pass,
                        score: None,
                        detail: Some("dormant: not evaluated by this run".to_string()),
                    })
                }
            }
        })
    }

    fn should_consult_supervisor(&self, _signal: &SupervisorSignal) -> bool {
        *lock(&self.rejected_since_commit) >= 2
    }

    fn supervisor<'a>(
        &'a self,
        signal: &'a SupervisorSignal,
        options: ChildCallOptions,
    ) -> ChildFuture<'a, SupervisorAdvice> {
        let steps: Vec<String> = signal
            .plan
            .steps
            .iter()
            .map(|step| format!("- {step}"))
            .collect();
        let trajectory: Vec<String> = signal
            .trajectory
            .iter()
            .map(|certificate| {
                if certificate.committed {
                    format!("{}: commit", certificate.proposal_id)
                } else {
                    let rejection = certificate
                        .rejection
                        .and_then(|rejection| serde_json::to_value(rejection).ok())
                        .and_then(|value| value.as_str().map(str::to_string))
                        .unwrap_or_else(|| "gate".to_string());
                    format!(
                        "{}: rejected ({rejection}) missed={}",
                        certificate.proposal_id,
                        certificate.missed_criterion_ids.join(",")
                    )
                }
            })
            .collect();
        let prompt = [
            "# RAVO supervisor".to_string(),
            "Several proposals were rejected. Decide whether the plan needs redirecting and give one concrete piece of advice if so.".to_string(),
            format!("<plan>\n{}\n</plan>", steps.join("\n")),
            format!("<trajectory>\n{}\n</trajectory>", trajectory.join("\n")),
            format!("Return JSON: {{ \"intervene\": true|false, \"advice\": \"one or two sentences\" }}. {JSON_ONLY}"),
        ]
        .join("\n\n");
        Box::pin(async move {
            self.structured(
                prompt,
                |value| {
                    let record = object(value);
                    Ok(SupervisorAdvice {
                        intervene: record.get("intervene") == Some(&Value::Bool(true)),
                        advice: record
                            .get("advice")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|advice| !advice.is_empty())
                            .map(str::to_string),
                    })
                },
                &options,
            )
            .await
        })
    }

    fn commit_gate<'a>(
        &'a self,
        proposal: &'a ControllerProposal,
        certificate: &'a RavoGateCertificate,
    ) -> Pin<Box<dyn Future<Output = GateOutcome> + Send + 'a>> {
        Box::pin(async move {
            let mirror = lock(&self.mirror).clone();
            let stepped = ravo_step(
                &mirror,
                &RavoProposal {
                    id: proposal.id.clone(),
                    artifact: proposal.artifact.clone(),
                },
                &evaluation_from_certificate(certificate),
                &RAVO_DEFAULT_CONFIG,
            );
            if !stepped.certificate.committed {
                let decision = decision_name(&stepped.certificate);
                self.log_outcome(
                    &proposal.id,
                    decision,
                    &stepped.certificate,
                    rejection_cause(decision, &stepped.certificate),
                );
                return GateOutcome {
                    accepted: false,
                    detail: Some("reducer replay rejected".to_string()),
                    stale: false,
                };
            }
            let verdicts = self.referee(proposal).await;
            let inputs = self.commit_inputs(proposal, &stepped.state, verdicts);
            let committed = tokio::task::spawn_blocking(move || commit(&inputs))
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            match committed {
                Ok(CommitOutcome::Stale) => {
                    self.log_outcome(
                        &proposal.id,
                        "reject_deep",
                        &stepped.certificate,
                        "baseline_changed",
                    );
                    GateOutcome {
                        accepted: false,
                        detail: Some("the stored RAVO state changed during the run".to_string()),
                        stale: true,
                    }
                }
                Ok(CommitOutcome::Failed(detail)) | Err(detail) => {
                    self.log_outcome(&proposal.id, "partial", &stepped.certificate, "gate");
                    GateOutcome {
                        accepted: false,
                        detail: Some(detail),
                        stale: false,
                    }
                }
                Ok(CommitOutcome::Applied(edits)) => {
                    *lock(&self.mirror) = stepped.state;
                    self.log_outcome(&proposal.id, "commit", &stepped.certificate, "none");
                    GateOutcome {
                        accepted: true,
                        detail: Some(format!("applied {edits} edits")),
                        stale: false,
                    }
                }
            }
        })
    }

    fn on_progress(&self, event: &ProgressEvent) {
        if let ProgressEvent::Evaluation {
            proposal_id,
            certificate,
        } = event
        {
            if !certificate.committed {
                *lock(&self.rejected_since_commit) += 1;
                lock(&self.mirror)
                    .evaluated_proposal_ids
                    .push(proposal_id.clone());
                let decision = decision_name(certificate);
                self.log_outcome(
                    proposal_id,
                    decision,
                    certificate,
                    rejection_cause(decision, certificate),
                );
            }
        }
        let value = event_value(event);
        if let ProgressEvent::Stopped { .. } = event {
            self.service
                .update(|status| status.last_event = Some(value), false);
            return;
        }
        self.service.update(
            |status| {
                match event {
                    ProgressEvent::Phase { phase, round } => {
                        if *phase == RavoPhase::Diagnose {
                            status.repairs += 1;
                        }
                        status.phase = *phase;
                        status.round = *round;
                    }
                    ProgressEvent::Proposal { proposal_id, .. } => {
                        status.candidate_id = Some(proposal_id.clone());
                    }
                    ProgressEvent::Evaluation { certificate, .. } => {
                        status.last_certificate = Some(certificate_summary(certificate));
                    }
                    ProgressEvent::Supervisor { .. } | ProgressEvent::Stopped { .. } => {}
                }
                status.last_event = Some(value);
            },
            true,
        );
    }

    fn on_checkpoint(&self, checkpoint: &ControllerCheckpoint) {
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(checkpoint).unwrap_or_default()
        );
        if let Err(error) = std::fs::write(&self.checkpoint_path, text) {
            tracing::debug!(%error, "RAVO checkpoint not written");
        }
        let _ = (&self.request, &self.cancel);
    }
}

/// Map a completed child's value.
trait MapPlan<T> {
    fn map_plan<U>(self, map: impl FnOnce(T) -> U) -> ChildResult<U>;
}

impl<T> MapPlan<T> for ChildResult<T> {
    fn map_plan<U>(self, map: impl FnOnce(T) -> U) -> ChildResult<U> {
        match self {
            ChildResult::Completed { value, tokens } => ChildResult::Completed {
                value: map(value),
                tokens,
            },
            ChildResult::Failed {
                status,
                tokens,
                error,
            } => ChildResult::Failed {
                status,
                tokens,
                error,
            },
        }
    }
}

/// Parse a `ravo.run` payload (TS `parseRavoRunPayload`).
///
/// # Errors
///
/// The TS messages for a missing task and malformed fields; `arc_agi` is
/// refused: the ARC-AGI evaluator is benchmark code outside the product.
pub fn parse_ravo_run_payload(payload: &Value) -> Result<RavoRunRequest, String> {
    let task = payload
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| "ravo.run task must be a non-empty string".to_string())?;
    let instructions = match payload.get("instructions") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Some(_) => return Err("ravo.run instructions must be a string when provided".to_string()),
    };
    let global = match payload.get("global") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("ravo.run global must be a boolean when provided".to_string()),
    };
    let positive = |key: &str| -> Result<Option<u64>, String> {
        match payload.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|value| *value >= 1)
                .map(Some)
                .ok_or_else(|| format!("ravo.run {key} must be a positive integer when provided")),
        }
    };
    if !matches!(payload.get("arc_agi"), None | Some(Value::Null)) {
        return Err(
            "ravo.run arc_agi is not available: the ARC-AGI evaluator is not part of this build"
                .to_string(),
        );
    }
    Ok(RavoRunRequest {
        task: task.to_string(),
        instructions,
        global,
        max_rounds: positive("max_rounds")?,
        max_repairs: positive("max_repairs")?,
        deadline_ms: positive("deadline_ms")?,
        token_budget: positive("token_budget")?,
    })
}
