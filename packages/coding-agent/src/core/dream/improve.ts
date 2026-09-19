/**
 * Stage 3: dreaming policy improvement by local search over the typed policy.
 *
 * From the current policy, `proposePolicies` produces M candidates by seeded
 * mutation (perturb one or two numeric fields with a bounded gaussian, or flip
 * one named rule), each drawn from an independent `rng.fork` so the candidate
 * set is order-independent. Every candidate is scored on the frozen tree pool by
 * the mean replay objective, and `selectBestPolicy` returns the argmax of
 * {current} ∪ candidates among the candidates whose mean replay QUALITY is no
 * lower than the current policy's. V is a pure function of (recorded tree, pool
 * score scale, per-rollout budget), the current policy is always in the set and
 * wins ties, so the chosen policy is UNCONDITIONALLY no-worse than current on
 * replay in V and never lower in quality: a candidate cannot win by collapsing
 * exploration, only by reaching at least the same best for less. The local path
 * spends zero model tokens; an optional injected candidate proposer (the
 * flag-gated LLM dreamer) supplies constrained-JSON candidates that are scored
 * and selected by the exact same rule, so a bad LLM policy can never regress the
 * deployed one.
 */

import { withSpan } from "@earendil-works/pi-ai";
import {
	computeObjectiveTerms,
	DEFAULT_OBJECTIVE,
	type ObjectiveScale,
	poolScoreScale,
	type ReplayObjectiveConfig,
} from "./objective.js";
import {
	clampPolicy,
	type ExplorationPolicy,
	POLICY_BOUNDS,
	policyId,
	RECOVERY_POLICIES,
	SELECTION_RULES,
	STOP_RULES,
} from "./policy.js";
import { simulatePolicy } from "./replay.js";
import type { SeededRng } from "./rng.js";
import type { RecordedTree } from "./store.js";

/** Ties in objective score (and the quality guard) within this tolerance resolve in favour of the current policy. */
const SELECT_EPS = 1e-9;
/** Gaussian perturbation size as a fraction of a numeric field's range. */
const MUTATION_SCALE = 0.15;
/** Probability a mutation flips a named rule instead of perturbing numbers. */
const RULE_FLIP_PROBABILITY = 0.34;

type NumericField = keyof typeof POLICY_BOUNDS;
const NUMERIC_FIELDS = Object.keys(POLICY_BOUNDS) as NumericField[];

/** Perturb one or two numeric fields, or flip one named rule; always re-clamped. */
export function mutatePolicy(policy: ExplorationPolicy, rng: SeededRng): ExplorationPolicy {
	const next: Record<string, unknown> = { ...policy };
	if (rng.next() < RULE_FLIP_PROBABILITY) {
		const which = rng.nextInt(3);
		if (which === 0) next.selectionRule = SELECTION_RULES[rng.nextInt(SELECTION_RULES.length)];
		else if (which === 1) next.recoveryPolicy = RECOVERY_POLICIES[rng.nextInt(RECOVERY_POLICIES.length)];
		else next.stopRule = STOP_RULES[rng.nextInt(STOP_RULES.length)];
	} else {
		const count = 1 + rng.nextInt(2);
		for (let i = 0; i < count; i++) {
			const field = NUMERIC_FIELDS[rng.nextInt(NUMERIC_FIELDS.length)]!;
			const bound = POLICY_BOUNDS[field];
			const span = bound.max - bound.min;
			next[field] = policy[field] + rng.nextGaussian() * span * MUTATION_SCALE;
		}
	}
	return clampPolicy(next);
}

/**
 * M candidates, each from `rng.fork(\`cand:${i}\`)`. Because each candidate
 * depends only on its own fork, the set is independent of iteration order.
 */
export function proposePolicies(current: ExplorationPolicy, m: number, rng: SeededRng): ExplorationPolicy[] {
	const count = Math.max(0, Math.trunc(m));
	const out: ExplorationPolicy[] = [];
	for (let i = 0; i < count; i++) {
		out.push(mutatePolicy(current, rng.fork(`cand:${i}`)));
	}
	return out;
}

