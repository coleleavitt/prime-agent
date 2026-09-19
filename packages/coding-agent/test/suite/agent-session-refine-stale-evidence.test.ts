import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import {
	type AssistantMessage,
	addSpanSink,
	type Context,
	fauxAssistantMessage,
	fauxToolCall,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
	type UserMessage,
} from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { REFINEMENT_LOG_COMPONENT } from "../../src/core/learning-index.js";
import { createCompactionSummaryMessage } from "../../src/core/messages.js";
import { fingerprintFailure, GLOBAL_FAILURE_LEDGER_ENV } from "../../src/core/ravo/failure-ledger.js";
import {
	type AutoRefineReview,
	getSessionRefinementHistoryPath,
	loadRefinementHistory,
	type RefinementResult,
} from "../../src/core/refinement/index.js";
import { createHarness, type Harness } from "./harness.js";

const MARKER = "DRIFT-MARKER the build tool is registered now";
const ORIGINAL_EVIDENCE = "ORIGINAL-EVIDENCE the build tool is not registered yet";
const DEPLOY_ERROR = 'manifest validation failed for release "12"';
const DEPLOY_FINGERPRINT = fingerprintFailure("tool_error", "deploy", undefined, DEPLOY_ERROR).id;
const REFINE_CANCELLED = "Refinement cancelled: the session was aborted or changed branch.";

type StaleEvidenceInternals = {
	_staleEvidenceReplan?: {
		options: { replanOf: string; triggerFingerprintIds?: string[]; reason?: string };
		source: string;
		run?: Promise<void>;
	};
	_assistantTurnsSinceAutoRefine: number;
	_lastAutoRefineReviewAt: number;
	_failureRefineTriggered: Set<string>;
	_pendingRequestedRefine?: Record<string, unknown>;
	_pendingAutoRefineReview?: { reason: "turn_interval" | "compact"; review: AutoRefineReview };
	_compactAutoRefinePending: boolean;
	_turnIntervalAutoRefinePending: boolean;
	_serializedPlanInFlight?: Promise<unknown>;
	_runApprovedRefine(reason: "turn_interval" | "compact", review: AutoRefineReview): Promise<void>;
	_maybeAutoRefine(reason: "turn_interval" | "compact"): Promise<void>;
	_scheduleAutoRefineAfterAgentEnd(): void;
	_runStaleEvidenceReplanNow(): Promise<void>;
	_requestAbort(dropLaunchedRefines: boolean): void;
	_scheduleStaleEvidenceReplanLaunch(): void;
	_maybeStartSerializedBackgroundPlan(): void;
	_runSerializedRefineCheckpoint(): Promise<void>;
	_invalidatePendingAutoRefineForBranchChange(): Promise<void>;
	_drainPendingRefinementForDisposal(): Promise<void>;
	_reviewAutoRefine(context: { reason: string; turnsSinceLastReview: number }): Promise<AutoRefineReview>;
	_armStaleEvidenceReplan(planId: string, options: Record<string, unknown>, source: string): boolean;
};

const REVIEW: AutoRefineReview = { shouldRefine: true, rationale: "durable lesson", instructions: "capture it" };

function memoryPlan(
	id: string,
	content = "The build tool is not yet registered; register it before building.",
): string {
	return JSON.stringify({
		summary: `note ${id}`,
		rationale: "evidence",
		expectedOutcome: "fewer failures",
		edits: [{ action: "create", kind: "memory", id, title: `Build tool ${id}`, content }],
	});
}

const EMPTY_PLAN = JSON.stringify({ summary: "nothing", rationale: "no edit", expectedOutcome: "", edits: [] });

function judgeReply(verdict: "pass" | "fail", addressedFingerprints: string[] = []): string {
	return JSON.stringify({
		verdict,
		score: verdict === "pass" ? 90 : 20,
		failedCriteria: verdict === "pass" ? [] : ["evidence"],
		addressedFingerprints,
		rationale:
			verdict === "pass" ? "judged fine" : "the memory is stale: the transcript shows the tool is registered",
	});
}

function userPromptText(context: Context): string {
	return context.messages
		.flatMap((message) => {
			if (message.role !== "user") return [];
			return typeof message.content === "string"
				? [message.content]
				: message.content.map((part) => (part.type === "text" ? part.text : ""));
		})
		.join("\n");
}

function section(prompt: string | undefined, tag: string): string | undefined {
	return new RegExp(`<${tag}>\\n([\\s\\S]*?)\\n</${tag}>`).exec(prompt ?? "")?.[1];
}

function userMessage(text: string): UserMessage {
	return { role: "user", content: [{ type: "text", text }], timestamp: Date.now() };
}

