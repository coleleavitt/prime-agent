/**
 * Stage 3: dreaming policy improvement by local search over the typed policy.
 *
 * From the current policy, `proposePolicies` produces M candidates by seeded
 * mutation (perturb one or two replay-live numeric fields with a bounded
 * gaussian, or flip the selection or stop rule), each drawn from an independent
 * `rng.fork` so the candidate set is order-independent. Replay-dead fields
 * (`REPLAY_DEAD_FIELDS`: branchWidth, refineDepth, recoveryPolicy) are never
 * mutated because a change to them replays identically and could never win; a
 * mutation that lands on the current policy's own id is retried on a sub-fork.
 *
 * Every candidate is scored on the MEASURED pool by the mean replay objective
 * (`objective.ts`: quality, anytime, charged cost, rounds saved), and
 * `selectBestPolicy` returns the argmax of {current} ∪ candidates among the
 * candidates whose replay QUALITY is no lower than the current policy's on
 * EVERY measured tree. A candidate's spend terms are evidence-backed
 * (`objective.ts`): on each measured tree it is charged at least the latest
 * probe and round at which it was still improving on the OTHER measured trees,
 * and the whole budget when there is no other tree, so a stop-early credit
 * needs a second tree to vouch for it. The incumbent is charged its RAW spend:
 * the measured trees are the ones it grew, so its recorded probes and rounds
 * ARE its online behaviour and a horizon taken from its late trees would
 * surcharge the early stops it really made (on a recorded circle-packing pool
 * of one late-best and three patience-stopped trees the symmetric rule charged
 * it 31 probes / 13 rounds where it spent 18/8, 15/7 and 19/8, and 106 grid
 * candidates that lose to the raw incumbent won against the surcharged one
 * while replaying identically under both rules). Asymmetric, a candidate is
 * never charged below its own spend and the incumbent never above its real one.
 * The recorded run 3 collapsed for want of a second tree (one 13-node tree
 * whose best was the first probe; the fixture and regression are in
 * `test/dream-improve.test.ts`). The rule closes that single-tree instance
 * only: a pool whose trees all happened to saturate by round R cannot be told
 * from a task that saturates by round R, so a fixed-rounds R candidate still
 * wins on such a pool and its online check is the probation in `loop.ts`
 * (`DreamProbationRecord`); a reverted policy's id comes back here as `revoked`.
 * The measured pool is the frozen pool minus every tree the current policy
 * replays out of support: on such a tree (one another policy grew — a priming
 * tree, or a tree from before a policy change) the incumbent's replay reveals
 * almost nothing and burns out-of-support rounds, so it is not a baseline, and
 * comparing a candidate against it would credit fictional quality gains that can
 * mask a real regression on the incumbent's own trees, or hand a win to a
 * candidate that merely burns fewer dead rounds while behaving identically
 * online. Only the incumbent's support decides membership; a candidate that is
 * off support on a measured tree is charged for it (and reported
 * `unmeasurable`), which is the off-policy penalty by design.
 * V is a pure function of (recorded tree, pool score scale, per-rollout budget),
 * the current policy is always in the set and wins ties, so the chosen policy is
 * UNCONDITIONALLY no-worse than current on replay in V and never lower in
 * quality: a candidate cannot win by collapsing exploration, only by reaching at
 * least the same best in fewer rounds, with fewer charged probes, or earlier.
 * The old objective's parallelism term is gone because it cancelled the cost
 * term exactly whenever a replay used all k1 rounds (see `objective.ts`), which
 * left "same best for less" unrewarded on every recorded run-2 pool.
 *
 * Each candidate gets a `CandidateVerdict` (why it did or did not win, with its
 * pool-mean terms and support coverage) and every step also runs a `leverScan`:
 * a fixed grid of local policies scored under the same rule, so the record says
 * whether the pool had ANY lever independent of what the dreamer proposed.
 *
 * The local path spends zero model tokens; an optional injected candidate
 * proposer (the flag-gated LLM dreamer) supplies constrained-JSON candidates
 * that are scored and selected by the exact same rule, so a bad LLM policy can
 * never regress the deployed one.
 */

