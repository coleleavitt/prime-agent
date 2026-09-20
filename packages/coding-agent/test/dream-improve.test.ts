import { readFileSync } from "node:fs";
import { join } from "node:path";
import { addSpanSink, type SpanEndRecord } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import {
	type DreamingScoreConfig,
	dreamerKindOf,
	LEVER_SCAN_BETAS,
	leverScanGrid,
	measurePool,
	mutatePolicy,
	proposePolicies,
	runDreaming,
	runLeverScan,
	scorePolicyOnPool,
	selectBestPolicy,
} from "../src/core/dream/improve.js";
import {
	computeObjective,
	computeObjectiveTerms,
	DEFAULT_OBJECTIVE,
	normalizedQuality,
	poolScoreScale,
	type ReplayObjectiveConfig,
} from "../src/core/dream/objective.js";
import {
	DEFAULT_POLICY,
	type ExplorationPolicy,
	parseExplorationPolicy,
	policyFieldsDiffering,
	policyId,
	REPLAY_DEAD_FIELDS,
	SELECTION_RULES,
	STOP_RULES,
} from "../src/core/dream/policy.js";
import type { ReplayResult } from "../src/core/dream/replay.js";
import { simulatePolicy } from "../src/core/dream/replay.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import { buildRecordedTree, type RecordedTree } from "../src/core/dream/store.js";
import type { NodeRecord, TreeRecord } from "../src/core/dream/types.js";

function node(over: Partial<NodeRecord> & Pick<NodeRecord, "id" | "parentId" | "seq" | "score">): NodeRecord {
	return { type: "node", branch: 0, round: 0, valid: true, artifactRef: "ref", tokens: 0, ts: 0, ...over };
}

// A shallow tree: root(0.3) with children revealed in seq order 0.5, 0.4, then 0.9.
// Revealing more of the root's children raises the best score, so a policy that
// stops early reaches a lower quality than one that keeps going.
const SYNTH: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "synth",
		taskId: "synthetic",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "synth-n0", parentId: null, seq: 0, score: 0.3 }),
	node({ id: "synth-n1", parentId: "synth-n0", seq: 1, branch: 0, score: 0.5 }),
	node({ id: "synth-n2", parentId: "synth-n0", seq: 2, branch: 1, score: 0.4 }),
	node({ id: "synth-n3", parentId: "synth-n0", seq: 3, branch: 2, score: 0.9 }),
];

function policy(over: Partial<ExplorationPolicy>): ExplorationPolicy {
	return { ...DEFAULT_POLICY, ...over };
}

const OBJECTIVE = DEFAULT_OBJECTIVE;
const CFG: DreamingScoreConfig = { k1: 5, k2: 10, objective: OBJECTIVE };
/** The synthetic pool's scale and the budget `w * k1` its tree is scored against. */
const SYNTH_SCALE = { scoreMin: 0.3, scoreMax: 0.9 };
const SYNTH_BUDGET = { workers: 2, k1: 5 };

const CURRENT = policy({ selectionRule: "explore-root", stopRule: "patience", beta: 1, batchSize: 1 });
const BETTER = policy({ selectionRule: "explore-root", stopRule: "never", batchSize: 1 });
const WORSE = policy({ selectionRule: "best-first", stopRule: "never", batchSize: 1 });

function pool(): RecordedTree[] {
	return [buildRecordedTree(SYNTH)];
}

/** `records` under a second tree id, so a pool can hold two identical trees. */
function twinOf(records: readonly TreeRecord[], treeId: string): RecordedTree {
	const from = (records[0] as { treeId: string }).treeId;
	return buildRecordedTree(
		records.map((record) => JSON.parse(JSON.stringify(record).split(from).join(treeId)) as TreeRecord),
	);
}

/**
 * SYNTH twice. With two identical trees each policy's cross-tree spend horizon
 * equals its own probes/rounds to best, which never exceeds its own spend, so
 * every charged number equals the raw one; on SYNTH alone every policy would be
 * charged the whole budget (10 probes, 5 rounds).
 */
function twinPool(): RecordedTree[] {
	return [buildRecordedTree(SYNTH), twinOf(SYNTH, "synth2")];
}

/**
 * Two trees the fixed-exploration control grew on circle-packing (seed 7, W 4,
 * k1 12, n 26): rounds 1 and 2 of the recorded experiment
 * `circle-packing-s7-n6-1789842143996`. Node and reveal lines only; replay needs
 * no blobs.
 */
const FIXTURE_DIR = join(__dirname, "fixtures", "dream");
const RECORDED_K1 = 12;
const RECORDED_K2 = 24;
const RECORDED_CFG: DreamingScoreConfig = { k1: RECORDED_K1, k2: RECORDED_K2, objective: DEFAULT_OBJECTIVE };

function loadFixture(name: string): RecordedTree {
	const records = readFileSync(join(FIXTURE_DIR, name), "utf8")
		.split("\n")
		.filter((line) => line.length > 0)
		.map((line) => JSON.parse(line) as TreeRecord);
	return buildRecordedTree(records);
}

function recordedPool(): RecordedTree[] {
	return [
		loadFixture("circle-packing-s7-i0-1789842143996.jsonl"),
		loadFixture("circle-packing-s7-i1-1789842143996.jsonl"),
	];
}

/** The initial policy the recorded experiment started from (`1be99d403b0405a3`). */
const EXPLORING = DEFAULT_POLICY;
/** The policy the recorded dream arm collapsed to (`f559ec93fc3b1773`): one round of at most W probes. */
const COLLAPSED = policy({ selectionRule: "weighted", stopRule: "fixed-rounds", beta: 1 });

/** The raw-scale objective this codebase shipped with: V = best - 0.01 N + 0.02 N / max(1, rounds). Kept here as the documented bug. */
function oldRawObjective(replay: ReplayResult): number {
	return replay.bestScore - 0.01 * replay.N + (0.02 * replay.N) / Math.max(1, replay.rounds);
}

function meanOver<T>(items: readonly T[], f: (item: T) => number): number {
	return items.reduce((sum, item) => sum + f(item), 0) / items.length;
}

/**
 * A `ReplayResult` literal for objective arithmetic. `bestSoFar` defaults to the
 * best score held from the first charged selection on; `selectedCells` and
 * `inSupport` follow from `N` and `outOfSupportCells`.
 */
function replay(over: Partial<ReplayResult> & Pick<ReplayResult, "N" | "rounds" | "bestScore">): ReplayResult {
	const outOfSupportCells = over.outOfSupportCells ?? 0;
	const selectedCells = over.N + outOfSupportCells;
	return {
		policyId: "x",
		treeId: "t",
		revealedIds: [],
		outOfSupportCells,
		selectedCells,
		inSupport: selectedCells === 0 ? 1 : over.N / selectedCells,
		bestSoFar: new Array<number>(selectedCells).fill(over.bestScore),
		probesToBest: selectedCells === 0 ? 0 : 1,
		roundsToBest: selectedCells === 0 ? 0 : 1,
		...over,
	};
}

