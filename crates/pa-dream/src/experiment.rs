//! Dream-RSI experiments: the paper's evidence, with its control arm (TS
//! `experiment.ts`).
//!
//! Several ARMS of the loop run from the same task, initial policy, seed,
//! clock and per-round budget, each into its own store under
//! `<dir>/experiments/<id>/<arm>`, and the result is
//! `<dir>/experiments/<id>/result.json` (schema `prime-agent.dream.experiment/1`,
//! read by `evals/dream/plot_experiment.py`). The `fixed` arm never dreams; the
//! headline compares every arm against it on PROBES, the discovery compute.
//! [`ExperimentArmRunner`] is the seam the in-session LLM runner implements;
//! the standalone runner is [`LocalArmRunner`] over the `dream` and `fixed` arms.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::dream_loop::{
    run_dream_loop, DreamHandlerCalls, DreamLoopOptions, DreamLoopResult, DreamRoundDreaming,
};
use crate::dreams::DreamsLogContext;
use crate::improve::CandidateVerdict;
use crate::json;
use crate::llm_loop::{DreamInitialRollout, DreamPhase};
use crate::objective::{ReplayObjectiveConfig, DEFAULT_OBJECTIVE};
use crate::policy::{policy_id, ExplorationPolicy, StopRule, DEFAULT_POLICY};
use crate::proposer::{ProposalTally, RejectCounts};
use crate::rng::Seed;
use crate::rollout::{DreamClock, ScoreImprovement};
use crate::store::{
    create_dir_private, experiment_arm_dir, experiment_dir, experiment_result_path, write_private,
    DreamStoreError,
};
use crate::task::DynTask;
use crate::tasks::{resolve_task, resolve_task_n, DreamTaskId, TaskSizeError};

/// The result file's schema id.
pub const EXPERIMENT_SCHEMA: &str = "prime-agent.dream.experiment/1";
/// Reaching the control's target is judged within this tolerance.
const TARGET_EPS: f64 = 1e-9;
const NO_FIXED_ARM_NOTE: &str = "no fixed arm ran: the headline multipliers are undefined";
/// Appended to every result: the objective it was scored by.
pub const OBJECTIVE_NOTE: &str =
    "objective: normalized (q in pool range, cost in budget fractions)";
/// Why a guided arm cannot run here.
pub const GUIDED_ARM_REJECTION_MESSAGE: &str = "dream-guided/fixed-guided need the in-session LLM proposer; run dream.experiment(...) from the kernel skill or /dream experiment --llm-proposer";

/// One experiment arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ExperimentArm {
    #[serde(rename = "dream")]
    Dream,
    #[serde(rename = "fixed")]
    Fixed,
    #[serde(rename = "dream-guided")]
    DreamGuided,
    #[serde(rename = "fixed-guided")]
    FixedGuided,
}

/// Every arm, in the TS order.
pub const EXPERIMENT_ARMS: &[ExperimentArm] = &[
    ExperimentArm::Dream,
    ExperimentArm::Fixed,
    ExperimentArm::DreamGuided,
    ExperimentArm::FixedGuided,
];

/// The arms the token-free local runner serves.
pub const LOCAL_EXPERIMENT_ARMS: &[ExperimentArm] = &[ExperimentArm::Dream, ExperimentArm::Fixed];

impl ExperimentArm {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dream => "dream",
            Self::Fixed => "fixed",
            Self::DreamGuided => "dream-guided",
            Self::FixedGuided => "fixed-guided",
        }
    }

    /// The arm named by `text`.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        EXPERIMENT_ARMS
            .iter()
            .copied()
            .find(|arm| arm.as_str() == text)
    }

    /// The control arms never dream.
    #[must_use]
    pub fn fixed_policy(self) -> bool {
        matches!(self, Self::Fixed | Self::FixedGuided)
    }

    /// The guided arms carry the semantic-guidance ablation.
    #[must_use]
    pub fn guided(self) -> bool {
        matches!(self, Self::DreamGuided | Self::FixedGuided)
    }
}

/// The note a wall-clock-scored task's result carries.
#[must_use]
pub fn timing_scoring_note(task: DreamTaskId) -> String {
    format!(
        "{task}: evaluate is wall-clock timed, so scores are not byte-deterministic; every arm's round 1 shares its seed, policy and tree id, but its scores differ within timing noise, so round 1 is identical across arms only on a deterministically scored task"
    )
}

/// The note a result carries when `k1 <= beta` keeps a beta-driven stop rule from ever firing.
#[must_use]
pub fn k1_stop_rule_note(k1: u32, policy: &ExplorationPolicy) -> Option<String> {
    if !matches!(policy.stop_rule, StopRule::Patience | StopRule::FixedRounds) || k1 > policy.beta {
        return None;
    }
    Some(format!(
        "k1 {k1} <= initialPolicy.beta {}: {} can never stop a rollout before the round cap, so every rollout runs exactly k1 rounds and replay cannot reward saving rounds",
        policy.beta,
        policy.stop_rule.as_str()
    ))
}