import { withSpan } from "@earendil-works/pi-ai";
import {
	computeObjectiveTerms,
	DEFAULT_OBJECTIVE,
	type ObjectiveEvidence,
	type ObjectiveScale,
	type ObjectiveTerms,
	objectiveBudgetOf,
	poolScoreScale,
	type ReplayObjectiveConfig,
} from "./objective.js";
import {
	clampPolicy,
	differsOnlyInReplayDeadFields,
	type ExplorationPolicy,
	POLICY_BOUNDS,
	policyFieldsDiffering,
	policyId,
	REPLAY_DEAD_FIELDS,
	SELECTION_RULES,
	STOP_RULES,
} from "./policy.js";
import { type ReplayResult, simulatePolicy } from "./replay.js";
import type { SeededRng } from "./rng.js";
import type { RecordedTree } from "./store.js";
import type { CandidateOrigin, CandidateReason, CandidateVerdict, DreamerKind, LeverScanRecord } from "./types.js";

/** Ties in objective score (and the quality guard) within this tolerance resolve in favour of the current policy. */
const SELECT_EPS = 1e-9;
/** Gaussian perturbation size as a fraction of a numeric field's range. */
const MUTATION_SCALE = 0.15;
/** Probability a mutation flips a named rule instead of perturbing numbers. */
const RULE_FLIP_PROBABILITY = 0.34;
/** Sub-fork retries before a mutation that keeps landing on the current id is forced to differ. */
const MUTATION_RETRIES = 8;

type NumericField = keyof typeof POLICY_BOUNDS;
/** The numeric fields replay reads; the rest are online-only and never mutated. */
const MUTABLE_NUMERIC_FIELDS = (Object.keys(POLICY_BOUNDS) as NumericField[]).filter(
	(field) => !REPLAY_DEAD_FIELDS.includes(field),
);

function mutateOnce(policy: ExplorationPolicy, rng: SeededRng): ExplorationPolicy {
	const next: Record<string, unknown> = { ...policy };
	if (rng.next() < RULE_FLIP_PROBABILITY) {
		if (rng.nextInt(2) === 0) next.selectionRule = SELECTION_RULES[rng.nextInt(SELECTION_RULES.length)];
		else next.stopRule = STOP_RULES[rng.nextInt(STOP_RULES.length)];
	} else {
		const count = 1 + rng.nextInt(2);
		for (let i = 0; i < count; i++) {
			const field = MUTABLE_NUMERIC_FIELDS[rng.nextInt(MUTABLE_NUMERIC_FIELDS.length)]!;
			const bound = POLICY_BOUNDS[field];
			const span = bound.max - bound.min;
			next[field] = policy[field] + rng.nextGaussian() * span * MUTATION_SCALE;
		}
	}
	return clampPolicy(next);
}

/**
 * Perturb one or two replay-live numeric fields, or flip the selection or stop
 * rule; always re-clamped and never equal to `policy` (a mutation that clamps
 * back onto the current id is retried on `rng.fork("retry:<i>")`, and after
 * `MUTATION_RETRIES` the selection rule is advanced deterministically).
 */
export function mutatePolicy(policy: ExplorationPolicy, rng: SeededRng): ExplorationPolicy {
	const currentId = policyId(policy);
	for (let attempt = 0; attempt <= MUTATION_RETRIES; attempt++) {
		const candidate = mutateOnce(policy, attempt === 0 ? rng : rng.fork(`retry:${attempt}`));
		if (policyId(candidate) !== currentId) return candidate;
	}
	const index = SELECTION_RULES.indexOf(policy.selectionRule);
	return { ...policy, selectionRule: SELECTION_RULES[(index + 1) % SELECTION_RULES.length]! };
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
	 * How far (in pool-range units, so scale-free) a candidate's replay quality
	 * may fall below the current policy's on any measured tree and still be
	 * eligible. Default 0: quality must not regress on any tree.
	 */
	qualityEps?: number;
}

/** A policy's mean replay terms over a pool, plus its support coverage. */
export interface PoolScore {
	value: number;
	quality: number;
	anytime: number;
	cost: number;
	roundsSaved: number;
	/** Mean revealed non-root nodes per tree. */
	N: number;
	/** Mean replay decision rounds per tree. */
	rounds: number;
	/** Mean out-of-support cells per tree. */
	outOfSupportCells: number;
	/** Mean `ReplayResult.inSupport` over the pool (1 for an empty pool). */
	inSupportMean: number;
	/** Min `ReplayResult.inSupport` over the pool (1 for an empty pool). */
	inSupportMin: number;
	/** Mean `ObjectiveTerms.chargedProbes` per tree (evidence-backed; see `objective.ts`). */
	chargedProbes: number;
	/** Mean `ObjectiveTerms.chargedRounds` per tree. */
	chargedRounds: number;
}

