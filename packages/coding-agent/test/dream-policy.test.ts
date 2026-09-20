import { describe, expect, it } from "vitest";
import {
	clampPolicy,
	DEFAULT_POLICY,
	differsOnlyInReplayDeadFields,
	type ExplorationPolicy,
	POLICY_BOUNDS,
	PolicyValidationError,
	PRIMING_DIVERSE,
	parseExplorationPolicy,
	policyFieldsDiffering,
	policyId,
	REPLAY_DEAD_FIELDS,
	SELECTION_RULES,
	STOP_RULES,
} from "../src/core/dream/policy.js";

describe("parseExplorationPolicy", () => {
	it("accepts the default policy round-trip", () => {
		const parsed = parseExplorationPolicy({ ...DEFAULT_POLICY });
		expect(parsed).toEqual(DEFAULT_POLICY);
	});

	it("accepts keys in any order", () => {
		const reordered = {
			targetScore: DEFAULT_POLICY.targetScore,
			stopRule: DEFAULT_POLICY.stopRule,
			batchSize: DEFAULT_POLICY.batchSize,
			explorationBias: DEFAULT_POLICY.explorationBias,
			selectionRule: DEFAULT_POLICY.selectionRule,
			beta: DEFAULT_POLICY.beta,
			branchWidth: DEFAULT_POLICY.branchWidth,
			promisingThreshold: DEFAULT_POLICY.promisingThreshold,
			recoveryPolicy: DEFAULT_POLICY.recoveryPolicy,
			refineDepth: DEFAULT_POLICY.refineDepth,
		};
		expect(parseExplorationPolicy(reordered)).toEqual(DEFAULT_POLICY);
	});

	it("rejects a non-object", () => {
		expect(() => parseExplorationPolicy(null)).toThrow(PolicyValidationError);
		expect(() => parseExplorationPolicy([DEFAULT_POLICY])).toThrow(PolicyValidationError);
		expect(() => parseExplorationPolicy("policy")).toThrow(PolicyValidationError);
	});

	it("rejects an unknown key", () => {
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, extra: 1 })).toThrow(PolicyValidationError);
	});

	it("rejects an out-of-range number", () => {
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, batchSize: 999 })).toThrow(PolicyValidationError);
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, promisingThreshold: -0.1 })).toThrow(
			PolicyValidationError,
		);
	});

	it("rejects a non-integer where an integer is required", () => {
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, branchWidth: 2.5 })).toThrow(PolicyValidationError);
	});

	it("rejects a non-finite number", () => {
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, targetScore: Number.POSITIVE_INFINITY })).toThrow(
			PolicyValidationError,
		);
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, targetScore: Number.NaN })).toThrow(
			PolicyValidationError,
		);
	});

	it("rejects a named rule outside the literal set", () => {
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, selectionRule: "nope" })).toThrow(PolicyValidationError);
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, stopRule: "halt" })).toThrow(PolicyValidationError);
	});

	it("rejects a stringified-code payload (unknown key or wrong type)", () => {
		// A smuggled code field can only ever arrive as an extra key ...
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, propose: "() => process.exit(1)" })).toThrow(
			PolicyValidationError,
		);
		// ... or as a wrong-typed known field. Both are rejected; nothing is executed.
		expect(() => parseExplorationPolicy({ ...DEFAULT_POLICY, batchSize: "() => 2" })).toThrow(PolicyValidationError);
	});
});

describe("clampPolicy", () => {
	it("projects out-of-range numbers into bounds and rounds integers", () => {
		const clamped = clampPolicy({
			...DEFAULT_POLICY,
			batchSize: 999,
			branchWidth: 2.4,
			promisingThreshold: -5,
			explorationBias: 100,
		});
		expect(clamped.batchSize).toBe(POLICY_BOUNDS.batchSize.max);
		expect(clamped.branchWidth).toBe(2);
		expect(clamped.promisingThreshold).toBe(POLICY_BOUNDS.promisingThreshold.min);
		expect(clamped.explorationBias).toBe(POLICY_BOUNDS.explorationBias.max);
	});

	it("snaps invalid named rules to the default", () => {
		const clamped = clampPolicy({ ...DEFAULT_POLICY, selectionRule: "nope", stopRule: 42 });
		expect(SELECTION_RULES).toContain(clamped.selectionRule);
		expect(STOP_RULES).toContain(clamped.stopRule);
		expect(clamped.selectionRule).toBe(DEFAULT_POLICY.selectionRule);
		expect(clamped.stopRule).toBe(DEFAULT_POLICY.stopRule);
	});

	it("drops unknown keys and yields a parseable policy", () => {
		const clamped = clampPolicy({ ...DEFAULT_POLICY, junk: "x" });
		expect(() => parseExplorationPolicy(clamped)).not.toThrow();
	});

	it("falls back to defaults for a non-object", () => {
		expect(clampPolicy(undefined)).toEqual(DEFAULT_POLICY);
	});
});

