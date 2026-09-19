/**
 * The replay objective V (Zheng et al. 2026, eq. 1), made scale-invariant.
 *
 *   V = q  -  beta1 * N / (W * k1)  +  beta2 * N / (max(1, rounds) * W)
 *
 * Eq. 1 is kept structurally (best quality, minus a probe penalty, plus a
 * parallelism bonus) but every term is dimensionless. Term 1 is the best valid
 * score reached, normalized into [0, 1] against the POOL's observed score range
 * (`poolScoreScale`). Term 2 is the fraction of the per-rollout probe budget
 * `W * k1` the replay spent (N = revealed non-root nodes). Term 3 is the mean
 * batch fill `N / (rounds * W)`, in [0, 1]. `beta1` and `beta2` therefore mean
 * "fraction of a full-quality point": spending the whole budget costs `beta1`,
 * and the two terms together never move V by more than `beta1 + beta2`.
 *
 * The paper tunes beta per domain because its penalty is on RAW scores. Ours was
 * `V = best - 0.01 N + 0.02 N / rounds` and on circle-packing (pool range 0.48)
 * a 36-probe rollout paid 0.36, more than any score gain, so the dreamer was
 * rewarded for frugality and learned to stop exploring (probes 36 -> 1 with the
 * best frozen at round 1). See `test/dream-improve.test.ts` for that recorded
 * regression and `docs/dream-rsi.md` for the numeric justification of the
 * defaults. The arithmetic is plain IEEE-754 in a fixed operation order, so the
 * same replay, scale and budget always yield the same V.
 */

import type { ReplayResult } from "./replay.js";
import type { RecordedTree } from "./store.js";

export interface ReplayObjectiveConfig {
	/** Cost of spending the whole per-rollout probe budget `W * k1`, in quality points. */
	beta1: number;
	/** Bonus for a fully parallel replay (every round fills all W slots), in quality points. */
	beta2: number;
}

/**
 * Defaults fixed by the recorded circle-packing and python-speedup runs: the
 * exploring-vs-collapsed break-even `beta1` (at `beta2` 0.05) is 0.639 on
 * circle-packing and 0.109 / 0.087 on python-speedup's control / dream pools, so
 * 0.05 keeps a 12.8x, 2.2x and 1.7x margin, while 0.1 would be a coin flip on
 * python-speedup. With `beta2 = beta1` a full-budget, full-parallelism replay
 * scores exactly q.
 */
export const DEFAULT_OBJECTIVE: ReplayObjectiveConfig = { beta1: 0.05, beta2: 0.05 };

/** The observed valid-score range of a pool; quality is normalized against it. */
export interface ObjectiveScale {
	scoreMin: number;
	scoreMax: number;
}

/** The per-rollout probe budget the cost term is measured against. */
export interface ObjectiveBudget {
	/** Max parallelism W (`tree.header.w`). */
	workers: number;
	/** Max online rounds k1. */
	k1: number;
}

export interface ObjectiveTerms {
	/** Best valid score, normalized into [0, 1] against the pool's range. */
	quality: number;
	/** `N / (W * k1)`: fraction of the per-rollout probe budget spent. */
	cost: number;
	/** `N / (max(1, rounds) * W)`: mean batch fill, in [0, 1]. */
	parallelism: number;
	/** `quality - beta1 * cost + beta2 * parallelism`. */
	value: number;
}

/**
 * Min and max over the VALID, finite node scores of every tree in the pool
 * (roots included). An empty pool, or one with no valid node, is `{0, 0}`.
 */
export function poolScoreScale(pool: readonly RecordedTree[]): ObjectiveScale {
	let scoreMin = Number.POSITIVE_INFINITY;
	let scoreMax = Number.NEGATIVE_INFINITY;
	for (const tree of pool) {
		for (const node of tree.nodes) {
			if (!node.valid || !Number.isFinite(node.score)) continue;
			if (node.score < scoreMin) scoreMin = node.score;
			if (node.score > scoreMax) scoreMax = node.score;
		}
	}
	if (scoreMin > scoreMax) return { scoreMin: 0, scoreMax: 0 };
	return { scoreMin, scoreMax };
}

/**
 * `clamp((best - min) / (max - min), 0, 1)`. A degenerate scale (`min == max`)
 * scores 1 at or above the single observed value and 0 below it. The clamp also
 * maps the replay's "nothing valid" sentinel (`bestScore` 0) to 0 whenever the
 * pool's scores are positive.
 */
export function normalizedQuality(bestScore: number, scale: ObjectiveScale): number {
	const span = scale.scoreMax - scale.scoreMin;
	if (!(span > 0)) return bestScore >= scale.scoreMax ? 1 : 0;
	const raw = (bestScore - scale.scoreMin) / span;
	return raw < 0 ? 0 : raw > 1 ? 1 : raw;
}

export function computeObjectiveTerms(
	replay: ReplayResult,
	cfg: ReplayObjectiveConfig,
	scale: ObjectiveScale,
	budget: ObjectiveBudget,
): ObjectiveTerms {
	const workers = Math.max(1, Math.trunc(budget.workers));
	const k1 = Math.max(1, Math.trunc(budget.k1));
	const quality = normalizedQuality(replay.bestScore, scale);
	const cost = replay.N / (workers * k1);
	const parallelism = replay.N / (Math.max(1, replay.rounds) * workers);
	const probePenalty = cfg.beta1 * cost;
	const parallelismBonus = cfg.beta2 * parallelism;
	return { quality, cost, parallelism, value: quality - probePenalty + parallelismBonus };
}

export function computeObjective(
	replay: ReplayResult,
	cfg: ReplayObjectiveConfig,
	scale: ObjectiveScale,
	budget: ObjectiveBudget,
): number {
	return computeObjectiveTerms(replay, cfg, scale, budget).value;
}
