import { spawnSync } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import type { Usage } from "@earendil-works/pi-ai";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { authorizeAssistedRavo } from "../src/core/ravo/authority.js";
import {
	extractFailures,
	type FailureLedger,
	type FailureRecord,
	failureOpponentId,
	fingerprintFailure,
	updateFailureLedger,
} from "../src/core/ravo/failure-ledger.js";
import type { JsonValue } from "../src/core/ravo/reducer.js";
import {
	deriveReplayCase,
	failureOpponentPassed,
	type ReplayCase,
	refereeOpponentId,
	refereeOpponentPassed,
	verdictFromOutcome,
} from "../src/core/ravo/referee.js";
import { adjudicateFailureClaims, captureReplayCase, runReplayCase } from "../src/core/ravo/referee-runner.js";
import {
	type RavoRunRequest,
	RavoRunService,
	type RavoRunServiceDeps,
	type RavoRunStatus,
} from "../src/core/ravo/run-service.js";
import type { HarnessState } from "../src/core/refinement/refinement.js";
import type { RunAgentHandler, RunAgentResult } from "../src/core/run-agent.js";

function resolvePython(): string | undefined {
	for (const candidate of ["python3", "python"]) {
		const probe = spawnSync(candidate, ["-c", "import sys; sys.stdout.write(sys.executable)"], { encoding: "utf8" });
		if (probe.status === 0 && probe.stdout.trim()) return probe.stdout.trim();
	}
	return undefined;
}

const python = resolvePython();

/**
 * The plan's exit test names `import paramiko`. Some machines (this one) have
 * paramiko installed, and the case has to actually raise for the "flaw upheld"
 * half to mean anything, so the first name the interpreter cannot import wins.
 */
function resolveMissingModule(interpreter: string): string {
	for (const candidate of ["paramiko", "paramiko_missing_dep"]) {
		const probe = spawnSync(interpreter, ["-I", "-c", `import ${candidate}`], { encoding: "utf8" });
		if (probe.status !== 0) return candidate;
	}
	throw new Error("no importable-free module name available for the replay case");
}

let missingModule = "paramiko";
let stubRoot = "";
const cleanup: Array<() => void> = [];
const previousEnv = {
	kernelPython: process.env.PRIME_AGENT_KERNEL_PYTHON,
	pythonPath: process.env.PYTHONPATH,
	agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
};

beforeAll(async () => {
	if (!python) return;
	missingModule = resolveMissingModule(python);
	stubRoot = await mkdtemp(path.join(tmpdir(), "referee-stub-"));
	await writeFile(path.join(stubRoot, `${missingModule}.py`), "VERSION = '1.0'\n", "utf8");
	process.env.PRIME_AGENT_KERNEL_PYTHON = python;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = await mkdtemp(path.join(tmpdir(), "referee-agent-"));
});

afterEach(() => {
	while (cleanup.length > 0) cleanup.pop()?.();
	if (previousEnv.pythonPath === undefined) delete process.env.PYTHONPATH;
	else process.env.PYTHONPATH = previousEnv.pythonPath;
});

