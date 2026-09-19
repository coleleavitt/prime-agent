import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import {
	addSpanSink,
	fauxAssistantMessage,
	fauxToolCall,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
} from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
	type FailureRecord,
	fingerprintFailure,
	GLOBAL_FAILURE_LEDGER_ENV,
} from "../../src/core/ravo/failure-ledger.js";
import type { ReplayCase } from "../../src/core/ravo/referee.js";
import type * as RefereeRunner from "../../src/core/ravo/referee-runner.js";
import { openTrustWindow, type TrustWindowEvidence } from "../../src/core/refinement/harness-trust.js";
import {
	getGlobalHarnessStateDir,
	getHarnessStatePath,
	getLocalHarnessStateDir,
	HARNESS_TRUST_ADJUSTED_MSG,
	HARNESS_TRUST_LOG_COMPONENT,
	HARNESS_TRUST_SETTLED_MSG,
	type HarnessEntry,
	type HarnessScope,
	type HarnessState,
	loadHarnessState,
	saveHarnessState,
} from "../../src/core/refinement/index.js";
import { createHarness, type Harness } from "./harness.js";

const selfCheck = vi.hoisted(() => ({ rejects: false }));

vi.mock("../../src/core/ravo/referee-runner.js", async (importOriginal) => {
	const actual = await importOriginal<typeof RefereeRunner>();
	return {
		...actual,
		verifyObservedReplayCases: (...args: Parameters<typeof actual.verifyObservedReplayCases>) =>
			selfCheck.rejects
				? new Promise<never>((_resolve, reject) => {
						setTimeout(() => reject(new Error("the self-check could not run")), 300);
					})
				: actual.verifyObservedReplayCases(...args),
	};
});

const MISSING_MODULE = "prime_agent_trust_probe_missing_module";
const OTHER_MODULE = "prime_agent_trust_probe_other_module";
const PROBE_FINGERPRINT = fingerprintFailure(
	"python_exception",
	"ipython",
	"ModuleNotFoundError",
	`No module named '${MISSING_MODULE}'`,
).id;
const DEPLOY_ERROR = 'manifest validation failed for release "12"';
const DEPLOY_FINGERPRINT = fingerprintFailure("tool_error", "deploy", undefined, DEPLOY_ERROR).id;
const SKILL_ID = "probe_skill";
const SKILL_REF = `skill:${SKILL_ID}`;
const MEMORY_ID = "seed_memory";
const SEEDED_AT = "2026-09-16T08:00:00.000Z";

type SessionInternals = {
	_replayVerification?: Promise<void>;
	_trustAdjudication?: Promise<void>;
	_trustAdjudicationsAwaiting: Map<string, unknown>;
	_trustAdjudicationsQueued: Set<string>;
	_pendingTrustEvidence: TrustWindowEvidence[];
	_globalPendingTrustEvidence: TrustWindowEvidence[];
};

function traceback(module: string): string {
	return [
		"Traceback (most recent call last):",
		'  File "<ipython-input-1>", line 1, in <module>',
		`    import ${module}`,
		`ModuleNotFoundError: No module named '${module}'`,
	].join("\n");
}