describe("computeObjective (quality, anytime, charged cost, rounds saved)", () => {
	const base = replay({ N: 3, rounds: 3, bestScore: 0.9 });

	it("matches V = (1 - beta3) q + beta3 anytime - beta1 S/B + beta2 (1 - rounds/k1)", () => {
		const q = (0.9 - 0.3) / (0.9 - 0.3);
		// Best from the first probe: anytime == q. B = 2 * 5 = 10, S = 3, rounds 3 of k1 5.
		const expected = 0.75 * q + 0.25 * q - 0.05 * (3 / 10) + 0.1 * (1 - 3 / 5);
		expect(computeObjective(base, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET)).toBeCloseTo(expected, 12);
		const terms = computeObjectiveTerms(base, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(terms.quality).toBe(1);
		expect(terms.anytime).toBe(1);
		expect(terms.cost).toBeCloseTo(0.3, 12);
		expect(terms.roundsSaved).toBeCloseTo(0.4, 12);
		expect(terms.value).toBeCloseTo(expected, 12);
		expect(terms.value).toBeCloseTo(1.025, 12);
	});

	it("averages the normalized best-so-far over the budget, holding the final best over the unspent tail", () => {
		// Five probes: best-so-far 0.3, 0.6, 0.6, 0.9, 0.9 -> normalized 0, 0.5, 0.5, 1, 1; five unspent probes at 1.
		const rising = replay({ N: 5, rounds: 3, bestScore: 0.9, bestSoFar: [0.3, 0.6, 0.6, 0.9, 0.9] });
		const terms = computeObjectiveTerms(rising, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(terms.quality).toBe(1);
		expect(terms.anytime).toBeCloseTo((0 + 0.5 + 0.5 + 1 + 1 + 5) / 10, 12);
		expect(terms.anytime).toBeLessThanOrEqual(terms.quality);
		// The betas are honoured term by term.
		const betas: ReplayObjectiveConfig = { beta1: 0.2, beta2: 0.1, beta3: 0.5 };
		expect(computeObjective(rising, betas, SYNTH_SCALE, SYNTH_BUDGET)).toBeCloseTo(
			0.5 * 1 + 0.5 * 0.8 - 0.2 * (5 / 10) + 0.1 * (1 - 3 / 5),
			12,
		);
	});

	it("scores a full budget at k1 rounds as q - beta1 and bounds the swing by beta1 S/B + beta2 |1 - rounds/k1|", () => {
		const full = replay({ N: 10, rounds: 5, bestScore: 0.9 });
		const terms = computeObjectiveTerms(full, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(terms.cost).toBe(1);
		expect(terms.roundsSaved).toBe(0);
		expect(terms.value).toBeCloseTo(1 - OBJECTIVE.beta1, 12);
		// Out-of-support cells are charged and the cost is NOT clamped; rounds past k1 go negative.
		const over = replay({ N: 6, rounds: 10, bestScore: 0.9, outOfSupportCells: 6 });
		const overTerms = computeObjectiveTerms(over, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(overTerms.cost).toBeCloseTo(1.2, 12);
		expect(overTerms.roundsSaved).toBe(-1);
		expect(Math.abs(overTerms.value - overTerms.quality)).toBeLessThanOrEqual(
			OBJECTIVE.beta1 * overTerms.cost + OBJECTIVE.beta2 * Math.abs(overTerms.roundsSaved) + 1e-12,
		);
		expect(overTerms.value).toBeLessThan(terms.value);
		// value <= 1 + beta2 everywhere: the best case is q 1, anytime 1, S 0, rounds 0.
		const free = replay({ N: 0, rounds: 0, bestScore: 0.9 });
		expect(computeObjective(free, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET)).toBeCloseTo(1 + OBJECTIVE.beta2, 12);
	});

	it("(e) reduces to the strictly-cost form q - beta1 S/B when beta3 = beta2 = 0", () => {
		const cfg: ReplayObjectiveConfig = { beta1: 0.05, beta2: 0, beta3: 0 };
		for (const result of [
			replay({
				N: 4,
				rounds: 2,
				bestScore: 0.6,
				outOfSupportCells: 3,
				bestSoFar: [0.3, 0.3, 0.6, 0.6, 0.6, 0.6, 0.6],
			}),
			replay({ N: 10, rounds: 5, bestScore: 0.9 }),
			replay({ N: 1, rounds: 1, bestScore: 0.4 }),
		]) {
			const terms = computeObjectiveTerms(result, cfg, SYNTH_SCALE, SYNTH_BUDGET);
			expect(terms.value).toBeCloseTo(terms.quality - 0.05 * ((result.N + result.outOfSupportCells) / 10), 12);
		}
	});

	it("(d) charges an out-of-support selection: it never helps anytime and always costs, so the value falls", () => {
		const supported = replay({ N: 4, rounds: 3, bestScore: 0.9, bestSoFar: [0.5, 0.5, 0.9, 0.9] });
		// The same walk with one exhausted selection between the second and third reveal: the running best repeats.
		const offSupport = replay({
			N: 4,
			rounds: 3,
			bestScore: 0.9,
			outOfSupportCells: 1,
			bestSoFar: [0.5, 0.5, 0.5, 0.9, 0.9],
		});
		const on = computeObjectiveTerms(supported, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		const off = computeObjectiveTerms(offSupport, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(off.quality).toBe(on.quality);
		expect(off.anytime).toBeLessThanOrEqual(on.anytime);
		expect(off.cost).toBeGreaterThan(on.cost);
		expect(off.roundsSaved).toBe(on.roundsSaved);
		expect(off.value).toBeLessThan(on.value);
		// Even when the exhausted selection comes after the best was found (anytime unchanged), cost still bites.
		const late = replay({
			N: 4,
			rounds: 3,
			bestScore: 0.9,
			outOfSupportCells: 1,
			bestSoFar: [0.5, 0.5, 0.9, 0.9, 0.9],
		});
		const lateTerms = computeObjectiveTerms(late, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(lateTerms.anytime).toBeLessThanOrEqual(on.anytime);
		expect(lateTerms.value).toBeLessThan(on.value);
	});

	it("charges the spend at the evidence horizon when one is given, and the raw spend when none is", () => {
		const cheap = replay({ N: 2, rounds: 1, bestScore: 0.9 });
		const raw = computeObjectiveTerms(cheap, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(raw.chargedProbes).toBe(2);
		expect(raw.chargedRounds).toBe(1);
		expect(raw.cost).toBeCloseTo(0.2, 12);
		expect(raw.roundsSaved).toBeCloseTo(0.8, 12);
		// Another tree of the same policy was still improving at probe 7, round 4: charge that.
		const backed = computeObjectiveTerms(cheap, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET, { probes: 7, rounds: 4 });
		expect(backed.chargedProbes).toBe(7);
		expect(backed.chargedRounds).toBe(4);
		expect(backed.cost).toBeCloseTo(0.7, 12);
		expect(backed.roundsSaved).toBeCloseTo(0.2, 12);
		// Quality and anytime never move: the horizon is a spend, not a reveal.
		expect(backed.quality).toBe(raw.quality);
		expect(backed.anytime).toBe(raw.anytime);
		expect(raw.value - backed.value).toBeCloseTo(OBJECTIVE.beta1 * 0.5 + OBJECTIVE.beta2 * 0.6, 12);
		// A horizon below the replay's own spend changes nothing (max, not replace).
		const under = computeObjectiveTerms(cheap, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET, { probes: 1, rounds: 0 });
		expect(under).toEqual(raw);
		// The whole budget as horizon (a single measured tree): cost 1, roundsSaved 0, V = q - beta1.
		const alone = computeObjectiveTerms(cheap, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET, { probes: 10, rounds: 5 });
		expect(alone.cost).toBe(1);
		expect(alone.roundsSaved).toBe(0);
		expect(alone.value).toBeCloseTo(1 - OBJECTIVE.beta1, 12);
	});
});

describe("normalizedQuality / poolScoreScale", () => {
	it("maps the pool's min and max to 0 and 1 and clamps outside", () => {
		expect(normalizedQuality(0.3, SYNTH_SCALE)).toBe(0);
		expect(normalizedQuality(0.9, SYNTH_SCALE)).toBe(1);
		expect(normalizedQuality(0.6, SYNTH_SCALE)).toBeCloseTo(0.5, 12);
		expect(normalizedQuality(0.1, SYNTH_SCALE)).toBe(0);
		expect(normalizedQuality(5, SYNTH_SCALE)).toBe(1);
	});

	it("guards a degenerate scale (min == max) and the nothing-valid sentinel", () => {
		const flat = { scoreMin: 0.7, scoreMax: 0.7 };
		expect(normalizedQuality(0.7, flat)).toBe(1);
		expect(normalizedQuality(0.71, flat)).toBe(1);
		expect(normalizedQuality(0.69, flat)).toBe(0);
		// A replay that revealed nothing valid reports bestScore 0.
		expect(normalizedQuality(0, SYNTH_SCALE)).toBe(0);
		expect(normalizedQuality(0, flat)).toBe(0);
		expect(normalizedQuality(0, { scoreMin: 0, scoreMax: 0 })).toBe(1);
	});

	it("takes the pool's range over valid finite node scores and is {0,0} when there are none", () => {
		expect(poolScoreScale(pool())).toEqual(SYNTH_SCALE);
		expect(poolScoreScale([])).toEqual({ scoreMin: 0, scoreMax: 0 });
		const withInvalid = buildRecordedTree([
			SYNTH[0]!,
			node({ id: "synth-n0", parentId: null, seq: 0, score: 0.3 }),
			node({ id: "synth-n1", parentId: "synth-n0", seq: 1, score: 0, valid: false }),
			node({ id: "synth-n2", parentId: "synth-n0", seq: 2, score: 7 }),
		]);
		expect(poolScoreScale([withInvalid])).toEqual({ scoreMin: 0.3, scoreMax: 7 });
		expect(poolScoreScale([withInvalid, buildRecordedTree(SYNTH)])).toEqual({ scoreMin: 0.3, scoreMax: 7 });
		const noneValid = buildRecordedTree([
			SYNTH[0]!,
			node({ id: "synth-n0", parentId: null, seq: 0, score: 0, valid: false }),
		]);
		expect(poolScoreScale([noneValid])).toEqual({ scoreMin: 0, scoreMax: 0 });
	});
});

describe("scorePolicyOnPool", () => {
	it("is the mean replay objective and mean quality over the pool, on the pool's scale and budget", () => {
		const trees = pool();
		// A lone tree has no other tree to vouch for stopping early, so the pool scorer charges the
		// whole budget (10 probes, 5 rounds) where a bare replay would charge its own 2 probes, 2 rounds.
		const replayed = simulatePolicy(trees[0]!, CURRENT, { k2: CFG.k2 });
		const budget = { workers: trees[0]!.header.w, k1: CFG.k1 };
		const terms = computeObjectiveTerms(replayed, OBJECTIVE, poolScoreScale(trees), budget, {
			probes: 10,
			rounds: 5,
		});
		const score = scorePolicyOnPool(CURRENT, trees, CFG);
		expect(score.value).toBeCloseTo(terms.value, 12);
		expect(score.quality).toBeCloseTo(terms.quality, 12);
		expect(score.chargedProbes).toBe(10);
		expect(score.chargedRounds).toBe(5);
		expect(replayed.N).toBe(2);
		expect(score.N).toBe(2);
		const raw = computeObjectiveTerms(replayed, OBJECTIVE, poolScoreScale(trees), budget);
		expect(raw.value - score.value).toBeCloseTo(OBJECTIVE.beta1 * (8 / 10) + OBJECTIVE.beta2 * (3 / 5), 12);
	});

	it("scores a policy that spends fewer probes differently from one that spends more", () => {
		const trees = pool();
		expect(scorePolicyOnPool(CURRENT, trees, CFG).value).not.toBeCloseTo(
			scorePolicyOnPool(BETTER, trees, CFG).value,
			6,
		);
	});

	it("returns 0 for an empty pool, with full support", () => {
		expect(scorePolicyOnPool(CURRENT, [], CFG)).toEqual({
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
		});
	});
});

describe("selectBestPolicy", () => {
	it("picks a strictly-better candidate", () => {
		const selection = selectBestPolicy(CURRENT, [BETTER], pool(), CFG);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(BETTER);
		expect(selection.chosenScore).toBeGreaterThan(selection.currentScore);
		expect(selection.chosenQuality).toBeGreaterThanOrEqual(selection.currentQuality);
		expect(selection.scoredCount).toBe(2);
		expect(selection.qualityRejected).toBe(0);
	});

	it("keeps the current policy when every candidate is worse", () => {
		const selection = selectBestPolicy(CURRENT, [WORSE], pool(), CFG);
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicy).toBe(CURRENT);
		expect(selection.chosenScore).toBeGreaterThanOrEqual(selection.currentScore);
	});

	it("resolves a tie in favour of the current policy", () => {
		const clone = policy({ selectionRule: "explore-root", stopRule: "patience", beta: 1, batchSize: 1 });
		expect(policyId(clone)).toBe(policyId(CURRENT));
		const selection = selectBestPolicy(CURRENT, [clone], pool(), CFG);
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicy).toBe(CURRENT);
	});

	it("is never worse than current over a mixed candidate set", () => {
		const selection = selectBestPolicy(CURRENT, [WORSE, BETTER, { ...CURRENT }], pool(), CFG);
		expect(selection.chosenScore).toBeGreaterThanOrEqual(selection.currentScore);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(BETTER);
	});

	it("rejects a candidate that reaches a lower quality even when its V would be higher", () => {
		// With a huge beta1 the one-probe policy would out-score BETTER on V; the
		// quality guard removes it before the argmax. On the twin pool the one probe
		// is charged 1 (its own probesToBest on the other tree) against BETTER's 3, so
		// V is 0.333 - 5 * 0.1 = -0.167 against 1 - 5 * 0.3 = -0.5; on SYNTH alone both
		// would be charged the whole budget and BETTER would win on quality outright.
		const oneProbe = policy({ selectionRule: "explore-root", stopRule: "fixed-rounds", beta: 1, batchSize: 1 });
		const cfg: DreamingScoreConfig = { ...CFG, objective: { beta1: 5, beta2: 0, beta3: 0 } };
		const better = scorePolicyOnPool(BETTER, twinPool(), cfg);
		const cheap = scorePolicyOnPool(oneProbe, twinPool(), cfg);
		expect(cheap.chargedProbes).toBe(1);
		expect(better.chargedProbes).toBe(3);
		expect(cheap.value).toBeCloseTo(1 / 3 - 0.5, 12);
		expect(better.value).toBeCloseTo(1 - 1.5, 12);
		expect(cheap.value).toBeGreaterThan(better.value);
		expect(cheap.quality).toBeLessThan(better.quality);
		const selection = selectBestPolicy(BETTER, [oneProbe], twinPool(), cfg);
		expect(selection.chosenPolicy).toBe(BETTER);
		expect(selection.improved).toBe(false);
		expect(selection.qualityRejected).toBe(1);
		expect(selection.currentQuality).toBe(1);
		expect(selection.evidenceTrees).toBe(1);
		// qualityEps widens the floor and lets the same candidate through.
		const relaxed = selectBestPolicy(BETTER, [oneProbe], twinPool(), { ...cfg, qualityEps: 1 });
		expect(relaxed.chosenPolicy).toBe(oneProbe);
		expect(relaxed.qualityRejected).toBe(0);
		// On the single tree the cheap policy is charged the full budget (10 probes) like BETTER.
		const alone = scorePolicyOnPool(oneProbe, pool(), cfg);
		expect(alone.chargedProbes).toBe(10);
		expect(alone.value).toBeLessThan(scorePolicyOnPool(BETTER, pool(), cfg).value);
	});
});

describe("the recorded exploration collapse (circle-packing s7, fixed-arm rounds 1-2)", () => {
	it("documents the bug: the raw-scale objective ranked the collapsed policy above the exploring one", () => {
		const trees = recordedPool();
		expect(policyId(EXPLORING)).toBe("1be99d403b0405a3");
		expect(policyId(COLLAPSED)).toBe("f559ec93fc3b1773");
		const exploring = trees.map((tree) => simulatePolicy(tree, EXPLORING, { k2: RECORDED_K2 }));
		const collapsed = trees.map((tree) => simulatePolicy(tree, COLLAPSED, { k2: RECORDED_K2 }));
		// The exploring policy reaches a far higher best with ~37 probes; the collapsed one spends 1.
		expect(meanOver(exploring, (r) => r.bestScore)).toBeGreaterThan(meanOver(collapsed, (r) => r.bestScore) + 0.2);
		expect(collapsed.every((r) => r.N === 1 && r.rounds === 1)).toBe(true);
		expect(exploring.every((r) => r.N >= 30)).toBe(true);
		// ...and the old defaults still preferred the collapsed policy on the mean (0.951 vs 0.891) and
		// on round 1 outright; only on round 2, where exploring reached the pool maximum, did the
		// 0.36 probe penalty fall just short of the 0.34 quality gap.
		expect(oldRawObjective(collapsed[0]!)).toBeGreaterThan(oldRawObjective(exploring[0]!) + 0.1);
		expect(oldRawObjective(exploring[1]!) - oldRawObjective(collapsed[1]!)).toBeLessThan(0.01);
		expect(meanOver(collapsed, oldRawObjective)).toBeGreaterThan(meanOver(exploring, oldRawObjective) + 0.05);
		// The raw penalty on one full rollout (0.36) exceeds the pool's whole score range (0.48 * 0.75).
		expect(0.01 * meanOver(exploring, (r) => r.N)).toBeGreaterThan(0.75 * (1.258098 - 0.774615));
	});

	it("ranks the exploring policy above the collapsed one under the normalized defaults, per tree and on the mean", () => {
		const trees = recordedPool();
		const scale = poolScoreScale(trees);
		expect(scale.scoreMin).toBeCloseTo(0.774615, 6);
		expect(scale.scoreMax).toBeCloseTo(1.258098, 6);
		for (const tree of trees) {
			const budget = { workers: tree.header.w, k1: RECORDED_K1 };
			const exploring = computeObjective(
				simulatePolicy(tree, EXPLORING, { k2: RECORDED_K2 }),
				OBJECTIVE,
				scale,
				budget,
			);
			const collapsed = computeObjective(
				simulatePolicy(tree, COLLAPSED, { k2: RECORDED_K2 }),
				OBJECTIVE,
				scale,
				budget,
			);
			// Per tree the gap is 0.205 (round 1: 0.681 vs 0.476) and 0.519 (round 2: 0.914 vs 0.395); it was
			// >= 0.3 under the old form because the collapsed policy earned no rounds-saved bonus (+0.0917
			// here) and the exploring one paid no anytime discount (its best arrives at probe 31-33 of 48).
			expect(exploring).toBeGreaterThan(collapsed + 0.2);
		}
		const exploring = scorePolicyOnPool(EXPLORING, trees, RECORDED_CFG);
		const collapsed = scorePolicyOnPool(COLLAPSED, trees, RECORDED_CFG);
		// Under the old form these were 0.878487 (== q: 37 probes over 12 rounds cancelled exactly) and
		// 0.356086. Now: exploring q 0.878487, anytime 0.708267, cost 37/48, roundsSaved 0 ->
		// 0.75 * 0.878487 + 0.25 * 0.708267 - 0.05 * 0.770833 = 0.797390; collapsed q 0.344627, cost 1/48,
		// roundsSaved 11/12 -> 0.344627 - 0.001042 + 0.091667 = 0.435252.
		expect(exploring.value).toBeCloseTo(0.79739, 5);
		expect(collapsed.value).toBeCloseTo(0.435252, 5);
		expect(exploring.anytime).toBeCloseTo(0.708267, 5);
		expect(exploring.cost).toBeCloseTo(37 / 48, 12);
		expect(exploring.roundsSaved).toBe(0);
		expect(collapsed.roundsSaved).toBeCloseTo(11 / 12, 12);
		expect(exploring.quality).toBeGreaterThan(collapsed.quality + 0.5);
		// They tie in V only at beta1 = 0.533 (a 10.7x margin over the default), before the quality guard.
		const breakEven =
			(0.75 * (exploring.quality - collapsed.quality) +
				0.25 * (exploring.anytime - collapsed.anytime) +
				0.1 * (exploring.roundsSaved - collapsed.roundsSaved)) /
			(exploring.cost - collapsed.cost);
		expect(breakEven).toBeCloseTo(0.5329, 3);
		expect(breakEven / DEFAULT_OBJECTIVE.beta1).toBeGreaterThan(10);
	});

	it("never lets the collapsed policy win the selection, in either direction", () => {
		const trees = recordedPool();
		const keep = selectBestPolicy(EXPLORING, [COLLAPSED, policy({ batchSize: 1, beta: 1 })], trees, RECORDED_CFG);
		expect(keep.chosenPolicy).toBe(EXPLORING);
		expect(keep.improved).toBe(false);
		expect(keep.qualityRejected).toBeGreaterThanOrEqual(1);
		const recover = selectBestPolicy(COLLAPSED, [EXPLORING], trees, RECORDED_CFG);
		expect(recover.chosenPolicy).toBe(EXPLORING);
		expect(recover.improved).toBe(true);
		expect(recover.chosenQuality).toBeGreaterThan(recover.currentQuality);
	});

	it("never rewards a one-probe-per-round candidate over one that reaches a higher best", () => {
		const trees = recordedPool();
		const oneProbe = policy({ selectionRule: "explore-root", stopRule: "never", batchSize: 1 });
		for (const tree of trees) {
			const replay = simulatePolicy(tree, oneProbe, { k2: RECORDED_K2 });
			expect(replay.N).toBeLessThanOrEqual(replay.rounds);
		}
		const cheap = scorePolicyOnPool(oneProbe, trees, RECORDED_CFG);
		// The root runs out of recorded children long before round 24, so the one-probe
		// walk is off support: its quality failure is reported `unmeasurable` (D-REC
		// precedence) and `qualityRejected` counts only `quality-rejected` verdicts.
		expect(cheap.inSupportMin).toBeLessThan(1);
		for (const current of [EXPLORING, policy({ beta: 3 }), policy({ selectionRule: "weighted" })]) {
			const currentScore = scorePolicyOnPool(current, trees, RECORDED_CFG);
			expect(currentScore.quality).toBeGreaterThan(cheap.quality + 0.4);
			const selection = selectBestPolicy(current, [oneProbe], trees, RECORDED_CFG);
			expect(selection.chosenPolicy).toBe(current);
			expect(selection.candidates[0]!.eligible).toBe(false);
			expect(selection.candidates[0]!.reason).toBe("unmeasurable");
			expect(selection.qualityRejected).toBe(0);
		}
		// (b) A one-probe policy that DOES reach the same best (best-first down the recorded chain, 12-14
		// probes plus 10-12 out-of-support cells over 24 rounds) passes the guard but still loses on V:
		// it saves probes and reaches the best earlier, but its 24 rounds exceed k1 (roundsSaved -1),
		// so full batches stay preferred at equal quality. Break-even beta2 is 0.0372 (2.7x under 0.10).
		const chain = policy({ stopRule: "never", batchSize: 1 });
		const chainScore = scorePolicyOnPool(chain, trees, RECORDED_CFG);
		const exploringScore = scorePolicyOnPool(EXPLORING, trees, RECORDED_CFG);
		expect(chainScore.quality).toBeCloseTo(exploringScore.quality, 12);
		expect(chainScore.rounds).toBe(24);
		expect(chainScore.roundsSaved).toBe(-1);
		expect(chainScore.cost).toBeLessThan(exploringScore.cost);
		expect(chainScore.anytime).toBeGreaterThan(exploringScore.anytime);
		expect(chainScore.value).toBeLessThan(exploringScore.value);
		expect(chainScore.value).toBeCloseTo(0.734635, 5);
		const selection = selectBestPolicy(EXPLORING, [chain], trees, RECORDED_CFG);
		expect(selection.chosenPolicy).toBe(EXPLORING);
		expect(selection.qualityRejected).toBe(0);
		// Off-support but eligible: the verdict is 'unmeasurable', not 'worse'.
		expect(selection.candidates[0]!.eligible).toBe(true);
		expect(selection.candidates[0]!.inSupportMin).toBeLessThan(1);
		expect(selection.candidates[0]!.reason).toBe("unmeasurable");
	});

	it("is scale invariant: multiplying every score by 1000 leaves V and the ranking unchanged", () => {
		const trees = recordedPool();
		const scaled = trees.map((tree) =>
			buildRecordedTree([
				tree.header,
				...tree.nodes.map((record) => ({ ...record, score: record.score * 1000 })),
				...tree.reveals,
			]),
		);
		// Rules whose replay itself is scale-free (no additive bias, no absolute target).
		const policies = [
			EXPLORING,
			policy({ batchSize: 1, stopRule: "fixed-rounds", beta: 1 }),
			policy({ selectionRule: "round-robin", beta: 2 }),
			policy({ selectionRule: "explore-root", stopRule: "never", batchSize: 2 }),
		];
		const original = policies.map((p) => scorePolicyOnPool(p, trees, RECORDED_CFG));
		const rescaled = policies.map((p) => scorePolicyOnPool(p, scaled, RECORDED_CFG));
		for (let index = 0; index < policies.length; index++) {
			expect(rescaled[index]!.value).toBeCloseTo(original[index]!.value, 9);
			expect(rescaled[index]!.quality).toBeCloseTo(original[index]!.quality, 9);
		}
		const rank = (scores: { value: number }[]) =>
			scores
				.map((score, index) => ({ score, index }))
				.sort((a, b) => b.score.value - a.score.value)
				.map((e) => e.index);
		expect(rank(rescaled)).toEqual(rank(original));
		const a = selectBestPolicy(policies[0]!, policies.slice(1), trees, RECORDED_CFG);
		const b = selectBestPolicy(policies[0]!, policies.slice(1), scaled, RECORDED_CFG);
		expect(policyId(b.chosenPolicy)).toBe(policyId(a.chosenPolicy));
		expect(b.qualityRejected).toBe(a.qualityRejected);
	});
});

/**
 * The recorded run-3 collapse (autocorrelation n 64, seed 7, W 3, k1 13, k2 26,
 * Sonnet-5 proposer and dreamer): the frozen pool at the first dreaming step
 * was ONE 13-node tree whose best node was the FIRST probe, so the dreamed
 * {fixed-rounds, beta 1} candidate (1 probe, 1 round) matched the incumbent's
 * q 1.0 and anytime 1.0 and won purely on the stop-early credit (V 1.0910 vs
 * 1.0295); deployed online it probed once per rollout and scored the uniform
 * baseline. `RUN3_TREE` is that tree; `RUN2_TREES` are the four run-2 trees
 * (same task and W, k1 6) the old objective found a real lever on.
 */
const RUN3_TREE = "autocorrelation-s7-i0-1789923274195.jsonl";
const RUN2_TREES = [0, 1, 2, 3].map((index) => `autocorrelation-s7-i${index}-1789858196752.jsonl`);
const RUN3_CFG: DreamingScoreConfig = { k1: 13, k2: 26, objective: DEFAULT_OBJECTIVE };
const RUN2_CFG: DreamingScoreConfig = { k1: 6, k2: 12, objective: DEFAULT_OBJECTIVE };
/** The dreamed winner of run 3's first step (`9edb7a5b887e861c`). */
const RUN3_COLLAPSE = policy({ stopRule: "fixed-rounds", beta: 1 });
/** The dreamer's other single-probe route: stop the moment the recorded best is in hand. */
const RUN3_THRESHOLD = policy({ stopRule: "threshold", targetScore: 0.5247272682369769 });
/** Run 3's second dreamed candidate (`a70474c08b2c78a1`): patience 2 at the default batch. */
const RUN3_PATIENT = policy({ stopRule: "patience", beta: 2 });

describe("the recorded run-3 collapse (autocorrelation s7, one 13-node tree whose best is the first probe)", () => {
	it("reproduces the recorded step under raw terms: the one-probe candidate out-scores the incumbent on stop-early credit alone", () => {
		const tree = loadFixture(RUN3_TREE);
		expect(tree.header.w).toBe(3);
		expect(policyId(RUN3_COLLAPSE)).toBe("9edb7a5b887e861c");
		expect(policyId(RUN3_PATIENT)).toBe("a70474c08b2c78a1");
		const incumbent = simulatePolicy(tree, EXPLORING, { k2: 26 });
		expect(incumbent).toMatchObject({ N: 13, rounds: 7, outOfSupportCells: 0, probesToBest: 1, roundsToBest: 1 });
		expect(incumbent.bestScore).toBeCloseTo(0.5247272682369769, 15);
		const collapse = simulatePolicy(tree, RUN3_COLLAPSE, { k2: 26 });
		expect(collapse).toMatchObject({ N: 1, rounds: 1, outOfSupportCells: 0, probesToBest: 1, roundsToBest: 1 });
		expect(collapse.bestScore).toBe(incumbent.bestScore);
		const scale = poolScoreScale([tree]);
		const budget = { workers: 3, k1: 13 };
		// Without evidence (a lone replay): q 1, anytime 1 for both; the incumbent pays cost 13/39 and earns
		// roundsSaved 6/13 (1.029487), the collapse pays 1/39 and earns 12/13 (1.091026): the recorded step.
		const incumbentRaw = computeObjectiveTerms(incumbent, DEFAULT_OBJECTIVE, scale, budget);
		const collapseRaw = computeObjectiveTerms(collapse, DEFAULT_OBJECTIVE, scale, budget);
		expect(incumbentRaw.quality).toBe(1);
		expect(incumbentRaw.anytime).toBe(1);
		expect(collapseRaw.anytime).toBe(1);
		expect(incumbentRaw.value).toBeCloseTo(1.029487, 6);
		expect(collapseRaw.value).toBeCloseTo(1.091026, 6);
		expect(collapseRaw.value - incumbentRaw.value).toBeCloseTo(0.05 * (12 / 39) + 0.1 * (6 / 13), 12);
	});

	it("no longer lets either one-probe candidate beat the incumbent on the single tree: no evidence, no credit", () => {
		const trees = [loadFixture(RUN3_TREE)];
		const selection = selectBestPolicy(EXPLORING, [RUN3_COLLAPSE, RUN3_THRESHOLD], trees, RUN3_CFG);
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicy).toBe(EXPLORING);
		expect(selection.measuredTrees).toBe(1);
		expect(selection.evidenceTrees).toBe(0);
		// The incumbent is charged what it spent (13 probes of B = 39, 7 rounds of 13): V = 1 - 0.05 * 13/39 +
		// 0.1 * 6/13 = 1.029487, the recorded step's number. Every candidate is charged the whole budget
		// (39 probes, 13 rounds, no other tree to vouch), so V = 1 - beta1 = 0.95 and both lose outright.
		expect(selection.current.chargedProbes).toBe(13);
		expect(selection.current.chargedRounds).toBe(7);
		expect(selection.currentScore).toBeCloseTo(1.029487, 6);
		expect(selection.currentMinBest).toBeCloseTo(0.5247272682369769, 15);
		for (const verdict of selection.candidates) {
			expect(verdict.reason).toBe("worse");
			expect(verdict.eligible).toBe(true);
			expect(verdict.N).toBe(1);
			expect(verdict.rounds).toBe(1);
			expect(verdict.chargedProbes).toBe(39);
			expect(verdict.chargedRounds).toBe(13);
			expect(verdict.evidenceTrees).toBe(0);
			expect(verdict.value).toBeCloseTo(0.95, 12);
		}
		// The lever scan on that tree: the recorded 336 of 337 grid policies are still eligible, none has a lever.
		const scan = runLeverScan(EXPLORING, trees, RUN3_CFG);
		expect(scan.policies).toBe(337);
		expect(scan.eligible).toBe(336);
		expect(scan.gap).toBe(0);
		expect(scan.bestPolicyId).toBe(policyId(EXPLORING));
	});

	it("charges a candidate that stops right after the best on one tree the probes and rounds it still needed on the other", () => {
		const [a, b] = [loadFixture(RUN3_TREE), loadFixture(RUN2_TREES[0]!)];
		expect(b!.header.w).toBe(3);
		// The incumbent replays both in full support: best at probe 1 / round 1 on A, probe 8 / round 5 on B.
		const incumbentA = simulatePolicy(a!, EXPLORING, { k2: 26 });
		const incumbentB = simulatePolicy(b!, EXPLORING, { k2: 26 });
		expect(incumbentA).toMatchObject({ N: 13, rounds: 7, outOfSupportCells: 0, probesToBest: 1, roundsToBest: 1 });
		expect(incumbentB).toMatchObject({ N: 13, rounds: 6, outOfSupportCells: 0, probesToBest: 8, roundsToBest: 5 });
		// RUN3_PATIENT stops after round 3 on A (4 probes, best at probe 1) and runs the full 6 rounds on B
		// (13 probes, best at probe 8 / round 5), in support on both.
		const patientA = simulatePolicy(a!, RUN3_PATIENT, { k2: 26 });
		const patientB = simulatePolicy(b!, RUN3_PATIENT, { k2: 26 });
		expect(patientA).toMatchObject({ N: 4, rounds: 3, outOfSupportCells: 0, probesToBest: 1, roundsToBest: 1 });
		expect(patientB).toMatchObject({ N: 13, rounds: 6, outOfSupportCells: 0, probesToBest: 8, roundsToBest: 5 });
		expect(patientA.bestScore).toBe(incumbentA.bestScore);
		expect(patientB.bestScore).toBe(incumbentB.bestScore);
		const selection = selectBestPolicy(EXPLORING, [RUN3_PATIENT, RUN3_COLLAPSE], [a!, b!], RUN3_CFG);
		expect(selection.measuredTrees).toBe(2);
		expect(selection.evidenceTrees).toBe(1);
		const [patient, collapse] = selection.candidates;
		// On A it is charged B's probesToBest 8 (not its own 4) and B's roundsToBest 5 (not its own 3);
		// on B its own 13 and 6 exceed A's 1 and 1: means (8 + 13) / 2 and (5 + 6) / 2.
		expect(patient!.inSupportMin).toBe(1);
		expect(patient!.N).toBe(8.5);
		expect(patient!.rounds).toBe(4.5);
		expect(patient!.chargedProbes).toBe(10.5);
		expect(patient!.chargedRounds).toBe(5.5);
		expect(patient!.cost).toBeCloseTo((8 / 39 + 13 / 39) / 2, 12);
		expect(patient!.roundsSaved).toBeCloseTo((1 - 5 / 13 + (1 - 6 / 13)) / 2, 12);
		expect(patient!.evidenceTrees).toBe(1);
		// The incumbent is charged its raw spend, 13 probes on each tree and 7 + 6 rounds (here the same numbers a
		// cross-tree horizon would give: max(13, 8), max(13, 1) probes and max(7, 5), max(6, 1) rounds).
		expect(selection.current.chargedProbes).toBe(13);
		expect(selection.current.chargedRounds).toBe(6.5);
		expect(selection.current).toEqual(scorePolicyOnPool(EXPLORING, [a!, b!], RUN3_CFG, "raw"));
		// With B vouching for its stopping, the patient policy is a real same-best-for-less win over the incumbent.
		expect(patient!.eligible).toBe(true);
		expect(patient!.reason).toBe("winner");
		expect(selection.improved).toBe(true);
		// The collapse candidate now fails the per-tree guard: on B one probe reveals 0.3601 against a root of 0.5.
		expect(collapse!.reason).toBe("quality-rejected");
		expect(collapse!.eligible).toBe(false);
		expect(simulatePolicy(b!, RUN3_COLLAPSE, { k2: 26 }).bestScore).toBe(0.5);
	});

	it("keeps the run-2 pool's lever under the evidence rule: +0.007109, the same winner, charged where the horizon exceeds its spend", () => {
		const trees = RUN2_TREES.map(loadFixture);
		const incumbent = scorePolicyOnPool(EXPLORING, trees, RUN2_CFG, "raw");
		// The incumbent is charged its raw spend: per tree N 13, 12, 10, 12 (mean 11.75, cost 11.75/18) over
		// 6 rounds each, V 0.842156. Under the cross-tree horizon (probesToBest 8, 11, 8, 8) tree i2's 10
		// probes would have been charged at i1's 11, a mean charge of 12 and V 0.841462: that surcharge on
		// the incumbent's own recorded trees is what the symmetric rule got wrong.
		expect(incumbent.N).toBe(11.75);
		expect(incumbent.chargedProbes).toBe(11.75);
		expect(incumbent.chargedRounds).toBe(6);
		expect(incumbent.quality).toBeCloseTo(0.883344, 6);
		expect(incumbent.anytime).toBeCloseTo(0.849147, 6);
		expect(incumbent.cost).toBeCloseTo(11.75 / 18, 12);
		expect(incumbent.roundsSaved).toBe(0);
		expect(incumbent.value).toBeCloseTo(0.842156, 6);
		const surcharged = scorePolicyOnPool(EXPLORING, trees, RUN2_CFG, "evidence");
		expect(surcharged.chargedProbes).toBe(12);
		expect(surcharged.value).toBeCloseTo(0.841462, 6);
		expect(incumbent.value - surcharged.value).toBeCloseTo(DEFAULT_OBJECTIVE.beta1 * (0.25 / 18), 12);
		const scan = runLeverScan(EXPLORING, trees, RUN2_CFG);
		expect(scan.policies).toBe(337);
		expect(scan.eligible).toBe(84);
		expect(scan.gap).toBeGreaterThan(0);
		// 0.849265 - 0.842156: the symmetric rule's +0.007804 minus the quarter probe it surcharged the incumbent.
		expect(scan.gap).toBeCloseTo(0.007109, 6);
		expect(scan.gap).toBeCloseTo(0.007804 - DEFAULT_OBJECTIVE.beta1 * (0.25 / 18), 6);
		const winner = policy({ selectionRule: "weighted", stopRule: "fixed-rounds", batchSize: 2, beta: 6 });
		expect(scan.bestPolicyId).toBe(policyId(winner));
		const winnerScore = scorePolicyOnPool(winner, trees, RUN2_CFG);
		// The same best on every tree with N 10, 9, 8, 9 (probesToBest 7, 9, 7, 7): tree i2 is charged at
		// i1's 9, so the candidate's mean charge is 9.25 against a mean N of 9.
		expect(winnerScore.quality).toBeCloseTo(incumbent.quality, 12);
		expect(winnerScore.N).toBe(9);
		expect(winnerScore.chargedProbes).toBe(9.25);
		expect(winnerScore.rounds).toBe(6);
		expect(winnerScore.value).toBeCloseTo(0.849265, 6);
		expect(winnerScore.value - incumbent.value).toBeCloseTo(scan.gap, 12);
		const selection = selectBestPolicy(EXPLORING, [winner], trees, RUN2_CFG);
		expect(selection.current).toEqual(incumbent);
		expect(selection.chosenScore - selection.currentScore).toBeCloseTo(scan.gap, 12);
	});
});

/**
 * Two trees a best-first / patience 2 / batchSize 2 incumbent grew on
 * sum-difference (W 2, k1 6; seeds 1 and 2 of a local rollout, scores to six
 * decimals). FLIP_EARLY: the root is the best, two root children score below it,
 * patience stops after round 2 (N 2, rounds 2, probesToBest 0). FLIP_LATE: the
 * best (0.865302) is n5, probe 5 of 10, round 4 of 6. A symmetric evidence rule
 * charged the incumbent 5 probes / 4 rounds on FLIP_EARLY where it spent 2 / 2
 * (its own late tree as the horizon); the batch-1 chain FLIP_CHAIN, which reaches
 * n5 by probe 4 on FLIP_LATE, then beat that surcharged incumbent while losing to
 * its raw spend. This is the skeptic's attack (c) in two small trees.
 */
const FLIP_EARLY: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "flip-early",
		taskId: "synthetic",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "flip-early-n0", parentId: null, seq: 0, round: 0, score: 0.798354 }),
	node({ id: "flip-early-n1", parentId: "flip-early-n0", seq: 1, round: 1, score: 0.793301 }),
	node({ id: "flip-early-n2", parentId: "flip-early-n0", seq: 2, round: 2, branch: 1, score: 0.763679 }),
];
const FLIP_LATE: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "flip-late",
		taskId: "synthetic",
		w: 2,
		seed: 2,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "flip-late-n0", parentId: null, seq: 0, round: 0, score: 0.802616 }),
	node({ id: "flip-late-n1", parentId: "flip-late-n0", seq: 1, round: 1, score: 0.788096 }),
	node({ id: "flip-late-n2", parentId: "flip-late-n0", seq: 2, round: 2, branch: 1, score: 0.820952 }),
	node({ id: "flip-late-n3", parentId: "flip-late-n2", seq: 3, round: 3, score: 0.824312 }),
	node({ id: "flip-late-n4", parentId: "flip-late-n1", seq: 4, round: 3, score: 0.788096 }),
	node({ id: "flip-late-n5", parentId: "flip-late-n3", seq: 5, round: 4, score: 0.865302 }),
	node({ id: "flip-late-n6", parentId: "flip-late-n0", seq: 6, round: 4, branch: 2, score: 0.800283 }),
	node({ id: "flip-late-n7", parentId: "flip-late-n5", seq: 7, round: 5, score: 0.840815 }),
	node({ id: "flip-late-n8", parentId: "flip-late-n0", seq: 8, round: 5, branch: 3, score: 0.828011 }),
	node({ id: "flip-late-n9", parentId: "flip-late-n7", seq: 9, round: 6, score: 0.806434 }),
	node({ id: "flip-late-n10", parentId: "flip-late-n8", seq: 10, round: 6, score: 0.802616 }),
];
const FLIP_CFG: DreamingScoreConfig = { k1: 6, k2: 12, objective: DEFAULT_OBJECTIVE };
const FLIP_INCUMBENT = policy({ batchSize: 2, beta: 2 });
const FLIP_CHAIN = policy({ batchSize: 1, beta: 2 });