/// The per-round budget every arm shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExperimentBudget {
    pub workers: u32,
    pub k1: u32,
    pub k2: u32,
    pub dreams: u32,
}

/// What to run.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentSpec {
    pub task: DreamTaskId,
    /// The task size; the task's default when `None` (the result records the resolved size).
    pub n: Option<usize>,
    pub seed: Seed,
    /// Rollouts per arm (>= 1).
    pub rounds: u32,
    pub budget: ExperimentBudget,
    /// Distinct arms, run in this order.
    pub arms: Vec<ExperimentArm>,
    pub objective: ReplayObjectiveConfig,
    pub initial_policy: ExplorationPolicy,
    pub priming_policies: Vec<ExplorationPolicy>,
}

impl ExperimentSpec {
    /// A spec with the default objective, initial policy and no priming.
    #[must_use]
    pub fn new(
        task: DreamTaskId,
        seed: Seed,
        rounds: u32,
        budget: ExperimentBudget,
        arms: Vec<ExperimentArm>,
    ) -> Self {
        Self {
            task,
            n: None,
            seed,
            rounds,
            budget,
            arms,
            objective: DEFAULT_OBJECTIVE,
            initial_policy: DEFAULT_POLICY,
            priming_policies: Vec::new(),
        }
    }
}

/// Where and how an experiment runs.
pub struct ExperimentRunOptions<'a> {
    /// The dream dir; the experiment writes only under `<dir>/experiments/<id>`.
    pub dir: &'a Path,
    pub clock: DreamClock<'a>,
    pub notes: Vec<String>,
    /// Replace an existing experiment directory of the same id.
    pub overwrite: bool,
}

/// Why an experiment did not run.
#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    /// A malformed spec (TS `RangeError`).
    #[error("{0}")]
    Range(String),
    /// An arm the runner cannot serve.
    #[error("{0}")]
    ArmUnavailable(String),
    #[error(transparent)]
    Store(#[from] DreamStoreError),
}

impl From<TaskSizeError> for ExperimentError {
    fn from(error: TaskSizeError) -> Self {
        Self::Range(error.0)
    }
}

/// One round's row.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentRoundRow {
    pub round: u32,
    pub tree_id: String,
    pub policy_id: String,
    pub round_best: f64,
    pub cumulative_best: f64,
    pub probes: u32,
    pub cumulative_probes: u32,
    pub agent_generated_calls: u32,
    pub cumulative_agent_generated_calls: u32,
    pub local_fallbacks: u64,
    pub llm_proposals: u64,
    pub llm_accepted: u64,
    pub llm_rejected: RejectCounts,
    pub decision_rounds: u32,
    pub pool_size: usize,
    pub handler_calls: DreamHandlerCalls,
    pub cumulative_handler_calls: u64,
    pub tokens: u64,
    pub cumulative_tokens: u64,
    pub dreaming: Option<DreamRoundDreaming>,
    pub probes_to_round_best: u32,
    pub improvements: Vec<ScoreImprovement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priming_tree_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priming_probes: Option<u32>,
}

/// The proposer/dreamer mode an arm ran with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentArmMode {
    pub proposer: &'static str,
    pub dreamer: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

impl ExperimentArmMode {
    /// The zero-token local mode.
    #[must_use]
    pub fn local() -> Self {
        Self {
            proposer: "local",
            dreamer: "local",
            model: None,
            thinking: None,
            max_output_tokens: None,
        }
    }
}

/// An arm's totals.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentArmTotals {
    pub probes: u32,
    pub agent_generated_calls: u32,
    pub local_fallbacks: u64,
    pub llm_proposals: u64,
    pub llm_accepted: u64,
    pub llm_rejected: RejectCounts,
    pub handler_calls: u64,
    pub tokens: u64,
    pub final_best: f64,
}

/// In-arm replay estimates on the arm's own final pool.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PolicyScoreOnOwnPool {
    pub initial: f64,
    #[serde(rename = "final")]
    pub final_score: f64,
}

/// One arm's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentArmResult {
    pub arm: ExperimentArm,
    pub fixed_policy: bool,
    pub guided: bool,
    pub mode: ExperimentArmMode,
    /// Relative to the dream dir: `experiments/<id>/<arm>`.
    pub store_dir: String,
    pub run_id: String,
    pub initial_policy_id: String,
    /// The LAST DEPLOYED policy.
    pub final_policy_id: String,
    /// The post-hoc winner over {initial} and every dreamed policy.
    pub selected_policy_id: String,
    pub policy_score_on_own_pool: PolicyScoreOnOwnPool,
    /// Rounds whose policy differs from the previous round's.
    pub policy_changes: u32,
    pub rounds: Vec<ExperimentRoundRow>,
    pub totals: ExperimentArmTotals,
    pub stopped_early: u32,
    pub final_selection: Vec<CandidateVerdict>,
}

/// Per-arm values keyed by arm name, in arm order.
pub type ArmValues = Map<String, Value>;

