import { describe, expect, it } from "vitest";
import {
	assertRunTerminalizedOutcomeEquality,
	type ProjectionKind,
	ReducerError,
	reduceRun,
	revalidateAggregate,
	validateProjectionSemantics,
} from "../src/core/workflow-v2-reducer.js";
import { ctrlEvent, EVIDENCE, hostEvent, turnSettlement, validDefinition } from "./workflow-v2-slice4-fixtures.js";

function expectReject(fn: () => unknown, codeRe: RegExp): void {
	try {
		fn();
	} catch (error) {
		expect(error).toBeInstanceOf(ReducerError);
		expect((error as ReducerError).code).toMatch(codeRe);
		return;
	}
	throw new Error("expected a ReducerError");
}

// A fully accepted, drained, succeeded single-node run.
function happyPathFacts(runId = "run-1"): unknown[] {
	const def = validDefinition();
	void def;
	return [
		ctrlEvent(runId, 1, "RunAdmitted", { evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 2, "RunStarted", { evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 4, "AttemptPrepared", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 5, "AttemptDispatchCommitted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 6, "AttemptAdmissionBound", {
			nodeId: "n1",
			attemptId: "a1",
			operationId: "op1",
			rlmChildId: "child1",
			turnId: "turn1",
			evidenceDigest: EVIDENCE,
		}),
		hostEvent("he1", "hc-1", "TurnStarted", {
			requestId: "req-a1",
			rlmChildId: "child1",
			turnId: "turn1",
			evidenceDigest: EVIDENCE,
		}),
		hostEvent("he2", "hc-2", "TurnSettled", {
			requestId: "req-a1",
			rlmChildId: "child1",
			turnId: "turn1",
			settlement: turnSettlement({ nodeId: "n1", attemptId: "a1", rlmChildId: "child1", turnId: "turn1" }),
			evidenceDigest: EVIDENCE,
		}),
		ctrlEvent(runId, 7, "AttemptSettlementObserved", {
			nodeId: "n1",
			attemptId: "a1",
			settlementDigest: EVIDENCE,
			outcome: "completed",
		}),
		ctrlEvent(runId, 8, "AttemptAccepted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 9, "RunDraining", { evidenceDigest: EVIDENCE }),
		ctrlEvent(runId, 10, "RunTerminalized", { evidenceDigest: EVIDENCE, outcome: "succeeded" }),
	];
}

describe("validateProjectionSemantics — shape", () => {
	it("accepts a legal run projection and returns a frozen sorted copy", () => {
		const p = validateProjectionSemantics("run", {
			phase: "created",
			intent: "none",
			outcome: null,
			conditions: ["admission_open"],
		});
		expect(p).toEqual({ phase: "created", intent: "none", outcome: null, conditions: ["admission_open"] });
		expect(Object.isFrozen(p)).toBe(true);
	});
	it("rejects an unknown kind", () => {
		expectReject(() => validateProjectionSemantics("bogus" as ProjectionKind, {}), /projection_kind_unknown/);
	});
	it("rejects extra keys", () => {
		expectReject(
			() =>
				validateProjectionSemantics("run", {
					phase: "created",
					intent: "none",
					outcome: null,
					conditions: [],
					x: 1,
				}),
			/projection_keys/,
		);
	});
	it("rejects an out-of-vocabulary phase/intent/outcome/condition", () => {
		expectReject(
			() => validateProjectionSemantics("run", { phase: "nope", intent: "none", outcome: null, conditions: [] }),
			/projection_phase/,
		);
		expectReject(
			() => validateProjectionSemantics("run", { phase: "created", intent: "nope", outcome: null, conditions: [] }),
			/projection_intent/,
		);
		expectReject(
			() =>
				validateProjectionSemantics("run", { phase: "terminal", intent: "none", outcome: "nope", conditions: [] }),
			/projection_outcome/,
		);
		expectReject(
			() =>
				validateProjectionSemantics("run", {
					phase: "created",
					intent: "none",
					outcome: null,
					conditions: ["nope"],
				}),
			/projection_condition/,
		);
	});
	it("rejects duplicate conditions", () => {
		expectReject(
			() =>
				validateProjectionSemantics("run", {
					phase: "created",
					intent: "none",
					outcome: null,
					conditions: ["admission_open", "admission_open"],
				}),
			/duplicate/,
		);
	});
	it("enforces terminal<->outcome coupling both ways", () => {
		expectReject(
			() =>
				validateProjectionSemantics("node", { phase: "terminal", intent: "none", outcome: null, conditions: [] }),
			/terminal_outcome/,
		);
		expectReject(
			() =>
				validateProjectionSemantics("node", {
					phase: "ready",
					intent: "none",
					outcome: "accepted",
					conditions: [],
				}),
			/nonterminal_outcome/,
		);
	});
	it("requires integrity_failed for a quarantined run", () => {
		expectReject(
			() =>
				validateProjectionSemantics("run", { phase: "quarantined", intent: "none", outcome: null, conditions: [] }),
			/quarantine_proof/,
		);
		expect(
			validateProjectionSemantics("run", {
				phase: "quarantined",
				intent: "none",
				outcome: null,
				conditions: ["integrity_failed"],
			}),
		).toBeTruthy();
	});
});

// The closed §11 CONTRADICTIONS table — every pair must be rejected.
const CONTRADICTION_MATRIX: Record<ProjectionKind, Array<[string, string]>> = {
	run: [
		["admission_open", "admission_fenced"],
		["integrity_verified", "integrity_failed"],
	],
	node: [
		["dependencies_pending", "dependencies_accepted"],
		["candidate_absent", "candidate_present"],
		["acceptance_pending", "acceptance_passed"],
		["acceptance_pending", "acceptance_failed"],
		["acceptance_passed", "acceptance_failed"],
	],
	attempt: [
		["admission_unbound", "admission_bound"],
		["result_absent", "result_present"],
		["usage_final", "usage_known_prefix"],
		["quiescence_unproved", "quiescence_proved"],
	],
	turn: [
		["result_absent", "result_present"],
		["usage_final", "usage_known_prefix"],
		["cancel_unactuated", "cancel_actuated"],
		["quiescence_unproved", "quiescence_proved"],
	],
	operation: [
		["claim_unheld", "claim_held"],
		["receipt_absent", "receipt_present"],
		["effect_unclassified", "effect_classified"],
	],
};
const NONTERMINAL_PHASE: Record<ProjectionKind, string> = {
	run: "active",
	node: "ready",
	attempt: "prepared",
	turn: "admitted",
	operation: "pending",
};
const NONTERMINAL_INTENT: Record<ProjectionKind, string> = {
	run: "none",
	node: "none",
	attempt: "execute",
	turn: "execute",
	operation: "deliver",
};

describe("validateProjectionSemantics — CONTRADICTIONS matrix (§11)", () => {
	for (const kind of Object.keys(CONTRADICTION_MATRIX) as ProjectionKind[]) {
		for (const [a, b] of CONTRADICTION_MATRIX[kind]) {
			it(`${kind}: rejects contradictory {${a}, ${b}}`, () => {
				expectReject(
					() =>
						validateProjectionSemantics(kind, {
							phase: NONTERMINAL_PHASE[kind],
							intent: NONTERMINAL_INTENT[kind],
							outcome: null,
							conditions: [a, b],
						}),
					/contradiction/,
				);
			});
		}
	}
});

// The closed §11 REQUIRED_PROOFS table — each terminal outcome with one proof missing must be rejected.
const REQUIRED_PROOF_MATRIX: Array<[ProjectionKind, string, string[]]> = [
	[
		"run",
		"succeeded",
		["admission_fenced", "owned_work_quiescent", "effects_classified", "budgets_within_limit", "integrity_verified"],
	],
	["node", "accepted", ["dependencies_accepted", "candidate_present", "acceptance_passed"]],
	["attempt", "completed", ["admission_bound", "result_present", "usage_final", "quiescence_proved"]],
	["turn", "completed", ["result_present", "usage_final", "quiescence_proved"]],
	["operation", "succeeded", ["receipt_present", "effect_classified"]],
];

describe("validateProjectionSemantics — REQUIRED_PROOFS matrix (§11)", () => {
	for (const [kind, outcome, proofs] of REQUIRED_PROOF_MATRIX) {
		it(`${kind}.${outcome}: accepts with all proofs`, () => {
			expect(
				validateProjectionSemantics(kind, {
					phase: "terminal",
					intent: NONTERMINAL_INTENT[kind],
					outcome,
					conditions: proofs,
				}),
			).toBeTruthy();
		});
		for (const missing of proofs) {
			it(`${kind}.${outcome}: rejects missing ${missing}`, () => {
				const partial = proofs.filter((p) => p !== missing);
				expectReject(
					() =>
						validateProjectionSemantics(kind, {
							phase: "terminal",
							intent: NONTERMINAL_INTENT[kind],
							outcome,
							conditions: partial,
						}),
					/missing_proof/,
				);
			});
		}
	}
});

describe("assertRunTerminalizedOutcomeEquality (§5/§11)", () => {
	const succeeded = {
		phase: "terminal",
		intent: "none",
		outcome: "succeeded",
		conditions: [
			"admission_fenced",
			"owned_work_quiescent",
			"effects_classified",
			"budgets_within_limit",
			"integrity_verified",
		],
	} as const;
	it("accepts equal outcome", () => {
		expect(() =>
			assertRunTerminalizedOutcomeEquality("succeeded", { ...succeeded, conditions: [...succeeded.conditions] }),
		).not.toThrow();
	});
	it("rejects a mismatched terminal event outcome", () => {
		expectReject(
			() => assertRunTerminalizedOutcomeEquality("failed", { ...succeeded, conditions: [...succeeded.conditions] }),
			/terminalized_outcome_mismatch/,
		);
	});
	it("rejects application to a non-terminal run", () => {
		expectReject(
			() =>
				assertRunTerminalizedOutcomeEquality(null, {
					phase: "active",
					intent: "start",
					outcome: null,
					conditions: ["admission_open"],
				}),
			/not_terminal/,
		);
	});
});

describe("reduceRun — determinism + replay equivalence", () => {
	it("folds the happy path to a succeeded run", () => {
		const agg = reduceRun(validDefinition(), happyPathFacts());
		expect(agg.run.phase).toBe("terminal");
		expect(agg.run.outcome).toBe("succeeded");
		expect(agg.nodes.n1.outcome).toBe("accepted");
		expect(agg.attempts.a1.projection.outcome).toBe("completed");
		revalidateAggregate(agg);
	});
	it("is deterministic across repeated folds", () => {
		const a = reduceRun(validDefinition(), happyPathFacts());
		const b = reduceRun(validDefinition(), happyPathFacts());
		expect(JSON.stringify(b)).toEqual(JSON.stringify(a));
	});
	it("is replay-equivalent across a JSON round-trip of the fact stream", () => {
		const facts = happyPathFacts();
		const direct = reduceRun(validDefinition(), facts);
		const roundTripped = reduceRun(validDefinition(), JSON.parse(JSON.stringify(facts)));
		expect(JSON.stringify(roundTripped)).toEqual(JSON.stringify(direct));
	});
	it("batch fold equals incremental fold prefix-by-prefix", () => {
		const facts = happyPathFacts();
		const full = reduceRun(validDefinition(), facts);
		// incremental: fold every prefix; the last equals the batch result
		let last = reduceRun(validDefinition(), facts.slice(0, 1));
		for (let i = 2; i <= facts.length; i++) last = reduceRun(validDefinition(), facts.slice(0, i));
		expect(JSON.stringify(last)).toEqual(JSON.stringify(full));
	});
});

describe("reduceRun — illegal-transition rejection", () => {
	it("rejects a fact stream that does not start with RunAdmitted", () => {
		expectReject(
			() => reduceRun(validDefinition(), [ctrlEvent("run-1", 1, "RunStarted", { evidenceDigest: EVIDENCE })]),
			/run_not_admitted/,
		);
	});
	it("rejects RunStarted twice", () => {
		const facts = [
			ctrlEvent("run-1", 1, "RunAdmitted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 2, "RunStarted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 3, "RunStarted", { evidenceDigest: EVIDENCE }),
		];
		expectReject(() => reduceRun(validDefinition(), facts), /run_start_phase/);
	});
	it("rejects a second nonterminal attempt on one node", () => {
		const facts = [
			ctrlEvent("run-1", 1, "RunAdmitted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 2, "RunStarted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 4, "AttemptPrepared", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 5, "AttemptPrepared", { nodeId: "n1", attemptId: "a2", evidenceDigest: EVIDENCE }),
		];
		expectReject(() => reduceRun(validDefinition(), facts), /attempt_node_nonterminal/);
	});
	it("rejects a controller sequence regression", () => {
		const facts = [
			ctrlEvent("run-1", 1, "RunAdmitted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 1, "RunStarted", { evidenceDigest: EVIDENCE }),
		];
		expectReject(() => reduceRun(validDefinition(), facts), /sequence_regressed/);
	});
	it("rejects terminalizing succeeded without proven quiescence", () => {
		// admit/start then immediately drain+terminalize succeeded with a live attempt
		const facts = [
			ctrlEvent("run-1", 1, "RunAdmitted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 2, "RunStarted", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 4, "AttemptPrepared", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 5, "RunDraining", { evidenceDigest: EVIDENCE }),
			ctrlEvent("run-1", 6, "RunTerminalized", { evidenceDigest: EVIDENCE, outcome: "succeeded" }),
		];
		expectReject(() => reduceRun(validDefinition(), facts), /missing_proof/);
	});
	it("rejects a duplicate host event id", () => {
		const runId = "run-1";
		const facts = [
			ctrlEvent(runId, 1, "RunAdmitted", { evidenceDigest: EVIDENCE }),
			ctrlEvent(runId, 2, "RunStarted", { evidenceDigest: EVIDENCE }),
			ctrlEvent(runId, 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE }),
			ctrlEvent(runId, 4, "AttemptPrepared", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
			ctrlEvent(runId, 5, "AttemptDispatchCommitted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
			ctrlEvent(runId, 6, "AttemptAdmissionBound", {
				nodeId: "n1",
				attemptId: "a1",
				operationId: "op1",
				rlmChildId: "child1",
				turnId: "turn1",
				evidenceDigest: EVIDENCE,
			}),
			hostEvent("dup", "hc-1", "TurnStarted", {
				requestId: "req-a1",
				rlmChildId: "child1",
				turnId: "turn1",
				evidenceDigest: EVIDENCE,
			}),
			hostEvent("dup", "hc-2", "TurnStarted", {
				requestId: "req-a1",
				rlmChildId: "child1",
				turnId: "turn1",
				evidenceDigest: EVIDENCE,
			}),
		];
		expectReject(() => reduceRun(validDefinition(), facts), /host_event_duplicate/);
	});
});
