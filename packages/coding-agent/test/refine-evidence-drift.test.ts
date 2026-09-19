import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { fauxAssistantMessage, type LogEntry, setLogSink } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { serializeConversation } from "../src/core/compaction/utils.js";
import { REFINEMENT_LOG_COMPONENT } from "../src/core/learning-index.js";
import {
	convertToLlm,
	createCompactionSummaryMessage,
	SESSION_SLASH_COMMAND_CUSTOM_TYPE,
} from "../src/core/messages.js";
import {
	captureRefineEvidence,
	formatRefinementHistoryForPrompt,
	isStaleEvidenceRejection,
	logRefinementOutcome,
	measureRefineEvidenceDrift,
	type RavoDecision,
	type RavoGateReport,
	type RefineEvidenceDrift,
	type RefinementResult,
	refinementOutcome,
	rejectedRefinementResult,
} from "../src/core/refinement/index.js";

function user(text: string): AgentMessage {
	return { role: "user", content: [{ type: "text", text }], timestamp: Date.now() };
}

function slashCommandRow(): AgentMessage {
	return {
		role: "custom",
		customType: SESSION_SLASH_COMMAND_CUSTOM_TYPE,
		content: "/model",
		display: true,
		details: undefined,
		timestamp: Date.now(),
	};
}

function gateReport(overrides: Partial<RavoGateReport> = {}): RavoGateReport {
	return {
		decision: "reject_deep",
		fastScore: 100,
		deepScore: 20,
		bestScore: 0,
		missedCriteria: ["evidence"],
		missedWeight: 1,
		epsilon: 1,
		screenThreshold: 50,
		deepTolerance: 10,
		rationale: "the transcript shows the tool is registered now",
		addressedFingerprints: [],
		failureOpponents: [],
		measurable: false,
		refereeCounts: { cleared: 0, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 0 },
		...overrides,
	};
}

function drift(kind: RefineEvidenceDrift["kind"]): RefineEvidenceDrift {
	return {
		kind,
		snapshotMessages: 2,
		judgeMessages: 3,
		appendedMessages: kind === "none" ? 0 : 1,
		removedMessages: kind === "rewritten" ? 1 : 0,
		appendedChars: kind === "none" ? 0 : 10,
		snapshotLeafId: "a",
		judgeLeafId: "b",
	};
}

