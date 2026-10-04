// The generator of the TS goldens in this directory (`tests/golden.rs` replays
// every scenario in Rust and compares the results, including exact file bytes).
//
// To regenerate: copy `packages/coding-agent/src/core/ravo/{failure-ledger,referee,canonical-json}.ts`
// into `ravo/` and `src/core/distill/resolution-index.ts` into `distill/` from
// `perf/session-catalog-resume`, rewrite their relative `.js` imports to `.ts`,
// and add beside this file: `ravo/reducer.ts` (the three type names only),
// `config.ts` (the real `getResolutionStorePath` formula, agent dir from
// `GOLDEN_AGENT_DIR`), `utils/git.ts` (`findGitPaths` answering the fixed repo
// `/golden/repo`), a `@earendil-works/pi-ai` stub exporting a no-op
// `getLogger`, and `proper-lockfile` from the repo's node_modules; then
//   esbuild generate.ts --bundle --platform=node --format=esm --external:proper-lockfile --outfile=gen.mjs
//   node gen.mjs <this directory>
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	applyReplayVerifications,
	extractFailures,
	type FailureKind,
	type FailureLedger,
	type FailureObservation,
	findProvisionalRegressions,
	fingerprintFailure,
	fingerprintToolResultText,
	formatFailureLedgerForPrompt,
	formatRecurrenceRefineInstructions,
	formatRegressionRefineInstructions,
	isActionableFailure,
	mergeFailureObservations,
	normalizeFailureLedger,
	normalizeFailureMessage,
	observationOrdinal,
	parsePythonTraceback,
	recordProvisionalRegressions,
	recurringFailures,
	updateFailureLedger,
} from "./ravo/failure-ledger.ts";
import { deriveReplayCase, normalizeReplayCases, replayProbeOf } from "./ravo/referee.ts";
import { getResolutionStorePath } from "./config.ts";
import { openResolutionStore, ResolutionIndex } from "./distill/resolution-index.ts";

const OUT = process.argv[2]!;
const save = (name: string, value: unknown) => writeFileSync(join(OUT, name), `${JSON.stringify(value, null, 2)}\n`);
const text = (value: unknown) => `${JSON.stringify(value, null, 2)}\n`;

// ---------------------------------------------------------------- fingerprints
const RAW_MESSAGES = [
	"exit 1",
	"exit 001",
	"  [Errno 2] No such file: \"/var/tmp/run-42/out.json\"   at   0x7ffe1234 id=3f2a9c8e7b6d5a4f   line 17\n",
	"[Errno 2] No such file or directory: '/tmp/build-1/out.txt'",
	"HTTPSConnectionPool(host='api.example.com', port=443): Max retries exceeded with url: /v2/items/1234",
	"'AgentClient' object has no attribute 'list_agents'",
	"x".repeat(500),
	`${"y".repeat(198)}\u{1F600}tail`,
	"Ünïcödé ΣΊΣΥΦΟΣ error at C:\\Users\\me\\x.py and c:/tmp/a-b/c.txt ~/home/.cache/x",
	"uuid 123e4567-e89b-12d3-a456-426614174000 and deadbeefcafe and 0xFF and -3.25 and +7",
	"quotes `back tick` \"double\" 'single' 'unterminated and \"multi\nline\"",
	`'${"q".repeat(400)}' then '${"r".repeat(401)}'`,
	"tabs\tand\u00a0nbsp\u2028ls\ufeffbom\u0085nel",
	"",
	"   ",
	"rate limited: retry after 12s (HTTP 429)",
];
const FINGERPRINT_CASES: Array<[FailureKind, string | undefined, string | undefined]> = [
	["tool_error", "bash", undefined],
	["python_exception", "ipython", "FileNotFoundError"],
	["provider_error", "anthropic", undefined],
	["tool_error", undefined, undefined],
	["python_exception", "deploy-widget", "requests.exceptions.ConnectionError"],
];
const fingerprints = [];
for (const raw of RAW_MESSAGES) {
	for (const [kind, source, exceptionClass] of FINGERPRINT_CASES) {
		fingerprints.push({
			kind,
			source: source ?? null,
			exceptionClass: exceptionClass ?? null,
			raw,
			normalized: normalizeFailureMessage(raw),
			fingerprint: fingerprintFailure(kind, source, exceptionClass, raw),
		});
	}
}
save("fingerprints.json", fingerprints);