function emptyPoolScore(): PoolScore {
	return {
		value: 0,
		quality: 0,
		anytime: 0,
		cost: 0,
		roundsSaved: 0,
		N: 0,
		rounds: 0,
		outOfSupportCells: 0,
		inSupportMean: 1,
		inSupportMin: 1,
		chargedProbes: 0,
		chargedRounds: 0,
	};
}

function simulateOnPool(
	policy: ExplorationPolicy,
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): ReplayResult[] {
	return sorted.map((tree) => simulatePolicy(tree, policy, { k2: cfg.k2 }));
}

/**
 * The spend horizon tree `index` is charged against: the largest `probesToBest`
 * and `roundsToBest` the same policy recorded on the OTHER trees of the pool,
 * or the tree's whole budget (`W * k1` probes, `k1` rounds) when it is alone.
 */
function evidenceFor(
	replays: readonly ReplayResult[],
	index: number,
	tree: RecordedTree,
	cfg: DreamingScoreConfig,
): ObjectiveEvidence {
	if (replays.length <= 1) return objectiveBudgetOf({ workers: tree.header.w, k1: cfg.k1 });
	let probes = 0;
	let rounds = 0;
	replays.forEach((other, otherIndex) => {
		if (otherIndex === index) return;
		if (other.probesToBest > probes) probes = other.probesToBest;
		if (other.roundsToBest > rounds) rounds = other.roundsToBest;
	});
	return { probes, rounds };
}

/**
 * How a policy's spend is charged on a pool: `evidence` (a candidate: each tree
 * at least at the horizon its other replays establish, the whole budget when
 * alone) or `raw` (the incumbent: exactly the probes and rounds it recorded).
 */
export type SpendCharge = "evidence" | "raw";

/**
 * The objective terms of one replay per tree (`replays[i]` is `sorted[i]`'s),
 * each tree's spend charged per `charge`. The per-tree qualities feed the guard;
 * the dreamer prompt digests the incumbent's `raw` numbers.
 */
export function termsOnPool(
	replays: readonly ReplayResult[],
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	scale: ObjectiveScale,
	charge: SpendCharge,
): ObjectiveTerms[] {
	return sorted.map((tree, index) =>
		computeObjectiveTerms(
			replays[index]!,
			cfg.objective,
			scale,
			{ workers: tree.header.w, k1: cfg.k1 },
			charge === "evidence" ? evidenceFor(replays, index, tree, cfg) : undefined,
		),
	);
}

/** Aggregate one replay per tree and its terms into the pool means. */
function aggregateReplays(replays: readonly ReplayResult[], terms: readonly ObjectiveTerms[]): PoolScore {
	if (replays.length === 0) return emptyPoolScore();
	const sum = emptyPoolScore();
	sum.inSupportMean = 0;
	replays.forEach((replay, index) => {
		const term = terms[index]!;
		sum.value += term.value;
		sum.quality += term.quality;
		sum.anytime += term.anytime;
		sum.cost += term.cost;
		sum.roundsSaved += term.roundsSaved;
		sum.chargedProbes += term.chargedProbes;
		sum.chargedRounds += term.chargedRounds;
		sum.N += replay.N;
		sum.rounds += replay.rounds;
		sum.outOfSupportCells += replay.outOfSupportCells;
		sum.inSupportMean += replay.inSupport;
		if (replay.inSupport < sum.inSupportMin) sum.inSupportMin = replay.inSupport;
	});
	const count = replays.length;
	return {
		value: sum.value / count,
		quality: sum.quality / count,
		anytime: sum.anytime / count,
		cost: sum.cost / count,
		roundsSaved: sum.roundsSaved / count,
		N: sum.N / count,
		rounds: sum.rounds / count,
		outOfSupportCells: sum.outOfSupportCells / count,
		inSupportMean: sum.inSupportMean / count,
		inSupportMin: sum.inSupportMin,
		chargedProbes: sum.chargedProbes / count,
		chargedRounds: sum.chargedRounds / count,
	};
}

/** A policy's pool means plus its per-tree terms (in `sorted` order). */
interface PoolTerms {
	score: PoolScore;
	terms: ObjectiveTerms[];
}

function poolTermsOf(
	replays: readonly ReplayResult[],
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	scale: ObjectiveScale,
	charge: SpendCharge,
): PoolTerms {
	const terms = termsOnPool(replays, sorted, cfg, scale, charge);
	return { score: aggregateReplays(replays, terms), terms };
}

function scoreOnPool(
	policy: ExplorationPolicy,
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	scale: ObjectiveScale,
	charge: SpendCharge,
): PoolTerms {
	return poolTermsOf(simulateOnPool(policy, sorted, cfg), sorted, cfg, scale, charge);
}

