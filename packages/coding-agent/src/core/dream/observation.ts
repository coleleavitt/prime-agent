/**
 * The decision interface both phases mirror, plus batch-legality enforcement.
 *
 * Online exploration and frozen replay both drive the SAME policy interpreter,
 * so they present the same view of the world: the revealed prefix, the legal
 * actions (roots plus open-branch frontiers), and a couple of cheap summaries.
 * The online driver builds this over a growing `DiscoveryTree`; replay builds
 * it over a `RecordedTree` plus a revealed-id set. Because the interpreter only
 * ever consumes an `ObservationView`, the identical policy JSON drives both.
 */

/** A candidate starting point for one new attempt. */
export interface Cell {
	nodeId: string;
	isRoot: boolean;
	parentId: string | null;
	score: number;
	valid: boolean;
}

/** A node in the revealed prefix. */
export interface RevealedNode {
	nodeId: string;
	parentId: string | null;
	isRoot: boolean;
	score: number;
	valid: boolean;
	seq: number;
	round: number;
}

export interface ObservationView {
	/** Max parallelism W: the largest batch the driver will accept. */
	readonly maxParallelism: number;
	/** The current decision round (0-based). */
	readonly round: number;
	/** The revealed prefix, in seq order. */
	observed(): RevealedNode[];
	/** Eligible cells this round: the root plus every open-branch frontier leaf. */
	legalActions(): Cell[];
	/** The subset of legal actions that are roots. */
	legalRoots(): Cell[];
	/** Best valid score seen in the revealed prefix (0 when nothing valid). */
	bestScore(): number;
	/** Number of revealed non-root nodes (probes spent). */
	revealedNonRootCount(): number;
}

export class LegalBatchError extends Error {}

/**
 * Defensive check the drivers run on every batch the interpreter emits: the
 * cells must be distinct, all currently legal, within W, and must never contain
 * both a node and its parent (the one such pair possible is the root and one of
 * the root's frontier leaf-children).
 */
export function assertLegalBatch(view: ObservationView, cells: readonly Cell[]): void {
	const chosen = new Set<string>();
	for (const cell of cells) {
		if (chosen.has(cell.nodeId)) {
			throw new LegalBatchError(`batch contains ${cell.nodeId} twice`);
		}
		chosen.add(cell.nodeId);
	}
	if (cells.length > view.maxParallelism) {
		throw new LegalBatchError(`batch of ${cells.length} exceeds max parallelism ${view.maxParallelism}`);
	}
	const legal = new Set(view.legalActions().map((cell) => cell.nodeId));
	for (const cell of cells) {
		if (!legal.has(cell.nodeId)) {
			throw new LegalBatchError(`cell ${cell.nodeId} is not a legal action`);
		}
	}
	for (const cell of cells) {
		if (cell.parentId !== null && chosen.has(cell.parentId)) {
			throw new LegalBatchError(`batch contains ${cell.nodeId} and its parent ${cell.parentId}`);
		}
	}
}
