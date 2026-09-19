import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type LogEntry, setLogSink } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { REFINEMENT_COMMITTED_MSG, REFINEMENT_LOG_COMPONENT } from "../src/core/learning-index.js";
import { emptyFailureLedger, observationOrdinal } from "../src/core/ravo/failure-ledger.js";
import {
	openTrustWindow,
	recordTrustWindowEvidence,
	settleTrustWindows,
} from "../src/core/refinement/harness-trust.js";
import {
	logRefinementOutcome,
	parseJudgeVerdict,
	type RavoGateReport,
	REFINEMENT_APPLIED_UNMEASURED_MSG,
	REFINEMENT_REJECTED_MSG,
	type RefineFinalDecision,
	type RefineReason,
	refineKindOf,
} from "../src/core/refinement/ravo.js";
import {
	getHarnessStatePath,
	isRollbackableRefinement,
	loadHarnessState,
	type RefinementResult,
} from "../src/core/refinement/refinement.js";

function gateReport(decision: RavoGateReport["decision"]): RavoGateReport {
	return {
		decision,
		fastScore: 0,
		deepScore: 0,
		bestScore: 0,
		missedCriteria: [],
		missedWeight: 0,
		epsilon: 0,
		screenThreshold: 0,
		deepTolerance: 0,
		rationale: "",
		addressedFingerprints: [],
		failureOpponents: [],
		measurable: decision === "commit",
		refereeCounts: { cleared: 0, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 0 },
	};
}

function refinementResult(ravo?: RavoGateReport): RefinementResult {
	return {
		id: "r1",
		summary: "",
		rationale: "",
		expectedOutcome: "",
		appliedEdits: [],
		harnessStatePath: "",
		...(ravo === undefined ? {} : { ravo }),
	};
}

describe("parseJudgeVerdict", () => {
	it("passes only on an explicit pass token", () => {
		expect(parseJudgeVerdict("pass")).toBe("pass");
		expect(parseJudgeVerdict("  PASS  ")).toBe("pass");
		expect(parseJudgeVerdict("accept")).toBe("pass");
		expect(parseJudgeVerdict("passed")).toBe("pass");
		expect(parseJudgeVerdict("true")).toBe("pass");
	});

	it("keeps the explicit negative and abstain tokens", () => {
		expect(parseJudgeVerdict("fail")).toBe("fail");
		expect(parseJudgeVerdict("reject")).toBe("fail");
		expect(parseJudgeVerdict("false")).toBe("fail");
		expect(parseJudgeVerdict("abstain")).toBe("abstain");
		expect(parseJudgeVerdict("unknown")).toBe("abstain");
		expect(parseJudgeVerdict("unsure")).toBe("abstain");
	});

	// The gate's whole purpose is to be able to refuse. A judge that drops the
	// verdict field used to be read as authorization, which made "pass" the
	// outcome of the single most common model error.
	it("abstains when the judge said nothing at all", () => {
		expect(parseJudgeVerdict(undefined)).toBe("abstain");
		expect(parseJudgeVerdict(null)).toBe("abstain");
		expect(parseJudgeVerdict("")).toBe("abstain");
		expect(parseJudgeVerdict("   ")).toBe("abstain");
	});

	it("abstains on prose and on a non-string field", () => {
		expect(parseJudgeVerdict("looks good to me")).toBe("abstain");
		expect(parseJudgeVerdict("probably fine")).toBe("abstain");
		expect(parseJudgeVerdict(42)).toBe("abstain");
		expect(parseJudgeVerdict({ verdict: "pass" })).toBe("abstain");
		expect(parseJudgeVerdict(true)).toBe("abstain");
	});
});

describe("isRollbackableRefinement", () => {
	it("offers a committed refinement as a rollback target", () => {
		expect(isRollbackableRefinement(refinementResult(gateReport("commit")))).toBe(true);
	});

	it("keeps pre-gate records eligible", () => {
		expect(isRollbackableRefinement(refinementResult())).toBe(true);
	});

	// Rejections are now appended to the same durable history so the gate's
	// negative decisions survive the session. They applied no edits, so they
	// must never be selected as something to undo.
	it("never offers a rejection as a rollback target", () => {
		expect(isRollbackableRefinement(refinementResult(gateReport("reject_deep")))).toBe(false);
		expect(isRollbackableRefinement(refinementResult(gateReport("reject_screen")))).toBe(false);
		expect(isRollbackableRefinement(refinementResult(gateReport("reject_criteria")))).toBe(false);
		expect(isRollbackableRefinement(refinementResult(gateReport("reject_unclaimed")))).toBe(false);
	});
});