/** Measured trees minus one, floor 0: the trees that can vouch for a stop-early credit. */
export function evidenceTreesOf(measuredTrees: number): number {
	return Math.max(0, Math.trunc(measuredTrees) - 1);
}

function sortedPool(pool: readonly RecordedTree[]): RecordedTree[] {
	return [...pool].sort((a, b) => a.header.treeId.localeCompare(b.header.treeId));
}

/**
 * Mean replay objective and mean normalized quality of a policy over the pool
 * AS GIVEN (no measured-pool exclusion: that is `selectBestPolicy`'s job), scored
 * against the pool's own score scale and evaluated on a deterministic treeId
 * ordering. `charge` defaults to `evidence` (how a candidate is scored); pass
 * `raw` for the incumbent's own numbers. Replay is rng-free. An empty pool
 * scores 0.
 */
export function scorePolicyOnPool(
	policy: ExplorationPolicy,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	charge: SpendCharge = "evidence",
): PoolScore {
	return scoreOnPool(policy, sortedPool(pool), cfg, poolScoreScale(pool), charge).score;
}

/** The trees of a pool the current policy replays in full support, with the incumbent's replays and its mean in-support share over the whole pool. */
export interface MeasuredPool {
	/** Trees handed in, sorted by tree id. */
	sorted: RecordedTree[];
	/** The subset of `sorted` whose incumbent replay charged no out-of-support cell, in the same order. */
	measured: RecordedTree[];
	/** The incumbent's replay on each `measured` tree. */
	replays: ReplayResult[];
	/** Mean `ReplayResult.inSupport` of the incumbent over ALL of `sorted` (1 for an empty pool). */
	currentInSupport: number;
}

/**
 * Split a pool into the trees the current policy replays exactly in support
 * (the measured pool) and the rest. Costs one simulation per tree.
 */
export function measurePool(
	current: ExplorationPolicy,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): MeasuredPool {
	const sorted = sortedPool(pool);
	const all = simulateOnPool(current, sorted, cfg);
	const measured: RecordedTree[] = [];
	const replays: ReplayResult[] = [];
	let inSupportSum = 0;
	sorted.forEach((tree, index) => {
		const replay = all[index]!;
		inSupportSum += replay.inSupport;
		if (replay.outOfSupportCells === 0) {
			measured.push(tree);
			replays.push(replay);
		}
	});
	return {
		sorted,
		measured,
		replays,
		currentInSupport: sorted.length === 0 ? 1 : inSupportSum / sorted.length,
	};
}

/** A candidate with its provenance; a bare `ExplorationPolicy` reads as origin `local`. */
export interface CandidateInput {
	policy: ExplorationPolicy;
	origin: CandidateOrigin;
}

export type CandidateList = readonly ExplorationPolicy[] | readonly CandidateInput[];

function isCandidateInput(item: ExplorationPolicy | CandidateInput): item is CandidateInput {
	return "policy" in item && typeof item.policy === "object";
}

function normalizeCandidates(candidates: CandidateList): CandidateInput[] {
	return (candidates as readonly (ExplorationPolicy | CandidateInput)[]).map((item) =>
		isCandidateInput(item) ? { policy: item.policy, origin: item.origin } : { policy: item, origin: "local" },
	);
}

/** `llm` when every candidate came from a child agent, `local` when none did, else `mixed`. */
export function dreamerKindOf(candidates: readonly CandidateInput[]): DreamerKind {
	if (candidates.length === 0) return "local";
	const llm = candidates.filter((candidate) => candidate.origin === "llm").length;
	return llm === 0 ? "local" : llm === candidates.length ? "llm" : "mixed";
}

