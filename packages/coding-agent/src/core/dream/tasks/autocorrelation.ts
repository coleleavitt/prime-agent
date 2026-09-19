/**
 * First autocorrelation inequality (Zheng et al. 2026, Appendix A).
 *
 * Find a non-negative integrable f supported on [-1/4, 1/4] with integral 1 that
 * MINIMIZES the peak of its autoconvolution,
 *   peak(f) = max_{t in [-1/2, 1/2]} (f * f)(t),   (f * f)(t) = int f(x) f(t - x) dx.
 * Dream-RSI maximizes, so the score of a valid candidate is `1 / peak` (higher is
 * better, always finite and positive) and an invalid one scores 0.
 *
 * REPRESENTATION. A step function with `n` bins of width h = 1/(2n) over
 * [-1/4, 1/4]: bin i covers [-1/4 + i h, -1/4 + (i+1) h) and carries a
 * non-negative weight w_i (the density on that bin). The integral is
 * sum_i w_i * h, and `evaluate` normalizes it to 1 before scoring, so a
 * candidate is judged by its SHAPE alone; an all-zero vector has no shape and is
 * invalid. The paper studies n in {32, 64, 128}; the default is 64.
 *
 * EXACT AUTOCONVOLUTION. The convolution of two width-h indicator functions is a
 * tent of height h and half-width h centered at the sum of their left edges plus
 * h, so (f * f) is a sum of tents and therefore piecewise linear with knots at
 *   t_m = -1/2 + m h,   m = 0 .. 2n,
 * where it is 0 at m = 0 and m = 2n. At an interior knot t_m only the tents
 * centered exactly there are non-zero (a tent centered at t_k vanishes at the
 * neighbouring knots t_k +- h), and the tents centered at t_m are the pairs
 * (i, j) with i + j = m - 1, each contributing w_i w_j h. Hence
 *   (f * f)(t_m) = h * c_{m-1},   c_k = sum_{i + j = k} w_i w_j,   k = 0 .. 2n - 2,
 * i.e. h times the discrete autoconvolution of the weight vector. A piecewise
 * linear function attains its maximum at a knot, so
 *   peak(f) = h * max_k c_k
 * EXACTLY (up to floating point), with no sampling or quadrature error.
 *
 * BASELINE. The root is the uniform density f = 2 on [-1/4, 1/4] (support of
 * length 1/2, integral 1). Its autoconvolution is the triangle of height
 * (f * f)(0) = int f(x) f(-x) dx = 4 * (1/2) = 2, so peak = 2 and score = 0.5. In
 * the discrete form: w_i = 2, c_{n-1} = 4n pairs of 4 -> h * 4n = 2.
 *
 * REPORTED RECORDS (for comparison; never encoded as targets). Matolcsi and
 * Vinuesa reached a peak of about 1.5098 with a 208-step function; AlphaEvolve
 * reported about 1.5053; SimpleTES reports 1.453675 (score about 0.688). The
 * peak is bounded below by about 1.28 (Cloninger and Steinerberger), so every
 * score is below 0.782.
 *
 * The local proposer is a seeded mutation of the weight vector (perturb a few
 * bins, move mass between adjacent bins, occasionally smooth or sharpen a
 * window), clamped to non-negative and re-normalized, so every proposal is
 * valid. `evaluate` recomputes validity and the peak independently of how the
 * candidate was made.
 */

import type { SeededRng } from "../rng.js";
import type { DreamFailClass, Evaluation, ProposeParams, ScoredTask } from "../task.js";

export interface AutocorrelationArtifact {
	n: number;
	weights: number[];
}

/** The bin counts the paper studies; the default is the middle one. */
export const AUTOCORRELATION_BIN_COUNTS = [32, 64, 128] as const;
export const DEFAULT_AUTOCORRELATION_N = 64;

/** Half-width of the support [-1/4, 1/4]; the support length is 2 * this = 1/2. */
export const SUPPORT_HALF_WIDTH = 0.25;
/** The uniform density with integral 1 on a support of length 1/2. */
export const UNIFORM_DENSITY = 2;
/** The uniform root's autoconvolution peak, (f * f)(0) = 4 * (1/2). */
export const UNIFORM_ROOT_PEAK = 2;

