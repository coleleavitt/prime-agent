import { join } from "node:path";
import type { AgentMessage, AgentTool } from "@earendil-works/pi-agent-core";
import { type Context, fauxAssistantMessage, fauxToolCall, type ToolResultMessage } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it, vi } from "vitest";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import {
	DEFAULT_RECURRENCE_THRESHOLD,
	emptyFailureLedger,
	extractFailures,
	FAILURE_OPPONENT_PREFIX,
	type FailureLedger,
	type FailureRecord,
	failureOpponentId,
	findProvisionalRegressions,
	fingerprintFailure,
	formatFailureLedgerForPrompt,
	formatRecurrenceRefineInstructions,
	formatRegressionRefineInstructions,
	normalizeFailureLedger,
	normalizeFailureMessage,
	parsePythonTraceback,
	recordProvisionalRegressions,
	recurringFailures,
	updateFailureLedger,
} from "../src/core/ravo/failure-ledger.js";
import type { JsonValue, RavoState } from "../src/core/ravo/reducer.js";
import { getLocalHarnessStateDir, loadHarnessState, saveHarnessState } from "../src/core/refinement/index.js";
import { createHarness, type Harness } from "./suite/harness.js";

function toolResult(text: string, options: { toolName?: string; isError?: boolean } = {}): ToolResultMessage {
	return {
		role: "toolResult",
		toolCallId: "call-1",
		toolName: options.toolName ?? "ipython",
		content: [{ type: "text", text }],
		isError: options.isError ?? false,
		timestamp: 1,
	};
}

const TRACEBACK = `Executing cell...
Traceback (most recent call last):
  File "<ipython-input-3>", line 2, in <module>
    run()
  File "/home/user/.agents/skills/deploy-widget/scripts/run.py", line 40, in run
    return client.fetch(url)
requests.exceptions.ConnectionError: HTTPSConnectionPool(host='api.example.com', port=443): Max retries exceeded with url: /v2/items/1234
`;

const fixedNow = () => "2026-01-01T00:00:00.000Z";

describe("failure ledger extraction", () => {
	it("extracts a Python traceback from a tool result and attributes it to the skill", () => {
		const observations = extractFailures([toolResult(TRACEBACK)], { fromEntryIndex: 0, turn: 3, now: fixedNow });
		expect(observations).toHaveLength(1);
		const [observation] = observations;
		expect(observation.fingerprint.kind).toBe("python_exception");
		expect(observation.fingerprint.exceptionClass).toBe("requests.exceptions.ConnectionError");
		expect(observation.fingerprint.source).toBe("deploy-widget");
		expect(observation.fingerprint.message).toContain("max retries exceeded with url: <path>");
		expect(observation.excerpt).toContain("requests.exceptions.ConnectionError");
		expect(observation.excerpt).toContain('File "/home/user/.agents/skills/deploy-widget/scripts/run.py"');
		expect(observation).toMatchObject({ entryIndex: 0, turn: 3, at: fixedNow() });
	});

	it("uses the tool name as source when no skills frame is present and prefers the traceback over isError", () => {
		const text = `Traceback (most recent call last):
  File "/tmp/x.py", line 1, in <module>
KeyError: 'missing'
`;
		const observations = extractFailures([toolResult(text, { toolName: "bash", isError: true })], {
			fromEntryIndex: 0,
			turn: 1,
		});
		expect(observations).toHaveLength(1);
		expect(observations[0].fingerprint).toMatchObject({
			kind: "python_exception",
			source: "bash",
			exceptionClass: "KeyError",
			message: "?",
		});
	});

	it("accepts a bare exception class line as a fallback", () => {
		const parsed = parsePythonTraceback(`Traceback (most recent call last):
  File "/tmp/x.py", line 1, in <module>
KeyboardInterrupt
`);
		expect(parsed?.exceptionClass).toBe("KeyboardInterrupt");
		expect(parsed?.message).toBe("");
	});

	it("extracts tool results flagged isError as tool_error", () => {
		const observations = extractFailures(
			[toolResult("bash: line 1: cargo: command not found (exit 127)", { toolName: "bash", isError: true })],
			{ fromEntryIndex: 0, turn: 2 },
		);
		expect(observations).toHaveLength(1);
		expect(observations[0].fingerprint).toMatchObject({
			kind: "tool_error",
			source: "bash",
			message: "bash: line #: cargo: command not found (exit #)",
		});
		expect(observations[0].fingerprint.exceptionClass).toBeUndefined();
	});

	it("extracts provider errors from assistant messages with stopReason error and ignores aborted", () => {
		const messages: AgentMessage[] = [
			fauxAssistantMessage("", { stopReason: "error", errorMessage: "429 rate limited: retry after 12s" }),
			fauxAssistantMessage("", { stopReason: "aborted", errorMessage: "aborted by user" }),
			fauxAssistantMessage("fine"),
		];
		const observations = extractFailures(messages, { fromEntryIndex: 0, turn: 5 });
		expect(observations).toHaveLength(1);
		expect(observations[0].fingerprint).toMatchObject({
			kind: "provider_error",
			source: "faux",
			message: "# rate limited: retry after #s",
		});
		expect(observations[0].entryIndex).toBe(0);
	});

	it("only scans entries from fromEntryIndex", () => {
		const messages: AgentMessage[] = [toolResult("boom", { isError: true }), toolResult("boom", { isError: true })];
		expect(extractFailures(messages, { fromEntryIndex: 1, turn: 1 }).map((o) => o.entryIndex)).toEqual([1]);
		expect(extractFailures(messages, { fromEntryIndex: 2, turn: 1 })).toEqual([]);
	});

	it("ignores successful tool results and user messages", () => {
		const messages: AgentMessage[] = [
			{ role: "user", content: [{ type: "text", text: "Traceback (most recent call last): no" }], timestamp: 1 },
			toolResult("ok: 3 files written"),
		];
		expect(extractFailures(messages, { fromEntryIndex: 0, turn: 1 })).toEqual([]);
	});
});

