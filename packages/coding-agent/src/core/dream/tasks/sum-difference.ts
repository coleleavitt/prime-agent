/**
 * Sum-difference (Zheng et al. 2026, Appendix A).
 *
 * For a finite set A of integers, maximize
 *   Gamma(A) = log(|A+A| / |A|) / log(|A-A| / |A|),
 * where A+A = { a + b : a, b in A } and A-A = { a - b : a, b in A }.
 *
 * The representation is a sorted set of distinct integers within a bounded
 * window; a proposal is a seeded add / remove / replace edit that keeps at
 * least two distinct integers. `evaluate` recomputes the sumset and difference
 * set independently and returns a finite Gamma only when it is well defined.
 */

import type { SeededRng } from "../rng.js";
import type { DreamFailClass, Evaluation, ScoredTask } from "../task.js";

export interface SumDifferenceArtifact {
	set: number[];
}

/** Integers are drawn from [-WINDOW, WINDOW]. */
const WINDOW = 40;
const WINDOW_SPAN = 2 * WINDOW + 1;
const INITIAL_SIZE = 8;
const MIN_SIZE = 2;

function windowValue(rng: SeededRng): number {
	return rng.nextInt(WINDOW_SPAN) - WINDOW;
}

function sortedDistinct(values: Iterable<number>): number[] {
	return [...new Set(values)].sort((a, b) => a - b);
}

function rootArtifact(rng: SeededRng): SumDifferenceArtifact {
	const values = new Set<number>();
	let guard = 0;
	while (values.size < INITIAL_SIZE && guard < INITIAL_SIZE * 16) {
		values.add(windowValue(rng));
		guard++;
	}
	while (values.size < MIN_SIZE) values.add(values.size);
	return { set: sortedDistinct(values) };
}

function addValue(current: Set<number>, rng: SeededRng): void {
	for (let attempt = 0; attempt < 16; attempt++) {
		const value = windowValue(rng);
		if (!current.has(value)) {
			current.add(value);
			return;
		}
	}
}

function removeValue(current: Set<number>, rng: SeededRng): void {
	if (current.size <= MIN_SIZE) return;
	const values = [...current];
	current.delete(values[rng.nextInt(values.length)]!);
}

function proposeEdit(parent: SumDifferenceArtifact, rng: SeededRng): SumDifferenceArtifact {
	const current = new Set(parent.set);
	const operation = rng.nextInt(3);
	if (operation === 0) {
		addValue(current, rng);
	} else if (operation === 1) {
		removeValue(current, rng);
	} else {
		removeValue(current, rng);
		addValue(current, rng);
	}
	while (current.size < MIN_SIZE) addValue(current, rng);
	return { set: sortedDistinct(current) };
}

function distinctSize(values: number[]): number {
	return new Set(values).size;
}

function evaluateArtifact(artifact: SumDifferenceArtifact): Evaluation {
	const { set } = artifact;
	if (!Array.isArray(set)) return invalid("invalid-shape");
	for (const value of set) {
		if (typeof value !== "number" || !Number.isInteger(value)) return invalid("invalid-shape");
	}
	const size = distinctSize(set);
	if (size < MIN_SIZE) return invalid("degenerate");
	const sums = new Set<number>();
	const diffs = new Set<number>();
	for (const a of set) {
		for (const b of set) {
			sums.add(a + b);
			diffs.add(a - b);
		}
	}
	const numerator = Math.log(sums.size / size);
	const denominator = Math.log(diffs.size / size);
	if (!Number.isFinite(denominator) || denominator === 0) return invalid("degenerate");
	const gamma = numerator / denominator;
	if (!Number.isFinite(gamma)) return invalid("non-finite");
	return { valid: true, score: gamma };
}

function invalid(failClass: DreamFailClass): Evaluation {
	return { valid: false, score: 0, failClass };
}

export function createSumDifferenceTask(): ScoredTask<SumDifferenceArtifact> {
	return {
		id: "sum-difference",
		root(rng) {
			return rootArtifact(rng);
		},
		propose(parent, params, rng, _round) {
			const source = parent ?? rootArtifact(rng);
			const depth = Math.max(1, Math.trunc(params.refineDepth));
			let best: SumDifferenceArtifact | undefined;
			let bestScore = Number.NEGATIVE_INFINITY;
			for (let attempt = 0; attempt < depth; attempt++) {
				const candidate = proposeEdit(source, rng);
				const evaluation = evaluateArtifact(candidate);
				const value = evaluation.valid ? evaluation.score : Number.NEGATIVE_INFINITY;
				if (best === undefined || value > bestScore) {
					best = candidate;
					bestScore = value;
				}
			}
			return best ?? proposeEdit(source, rng);
		},
		evaluate(candidate) {
			return evaluateArtifact(candidate);
		},
		serialize(candidate) {
			return { set: [...candidate.set] };
		},
		deserialize(value) {
			if (typeof value !== "object" || value === null) {
				throw new TypeError("sum-difference artifact must be an object");
			}
			const raw = (value as Record<string, unknown>).set;
			if (!Array.isArray(raw)) throw new TypeError("sum-difference artifact must have a set array");
			const integers = raw.map((entry) => {
				if (typeof entry !== "number" || !Number.isFinite(entry)) {
					throw new TypeError("sum-difference set contains a non-finite value");
				}
				return Math.trunc(entry);
			});
			return { set: sortedDistinct(integers) };
		},
	};
}
