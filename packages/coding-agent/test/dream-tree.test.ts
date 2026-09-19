import { describe, expect, it } from "vitest";
import { buildTreeFromRecords, createTree } from "../src/core/dream/tree.js";
import type { NodeRecord, TreeHeaderRecord, TreeRecord } from "../src/core/dream/types.js";

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
});