afterAll(() => {
	if (previousEnv.kernelPython === undefined) delete process.env.PRIME_AGENT_KERNEL_PYTHON;
	else process.env.PRIME_AGENT_KERNEL_PYTHON = previousEnv.kernelPython;
	if (previousEnv.agentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousEnv.agentDir;
});

function tracebackText(module: string): string {
	return [
		"Executing cell...",
		"Traceback (most recent call last):",
		'  File "<ipython-input-3>", line 1, in <module>',
		`    import ${module}`,
		`ModuleNotFoundError: No module named '${module}'`,
		"",
	].join("\n");
}

function missingModuleFingerprint(module: string) {
	return fingerprintFailure("python_exception", "ipython", "ModuleNotFoundError", `No module named '${module}'`);
}

function verifiedCase(module: string): ReplayCase {
	return {
		language: "python",
		source: `import ${module}`,
		exceptionClass: "ModuleNotFoundError",
		verifiedAt: "2026-09-15T00:00:00.000Z",
	};
}

function recurringRecord(module: string, replayCase: ReplayCase | undefined): FailureRecord {
	return {
		fingerprint: missingModuleFingerprint(module),
		count: 3,
		firstSeenTurn: 2,
		lastSeenTurn: 6,
		firstSeenAt: "2026-09-15T00:00:00.000Z",
		lastSeenAt: "2026-09-15T00:05:00.000Z",
		excerpt: `ModuleNotFoundError: No module named '${module}'`,
		addressedByProposalIds: [],
		...(replayCase === undefined ? {} : { replayCase }),
	};
}

function ledgerOf(record: FailureRecord): FailureLedger {
	return { schema: 1, lastScannedEntryIndex: 3, failures: { [record.fingerprint.id]: record } };
}

describe.skipIf(!python)("referee replay cases", () => {
	it("derives an executable case from a missing-module traceback and stores it on the ledger record", () => {
		const fingerprint = missingModuleFingerprint("paramiko");
		const derived = deriveReplayCase(fingerprint, "ModuleNotFoundError: No module named 'paramiko'");
		expect(derived).toEqual({ language: "python", source: "import paramiko", exceptionClass: "ModuleNotFoundError" });
		expect(derived?.verifiedAt).toBeUndefined();

		const observations = extractFailures(
			[
				{
					role: "toolResult",
					toolCallId: "call-1",
					toolName: "ipython",
					content: [{ type: "text", text: tracebackText("paramiko") }],
					isError: false,
					timestamp: 1,
				},
			],
			{ fromEntryIndex: 0, turn: 3 },
		);
		expect(observations[0]?.replayCase?.source).toBe("import paramiko");
		const ledger = updateFailureLedger({ schema: 1, failures: {}, lastScannedEntryIndex: 0 }, observations).ledger;
		expect(ledger.failures[observations[0].fingerprint.id].replayCase?.source).toBe("import paramiko");
	});

	it("derives nothing for a failure with no side-effect-free reproduction", () => {
		const fingerprint = fingerprintFailure("python_exception", "ipython", "KeyError", "'results'");
		expect(deriveReplayCase(fingerprint, "KeyError: 'results'")).toBeUndefined();
		expect(deriveReplayCase({ ...fingerprint, kind: "tool_error" }, "No module named 'paramiko'")).toBeUndefined();
	});

	it("re-executes the case in a subprocess: raises without the module, clean with a scratch root on sys.path", async () => {
		const replay = verifiedCase(missingModule);
		const raised = await runReplayCase(replay);
		expect(raised).toMatchObject({ kind: "raised", exceptionClass: "ModuleNotFoundError" });
		expect(verdictFromOutcome(replay, raised)).toBe("upheld");

		const clean = await runReplayCase(replay, { sysPath: [stubRoot] });
		expect(clean.kind).toBe("clean");
		expect(verdictFromOutcome(replay, clean)).toBe("cleared");
	});

	it("keeps a captured case only when the self-check reproduces the recorded exception", async () => {
		const observation = extractFailures(
			[
				{
					role: "toolResult",
					toolCallId: "call-1",
					toolName: "ipython",
					content: [{ type: "text", text: tracebackText(missingModule) }],
					isError: false,
					timestamp: 1,
				},
			],
			{ fromEntryIndex: 0, turn: 1 },
		)[0];
		const captured = await captureReplayCase(observation, { now: () => "2026-09-15T12:00:00.000Z" });
		expect(captured).toMatchObject({ source: `import ${missingModule}`, verifiedAt: "2026-09-15T12:00:00.000Z" });
		// The same derivation against an environment where the import works never
		// reproduced, so it is not recorded as evidence.
		expect(await captureReplayCase(observation, { sysPath: [stubRoot] })).toBeUndefined();
	});

	it("fails closed when the verification cannot run, and abstains when there is no evidence", async () => {
		const replay = verifiedCase(missingModule);
		const outcome = await runReplayCase(replay, { pythonPath: path.join(stubRoot, "no-such-python") });
		expect(outcome.kind).toBe("unrunnable");
		const unverifiable = verdictFromOutcome(replay, outcome);
		expect(unverifiable).toBe("unverifiable");
		const verdict = { fingerprintId: "f", status: unverifiable, detail: outcome.detail };
		expect(failureOpponentPassed(true, verdict)).toBe(false);
		expect(refereeOpponentPassed(true, verdict)).toBe(false);

		// A case that never reproduced is not an oracle: a clean run of it says
		// nothing, so the referee abstains and the claim is honoured as before.
		const unverified: ReplayCase = { language: "python", source: `import ${missingModule}` };
		expect(verdictFromOutcome(unverified, { kind: "clean", detail: "" })).toBe("no_evidence");
		const blind = recurringRecord(missingModule, undefined);
		const [noEvidence] = await adjudicateFailureClaims([blind], [blind.fingerprint.id]);
		expect(noEvidence.status).toBe("no_evidence");
		expect(failureOpponentPassed(true, noEvidence)).toBe(true);
		expect(refereeOpponentPassed(true, noEvidence)).toBe(true);
		expect(failureOpponentPassed(false, noEvidence)).toBe(false);
	});

	it("adjudicates only the fingerprints the proposal claims", async () => {
		const record = recurringRecord(missingModule, verifiedCase(missingModule));
		expect(await adjudicateFailureClaims([record], [])).toEqual([]);
		const [verdict] = await adjudicateFailureClaims([record], [record.fingerprint.id]);
		expect(verdict).toMatchObject({ fingerprintId: record.fingerprint.id, status: "upheld" });
	});
});

const artifact = {
	summary: "Note the missing dependency",
	edits: [{ action: "create", kind: "memory", title: "deps", content: "remember the import" }],
} as unknown as JsonValue;
const baseline = { entries: { memory: {} }, refinements: [] } as unknown as JsonValue;

describe.skipIf(!python)("assisted gate with the referee", () => {
	it("rejects a refuted claim naming the failure opponent, and commits the same claim once the case runs clean", async () => {
		const record = recurringRecord(missingModule, verifiedCase(missingModule));
		const fingerprintId = record.fingerprint.id;
		const observation = {
			status: "pass" as const,
			score: 90,
			failedCriteria: [],
			addressedFingerprints: [fingerprintId],
		};

		const refuted = await adjudicateFailureClaims([record], [fingerprintId]);
		expect(refuted[0].status).toBe("upheld");
		const rejected = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation,
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: refuted,
			epsilon: 1,
			turn: 3,
		});
		expect(rejected.certificate.committed).toBe(false);
		expect(rejected.certificate.rejection).toBe("opponents");
		expect(rejected.certificate.missedCriterionIds).toEqual([
			failureOpponentId(fingerprintId),
			refereeOpponentId(fingerprintId),
		]);
		expect(rejected.certificate.missedCurrentWeight).toBe(2);

		process.env.PYTHONPATH = stubRoot;
		const cleared = await adjudicateFailureClaims([record], [fingerprintId]);
		expect(cleared[0].status).toBe("cleared");
		const committed = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation,
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: cleared,
			epsilon: 1,
			turn: 3,
		});
		expect(committed.certificate.committed).toBe(true);
		expect(committed.certificate.missedCriterionIds).toEqual([]);
		expect(committed.nextState.lineage.at(-1)).toMatchObject({ claimedFingerprints: [fingerprintId] });
	});

	it("leaves the gate exactly as it was for a fingerprint with no replay case", async () => {
		const record = recurringRecord(missingModule, undefined);
		const fingerprintId = record.fingerprint.id;
		const verdicts = await adjudicateFailureClaims([record], [fingerprintId]);
		const authorized = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: [fingerprintId] },
			failureOpponents: [failureOpponentId(fingerprintId)],
			refereeVerdicts: verdicts,
			epsilon: 1,
			turn: 3,
		});
		expect(authorized.certificate.committed).toBe(true);
		expect(authorized.certificate.criteria.map((item) => item.criterionId)).not.toContain(
			refereeOpponentId(fingerprintId),
		);
	});
});

