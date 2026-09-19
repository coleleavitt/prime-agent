/**
 * The full Dream-RSI orchestrator (synchronous, local, zero-token).
 *
 * One `runDreamLoop` is a single `dream.run` span. Iteration 0 rolls out with the
 * initial policy (`dream.explore`); each subsequent iteration freezes the current
 * tree pool, dreams a no-worse policy over it (`dream.dream`), and redeploys that
 * policy in a fresh rollout (`dream.redeploy` wrapping the rollout's own
 * `dream.explore`). Because `withSpan` restores the parent context when each sync
 * child returns, every child is a direct descendant of the still-open `dream.run`
 * — no detached roots, no child outliving its parent.
 *
 * The reported final policy is `selectBestPolicy` over {initial} union every
 * policy the loop chose, scored on the final pool, so `finalPolicyScore` is
 * UNCONDITIONALLY at least `initialPolicyScore` and the final policy's mean
 * replay quality is at least the initial one's (the initial policy always wins a
 * tie and the quality guard excludes anything below it). Every score, tree
 * shape, policy id and round table is a function of the seed alone: the rng is
 * forked per iteration and per dreaming step by label, and every attempt fork is
 * labelled by round, parent seq and child slot (`attemptRngLabel`), never by an
 * id. The clock reaches only the on-disk identity (tree ids, node ids,
 * `createdTs`/`ts`, the run id), so two runs of one seed at different wall times
 * yield identical round tables under different ids, and one seed and one clock
 * give a byte-reproducible run.
 *
 * `fixedPolicy` is the paper's "Recursive Fixed Exploration" control: the same
 * loop, the same rollouts, the same growing pool, but no dreaming at all — every
 * iteration redeploys the initial policy. Because the loop never draws from the
 * root rng and every rollout forks by iteration label, a fixed-policy run and a
 * dreaming run share a byte-identical iteration 0.
 *
 * The async in-session driver (`runDreamLoopWithAgent`), where dreaming runs past
 * the user turn and mints detached-root spans, lives in the flag-gated `llm.ts`.
 */

import { withSpan } from "@earendil-works/pi-ai";
import { runDreaming, selectBestPolicy } from "./improve.js";
import { DEFAULT_OBJECTIVE, type ReplayObjectiveConfig } from "./objective.js";
import { DEFAULT_POLICY, type ExplorationPolicy, policyId } from "./policy.js";
import { type ProposalTally, zeroProposalTally } from "./proposer.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import { type ExploreResult, runOnlineExploration } from "./rollout.js";
import { listTrees, type RecordedTree, readTree } from "./store.js";
import type { DreamTaskId, ScoredTask } from "./task.js";
import type { DreamClock, DreamMode } from "./types.js";

export interface DreamLoopOptions {
	task: ScoredTask<unknown>;
	/** Task id recorded on every tree header; defaults to `task.id`. */
	taskId?: DreamTaskId;
	/** Task size parameter (e.g. circle count) recorded on the header. */
	n?: number;
	seed: number | string;
	/** The one injected clock; the only clock-derived ids are the tree ids and the run id. */
	clock: DreamClock;
	/** Max parallelism W: cells per round. */
	workers: number;
	/** Max online rounds per rollout. */
	k1: number;
	/** Max replay rounds per policy simulation. */
	k2: number;
	/** Revised policies M per dreaming step. */
	dreams: number;
	/** Explore/dream/redeploy iterations after the initial rollout. */
	iterations: number;
	/** Dream store directory the trees and blobs are written under. */
	dir: string;
	objective?: ReplayObjectiveConfig;
	/** Injected rng; defaults to a fresh seeded rng from `seed`. */
	rng?: SeededRng;
	/** The hand-written policy iteration 0 rolls out with and dreaming starts from; defaults to `DEFAULT_POLICY`. */
	initialPolicy?: ExplorationPolicy;
	/**
	 * The fixed-exploration control: never dream. Every iteration redeploys
	 * `initialPolicy`; the pool still grows and every rollout is unchanged, so the
	 * control shares the dreaming run's iteration 0 byte for byte.
	 */
	fixedPolicy?: boolean;
	/**
	 * Optional injected candidate proposer (the flag-gated LLM dreamer), threaded
	 * into `runDreaming`. It must return already-parsed, in-bounds policies; they
	 * are scored and selected by the same no-regression rule as local candidates.
	 * Absent, the loop uses the local `proposePolicies` and stays byte-identical.
	 */
	proposeCandidates?: (current: ExplorationPolicy, m: number, rng: SeededRng) => ExplorationPolicy[];
}