function flipPool(): RecordedTree[] {
	return [buildRecordedTree(FLIP_EARLY), buildRecordedTree(FLIP_LATE)];
}

describe("(c) the incumbent is charged its raw spend, a candidate never below its own", () => {
	it("replays the pair as recorded: the incumbent stops early on FLIP_EARLY and finds FLIP_LATE's best at probe 5", () => {
		const [early, late] = flipPool();
		expect(simulatePolicy(early!, FLIP_INCUMBENT, { k2: 12 })).toMatchObject({
			N: 2,
			rounds: 2,
			outOfSupportCells: 0,
			probesToBest: 0,
			roundsToBest: 0,
			bestScore: 0.798354,
		});
		expect(simulatePolicy(late!, FLIP_INCUMBENT, { k2: 12 })).toMatchObject({
			N: 10,
			rounds: 6,
			outOfSupportCells: 0,
			probesToBest: 5,
			roundsToBest: 4,
			bestScore: 0.865302,
		});
		// The chain walks the same two probes on FLIP_EARLY and reaches n5 in 4 probes on FLIP_LATE (root -> n2 -> n3 -> n5).
		expect(simulatePolicy(early!, FLIP_CHAIN, { k2: 12 })).toMatchObject({ N: 2, rounds: 2, outOfSupportCells: 0 });
		expect(simulatePolicy(late!, FLIP_CHAIN, { k2: 12 })).toMatchObject({
			N: 6,
			rounds: 6,
			outOfSupportCells: 0,
			probesToBest: 4,
			roundsToBest: 4,
			bestScore: 0.865302,
		});
	});

	it("does not let the chain flip the selection by surcharging the incumbent's real early stop", () => {
		const pool = flipPool();
		const raw = scorePolicyOnPool(FLIP_INCUMBENT, pool, FLIP_CFG, "raw");
		const surcharged = scorePolicyOnPool(FLIP_INCUMBENT, pool, FLIP_CFG, "evidence");
		const chain = scorePolicyOnPool(FLIP_CHAIN, pool, FLIP_CFG);
		// Raw: FLIP_EARLY charged 2 probes / 2 rounds, FLIP_LATE 10 / 6, mean 6 / 4. A cross-tree horizon would
		// charge FLIP_EARLY at FLIP_LATE's probesToBest 5 / roundsToBest 4: mean 7.5 / 5, costing the incumbent
		// beta1 * 3/12 + beta2 * 2/6 halved over the pool = 0.022917 in V.
		expect(raw.chargedProbes).toBe(6);
		expect(raw.chargedRounds).toBe(4);
		expect(surcharged.chargedProbes).toBe(7.5);
		expect(surcharged.chargedRounds).toBe(5);
		expect(raw.value - surcharged.value).toBeCloseTo(
			(DEFAULT_OBJECTIVE.beta1 * (3 / 12) + DEFAULT_OBJECTIVE.beta2 * (2 / 6)) / 2,
			12,
		);
		// The chain's own horizon (FLIP_LATE's probe 4 / round 4) charges its FLIP_EARLY stop at 4 / 4: mean 5 / 5.
		expect(chain.N).toBe(4);
		expect(chain.chargedProbes).toBe(5);
		expect(chain.chargedRounds).toBe(5);
		expect(chain.quality).toBeCloseTo(raw.quality, 12);
		// Below the raw incumbent, above the surcharged one: the flip the symmetric rule allowed.
		expect(chain.value).toBeLessThan(raw.value);
		expect(chain.value).toBeGreaterThan(surcharged.value);
		// (0.659562 / 0.636645 / 0.651263 on the full-precision recorded trees; the literals hold six decimals.)
		expect(raw.value).toBeCloseTo(0.659565, 6);
		expect(surcharged.value).toBeCloseTo(0.636648, 6);
		expect(chain.value).toBeCloseTo(0.651266, 6);
		const selection = selectBestPolicy(FLIP_INCUMBENT, [FLIP_CHAIN], pool, FLIP_CFG);
		expect(selection.improved).toBe(false);
		expect(selection.current).toEqual(raw);
		expect(selection.currentMinBest).toBe(0.798354);
		expect(selection.candidates[0]!.reason).toBe("worse");
		expect(selection.candidates[0]!.eligible).toBe(true);
		expect(selection.candidates[0]!.chargedProbes).toBe(5);
	});

	it("holds across the whole lever-scan grid: no candidate is charged below its own spend, the incumbent never above it", () => {
		for (const [current, pool, cfg] of [
			[FLIP_INCUMBENT, flipPool(), FLIP_CFG],
			[EXPLORING, RUN2_TREES.map(loadFixture), RUN2_CFG],
			[INCUMBENT, exactPool(), EXACT_CFG],
		] as const) {
			const selection = selectBestPolicy(current, leverScanGrid(current, 4), pool, cfg);
			const rawCurrent = scorePolicyOnPool(current, pool, cfg, "raw");
			expect(selection.current.chargedProbes).toBe(rawCurrent.N + rawCurrent.outOfSupportCells);
			expect(selection.current.chargedRounds).toBe(rawCurrent.rounds);
			for (const verdict of selection.candidates) {
				if (verdict.reason === "identical" || verdict.reason === "unmeasurable") continue;
				expect(verdict.chargedProbes).toBeGreaterThanOrEqual(verdict.N + verdict.outOfSupportCells - 1e-12);
				expect(verdict.chargedRounds).toBeGreaterThanOrEqual(verdict.rounds - 1e-12);
			}
		}
	});
});