export interface DreamingScoreConfig {
	/** Max online rounds; with a tree's `w` it fixes the per-rollout probe budget the cost term is measured against. */
	k1: number;
	/** Max replay rounds per simulation. */
	k2: number;
	objective: ReplayObjectiveConfig;
	/**
	 * How far (in pool-range units, so scale-free) a candidate's mean replay
	 * quality may fall below the current policy's and still be eligible. Default 0:
	 * quality must not regress.
	 */
	qualityEps?: number;
}

/** A policy's mean replay objective and mean normalized quality over a pool. */
export interface PoolScore {
	value: number;
	quality: number;
}

function scoreOnPool(
	policy: ExplorationPolicy,
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	scale: ObjectiveScale,
): PoolScore {
	if (sorted.length === 0) return { value: 0, quality: 0 };
	let value = 0;
	let quality = 0;
	for (const tree of sorted) {
		const terms = computeObjectiveTerms(simulatePolicy(tree, policy, { k2: cfg.k2 }), cfg.objective, scale, {
			workers: tree.header.w,
			k1: cfg.k1,
		});
		value += terms.value;
		quality += terms.quality;
	}
	return { value: value / sorted.length, quality: quality / sorted.length };
}

function sortedPool(pool: readonly RecordedTree[]): RecordedTree[] {
	return [...pool].sort((a, b) => a.header.treeId.localeCompare(b.header.treeId));
}

/**
 * Mean replay objective and mean normalized quality of a policy over the pool,
 * scored against the pool's own score scale and evaluated on a deterministic
 * treeId ordering. Replay is rng-free, so `_rng` is accepted only to match the
 * documented surface and is not consulted. An empty pool scores 0.
 */
export function scorePolicyOnPool(
	policy: ExplorationPolicy,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	_rng?: SeededRng,
): PoolScore {
	return scoreOnPool(policy, sortedPool(pool), cfg, poolScoreScale(pool));
}

export interface PolicySelection {
	chosenPolicy: ExplorationPolicy;
	chosenScore: number;
	currentScore: number;
	/** Mean normalized replay quality of the chosen policy on the pool. */
	chosenQuality: number;
	/** Mean normalized replay quality of the current policy on the pool. */
	currentQuality: number;
	improved: boolean;
	scoredCount: number;
	/** Candidates excluded by the quality guard before the argmax. */
	qualityRejected: number;
	candidatePolicyIds: string[];
}

/**
 * Score {current} ∪ candidates and return the argmax over the ELIGIBLE entries:
 * the current policy, plus every candidate whose mean quality is at least
 * `current - qualityEps` (within `SELECT_EPS`). Ties in V resolve to the current
 * policy, then to the lowest policyId. The chosen policy is therefore never
 * worse than current in V and never lower in quality, and `improved` is true
 * only when an eligible candidate strictly beats current in V.
 */
export function selectBestPolicy(
	current: ExplorationPolicy,
	candidates: readonly ExplorationPolicy[],
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): PolicySelection {
	const sorted = sortedPool(pool);
	const scale = poolScoreScale(sorted);
	const currentScore = scoreOnPool(current, sorted, cfg, scale);
	const qualityFloor = currentScore.quality - Math.max(0, cfg.qualityEps ?? 0) - SELECT_EPS;
	const entries = [
		{ policy: current, id: policyId(current), score: currentScore, isCurrent: true },
		...candidates.map((policy) => ({
			policy,
			id: policyId(policy),
			score: scoreOnPool(policy, sorted, cfg, scale),
			isCurrent: false,
		})),
	];
	const eligible = entries.filter((entry) => entry.isCurrent || entry.score.quality >= qualityFloor);
	const maxScore = Math.max(...eligible.map((entry) => entry.score.value));
	const winners = eligible.filter((entry) => entry.score.value >= maxScore - SELECT_EPS);
	const currentWinner = winners.find((entry) => entry.isCurrent);
	const winner = currentWinner ?? [...winners].sort((a, b) => a.id.localeCompare(b.id))[0]!;
	const improved = !winner.isCurrent && winner.score.value > currentScore.value + SELECT_EPS;
	return {
		chosenPolicy: winner.policy,
		chosenScore: winner.score.value,
		currentScore: currentScore.value,
		chosenQuality: winner.score.quality,
		currentQuality: currentScore.quality,
		improved,
		scoredCount: entries.length,
		qualityRejected: entries.length - eligible.length,
		candidatePolicyIds: candidates.map(policyId),
	};
}