/// The headline comparison against the fixed control.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentHeadline {
    pub reference: &'static str,
    pub target: f64,
    pub probes_to_target: ArmValues,
    pub calls_multiplier: ArmValues,
    pub equal_budget: u32,
    pub best_at_budget: ArmValues,
    pub score_multiplier: ArmValues,
    pub delta_best: ArmValues,
    pub probes_to_target_exact: ArmValues,
    pub calls_multiplier_exact: ArmValues,
}

/// The persisted result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentResult {
    pub schema: &'static str,
    pub experiment_id: String,
    pub task: DreamTaskIdName,
    pub scoring: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
    pub seed: Seed,
    pub rounds: u32,
    pub budget: ExperimentBudget,
    pub objective: ReplayObjectiveConfig,
    pub initial_policy_id: String,
    pub initial_policy: ExplorationPolicy,
    pub arms: Vec<ExperimentArmResult>,
    pub headline: Option<ExperimentHeadline>,
    pub shared_initial_rollout: bool,
    pub created_ts: u64,
    pub notes: Vec<String>,
}

/// A task id serialized as its wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamTaskIdName(pub DreamTaskId);

impl Serialize for DreamTaskIdName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

/// One arm's planned run.
pub struct ExperimentArmPlan {
    pub arm: ExperimentArm,
    pub index: usize,
    /// Absolute store directory.
    pub dir: PathBuf,
    /// `experiments/<id>/<arm>`.
    pub store_dir: String,
    /// `<experimentId>/<arm>`.
    pub run_label: String,
}

/// A validated experiment, laid out on disk.
pub struct ExperimentPlan {
    pub experiment_id: String,
    pub task: std::sync::Arc<dyn DynTask>,
    pub task_id: DreamTaskId,
    pub n: Option<usize>,
    pub seed: Seed,
    pub rounds: u32,
    pub budget: ExperimentBudget,
    pub objective: ReplayObjectiveConfig,
    pub initial_policy: ExplorationPolicy,
    pub priming_policies: Vec<ExplorationPolicy>,
    pub dir: PathBuf,
    pub result_path: PathBuf,
    pub arms: Vec<ExperimentArmPlan>,
    pub created_ts: u64,
    pub notes: Vec<String>,
}

impl ExperimentPlan {
    /// The loop options one arm runs with.
    #[must_use]
    pub fn loop_options<'a>(
        &'a self,
        arm: &'a ExperimentArmPlan,
        clock: DreamClock<'a>,
    ) -> DreamLoopOptions<'a> {
        DreamLoopOptions {
            task: self.task.as_ref(),
            task_id: self.task_id.as_str().to_string(),
            n: self.n.and_then(|n| u32::try_from(n).ok()),
            seed: self.seed.clone(),
            clock,
            workers: self.budget.workers,
            k1: self.budget.k1,
            k2: self.budget.k2,
            dreams: usize::try_from(self.budget.dreams).unwrap_or(usize::MAX),
            iterations: self.rounds - 1,
            dir: &arm.dir,
            objective: self.objective,
            rng: None,
            initial_policy: self.initial_policy,
            fixed_policy: arm.arm.fixed_policy(),
            candidates: None,
            run_label: Some(arm.run_label.clone()),
            dreams_log_context: DreamsLogContext {
                experiment_id: Some(self.experiment_id.clone()),
                arm: Some(arm.arm.as_str().to_string()),
            },
            priming_policies: self.priming_policies.clone(),
        }
    }
}

/// `<task>-s<seed>-n<rounds>-<clock>`: an experiment's id (TS `experimentIdFor`).
#[must_use]
pub fn experiment_id_for(spec: &ExperimentSpec, clock: DreamClock<'_>) -> String {
    format!(
        "{}-s{}-n{}-{}",
        spec.task.as_str(),
        spec.seed,
        spec.rounds,
        clock()
    )
}