// ---------------------------------------------------------------- tracebacks
const TRACEBACKS = [
	`Executing cell...
Traceback (most recent call last):
  File "<ipython-input-3>", line 2, in <module>
    run()
  File "/home/user/.agents/skills/deploy-widget/scripts/run.py", line 40, in run
    return client.fetch(url)
requests.exceptions.ConnectionError: HTTPSConnectionPool(host='api.example.com', port=443): Max retries exceeded with url: /v2/items/1234
`,
	`Traceback (most recent call last):
  File "/tmp/x.py", line 1, in <module>
KeyboardInterrupt
`,
	"Traceback (most recent call last):\r\n  File \"a.py\", line 1, in <module>\r\nValueError: bad\r\n",
	`Traceback (most recent call last):
  File "first.py", line 1, in <module>
TypeError: first
During handling...
Traceback (most recent call last):
  File "C:\\work\\skills\\win-skill\\main.py", line 9, in go
  File "/x/skills/a.py", line 3, in f
ModuleNotFoundError: No module named 'polars'`,
	"Traceback (most recent call last)\nno frames here\nKeyError",
	"Traceback (most recent call last):\n  File \"x\", line 1\nfoo.bar.BazWarning: careful\u2028tail",
	"Traceback (most recent call last):\n  File \"x\", line 1\nNotAnErr: nope",
	`Traceback (most recent call last):\n  File "x", line 1\nRuntimeError: ${"long ".repeat(120)}`,
	"no traceback at all",
];
save(
	"tracebacks.json",
	TRACEBACKS.map((input) => ({
		text: input,
		parsed: parsePythonTraceback(input) ?? null,
		asToolResult: fingerprintToolResultText("ipython", input, false) ?? null,
		asError: fingerprintToolResultText("bash", input, true) ?? null,
	})),
);

// ---------------------------------------------------------------- extraction
const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const assistant = (stopReason: string, extra: Record<string, unknown> = {}) => ({
	role: "assistant",
	content: [],
	api: "faux",
	provider: "faux",
	model: "faux-1",
	usage,
	stopReason,
	timestamp: 1,
	...extra,
});
const toolResult = (toolName: string, textValue: string, isError: boolean, details?: unknown) => ({
	role: "toolResult",
	toolCallId: "call-1",
	toolName,
	content: [{ type: "text", text: textValue }, { type: "image", data: "AAAA", mimeType: "image/png" }],
	...(details === undefined ? {} : { details }),
	isError,
	timestamp: 1,
});
const missingModule = [
	"Traceback (most recent call last):",
	'  File "<ipython-input-1>", line 1, in <module>',
	"    import polars",
	"ModuleNotFoundError: No module named 'polars'",
];
const missingDistribution = [
	"Traceback (most recent call last):",
	'  File "<ipython-input-2>", line 1, in <module>',
	'    importlib.metadata.version("foo-bar")',
	"importlib.metadata.PackageNotFoundError: No package metadata was found for foo-bar",
];
const messages = [
	{ role: "user", content: [{ type: "text", text: "Traceback (most recent call last): no" }], timestamp: 1 },
	toolResult("ipython", missingModule.join("\n"), true, {
		status: "error",
		error: { ename: "ModuleNotFoundError", evalue: "No module named 'polars'", traceback: missingModule },
	}),
	toolResult("ipython", missingDistribution.join("\n"), true, {
		status: "error",
		error: { ename: "PackageNotFoundError", evalue: "x", traceback: missingDistribution },
	}),
	// The traceback was printed, not raised: no replay case.
	toolResult("ipython", missingModule.join("\n"), false, { status: "ok" }),
	// The kernel's error disagrees with the text: no replay case.
	toolResult("ipython", missingModule.join("\n"), true, {
		status: "error",
		error: { ename: "KeyError", evalue: "x", traceback: ["KeyError: 'x'"] },
	}),
	toolResult("bash", "bash: line 1: cargo: command not found (exit 127)", true),
	toolResult("edit", "   ", true),
	toolResult("", "flagged without a name", true),
	toolResult("read", "ok: 3 files written", false),
	assistant("error", { errorMessage: "429 rate limited: retry after 12s" }),
	assistant("error", { errorMessage: "   ", stopReasonRaw: "content_filter" }),
	assistant("error", {}),
	assistant("error", { provider: "", errorMessage: "boom" }),
	assistant("aborted", { errorMessage: "aborted by user" }),
	assistant("stop"),
];
let tick = 0;
const fixedNow = () => `2026-01-01T00:00:0${tick++ % 10}.000Z`;
save("extract.json", {
	messages,
	runs: [0, 3, 20].map((fromEntryIndex) => {
		tick = 0;
		return { fromEntryIndex, turn: 4, observations: extractFailures(messages as never, { fromEntryIndex, turn: 4, now: fixedNow }) };
	}),
});