/**
 * A tree the incumbent `INCUMBENT` (best-first, fixed-rounds 3, batchSize 2, W 2)
 * replays exactly: round 1 root -> n1 (0.5), round 2 n1 -> n2 (0.9), round 3
 * {n2, root} -> n3 (0.8), n4 (0.4). The best (n2) is in hand after probe 2, so a
 * candidate that stops after round 2 (fewer rounds) or probes one cell in round 3
 * (fewer probes, same rounds) reaches the same best for less.
 */
const EXACT: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "exact",
		taskId: "synthetic",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "exact-n0", parentId: null, seq: 0, round: 0, score: 0.3 }),
	node({ id: "exact-n1", parentId: "exact-n0", seq: 1, round: 1, score: 0.5 }),
	node({ id: "exact-n2", parentId: "exact-n1", seq: 2, round: 2, score: 0.9 }),
	node({ id: "exact-n3", parentId: "exact-n2", seq: 3, round: 3, score: 0.8 }),
	node({ id: "exact-n4", parentId: "exact-n0", seq: 4, round: 3, branch: 1, score: 0.4 }),
];
const EXACT_CFG: DreamingScoreConfig = { k1: 3, k2: 6, objective: DEFAULT_OBJECTIVE };
const INCUMBENT = policy({ selectionRule: "best-first", stopRule: "fixed-rounds", beta: 3, batchSize: 2 });
const FEWER_PROBES = policy({ ...INCUMBENT, batchSize: 1 });
const FEWER_ROUNDS = policy({ ...INCUMBENT, beta: 2 });

