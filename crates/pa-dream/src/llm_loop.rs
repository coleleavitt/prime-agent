//! The in-session Dream-RSI orchestrator (TS `runDreamLoopWithAgent` and
//! `dreamLoopBody` in `llm.ts`).
//!
//! It mirrors the local loop (`dream_loop`) and adds what the LLM path needs:
//! the LLM proposer and dreamer as INDEPENDENT toggles, the semantic-guidance
//! ablation, per-round accounting (handler calls and tokens per role, the
//! proposer tally), the dreamer's verdict history (a reverted adoption is
//! shown as `revoked`), a shared round 1 (`initial_rollout`), cancellation,
//! and observability-only progress. Because a run outlives the turn that
//! started it, `dream.run` is a DETACHED root span carrying the launching
//! turn's `trigger.trace_id`.
//!
//! With both toggles off it spends no token and calls no runner; its trees
//! are byte-identical to the local loop's for the same seed and clock.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio_util::sync::CancellationToken;
use tracing::field::Empty;

use crate::child::DEFAULT_CHILD_TOKEN_BUDGET;
use crate::child::{ChildRuntimeScope, RunAgent, RunAgentOptions, RunAgentRequest, RunAgentResult};
use crate::dream_loop::{
    dream_run_id, freeze_pool, judge_probation, merged_round_curve, priming_tree_id,
    DreamHandlerCalls, DreamLoopResult, DreamMode, DreamRoundDreaming, DreamRoundRecord,
    DreamRoundTokens,
};
use crate::dreams::{dreams_path, DreamStepInput, DreamsLog, DreamsLogContext};
use crate::improve::{
    run_dreaming, select_best_policy, CandidateInput, CandidateReason, CandidateSource,
    DreamResult, DreamingOptions, DreamingScoreConfig,
};
use crate::llm::{
    build_guidance_input, dream_abort, history_of, propose_policies_with_agent, resolve_guidance,
    revoked_history_entry, DreamHistoryEntry, DreamerContext, LlmDreamerOptions, LlmProposer,
    LlmProposerOptions, SemanticGuidance, DEFAULT_GUIDANCE_MAX_ARTIFACT_CHARS,
    DEFAULT_GUIDANCE_TOP_K,
};
use crate::objective::ReplayObjectiveConfig;
use crate::policy::{policy_id, ExplorationPolicy};
use crate::proposer::{ProposalTally, Proposer};
use crate::rejections::{rejections_path, RejectionLog};
use crate::rng::{Seed, SeededRng};
use crate::rollout::{
    run_online_exploration, DreamClock, ExploreOptions, ExploreResult, ScoreImprovement,
};
use crate::store::{tree_path, DreamStoreError, RecordedTree};
use crate::task::DynTask;

/// Which phase of the loop just began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamPhase {
    Rollout,
    Dreaming,
    Redeploying,
}

impl DreamPhase {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rollout => "rollout",
            Self::Dreaming => "dreaming",
            Self::Redeploying => "redeploying",
        }
    }
}

/// Observability-only progress (TS `DreamProgressEvent`): it never touches
/// the rng, the tree, scoring or persistence.
#[derive(Debug, Clone, PartialEq)]
pub enum DreamProgressEvent {
    Phase {
        phase: DreamPhase,
        iteration: u32,
        best_node_score: f64,
        tree_id: Option<String>,
    },
    Completed {
        iteration: u32,
        best_node_score: f64,
        final_policy_score: f64,
        improved: bool,
    },
}

/// A round-1 rollout performed elsewhere (once per experiment, copied into
/// every arm's store; TS `DreamInitialRollout` / `ExperimentSharedRollout`).
/// The loop builds its iteration-0 record from it; the trees MUST already be
/// in the loop's store.
#[derive(Debug, Clone, PartialEq)]
pub struct DreamInitialRollout {
    pub tree_id: String,
    /// The round's best: max over the initial and any priming rollouts.
    pub best_score: f64,
    /// The initial rollout's revealed non-root nodes.
    pub revealed_count: u32,
    /// Probes a child generated, summed over the shared rollouts.
    pub agent_generated_count: u32,
    pub proposals: ProposalTally,
    /// The initial rollout's online decision rounds.
    pub rounds: u32,
    pub tokens: u64,
    pub handler_calls: DreamHandlerCalls,
    pub probes_to_best: u32,
    pub improvements: Vec<ScoreImprovement>,
    pub priming_tree_ids: Option<Vec<String>>,
    pub priming_probes: Option<u32>,
}

