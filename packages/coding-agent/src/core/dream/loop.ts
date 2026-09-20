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
 * `createdTs`/`ts`, the run id and the dreams-log key), so two runs of one seed
 * at different wall times yield identical round tables under different ids, and
 * one seed and one clock give a byte-reproducible run.
 *
 * `fixedPolicy` is the paper's "Recursive Fixed Exploration" control: the same
 * loop, the same rollouts, the same growing pool, but no dreaming at all — every
 * iteration redeploys the initial policy. Because the loop never draws from the
 * root rng and every rollout forks by iteration label, a fixed-policy run and a
 * dreaming run share a byte-identical iteration 0.
 *
 * `primingPolicies` (default none, byte-identical to a plain run) roll out once
 * each at iteration 0 on their own labelled forks (`prime:<i>`) so the frozen
 * pool holds branches the initial policy would never open; their probes and
 * tokens are charged to round 1. The pool is every tree in `dir`: it grows
 * across rounds within one run (one experiment arm), never across arms or seeds
 * — a seed is an independent replicate and the arms are a controlled pair.
 *
 * Every dreaming step is written to `<dir>/dreams/<runId>.jsonl` (`dreams.ts`),
 * the post-hoc final selection as iteration -1.
 *
 * The async in-session driver (`runDreamLoopWithAgent`), where dreaming runs past
 * the user turn and mints detached-root spans, lives in the flag-gated `llm.ts`.
 */

import { withSpan } from "@earendil-works/pi-ai";
import { DreamsLog, type DreamsLogContext, dreamsPath } from "./dreams.js";
import { type CandidateList, runDreaming, selectBestPolicy } from "./improve.js";
import { DEFAULT_OBJECTIVE, type ReplayObjectiveConfig } from "./objective.js";
import { DEFAULT_POLICY, type ExplorationPolicy, policyId } from "./policy.js";
import { type ProposalTally, zeroProposalTally } from "./proposer.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import { type ExploreResult, improvementsOf, runOnlineExploration, type ScoreImprovement } from "./rollout.js";
import { listTrees, type RecordedTree, readTree } from "./store.js";
import type { DreamTaskId, ScoredTask } from "./task.js";
import type { CandidateVerdict, DreamClock, DreamerKind, DreamMode, LeverScanRecord } from "./types.js";

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
	proposeCandidates?: (current: ExplorationPolicy, m: number, rng: SeededRng) => CandidateList;
	/**
	 * A clock-free label folded into the run id and every per-run log key (the
	 * experiment runner passes `<experimentId>/<arm>`), so arms of one experiment
	 * under a frozen clock no longer share a run id. Absent, the run id keeps its
	 * bare `<task>-s<seed>-r<clock>` form.
	 */
	runLabel?: string;
	/** Experiment provenance stamped on every dreams-log line. */
	dreamsLogContext?: DreamsLogContext;
	/**
	 * Policies rolled out once each at iteration 0, in addition to the initial
	 * rollout, on forks `prime:<i>`; their trees join the pool and their probes
	 * and tokens are charged to round 1. Default none.
	 */
	primingPolicies?: readonly ExplorationPolicy[];
}

/** Actual `RunAgentHandler` invocations per role, retries included. All zero on the local path. */
export interface DreamHandlerCalls {
	proposer: number;
	dreamer: number;
	guidance: number;
}

/**
 * What one dreaming step recorded on the round it chose the policy for.
 * `candidates` is the proposed count (kept a number: the result schema is
 * additive-only); the per-candidate verdicts, the dreamer kind and the lever
 * scan are the additive fields the local path always fills and the LLM driver
 * must fill too.
 */
export interface DreamRoundDreaming {
	currentScore: number;
	chosenScore: number;
	improved: boolean;
	candidates: number;
	candidateVerdicts?: CandidateVerdict[];
	dreamer?: DreamerKind;
	leverScan?: LeverScanRecord | null;
	/** Trees of the frozen pool the current policy replayed in full support; the scores are means over these only. */
	measuredTrees?: number;
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
	dreaming: DreamRoundDreaming | null;
	/**
	 * Probe index (1-based within this round's charged probes; 0 when the root is
	 * best) at which `roundBest` was first reached. With priming trees on round 1
	 * the index runs over the initial rollout's probes and then each priming tree's.
	 */
	probesToRoundBest?: number;
	/** The round's best-so-far curve at its improvements only, over the same probe index. */
	improvements?: ScoreImprovement[];
	/** Round 1 only, when priming policies were rolled out: their tree ids in order. */
	primingTreeIds?: string[];
	/** Round 1 only: probes the priming rollouts spent (included in `probes`). */
	primingProbes?: number;
}

