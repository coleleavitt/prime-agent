import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { AgentMessage, AgentTool } from "@earendil-works/pi-agent-core";
import { type Context, fauxAssistantMessage, fauxToolCall, type ToolResultMessage } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it, vi } from "vitest";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import {
	applyReplayVerifications,
	DEFAULT_RECURRENCE_THRESHOLD,
	emptyFailureLedger,
	extractFailures,
	FAILURE_OPPONENT_PREFIX,
	type FailureKind,
	type FailureLedger,
	type FailureRecord,
	failureOpponentId,
	findProvisionalRegressions,
	fingerprintFailure,
	formatFailureLedgerForPrompt,
	formatRecurrenceRefineInstructions,
	formatRegressionRefineInstructions,
	GLOBAL_FAILURE_LEDGER_ENV,
	isActionableFailure,
	mergeFailureObservations,
	normalizeFailureLedger,
	normalizeFailureMessage,
	observationOrdinal,
	parsePythonTraceback,
	recordProvisionalRegressions,
	recurringFailures,
	updateFailureLedger,
} from "../src/core/ravo/failure-ledger.js";
import type { JsonValue, RavoState } from "../src/core/ravo/reducer.js";
import {
	deriveReplayCase,
	MAX_REPLAY_CASES,
	mergeReplayCase,
	type ReplayCase,
	replayCasesOf,
	replayProbeOf,
	verifiedReplayCasesOf,
} from "../src/core/ravo/referee.js";
import {
	getGlobalHarnessStateDir,
	getHarnessStatePath,
	getLocalHarnessStateDir,
	loadHarnessState,
	saveHarnessState,
} from "../src/core/refinement/index.js";
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
		expect(recurrence).toContain("failure:abc");
		const regression = formatRegressionRefineInstructions(
			[{ championId: "prop-1", fingerprints: ["abc"], committedTurn: 3, untilTurn: 23 }],
			[record("abc", 4)],
		);
		expect(regression).toContain("triggered by regression");
		expect(regression).toContain("proposal prop-1 (observation window 3..23) claimed to address: failure:abc");
		expect(regression).toContain("count=4");
	});

	it("leaves the claim to the evaluator instead of asking the proposer to set addressedFingerprints", () => {
		const recurrence = formatRecurrenceRefineInstructions([record("abc", 2)]);
		const regression = formatRegressionRefineInstructions(
			[{ championId: "prop-1", fingerprints: ["abc"], committedTurn: 3, untilTurn: 23 }],
			[record("abc", 4)],
		);
		for (const text of [recurrence, regression]) {
			expect(text).not.toContain("addressedFingerprints");
			expect(text).toContain("The evaluator decides which fingerprints");
			expect(text).toContain("target the cause");
		}
	});

	it("shows the newest verified replay case and ignores unverified ones", () => {
		const verified: ReplayCase = { language: "python", source: "import a", verifiedAt: "t1" };
		const unverified: ReplayCase = { language: "python", source: "import b" };
		const text = formatFailureLedgerForPrompt([{ ...record("abc", 3), replayCases: [verified, unverified] }]);
		expect(text).toContain("replay=verified");
		expect(text).toContain("replay case (re-run to check the fix): import a");
		expect(text).not.toContain("import b");
		expect(formatFailureLedgerForPrompt([{ ...record("abc", 3), replayCases: [unverified] }])).not.toContain(
			"replay",
		);
	});

	it("never lists a verified case of a retired probe kind as replay evidence", () => {
		const verifiedAt = "2026-09-15T00:00:00.000Z";
		const attribute: ReplayCase = {
			language: "python",
			source: 'import json\ngetattr(json, "load_string")',
			exceptionClass: "AttributeError",
			verifiedAt,
		};
		const executable: ReplayCase = {
			language: "python",
			source: 'import shutil\nif shutil.which("build") is None:\n    raise FileNotFoundError("build")',
			exceptionClass: "FileNotFoundError",
			verifiedAt,
		};
		const name: ReplayCase = {
			language: "python",
			source: "from json import load_string",
			exceptionClass: "ImportError",
			verifiedAt,
		};
		const legacy = { ...record("abc", 3), replayCases: [attribute, executable, name] };
		for (const text of [
			formatFailureLedgerForPrompt([legacy]),
			formatRecurrenceRefineInstructions([legacy]),
			formatRegressionRefineInstructions(
				[{ championId: "prop-1", fingerprints: ["abc"], committedTurn: 3, untilTurn: 23 }],
				[legacy],
			),
		]) {
			expect(text).toContain("- failure:abc [python_exception]");
			expect(text).not.toMatch(/turns=\S+ replay=verified/);
			expect(text).not.toContain("replay case (re-run");
			expect(text).not.toContain("shutil.which");
			expect(text).not.toContain("getattr");
			expect(text).not.toContain("from json import");
		}
		const mixed = formatFailureLedgerForPrompt([
			{ ...legacy, replayCases: [{ language: "python", source: "import a", verifiedAt }, attribute] },
		]);
		expect(mixed).toContain("replay case (re-run to check the fix): import a");
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
					provisional: { committedTurn: 10, untilTurn: 30, clock: "ordinal" },
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
			clock: "ordinal",
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

	it("never regresses a window opened on the per-session turn clock", () => {
		const perSession = ravoState();
		perSession.lineage[1] = { ...perSession.lineage[1], provisional: { committedTurn: 10, untilTurn: 30 } };
		expect(findProvisionalRegressions(perSession, ["abc", "def"], 15)).toEqual([]);
	});
});

function failureRecord(
	kind: FailureKind,
	exceptionClass: string | undefined,
	excerpt: string,
	options: { source?: string; message?: string } = {},
): FailureRecord {
	return {
		fingerprint: fingerprintFailure(kind, options.source ?? "ipython", exceptionClass, options.message ?? excerpt),
		count: 1,
		firstSeenTurn: 1,
		lastSeenTurn: 2,
		firstSeenAt: "t1",
		lastSeenAt: "t2",
		excerpt,
		addressedByProposalIds: [],
	};
}