export interface PolicySelection {
	chosenPolicy: ExplorationPolicy;
	chosenScore: number;
	currentScore: number;
	/** Mean normalized replay quality of the chosen policy on the pool. */
	chosenQuality: number;
	/** Mean normalized replay quality of the current policy on the pool. */
	currentQuality: number;
	/** The current policy's full score on the measured pool (spend charged raw). */
	current: PoolScore;
	/**
	 * The current policy's lowest replay `bestScore` over the measured trees (0
	 * with none): the probation floor an adopted candidate's first online rollout
	 * must reach (`loop.ts`).
	 */
	currentMinBest: number;
	improved: boolean;
	/** Policies scored: current plus every distinct, non-identical candidate (a replay-dead-only one is scored as current). */
	scoredCount: number;
	/** Candidates whose verdict is `quality-rejected` (in support, simulated, failed the guard); never overlaps `unmeasurable`. */
	qualityRejected: number;
	candidatePolicyIds: string[];
	/** One verdict per candidate, in input order. */
	candidates: CandidateVerdict[];
	dreamer: DreamerKind;
	/** Trees handed in. */
	poolSize: number;
	/** Trees the current policy replays in full support; every score is a mean over these only. */
	measuredTrees: number;
	/** `measuredTrees - 1` (floor 0): the trees whose replays can vouch for a stop-early credit. */
	evidenceTrees: number;
	/** The current policy's mean in-support share over the WHOLE pool (1 for an empty pool). */
	currentInSupport: number;
	/** `simulatePolicy` calls this selection made: the current policy on every tree, then each simulated candidate on the measured trees. */
	simulations: number;
}

interface ScoredEntry {
	input: CandidateInput;
	index: number;
	id: string;
	changed: string[];
	duplicateOf: number | null;
	identical: boolean;
	/** Differs from current only in `REPLAY_DEAD_FIELDS`: scored as current, never simulated. */
	replayDead: boolean;
	/** An id this run already adopted and reverted: simulated for the record, never eligible. */
	revoked: boolean;
	score: PoolScore;
	/** Simulated (not identical, duplicate or replay-dead), not revoked, and passed the quality guard. */
	eligible: boolean;
	/** Simulated (not identical, duplicate or replay-dead) and failed the quality guard. */
	qualityRejected: boolean;
}

/** True when `terms` is no lower in quality than `baseline` on EVERY tree, within `qualityEps + SELECT_EPS`. */
function passesQualityGuard(
	terms: readonly ObjectiveTerms[],
	baseline: readonly ObjectiveTerms[],
	qualityEps: number,
): boolean {
	const slack = Math.max(0, qualityEps) + SELECT_EPS;
	return terms.every((term, index) => term.quality >= baseline[index]!.quality - slack);
}

/**
 * Score {current} ∪ candidates on the measured pool (`measurePool`: the trees
 * the current policy replays in full support) and return the argmax over the
 * ELIGIBLE entries: the current policy, plus every distinct candidate whose
 * replay quality on EVERY measured tree is at least the current policy's on
 * that tree minus `qualityEps` (within `SELECT_EPS`). Ties in V resolve to the
 * current policy, then to the lowest policyId. The chosen policy is therefore
 * never worse than current in V and never lower in quality on any measured
 * tree, and `improved` is true only when an eligible candidate strictly beats
 * current in V. The result is identical to calling this on the measured trees
 * alone; when no tree is measured nothing is eligible and every simulated
 * candidate is `unmeasurable`. A candidate's spend is charged against the
 * horizon its own replays on the other measured trees establish (`termsOnPool`
 * with `evidence`), so with one measured tree no candidate earns a stop-early
 * credit; the incumbent's spend is its raw recorded one (`raw`), never above
 * what it really spent on the trees it grew.
 *
 * A candidate whose id equals the current's (`identical`) or an earlier
 * candidate's (`duplicate`) is not simulated again and never enters the argmax.
 * A candidate that differs from current only in replay-dead fields is scored
 * as current without a simulation (its replay is the same walk), kept out of
 * the argmax and reported `unmeasurable`. A candidate whose id is in `revoked`
 * (a policy this run adopted and reverted after its probation rollout) is
 * simulated so its verdict carries real numbers but is never eligible.
 */