/// Validate a spec, resolve the task once and lay out the arm stores.
///
/// # Errors
///
/// [`ExperimentError`] for a malformed spec, an arm outside `allowed_arms`, or
/// an existing experiment directory without `overwrite`.
pub fn plan_experiment(
    spec: &ExperimentSpec,
    options: &ExperimentRunOptions<'_>,
    allowed_arms: &[ExperimentArm],
) -> Result<ExperimentPlan, ExperimentError> {
    if spec.rounds < 1 {
        return Err(ExperimentError::Range(format!(
            "experiment rounds must be an integer >= 1 (got {})",
            spec.rounds
        )));
    }
    if spec.arms.is_empty() {
        return Err(ExperimentError::Range(
            "experiment needs at least one arm".to_string(),
        ));
    }
    let distinct: std::collections::HashSet<ExperimentArm> = spec.arms.iter().copied().collect();
    if distinct.len() != spec.arms.len() {
        let names: Vec<&str> = spec.arms.iter().map(|arm| arm.as_str()).collect();
        return Err(ExperimentError::Range(format!(
            "experiment arms must be distinct (got {})",
            names.join(",")
        )));
    }
    if spec.arms.iter().any(|arm| !allowed_arms.contains(arm)) {
        return Err(ExperimentError::ArmUnavailable(
            GUIDED_ARM_REJECTION_MESSAGE.to_string(),
        ));
    }
    for (key, value) in [
        ("workers", spec.budget.workers),
        ("k1", spec.budget.k1),
        ("k2", spec.budget.k2),
        ("dreams", spec.budget.dreams),
    ] {
        if value < 1 {
            return Err(ExperimentError::Range(format!(
                "experiment budget {key} must be an integer >= 1 (got {value})"
            )));
        }
    }
    let experiment_id = experiment_id_for(spec, options.clock);
    let result_path = experiment_result_path(options.dir, &experiment_id);
    let existing = experiment_dir(options.dir, &experiment_id);
    if !options.overwrite && existing.exists() {
        return Err(DreamStoreError::Message(format!(
            "experiment {experiment_id} already exists at {}; pass overwrite to replace it",
            existing.display()
        ))
        .into());
    }
    let n = resolve_task_n(spec.task, spec.n);
    let task = resolve_task(spec.task, n)?;
    let mut notes = options.notes.clone();
    if spec.task.scoring() == "timing" {
        notes.push(timing_scoring_note(spec.task));
    }
    if !spec.arms.contains(&ExperimentArm::Fixed) {
        notes.push(NO_FIXED_ARM_NOTE.to_string());
    }
    if let Some(note) = k1_stop_rule_note(spec.budget.k1, &spec.initial_policy) {
        notes.push(note);
    }
    notes.push(OBJECTIVE_NOTE.to_string());
    let arms = spec
        .arms
        .iter()
        .enumerate()
        .map(|(index, arm)| ExperimentArmPlan {
            arm: *arm,
            index,
            dir: experiment_arm_dir(options.dir, &experiment_id, arm.as_str()),
            store_dir: format!("experiments/{experiment_id}/{}", arm.as_str()),
            run_label: format!("{experiment_id}/{}", arm.as_str()),
        })
        .collect();
    Ok(ExperimentPlan {
        task,
        task_id: spec.task,
        n,
        seed: spec.seed.clone(),
        rounds: spec.rounds,
        budget: spec.budget,
        objective: spec.objective,
        initial_policy: spec.initial_policy,
        priming_policies: spec.priming_policies.clone(),
        dir: options.dir.to_path_buf(),
        result_path,
        arms,
        created_ts: (options.clock)(),
        notes,
        experiment_id,
    })
}

/// Derive an arm's rows and totals from the loop's records.
#[must_use]
pub fn build_arm_result(
    arm: &ExperimentArmPlan,
    mode: ExperimentArmMode,
    run: &DreamLoopResult,
) -> ExperimentArmResult {
    let mut cumulative_best: Option<f64> = None;
    let mut cumulative_probes = 0;
    let mut cumulative_agent = 0;
    let mut cumulative_handler_calls = 0;
    let mut cumulative_tokens = 0;
    let mut policy_changes = 0;
    let mut previous: Option<&str> = None;
    let mut totals = ProposalTally::default();
    let rounds: Vec<ExperimentRoundRow> = run
        .rounds
        .iter()
        .map(|record| {
            if cumulative_best.is_none_or(|best| record.round_best > best) {
                cumulative_best = Some(record.round_best);
            }
            cumulative_probes += record.probes;
            cumulative_agent += record.agent_generated_calls;
            totals = totals.plus(&record.proposals);
            let handler_calls = record.handler_calls.proposer
                + record.handler_calls.dreamer
                + record.handler_calls.guidance;
            cumulative_handler_calls += handler_calls;
            let tokens = record.tokens.rollout + record.tokens.dreamer + record.tokens.guidance;
            cumulative_tokens += tokens;
            if previous.is_some_and(|previous| previous != record.policy_id) {
                policy_changes += 1;
            }
            previous = Some(&record.policy_id);
            ExperimentRoundRow {
                round: record.iteration + 1,
                tree_id: record.tree_id.clone(),
                policy_id: record.policy_id.clone(),
                round_best: record.round_best,
                cumulative_best: cumulative_best.unwrap_or(0.0),
                probes: record.probes,
                cumulative_probes,
                agent_generated_calls: record.agent_generated_calls,
                cumulative_agent_generated_calls: cumulative_agent,
                local_fallbacks: record.proposals.local_fallbacks,
                llm_proposals: record.proposals.llm_proposals,
                llm_accepted: record.proposals.llm_accepted,
                llm_rejected: record.proposals.llm_rejected,
                decision_rounds: record.decision_rounds,
                pool_size: record.pool_size,
                handler_calls: record.handler_calls,
                cumulative_handler_calls,
                tokens,
                cumulative_tokens,
                dreaming: record.dreaming.clone(),
                probes_to_round_best: record.probes_to_round_best,
                improvements: record.improvements.clone(),
                priming_tree_ids: record.priming_tree_ids.clone(),
                priming_probes: record.priming_probes,
            }
        })
        .collect();
    ExperimentArmResult {
        arm: arm.arm,
        fixed_policy: arm.arm.fixed_policy(),
        guided: arm.arm.guided(),
        mode,
        store_dir: arm.store_dir.clone(),
        run_id: run.run_id.clone(),
        initial_policy_id: run.initial_policy_id.clone(),
        final_policy_id: rounds.last().map_or_else(
            || run.initial_policy_id.clone(),
            |row| row.policy_id.clone(),
        ),
        selected_policy_id: run.final_policy_id.clone(),
        policy_score_on_own_pool: PolicyScoreOnOwnPool {
            initial: run.initial_policy_score,
            final_score: run.final_policy_score,
        },
        policy_changes,
        totals: ExperimentArmTotals {
            probes: cumulative_probes,
            agent_generated_calls: cumulative_agent,
            local_fallbacks: totals.local_fallbacks,
            llm_proposals: totals.llm_proposals,
            llm_accepted: totals.llm_accepted,
            llm_rejected: totals.llm_rejected,
            handler_calls: cumulative_handler_calls,
            tokens: cumulative_tokens,
            final_best: cumulative_best.unwrap_or(0.0),
        },
        rounds,
        stopped_early: run.stopped_early,
        final_selection: run.final_selection.clone(),
    }
}