/** A cell that raised: replay cases derive only from the kernel's own error details on an ipython result. */
function probeTool(module = MISSING_MODULE): AgentTool {
	const text = traceback(module);
	return {
		name: "ipython",
		label: "IPython",
		description: "Returns a traceback",
		parameters: Type.Object({}),
		execute: async () => ({
			content: [{ type: "text", text }],
			details: {
				status: "error",
				errorEname: "ModuleNotFoundError",
				error: { ename: "ModuleNotFoundError", evalue: `No module named '${module}'`, traceback: text.split("\n") },
			},
		}),
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

function probeRecord(count: number, replayCases: ReplayCase[]): FailureRecord {
	return {
		fingerprint: fingerprintFailure(
			"python_exception",
			"ipython",
			"ModuleNotFoundError",
			`No module named '${MISSING_MODULE}'`,
		),
		count,
		firstSeenTurn: 1,
		lastSeenTurn: count,
		firstSeenAt: SEEDED_AT,
		lastSeenAt: SEEDED_AT,
		excerpt: `ModuleNotFoundError: No module named '${MISSING_MODULE}'`,
		addressedByProposalIds: [],
		...(replayCases.length > 0 ? { replayCases } : {}),
	};
}

const VERIFIED_PROBE: ReplayCase = {
	language: "python",
	source: `import ${MISSING_MODULE}`,
	exceptionClass: "ModuleNotFoundError",
	verifiedAt: SEEDED_AT,
};

function deployRecord(count: number): FailureRecord {
	return {
		fingerprint: fingerprintFailure("tool_error", "deploy", undefined, DEPLOY_ERROR),
		count,
		firstSeenTurn: 1,
		lastSeenTurn: count,
		firstSeenAt: SEEDED_AT,
		lastSeenAt: SEEDED_AT,
		excerpt: DEPLOY_ERROR,
		addressedByProposalIds: [],
	};
}

function seedEntry(kind: "skill" | "memory", id: string, scope: HarnessScope): HarnessEntry {
	return {
		id,
		kind,
		title: id,
		content: `Seeded ${kind} ${id}.`,
		path: "general",
		scope,
		reference: kind === "skill" ? { type: "python", import: MISSING_MODULE, callable: "probe" } : {},
		arguments: {},
		metadata: {},
		source: "refine",
		created_at: SEEDED_AT,
		updated_at: SEEDED_AT,
		version: 1,
	};
}

function writeFailures(records: readonly FailureRecord[]): void {
	const dir = getGlobalHarnessStateDir();
	const state = loadHarnessState(dir, "global");
	state.failures = {
		schema: 1,
		failures: Object.fromEntries(records.map((record) => [record.fingerprint.id, record])),
		lastScannedEntryIndex: 0,
	};
	saveHarnessState(dir, state);
}

/** A committed skill (or memory) fix of `fingerprint` with its open trust window over [2, untilTurn]. */
function seedWindow(
	dir: string,
	scope: HarnessScope,
	options: { kind: "skill" | "memory"; fingerprint: string; untilTurn: number },
): void {
	const state = loadHarnessState(dir, scope);
	const id = options.kind === "skill" ? SKILL_ID : MEMORY_ID;
	state.entries[options.kind][id] = seedEntry(options.kind, id, scope);
	const ref = `${options.kind}:${id}`;
	state.trustWindows = openTrustWindow(state.trustWindows, {
		proposalId: "refine_seed",
		touched: [ref],
		claimedFingerprints: [options.fingerprint],
		committedTurn: 2,
		untilTurn: options.untilTurn,
		...(options.kind === "skill" ? { skillImports: { [ref]: [MISSING_MODULE] } } : {}),
	});
	saveHarnessState(dir, state);
}

function resolvePython(): string | undefined {
	for (const candidate of ["python3", "python"]) {
		const probe = spawnSync(candidate, ["-c", "import sys; sys.stdout.write(sys.executable)"], { encoding: "utf8" });
		if (probe.status === 0 && probe.stdout.trim()) return probe.stdout.trim();
	}
	return undefined;
}

const PYTHON = resolvePython();

describe("AgentSession post-commit trust adjudication", () => {
	const harnesses: Harness[] = [];
	const tempDirs: string[] = [];
	const previousEnv = {
		agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
		globalLedger: process.env[GLOBAL_FAILURE_LEDGER_ENV],
		kernelPython: process.env.PRIME_AGENT_KERNEL_PYTHON,
		pythonPath: process.env.PYTHONPATH,
	};
	let agentDir: string;
	let spans: SpanEndRecord[];
	let logs: LogEntry[];
	let removeSpanSink: () => void;

	beforeEach(() => {
		agentDir = mkdtempSync(join(tmpdir(), "prime-agent-trust-adjudication-"));
		tempDirs.push(agentDir);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
		delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
		process.env.PRIME_AGENT_KERNEL_PYTHON = PYTHON ?? join(agentDir, "no-kernel-python");
		spans = [];
		logs = [];
		removeSpanSink = addSpanSink((record) => spans.push(record));
		setLogSink((entry) => logs.push(entry));
	});

	afterEach(() => {
		selfCheck.rejects = false;
		removeSpanSink();
		setLogSink(undefined);
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		while (tempDirs.length > 0) rmSync(tempDirs.pop()!, { recursive: true, force: true });
		for (const [key, value] of [
			["PRIME_AGENT_CODING_AGENT_DIR", previousEnv.agentDir],
			[GLOBAL_FAILURE_LEDGER_ENV, previousEnv.globalLedger],
			["PRIME_AGENT_KERNEL_PYTHON", previousEnv.kernelPython],
			["PYTHONPATH", previousEnv.pythonPath],
		] as const) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	});

	const named = (name: string) => spans.filter((span) => span.name === name);
	const trustLogs = (msg: string) =>
		logs.filter((entry) => entry.component === HARNESS_TRUST_LOG_COMPONENT && entry.msg === msg);

	async function sessionWith(tools: AgentTool[]): Promise<Harness> {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools,
			settings: { autoRefine: { enabled: false, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		return harness;
	}

	function localDir(harness: Harness): string {
		return getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
	}

	function internalsOf(harness: Harness): SessionInternals {
		return harness.session as unknown as SessionInternals;
	}

	async function runTool(harness: Harness, tool: string): Promise<void> {
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall(tool, {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);
		await harness.session.prompt(`run ${tool}`);
		await harness.session.waitForIdle();
	}

	function stateOf(harness: Harness, scope: HarnessScope): HarnessState {
		return scope === "global"
			? loadHarnessState(getGlobalHarnessStateDir(), "global")
			: loadHarnessState(localDir(harness), "local");
	}

	/** A turn with no tool call: its boundary is where a verdict that landed since is written to the local state. */
	async function nextTurn(harness: Harness): Promise<void> {
		harness.setResponses([fauxAssistantMessage("noted")]);
		await harness.session.prompt("continue");
		await harness.session.waitForIdle();
	}

	/** Make the replayed import take `seconds` before it raises, so its verdict lands after the turn went idle. */
	function slowMissingModule(seconds: number): void {
		const stubRoot = join(agentDir, "slow-python-root");
		mkdirSync(stubRoot, { recursive: true });
		writeFileSync(
			join(stubRoot, `${MISSING_MODULE}.py`),
			`import time\ntime.sleep(${seconds})\nraise ModuleNotFoundError("No module named '${MISSING_MODULE}'")\n`,
			"utf8",
		);
		process.env.PYTHONPATH = stubRoot;
	}

	it.skipIf(PYTHON === undefined)(
		"debits the skill a committed refine wrote when its own import failure recurs inside the trust window (python)",
		async () => {
			writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
			const harness = await sessionWith([probeTool()]);
			seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });

			await runTool(harness, "ipython");
			await internalsOf(harness)._trustAdjudication;
			await nextTurn(harness);

			const state = stateOf(harness, "local");
			expect(state.entries.skill[SKILL_ID].trust?.score).toBe(35);
			expect(state.entries.skill[SKILL_ID].trust?.events).toEqual([
				expect.objectContaining({
					reason: "measured_fault",
					delta: -15,
					proposalId: "refine_seed",
					fingerprintId: PROBE_FINGERPRINT,
				}),
			]);
			expect(state.trustWindows?.refine_seed).toMatchObject({
				outcome: "faulted",
				faultedFingerprints: [PROBE_FINGERPRINT],
				recurrences: { [PROBE_FINGERPRINT]: 3 },
				adjudications: [{ entry: SKILL_REF, fingerprintId: PROBE_FINGERPRINT, status: "upheld", ordinal: 3 }],
			});

			const [adjudicate] = named("harness.trust.adjudicate");
			expect(named("harness.trust.adjudicate")).toHaveLength(1);
			const probeTrace = named("tool.execute").find((span) => span.attrs["tool.name"] === "ipython")?.traceId;
			expect(probeTrace).toEqual(expect.any(String));
			expect(adjudicate).toMatchObject({
				status: "ok",
				parentSpanId: undefined,
				attrs: {
					"session.id": harness.session.sessionId,
					"trigger.trace_id": probeTrace,
					"trust.jobs": 1,
					"trust.ran": 1,
					"trust.upheld": 1,
					"trust.aborted": false,
				},
			});
			expect(adjudicate.traceId).not.toBe(probeTrace);
			expect(named("ravo.referee")[0]?.parentSpanId).toBe(adjudicate.spanId);

			expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([
				expect.objectContaining({
					proposalId: "refine_seed",
					scope: "local",
					from: "open",
					outcome: "faulted",
					fingerprints: [PROBE_FINGERPRINT],
				}),
			]);
			expect(trustLogs(HARNESS_TRUST_ADJUSTED_MSG)).toEqual([
				expect.objectContaining({
					proposalId: "refine_seed",
					scope: "local",
					entry: SKILL_REF,
					reason: "measured_fault",
					delta: -15,
					before: 50,
					after: 35,
					dormant: false,
					fingerprintId: PROBE_FINGERPRINT,
				}),
			]);

			await runTool(harness, "ipython");
			await internalsOf(harness)._trustAdjudication;
			expect(named("harness.trust.adjudicate")).toHaveLength(1);
			expect(stateOf(harness, "local").entries.skill[SKILL_ID].trust?.score).toBe(35);
		},
	);

	it.skipIf(PYTHON === undefined)(
		"adjudicates a global window and writes its evidence at a root harness.ledger.flush (python)",
		async () => {
			writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
			seedWindow(getGlobalHarnessStateDir(), "global", {
				kind: "skill",
				fingerprint: PROBE_FINGERPRINT,
				untilTurn: 22,
			});
			const harness = await sessionWith([probeTool()]);

			await runTool(harness, "ipython");
			await internalsOf(harness)._trustAdjudication;

			const state = stateOf(harness, "global");
			expect(state.entries.skill[SKILL_ID].trust?.score).toBe(35);
			expect(state.trustWindows?.refine_seed).toMatchObject({
				outcome: "faulted",
				recurrences: { [PROBE_FINGERPRINT]: 3 },
				adjudications: [{ entry: SKILL_REF, status: "upheld", ordinal: 3 }],
			});
			expect(stateOf(harness, "local").trustWindows).toBeUndefined();

			const flushes = named("harness.ledger.flush");
			const recording = flushes.find((span) => span.attrs["trust.recurrences"] === 1);
			expect(recording?.attrs).toMatchObject({ "trust.adjudications": 0, "trust.faulted": 0 });
			const landing = flushes.filter((span) => span.attrs["trust.adjudications"] === 1);
			expect(landing).toEqual([
				expect.objectContaining({
					status: "ok",
					parentSpanId: undefined,
					attrs: expect.objectContaining({ "ledger.scope": "global", "trust.faulted": 1, "trust.clean": 0 }),
				}),
			]);
			expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([
				expect.objectContaining({ scope: "global", outcome: "faulted", from: "open" }),
			]);
		},
	);

	it.skipIf(PYTHON === undefined)(
		"adjudicates once the self-check lands, without a second recurrence (python)",
		async () => {
			writeFailures([probeRecord(2, [])]);
			const harness = await sessionWith([probeTool()]);
			seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });
			const internals = internalsOf(harness);

			await runTool(harness, "ipython");
			expect(internals._replayVerification).toBeDefined();
			expect(internals._trustAdjudicationsAwaiting.size).toBe(1);
			expect(named("harness.trust.adjudicate")).toEqual([]);

			await internals._replayVerification;
			expect(internals._trustAdjudicationsAwaiting.size).toBe(0);
			await internals._trustAdjudication;
			await nextTurn(harness);

			expect(named("ravo.replay_verify")).toHaveLength(1);
			expect(named("harness.trust.adjudicate")).toEqual([
				expect.objectContaining({ parentSpanId: undefined, attrs: expect.objectContaining({ "trust.upheld": 1 }) }),
			]);
			const state = stateOf(harness, "local");
			expect(state.entries.skill[SKILL_ID].trust?.score).toBe(35);
			expect(state.trustWindows?.refine_seed?.outcome).toBe("faulted");
		},
	);

	it.skipIf(PYTHON === undefined)(
		"stops re-running a (window, entry, fingerprint) after three recorded runs (python)",
		async () => {
			const stubRoot = join(agentDir, "stub-python-root");
			mkdirSync(stubRoot, { recursive: true });
			writeFileSync(join(stubRoot, `${MISSING_MODULE}.py`), "", "utf8");
			process.env.PYTHONPATH = stubRoot;
			writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
			const harness = await sessionWith([probeTool()]);
			seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });

			for (let probe = 0; probe < 4; probe++) {
				await runTool(harness, "ipython");
				await internalsOf(harness)._trustAdjudication;
			}

			const adjudications = named("harness.trust.adjudicate");
			expect(adjudications).toHaveLength(3);
			for (const span of adjudications) {
				expect(span.attrs).toMatchObject({ "trust.cleared": 1, "trust.upheld": 0 });
			}
			const state = stateOf(harness, "local");
			const window = state.trustWindows?.refine_seed;
			expect(window?.outcome).toBe("open");
			expect(window?.adjudications).toEqual([
				expect.objectContaining({ entry: SKILL_REF, status: "cleared", ordinal: 3, runs: expect.any(Array) }),
			]);
			expect(window?.adjudications?.[0]?.runs).toHaveLength(3);
			expect(window?.recurrences).toEqual({ [PROBE_FINGERPRINT]: 3 });
			expect(state.entries.skill[SKILL_ID].trust).toBeUndefined();
		},
	);

	it.skipIf(PYTHON === undefined)(
		"leaves a landing local verdict for the next turn boundary instead of writing the local state while a cell may run (python)",
		async () => {
			slowMissingModule(0.5);
			writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
			const harness = await sessionWith([probeTool()]);
			seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });
			const internals = internalsOf(harness);

			await runTool(harness, "ipython");
			expect(internals._trustAdjudication).toBeDefined();
			const statePath = getHarnessStatePath(localDir(harness));
			const beforeLanding = readFileSync(statePath, "utf8");
			await internals._trustAdjudication;

			expect(readFileSync(statePath, "utf8")).toBe(beforeLanding);
			expect(internals._pendingTrustEvidence).toEqual([
				expect.objectContaining({ type: "adjudication", entry: SKILL_REF, status: "upheld", ordinal: 3 }),
			]);
			expect(named("harness.trust.adjudicate")).toEqual([
				expect.objectContaining({ attrs: expect.objectContaining({ "trust.upheld": 1 }) }),
			]);

			await nextTurn(harness);
			const state = stateOf(harness, "local");
			expect(state.entries.skill[SKILL_ID].trust?.score).toBe(35);
			expect(state.trustWindows?.refine_seed?.outcome).toBe("faulted");
			expect(internals._pendingTrustEvidence).toEqual([]);
		},
	);

	it.skipIf(PYTHON === undefined)(
		"drops an upheld verdict for a skill rewritten to another import while its replay ran (python)",
		async () => {
			slowMissingModule(0.5);
			writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
			const harness = await sessionWith([probeTool()]);
			seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });
			const internals = internalsOf(harness);

			await runTool(harness, "ipython");
			const running = internals._trustAdjudication;
			expect(running).toBeDefined();
			// The kernel's harness API rewrites the skill while its replay is still running.
			const rewritten = stateOf(harness, "local");
			rewritten.entries.skill[SKILL_ID] = {
				...rewritten.entries.skill[SKILL_ID],
				reference: { type: "python", import: "json", callable: "probe" },
				source: "kernel",
				version: 2,
			};
			saveHarnessState(localDir(harness), rewritten);
			await running;
			expect(named("harness.trust.adjudicate")[0]?.attrs).toMatchObject({ "trust.upheld": 1 });

			await nextTurn(harness);
			const state = stateOf(harness, "local");
			expect(state.entries.skill[SKILL_ID].trust).toBeUndefined();
			expect(state.trustWindows?.refine_seed).toMatchObject({
				outcome: "open",
				recurrences: { [PROBE_FINGERPRINT]: 3 },
			});
			expect(state.trustWindows?.refine_seed).not.toHaveProperty("adjudications");
			expect(internals._pendingTrustEvidence).toEqual([]);
			expect(trustLogs(HARNESS_TRUST_ADJUSTED_MSG)).toEqual([]);
		},
	);

	it("frees the trust replays awaiting a self-check batch that fails to run", async () => {
		selfCheck.rejects = true;
		writeFailures([probeRecord(2, [])]);
		const harness = await sessionWith([probeTool()]);
		seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });
		const internals = internalsOf(harness);

		await runTool(harness, "ipython");
		expect(internals._replayVerification).toBeDefined();
		expect(internals._trustAdjudicationsAwaiting.size).toBe(1);
		expect(internals._trustAdjudicationsQueued.size).toBe(1);

		await internals._replayVerification;

		expect(internals._trustAdjudicationsAwaiting.size).toBe(0);
		expect(internals._trustAdjudicationsQueued.size).toBe(0);
		expect(internals._trustAdjudication).toBeUndefined();
		expect(named("harness.trust.adjudicate")).toEqual([]);
	});

	it("runs no replay for a different missing module under the same fingerprint, and still closes the window contested", async () => {
		writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
		const harness = await sessionWith([probeTool(OTHER_MODULE)]);
		seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 3 });
		const internals = internalsOf(harness);

		await runTool(harness, "ipython");
		await internals._replayVerification;
		await internals._trustAdjudication;
		expect(stateOf(harness, "local").trustWindows?.refine_seed).toMatchObject({
			outcome: "open",
			recurrences: { [PROBE_FINGERPRINT]: 3 },
		});
		expect(internals._trustAdjudicationsAwaiting.size).toBe(0);

		await runTool(harness, "ipython");
		await internals._replayVerification;
		await internals._trustAdjudication;

		expect(named("harness.trust.adjudicate")).toEqual([]);
		expect(named("ravo.referee")).toEqual([]);
		const state = stateOf(harness, "local");
		expect(state.trustWindows?.refine_seed).toMatchObject({
			outcome: "contested",
			settledTurn: 4,
			recurrences: { [PROBE_FINGERPRINT]: 3 },
		});
		expect(state.trustWindows?.refine_seed).not.toHaveProperty("adjudications");
		expect(state.entries.skill[SKILL_ID].trust).toBeUndefined();
	});

	it("closes a memory window whose claimed failure recurred as contested at the ledger flush, without running a replay", async () => {
		writeFailures([deployRecord(2)]);
		const harness = await sessionWith([deployTool()]);
		seedWindow(localDir(harness), "local", { kind: "memory", fingerprint: DEPLOY_FINGERPRINT, untilTurn: 3 });

		await runTool(harness, "deploy");
		expect(stateOf(harness, "local").trustWindows?.refine_seed).toMatchObject({
			outcome: "open",
			recurrences: { [DEPLOY_FINGERPRINT]: 3 },
		});
		expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([]);

		await runTool(harness, "deploy");

		const state = stateOf(harness, "local");
		expect(state.trustWindows?.refine_seed?.outcome).toBe("contested");
		expect(state.entries.memory[MEMORY_ID].trust).toBeUndefined();
		expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([
			expect.objectContaining({
				proposalId: "refine_seed",
				scope: "local",
				from: "open",
				outcome: "contested",
				ordinal: 4,
				fingerprints: [DEPLOY_FINGERPRINT],
			}),
		]);
		expect(trustLogs(HARNESS_TRUST_ADJUSTED_MSG)).toEqual([]);
		expect(named("harness.trust.adjudicate")).toEqual([]);
		expect(named("ravo.replay_case")).toEqual([]);
	});

	it("folds pending trust evidence into a refine apply before it settles windows", async () => {
		writeFailures([deployRecord(30)]);
		const harness = await sessionWith([]);
		harness.setResponses([fauxAssistantMessage("hi")]);
		await harness.session.prompt("hello");
		await harness.session.waitForIdle();
		seedWindow(localDir(harness), "local", { kind: "memory", fingerprint: DEPLOY_FINGERPRINT, untilTurn: 3 });
		internalsOf(harness)._pendingTrustEvidence.push({
			type: "recurrence",
			proposalId: "refine_seed",
			fingerprintId: DEPLOY_FINGERPRINT,
			ordinal: 3,
		});
		harness.setResponses([
			fauxAssistantMessage(
				JSON.stringify({
					summary: "Deploy note",
					rationale: "evidence",
					expectedOutcome: "fewer failures",
					edits: [
						{
							action: "create",
							kind: "memory",
							id: "deploy_manifest",
							title: "Deploy manifest",
							content: "Validate the manifest before deploying.",
						},
					],
				}),
			),
			fauxAssistantMessage(
				JSON.stringify({
					verdict: "pass",
					score: 90,
					failedCriteria: [],
					addressedFingerprints: [],
					rationale: "ok",
				}),
			),
		]);

		const result = await harness.session.refine({ instructions: "capture the deploy lesson" });

		expect(result.ravo).toMatchObject({ decision: "commit", measurable: false });
		const state = stateOf(harness, "local");
		expect(state.trustWindows?.refine_seed).toMatchObject({ outcome: "contested", settledTurn: 30 });
		expect(state.entries.memory[MEMORY_ID].trust).toBeUndefined();
		expect(state.entries.memory.deploy_manifest).toBeDefined();
		expect(named("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"trust.contested": 1,
			"trust.clean": 0,
			"trust.faulted": 0,
		});
		expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([
			expect.objectContaining({ proposalId: "refine_seed", scope: "local", outcome: "contested" }),
		]);
	});

	it("settles a global memory trust window contested at the root ledger flush when its claimed failure recurs", async () => {
		writeFailures([deployRecord(2)]);
		const harness = await sessionWith([deployTool()]);
		seedWindow(getGlobalHarnessStateDir(), "global", {
			kind: "memory",
			fingerprint: DEPLOY_FINGERPRINT,
			untilTurn: 3,
		});

		await runTool(harness, "deploy");
		expect(stateOf(harness, "global").trustWindows?.refine_seed).toMatchObject({
			outcome: "open",
			recurrences: { [DEPLOY_FINGERPRINT]: 3 },
		});
		expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([]);

		await runTool(harness, "deploy");

		const state = stateOf(harness, "global");
		expect(state.trustWindows?.refine_seed?.outcome).toBe("contested");
		expect(state.entries.memory[MEMORY_ID].trust).toBeUndefined();
		expect(stateOf(harness, "local").trustWindows).toBeUndefined();
		const settling = named("harness.ledger.flush").find((span) => span.attrs["trust.contested"] === 1);
		expect(settling?.attrs).toMatchObject({ "ledger.scope": "global", "trust.faulted": 0, "trust.clean": 0 });
		expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([
			expect.objectContaining({
				proposalId: "refine_seed",
				scope: "global",
				from: "open",
				outcome: "contested",
				ordinal: 4,
				fingerprints: [DEPLOY_FINGERPRINT],
			}),
		]);
		expect(named("harness.trust.adjudicate")).toEqual([]);
	});

	it("folds global pending trust evidence into a global refine apply before it settles windows", async () => {
		writeFailures([deployRecord(30)]);
		const harness = await sessionWith([]);
		harness.setResponses([fauxAssistantMessage("hi")]);
		await harness.session.prompt("hello");
		await harness.session.waitForIdle();
		seedWindow(getGlobalHarnessStateDir(), "global", {
			kind: "memory",
			fingerprint: DEPLOY_FINGERPRINT,
			untilTurn: 3,
		});
		internalsOf(harness)._globalPendingTrustEvidence.push({
			type: "recurrence",
			proposalId: "refine_seed",
			fingerprintId: DEPLOY_FINGERPRINT,
			ordinal: 3,
		});
		harness.setResponses([
			fauxAssistantMessage(
				JSON.stringify({
					summary: "Deploy note",
					rationale: "evidence",
					expectedOutcome: "fewer failures",
					edits: [
						{
							action: "create",
							kind: "memory",
							id: "deploy_manifest",
							title: "Deploy manifest",
							content: "Validate the manifest before deploying.",
						},
					],
				}),
			),
			fauxAssistantMessage(
				JSON.stringify({
					verdict: "pass",
					score: 90,
					failedCriteria: [],
					addressedFingerprints: [],
					rationale: "ok",
				}),
			),
		]);

		const result = await harness.session.refine({ global: true, instructions: "capture the deploy lesson" });

		expect(result.ravo).toMatchObject({ decision: "commit", measurable: false });
		const state = stateOf(harness, "global");
		expect(state.trustWindows?.refine_seed).toMatchObject({ outcome: "contested", settledTurn: 30 });
		expect(state.entries.memory[MEMORY_ID].trust).toBeUndefined();
		expect(state.entries.memory.deploy_manifest).toBeDefined();
		expect(named("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"refine.scope": "global",
			"trust.contested": 1,
			"trust.clean": 0,
			"trust.faulted": 0,
		});
		expect(trustLogs(HARNESS_TRUST_SETTLED_MSG)).toEqual([
			expect.objectContaining({ proposalId: "refine_seed", scope: "global", outcome: "contested" }),
		]);
	});

	it("adjudicates nothing and moves no trust with the global ledger off", async () => {
		process.env[GLOBAL_FAILURE_LEDGER_ENV] = "0";
		writeFailures([probeRecord(2, [VERIFIED_PROBE])]);
		const harness = await sessionWith([probeTool()]);
		seedWindow(localDir(harness), "local", { kind: "skill", fingerprint: PROBE_FINGERPRINT, untilTurn: 22 });

		await runTool(harness, "ipython");
		await internalsOf(harness)._trustAdjudication;

		expect(named("harness.trust.adjudicate")).toEqual([]);
		const state = stateOf(harness, "local");
		expect(state.trustWindows?.refine_seed?.outcome).toBe("open");
		expect(state.trustWindows?.refine_seed).not.toHaveProperty("recurrences");
		expect(state.entries.skill[SKILL_ID].trust).toBeUndefined();
	});
});