// ---------------------------------------------------------------- ledger updates
const obs = (
	id: string,
	turn: number,
	options: { kind?: FailureKind; message?: string; excerpt?: string; exceptionClass?: string; entryIndex?: number; replay?: string } = {},
): FailureObservation => {
	const fingerprint = { id, kind: options.kind ?? ("tool_error" as FailureKind), message: options.message ?? `m-${id}`, source: "bash" } as FailureObservation["fingerprint"];
	if (options.exceptionClass) fingerprint.exceptionClass = options.exceptionClass;
	return {
		fingerprint,
		excerpt: options.excerpt ?? `excerpt ${id} turn ${turn}`,
		entryIndex: options.entryIndex ?? turn,
		turn,
		at: `t${turn}`,
		...(options.replay ? { replayCase: { language: "python" as const, source: options.replay, exceptionClass: "ModuleNotFoundError" } } : {}),
	};
};
type Step = { mode: "update" | "merge"; observations: FailureObservation[]; threshold?: number; scannedThroughEntryIndex?: number };
const STEPS: Step[] = [
	{ mode: "update", observations: [obs("a", 1), obs("b", 1)] },
	{ mode: "update", observations: [], scannedThroughEntryIndex: 9 },
	{ mode: "update", observations: [obs("a", 2, { entryIndex: 12 }), obs("c", 2, { excerpt: "request timed out" })] },
	{ mode: "update", observations: [obs("c", 3, { excerpt: "fetch failed" }), obs("c", 3)], threshold: 2 },
	{ mode: "merge", observations: [obs("d", 4, { replay: "import polars" }), obs("d", 4, { replay: "import os; os.system('x')" })] },
	{ mode: "merge", observations: [obs("d", 5, { replay: "import numpy" }), obs("e", 5, { kind: "provider_error", message: "# rate limited" })] },
	{ mode: "update", observations: [obs("e", 6, { kind: "provider_error", message: "overloaded" }), obs("f", 6, { exceptionClass: "httpx.ConnectError" })], threshold: 1 },
	{ mode: "update", observations: [obs("c", 7), obs("c", 7), obs("c", 7)] },
];
let ledger: FailureLedger = normalizeFailureLedger(undefined);
const updates = [];
for (const step of STEPS) {
	const opts = { ...(step.threshold === undefined ? {} : { threshold: step.threshold }), ...(step.scannedThroughEntryIndex === undefined ? {} : { scannedThroughEntryIndex: step.scannedThroughEntryIndex }) };
	const result = step.mode === "update" ? updateFailureLedger(ledger, step.observations, opts) : mergeFailureObservations(ledger, step.observations, opts);
	ledger = result.ledger;
	updates.push({
		step,
		ledgerText: text(ledger),
		newlyRecurring: result.newlyRecurring.map((record) => record.fingerprint.id),
		recurring: recurringFailures(ledger).map((record) => record.fingerprint.id),
		recurringAt1: recurringFailures(ledger, 1).map((record) => record.fingerprint.id),
		ordinal: observationOrdinal(ledger),
		actionable: Object.fromEntries(Object.entries(ledger.failures).map(([id, record]) => [id, isActionableFailure(record)])),
	});
}
save("ledger-updates.json", updates);

