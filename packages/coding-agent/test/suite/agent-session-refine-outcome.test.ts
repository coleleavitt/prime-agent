import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import {
	addSpanSink,
	type Context,
	fauxAssistantMessage,
	fauxToolCall,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
} from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentSession } from "../../src/core/agent-session.js";
import { REFINEMENT_LOG_COMPONENT } from "../../src/core/learning-index.js";
import {
	type FailureRecord,
	fingerprintFailure,
	GLOBAL_FAILURE_LEDGER_ENV,
	observationOrdinal,
	recordProvisionalRegressions,
} from "../../src/core/ravo/failure-ledger.js";
import {
	applyRefinementProposal,
	getGlobalHarnessStateDir,
	getLocalHarnessStateDir,
	loadHarnessState,
	type RefinementResult,
	saveHarnessState,
} from "../../src/core/refinement/index.js";
import { createHarness, type Harness } from "./harness.js";

const DEPLOY_ERROR = 'manifest validation failed for release "12"';
// Normalization erases quoted text, so this is the same fingerprint; only the raw excerpt says it was a denial.
const DENIED_DEPLOY_ERROR = 'manifest validation failed for release "12: request was not approved"';
const DEPLOY_FINGERPRINT = fingerprintFailure("tool_error", "deploy", undefined, DEPLOY_ERROR).id;
const MISSING_MODULE = "prime_agent_replay_probe_missing_module";
const MISSING_MODULE_TRACEBACK = [
	"Traceback (most recent call last):",
	'  File "<ipython-input-1>", line 1, in <module>',
	`    import ${MISSING_MODULE}`,
	`ModuleNotFoundError: No module named '${MISSING_MODULE}'`,
].join("\n");

type SessionInternals = {
	_replayVerification?: Promise<void>;
	_pendingReplayVerifications: unknown[];
};

const REFINE_CANCELLED = "Refinement cancelled: the session was aborted or changed branch.";
const COMPACTION_SUMMARY = "Compacted: the user asked twice and was answered twice.";

type RefineQueueInternals = {
	_failureRefineTriggered: Set<string>;
	_queueFailureTriggeredRefine(
		instructions: string,
		reason: "recurrence" | "regression",
		fingerprintIds: readonly string[],
		global?: boolean,
	): void;
	_consumePendingRequestedRefine(): boolean;
	_invalidatePendingAutoRefineForBranchChange(): Promise<void>;
};

function failureRecord(source: string, message: string, count: number): FailureRecord {
	return {
		fingerprint: fingerprintFailure("tool_error", source, undefined, message),
		count,
		firstSeenTurn: 1,
		lastSeenTurn: count,
		firstSeenAt: "2026-01-01T00:00:00.000Z",
		lastSeenAt: "2026-01-01T00:05:00.000Z",
		excerpt: message,
		addressedByProposalIds: [],
	};
}

/** Failures other sessions on this machine keep hitting; this session never sees them. */
const UNRELATED_RECURRING = [
	failureRecord("lint", "rule set for the workspace is empty", 3),
	failureRecord("migrate", "schema table absent from the catalog", 2),
	failureRecord("render", "template variable left unbound in the layout", 4),
];

/** Write records into the global failure ledger, as earlier sessions on this machine would have. */
function seedGlobalFailures(records: readonly FailureRecord[]): void {
	const dir = getGlobalHarnessStateDir();
	const state = loadHarnessState(dir, "global");
	const failures = { ...(state.failures?.failures ?? {}) };
	for (const record of records) failures[record.fingerprint.id] = record;
	state.failures = { schema: 1, failures, lastScannedEntryIndex: state.failures?.lastScannedEntryIndex ?? 0 };
	saveHarnessState(dir, state);
}

function failingTool(name: string, message: string): AgentTool {
	return {
		name,
		label: name,
		description: "Always fails",
		parameters: Type.Object({}),
		execute: async () => {
			throw new Error(message);
		},
	};
}

