import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { createSeededRng } from "../src/core/dream/rng.js";
import type { ProposeParams } from "../src/core/dream/task.js";
import {
	AUTOCORRELATION_BIN_COUNTS,
	AUTOCORRELATION_SHAPE_EXAMPLE,
	type AutocorrelationArtifact,
	autoconvolutionKnots,
	autoconvolutionPeak,
	autocorrelationPromptContext,
	binWidth,
	createAutocorrelationTask,
	DEFAULT_AUTOCORRELATION_N,
	normalizeWeights,
	UNIFORM_DENSITY,
	UNIFORM_ROOT_PEAK,
} from "../src/core/dream/tasks/autocorrelation.js";
import { DREAM_TASK_IDS, resolveTask, resolveTaskN, taskPromptContext } from "../src/core/dream/tasks/index.js";
import { canonicalJson } from "../src/core/ravo/canonical-json.js";

const PROPOSE: ProposeParams = { stepScale: 0.2, refineDepth: 4, branchWidth: 2 };

function spikes(n: number, entries: Record<number, number>): AutocorrelationArtifact {
	const weights = new Array<number>(n).fill(0);
	for (const [index, value] of Object.entries(entries)) weights[Number(index)] = value;
	return { n, weights };
}

function integralOf(weights: readonly number[]): number {
	return weights.reduce((sum, value) => sum + value, 0) * binWidth(weights.length);
}

describe("autocorrelation task: exact autoconvolution", () => {
	for (const n of AUTOCORRELATION_BIN_COUNTS) {
		it(`scores the uniform root exactly 0.5 for n=${n}`, () => {
			const task = createAutocorrelationTask(n);
			const root = task.root(createSeededRng(1));
			expect(root.n).toBe(n);
			expect(root.weights).toEqual(new Array(n).fill(UNIFORM_DENSITY));
			expect(integralOf(root.weights)).toBe(1);
			expect(autoconvolutionPeak(root.weights)).toBe(UNIFORM_ROOT_PEAK);
			const evaluation = task.evaluate(root);
			expect(evaluation).toEqual({ valid: true, score: 0.5 });
		});
	}

	it("evaluates the knots of the uniform density as the triangle h * c_k", () => {
		const n = 64;
		const h = binWidth(n);
		const knots = autoconvolutionKnots(new Array<number>(n).fill(UNIFORM_DENSITY));
		expect(knots).toHaveLength(2 * n - 1);
		// c_k = 4 * (min(k, 2n-2-k) + 1) pairs of 2 * 2 for the uniform weights.
		for (let k = 0; k < knots.length; k++) {
			expect(knots[k]).toBeCloseTo(h * 4 * (Math.min(k, 2 * n - 2 - k) + 1), 12);
		}
		expect(knots[n - 1]).toBe(UNIFORM_ROOT_PEAK);
	});

	it("integrates the piecewise-linear autoconvolution to (int f)^2 = 1 for any normalized shape", () => {
		// The trapezoid rule is exact for a piecewise-linear function that is 0 at
		// both ends, so sum(knots) * h must equal the exact integral of f * f.
		const rng = createSeededRng(11);
		for (let trial = 0; trial < 6; trial++) {
			const n = 32;
			const raw = Array.from({ length: n }, () => (rng.next() < 0.3 ? 0 : rng.next() * 5));
			const normalized = normalizeWeights(raw);
			expect(normalized).not.toBeNull();
			const knots = autoconvolutionKnots(normalized!);
			expect(knots.reduce((sum, value) => sum + value, 0) * binWidth(n)).toBeCloseTo(1, 10);
		}
	});

	it("scores a two-spike candidate by the closed form", () => {
		// Equal spikes A at bins p != q: normalization gives A = n, the lag p + q
		// collects 2 A^2, so peak = h * 2 n^2 = n and score = 1 / n regardless of
		// the raw height or where the spikes sit.
		for (const n of AUTOCORRELATION_BIN_COUNTS) {
			const task = createAutocorrelationTask(n);
			expect(task.evaluate(spikes(n, { 0: 5, [n - 1]: 5 })).score).toBe(1 / n);
			expect(task.evaluate(spikes(n, { 3: 0.25, 17: 0.25 })).score).toBe(1 / n);
			// A single spike normalizes to height 2n and autoconvolves to h * 4 n^2 = 2n.
			expect(task.evaluate(spikes(n, { 9: 1 })).score).toBe(1 / (2 * n));
		}
		// Unequal spikes a, b: the lags carry a^2, b^2 and 2ab, so
		// peak = max(a^2, b^2, 2ab) / ((a + b)^2 h).
		const n = 64;
		const task = createAutocorrelationTask(n);
		const a = 3;
		const b = 1;
		const expectedPeak = Math.max(a * a, b * b, 2 * a * b) / ((a + b) * (a + b) * binWidth(n));
		expect(expectedPeak).toBe(72);
		expect(task.evaluate(spikes(n, { 10: a, 40: b })).score).toBeCloseTo(1 / expectedPeak, 14);
	});

	it("judges shape only: scaling every weight leaves the score unchanged", () => {
		const task = createAutocorrelationTask(32);
		const rng = createSeededRng(5);
		const weights = Array.from({ length: 32 }, () => rng.next());
		const base = task.evaluate({ n: 32, weights }).score;
		expect(base).toBeGreaterThan(0);
		expect(task.evaluate({ n: 32, weights: weights.map((value) => value * 1000) }).score).toBeCloseTo(base, 12);
		expect(task.evaluate({ n: 32, weights: weights.map((value) => value / 1000) }).score).toBeCloseTo(base, 12);
	});
});

