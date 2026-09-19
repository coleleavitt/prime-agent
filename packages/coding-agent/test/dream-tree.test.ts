import { describe, expect, it } from "vitest";
import { buildTreeFromRecords, createTree, nodeRecordOf } from "../src/core/dream/tree.js";
import {
	isNodeOrigin,
	NODE_ORIGINS,
	type NodeRecord,
	nodeOrigin,
	type TreeHeaderRecord,
	type TreeRecord,
} from "../src/core/dream/types.js";

function header(treeId = "t1"): TreeHeaderRecord {
	return {
		type: "tree",
		version: 1,
		treeId,
		taskId: "circle-packing",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	};
}

/** A LEGACY node line: written before provenance, so it carries no `origin`. */
function node(over: Partial<NodeRecord> & Pick<NodeRecord, "id" | "parentId" | "seq">): NodeRecord {
	return {
		type: "node",
		branch: 0,
		round: 0,
		score: 0,
		valid: true,
		artifactRef: "ref",
		tokens: 0,
		ts: 0,
		...over,
	};
}

describe("buildTreeFromRecords", () => {
	const records: TreeRecord[] = [
		header(),
		node({ id: "t1-n0", parentId: null, seq: 0, score: 0.3 }),
		node({ id: "t1-n1", parentId: "t1-n0", seq: 1, branch: 0, score: 0.5 }),
		node({ id: "t1-n2", parentId: "t1-n1", seq: 2, branch: 0, score: 0.7 }),
	];

	it("reconstructs parent/child structure in seq order", () => {
		const tree = buildTreeFromRecords(records);
		expect(tree.rootId).toBe("t1-n0");
		expect(tree.children("t1-n0").map((child) => child.id)).toEqual(["t1-n1"]);
		expect(tree.children("t1-n1").map((child) => child.id)).toEqual(["t1-n2"]);
		expect(tree.children("t1-n2")).toEqual([]);
	});

	it("exposes A(T) = {root} ∪ {leaves}", () => {
		const tree = buildTreeFromRecords(records);
		expect(tree.leaves().map((leaf) => leaf.id)).toEqual(["t1-n2"]);
		expect(tree.eligible().map((node) => node.id)).toEqual(["t1-n0", "t1-n2"]);
	});

	it("reports the best valid node and score", () => {
		const tree = buildTreeFromRecords(records);
		expect(tree.bestScore()).toBeCloseTo(0.7, 12);
		expect(tree.bestNode()?.id).toBe("t1-n2");
	});

	it("drops a node from A(T) once it gains a child", () => {
		const tree = buildTreeFromRecords(records);
		const added = tree.addNode({
			parentId: "t1-n2",
			round: 1,
			score: 0.9,
			valid: true,
			artifactRef: "ref",
			tokens: 0,
			ts: 1,
		});
		expect(added.id).toBe("t1-n3");
		expect(added.seq).toBe(3);
		expect(added.branch).toBe(0);
		expect(tree.leaves().map((leaf) => leaf.id)).toEqual(["t1-n3"]);
		expect(tree.eligible().map((node) => node.id)).toEqual(["t1-n0", "t1-n3"]);
		expect(tree.bestNode()?.id).toBe("t1-n3");
	});

	it("ignores only-root trees as their own leaf", () => {
		const tree = buildTreeFromRecords([header(), node({ id: "t1-n0", parentId: null, seq: 0, score: 0.1 })]);
		expect(tree.leaves().map((leaf) => leaf.id)).toEqual(["t1-n0"]);
		expect(tree.eligible().map((node) => node.id)).toEqual(["t1-n0"]);
	});

	it("throws when there is no single root", () => {
		expect(() =>
			buildTreeFromRecords([
				header(),
				node({ id: "t1-n0", parentId: null, seq: 0 }),
				node({ id: "t1-nX", parentId: null, seq: 1 }),
			]),
		).toThrow();
	});

	it("reads a legacy tree without origins as a root plus local nodes, and keeps recorded origins", () => {
		const legacy = buildTreeFromRecords(records);
		expect(legacy.allNodes().map((entry) => entry.origin)).toEqual(["root", "local", "local"]);
		expect(legacy.originCounts()).toEqual({ root: 1, local: 2, llm: 0 });

		const recorded = buildTreeFromRecords([
			header(),
			node({ id: "t1-n0", parentId: null, seq: 0, origin: "root" }),
			node({ id: "t1-n1", parentId: "t1-n0", seq: 1, origin: "llm" }),
			node({ id: "t1-n2", parentId: "t1-n0", seq: 2, branch: 1, origin: "local" }),
			node({ id: "t1-n3", parentId: "t1-n1", seq: 3, origin: "llm" }),
		]);
		expect(recorded.allNodes().map((entry) => entry.origin)).toEqual(["root", "llm", "local", "llm"]);
		expect(recorded.originCounts()).toEqual({ root: 1, local: 1, llm: 2 });
		// Round-tripping through records preserves every origin.
		expect(buildTreeFromRecords([header(), ...recorded.toNodeRecords()]).originCounts()).toEqual(
			recorded.originCounts(),
		);
	});
});