/// Fold the initial rollout and its priming rollouts into one round-1 record's
/// facts (TS `mergePrimedRollouts`); calls and the tally are the caller's.
#[must_use]
pub fn merge_primed_rollouts(
    initial: &ExploreResult,
    primed: &[ExploreResult],
) -> DreamInitialRollout {
    let all: Vec<&ExploreResult> = std::iter::once(initial).chain(primed).collect();
    let (probes_to_best, improvements) = merged_round_curve(&all);
    let mut best_score = initial.best_score;
    let mut priming_probes = 0;
    let mut tokens = initial.tokens;
    let mut agent_generated_count = initial.agent_generated_count;
    for prime in primed {
        if prime.best_score > best_score {
            best_score = prime.best_score;
        }
        priming_probes += prime.revealed_count;
        tokens += prime.tokens;
        agent_generated_count += prime.agent_generated_count;
    }
    DreamInitialRollout {
        tree_id: initial.tree_id.clone(),
        best_score,
        revealed_count: initial.revealed_count,
        agent_generated_count,
        proposals: ProposalTally::default(),
        rounds: initial.rounds,
        tokens,
        handler_calls: DreamHandlerCalls::default(),
        probes_to_best,
        improvements,
        priming_tree_ids: (!primed.is_empty())
            .then(|| primed.iter().map(|prime| prime.tree_id.clone()).collect()),
        priming_probes: (!primed.is_empty()).then_some(priming_probes),
    }
}

/// A runner that counts every invocation (retries included).
pub(crate) struct CountingRunner<'a> {
    inner: &'a dyn RunAgent,
    calls: AtomicU64,
}

impl<'a> CountingRunner<'a> {
    pub(crate) fn new(inner: &'a dyn RunAgent) -> Self {
        Self {
            inner,
            calls: AtomicU64::new(0),
        }
    }

    /// The invocations since the last call, resetting the count.
    pub(crate) fn take(&self) -> u64 {
        self.calls.swap(0, Ordering::Relaxed)
    }
}

impl RunAgent for CountingRunner<'_> {
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.run(request, options)
    }
}

/// A dreaming step whose candidates were resolved before it (the LLM dreamer's).
struct ResolvedCandidates(Vec<CandidateInput>);

impl CandidateSource for ResolvedCandidates {
    fn propose(
        &mut self,
        _current: &ExplorationPolicy,
        _m: usize,
        _rng: &SeededRng,
    ) -> Vec<CandidateInput> {
        std::mem::take(&mut self.0)
    }
}

/// One in-session loop's options.
pub struct DreamLoopWithAgentOptions<'a> {
    pub runner: &'a dyn RunAgent,
    pub task: &'a dyn DynTask,
    /// Task id recorded on every header.
    pub task_id: String,
    pub n: Option<u32>,
    pub seed: Seed,
    pub clock: DreamClock<'a>,
    pub workers: u32,
    pub k1: u32,
    pub k2: u32,
    pub dreams: usize,
    pub iterations: u32,
    pub dir: &'a Path,
    pub objective: ReplayObjectiveConfig,
    /// Injected rng; a fresh one from `seed` when `None`.
    pub rng: Option<SeededRng>,
    pub initial_policy: ExplorationPolicy,
    /// The fixed-exploration control: never dream.
    pub fixed_policy: bool,
    /// A shared round 1; when set, iteration 0 performs no rollout.
    pub initial_rollout: Option<DreamInitialRollout>,
    /// The semantic-guidance ablation (requires `use_llm_proposer`).
    pub semantic_guidance: Option<SemanticGuidance>,
    pub use_llm_proposer: bool,
    pub use_llm_dreamer: bool,
    /// The scope the proposer, dreamer and guidance children share.
    pub scope: ChildRuntimeScope,
    /// The run's cancellation; a cancel stops the run with [`DreamStoreError::Aborted`].
    pub cancel: CancellationToken,
    /// Task contract appended to the proposer prompt.
    pub proposer_prompt_context: Option<String>,
    /// Per-attempt child token budget; [`DEFAULT_CHILD_TOKEN_BUDGET`] when `None`.
    pub child_token_budget: Option<u64>,
    /// Observability-only per-phase progress.
    pub on_progress: Option<&'a mut dyn FnMut(DreamProgressEvent)>,
    /// A clock-free label folded into the run id and every per-run log key.
    pub run_label: Option<String>,
    pub dreams_log_context: DreamsLogContext,
    /// Rolled out once each at iteration 0 on forks `prime:<i>` (ignored with `initial_rollout`).
    pub priming_policies: Vec<ExplorationPolicy>,
    /// The launching turn's trace id, stamped on the detached `dream.run` root.
    pub trigger_trace_id: Option<String>,
}