/** EXACT alone: no second tree can vouch for stopping early, so every policy is charged 6 probes and 3 rounds. */
function exactTree(): RecordedTree[] {
	return [buildRecordedTree(EXACT)];
}

/**
 * EXACT and an identical twin. Each policy's cross-tree horizon is its own
 * probes/rounds to best on the twin (INCUMBENT 2/2, FEWER_PROBES 2/2,
 * FEWER_ROUNDS 2/2), never above its own spend, so every charged number below
 * equals the raw one and the same-best-for-less arithmetic is unchanged from a
 * single tree scored without evidence.
 */
function exactPool(): RecordedTree[] {
	return [buildRecordedTree(EXACT), twinOf(EXACT, "exact2")];
}

describe("(a) same best for less now strictly wins where the incumbent replays exactly", () => {
	it("replays the incumbent exactly and finds the same best with fewer probes or fewer rounds", () => {
		const tree = exactTree()[0]!;
		const incumbent = simulatePolicy(tree, INCUMBENT, { k2: 6 });
		expect(incumbent.revealedIds).toEqual(["exact-n0", "exact-n1", "exact-n2", "exact-n3", "exact-n4"]);
		expect(incumbent.N).toBe(4);
		expect(incumbent.rounds).toBe(3);
		expect(incumbent.outOfSupportCells).toBe(0);
		expect(incumbent.probesToBest).toBe(2);
		expect(incumbent.roundsToBest).toBe(2);
		const probes = simulatePolicy(tree, FEWER_PROBES, { k2: 6 });
		expect(probes.bestScore).toBe(incumbent.bestScore);
		expect(probes.N).toBe(3);
		expect(probes.rounds).toBe(3);
		expect(probes.probesToBest).toBe(2);
		const rounds = simulatePolicy(tree, FEWER_ROUNDS, { k2: 6 });
		expect(rounds.bestScore).toBe(incumbent.bestScore);
		expect(rounds.N).toBe(2);
		expect(rounds.rounds).toBe(2);
		expect(rounds.roundsToBest).toBe(2);
		// The twin replays identically under another id.
		const twin = exactPool()[1]!;
		expect(twin.header.treeId).toBe("exact2");
		expect(simulatePolicy(twin, INCUMBENT, { k2: 6 }).revealedIds).toEqual(
			incumbent.revealedIds.map((id) => id.replace("exact-", "exact2-")),
		);
	});

	it("earns no stop-early credit on a single tree: both cheaper policies lose to the incumbent's raw spend", () => {
		// With one measured tree a candidate's horizon is the budget itself: both are charged
		// B = 2 * 3 = 6 probes and k1 = 3 rounds, so cost 1 and roundsSaved 0 and V collapses to
		// (1 - beta3) q + beta3 anytime - beta1: q 1 and anytime (1/3 + 5) / 6 = 0.888889 (the best
		// arrives at probe 2 of 6, after one probe at 0.5, normalized 1/3), so V = 0.75 + 0.222222 - 0.05
		// = 0.922222. The incumbent is charged what it spent, 4 probes over 3 rounds: cost 4/6 and
		// V = 0.972222 - 0.033333 = 0.938889, above both candidates by beta1 * 2/6 = 0.016667.
		const current = scorePolicyOnPool(INCUMBENT, exactTree(), EXACT_CFG, "raw");
		const probes = scorePolicyOnPool(FEWER_PROBES, exactTree(), EXACT_CFG);
		const rounds = scorePolicyOnPool(FEWER_ROUNDS, exactTree(), EXACT_CFG);
		for (const score of [probes, rounds]) {
			expect(score.chargedProbes).toBe(6);
			expect(score.chargedRounds).toBe(3);
			expect(score.cost).toBe(1);
			expect(score.roundsSaved).toBe(0);
			expect(score.anytime).toBeCloseTo(16 / 18, 12);
			expect(score.value).toBeCloseTo(0.75 + 0.25 * (16 / 18) - 0.05, 12);
		}
		expect(current.chargedProbes).toBe(4);
		expect(current.chargedRounds).toBe(3);
		expect(current.anytime).toBeCloseTo(16 / 18, 12);
		expect(current.value).toBeCloseTo(0.75 + 0.25 * (16 / 18) - 0.05 * (4 / 6), 12);
		expect(current.value - probes.value).toBeCloseTo(DEFAULT_OBJECTIVE.beta1 * (2 / 6), 12);
		expect(probes.N).toBe(3);
		expect(rounds.N).toBe(2);
		expect(rounds.rounds).toBe(2);
		const selection = selectBestPolicy(INCUMBENT, [FEWER_PROBES, FEWER_ROUNDS], exactTree(), EXACT_CFG);
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicy).toBe(INCUMBENT);
		expect(selection.current).toEqual(current);
		expect(selection.currentMinBest).toBe(0.9);
		expect(selection.measuredTrees).toBe(1);
		expect(selection.evidenceTrees).toBe(0);
		expect(selection.candidates.map((candidate) => candidate.reason)).toEqual(["worse", "worse"]);
		expect(selection.candidates.every((candidate) => candidate.evidenceTrees === 0)).toBe(true);
		expect(runLeverScan(INCUMBENT, exactTree(), EXACT_CFG).gap).toBe(0);
	});

	it("scores fewer probes at equal rounds strictly higher (the old form scored this an exact tie)", () => {
		const current = scorePolicyOnPool(INCUMBENT, exactPool(), EXACT_CFG);
		const probes = scorePolicyOnPool(FEWER_PROBES, exactPool(), EXACT_CFG);
		expect(probes.quality).toBe(current.quality);
		expect(probes.anytime).toBeCloseTo(current.anytime, 12);
		expect(probes.roundsSaved).toBe(current.roundsSaved);
		expect(probes.cost).toBeCloseTo(3 / 6, 12);
		expect(current.cost).toBeCloseTo(4 / 6, 12);
		// The twin vouches: FEWER_PROBES reached the best at probe 2 there, below its own 3.
		expect(probes.chargedProbes).toBe(3);
		expect(current.chargedProbes).toBe(4);
		expect(probes.value - current.value).toBeCloseTo(DEFAULT_OBJECTIVE.beta1 / 6, 12);
		const selection = selectBestPolicy(INCUMBENT, [FEWER_PROBES], exactPool(), EXACT_CFG);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(FEWER_PROBES);
		expect(selection.candidates[0]!.reason).toBe("winner");
	});

	it("scores fewer rounds strictly higher again, and above fewer probes alone", () => {
		const current = scorePolicyOnPool(INCUMBENT, exactPool(), EXACT_CFG);
		const rounds = scorePolicyOnPool(FEWER_ROUNDS, exactPool(), EXACT_CFG);
		expect(rounds.quality).toBe(current.quality);
		expect(rounds.chargedProbes).toBe(2);
		expect(rounds.chargedRounds).toBe(2);
		expect(rounds.roundsSaved).toBeCloseTo(1 / 3, 12);
		expect(rounds.value - current.value).toBeCloseTo(
			DEFAULT_OBJECTIVE.beta1 * (2 / 6) + DEFAULT_OBJECTIVE.beta2 * (1 / 3),
			12,
		);
		const selection = selectBestPolicy(INCUMBENT, [FEWER_PROBES, FEWER_ROUNDS], exactPool(), EXACT_CFG);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(FEWER_ROUNDS);
		expect(selection.candidates.map((candidate) => candidate.reason)).toEqual(["worse", "winner"]);
		expect(selection.candidates.every((candidate) => candidate.eligible && candidate.inSupportMin === 1)).toBe(true);
	});

	it("still wins under the strictly-cost objective (beta3 = beta2 = 0)", () => {
		const cfg: DreamingScoreConfig = { ...EXACT_CFG, objective: { beta1: 0.05, beta2: 0, beta3: 0 } };
		const selection = selectBestPolicy(INCUMBENT, [FEWER_PROBES, FEWER_ROUNDS], exactPool(), cfg);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(FEWER_ROUNDS);
		expect(selection.chosenScore - selection.currentScore).toBeCloseTo(0.05 * (2 / 6), 12);
	});
});

