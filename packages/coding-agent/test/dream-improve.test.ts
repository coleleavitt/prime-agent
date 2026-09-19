import { describe, expect, it } from "vitest";
import {
	type DreamingScoreConfig,
	mutatePolicy,
	proposePolicies,
	runDreaming,
	scorePolicyOnPool,
	selectBestPolicy,
} from "../src/core/dream/improve.js";
import { computeObjective } from "../src/core/dream/objective.js";
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

const OBJECTIVE = { beta1: 0.01, beta2: 0.02 };
const CFG: DreamingScoreConfig = { k2: 10, objective: OBJECTIVE };

const CURRENT = policy({ selectionRule: "explore-root", stopRule: "patience", beta: 1, batchSize: 1 });
const BETTER = policy({ selectionRule: "explore-root", stopRule: "never", batchSize: 1 });
const WORSE = policy({ selectionRule: "best-first", stopRule: "never", batchSize: 1 });

function pool(): RecordedTree[] {
	return [buildRecordedTree(SYNTH)];
}

describe("computeObjective (eq. 1)", () => {
	const base: ReplayResult = {
		policyId: "x",
		treeId: "t",
		revealedIds: [],
		N: 3,
		rounds: 3,
		bestScore: 0.9,
		outOfSupportRounds: 0,
	};

	it("matches V = maxScore - beta1*N + beta2*N/max(1,rounds)", () => {
		expect(computeObjective(base, OBJECTIVE)).toBeCloseTo(0.9 - 0.01 * 3 + (0.02 * 3) / 3, 12);
	});

	it("uses max(1, rounds) when rounds is 0", () => {
		const zeroRounds: ReplayResult = { ...base, bestScore: 2, N: 5, rounds: 0 };
		expect(computeObjective(zeroRounds, OBJECTIVE)).toBeCloseTo(2 - 0.01 * 5 + (0.02 * 5) / 1, 12);
	});
});

describe("scorePolicyOnPool", () => {
	it("is the mean replay objective over the pool", () => {
		const trees = pool();
		const manual = computeObjective(simulatePolicy(trees[0]!, CURRENT, { k2: CFG.k2 }), OBJECTIVE);
		expect(scorePolicyOnPool(CURRENT, trees, CFG)).toBeCloseTo(manual, 12);
	});

	it("scores a policy that spends fewer probes differently from one that spends more", () => {
		const trees = pool();
		expect(scorePolicyOnPool(CURRENT, trees, CFG)).not.toBeCloseTo(scorePolicyOnPool(BETTER, trees, CFG), 6);
	});

	it("returns 0 for an empty pool", () => {
		expect(scorePolicyOnPool(CURRENT, [], CFG)).toBe(0);
	});
});

describe("selectBestPolicy", () => {
	it("picks a strictly-better candidate", () => {
		const selection = selectBestPolicy(CURRENT, [BETTER], pool(), CFG);
		expect(selection.improved).toBe(true);
		expect(selection.chosenPolicy).toBe(BETTER);
		expect(selection.chosenScore).toBeGreaterThan(selection.currentScore);
		expect(selection.scoredCount).toBe(2);
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
	it("spends zero tokens and is never worse than current", () => {
		const result = runDreaming({
			current: CURRENT,
			pool: pool(),
			dreams: 8,
			k2: 10,
			rng: createSeededRng(7),
			objective: OBJECTIVE,
		});
		expect(result.tokens).toBe(0);
		expect(result.chosenScore).toBeGreaterThanOrEqual(result.currentScore);
		expect(result.candidatePolicyIds).toHaveLength(8);
		expect(() => parseExplorationPolicy(result.chosenPolicy)).not.toThrow();
	});

	it("is deterministic for a fixed seed", () => {
		const first = runDreaming({
			current: CURRENT,
			pool: pool(),
			dreams: 8,
			k2: 10,
			rng: createSeededRng(7),
			objective: OBJECTIVE,
		});
		const second = runDreaming({
			current: CURRENT,
			pool: pool(),
			dreams: 8,
			k2: 10,
			rng: createSeededRng(7),
			objective: OBJECTIVE,
		});
		expect(second.chosenPolicyId).toBe(first.chosenPolicyId);
		expect(second.chosenScore).toBeCloseTo(first.chosenScore, 12);
		expect(second.improved).toBe(first.improved);
	});

	it("redeploys an injected better candidate through the same selection rule", () => {
		const result = runDreaming({
			current: CURRENT,
			pool: pool(),
			dreams: 1,
			k2: 10,
			rng: createSeededRng(1),
			objective: OBJECTIVE,
			proposeCandidates: () => [BETTER],
		});
		expect(result.improved).toBe(true);
		expect(result.chosenPolicyId).toBe(policyId(BETTER));
		expect(result.poolSize).toBe(1);
	});
});