/// Run the in-session loop.
///
/// # Errors
///
/// [`DreamStoreError::Aborted`] when the run was cancelled (the span records
/// `dream.stopped: aborted`); [`DreamStoreError::Message`] for a configuration
/// error (semantic guidance without the LLM proposer, checked before any span,
/// child call or file) or a missing shared tree; a store error otherwise.
pub fn run_dream_loop_with_agent(
    options: DreamLoopWithAgentOptions<'_>,
) -> Result<DreamLoopResult, DreamStoreError> {
    if options.semantic_guidance.is_some() && !options.use_llm_proposer {
        return Err(DreamStoreError::Message(
            "semanticGuidance requires useLlmProposer".to_string(),
        ));
    }
    let priming = if options.initial_rollout.is_some() {
        0
    } else {
        options.priming_policies.len()
    };
    let shared_priming = options
        .initial_rollout
        .as_ref()
        .and_then(|shared| shared.priming_tree_ids.as_ref())
        .map_or(0, Vec::len);
    let span = tracing::info_span!(
        parent: None,
        "dream.run",
        dream.task = %options.task_id,
        dream.seed = %options.seed,
        dream.workers = options.workers,
        dream.k1 = options.k1,
        dream.k2 = options.k2,
        dream.dreams = options.dreams,
        dream.iterations = options.iterations,
        dream.mode = "llm",
        dream.fixed_policy = options.fixed_policy,
        dream.priming_policies = priming + shared_priming,
        dream.child_model = Empty,
        dream.child_thinking = Empty,
        dream.child_max_output_tokens = Empty,
        trigger.trace_id = Empty,
        dream.run_id = Empty,
        dream.stopped = Empty,
        error = Empty,
    );
    if let Some(model) = options
        .scope
        .model
        .as_deref()
        .filter(|model| !model.is_empty())
    {
        span.record("dream.child_model", model);
    }
    if let Some(thinking) = options.scope.thinking_level.as_deref() {
        span.record("dream.child_thinking", thinking);
    }
    if let Some(cap) = options.scope.max_output_tokens {
        span.record("dream.child_max_output_tokens", cap);
    }
    if let Some(trigger) = options.trigger_trace_id.as_deref() {
        span.record("trigger.trace_id", trigger);
    }
    let result = {
        let _entered = span.enter();
        loop_body(options, &span)
    };
    if let Err(error) = &result {
        if error.is_abort() {
            span.record("dream.stopped", "aborted");
        } else {
            span.record("error", error.to_string().as_str());
        }
    }
    result
}

/// The run's records so far.
#[derive(Default)]
struct Ledger {
    tree_ids: Vec<String>,
    rounds: Vec<DreamRoundRecord>,
    best_node_score: Option<f64>,
    total_tokens: u64,
    stopped_early: u32,
}

impl Ledger {
    fn best(&self) -> f64 {
        self.best_node_score.unwrap_or(0.0)
    }

