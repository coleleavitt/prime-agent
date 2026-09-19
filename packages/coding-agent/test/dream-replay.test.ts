import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { interpretPolicy } from "../src/core/dream/interpreter.js";
import { assertLegalBatch, LegalBatchError, type ObservationView } from "../src/core/dream/observation.js";
import { DEFAULT_POLICY, type ExplorationPolicy } from "../src/core/dream/policy.js";
import { createReplaySimulator, simulatePolicy } from "../src/core/dream/replay.js";
import { buildRecordedTree, DreamStoreError, readTree, TreeWriter } from "../src/core/dream/store.js";
import type { NodeRecord, TreeHeaderRecord, TreeRecord } from "../src/core/dream/types.js";

function header(treeId: string): TreeHeaderRecord {
	return {
		type: "tree",
		version: 1,
		treeId,
		taskId: "synthetic",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	};
}

function node(over: Partial<NodeRecord> & Pick<NodeRecord, "id" | "parentId" | "seq" | "score">): NodeRecord {
	return {
		type: "node",
		branch: 0,
		round: 0,
		valid: true,
		artifactRef: "ref",
		tokens: 0,
		ts: 0,
		...over,
	};
}

function policy(over: Partial<ExplorationPolicy>): ExplorationPolicy {
	return { ...DEFAULT_POLICY, ...over };
}

// t1: root(0.3) with children n1(0.5, has grandchild n2=0.7) and n3(0.4, terminal).
const T1_NODES: NodeRecord[] = [
	node({ id: "t1-n0", parentId: null, seq: 0, score: 0.3 }),
	node({ id: "t1-n1", parentId: "t1-n0", seq: 1, branch: 0, score: 0.5 }),
	node({ id: "t1-n2", parentId: "t1-n1", seq: 2, branch: 0, score: 0.7 }),
	node({ id: "t1-n3", parentId: "t1-n0", seq: 3, branch: 1, score: 0.4 }),
];
const T1: TreeRecord[] = [header("t1"), ...T1_NODES];

// t2: a single deep chain root -> n1 -> n2, to exercise root-after-exhaustion.
const T2: TreeRecord[] = [
	header("t2"),
	node({ id: "t2-n0", parentId: null, seq: 0, score: 0.2 }),
	node({ id: "t2-n1", parentId: "t2-n0", seq: 1, branch: 0, score: 0.6 }),
	node({ id: "t2-n2", parentId: "t2-n1", seq: 2, branch: 0, score: 0.8 }),
];

const BEST_FIRST = policy({ selectionRule: "best-first", stopRule: "never", batchSize: 2 });
const EXPLORE_ROOT_NEVER = policy({ selectionRule: "explore-root", stopRule: "never", batchSize: 1 });
const EXPLORE_ROOT_PATIENCE = policy({ selectionRule: "explore-root", stopRule: "patience", beta: 1, batchSize: 1 });

describe("simulatePolicy", () => {
	it("is deterministic and zero-cost (repeated runs are identical)", () => {
		const a = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 10 });
		const b = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 10 });
		expect(a).toEqual(b);
	});

	it("reveals the root's earliest-seq child first and a non-root leaf's recorded child", () => {
		const result = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 10 });
		// root's earliest-seq unrevealed child (n1, seq 1) is revealed before n3 (seq 3).
		expect(result.revealedIds).toEqual(["t1-n0", "t1-n1", "t1-n2", "t1-n3"]);
		// n2 is only reachable by probing the non-root leaf n1: reaching 0.7 proves it happened.
		expect(result.revealedIds).toContain("t1-n2");
		expect(result.bestScore).toBeCloseTo(0.7, 12);
		expect(result.N).toBe(3);
		expect(result.rounds).toBe(3);
	});

	it("counts a legally-selected exhausted cell as out of support and reveals nothing", () => {
		// Selecting the terminal leaf n2 alongside the root reveals nothing for n2.
		const result = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 10 });
		expect(result.outOfSupportRounds).toBe(1);
	});

	it("keeps probing an exhausted root out of support without revealing deeper nodes", () => {
		// explore-root only ever probes the root; once its lone child is revealed it
		// is out of support every round and never reaches the grandchild n2.
		const result = simulatePolicy(buildRecordedTree(T2), EXPLORE_ROOT_NEVER, { k2: 5 });
		expect(result.revealedIds).toEqual(["t2-n0", "t2-n1"]);
		expect(result.revealedIds).not.toContain("t2-n2");
		expect(result.outOfSupportRounds).toBeGreaterThan(0);
		expect(result.rounds).toBe(5);
	});

	it("stops at k2", () => {
		const result = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 1 });
		expect(result.rounds).toBe(1);
		expect(result.revealedIds).toEqual(["t1-n0", "t1-n1"]);
		expect(result.N).toBe(1);
	});

	it("stops once every recorded node is revealed", () => {
		const result = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 50 });
		expect(result.revealedIds).toHaveLength(4);
		expect(result.rounds).toBeLessThan(50);
	});

	it("gives different results for policies that differ only in the stop rule", () => {
		const patience = simulatePolicy(buildRecordedTree(T1), EXPLORE_ROOT_PATIENCE, { k2: 12 });
		const never = simulatePolicy(buildRecordedTree(T1), EXPLORE_ROOT_NEVER, { k2: 12 });
		expect(patience.rounds).toBe(2);
		expect(never.rounds).toBe(12);
		expect(patience).not.toEqual(never);
	});
});

