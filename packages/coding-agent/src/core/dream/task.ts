/**
 * The pluggable scored objective the discovery tree grows over.
 *
 * A `ScoredTask` is pure and deterministic given the injected `SeededRng`: it
 * generates a root artifact, perturbs a parent into a child, and scores an
 * artifact by INDEPENDENTLY recomputing validity. A rollout over such a task
 * runs with zero model tokens and no network — the local proposer is just
 * `task.propose`.
 *
 * NOTE (lane coordination): this file holds the interface the Dream-RSI file
 * spec calls `scored-task.ts`. It lives at `task.ts` per the L-task lane
 * assignment; consumers import `ScoredTask`, `ProposeParams`, `Evaluation`,
 * `DreamTaskId` and `DreamFailClass` from `./task.js`.
 */

import type { SeededRng } from "./rng.js";

/** Identifier for a registered scored task. */
export type DreamTaskId = "circle-packing" | "sum-difference" | "python-speedup";

/**
 * Why an artifact scored invalid. Kept as a small open string so tasks can name
 * their own failure modes; the objective only ever gates on `Evaluation.valid`.
 */
export type DreamFailClass =
	| "invalid-shape"
	| "out-of-bounds"
	| "overlap"
	| "negative-radius"
	| "non-finite"
	| "too-small"
	| "degenerate"
	// python-speedup: correctness gate failure classes.
	| "incorrect"
	| "timeout"
	| "runtime-error";

/**
 * Knobs the exploration strategy projects onto a single generation attempt.
 * `stepScale` sizes the seeded perturbation; `refineDepth` is how many
 * perturbation candidates a single `propose` may draw before returning its
 * best; `branchWidth` is the raw strategy width the scale derives from.
 */
export interface ProposeParams {
	stepScale: number;
	refineDepth: number;
	branchWidth: number;
}

/** The outcome of scoring one artifact. `score` is always finite (0 when invalid). */
export interface Evaluation {
	valid: boolean;
	score: number;
	failClass?: DreamFailClass;
}

export interface ScoredTask<C> {
	readonly id: DreamTaskId;
	/** Generate the root artifact for a fresh tree. */
	root(rng: SeededRng): C;
	/**
	 * Produce a child artifact from `parent` (or from scratch when `parent` is
	 * null). Deterministic given `rng`.
	 */
	propose(parent: C | null, params: ProposeParams, rng: SeededRng, round: number): C;
	/** Recompute validity and score for an artifact, independent of how it was made. */
	evaluate(candidate: C): Evaluation;
	/** JSON-safe projection used for stable blob digests and persistence. */
	serialize(candidate: C): unknown;
	/** Parse and clamp a JSON value back into an artifact, validating its shape. */
	deserialize(value: unknown): C;
}