/** Actual `RunAgentHandler` invocations per role, retries included. All zero on the local path. */
export interface DreamHandlerCalls {
	proposer: number;
	dreamer: number;
	guidance: number;
}

/**
 * One rollout of a loop, as the experiment runner records it. `probes` (revealed
 * non-root nodes, `tree.size - 1`) is the discovery compute on every path; handler
 * calls and tokens are cost and are never mixed into that axis. Provenance splits
 * the probes: `agentGeneratedCalls` are the probes a child agent actually
 * generated (`origin: "llm"` nodes) and `proposals` says what happened to every
 * child result, so a local stand-in for a rejected child output is never counted
 * as the agent's work. Collecting a record touches no rng, tree or persistence
 * (reading node origins is read-only), so the trees a loop grows are
 * byte-identical with and without the records.
 */
export interface DreamRoundRecord {
	/** 0 is the initial rollout; the paper's round is `iteration + 1`. */
	iteration: number;
	treeId: string;
	/** The policy that grew this tree. */
	policyId: string;
	/** `ExploreResult.bestScore`: the best valid node score of this rollout (0 when none). */
	roundBest: number;
	/** `ExploreResult.revealedCount` = `tree.size - 1`: evaluated attempts. */
	probes: number;
	/**
	 * `ExploreResult.agentGeneratedCount`: probes whose candidate a child agent
	 * generated. 0 on the local path. The LLM driver MUST set it; absent, the
	 * experiment reports it as 0 (untracked), never as `probes`.
	 */
	agentGeneratedCalls?: number;
	/**
	 * Proposer provenance for this rollout (see `ProposalTally`): child results
	 * examined, accepted, rejected by reason, and local fallbacks. All zero on the
	 * local path; the LLM driver MUST set it. Absent reads as all zero.
	 */
	proposals?: ProposalTally;
	/** `ExploreResult.rounds`: online decision rounds the policy took. */
	decisionRounds: number;
	/**
	 * Trees in the pool the dreaming step froze before this rollout. On iteration 0
	 * and on a fixed-policy iteration nothing is frozen, so it is the iteration
	 * index, which equals the pool size in a fresh store.
	 */
	poolSize: number;
	/** Child token usage per role; 0/0/0 on the local path. */
	tokens: { rollout: number; dreamer: number; guidance: number };
	handlerCalls: DreamHandlerCalls;
	/** The dreaming step that chose this rollout's policy; null at iteration 0 and on every fixed-policy iteration. */
	dreaming: { currentScore: number; chosenScore: number; improved: boolean; candidates: number } | null;
}

export interface DreamLoopResult {
	runId: string;
	task: DreamTaskId;
	seed: number | string;
	mode: DreamMode;
	iterations: number;
	/** True when the run was the fixed-exploration control and never dreamed. */
	fixedPolicy: boolean;
	/** Tree ids in rollout order (iteration 0 first). */
	treeIds: string[];
	/** One record per rollout, iteration 0 first (length `iterations + 1`). */
	rounds: DreamRoundRecord[];
	initialPolicyId: string;
	/** The initial policy's mean replay objective on the final pool. */
	initialPolicyScore: number;
	finalPolicy: ExplorationPolicy;
	finalPolicyId: string;
	/** The chosen policy's mean replay objective on the final pool; never below `initialPolicyScore`. */
	finalPolicyScore: number;
	/** True only when the chosen policy strictly beats the initial one on the final pool. */
	improved: boolean;
	/** Best valid node score across every rollout of the run. */
	bestNodeScore: number;
	tokens: number;
}

/** Freeze the pool for a task: every recorded tree with a matching header, sorted by tree id. */
export function freezePool(dir: string, taskId: DreamTaskId): RecordedTree[] {
	return listTrees(dir)
		.filter((summary) => summary.taskId === taskId)
		.sort((a, b) => a.treeId.localeCompare(b.treeId))
		.map((summary) => readTree(summary.treeId, dir));
}