fn ratio(numerator: Option<f64>, denominator: Option<f64>) -> Option<f64> {
    match (numerator, denominator) {
        (Some(numerator), Some(denominator)) if denominator > 0.0 => Some(numerator / denominator),
        _ => None,
    }
}

/// The probe at which an arm first reached `target` (probe-granular).
#[must_use]
pub fn exact_probes_to_target(rows: &[ExperimentRoundRow], target: f64) -> Option<u32> {
    let mut before = 0;
    for row in rows {
        if let Some(point) = row
            .improvements
            .iter()
            .find(|point| point.score >= target - TARGET_EPS)
        {
            return Some(before + point.probe);
        }
        before = row.cumulative_probes;
    }
    None
}

fn optional(value: Option<f64>) -> Value {
    value.map_or(Value::Null, json::number)
}

/// The headline against the `fixed` control; `None` without one.
#[must_use]
pub fn compute_headline(arms: &[ExperimentArmResult]) -> Option<ExperimentHeadline> {
    let control = arms.iter().find(|arm| arm.arm == ExperimentArm::Fixed)?;
    let target = control.totals.final_best;
    let equal_budget = arms.iter().map(|arm| arm.totals.probes).min().unwrap_or(0);
    let reach = |arm: &ExperimentArmResult| {
        arm.rounds
            .iter()
            .find(|row| row.cumulative_best >= target - TARGET_EPS)
            .map(|row| f64::from(row.cumulative_probes))
    };
    let exact =
        |arm: &ExperimentArmResult| exact_probes_to_target(&arm.rounds, target).map(f64::from);
    let at_budget = |arm: &ExperimentArmResult| {
        arm.rounds
            .iter()
            .rev()
            .find(|row| row.cumulative_probes <= equal_budget)
            .map(|row| row.cumulative_best)
    };
    let mut headline = ExperimentHeadline {
        reference: "fixed",
        target,
        probes_to_target: Map::new(),
        calls_multiplier: Map::new(),
        equal_budget,
        best_at_budget: Map::new(),
        score_multiplier: Map::new(),
        delta_best: Map::new(),
        probes_to_target_exact: Map::new(),
        calls_multiplier_exact: Map::new(),
    };
    let (control_reach, control_exact, control_budget) =
        (reach(control), exact(control), at_budget(control));
    for arm in arms {
        let name = arm.arm.as_str().to_string();
        headline
            .probes_to_target
            .insert(name.clone(), optional(reach(arm)));
        headline
            .probes_to_target_exact
            .insert(name.clone(), optional(exact(arm)));
        headline
            .best_at_budget
            .insert(name.clone(), optional(at_budget(arm)));
        headline
            .delta_best
            .insert(name.clone(), json::number(arm.totals.final_best - target));
    }
    for arm in arms {
        let name = arm.arm.as_str().to_string();
        headline
            .calls_multiplier
            .insert(name.clone(), optional(ratio(control_reach, reach(arm))));
        headline
            .calls_multiplier_exact
            .insert(name.clone(), optional(ratio(control_exact, exact(arm))));
        headline
            .score_multiplier
            .insert(name, optional(ratio(at_budget(arm), control_budget)));
    }
    Some(headline)
}

/// Observability-only phase progress a runner relays from inside an arm.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentArmProgress {
    pub phase: DreamPhase,
    pub iteration: u32,
    pub best_node_score: f64,
    pub tree_id: Option<String>,
}

/// What an in-session experiment reports as it runs (TS `ExperimentProgressEvent`).
#[derive(Debug, Clone, PartialEq)]
pub enum ExperimentProgressEvent {
    ArmStart {
        arm: ExperimentArm,
        arm_index: usize,
        arm_count: usize,
    },
    Phase {
        arm: ExperimentArm,
        progress: ExperimentArmProgress,
    },
    Round {
        arm: ExperimentArm,
        round: u32,
        round_best: f64,
        cumulative_best: f64,
        cumulative_probes: u32,
        tokens: u64,
    },
    ArmEnd {
        arm: ExperimentArm,
        final_best: f64,
        total_probes: u32,
    },
    Completed {
        experiment_id: String,
        result_path: PathBuf,
    },
}

