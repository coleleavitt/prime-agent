/**
 * The replay objective V (Zheng et al. 2026, eq. 1).
 *
 *   V = max_{v in revealed} s_v  -  beta1 * N  +  beta2 * N / max(1, rounds)
 *
 * Term 1 is the best quality reached, term 2 penalizes probes spent (N = revealed
 * non-root nodes), and term 3 rewards attempts per decision round (parallelism).
 * The arithmetic is plain IEEE-754 in a fixed operation order — no rationals —
 * so the same replay result always yields the same V.
 */

import type { ReplayResult } from "./replay.js";

export interface ReplayObjectiveConfig {
	/** Per-probe penalty. */
	beta1: number;
	/** Per-probe, per-round parallelism bonus. */
	beta2: number;
}

export const DEFAULT_OBJECTIVE: ReplayObjectiveConfig = { beta1: 0.01, beta2: 0.02 };

export function computeObjective(replay: ReplayResult, cfg: ReplayObjectiveConfig): number {
	const quality = replay.bestScore;
	const probePenalty = cfg.beta1 * replay.N;
	const parallelismBonus = (cfg.beta2 * replay.N) / Math.max(1, replay.rounds);
	return quality - probePenalty + parallelismBonus;
}