const usage = (totalTokens: number): Usage => ({
	input: totalTokens,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});

function fauxRunAgent(proposal: unknown): RunAgentHandler {
	return async (request) => {
		const role = /^# RAVO (\w+)/.exec(request.prompt)?.[1];
		const value =
			role === "inspect"
				? { summary: "the dependency is missing", facts: ["the import fails"] }
				: role === "plan"
					? { steps: ["record the dependency"] }
					: role === "implement" || role === "repair"
						? proposal
						: role === "judge"
							? { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [], rationale: "ok" }
							: { intervene: false };
		const result: RunAgentResult = {
			status: "completed",
			output: JSON.stringify(value),
			messages: [],
			model: "faux/child",
			turns: 1,
			toolCalls: 0,
			usage: usage(10),
		};
		return result;
	};
}

async function runService(failures: FailureLedger, proposal: unknown) {
	const harnessDir = await mkdtemp(path.join(tmpdir(), "referee-run-"));
	let state: HarnessState = {
		schema: 1,
		entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
		refinements: [],
		failures,
	};
	const saveState = vi.fn(async (next: HarnessState) => {
		state = structuredClone(next);
	});
	const updates: RavoRunStatus[] = [];
	const deps: RavoRunServiceDeps = {
		runAgent: fauxRunAgent(proposal),
		harnessDir,
		loadState: async () => structuredClone(state),
		saveState,
		onUpdate: (status) => updates.push(status),
	};
	return { service: new RavoRunService(deps), saveState, state: () => state };
}

