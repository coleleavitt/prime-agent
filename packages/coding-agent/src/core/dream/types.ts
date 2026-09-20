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

import type { ExplorationPolicy } from "./policy.js";
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

/** Who produced a dreaming step's candidate set: a child agent, the local mutator, or both. */
export type DreamerKind = "llm" | "local" | "mixed";

/**
 * Why a dreaming candidate was or was not chosen, relative to the policy the
 * step chose. `winner`: chosen and a strict improvement. `tie`: eligible, tied
 * the chosen value, lost the tie-break. `worse`: eligible, fully in support,
 * below the chosen value. `quality-rejected`: fully in support, failed the
 * quality guard. `unmeasurable`: differs from current only in replay-dead
 * fields (`REPLAY_DEAD_FIELDS`), or selected an out-of-support cell on some
 * measured tree (its replay is biased and says nothing about it), or no tree was
 * measurable at all (the current policy is off support on every tree), and was
 * not the winner.
 * `identical`: the current policy's own id. `duplicate`: the same id as an
 * earlier candidate (`duplicateOf`). Identical and duplicate candidates are
 * simulated at most once and never enter the argmax.
 */
export const CANDIDATE_REASONS = [
	"winner",
	"tie",
	"worse",
	"quality-rejected",
	"unmeasurable",
	"identical",
	"duplicate",
] as const;
export type CandidateReason = (typeof CANDIDATE_REASONS)[number];

export function isCandidateReason(value: unknown): value is CandidateReason {
	return typeof value === "string" && (CANDIDATE_REASONS as readonly string[]).includes(value);
}

/**
 * One scored candidate of a dreaming step; the means of its replay terms over
 * the MEASURED pool (the trees the current policy replays in full support) plus
 * the verdict.
 */
export interface CandidateVerdict {
	/** Position in the candidate list handed to the selection. */
	index: number;
	policyId: string;
	policy: ExplorationPolicy;
	origin: CandidateOrigin;
	/** Fields differing from the current policy, in schema order. */
	changed: string[];
	/** Index of the earlier candidate this one duplicates, else null. */
	duplicateOf: number | null;
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
	inSupportMean: number;
	inSupportMin: number;
	/** Passed the quality guard and entered the argmax (never for identical/duplicate). */
	eligible: boolean;
	reason: CandidateReason;
}

/**
 * The lever scan of a dreaming step: a fixed deterministic grid of local
 * policies scored on the same pool under the same rule, independent of what the
 * dreamer proposed. `gap` is how far the best eligible grid policy beats the
 * current one (0 when nothing does), so an inert step can be labelled "no lever
 * on this pool" rather than "the dreamer proposed nothing better".
 */
export interface LeverScanRecord {
	/** Distinct grid policies scored (the current one included). */
	policies: number;
	/** Grid policies that passed the quality guard. */
	eligible: number;
	bestValue: number;
	bestPolicyId: string;
	gap: number;
	/** `simulatePolicy` calls the scan made (the current policy on every tree, then each grid policy on the measured trees). */
	simulations: number;
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
