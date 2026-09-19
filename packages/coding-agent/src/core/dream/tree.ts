/**
 * The pure in-memory discovery tree.
 *
 * The eligible set A(T) is {root} ∪ {leaves}. Because a node leaves A(T) the
 * moment it gains a child, every non-root node ends with at most one child while
 * the root accumulates many — exactly the shape the frozen replay simulator
 * relies on. This module has no IO: the online driver mutates a tree with
 * `addNode` and the store reconstructs one with `buildTreeFromRecords`.
 *
 * Every node carries its `origin` (`root` / `local` / `llm`), so a tree can say
 * how many of its candidates a child agent generated as opposed to the local
 * mutator standing in for a rejected child output. A node record without an
 * origin predates provenance and reads as `local` (`root` for the root).
 */

import type { DreamFailClass } from "./task.js";
import {
	type CandidateOrigin,
	isNodeRecord,
	isTreeHeaderRecord,
	type NodeOrigin,
	type NodeRecord,
	nodeOrigin,
	type TreeHeaderRecord,
	type TreeRecord,
} from "./types.js";

export interface DiscoveryNode {
	id: string;
	parentId: string | null;
	branch: number;
	seq: number;
	round: number;
	score: number;
	valid: boolean;
	failClass?: DreamFailClass;
	origin: NodeOrigin;
	artifactRef: string;
	tokens: number;
	ts: number;
}

/** Fields the caller supplies when appending a node; `seq`/`id`/`branch` are assigned by the tree. */
export interface NodeInput {
	parentId: string;
	round: number;
	score: number;
	valid: boolean;
	failClass?: DreamFailClass;
	/** Who generated the candidate; defaults to the local proposer. */
	origin?: CandidateOrigin;
	artifactRef: string;
	tokens: number;
	ts: number;
}

/** Node counts by origin over one tree; `root` is always 1 for a built tree. */
export type OriginCounts = Record<NodeOrigin, number>;

function finiteScore(score: number): number {
	return Number.isFinite(score) ? score : 0;
}

/** The persisted line for one node: scalar fields only, `origin` always written. */
export function nodeRecordOf(node: DiscoveryNode): NodeRecord {
	return {
		type: "node",
		id: node.id,
		parentId: node.parentId,
		branch: node.branch,
		seq: node.seq,
		round: node.round,
		score: node.score,
		valid: node.valid,
		...(node.failClass !== undefined ? { failClass: node.failClass } : {}),
		origin: node.origin,
		artifactRef: node.artifactRef,
		tokens: node.tokens,
		ts: node.ts,
	};
}

export class DiscoveryTree {
	readonly header: TreeHeaderRecord;
	readonly rootId: string;
	private readonly byId = new Map<string, DiscoveryNode>();
	private readonly childIds = new Map<string, string[]>();
	private nextSeq: number;

	constructor(header: TreeHeaderRecord, nodes: readonly DiscoveryNode[]) {
		this.header = header;
		const roots = nodes.filter((node) => node.parentId === null);
		if (roots.length !== 1) {
			throw new Error(`a discovery tree needs exactly one root, found ${roots.length}`);
		}
		this.rootId = roots[0]!.id;
		let maxSeq = -1;
		for (const node of [...nodes].sort((a, b) => a.seq - b.seq)) {
			this.byId.set(node.id, node);
			if (node.parentId !== null) {
				const siblings = this.childIds.get(node.parentId) ?? [];
				siblings.push(node.id);
				this.childIds.set(node.parentId, siblings);
			}
			maxSeq = Math.max(maxSeq, node.seq);
		}
		this.nextSeq = maxSeq + 1;
	}

	nodeById(id: string): DiscoveryNode | undefined {
		return this.byId.get(id);
	}

	/** Children of a node, in seq (creation) order. */
	children(id: string): DiscoveryNode[] {
		return (this.childIds.get(id) ?? []).map((childId) => this.byId.get(childId)!);
	}

	/** Nodes with zero children. */
	leaves(): DiscoveryNode[] {
		const out: DiscoveryNode[] = [];
		for (const node of this.byId.values()) {
			if ((this.childIds.get(node.id) ?? []).length === 0) out.push(node);
		}
		return out.sort((a, b) => a.seq - b.seq);
	}