describe("failure fingerprint normalization", () => {
	it("normalizes numbers, quoted strings, paths, hex ids, and whitespace", () => {
		expect(
			normalizeFailureMessage(
				`  [Errno 2] No such file: "/var/tmp/run-42/out.json"   at   0x7ffe1234 id=3f2a9c8e7b6d5a4f   line 17\n`,
			),
		).toBe("[errno #] no such file: ? at <hex> id=<hex> line #");
		expect(normalizeFailureMessage("x".repeat(500))).toHaveLength(200);
	});

	it("yields the same id for the same error with different numbers and paths", () => {
		const a = fingerprintFailure(
			"python_exception",
			"ipython",
			"FileNotFoundError",
			"[Errno 2] No such file or directory: '/tmp/build-1/out.txt'",
		);
		const b = fingerprintFailure(
			"python_exception",
			"ipython",
			"FileNotFoundError",
			"[Errno 2] No such file or directory: '/home/other/build-77/result.txt'",
		);
		expect(a.id).toBe(b.id);
		expect(a.id).toMatch(/^[0-9a-f]{16}$/);
		expect(a).toEqual(b);
	});

	it("yields different ids when kind, source, or class differ", () => {
		const base = fingerprintFailure("tool_error", "bash", undefined, "exit 1");
		expect(fingerprintFailure("tool_error", "ipython", undefined, "exit 1").id).not.toBe(base.id);
		expect(fingerprintFailure("python_exception", "bash", undefined, "exit 1").id).not.toBe(base.id);
		expect(fingerprintFailure("tool_error", "bash", "RuntimeError", "exit 1").id).not.toBe(base.id);
	});

	it("is stable across processes (fixed vector)", () => {
		const fp = fingerprintFailure("tool_error", "bash", undefined, "exit 1");
		expect(fp.id).toBe(fingerprintFailure("tool_error", "bash", undefined, "exit 001").id);
		expect(failureOpponentId(fp)).toBe(`${FAILURE_OPPONENT_PREFIX}${fp.id}`);
		expect(failureOpponentId(failureOpponentId(fp))).toBe(`${FAILURE_OPPONENT_PREFIX}${fp.id}`);
	});
});

function observation(id: string, turn: number, entryIndex = turn) {
	return {
		fingerprint: { id, kind: "tool_error" as const, source: "bash", message: `m-${id}` },
		excerpt: `excerpt ${id} turn ${turn}`,
		entryIndex,
		turn,
		at: `t${turn}`,
	};
}

