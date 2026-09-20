/**
 * The replay objective V (after Zheng et al. 2026, eq. 1), made scale-invariant
 * and given a within-budget lever.
 *
 *   V = (1 - beta3) * q  +  beta3 * anytime  -  beta1 * S / (W * k1)  +  beta2 * (1 - rounds / k1)
 *
 * with N = revealed non-root nodes, oos = out-of-support cells (legal selections
 * that revealed nothing), S = N + oos the CHARGED selections, B = W * k1 the
 * per-rollout probe budget and `rounds` the replay decision rounds.
 *
 * - `quality` q: best valid score reached, normalized into [0, 1] against the
 *   POOL's observed score range (`poolScoreScale`).
 * - `anytime`: the mean normalized best-so-far over the budget, evaluated after
 *   every charged selection and held flat at q for the unspent tail:
 *   `(sum_{p<=S} q_p + max(0, B - S) * q) / max(B, S)`. An out-of-support
 *   selection repeats the running best, so it counts as a probe that found
 *   nothing. `anytime` is in [0, 1] and never exceeds q; it rewards reaching the
 *   best EARLY, which is what the experiment headline (probes to a target)
 *   measures.
 * - `cost`: `S / B`, the charged fraction of the budget. Out-of-support cells
 *   are charged because online they would have been probes. It is NOT clamped:
 *   with k2 > k1 a replay may charge more than one budget.
 * - `roundsSaved`: `1 - rounds / k1`, UNCAPPED, so a replay that runs past k1
 *   (k2 > k1 allows it) goes negative.
 *
 * Evidence-backed spend. A candidate's stop-early credit (fewer charged probes,
 * fewer rounds) is only ever earned against EVIDENCE that stopping was safe.
 * When a candidate is scored on a pool (`improve.ts`), the spend terms of its
 * replay on tree t are charged at `S_eff = max(S_t, H_probes(-t))` and
 * `rounds_eff = max(rounds_t, H_rounds(-t))`, where `H_probes(-t)` is the largest
 * `probesToBest` and `H_rounds(-t)` the largest `roundsToBest` the SAME candidate
 * recorded on the OTHER measured trees: the latest point at which it was still
 * improving somewhere else. With no other tree `H_probes = B` and `H_rounds = k1`,
 * so on a single-tree pool no candidate can earn a stop-early credit at all.
 * Quality and anytime are unchanged (anytime's flat tail never credits
 * stopping). The INCUMBENT is charged its raw `S` and `rounds`: the measured
 * trees are the ones it grew, its recorded spend IS its online behaviour, and
 * charging it at a horizon from its own late trees surcharged the early stops
 * it really made (a symmetric rule let 106 grid candidates on 43 recorded
 * circle-packing pools win against the surcharged incumbent while losing to
 * its raw spend, with identical candidate numbers under both rules). So a
 * candidate is never charged below its own spend and the incumbent never above
 * its real one. `computeObjectiveTerms` takes the horizon as an optional
 * `evidence` argument (absent: the raw `S` and `rounds`, which is what a single
 * replay and the incumbent report). `chargedProbes` and `chargedRounds` on the
 * terms show what was charged.
 *
 * What the rule closes, and what it does not. The recorded run 3
 * (autocorrelation n 64, seed 7, W 3, k1 13, k2 26, Sonnet-5 proposer and
 * dreamer, fixture `test/fixtures/dream/autocorrelation-s7-i0-1789923274195.jsonl`)
 * is the single-tree instance: the frozen pool was ONE 13-node tree whose best
 * node was the FIRST probe, so the dreamed policy {fixed-rounds, beta 1} (1
 * probe, 1 round) matched the incumbent's q 1.0 and anytime 1.0 and won purely
 * on the stop-early credit (cost 0.026 vs 0.51, roundsSaved 0.923 vs 0: V 1.0910
 * vs 1.0295; 336 of 337 grid policies were eligible), then probed once per
 * rollout online and scored the uniform baseline. Under this rule the candidate
 * is charged the full budget (B 39, 13 rounds) on that tree, V 0.95 against the
 * incumbent's raw 1.0295, and loses. The same collapse with two or more trees is
 * NOT closed by any frozen-pool rule: a fixed-rounds R candidate with R = the
 * largest roundsToBest in the pool spends at least its own horizon on every
 * tree, walks the incumbent's path (equal per-tree quality) and wins on the
 * spend it saves, and a pool of patience-stopped incumbent trees never holds
 * the "still improving after R" evidence the rule asks for (on 220 of 220
 * circle-packing and 35 of 35 sum-difference pairs and triples of such trees it
 * was a strict winner; rolled out on 60 fresh circle-packing seeds it scored
 * 1.052 against the incumbent's 1.193 with 2 probes instead of 30.6). A pool
 * whose trees all saturated by round R cannot be told from a task that does.
 * The online check is therefore the probation in `loop.ts`: an adopted policy's
 * first redeploy rollout must reach the incumbent's lowest replay best on the
 * pool it won on, or it is reverted and revoked for the run.
 *
 * Why the parallelism term is gone. The previous form
 * `V = q - beta1 N/(W k1) + beta2 N/(rounds W)` satisfied, with beta1 == beta2
 * == beta, the identity `V - q = (beta / W) * N * (1 / rounds - 1 / k1)`, which is
 * exactly zero whenever rounds == k1 for EVERY N. On the recorded run 2
 * (autocorrelation, W 3, k1 6 == DEFAULT_POLICY.beta, so patience could never
 * stop a rollout early) every replay ran k1 rounds and V collapsed to q: of 6549
 * reachable policies none could be both quality-eligible and V-better, and
 * dreaming was inert for that reason alone. Under the new form V is strictly
 * decreasing in S at every round count, and `beta2 > beta1` makes saving one
 * round worth more than the at most W probes it could cost, so the ordering is
 * rounds first, then probes, then earliness, with quality guarded separately
 * (`improve.ts`).
 *
 * Bounds. `value <= 1 + beta2`; the cost terms move V by at most
 * `beta1 * S / B + beta2 * |1 - rounds / k1|`. With beta3 = beta2 = 0 the
 * objective reduces to the strictly-cost form `q - beta1 * S / B`.
 *
 * What V cannot do. Replay reveals only what the recording holds, so no
 * candidate can score a higher quality than the recording contains: V rewards
 * "the same best for less" and can never reward "more". The quality guard
 * (`improve.ts`) is therefore blind to quality a cheaper policy would LOSE
 * online (its fewer probes reveal the recorded best because the recording is
 * there to be walked), every same-best-for-less winner is adopted, and once a
 * cheaper policy is deployed no exploring policy can re-win against it on the
 * trees it grew: its extra probes reveal nothing there, so it is `worse` at
 * equal quality. On the recorded autocorrelation pools the adopted winners were
 * measurably worse online in 7 of 8 cases (by 2e-6 to 8e-4 on a best of ~0.5005)
 * and spent 2 to 6 fewer probes; the ratchet only turns towards spending less.
 * This is the out-of-support limit of a frozen replay, not a tunable, and it is
 * why the selection scores only trees the incumbent replays in full support
 * (`measurePool`) and why the experiment headline, not V, is the measure of a
 * run.
 *
 * Break-even numbers on the recorded fixtures under this V (W 4, k1 12, k2 24,
 * `DEFAULT_OBJECTIVE`; `test/fixtures/dream`, circle-packing s7 rounds 1-2). The
 * exploring policy `1be99d403b0405a3` scores V 0.797390 (q 0.878487, anytime
 * 0.708267, cost 0.770833, roundsSaved 0) and the one-probe collapsed policy
 * `f559ec93fc3b1773` V 0.435252 (q 0.344627, cost 0.020833, roundsSaved
 * 0.916667); they tie in V only at beta1 = 0.533, a 10.7x margin over 0.05 (and
 * the quality guard rejects the collapsed policy regardless). The chain policy
 * (best-first, batchSize 1, stop never: 13 probes plus 11 out-of-support cells
 * over 24 rounds) reaches the same quality with cost 0.5 and anytime 0.803087
 * but roundsSaved -1, and ties the exploring policy only when beta2 <= 0.0372,
 * so 0.10 keeps full batches preferred with a 2.7x margin. On the run-2
 * autocorrelation dream pool (4 trees, W 3, k1 6) the incumbent scores
 * V 0.842156 (q 0.883344, anytime 0.849147, cost 11.75/18, roundsSaved 0: its
 * raw mean N 11.75) and the lever-scan grid (337 policies, 84 eligible) opens a
 * gap of +0.007109 with weighted/fixed-rounds/batchSize 2/beta 6 (V 0.849265:
 * same best, N 9 charged at 9.25 because its 8-probe tree is charged at another
 * tree's probesToBest 9, 6 rounds): "the same best for fewer probes", which the
 * old form scored as an exact tie. A replay gap is a replay statement only: on
 * recorded pools of 2 to 4 incumbent trees every adopted lever-scan winner (22
 * of 22, gaps 0.0006 to 0.065) rolled out with a lower mean best than the
 * incumbent on 40 fresh seeds, which is why the probation, not the gap, decides
 * what stays deployed.
 * See `test/dream-improve.test.ts` for the recorded collapse regressions and
 * `docs/dream-rsi.md` for the rest. The arithmetic is plain IEEE-754 in a fixed
 * operation order, so the same replay, scale and budget always yield the same V.
 */