describe.skipIf(!python)("RavoRunService with the referee", () => {
	const request: RavoRunRequest = { task: "stop the import failure", maxRounds: 2, maxRepairs: 0 };

	it("refuses the proposal whose claim the replay case refutes, and accepts it byte-identical once the case runs clean", async () => {
		const record = recurringRecord(missingModule, verifiedCase(missingModule));
		const fingerprintId = record.fingerprint.id;
		const proposal = {
			summary: "Remember the dependency",
			rationale: "The import failed three times.",
			expectedOutcome: "No more import failures.",
			addressedFingerprints: [fingerprintId],
			edits: [
				{
					action: "create",
					kind: "memory",
					title: "Missing dependency",
					content: `Install ${missingModule} before importing it.`,
					path: "deps/python",
				},
			],
		};

		const refuted = await runService(ledgerOf(record), proposal);
		const rejectedRun = await refuted.service.start(request);
		expect(rejectedRun.stopReason).not.toBe("accepted");
		expect(rejectedRun.lastCertificate?.status).toBe("reject_criteria");
		expect(rejectedRun.lastCertificate?.missed).toEqual([
			failureOpponentId(fingerprintId),
			refereeOpponentId(fingerprintId),
		]);
		expect(refuted.saveState).not.toHaveBeenCalled();

		// Same ledger, same proposal object; only the world changed.
		process.env.PYTHONPATH = stubRoot;
		const cleared = await runService(ledgerOf(record), proposal);
		const acceptedRun = await cleared.service.start(request);
		expect(acceptedRun.stopReason).toBe("accepted");
		expect(acceptedRun.lastCertificate?.status).toBe("commit");
		expect(acceptedRun.lastCertificate?.missed).toEqual([]);
		expect(cleared.saveState).toHaveBeenCalledOnce();
		expect(cleared.state().ravo?.lineage.at(-1)).toMatchObject({ claimedFingerprints: [fingerprintId] });
	});

	it("rejects at the deep gate when the judge itself returns a fail verdict", async () => {
		const record = recurringRecord(missingModule, undefined);
		const proposal = {
			summary: "Remember the dependency",
			rationale: "The import failed three times.",
			expectedOutcome: "No more import failures.",
			addressedFingerprints: [record.fingerprint.id],
			edits: [{ action: "create", kind: "memory", title: "Missing dependency", content: "install it" }],
		};
		const harnessDir = await mkdtemp(path.join(tmpdir(), "referee-run-"));
		const state: HarnessState = {
			schema: 1,
			entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
			refinements: [],
			failures: ledgerOf(record),
		};
		const saveState = vi.fn(async () => {});
		const base = fauxRunAgent(proposal);
		const service = new RavoRunService({
			runAgent: async (req, options) => {
				if (!req.prompt.startsWith("# RAVO judge")) return base(req, options);
				return {
					status: "completed",
					output: JSON.stringify({ verdict: "fail", score: 95, failedCriteria: [], rationale: "worse" }),
					messages: [],
					model: "faux/child",
					turns: 1,
					toolCalls: 0,
					usage: usage(10),
				};
			},
			harnessDir,
			loadState: async () => structuredClone(state),
			saveState,
			onUpdate: () => {},
		});
		const terminal = await service.start(request);
		expect(terminal.stopReason).not.toBe("accepted");
		expect(terminal.lastCertificate?.status).toBe("reject_deep");
		expect(terminal.lastCertificate?.deepScore).toBe(95);
		expect(saveState).not.toHaveBeenCalled();
	});
});