describe("updateFailureLedger threshold semantics", () => {
	it("reports a fingerprint as newly recurring only in the update where its count crosses the threshold", () => {
		expect(DEFAULT_RECURRENCE_THRESHOLD).toBe(2);
		const first = updateFailureLedger(emptyFailureLedger(), [observation("a", 1)]);
		expect(first.newlyRecurring).toEqual([]);
		expect(first.ledger.failures.a).toMatchObject({ count: 1, firstSeenTurn: 1, lastSeenTurn: 1, firstSeenAt: "t1" });
		expect(first.ledger.lastScannedEntryIndex).toBe(2);

		const second = updateFailureLedger(first.ledger, [observation("a", 4)]);
		expect(second.newlyRecurring.map((r) => r.fingerprint.id)).toEqual(["a"]);
		expect(second.ledger.failures.a).toMatchObject({
			count: 2,
			firstSeenTurn: 1,
			lastSeenTurn: 4,
			firstSeenAt: "t1",
			lastSeenAt: "t4",
			excerpt: "excerpt a turn 4",
		});

		const third = updateFailureLedger(second.ledger, [observation("a", 5)]);
		expect(third.newlyRecurring).toEqual([]);
		expect(third.ledger.failures.a.count).toBe(3);
		expect(first.ledger.failures.a.count).toBe(1);
	});

	it("crosses the threshold within a single batch and honours a custom threshold", () => {
		const batch = updateFailureLedger(emptyFailureLedger(), [observation("b", 2, 3), observation("b", 2, 5)]);
		expect(batch.newlyRecurring.map((r) => r.fingerprint.id)).toEqual(["b"]);
		expect(batch.ledger.lastScannedEntryIndex).toBe(6);

		const strict = updateFailureLedger(emptyFailureLedger(), [observation("c", 1), observation("c", 2)], {
			threshold: 3,
		});
		expect(strict.newlyRecurring).toEqual([]);
		const crossed = updateFailureLedger(strict.ledger, [observation("c", 3)], { threshold: 3 });
		expect(crossed.newlyRecurring.map((r) => r.fingerprint.id)).toEqual(["c"]);
	});

	it("advances lastScannedEntryIndex to scannedThroughEntryIndex even without observations", () => {
		const updated = updateFailureLedger(emptyFailureLedger(), [], { scannedThroughEntryIndex: 9 });
		expect(updated.ledger.lastScannedEntryIndex).toBe(9);
		expect(updated.newlyRecurring).toEqual([]);
		const later = updateFailureLedger(updated.ledger, [observation("d", 1, 2)], { scannedThroughEntryIndex: 4 });
		expect(later.ledger.lastScannedEntryIndex).toBe(9);
	});

	it("lists recurring failures sorted by count desc then lastSeenTurn desc", () => {
		let ledger: FailureLedger = emptyFailureLedger();
		for (const [id, turns] of [
			["x", [1, 2]],
			["y", [1, 2, 3]],
			["z", [4, 5]],
			["once", [6]],
		] as const) {
			ledger = updateFailureLedger(
				ledger,
				turns.map((turn) => observation(id, turn)),
			).ledger;
		}
		expect(recurringFailures(ledger).map((r) => r.fingerprint.id)).toEqual(["y", "z", "x"]);
		expect(recurringFailures(ledger, 3).map((r) => r.fingerprint.id)).toEqual(["y"]);
	});

	it("round-trips through normalizeFailureLedger and drops malformed records", () => {
		const ledger = updateFailureLedger(emptyFailureLedger(), [observation("a", 1)]).ledger;
		expect(normalizeFailureLedger(JSON.parse(JSON.stringify(ledger)))).toEqual(ledger);
		expect(normalizeFailureLedger(undefined)).toEqual(emptyFailureLedger());
		expect(
			normalizeFailureLedger({ schema: 1, lastScannedEntryIndex: 3, failures: { bad: { count: 2 }, worse: 7 } }),
		).toEqual({ schema: 1, failures: {}, lastScannedEntryIndex: 3 });
	});
});

