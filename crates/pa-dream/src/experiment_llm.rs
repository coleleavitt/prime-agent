//! The in-session, token-spending arm runner for Dream-RSI experiments (TS
//! `experiment-llm.ts`).
//!
//! [`AgentArmRunner`] drives every arm through
//! [`run_dream_loop_with_agent`](crate::llm_loop::run_dream_loop_with_agent)
//! with the arm's `fixed_policy` / guidance settings, so the four arms
//! (`dream`, `fixed`, `dream-guided`, `fixed-guided`) differ ONLY in whether
//! they dream and whether the proposer prompt carries guidance.
//!
//! Round 1 is shared: `prepare` rolls it out ONCE (the initial policy on the
//! exact `SeededRng::new(seed).fork("iter:0")` stream every arm's loop would
//! use, plus any priming rollouts) into the first arm's store, copies the
//! trees byte for byte into every other arm's store with `copy_tree`, and
//! hands every arm the same round-1 record, so round 1 is identical across
//! arms even though the LLM proposer is not deterministic. Its handler calls,
//! tally and tokens are charged to every arm's round 1; its rejections are
//! logged once, under the first arm's store, as `<experimentId>-shared`.

use tokio_util::sync::CancellationToken;

use crate::child::{ChildRuntimeScope, RunAgent, DEFAULT_CHILD_TOKEN_BUDGET};
use crate::dream_loop::{priming_tree_id, DreamHandlerCalls, DreamLoopResult};
use crate::dreams::DreamsLogContext;
use crate::experiment::{
    run_experiment_with_hooks, ExperimentArm, ExperimentArmMode, ExperimentArmPlan,
    ExperimentArmProgress, ExperimentArmRunner, ExperimentError, ExperimentHooks, ExperimentPlan,
    ExperimentResult, ExperimentRunOptions, ExperimentSpec, EXPERIMENT_ARMS,
};
use crate::llm::{dream_abort, LlmProposer, LlmProposerOptions, SemanticGuidance};
use crate::llm_loop::{
    merge_primed_rollouts, run_dream_loop_with_agent, CountingRunner, DreamInitialRollout,
    DreamLoopWithAgentOptions, DreamProgressEvent,
};
use crate::policy::ExplorationPolicy;
use crate::proposer::Proposer;
use crate::rejections::{rejections_path, RejectionLog};
use crate::rng::SeededRng;
use crate::rollout::{run_online_exploration, DreamClock, ExploreOptions, ExploreResult};
use crate::store::{copy_tree, DreamStoreError};
use crate::tasks::{resolve_task_n, task_prompt_context};

/// Guided arms are the semantic-guidance ablation of the LLM PROPOSER.
///
/// # Errors
///
/// [`ExperimentError::ArmUnavailable`] naming the guided arms when the
/// proposer is local.
pub fn assert_guided_arms_served(
    arms: &[ExperimentArm],
    use_llm_proposer: bool,
) -> Result<(), ExperimentError> {
    let guided: Vec<&str> = arms
        .iter()
        .filter(|arm| arm.guided())
        .map(|arm| arm.as_str())
        .collect();
    if !guided.is_empty() && !use_llm_proposer {
        return Err(ExperimentError::ArmUnavailable(format!(
            "{} require useLlmProposer: the semantic-guidance ablation prefixes the LLM proposer prompt, so it has no meaning on the local proposer",
            guided.join("/")
        )));
    }
    Ok(())
}

/// What the agent-backed arm runner is built from.
#[derive(Clone)]
pub struct AgentArmRunnerOptions {
    pub scope: ChildRuntimeScope,
    pub cancel: CancellationToken,
    pub use_llm_proposer: bool,
    pub use_llm_dreamer: bool,
    /// The task's public contract for the proposer prompt.
    pub proposer_prompt_context: Option<String>,
    /// Per-attempt child token budget; [`DEFAULT_CHILD_TOKEN_BUDGET`] when `None`.
    pub child_token_budget: Option<u64>,
    /// Roll round 1 out once and copy it into every arm (default `true`).
    pub share_initial_rollout: bool,
}

impl AgentArmRunnerOptions {
    /// Both toggles as given, a fresh scope's defaults, round 1 shared.
    #[must_use]
    pub fn new(scope: ChildRuntimeScope, cancel: CancellationToken) -> Self {
        Self {
            scope,
            cancel,
            use_llm_proposer: false,
            use_llm_dreamer: false,
            proposer_prompt_context: None,
            child_token_budget: None,
            share_initial_rollout: true,
        }
    }
}

