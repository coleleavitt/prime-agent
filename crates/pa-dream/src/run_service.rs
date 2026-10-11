//! `DreamRunService`: the in-session, background wrapper around the agent
//! loop and the experiment runner (TS `run-service.ts`).
//!
//! It owns a single run slot (a run or an experiment), runs it on its own
//! thread, relays a cancel into it, and publishes a [`DreamRunStatus`]
//! snapshot through `on_update` at each phase boundary, so the kernel skill
//! (`dream.status`), the `/dream` command and any session-event surface
//! observe the same run. It opens no span of its own: the loop's `dream.run`
//! and the experiment's `dream.experiment` are detached roots carrying the
//! launching turn's trace id, which `start` captures on the caller's thread.
//!
//! With `llm_proposer` / `llm_dreamer` off (the default) nothing calls the
//! runner and nothing spends a token.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::child::{ChildRuntimeScope, RunAgent, RunAgentOptions, RunAgentRequest, RunAgentResult};
use crate::experiment::{
    EXPERIMENT_ARMS,
    ExperimentArm,
    ExperimentArmRunner,
    ExperimentBudget,
    ExperimentError,
    ExperimentHooks,
    ExperimentProgressEvent,
    ExperimentRunOptions,
    ExperimentSpec,
    LOCAL_EXPERIMENT_ARMS,
    LocalArmRunner,
    experiment_id_for,
    run_experiment_with_hooks,
};
use crate::experiment_llm::{AgentArmRunner, AgentArmRunnerOptions, assert_guided_arms_served};
use crate::llm::DreamChildRole;
use crate::llm_loop::{DreamLoopWithAgentOptions, DreamProgressEvent, run_dream_loop_with_agent};
use crate::objective::DEFAULT_OBJECTIVE;
use crate::policy::{DEFAULT_POLICY, ExplorationPolicy};
use crate::rng::{Seed, SeededRng};
use crate::store::{DreamStoreError, experiment_result_path};
use crate::tasks::{DreamTaskId, resolve_task, resolve_task_n, task_prompt_context};

/// Why a run ended. `Completed` covers improved and no-improvement finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DreamStopReason {
    Completed,
    Cancelled,
    Error,
}

impl DreamStopReason {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

/// A run's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DreamRunPhase {
    Idle,
    Rollout,
    Dreaming,
    Redeploying,
    Accepted,
    Stopped,
}

/// Whether the slot holds a run or an experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DreamRunKind {
    Run,
    Experiment,
}

/// A snapshot of one run (TS `DreamRunStatus`). Every field but
/// `result_paths` is a scalar; the experiment and seed fields are optional
/// and additive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamRunStatus {
    pub run_id: String,
    pub phase: DreamRunPhase,
    #[serde(serialize_with = "task_name", deserialize_with = "task_of")]
    pub task: DreamTaskId,
    pub iteration: u32,
    pub best_node_score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_policy_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub improved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<DreamStopReason>,
    pub started_at: u64,
    pub updated_at: u64,
    /// Set when the run ended with an unexpected error instead of a clean stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub kind: DreamRunKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<ExperimentArm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm_count: Option<usize>,
    /// The current arm's round (1-based) and the rounds per arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_probes: Option<u32>,
    /// Where the LAST completed seed's `result.json` landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_count: Option<usize>,
    /// Every completed seed's `result.json`, in run order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_paths: Option<Vec<String>>,
    /// Token total of the last COMPLETED seed (all arms), on the LLM path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `serialize_with` signature
fn task_name<S: serde::Serializer>(task: &DreamTaskId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(task.as_str())
}

fn task_of<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<DreamTaskId, D::Error> {
    let name = String::deserialize(deserializer)?;
    DreamTaskId::from_name(&name)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown dream task {name}")))
}

/// A status read back from its JSON (a `dream.status` reply).
///
/// # Errors
///
/// The deserialization error.
pub fn status_from_value(value: &serde_json::Value) -> Result<DreamRunStatus, serde_json::Error> {
    DreamRunStatus::deserialize(value)
}