export function selectBestPolicy(
	current: ExplorationPolicy,
	candidates: CandidateList,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	revoked: ReadonlySet<string> = new Set<string>(),
): PolicySelection {
	const inputs = normalizeCandidates(candidates);
	const measuredPool = measurePool(current, pool, cfg);
	const measured = measuredPool.measured;
	const measurable = measured.length > 0;
	const scale = poolScoreScale(measured);
	const currentId = policyId(current);
	const currentTerms = poolTermsOf(measuredPool.replays, measured, cfg, scale, "raw");
	const currentScore = currentTerms.score;
	const currentMinBest = measuredPool.replays.reduce(
		(min, replay) => (replay.bestScore < min ? replay.bestScore : min),
		measurable ? Number.POSITIVE_INFINITY : 0,
	);
	const qualityEps = cfg.qualityEps ?? 0;
	let simulations = measuredPool.sorted.length;

	const entries: ScoredEntry[] = [];
	const passed = new Map<number, boolean>();
	const seen = new Map<string, number>();
	inputs.forEach((input, index) => {
		const id = policyId(input.policy);
		const changed = policyFieldsDiffering(input.policy, current);
		const identical = id === currentId;
		const duplicateOf = identical ? null : (seen.get(id) ?? null);
		if (!identical && duplicateOf === null) seen.set(id, index);
		const replayDead = !identical && differsOnlyInReplayDeadFields(input.policy, current);
		const isRevoked = !identical && revoked.has(id);
		let score: PoolScore;
		let passes: boolean;
		if (identical || replayDead) {
			score = currentScore;
			passes = measurable;
		} else if (duplicateOf !== null) {
			score = entries[duplicateOf]!.score;
			passes = passed.get(duplicateOf) ?? false;
		} else {
			const scored = scoreOnPool(input.policy, measured, cfg, scale, "evidence");
			simulations += measured.length;
			score = scored.score;
			passes = measurable && passesQualityGuard(scored.terms, currentTerms.terms, qualityEps);
		}
		passed.set(index, passes);
		const simulated = !identical && duplicateOf === null && !replayDead;
		entries.push({
			input,
			index,
			id,
			changed,
			duplicateOf,
			identical,
			replayDead,
			revoked: isRevoked,
			score,
			eligible: simulated && passes && !isRevoked,
			qualityRejected: simulated && measurable && !passes,
		});
	});

	const eligible = entries.filter((entry) => entry.eligible);
	let maxScore = currentScore.value;
	for (const entry of eligible) if (entry.score.value > maxScore) maxScore = entry.score.value;
	const winners = eligible.filter((entry) => entry.score.value >= maxScore - SELECT_EPS);
	const currentWins = currentScore.value >= maxScore - SELECT_EPS;
	const winner = currentWins ? undefined : [...winners].sort((a, b) => a.id.localeCompare(b.id))[0];
	const improved = winner !== undefined && winner.score.value > currentScore.value + SELECT_EPS;
	const chosen = improved ? winner : undefined;
	const chosenScore = chosen ? chosen.score : currentScore;

	const verdicts: CandidateVerdict[] = entries.map((entry) => {
		const offSupport = entry.score.inSupportMin < 1;
		const isChosen = chosen !== undefined && entry.index === chosen.index;
		let reason: CandidateReason;
		if (entry.identical) reason = "identical";
		else if (entry.duplicateOf !== null) reason = "duplicate";
		else if (entry.revoked) reason = "revoked";
		else if (isChosen) reason = "winner";
		else if (entry.replayDead || offSupport || !measurable) reason = "unmeasurable";
		else if (entry.qualityRejected) reason = "quality-rejected";
		else if (entry.score.value >= chosenScore.value - SELECT_EPS) reason = "tie";
		else reason = "worse";
		return {
			index: entry.index,
			policyId: entry.id,
			policy: entry.input.policy,
			origin: entry.input.origin,
			changed: entry.changed,
			duplicateOf: entry.duplicateOf,
			value: entry.score.value,
			quality: entry.score.quality,
			anytime: entry.score.anytime,
			cost: entry.score.cost,
			roundsSaved: entry.score.roundsSaved,
			N: entry.score.N,
			rounds: entry.score.rounds,
			outOfSupportCells: entry.score.outOfSupportCells,
			inSupportMean: entry.score.inSupportMean,
			inSupportMin: entry.score.inSupportMin,
			chargedProbes: entry.score.chargedProbes,
			chargedRounds: entry.score.chargedRounds,
			evidenceTrees: evidenceTreesOf(measured.length),
			eligible: entry.eligible,
			reason,
		};
	});

	return {
		chosenPolicy: chosen ? chosen.input.policy : current,
		chosenScore: chosenScore.value,
		currentScore: currentScore.value,
		chosenQuality: chosenScore.quality,
		currentQuality: currentScore.quality,
		current: currentScore,
		currentMinBest,
		improved,
		scoredCount: 1 + entries.filter((entry) => !entry.identical && entry.duplicateOf === null).length,
		qualityRejected: verdicts.filter((verdict) => verdict.reason === "quality-rejected").length,
		candidatePolicyIds: entries.map((entry) => entry.id),
		candidates: verdicts,
		dreamer: dreamerKindOf(inputs),
		poolSize: measuredPool.sorted.length,
		measuredTrees: measured.length,
		evidenceTrees: evidenceTreesOf(measured.length),
		currentInSupport: measuredPool.currentInSupport,
		simulations,
	};
}