describe("createTree + addNode", () => {
	it("creates a root at seq 0 and assigns monotonic seq/id/branch", () => {
		const tree = createTree(header(), "root", 0.2);
		expect(tree.rootId).toBe("t1-n0");
		expect(tree.nodeById("t1-n0")?.score).toBeCloseTo(0.2, 12);
		expect(tree.nodeById("t1-n0")?.valid).toBe(true);

		const first = tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: 0.4,
			valid: true,
			artifactRef: "d1",
			tokens: 0,
			ts: 1,
		});
		const second = tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: 0.1,
			valid: true,
			artifactRef: "d2",
			tokens: 0,
			ts: 1,
		});
		expect([first.id, first.seq, first.branch]).toEqual(["t1-n1", 1, 0]);
		expect([second.id, second.seq, second.branch]).toEqual(["t1-n2", 2, 1]);
		expect(tree.children("t1-n0").map((child) => child.id)).toEqual(["t1-n1", "t1-n2"]);
		expect(tree.bestScore()).toBeCloseTo(0.4, 12);
		expect(tree.size).toBe(3);
	});

	it("coerces a non-finite score to 0", () => {
		const tree = createTree(header(), "root", 0.2);
		const added = tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: Number.POSITIVE_INFINITY,
			valid: false,
			artifactRef: "d1",
			tokens: 0,
			ts: 1,
		});
		expect(added.score).toBe(0);
	});

	it("marks the root, defaults an added node to local, and records an llm origin when given", () => {
		const tree = createTree(header(), "root", 0.2);
		expect(tree.nodeById(tree.rootId)?.origin).toBe("root");
		const local = tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: 0.3,
			valid: true,
			artifactRef: "a",
			tokens: 0,
			ts: 1,
		});
		const fallback = tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: 0.3,
			valid: true,
			origin: "local",
			artifactRef: "b",
			tokens: 900,
			ts: 1,
		});
		const agent = tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: 0.5,
			valid: true,
			origin: "llm",
			artifactRef: "c",
			tokens: 270,
			ts: 1,
		});
		expect([local.origin, fallback.origin, agent.origin]).toEqual(["local", "local", "llm"]);
		expect(tree.originCounts()).toEqual({ root: 1, local: 2, llm: 1 });
		// The tokens a rejected child spent stay on the fallback node, but its origin says the agent did not generate it.
		expect(fallback.tokens).toBe(900);
	});

	it("writes origin on every persisted node record, keeping the line scalar-only", () => {
		const tree = createTree(header(), "root", 0.2);
		tree.addNode({ parentId: "t1-n0", round: 1, score: 0.3, valid: true, artifactRef: "a", tokens: 0, ts: 1 });
		tree.addNode({
			parentId: "t1-n0",
			round: 1,
			score: 0.4,
			valid: false,
			failClass: "overlap",
			origin: "llm",
			artifactRef: "b",
			tokens: 5,
			ts: 1,
		});
		const records = tree.toNodeRecords();
		expect(records.map((record) => record.origin)).toEqual(["root", "local", "llm"]);
		expect(records[2]!.failClass).toBe("overlap");
		expect(records[0]).toEqual(nodeRecordOf(tree.nodeById("t1-n0")!));
		for (const record of records) {
			for (const [key, value] of Object.entries(record)) {
				const scalar = value === null || ["string", "number", "boolean"].includes(typeof value);
				expect(scalar, `node.${key} must be scalar`).toBe(true);
			}
		}
		expect(Object.keys(records[2]!)).toEqual([
			"type",
			"id",
			"parentId",
			"branch",
			"seq",
			"round",
			"score",
			"valid",
			"failClass",
			"origin",
			"artifactRef",
			"tokens",
			"ts",
		]);
	});
});

describe("nodeOrigin", () => {
	it("resolves a recorded origin, and the legacy default otherwise", () => {
		expect(NODE_ORIGINS).toEqual(["root", "local", "llm"]);
		expect(nodeOrigin({ parentId: null, origin: undefined })).toBe("root");
		expect(nodeOrigin({ parentId: "x", origin: undefined })).toBe("local");
		expect(nodeOrigin({ parentId: "x", origin: "llm" })).toBe("llm");
		expect(nodeOrigin({ parentId: "x", origin: "root" })).toBe("root");
		// An unknown value on a hand-edited line is ignored in favour of the default.
		expect(nodeOrigin({ parentId: "x", origin: "agent" as never })).toBe("local");
		expect(isNodeOrigin("llm")).toBe(true);
		expect(isNodeOrigin("agent")).toBe(false);
		expect(isNodeOrigin(undefined)).toBe(false);
	});
});