    /// Record one round and reset the round's accounting.
    #[allow(clippy::too_many_arguments)] // one record's facts, as the TS closure takes them
    fn record(
        &mut self,
        facts: &DreamInitialRollout,
        policy: &ExplorationPolicy,
        iteration: u32,
        pool_size: usize,
        dreaming: Option<DreamRoundDreaming>,
        account: &mut RoundAccount,
        handler_calls: DreamHandlerCalls,
        k1: u32,
    ) {
        self.tree_ids.push(facts.tree_id.clone());
        let tokens = account.tokens;
        self.total_tokens += tokens.rollout + tokens.dreamer + tokens.guidance;
        if self
            .best_node_score
            .is_none_or(|best| facts.best_score > best)
        {
            self.best_node_score = Some(facts.best_score);
        }
        if facts.rounds < k1 {
            self.stopped_early += 1;
        }
        self.rounds.push(DreamRoundRecord {
            iteration,
            tree_id: facts.tree_id.clone(),
            policy_id: policy_id(policy),
            round_best: facts.best_score,
            probes: facts.revealed_count + facts.priming_probes.unwrap_or(0),
            agent_generated_calls: facts.agent_generated_count,
            proposals: account.tally,
            decision_rounds: facts.rounds,
            pool_size,
            tokens,
            handler_calls,
            dreaming,
            probes_to_round_best: facts.probes_to_best,
            improvements: facts.improvements.clone(),
            priming_tree_ids: facts.priming_tree_ids.clone(),
            priming_probes: facts
                .priming_tree_ids
                .as_ref()
                .map(|_| facts.priming_probes.unwrap_or(0)),
        });
        *account = RoundAccount::default();
    }
}

/// Per-round accounting, snapshot and reset by every record.
#[derive(Default)]
struct RoundAccount {
    tokens: DreamRoundTokens,
    tally: ProposalTally,
    shared_calls: Option<DreamHandlerCalls>,
}