export interface DreamLoopResult {
	runId: string;
	task: DreamTaskId;
	seed: number | string;
	mode: DreamMode;
	iterations: number;
	/** True when the run was the fixed-exploration control and never dreamed. */
	fixedPolicy: boolean;
	/** Tree ids in rollout order (iteration 0 first); priming trees are on `rounds[0].primingTreeIds`. */
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
	/** Rollouts (priming excluded) whose stop rule fired before the round cap k1. */
	stoppedEarly?: number;
	/** Verdicts of the post-hoc final selection over {initial} and every chosen policy. */
	finalSelection?: CandidateVerdict[];
}

/** Freeze the pool for a task: every recorded tree with a matching header, sorted by tree id. */
export function freezePool(dir: string, taskId: DreamTaskId): RecordedTree[] {
	return listTrees(dir)
		.filter((summary) => summary.taskId === taskId)
		.sort((a, b) => a.treeId.localeCompare(b.treeId))
		.map((summary) => readTree(summary.treeId, dir));
}

/**
 * The run id, which is also the key of every per-run log (`dreams/`,
 * `rejections/`): `<task>-s<seed>-r<clock>`, plus `-<label>` when a `runLabel`
 * is given (characters outside `[A-Za-z0-9._-]` become `_`, so a label such as
 * `<experimentId>/<arm>` is a safe file name).
 */
export function dreamRunId(taskId: DreamTaskId, seed: number | string, clockMs: number, runLabel?: string): string {
	const base = `${taskId}-s${seed}-r${clockMs}`;
	if (runLabel === undefined || runLabel.length === 0) return base;
	return `${base}-${runLabel.replace(/[^A-Za-z0-9._-]+/g, "_")}`;
}

/** The tree id of the `index`-th priming rollout of a run; distinct from the initial rollout's under one clock. */
export function primingTreeId(taskId: DreamTaskId, seed: number | string, index: number, clockMs: number): string {
	return `${taskId}-s${seed}-i0p${index}-${clockMs}`;
}

/**
 * The round-1 curve over the initial rollout followed by each priming rollout:
 * probe indices are 1-based over the concatenated charged probes. Roots are not
 * probes: every rollout's root (each is its own seeded artifact) is known before
 * the first probe, so the best valid root of the round sits at probe 0 and the
 * curve's last point is always the round's best (`roundBest`).
 */
export function mergedRoundCurve(results: readonly ExploreResult[]): {
	probesToBest: number;
	improvements: ScoreImprovement[];
} {
	if (results.length === 1) {
		const only = results[0]!;
		return { probesToBest: only.probesToBest, improvements: only.improvements };
	}
	let bestRoot: { seq: number; score: number; valid: boolean } | undefined;
	const probes: { seq: number; score: number; valid: boolean }[] = [];
	let offset = 0;
	for (const result of results) {
		for (const node of result.tree.allNodes()) {
			if (node.parentId === null) {
				if (node.valid && (bestRoot === undefined || node.score > bestRoot.score)) {
					bestRoot = { seq: 0, score: node.score, valid: true };
				}
				continue;
			}
			probes.push({ seq: offset + node.seq, score: node.score, valid: node.valid });
		}
		offset += result.revealedCount;
	}
	const improvements = improvementsOf(bestRoot ? [bestRoot, ...probes] : probes);
	return { probesToBest: improvements.at(-1)?.probe ?? 0, improvements };
}