export interface DreamingOptions {
	current: ExplorationPolicy;
	pool: readonly RecordedTree[];
	/** Revised policies M per dreaming step. */
	dreams: number;
	/** Max online rounds of the rollouts in the pool (the cost term's budget with each tree's `w`). */
	k1: number;
	/** Max replay rounds per policy simulation. */
	k2: number;
	rng: SeededRng;
	objective?: ReplayObjectiveConfig;
	/** See `DreamingScoreConfig.qualityEps`; default 0. */
	qualityEps?: number;
	/**
	 * Optional injected candidate proposer (the flag-gated LLM dreamer). It must
	 * return already-parsed, in-bounds policies; they are scored and selected by
	 * the same rule as local candidates.
	 */
	proposeCandidates?: (current: ExplorationPolicy, m: number, rng: SeededRng) => ExplorationPolicy[];
}

export interface DreamResult {
	chosenPolicy: ExplorationPolicy;
	chosenPolicyId: string;
	chosenScore: number;
	currentScore: number;
	chosenQuality: number;
	currentQuality: number;
	improved: boolean;
	scoredCount: number;
	qualityRejected: number;
	candidatePolicyIds: string[];
	poolSize: number;
	tokens: number;
}

/**
 * One dreaming step: propose M candidates, score {current} ∪ candidates on the
 * pool, select the no-worse policy. Opens `dream.dream` wrapping one coarse
 * `dream.replay` summary span (one per dreaming step, not one per simulation).
 */
export function runDreaming(options: DreamingOptions): DreamResult {
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const cfg: DreamingScoreConfig = {
		k1: options.k1,
		k2: options.k2,
		objective,
		...(options.qualityEps !== undefined ? { qualityEps: options.qualityEps } : {}),
	};
	const propose = options.proposeCandidates ?? proposePolicies;
	const candidates = propose(options.current, options.dreams, options.rng);
	return withSpan(
		"dream.dream",
		{ "dream.candidates": candidates.length, "dream.pool_size": options.pool.length },
		(span) => {
			const selection = withSpan(
				"dream.replay",
				{
					"dream.policy_id": policyId(options.current),
					"dream.simulations": (candidates.length + 1) * options.pool.length,
				},
				() => selectBestPolicy(options.current, candidates, options.pool, cfg),
			);
			span.setAttributes({
				"dream.chosen_policy_id": policyId(selection.chosenPolicy),
				"dream.chosen_score": selection.chosenScore,
				"dream.current_score": selection.currentScore,
				"dream.chosen_quality": selection.chosenQuality,
				"dream.current_quality": selection.currentQuality,
				"dream.quality_rejected": selection.qualityRejected,
				"dream.improved": selection.improved,
			});
			return {
				chosenPolicy: selection.chosenPolicy,
				chosenPolicyId: policyId(selection.chosenPolicy),
				chosenScore: selection.chosenScore,
				currentScore: selection.currentScore,
				chosenQuality: selection.chosenQuality,
				currentQuality: selection.currentQuality,
				improved: selection.improved,
				scoredCount: selection.scoredCount,
				qualityRejected: selection.qualityRejected,
				candidatePolicyIds: selection.candidatePolicyIds,
				poolSize: options.pool.length,
				tokens: 0,
			};
		},
	);
}