#[allow(clippy::too_many_lines)] // one orchestration, kept whole as in the TS
fn loop_body(
    mut options: DreamLoopWithAgentOptions<'_>,
    run_span: &tracing::Span,
) -> Result<DreamLoopResult, DreamStoreError> {
    let task_id = options.task_id.clone();
    let objective = options.objective;
    let rng = options
        .rng
        .take()
        .unwrap_or_else(|| SeededRng::new(&options.seed));
    let score_cfg = DreamingScoreConfig {
        k1: options.k1,
        k2: options.k2,
        objective,
        quality_eps: 0.0,
    };
    let child_token_budget = options
        .child_token_budget
        .unwrap_or(DEFAULT_CHILD_TOKEN_BUDGET);
    let initial_policy = options.initial_policy;
    let k1 = options.k1.max(1);
    let iterations = options.iterations;
    let fixed_policy = options.fixed_policy;
    let clock = options.clock;
    let dir = options.dir;
    let cancel = options.cancel.clone();
    let run_id = dream_run_id(
        &task_id,
        &options.seed,
        clock(),
        options.run_label.as_deref(),
    );
    run_span.record("dream.run_id", run_id.as_str());

    let proposer_runner = CountingRunner::new(options.runner);
    let dreamer_runner = CountingRunner::new(options.runner);
    let guidance_runner = CountingRunner::new(options.runner);
    let rejections = (options.use_llm_proposer || options.use_llm_dreamer)
        .then(|| RejectionLog::new(rejections_path(dir, &run_id), clock));
    let dreams_log = DreamsLog::new(
        dreams_path(dir, &run_id),
        clock,
        options.dreams_log_context.clone(),
    );
    let mut guidance_option = options.semantic_guidance.take();
    let mut progress = options.on_progress.take();
    let mut emit = |event: DreamProgressEvent| {
        if let Some(progress) = progress.as_mut() {
            progress(event);
        }
    };

    let mut account = RoundAccount::default();
    let rollout = |policy: &ExplorationPolicy,
                   iteration: u32,
                   guidance: &str,
                   fork: &str,
                   tree_id: Option<String>,
                   account: &mut RoundAccount|
     -> Result<ExploreResult, DreamStoreError> {
        if cancel.is_cancelled() {
            return Err(dream_abort("dream run aborted before rollout"));
        }
        let mut llm = options.use_llm_proposer.then(|| {
            LlmProposer::new(
                &proposer_runner,
                options.task,
                LlmProposerOptions {
                    scope: &options.scope,
                    cancel: &cancel,
                    token_budget: child_token_budget,
                    prompt_context: options.proposer_prompt_context.clone(),
                    guidance: (!guidance.is_empty()).then(|| guidance.to_string()),
                    rejections: rejections.as_ref(),
                    iteration,
                },
            )
        });
        let result = run_online_exploration(ExploreOptions {
            task: options.task,
            task_id: task_id.clone(),
            n: options.n,
            seed: options.seed.clone(),
            rng: rng.fork(fork),
            clock,
            workers: options.workers,
            k1: options.k1,
            dir,
            policy: *policy,
            iteration,
            proposer: llm.as_mut().map(|proposer| proposer as &mut dyn Proposer),
            tree_id,
            cancel: Some(&cancel),
        });
        // A rollout cut short by an abort still spent what its tally holds.
        if let Some(proposer) = &llm {
            account.tally = account.tally.plus(&proposer.tally);
        }
        result
    };

    let mut ledger = Ledger::default();
    let mut chosen_policies: Vec<CandidateInput> = Vec::new();
    let mut history: Vec<DreamHistoryEntry> = Vec::new();
    let counters = [&proposer_runner, &dreamer_runner, &guidance_runner];
    let record = |ledger: &mut Ledger,
                  facts: &DreamInitialRollout,
                  policy: &ExplorationPolicy,
                  iteration: u32,
                  pool_size: usize,
                  dreaming: Option<DreamRoundDreaming>,
                  account: &mut RoundAccount| {
        let calls = account.shared_calls.take().unwrap_or(DreamHandlerCalls {
            proposer: counters[0].take(),
            dreamer: counters[1].take(),
            guidance: counters[2].take(),
        });
        ledger.record(
            facts, policy, iteration, pool_size, dreaming, account, calls, k1,
        );
    };

    let priming_count;
    if let Some(shared) = options.initial_rollout.take() {
        if !tree_path(&shared.tree_id, dir).exists() {
            return Err(DreamStoreError::Message(format!(
                "shared initial rollout {} is not in the store {}",
                shared.tree_id,
                dir.display()
            )));
        }
        for prime_id in shared.priming_tree_ids.iter().flatten() {
            if !tree_path(prime_id, dir).exists() {
                return Err(DreamStoreError::Message(format!(
                    "shared priming rollout {prime_id} is not in the store {}",
                    dir.display()
                )));
            }
        }
        if cancel.is_cancelled() {
            return Err(dream_abort("dream run aborted before rollout"));
        }
        priming_count = shared.priming_tree_ids.as_ref().map_or(0, Vec::len);
        account.shared_calls = Some(shared.handler_calls);
        account.tokens.rollout = shared.tokens;
        account.tally = shared.proposals;
        record(
            &mut ledger,
            &shared,
            &initial_policy,
            0,
            0,
            None,
            &mut account,
        );
    } else {
        let first = rollout(&initial_policy, 0, "", "iter:0", None, &mut account)?;
        let mut primed = Vec::with_capacity(options.priming_policies.len());
        for (index, policy) in options.priming_policies.iter().enumerate() {
            let tree_id = priming_tree_id(&task_id, &options.seed, index, clock());
            primed.push(rollout(
                policy,
                0,
                "",
                &format!("prime:{index}"),
                Some(tree_id),
                &mut account,
            )?);
        }
        priming_count = primed.len();
        let merged = merge_primed_rollouts(&first, &primed);
        account.tokens.rollout = merged.tokens;
        record(
            &mut ledger,
            &merged,
            &initial_policy,
            0,
            0,
            None,
            &mut account,
        );
    }
    emit(DreamProgressEvent::Phase {
        phase: DreamPhase::Rollout,
        iteration: 0,
        best_node_score: ledger.best(),
        tree_id: ledger.tree_ids.first().cloned(),
    });

    let mut current = initial_policy;
    let mut revoked: HashSet<String> = HashSet::new();
    let mut probation_reverts = 0;
    for iteration in 1..=iterations {
        if cancel.is_cancelled() {
            return Err(dream_abort("dream run aborted before iteration"));
        }
        let incumbent = current;
        let mut adopted: Option<DreamResult> = None;
        let mut pool: Option<Vec<RecordedTree>> = None;
        let mut guidance = String::new();
        if let Some(option) = guidance_option.as_mut() {
            let frozen = freeze_pool(dir, &task_id)?;
            let produced = resolve_guidance(
                &guidance_runner,
                option,
                &build_guidance_input(
                    &frozen,
                    &task_id,
                    iteration,
                    DEFAULT_GUIDANCE_TOP_K,
                    DEFAULT_GUIDANCE_MAX_ARTIFACT_CHARS,
                ),
                &options.scope,
                &cancel,
                child_token_budget,
            )?;
            guidance = produced.text;
            account.tokens.guidance = produced.tokens;
            pool = Some(frozen);
        }
        let mut pool_size = pool.as_ref().map_or(
            usize::try_from(iteration).unwrap_or(usize::MAX) + priming_count,
            Vec::len,
        );
        let mut dreaming = None;
        if !fixed_policy {
            emit(DreamProgressEvent::Phase {
                phase: DreamPhase::Dreaming,
                iteration,
                best_node_score: ledger.best(),
                tree_id: None,
            });
            let pool = match pool.take() {
                Some(pool) => pool,
                None => freeze_pool(dir, &task_id)?,
            };
            pool_size = pool.len();
            let mut resolved = None;
            if options.use_llm_dreamer {
                let dreamed = propose_policies_with_agent(
                    &dreamer_runner,
                    &current,
                    options.dreams,
                    &LlmDreamerOptions {
                        scope: &options.scope,
                        cancel: &cancel,
                        token_budget: child_token_budget,
                        local_fallback_rng: rng.fork(&format!("dream-fallback:{iteration}")),
                        context: DreamerContext {
                            iteration,
                            pool: &pool,
                            objective: Some(objective),
                            workers: Some(options.workers),
                            k1: Some(options.k1),
                            k2: Some(options.k2),
                            history: &history,
                        },
                        rejections: rejections.as_ref(),
                    },
                )?;
                account.tokens.dreamer = dreamed.tokens;
                resolved = Some(ResolvedCandidates(dreamed.candidates));
            }
            let dream = run_dreaming(DreamingOptions {
                current,
                pool: &pool,
                dreams: options.dreams,
                k1: options.k1,
                k2: options.k2,
                rng: rng.fork(&format!("dream:{iteration}")),
                objective,
                quality_eps: 0.0,
                iteration,
                candidates: resolved
                    .as_mut()
                    .map(|resolved| resolved as &mut dyn CandidateSource),
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
            history.extend(history_of(iteration, &dream.candidates));
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
        if cancel.is_cancelled() {
            return Err(dream_abort("dream run aborted before redeploy"));
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
            dream.tree_id = Empty,
            dream.probation_floor = Empty,
            dream.reverted = Empty,
        );
        let (result, probation) = {
            let _redeploy = redeploy_span.enter();
            let result = rollout(
                &deployed,
                iteration,
                &guidance,
                &format!("iter:{iteration}"),
                None,
                &mut account,
            )?;
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
        if let (Some(probation), Some(dreaming), Some(step)) =
            (probation, dreaming.as_mut(), adopted.as_ref())
        {
            dreams_log.record_probation(i64::from(iteration), &probation)?;
            if probation.reverted {
                revoked.insert(probation.policy_id.clone());
                current = incumbent;
                probation_reverts += 1;
                if let Some(winner) = step
                    .candidates
                    .iter()
                    .find(|verdict| verdict.reason == CandidateReason::Winner)
                {
                    history.push(revoked_history_entry(iteration, winner, &probation));
                }
            }
            dreaming.probation = Some(probation);
        }
        account.tokens.rollout = result.tokens;
        let facts = merge_primed_rollouts(&result, &[]);
        let tree_id = result.tree_id.clone();
        record(
            &mut ledger,
            &facts,
            &deployed,
            iteration,
            pool_size,
            dreaming,
            &mut account,
        );
        emit(DreamProgressEvent::Phase {
            phase: DreamPhase::Redeploying,
            iteration,
            best_node_score: ledger.best(),
            tree_id: Some(tree_id),
        });
    }

    let final_pool = freeze_pool(dir, &task_id)?;
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
    let best_node_score = ledger.best();
    emit(DreamProgressEvent::Completed {
        iteration: iterations,
        best_node_score,
        final_policy_score: selection.chosen_score,
        improved: selection.improved,
    });
    Ok(DreamLoopResult {
        run_id,
        task: task_id,
        seed: options.seed.clone(),
        mode: DreamMode::Llm,
        iterations,
        fixed_policy,
        tree_ids: ledger.tree_ids,
        rounds: ledger.rounds,
        initial_policy_id: policy_id(&initial_policy),
        initial_policy_score: selection.current_score,
        final_policy_id: policy_id(&selection.chosen_policy),
        final_policy: selection.chosen_policy,
        final_policy_score: selection.chosen_score,
        improved: selection.improved,
        best_node_score,
        tokens: ledger.total_tokens,
        stopped_early: ledger.stopped_early,
        final_selection: selection.candidates,
        probation_reverts,
    })
}