export function runDreamLoop(options: DreamLoopOptions): DreamLoopResult {
	const taskId: DreamTaskId = options.taskId ?? options.task.id;
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const rng = options.rng ?? createSeededRng(options.seed);
	const iterations = Math.max(0, Math.trunc(options.iterations));
	const scoreCfg = { k1: options.k1, k2: options.k2, objective };
	const initialPolicy = options.initialPolicy ?? DEFAULT_POLICY;
	const fixedPolicy = options.fixedPolicy === true;
	const priming = options.primingPolicies ?? [];
	const k1 = Math.max(1, Math.trunc(options.k1));

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
			"dream.priming_policies": priming.length,
		},
		(runSpan) => {
			const runId = dreamRunId(taskId, options.seed, options.clock(), options.runLabel);
			runSpan.setAttributes({ "dream.run_id": runId });
			const dreamsLog = new DreamsLog(dreamsPath(options.dir, runId), options.clock, options.dreamsLogContext);
			const treeIds: string[] = [];
			const rounds: DreamRoundRecord[] = [];
			const chosenPolicies: ExplorationPolicy[] = [];
			let bestNodeScore = 0;
			let seenBest = false;
			let tokens = 0;
			let stoppedEarly = 0;

			const rollout = (policy: ExplorationPolicy, iteration: number, fork: string, treeId?: string): ExploreResult =>
				runOnlineExploration({
					task: options.task,
					taskId,
					n: options.n,
					seed: options.seed,
					rng: rng.fork(fork),
					clock: options.clock,
					workers: options.workers,
					k1: options.k1,
					dir: options.dir,
					policy,
					iteration,
					...(treeId !== undefined ? { treeId } : {}),
				});

			const noteBest = (score: number): void => {
				if (!seenBest || score > bestNodeScore) {
					bestNodeScore = score;
					seenBest = true;
				}
			};

			const record = (
				result: ExploreResult,
				policy: ExplorationPolicy,
				iteration: number,
				poolSize: number,
				dreaming: DreamRoundRecord["dreaming"],
				primed: readonly ExploreResult[] = [],
			): void => {
				treeIds.push(result.treeId);
				tokens += result.tokens;
				noteBest(result.bestScore);
				if (result.rounds < k1) stoppedEarly += 1;
				let probes = result.revealedCount;
				let roundBest = result.bestScore;
				let primingProbes = 0;
				for (const prime of primed) {
					tokens += prime.tokens;
					noteBest(prime.bestScore);
					probes += prime.revealedCount;
					primingProbes += prime.revealedCount;
					if (prime.bestScore > roundBest) roundBest = prime.bestScore;
				}
				const curve = mergedRoundCurve([result, ...primed]);
				rounds.push({
					iteration,
					treeId: result.treeId,
					policyId: policyId(policy),
					roundBest,
					probes,
					agentGeneratedCalls: result.agentGeneratedCount,
					proposals: zeroProposalTally(),
					decisionRounds: result.rounds,
					poolSize,
					tokens: {
						rollout: result.tokens + primed.reduce((sum, prime) => sum + prime.tokens, 0),
						dreamer: 0,
						guidance: 0,
					},
					handlerCalls: { proposer: 0, dreamer: 0, guidance: 0 },
					dreaming,
					probesToRoundBest: curve.probesToBest,
					improvements: curve.improvements,
					...(primed.length > 0 ? { primingTreeIds: primed.map((prime) => prime.treeId), primingProbes } : {}),
				});
			};

			const initial = rollout(initialPolicy, 0, "iter:0");
			const primed = priming.map((policy, index) =>
				rollout(policy, 0, `prime:${index}`, primingTreeId(taskId, options.seed, index, options.clock())),
			);
			record(initial, initialPolicy, 0, 0, null, primed);

			let current: ExplorationPolicy = initialPolicy;
			for (let iteration = 1; iteration <= iterations; iteration++) {
				let poolSize = iteration + primed.length;
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
						iteration,
						...(options.proposeCandidates ? { proposeCandidates: options.proposeCandidates } : {}),
					});
					dreamsLog.recordStep({
						iteration,
						poolSize,
						selection: {
							candidates: dream.candidates,
							currentScore: dream.currentScore,
							chosenPolicy: dream.chosenPolicy,
							improved: dream.improved,
							dreamer: dream.dreamer,
							measuredTrees: dream.measuredTrees,
						},
						leverScan: dream.leverScan,
					});
					current = dream.chosenPolicy;
					chosenPolicies.push(current);
					dreaming = {
						currentScore: dream.currentScore,
						chosenScore: dream.chosenScore,
						improved: dream.improved,
						candidates: dream.candidatePolicyIds.length,
						candidateVerdicts: dream.candidates,
						dreamer: dream.dreamer,
						leverScan: dream.leverScan,
						measuredTrees: dream.measuredTrees,
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
						const result = rollout(current, iteration, `iter:${iteration}`);
						span.setAttributes({ "dream.tree_id": result.treeId });
						return result;
					},
				);
				record(redeployed, current, iteration, poolSize, dreaming);
			}

			const finalPool = freezePool(options.dir, taskId);
			const selection = selectBestPolicy(initialPolicy, chosenPolicies, finalPool, scoreCfg);
			dreamsLog.recordStep({ iteration: -1, poolSize: finalPool.length, selection, leverScan: null });
			return {
				runId,
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
				stoppedEarly,
				finalSelection: selection.candidates,
			};
		},
	);
}