describe("autocorrelation task: validity", () => {
	const n = 32;
	const task = createAutocorrelationTask(n);

	it("rejects a negative weight with zero score", () => {
		const weights = new Array<number>(n).fill(UNIFORM_DENSITY);
		weights[7] = -1e-9;
		const evaluation = task.evaluate({ n, weights });
		expect(evaluation).toEqual({ valid: false, score: 0, failClass: "negative-weight" });
	});

	it("rejects an all-zero vector as degenerate", () => {
		expect(task.evaluate({ n, weights: new Array<number>(n).fill(0) })).toEqual({
			valid: false,
			score: 0,
			failClass: "degenerate",
		});
	});

	it("rejects non-finite weights and wrong shapes", () => {
		const nan = new Array<number>(n).fill(UNIFORM_DENSITY);
		nan[0] = Number.NaN;
		expect(task.evaluate({ n, weights: nan }).failClass).toBe("non-finite");
		const infinite = new Array<number>(n).fill(UNIFORM_DENSITY);
		infinite[n - 1] = Number.POSITIVE_INFINITY;
		expect(task.evaluate({ n, weights: infinite }).failClass).toBe("non-finite");
		expect(task.evaluate({ n, weights: new Array<number>(n - 1).fill(1) }).failClass).toBe("invalid-shape");
		expect(task.evaluate({ n: 64, weights: new Array<number>(64).fill(1) }).failClass).toBe("invalid-shape");
	});

	it("requires an integer bin count of at least 2", () => {
		expect(() => createAutocorrelationTask(1)).toThrow(RangeError);
		expect(() => createAutocorrelationTask(2.5)).toThrow(RangeError);
		expect(createAutocorrelationTask(2).evaluate({ n: 2, weights: [1, 1] }).score).toBe(0.5);
	});
});