describe("legality", () => {
	it("interpretPolicy returns an empty batch when there are no legal actions", () => {
		const emptyView: ObservationView = {
			maxParallelism: 4,
			round: 0,
			observed: () => [],
			legalActions: () => [],
			legalRoots: () => [],
			bestScore: () => 0,
			revealedNonRootCount: () => 0,
		};
		expect(interpretPolicy(DEFAULT_POLICY, emptyView)).toEqual([]);
	});

	it("never emits a batch containing both the root and one of its children", () => {
		const sim = createReplaySimulator(buildRecordedTree(T1));
		const rootCell = sim.view().legalActions()[0]!;
		sim.revealFor(rootCell); // reveal n1, a direct child of the root
		const view = sim.view();
		const legal = view.legalActions();
		expect(legal.map((cell) => cell.nodeId)).toEqual(["t1-n0", "t1-n1"]);

		const batch = interpretPolicy(BEST_FIRST, view);
		expect(batch.map((cell) => cell.nodeId)).toEqual(["t1-n1"]);
		expect(() => assertLegalBatch(view, batch)).not.toThrow();
		// The full legal set does contain the forbidden parent+child pair.
		expect(() => assertLegalBatch(view, legal)).toThrow(LegalBatchError);
	});

	it("every batch simulatePolicy emits passes the legality check", () => {
		// simulatePolicy calls assertLegalBatch internally; a clean run proves each
		// batch was distinct, within W, and free of any parent+child pair.
		expect(() => simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 50 })).not.toThrow();
		expect(() => simulatePolicy(buildRecordedTree(T1), EXPLORE_ROOT_NEVER, { k2: 50 })).not.toThrow();
	});
});

describe("store round-trip", () => {
	let dir: string;

	afterEach(() => {
		if (dir) rmSync(dir, { recursive: true, force: true });
	});

	it("readTree reproduces the in-memory tree and replay result byte-for-byte", () => {
		dir = mkdtempSync(join(tmpdir(), "dream-store-"));
		const writer = new TreeWriter("t1", dir);
		writer.writeHeader(header("t1"));
		for (const record of T1_NODES) writer.appendNode(record);
		writer.appendReveal({ type: "reveal", round: 0, ids: ["t1-n1"] });

		const fromDisk = readTree("t1", dir);
		expect(fromDisk.header.treeId).toBe("t1");
		expect(fromDisk.reveals).toHaveLength(1);
		expect(fromDisk.nodes.map((node) => node.id)).toEqual(["t1-n0", "t1-n1", "t1-n2", "t1-n3"]);

		const diskResult = simulatePolicy(fromDisk, BEST_FIRST, { k2: 10 });
		const memoryResult = simulatePolicy(buildRecordedTree(T1), BEST_FIRST, { k2: 10 });
		expect(diskResult).toEqual(memoryResult);
	});

	it("writeHeader is idempotent so a same-id re-run does not append duplicates", () => {
		dir = mkdtempSync(join(tmpdir(), "dream-store-"));
		const writer = new TreeWriter("t1", dir);
		writer.writeHeader(header("t1"));
		for (const record of T1_NODES) writer.appendNode(record);
		// A second run truncates and rewrites rather than doubling the file.
		const rerun = new TreeWriter("t1", dir);
		rerun.writeHeader(header("t1"));
		for (const record of T1_NODES) rerun.appendNode(record);
		expect(readTree("t1", dir).nodes).toHaveLength(4);
	});

	it("throws for a missing tree", () => {
		dir = mkdtempSync(join(tmpdir(), "dream-store-"));
		expect(() => readTree("absent", dir)).toThrow(DreamStoreError);
	});
});
