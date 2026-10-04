//! The full Dream-RSI orchestrator: synchronous, local, zero-token (TS `loop.ts`).
//!
//! One run is a `dream.run` span. Iteration 0 rolls out the initial policy;
//! each later iteration freezes the pool, dreams a no-worse policy over it
//! (`dream.dream`) and redeploys it (`dream.redeploy`). The first redeploy of
//! every adopted policy is a PROBATION: when its best falls below the
//! incumbent's lowest replay best on the pool it won on, the adoption is
//! reverted and the policy id revoked for the rest of the run. `fixed_policy`
//! is the paper's Recursive Fixed Exploration control: same loop, no dreaming.
//! Every score, shape and policy id is a function of the seed alone; the clock
//! reaches only the on-disk identity.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

use crate::dreams::{
    dreams_path, DreamProbationRecord, DreamStepInput, DreamsLog, DreamsLogContext,
};
use crate::improve::{
    run_dreaming, select_best_policy, CandidateInput, CandidateSource, CandidateVerdict,
    DreamResult, DreamerKind, DreamingOptions, DreamingScoreConfig, LeverScanRecord,
};
use crate::objective::ReplayObjectiveConfig;
use crate::policy::{policy_id, ExplorationPolicy};
use crate::proposer::ProposalTally;
use crate::rng::{Seed, SeededRng};
use crate::rollout::{
    improvements_of, run_online_exploration, DreamClock, ExploreOptions, ExploreResult,
    ScoreImprovement,
};
use crate::store::{list_trees, read_tree, DreamStoreError, RecordedTree};
use crate::task::DynTask;

/// A rollout's best may sit this far below the probation floor and still reach it.
pub const PROBATION_EPS: f64 = 1e-9;

/// Whether a run's proposer/dreamer are local or LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DreamMode {
    Local,
    Llm,
}

/// One loop's options.
pub struct DreamLoopOptions<'a> {
    pub task: &'a dyn DynTask,
    /// Task id recorded on every header.
    pub task_id: String,
    pub n: Option<u32>,
    pub seed: Seed,
    pub clock: DreamClock<'a>,
    pub workers: u32,
    pub k1: u32,
    pub k2: u32,
    /// Revised policies M per dreaming step.
    pub dreams: usize,
    /// Explore/dream/redeploy iterations after the initial rollout.
    pub iterations: u32,
    pub dir: &'a Path,
    pub objective: ReplayObjectiveConfig,
    /// Injected rng; a fresh one from `seed` when `None`.
    pub rng: Option<SeededRng>,
    pub initial_policy: ExplorationPolicy,
    /// The fixed-exploration control: never dream.
    pub fixed_policy: bool,
    /// The dreaming candidate source; the local dreamer when `None`.
    pub candidates: Option<&'a mut dyn CandidateSource>,
    /// A clock-free label folded into the run id (`<experimentId>/<arm>`).
    pub run_label: Option<String>,
    pub dreams_log_context: DreamsLogContext,
    /// Rolled out once each at iteration 0 on forks `prime:<i>`.
    pub priming_policies: Vec<ExplorationPolicy>,
}

/// Handler invocations per role (all zero on the local path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct DreamHandlerCalls {
    pub proposer: u64,
    pub dreamer: u64,
    pub guidance: u64,
}

/// Child token usage per role (all zero on the local path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct DreamRoundTokens {
    pub rollout: u64,
    pub dreamer: u64,
    pub guidance: u64,
}

/// What a dreaming step recorded on the round it chose the policy for.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamRoundDreaming {
    pub current_score: f64,
    pub chosen_score: f64,
    pub improved: bool,
    /// The proposed count.
    pub candidates: usize,
    pub candidate_verdicts: Vec<CandidateVerdict>,
    pub dreamer: DreamerKind,
    pub lever_scan: Option<LeverScanRecord>,
    pub measured_trees: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probation: Option<DreamProbationRecord>,
}

/// One rollout of a loop.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamRoundRecord {
    /// 0 is the initial rollout; the paper's round is `iteration + 1`.
    pub iteration: u32,
    pub tree_id: String,
    /// The policy that grew this tree.
    pub policy_id: String,
    pub round_best: f64,
    /// Evaluated attempts (priming probes included on round 1).
    pub probes: u32,
    pub agent_generated_calls: u32,
    pub proposals: ProposalTally,
    pub decision_rounds: u32,
    pub pool_size: usize,
    pub tokens: DreamRoundTokens,
    pub handler_calls: DreamHandlerCalls,
    pub dreaming: Option<DreamRoundDreaming>,
    pub probes_to_round_best: u32,
    pub improvements: Vec<ScoreImprovement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priming_tree_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priming_probes: Option<u32>,
}