export function runDreamLoop(options: DreamLoopOptions): DreamLoopResult {
	const taskId: DreamTaskId = options.taskId ?? options.task.id;
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const rng = options.rng ?? createSeededRng(options.seed);
	const iterations = Math.max(0, Math.trunc(options.iterations));
	const scoreCfg = { k1: options.k1, k2: options.k2, objective };
	const initialPolicy = options.initialPolicy ?? DEFAULT_POLICY;
	const fixedPolicy = options.fixedPolicy === true;

	return withSpan(
		"dream.run",
		{
			"dream.task": taskId,
			"dream.seed": options.seed,
			"dream.workers": options.workers,
			"dream.k1": options.k1,
			"dream.k2": options.k2,
			"dream.dreams": options.dreams,
			"dream.iterations": iterations,
			"dream.mode": "local",
			"dream.fixed_policy": fixedPolicy,
		},
		() => {
			const treeIds: string[] = [];
			const rounds: DreamRoundRecord[] = [];
			const chosenPolicies: ExplorationPolicy[] = [];
			let bestNodeScore = 0;
			let seenBest = false;
			let tokens = 0;

			const rollout = (policy: ExplorationPolicy, iteration: number): ExploreResult =>
				runOnlineExploration({
					task: options.task,
					taskId,
					n: options.n,
					seed: options.seed,
					rng: rng.fork(`iter:${iteration}`),
					clock: options.clock,
					workers: options.workers,
					k1: options.k1,
					dir: options.dir,
					policy,
					iteration,
				});

			const record = (
				result: ExploreResult,
				policy: ExplorationPolicy,
				iteration: number,
				poolSize: number,
				dreaming: DreamRoundRecord["dreaming"],
			): void => {
				treeIds.push(result.treeId);
				tokens += result.tokens;
				if (!seenBest || result.bestScore > bestNodeScore) {
					bestNodeScore = result.bestScore;
					seenBest = true;
				}
				rounds.push({
					iteration,
					treeId: result.treeId,
					policyId: policyId(policy),
					roundBest: result.bestScore,
					probes: result.revealedCount,
					agentGeneratedCalls: result.agentGeneratedCount,
					proposals: zeroProposalTally(),
					decisionRounds: result.rounds,
					poolSize,
					tokens: { rollout: result.tokens, dreamer: 0, guidance: 0 },
					handlerCalls: { proposer: 0, dreamer: 0, guidance: 0 },
					dreaming,
				});
			};

			record(rollout(initialPolicy, 0), initialPolicy, 0, 0, null);

			let current: ExplorationPolicy = initialPolicy;
			for (let iteration = 1; iteration <= iterations; iteration++) {
				let poolSize = iteration;
				let dreaming: DreamRoundRecord["dreaming"] = null;
				if (!fixedPolicy) {
					const pool = freezePool(options.dir, taskId);
					poolSize = pool.length;
					const dream = runDreaming({
						current,
						pool,
						dreams: options.dreams,
						k1: options.k1,
						k2: options.k2,
						rng: rng.fork(`dream:${iteration}`),
						objective,
						...(options.proposeCandidates ? { proposeCandidates: options.proposeCandidates } : {}),
					});
					current = dream.chosenPolicy;
					chosenPolicies.push(current);
					dreaming = {
						currentScore: dream.currentScore,
						chosenScore: dream.chosenScore,
						improved: dream.improved,
						candidates: dream.candidatePolicyIds.length,
					};
				}
				const redeployed = withSpan(
					"dream.redeploy",
					{
						"dream.policy_id": policyId(current),
						"dream.k1": options.k1,
						"dream.workers": options.workers,
						"dream.iteration": iteration,
						"dream.fixed_policy": fixedPolicy,
					},
					(span) => {
						const result = rollout(current, iteration);
						span.setAttributes({ "dream.tree_id": result.treeId });
						return result;
					},
				);
				record(redeployed, current, iteration, poolSize, dreaming);
			}

			const finalPool = freezePool(options.dir, taskId);
			const selection = selectBestPolicy(initialPolicy, chosenPolicies, finalPool, scoreCfg);
			return {
				runId: `${taskId}-s${options.seed}-r${options.clock()}`,
				task: taskId,
				seed: options.seed,
				mode: "local" satisfies DreamMode,
				iterations,
				fixedPolicy,
				treeIds,
				rounds,
				initialPolicyId: policyId(initialPolicy),
				initialPolicyScore: selection.currentScore,
				finalPolicy: selection.chosenPolicy,
				finalPolicyId: policyId(selection.chosenPolicy),
				finalPolicyScore: selection.chosenScore,
				improved: selection.improved,
				bestNodeScore,
				tokens,
			};
		},
	);
}