/// The in-session arm runner (TS `createAgentExperimentRunner`).
pub struct AgentArmRunner<'a> {
    runner: &'a dyn RunAgent,
    options: AgentArmRunnerOptions,
}

impl<'a> AgentArmRunner<'a> {
    #[must_use]
    pub fn new(runner: &'a dyn RunAgent, options: AgentArmRunnerOptions) -> Self {
        Self { runner, options }
    }

    fn token_budget(&self) -> u64 {
        self.options
            .child_token_budget
            .unwrap_or(DEFAULT_CHILD_TOKEN_BUDGET)
    }

    /// Round 1, once (TS `sharedInitialRollout`).
    fn shared_initial_rollout(
        &self,
        plan: &ExperimentPlan,
        clock: DreamClock<'_>,
    ) -> Result<DreamInitialRollout, DreamStoreError> {
        let Some(first) = plan.arms.first() else {
            return Err(DreamStoreError::Message(
                "an experiment needs at least one arm".to_string(),
            ));
        };
        let cancel = &self.options.cancel;
        if cancel.is_cancelled() {
            return Err(dream_abort(
                "dream experiment aborted before the shared rollout",
            ));
        }
        let counting = CountingRunner::new(self.runner);
        let rejections = RejectionLog::new(
            rejections_path(&first.dir, &format!("{}-shared", plan.experiment_id)),
            clock,
        );
        let task = plan.task.as_ref();
        let task_id = plan.task_id.as_str().to_string();
        let n = plan.n.and_then(|n| u32::try_from(n).ok());
        let mut tally = crate::proposer::ProposalTally::default();
        let mut rollout = |policy: &ExplorationPolicy,
                           fork: &str,
                           tree_id: Option<String>|
         -> Result<ExploreResult, DreamStoreError> {
            let mut llm = self.options.use_llm_proposer.then(|| {
                LlmProposer::new(
                    &counting,
                    task,
                    LlmProposerOptions {
                        scope: &self.options.scope,
                        cancel,
                        token_budget: self.token_budget(),
                        prompt_context: self.options.proposer_prompt_context.clone(),
                        guidance: None,
                        rejections: Some(&rejections),
                        iteration: 0,
                    },
                )
            });
            let result = run_online_exploration(ExploreOptions {
                task,
                task_id: task_id.clone(),
                n,
                seed: plan.seed.clone(),
                rng: SeededRng::new(&plan.seed).fork(fork),
                clock,
                workers: plan.budget.workers,
                k1: plan.budget.k1,
                dir: &first.dir,
                policy: *policy,
                iteration: 0,
                proposer: llm.as_mut().map(|proposer| proposer as &mut dyn Proposer),
                tree_id,
                cancel: Some(cancel),
            });
            if let Some(proposer) = &llm {
                tally = tally.plus(&proposer.tally);
            }
            result
        };
        let explore = rollout(&plan.initial_policy, "iter:0", None)?;
        let mut primed = Vec::with_capacity(plan.priming_policies.len());
        for (index, policy) in plan.priming_policies.iter().enumerate() {
            // A local rollout stops early on a cancel instead of failing:
            // never share a truncated round 1.
            if cancel.is_cancelled() {
                return Err(dream_abort(
                    "dream experiment aborted during the shared rollout",
                ));
            }
            let tree_id = priming_tree_id(plan.task_id.as_str(), &plan.seed, index, clock());
            primed.push(rollout(policy, &format!("prime:{index}"), Some(tree_id))?);
        }
        if cancel.is_cancelled() {
            return Err(dream_abort(
                "dream experiment aborted during the shared rollout",
            ));
        }
        for arm in plan.arms.iter().skip(1) {
            copy_tree(&explore.tree_id, &first.dir, &arm.dir)?;
            for prime in &primed {
                copy_tree(&prime.tree_id, &first.dir, &arm.dir)?;
            }
        }
        Ok(DreamInitialRollout {
            proposals: tally,
            handler_calls: DreamHandlerCalls {
                proposer: counting.take(),
                dreamer: 0,
                guidance: 0,
            },
            ..merge_primed_rollouts(&explore, &primed)
        })
    }
}