describe("formatFailureLedgerForPrompt", () => {
	const record = (id: string, count: number, extra: Partial<FailureRecord["fingerprint"]> = {}): FailureRecord => ({
		fingerprint: {
			id,
			kind: "python_exception",
			source: "ipython",
			exceptionClass: "KeyError",
			message: "?",
			...extra,
		},
		count,
		firstSeenTurn: 2,
		lastSeenTurn: 7,
		firstSeenAt: "t2",
		lastSeenAt: "t7",
		excerpt: "  File \"x.py\", line 1\nKeyError: 'missing'",
		addressedByProposalIds: [],
	});

	it("formats one line per record with the opponent id, kind, source, class, count, turns and excerpt", () => {
		const text = formatFailureLedgerForPrompt([record("abc", 3)]);
		expect(text).toContain("- failure:abc [python_exception] source=ipython class=KeyError count=3 turns=2..7");
		expect(text).toContain("File \"x.py\", line 1 KeyError: 'missing'");
	});

	it("returns None. for an empty list and truncates to the limit", () => {
		expect(formatFailureLedgerForPrompt([])).toBe("None.");
		const text = formatFailureLedgerForPrompt([record("a", 2), record("b", 2), record("c", 2)], 2);
		expect(text).toContain("failure:a");
		expect(text).toContain("failure:b");
		expect(text).not.toContain("failure:c");
		expect(text).toContain("... 1 more");
	});

	it("names the fingerprints in the recurrence and regression refine instructions", () => {
		const recurrence = formatRecurrenceRefineInstructions([record("abc", 2)]);
		expect(recurrence).toContain("triggered by recurrence");
		expect(recurrence).toContain("addressedFingerprints");
		expect(recurrence).toContain("failure:abc");
		const regression = formatRegressionRefineInstructions(
			[{ championId: "prop-1", fingerprints: ["abc"], committedTurn: 3, untilTurn: 23 }],
			[record("abc", 4)],
		);
		expect(regression).toContain("triggered by regression");
		expect(regression).toContain("proposal prop-1 (window turns 3..23) claimed to address: failure:abc");
		expect(regression).toContain("count=4");
	});
});

describe("provisional regression detection", () => {
	function ravoState(): RavoState<JsonValue> {
		return {
			lineage: [
				{ proposalId: "old", parentId: null, score: 1, artifact: null, missedCriterionIds: [] },
				{
					proposalId: "prov",
					parentId: "old",
					score: 2,
					artifact: null,
					missedCriterionIds: [],
					claimedFingerprints: ["abc", "def"],
					provisional: { committedTurn: 10, untilTurn: 30 },
				},
			],
			championId: "prov",
			opponents: { criteria: [] },
			evaluatedProposalIds: ["old", "prov"],
		};
	}

	it("detects a claimed fingerprint recurring inside the window and records it without touching lineage order", () => {
		const regressions = findProvisionalRegressions(ravoState(), ["def", "zzz"], 15);
		expect(regressions).toEqual([{ championId: "prov", fingerprints: ["def"], committedTurn: 10, untilTurn: 30 }]);
		const recorded = recordProvisionalRegressions(ravoState(), regressions, 15);
		expect(recorded.lineage.map((c) => c.proposalId)).toEqual(["old", "prov"]);
		expect(recorded.lineage[1].provisional).toEqual({
			committedTurn: 10,
			untilTurn: 30,
			observedRecurrence: { turn: 15, fingerprints: ["def"] },
		});
		expect(recorded.lineage[1].score).toBe(2);
	});

	it("ignores recurrences outside the window, unclaimed fingerprints, and non-provisional champions", () => {
		expect(findProvisionalRegressions(ravoState(), ["abc"], 31)).toEqual([]);
		expect(findProvisionalRegressions(ravoState(), ["abc"], 9)).toEqual([]);
		expect(findProvisionalRegressions(ravoState(), ["zzz"], 15)).toEqual([]);
		expect(findProvisionalRegressions(undefined, ["abc"], 15)).toEqual([]);
		const legacy = ravoState();
		legacy.lineage = [legacy.lineage[0]];
		expect(findProvisionalRegressions(legacy, ["abc"], 15)).toEqual([]);
	});
});