describe("failure actionability", () => {
	const provider = (source: string, excerpt: string) =>
		failureRecord("provider_error", undefined, excerpt, { source });
	const tool = (source: string, excerpt: string) => failureRecord("tool_error", undefined, excerpt, { source });
	const python = (exceptionClass: string, excerpt: string) =>
		failureRecord("python_exception", exceptionClass, excerpt, { message: excerpt.replace(/^[\w.]+: ?/, "") });

	const nonActionable: Array<[string, FailureRecord]> = [
		[
			"a provider rate limit only the body names",
			failureRecord(
				"provider_error",
				undefined,
				'OpenWebUI request failed: 400 {"detail":"Rate limit exceeded: 10 requests per minute. Please wait before trying again."}',
				{ source: "openwebui", message: 'OpenWebUI request failed: 400 {"detail":"x"}' },
			),
		],
		["a provider 429", provider("anthropic", "429 Too Many Requests")],
		["an overloaded provider", provider("anthropic", '529 {"type":"error","error":{"type":"overloaded_error"}}')],
		[
			"an unavailable provider",
			provider(
				"openwebui",
				"OpenWebUI stream reported an error: litellm.ServiceUnavailableError: BedrockException - internalServerException",
			),
		],
		["a provider 503", provider("openai", "HTTP 503 from upstream")],
		["a provider fetch failure", provider("openwebui", "fetch failed")],
		["a terminated provider stream", provider("openwebui", "terminated")],
		[
			"a provider refusal",
			provider(
				"anthropic",
				"Anthropic refused this request (stop_reason: refusal). The input and any thinking or output tokens produced before the refusal are billed.",
			),
		],
		["a provider content filter", provider("openwebui", "OpenWebUI stream ended with finish_reason=content_filter")],
		[
			"an empty provider completion",
			provider("openwebui", "OpenWebUI returned an empty completion (finish=stop, done=true, usage=true)"),
		],
		["a user denying a fetch", tool("web_fetch", "URL fetch was not approved")],
		["an aborted request", tool("ipython", "Request was aborted")],
		["an aborted operation", tool("web_fetch", "This operation was aborted")],
		["a shut-down kernel", tool("ipython", "Kernel has been shut down")],
		["a remote HTTP 403", tool("web_fetch", "fetch HTTP 403")],
		["a remote HTTP 502", tool("web_fetch", "fetch HTTP 502")],
		["a remote DNS failure", tool("web_fetch", "getaddrinfo ENOTFOUND example.invalid")],
		["a remote fetch failure", tool("web_fetch", "fetch failed")],
		["a TimeoutError", python("TimeoutError", "TimeoutError: timed out")],
		["an asyncio TimeoutError without a message", python("asyncio.TimeoutError", "asyncio.TimeoutError")],
		[
			"a subprocess that timed out",
			python("subprocess.TimeoutExpired", "subprocess.TimeoutExpired: Command 'make' timed out after 10 seconds"),
		],
		["a locked sqlite database", python("sqlite3.OperationalError", "sqlite3.OperationalError: database is locked")],
		["an httpx connect error", python("httpx.ConnectError", "httpx.ConnectError: All connection attempts failed")],
	];

	it.each(nonActionable)("excludes %s", (_label, record) => {
		expect(isActionableFailure(record)).toBe(false);
	});

	const actionable: Array<[string, FailureRecord]> = [
		[
			"bash(timeout=) keyword misuse",
			python("TypeError", "TypeError: bash() got an unexpected keyword argument 'timeout'"),
		],
		[
			"a guessed skill module attribute",
			python("AttributeError", "AttributeError: module 'agent_observe' has no attribute 'recent'"),
		],
		["a missing module", python("ModuleNotFoundError", "ModuleNotFoundError: No module named 'openpyxl'")],
		["a tool error the harness can prevent", tool("flaky", "cannot connect to db host at port 5432 (attempt 1)")],
		[
			"a provider request the harness shaped wrongly",
			provider(
				"perplexity",
				"Perplexity web transport requires exactly one user message and no conversation history",
			),
		],
		[
			"a limit message outside a provider",
			python(
				"RuntimeError",
				"RuntimeError: Target session has too many pending messages: 20 unfinished, limit is 20",
			),
		],
	];

	it.each(actionable)("keeps %s", (_label, record) => {
		expect(isActionableFailure(record)).toBe(true);
	});

	it("counts non-actionable failures but never reports them as recurring", () => {
		const denied = {
			fingerprint: fingerprintFailure("tool_error", "web_fetch", undefined, "URL fetch was not approved"),
			excerpt: "URL fetch was not approved",
			entryIndex: 0,
			turn: 1,
			at: "t1",
		};
		const flaky = {
			fingerprint: fingerprintFailure("tool_error", "flaky", undefined, "cannot connect"),
			excerpt: "cannot connect",
			entryIndex: 1,
			turn: 1,
			at: "t1",
		};
		const first = updateFailureLedger(emptyFailureLedger(), [denied, flaky]);
		const second = updateFailureLedger(first.ledger, [
			{ ...denied, entryIndex: 2 },
			{ ...flaky, entryIndex: 3 },
		]);
		expect(second.ledger.failures[denied.fingerprint.id].count).toBe(2);
		expect(observationOrdinal(second.ledger)).toBe(4);
		expect(second.newlyRecurring.map((item) => item.fingerprint.id)).toEqual([flaky.fingerprint.id]);
		expect(recurringFailures(second.ledger).map((item) => item.fingerprint.id)).toEqual([flaky.fingerprint.id]);
		expect(mergeFailureObservations(first.ledger, [{ ...denied, entryIndex: 2 }]).newlyRecurring).toEqual([]);
	});

	it.each([
		["a remote HTTP 999", "fetch HTTP 999"],
		["a remote redirect", "fetch HTTP 302"],
		["a remote success status", "fetch HTTP 200"],
	])("excludes %s", (_label, excerpt) => {
		expect(isActionableFailure(failureRecord("tool_error", undefined, excerpt, { source: "web_fetch" }))).toBe(false);
	});

	const occurrence = (kind: FailureKind, source: string, excerpt: string, entryIndex: number) => ({
		fingerprint: fingerprintFailure(kind, source, undefined, excerpt),
		excerpt,
		entryIndex,
		turn: entryIndex,
		at: `t${entryIndex}`,
	});

	it("keeps a remote fetch fingerprint out of the gate whatever status its latest occurrence carried", () => {
		const statuses = [403, 404, 403, 503, 404];
		let ledger = updateFailureLedger(
			emptyFailureLedger(),
			statuses.map((status, index) => occurrence("tool_error", "web_fetch", `fetch HTTP ${status}`, index)),
		).ledger;
		const [id] = Object.keys(ledger.failures);
		expect(ledger.failures[id].fingerprint.message).toBe("fetch http #");
		expect(recurringFailures(ledger)).toEqual([]);

		const flipped = updateFailureLedger(ledger, [occurrence("tool_error", "web_fetch", "fetch HTTP 999", 9)]);
		ledger = flipped.ledger;
		expect(ledger.failures[id]).toMatchObject({ count: 6, excerpt: "fetch HTTP 999", nonActionableCount: 6 });
		expect(flipped.newlyRecurring).toEqual([]);
		expect(recurringFailures(ledger)).toEqual([]);
	});

	it("keeps a fingerprint actionable when one timeout hides among nine harness-fixable occurrences", () => {
		// Two bash errors share a prefix longer than the normalized message cap, so they fingerprint identically,
		// and only the raw excerpt of one of them says it timed out.
		const prefix = `bash: build failed: ${"error: cannot find symbol in module layout config ".repeat(5)}`;
		const fixable = occurrence("tool_error", "bash", `${prefix} while compiling target; see log`, 0);
		const timedOut = occurrence("tool_error", "bash", `${prefix} while waiting the request timed out`, 0);
		expect(timedOut.fingerprint.id).toBe(fixable.fingerprint.id);
		expect(isActionableFailure(timedOut)).toBe(false);
		expect(isActionableFailure(fixable)).toBe(true);
		const id = fixable.fingerprint.id;

		let ledger = updateFailureLedger(emptyFailureLedger(), [timedOut]).ledger;
		expect(ledger.failures[id]).toMatchObject({ count: 1, nonActionableCount: 1 });
		expect(recurringFailures(ledger, 1)).toEqual([]);
		for (let index = 1; index < 10; index++) {
			ledger = updateFailureLedger(ledger, [{ ...fixable, entryIndex: index }]).ledger;
		}
		expect(ledger.failures[id]).toMatchObject({ count: 10, nonActionableCount: 1 });
		expect(recurringFailures(ledger).map((record) => record.fingerprint.id)).toEqual([id]);
		const reloaded = normalizeFailureLedger(JSON.parse(JSON.stringify(ledger)));
		expect(reloaded).toEqual(ledger);
		expect(recurringFailures(reloaded).map((record) => record.fingerprint.id)).toEqual([id]);
	});

	it("keeps a fingerprint non-actionable while a strict majority of its occurrences were, across saves and merges", () => {
		const rateLimited = occurrence(
			"provider_error",
			"openwebui",
			'OpenWebUI request failed: 429 {"detail":"rate limit exceeded"}',
			0,
		);
		const notFound = occurrence(
			"provider_error",
			"openwebui",
			'OpenWebUI request failed: 400 {"detail":"model not found"}',
			1,
		);
		expect(notFound.fingerprint.id).toBe(rateLimited.fingerprint.id);
		const id = rateLimited.fingerprint.id;
		expect(isActionableFailure(notFound)).toBe(true);
		expect(isActionableFailure(rateLimited)).toBe(false);

		let ledger = updateFailureLedger(
			emptyFailureLedger(),
			Array.from({ length: 6 }, (_, index) => ({ ...rateLimited, entryIndex: index })),
		).ledger;
		ledger = normalizeFailureLedger(JSON.parse(JSON.stringify(ledger)));
		const merged = mergeFailureObservations(
			ledger,
			Array.from({ length: 4 }, (_, index) => ({ ...notFound, entryIndex: 6 + index })),
		);
		expect(merged.ledger.failures[id]).toMatchObject({ count: 10, nonActionableCount: 6, excerpt: notFound.excerpt });
		expect(merged.newlyRecurring).toEqual([]);
		expect(recurringFailures(merged.ledger)).toEqual([]);
		expect(recurringFailures(normalizeFailureLedger(JSON.parse(JSON.stringify(merged.ledger))))).toEqual([]);

		// Five of ten is not a majority.
		const even = updateFailureLedger(
			emptyFailureLedger(),
			Array.from({ length: 10 }, (_, index) => ({
				...(index % 2 === 0 ? rateLimited : notFound),
				entryIndex: index,
			})),
		);
		expect(even.ledger.failures[id]).toMatchObject({ count: 10, nonActionableCount: 5 });
		expect(recurringFailures(even.ledger).map((record) => record.fingerprint.id)).toEqual([id]);

		// A fingerprint that has only ever classified actionable carries no tally at all.
		const actionable = updateFailureLedger(emptyFailureLedger(), [notFound, { ...notFound, entryIndex: 2 }]);
		expect(actionable.ledger.failures[id].nonActionableCount).toBeUndefined();
		expect(actionable.newlyRecurring.map((record) => record.fingerprint.id)).toEqual([id]);
		expect(normalizeFailureLedger(JSON.parse(JSON.stringify(actionable.ledger)))).toEqual(actionable.ledger);
	});

	it("reads a record written before the tally: the legacy flag counts every occurrence, a stored excerpt counts one", () => {
		const rateLimited = occurrence(
			"provider_error",
			"openwebui",
			'OpenWebUI request failed: 429 {"detail":"rate limit exceeded"}',
			0,
		);
		const notFound = { ...rateLimited, excerpt: 'OpenWebUI request failed: 400 {"detail":"model not found"}' };
		const id = rateLimited.fingerprint.id;
		const legacy = (count: number, excerpt: string, extra: Record<string, unknown> = {}) => ({
			fingerprint: rateLimited.fingerprint,
			count,
			firstSeenTurn: 1,
			lastSeenTurn: 2,
			firstSeenAt: "t1",
			lastSeenAt: "t2",
			excerpt,
			addressedByProposalIds: [],
			...extra,
		});
		const load = (record: Record<string, unknown>) =>
			normalizeFailureLedger({ schema: 1, lastScannedEntryIndex: 0, failures: { [id]: record } });

		const flagged = load(legacy(5, notFound.excerpt, { nonActionable: true })).failures[id];
		expect(flagged).toMatchObject({ count: 5, nonActionableCount: 5 });
		expect(flagged.nonActionable).toBeUndefined();
		expect(load(legacy(3, rateLimited.excerpt)).failures[id]).toMatchObject({ nonActionableCount: 1 });
		expect(load(legacy(3, notFound.excerpt)).failures[id].nonActionableCount).toBeUndefined();
		expect(load(legacy(3, notFound.excerpt, { nonActionableCount: 9 })).failures[id].nonActionableCount).toBe(3);

		// Held in memory without a tally: the stored excerpt is read before an observation overwrites it.
		const inMemory = (excerpt: string): FailureLedger => ({
			schema: 1,
			lastScannedEntryIndex: 0,
			failures: { [id]: legacy(1, excerpt) as FailureRecord },
		});
		const twice = updateFailureLedger(inMemory(rateLimited.excerpt), [{ ...rateLimited, entryIndex: 1 }]).ledger;
		expect(twice.failures[id]).toMatchObject({ count: 2, nonActionableCount: 2 });
		expect(recurringFailures(twice)).toEqual([]);
		const thenFixable = updateFailureLedger(inMemory(rateLimited.excerpt), [{ ...notFound, entryIndex: 1 }]).ledger;
		expect(thenFixable.failures[id]).toMatchObject({ count: 2, nonActionableCount: 1, excerpt: notFound.excerpt });
		expect(normalizeFailureLedger(JSON.parse(JSON.stringify(thenFixable))).failures[id].nonActionableCount).toBe(1);
		expect(
			updateFailureLedger(
				{
					schema: 1,
					lastScannedEntryIndex: 0,
					failures: { [id]: { ...legacy(4, notFound.excerpt), nonActionable: true } },
				},
				[{ ...notFound, entryIndex: 1 }],
			).ledger.failures[id],
		).toMatchObject({ count: 5, nonActionableCount: 4 });
	});

	it("counts every occurrence non-actionable when the fingerprint alone classifies that way, whatever was stored", () => {
		const legacyOf = (observed: ReturnType<typeof occurrence>, count: number, extra: Record<string, unknown> = {}) =>
			normalizeFailureLedger({
				schema: 1,
				lastScannedEntryIndex: 0,
				failures: {
					[observed.fingerprint.id]: {
						fingerprint: observed.fingerprint,
						count,
						firstSeenTurn: 1,
						lastSeenTurn: 2,
						firstSeenAt: "t1",
						lastSeenAt: "t2",
						excerpt: observed.excerpt,
						addressedByProposalIds: [],
						...extra,
					},
				},
			});
		const timedOut = {
			fingerprint: fingerprintFailure("python_exception", "ipython", "TimeoutError", ""),
			excerpt: "TimeoutError",
			entryIndex: 0,
			turn: 0,
			at: "t0",
		};
		for (const [observed, next] of [
			[
				occurrence("tool_error", "web_fetch", "fetch HTTP 403", 0),
				occurrence("tool_error", "web_fetch", "fetch HTTP 999", 1),
			],
			[
				occurrence("provider_error", "anthropic", "429 Too Many Requests", 0),
				occurrence("provider_error", "anthropic", "429 Too Many Requests", 1),
			],
			[
				occurrence("tool_error", "web_fetch", "URL fetch was not approved", 0),
				occurrence("tool_error", "web_fetch", "URL fetch was not approved", 1),
			],
			[timedOut, { ...timedOut, excerpt: '  File "<ipython-input-2>", line 1\nTimeoutError', entryIndex: 1 }],
		] as const) {
			const id = observed.fingerprint.id;
			const label = observed.fingerprint.message || String(observed.fingerprint.exceptionClass);
			const legacy = legacyOf(observed, 5);
			expect(legacy.failures[id], label).toMatchObject({ count: 5, nonActionableCount: 5 });
			expect(recurringFailures(legacy), label).toEqual([]);
			const merged = mergeFailureObservations(legacy, [next]);
			expect(merged.newlyRecurring, label).toEqual([]);
			const reloaded = normalizeFailureLedger(JSON.parse(JSON.stringify(merged.ledger)));
			expect(reloaded.failures[id], label).toMatchObject({ count: 6, nonActionableCount: 6 });
			expect(recurringFailures(reloaded), label).toEqual([]);
			// A tally an earlier build under-counted is not trusted over what the fingerprint says.
			expect(legacyOf(observed, 6, { nonActionableCount: 2 }).failures[id].nonActionableCount, label).toBe(6);
			expect(isActionableFailure({ ...reloaded.failures[id], nonActionableCount: 0 }), label).toBe(false);
		}
	});

	it("reports a record as newly recurring when it turns actionable after it crossed the threshold", () => {
		const prefix = `bash: build failed: ${"error: cannot find symbol in module layout config ".repeat(5)}`;
		const fixable = occurrence("tool_error", "bash", `${prefix} while compiling target; see log`, 0);
		const timedOut = occurrence("tool_error", "bash", `${prefix} while waiting the request timed out`, 0);
		const id = fixable.fingerprint.id;
		let ledger = emptyFailureLedger();
		const fired: number[] = [];
		[timedOut, timedOut, ...Array.from({ length: 20 }, () => fixable)].forEach((observed, index) => {
			const updated = updateFailureLedger(normalizeFailureLedger(JSON.parse(JSON.stringify(ledger))), [
				{ ...observed, entryIndex: index },
			]);
			if (updated.newlyRecurring.some((record) => record.fingerprint.id === id)) fired.push(index);
			ledger = updated.ledger;
		});
		// Count 4 with 2 timeouts is the first update where the timeouts are no longer a strict majority.
		expect(fired).toEqual([3]);
		expect(ledger.failures[id]).toMatchObject({ count: 22, nonActionableCount: 2 });

		const rateLimited = occurrence(
			"provider_error",
			"openwebui",
			'OpenWebUI request failed: 429 {"detail":"rate limit exceeded"}',
			0,
		);
		const notFound = occurrence(
			"provider_error",
			"openwebui",
			'OpenWebUI request failed: 400 {"detail":"model not found"}',
			1,
		);
		let global = emptyFailureLedger();
		const mergedAt: number[] = [];
		const sequence = [rateLimited, rateLimited, ...Array.from({ length: 10 }, () => notFound), rateLimited];
		sequence.forEach((observed, index) => {
			const merged = mergeFailureObservations(normalizeFailureLedger(JSON.parse(JSON.stringify(global))), [
				{ ...observed, entryIndex: index },
			]);
			if (merged.newlyRecurring.length > 0) mergedAt.push(index);
			global = merged.ledger;
		});
		expect(mergedAt).toEqual([3]);
		expect(recurringFailures(global).map((record) => record.fingerprint.id)).toEqual([rateLimited.fingerprint.id]);

		// Leaving the recurring set and entering it again is a new entry.
		const muted = updateFailureLedger(
			global,
			Array.from({ length: 10 }, (_, index) => ({ ...rateLimited, entryIndex: 20 + index })),
		);
		expect(muted.newlyRecurring).toEqual([]);
		expect(recurringFailures(muted.ledger)).toEqual([]);
		const rearmed = updateFailureLedger(
			muted.ledger,
			Array.from({ length: 3 }, (_, index) => ({ ...notFound, entryIndex: 40 + index })),
		);
		expect(rearmed.newlyRecurring.map((record) => record.fingerprint.id)).toEqual([rateLimited.fingerprint.id]);
	});
});