/// How an experiment drives one arm: the seam the in-session LLM runner
/// implements (sharing round 1 across arms, guided prompts). Implementations
/// run the arm's whole loop into `arm.dir` and report its records.
pub trait ExperimentArmRunner {
    /// The mode recorded on the arm's result.
    fn mode(&self, arm: &ExperimentArmPlan) -> ExperimentArmMode;

    /// An optional shared round 1, rolled out once into `plan.arms[0].dir` and
    /// copied into every other arm's store; `None` (the default) lets every
    /// arm roll out its own.
    ///
    /// # Errors
    ///
    /// [`ExperimentError`] when an arm cannot be served or the rollout fails.
    fn prepare(
        &mut self,
        plan: &ExperimentPlan,
        clock: DreamClock<'_>,
    ) -> Result<Option<DreamInitialRollout>, ExperimentError> {
        let _ = (plan, clock);
        Ok(None)
    }

    /// Run `plan.rounds` rollouts of `arm` (round 1 adopted from `shared` when given).
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] when the arm's store cannot be written or the run was cancelled.
    fn run(
        &mut self,
        plan: &ExperimentPlan,
        arm: &ExperimentArmPlan,
        clock: DreamClock<'_>,
        shared: Option<&DreamInitialRollout>,
        progress: &mut dyn FnMut(ExperimentArmProgress),
    ) -> Result<DreamLoopResult, DreamStoreError>;
}

/// The zero-token local runner: [`run_dream_loop`] per arm.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalArmRunner;

impl ExperimentArmRunner for LocalArmRunner {
    fn mode(&self, _arm: &ExperimentArmPlan) -> ExperimentArmMode {
        ExperimentArmMode::local()
    }

    fn run(
        &mut self,
        plan: &ExperimentPlan,
        arm: &ExperimentArmPlan,
        clock: DreamClock<'_>,
        shared: Option<&DreamInitialRollout>,
        _progress: &mut dyn FnMut(ExperimentArmProgress),
    ) -> Result<DreamLoopResult, DreamStoreError> {
        if shared.is_some() {
            return Err(DreamStoreError::Message(
                "the local arm runner does not take a shared initial rollout".to_string(),
            ));
        }
        run_dream_loop(plan.loop_options(arm, clock))
    }
}

/// What an in-session experiment adds to a run (all off for the standalone runner).
#[derive(Default)]
pub struct ExperimentHooks<'a, 'p> {
    /// Checked before every arm and before the result is written; a cancel
    /// leaves no result.
    pub cancel: Option<&'a CancellationToken>,
    pub on_progress: Option<&'p mut dyn FnMut(ExperimentProgressEvent)>,
    /// Open `dream.experiment` as a DETACHED root (the run outlives its turn).
    pub detached: bool,
    /// The launching turn's trace id, stamped on a detached root.
    pub trigger_trace_id: Option<String>,
}

/// Write `result.json` (2-space JSON plus a newline) and return its path.
///
/// # Errors
///
/// [`DreamStoreError`] on a filesystem failure.
pub fn write_experiment_result(
    dir: &Path,
    result: &ExperimentResult,
) -> Result<PathBuf, DreamStoreError> {
    let path = experiment_result_path(dir, &result.experiment_id);
    if let Some(parent) = path.parent() {
        create_dir_private(parent)?;
    }
    write_private(&path, &format!("{}\n", json::stringify_pretty(result)))?;
    Ok(path)
}

/// Run an experiment through `runner`, serving only `allowed_arms`.
///
/// # Errors
///
/// [`ExperimentError`] from planning or from an arm's store.
pub fn run_experiment_with_runner(
    spec: &ExperimentSpec,
    options: &ExperimentRunOptions<'_>,
    runner: &mut dyn ExperimentArmRunner,
    allowed_arms: &[ExperimentArm],
) -> Result<ExperimentResult, ExperimentError> {
    run_experiment_with_hooks(
        spec,
        options,
        runner,
        allowed_arms,
        ExperimentHooks::default(),
    )
}