describe("refine kinds", () => {
	it("maps every reason to its kind", () => {
		const kinds: Record<RefineReason, string> = {
			manual: "directed",
			refine_run: "directed",
			rollback: "directed",
			ravo_run: "directed",
			recurrence: "failure",
			regression: "failure",
			turn_interval: "checkpoint",
			compact: "checkpoint",
		};
		for (const [reason, kind] of Object.entries(kinds)) {
			expect(refineKindOf(reason as RefineReason)).toBe(kind);
		}
	});
});

describe("logRefinementOutcome", () => {
	let entries: LogEntry[];

	beforeEach(() => {
		entries = [];
		setLogSink((entry) => entries.push(entry));
	});

	afterEach(() => {
		setLogSink(undefined);
	});

	const refinementLines = () => entries.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT);
	const outcome = (decision: RefineFinalDecision, addressed: string[] = []) => ({
		proposalId: "refine_1",
		decision,
		addressed,
		deepScore: 72,
		missed: 1,
		claimed: addressed.length,
		reason: "recurrence" as const,
		scope: "global" as const,
	});

	it("emits refinement.committed only for a commit that claimed fingerprints", () => {
		logRefinementOutcome(outcome("commit", ["fp1", "fp2"]));
		const lines = refinementLines();
		expect(lines).toHaveLength(1);
		expect(lines[0]).toMatchObject({
			msg: REFINEMENT_COMMITTED_MSG,
			proposalId: "refine_1",
			addressed: ["fp1", "fp2"],
			deepScore: 72,
			missed: 1,
			reason: "recurrence",
			scope: "global",
		});
		expect(lines[0]).not.toHaveProperty("decision");
		expect(lines[0]).not.toHaveProperty("claimed");
	});

	it("reports an unmeasured apply without a committed line", () => {
		for (const decision of ["commit_unmeasured", "rollback"] as const) {
			entries.length = 0;
			logRefinementOutcome({ ...outcome(decision), reason: "turn_interval", scope: "local" });
			const lines = refinementLines();
			expect(lines).toHaveLength(1);
			expect(lines[0]).toMatchObject({
				msg: REFINEMENT_APPLIED_UNMEASURED_MSG,
				proposalId: "refine_1",
				deepScore: 72,
				reason: "turn_interval",
				scope: "local",
			});
			expect(lines[0]).not.toHaveProperty("addressed");
		}
		// A "commit" that claims nothing is not measurable either.
		entries.length = 0;
		logRefinementOutcome(outcome("commit"));
		expect(refinementLines().map((entry) => entry.msg)).toEqual([REFINEMENT_APPLIED_UNMEASURED_MSG]);
	});

	it("reports every non-applying decision as refinement.rejected with its decision", () => {
		const rejections: RefineFinalDecision[] = [
			"reject_screen",
			"reject_deep",
			"reject_criteria",
			"reject_unclaimed",
			"partial",
			"no_edits",
		];
		for (const decision of rejections) {
			entries.length = 0;
			logRefinementOutcome(outcome(decision, ["fp1"]));
			const lines = refinementLines();
			expect(lines).toHaveLength(1);
			expect(lines[0]).toMatchObject({
				msg: REFINEMENT_REJECTED_MSG,
				proposalId: "refine_1",
				decision,
				deepScore: 72,
				missed: 1,
				claimed: 1,
				reason: "recurrence",
				scope: "global",
			});
		}
	});
});

describe("loadHarnessState corruption reporting", () => {
	let dir: string;
	let entries: LogEntry[];

	beforeEach(() => {
		dir = mkdtempSync(join(tmpdir(), "harness-corrupt-"));
		entries = [];
		setLogSink((entry) => entries.push(entry));
	});

	afterEach(() => {
		setLogSink(undefined);
		rmSync(dir, { recursive: true, force: true });
	});

	const corruptWarnings = () => entries.filter((entry) => entry.msg === "harness.state.corrupt");

	it("reports an unparsable state file instead of silently starting empty", () => {
		writeFileSync(getHarnessStatePath(dir), "{ not json", "utf8");
		const state = loadHarnessState(dir, "global");
		expect(state.entries.memory).toEqual({});
		const warnings = corruptWarnings();
		expect(warnings).toHaveLength(1);
		expect(warnings[0]?.reason).toBe("unreadable");
		expect(warnings[0]?.scope).toBe("global");
	});

	it("reports a state file that parses but is not an object", () => {
		writeFileSync(getHarnessStatePath(dir), "[]", "utf8");
		loadHarnessState(dir, "local");
		const warnings = corruptWarnings();
		expect(warnings).toHaveLength(1);
		expect(warnings[0]?.reason).toBe("not-an-object");
		expect(warnings[0]?.scope).toBe("local");
	});

	it("stays silent for a healthy file and for a first run with no file", () => {
		loadHarnessState(dir, "global");
		writeFileSync(
			getHarnessStatePath(dir),
			JSON.stringify({ schema: 1, entries: { prompt: {}, memory: {}, skill: {}, subagent: {} }, refinements: [] }),
			"utf8",
		);
		loadHarnessState(dir, "global");
		expect(corruptWarnings()).toHaveLength(0);
	});
});