describe("refine evidence drift", () => {
	it("measures no drift when the conversation is unchanged", () => {
		const messages = [user("first"), fauxAssistantMessage("reply")];

		const result = measureRefineEvidenceDrift(captureRefineEvidence(messages, "a"), messages, "a");

		expect(result).toMatchObject({
			kind: "none",
			appendedMessages: 0,
			removedMessages: 0,
			appendedChars: 0,
			snapshotLeafId: "a",
			judgeLeafId: "a",
		});
		expect(result.snapshotMessages).toBe(result.judgeMessages);
	});

	it("counts only messages and characters the judge reads", () => {
		const messages = [user("first"), fauxAssistantMessage("reply")];
		const snapshot = captureRefineEvidence(messages, "a");
		const appended = user("the build tool is registered now");
		messages.push(appended, slashCommandRow());

		expect(measureRefineEvidenceDrift(snapshot, messages, "b")).toMatchObject({
			kind: "appended",
			snapshotMessages: 2,
			judgeMessages: 4,
			appendedMessages: 1,
			removedMessages: 0,
			appendedChars: serializeConversation(convertToLlm([appended])).length,
		});

		const onlySlash = [...snapshot.messages, slashCommandRow()];
		expect(measureRefineEvidenceDrift(snapshot, onlySlash, "b")).toMatchObject({
			kind: "none",
			appendedMessages: 0,
			appendedChars: 0,
		});
	});

	it("classifies a compaction-shaped replacement as rewritten", () => {
		const messages = [user("old evidence"), fauxAssistantMessage("old reply"), user("kept")];
		const snapshot = captureRefineEvidence(messages, "a");
		const live: AgentMessage[] = [
			createCompactionSummaryMessage("summary of old evidence", 1000, new Date().toISOString()),
			...messages.slice(2).map((message) => structuredClone(message)),
		];

		const result = measureRefineEvidenceDrift(snapshot, live, "c");

		expect(result.kind).toBe("rewritten");
		expect(result.removedMessages).toBeGreaterThanOrEqual(1);
		expect(result.appendedChars).toBeGreaterThan(0);
	});

	it("tolerates removal of a trailing error message with no content", () => {
		const messages = [user("do it"), fauxAssistantMessage([], { stopReason: "error", errorMessage: "overloaded" })];
		const snapshot = captureRefineEvidence(messages, "a");
		const live = [messages[0]!, fauxAssistantMessage("done")];

		expect(measureRefineEvidenceDrift(snapshot, live, "b")).toMatchObject({
			kind: "appended",
			removedMessages: 0,
			appendedMessages: 1,
		});
	});

	it("classifies removal of a partial-content error message as rewritten, not cancelled", () => {
		const messages = [
			user("do it"),
			fauxAssistantMessage("half of an answer", { stopReason: "error", errorMessage: "stream reset" }),
		];
		const snapshot = captureRefineEvidence(messages, "a");
		const live = [messages[0]!, fauxAssistantMessage("the whole answer")];

		expect(measureRefineEvidenceDrift(snapshot, live, "b")).toMatchObject({
			kind: "rewritten",
			removedMessages: 1,
			appendedMessages: 1,
		});
	});

	it("counts a message inserted before one the proposer read as appended, not rewritten", () => {
		const failed = fauxAssistantMessage("half of an answer", { stopReason: "error", errorMessage: "stream reset" });
		const messages = [user("do it"), failed];
		const snapshot = captureRefineEvidence(messages, "a");
		const inserted: AgentMessage = {
			role: "custom",
			customType: "ipython_state",
			content: "[python-state]\n\nYour Python kernel persisted through compaction.",
			display: false,
			details: undefined,
			timestamp: Date.now(),
		};
		const live = [messages[0]!, inserted, failed];

		expect(measureRefineEvidenceDrift(snapshot, live, "a")).toMatchObject({
			kind: "appended",
			removedMessages: 0,
			appendedMessages: 1,
			appendedChars: serializeConversation(convertToLlm([inserted])).length,
		});
	});

	it("counts only what a compaction replaced when it keeps a tail", () => {
		const kept = [user("kept"), fauxAssistantMessage("kept reply")];
		const messages = [user("old evidence"), fauxAssistantMessage("old reply"), ...kept];
		const snapshot = captureRefineEvidence(messages, "a");
		const summary = createCompactionSummaryMessage("summary of old evidence", 1000, new Date().toISOString());

		expect(measureRefineEvidenceDrift(snapshot, [summary, ...kept], "c")).toMatchObject({
			kind: "rewritten",
			snapshotMessages: 4,
			judgeMessages: 3,
			removedMessages: 2,
			appendedMessages: 1,
			appendedChars: serializeConversation(convertToLlm([summary])).length,
		});
	});

	it("tags only judge-driven rejections after drift as stale evidence", () => {
		const judged: RavoDecision[] = ["reject_deep", "reject_criteria", "reject_unclaimed"];
		for (const decision of judged) {
			for (const kind of ["appended", "rewritten"] as const) {
				expect(isStaleEvidenceRejection(gateReport({ decision }), drift(kind))).toBe(true);
			}
		}
		for (const decision of ["commit", "reject_screen"] as const) {
			expect(isStaleEvidenceRejection(gateReport({ decision }), drift("appended"))).toBe(false);
		}
		expect(isStaleEvidenceRejection(gateReport({ judgeError: "overloaded" }), drift("appended"))).toBe(false);
		expect(isStaleEvidenceRejection(gateReport(), drift("none"))).toBe(false);
		expect(isStaleEvidenceRejection(gateReport(), undefined)).toBe(false);
		for (const status of ["upheld", "unverifiable", "no_evidence"] as const) {
			const refereeCounts = { cleared: 0, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 0 };
			refereeCounts[status] = 1;
			expect(
				isStaleEvidenceRejection(gateReport({ decision: "reject_criteria", refereeCounts }), drift("appended")),
			).toBe(false);
		}
		const cleared = { cleared: 1, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 1 };
		expect(isStaleEvidenceRejection(gateReport({ refereeCounts: cleared }), drift("appended"))).toBe(true);
	});

	describe("outcome records", () => {
		let logs: LogEntry[];

		beforeEach(() => {
			logs = [];
			setLogSink((entry) => logs.push(entry));
		});

		afterEach(() => {
			setLogSink(undefined);
		});

		it("logs stale-evidence fields only on a stale rejection and replanOf on any outcome of a re-plan", () => {
			const base = { reason: "turn_interval" as const, scope: "local" as const };
			logRefinementOutcome(
				refinementOutcome({
					...base,
					proposalId: "refine_1",
					decision: "reject_deep",
					report: gateReport(),
					cause: "stale_evidence",
					staleEvidence: true,
					driftKind: "appended",
					driftMessages: 1,
					replanScheduled: true,
				}),
			);
			logRefinementOutcome(
				refinementOutcome({
					...base,
					proposalId: "refine_2",
					decision: "reject_deep",
					report: gateReport(),
					cause: "gate",
					staleEvidence: false,
					driftKind: "none",
					driftMessages: 0,
					replanScheduled: false,
				}),
			);
			logRefinementOutcome(
				refinementOutcome({
					...base,
					proposalId: "refine_3",
					decision: "commit",
					report: gateReport({ decision: "commit", addressedFingerprints: ["abc"], measurable: true }),
					replanOf: "refine_1",
				}),
			);
			logRefinementOutcome(
				refinementOutcome({ ...base, proposalId: "refine_4", decision: "commit_unmeasured", replanOf: "refine_1" }),
			);

			const lines = logs.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT);
			expect(lines.map((line) => line.msg)).toEqual([
				"refinement.rejected",
				"refinement.rejected",
				"refinement.committed",
				"refinement.applied_unmeasured",
			]);
			expect(lines[0]).toMatchObject({
				proposalId: "refine_1",
				staleEvidence: true,
				driftKind: "appended",
				driftMessages: 1,
				replanScheduled: true,
			});
			expect(lines[0]).not.toHaveProperty("replanOf");
			for (const key of ["staleEvidence", "driftKind", "driftMessages", "replanScheduled", "replanOf"]) {
				expect(lines[1]).not.toHaveProperty(key);
			}
			expect(lines[2]).toMatchObject({ proposalId: "refine_3", replanOf: "refine_1" });
			expect(lines[2]).not.toHaveProperty("staleEvidence");
			expect(lines[3]).toMatchObject({ proposalId: "refine_4", replanOf: "refine_1" });
		});
	});

	it("shows a stale-evidence rejection as a timing artefact joined to its re-plan", () => {
		const proposal = {
			summary: "Build tool not registered",
			rationale: "r",
			expectedOutcome: "o",
			edits: [{ action: "create" as const, kind: "memory" as const, title: "Tool", content: "not registered" }],
		};
		const stale: RefinementResult = {
			...rejectedRefinementResult(proposal, gateReport({ decision: "reject_criteria" }), {
				id: "refine_20260916120000000",
				scope: "local",
				cause: "stale_evidence",
			}),
			staleEvidence: true,
		};
		const replan: RefinementResult = {
			id: "refine_20260916120000500",
			summary: "Record how to register the build tool",
			rationale: "r",
			expectedOutcome: "o",
			appliedEdits: [],
			harnessStatePath: "",
			scope: "local",
			replanOf: stale.id,
		};

		const alone = formatRefinementHistoryForPrompt([stale]);
		expect(alone).toContain("gate: reject_criteria (");
		expect(alone).toContain("a timing artefact of planning: the conversation changed while it was planned");
		expect(alone).not.toContain("re-planned as");

		const joined = formatRefinementHistoryForPrompt([stale, replan]);
		const [staleItem, replanItem] = joined.split("\n\n");
		expect(staleItem).toContain(`re-planned as ${replan.id})`);
		expect(staleItem).toContain('"the transcript shows the tool is registered now"');
		expect(replanItem?.startsWith(`[${replan.id}] replanOf=${stale.id} Record how to register`)).toBe(true);

		const gate = formatRefinementHistoryForPrompt([
			rejectedRefinementResult(proposal, gateReport(), { id: "refine_20260916120000001", cause: "gate" }),
			{ ...replan, replanOf: "refine_20260916120000001" },
		]);
		expect(gate).not.toContain("re-planned as");
		expect(gate).not.toContain("timing artefact");
	});
});