	/** Eligible starting points A(T) = {root} ∪ {leaves}. */
	eligible(): DiscoveryNode[] {
		const seen = new Set<string>();
		const out: DiscoveryNode[] = [];
		const root = this.byId.get(this.rootId)!;
		out.push(root);
		seen.add(root.id);
		for (const leaf of this.leaves()) {
			if (!seen.has(leaf.id)) {
				out.push(leaf);
				seen.add(leaf.id);
			}
		}
		return out.sort((a, b) => a.seq - b.seq);
	}

	/** All nodes, in seq order. */
	allNodes(): DiscoveryNode[] {
		return [...this.byId.values()].sort((a, b) => a.seq - b.seq);
	}

	get size(): number {
		return this.byId.size;
	}

	/** Node counts by origin: how many candidates the agent generated versus the local mutator. */
	originCounts(): OriginCounts {
		const counts: OriginCounts = { root: 0, local: 0, llm: 0 };
		for (const node of this.byId.values()) counts[node.origin] += 1;
		return counts;
	}

	/** Max score over valid nodes; 0 when nothing valid. */
	bestScore(): number {
		let best = 0;
		let seen = false;
		for (const node of this.byId.values()) {
			if (node.valid && (!seen || node.score > best)) {
				best = node.score;
				seen = true;
			}
		}
		return seen ? best : 0;
	}

	/** The highest-scoring valid node, if any. */
	bestNode(): DiscoveryNode | undefined {
		let best: DiscoveryNode | undefined;
		for (const node of this.byId.values()) {
			if (node.valid && (best === undefined || node.score > best.score)) best = node;
		}
		return best;
	}

	/**
	 * Append a child of `input.parentId`, assigning a monotonic seq, the id
	 * `<treeId>-n<seq>`, and the next child-slot index. Returns the new node.
	 */
	addNode(input: NodeInput): DiscoveryNode {
		const parent = this.byId.get(input.parentId);
		if (!parent) throw new Error(`unknown parent ${input.parentId}`);
		const seq = this.nextSeq++;
		const siblings = this.childIds.get(input.parentId) ?? [];
		const node: DiscoveryNode = {
			id: `${this.header.treeId}-n${seq}`,
			parentId: input.parentId,
			branch: siblings.length,
			seq,
			round: input.round,
			score: finiteScore(input.score),
			valid: input.valid,
			failClass: input.failClass,
			origin: input.origin ?? "local",
			artifactRef: input.artifactRef,
			tokens: input.tokens,
			ts: input.ts,
		};
		this.byId.set(node.id, node);
		siblings.push(node.id);
		this.childIds.set(input.parentId, siblings);
		return node;
	}

	/** The node records for this tree, in seq order (for persistence). */
	toNodeRecords(): NodeRecord[] {
		return this.allNodes().map(nodeRecordOf);
	}
}

/**
 * Build a fresh tree with a single root node (seq 0). The root is treated as
 * valid by default; the online tasks always produce a valid root.
 */
export function createTree(
	header: TreeHeaderRecord,
	rootArtifactRef: string,
	rootScore: number,
	rootValid = true,
): DiscoveryTree {
	const root: DiscoveryNode = {
		id: `${header.treeId}-n0`,
		parentId: null,
		branch: 0,
		seq: 0,
		round: 0,
		score: finiteScore(rootScore),
		valid: rootValid,
		origin: "root",
		artifactRef: rootArtifactRef,
		tokens: 0,
		ts: header.createdTs,
	};
	return new DiscoveryTree(header, [root]);
}

/** Reconstruct a tree from persisted records; children come back in seq order. */
export function buildTreeFromRecords(records: readonly TreeRecord[]): DiscoveryTree {
	const header = records.find(isTreeHeaderRecord);
	if (!header) throw new Error("records have no tree header");
	const nodes: DiscoveryNode[] = records.filter(isNodeRecord).map((record) => ({
		id: record.id,
		parentId: record.parentId,
		branch: record.branch,
		seq: record.seq,
		round: record.round,
		score: finiteScore(record.score),
		valid: record.valid,
		failClass: record.failClass,
		origin: nodeOrigin(record),
		artifactRef: record.artifactRef,
		tokens: record.tokens,
		ts: record.ts,
	}));
	return new DiscoveryTree(header, nodes);
}