// ---------------------------------------------------------------- normalization
const RAW_LEDGERS: unknown[] = [
	undefined,
	[],
	{ lastScannedEntryIndex: -3, failures: [] },
	{ lastScannedEntryIndex: 4.5, failures: {} },
	{
		schema: 7,
		lastScannedEntryIndex: 11,
		extra: true,
		failures: {
			keyed: {
				fingerprint: { kind: "tool_error", message: "m", source: 3, exceptionClass: "X", extra: 1 },
				count: 5,
				firstSeenTurn: -1,
				lastSeenTurn: 2.5,
				excerpt: "plain",
				addressedByProposalIds: ["p1", 2, "p3"],
				nonActionableCount: 9,
				unknown: "dropped",
			},
			legacyFlag: { fingerprint: { id: "", kind: "tool_error", message: "m2" }, count: 3, nonActionable: true },
			legacyExcerpt: { fingerprint: { id: "own", kind: "tool_error", message: "m3" }, count: 3, excerpt: "socket hang up" },
			byMessage: { fingerprint: { kind: "provider_error", message: "overloaded" }, count: 4, nonActionableCount: 1 },
			zero: { fingerprint: { kind: "tool_error", message: "z" }, count: 0, nonActionableCount: 3 },
			cases: {
				fingerprint: { kind: "python_exception", message: "c", exceptionClass: "ModuleNotFoundError" },
				count: 2,
				replayCase: { language: "python", source: "import legacy", verifiedAt: "t0" },
				replayCases: [
					{ language: "python", source: "import a", sysPath: ["", "/x", 3], verifiedAt: "" },
					{ language: "python", source: "import os; os.system('x')" },
					{ language: "ruby", source: "import b" },
					{ language: "python", source: "import a", exceptionClass: "E", verifiedAt: "t1" },
					...["c", "d", "e", "f", "g", "h", "i", "j"].map((name) => ({ language: "python", source: `import ${name}` })),
				],
			},
			retired: {
				fingerprint: { kind: "python_exception", message: "r" },
				count: 1,
				replayCases: [{ language: "python", source: "from a import b" }],
			},
			badKind: { fingerprint: { kind: "nope", message: "m" }, count: 1 },
			noMessage: { fingerprint: { kind: "tool_error" }, count: 1 },
			notObject: 3,
		},
	},
];
save(
	"normalize.json",
	RAW_LEDGERS.map((raw) => ({ raw: raw ?? null, ledgerText: text(normalizeFailureLedger(raw)) })),
);

// ---------------------------------------------------------------- prompt text
const promptLedger = normalizeFailureLedger(JSON.parse(updates[updates.length - 1]!.ledgerText));
const verified = applyReplayVerifications(promptLedger, [{ fingerprintId: "d", source: "import numpy", verifiedAt: "v1" }]);
const all = Object.values(verified.failures);
const regressions = [
	{ championId: "p-1", fingerprints: ["a", "failure:c"], committedTurn: 3, untilTurn: 23 },
	{ championId: "p-2", fingerprints: [], committedTurn: 0, untilTurn: 0 },
];
save("prompt.json", {
	ledgerText: text(verified),
	limit12: formatFailureLedgerForPrompt(all),
	limit2: formatFailureLedgerForPrompt(all, 2),
	limit0: formatFailureLedgerForPrompt(all, 0),
	empty: formatFailureLedgerForPrompt([]),
	recurrence: formatRecurrenceRefineInstructions(all),
	regressions,
	regression: formatRegressionRefineInstructions(regressions, all),
	longExcerpt: formatFailureLedgerForPrompt([
		{ ...all[0]!, excerpt: `  ${"word\n\t".repeat(80)}  `, fingerprint: { ...all[0]!.fingerprint, source: "", exceptionClass: "" } },
	]),
});

// ---------------------------------------------------------------- actionability
const EXCERPTS = [
	"The operation was aborted",
	"fetch HTTP 503 for https://x",
	"fetch http 404x",
	"Error: getaddrinfo ENOTFOUND api.x",
	"connect ECONNREFUSED",
	"request timed out after 30s",
	"TimeoutError",
	"timeout=5 is not a keyword",
	"database is locked",
	"denied by the user",
	"Kernel has been shutdown",
	"rate_limit_error",
	"Too Many Requests",
	"error 429",
	"HTTP 502 Bad Gateway",
	"status code: 500",
	"internal-server error",
	"content filter triggered",
	"empty completion",
	"the model refused",
	"service unavailable",
	"overloaded_error",
	"ordinary KeyError",
];
const actionability = [];
for (const excerpt of EXCERPTS) {
	for (const kind of ["tool_error", "provider_error"] as FailureKind[]) {
		const fingerprint = fingerprintFailure(kind, "src", undefined, "unrelated");
		actionability.push({ kind, excerpt, actionable: isActionableFailure({ fingerprint, excerpt }) });
	}
}
for (const exceptionClass of ["TimeoutError", "asyncio.TimeoutError", "httpx.ConnectError", "ConnectionError"]) {
	const fingerprint = fingerprintFailure("python_exception", "ipython", exceptionClass, "x");
	actionability.push({ kind: "python_exception", exceptionClass, excerpt: "x", actionable: isActionableFailure({ fingerprint, excerpt: "x" }) });
}
save("actionability.json", actionability);