describe("autocorrelation task: proposer", () => {
	it("proposes deterministically under a seed and returns a valid normalized candidate", () => {
		const task = createAutocorrelationTask(64);
		const parent = task.root(createSeededRng(3));
		const first = task.propose(parent, PROPOSE, createSeededRng(99), 1);
		const second = task.propose(parent, PROPOSE, createSeededRng(99), 1);
		expect(canonicalJson(task.serialize(first))).toBe(canonicalJson(task.serialize(second)));
		expect(first.weights).not.toEqual(parent.weights);
		expect(task.evaluate(first).valid).toBe(true);
		expect(integralOf(first.weights)).toBeCloseTo(1, 12);
		for (const value of first.weights) expect(value).toBeGreaterThanOrEqual(0);

		const other = task.propose(parent, PROPOSE, createSeededRng(100), 1);
		expect(canonicalJson(task.serialize(other))).not.toBe(canonicalJson(task.serialize(first)));
	});

	it("keeps every candidate valid and normalized across seeds and steps", () => {
		for (const n of AUTOCORRELATION_BIN_COUNTS) {
			const task = createAutocorrelationTask(n);
			for (let seed = 1; seed <= 8; seed++) {
				let current = task.root(createSeededRng(seed));
				const rng = createSeededRng(seed * 31 + 5);
				for (let step = 1; step <= 6; step++) {
					current = task.propose(current, { ...PROPOSE, stepScale: 0.8, branchWidth: 8 }, rng, step);
					const evaluation = task.evaluate(current);
					expect(evaluation.valid).toBe(true);
					expect(evaluation.failClass).toBeUndefined();
					expect(evaluation.score).toBeGreaterThan(0);
					expect(integralOf(current.weights)).toBeCloseTo(1, 10);
				}
			}
		}
	});

	it("proposes from the uniform root when the parent is null", () => {
		const task = createAutocorrelationTask(32);
		const candidate = task.propose(null, PROPOSE, createSeededRng(2), 1);
		expect(candidate.n).toBe(32);
		expect(task.evaluate(candidate).valid).toBe(true);
	});

	it("improves the score above the uniform 0.5 in a 200-step local hill-climb", () => {
		for (const seed of [1, 2, 3]) {
			const task = createAutocorrelationTask(DEFAULT_AUTOCORRELATION_N);
			let current = task.root(createSeededRng(seed));
			let currentScore = task.evaluate(current).score;
			expect(currentScore).toBe(0.5);
			const rng = createSeededRng(seed * 7 + 1);
			for (let step = 1; step <= 200; step++) {
				const candidate = task.propose(current, PROPOSE, rng, step);
				const score = task.evaluate(candidate).score;
				if (score > currentScore) {
					current = candidate;
					currentScore = score;
				}
			}
			expect(currentScore).toBeGreaterThan(0.5);
			// A score above 0.782 would beat the proven lower bound on the peak.
			expect(currentScore).toBeLessThan(0.782);
		}
	});
});

describe("autocorrelation task: serialization", () => {
	const task = createAutocorrelationTask(64);

	it("round-trips through serialize/deserialize with a stable canonical blob", () => {
		const candidate = task.propose(task.root(createSeededRng(5)), PROPOSE, createSeededRng(6), 1);
		const serialized = task.serialize(candidate);
		const restored = task.deserialize(serialized);
		expect(canonicalJson(task.serialize(restored))).toBe(canonicalJson(serialized));
		expect(task.evaluate(restored).score).toBe(task.evaluate(candidate).score);
		expect(canonicalJson(task.serialize(candidate))).toBe(canonicalJson(serialized));
	});

	it("carries the candidate's peak for the proposer prompt and ignores it on parse", () => {
		const root = task.root(createSeededRng(1));
		const serialized = task.serialize(root) as { n: number; peak: number | null; weights: number[] };
		expect(serialized.n).toBe(64);
		expect(serialized.peak).toBe(UNIFORM_ROOT_PEAK);
		expect(serialized.weights).toEqual(root.weights);
		expect(task.serialize({ n: 64, weights: new Array<number>(64).fill(0) })).toMatchObject({ peak: null });
		const restored = task.deserialize({ n: 64, peak: 0.001, weights: root.weights });
		expect(restored).toEqual({ n: 64, weights: root.weights });
	});

	it("accepts a missing n, clamps negative weights to zero, and rejects bad input", () => {
		const weights = new Array<number>(64).fill(1);
		expect(task.deserialize({ weights })).toEqual({ n: 64, weights });
		const slightlyNegative = [...weights];
		slightlyNegative[0] = -1e-12;
		expect(task.deserialize({ weights: slightlyNegative }).weights[0]).toBe(0);
		expect(() => task.deserialize({ n: 32, weights })).toThrow(/n = 64/);
		expect(() => task.deserialize({ n: 64, weights: weights.slice(1) })).toThrow(/length 64/);
		expect(() => task.deserialize({ n: 64, weights: [...weights.slice(1), Number.NaN] })).toThrow(/non-finite/);
		expect(() => task.deserialize({ n: 64, weights: "1,2,3" })).toThrow(TypeError);
		expect(() => task.deserialize(null)).toThrow(TypeError);
		expect(() => task.deserialize([1, 2, 3])).toThrow(TypeError);
	});
});