impl DreamRunStatus {
    fn new(run_id: String, task: DreamTaskId, kind: DreamRunKind, started_at: u64) -> Self {
        Self {
            run_id,
            phase: DreamRunPhase::Idle,
            task,
            iteration: 0,
            best_node_score: 0.0,
            final_policy_score: None,
            improved: None,
            stop_reason: None,
            started_at,
            updated_at: started_at,
            error: None,
            kind,
            experiment_id: None,
            arm: None,
            arm_index: None,
            arm_count: None,
            round: None,
            rounds: None,
            cumulative_probes: None,
            result_path: None,
            seed: None,
            seed_index: None,
            seed_count: None,
            result_paths: None,
            tokens: None,
        }
    }
}

/// The child-agent knobs a run or experiment may set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DreamChildOptions {
    /// Child model selector (`provider/id`); the session model when `None`.
    pub model: Option<String>,
    /// Child thinking level for every role; [`DREAM_CHILD_THINKING`] when `None`.
    pub thinking: Option<String>,
    /// Visible-answer cap for every role; per-role defaults when `None`.
    pub max_output_tokens: Option<u64>,
}

/// One run's request (TS `DreamRunRequest`).
#[derive(Debug, Clone, PartialEq)]
pub struct DreamRunRequest {
    pub task: DreamTaskId,
    pub n: Option<usize>,
    pub seed: Option<u64>,
    pub workers: Option<u32>,
    pub k1: Option<u32>,
    pub k2: Option<u32>,
    pub dreams: Option<u32>,
    pub iterations: Option<u32>,
    pub llm_proposer: bool,
    pub llm_dreamer: bool,
    pub child: DreamChildOptions,
    pub priming_policies: Vec<ExplorationPolicy>,
}

impl DreamRunRequest {
    /// A request for `task` with every knob at its default.
    #[must_use]
    pub fn new(task: DreamTaskId) -> Self {
        Self {
            task,
            n: None,
            seed: None,
            workers: None,
            k1: None,
            k2: None,
            dreams: None,
            iterations: None,
            llm_proposer: false,
            llm_dreamer: false,
            child: DreamChildOptions::default(),
            priming_policies: Vec::new(),
        }
    }
}

/// One experiment's request (TS `DreamExperimentRequest`).
#[derive(Debug, Clone, PartialEq)]
pub struct DreamExperimentRequest {
    pub task: DreamTaskId,
    pub n: Option<usize>,
    pub seed: Option<u64>,
    /// Several seeds, run sequentially under one run id (exclusive with `seed`).
    pub seeds: Option<Vec<u64>>,
    pub rounds: Option<u32>,
    pub arms: Option<Vec<ExperimentArm>>,
    pub workers: Option<u32>,
    pub k1: Option<u32>,
    pub k2: Option<u32>,
    pub dreams: Option<u32>,
    pub llm_proposer: bool,
    pub llm_dreamer: bool,
    pub child: DreamChildOptions,
    pub priming_policies: Vec<ExplorationPolicy>,
}

impl DreamExperimentRequest {
    /// A request for `task` with every knob at its default.
    #[must_use]
    pub fn new(task: DreamTaskId) -> Self {
        Self {
            task,
            n: None,
            seed: None,
            seeds: None,
            rounds: None,
            arms: None,
            workers: None,
            k1: None,
            k2: None,
            dreams: None,
            llm_proposer: false,
            llm_dreamer: false,
            child: DreamChildOptions::default(),
            priming_policies: Vec::new(),
        }
    }
}

/// The most seeds one in-session experiment may run.
pub const DREAM_MAX_SEEDS: usize = 16;

/// Run defaults; they mirror the standalone `prime-agent dream` CLI.
pub const DREAM_RUN_SEED: u64 = 1;
pub const DREAM_RUN_WORKERS: u32 = 4;
pub const DREAM_RUN_K1: u32 = 12;
pub const DREAM_RUN_K2: u32 = 24;
pub const DREAM_RUN_DREAMS: u32 = 16;
pub const DREAM_RUN_ITERATIONS: u32 = 3;
/// Experiment rollouts per arm by default.
pub const DREAM_EXPERIMENT_ROUNDS: u32 = 4;