const REPLAY_VERIFIED_AT = "2026-09-15T00:00:00.000Z";

function storedReplayCase(source: string, exceptionClass: string, verifiedAt?: string): ReplayCase {
	return { language: "python", source, exceptionClass, ...(verifiedAt === undefined ? {} : { verifiedAt }) };
}

/** Cases of the probe kinds no build derives any more, as a pre-retirement or hand-edited ledger holds them. */
const retiredReplayCase = {
	attribute: (name: string, verifiedAt?: string) =>
		storedReplayCase(`import json\ngetattr(json, "${name}")`, "AttributeError", verifiedAt),
	name: (name: string, verifiedAt?: string) => storedReplayCase(`from json import ${name}`, "ImportError", verifiedAt),
	executable: (program: string, verifiedAt?: string) =>
		storedReplayCase(
			`import shutil\nif shutil.which("${program}") is None:\n    raise FileNotFoundError("${program}")`,
			"FileNotFoundError",
			verifiedAt,
		),
};

describe("replay case storage", () => {
	const fingerprint = fingerprintFailure(
		"python_exception",
		"ipython",
		"ModuleNotFoundError",
		"No module named 'paramiko'",
	);
	const replay = (module: string, verifiedAt?: string): ReplayCase =>
		storedReplayCase(`import ${module}`, "ModuleNotFoundError", verifiedAt);
	const distribution = (name: string, verifiedAt?: string): ReplayCase =>
		storedReplayCase(
			`import importlib.metadata\nimportlib.metadata.version("${name}")`,
			"PackageNotFoundError",
			verifiedAt,
		);
	const observe = (module: string, entryIndex: number) => ({
		fingerprint,
		excerpt: `ModuleNotFoundError: No module named '${module}'`,
		entryIndex,
		turn: entryIndex,
		at: `t${entryIndex}`,
		replayCase: replay(module),
	});
	const baseRecord = (): FailureRecord => ({
		fingerprint,
		count: 4,
		firstSeenTurn: 1,
		lastSeenTurn: 7,
		firstSeenAt: "t1",
		lastSeenAt: "t7",
		excerpt: "ModuleNotFoundError: No module named 'paramiko'",
		addressedByProposalIds: ["p0"],
		nonActionableCount: 1,
	});
	const load = (record: object) =>
		normalizeFailureLedger({ schema: 1, lastScannedEntryIndex: 3, failures: { [fingerprint.id]: record } });
	const sourcesOf = (record: FailureRecord) => replayCasesOf(record).map((item) => item.source);

	it("drops every case that is not a valid probe on load, legacy field included, and leaves the rest of the record unchanged", () => {
		const paramiko = replay("paramiko", REPLAY_VERIFIED_AT);
		const dist = distribution("prime-agent-no-such-dist", REPLAY_VERIFIED_AT);
		const loaded = load({
			...baseRecord(),
			replayCase: retiredReplayCase.attribute("a", REPLAY_VERIFIED_AT),
			replayCases: [
				retiredReplayCase.name("b"),
				paramiko,
				retiredReplayCase.executable("rg"),
				replay("pip"),
				replay("pkg._x"),
				{ language: "js", source: "x" },
				dist,
				{ ...paramiko, source: "import paramiko\n" },
			],
		});
		const caseless = load(baseRecord());
		const record = loaded.failures[fingerprint.id];
		expect(record.replayCases).toEqual([paramiko, dist]);
		expect(record).not.toHaveProperty("replayCase");
		const { replayCases: _cases, ...rest } = record;
		expect(rest).toEqual(caseless.failures[fingerprint.id]);
		expect(observationOrdinal(loaded)).toBe(observationOrdinal(caseless));
		expect(recurringFailures(loaded).map((item) => item.fingerprint.id)).toEqual(
			recurringFailures(caseless).map((item) => item.fingerprint.id),
		);
		expect(recurringFailures(caseless).map((item) => item.fingerprint.id)).toEqual([fingerprint.id]);
		expect(normalizeFailureLedger(JSON.parse(JSON.stringify(loaded)))).toEqual(loaded);
	});

	it("keeps a record whose cases were all retired, with no replayCases key and its count intact", () => {
		const record = load({
			...baseRecord(),
			replayCase: retiredReplayCase.attribute("a", REPLAY_VERIFIED_AT),
			replayCases: [retiredReplayCase.name("b", REPLAY_VERIFIED_AT), retiredReplayCase.executable("rg")],
		}).failures[fingerprint.id];
		expect(record).toEqual(load(baseRecord()).failures[fingerprint.id]);
		expect(record.count).toBe(4);
		expect(record).not.toHaveProperty("replayCases");
		expect(record).not.toHaveProperty("replayCase");
	});

	it("applies the case bound to live probes only when loading", () => {
		const live = Array.from({ length: MAX_REPLAY_CASES }, (_, index) => replay(`n${index}`));
		const interleaved = live.flatMap((item, index) => [item, retiredReplayCase.attribute(`a${index}`)]);
		const record = load({
			...baseRecord(),
			replayCases: [...interleaved, retiredReplayCase.name("b"), retiredReplayCase.executable("rg")],
		}).failures[fingerprint.id];
		expect(sourcesOf(record)).toEqual(live.map((item) => item.source));
	});

	it("parses each stored case once on load, however old it is in the list", () => {
		const live = Array.from({ length: MAX_REPLAY_CASES }, (_, index) => replay(`parsed${index}`));
		const exec = vi.spyOn(RegExp.prototype, "exec");
		try {
			const record = load({ ...baseRecord(), replayCase: live[0], replayCases: live.slice(1) }).failures[
				fingerprint.id
			];
			const parses = live.map((item) => exec.mock.calls.filter(([input]) => input === item.source).length);
			expect(record.replayCases).toEqual(live);
			expect(parses[0]).toBeGreaterThan(0);
			expect(parses).toEqual(live.map(() => parses[0]));
		} finally {
			exec.mockRestore();
		}
	});

	it("never evicts verified live evidence to keep a retired case, through the local update and the global merge", () => {
		const paramiko = replay("paramiko", REPLAY_VERIFIED_AT);
		const retired = [
			retiredReplayCase.attribute("a0", REPLAY_VERIFIED_AT),
			retiredReplayCase.attribute("a1", REPLAY_VERIFIED_AT),
			retiredReplayCase.attribute("a2", REPLAY_VERIFIED_AT),
			retiredReplayCase.name("n0", REPLAY_VERIFIED_AT),
			retiredReplayCase.name("n1", REPLAY_VERIFIED_AT),
			retiredReplayCase.executable("rg", REPLAY_VERIFIED_AT),
			retiredReplayCase.executable("fd", REPLAY_VERIFIED_AT),
		];
		const stored = [paramiko, ...retired];
		expect(stored).toHaveLength(MAX_REPLAY_CASES);
		const loaded = load({ ...baseRecord(), replayCases: stored });
		const inMemory: FailureLedger = {
			schema: 1,
			lastScannedEntryIndex: 3,
			failures: { [fingerprint.id]: { ...baseRecord(), replayCases: stored } },
		};
		const requests = observe("requests", 8);
		for (const [label, updated] of [
			["local update", updateFailureLedger(loaded, [requests]).ledger],
			["global merge", mergeFailureObservations(inMemory, [requests]).ledger],
		] as const) {
			const record = updated.failures[fingerprint.id];
			expect(sourcesOf(record), label).toEqual(["import paramiko", "import requests"]);
			expect(
				verifiedReplayCasesOf(record).map((item) => item.source),
				label,
			).toEqual(["import paramiko"]);
			expect(record.count, label).toBe(5);
		}
	});

	it("drops inert cases on every ledger write: untouched records, verifications, and a new record", () => {
		const ledger: FailureLedger = {
			schema: 1,
			lastScannedEntryIndex: 0,
			failures: {
				[fingerprint.id]: { ...baseRecord(), replayCases: [retiredReplayCase.attribute("a"), replay("paramiko")] },
			},
		};
		const untouched = updateFailureLedger(ledger, [observation("other", 1)]).ledger;
		expect(untouched.failures[fingerprint.id].replayCases).toEqual([replay("paramiko")]);
		expect(untouched.failures.other.count).toBe(1);

		const verified = applyReplayVerifications(ledger, [
			{ fingerprintId: fingerprint.id, source: "import paramiko", verifiedAt: "v" },
		]);
		expect(verified.failures[fingerprint.id].replayCases).toEqual([replay("paramiko", "v")]);

		const importName = fingerprintFailure("python_exception", "ipython", "ImportError", "cannot import name 'x'");
		const created = updateFailureLedger(emptyFailureLedger(), [
			{
				fingerprint: importName,
				excerpt: "ImportError: cannot import name 'x' from 'json'",
				entryIndex: 0,
				turn: 1,
				at: "t1",
				replayCase: retiredReplayCase.name("x"),
			},
		]).ledger.failures[importName.id];
		expect(created.count).toBe(1);
		expect(created).not.toHaveProperty("replayCases");
	});

	it("mergeReplayCase ignores an incoming case that is not a probe and drops existing ones", () => {
		expect(mergeReplayCase([retiredReplayCase.attribute("a")], retiredReplayCase.name("b"))).toEqual([]);
		expect(
			mergeReplayCase([retiredReplayCase.attribute("a"), replay("a")], replay("b")).map((item) => item.source),
		).toEqual(["import a", "import b"]);
		expect(mergeReplayCase([replay("a", "v0")], replay("a"))).toEqual([replay("a", "v0")]);
	});

	it("stores every case deriveReplayCase can produce", () => {
		for (const [exceptionClass, message, excerpt, kind] of [
			[
				"ModuleNotFoundError",
				"No module named 'paramiko'",
				"ModuleNotFoundError: No module named 'paramiko'",
				"module",
			],
			["ModuleNotFoundError", "No module named 'a.b'", "ModuleNotFoundError: No module named 'a.b'", "module"],
			[
				"importlib.metadata.PackageNotFoundError",
				"No package metadata was found for my-dist",
				"importlib.metadata.PackageNotFoundError: No package metadata was found for my-dist",
				"distribution",
			],
			["PackageNotFoundError", "my_dist", "PackageNotFoundError: my_dist", "distribution"],
		] as const) {
			const derivedFingerprint = fingerprintFailure("python_exception", "ipython", exceptionClass, message);
			const derived = deriveReplayCase(derivedFingerprint, excerpt);
			expect(derived, excerpt).toBeDefined();
			expect(replayProbeOf(derived!)?.kind, excerpt).toBe(kind);
			const stored = updateFailureLedger(emptyFailureLedger(), [
				{ fingerprint: derivedFingerprint, excerpt, entryIndex: 0, turn: 1, at: "t1", replayCase: derived },
			]).ledger.failures[derivedFingerprint.id];
			expect(stored.replayCases, excerpt).toEqual([derived]);
		}
	});

	it("maps a legacy single replayCase into replayCases on load and never writes the legacy field", () => {
		const legacyCase = replay("x", REPLAY_VERIFIED_AT);
		const loaded = normalizeFailureLedger({
			schema: 1,
			lastScannedEntryIndex: 2,
			failures: {
				[fingerprint.id]: {
					fingerprint,
					count: 2,
					firstSeenTurn: 1,
					lastSeenTurn: 2,
					firstSeenAt: "t1",
					lastSeenAt: "t2",
					excerpt: "e",
					addressedByProposalIds: [],
					replayCase: legacyCase,
				},
			},
		});
		const record = loaded.failures[fingerprint.id];
		expect(record.replayCase).toBeUndefined();
		expect(record.replayCases).toEqual([legacyCase]);
		expect(normalizeFailureLedger(JSON.parse(JSON.stringify(loaded)))).toEqual(loaded);

		// A legacy record still held in memory migrates on its next update.
		const { replayCases: _stored, ...withoutCases } = record;
		const updated = updateFailureLedger(
			{ ...loaded, failures: { [fingerprint.id]: { ...withoutCases, replayCase: replay("a") } } },
			[observe("b", 3)],
		).ledger.failures[fingerprint.id];
		expect(updated.replayCase).toBeUndefined();
		expect(updated.replayCases?.map((item) => item.source)).toEqual(["import a", "import b"]);
	});

	it("keeps cases distinct by source, newest last, bounded, and never downgrades a verified case", () => {
		let ledger = updateFailureLedger(emptyFailureLedger(), [observe("s0", 0), observe("s1", 1)]).ledger;
		ledger = applyReplayVerifications(ledger, [
			{ fingerprintId: fingerprint.id, source: "import s0", verifiedAt: "v0" },
		]);
		ledger = updateFailureLedger(ledger, [observe("s0", 2)]).ledger;
		expect(replayCasesOf(ledger.failures[fingerprint.id])).toEqual([replay("s1"), replay("s0", "v0")]);

		const many = Array.from({ length: MAX_REPLAY_CASES + 3 }, (_, index) => observe(`n${index}`, index + 10));
		ledger = updateFailureLedger(ledger, many).ledger;
		const sources = replayCasesOf(ledger.failures[fingerprint.id]).map((item) => item.source);
		expect(sources).toEqual(many.slice(-MAX_REPLAY_CASES).map((item) => item.replayCase.source));
		expect(ledger.failures[fingerprint.id].count).toBe(3 + many.length);

		const bounded = normalizeFailureLedger({
			schema: 1,
			lastScannedEntryIndex: 0,
			failures: {
				[fingerprint.id]: {
					...ledger.failures[fingerprint.id],
					replayCases: [...many.map((item) => item.replayCase), replay("n0"), { language: "js", source: "x" }],
				},
			},
		}).failures[fingerprint.id];
		expect(bounded.replayCases).toHaveLength(MAX_REPLAY_CASES);
		expect(bounded.replayCases?.at(-1)?.source).toBe("import n0");
	});

	it("applies verifications purely, only to matching unverified cases", () => {
		const ledger = updateFailureLedger(emptyFailureLedger(), [observe("a", 0), observe("b", 1)]).ledger;
		const snapshot = structuredClone(ledger);
		const verified = applyReplayVerifications(ledger, [
			{ fingerprintId: fingerprint.id, source: "import b", verifiedAt: "v1" },
			{ fingerprintId: fingerprint.id, source: "import evicted", verifiedAt: "v2" },
			{ fingerprintId: "unknown", source: "import a", verifiedAt: "v3" },
		]);
		expect(ledger).toEqual(snapshot);
		expect(replayCasesOf(verified.failures[fingerprint.id])).toEqual([replay("a"), replay("b", "v1")]);
		expect(
			applyReplayVerifications(verified, [{ fingerprintId: fingerprint.id, source: "import b", verifiedAt: "v9" }]),
		).toBe(verified);
		expect(applyReplayVerifications(ledger, [])).toBe(ledger);
	});
});