// ---------------------------------------------------------------- regressions
const ravo = {
	version: 1,
	lineage: [
		{ proposalId: "p-1", claimedFingerprints: ["b", "a", "a", 3], provisional: { committedTurn: 5, untilTurn: 25, clock: "ordinal" }, score: 1 },
		{ proposalId: "p-2", claimedFingerprints: ["a"], provisional: { committedTurn: 5, untilTurn: 25, clock: "local-ordinal" } },
		{ proposalId: "p-3", claimedFingerprints: ["a"], provisional: { committedTurn: 5, untilTurn: 25 } },
		{ proposalId: "p-4", claimedFingerprints: ["a"], provisional: { committedTurn: 30, untilTurn: 40, clock: "ordinal" } },
		{ proposalId: "p-5", claimedFingerprints: ["z"], provisional: { committedTurn: 5, untilTurn: 25, clock: "ordinal" } },
		{ proposalId: 6, claimedFingerprints: ["a"], provisional: { committedTurn: 5, untilTurn: 25, clock: "ordinal" } },
		{ proposalId: "p-7", claimedFingerprints: ["a"] },
		{ proposalId: "p-8", claimedFingerprints: ["a"], provisional: { committedTurn: 5.5, untilTurn: 25, clock: "ordinal", observedRecurrence: { turn: 1, fingerprints: [] }, note: "kept" } },
	],
	tail: "kept",
};
const found = findProvisionalRegressions(ravo as never, ["a", "b"], 10, "ordinal");
const foundLocal = findProvisionalRegressions(ravo as never, ["a"], 10, "local-ordinal");
save("regressions.json", {
	ravo,
	found,
	foundLocal,
	none: findProvisionalRegressions(ravo as never, [], 10, "ordinal"),
	outside: findProvisionalRegressions(ravo as never, ["a"], 26, "ordinal"),
	recordedText: text(recordProvisionalRegressions(ravo as never, found, 12)),
});

// ---------------------------------------------------------------- replay cases
const replayInputs = [
	["ModuleNotFoundError", "ModuleNotFoundError: No module named 'polars.io'"],
	["ModuleNotFoundError", "ModuleNotFoundError: No Module Named \"pip\""],
	["ModuleNotFoundError", "No module named 'a._b'"],
	["importlib.metadata.PackageNotFoundError", "PackageNotFoundError: foo_bar  \nmore"],
	["PackageNotFoundError", "PackageNotFoundError: 'quoted-dist'"],
	["ValueError", "No package metadata was found for Some.Dist\r\n"],
	["ValueError", "PackageNotFoundError: not-this-class"],
	["AttributeError", "module 'x' has no attribute 'y'"],
];
save(
	"replay.json",
	{
		derived: replayInputs.map(([exceptionClass, excerpt]) => {
			const fingerprint = fingerprintFailure("python_exception", "ipython", exceptionClass, excerpt!);
			return { exceptionClass, excerpt, replayCase: deriveReplayCase(fingerprint, excerpt!) ?? null };
		}),
		probes: ["import a", "import a.b_c", "import _a", "import tkinter", "import a ", "import importlib.metadata\nimportlib.metadata.version(\"x-y\")", "import importlib.metadata\nimportlib.metadata.version(\"-x\")", "from a import b"].map(
			(source) => ({ source, valid: replayProbeOf({ source }) !== undefined }),
		),
		normalized: normalizeReplayCases(
			[{ language: "python", source: "import b", sysPath: ["/p"] }, { language: "python", source: "import b", verifiedAt: "v" }, { language: "python", source: "import c" }],
			{ language: "python", source: "import b", verifiedAt: "legacy" },
		),
	},
);