/** Window radii (in bins) a smoothing or sharpening move may pick from: 1 .. this. */
const MAX_WINDOW_RADIUS = 3;

export function isAutocorrelationBinCount(value: unknown): value is (typeof AUTOCORRELATION_BIN_COUNTS)[number] {
	return typeof value === "number" && (AUTOCORRELATION_BIN_COUNTS as readonly number[]).includes(value);
}

/** Bin width h = (support length) / n = 1 / (2n). */
export function binWidth(n: number): number {
	return (2 * SUPPORT_HALF_WIDTH) / n;
}

/**
 * The values of (f * f) at the 2n - 1 interior knots t_m = -1/2 + m h for
 * m = 1 .. 2n - 1: entry k is h * c_k with c_k the discrete autoconvolution of
 * `weights` at lag k. The knots at m = 0 and m = 2n carry 0 and are omitted.
 */
export function autoconvolutionKnots(weights: readonly number[]): number[] {
	const n = weights.length;
	const h = binWidth(n);
	const knots = new Array<number>(Math.max(0, 2 * n - 1)).fill(0);
	for (let i = 0; i < n; i++) {
		const wi = weights[i]!;
		if (wi === 0) continue;
		for (let j = 0; j < n; j++) {
			knots[i + j]! += wi * weights[j]!;
		}
	}
	for (let k = 0; k < knots.length; k++) knots[k] = knots[k]! * h;
	return knots;
}

/** max_t (f * f)(t): the largest knot value, since a piecewise-linear function peaks at a knot. */
export function autoconvolutionPeak(weights: readonly number[]): number {
	let peak = 0;
	for (const value of autoconvolutionKnots(weights)) if (value > peak) peak = value;
	return peak;
}

/**
 * Scale `weights` so sum_i w_i * h = 1. Returns null when the integral is not a
 * positive finite number (all-zero, or a non-finite entry), which is what makes
 * a candidate `degenerate`.
 */
export function normalizeWeights(weights: readonly number[]): number[] | null {
	const h = binWidth(weights.length);
	let sum = 0;
	for (const value of weights) sum += value;
	const integral = sum * h;
	if (!Number.isFinite(integral) || integral <= 0) return null;
	return weights.map((value) => value / integral);
}

function uniformWeights(n: number): number[] {
	return new Array<number>(n).fill(UNIFORM_DENSITY);
}

function rootArtifact(n: number): AutocorrelationArtifact {
	return { n, weights: uniformWeights(n) };
}

function evaluateArtifact(artifact: AutocorrelationArtifact, n: number): Evaluation {
	if (
		typeof artifact !== "object" ||
		artifact === null ||
		artifact.n !== n ||
		!Array.isArray(artifact.weights) ||
		artifact.weights.length !== n
	) {
		return invalid("invalid-shape");
	}
	for (const value of artifact.weights) {
		if (typeof value !== "number" || !Number.isFinite(value)) return invalid("non-finite");
	}
	for (const value of artifact.weights) {
		if (value < 0) return invalid("negative-weight");
	}
	const normalized = normalizeWeights(artifact.weights);
	if (normalized === null) return invalid("degenerate");
	const peak = autoconvolutionPeak(normalized);
	if (!Number.isFinite(peak) || peak <= 0) return invalid("non-finite");
	const score = 1 / peak;
	if (!Number.isFinite(score)) return invalid("non-finite");
	return { valid: true, score };
}

function invalid(failClass: DreamFailClass): Evaluation {
	return { valid: false, score: 0, failClass };
}

/** Add seeded gaussian noise, sized by `stepScale` times the mean density, to `count` random bins. */
function perturbBins(weights: number[], stepScale: number, count: number, rng: SeededRng): void {
	const n = weights.length;
	for (let move = 0; move < count; move++) {
		const i = rng.nextInt(n);
		weights[i] = Math.max(0, weights[i]! + rng.nextGaussian() * stepScale * UNIFORM_DENSITY);
	}
}