impl ExperimentArmRunner for AgentArmRunner<'_> {
    fn mode(&self, _arm: &ExperimentArmPlan) -> ExperimentArmMode {
        let scope = &self.options.scope;
        ExperimentArmMode {
            proposer: if self.options.use_llm_proposer {
                "llm"
            } else {
                "local"
            },
            dreamer: if self.options.use_llm_dreamer {
                "llm"
            } else {
                "local"
            },
            model: scope.model.clone().filter(|model| !model.is_empty()),
            thinking: scope.thinking_level.clone(),
            max_output_tokens: scope.max_output_tokens,
        }
    }

    fn prepare(
        &mut self,
        plan: &ExperimentPlan,
        clock: DreamClock<'_>,
    ) -> Result<Option<DreamInitialRollout>, ExperimentError> {
        let arms: Vec<ExperimentArm> = plan.arms.iter().map(|arm| arm.arm).collect();
        assert_guided_arms_served(&arms, self.options.use_llm_proposer)?;
        if !self.options.share_initial_rollout || plan.arms.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.shared_initial_rollout(plan, clock)?))
    }

    fn run(
        &mut self,
        plan: &ExperimentPlan,
        arm: &ExperimentArmPlan,
        clock: DreamClock<'_>,
        shared: Option<&DreamInitialRollout>,
        progress: &mut dyn FnMut(ExperimentArmProgress),
    ) -> Result<DreamLoopResult, DreamStoreError> {
        let mut relay = |event: DreamProgressEvent| {
            if let DreamProgressEvent::Phase {
                phase,
                iteration,
                best_node_score,
                tree_id,
            } = event
            {
                progress(ExperimentArmProgress {
                    phase,
                    iteration,
                    best_node_score,
                    tree_id,
                });
            }
        };
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            runner: self.runner,
            task: plan.task.as_ref(),
            task_id: plan.task_id.as_str().to_string(),
            n: plan.n.and_then(|n| u32::try_from(n).ok()),
            seed: plan.seed.clone(),
            clock,
            workers: plan.budget.workers,
            k1: plan.budget.k1,
            k2: plan.budget.k2,
            dreams: usize::try_from(plan.budget.dreams).unwrap_or(usize::MAX),
            iterations: plan.rounds - 1,
            dir: &arm.dir,
            objective: plan.objective,
            rng: None,
            initial_policy: plan.initial_policy,
            fixed_policy: arm.arm.fixed_policy(),
            initial_rollout: shared.cloned(),
            semantic_guidance: arm.arm.guided().then_some(SemanticGuidance::Child),
            use_llm_proposer: self.options.use_llm_proposer,
            use_llm_dreamer: self.options.use_llm_dreamer,
            scope: self.options.scope.clone(),
            cancel: self.options.cancel.clone(),
            proposer_prompt_context: self.options.proposer_prompt_context.clone(),
            child_token_budget: Some(self.token_budget()),
            on_progress: Some(&mut relay),
            run_label: Some(arm.run_label.clone()),
            dreams_log_context: DreamsLogContext {
                experiment_id: Some(plan.experiment_id.clone()),
                arm: Some(arm.arm.as_str().to_string()),
            },
            priming_policies: plan.priming_policies.clone(),
            // Each arm's run is its own detached root linking back to the experiment.
            trigger_trace_id: pa_types::trace_context::current()
                .map(|context| context.trace_id_hex()),
        })
    }
}

/// Run one experiment with the agent-backed runner (TS
/// `runExperimentWithAgent`): every arm is served, a guided arm without the
/// LLM proposer is refused before anything is created, and the detached
/// `dream.experiment` root carries `trigger_trace_id`.
///
/// # Errors
///
/// [`ExperimentError`] from planning, the shared rollout, an arm, or a cancel.
pub fn run_experiment_with_agent(
    spec: &ExperimentSpec,
    options: &ExperimentRunOptions<'_>,
    runner: &dyn RunAgent,
    agent: AgentArmRunnerOptions,
    on_progress: Option<&mut dyn FnMut(crate::experiment::ExperimentProgressEvent)>,
    trigger_trace_id: Option<String>,
) -> Result<ExperimentResult, ExperimentError> {
    assert_guided_arms_served(&spec.arms, agent.use_llm_proposer)?;
    let mut agent = agent;
    if agent.proposer_prompt_context.is_none() {
        agent.proposer_prompt_context =
            task_prompt_context(spec.task, resolve_task_n(spec.task, spec.n));
    }
    let cancel = agent.cancel.clone();
    let mut arm_runner = AgentArmRunner::new(runner, agent);
    run_experiment_with_hooks(
        spec,
        options,
        &mut arm_runner,
        EXPERIMENT_ARMS,
        ExperimentHooks {
            cancel: Some(&cancel),
            on_progress,
            detached: true,
            trigger_trace_id,
        },
    )
}
