/**
 * The full Dream-RSI orchestrator (synchronous, local, zero-token).
 *
 * One `runDreamLoop` is a single `dream.run` span. Iteration 0 rolls out with the
 * default policy (`dream.explore`); each subsequent iteration freezes the current
 * tree pool, dreams a no-worse policy over it (`dream.dream`), and redeploys that
 * policy in a fresh rollout (`dream.redeploy` wrapping the rollout's own
 * `dream.explore`). Because `withSpan` restores the parent context when each sync
 * child returns, every child is a direct descendant of the still-open `dream.run`
 * — no detached roots, no child outliving its parent.
 *
 * The reported final policy is `selectBestPolicy` over {default} union every
 * policy the loop chose, scored on the final pool, so `finalPolicyScore` is
 * UNCONDITIONALLY at least `initialPolicyScore` (the default policy always wins a
 * tie). Everything is deterministic in the injected seed and clock: the rng is
 * forked per iteration and per dreaming step by label, and each rollout's tree id
 * is distinct by iteration, so a whole run is byte-reproducible.
 *
 * The async in-session driver (`runDreamLoopWithAgent`), where dreaming runs past
 * the user turn and mints detached-root spans, lives in the flag-gated `llm.ts`.
 */

import { withSpan } from "@earendil-works/pi-ai";
import { runDreaming, selectBestPolicy } from "./improve.js";
import { DEFAULT_OBJECTIVE, type ReplayObjectiveConfig } from "./objective.js";
import { DEFAULT_POLICY, type ExplorationPolicy, policyId } from "./policy.js";
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
	/**
	 * Optional injected candidate proposer (the flag-gated LLM dreamer), threaded
	 * into `runDreaming`. It must return already-parsed, in-bounds policies; they
	 * are scored and selected by the same no-regression rule as local candidates.
	 * Absent, the loop uses the local `proposePolicies` and stays byte-identical.
	 */
	proposeCandidates?: (current: ExplorationPolicy, m: number, rng: SeededRng) => ExplorationPolicy[];
}

export interface DreamLoopResult {
	runId: string;
	task: DreamTaskId;
	seed: number | string;
	mode: DreamMode;
	iterations: number;
	/** Tree ids in rollout order (iteration 0 first). */
	treeIds: string[];
	initialPolicyId: string;
	/** The default policy's mean replay objective on the final pool. */
	initialPolicyScore: number;
	finalPolicy: ExplorationPolicy;
	finalPolicyId: string;
	/** The chosen policy's mean replay objective on the final pool; never below `initialPolicyScore`. */
	finalPolicyScore: number;
	/** True only when the chosen policy strictly beats the default on the final pool. */
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
	const scoreCfg = { k2: options.k2, objective };

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
		},
		() => {
			const treeIds: string[] = [];
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

			const noteBest = (score: number): void => {
				if (!seenBest || score > bestNodeScore) {
					bestNodeScore = score;
					seenBest = true;
				}
			};

			const first = rollout(DEFAULT_POLICY, 0);
			treeIds.push(first.treeId);
			tokens += first.tokens;
			noteBest(first.bestScore);

			let current: ExplorationPolicy = DEFAULT_POLICY;
			for (let iteration = 1; iteration <= iterations; iteration++) {
				const pool = freezePool(options.dir, taskId);
				const dream = runDreaming({
					current,
					pool,
					dreams: options.dreams,
					k2: options.k2,
					rng: rng.fork(`dream:${iteration}`),
					objective,
					...(options.proposeCandidates ? { proposeCandidates: options.proposeCandidates } : {}),
				});
				current = dream.chosenPolicy;
				chosenPolicies.push(current);
				const redeployed = withSpan(
					"dream.redeploy",
					{
						"dream.policy_id": policyId(current),
						"dream.k1": options.k1,
						"dream.workers": options.workers,
						"dream.iteration": iteration,
					},
					(span) => {
						const result = rollout(current, iteration);
						span.setAttributes({ "dream.tree_id": result.treeId });
						return result;
					},
				);
				treeIds.push(redeployed.treeId);
				tokens += redeployed.tokens;
				noteBest(redeployed.bestScore);
			}

			const finalPool = freezePool(options.dir, taskId);
			const selection = selectBestPolicy(DEFAULT_POLICY, chosenPolicies, finalPool, scoreCfg);
			return {
				runId: `${taskId}-s${options.seed}-r${options.clock()}`,
				task: taskId,
				seed: options.seed,
				mode: "local" satisfies DreamMode,
				iterations,
				treeIds,
				initialPolicyId: policyId(DEFAULT_POLICY),
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