fn experiment_span(plan: &ExperimentPlan, mode: &str, detached: bool) -> tracing::Span {
    let arm_names: Vec<&str> = plan.arms.iter().map(|arm| arm.arm.as_str()).collect();
    let arms = arm_names.join(",");
    if detached {
        tracing::info_span!(
            parent: None,
            "dream.experiment",
            dream.experiment_id = %plan.experiment_id,
            dream.task = plan.task_id.as_str(),
            dream.seed = %plan.seed,
            dream.rounds = plan.rounds,
            dream.arms = %arms,
            dream.mode = mode,
            trigger.trace_id = tracing::field::Empty,
            dream.stopped = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    } else {
        tracing::info_span!(
            "dream.experiment",
            dream.experiment_id = %plan.experiment_id,
            dream.task = plan.task_id.as_str(),
            dream.seed = %plan.seed,
            dream.rounds = plan.rounds,
            dream.arms = %arms,
            dream.mode = mode,
            trigger.trace_id = tracing::field::Empty,
            dream.stopped = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    }
}

/// [`run_experiment_with_runner`] with the in-session hooks (TS
/// `runExperimentWithRunner`): cancellation before every arm and before the
/// result, progress events, a shared round 1 through
/// [`ExperimentArmRunner::prepare`], and a detached `dream.experiment` root
/// marked `dream.stopped: aborted` on a cancel. A cancelled or failed
/// experiment writes no `result.json`.
///
/// # Errors
///
/// [`ExperimentError`] from planning, an arm's store, or a cancel
/// ([`DreamStoreError::Aborted`]).
pub fn run_experiment_with_hooks(
    spec: &ExperimentSpec,
    options: &ExperimentRunOptions<'_>,
    runner: &mut dyn ExperimentArmRunner,
    allowed_arms: &[ExperimentArm],
    mut hooks: ExperimentHooks<'_, '_>,
) -> Result<ExperimentResult, ExperimentError> {
    let plan = plan_experiment(spec, options, allowed_arms)?;
    let existing = experiment_dir(&plan.dir, &plan.experiment_id);
    match std::fs::remove_dir_all(&existing) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(DreamStoreError::io(&existing, error).into()),
    }
    let modes: Vec<ExperimentArmMode> = plan.arms.iter().map(|arm| runner.mode(arm)).collect();
    let mode = if modes
        .iter()
        .any(|mode| mode.proposer == "llm" || mode.dreamer == "llm")
    {
        "llm"
    } else {
        "local"
    };
    let span = experiment_span(&plan, mode, hooks.detached);
    if let Some(trigger) = hooks.trigger_trace_id.as_deref().filter(|_| hooks.detached) {
        span.record("trigger.trace_id", trigger);
    }
    let result = {
        let _entered = span.enter();
        run_arms(&plan, &modes, options.clock, runner, &mut hooks)
    };
    if let Err(error) = &result {
        if matches!(error, ExperimentError::Store(store) if store.is_abort())
            || hooks.cancel.is_some_and(CancellationToken::is_cancelled)
        {
            span.record("dream.stopped", "aborted");
        } else {
            span.record("error", error.to_string().as_str());
        }
    }
    result
}

fn run_arms(
    plan: &ExperimentPlan,
    modes: &[ExperimentArmMode],
    clock: DreamClock<'_>,
    runner: &mut dyn ExperimentArmRunner,
    hooks: &mut ExperimentHooks<'_, '_>,
) -> Result<ExperimentResult, ExperimentError> {
    let cancel = hooks.cancel;
    let check = |place: &str| -> Result<(), ExperimentError> {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(DreamStoreError::Aborted(format!(
                "dream experiment aborted before {place}"
            ))
            .into());
        }
        Ok(())
    };
    let mut emit = |event: ExperimentProgressEvent| {
        if let Some(progress) = hooks.on_progress.as_mut() {
            progress(event);
        }
    };
    check("the first arm")?;
    let shared = runner.prepare(plan, clock)?;
    let mut arms = Vec::with_capacity(plan.arms.len());
    for arm in &plan.arms {
        check(&format!("arm {}", arm.arm.as_str()))?;
        emit(ExperimentProgressEvent::ArmStart {
            arm: arm.arm,
            arm_index: arm.index,
            arm_count: plan.arms.len(),
        });
        let arm_span = tracing::info_span!(
            "dream.experiment_arm",
            dream.experiment_id = %plan.experiment_id,
            dream.arm = arm.arm.as_str(),
            dream.fixed_policy = arm.arm.fixed_policy(),
            dream.guided = arm.arm.guided(),
            dream.run_id = tracing::field::Empty,
        );
        let run = {
            let _arm = arm_span.enter();
            let mut relay = |progress: ExperimentArmProgress| {
                emit(ExperimentProgressEvent::Phase {
                    arm: arm.arm,
                    progress,
                });
            };
            let run = runner.run(plan, arm, clock, shared.as_ref(), &mut relay)?;
            arm_span.record("dream.run_id", run.run_id.as_str());
            run
        };
        let result = build_arm_result(
            arm,
            modes
                .get(arm.index)
                .cloned()
                .unwrap_or_else(ExperimentArmMode::local),
            &run,
        );
        for row in &result.rounds {
            emit(ExperimentProgressEvent::Round {
                arm: arm.arm,
                round: row.round,
                round_best: row.round_best,
                cumulative_best: row.cumulative_best,
                cumulative_probes: row.cumulative_probes,
                tokens: row.tokens,
            });
        }
        emit(ExperimentProgressEvent::ArmEnd {
            arm: arm.arm,
            final_best: result.totals.final_best,
            total_probes: result.totals.probes,
        });
        arms.push(result);
    }
    check("writing the result")?;
    let result = ExperimentResult {
        schema: EXPERIMENT_SCHEMA,
        experiment_id: plan.experiment_id.clone(),
        task: DreamTaskIdName(plan.task_id),
        scoring: plan.task_id.scoring(),
        n: plan.n,
        seed: plan.seed.clone(),
        rounds: plan.rounds,
        budget: plan.budget,
        objective: plan.objective,
        initial_policy_id: policy_id(&plan.initial_policy),
        initial_policy: plan.initial_policy,
        headline: compute_headline(&arms),
        arms,
        shared_initial_rollout: shared.is_some(),
        created_ts: plan.created_ts,
        notes: plan.notes.clone(),
    };
    let result_path = write_experiment_result(&plan.dir, &result)?;
    emit(ExperimentProgressEvent::Completed {
        experiment_id: plan.experiment_id.clone(),
        result_path,
    });
    Ok(result)
}