/// Child defaults (TS `DREAM_CHILD_DEFAULTS`): thinking off for every role,
/// and per-role visible-answer caps.
pub const DREAM_CHILD_THINKING: &str = "off";
pub const DREAM_CHILD_CAP_PROPOSER: u64 = 4096;
pub const DREAM_CHILD_CAP_PROPOSER_PYTHON_SPEEDUP: u64 = 8192;
pub const DREAM_CHILD_CAP_DREAMER: u64 = 4096;
pub const DREAM_CHILD_CAP_GUIDANCE: u64 = 2048;
/// Turns a dream child may take (tools are `none`, so one in practice).
pub const DREAM_CHILD_MAX_TURNS: u32 = 8;

/// The default visible-answer cap of one role for a task.
#[must_use]
pub fn dream_child_output_cap(role: DreamChildRole, task: DreamTaskId) -> u64 {
    match role {
        DreamChildRole::Proposer if task == DreamTaskId::PythonSpeedup => {
            DREAM_CHILD_CAP_PROPOSER_PYTHON_SPEEDUP
        }
        DreamChildRole::Proposer => DREAM_CHILD_CAP_PROPOSER,
        DreamChildRole::Dreamer => DREAM_CHILD_CAP_DREAMER,
        DreamChildRole::Guidance => DREAM_CHILD_CAP_GUIDANCE,
    }
}

/// The scope a run's children share (TS `dreamChildScope`): the request's
/// model (else the session's), the thinking level (default off) and the
/// PROPOSER's cap; the dreamer and guidance caps are applied per call by
/// [`RoleCappedRunner`]. An explicit cap applies to every role.
#[must_use]
pub fn dream_child_scope(
    task: DreamTaskId,
    options: &DreamChildOptions,
    session_model: Option<&str>,
) -> ChildRuntimeScope {
    ChildRuntimeScope {
        model: options
            .model
            .clone()
            .or_else(|| session_model.map(str::to_string))
            .filter(|model| !model.is_empty()),
        max_turns: Some(DREAM_CHILD_MAX_TURNS),
        token_budget: None,
        thinking_level: Some(
            options
                .thinking
                .clone()
                .unwrap_or_else(|| DREAM_CHILD_THINKING.to_string()),
        ),
        max_output_tokens: Some(
            options
                .max_output_tokens
                .unwrap_or_else(|| dream_child_output_cap(DreamChildRole::Proposer, task)),
        ),
    }
}

/// The runner a run's children go through (TS `roleCappedRunAgent`): the
/// dreamer and guidance children get their own default caps in place of the
/// proposer's the shared scope carries; an explicit cap passes through.
pub struct RoleCappedRunner {
    inner: Arc<dyn RunAgent>,
    task: DreamTaskId,
    explicit_cap: bool,
}

impl RoleCappedRunner {
    #[must_use]
    pub fn new(inner: Arc<dyn RunAgent>, task: DreamTaskId, options: &DreamChildOptions) -> Self {
        Self {
            inner,
            task,
            explicit_cap: options.max_output_tokens.is_some(),
        }
    }
}

impl RunAgent for RoleCappedRunner {
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult {
        let role = DreamChildRole::of_prompt(&request.prompt);
        match role {
            Some(role @ (DreamChildRole::Dreamer | DreamChildRole::Guidance))
                if !self.explicit_cap =>
            {
                let capped = RunAgentOptions {
                    max_output_tokens: Some(dream_child_output_cap(role, self.task)),
                    ..options.clone()
                };
                self.inner.run(request, &capped)
            }
            _ => self.inner.run(request, options),
        }
    }
}

/// Validate an experiment's seed list: 1..=[`DREAM_MAX_SEEDS`] distinct seeds.
///
/// # Errors
///
/// The TS `RangeError` message.
pub fn validate_dream_seeds(seeds: &[u64]) -> Result<Vec<u64>, String> {
    if seeds.is_empty() {
        return Err("dream experiment seeds must not be empty".to_string());
    }
    if seeds.len() > DREAM_MAX_SEEDS {
        return Err(format!(
            "dream experiment seeds must list at most {DREAM_MAX_SEEDS} seeds (got {})",
            seeds.len()
        ));
    }
    let distinct: std::collections::HashSet<u64> = seeds.iter().copied().collect();
    if distinct.len() != seeds.len() {
        return Err("dream experiment seeds must be distinct".to_string());
    }
    Ok(seeds.to_vec())
}