describe("agent-session turn-boundary hook", () => {
	const harnesses: Harness[] = [];
	const previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	const previousGlobalLedger = process.env[GLOBAL_FAILURE_LEDGER_ENV];

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
		else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
		if (previousGlobalLedger === undefined) delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
		else process.env[GLOBAL_FAILURE_LEDGER_ENV] = previousGlobalLedger;
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
					provisional: { committedTurn: 0, untilTurn: 20, clock: "local-ordinal" },
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
			`proposal prov-1 (observation window 0..20) claimed to address: failure:${expectedId}`,
		);
		const state = loadHarnessState(localDir, "local");
		expect(state.failures?.failures[expectedId]?.count).toBe(1);
		// The recurrence is stamped with whatever clock the session measures windows in.
		expect(state.ravo?.lineage[0].provisional).toMatchObject({
			committedTurn: 0,
			untilTurn: 20,
			clock: "local-ordinal",
			observedRecurrence: { fingerprints: [expectedId] },
		});
	});

	it("never regresses a champion on a record written before the tally whose fingerprint alone is non-actionable", async () => {
		const remoteFetch: AgentTool = {
			name: "remote_fetch",
			label: "Remote fetch",
			description: "Always gets a 404",
			parameters: Type.Object({}),
			execute: async () => {
				throw new Error("fetch HTTP 404");
			},
		};
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools: [remoteFetch],
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const fingerprint = fingerprintFailure("tool_error", "remote_fetch", undefined, "fetch HTTP 403");
		const seeded = loadHarnessState(localDir, "local");
		seeded.failures = {
			schema: 1,
			lastScannedEntryIndex: 0,
			failures: {
				[fingerprint.id]: {
					fingerprint,
					count: 5,
					firstSeenTurn: 1,
					lastSeenTurn: 2,
					firstSeenAt: "t1",
					lastSeenAt: "t2",
					excerpt: "fetch HTTP 403",
					addressedByProposalIds: [],
				},
			},
		};
		seeded.ravo = {
			...emptyAssistedRavoState(),
			lineage: [
				{
					proposalId: "prov-1",
					parentId: null,
					score: 3,
					artifact: null,
					missedCriterionIds: [],
					claimedFingerprints: [fingerprint.id],
					provisional: { committedTurn: 0, untilTurn: 20, clock: "local-ordinal" },
				},
			],
			championId: "prov-1",
			evaluatedProposalIds: ["prov-1"],
		};
		saveHarnessState(localDir, seeded);

		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("remote_fetch", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			fauxAssistantMessage(emptyPlan()),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();

		const state = loadHarnessState(localDir, "local");
		expect(state.failures?.failures[fingerprint.id]).toMatchObject({ count: 6, nonActionableCount: 6 });
		expect(state.ravo?.lineage[0].provisional).toEqual({ committedTurn: 0, untilTurn: 20, clock: "local-ordinal" });
		expect(harness.session.handleRefineHostRequest("refine.status")).toEqual({ pending: false, in_flight: false });
		expect(harness.eventsOfType("refine_complete")).toHaveLength(0);
		expect(harness.getPendingResponseCount()).toBe(1);
	});

	it("rewrites the local and global ledgers without inert replay cases at the next flush, leaving every count intact", async () => {
		const harness = await createHarness({ persistSession: true, rlmDepth: 0, tools: [flakyTool()] });
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const globalDir = getGlobalHarnessStateDir();
		const fingerprint = fingerprintFailure(
			"python_exception",
			"ipython",
			"ModuleNotFoundError",
			"No module named 'paramiko'",
		);
		const flakyId = fingerprintFailure(
			"tool_error",
			"flaky",
			undefined,
			"cannot connect to db host at port 5432 (attempt 1)",
		).id;
		const paramiko = storedReplayCase("import paramiko", "ModuleNotFoundError", REPLAY_VERIFIED_AT);
		const ledgers = [
			[localDir, "local"],
			[globalDir, "global"],
		] as const;
		for (const [dir, scope] of ledgers) {
			const seeded = loadHarnessState(dir, scope);
			seeded.failures = {
				schema: 1,
				lastScannedEntryIndex: 0,
				failures: {
					[fingerprint.id]: {
						fingerprint,
						count: 3,
						firstSeenTurn: 1,
						lastSeenTurn: 1,
						firstSeenAt: "t1",
						lastSeenAt: "t1",
						excerpt: "ModuleNotFoundError: No module named 'paramiko'",
						addressedByProposalIds: [],
						replayCase: retiredReplayCase.attribute("load_string", REPLAY_VERIFIED_AT),
						replayCases: [retiredReplayCase.name("load_string"), paramiko, retiredReplayCase.executable("rg")],
					},
				},
			};
			saveHarnessState(dir, seeded);
		}

		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("flaky", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();

		for (const [dir, scope] of ledgers) {
			const raw = JSON.parse(readFileSync(getHarnessStatePath(dir), "utf8")) as { failures: FailureLedger };
			const record = raw.failures.failures[fingerprint.id];
			expect(record.replayCases, scope).toEqual([paramiko]);
			expect(record, scope).not.toHaveProperty("replayCase");
			expect(record.count, scope).toBe(3);
			expect(raw.failures.failures[flakyId]?.count, scope).toBe(1);
			expect(observationOrdinal(loadHarnessState(dir, scope).failures), scope).toBe(4);
		}
		expect(harness.eventsOfType("refine_complete")).toHaveLength(0);
	});
});