/**
 * OWN is a tree the incumbent `OWNER` (best-first, never, batchSize 2, W 2, k1 3)
 * grew and replays exactly: round 1 root -> n1 (0.5), round 2 n1 -> n2 (0.6),
 * round 3 {n2, root} -> n3 (0.9), n4 (0.2). FOREIGN is a tree explore-root grew:
 * three root children 0.2, 0.95, 0.3 in seq order. OWNER replays FOREIGN out of
 * support (round 1 reveals 0.2, then best-first sits on the childless 0.2 leaf and
 * burns a dead cell every round to k2), so FOREIGN is not a baseline for it.
 */
const OWN: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "own",
		taskId: "synthetic",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "own-n0", parentId: null, seq: 0, round: 0, score: 0.1 }),
	node({ id: "own-n1", parentId: "own-n0", seq: 1, round: 1, score: 0.5 }),
	node({ id: "own-n2", parentId: "own-n1", seq: 2, round: 2, score: 0.6 }),
	node({ id: "own-n3", parentId: "own-n2", seq: 3, round: 3, score: 0.9 }),
	node({ id: "own-n4", parentId: "own-n0", seq: 4, round: 3, branch: 1, score: 0.2 }),
];
const FOREIGN: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "foreign",
		taskId: "synthetic",
		w: 2,
		seed: 2,
		policyId: "q",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "foreign-n0", parentId: null, seq: 0, round: 0, score: 0.1 }),
	node({ id: "foreign-n1", parentId: "foreign-n0", seq: 1, round: 1, score: 0.2 }),
	node({ id: "foreign-n2", parentId: "foreign-n0", seq: 2, round: 2, branch: 1, score: 0.95 }),
	node({ id: "foreign-n3", parentId: "foreign-n0", seq: 3, round: 3, branch: 2, score: 0.3 }),
];
const MEASURED_CFG: DreamingScoreConfig = { k1: 3, k2: 6, objective: DEFAULT_OBJECTIVE };
const OWNER = policy({ selectionRule: "best-first", stopRule: "never", batchSize: 2 });
/** Regresses on OWN (best 0.5 of 0.9, in support) but reveals FOREIGN's 0.95 in two rounds. */
const FOREIGN_FRIENDLY = policy({ selectionRule: "explore-root", stopRule: "fixed-rounds", beta: 2, batchSize: 1 });
/** Patience incumbents that replay OWN identically and differ only in how long they burn dead cells on FOREIGN. */
const PATIENT = policy({ selectionRule: "best-first", stopRule: "patience", beta: 4, batchSize: 2 });
const IMPATIENT = policy({ ...PATIENT, beta: 2 });

function ownPool(): RecordedTree[] {
	return [buildRecordedTree(OWN)];
}

function mixedPool(): RecordedTree[] {
	return [buildRecordedTree(OWN), buildRecordedTree(FOREIGN)];
}

describe("the measured pool (trees the incumbent replays in full support)", () => {
	it("replays OWN exactly and FOREIGN out of support", () => {
		const [own, foreign] = mixedPool();
		const onOwn = simulatePolicy(own!, OWNER, { k2: 6 });
		expect(onOwn.revealedIds).toEqual(["own-n0", "own-n1", "own-n2", "own-n3", "own-n4"]);
		expect(onOwn.outOfSupportCells).toBe(0);
		expect(onOwn.rounds).toBe(3);
		const onForeign = simulatePolicy(foreign!, OWNER, { k2: 6 });
		expect(onForeign.N).toBe(1);
		expect(onForeign.outOfSupportCells).toBe(5);
		expect(onForeign.rounds).toBe(6);
		expect(onForeign.bestScore).toBe(0.2);
		const measured = measurePool(OWNER, mixedPool(), MEASURED_CFG);
		expect(measured.sorted.map((tree) => tree.header.treeId)).toEqual(["foreign", "own"]);
		expect(measured.measured.map((tree) => tree.header.treeId)).toEqual(["own"]);
		expect(measured.replays).toHaveLength(1);
		expect(measured.currentInSupport).toBeCloseTo((1 + 1 / 6) / 2, 12);
	});

	it("does not let a fictional gain on a foreign tree mask a real regression on the incumbent's own tree", () => {
		// Pool means over BOTH trees would pass the guard: the incumbent's off-support replay of
		// FOREIGN drags its mean quality down to where the candidate's 0.95 there covers a 0.4 drop on OWN.
		const fictional = scorePolicyOnPool(FOREIGN_FRIENDLY, mixedPool(), MEASURED_CFG);
		const baseline = scorePolicyOnPool(OWNER, mixedPool(), MEASURED_CFG);
		expect(fictional.quality).toBeGreaterThan(baseline.quality);
		expect(fictional.inSupportMin).toBe(1);
		const selection = selectBestPolicy(OWNER, [FOREIGN_FRIENDLY], mixedPool(), MEASURED_CFG);
		expect(selection.poolSize).toBe(2);
		expect(selection.measuredTrees).toBe(1);
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicy).toBe(OWNER);
		const verdict = selection.candidates[0]!;
		expect(verdict.reason).toBe("quality-rejected");
		expect(verdict.eligible).toBe(false);
		expect(verdict.inSupportMin).toBe(1);
		expect(verdict.quality).toBeCloseTo(0.5, 12);
		expect(selection.currentQuality).toBe(1);
		expect(selection.qualityRejected).toBe(1);
		// The result is exactly the selection on the measured trees alone.
		const ownOnly = selectBestPolicy(OWNER, [FOREIGN_FRIENDLY], ownPool(), MEASURED_CFG);
		expect(selection.currentScore).toBe(ownOnly.currentScore);
		expect(selection.chosenScore).toBe(ownOnly.chosenScore);
		expect(selection.current).toEqual(ownOnly.current);
		expect(verdict.value).toBe(ownOnly.candidates[0]!.value);
		expect(ownOnly.measuredTrees).toBe(1);
		expect(ownOnly.currentInSupport).toBe(1);
	});

	it("does not let a candidate win by burning fewer dead rounds on a tree the incumbent is off support on", () => {
		const [own, foreign] = mixedPool();
		expect(simulatePolicy(own!, IMPATIENT, { k2: 6 })).toEqual({
			...simulatePolicy(own!, PATIENT, { k2: 6 }),
			policyId: policyId(IMPATIENT),
		});
		const patientForeign = simulatePolicy(foreign!, PATIENT, { k2: 6 });
		const impatientForeign = simulatePolicy(foreign!, IMPATIENT, { k2: 6 });
		expect(patientForeign.rounds).toBe(5);
		expect(impatientForeign.rounds).toBe(3);
		expect(patientForeign.bestScore).toBe(impatientForeign.bestScore);
		// Pool means over both trees would hand IMPATIENT the win on the burn alone.
		expect(scorePolicyOnPool(IMPATIENT, mixedPool(), MEASURED_CFG).value).toBeGreaterThan(
			scorePolicyOnPool(PATIENT, mixedPool(), MEASURED_CFG).value,
		);
		const selection = selectBestPolicy(PATIENT, [IMPATIENT], mixedPool(), MEASURED_CFG);
		expect(selection.measuredTrees).toBe(1);
		expect(selection.improved).toBe(false);
		// On the one measured tree (OWN) both walk identically: N 4, 3 rounds. IMPATIENT, a candidate with
		// no other tree to vouch for it, is charged the whole budget (6 probes, cost 1, roundsSaved 0);
		// PATIENT, the incumbent, its raw 4 probes (cost 4/6), so the twin walk loses by beta1 * 2/6.
		expect(selection.candidates[0]!.reason).toBe("worse");
		expect(selection.candidates[0]!.rounds).toBe(3);
		expect(selection.candidates[0]!.N).toBe(4);
		expect(selection.candidates[0]!.chargedProbes).toBe(6);
		expect(selection.candidates[0]!.roundsSaved).toBe(0);
		expect(selection.current.chargedProbes).toBe(4);
		expect(selection.current.roundsSaved).toBe(0);
		expect(selection.currentScore - selection.candidates[0]!.value).toBeCloseTo(
			DEFAULT_OBJECTIVE.beta1 * (2 / 6),
			12,
		);
	});

	it("measures nothing when the incumbent is off support on every tree: nothing eligible, every candidate unmeasurable", () => {
		const selection = selectBestPolicy(
			OWNER,
			[FOREIGN_FRIENDLY, PATIENT],
			[buildRecordedTree(FOREIGN)],
			MEASURED_CFG,
		);
		expect(selection.poolSize).toBe(1);
		expect(selection.measuredTrees).toBe(0);
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicy).toBe(OWNER);
		expect(selection.currentScore).toBe(0);
		expect(selection.currentMinBest).toBe(0);
		expect(selection.candidates.map((candidate) => candidate.reason)).toEqual(["unmeasurable", "unmeasurable"]);
		expect(selection.candidates.every((candidate) => !candidate.eligible)).toBe(true);
		expect(selection.qualityRejected).toBe(0);
		expect(selection.simulations).toBe(1);
		expect(runLeverScan(OWNER, [buildRecordedTree(FOREIGN)], MEASURED_CFG)).toMatchObject({
			eligible: 0,
			gap: 0,
			bestPolicyId: policyId(OWNER),
		});
	});

	it("scores the lever scan on the measured pool too", () => {
		const mixed = runLeverScan(PATIENT, mixedPool(), MEASURED_CFG);
		const own = runLeverScan(PATIENT, ownPool(), MEASURED_CFG);
		expect(mixed.policies).toBe(own.policies);
		expect(mixed.eligible).toBe(own.eligible);
		expect(mixed.gap).toBe(own.gap);
		expect(mixed.bestValue).toBe(own.bestValue);
		expect(mixed.bestPolicyId).toBe(own.bestPolicyId);
		// The scan pays one extra simulation per unmeasured tree: the incumbent's support check.
		expect(mixed.simulations).toBe(own.simulations + 1);
	});

	it("counts the simulations it actually makes: current on every tree, each simulated candidate on the measured trees", () => {
		const deadOnly = policy({ ...OWNER, branchWidth: 7 });
		const selection = selectBestPolicy(
			OWNER,
			[{ ...OWNER }, FOREIGN_FRIENDLY, { ...FOREIGN_FRIENDLY }, deadOnly, PATIENT],
			mixedPool(),
			MEASURED_CFG,
		);
		// PATIENT walks OWN exactly as OWNER does but, charged the whole budget against OWNER's raw 4 probes, is 'worse'.
		expect(selection.candidates.map((candidate) => candidate.reason)).toEqual([
			"identical",
			"quality-rejected",
			"duplicate",
			"unmeasurable",
			"worse",
		]);
		// 2 (OWNER on own and foreign) + 1 (FOREIGN_FRIENDLY on own) + 1 (PATIENT on own, an identical walk).
		expect(selection.simulations).toBe(4);
		const scan = runLeverScan(OWNER, mixedPool(), MEASURED_CFG);
		expect(scan.simulations).toBe(2 + (scan.policies - 1) * 1);
	});
});

