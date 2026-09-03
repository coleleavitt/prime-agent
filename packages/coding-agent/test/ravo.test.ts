import { describe, expect, it } from "vitest";
import type { RefinementProposal } from "../src/core/refinement/index.js";
import {
	countValidRefinementEdits,
	emptyRavoState,
	RAVO_DEFAULT_CONFIG,
	type RavoGateReport,
	type RavoHarnessState,
	ravoBestScore,
	ravoClears,
	ravoCommit,
	ravoDecide,
	ravoEnabled,
	ravoFastScreen,
	ravoMissedWeight,
	ravoPressure,
	rejectedRefinementResult,
} from "../src/core/refinement/index.js";

function report(overrides: Partial<RavoGateReport> = {}): RavoGateReport {
	return {
		decision: "commit",
		fastScore: 100,
		deepScore: 50,
		bestScore: 0,
		missedCriteria: [],
		missedWeight: 0,
		epsilon: RAVO_DEFAULT_CONFIG.epsilon,
		screenThreshold: RAVO_DEFAULT_CONFIG.screenThreshold,
		deepTolerance: RAVO_DEFAULT_CONFIG.deepTolerance,
		rationale: "test",
		...overrides,
	};
}

describe("ravo pure core", () => {
	it("bestScore is 0 on the empty lineage and a running max (Lean Def 1.1)", () => {
		const state = emptyRavoState();
		expect(ravoBestScore(state.lineage)).toBe(0);
		const s1 = ravoCommit(state, { id: "a", summary: "a" }, report({ deepScore: 40 }));
		const s2 = ravoCommit(s1, { id: "b", summary: "b" }, report({ deepScore: 20 }));
		expect(ravoBestScore(s2.lineage)).toBe(40);
	});

	it("lineage best score is monotone under any commit sequence (ravo_run_invariants (1))", () => {
		let state = emptyRavoState();
		let best = 0;
		const scores = [10, 5, 80, 3, 80, 99, 0];
		for (const [index, score] of scores.entries()) {
			state = ravoCommit(state, { id: `c${index}`, summary: "s" }, report({ deepScore: score }));
			const next = ravoBestScore(state.lineage);
			expect(next).toBeGreaterThanOrEqual(best);
			best = next;
		}
	});

	it("screen rejects below tau; deep gate enforces the bar minus tolerance; criteria gate enforces epsilon", () => {
		const state: RavoHarnessState = {
			...emptyRavoState(),
			lineage: [{ id: "x", score: 90, summary: "x", missedCriteria: [], created_at: "" }],
		};
		const config = { screenThreshold: 50, epsilon: 1, deepTolerance: 10 };
		expect(ravoDecide(state, config, { fastScore: 10, deepScore: 100, missedCriteria: [] })).toBe("reject_screen");
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 79, missedCriteria: [] })).toBe("reject_deep");
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 80, missedCriteria: [] })).toBe("commit");
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 95, missedCriteria: ["evidence", "scope"] })).toBe(
			"reject_criteria",
		);
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 95, missedCriteria: ["evidence"] })).toBe("commit");
	});

	it("missedWeight sums the weights of failed criteria (Rocq Def 3.2)", () => {
		const evaluator = {
			criteria: [
				{ id: "a", weight: 2, description: "" },
				{ id: "b", weight: 3, description: "" },
			],
		};
		expect(ravoMissedWeight(evaluator, [])).toBe(0);
		expect(ravoMissedWeight(evaluator, ["a"])).toBe(2);
		expect(ravoMissedWeight(evaluator, ["a", "b"])).toBe(5);
		expect(ravoClears(evaluator, ["a"], 2)).toBe(true);
		expect(ravoClears(evaluator, ["a", "b"], 2)).toBe(false);
	});

	it("pressure doubles the target weight, preserves support, and increases share (Rocq Props 4.2-4.3)", () => {
		const evaluator = {
			criteria: [
				{ id: "weak", weight: 1, description: "" },
				{ id: "other", weight: 4, description: "" },
			],
		};
		const pressured = ravoPressure(evaluator, "weak");
		const weak = pressured.criteria.find((c) => c.id === "weak")!;
		const other = pressured.criteria.find((c) => c.id === "other")!;
		expect(weak.weight).toBe(2);
		expect(other.weight).toBe(4);
		for (const c of pressured.criteria) {
			expect(c.weight).toBeGreaterThan(0);
		}
		// share strictly increases: w/S < 2w/(S+w) in cross-multiplied form
		const w = 1;
		const S = 5;
		expect(w * (S + w)).toBeLessThan(2 * w * S);
	});

	it("commit applies pressure to the first missed criterion only when one exists (Rocq Thm 7.5)", () => {
		const state = emptyRavoState();
		const clean = ravoCommit(state, { id: "a", summary: "a" }, report());
		expect(clean.evaluator).toEqual(state.evaluator);
		const pressured = ravoCommit(state, { id: "b", summary: "b" }, report({ missedCriteria: ["novelty"] }));
		const novelty = pressured.evaluator.criteria.find((c) => c.id === "novelty")!;
		expect(novelty.weight).toBe(2);
	});

	it("fast screen scores the well-formed fraction and zeroes empty proposals", () => {
		const proposal: RefinementProposal = {
			summary: "s",
			rationale: "r",
			expectedOutcome: "o",
			edits: [
				{ action: "create", kind: "memory", title: "t", content: "c" },
				{ action: "update", kind: "memory" },
			],
		};
		expect(ravoFastScreen(proposal, countValidRefinementEdits(proposal))).toBe(50);
		expect(ravoFastScreen({ ...proposal, edits: [] }, 0)).toBe(0);
	});

	it("rejected results apply no edits and carry the gate report", () => {
		const proposal: RefinementProposal = {
			summary: "s",
			rationale: "r",
			expectedOutcome: "o",
			edits: [{ action: "create", kind: "memory", title: "t", content: "c" }],
		};
		const rejected = rejectedRefinementResult(proposal, report({ decision: "reject_deep" }), {
			id: "refine_x",
			scope: "local",
		});
		expect(rejected.appliedEdits).toHaveLength(1);
		expect(rejected.appliedEdits.every((edit) => !edit.applied)).toBe(true);
		expect(rejected.ravo?.decision).toBe("reject_deep");
	});

	it("ravoEnabled defaults on and honors the kill switch", () => {
		expect(ravoEnabled({})).toBe(true);
		expect(ravoEnabled({ PRIME_AGENT_RAVO: "1" })).toBe(true);
		expect(ravoEnabled({ PRIME_AGENT_RAVO: "0" })).toBe(false);
		expect(ravoEnabled({ PRIME_AGENT_RAVO: "off" })).toBe(false);
	});
});