/** The `beta` values the lever scan grid takes. */
export const LEVER_SCAN_BETAS: readonly number[] = [1, 2, 3, 4, 6, 8, 12];

/**
 * The fixed lever-scan grid around `current`: every selection rule x every stop
 * rule x batchSize 1..W x `LEVER_SCAN_BETAS`, at the current policy's other
 * numeric values, plus the current policy; deduplicated by policy id and in a
 * deterministic order. Touches no rng.
 */
export function leverScanGrid(current: ExplorationPolicy, workers: number): ExplorationPolicy[] {
	const maxBatch = Math.min(POLICY_BOUNDS.batchSize.max, Math.max(1, Math.trunc(workers)));
	const out: ExplorationPolicy[] = [];
	const ids = new Set<string>();
	const push = (policy: ExplorationPolicy): void => {
		const id = policyId(policy);
		if (ids.has(id)) return;
		ids.add(id);
		out.push(policy);
	};
	push(current);
	for (const selectionRule of SELECTION_RULES) {
		for (const stopRule of STOP_RULES) {
			for (let batchSize = 1; batchSize <= maxBatch; batchSize++) {
				for (const beta of LEVER_SCAN_BETAS) {
					push(clampPolicy({ ...current, selectionRule, stopRule, batchSize, beta }));
				}
			}
		}
	}
	return out;
}

/** Max `header.w` over the pool; 1 for an empty pool. */
function poolWorkers(pool: readonly RecordedTree[]): number {
	let workers = 1;
	for (const tree of pool) if (tree.header.w > workers) workers = Math.trunc(tree.header.w);
	return workers;
}

/**
 * Score the lever-scan grid on the pool under the selection rule and summarize
 * it. `gap` is `bestValue - currentValue` over the eligible grid policies, 0 when
 * none beats current; `bestPolicyId` is the current id in that case.
 */
