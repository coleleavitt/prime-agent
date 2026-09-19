/**
 * JSON-safe wire and record types for the Dream-RSI subsystem.
 *
 * A discovery tree persists as a JSONL file: one header line, then one node
 * line per generation+evaluation attempt, then one reveal line per online
 * round. Every field on a node line is a scalar (string, number, boolean) so a
 * line stays cheap to parse and never smuggles an artifact into the log; the
 * full artifact lives in a sibling blob keyed by `seq`.
 *
 * `DreamFailClass` is owned by `./task.js` (the scored-task lane): a node line
 * only ever records it, so this module imports the type and does not redefine
 * it, keeping a single source of truth for failure classes.
 */

import type { DreamFailClass } from "./task.js";

/** Injected monotonic wall clock (milliseconds). Never `Date.now` inside the core. */
export type DreamClock = () => number;

/** Whether a run's proposer/dreamer are the local zero-token path or the LLM path. */
export type DreamMode = "local" | "llm";

/**
 * Who generated a node's artifact: the task's seeded `root`, the `local`
 * zero-token proposer, or a child agent (`llm`). On the LLM-proposer path a
 * `local` non-root node is a FALLBACK — the child's output was rejected and the
 * local mutator stood in — so counting `llm` nodes gives the agent-generated
 * candidates, never the handler calls.
 */
export const NODE_ORIGINS = ["root", "local", "llm"] as const;
export type NodeOrigin = (typeof NODE_ORIGINS)[number];

/** The origin a proposed (non-root) candidate can carry. */
export type CandidateOrigin = Exclude<NodeOrigin, "root">;

export function isNodeOrigin(value: unknown): value is NodeOrigin {
	return typeof value === "string" && (NODE_ORIGINS as readonly string[]).includes(value);
}

/** Line 0 of a tree file: the run metadata shared by every node in the tree. */
export interface TreeHeaderRecord {
	type: "tree";
	version: 1;
	treeId: string;
	taskId: string;
	/** Task size parameter (e.g. circle count), when the task takes one. */
	n?: number;
	/** Max parallelism W the rollout used. */
	w: number;
	seed: number | string;
	policyId: string;
	iteration: number;
	createdTs: number;
}

/**
 * One node of the discovery tree. `seq` is monotonic within a tree and the id
 * is `<treeId>-n<seq>`; the root is `seq` 0 with `parentId` null. `artifactRef`
 * is the blob digest (or a marker for the root) — the artifact itself is never
 * inlined here.
 */
export interface NodeRecord {
	type: "node";
	id: string;
	parentId: string | null;
	/** Child-slot index within the parent (0-based), in creation order. */
	branch: number;
	seq: number;
	round: number;
	/** Finite; 0 when invalid. Never Infinity/NaN (JSON would turn those into null). */
	score: number;
	valid: boolean;
	failClass?: DreamFailClass;
	/**
	 * Provenance. Every line written since provenance was added carries it; a
	 * line without it predates provenance and reads through `nodeOrigin` as
	 * `root` for the root and `local` for every other node.
	 */
	origin?: NodeOrigin;
	artifactRef: string;
	tokens: number;
	ts: number;
}

/** A node line's origin with the legacy default: `root` for the root, `local` for every other node. */
export function nodeOrigin(record: Pick<NodeRecord, "parentId" | "origin">): NodeOrigin {
	if (isNodeOrigin(record.origin)) return record.origin;
	return record.parentId === null ? "root" : "local";
}

/** One online round's reveal set: informational for `show`; replay does not read it. */
export interface RevealRecord {
	type: "reveal";
	round: number;
	ids: string[];
}

export type TreeRecord = TreeHeaderRecord | NodeRecord | RevealRecord;

export function isTreeHeaderRecord(record: TreeRecord): record is TreeHeaderRecord {
	return record.type === "tree";
}

export function isNodeRecord(record: TreeRecord): record is NodeRecord {
	return record.type === "node";
}

export function isRevealRecord(record: TreeRecord): record is RevealRecord {
	return record.type === "reveal";
}