describe("observationOrdinal", () => {
	const ledgerWith = (counts: readonly number[]) => {
		const ledger = emptyFailureLedger();
		counts.forEach((count, index) => {
			ledger.failures[`fp${index}`] = {
				fingerprint: { id: `fp${index}`, kind: "tool_error", message: "m" },
				count,
				firstSeenTurn: 0,
				lastSeenTurn: 0,
				firstSeenAt: "",
				lastSeenAt: "",
				excerpt: "",
				addressedByProposalIds: [],
			};
		});
		return ledger;
	};

	it("is zero for an absent or empty ledger", () => {
		expect(observationOrdinal(undefined)).toBe(0);
		expect(observationOrdinal(emptyFailureLedger())).toBe(0);
	});

	it("totals observed occurrences, not distinct fingerprints", () => {
		expect(observationOrdinal(ledgerWith([3, 1, 8]))).toBe(12);
	});

	it("only ever advances as occurrences accrue", () => {
		const before = observationOrdinal(ledgerWith([3, 1]));
		const after = observationOrdinal(ledgerWith([3, 2, 5]));
		expect(after).toBeGreaterThan(before);
	});
});

describe("trust windows settle across a session boundary", () => {
	const lookup = () => ({ trust: undefined });

	// The bug this replaces: committedTurn/untilTurn were per-session
	// assistant-turn counts, which restart at 0. A window opened at turn 40 in
	// session A asks about turns 40..60, and session B is at turn 2, so the
	// window could never settle except for a champion committed at turn 0.
	it("never settles when measured in per-session turns", () => {
		const windows = openTrustWindow(undefined, {
			proposalId: "p1",
			touched: ["memory:m1"],
			claimedFingerprints: ["fp1"],
			committedTurn: 40,
			untilTurn: 60,
		});
		const sessionBTurn = 2;
		const settled = settleTrustWindows(windows, lookup, { turn: sessionBTurn });
		expect(settled.windows.p1?.outcome).toBe("open");
	});

	it("settles clean when measured in the durable observation ordinal", () => {
		const committedAt = observationOrdinal(
			(() => {
				const ledger = emptyFailureLedger();
				ledger.failures.a = {
					fingerprint: { id: "a", kind: "tool_error", message: "m" },
					count: 40,
					firstSeenTurn: 0,
					lastSeenTurn: 0,
					firstSeenAt: "",
					lastSeenAt: "",
					excerpt: "",
					addressedByProposalIds: [],
				};
				return ledger;
			})(),
		);
		const windows = openTrustWindow(undefined, {
			proposalId: "p1",
			touched: ["memory:m1"],
			claimedFingerprints: ["fp1"],
			committedTurn: committedAt,
			untilTurn: committedAt + 20,
		});
		// A later session: the ordinal carried forward and kept advancing.
		const settled = settleTrustWindows(windows, lookup, { turn: committedAt + 21 });
		expect(settled.windows.p1?.outcome).toBe("clean");
		expect(settled.windows.p1?.settledTurn).toBe(committedAt + 21);
	});

	it("faults on upheld evidence recorded for a touched skill whatever the ordinal at settle; never faults a memory-only window", () => {
		const evidence = [
			{
				type: "adjudication" as const,
				proposalId: "p1",
				entry: "skill:s1",
				fingerprintId: "fp1",
				status: "upheld" as const,
				ordinal: 101,
				at: "2026-09-16T10:00:00.000Z",
			},
		];
		const window = { proposalId: "p1", claimedFingerprints: ["fp1"], committedTurn: 100, untilTurn: 120 };
		const skillWindows = recordTrustWindowEvidence(
			openTrustWindow(undefined, { ...window, touched: ["skill:s1"], skillImports: { "skill:s1": ["pkg"] } }),
			evidence,
		);
		const settled = settleTrustWindows(skillWindows, lookup, { turn: 500 });
		expect(settled.windows.p1?.outcome).toBe("faulted");
		expect(settled.windows.p1?.faultedFingerprints).toEqual(["fp1"]);

		const memoryWindows = openTrustWindow(undefined, { ...window, touched: ["memory:m1"] });
		expect(recordTrustWindowEvidence(memoryWindows, evidence)).toEqual(memoryWindows);
		expect(
			settleTrustWindows(recordTrustWindowEvidence(memoryWindows, evidence), lookup, { turn: 110 }).windows.p1
				?.outcome,
		).toBe("open");
	});
});
