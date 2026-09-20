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
 * candidates whose mean replay QUALITY is no lower than the current policy's.
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
	type ObjectiveScale,
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
	 * How far (in pool-range units, so scale-free) a candidate's mean replay
	 * quality may fall below the current policy's and still be eligible. Default 0:
	 * quality must not regress.
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
	};
}

function simulateOnPool(
	policy: ExplorationPolicy,
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): ReplayResult[] {
	return sorted.map((tree) => simulatePolicy(tree, policy, { k2: cfg.k2 }));
}

/** Aggregate one replay per tree (`replays[i]` is `sorted[i]`'s) into the pool means. */
function aggregateReplays(
	replays: readonly ReplayResult[],
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	scale: ObjectiveScale,
): PoolScore {
	if (sorted.length === 0) return emptyPoolScore();
	const sum = emptyPoolScore();
	sum.inSupportMean = 0;
	sorted.forEach((tree, index) => {
		const replay = replays[index]!;
		const terms = computeObjectiveTerms(replay, cfg.objective, scale, { workers: tree.header.w, k1: cfg.k1 });
		sum.value += terms.value;
		sum.quality += terms.quality;
		sum.anytime += terms.anytime;
		sum.cost += terms.cost;
		sum.roundsSaved += terms.roundsSaved;
		sum.N += replay.N;
		sum.rounds += replay.rounds;
		sum.outOfSupportCells += replay.outOfSupportCells;
		sum.inSupportMean += replay.inSupport;
		if (replay.inSupport < sum.inSupportMin) sum.inSupportMin = replay.inSupport;
	});
	const count = sorted.length;
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
	};
}

function scoreOnPool(
	policy: ExplorationPolicy,
	sorted: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
	scale: ObjectiveScale,
): PoolScore {
	return aggregateReplays(simulateOnPool(policy, sorted, cfg), sorted, cfg, scale);
}

function sortedPool(pool: readonly RecordedTree[]): RecordedTree[] {
	return [...pool].sort((a, b) => a.header.treeId.localeCompare(b.header.treeId));
}

/**
 * Mean replay objective and mean normalized quality of a policy over the pool
 * AS GIVEN (no measured-pool exclusion: that is `selectBestPolicy`'s job), scored
 * against the pool's own score scale and evaluated on a deterministic treeId
 * ordering. Replay is rng-free, so `_rng` is accepted only to match the
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
	/** The current policy's full score on the measured pool. */
	current: PoolScore;
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
	score: PoolScore;
	/** Simulated (not identical, duplicate or replay-dead) and passed the quality guard. */
	eligible: boolean;
	/** Simulated (not identical, duplicate or replay-dead) and failed the quality guard. */
	qualityRejected: boolean;
}

/**
 * Score {current} ∪ candidates on the measured pool (`measurePool`: the trees
 * the current policy replays in full support) and return the argmax over the
 * ELIGIBLE entries: the current policy, plus every distinct candidate whose mean
 * quality is at least `current - qualityEps` (within `SELECT_EPS`). Ties in V
 * resolve to the current policy, then to the lowest policyId. The chosen policy
 * is therefore never worse than current in V and never lower in quality, and
 * `improved` is true only when an eligible candidate strictly beats current in V.
 * The result is identical to calling this on the measured trees alone; when no
 * tree is measured nothing is eligible and every simulated candidate is
 * `unmeasurable`.
 *
 * A candidate whose id equals the current's (`identical`) or an earlier
 * candidate's (`duplicate`) is not simulated again and never enters the argmax.
 * A candidate that differs from current only in replay-dead fields is scored
 * as current without a simulation (its replay is the same walk), kept out of
 * the argmax and reported `unmeasurable`.
 */
export function selectBestPolicy(
	current: ExplorationPolicy,
	candidates: CandidateList,
	pool: readonly RecordedTree[],
	cfg: DreamingScoreConfig,
): PolicySelection {
	const inputs = normalizeCandidates(candidates);
	const measuredPool = measurePool(current, pool, cfg);
	const measured = measuredPool.measured;
	const measurable = measured.length > 0;
	const scale = poolScoreScale(measured);
	const currentId = policyId(current);
	const currentScore = aggregateReplays(measuredPool.replays, measured, cfg, scale);
	const qualityFloor = currentScore.quality - Math.max(0, cfg.qualityEps ?? 0) - SELECT_EPS;
	let simulations = measuredPool.sorted.length;

	const entries: ScoredEntry[] = [];
	const seen = new Map<string, number>();
	inputs.forEach((input, index) => {
		const id = policyId(input.policy);
		const changed = policyFieldsDiffering(input.policy, current);
		const identical = id === currentId;
		const duplicateOf = identical ? null : (seen.get(id) ?? null);
		if (!identical && duplicateOf === null) seen.set(id, index);
		const replayDead = !identical && differsOnlyInReplayDeadFields(input.policy, current);
		let score: PoolScore;
		if (identical || replayDead) score = currentScore;
		else if (duplicateOf !== null) score = entries[duplicateOf]!.score;
		else {
			score = scoreOnPool(input.policy, measured, cfg, scale);
			simulations += measured.length;
		}
		const simulated = !identical && duplicateOf === null && !replayDead;
		const passes = measurable && score.quality >= qualityFloor;
		entries.push({
			input,
			index,
			id,
			changed,
			duplicateOf,
			identical,
			replayDead,
			score,
			eligible: simulated && passes,
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
		improved,
		scoredCount: 1 + entries.filter((entry) => !entry.identical && entry.duplicateOf === null).length,
		qualityRejected: verdicts.filter((verdict) => verdict.reason === "quality-rejected").length,
		candidatePolicyIds: entries.map((entry) => entry.id),
		candidates: verdicts,
		dreamer: dreamerKindOf(inputs),
		poolSize: measuredPool.sorted.length,
		measuredTrees: measured.length,
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
	/** One verdict per proposed candidate, in proposal order. */
	candidates: CandidateVerdict[];
	dreamer: DreamerKind;
	/** The fixed-grid lever scan of this step; null when disabled. */
	leverScan: LeverScanRecord | null;
	poolSize: number;
	/** Trees the current policy replays in full support: the pool every score is a mean over. */
	measuredTrees: number;
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
					const selected = selectBestPolicy(options.current, candidates, options.pool, cfg);
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
				improved: selection.improved,
				scoredCount: selection.scoredCount,
				qualityRejected: selection.qualityRejected,
				candidatePolicyIds: selection.candidatePolicyIds,
				candidates: selection.candidates,
				dreamer: selection.dreamer,
				leverScan,
				poolSize: options.pool.length,
				measuredTrees: selection.measuredTrees,
				simulations: selection.simulations,
				tokens: 0,
			};
		},
	);
}