describe("agent-session turn-boundary hook", () => {
	const harnesses: Harness[] = [];
	const previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
		else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	});

	function flakyTool(): AgentTool {
		return {
			name: "flaky",
			label: "Flaky",
			description: "Always fails",
			parameters: Type.Object({}),
			execute: async () => {
				throw new Error("cannot connect to db host at port 5432 (attempt 1)");
			},
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

	function emptyPlan(): string {
		return JSON.stringify({ summary: "noop", rationale: "none", expectedOutcome: "none", edits: [] });
	}

	it("records failures per turn, triggers one deduped recurrence refine, and persists the ledger locally", async () => {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools: [flakyTool()],
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const expectedId = fingerprintFailure(
			"tool_error",
			"flaky",
			undefined,
			"cannot connect to db host at port 9 (attempt 2)",
		).id;

		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("flaky", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("first done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();

		const afterFirst = loadHarnessState(localDir, "local").failures;
		// A tool failure is observed at the next assistant boundary: turn 2 of the branch.
		expect(afterFirst?.failures[expectedId]).toMatchObject({ count: 1, firstSeenTurn: 2, lastSeenTurn: 2 });
		expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
		expect(harness.eventsOfType("refine_complete")).toHaveLength(0);

		const plannerPrompts: string[] = [];
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("flaky", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("second done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(emptyPlan());
			},
		]);
		await harness.session.prompt("two");
		await vi.waitFor(() => expect(harness.eventsOfType("refine_complete")).toHaveLength(1), { timeout: 5000 });

		expect(plannerPrompts).toHaveLength(1);
		expect(plannerPrompts[0]).toContain("Automatic refine triggered by recurrence");
		expect(plannerPrompts[0]).toContain(`failure:${expectedId}`);
		expect(plannerPrompts[0]).toContain("count=2");
		const afterSecond = loadHarnessState(localDir, "local").failures;
		expect(afterSecond?.failures[expectedId]).toMatchObject({ count: 2, firstSeenTurn: 2, lastSeenTurn: 4 });

		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("flaky", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("third done"),
		]);
		await harness.session.prompt("three");
		await harness.session.waitForIdle();
		expect(harness.session.handleRefineHostRequest("refine.status")).toMatchObject({ pending: false });
		expect(harness.eventsOfType("refine_complete")).toHaveLength(1);
		expect(harness.getPendingResponseCount()).toBe(0);
		const afterThird = loadHarnessState(localDir, "local").failures;
		expect(afterThird?.failures[expectedId]).toMatchObject({ count: 3, lastSeenTurn: 6 });
		expect(afterThird?.lastScannedEntryIndex).toBe(
			harness.sessionManager.getBranch().filter((entry) => entry.type === "message").length,
		);
	});

	it("triggers a regression refine when a provisional champion's claimed fingerprint recurs in its window", async () => {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools: [flakyTool()],
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const expectedId = fingerprintFailure(
			"tool_error",
			"flaky",
			undefined,
			"cannot connect to db host at port 1 (attempt 1)",
		).id;
		const seeded = loadHarnessState(localDir, "local");
		seeded.ravo = {
			...emptyAssistedRavoState(),
			lineage: [
				{
					proposalId: "prov-1",
					parentId: null,
					score: 3,
					artifact: null,
					missedCriterionIds: [],
					claimedFingerprints: [expectedId],
					provisional: { committedTurn: 0, untilTurn: 20 },
				},
			],
			championId: "prov-1",
			evaluatedProposalIds: ["prov-1"],
		};
		saveHarnessState(localDir, seeded);
		expect(loadHarnessState(localDir, "local").ravo?.lineage[0].claimedFingerprints).toEqual([expectedId]);

		const plannerPrompts: string[] = [];
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("flaky", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(emptyPlan());
			},
		]);
		await harness.session.prompt("one");
		await vi.waitFor(() => expect(harness.eventsOfType("refine_complete")).toHaveLength(1), { timeout: 5000 });

		expect(plannerPrompts[0]).toContain("Automatic refine triggered by regression");
		expect(plannerPrompts[0]).toContain(
			`proposal prov-1 (window turns 0..20) claimed to address: failure:${expectedId}`,
		);
		const state = loadHarnessState(localDir, "local");
		expect(state.failures?.failures[expectedId]?.count).toBe(1);
		expect(state.ravo?.lineage[0].provisional).toEqual({
			committedTurn: 0,
			untilTurn: 20,
			observedRecurrence: { turn: 2, fingerprints: [expectedId] },
		});
	});
});
