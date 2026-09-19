import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
	type DreamingScoreConfig,
	mutatePolicy,
	proposePolicies,
	runDreaming,
	scorePolicyOnPool,
	selectBestPolicy,
} from "../src/core/dream/improve.js";
import {
	computeObjective,
	computeObjectiveTerms,
	DEFAULT_OBJECTIVE,
	normalizedQuality,
	poolScoreScale,
} from "../src/core/dream/objective.js";
import { DEFAULT_POLICY, type ExplorationPolicy, parseExplorationPolicy, policyId } from "../src/core/dream/policy.js";
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

describe("computeObjective (eq. 1, normalized)", () => {
	const base: ReplayResult = {
		policyId: "x",
		treeId: "t",
		revealedIds: [],
		N: 3,
		rounds: 3,
		bestScore: 0.9,
		outOfSupportRounds: 0,
	};

	it("matches V = q - beta1*N/(W*k1) + beta2*N/(max(1,rounds)*W)", () => {
		const q = (0.9 - 0.3) / (0.9 - 0.3);
		const expected = q - 0.05 * (3 / (2 * 5)) + 0.05 * (3 / (3 * 2));
		expect(computeObjective(base, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET)).toBeCloseTo(expected, 12);
		const terms = computeObjectiveTerms(base, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(terms.quality).toBe(1);
		expect(terms.cost).toBeCloseTo(0.3, 12);
		expect(terms.parallelism).toBeCloseTo(0.5, 12);
		expect(terms.value).toBeCloseTo(expected, 12);
	});

	it("uses max(1, rounds) when rounds is 0 and honours the betas", () => {
		const zeroRounds: ReplayResult = { ...base, bestScore: 0.6, N: 5, rounds: 0 };
		const q = (0.6 - 0.3) / 0.6;
		expect(computeObjective(zeroRounds, { beta1: 0.2, beta2: 0.1 }, SYNTH_SCALE, SYNTH_BUDGET)).toBeCloseTo(
			q - 0.2 * (5 / 10) + 0.1 * (5 / (1 * 2)),
			12,
		);
	});

	it("bounds the cost and parallelism swing by beta1 + beta2 over a full budget", () => {
		const full: ReplayResult = { ...base, bestScore: 0.9, N: 10, rounds: 5 };
		const terms = computeObjectiveTerms(full, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(terms.cost).toBe(1);
		expect(terms.parallelism).toBe(1);
		// Full budget at full parallelism with beta1 == beta2 scores exactly q.
		expect(terms.value).toBeCloseTo(1, 12);
		const one: ReplayResult = { ...base, bestScore: 0.9, N: 1, rounds: 1 };
		const oneTerms = computeObjectiveTerms(one, OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET);
		expect(Math.abs(oneTerms.value - oneTerms.quality)).toBeLessThanOrEqual(OBJECTIVE.beta1 + OBJECTIVE.beta2);
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
		const terms = computeObjectiveTerms(
			simulatePolicy(trees[0]!, CURRENT, { k2: CFG.k2 }),
			OBJECTIVE,
			poolScoreScale(trees),
			{ workers: trees[0]!.header.w, k1: CFG.k1 },
		);
		const score = scorePolicyOnPool(CURRENT, trees, CFG);
		expect(score.value).toBeCloseTo(terms.value, 12);
		expect(score.quality).toBeCloseTo(terms.quality, 12);
	});

	it("scores a policy that spends fewer probes differently from one that spends more", () => {
		const trees = pool();
		expect(scorePolicyOnPool(CURRENT, trees, CFG).value).not.toBeCloseTo(
			scorePolicyOnPool(BETTER, trees, CFG).value,
			6,
		);
	});

	it("returns 0 for an empty pool", () => {
		expect(scorePolicyOnPool(CURRENT, [], CFG)).toEqual({ value: 0, quality: 0 });
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
		// quality guard removes it before the argmax.
		const oneProbe = policy({ selectionRule: "explore-root", stopRule: "fixed-rounds", beta: 1, batchSize: 1 });
		const cfg: DreamingScoreConfig = { ...CFG, objective: { beta1: 5, beta2: 0 } };
		const better = scorePolicyOnPool(BETTER, pool(), cfg);
		const cheap = scorePolicyOnPool(oneProbe, pool(), cfg);
		expect(cheap.value).toBeGreaterThan(better.value);
		expect(cheap.quality).toBeLessThan(better.quality);
		const selection = selectBestPolicy(BETTER, [oneProbe], pool(), cfg);
		expect(selection.chosenPolicy).toBe(BETTER);
		expect(selection.improved).toBe(false);
		expect(selection.qualityRejected).toBe(1);
		expect(selection.currentQuality).toBe(1);
		// qualityEps widens the floor and lets the same candidate through.
		const relaxed = selectBestPolicy(BETTER, [oneProbe], pool(), { ...cfg, qualityEps: 1 });
		expect(relaxed.chosenPolicy).toBe(oneProbe);
		expect(relaxed.qualityRejected).toBe(0);
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
			expect(exploring).toBeGreaterThan(collapsed + 0.3);
		}
		const exploring = scorePolicyOnPool(EXPLORING, trees, RECORDED_CFG);
		const collapsed = scorePolicyOnPool(COLLAPSED, trees, RECORDED_CFG);
		expect(exploring.value).toBeCloseTo(0.878487, 5);
		expect(collapsed.value).toBeCloseTo(0.356086, 5);
		expect(exploring.quality).toBeGreaterThan(collapsed.quality + 0.5);
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
		for (const current of [EXPLORING, policy({ beta: 3 }), policy({ selectionRule: "weighted" })]) {
			const currentScore = scorePolicyOnPool(current, trees, RECORDED_CFG);
			expect(currentScore.quality).toBeGreaterThan(cheap.quality + 0.4);
			const selection = selectBestPolicy(current, [oneProbe], trees, RECORDED_CFG);
			expect(selection.chosenPolicy).toBe(current);
			expect(selection.qualityRejected).toBe(1);
		}
		// A one-probe policy that DOES reach the same best (best-first down the recorded chain, 12-14
		// probes over 24 rounds) passes the guard but still loses on V: the probes it saves are worth
		// less than the parallelism it gives up, so full batches stay preferred at equal quality.
		const chain = policy({ stopRule: "never", batchSize: 1 });
		const chainScore = scorePolicyOnPool(chain, trees, RECORDED_CFG);
		const exploringScore = scorePolicyOnPool(EXPLORING, trees, RECORDED_CFG);
		expect(chainScore.quality).toBeCloseTo(exploringScore.quality, 12);
		expect(chainScore.value).toBeLessThan(exploringScore.value);
		const selection = selectBestPolicy(EXPLORING, [chain], trees, RECORDED_CFG);
		expect(selection.chosenPolicy).toBe(EXPLORING);
		expect(selection.qualityRejected).toBe(0);
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
