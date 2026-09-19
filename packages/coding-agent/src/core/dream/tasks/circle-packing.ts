/**
 * Circle packing in the unit square (Zheng et al. 2026, Appendix A).
 *
 * Choose `n` centers in [0,1]^2 and radii r_i >= 0 so every circle is inside
 * the square and no two overlap; maximize the sum of radii. The score of a
 * valid packing is sum_i r_i, and an invalid one scores 0.
 *
 * The root is a jittered grid; a proposal perturbs the centers by a seeded
 * gaussian and then assigns radii by a closed-form FEASIBLE repair:
 *   r_i = 0.5 * min(boundaryDist_i, min_{j != i} dist(i, j)),
 * which guarantees r_i <= boundaryDist_i (inside the square) and
 * r_i + r_j <= dist(i, j) (non-overlapping). The repair is feasible, not
 * maximal, so there is always headroom for the search to improve.
 */

import type { SeededRng } from "../rng.js";
import type { DreamFailClass, Evaluation, ScoredTask } from "../task.js";

export interface CirclePackingArtifact {
	n: number;
	xs: number[];
	ys: number[];
	rs: number[];
}

/** Tolerance for the independent validity recomputation in `evaluate`. */
export const CIRCLE_PACKING_EPS = 1e-9;

/**
 * How far, in grid-cell widths, the root centers are jittered off the grid.
 * Well above 1 so the root is a genuinely loose packing with headroom for the
 * search to improve, while still being a (heavily) jittered grid.
 */
const ROOT_JITTER = 1.8;

function gridShape(n: number): { cols: number; rows: number } {
	const cols = Math.max(1, Math.ceil(Math.sqrt(n)));
	const rows = Math.max(1, Math.ceil(n / cols));
	return { cols, rows };
}

function clampUnit(value: number): number {
	if (value < 0) return 0;
	if (value > 1) return 1;
	return value;
}

/** Assign feasible radii for the given centers by the closed-form repair. */
function repairRadii(xs: number[], ys: number[]): number[] {
	const n = xs.length;
	const rs = new Array<number>(n);
	for (let i = 0; i < n; i++) {
		const xi = xs[i]!;
		const yi = ys[i]!;
		const boundary = Math.min(xi, 1 - xi, yi, 1 - yi);
		let nearest = Number.POSITIVE_INFINITY;
		for (let j = 0; j < n; j++) {
			if (j === i) continue;
			const dx = xi - xs[j]!;
			const dy = yi - ys[j]!;
			const dist = Math.sqrt(dx * dx + dy * dy);
			if (dist < nearest) nearest = dist;
		}
		const limit = Number.isFinite(nearest) ? Math.min(boundary, nearest) : boundary;
		rs[i] = Math.max(0, 0.5 * limit);
	}
	return rs;
}

function rootArtifact(n: number, rng: SeededRng): CirclePackingArtifact {
	const { cols, rows } = gridShape(n);
	const cellW = 1 / cols;
	const cellH = 1 / rows;
	const xs = new Array<number>(n);
	const ys = new Array<number>(n);
	for (let i = 0; i < n; i++) {
		const col = i % cols;
		const row = Math.floor(i / cols);
		const jitterX = (rng.next() - 0.5) * ROOT_JITTER * cellW;
		const jitterY = (rng.next() - 0.5) * ROOT_JITTER * cellH;
		xs[i] = clampUnit((col + 0.5) * cellW + jitterX);
		ys[i] = clampUnit((row + 0.5) * cellH + jitterY);
	}
	return { n, xs, ys, rs: repairRadii(xs, ys) };
}

/**
 * Perturb a small random subset of centers by a seeded gaussian scaled by
 * `stepScale`, then re-assign feasible radii. A local move (rather than jostling
 * every circle at once) is what lets hill-climbing separate the crowded circles
 * that bind the score, so the search reliably improves over a loose root.
 */
function perturb(
	parent: CirclePackingArtifact,
	stepScale: number,
	moveCount: number,
	rng: SeededRng,
): CirclePackingArtifact {
	const n = parent.n;
	const xs = [...parent.xs];
	const ys = [...parent.ys];
	const count = Math.max(1, Math.min(n, moveCount));
	for (let move = 0; move < count; move++) {
		const i = rng.nextInt(n);
		xs[i] = clampUnit(xs[i]! + rng.nextGaussian() * stepScale);
		ys[i] = clampUnit(ys[i]! + rng.nextGaussian() * stepScale);
	}
	return { n, xs, ys, rs: repairRadii(xs, ys) };
}

