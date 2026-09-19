/**
 * The in-session, token-spending arm runner for Dream-RSI experiments.
 *
 * `experiment.ts` owns the plan, the per-arm records, the headline and the
 * detached-root `dream.experiment` span (`runExperimentWithRunner`); this module
 * supplies the `ExperimentArmRunner` that drives every arm through
 * `runDreamLoopWithAgent` with the arm's `fixedPolicy` / semantic-guidance
 * settings, so the four arms — `dream`, `fixed`, `dream-guided`, `fixed-guided` —
 * differ ONLY in whether they dream and whether the proposer prompt carries
 * guidance. Everything else (task instance, initial policy, seed, clock, budget,
 * proposer and dreamer mode) is identical by construction.
 *
 * Round 1 is shared: `prepare` rolls it out ONCE (with the initial policy and the
 * exact `createSeededRng(seed).fork("iter:0")` stream every arm's loop would use)
 * into the first arm's store, copies the tree byte for byte into every other arm's
 * store, and hands every arm the same `initialRollout`, so round 1 is identical
 * across arms even though the LLM proposer is not deterministic. The shared
 * rollout's `dream.explore` is a child of `dream.experiment`; each arm's `dream.run`
 * is its own detached root whose `trigger.trace_id` is the experiment's trace.
 *
 * Like `llm.ts`, this file is NOT re-exported by `index.ts`: importing it pulls
 * the child-agent call path. It is reached by `agent-session.ts` (which attaches
 * `createAgentExperimentRunner` to `DreamRunService`) and by tests with a stub
 * handler. Every LLM arm spends tokens; the local-proposer/local-dreamer
 * configuration never calls the handler.
 */

import type { ChildRuntimeScope } from "../ravo/runtime-adapter.js";
import type { RunAgentHandler } from "../run-agent.js";
import {
	armSettings,
	EXPERIMENT_ARMS,
	type ExperimentArm,
	type ExperimentArmPlan,
	type ExperimentArmProgress,
	type ExperimentArmRunner,
	ExperimentArmUnavailableError,
	type ExperimentPlan,
	type ExperimentProgressEvent,
	type ExperimentResult,
	type ExperimentRunOptions,
	type ExperimentSharedRollout,
	type ExperimentSpec,
	runExperimentWithRunner,
} from "./experiment.js";
import {
	createLlmProposer,
	DEFAULT_CHILD_TOKEN_BUDGET,
	DreamAbortError,
	runDreamLoopWithAgent,
	runOnlineExplorationWithAgent,
} from "./llm.js";
import type { DreamLoopResult } from "./loop.js";
import { asyncOf, createLocalProposer, zeroProposalTally } from "./proposer.js";
import { RejectionLog, rejectionsPath } from "./rejections.js";
import { createSeededRng } from "./rng.js";
import type { DreamExperimentLlmContext } from "./run-service.js";
import { copyTree } from "./store.js";
import { resolveTaskN, taskPromptContext } from "./tasks/index.js";

/** What the agent-backed arm runner is built from: the service's LLM context plus two optional knobs. */
export interface AgentExperimentRunnerOptions extends DreamExperimentLlmContext {
	/** Per-attempt token budget handed to each child call; defaults to `DEFAULT_CHILD_TOKEN_BUDGET`. */
	childTokenBudget?: number;
	/**
	 * Roll round 1 out once and copy it into every arm (default). `false` lets each
	 * arm roll out its own round 1, which is NOT identical across arms on the LLM
	 * proposer path.
	 */
	shareInitialRollout?: boolean;
}

/** Guided arms are the semantic-guidance ablation of the LLM PROPOSER; without it there is no prompt to guide. */
export function assertGuidedArmsServed(arms: readonly ExperimentArm[], useLlmProposer: boolean): void {
	const guided = arms.filter((arm) => armSettings(arm).guided);
	if (guided.length > 0 && !useLlmProposer) {
		throw new ExperimentArmUnavailableError(
			`${guided.join("/")} require useLlmProposer: the semantic-guidance ablation prefixes the LLM proposer prompt, so it has no meaning on the local proposer`,
		);
	}
}

