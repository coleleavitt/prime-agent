/**
 * The fixed interpreter of an ExplorationPolicy.
 *
 * This is the ONLY code that acts on a policy. It reads an `ObservationView`,
 * ranks the eligible cells by the policy's named selection rule, and cuts a
 * legal batch of at most `min(batchSize, W)` cells that never contains both a
 * node and its parent. It is deterministic: the default rules use no randomness,
 * so `rng` is accepted for future stochastic rules but unused by them, and
 * replay drives the interpreter with no rng at all.
 */

import type { Cell, ObservationView } from "./observation.js";
import type { ExplorationPolicy } from "./policy.js";
import type { SeededRng } from "./rng.js";
import type { ProposeParams } from "./task.js";

/** Base perturbation size a policy's `branchWidth` scales. */
export const BASE_STEP = 0.1;

/** Project a policy's knobs onto the parameters a single generation attempt takes. */
export function projectProposeParams(policy: ExplorationPolicy): ProposeParams {
	return {
		stepScale: policy.branchWidth * BASE_STEP,
		refineDepth: policy.refineDepth,
		branchWidth: policy.branchWidth,
	};
}

function effectiveScore(cell: Cell): number {
	return cell.valid ? cell.score : Number.NEGATIVE_INFINITY;
}

function byScoreDesc(a: Cell, b: Cell): number {
	return effectiveScore(b) - effectiveScore(a) || a.nodeId.localeCompare(b.nodeId);
}

/** Rank the eligible cells according to the policy's selection rule (deterministic). */
export function rankEligible(policy: ExplorationPolicy, view: ObservationView): Cell[] {
	const actions = view.legalActions();
	switch (policy.selectionRule) {
		case "best-first":
			return [...actions].sort(byScoreDesc);
		case "explore-root": {
			const roots = actions.filter((cell) => cell.isRoot);
			const rest = actions.filter((cell) => !cell.isRoot).sort(byScoreDesc);
			return [...roots, ...rest];
		}
		case "round-robin":
			return [...actions].sort((a, b) => a.nodeId.localeCompare(b.nodeId));
		case "weighted": {
			const best = view.bestScore();
			const weight = (cell: Cell): number => {
				const score = effectiveScore(cell);
				const promising = cell.valid && best > 0 && score >= policy.promisingThreshold * best;
				return score + (promising ? policy.explorationBias : 0);
			};
			return [...actions].sort((a, b) => weight(b) - weight(a) || a.nodeId.localeCompare(b.nodeId));
		}
	}
}

/**
 * Produce a legal batch from the view. Greedy over the ranked cells: skip a cell
 * that duplicates one already chosen, that is the parent of a chosen cell, or
 * whose parent is already chosen. The result is therefore always distinct, at
 * most `min(batchSize, W)` cells, and free of any parent+child pair.
 */
export function interpretPolicy(policy: ExplorationPolicy, view: ObservationView, _rng?: SeededRng): Cell[] {
	const limit = Math.min(policy.batchSize, view.maxParallelism);
	if (limit <= 0) return [];
	const ranked = rankEligible(policy, view);
	const chosen: Cell[] = [];
	const chosenIds = new Set<string>();
	for (const cell of ranked) {
		if (chosen.length >= limit) break;
		if (chosenIds.has(cell.nodeId)) continue;
		if (cell.parentId !== null && chosenIds.has(cell.parentId)) continue;
		if (chosen.some((picked) => picked.parentId === cell.nodeId)) continue;
		chosen.push(cell);
		chosenIds.add(cell.nodeId);
	}
	return chosen;
}

/** The evolving state a stop rule reads each round. */
export interface StopState {
	/** Rounds completed so far. */
	round: number;
	/** Best valid score seen. */
	bestScore: number;
	/** Consecutive rounds with no improvement to `bestScore`. */
	roundsSinceImprovement: number;
	/** Revealed non-root nodes (probes spent). */
	revealedNonRootCount: number;
}

/** Decide whether exploration should stop after the current round. */
export function applyStopRule(policy: ExplorationPolicy, state: StopState): boolean {
	switch (policy.stopRule) {
		case "patience":
			return state.roundsSinceImprovement >= policy.beta;
		case "threshold":
			return state.bestScore >= policy.targetScore;
		case "fixed-rounds":
			return state.round >= policy.beta;
		case "never":
			return false;
	}
}
