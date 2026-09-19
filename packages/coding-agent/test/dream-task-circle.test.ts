import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { createSeededRng } from "../src/core/dream/rng.js";
import type { ProposeParams } from "../src/core/dream/task.js";
import {
	CIRCLE_PACKING_EPS,
	type CirclePackingArtifact,
	createCirclePackingTask,
} from "../src/core/dream/tasks/circle-packing.js";
import { canonicalJson } from "../src/core/ravo/canonical-json.js";

const REFINE: ProposeParams = { stepScale: 0.15, refineDepth: 4, branchWidth: 2 };

describe("createSeededRng", () => {
	it("is deterministic for the same seed and diverges for different seeds", () => {
		const a = createSeededRng(7);
		const b = createSeededRng(7);
		const c = createSeededRng(8);
		const drawA = Array.from({ length: 8 }, () => a.next());
		const drawB = Array.from({ length: 8 }, () => b.next());
		const drawC = Array.from({ length: 8 }, () => c.next());
		expect(drawB).toEqual(drawA);
		expect(drawC).not.toEqual(drawA);
	});

	it("forks deterministically and independently of the parent's draws", () => {
		const early = createSeededRng(42).fork("branch");
		const parent = createSeededRng(42);
		for (let i = 0; i < 100; i++) parent.next();
		const late = parent.fork("branch");
		const earlyDraws = Array.from({ length: 5 }, () => early.next());
		const lateDraws = Array.from({ length: 5 }, () => late.next());
		expect(lateDraws).toEqual(earlyDraws);

		const other = createSeededRng(42).fork("other");
		expect(Array.from({ length: 5 }, () => other.next())).not.toEqual(earlyDraws);
	});

	it("keeps nextInt in range and nextGaussian finite", () => {
		const rng = createSeededRng(123);
		for (let i = 0; i < 2000; i++) {
			const value = rng.nextInt(11);
			expect(Number.isInteger(value)).toBe(true);
			expect(value).toBeGreaterThanOrEqual(0);
			expect(value).toBeLessThan(11);
			expect(Number.isFinite(rng.nextGaussian())).toBe(true);
		}
	});

	it("reaches no ambient randomness or wall clock", () => {
		const source = readFileSync(new URL("../src/core/dream/rng.ts", import.meta.url), "utf8");
		// Match calls, not the prose in the module comment.
		expect(source).not.toMatch(/Math\.random\s*\(/);
		expect(source).not.toMatch(/Date\.now\s*\(/);
	});
});

describe("circle-packing task", () => {
	for (const n of [26, 32]) {
		it(`produces a valid root with positive score for n=${n}`, () => {
			const task = createCirclePackingTask(n);
			for (let seed = 1; seed <= 12; seed++) {
				const root = task.root(createSeededRng(seed));
				expect(root.n).toBe(n);
				const evaluation = task.evaluate(root);
				expect(evaluation.valid).toBe(true);
				expect(evaluation.score).toBeGreaterThan(0);
				expect(Number.isFinite(evaluation.score)).toBe(true);
			}
		});
	}

	it("proposes deterministically for identical inputs", () => {
		const task = createCirclePackingTask(26);
		const parent = task.root(createSeededRng(3));
		const first = task.propose(parent, REFINE, createSeededRng(99), 1);
		const second = task.propose(parent, REFINE, createSeededRng(99), 1);
		expect(canonicalJson(task.serialize(first))).toBe(canonicalJson(task.serialize(second)));
	});

	it("keeps candidates feasible across many seeds via the closed-form repair", () => {
		for (let seed = 1; seed <= 24; seed++) {
			const task = createCirclePackingTask(26);
			let current = task.root(createSeededRng(seed));
			expect(task.evaluate(current).valid).toBe(true);
			const rng = createSeededRng(seed * 31 + 5);
			for (let step = 1; step <= 8; step++) {
				current = task.propose(current, REFINE, rng, step);
				const evaluation = task.evaluate(current);
				expect(evaluation.valid).toBe(true);
				expect(evaluation.failClass).toBeUndefined();
				expect(Number.isFinite(evaluation.score)).toBe(true);
			}
		}
	});

	it("scores a valid packing as the sum of radii", () => {
		const task = createCirclePackingTask(2);
		// Two circles at opposite corners, radii within bounds and non-overlapping.
		const artifact: CirclePackingArtifact = { n: 2, xs: [0.2, 0.8], ys: [0.2, 0.8], rs: [0.15, 0.15] };
		const evaluation = task.evaluate(artifact);
		expect(evaluation.valid).toBe(true);
		expect(evaluation.score).toBeCloseTo(0.3, 12);
	});

	it("rejects an overlapping packing with the overlap fail class and zero score", () => {
		const task = createCirclePackingTask(2);
		const overlapping: CirclePackingArtifact = { n: 2, xs: [0.5, 0.5], ys: [0.5, 0.5], rs: [0.3, 0.3] };
		const evaluation = task.evaluate(overlapping);
		expect(evaluation.valid).toBe(false);
		expect(evaluation.score).toBe(0);
		expect(evaluation.failClass).toBe("overlap");
	});

	it("classifies out-of-bounds, negative-radius, non-finite and bad-shape artifacts", () => {
		const task = createCirclePackingTask(1);
		expect(task.evaluate({ n: 1, xs: [0.95], ys: [0.5], rs: [0.2] }).failClass).toBe("out-of-bounds");
		expect(task.evaluate({ n: 1, xs: [0.5], ys: [0.5], rs: [-0.1] }).failClass).toBe("negative-radius");
		expect(task.evaluate({ n: 1, xs: [Number.NaN], ys: [0.5], rs: [0.1] }).failClass).toBe("non-finite");
		expect(task.evaluate({ n: 2, xs: [0.5], ys: [0.5, 0.5], rs: [0.1, 0.1] }).failClass).toBe("invalid-shape");
	});

	it("round-trips through serialize/deserialize with a stable canonical blob", () => {
		const task = createCirclePackingTask(26);
		const root = task.root(createSeededRng(5));
		const serialized = task.serialize(root);
		const restored = task.deserialize(serialized);
		expect(canonicalJson(task.serialize(restored))).toBe(canonicalJson(serialized));
		expect(task.evaluate(restored).score).toBeCloseTo(task.evaluate(root).score, 12);
		// A second serialization of the same artifact is byte-identical.
		expect(canonicalJson(task.serialize(root))).toBe(canonicalJson(serialized));
	});

	it("never scores an overlapping packing as valid within the epsilon tolerance", () => {
		const task = createCirclePackingTask(2);
		const justTouching: CirclePackingArtifact = { n: 2, xs: [0.3, 0.7], ys: [0.5, 0.5], rs: [0.2, 0.2] };
		// dist = 0.4, r_i + r_j = 0.4: touching is allowed within eps.
		expect(task.evaluate(justTouching).valid).toBe(true);
		const overlapping: CirclePackingArtifact = {
			n: 2,
			xs: [0.3, 0.7],
			ys: [0.5, 0.5],
			rs: [0.2 + 10 * CIRCLE_PACKING_EPS, 0.2],
		};
		expect(task.evaluate(overlapping).valid).toBe(false);
	});
});