import type { ReplayResult } from "./replay.js";
import type { RecordedTree } from "./store.js";

export interface ReplayObjectiveConfig {
	/** Cost of charging the whole per-rollout probe budget `W * k1`, in quality points. */
	beta1: number;
	/** Bonus for finishing in zero rounds (scaled by `1 - rounds / k1`), in quality points. */
	beta2: number;
	/** Weight of the anytime term against final quality, in [0, 1]. */
	beta3: number;
}

/**
 * Defaults: `beta2 > beta1` so one saved round outweighs the at most W probes it
 * can cost; `beta3 = 0.25` so a quarter of the quality weight rewards reaching
 * the best early. Both cost terms together are small against the quality
 * range (see the break-even figures in the module docstring).
 */
export const DEFAULT_OBJECTIVE: ReplayObjectiveConfig = { beta1: 0.05, beta2: 0.1, beta3: 0.25 };

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

/**
 * The spend horizon the cost terms are charged against: the latest probe and
 * round at which the same policy was still improving on the other measured
 * trees (`improve.ts`), or the whole budget when there is no other tree.
 */
export interface ObjectiveEvidence {
	probes: number;
	rounds: number;
}

export interface ObjectiveTerms {
	/** Best valid score, normalized into [0, 1] against the pool's range. */
	quality: number;
	/** Mean normalized best-so-far over the budget, in [0, quality]. */
	anytime: number;
	/** `chargedProbes / (W * k1)`: charged fraction of the probe budget (may exceed 1). */
	cost: number;
	/** `1 - chargedRounds / k1`, uncapped (negative past k1). */
	roundsSaved: number;
	/** `max(N + outOfSupportCells, evidence.probes)`: the selections the cost term charged. */
	chargedProbes: number;
	/** `max(rounds, evidence.rounds)`: the rounds the roundsSaved term charged. */
	chargedRounds: number;
	/** `(1 - beta3) * quality + beta3 * anytime - beta1 * cost + beta2 * roundsSaved`. */
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

/** The probe budget `W * k1` and round cap `k1` a tree is scored against, as the cost terms read them. */
export function objectiveBudgetOf(budget: ObjectiveBudget): { probes: number; rounds: number } {
	const workers = Math.max(1, Math.trunc(budget.workers));
	const k1 = Math.max(1, Math.trunc(budget.k1));
	return { probes: workers * k1, rounds: k1 };
}

/**
 * The terms of one replay. Without `evidence` the spend terms charge the replay's
 * own `S` and `rounds` (a single replay has no horizon to be charged against);
 * with it they charge `max(S, evidence.probes)` and `max(rounds, evidence.rounds)`.
 */
export function computeObjectiveTerms(
	replay: ReplayResult,
	cfg: ReplayObjectiveConfig,
	scale: ObjectiveScale,
	budget: ObjectiveBudget,
	evidence?: ObjectiveEvidence,
): ObjectiveTerms {
	const { probes: budgetProbes, rounds: k1 } = objectiveBudgetOf(budget);
	const quality = normalizedQuality(replay.bestScore, scale);
	const charged = replay.N + replay.outOfSupportCells;
	let anytimeSum = 0;
	for (let probe = 0; probe < charged; probe++) {
		anytimeSum += normalizedQuality(replay.bestSoFar[probe] ?? replay.bestScore, scale);
	}
	anytimeSum += Math.max(0, budgetProbes - charged) * quality;
	const anytime = anytimeSum / Math.max(budgetProbes, charged);
	const chargedProbes = evidence ? Math.max(charged, evidence.probes) : charged;
	const chargedRounds = evidence ? Math.max(replay.rounds, evidence.rounds) : replay.rounds;
	const cost = chargedProbes / budgetProbes;
	const roundsSaved = 1 - chargedRounds / k1;
	const value = (1 - cfg.beta3) * quality + cfg.beta3 * anytime - cfg.beta1 * cost + cfg.beta2 * roundsSaved;
	return { quality, anytime, cost, roundsSaved, chargedProbes, chargedRounds, value };
}

export function computeObjective(
	replay: ReplayResult,
	cfg: ReplayObjectiveConfig,
	scale: ObjectiveScale,
	budget: ObjectiveBudget,
): number {
	return computeObjectiveTerms(replay, cfg, scale, budget).value;
}