export function runLeverScan(
	current: ExplorationPolicy,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): LeverScanRecord {
	const grid = leverScanGrid(current, poolWorkers(pool));
	const selection = selectBestPolicy(current, grid, pool, cfg);
	const eligible = selection.candidates.filter((candidate) => candidate.eligible);
	const gap = selection.improved ? selection.chosenScore - selection.currentScore : 0;
	return {
		policies: grid.length,
		eligible: eligible.length,
		bestValue: selection.chosenScore,
		bestPolicyId: policyId(selection.chosenPolicy),
		gap,
		simulations: selection.simulations,
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
	/** Loop iteration this step belongs to, stamped on the `dream.dream`/`dream.replay`/`dream.candidate` spans. */
	iteration?: number;
	/**
	 * Optional injected candidate proposer (the flag-gated LLM dreamer). It must
	 * return already-parsed, in-bounds policies (bare, origin `local`, or with an
	 * explicit origin); they are scored and selected by the same rule as local
	 * candidates.
	 */
	proposeCandidates?: (current: ExplorationPolicy, m: number, rng: SeededRng) => CandidateList;
	/** When false, skip the lever scan (it is deterministic and rng-free; on by default). */
	leverScan?: boolean;
	/** Policy ids this run adopted and reverted after probation (`loop.ts`): scored, reported `revoked`, never eligible. */
	revoked?: ReadonlySet<string>;
}

export interface DreamResult {
	chosenPolicy: ExplorationPolicy;
	chosenPolicyId: string;
	chosenScore: number;
	currentScore: number;
	chosenQuality: number;
	currentQuality: number;
	/** The current policy's full score on the measured pool (spend charged raw). */
	current: PoolScore;
	/** The current policy's lowest replay best over the measured trees: the probation floor (`loop.ts`). */
	currentMinBest: number;
	improved: boolean;
	scoredCount: number;
	qualityRejected: number;
	candidatePolicyIds: string[];
	/** One verdict per proposed candidate, in proposal order. */
	candidates: CandidateVerdict[];
	dreamer: DreamerKind;
	/** The fixed-grid lever scan of this step; null when disabled. */
	leverScan: LeverScanRecord | null;
	poolSize: number;
	/** Trees the current policy replays in full support: the pool every score is a mean over. */
	measuredTrees: number;
	/** `measuredTrees - 1` (floor 0): the trees that can vouch for a stop-early credit. */
	evidenceTrees: number;
	/** `simulatePolicy` calls the selection made (the lever scan's are on `leverScan.simulations`). */
	simulations: number;
	tokens: number;
}

/** Scalar attributes of one post-hoc `dream.candidate` span. */
export function candidateSpanAttrs(
	verdict: CandidateVerdict,
	iteration: number,
): Record<string, string | number | boolean> {
	return {
		"dream.iteration": iteration,
		"dream.candidate_index": verdict.index,
		"dream.policy_id": verdict.policyId,
		"dream.origin": verdict.origin,
		"dream.reason": verdict.reason,
		"dream.eligible": verdict.eligible,
		"dream.value": verdict.value,
		"dream.quality": verdict.quality,
		"dream.anytime": verdict.anytime,
		"dream.cost": verdict.cost,
		"dream.rounds_saved": verdict.roundsSaved,
		"dream.in_support_min": verdict.inSupportMin,
		"dream.charged_probes": verdict.chargedProbes,
		"dream.charged_rounds": verdict.chargedRounds,
		"dream.changed": verdict.changed.join(","),
	};
}

/**
 * One dreaming step: propose M candidates, score {current} ∪ candidates on the
 * pool, select the no-worse policy, then run the lever scan. Opens `dream.dream`
 * wrapping one coarse `dream.replay` summary span (one per dreaming step, not
 * one per simulation; its `dream.simulations` is the exact count the selection
 * made, the lever scan's count sits on `dream.dream` as `dream.lever_simulations`)
 * and one post-hoc, zero-duration `dream.candidate` span per candidate carrying
 * its verdict.
 */
export function runDreaming(options: DreamingOptions): DreamResult {
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const iteration = Math.max(0, Math.trunc(options.iteration ?? 0));
	const cfg: DreamingScoreConfig = {
		k1: options.k1,
		k2: options.k2,
		objective,
		...(options.qualityEps !== undefined ? { qualityEps: options.qualityEps } : {}),
	};
	const propose = options.proposeCandidates ?? proposePolicies;
	const candidates = normalizeCandidates(propose(options.current, options.dreams, options.rng));
	return withSpan(
		"dream.dream",
		{ "dream.candidates": candidates.length, "dream.pool_size": options.pool.length, "dream.iteration": iteration },
		(span) => {
			const selection = withSpan(
				"dream.replay",
				{ "dream.policy_id": policyId(options.current), "dream.iteration": iteration },
				(replaySpan) => {
					const selected = selectBestPolicy(options.current, candidates, options.pool, cfg, options.revoked);
					replaySpan.setAttributes({
						"dream.simulations": selected.simulations,
						"dream.measured_trees": selected.measuredTrees,
					});
					return selected;
				},
			);
			const leverScan = options.leverScan === false ? null : runLeverScan(options.current, options.pool, cfg);
			for (const verdict of selection.candidates) {
				withSpan("dream.candidate", candidateSpanAttrs(verdict, iteration), () => undefined);
			}
			span.setAttributes({
				"dream.chosen_policy_id": policyId(selection.chosenPolicy),
				"dream.chosen_score": selection.chosenScore,
				"dream.current_score": selection.currentScore,
				"dream.chosen_quality": selection.chosenQuality,
				"dream.current_quality": selection.currentQuality,
				"dream.quality_rejected": selection.qualityRejected,
				"dream.unmeasurable": selection.candidates.filter((c) => c.reason === "unmeasurable").length,
				"dream.improved": selection.improved,
				"dream.dreamer": selection.dreamer,
				"dream.in_support_current": selection.currentInSupport,
				"dream.measured_trees": selection.measuredTrees,
				"dream.evidence_trees": selection.evidenceTrees,
				"dream.simulations": selection.simulations,
				...(leverScan
					? {
							"dream.lever_gap": leverScan.gap,
							"dream.lever_policies": leverScan.policies,
							"dream.lever_simulations": leverScan.simulations,
						}
					: {}),
			});
			return {
				chosenPolicy: selection.chosenPolicy,
				chosenPolicyId: policyId(selection.chosenPolicy),
				chosenScore: selection.chosenScore,
				currentScore: selection.currentScore,
				chosenQuality: selection.chosenQuality,
				currentQuality: selection.currentQuality,
				current: selection.current,
				currentMinBest: selection.currentMinBest,
				improved: selection.improved,
				scoredCount: selection.scoredCount,
				qualityRejected: selection.qualityRejected,
				candidatePolicyIds: selection.candidatePolicyIds,
				candidates: selection.candidates,
				dreamer: selection.dreamer,
				leverScan,
				poolSize: options.pool.length,
				measuredTrees: selection.measuredTrees,
				evidenceTrees: selection.evidenceTrees,
				simulations: selection.simulations,
				tokens: 0,
			};
		},
	);
}