/// The standalone, zero-token runner: the `dream` and `fixed` arms only.
///
/// # Errors
///
/// [`ExperimentError`] from planning or from an arm's store.
pub fn run_experiment(
    spec: &ExperimentSpec,
    options: &ExperimentRunOptions<'_>,
) -> Result<ExperimentResult, ExperimentError> {
    run_experiment_with_runner(spec, options, &mut LocalArmRunner, LOCAL_EXPERIMENT_ARMS)
}

fn is_count_or_absent(totals: &Map<String, Value>, key: &str) -> bool {
    totals.get(key).is_none_or(Value::is_number)
}

/// Structural check of a parsed result file (TS `isExperimentResult`): the
/// versioned schema plus the fields every reader relies on. Fields added to
/// schema 1 later are optional, so an older file still validates; a malformed
/// value does not.
#[must_use]
pub fn is_experiment_result(value: &Value) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    let string = |key: &str| record.get(key).is_some_and(Value::is_string);
    let object = |key: &str| record.get(key).is_some_and(Value::is_object);
    let array = |key: &str| record.get(key).is_some_and(Value::is_array);
    if record.get("schema").and_then(Value::as_str) != Some(EXPERIMENT_SCHEMA) {
        return false;
    }
    if !string("experimentId") || !string("task") {
        return false;
    }
    if record
        .get("scoring")
        .is_some_and(|scoring| !matches!(scoring.as_str(), Some("deterministic" | "timing")))
    {
        return false;
    }
    if !record.get("rounds").is_some_and(Value::is_number) || !object("budget") {
        return false;
    }
    if !string("initialPolicyId") || !object("initialPolicy") || !array("arms") || !array("notes") {
        return false;
    }
    if !record
        .get("headline")
        .is_some_and(|headline| headline.is_null() || headline.is_object())
    {
        return false;
    }
    if !record
        .get("sharedInitialRollout")
        .is_some_and(Value::is_boolean)
        || !record.get("createdTs").is_some_and(Value::is_number)
    {
        return false;
    }
    record["arms"].as_array().is_some_and(|arms| {
        arms.iter().all(|arm| {
            let Some(arm) = arm.as_object() else {
                return false;
            };
            let Some(totals) = arm.get("totals").and_then(Value::as_object) else {
                return false;
            };
            arm.get("arm")
                .and_then(Value::as_str)
                .is_some_and(|name| ExperimentArm::from_name(name).is_some())
                && arm.get("runId").is_some_and(Value::is_string)
                && arm.get("storeDir").is_some_and(Value::is_string)
                && arm.get("selectedPolicyId").is_none_or(Value::is_string)
                && arm.get("rounds").is_some_and(Value::is_array)
                && [
                    "agentGeneratedCalls",
                    "localFallbacks",
                    "llmProposals",
                    "llmAccepted",
                ]
                .iter()
                .all(|key| is_count_or_absent(totals, key))
                && totals.get("llmRejected").is_none_or(|rejected| {
                    rejected
                        .as_object()
                        .is_some_and(|counts| counts.values().all(Value::is_number))
                })
        })
    })
}

/// Read and validate a persisted result.
///
/// # Errors
///
/// [`DreamStoreError`] when it is missing, unparsable or of another schema.
pub fn read_experiment_result(dir: &Path, experiment_id: &str) -> Result<Value, DreamStoreError> {
    let path = experiment_result_path(dir, experiment_id);
    if !path.exists() {
        return Err(DreamStoreError::Message(format!(
            "no experiment {experiment_id} in {}",
            dir.display()
        )));
    }
    let text = std::fs::read_to_string(&path).map_err(|error| DreamStoreError::io(&path, error))?;
    let value = json::parse(&text).map_err(|_| {
        DreamStoreError::Message(format!(
            "experiment result {} is not valid JSON",
            path.display()
        ))
    })?;
    if !is_experiment_result(&value) {
        return Err(DreamStoreError::Message(format!(
            "experiment result {} is not a {EXPERIMENT_SCHEMA} document",
            path.display()
        )));
    }
    Ok(value)
}