/**
 * Build the `ExperimentArmRunner` `DreamRunService.startExperiment` (and
 * `runExperimentWithAgent`) drive LLM experiments with. `mode` records the
 * proposer/dreamer as configured, identically for every arm; a fixed arm never
 * invokes its dreamer, which its `handlerCalls.dreamer === 0` shows.
 */
export function createAgentExperimentRunner(context: AgentExperimentRunnerOptions): ExperimentArmRunner {
	const { runAgent, scope, signal, useLlmProposer, useLlmDreamer } = context;
	const share = context.shareInitialRollout ?? true;
	const tokenBudget = context.childTokenBudget ?? DEFAULT_CHILD_TOKEN_BUDGET;
	const promptContext = context.proposerPromptContext;

	return {
		mode: () => ({
			proposer: useLlmProposer ? "llm" : "local",
			dreamer: useLlmDreamer ? "llm" : "local",
			...(scope.model ? { model: scope.model } : {}),
		}),
		prepare: async (plan: ExperimentPlan): Promise<ExperimentSharedRollout | undefined> => {
			assertGuidedArmsServed(
				plan.arms.map((arm) => arm.arm),
				useLlmProposer,
			);
			if (!share || plan.arms.length === 0) return undefined;
			return sharedInitialRollout(plan, { runAgent, scope, signal, useLlmProposer, tokenBudget, promptContext });
		},
		run: (
			arm: ExperimentArmPlan,
			shared: ExperimentSharedRollout | undefined,
			onProgress: (event: ExperimentArmProgress) => void,
		): Promise<DreamLoopResult> =>
			runDreamLoopWithAgent({
				...arm.loop,
				runAgent,
				scope,
				signal,
				useLlmProposer,
				useLlmDreamer,
				childTokenBudget: tokenBudget,
				...(promptContext ? { proposerPromptContext: promptContext } : {}),
				...(shared ? { initialRollout: shared } : {}),
				...(arm.guided ? { semanticGuidance: true as const } : {}),
				onProgress: (event) => {
					if (event.type !== "phase") return;
					onProgress({
						phase: event.phase,
						iteration: event.iteration,
						bestNodeScore: event.bestNodeScore,
						...(event.treeId !== undefined ? { treeId: event.treeId } : {}),
					});
				},
			}),
	};
}

interface SharedRolloutOptions {
	runAgent: RunAgentHandler;
	scope: ChildRuntimeScope;
	signal: AbortSignal;
	useLlmProposer: boolean;
	tokenBudget: number;
	promptContext: string | undefined;
}

/**
 * Round 1, once. The rollout is exactly what each arm's loop would perform for
 * iteration 0 (same policy, rng fork label, clock, budget and tree id), grown into
 * the first arm's store and copied to the others. Its proposer handler calls are
 * counted (retries included) and reported on every arm's round-1 record, since
 * every arm shares that cost; so are its provenance (`agentGeneratedCount`, the
 * proposer tally), and its rejections are logged under the first arm's store as
 * `<experimentId>-shared`.
 */