/** Move a seeded fraction (at most `stepScale`) of one bin's mass into an adjacent bin, `count` times. */
function moveMass(weights: number[], stepScale: number, count: number, rng: SeededRng): void {
	const n = weights.length;
	for (let move = 0; move < count; move++) {
		const i = rng.nextInt(n);
		const direction = rng.nextInt(2) === 0 ? -1 : 1;
		const j = i + direction >= 0 && i + direction < n ? i + direction : i - direction;
		const delta = Math.min(1, stepScale) * rng.next() * weights[i]!;
		weights[i] = weights[i]! - delta;
		weights[j] = weights[j]! + delta;
	}
}

/**
 * Blend a random window toward its own average (smoothing) or away from it
 * (sharpening, clamped at 0). Smoothing flattens a plateau; sharpening carves
 * the edges of one.
 */
function smoothOrSharpen(weights: number[], stepScale: number, rng: SeededRng): void {
	const n = weights.length;
	const center = rng.nextInt(n);
	const radius = 1 + rng.nextInt(MAX_WINDOW_RADIUS);
	const sign = rng.nextInt(2) === 0 ? 1 : -1;
	const alpha = Math.min(1, stepScale);
	const lo = Math.max(0, center - radius);
	const hi = Math.min(n - 1, center + radius);
	let sum = 0;
	for (let k = lo; k <= hi; k++) sum += weights[k]!;
	const average = sum / (hi - lo + 1);
	for (let k = lo; k <= hi; k++) {
		weights[k] = Math.max(0, weights[k]! + sign * alpha * (average - weights[k]!));
	}
}

/**
 * One seeded local mutation of `parent`, always returned normalized. The parent
 * is normalized first so the move sizes are relative to a fixed mass; a mutation
 * that destroys all mass (only possible through clamping) falls back to the
 * normalized parent itself.
 */
function mutate(
	parent: AutocorrelationArtifact,
	stepScale: number,
	moveCount: number,
	rng: SeededRng,
): AutocorrelationArtifact {
	const n = parent.n;
	const base = normalizeWeights(parent.weights) ?? uniformWeights(n);
	const weights = [...base];
	const operation = rng.nextInt(6);
	if (operation < 3) {
		perturbBins(weights, stepScale, moveCount, rng);
	} else if (operation < 5) {
		moveMass(weights, stepScale, moveCount, rng);
	} else {
		smoothOrSharpen(weights, stepScale, rng);
	}
	const normalized = normalizeWeights(weights);
	return { n, weights: normalized ?? base };
}

function proposeCandidate(
	source: AutocorrelationArtifact,
	params: ProposeParams,
	rng: SeededRng,
	n: number,
): AutocorrelationArtifact {
	const depth = Math.max(1, Math.trunc(params.refineDepth));
	const moveCount = Math.max(1, Math.round(params.branchWidth));
	let best: AutocorrelationArtifact | undefined;
	let bestScore = Number.NEGATIVE_INFINITY;
	for (let attempt = 0; attempt < depth; attempt++) {
		const candidate = mutate(source, params.stepScale, moveCount, rng);
		const evaluation = evaluateArtifact(candidate, n);
		const value = evaluation.valid ? evaluation.score : Number.NEGATIVE_INFINITY;
		if (best === undefined || value > bestScore) {
			best = candidate;
			bestScore = value;
		}
	}
	return best ?? mutate(source, params.stepScale, moveCount, rng);
}

/** The concrete shape example every autocorrelation contract carries: a parseable object, small enough to read at a glance. */
export const AUTOCORRELATION_SHAPE_EXAMPLE = '{"n": 4, "weights": [1.5, 2.5, 2.5, 1.5]}';

/**
 * Public contract handed to an LLM proposer. Everything in it is derivable from
 * the problem statement; the candidate's own peak travels in its serialized form
 * (`peak`), so the proposer sees what it must beat. When `n` is omitted the text
 * describes the contract generically in terms of the candidate's `n`; callers
 * that know the bin count pass it so the contract names the exact length. The
 * last two lines state the exact output shape, with a parseable example, and the
 * exact weights count, since a wrong length is the measured rejection.
 */