function noopTool(): AgentTool {
	return {
		name: "noop",
		label: "Noop",
		description: "Does nothing",
		parameters: Type.Object({}),
		execute: async () => ({ content: [{ type: "text", text: "ok" }], details: {} }),
	};
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

/** A cell that raised: replay cases derive only from the kernel's own error details on an ipython result. */
function probeTool(): AgentTool {
	return {
		name: "ipython",
		label: "IPython",
		description: "Returns a traceback",
		parameters: Type.Object({}),
		execute: async () => ({
			content: [{ type: "text", text: MISSING_MODULE_TRACEBACK }],
			details: {
				status: "error",
				errorEname: "ModuleNotFoundError",
				error: {
					ename: "ModuleNotFoundError",
					evalue: `No module named '${MISSING_MODULE}'`,
					traceback: MISSING_MODULE_TRACEBACK.split("\n"),
				},
			},
		}),
	};
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

function planReply(edits: unknown[]): string {
	return JSON.stringify({ summary: "Deploy note", rationale: "evidence", expectedOutcome: "fewer failures", edits });
}

const MEMORY_EDIT = {
	action: "create",
	kind: "memory",
	id: "deploy_manifest",
	title: "Deploy manifest",
	content: "Validate the manifest before deploying.",
};

function judgeReply(addressedFingerprints: string[]): string {
	return JSON.stringify({
		verdict: "pass",
		score: 90,
		failedCriteria: [],
		addressedFingerprints,
		rationale: "judged",
	});
}

function resolvePython(): string | undefined {
	for (const candidate of ["python3", "python"]) {
		const probe = spawnSync(candidate, ["-c", "import sys; sys.stdout.write(sys.executable)"], { encoding: "utf8" });
		if (probe.status === 0 && probe.stdout.trim()) return probe.stdout.trim();
	}
	return undefined;
}

describe("AgentSession refine outcomes, spans, and failure wiring", () => {
	const harnesses: Harness[] = [];
	const tempDirs: string[] = [];
	const previousEnv = {
		agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
		globalLedger: process.env[GLOBAL_FAILURE_LEDGER_ENV],
		kernelPython: process.env.PRIME_AGENT_KERNEL_PYTHON,
	};
	let spans: SpanEndRecord[];
	let logs: LogEntry[];
	let removeSpanSink: () => void;

	beforeEach(() => {
		// Each test gets its own machine: the global ledger is on by default and
		// would otherwise carry failure counts from one test into the next.
		const agentDir = mkdtempSync(join(tmpdir(), "prime-agent-refine-outcome-"));
		tempDirs.push(agentDir);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
		delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
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
			["PRIME_AGENT_KERNEL_PYTHON", previousEnv.kernelPython],
		] as const) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	});

	const spansNamed = (name: string) => spans.filter((span) => span.name === name);
	const outcomeLines = () => logs.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT);

	async function sessionWith(tools: AgentTool[], autoRefine = true): Promise<Harness> {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools,
			settings: { autoRefine: { enabled: autoRefine, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		return harness;
	}

	async function failDeployTwice(harness: Harness, afterRecurrence: Parameters<Harness["appendResponses"]>[0]) {
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("first done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("second done"),
			...afterRecurrence,
		]);
		await harness.session.prompt("two");
	}

	it.each([
		{
			name: "rejects a failure-triggered proposal that claims nothing",
			addressed: [] as string[],
			decision: "reject_unclaimed",
			applied: 0,
			windowOpened: false,
			line: { msg: "refinement.rejected", decision: "reject_unclaimed", claimed: 0 },
		},
		{
			name: "commits a claimed failure fix as measured and opens its trust window",
			addressed: [DEPLOY_FINGERPRINT],
			decision: "commit",
			applied: 1,
			windowOpened: true,
			line: { msg: "refinement.committed", addressed: [DEPLOY_FINGERPRINT] },
		},
	])("$name", async ({ addressed, decision, applied, windowOpened, line }) => {
		const harness = await sessionWith([deployTool()]);
		const plannerPrompts: string[] = [];
		await failDeployTwice(harness, [
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([MEMORY_EDIT]));
			},
			fauxAssistantMessage(judgeReply(addressed)),
		]);
		// refine.apply ends after refine_complete is emitted, so wait for the span, not the event.
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		expect(harness.eventsOfType("refine_complete")).toHaveLength(1);
		const result: RefinementResult = harness.eventsOfType("refine_complete")[0]!.result;

		expect(plannerPrompts[0]).toContain("Automatic refine triggered by recurrence");
		expect(result.ravo?.decision).toBe(decision);

		const [plan] = spansNamed("refine.plan");
		const [apply] = spansNamed("refine.apply");
		expect(spansNamed("refine.plan")).toHaveLength(1);
		expect(spansNamed("refine.apply")).toHaveLength(1);
		expect(plan).toMatchObject({
			status: "ok",
			parentSpanId: undefined,
			attrs: {
				"refinement.id": result.id,
				"refine.source": "self",
				"refine.reason": "recurrence",
				"refine.kind": "failure",
				"refine.scope": "local",
				"refine.rollback": false,
				"refine.edits": 1,
				"refine.recurring_failures": 1,
				"ravo.decision": decision,
				"ravo.claimed": addressed.length,
				"ravo.measurable": decision === "commit",
				"ravo.judge_error": false,
				"referee.cleared": 0,
				"referee.upheld": 0,
				"referee.unverifiable": 0,
				"referee.no_evidence": 0,
				// A memory edit changes no skill, so a claim is never replayed.
				"referee.not_applicable": addressed.length,
			},
		});
		expect(typeof plan!.attrs["ravo.fast_score"]).toBe("number");
		expect(typeof plan!.attrs["ravo.deep_score"]).toBe("number");
		expect(typeof plan!.attrs["ravo.missed"]).toBe("number");
		expect(typeof plan!.attrs["ravo.missed_weight"]).toBe("number");
		// Planning is detached from the turn that triggered it, which is kept as an attribute.
		expect(plan!.attrs["trigger.trace_id"]).toEqual(expect.any(String));
		expect(plan!.attrs["trigger.trace_id"]).not.toBe(plan!.traceId);
		expect(apply).toMatchObject({
			status: "ok",
			parentSpanId: undefined,
			attrs: {
				"refinement.id": result.id,
				"refine.decision": decision,
				"refine.applied_edits": applied,
				"refine.trust_window_opened": windowOpened,
				"refine.scope": "local",
			},
		});

		expect(outcomeLines()).toEqual([
			expect.objectContaining({ ...line, proposalId: result.id, reason: "recurrence", scope: "local" }),
		]);
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const state = loadHarnessState(localDir, "local");
		if (decision === "commit") {
			expect(state.refinements.at(-1)).toMatchObject({ id: result.id, reason: "recurrence" });
			expect(state.ravo?.lineage[0]?.provisional).toMatchObject({ clock: "ordinal" });
			expect(state.trustWindows?.[result.id]?.outcome).toBe("open");
		} else {
			expect(state.refinements).toEqual([]);
			expect(state.ravo?.lineage).toEqual([]);
			expect(state.ravo?.evaluatedProposalIds).toEqual([result.id]);
		}
	});

	it("applies a directed claimless refine unmeasured, leaving the RAVO state as it was", async () => {
		const harness = await sessionWith([]);
		harness.setResponses([fauxAssistantMessage("hi")]);
		await harness.session.prompt("hello");
		await harness.session.waitForIdle();
		harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);

		const result = await harness.session.refine({ instructions: "capture the deploy lesson" });

		expect(result.ravo).toMatchObject({ decision: "commit", measurable: false });
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.source": "user",
			"refine.reason": "manual",
			"refine.kind": "directed",
			"ravo.measurable": false,
		});
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"refine.applied_edits": 1,
			"refine.trust_window_opened": false,
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: "refinement.applied_unmeasured",
				proposalId: result.id,
				reason: "manual",
				scope: "local",
			}),
		]);
		const state = loadHarnessState(dirnameOf(result.harnessStatePath), "local");
		expect(state.entries.memory.deploy_manifest).toBeDefined();
		expect(state.refinements.at(-1)).toMatchObject({ id: result.id, reason: "manual" });
		expect(state.ravo?.lineage ?? []).toEqual([]);
		expect(state.ravo?.evaluatedProposalIds ?? []).toEqual([]);
	});

	it("ends refine.plan as an error when planning throws, and reports no outcome", async () => {
		const harness = await sessionWith([]);
		harness.setResponses([fauxAssistantMessage("hi")]);
		await harness.session.prompt("hello");
		await harness.session.waitForIdle();

		await expect(harness.session.refine({ instructions: "x" })).rejects.toThrow("Refinement failed");

		expect(spansNamed("refine.plan")).toEqual([
			expect.objectContaining({ status: "error", parentSpanId: undefined, error: expect.any(String) }),
		]);
		expect(spansNamed("refine.apply")).toEqual([]);
		expect(outcomeLines()).toEqual([]);
	});

	it("ends refine.plan ok with refine.skipped when an extension skips the round", async () => {
		const harness = await createHarness({
			persistSession: true,
			extensionFactories: [
				(pi) => {
					pi.on("session_before_refine", async () => ({ skip: true }));
				},
			],
		});
		harnesses.push(harness);

		await expect(harness.session.refine()).rejects.toThrow("Refinement skipped by extension");

		expect(spansNamed("refine.plan")).toEqual([
			expect.objectContaining({ status: "ok", attrs: expect.objectContaining({ "refine.skipped": true }) }),
		]);
		expect(spansNamed("refine.apply")).toEqual([]);
		expect(outcomeLines()).toEqual([]);
	});

	it("detects a regression of a global champion committed by an earlier session, on the ordinal clock", async () => {
		const first = await sessionWith([deployTool()], false);
		await failDeployTwice(first, []);
		await first.session.waitForIdle();
		first.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		const committed = await first.session.refine({ instructions: "fix deploys everywhere", global: true });
		expect(committed.ravo).toMatchObject({ decision: "commit", measurable: true });
		const globalDir = getGlobalHarnessStateDir();
		expect(loadHarnessState(globalDir, "global").ravo?.lineage[0]).toMatchObject({
			proposalId: committed.id,
			claimedFingerprints: [DEPLOY_FINGERPRINT],
			provisional: { committedTurn: 2, untilTurn: 22, clock: "ordinal" },
		});
		harnesses.pop()?.cleanup();
		logs.length = 0;
		spans.length = 0;

		const second = await sessionWith([deployTool()]);
		const plannerPrompts: string[] = [];
		second.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await second.session.prompt("deploy again");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		expect(second.eventsOfType("refine_complete")).toHaveLength(1);

		expect(plannerPrompts[0]).toContain("Automatic refine triggered by regression");
		expect(plannerPrompts[0]).toContain(`proposal ${committed.id} (observation window 2..22)`);
		const globalState = loadHarnessState(globalDir, "global");
		expect(globalState.failures?.failures[DEPLOY_FINGERPRINT]?.count).toBe(3);
		expect(globalState.ravo?.lineage[0]?.provisional).toEqual({
			committedTurn: 2,
			untilTurn: 22,
			clock: "ordinal",
			observedRecurrence: { turn: 3, fingerprints: [DEPLOY_FINGERPRINT] },
		});
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "regression",
			"refine.kind": "failure",
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: "refinement.rejected", decision: "no_edits", reason: "regression" }),
		]);
	});

	it("never regresses a champion on a non-actionable recurrence", async () => {
		const first = await sessionWith([deployTool()], false);
		await failDeployTwice(first, []);
		await first.session.waitForIdle();
		first.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		await first.session.refine({ instructions: "fix deploys everywhere", global: true });
		harnesses.pop()?.cleanup();

		expect(fingerprintFailure("tool_error", "deploy", undefined, DENIED_DEPLOY_ERROR).id).toBe(DEPLOY_FINGERPRINT);
		const second = await sessionWith([
			{
				...deployTool(),
				execute: async () => {
					throw new Error(DENIED_DEPLOY_ERROR);
				},
			},
		]);
		second.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);
		await second.session.prompt("deploy again");
		await second.session.waitForIdle();

		const globalState = loadHarnessState(getGlobalHarnessStateDir(), "global");
		// Counted against the claimed fingerprint, inside the window, yet not held against the champion.
		expect(globalState.failures?.failures[DEPLOY_FINGERPRINT]?.count).toBe(3);
		expect(globalState.ravo?.lineage[0]?.provisional).toEqual({ committedTurn: 2, untilTurn: 22, clock: "ordinal" });
		expect(second.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
		expect(second.eventsOfType("refine_complete")).toHaveLength(0);
	});

	it("does not charge a manual /refine for failures that recur only in other sessions", async () => {
		seedGlobalFailures(UNRELATED_RECURRING);
		const harness = await sessionWith([]);
		harness.setResponses([fauxAssistantMessage("hi")]);
		await harness.session.prompt("hello");
		await harness.session.waitForIdle();
		harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);

		const result = await harness.session.refine({ instructions: "capture the deploy lesson" });

		expect(result.ravo).toMatchObject({
			decision: "commit",
			measurable: false,
			failureOpponents: [],
			missedWeight: 0,
		});
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({ "refine.recurring_failures": 0 });
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"refine.applied_edits": 1,
		});
		expect(
			loadHarnessState(dirnameOf(result.harnessStatePath), "local").entries.memory.deploy_manifest,
		).toBeDefined();
	});

	it("holds a recurrence refine to the fingerprints that triggered it, not everything recurring", async () => {
		seedGlobalFailures(UNRELATED_RECURRING);
		const harness = await sessionWith([deployTool()]);
		await failDeployTwice(harness, [
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		const result: RefinementResult = harness.eventsOfType("refine_complete")[0]!.result;

		expect(result.ravo).toMatchObject({
			decision: "commit",
			measurable: true,
			failureOpponents: [`failure:${DEPLOY_FINGERPRINT}`],
			addressedFingerprints: [DEPLOY_FINGERPRINT],
		});
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.kind": "failure",
			"refine.recurring_failures": 1,
		});
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({ "refine.decision": "commit" });
	});

	it("charges a directed refine for a recurring failure this session hit and did not claim", async () => {
		seedGlobalFailures(UNRELATED_RECURRING);
		const harness = await sessionWith([deployTool()], false);
		await failDeployTwice(harness, []);
		await harness.session.waitForIdle();
		harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);

		const result = await harness.session.refine({ instructions: "note the preferred editor" });

		expect(result.ravo?.failureOpponents).toEqual([`failure:${DEPLOY_FINGERPRINT}`]);
		expect(result.ravo?.missedCriteria).toContain(`failure:${DEPLOY_FINGERPRINT}`);
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.recurring_failures": 1,
			"ravo.missed_weight": 1,
		});
	});

	it("does not charge a directed refine for globally recurring failures this session hit only once", async () => {
		seedGlobalFailures(UNRELATED_RECURRING);
		const [lint, migrate] = UNRELATED_RECURRING;
		const harness = await sessionWith(
			[failingTool("lint", lint!.excerpt), failingTool("migrate", migrate!.excerpt)],
			false,
		);
		harness.setResponses([
			fauxAssistantMessage([fauxToolCall("lint", {}), fauxToolCall("migrate", {})], { stopReason: "toolUse" }),
			fauxAssistantMessage("checked"),
		]);
		await harness.session.prompt("check the workspace");
		await harness.session.waitForIdle();
		const globalFailures = loadHarnessState(getGlobalHarnessStateDir(), "global").failures?.failures;
		expect(globalFailures?.[lint!.fingerprint.id]?.count).toBe(4);
		expect(globalFailures?.[migrate!.fingerprint.id]?.count).toBe(3);
		harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);

		const result = await harness.session.refine({ instructions: "remember tabs" });

		expect(result.ravo).toMatchObject({
			decision: "commit",
			measurable: false,
			failureOpponents: [],
			missedWeight: 0,
		});
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({ "refine.recurring_failures": 0 });
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({ "refine.decision": "commit_unmeasured" });
	});

	it.each([
		{ idleTurns: 20, charged: true },
		{ idleTurns: 21, charged: false },
	])(
		"charges a directed refine for a failure that recurred in this session $idleTurns turns ago: $charged",
		async ({ idleTurns, charged }) => {
			const harness = await sessionWith([deployTool(), noopTool()], false);
			await failDeployTwice(harness, []);
			await harness.session.waitForIdle();
			harness.setResponses([
				...Array.from({ length: idleTurns - 1 }, () =>
					fauxAssistantMessage(fauxToolCall("noop", {}), { stopReason: "toolUse" }),
				),
				fauxAssistantMessage("idle"),
			]);
			await harness.session.prompt("keep working");
			await harness.session.waitForIdle();
			harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);

			const result = await harness.session.refine({ instructions: "note the preferred editor" });

			const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
			expect(loadHarnessState(localDir, "local").failures?.failures[DEPLOY_FINGERPRINT]).toMatchObject({
				count: 2,
				lastSeenTurn: 4,
			});
			expect(result.ravo?.failureOpponents).toEqual(charged ? [`failure:${DEPLOY_FINGERPRINT}`] : []);
			expect(result.ravo?.decision).toBe("commit");
			expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
				"refine.recurring_failures": charged ? 1 : 0,
				"ravo.missed_weight": charged ? 1 : 0,
			});
		},
	);

	it.each([
		{ name: "right after", warmupTurns: 0 },
		{ name: "30 turns after", warmupTurns: 30 },
	])(
		"does not charge a directed refine for failures seen only on the branch it rewound from, $name",
		async ({ warmupTurns }) => {
			const harness = await sessionWith(
				[deployTool(), failingTool("lint", "rule set for the workspace is empty"), noopTool()],
				false,
			);
			if (warmupTurns > 0) {
				harness.setResponses([
					...Array.from({ length: warmupTurns - 1 }, () =>
						fauxAssistantMessage(fauxToolCall("noop", {}), { stopReason: "toolUse" }),
					),
					fauxAssistantMessage("warm"),
				]);
				await harness.session.prompt("warm up");
				await harness.session.waitForIdle();
			}
			for (const label of ["one", "two"]) {
				harness.setResponses([
					fauxAssistantMessage([fauxToolCall("deploy", {}), fauxToolCall("lint", {})], { stopReason: "toolUse" }),
					fauxAssistantMessage(label),
				]);
				await harness.session.prompt(label);
				await harness.session.waitForIdle();
			}
			const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
			expect(loadHarnessState(localDir, "local").failures?.failures[DEPLOY_FINGERPRINT]).toMatchObject({
				count: 2,
				lastSeenTurn: warmupTurns + 4,
			});
			const firstUser = harness.sessionManager
				.getBranch()
				.find((entry) => entry.type === "message" && entry.message.role === "user")!;
			await harness.session.navigateTree(firstUser.id);
			harness.setResponses([fauxAssistantMessage("fresh branch")]);
			await harness.session.prompt("unrelated work");
			await harness.session.waitForIdle();
			harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);

			const result = await harness.session.refine({ instructions: "remember tabs" });

			// Both still recur, but at a later turn than this branch has reached, so they are not recent here.
			expect(result.ravo).toMatchObject({ decision: "commit", failureOpponents: [], missedWeight: 0 });
			expect(result.appliedEdits.filter((edit) => edit.applied)).toHaveLength(1);
			expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({ "refine.recurring_failures": 0 });
		},
	);

	it("measures a recurrence merged into the agent's refine.run against the failure that queued it", async () => {
		// One earlier occurrence elsewhere: this session's single occurrence is what makes it recur.
		seedGlobalFailures([failureRecord("deploy", DEPLOY_ERROR, 1)]);
		let session: AgentSession | undefined;
		const rememberTool: AgentTool = {
			name: "remember",
			label: "Remember",
			description: "Asks for a refine",
			parameters: Type.Object({}),
			execute: async () => {
				const scheduled = session!.handleRefineHostRequest("refine.run", {
					instructions: "Remember that the user prefers tabs.",
				});
				return { content: [{ type: "text", text: JSON.stringify(scheduled) }], details: {} };
			},
		};
		const harness = await sessionWith([deployTool(), rememberTool]);
		session = harness.session;
		const judgePrompts: string[] = [];
		harness.setResponses([
			fauxAssistantMessage([fauxToolCall("remember", {}), fauxToolCall("deploy", {})], { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			(context) => {
				judgePrompts.push(userPromptText(context));
				return fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT]));
			},
		]);
		await harness.session.prompt("go");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		const result: RefinementResult = harness.eventsOfType("refine_complete")[0]!.result;

		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const localState = loadHarnessState(localDir, "local");
		expect(localState.failures?.failures[DEPLOY_FINGERPRINT]?.count).toBe(1);
		expect(loadHarnessState(getGlobalHarnessStateDir(), "global").failures?.failures[DEPLOY_FINGERPRINT]?.count).toBe(
			2,
		);
		expect(judgePrompts[0]).toContain(`candidates: ${DEPLOY_FINGERPRINT}`);
		expect(result.ravo).toMatchObject({
			decision: "commit",
			measurable: true,
			failureOpponents: [`failure:${DEPLOY_FINGERPRINT}`],
			addressedFingerprints: [DEPLOY_FINGERPRINT],
		});
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "recurrence",
			"refine.kind": "directed",
			"refine.recurring_failures": 1,
		});
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit",
			"refine.trust_window_opened": true,
		});
		expect(localState.trustWindows?.[result.id]?.outcome).toBe("open");
	});

	it("runs the agent's local refine.run in local scope beside a queued global repair", async () => {
		let session: AgentSession | undefined;
		const rememberTool: AgentTool = {
			name: "remember",
			label: "Remember",
			description: "Asks for a refine while a global repair is queued",
			parameters: Type.Object({}),
			execute: async () => {
				(session as unknown as RefineQueueInternals)._queueFailureTriggeredRefine(
					"repair the global champion",
					"regression",
					[DEPLOY_FINGERPRINT],
					true,
				);
				// What refine.run(instructions) sends: the skill leaves the key out for global_=False.
				const scheduled = session!.handleRefineHostRequest("refine.run", {
					instructions: "remember this user prefers tabs in this repo",
				});
				return { content: [{ type: "text", text: JSON.stringify(scheduled) }], details: {} };
			},
		};
		const harness = await sessionWith([rememberTool]);
		session = harness.session;
		const plannerPrompts: string[] = [];
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("remember", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("noted"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(
					planReply([{ ...MEMORY_EDIT, id: "tabs_pref", title: "Tabs", content: "The user prefers tabs here." }]),
				);
			},
			fauxAssistantMessage(judgeReply([])),
		]);
		await harness.session.prompt("remember my indentation preference");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2), { timeout: 10_000 });

		expect(plannerPrompts).toHaveLength(2);
		expect(plannerPrompts[0]).toContain("repair the global champion");
		expect(plannerPrompts[0]).toContain("Requested refinement scope: global");
		expect(plannerPrompts[0]).not.toContain("prefers tabs");
		expect(plannerPrompts[1]).toContain("remember this user prefers tabs in this repo");
		expect(plannerPrompts[1]).toContain("Requested refinement scope: local");
		expect(plannerPrompts[1]).not.toContain("repair the global champion");
		expect(
			spansNamed("refine.apply").map((span) => [span.attrs["refine.scope"], span.attrs["refine.decision"]]),
		).toEqual([
			["global", "no_edits"],
			["local", "commit_unmeasured"],
		]);
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		expect(loadHarnessState(localDir, "local").entries.memory.tabs_pref).toBeDefined();
		expect(loadHarnessState(getGlobalHarnessStateDir(), "global").entries.memory.tabs_pref).toBeUndefined();
		expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
	});

	it.each(["requestAbort", "branch change"] as const)(
		"drops a launched refine still waiting behind another on %s, reports it, and frees its failure",
		async (how) => {
			const harness = await sessionWith([], false);
			const internals = harness.session as unknown as RefineQueueInternals;
			internals._failureRefineTriggered.add(`regression:${DEPLOY_FINGERPRINT}`);
			internals._queueFailureTriggeredRefine("repair the local champion", "regression", [DEPLOY_FINGERPRINT]);
			internals._queueFailureTriggeredRefine("repair the global champion", "regression", [DEPLOY_FINGERPRINT], true);
			let releasePlan: () => void = () => {};
			const planGate = new Promise<void>((resolve) => {
				releasePlan = resolve;
			});
			let planStarted: () => void = () => {};
			const started = new Promise<void>((resolve) => {
				planStarted = resolve;
			});
			const plannerPrompts: string[] = [];
			harness.setResponses([
				async (context) => {
					plannerPrompts.push(userPromptText(context));
					planStarted();
					await planGate;
					return fauxAssistantMessage(planReply([MEMORY_EDIT]));
				},
				(context) => {
					plannerPrompts.push(userPromptText(context));
					return fauxAssistantMessage(planReply([MEMORY_EDIT]));
				},
				fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
			]);
			const runs: Promise<RefinementResult>[] = [];
			const refine = harness.session.refine.bind(harness.session);
			vi.spyOn(harness.session, "refine").mockImplementation((...args) => {
				const run = refine(...args);
				runs.push(run);
				return run;
			});
			const failed: string[] = [];
			harness.session.subscribe((event) => {
				if (event.type === "refine_failed") failed.push(event.error);
			});

			expect(internals._consumePendingRequestedRefine()).toBe(true);
			expect(runs).toHaveLength(2);
			await started;
			let invalidated: Promise<void> | undefined;
			if (how === "requestAbort") harness.session.requestAbort();
			else invalidated = internals._invalidatePendingAutoRefineForBranchChange();
			releasePlan();
			await invalidated;
			const settled = await Promise.allSettled(runs);

			expect(plannerPrompts).toHaveLength(1);
			expect(plannerPrompts[0]).toContain("repair the local champion");
			expect(settled.map((run) => run.status)).toEqual(["rejected", "rejected"]);
			expect(String((settled[1] as PromiseRejectedResult).reason)).toContain("Refinement cancelled");
			expect(spansNamed("refine.plan")).toHaveLength(1);
			expect(spansNamed("refine.apply")).toEqual([]);
			// The refine that was planning reports its abort, and the dropped one its cancellation.
			await vi.waitFor(() => expect(failed).toHaveLength(2));
			expect(failed[1]).toBe(REFINE_CANCELLED);
			// Neither repaired anything, so the next regression of the fingerprint queues a repair again.
			expect(internals._failureRefineTriggered.has(`regression:${DEPLOY_FINGERPRINT}`)).toBe(false);
			expect(loadHarnessState(getGlobalHarnessStateDir(), "global").entries.memory.deploy_manifest).toBeUndefined();
		},
	);

	it.each([
		{ compaction: "fails", summarize: false },
		{ compaction: "succeeds", summarize: true },
	])(
		"keeps a launched refine waiting behind another when a manual compaction $compaction during the plan",
		async ({ summarize }) => {
			const harness = await createHarness({
				persistSession: true,
				rlmDepth: 0,
				settings: {
					autoRefine: { enabled: false, turnInterval: 25, cooldownMs: 20 * 60_000 },
					compaction: summarize ? { keepRecentTokens: 1 } : {},
				},
			});
			harnesses.push(harness);
			const internals = harness.session as unknown as RefineQueueInternals;
			let releasePlan: () => void = () => {};
			const planGate = new Promise<void>((resolve) => {
				releasePlan = resolve;
			});
			let planStarted: () => void = () => {};
			const started = new Promise<void>((resolve) => {
				planStarted = resolve;
			});
			let releaseSummary: () => void = () => {};
			const summaryGate = new Promise<void>((resolve) => {
				releaseSummary = resolve;
			});
			const order: string[] = [];
			const plannerPrompts: string[] = [];
			const responder = async (context: Context) => {
				const text = userPromptText(context);
				if (text.includes("Requested refinement scope")) {
					plannerPrompts.push(text);
					order.push(`plan ${plannerPrompts.length}`);
					if (plannerPrompts.length === 1) {
						planStarted();
						await planGate;
					}
					return fauxAssistantMessage(planReply([]));
				}
				if (order.length === 0) return fauxAssistantMessage(`reply ${text.length}`);
				order.push("summary");
				await summaryGate;
				return fauxAssistantMessage(COMPACTION_SUMMARY);
			};
			harness.setResponses(Array.from({ length: 12 }, () => responder));
			harness.session.subscribe((event) => {
				if (event.type === "compaction_end") order.push("compaction_end");
			});
			await harness.session.prompt("one");
			await harness.session.prompt("two");
			await harness.session.waitForIdle();
			const runs: Promise<RefinementResult>[] = [];
			const refine = harness.session.refine.bind(harness.session);
			vi.spyOn(harness.session, "refine").mockImplementation((...args) => {
				const run = refine(...args);
				runs.push(run);
				return run;
			});
			const failed: string[] = [];
			harness.session.subscribe((event) => {
				if (event.type === "refine_failed") failed.push(event.error);
			});
			internals._failureRefineTriggered.add(`regression:${DEPLOY_FINGERPRINT}`);
			internals._queueFailureTriggeredRefine("repair the local champion", "regression", [DEPLOY_FINGERPRINT]);
			internals._queueFailureTriggeredRefine("repair the global champion", "regression", [DEPLOY_FINGERPRINT], true);
			order.push("launched");
			expect(internals._consumePendingRequestedRefine()).toBe(true);
			await started;

			const compaction = harness.session.compact().then(
				() => "compacted",
				(error: unknown) => `failed: ${String(error)}`,
			);
			if (summarize) await vi.waitFor(() => expect(order).toContain("summary"));
			releasePlan();
			// The aborted plan settles while the compaction is still summarizing; the waiting refine must not plan yet.
			await expect(runs[0]).rejects.toThrow();
			// The cancelled refine does not free the failure the refine still waiting behind it repairs.
			expect(internals._failureRefineTriggered.has(`regression:${DEPLOY_FINGERPRINT}`)).toBe(true);
			await new Promise((resolve) => setTimeout(resolve, 20));
			releaseSummary();
			const compacted = await compaction;
			const settled = await Promise.allSettled(runs);

			expect(compacted).toMatch(summarize ? /^compacted$/ : /too short to compact/);
			expect(settled.map((run) => run.status)).toEqual(["rejected", "fulfilled"]);
			expect(plannerPrompts).toHaveLength(2);
			expect(plannerPrompts[1]).toContain("repair the global champion");
			expect(order.indexOf("plan 2")).toBeGreaterThan(order.indexOf("compaction_end"));
			if (summarize) expect(plannerPrompts[1]).toContain(COMPACTION_SUMMARY);
			expect(spansNamed("refine.apply").map((span) => span.attrs["refine.scope"])).toEqual(["global"]);
			// Only the refine the compaction aborted reports a failure.
			expect(failed).toHaveLength(1);
		},
	);

	it("reports a queued failure refine an abort drops and lets its failure queue a repair again", async () => {
		await commitGlobalDeployChampion();
		let session: AgentSession | undefined;
		const stopTool: AgentTool = {
			name: "stop",
			label: "Stop",
			description: "The user presses escape",
			parameters: Type.Object({}),
			execute: async () => {
				session!.requestAbort();
				return { content: [{ type: "text", text: "stopped" }], details: {} };
			},
		};
		const second = await sessionWith([deployTool(), stopTool]);
		session = second.session;
		const failed: string[] = [];
		second.session.subscribe((event) => {
			if (event.type === "refine_failed") failed.push(event.error);
		});
		second.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			// The regression is queued at the end of this message, before the tool it calls aborts the turn.
			fauxAssistantMessage(fauxToolCall("stop", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("unreachable"),
		]);
		await second.session.prompt("deploy");
		await second.session.waitForIdle();

		expect(failed).toEqual([REFINE_CANCELLED]);
		expect(second.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
		expect(spansNamed("refine.plan")).toEqual([]);

		second.session.resumeQueuedWork();
		const plannerPrompts: string[] = [];
		second.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await second.session.prompt("deploy again");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });

		expect(plannerPrompts[0]).toContain("Automatic refine triggered by regression");
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "regression",
			"refine.scope": "global",
		});
	});

	it("gates the agent's own refine.run as directed when a recurrence merges into it", async () => {
		let session: AgentSession | undefined;
		const rememberTool: AgentTool = {
			name: "remember",
			label: "Remember",
			description: "Asks for a refine",
			parameters: Type.Object({}),
			execute: async () => {
				const scheduled = session!.handleRefineHostRequest("refine.run", {
					instructions: "Remember that the user prefers tabs.",
				});
				return { content: [{ type: "text", text: JSON.stringify(scheduled) }], details: {} };
			},
		};
		const harness = await sessionWith([deployTool(), rememberTool]);
		session = harness.session;
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("first done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();
		const plannerPrompts: string[] = [];
		harness.setResponses([
			fauxAssistantMessage([fauxToolCall("remember", {}), fauxToolCall("deploy", {})], { stopReason: "toolUse" }),
			fauxAssistantMessage("second done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(
					planReply([{ ...MEMORY_EDIT, id: "editor_tabs", title: "Tabs", content: "The user prefers tabs." }]),
				);
			},
			fauxAssistantMessage(judgeReply([])),
		]);
		await harness.session.prompt("two");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });

		expect(plannerPrompts[0]).toContain("Remember that the user prefers tabs.");
		expect(plannerPrompts[0]).toContain("Automatic refine triggered by recurrence");
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "recurrence",
			"refine.kind": "directed",
			"ravo.decision": "commit",
		});
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"refine.applied_edits": 1,
		});
	});

	it("reports an apply-time binding downgrade exactly once, as a reject_deep rejection", async () => {
		const harness = await sessionWith([]);
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const seeded = loadHarnessState(localDir, "local");
		applyRefinementProposal(
			seeded,
			{
				summary: "Seed memory",
				rationale: "seed",
				expectedOutcome: "seeded",
				edits: [{ action: "create", kind: "memory", id: "shared", title: "Shared", content: "planning baseline" }],
			},
			{ id: "seed_shared", scope: "local" },
		);
		saveHarnessState(localDir, seeded);
		let releasePlan: () => void = () => {};
		const planGate = new Promise<void>((resolve) => {
			releasePlan = resolve;
		});
		let planStarted: () => void = () => {};
		const started = new Promise<void>((resolve) => {
			planStarted = resolve;
		});
		harness.setResponses([
			async () => {
				planStarted();
				await planGate;
				return fauxAssistantMessage(
					planReply([{ action: "update", kind: "memory", id: "shared", title: "Shared", content: "planned" }]),
				);
			},
			fauxAssistantMessage(judgeReply([])),
		]);

		const running = harness.session.refine({ instructions: "update shared memory" });
		await started;
		const concurrent = loadHarnessState(localDir, "local");
		concurrent.entries.memory.shared!.content = "concurrent kernel content";
		concurrent.entries.memory.shared!.version++;
		saveHarnessState(localDir, concurrent);
		releasePlan();
		const result = await running;

		expect(result.ravo?.decision).toBe("reject_deep");
		expect(spansNamed("refine.plan")[0]?.attrs["ravo.decision"]).toBe("commit");
		expect(spansNamed("refine.apply")).toEqual([
			expect.objectContaining({
				status: "ok",
				attrs: expect.objectContaining({
					"refinement.id": result.id,
					"refine.decision": "reject_deep",
					"refine.applied_edits": 0,
					"refine.trust_window_opened": false,
				}),
			}),
		]);
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: "refinement.rejected",
				decision: "reject_deep",
				proposalId: result.id,
				reason: "manual",
				scope: "local",
			}),
		]);
		expect(loadHarnessState(localDir, "local").entries.memory.shared?.content).toBe("concurrent kernel content");
	});

	it("reports a gate commit whose edits fail to apply as partial", async () => {
		const harness = await sessionWith([]);
		harness.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT, { action: "delete", kind: "memory", id: "never_created" }])),
			fauxAssistantMessage(judgeReply([])),
		]);

		const result = await harness.session.refine({ instructions: "tidy the deploy notes" });

		// A proposal applies whole or not at all, so one failed edit takes the other down with it.
		expect(result.appliedEdits.map((edit) => edit.error)).toEqual([
			"proposal was not applied because another edit failed",
			"entry not found",
		]);
		expect(spansNamed("refine.plan")[0]?.attrs["ravo.decision"]).toBe("commit");
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "partial",
			"refine.applied_edits": 0,
			"refine.trust_window_opened": false,
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: "refinement.rejected", decision: "partial", proposalId: result.id }),
		]);
	});

	it("reports a rollback as applied unmeasured", async () => {
		const harness = await sessionWith([]);
		harness.setResponses([fauxAssistantMessage(planReply([MEMORY_EDIT])), fauxAssistantMessage(judgeReply([]))]);
		const committed = await harness.session.refine({ instructions: "capture the deploy lesson" });
		spans.length = 0;
		logs.length = 0;

		const rolledBack = await harness.session.refine({ rollbackId: committed.id });

		expect(rolledBack.rollbackOf).toBe(committed.id);
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "rollback",
			"refine.kind": "directed",
			"refine.rollback": true,
		});
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refinement.id": rolledBack.id,
			"refine.decision": "rollback",
			"refine.applied_edits": 1,
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: "refinement.applied_unmeasured",
				proposalId: rolledBack.id,
				reason: "rollback",
			}),
		]);
		expect(
			loadHarnessState(dirnameOf(rolledBack.harnessStatePath), "local").entries.memory.deploy_manifest,
		).toBeUndefined();
	});

	/** Session one fails deploy twice and commits a global champion claiming it. */
	async function commitGlobalDeployChampion(): Promise<RefinementResult> {
		const first = await sessionWith([deployTool()], false);
		await failDeployTwice(first, []);
		await first.session.waitForIdle();
		first.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		const committed = await first.session.refine({ instructions: "fix deploys everywhere", global: true });
		expect(committed.ravo).toMatchObject({ decision: "commit", measurable: true });
		harnesses.splice(harnesses.indexOf(first), 1);
		first.cleanup();
		logs.length = 0;
		spans.length = 0;
		return committed;
	}

	it("repairs a regressed global champion in global scope, where its entries live", async () => {
		const committed = await commitGlobalDeployChampion();
		const second = await sessionWith([deployTool()]);
		const plannerPrompts: string[] = [];
		const repaired = "Validate the manifest and the release tag before deploying.";
		second.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([{ ...MEMORY_EDIT, action: "update", content: repaired }]));
			},
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		await second.session.prompt("deploy again");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });

		expect(plannerPrompts[0]).toContain("Automatic refine triggered by regression");
		expect(plannerPrompts[0]).toContain("Requested refinement scope: global");
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "regression",
			"refine.kind": "failure",
			"refine.scope": "global",
			"refine.recurring_failures": 1,
			"ravo.decision": "commit",
		});
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit",
			"refine.scope": "global",
			"refine.applied_edits": 1,
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: "refinement.committed", reason: "regression", scope: "global" }),
		]);
		const globalState = loadHarnessState(getGlobalHarnessStateDir(), "global");
		expect(globalState.entries.memory.deploy_manifest?.content).toBe(repaired);
		// The recurrence recorded on the regressed champion survives the repair's own lineage write.
		expect(globalState.ravo?.lineage.find((champion) => champion.proposalId === committed.id)?.provisional).toEqual({
			committedTurn: 2,
			untilTurn: 22,
			clock: "ordinal",
			observedRecurrence: { turn: 3, fingerprints: [DEPLOY_FINGERPRINT] },
		});
		const localDir = getLocalHarnessStateDir(second.sessionManager.getSessionArtifactDir())!;
		expect(loadHarnessState(localDir, "local").entries.memory.deploy_manifest).toBeUndefined();
	});

	it("commits a global refine while another session records a regression on the champion it binds", async () => {
		const committed = await commitGlobalDeployChampion();
		const globalDir = getGlobalHarnessStateDir();
		const second = await sessionWith([], false);
		second.setResponses([
			() => {
				// Another process's turn-boundary flush lands while this refine plans.
				const state = loadHarnessState(globalDir, "global");
				state.ravo = recordProvisionalRegressions(
					state.ravo!,
					[{ championId: committed.id, fingerprints: [DEPLOY_FINGERPRINT], committedTurn: 2, untilTurn: 22 }],
					3,
				);
				saveHarnessState(globalDir, state);
				return fauxAssistantMessage(
					planReply([
						{ ...MEMORY_EDIT, id: "release_notes", title: "Release notes", content: "Keep notes short." },
					]),
				);
			},
			fauxAssistantMessage(judgeReply([])),
		]);

		const result = await second.session.refine({ instructions: "remember the release notes style", global: true });

		expect(result.ravo?.decision).toBe("commit");
		expect(result.appliedEdits.every((edit) => edit.applied)).toBe(true);
		const globalState = loadHarnessState(globalDir, "global");
		expect(globalState.entries.memory.release_notes).toBeDefined();
		expect(globalState.ravo?.lineage).toHaveLength(1);
		expect(globalState.ravo?.lineage[0]?.provisional?.observedRecurrence).toEqual({
			turn: 3,
			fingerprints: [DEPLOY_FINGERPRINT],
		});
	});

	it("checks a long-lived session's recurrence against the ordinal other sessions advanced", async () => {
		const noise = "workspace index is stale";
		const longLived = await sessionWith([deployTool(), failingTool("index", noise)]);
		longLived.setResponses([
			fauxAssistantMessage(fauxToolCall("index", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("indexed"),
		]);
		await longLived.session.prompt("index");
		await longLived.session.waitForIdle();

		// Meanwhile other sessions observe 30 failures, then one commits a global champion on the advanced clock.
		const first = await sessionWith([deployTool()], false);
		await failDeployTwice(first, []);
		await first.session.waitForIdle();
		seedGlobalFailures([failureRecord("elsewhere", "cache directory is read-only", 30)]);
		first.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		const committed = await first.session.refine({ instructions: "fix deploys everywhere", global: true });
		const globalDir = getGlobalHarnessStateDir();
		expect(loadHarnessState(globalDir, "global").ravo?.lineage[0]?.provisional).toEqual({
			committedTurn: 33,
			untilTurn: 53,
			clock: "ordinal",
		});
		spans.length = 0;

		const plannerPrompts: string[] = [];
		longLived.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("deployed"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await longLived.session.prompt("deploy");
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });

		expect(plannerPrompts[0]).toContain(`proposal ${committed.id} (observation window 33..53)`);
		expect(loadHarnessState(globalDir, "global").ravo?.lineage[0]?.provisional?.observedRecurrence).toEqual({
			turn: 34,
			fingerprints: [DEPLOY_FINGERPRINT],
		});
		expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
			"refine.reason": "regression",
			"refine.scope": "global",
		});
	});

	it("runs a parked global repair after the local one at the serialized checkpoint", async () => {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			serializedRefine: true,
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		const internals = harness.session as unknown as {
			_queueFailureTriggeredRefine(
				instructions: string,
				reason: "regression",
				fingerprintIds: readonly string[],
				global?: boolean,
			): void;
		};
		internals._queueFailureTriggeredRefine("repair the local champion", "regression", [DEPLOY_FINGERPRINT]);
		internals._queueFailureTriggeredRefine("repair the global champion", "regression", [DEPLOY_FINGERPRINT], true);
		const plannerPrompts: string[] = [];
		const emptyPlanner = (context: Context) => {
			plannerPrompts.push(userPromptText(context));
			return fauxAssistantMessage(planReply([]));
		};
		harness.setResponses([fauxAssistantMessage("hi"), emptyPlanner, emptyPlanner]);

		await harness.session.prompt("hello");
		await harness.session.waitForIdle();
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2), { timeout: 10_000 });

		expect(plannerPrompts.map((prompt) => prompt.includes("repair the global champion"))).toEqual([false, true]);
		expect(spansNamed("refine.apply").map((span) => span.attrs["refine.scope"])).toEqual(["local", "global"]);
		expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
	});

	it("never checks a window on the global ordinal against the local one once the ledger is turned off", async () => {
		seedGlobalFailures([failureRecord("elsewhere", "cache directory is read-only", 30)]);
		const harness = await sessionWith([deployTool(), failingTool("fetch_upstream", "upstream request was aborted")]);
		await failDeployTwice(harness, [
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		await harness.session.waitForIdle();
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const window = { committedTurn: 32, untilTurn: 52, clock: "ordinal" };
		expect(loadHarnessState(localDir, "local").ravo?.lineage[0]?.provisional).toEqual(window);
		spans.length = 0;

		process.env[GLOBAL_FAILURE_LEDGER_ENV] = "0";
		harness.setResponses([
			fauxAssistantMessage(
				Array.from({ length: 40 }, () => fauxToolCall("fetch_upstream", {})),
				{ stopReason: "toolUse" },
			),
			fauxAssistantMessage("fetched"),
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);
		await harness.session.prompt("fetch");
		await harness.session.waitForIdle();
		await harness.session.prompt("deploy again");
		await harness.session.waitForIdle();

		const state = loadHarnessState(localDir, "local");
		// The local ordinal is inside [32, 52], but the window is on the other clock.
		expect(observationOrdinal(state.failures)).toBe(43);
		expect(state.ravo?.lineage[0]?.provisional).toEqual(window);
		expect(spansNamed("refine.plan")).toEqual([]);
		expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
	});

	describe("with the global ledger off", () => {
		beforeEach(() => {
			process.env[GLOBAL_FAILURE_LEDGER_ENV] = "0";
		});

		async function commitLocalDeployFix(harness: Harness): Promise<RefinementResult> {
			await failDeployTwice(harness, [
				fauxAssistantMessage(planReply([MEMORY_EDIT])),
				fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
			]);
			await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
			await harness.session.waitForIdle();
			const committed: RefinementResult = harness.eventsOfType("refine_complete")[0]!.result;
			expect(committed.ravo).toMatchObject({ decision: "commit", measurable: true });
			const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
			// Stamped on the local ledger's ordinal, which advances with every observation in this session.
			expect(loadHarnessState(localDir, "local").ravo?.lineage[0]?.provisional).toEqual({
				committedTurn: 2,
				untilTurn: 22,
				clock: "local-ordinal",
			});
			spans.length = 0;
			return committed;
		}

		it("regresses a local champion inside its window on the local ordinal", async () => {
			const harness = await sessionWith([deployTool()]);
			const committed = await commitLocalDeployFix(harness);
			const plannerPrompts: string[] = [];
			harness.setResponses([
				fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("done"),
				(context) => {
					plannerPrompts.push(userPromptText(context));
					return fauxAssistantMessage(planReply([]));
				},
			]);
			await harness.session.prompt("deploy again");
			await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });

			expect(plannerPrompts[0]).toContain(`proposal ${committed.id} (observation window 2..22)`);
			expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
				"refine.reason": "regression",
				"refine.scope": "local",
			});
			expect(loadHarnessState(getGlobalHarnessStateDir(), "global").failures).toBeUndefined();
		});

		it("lets a local champion's window close after enough unrelated observations", async () => {
			const harness = await sessionWith([
				deployTool(),
				failingTool("fetch_upstream", "upstream request was aborted"),
			]);
			await commitLocalDeployFix(harness);
			harness.setResponses([
				fauxAssistantMessage(
					Array.from({ length: 40 }, () => fauxToolCall("fetch_upstream", {})),
					{ stopReason: "toolUse" },
				),
				fauxAssistantMessage("fetched"),
				fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("done"),
			]);
			await harness.session.prompt("fetch");
			await harness.session.waitForIdle();
			await harness.session.prompt("deploy again");
			await harness.session.waitForIdle();

			const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
			const state = loadHarnessState(localDir, "local");
			expect(state.failures?.failures[DEPLOY_FINGERPRINT]?.count).toBe(3);
			expect(state.ravo?.lineage[0]?.provisional).toEqual({
				committedTurn: 2,
				untilTurn: 22,
				clock: "local-ordinal",
			});
			expect(spansNamed("refine.plan")).toEqual([]);
			expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
		});

		it("does not reopen a window that closed on the local ordinal once the ledger is back on", async () => {
			const harness = await sessionWith([
				deployTool(),
				failingTool("fetch_upstream", "upstream request was aborted"),
			]);
			await commitLocalDeployFix(harness);
			harness.setResponses([
				fauxAssistantMessage(
					Array.from({ length: 25 }, () => fauxToolCall("fetch_upstream", {})),
					{ stopReason: "toolUse" },
				),
				fauxAssistantMessage("fetched"),
			]);
			await harness.session.prompt("fetch");
			await harness.session.waitForIdle();
			const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
			expect(observationOrdinal(loadHarnessState(localDir, "local").failures)).toBe(27);

			delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
			seedGlobalFailures([failureRecord("lint", "rule set for the workspace is empty", 2)]);
			harness.setResponses([
				fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("done"),
			]);
			await harness.session.prompt("deploy again");
			await harness.session.waitForIdle();

			const globalState = loadHarnessState(getGlobalHarnessStateDir(), "global");
			// The global ordinal is inside [2, 22], but the window is on the other clock and closed on its own.
			expect(observationOrdinal(globalState.failures)).toBe(3);
			expect(globalState.failures?.failures[DEPLOY_FINGERPRINT]?.count).toBe(1);
			const state = loadHarnessState(localDir, "local");
			expect(observationOrdinal(state.failures)).toBe(28);
			expect(state.ravo?.lineage[0]?.provisional).toEqual({
				committedTurn: 2,
				untilTurn: 22,
				clock: "local-ordinal",
			});
			expect(spansNamed("refine.plan")).toEqual([]);
			expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
		});

		it("regresses a champion committed with the ledger off, in a session resumed with it on, on the local ordinal", async () => {
			const first = await sessionWith([deployTool()]);
			const committed = await commitLocalDeployFix(first);
			const sessionFile = first.sessionManager.getSessionFile()!;
			const localDir = getLocalHarnessStateDir(first.sessionManager.getSessionArtifactDir())!;
			await first.session.disposeAsync();

			delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
			// The global ordinal is far outside [2, 22]: only the local ordinal places the recurrence in the window.
			seedGlobalFailures([failureRecord("elsewhere", "cache directory is read-only", 200)]);
			const resumed = await createHarness({
				existingSessionFile: sessionFile,
				rlmDepth: 0,
				tools: [deployTool()],
				settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
			});
			harnesses.push(resumed);
			const plannerPrompts: string[] = [];
			resumed.setResponses([
				fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("done"),
				(context) => {
					plannerPrompts.push(userPromptText(context));
					return fauxAssistantMessage(planReply([]));
				},
			]);
			await resumed.session.prompt("deploy again");
			await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });

			expect(observationOrdinal(loadHarnessState(getGlobalHarnessStateDir(), "global").failures)).toBe(201);
			const state = loadHarnessState(localDir, "local");
			expect(observationOrdinal(state.failures)).toBe(3);
			expect(state.ravo?.lineage[0]?.provisional).toEqual({
				committedTurn: 2,
				untilTurn: 22,
				clock: "local-ordinal",
				observedRecurrence: { turn: 3, fingerprints: [DEPLOY_FINGERPRINT] },
			});
			expect(plannerPrompts[0]).toContain(`proposal ${committed.id} (observation window 2..22)`);
			expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
				"refine.reason": "regression",
				"refine.scope": "local",
			});
		});

		it("holds that repair to the failure only its own session counted, so a claimed fix commits", async () => {
			const first = await sessionWith([deployTool()]);
			await commitLocalDeployFix(first);
			const sessionFile = first.sessionManager.getSessionFile()!;
			await first.session.disposeAsync();

			delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
			seedGlobalFailures([failureRecord("elsewhere", "cache directory is read-only", 200)]);
			const resumed = await createHarness({
				existingSessionFile: sessionFile,
				rlmDepth: 0,
				tools: [deployTool()],
				settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
			});
			harnesses.push(resumed);
			const judgePrompts: string[] = [];
			resumed.setResponses([
				fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("done"),
				fauxAssistantMessage(
					planReply([
						{
							...MEMORY_EDIT,
							id: "deploy_manifest_schema",
							title: "Deploy manifest schema",
							content: "Run the manifest schema check before every deploy; release ids are quoted strings.",
						},
					]),
				),
				(context) => {
					judgePrompts.push(userPromptText(context));
					return fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT]));
				},
			]);
			await resumed.session.prompt("deploy again");
			await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
			const result: RefinementResult = resumed.eventsOfType("refine_complete")[0]!.result;

			// It recurs in the session that counted it while the ledger was off, not in the global ledger.
			expect(
				loadHarnessState(getGlobalHarnessStateDir(), "global").failures?.failures[DEPLOY_FINGERPRINT]?.count,
			).toBe(1);
			expect(judgePrompts[0]).toContain(`candidates: ${DEPLOY_FINGERPRINT}`);
			expect(result.ravo).toMatchObject({
				decision: "commit",
				measurable: true,
				failureOpponents: [`failure:${DEPLOY_FINGERPRINT}`],
				addressedFingerprints: [DEPLOY_FINGERPRINT],
			});
			expect(spansNamed("refine.plan")[0]?.attrs).toMatchObject({
				"refine.reason": "regression",
				"refine.kind": "failure",
				"refine.recurring_failures": 1,
			});
			expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
				"refine.decision": "commit",
				"refine.applied_edits": 1,
			});
		});

		it("opens a global window without a clock, so nothing ever checks it", async () => {
			// Counts carried over from when the ledger was on; with it off nothing advances them.
			seedGlobalFailures([failureRecord("deploy", DEPLOY_ERROR, 2)]);
			const harness = await sessionWith([deployTool()], false);
			await failDeployTwice(harness, []);
			await harness.session.waitForIdle();
			harness.setResponses([
				fauxAssistantMessage(planReply([MEMORY_EDIT])),
				fauxAssistantMessage(judgeReply([DEPLOY_FINGERPRINT])),
			]);

			const committed = await harness.session.refine({ instructions: "fix deploys everywhere", global: true });

			expect(committed.ravo).toMatchObject({ decision: "commit", measurable: true });
			expect(loadHarnessState(getGlobalHarnessStateDir(), "global").ravo?.lineage[0]?.provisional).toEqual({
				committedTurn: 2,
				untilTurn: 22,
			});
		});
	});

	function replayCasesOnDisk(harness: Harness) {
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const records = [
			...Object.values(loadHarnessState(localDir, "local").failures?.failures ?? {}),
			...Object.values(loadHarnessState(getGlobalHarnessStateDir(), "global").failures?.failures ?? {}),
		];
		return records.flatMap((record) => record.replayCases ?? []);
	}

	async function probeOnce(harness: Harness): Promise<SessionInternals> {
		const internals = harness.session as unknown as SessionInternals;
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("ipython", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("probed"),
		]);
		await harness.session.prompt("probe");
		await harness.session.waitForIdle();
		expect(internals._replayVerification).toBeDefined();
		await internals._replayVerification;
		expect(internals._pendingReplayVerifications).toHaveLength(1);
		return internals;
	}

	it.skipIf(resolvePython() === undefined)(
		"verifies a derived replay case off the turn path and merges it at the next flush",
		async () => {
			process.env.PRIME_AGENT_KERNEL_PYTHON = resolvePython();
			const harness = await sessionWith([probeTool()], false);
			const internals = await probeOnce(harness);

			// Recorded at the boundary, verified afterwards: not merged until the next flush.
			expect(replayCasesOnDisk(harness)).toEqual([
				expect.objectContaining({ source: `import ${MISSING_MODULE}`, exceptionClass: "ModuleNotFoundError" }),
				expect.objectContaining({ source: `import ${MISSING_MODULE}`, exceptionClass: "ModuleNotFoundError" }),
			]);
			expect(replayCasesOnDisk(harness).every((replay) => replay.verifiedAt === undefined)).toBe(true);
			const verify = spansNamed("ravo.replay_verify");
			expect(verify).toEqual([
				expect.objectContaining({
					status: "ok",
					parentSpanId: undefined,
					attrs: expect.objectContaining({ "referee.cases": 1, "referee.ran": 1, "referee.verified": 1 }),
				}),
			]);
			expect(spansNamed("ravo.replay_case")[0]?.parentSpanId).toBe(verify[0]!.spanId);

			// The same failure again: the next boundary merges the verification, and the case is not re-run.
			harness.setResponses([
				fauxAssistantMessage(fauxToolCall("ipython", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("probed again"),
			]);
			await harness.session.prompt("probe again");
			await harness.session.waitForIdle();

			const merged = replayCasesOnDisk(harness);
			expect(merged).toHaveLength(2);
			expect(merged.every((replay) => typeof replay.verifiedAt === "string")).toBe(true);
			expect(internals._pendingReplayVerifications).toEqual([]);
			expect(internals._replayVerification).toBeUndefined();
			expect(spansNamed("ravo.replay_verify")).toHaveLength(1);
		},
	);

	it.skipIf(resolvePython() === undefined)(
		"waits for a self-check still running when the session is disposed right after its last turn",
		async () => {
			process.env.PRIME_AGENT_KERNEL_PYTHON = resolvePython();
			const harness = await sessionWith([probeTool()], false);
			const internals = harness.session as unknown as SessionInternals;
			harness.setResponses([
				fauxAssistantMessage(fauxToolCall("ipython", {}), { stopReason: "toolUse" }),
				fauxAssistantMessage("probed"),
			]);
			await harness.session.prompt("probe");
			await harness.session.waitForIdle();
			expect(internals._replayVerification).toBeDefined();

			await harness.session.disposeAsync();

			const merged = replayCasesOnDisk(harness);
			expect(merged).toHaveLength(2);
			expect(merged.every((replay) => typeof replay.verifiedAt === "string")).toBe(true);
		},
	);

	it.skipIf(resolvePython() === undefined)(
		"merges a self-check that finished after the last turn when the session is disposed",
		async () => {
			process.env.PRIME_AGENT_KERNEL_PYTHON = resolvePython();
			const harness = await sessionWith([probeTool()], false);
			await probeOnce(harness);
			expect(replayCasesOnDisk(harness).some((replay) => replay.verifiedAt !== undefined)).toBe(false);

			await harness.session.disposeAsync();

			const merged = replayCasesOnDisk(harness);
			expect(merged).toHaveLength(2);
			expect(merged.every((replay) => typeof replay.verifiedAt === "string")).toBe(true);
		},
	);
});

function dirnameOf(path: string): string {
	return path.replace(/\/[^/]+$/, "");
}