async function sharedInitialRollout(
	plan: ExperimentPlan,
	options: SharedRolloutOptions,
): Promise<ExperimentSharedRollout> {
	const first = plan.arms[0]!;
	if (options.signal.aborted) throw new DreamAbortError("dream experiment aborted before the shared rollout");
	let proposerCalls = 0;
	const counting: RunAgentHandler = (request, callOptions) => {
		proposerCalls += 1;
		return options.runAgent(request, callOptions);
	};
	const tally = zeroProposalTally();
	const proposer = options.useLlmProposer
		? createLlmProposer(counting, plan.task, {
				scope: options.scope,
				signal: options.signal,
				tokenBudget: options.tokenBudget,
				tally,
				iteration: 0,
				rejections: new RejectionLog(rejectionsPath(first.dir, `${plan.experimentId}-shared`), first.loop.clock),
				...(options.promptContext ? { promptContext: options.promptContext } : {}),
			})
		: asyncOf(createLocalProposer(plan.task));
	const explore = await runOnlineExplorationWithAgent(
		{
			task: plan.task,
			taskId: plan.taskId,
			...(plan.n !== undefined ? { n: plan.n } : {}),
			seed: plan.seed,
			rng: createSeededRng(plan.seed).fork("iter:0"),
			clock: first.loop.clock,
			workers: plan.budget.workers,
			k1: plan.budget.k1,
			dir: first.dir,
			policy: plan.initialPolicy,
			iteration: 0,
		},
		proposer,
		options.signal,
	);
	// A local-proposer rollout stops early on abort instead of throwing; never share a truncated round 1.
	if (options.signal.aborted) throw new DreamAbortError("dream experiment aborted during the shared rollout");
	for (const arm of plan.arms.slice(1)) copyTree(explore.treeId, first.dir, arm.dir);
	return {
		treeId: explore.treeId,
		bestScore: explore.bestScore,
		revealedCount: explore.revealedCount,
		agentGeneratedCount: explore.agentGeneratedCount,
		proposals: tally,
		rounds: explore.rounds,
		tokens: explore.tokens,
		handlerCalls: { proposer: proposerCalls, dreamer: 0, guidance: 0 },
	};
}

export interface ExperimentWithAgentOptions extends ExperimentRunOptions {
	runAgent: RunAgentHandler;
	scope: ChildRuntimeScope;
	/** The session abort signal; a cancel leaves no partial result. */
	signal: AbortSignal;
	useLlmProposer: boolean;
	useLlmDreamer: boolean;
	childTokenBudget?: number;
	/** The task's public contract for the proposer prompt; defaults to `taskPromptContext(spec.task, resolveTaskN(spec))`. */
	proposerPromptContext?: string;
	/** See `AgentExperimentRunnerOptions.shareInitialRollout`; default true. */
	shareInitialRollout?: boolean;
	onProgress?: (event: ExperimentProgressEvent) => void;
}

/**
 * Run one experiment with the agent-backed runner: all four arms are served. A
 * guided arm without `useLlmProposer` rejects before the plan, so nothing is
 * created. The detached-root `dream.experiment` span, the per-arm
 * `dream.experiment_arm` children and the result file are `runExperimentWithRunner`'s.
 */
export async function runExperimentWithAgent(
	spec: ExperimentSpec,
	options: ExperimentWithAgentOptions,
): Promise<ExperimentResult> {
	assertGuidedArmsServed(spec.arms, options.useLlmProposer);
	const promptContext = options.proposerPromptContext ?? taskPromptContext(spec.task, resolveTaskN(spec));
	const runner = createAgentExperimentRunner({
		runAgent: options.runAgent,
		scope: options.scope,
		signal: options.signal,
		useLlmProposer: options.useLlmProposer,
		useLlmDreamer: options.useLlmDreamer,
		...(options.childTokenBudget !== undefined ? { childTokenBudget: options.childTokenBudget } : {}),
		...(promptContext ? { proposerPromptContext: promptContext } : {}),
		...(options.shareInitialRollout !== undefined ? { shareInitialRollout: options.shareInitialRollout } : {}),
	});
	return runExperimentWithRunner(spec, {
		dir: options.dir,
		clock: options.clock,
		...(options.notes ? { notes: options.notes } : {}),
		...(options.overwrite !== undefined ? { overwrite: options.overwrite } : {}),
		runner,
		allowedArms: EXPERIMENT_ARMS,
		signal: options.signal,
		...(options.onProgress ? { onProgress: options.onProgress } : {}),
	});
}