describe("policyId", () => {
	it("is stable under key reordering", () => {
		const reordered: ExplorationPolicy = {
			explorationBias: DEFAULT_POLICY.explorationBias,
			targetScore: DEFAULT_POLICY.targetScore,
			promisingThreshold: DEFAULT_POLICY.promisingThreshold,
			beta: DEFAULT_POLICY.beta,
			batchSize: DEFAULT_POLICY.batchSize,
			refineDepth: DEFAULT_POLICY.refineDepth,
			branchWidth: DEFAULT_POLICY.branchWidth,
			stopRule: DEFAULT_POLICY.stopRule,
			recoveryPolicy: DEFAULT_POLICY.recoveryPolicy,
			selectionRule: DEFAULT_POLICY.selectionRule,
		};
		expect(policyId(reordered)).toBe(policyId(DEFAULT_POLICY));
	});

	it("ignores stray runtime properties", () => {
		const withStray = { ...DEFAULT_POLICY, stray: "ignored" } as unknown as ExplorationPolicy;
		expect(policyId(withStray)).toBe(policyId(DEFAULT_POLICY));
	});

	it("changes when any field changes", () => {
		expect(policyId({ ...DEFAULT_POLICY, batchSize: DEFAULT_POLICY.batchSize + 1 })).not.toBe(
			policyId(DEFAULT_POLICY),
		);
		expect(policyId({ ...DEFAULT_POLICY, selectionRule: "round-robin" })).not.toBe(policyId(DEFAULT_POLICY));
	});
});

describe("replay-dead fields", () => {
	it("names exactly the fields the interpreter never reads", () => {
		expect([...REPLAY_DEAD_FIELDS]).toEqual(["branchWidth", "refineDepth", "recoveryPolicy"]);
	});

	it("lists differing fields in schema order", () => {
		expect(policyFieldsDiffering(DEFAULT_POLICY, DEFAULT_POLICY)).toEqual([]);
		expect(
			policyFieldsDiffering(
				{ ...DEFAULT_POLICY, beta: 3, selectionRule: "weighted", refineDepth: 1 },
				DEFAULT_POLICY,
			),
		).toEqual(["selectionRule", "refineDepth", "beta"]);
	});

	it("flags a candidate that differs from current only in replay-dead fields, and nothing else", () => {
		expect(differsOnlyInReplayDeadFields(DEFAULT_POLICY, DEFAULT_POLICY)).toBe(false);
		expect(differsOnlyInReplayDeadFields({ ...DEFAULT_POLICY, branchWidth: 5 }, DEFAULT_POLICY)).toBe(true);
		expect(
			differsOnlyInReplayDeadFields({ ...DEFAULT_POLICY, recoveryPolicy: "widen", refineDepth: 0 }, DEFAULT_POLICY),
		).toBe(true);
		expect(differsOnlyInReplayDeadFields({ ...DEFAULT_POLICY, branchWidth: 5, beta: 2 }, DEFAULT_POLICY)).toBe(false);
		expect(differsOnlyInReplayDeadFields({ ...DEFAULT_POLICY, stopRule: "never" }, DEFAULT_POLICY)).toBe(false);
	});
});

describe("PRIMING_DIVERSE", () => {
	it("is two parseable, distinct policies that differ from the default in replay-live fields", () => {
		expect(PRIMING_DIVERSE).toHaveLength(2);
		const ids = new Set(PRIMING_DIVERSE.map(policyId));
		expect(ids.size).toBe(2);
		expect(ids.has(policyId(DEFAULT_POLICY))).toBe(false);
		for (const policy of PRIMING_DIVERSE) {
			expect(() => parseExplorationPolicy(policy)).not.toThrow();
			expect(differsOnlyInReplayDeadFields(policy, DEFAULT_POLICY)).toBe(false);
			expect(policy.stopRule).toBe("never");
		}
		expect(PRIMING_DIVERSE[0]).toMatchObject({ selectionRule: "explore-root", batchSize: 8 });
		expect(PRIMING_DIVERSE[1]).toMatchObject({ selectionRule: "best-first", batchSize: 1 });
	});
});