function deployTool(): AgentTool {
	return {
		name: "deploy",
		label: "Deploy",
		description: "Always fails",
		parameters: Type.Object({}),
		execute: async () => {
			throw new Error(DEPLOY_ERROR);
		},
	};
}

const settle = (ms = 50) => new Promise<void>((resolve) => setTimeout(resolve, ms));

describe("AgentSession stale-evidence refinement rounds", () => {
	const harnesses: Harness[] = [];
	const tempDirs: string[] = [];
	const previousEnv = {
		agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
		globalLedger: process.env[GLOBAL_FAILURE_LEDGER_ENV],
	};
	let spans: SpanEndRecord[];
	let logs: LogEntry[];
	let removeSpanSink: () => void;

	beforeEach(() => {
		const agentDir = mkdtempSync(join(tmpdir(), "prime-agent-stale-evidence-"));
		tempDirs.push(agentDir);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
		process.env[GLOBAL_FAILURE_LEDGER_ENV] = "0";
		spans = [];
		logs = [];
		removeSpanSink = addSpanSink((record) => spans.push(record));
		setLogSink((entry) => logs.push(entry));
	});

	afterEach(() => {
		removeSpanSink();
		setLogSink(undefined);
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		while (tempDirs.length > 0) rmSync(tempDirs.pop()!, { recursive: true, force: true });
		for (const [key, value] of [
			["PRIME_AGENT_CODING_AGENT_DIR", previousEnv.agentDir],
			[GLOBAL_FAILURE_LEDGER_ENV, previousEnv.globalLedger],
		] as const) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	});

	const spansNamed = (name: string) => spans.filter((span) => span.name === name);
	const outcomeLines = () => logs.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT);
	const internalsOf = (harness: Harness) => harness.session as unknown as StaleEvidenceInternals;

	async function persistedSession(
		tools: AgentTool[] = [],
		options: { autoRefineReviewer?: () => Promise<AutoRefineReview> } = {},
	): Promise<Harness> {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools,
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
			...options,
		});
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("noted")]);
		await harness.session.prompt(ORIGINAL_EVIDENCE);
		await harness.session.waitForIdle();
		return harness;
	}

	/** A planner step that moves the conversation while the proposal is planned. */
	function driftingPlanner(harness: Harness, plan: string, prompts?: string[], marker = MARKER) {
		return (context: Context) => {
			prompts?.push(userPromptText(context));
			const message = userMessage(marker);
			harness.session.messages.push(message);
			harness.sessionManager.appendMessage(message);
			return fauxAssistantMessage(plan);
		};
	}

	function capturing(reply: string, prompts: string[]) {
		return (context: Context) => {
			prompts.push(userPromptText(context));
			return fauxAssistantMessage(reply);
		};
	}

	/** A planner step that waits for the test, so a re-plan can be interrupted while it plans. */
	function gatedPlanner(plan: string): {
		step: () => Promise<AssistantMessage>;
		started: Promise<void>;
		release(): void;
	} {
		let markStarted: () => void = () => {};
		const started = new Promise<void>((resolve) => {
			markStarted = resolve;
		});
		let release: () => void = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		return {
			step: async () => {
				markStarted();
				await gate;
				return fauxAssistantMessage(plan);
			},
			started,
			release: () => release(),
		};
	}

	/** Holds a launch: a refine that reports a result marks the agent busy until the test releases it. */
	function holdLaunchOnComplete(harness: Harness): { release(): void } {
		let holding = true;
		harness.session.subscribe((event) => {
			if (holding && event.type === "refine_complete") {
				(harness.session.agent.state as { isStreaming: boolean }).isStreaming = true;
			}
		});
		return {
			release() {
				holding = false;
				(harness.session.agent.state as { isStreaming: boolean }).isStreaming = false;
			},
		};
	}

	/** An interval round whose plan the judge rejects on evidence that arrived while it was planned. */
	async function staleIntervalRound(harness: Harness): Promise<string> {
		const internals = internalsOf(harness);
		internals._assistantTurnsSinceAutoRefine = 30;
		internals._lastAutoRefineReviewAt = 0;
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("tool_not_registered")),
			fauxAssistantMessage(judgeReply("fail")),
		]);
		await internals._runApprovedRefine("turn_interval", REVIEW);
		const first = spansNamed("refine.plan").at(-1)!;
		return first.attrs["refinement.id"] as string;
	}

	it("gives the judge the live conversation and records the drift on refine.plan", async () => {
		const harness = await persistedSession();
		const plannerPrompts: string[] = [];
		const judgePrompts: string[] = [];
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("tool_not_registered"), plannerPrompts),
			capturing(judgeReply("fail"), judgePrompts),
		]);

		await harness.session.refine({ instructions: "capture lessons" });

		expect(plannerPrompts[0]).toContain(ORIGINAL_EVIDENCE);
		expect(plannerPrompts[0]).not.toContain(MARKER);
		expect(section(judgePrompts[0], "conversation")).toContain(MARKER);
		const attrs = spansNamed("refine.plan")[0]!.attrs;
		expect(attrs).toMatchObject({
			"refine.evidence_drift": "appended",
			"refine.drift_messages": 1,
			"refine.drift_removed_messages": 0,
			"refine.stale_evidence": true,
		});
		expect(attrs["refine.judge_messages"]).toBe((attrs["refine.snapshot_messages"] as number) + 1);
		expect(attrs["refine.drift_chars"]).toBeGreaterThan(0);
		expect(typeof attrs["refine.snapshot_leaf_id"]).toBe("string");
		expect(typeof attrs["refine.judge_leaf_id"]).toBe("string");
		expect(attrs["refine.snapshot_leaf_id"]).not.toBe(attrs["refine.judge_leaf_id"]);
	});

	it("tags a stale-evidence rejection everywhere but does not re-plan a user /refine", async () => {
		const harness = await persistedSession();
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("tool_not_registered")),
			fauxAssistantMessage(judgeReply("fail")),
		]);

		const result = await harness.session.refine({ instructions: "capture lessons" });

		expect(result.staleEvidence).toBe(true);
		expect(result.rejectionCause).toBe("stale_evidence");
		expect(result).not.toHaveProperty("replanOf");
		expect(spansNamed("refine.apply")[0]!.attrs).toMatchObject({
			"refine.decision": "reject_deep",
			"refine.stale_evidence": true,
			"refine.replan_scheduled": false,
			"refine.rejection_cause": "stale_evidence",
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: "refinement.rejected",
				proposalId: result.id,
				staleEvidence: true,
				driftKind: "appended",
				driftMessages: 1,
				replanScheduled: false,
				cause: "stale_evidence",
			}),
		]);
		const entry = harness.sessionManager
			.getEntries()
			.find((item) => item.type === "custom" && item.customType === "prime-agent.refinement");
		expect((entry as { data?: RefinementResult } | undefined)?.data).toMatchObject({
			id: result.id,
			staleEvidence: true,
		});
		expect(loadRefinementHistory(getSessionRefinementHistoryPath(harness.session.sessionId)!, "local")).toEqual([
			expect.objectContaining({ id: result.id, staleEvidence: true }),
		]);

		await settle();
		expect(spansNamed("refine.plan")).toHaveLength(1);
		expect(internalsOf(harness)._staleEvidenceReplan).toBeUndefined();
	});

	it("does not tag a rejection without drift or a commit with drift", async () => {
		const harness = await persistedSession();
		harness.setResponses([fauxAssistantMessage(memoryPlan("steady_note")), fauxAssistantMessage(judgeReply("fail"))]);

		const rejected = await harness.session.refine({ instructions: "capture lessons" });

		expect(spansNamed("refine.plan")[0]!.attrs).toMatchObject({
			"refine.evidence_drift": "none",
			"refine.drift_messages": 0,
			"refine.drift_chars": 0,
			"refine.stale_evidence": false,
		});
		expect(rejected).not.toHaveProperty("staleEvidence");
		expect(rejected.rejectionCause).toBe("gate");
		expect(outcomeLines()[0]).toMatchObject({ msg: "refinement.rejected", cause: "gate" });
		expect(outcomeLines()[0]).not.toHaveProperty("staleEvidence");

		harness.setResponses([
			driftingPlanner(harness, memoryPlan("moving_note")),
			fauxAssistantMessage(judgeReply("pass")),
		]);
		const committed = await harness.session.refine({ instructions: "capture lessons" });

		expect(spansNamed("refine.plan")[1]!.attrs).toMatchObject({
			"refine.evidence_drift": "appended",
			"refine.stale_evidence": false,
		});
		expect(spansNamed("refine.apply")[1]!.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"refine.stale_evidence": false,
			"refine.replan_scheduled": false,
		});
		expect(committed).not.toHaveProperty("staleEvidence");
		expect(outcomeLines()[1]).toMatchObject({ msg: "refinement.applied_unmeasured" });
		expect(outcomeLines()[1]).not.toHaveProperty("staleEvidence");
	});

	it("judges a conversation compacted mid-plan live and tags its rejection stale", async () => {
		const harness = await persistedSession();
		const judgePrompts: string[] = [];
		harness.setResponses([
			() => {
				harness.session.agent.state.messages = [
					createCompactionSummaryMessage(
						"COMPACTED-SUMMARY the tool was registered",
						1000,
						new Date().toISOString(),
					),
					userMessage("kept after compaction"),
				];
				return fauxAssistantMessage(memoryPlan("tool_not_registered"));
			},
			capturing(judgeReply("fail"), judgePrompts),
		]);

		const result = await harness.session.refine({ instructions: "capture lessons" });

		const conversation = section(judgePrompts[0], "conversation");
		expect(conversation).toContain("COMPACTED-SUMMARY");
		expect(conversation).not.toContain(ORIGINAL_EVIDENCE);
		const plan = spansNamed("refine.plan")[0]!;
		expect(plan.status).toBe("ok");
		expect(plan.attrs).toMatchObject({ "refine.evidence_drift": "rewritten", "refine.stale_evidence": true });
		expect(plan.attrs["refine.drift_removed_messages"]).toBeGreaterThanOrEqual(1);
		expect(result.staleEvidence).toBe(true);
	});

	it("keeps an interval round open on a stale-evidence rejection and re-plans once on the current conversation", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		let releaseReplan: () => void = () => {};
		const replanGate = new Promise<void>((resolve) => {
			releaseReplan = resolve;
		});
		let replanStarted: () => void = () => {};
		const started = new Promise<void>((resolve) => {
			replanStarted = resolve;
		});
		const replanPrompts: string[] = [];
		internals._assistantTurnsSinceAutoRefine = 30;
		internals._lastAutoRefineReviewAt = 0;
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("tool_not_registered")),
			fauxAssistantMessage(judgeReply("fail")),
			async (context) => {
				replanPrompts.push(userPromptText(context));
				replanStarted();
				await replanGate;
				return fauxAssistantMessage(memoryPlan("register_build_tool", "Register the build tool with `tools add`."));
			},
			fauxAssistantMessage(judgeReply("pass")),
		]);

		await internals._runApprovedRefine("turn_interval", REVIEW);

		const firstId = spansNamed("refine.plan")[0]!.attrs["refinement.id"] as string;
		expect(internals._assistantTurnsSinceAutoRefine).toBe(30);
		expect(internals._lastAutoRefineReviewAt).toBe(0);
		expect(internals._staleEvidenceReplan?.options.replanOf).toBe(firstId);

		await started;
		expect(section(replanPrompts[0], "conversation")).toContain(MARKER);
		expect(internals._assistantTurnsSinceAutoRefine).toBe(30);
		releaseReplan();
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2));
		await vi.waitFor(() => expect(internals._staleEvidenceReplan).toBeUndefined());

		const plans = spansNamed("refine.plan");
		expect(plans).toHaveLength(2);
		expect(plans[0]!.attrs).not.toHaveProperty("refine.replan_of");
		expect(plans[1]!.attrs).toMatchObject({
			"refine.replan_of": firstId,
			"refine.source": "auto",
			"refine.reason": "turn_interval",
			"refine.stale_evidence": false,
		});
		expect(spansNamed("refine.apply")[0]!.attrs).toMatchObject({ "refine.replan_scheduled": true });
		expect(spansNamed("refine.apply")[1]!.attrs).toMatchObject({ "refine.replan_of": firstId });
		expect(outcomeLines().map((line) => [line.msg, line.staleEvidence, line.replanScheduled, line.replanOf])).toEqual(
			[
				["refinement.rejected", true, true, undefined],
				["refinement.applied_unmeasured", undefined, undefined, firstId],
			],
		);
		expect(internals._assistantTurnsSinceAutoRefine).toBe(0);
		expect(internals._lastAutoRefineReviewAt).toBeGreaterThan(0);
	});

	it("re-plans at most once: a second stale-evidence rejection closes the round", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		internals._assistantTurnsSinceAutoRefine = 30;
		internals._lastAutoRefineReviewAt = 0;
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("tool_not_registered")),
			fauxAssistantMessage(judgeReply("fail")),
			driftingPlanner(harness, memoryPlan("tool_still_not_registered"), undefined, `${MARKER} again`),
			fauxAssistantMessage(judgeReply("fail")),
		]);

		await internals._runApprovedRefine("turn_interval", REVIEW);
		const firstId = spansNamed("refine.plan")[0]!.attrs["refinement.id"] as string;
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2));
		await settle();

		expect(spansNamed("refine.plan")).toHaveLength(2);
		expect(harness.getPendingResponseCount()).toBe(0);
		const rejected = outcomeLines().filter((line) => line.msg === "refinement.rejected");
		expect(rejected).toHaveLength(2);
		expect(rejected[1]).toMatchObject({ staleEvidence: true, replanScheduled: false, replanOf: firstId });
		expect(spansNamed("refine.apply")[1]!.attrs).toMatchObject({
			"refine.stale_evidence": true,
			"refine.replan_scheduled": false,
			"refine.replan_of": firstId,
		});
		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(internals._assistantTurnsSinceAutoRefine).toBe(0);
		expect(internals._lastAutoRefineReviewAt).toBeGreaterThan(0);

		// Only an automatic, agent-requested or failure refine earns one, and never a re-plan or a rollback.
		expect(internals._armStaleEvidenceReplan("refine_x", { replanOf: firstId }, "auto")).toBe(false);
		expect(internals._armStaleEvidenceReplan("refine_x", { rollbackId: firstId }, "self")).toBe(false);
		expect(internals._armStaleEvidenceReplan("refine_x", {}, "user")).toBe(false);
		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(internals._armStaleEvidenceReplan("refine_x", { reason: "refine_run" }, "self")).toBe(true);
		expect(internals._armStaleEvidenceReplan("refine_y", { reason: "refine_run" }, "self")).toBe(false);
		expect(internals._staleEvidenceReplan?.options.replanOf).toBe("refine_x");
	});

	it("launches an armed re-plan while session work is queued", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		const hold = holdLaunchOnComplete(harness);
		const firstId = await staleIntervalRound(harness);
		await settle();
		expect(internals._staleEvidenceReplan?.options.replanOf).toBe(firstId);
		expect(internals._staleEvidenceReplan?.run).toBeUndefined();
		expect(spansNamed("refine.plan")).toHaveLength(1);

		hold.release();
		harness.session.agent.hasQueuedMessages = () => true;
		harness.appendResponses([fauxAssistantMessage(EMPTY_PLAN)]);
		internals._scheduleStaleEvidenceReplanLaunch();
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2));

		expect(spansNamed("refine.plan")[1]!.attrs).toMatchObject({ "refine.replan_of": firstId });
		expect(spansNamed("refine.apply")[1]!.attrs).toMatchObject({ "refine.decision": "no_edits" });
	});

	it("holds a recurrence refine's fingerprint through a stale-evidence rejection and releases it when the re-plan is dropped", async () => {
		const harness = await persistedSession([deployTool()]);
		const internals = internalsOf(harness);
		const hold = holdLaunchOnComplete(harness);
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("first done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("second done"),
			driftingPlanner(harness, memoryPlan("deploy_manifest")),
			fauxAssistantMessage(judgeReply("pass")),
		]);
		await harness.session.prompt("two");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		await settle();

		expect(spansNamed("refine.apply")[0]!.attrs).toMatchObject({
			"refine.decision": "reject_unclaimed",
			"refine.stale_evidence": true,
			"refine.replan_scheduled": true,
		});
		expect(spansNamed("refine.plan")[0]!.attrs).toMatchObject({ "refine.reason": "recurrence" });
		expect(internals._failureRefineTriggered.has(`recurrence:${DEPLOY_FINGERPRINT}`)).toBe(true);
		expect(internals._staleEvidenceReplan?.source).toBe("self");
		expect(internals._staleEvidenceReplan?.options.triggerFingerprintIds).toEqual([DEPLOY_FINGERPRINT]);
		expect(internals._staleEvidenceReplan?.run).toBeUndefined();
		const failuresBefore = harness.eventsOfType("refine_failed").length;

		harness.session.requestAbort();
		hold.release();

		expect(harness.eventsOfType("refine_failed").slice(failuresBefore)).toEqual([
			{ type: "refine_failed", error: REFINE_CANCELLED },
		]);
		expect(internals._failureRefineTriggered.has(`recurrence:${DEPLOY_FINGERPRINT}`)).toBe(false);
		expect(internals._staleEvidenceReplan).toBeUndefined();
		await settle();
		expect(spansNamed("refine.plan")).toHaveLength(1);
	});

	it("re-plans a stale serialized background plan at the same checkpoint and keeps its failure triggers spent", async () => {
		const reviewer = vi.fn(async () => REVIEW);
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			serializedRefine: true,
			settings: { autoRefine: { enabled: true, turnInterval: 1, cooldownMs: 0 } },
			autoRefineReviewer: reviewer,
		});
		harnesses.push(harness);
		const internals = internalsOf(harness);
		const replanPrompts: string[] = [];
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("tool_not_registered")),
			fauxAssistantMessage(judgeReply("fail")),
			capturing(memoryPlan("register_build_tool", "Register the build tool with `tools add`."), replanPrompts),
			fauxAssistantMessage(judgeReply("pass")),
		]);
		internals._assistantTurnsSinceAutoRefine = 1;

		internals._maybeStartSerializedBackgroundPlan();
		await internals._runSerializedRefineCheckpoint();

		expect(reviewer).toHaveBeenCalledTimes(1);
		const plans = spansNamed("refine.plan");
		expect(plans).toHaveLength(2);
		const firstId = plans[0]!.attrs["refinement.id"];
		expect(plans[1]!.attrs).toMatchObject({ "refine.replan_of": firstId, "refine.source": "auto" });
		expect(spansNamed("refine.apply").map((span) => span.attrs["refine.decision"])).toEqual([
			"reject_deep",
			"commit_unmeasured",
		]);
		expect(section(replanPrompts[0], "conversation")).toContain(MARKER);
		expect(internals._assistantTurnsSinceAutoRefine).toBe(0);
		expect(internals._serializedPlanInFlight).toBeUndefined();
		expect(internals._staleEvidenceReplan).toBeUndefined();

		// A queued failure repair whose re-plan completes keeps its fingerprint spent.
		const fingerprint = "feedfacefeedface";
		internals._failureRefineTriggered.add(`recurrence:${fingerprint}`);
		internals._pendingRequestedRefine = {
			instructions: "repair the recurring failure",
			reason: "recurrence",
			kind: "failure",
			triggerFingerprintIds: [fingerprint],
		};
		harness.setResponses([
			driftingPlanner(harness, memoryPlan("repair_note"), undefined, `${MARKER} for the repair`),
			fauxAssistantMessage(judgeReply("pass")),
			fauxAssistantMessage(EMPTY_PLAN),
		]);

		await internals._runSerializedRefineCheckpoint();

		const applies = spansNamed("refine.apply");
		expect(applies.slice(2).map((span) => span.attrs["refine.decision"])).toEqual(["reject_unclaimed", "no_edits"]);
		expect(applies[2]!.attrs).toMatchObject({ "refine.stale_evidence": true, "refine.replan_scheduled": true });
		expect(spansNamed("refine.plan")[3]!.attrs).toMatchObject({
			"refine.source": "self",
			"refine.reason": "recurrence",
			"refine.kind": "failure",
			"refine.replan_of": spansNamed("refine.plan")[2]!.attrs["refinement.id"],
		});
		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(internals._failureRefineTriggered.has(`recurrence:${fingerprint}`)).toBe(true);
	});

	it("drops an armed auto re-plan silently on a branch change and drains one at disposal", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		const hold = holdLaunchOnComplete(harness);
		await staleIntervalRound(harness);
		await settle();
		expect(internals._staleEvidenceReplan).toBeDefined();
		const failuresBefore = harness.eventsOfType("refine_failed").length;

		await internals._invalidatePendingAutoRefineForBranchChange();

		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(harness.eventsOfType("refine_failed")).toHaveLength(failuresBefore);
		await settle();
		expect(spansNamed("refine.plan")).toHaveLength(1);

		const secondId = await staleIntervalRound(harness);
		await settle();
		expect(internals._staleEvidenceReplan?.options.replanOf).toBe(secondId);
		hold.release();
		harness.appendResponses([fauxAssistantMessage(EMPTY_PLAN)]);

		await internals._drainPendingRefinementForDisposal();

		const replans = spansNamed("refine.plan").filter((span) => span.attrs["refine.replan_of"] === secondId);
		expect(replans).toHaveLength(1);
		expect(internals._staleEvidenceReplan).toBeUndefined();
	});

	it("keeps a recurrence refine's fingerprint spent when its re-plan completes", async () => {
		const harness = await persistedSession([deployTool()]);
		const internals = internalsOf(harness);
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("first done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("second done"),
			driftingPlanner(harness, memoryPlan("deploy_manifest")),
			fauxAssistantMessage(judgeReply("pass")),
			fauxAssistantMessage(EMPTY_PLAN),
		]);
		await harness.session.prompt("two");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2), { timeout: 10_000 });
		await vi.waitFor(() => expect(internals._staleEvidenceReplan).toBeUndefined());
		await settle();

		const plans = spansNamed("refine.plan");
		const firstId = plans[0]!.attrs["refinement.id"];
		expect(plans[1]!.attrs).toMatchObject({
			"refine.source": "self",
			"refine.reason": "recurrence",
			"refine.replan_of": firstId,
		});
		expect(
			spansNamed("refine.apply").map((span) => [
				span.attrs["refine.decision"],
				span.attrs["refine.replan_scheduled"],
			]),
		).toEqual([
			["reject_unclaimed", true],
			["no_edits", undefined],
		]);
		expect(internals._failureRefineTriggered.has(`recurrence:${DEPLOY_FINGERPRINT}`)).toBe(true);
		expect(harness.eventsOfType("refine_failed")).toEqual([]);
	});

	it("keeps an armed re-plan through a compaction abort and cancels it on a branch change while it plans", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		const fingerprint = "0badc0de0badc0de";
		internals._failureRefineTriggered.add(`regression:${fingerprint}`);
		expect(
			internals._armStaleEvidenceReplan(
				"refine_prev",
				{ instructions: "repair", reason: "regression", kind: "failure", triggerFingerprintIds: [fingerprint] },
				"self",
			),
		).toBe(true);

		internals._requestAbort(false);

		expect(internals._staleEvidenceReplan?.options.replanOf).toBe("refine_prev");
		expect(internals._failureRefineTriggered.has(`regression:${fingerprint}`)).toBe(true);

		const planner = gatedPlanner(memoryPlan("regression_note"));
		harness.setResponses([planner.step, fauxAssistantMessage(judgeReply("pass"))]);
		internals._scheduleStaleEvidenceReplanLaunch();
		await planner.started;
		expect(internals._staleEvidenceReplan?.run).toBeDefined();
		const failuresBefore = harness.eventsOfType("refine_failed").length;

		const branchChange = internals._invalidatePendingAutoRefineForBranchChange();
		planner.release();
		await branchChange;
		await vi.waitFor(() => expect(internals._staleEvidenceReplan).toBeUndefined());

		expect(internals._failureRefineTriggered.has(`regression:${fingerprint}`)).toBe(false);
		expect(harness.eventsOfType("refine_failed").slice(failuresBefore)).toHaveLength(1);
		expect(spansNamed("refine.plan").map((span) => [span.status, span.attrs["refine.replan_of"]])).toEqual([
			["error", "refine_prev"],
		]);
		expect(spansNamed("refine.apply")).toHaveLength(0);
	});

	it("waits for a re-plan already launched instead of skipping it at a checkpoint or disposal", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		expect(
			internals._armStaleEvidenceReplan("refine_prev", { instructions: "x", reason: "turn_interval" }, "auto"),
		).toBe(true);
		const planner = gatedPlanner(EMPTY_PLAN);
		harness.setResponses([planner.step]);
		internals._scheduleStaleEvidenceReplanLaunch();
		await planner.started;

		let returned = false;
		const runNow = internals._runStaleEvidenceReplanNow().then(() => {
			returned = true;
		});
		await settle();
		expect(returned).toBe(false);
		planner.release();
		await runNow;

		expect(spansNamed("refine.apply").map((span) => span.attrs["refine.replan_of"])).toEqual(["refine_prev"]);
		expect(spansNamed("refine.plan")).toHaveLength(1);
		expect(internals._staleEvidenceReplan).toBeUndefined();
	});

	it("releases a serialized re-plan's failures when a branch change interrupts it, reporting only an agent-requested one", async () => {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			serializedRefine: true,
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 0 } },
		});
		harnesses.push(harness);
		const internals = internalsOf(harness);
		const interruptingPlanner = () => {
			let branchChange: Promise<void> | undefined;
			return {
				step: () => {
					branchChange = internals._invalidatePendingAutoRefineForBranchChange();
					return fauxAssistantMessage(memoryPlan("interrupted"));
				},
				settled: () => branchChange,
			};
		};
		const fingerprint = "facefeedfacefeed";
		internals._failureRefineTriggered.add(`recurrence:${fingerprint}`);
		internals._armStaleEvidenceReplan(
			"refine_self",
			{ instructions: "repair", reason: "recurrence", kind: "failure", triggerFingerprintIds: [fingerprint] },
			"self",
		);
		const selfPlanner = interruptingPlanner();
		harness.setResponses([selfPlanner.step]);

		await internals._runStaleEvidenceReplanNow();
		await selfPlanner.settled();

		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(internals._failureRefineTriggered.has(`recurrence:${fingerprint}`)).toBe(false);
		expect(harness.eventsOfType("refine_failed")).toHaveLength(1);

		expect(
			internals._armStaleEvidenceReplan("refine_auto", { instructions: "x", reason: "turn_interval" }, "auto"),
		).toBe(true);
		const autoPlanner = interruptingPlanner();
		harness.setResponses([autoPlanner.step]);

		await internals._runStaleEvidenceReplanNow();
		await autoPlanner.settled();

		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(harness.eventsOfType("refine_failed")).toHaveLength(1);
		expect(spansNamed("refine.plan").map((span) => [span.status, span.attrs["refine.replan_of"]])).toEqual([
			["error", "refine_self"],
			["error", "refine_auto"],
		]);
	});

	it("keeps an auto trigger for another reason pending while a round waits for its re-plan, without a second review", async () => {
		const reviewer = vi.fn(async () => REVIEW);
		const harness = await persistedSession([], { autoRefineReviewer: reviewer });
		const internals = internalsOf(harness);
		holdLaunchOnComplete(harness);
		await staleIntervalRound(harness);
		await settle();
		expect(internals._staleEvidenceReplan?.run).toBeUndefined();
		internals._compactAutoRefinePending = false;
		internals._turnIntervalAutoRefinePending = false;

		await internals._maybeAutoRefine("compact");
		await internals._maybeAutoRefine("turn_interval");

		expect(internals._compactAutoRefinePending).toBe(true);
		expect(internals._turnIntervalAutoRefinePending).toBe(false);
		expect(reviewer).not.toHaveBeenCalled();
		expect(internals._staleEvidenceReplan?.run).toBeUndefined();
		expect(spansNamed("refine.plan")).toHaveLength(1);
	});

	it("drops an armed auto re-plan silently when auto-refine is turned off before it launches", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		const hold = holdLaunchOnComplete(harness);
		await staleIntervalRound(harness);
		await settle();
		expect(internals._staleEvidenceReplan).toBeDefined();
		const failuresBefore = harness.eventsOfType("refine_failed").length;

		hold.release();
		harness.session.settingsManager.applyOverrides({ autoRefine: { enabled: false } });
		internals._scheduleStaleEvidenceReplanLaunch();
		await settle();

		expect(internals._staleEvidenceReplan).toBeUndefined();
		expect(harness.eventsOfType("refine_failed")).toHaveLength(failuresBefore);
		expect(spansNamed("refine.plan")).toHaveLength(1);
	});

	it("runs a review approved while an agent-requested re-plan held the round once that re-plan settles", async () => {
		const harness = await persistedSession();
		const internals = internalsOf(harness);
		await settle();
		internals._lastAutoRefineReviewAt = 0;
		internals._pendingAutoRefineReview = { reason: "turn_interval", review: REVIEW };
		expect(
			internals._armStaleEvidenceReplan("refine_prev", { instructions: "record it", reason: "refine_run" }, "self"),
		).toBe(true);
		harness.setResponses([
			fauxAssistantMessage(EMPTY_PLAN),
			fauxAssistantMessage(memoryPlan("approved_review")),
			fauxAssistantMessage(judgeReply("pass")),
		]);

		internals._scheduleAutoRefineAfterAgentEnd();
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2));

		expect(
			spansNamed("refine.plan").map((span) => [
				span.attrs["refine.source"],
				span.attrs["refine.reason"],
				span.attrs["refine.replan_of"],
			]),
		).toEqual([
			["self", "refine_run", "refine_prev"],
			["auto", "turn_interval", undefined],
		]);
		await vi.waitFor(() => expect(internals._pendingAutoRefineReview).toBeUndefined());
		expect(internals._turnIntervalAutoRefinePending).toBe(false);
	});

	it("tells the proposer, reviewer and judge overview to record a transient condition only with how to re-check it", async () => {
		const harness = await persistedSession();
		const planner: { system?: string; user?: string } = {};
		const judgePrompts: string[] = [];
		const reviewerPrompts: string[] = [];
		harness.setResponses([
			(context) => {
				planner.system = context.systemPrompt;
				planner.user = userPromptText(context);
				return fauxAssistantMessage(memoryPlan("steady_note"));
			},
			capturing(judgeReply("fail"), judgePrompts),
			capturing(JSON.stringify({ shouldRefine: false, rationale: "nothing" }), reviewerPrompts),
		]);

		await harness.session.refine({ instructions: "capture lessons" });
		await internalsOf(harness)._reviewAutoRefine({ reason: "turn_interval", turnsSinceLastReview: 3 });

		expect(planner.system).toContain(
			"Record a transient condition that a later command can change (an open blocker, a pending rename, a service not yet registered) only together with how to re-check it",
		);
		expect(planner.system).not.toContain("temporary blockers");
		const scope = section(planner.user, "scope_policy");
		expect(scope).toContain(
			"record a transient condition (an open blocker, a pending rename) only with how to re-check it",
		);
		expect(scope).not.toContain("temporary blockers");
		const overview = section(judgePrompts[0], "current_harness_state");
		expect(overview).toContain(
			"record a transient condition (an open blocker, a pending rename) only with how to re-check it",
		);
		expect(overview).not.toContain("temporary blockers");
		expect(reviewerPrompts[0]).toContain("belongs there only with how to re-check it");
		expect(reviewerPrompts[0]).not.toContain("temporary blockers");
	});
});