function scoreOf(artifact: CirclePackingArtifact): Evaluation {
	return evaluateArtifact(artifact);
}

function evaluateArtifact(artifact: CirclePackingArtifact): Evaluation {
	const { n, xs, ys, rs } = artifact;
	if (!Number.isInteger(n) || n < 1 || xs.length !== n || ys.length !== n || rs.length !== n) {
		return invalid("invalid-shape");
	}
	for (let i = 0; i < n; i++) {
		if (!Number.isFinite(xs[i]) || !Number.isFinite(ys[i]) || !Number.isFinite(rs[i])) {
			return invalid("non-finite");
		}
	}
	const eps = CIRCLE_PACKING_EPS;
	for (let i = 0; i < n; i++) {
		const r = rs[i]!;
		if (r < -eps) return invalid("negative-radius");
		const x = xs[i]!;
		const y = ys[i]!;
		if (x - r < -eps || x + r > 1 + eps || y - r < -eps || y + r > 1 + eps) {
			return invalid("out-of-bounds");
		}
	}
	for (let i = 0; i < n; i++) {
		for (let j = i + 1; j < n; j++) {
			const dx = xs[i]! - xs[j]!;
			const dy = ys[i]! - ys[j]!;
			const dist = Math.sqrt(dx * dx + dy * dy);
			if (rs[i]! + rs[j]! > dist + eps) return invalid("overlap");
		}
	}
	let sum = 0;
	for (let i = 0; i < n; i++) sum += rs[i]!;
	if (!Number.isFinite(sum)) return invalid("non-finite");
	return { valid: true, score: sum };
}

function invalid(failClass: DreamFailClass): Evaluation {
	return { valid: false, score: 0, failClass };
}

function toNumberArray(value: unknown, length: number, transform: (raw: number) => number): number[] {
	if (!Array.isArray(value) || value.length !== length) {
		throw new TypeError("circle-packing artifact array has the wrong shape");
	}
	return value.map((raw) => {
		if (typeof raw !== "number" || !Number.isFinite(raw)) {
			throw new TypeError("circle-packing artifact contains a non-finite value");
		}
		return transform(raw);
	});
}

export function createCirclePackingTask(n: number): ScoredTask<CirclePackingArtifact> {
	if (!Number.isInteger(n) || n < 1) {
		throw new RangeError("circle-packing requires an integer n >= 1");
	}
	return {
		id: "circle-packing",
		root(rng) {
			return rootArtifact(n, rng);
		},
		propose(parent, params, rng, _round) {
			const source = parent ?? rootArtifact(n, rng);
			const depth = Math.max(1, Math.trunc(params.refineDepth));
			const moveCount = Math.max(1, Math.round(params.branchWidth));
			let best: CirclePackingArtifact | undefined;
			let bestScore = Number.NEGATIVE_INFINITY;
			for (let attempt = 0; attempt < depth; attempt++) {
				const candidate = perturb(source, params.stepScale, moveCount, rng);
				const evaluation = scoreOf(candidate);
				const value = evaluation.valid ? evaluation.score : Number.NEGATIVE_INFINITY;
				if (best === undefined || value > bestScore) {
					best = candidate;
					bestScore = value;
				}
			}
			return best ?? perturb(source, params.stepScale, moveCount, rng);
		},
		evaluate(candidate) {
			return evaluateArtifact(candidate);
		},
		serialize(candidate) {
			return { n: candidate.n, xs: [...candidate.xs], ys: [...candidate.ys], rs: [...candidate.rs] };
		},
		deserialize(value) {
			if (typeof value !== "object" || value === null) {
				throw new TypeError("circle-packing artifact must be an object");
			}
			const record = value as Record<string, unknown>;
			const count = record.n;
			if (typeof count !== "number" || !Number.isInteger(count) || count < 1) {
				throw new TypeError("circle-packing artifact has an invalid n");
			}
			const xs = toNumberArray(record.xs, count, clampUnit);
			const ys = toNumberArray(record.ys, count, clampUnit);
			const rs = toNumberArray(record.rs, count, (raw) => Math.max(0, raw));
			return { n: count, xs, ys, rs };
		},
	};
}