/// The UTC `YYYYMMDDHHMMSS` of a millisecond clock reading (TS
/// `toISOString().replace(/[^0-9]/g, "").slice(0, 14)`).
fn utc_stamp(ms: u64) -> String {
    let seconds = ms / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let rem = seconds % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// A fresh run id: `dream_<YYYYMMDDHHMMSS>_<8 hex>`.
#[must_use]
pub fn new_run_id(now_ms: u64) -> String {
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    format!("dream_{}_{}", utc_stamp(now_ms), &uuid[..8])
}

/// The service's injected clock (milliseconds).
pub type DreamNow = Arc<dyn Fn() -> u64 + Send + Sync>;
/// The status sink.
pub type DreamStatusSink = Arc<dyn Fn(&DreamRunStatus) + Send + Sync>;

/// What a service is built from (TS `DreamRunServiceDeps`).
#[derive(Clone)]
pub struct DreamRunServiceDeps {
    pub runner: Arc<dyn RunAgent>,
    /// The session model selector (`provider/id`) children default to.
    pub session_model: Option<String>,
    /// The dream store directory.
    pub dir: PathBuf,
    pub now: DreamNow,
    /// An injected rng for runs; a fresh seeded one per run when `None`.
    pub rng: Option<SeededRng>,
    /// Whether LLM experiment arms can be served (the in-session runner).
    pub llm_experiments: bool,
    pub on_update: DreamStatusSink,
}

#[derive(Default)]
struct Slot {
    status: Option<DreamRunStatus>,
    cancel: Option<CancellationToken>,
    running: bool,
}

/// A started run: its id and the thread that settles with its terminal status.
pub struct StartedRun {
    pub run_id: String,
    pub completion: JoinHandle<Result<DreamRunStatus, String>>,
}

/// The single-slot run service.
#[derive(Clone)]
pub struct DreamRunService {
    deps: Arc<DreamRunServiceDeps>,
    slot: Arc<Mutex<Slot>>,
}

impl DreamRunService {
    #[must_use]
    pub fn new(deps: DreamRunServiceDeps) -> Self {
        Self {
            deps: Arc::new(deps),
            slot: Arc::new(Mutex::new(Slot::default())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a run holds the slot.
    #[must_use]
    pub fn running(&self) -> bool {
        self.lock().running
    }

    /// The current (or last) run's status.
    #[must_use]
    pub fn status(&self) -> Option<DreamRunStatus> {
        self.lock().status.clone()
    }

    /// Request cancellation; `true` when a run was running.
    #[allow(clippy::must_use_candidate)] // a command; the flag is informational
    pub fn cancel(&self) -> bool {
        let slot = self.lock();
        match (&slot.cancel, slot.running) {
            (Some(cancel), true) => {
                cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Start a run in the background (TS `start` via `_startDreamBackground`).
    ///
    /// # Errors
    ///
    /// The refusal reason when a run is already in progress.
    pub fn start(
        &self,
        request: DreamRunRequest,
        trigger_trace_id: Option<String>,
    ) -> Result<StartedRun, String> {
        let task = request.task;
        self.launch(
            task,
            DreamRunKind::Run,
            |_| {},
            move |service, cancel, _started| service.run(&request, &cancel, trigger_trace_id),
        )
    }

    /// Start an experiment in the same slot (TS `startExperiment`).
    ///
    /// # Errors
    ///
    /// A malformed seed list (before anything starts) or a busy slot.
    pub fn start_experiment(
        &self,
        request: DreamExperimentRequest,
        trigger_trace_id: Option<String>,
    ) -> Result<StartedRun, String> {
        if request.seed.is_some() && request.seeds.is_some() {
            return Err("dream experiment takes either seed or seeds, not both".to_string());
        }
        let seeds = match &request.seeds {
            Some(seeds) => validate_dream_seeds(seeds)?,
            None => vec![request.seed.unwrap_or(DREAM_RUN_SEED)],
        };
        let rounds = request.rounds.unwrap_or(DREAM_EXPERIMENT_ROUNDS);
        let arms = request
            .arms
            .clone()
            .unwrap_or_else(|| LOCAL_EXPERIMENT_ARMS.to_vec());
        let settings = ExperimentSettings {
            request,
            rounds,
            arms,
            seeds,
        };
        let (arm_count, seed_count, first_seed) =
            (settings.arms.len(), settings.seeds.len(), settings.seeds[0]);
        let task = settings.request.task;
        self.launch(
            task,
            DreamRunKind::Experiment,
            move |status| {
                status.rounds = Some(rounds);
                status.arm_count = Some(arm_count);
                status.seed = Some(first_seed);
                status.seed_index = Some(0);
                status.seed_count = Some(seed_count);
            },
            move |service, cancel, started_at| {
                service.run_experiment(&settings, &cancel, started_at, trigger_trace_id.as_deref())
            },
        )
    }

    fn launch(
        &self,
        task: DreamTaskId,
        kind: DreamRunKind,
        initial: impl FnOnce(&mut DreamRunStatus),
        body: impl FnOnce(&Self, CancellationToken, u64) -> Result<DreamRunStatus, String>
        + Send
        + 'static,
    ) -> Result<StartedRun, String> {
        let cancel = CancellationToken::new();
        let (run_id, started_at) = {
            let mut slot = self.lock();
            if slot.running {
                return Err(slot.status.as_ref().map_or_else(
                    || "a Dream-RSI run is already in progress".to_string(),
                    |status| format!("Dream-RSI run {} is already in progress", status.run_id),
                ));
            }
            let started_at = (self.deps.now)();
            let run_id = new_run_id(started_at);
            let mut status = DreamRunStatus::new(run_id.clone(), task, kind, started_at);
            initial(&mut status);
            slot.status = Some(status);
            slot.cancel = Some(cancel.clone());
            slot.running = true;
            (run_id, started_at)
        };
        let service = self.clone();
        let spawned = std::thread::Builder::new()
            .name("dream-run".to_string())
            .spawn(move || {
                let result = body(&service, cancel, started_at);
                if let Err(error) = &result {
                    service.update(|status| {
                        status.phase = DreamRunPhase::Stopped;
                        status.error = Some(error.clone());
                    });
                }
                let mut slot = service.lock();
                slot.running = false;
                slot.cancel = None;
                result
            });
        match spawned {
            Ok(completion) => Ok(StartedRun { run_id, completion }),
            Err(error) => {
                let mut slot = self.lock();
                slot.running = false;
                slot.cancel = None;
                Err(format!("could not start the Dream-RSI run: {error}"))
            }
        }
    }

    /// Patch the status, stamp it, and publish it.
    fn update(&self, patch: impl FnOnce(&mut DreamRunStatus)) -> DreamRunStatus {
        self.update_with(patch, true)
    }

    fn update_with(&self, patch: impl FnOnce(&mut DreamRunStatus), emit: bool) -> DreamRunStatus {
        let snapshot = {
            let mut slot = self.lock();
            let Some(status) = slot.status.as_mut() else {
                return DreamRunStatus::new(
                    String::new(),
                    DreamTaskId::CirclePacking,
                    DreamRunKind::Run,
                    0,
                );
            };
            patch(status);
            status.updated_at = (self.deps.now)();
            status.clone()
        };
        if emit {
            // Observers never abort the run.
            let sink = Arc::clone(&self.deps.on_update);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(&snapshot)));
        }
        snapshot
    }

    fn scope(&self, task: DreamTaskId, options: &DreamChildOptions) -> ChildRuntimeScope {
        dream_child_scope(task, options, self.deps.session_model.as_deref())
    }

    fn run(
        &self,
        request: &DreamRunRequest,
        cancel: &CancellationToken,
        trigger_trace_id: Option<String>,
    ) -> Result<DreamRunStatus, String> {
        let seed = request.seed.unwrap_or(DREAM_RUN_SEED);
        let n = resolve_task_n(request.task, request.n);
        let task = resolve_task(request.task, n).map_err(|error| error.0)?;
        let runner =
            RoleCappedRunner::new(Arc::clone(&self.deps.runner), request.task, &request.child);
        let now = Arc::clone(&self.deps.now);
        let clock = move || now();
        let mut progress = |event: DreamProgressEvent| {
            self.update(|status| apply_progress(status, &event));
        };
        let outcome = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            runner: &runner,
            task: task.as_ref(),
            task_id: request.task.as_str().to_string(),
            n: n.and_then(|n| u32::try_from(n).ok()),
            seed: Seed::from(seed),
            clock: &clock,
            workers: request.workers.unwrap_or(DREAM_RUN_WORKERS),
            k1: request.k1.unwrap_or(DREAM_RUN_K1),
            k2: request.k2.unwrap_or(DREAM_RUN_K2),
            dreams: usize::try_from(request.dreams.unwrap_or(DREAM_RUN_DREAMS))
                .unwrap_or(usize::MAX),
            iterations: request.iterations.unwrap_or(DREAM_RUN_ITERATIONS),
            dir: &self.deps.dir,
            objective: DEFAULT_OBJECTIVE,
            rng: Some(
                self.deps
                    .rng
                    .clone()
                    .unwrap_or_else(|| SeededRng::new(&Seed::from(seed))),
            ),
            initial_policy: DEFAULT_POLICY,
            fixed_policy: false,
            initial_rollout: None,
            semantic_guidance: None,
            use_llm_proposer: request.llm_proposer,
            use_llm_dreamer: request.llm_dreamer,
            scope: self.scope(request.task, &request.child),
            cancel: cancel.clone(),
            proposer_prompt_context: task_prompt_context(request.task, n),
            child_token_budget: None,
            on_progress: Some(&mut progress),
            run_label: None,
            dreams_log_context: crate::dreams::DreamsLogContext::default(),
            priming_policies: request.priming_policies.clone(),
            trigger_trace_id,
        });
        match outcome {
            Ok(result) => Ok(self.update(|status| {
                status.phase = if result.improved {
                    DreamRunPhase::Accepted
                } else {
                    DreamRunPhase::Stopped
                };
                status.stop_reason = Some(DreamStopReason::Completed);
                status.final_policy_score = Some(result.final_policy_score);
                status.improved = Some(result.improved);
                status.best_node_score = result.best_node_score;
                status.iteration = result.iterations;
            })),
            Err(error) if error.is_abort() || cancel.is_cancelled() => Ok(self.update(|status| {
                status.phase = DreamRunPhase::Stopped;
                status.stop_reason = Some(DreamStopReason::Cancelled);
            })),
            Err(error) => Err(error.to_string()),
        }
    }

    #[allow(clippy::too_many_lines)] // one seed loop, kept whole as in the TS
    fn run_experiment(
        &self,
        settings: &ExperimentSettings,
        cancel: &CancellationToken,
        started_at: u64,
        trigger_trace_id: Option<&str>,
    ) -> Result<DreamRunStatus, String> {
        let request = &settings.request;
        let use_llm_proposer = request.llm_proposer;
        let use_llm_dreamer = request.llm_dreamer;
        assert_guided_arms_served(&settings.arms, use_llm_proposer)
            .map_err(|_| "dream-guided/fixed-guided require llmProposer".to_string())?;
        let needs_llm = use_llm_proposer || use_llm_dreamer;
        if needs_llm && !self.deps.llm_experiments {
            return Err("LLM experiment arms need an in-session LLM arm runner; this session has none, so llmProposer/llmDreamer and the guided arms are unavailable".to_string());
        }
        let n = resolve_task_n(request.task, request.n);
        let runner =
            RoleCappedRunner::new(Arc::clone(&self.deps.runner), request.task, &request.child);
        let mut agent = AgentArmRunner::new(
            &runner,
            AgentArmRunnerOptions {
                scope: self.scope(request.task, &request.child),
                cancel: cancel.clone(),
                use_llm_proposer,
                use_llm_dreamer,
                proposer_prompt_context: task_prompt_context(request.task, n),
                child_token_budget: None,
                share_initial_rollout: true,
            },
        );
        let mut local = LocalArmRunner;
        // One frozen clock per experiment: every arm's tree ids share it.
        let clock = move || started_at;
        let mut result_paths: Vec<String> = Vec::new();
        let mut best = 0.0_f64;
        let outcome: Result<(), ExperimentError> = (|| {
            for (seed_index, seed) in settings.seeds.iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(DreamStoreError::Aborted(format!(
                        "dream experiment aborted before seed {seed}"
                    ))
                    .into());
                }
                let spec = ExperimentSpec {
                    n: request.n,
                    priming_policies: request.priming_policies.clone(),
                    ..ExperimentSpec::new(
                        request.task,
                        Seed::from(*seed),
                        settings.rounds,
                        ExperimentBudget {
                            workers: request.workers.unwrap_or(DREAM_RUN_WORKERS),
                            k1: request.k1.unwrap_or(DREAM_RUN_K1),
                            k2: request.k2.unwrap_or(DREAM_RUN_K2),
                            dreams: request.dreams.unwrap_or(DREAM_RUN_DREAMS),
                        },
                        settings.arms.clone(),
                    )
                };
                let experiment_id = experiment_id_for(&spec, &clock);
                self.update_with(
                    |status| {
                        status.seed = Some(*seed);
                        status.seed_index = Some(seed_index);
                        status.seed_count = Some(settings.seeds.len());
                        status.experiment_id = Some(experiment_id);
                        status.arm = None;
                        status.arm_index = None;
                        status.round = Some(0);
                        status.iteration = 0;
                        status.cumulative_probes = Some(0);
                    },
                    seed_index > 0,
                );
                let mut progress = |event: ExperimentProgressEvent| {
                    self.update(|status| apply_experiment_progress(status, &event));
                };
                let options = ExperimentRunOptions {
                    dir: &self.deps.dir,
                    clock: &clock,
                    notes: Vec::new(),
                    overwrite: false,
                };
                let arm_runner: &mut dyn ExperimentArmRunner =
                    if needs_llm { &mut agent } else { &mut local };
                let result = run_experiment_with_hooks(
                    &spec,
                    &options,
                    arm_runner,
                    if needs_llm {
                        EXPERIMENT_ARMS
                    } else {
                        LOCAL_EXPERIMENT_ARMS
                    },
                    ExperimentHooks {
                        cancel: Some(cancel),
                        on_progress: Some(&mut progress),
                        detached: true,
                        trigger_trace_id: trigger_trace_id.map(str::to_string),
                    },
                )?;
                result_paths.push(
                    experiment_result_path(&self.deps.dir, &result.experiment_id)
                        .display()
                        .to_string(),
                );
                for arm in &result.arms {
                    best = best.max(arm.totals.final_best);
                }
                let tokens: u64 = result.arms.iter().map(|arm| arm.totals.tokens).sum();
                let paths = result_paths.clone();
                self.update(|status| {
                    status.experiment_id = Some(result.experiment_id.clone());
                    status.result_path = paths.last().cloned();
                    status.result_paths = Some(paths);
                    status.best_node_score = best;
                    if needs_llm {
                        status.tokens = Some(tokens);
                    }
                });
            }
            Ok(())
        })();
        let paths = result_paths;
        match outcome {
            Ok(()) => Ok(self.update(|status| {
                status.phase = DreamRunPhase::Stopped;
                status.stop_reason = Some(DreamStopReason::Completed);
                status.result_paths = Some(paths);
            })),
            Err(ExperimentError::Store(error)) if error.is_abort() => Ok(self.cancelled(paths)),
            Err(_) if cancel.is_cancelled() => Ok(self.cancelled(paths)),
            Err(error) => Err(error.to_string()),
        }
    }

    fn cancelled(&self, paths: Vec<String>) -> DreamRunStatus {
        self.update(|status| {
            status.phase = DreamRunPhase::Stopped;
            status.stop_reason = Some(DreamStopReason::Cancelled);
            status.result_paths = Some(paths);
        })
    }
}

struct ExperimentSettings {
    request: DreamExperimentRequest,
    rounds: u32,
    arms: Vec<ExperimentArm>,
    seeds: Vec<u64>,
}

/// Map a loop progress event onto the status (TS `statusPatch`).
fn apply_progress(status: &mut DreamRunStatus, event: &DreamProgressEvent) {
    match event {
        DreamProgressEvent::Phase {
            phase,
            iteration,
            best_node_score,
            ..
        } => {
            status.phase = run_phase(*phase);
            status.iteration = *iteration;
            status.best_node_score = *best_node_score;
        }
        DreamProgressEvent::Completed {
            iteration,
            best_node_score,
            final_policy_score,
            improved,
        } => {
            status.final_policy_score = Some(*final_policy_score);
            status.improved = Some(*improved);
            status.best_node_score = *best_node_score;
            status.iteration = *iteration;
        }
    }
}

fn run_phase(phase: crate::llm_loop::DreamPhase) -> DreamRunPhase {
    match phase {
        crate::llm_loop::DreamPhase::Rollout => DreamRunPhase::Rollout,
        crate::llm_loop::DreamPhase::Dreaming => DreamRunPhase::Dreaming,
        crate::llm_loop::DreamPhase::Redeploying => DreamRunPhase::Redeploying,
    }
}

/// Map an experiment event onto the status (TS `#experimentPatch`);
/// `best_node_score` stays the max seen.
fn apply_experiment_progress(status: &mut DreamRunStatus, event: &ExperimentProgressEvent) {
    let best = status.best_node_score;
    match event {
        ExperimentProgressEvent::ArmStart {
            arm,
            arm_index,
            arm_count,
        } => {
            status.phase = DreamRunPhase::Rollout;
            status.arm = Some(*arm);
            status.arm_index = Some(*arm_index);
            status.arm_count = Some(*arm_count);
            status.round = Some(0);
            status.iteration = 0;
            status.cumulative_probes = Some(0);
        }
        ExperimentProgressEvent::Phase { progress, .. } => {
            status.phase = run_phase(progress.phase);
            status.iteration = progress.iteration;
            status.round = Some(progress.iteration + 1);
            status.best_node_score = best.max(progress.best_node_score);
        }
        ExperimentProgressEvent::Round {
            round,
            cumulative_best,
            cumulative_probes,
            ..
        } => {
            status.round = Some(*round);
            status.iteration = round.saturating_sub(1);
            status.cumulative_probes = Some(*cumulative_probes);
            status.best_node_score = best.max(*cumulative_best);
        }
        ExperimentProgressEvent::ArmEnd { final_best, .. } => {
            status.best_node_score = best.max(*final_best);
        }
        ExperimentProgressEvent::Completed {
            experiment_id,
            result_path,
        } => {
            status.experiment_id = Some(experiment_id.clone());
            status.result_path = Some(result_path.display().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_id_stamp_is_the_iso_digits() {
        // 2023-11-14T22:13:20.000Z
        assert_eq!(utc_stamp(1_700_000_000_000), "20231114221320");
        assert_eq!(utc_stamp(0), "19700101000000");
        // 2024-02-29T23:59:59.999Z (a leap day)
        assert_eq!(utc_stamp(1_709_251_199_999), "20240229235959");
        let id = new_run_id(1_700_000_000_000);
        assert!(id.starts_with("dream_20231114221320_"), "{id}");
        assert_eq!(id.len(), "dream_20231114221320_".len() + 8);
    }

    #[test]
    fn seeds_are_validated_like_the_ts_range_errors() {
        assert_eq!(validate_dream_seeds(&[7, 8]), Ok(vec![7, 8]));
        assert_eq!(
            validate_dream_seeds(&[]),
            Err("dream experiment seeds must not be empty".to_string())
        );
        assert_eq!(
            validate_dream_seeds(&[1, 1]),
            Err("dream experiment seeds must be distinct".to_string())
        );
        let many: Vec<u64> = (0..17).collect();
        assert_eq!(
            validate_dream_seeds(&many),
            Err("dream experiment seeds must list at most 16 seeds (got 17)".to_string())
        );
    }

    #[test]
    fn the_scope_defaults_to_thinking_off_and_the_proposer_cap() {
        let scope = dream_child_scope(
            DreamTaskId::PythonSpeedup,
            &DreamChildOptions::default(),
            Some("anthropic/claude-sonnet-5"),
        );
        assert_eq!(
            scope,
            ChildRuntimeScope {
                model: Some("anthropic/claude-sonnet-5".to_string()),
                max_turns: Some(8),
                token_budget: None,
                thinking_level: Some("off".to_string()),
                max_output_tokens: Some(8192),
            }
        );
        let explicit = dream_child_scope(
            DreamTaskId::CirclePacking,
            &DreamChildOptions {
                model: Some("faux/x".to_string()),
                thinking: Some("high".to_string()),
                max_output_tokens: Some(1000),
            },
            Some("anthropic/claude-sonnet-5"),
        );
        assert_eq!(
            (
                explicit.model.as_deref(),
                explicit.thinking_level.as_deref(),
                explicit.max_output_tokens
            ),
            (Some("faux/x"), Some("high"), Some(1000))
        );
        assert_eq!(
            dream_child_scope(
                DreamTaskId::CirclePacking,
                &DreamChildOptions::default(),
                None
            )
            .max_output_tokens,
            Some(4096)
        );
    }
}