export function autocorrelationPromptContext(n?: number): string {
	const bins = n === undefined ? "n" : String(n);
	const last = n === undefined ? "n-1" : String(n - 1);
	const width = n === undefined ? "1/(2n)" : `1/${2 * n}`;
	const yours =
		n === undefined
			? 'yours must keep the current candidate\'s "n" and have exactly n weights'
			: `yours must have "n": ${n} and exactly ${n} weights`;
	return [
		"Task: propose a step function f on [-1/4, 1/4] whose autoconvolution peak max_t (f*f)(t) is as SMALL as possible.",
		"Contract:",
		`- The candidate is {"n": ${bins}, "weights": [w_0, ..., w_${last}]}: ${bins} bins of width h = ${width} covering [-1/4, 1/4] left to right; w_i is the density on bin i.`,
		"- Every weight must be a finite number >= 0 and at least one must be positive. The evaluator rescales the weights so the integral sum_i w_i * h is exactly 1, so only the shape matters.",
		`- The current candidate's "peak" field is its max_t (f*f)(t) after that rescaling (the uniform density has peak ${UNIFORM_ROOT_PEAK}); the score is 1 / peak, higher is better, and a candidate must LOWER the peak to improve.`,
		"- The autoconvolution of a step function is piecewise linear and is evaluated exactly at its knots; there is no sampling to exploit.",
		"Public hints:",
		"- Moving mass from the middle toward both edges lowers the central peak (f*f)(0) at the cost of raising the shoulders; the optimum balances a wide flat top of the autoconvolution.",
		"- Good known solutions are not the uniform density: they look like an asymmetric plateau, with a spike near one edge and a gentle taper toward the other.",
		"- Small local edits (a few bins at a time) that keep the autoconvolution's top flat tend to help; a single dominant bin makes the peak grow with n and is the worst shape.",
		`Exact output shape: {"n": ${bins}, "weights": [w_0, ..., w_${last}]}, a JSON object with exactly these two keys (no "peak"). For example with n = 4: ${AUTOCORRELATION_SHAPE_EXAMPLE}; ${yours}.`,
		`Return the complete candidate object; its weights array must have exactly ${bins} entries.`,
	].join("\n");
}

function toWeights(value: unknown, n: number): number[] {
	if (!Array.isArray(value) || value.length !== n) {
		throw new TypeError(`autocorrelation artifact must have a weights array of length ${n}`);
	}
	return value.map((raw) => {
		if (typeof raw !== "number" || !Number.isFinite(raw)) {
			throw new TypeError("autocorrelation artifact contains a non-finite weight");
		}
		return Math.max(0, raw);
	});
}

export function createAutocorrelationTask(n: number): ScoredTask<AutocorrelationArtifact> {
	if (!Number.isInteger(n) || n < 2) {
		throw new RangeError("autocorrelation requires an integer n >= 2");
	}
	return {
		id: "autocorrelation",
		root(_rng) {
			return rootArtifact(n);
		},
		propose(parent, params, rng, _round) {
			return proposeCandidate(parent ?? rootArtifact(n), params, rng, n);
		},
		evaluate(candidate) {
			return evaluateArtifact(candidate, n);
		},
		serialize(candidate) {
			const normalized = normalizeWeights(candidate.weights);
			const peak = normalized === null ? null : autoconvolutionPeak(normalized);
			return { n: candidate.n, peak, weights: [...candidate.weights] };
		},
		deserialize(value) {
			if (typeof value !== "object" || value === null) {
				throw new TypeError("autocorrelation artifact must be an object");
			}
			const record = value as Record<string, unknown>;
			if (record.n !== undefined && record.n !== n) {
				throw new TypeError(`autocorrelation artifact must have n = ${n}`);
			}
			return { n, weights: toWeights(record.weights, n) };
		},
	};
}
