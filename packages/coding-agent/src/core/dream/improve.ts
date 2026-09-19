/**
 * Stage 3: dreaming policy improvement by local search over the typed policy.
 *
 * From the current policy, `proposePolicies` produces M candidates by seeded
 * mutation (perturb one or two numeric fields with a bounded gaussian, or flip
 * one named rule), each drawn from an independent `rng.fork` so the candidate
 * set is order-independent. Every candidate is scored on the frozen tree pool by
 * the mean replay objective, and `selectBestPolicy` returns the argmax of
 * {current} ∪ candidates. Because the current policy is always in the set and
 * wins ties, and V is a pure function of a recorded tree, the chosen policy is
 * UNCONDITIONALLY no-worse than current on replay. The local path spends zero
 * model tokens; an optional injected candidate proposer (the flag-gated LLM
 * dreamer) supplies constrained-JSON candidates that are scored and selected by
 * the exact same rule, so a bad LLM policy can never regress the deployed one.
 */

import { withSpan } from "@earendil-works/pi-ai";
import { computeObjective, DEFAULT_OBJECTIVE, type ReplayObjectiveConfig } from "./objective.js";
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

/** Ties in objective score within this tolerance resolve in favour of the current policy. */
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
	k2: number;
	objective: ReplayObjectiveConfig;
}

/**
 * Mean replay objective of a policy over the pool, evaluated on a deterministic
 * treeId ordering. Replay is rng-free, so `_rng` is accepted only to match the
 * documented surface and is not consulted. An empty pool scores 0.
 */
export function scorePolicyOnPool(
	policy: ExplorationPolicy,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	_rng?: SeededRng,
): number {
	if (pool.length === 0) return 0;
	const sorted = [...pool].sort((a, b) => a.header.treeId.localeCompare(b.header.treeId));
	let sum = 0;
	for (const tree of sorted) {
		sum += computeObjective(simulatePolicy(tree, policy, { k2: cfg.k2 }), cfg.objective);
	}
	return sum / sorted.length;
}

export interface PolicySelection {
	chosenPolicy: ExplorationPolicy;
	chosenScore: number;
	currentScore: number;
	improved: boolean;
	scoredCount: number;
	candidatePolicyIds: string[];
}

/**
 * Score {current} ∪ candidates and return the argmax. Ties within `SELECT_EPS`
 * resolve to the current policy, then to the lowest policyId. The chosen policy
 * is therefore never worse than current on the pool, and `improved` is true only
 * when a candidate strictly beats current.
 */
export function selectBestPolicy(
	current: ExplorationPolicy,
	candidates: readonly ExplorationPolicy[],
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): PolicySelection {
	const currentScore = scorePolicyOnPool(current, pool, cfg);
	const entries = [
		{ policy: current, id: policyId(current), score: currentScore, isCurrent: true },
		...candidates.map((policy) => ({
			policy,
			id: policyId(policy),
			score: scorePolicyOnPool(policy, pool, cfg),
			isCurrent: false,
		})),
	];
	const maxScore = Math.max(...entries.map((entry) => entry.score));
	const winners = entries.filter((entry) => entry.score >= maxScore - SELECT_EPS);
	const currentWinner = winners.find((entry) => entry.isCurrent);
	const winner = currentWinner ?? [...winners].sort((a, b) => a.id.localeCompare(b.id))[0]!;
	const improved = !winner.isCurrent && winner.score > currentScore + SELECT_EPS;
	return {
		chosenPolicy: winner.policy,
		chosenScore: winner.score,
		currentScore,
		improved,
		scoredCount: entries.length,
		candidatePolicyIds: candidates.map(policyId),
	};
}

export interface DreamingOptions {
	current: ExplorationPolicy;
	pool: readonly RecordedTree[];
	/** Revised policies M per dreaming step. */
	dreams: number;
	k2: number;
	rng: SeededRng;
	objective?: ReplayObjectiveConfig;
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
	improved: boolean;
	scoredCount: number;
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
	const cfg: DreamingScoreConfig = { k2: options.k2, objective };
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
				"dream.improved": selection.improved,
			});
			return {
				chosenPolicy: selection.chosenPolicy,
				chosenPolicyId: policyId(selection.chosenPolicy),
				chosenScore: selection.chosenScore,
				currentScore: selection.currentScore,
				improved: selection.improved,
				scoredCount: selection.scoredCount,
				candidatePolicyIds: selection.candidatePolicyIds,
				poolSize: options.pool.length,
				tokens: 0,
			};
		},
	);
}