describe("candidate verdicts", () => {
	it("labels the current policy's own id 'identical' and keeps it out of the argmax and scoredCount", () => {
		const selection = selectBestPolicy(CURRENT, [{ ...CURRENT }, BETTER], pool(), CFG);
		expect(selection.candidates[0]).toMatchObject({
			index: 0,
			policyId: policyId(CURRENT),
			origin: "local",
			changed: [],
			duplicateOf: null,
			eligible: false,
			reason: "identical",
		});
		expect(selection.candidates[0]!.value).toBe(selection.currentScore);
		expect(selection.candidates[1]!.reason).toBe("winner");
		expect(selection.scoredCount).toBe(2);
		expect(selection.candidatePolicyIds).toEqual([policyId(CURRENT), policyId(BETTER)]);
	});

	it("labels a repeated candidate 'duplicate' of the first, scored once", () => {
		const selection = selectBestPolicy(CURRENT, [BETTER, { ...BETTER }, WORSE, { ...WORSE }], pool(), CFG);
		const reasons = selection.candidates.map((candidate) => candidate.reason);
		expect(reasons[0]).toBe("winner");
		expect(reasons[1]).toBe("duplicate");
		expect(reasons[3]).toBe("duplicate");
		expect(selection.candidates[1]!.duplicateOf).toBe(0);
		expect(selection.candidates[3]!.duplicateOf).toBe(2);
		expect(selection.candidates[1]!.value).toBe(selection.candidates[0]!.value);
		expect(selection.candidates[1]!.eligible).toBe(false);
		expect(selection.scoredCount).toBe(3);
		expect(selection.chosenPolicy).toBe(BETTER);
	});

	it("labels a replay-dead-only change 'unmeasurable': scored as current, never eligible", () => {
		const deadOnly = policy({ ...CURRENT, branchWidth: 7, refineDepth: 0, recoveryPolicy: "abandon" });
		expect(policyId(deadOnly)).not.toBe(policyId(CURRENT));
		const selection = selectBestPolicy(CURRENT, [deadOnly], pool(), CFG);
		const verdict = selection.candidates[0]!;
		expect(verdict.reason).toBe("unmeasurable");
		expect(verdict.eligible).toBe(false);
		expect(verdict.changed).toEqual(["recoveryPolicy", "branchWidth", "refineDepth"]);
		expect(verdict.changed.every((field) => (REPLAY_DEAD_FIELDS as readonly string[]).includes(field))).toBe(true);
		expect(verdict.value).toBe(selection.currentScore);
		expect(verdict.quality).toBe(selection.currentQuality);
		expect(selection.improved).toBe(false);
		expect(selection.qualityRejected).toBe(0);
		expect(selection.scoredCount).toBe(2);
	});

	it("labels an off-support quality failure 'unmeasurable' and an in-support one 'quality-rejected'", () => {
		// WORSE probes the exhausted leaf n1 every round after the first (in-support 1/k2), never reaching 0.9.
		const inSupportCheap = policy({ selectionRule: "explore-root", stopRule: "fixed-rounds", beta: 1, batchSize: 1 });
		const selection = selectBestPolicy(BETTER, [WORSE, inSupportCheap], pool(), CFG);
		const [offSupport, cheap] = selection.candidates;
		expect(offSupport!.inSupportMin).toBeLessThan(1);
		expect(offSupport!.quality).toBeLessThan(selection.currentQuality);
		expect(offSupport!.eligible).toBe(false);
		expect(offSupport!.reason).toBe("unmeasurable");
		expect(cheap!.inSupportMin).toBe(1);
		expect(cheap!.quality).toBeLessThan(selection.currentQuality);
		expect(cheap!.eligible).toBe(false);
		expect(cheap!.reason).toBe("quality-rejected");
		// The count agrees with the verdicts: the off-support failure is `unmeasurable`, not double-counted.
		expect(selection.qualityRejected).toBe(1);
		expect(selection.qualityRejected).toBe(
			selection.candidates.filter((candidate) => candidate.reason === "quality-rejected").length,
		);
	});

	it("labels an eligible equal-value loser 'tie' and reports origins and the dreamer kind", () => {
		// The threshold rule never fires at targetScore 1e6, so a different targetScore replays identically.
		const twin = policy({ ...FEWER_ROUNDS, targetScore: 999_999 });
		const [first, second] = [FEWER_ROUNDS, twin].sort((a, b) => policyId(a).localeCompare(policyId(b)));
		const selection = selectBestPolicy(
			INCUMBENT,
			[
				{ policy: second!, origin: "llm" },
				{ policy: first!, origin: "local" },
			],
			exactPool(),
			EXACT_CFG,
		);
		expect(selection.improved).toBe(true);
		expect(policyId(selection.chosenPolicy)).toBe(policyId(first!));
		expect(selection.candidates.map((candidate) => candidate.reason)).toEqual(["tie", "winner"]);
		expect(selection.candidates.map((candidate) => candidate.origin)).toEqual(["llm", "local"]);
		expect(selection.dreamer).toBe("mixed");
		expect(dreamerKindOf([{ policy: BETTER, origin: "llm" }])).toBe("llm");
		expect(dreamerKindOf([{ policy: BETTER, origin: "local" }])).toBe("local");
		expect(dreamerKindOf([])).toBe("local");
	});

	it("carries the pool-mean terms on every verdict", () => {
		const selection = selectBestPolicy(INCUMBENT, [FEWER_PROBES], exactPool(), EXACT_CFG);
		const verdict = selection.candidates[0]!;
		const score = scorePolicyOnPool(FEWER_PROBES, exactPool(), EXACT_CFG);
		expect(verdict).toMatchObject({
			value: score.value,
			quality: score.quality,
			anytime: score.anytime,
			cost: score.cost,
			roundsSaved: score.roundsSaved,
			N: 3,
			rounds: 3,
			outOfSupportCells: 0,
			inSupportMean: 1,
			inSupportMin: 1,
			chargedProbes: 3,
			chargedRounds: 3,
			evidenceTrees: 1,
		});
		expect(selection.current).toEqual(scorePolicyOnPool(INCUMBENT, exactPool(), EXACT_CFG));
	});
});

describe("revoked candidates", () => {
	it("labels a revoked id 'revoked': simulated for the record, never eligible, never the winner", () => {
		const revoked = new Set([policyId(FEWER_ROUNDS)]);
		const plain = selectBestPolicy(INCUMBENT, [FEWER_ROUNDS, FEWER_PROBES], exactPool(), EXACT_CFG);
		expect(plain.candidates.map((candidate) => candidate.reason)).toEqual(["winner", "worse"]);
		const selection = selectBestPolicy(
			INCUMBENT,
			[FEWER_ROUNDS, FEWER_PROBES, { ...FEWER_ROUNDS }],
			exactPool(),
			EXACT_CFG,
			revoked,
		);
		expect(selection.candidates.map((candidate) => candidate.reason)).toEqual(["revoked", "winner", "duplicate"]);
		const [reverted, winner] = selection.candidates;
		expect(reverted!.eligible).toBe(false);
		// Its numbers are real (the same replay as without the revocation), it just cannot be chosen.
		expect(reverted!.value).toBe(plain.candidates[0]!.value);
		expect(reverted!.value).toBeGreaterThan(winner!.value);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(FEWER_PROBES);
		expect(selection.simulations).toBe(plain.simulations);
		expect(selection.qualityRejected).toBe(0);
		// The current policy's own id is never 'revoked', even when listed.
		const self = selectBestPolicy(
			INCUMBENT,
			[{ ...INCUMBENT }],
			exactPool(),
			EXACT_CFG,
			new Set([policyId(INCUMBENT)]),
		);
		expect(self.candidates[0]!.reason).toBe("identical");
	});

	it("threads through runDreaming and the candidate span", () => {
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let result: ReturnType<typeof runDreaming>;
		try {
			result = runDreaming({
				current: INCUMBENT,
				pool: exactPool(),
				dreams: 1,
				k1: EXACT_CFG.k1,
				k2: EXACT_CFG.k2,
				rng: createSeededRng(1),
				proposeCandidates: () => [FEWER_ROUNDS],
				revoked: new Set([policyId(FEWER_ROUNDS)]),
				leverScan: false,
			});
		} finally {
			unsubscribe();
		}
		expect(result.improved).toBe(false);
		expect(result.chosenPolicyId).toBe(policyId(INCUMBENT));
		expect(result.candidates[0]!.reason).toBe("revoked");
		expect(result.current).toEqual(scorePolicyOnPool(INCUMBENT, exactPool(), EXACT_CFG, "raw"));
		expect(result.currentMinBest).toBe(0.9);
		const candidate = spans.find((span) => span.name === "dream.candidate")!;
		expect(candidate.attrs["dream.reason"]).toBe("revoked");
		expect(candidate.attrs["dream.eligible"]).toBe(false);
	});
});