describe("autocorrelation task: registry and prompt context", () => {
	it("is registered with the default n=64 and the paper's bin counts", () => {
		expect(DREAM_TASK_IDS).toContain("autocorrelation");
		const task = resolveTask({ task: "autocorrelation" });
		expect(task.id).toBe("autocorrelation");
		expect((task.root(createSeededRng(1)) as AutocorrelationArtifact).n).toBe(DEFAULT_AUTOCORRELATION_N);
		for (const n of AUTOCORRELATION_BIN_COUNTS) {
			expect(
				(resolveTask({ task: "autocorrelation", n }).root(createSeededRng(1)) as AutocorrelationArtifact).n,
			).toBe(n);
		}
		expect(() => resolveTask({ task: "autocorrelation", n: 50 })).toThrow(/32, 64, 128/);
	});

	it("states the public contract, the score and hints, and names the bin count when given", () => {
		const generic = taskPromptContext("autocorrelation");
		expect(generic).toBe(autocorrelationPromptContext());
		expect(generic).toContain("[-1/4, 1/4]");
		expect(generic).toContain("1 / peak");
		expect(generic).toContain("Public hints:");
		expect(generic).toContain("exactly n entries");
		const sized = taskPromptContext("autocorrelation", 128);
		expect(sized).toContain('"n": 128');
		expect(sized).toContain("width h = 1/256");
		expect(sized).toContain("exactly 128 entries");
		expect(sized).toContain('"n": 128 and exactly 128 weights');
		expect(sized).toContain(`has peak ${UNIFORM_ROOT_PEAK}`);
		expect(sized).not.toContain("exactly n entries");
		// The exact-shape line carries a parseable example and names the two keys.
		expect(sized).toContain(AUTOCORRELATION_SHAPE_EXAMPLE);
		expect(JSON.parse(AUTOCORRELATION_SHAPE_EXAMPLE)).toEqual({ n: 4, weights: [1.5, 2.5, 2.5, 1.5] });
		expect(sized).toContain('Exact output shape: {"n": 128, "weights": [w_0, ..., w_127]}');
		expect(generic).toContain('Exact output shape: {"n": n, "weights": [w_0, ..., w_n-1]}');
		expect(taskPromptContext("circle-packing")).toBeUndefined();
		// The registry resolves the size the prompt should name: the spec's n, else the task default.
		expect(resolveTaskN({ task: "autocorrelation" })).toBe(DEFAULT_AUTOCORRELATION_N);
		expect(resolveTaskN({ task: "autocorrelation", n: 128 })).toBe(128);
		expect(resolveTaskN({ task: "circle-packing" })).toBe(26);
		expect(resolveTaskN({ task: "sum-difference" })).toBeUndefined();
		expect(taskPromptContext("autocorrelation", resolveTaskN({ task: "autocorrelation" }))).toContain(
			`exactly ${DEFAULT_AUTOCORRELATION_N} entries`,
		);
	});

	it("reaches no ambient randomness or wall clock", () => {
		const source = readFileSync(new URL("../src/core/dream/tasks/autocorrelation.ts", import.meta.url), "utf8");
		expect(source).not.toMatch(/Math\.random\s*\(/);
		expect(source).not.toMatch(/Date\.now\s*\(/);
	});
});