// ---------------------------------------------------------------- resolution
const agentDir = mkdtempSync(join(tmpdir(), "resolution-golden-"));
process.env.GOLDEN_AGENT_DIR = agentDir;
let clock = 1_700_000_000_000;
// Only the index's own `recordedAt` stamps advance the clock (proper-lockfile reads it too).
Date.now = () => {
	if ((new Error().stack ?? "").includes("ResolutionIndex.record")) clock += 1000;
	return clock;
};
const attributeError = (line: number, source: string, name = "list_agents") =>
	[
		"Traceback (most recent call last):",
		`  File "<ipython-input-${line}>", line 1, in <module>`,
		`    ${source}`,
		`AttributeError: 'AgentClient' object has no attribute '${name}'`,
	].join("\n");
const CELLS = [
	{ code: "agents = client.list_agents()", output: attributeError(1, "agents = client.list_agents()"), isError: true },
	{ code: "agents = client.agents()\nprint(f\"{len(agents)} agents\")", output: "3 agents", isError: false },
	{ code: "for agent in client.list_agents():\n    print(agent.name)", output: attributeError(3, "for agent in client.list_agents():"), isError: true },
	{ code: "import pandas as pd", output: "", isError: false },
	{ code: "  total = frame.agg(how)  ", output: "Traceback (most recent call last):\n  File \"<x>\", line 1\nValueError: cannot reindex on an axis with duplicate labels", isError: true },
	{ code: "total = frame.agg(how)", output: "ok", isError: false },
	{ code: "total = frame.agg('sum')", output: "ok", isError: false },
	{ code: "handle.run()", output: "", isError: true },
	{ code: "x".repeat(1300) + " handle", output: "done", isError: false },
	{ code: "client.list_agents()", output: attributeError(9, "client.list_agents()"), isError: true },
	{ code: "agents = client.agents()\nprint(f\"{len(agents)} agents\")", output: attributeError(10, "client.agents()"), isError: true },
	{ code: "agents = client.list_agents()", output: attributeError(11, "agents = client.list_agents()"), isError: true },
];
const sessionA = new ResolutionIndex({ store: openResolutionStore("/golden/repo/sub") });
const hintsA = CELLS.map((cell) => sessionA.observe(cell)?.text ?? null);
const storePath = getResolutionStorePath("/golden/repo", agentDir);
const storeAfterA = readFileSync(storePath, "utf8");
// A second session in the same repo learns from the store.
const sessionB = new ResolutionIndex({ store: openResolutionStore("/golden/repo") });
const CELLS_B = [
	{ code: "for a in client.list_agents(): pass", output: attributeError(1, "client.list_agents()"), isError: true },
	{ code: "total = frame.agg(how)", output: "Traceback (most recent call last):\n  File \"<x>\", line 1\nValueError: cannot reindex on an axis with duplicate labels", isError: true },
	{ code: "handle.run(retry=True)", output: "  ", isError: true },
];
const hintsB = CELLS_B.map((cell) => sessionB.observe(cell)?.text ?? null);
const storeAfterB = readFileSync(storePath, "utf8");
save("resolution.json", {
	repo: "/golden/repo",
	storeName: storePath.slice(agentDir.length + 1),
	clockStart: 1_700_000_000_000,
	clockStep: 1000,
	cells: CELLS,
	hints: hintsA,
	sessionRecords: sessionA.records(),
	unresolved: sessionA.unresolved(),
	storeAfterA,
	cellsB: CELLS_B,
	hintsB,
	storeAfterB,
	storeNames: ["/golden/repo", "/x/my repo", "/x/ünï", "/x/a\u{1F600}b", "/", "/x/trailing/"].map((repo) => ({
		repo,
		name: getResolutionStorePath(repo, "/agent").slice("/agent/".length),
	})),
});
rmSync(agentDir, { recursive: true, force: true });

// ---------------------------------------------------------------- harness state
const emptyState = { schema: 1, entries: { prompt: {}, memory: {}, skill: {}, subagent: {} }, refinements: [] };
save("harness.json", {
	emptyWithFailuresText: text({ ...emptyState, failures: verified }),
});