describe("lever scan", () => {
	it("builds the fixed grid: every selection x stop rule x batchSize 1..W x LEVER_SCAN_BETAS plus current, deduplicated", () => {
		const grid = leverScanGrid(DEFAULT_POLICY, 2);
		const ids = new Set(grid.map(policyId));
		expect(ids.size).toBe(grid.length);
		// DEFAULT_POLICY has batchSize 4, outside 1..2, so it is the one extra entry.
		expect(grid.length).toBe(SELECTION_RULES.length * STOP_RULES.length * 2 * LEVER_SCAN_BETAS.length + 1);
		expect(grid[0]).toBe(DEFAULT_POLICY);
		expect(leverScanGrid(DEFAULT_POLICY, 4).length).toBe(
			SELECTION_RULES.length * STOP_RULES.length * 4 * LEVER_SCAN_BETAS.length,
		);
		// Only the four scanned fields ever differ from current.
		for (const entry of grid.slice(1)) {
			const changed = policyFieldsDiffering(entry, DEFAULT_POLICY);
			expect(changed.every((field) => ["selectionRule", "stopRule", "batchSize", "beta"].includes(field))).toBe(
				true,
			);
		}
	});

	it("is deterministic and rng-free: the same pool yields a byte-equal record", () => {
		const first = runLeverScan(INCUMBENT, exactPool(), EXACT_CFG);
		const second = runLeverScan(INCUMBENT, exactPool(), EXACT_CFG);
		expect(JSON.stringify(second)).toBe(JSON.stringify(first));
		expect(first.policies).toBe(leverScanGrid(INCUMBENT, 2).length);
		expect(first.eligible).toBeGreaterThan(0);
		expect(first.gap).toBeGreaterThan(0);
		expect(first.bestValue - scorePolicyOnPool(INCUMBENT, exactPool(), EXACT_CFG).value).toBeCloseTo(first.gap, 12);
		// The grid contains FEWER_ROUNDS (beta 2, batchSize 2) and nothing beats stopping right after the best.
		expect(first.bestPolicyId).toBe(policyId(FEWER_ROUNDS));
	});

	it("reports gap 0 and the current id when no grid policy beats current", () => {
		const scan = runLeverScan(FEWER_ROUNDS, exactPool(), EXACT_CFG);
		expect(scan.gap).toBe(0);
		expect(scan.bestPolicyId).toBe(policyId(FEWER_ROUNDS));
		expect(scan.bestValue).toBeCloseTo(scorePolicyOnPool(FEWER_ROUNDS, exactPool(), EXACT_CFG).value, 12);
	});

	it("finds the recorded fixture pool has a lever the old form hid: same best, fewer probes", () => {
		const scan = runLeverScan(EXPLORING, recordedPool(), RECORDED_CFG);
		expect(scan.policies).toBe(leverScanGrid(EXPLORING, 4).length);
		expect(scan.gap).toBeGreaterThanOrEqual(0);
		expect(scan.eligible).toBeGreaterThan(0);
	});
});

describe("proposePolicies / mutatePolicy", () => {
	it("is deterministic for a fixed seed", () => {
		const a = proposePolicies(CURRENT, 5, createSeededRng(1)).map(policyId);
		const b = proposePolicies(CURRENT, 5, createSeededRng(1)).map(policyId);
		expect(a).toEqual(b);
	});

	it("derives each candidate from its own labelled fork (order-independent)", () => {
		const candidates = proposePolicies(CURRENT, 4, createSeededRng(1));
		const standalone = mutatePolicy(CURRENT, createSeededRng(1).fork("cand:2"));
		expect(policyId(candidates[2]!)).toBe(policyId(standalone));
	});

	it("always yields an in-bounds, parseable policy", () => {
		for (let seed = 0; seed < 50; seed++) {
			const mutated = mutatePolicy(CURRENT, createSeededRng(seed));
			expect(() => parseExplorationPolicy(mutated)).not.toThrow();
		}
	});

	it("never returns the current policy's id and never touches a replay-dead field", () => {
		for (const current of [CURRENT, DEFAULT_POLICY, INCUMBENT]) {
			for (let seed = 0; seed < 200; seed++) {
				const mutated = mutatePolicy(current, createSeededRng(seed));
				expect(policyId(mutated)).not.toBe(policyId(current));
				for (const field of REPLAY_DEAD_FIELDS) expect(mutated[field]).toBe(current[field]);
			}
		}
		for (const candidate of proposePolicies(DEFAULT_POLICY, 64, createSeededRng(3))) {
			expect(policyId(candidate)).not.toBe(policyId(DEFAULT_POLICY));
		}
	});
});

describe("runDreaming", () => {
	it("spends zero tokens and is never worse than current in V or quality", () => {
		const result = runDreaming({
			current: CURRENT,
			pool: pool(),
			dreams: 8,
			k1: CFG.k1,
			k2: CFG.k2,
			rng: createSeededRng(7),
			objective: OBJECTIVE,
		});
		expect(result.tokens).toBe(0);
		expect(result.chosenScore).toBeGreaterThanOrEqual(result.currentScore);
		expect(result.chosenQuality).toBeGreaterThanOrEqual(result.currentQuality - 1e-9);
		expect(result.candidatePolicyIds).toHaveLength(8);
		expect(result.qualityRejected).toBeGreaterThanOrEqual(0);
		expect(result.qualityRejected).toBeLessThanOrEqual(8);
		expect(() => parseExplorationPolicy(result.chosenPolicy)).not.toThrow();
	});

	it("is deterministic for a fixed seed", () => {
		const options = {
			current: CURRENT,
			pool: pool(),
			dreams: 8,
			k1: CFG.k1,
			k2: CFG.k2,
			objective: OBJECTIVE,
		};
		const first = runDreaming({ ...options, rng: createSeededRng(7) });
		const second = runDreaming({ ...options, rng: createSeededRng(7) });
		expect(second.chosenPolicyId).toBe(first.chosenPolicyId);
		expect(second.chosenScore).toBeCloseTo(first.chosenScore, 12);
		expect(second.improved).toBe(first.improved);
		expect(second.qualityRejected).toBe(first.qualityRejected);
	});

	it("redeploys an injected better candidate through the same selection rule", () => {
		const result = runDreaming({
			current: CURRENT,
			pool: pool(),
			dreams: 1,
			k1: CFG.k1,
			k2: CFG.k2,
			rng: createSeededRng(1),
			objective: OBJECTIVE,
			proposeCandidates: () => [BETTER],
		});
		expect(result.improved).toBe(true);
		expect(result.chosenPolicyId).toBe(policyId(BETTER));
		expect(result.poolSize).toBe(1);
	});

	it("returns verdicts, the dreamer kind and the lever scan, and stamps the iteration on its spans", () => {
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let result: ReturnType<typeof runDreaming>;
		try {
			result = runDreaming({
				current: INCUMBENT,
				pool: exactPool(),
				dreams: 2,
				k1: EXACT_CFG.k1,
				k2: EXACT_CFG.k2,
				rng: createSeededRng(1),
				iteration: 3,
				proposeCandidates: () => [
					{ policy: FEWER_PROBES, origin: "llm" },
					{ policy: { ...INCUMBENT }, origin: "local" },
				],
			});
		} finally {
			unsubscribe();
		}
		expect(result.improved).toBe(true);
		expect(result.chosenPolicyId).toBe(policyId(FEWER_PROBES));
		expect(result.dreamer).toBe("mixed");
		expect(result.candidates.map((candidate) => candidate.reason)).toEqual(["winner", "identical"]);
		expect(result.candidates.map((candidate) => candidate.origin)).toEqual(["llm", "local"]);
		expect(result.leverScan).not.toBeNull();
		expect(result.leverScan!.gap).toBeGreaterThan(0);
		expect(result.leverScan).toEqual(runLeverScan(INCUMBENT, exactPool(), EXACT_CFG));

		const dream = spans.find((span) => span.name === "dream.dream")!;
		expect(dream.attrs["dream.iteration"]).toBe(3);
		expect(dream.attrs["dream.lever_gap"]).toBe(result.leverScan!.gap);
		expect(dream.attrs["dream.lever_policies"]).toBe(result.leverScan!.policies);
		expect(dream.attrs["dream.dreamer"]).toBe("mixed");
		expect(dream.attrs["dream.unmeasurable"]).toBe(0);
		expect(dream.attrs["dream.measured_trees"]).toBe(2);
		expect(dream.attrs["dream.evidence_trees"]).toBe(1);
		expect(result.evidenceTrees).toBe(1);
		expect(dream.attrs["dream.simulations"]).toBe(result.simulations);
		expect(dream.attrs["dream.lever_simulations"]).toBe(result.leverScan!.simulations);
		expect(dream.attrs["dream.quality_rejected"]).toBe(0);
		const replay = spans.find((span) => span.name === "dream.replay")!;
		expect(replay.attrs["dream.iteration"]).toBe(3);
		expect(replay.parentSpanId).toBe(dream.spanId);
		// Exactly the simulations made: INCUMBENT on both trees plus FEWER_PROBES on both; the identical candidate is skipped.
		expect(replay.attrs["dream.simulations"]).toBe(4);
		expect(result.simulations).toBe(4);
		expect(result.measuredTrees).toBe(2);
		expect(replay.attrs["dream.measured_trees"]).toBe(2);
		const candidates = spans.filter((span) => span.name === "dream.candidate");
		expect(candidates).toHaveLength(2);
		for (const span of candidates) {
			expect(span.parentSpanId).toBe(dream.spanId);
			expect(span.attrs["dream.iteration"]).toBe(3);
			for (const value of Object.values(span.attrs)) expect(["string", "number", "boolean"]).toContain(typeof value);
		}
		expect(candidates[0]!.attrs).toMatchObject({
			"dream.candidate_index": 0,
			"dream.policy_id": policyId(FEWER_PROBES),
			"dream.origin": "llm",
			"dream.reason": "winner",
			"dream.changed": "batchSize",
			"dream.in_support_min": 1,
			"dream.charged_probes": 3,
			"dream.charged_rounds": 3,
		});
		expect(candidates[1]!.attrs["dream.reason"]).toBe("identical");
	});

	it("skips the lever scan when asked and leaves the selection untouched", () => {
		const options = {
			current: INCUMBENT,
			pool: exactPool(),
			dreams: 1,
			k1: EXACT_CFG.k1,
			k2: EXACT_CFG.k2,
			proposeCandidates: () => [FEWER_ROUNDS],
		};
		const scanned = runDreaming({ ...options, rng: createSeededRng(1) });
		const unscanned = runDreaming({ ...options, rng: createSeededRng(1), leverScan: false });
		expect(unscanned.leverScan).toBeNull();
		expect(unscanned.chosenPolicyId).toBe(scanned.chosenPolicyId);
		expect(unscanned.candidates).toEqual(scanned.candidates);
	});

	it("refuses an injected candidate that collapses exploration on the recorded pool", () => {
		const result = runDreaming({
			current: EXPLORING,
			pool: recordedPool(),
			dreams: 1,
			k1: RECORDED_K1,
			k2: RECORDED_K2,
			rng: createSeededRng(1),
			proposeCandidates: () => [COLLAPSED],
		});
		expect(result.improved).toBe(false);
		expect(result.chosenPolicyId).toBe(policyId(EXPLORING));
		expect(result.qualityRejected).toBe(1);
	});
});