/// One loop's outcome.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamLoopResult {
    pub run_id: String,
    pub task: String,
    pub seed: Seed,
    pub mode: DreamMode,
    pub iterations: u32,
    pub fixed_policy: bool,
    pub tree_ids: Vec<String>,
    pub rounds: Vec<DreamRoundRecord>,
    pub initial_policy_id: String,
    pub initial_policy_score: f64,
    pub final_policy: ExplorationPolicy,
    pub final_policy_id: String,
    pub final_policy_score: f64,
    pub improved: bool,
    pub best_node_score: f64,
    pub tokens: u64,
    /// Rollouts (priming excluded) whose stop rule fired before k1.
    pub stopped_early: u32,
    pub final_selection: Vec<CandidateVerdict>,
    pub probation_reverts: u32,
}

/// Judge the probation rollout of the policy `step` adopted over `incumbent`.
#[must_use]
pub fn judge_probation(
    step: &DreamResult,
    incumbent: &ExplorationPolicy,
    rollout: &ExploreResult,
) -> DreamProbationRecord {
    let winner = step
        .candidates
        .iter()
        .find(|verdict| verdict.reason == crate::improve::CandidateReason::Winner);
    DreamProbationRecord {
        policy_id: step.chosen_policy_id.clone(),
        incumbent_policy_id: policy_id(incumbent),
        tree_id: rollout.tree_id.clone(),
        round_best: rollout.best_score,
        floor: step.current_min_best,
        charged_probes: winner.map_or(step.current.charged_probes, |verdict| {
            verdict.charged_probes
        }),
        charged_rounds: winner.map_or(step.current.charged_rounds, |verdict| {
            verdict.charged_rounds
        }),
        incumbent_charged_probes: step.current.charged_probes,
        incumbent_charged_rounds: step.current.charged_rounds,
        evidence_trees: step.evidence_trees,
        reverted: rollout.best_score < step.current_min_best - PROBATION_EPS,
    }
}

/// Freeze the pool for a task: every recorded tree with that task id, sorted by tree id.
///
/// # Errors
///
/// [`DreamStoreError`] when a listed tree cannot be read.
pub fn freeze_pool(dir: &Path, task_id: &str) -> Result<Vec<RecordedTree>, DreamStoreError> {
    list_trees(dir)
        .into_iter()
        .filter(|summary| summary.task_id == task_id)
        .map(|summary| read_tree(&summary.tree_id, dir))
        .collect()
}

/// `<task>-s<seed>-r<clock>`, plus `-<label>` with characters outside
/// `[A-Za-z0-9._-]` (runs of them) replaced by `_`.
#[must_use]
pub fn dream_run_id(task_id: &str, seed: &Seed, clock_ms: u64, run_label: Option<&str>) -> String {
    let base = format!("{task_id}-s{seed}-r{clock_ms}");
    let Some(label) = run_label.filter(|label| !label.is_empty()) else {
        return base;
    };
    let mut safe = String::with_capacity(label.len());
    let mut in_run = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            safe.push(c);
            in_run = false;
        } else if !in_run {
            safe.push('_');
            in_run = true;
        }
    }
    format!("{base}-{safe}")
}

/// The tree id of the `index`-th priming rollout of a run.
#[must_use]
pub fn priming_tree_id(task_id: &str, seed: &Seed, index: usize, clock_ms: u64) -> String {
    format!("{task_id}-s{seed}-i0p{index}-{clock_ms}")
}

/// The round-1 curve over the initial rollout followed by each priming rollout.
#[must_use]
pub fn merged_round_curve(results: &[&ExploreResult]) -> (u32, Vec<ScoreImprovement>) {
    if let [only] = results {
        return (only.probes_to_best, only.improvements.clone());
    }
    let mut best_root: Option<f64> = None;
    let mut probes: Vec<(u32, f64, bool)> = Vec::new();
    let mut offset = 0;
    for result in results {
        for node in result.tree.all_nodes() {
            if node.parent_id.is_none() {
                if node.valid && best_root.is_none_or(|best| node.score > best) {
                    best_root = Some(node.score);
                }
                continue;
            }
            probes.push((offset + node.seq, node.score, node.valid));
        }
        offset += result.revealed_count;
    }
    let curve = improvements_of(
        best_root
            .map(|score| (0, score, true))
            .into_iter()
            .chain(probes),
    );
    (curve.last().map_or(0, |point| point.probe), curve)
}

/// Run the loop.
///
/// # Errors
///
/// [`DreamStoreError`] when a tree or log cannot be written or read.
#[allow(clippy::too_many_lines)] // one orchestration, kept whole as in the TS
pub fn run_dream_loop(
    mut options: DreamLoopOptions<'_>,
) -> Result<DreamLoopResult, DreamStoreError> {
    let task_id = options.task_id.clone();
    let rng = options
        .rng
        .take()
        .unwrap_or_else(|| SeededRng::new(&options.seed));
    let iterations = options.iterations;
    let initial_policy = options.initial_policy;
    let fixed_policy = options.fixed_policy;
    let k1 = options.k1.max(1);
    let clock = options.clock;
    let score_cfg = DreamingScoreConfig {
        k1: options.k1,
        k2: options.k2,
        objective: options.objective,
        quality_eps: 0.0,
    };
    let run_span = tracing::info_span!(
        "dream.run",
        dream.task = %task_id,
        dream.seed = %options.seed,
        dream.workers = options.workers,
        dream.k1 = options.k1,
        dream.k2 = options.k2,
        dream.dreams = options.dreams,
        dream.iterations = iterations,
        dream.mode = "local",
        dream.fixed_policy = fixed_policy,
        dream.priming_policies = options.priming_policies.len(),
        dream.run_id = tracing::field::Empty,
    );
    let _run = run_span.enter();
    let run_id = dream_run_id(
        &task_id,
        &options.seed,
        clock(),
        options.run_label.as_deref(),
    );
    run_span.record("dream.run_id", run_id.as_str());
    let dreams_log = DreamsLog::new(
        dreams_path(options.dir, &run_id),
        clock,
        options.dreams_log_context.clone(),
    );

    let rollout =
        |policy: &ExplorationPolicy, iteration: u32, fork: &str, tree_id: Option<String>| {
            run_online_exploration(ExploreOptions {
                task: options.task,
                task_id: task_id.clone(),
                n: options.n,
                seed: options.seed.clone(),
                rng: rng.fork(fork),
                clock,
                workers: options.workers,
                k1: options.k1,
                dir: options.dir,
                policy: *policy,
                iteration,
                proposer: None,
                tree_id,
            })
        };

    let mut tree_ids = Vec::new();
    let mut rounds: Vec<DreamRoundRecord> = Vec::new();
    let mut chosen_policies: Vec<CandidateInput> = Vec::new();
    let mut best_node_score: Option<f64> = None;
    let mut tokens = 0;
    let mut stopped_early = 0;
    let mut record = |result: &ExploreResult,
                      policy: &ExplorationPolicy,
                      iteration: u32,
                      pool_size: usize,
                      dreaming: Option<DreamRoundDreaming>,
                      primed: &[ExploreResult]| {
        tree_ids.push(result.tree_id.clone());
        let mut note_best = |score: f64| {
            if best_node_score.is_none_or(|best| score > best) {
                best_node_score = Some(score);
            }
        };
        tokens += result.tokens;
        note_best(result.best_score);
        if result.rounds < k1 {
            stopped_early += 1;
        }
        let mut probes = result.revealed_count;
        let mut round_best = result.best_score;
        let mut priming_probes = 0;
        for prime in primed {
            tokens += prime.tokens;
            note_best(prime.best_score);
            probes += prime.revealed_count;
            priming_probes += prime.revealed_count;
            if prime.best_score > round_best {
                round_best = prime.best_score;
            }
        }
        let all: Vec<&ExploreResult> = std::iter::once(result).chain(primed).collect();
        let (probes_to_round_best, improvements) = merged_round_curve(&all);
        rounds.push(DreamRoundRecord {
            iteration,
            tree_id: result.tree_id.clone(),
            policy_id: policy_id(policy),
            round_best,
            probes,
            agent_generated_calls: result.agent_generated_count,
            proposals: ProposalTally::default(),
            decision_rounds: result.rounds,
            pool_size,
            tokens: DreamRoundTokens {
                rollout: result.tokens + primed.iter().map(|prime| prime.tokens).sum::<u64>(),
                dreamer: 0,
                guidance: 0,
            },
            handler_calls: DreamHandlerCalls::default(),
            dreaming,
            probes_to_round_best,
            improvements,
            priming_tree_ids: (!primed.is_empty())
                .then(|| primed.iter().map(|prime| prime.tree_id.clone()).collect()),
            priming_probes: (!primed.is_empty()).then_some(priming_probes),
        });
    };

    let initial = rollout(&initial_policy, 0, "iter:0", None)?;
    let mut primed = Vec::with_capacity(options.priming_policies.len());
    for (index, policy) in options.priming_policies.iter().enumerate() {
        let tree_id = priming_tree_id(&task_id, &options.seed, index, clock());
        primed.push(rollout(
            policy,
            0,
            &format!("prime:{index}"),
            Some(tree_id),
        )?);
    }
    record(&initial, &initial_policy, 0, 0, None, &primed);

    let mut current = initial_policy;
    let mut revoked: HashSet<String> = HashSet::new();
    let mut probation_reverts = 0;
    for iteration in 1..=iterations {
        let mut pool_size = usize::try_from(iteration).unwrap_or(usize::MAX) + primed.len();
        let mut dreaming = None;
        let incumbent = current;
        let mut adopted: Option<DreamResult> = None;
        if !fixed_policy {
            let pool = freeze_pool(options.dir, &task_id)?;
            pool_size = pool.len();
            let dream = run_dreaming(DreamingOptions {
                current,
                pool: &pool,
                dreams: options.dreams,
                k1: options.k1,
                k2: options.k2,
                rng: rng.fork(&format!("dream:{iteration}")),
                objective: options.objective,
                quality_eps: 0.0,
                iteration,
                candidates: options.candidates.as_deref_mut(),
                lever_scan: true,
                revoked: &revoked,
            });
            dreams_log.record_step(&DreamStepInput {
                iteration: i64::from(iteration),
                pool_size,
                candidates: &dream.candidates,
                current_score: dream.current_score,
                chosen_policy: &dream.chosen_policy,
                improved: dream.improved,
                dreamer: dream.dreamer,
                measured_trees: dream.measured_trees,
                lever_scan: dream.lever_scan.as_ref(),
            })?;
            current = dream.chosen_policy;
            chosen_policies.push(CandidateInput::from(current));
            dreaming = Some(DreamRoundDreaming {
                current_score: dream.current_score,
                chosen_score: dream.chosen_score,
                improved: dream.improved,
                candidates: dream.candidate_policy_ids.len(),
                candidate_verdicts: dream.candidates.clone(),
                dreamer: dream.dreamer,
                lever_scan: dream.lever_scan.clone(),
                measured_trees: dream.measured_trees,
                probation: None,
            });
            if dream.improved {
                adopted = Some(dream);
            }
        }
        let deployed = current;
        let redeploy_span = tracing::info_span!(
            "dream.redeploy",
            dream.policy_id = %policy_id(&deployed),
            dream.k1 = options.k1,
            dream.workers = options.workers,
            dream.iteration = iteration,
            dream.fixed_policy = fixed_policy,
            dream.probation = adopted.is_some(),
            dream.tree_id = tracing::field::Empty,
            dream.probation_floor = tracing::field::Empty,
            dream.reverted = tracing::field::Empty,
        );
        let (result, probation) = {
            let _redeploy = redeploy_span.enter();
            let result = rollout(&deployed, iteration, &format!("iter:{iteration}"), None)?;
            redeploy_span.record("dream.tree_id", result.tree_id.as_str());
            let probation = adopted
                .as_ref()
                .map(|step| judge_probation(step, &incumbent, &result));
            if let Some(probation) = &probation {
                redeploy_span.record("dream.probation_floor", probation.floor);
                redeploy_span.record("dream.reverted", probation.reverted);
            }
            (result, probation)
        };
        if let (Some(probation), Some(dreaming)) = (probation, dreaming.as_mut()) {
            dreams_log.record_probation(i64::from(iteration), &probation)?;
            if probation.reverted {
                revoked.insert(probation.policy_id.clone());
                current = incumbent;
                probation_reverts += 1;
            }
            dreaming.probation = Some(probation);
        }
        record(&result, &deployed, iteration, pool_size, dreaming, &[]);
    }

    let final_pool = freeze_pool(options.dir, &task_id)?;
    let selection = select_best_policy(
        &initial_policy,
        &chosen_policies,
        &final_pool,
        &score_cfg,
        &revoked,
    );
    dreams_log.record_step(&DreamStepInput {
        iteration: -1,
        pool_size: final_pool.len(),
        candidates: &selection.candidates,
        current_score: selection.current_score,
        chosen_policy: &selection.chosen_policy,
        improved: selection.improved,
        dreamer: selection.dreamer,
        measured_trees: selection.measured_trees,
        lever_scan: None,
    })?;
    Ok(DreamLoopResult {
        run_id,
        task: task_id,
        seed: options.seed.clone(),
        mode: DreamMode::Local,
        iterations,
        fixed_policy,
        tree_ids,
        rounds,
        initial_policy_id: policy_id(&initial_policy),
        initial_policy_score: selection.current_score,
        final_policy_id: policy_id(&selection.chosen_policy),
        final_policy: selection.chosen_policy,
        final_policy_score: selection.chosen_score,
        improved: selection.improved,
        best_node_score: best_node_score.unwrap_or(0.0),
        tokens,
        stopped_early,
        final_selection: selection.candidates,
        probation_reverts,
    })
}
