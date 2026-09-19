import { AsyncLocalStorage } from "node:async_hooks";
import { once } from "node:events";
import { existsSync, readFileSync } from "node:fs";
import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import path from "node:path";
import { Worker } from "node:worker_threads";
import {
	addSpanSink,
	installAsyncTraceContextStorage,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
	type Usage,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { REFINEMENT_COMMITTED_MSG, REFINEMENT_LOG_COMPONENT, rollUpLearningDays } from "../src/core/learning-index.js";
import type { ArcRunner } from "../src/core/ravo/arc-agi-evaluator.js";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import { type FailureLedger, recordProvisionalRegressions } from "../src/core/ravo/failure-ledger.js";
import type { JsonValue, RavoGateCertificate, RavoState } from "../src/core/ravo/reducer.js";
import {
	creditedRavoRunClaims,
	judgedRavoRunClaims,
	type RavoRunRequest,
	RavoRunService,
	type RavoRunServiceDeps,
	type RavoRunStatus,
	ravoRunHarnessStores,
} from "../src/core/ravo/run-service.js";
import type { RetainedWorkerRequest, RetainedWorkerRuntime } from "../src/core/ravo/runtime-adapter.js";
import { REFINEMENT_APPLIED_UNMEASURED_MSG, REFINEMENT_REJECTED_MSG } from "../src/core/refinement/ravo.js";
import {
	applyRefinementProposal,
	getHarnessStatePath,
	type HarnessScope,
	type HarnessState,
	loadHarnessState,
	normalizeRefinementProposal,
	saveHarnessState,
	withHarnessStateLock,
} from "../src/core/refinement/refinement.js";
import type { RunAgentHandler, RunAgentOptions, RunAgentResult } from "../src/core/run-agent.js";

type Role = "inspect" | "plan" | "implement" | "repair" | "judge" | "supervisor";
type Script = Partial<Record<Role, (call: number, prompt: string, options?: RunAgentOptions) => unknown>>;

const usage = (totalTokens: number): Usage => ({
	input: totalTokens,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});

const proposal = (addressedFingerprints: string[] = []) => ({
	summary: "Record the deploy gate",
	rationale: "The user asked for a database barrier twice.",
	expectedOutcome: "Future deploys wait on the barrier.",
	addressedFingerprints,
	edits: [
		{
			action: "create",
			kind: "memory",
			title: "Deploy barrier",
			content: "Wait on the database-state barrier before deploying.",
			path: "deploy/gates",
		},
	],
});
// The deep gate only passes on an explicit pass token: a judge that omits the
// field abstains, which rejects. These fixtures predate that and mean "judged
// and passed", so they say so.
const judgeVerdict = (score: number, failedCriteria: string[] = []) => ({
	verdict: "pass",
	score,
	failedCriteria,
	addressedFingerprints: [],
	rationale: "judged",
});
const defaultScript: Script = {
	inspect: () => ({ summary: "harness has no deploy notes", facts: ["no memory mentions deploy"] }),
	plan: () => ({ steps: ["create memory deploy_barrier"] }),
	implement: () => proposal(),
	repair: () => proposal(),
	judge: () => judgeVerdict(80),
	supervisor: () => ({ intervene: false }),
};

function fakeRunAgent(script: Script): { runAgent: RunAgentHandler; calls: Record<Role, number> } {
	const calls: Record<Role, number> = { inspect: 0, plan: 0, implement: 0, repair: 0, judge: 0, supervisor: 0 };
	const runAgent: RunAgentHandler = async (request, options) => {
		const match = request.prompt.match(/^# RAVO (\w+)/);
		const role = match?.[1] as Role | undefined;
		if (!role || !(role in calls)) throw new Error(`unknown role prompt: ${request.prompt.slice(0, 40)}`);
		calls[role] += 1;
		const handler = script[role] ?? defaultScript[role];
		if (!handler) throw new Error(`no script for ${role}`);
		const value = await handler(calls[role], request.prompt, options);
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
	return { runAgent, calls };
}

function harnessState(overrides: Partial<HarnessState> = {}): HarnessState {
	return {
		schema: 1,
		entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
		refinements: [],
		...overrides,
	};
}

function seededRavo(score: number): RavoState<JsonValue> {
	const state = emptyAssistedRavoState();
	return {
		...state,
		lineage: [{ proposalId: "seed", parentId: null, score, artifact: null, missedCriterionIds: [] }],
		championId: "seed",
		evaluatedProposalIds: ["seed"],
	};
}

function failures(fingerprintId: string): FailureLedger {
	return {
		schema: 1,
		lastScannedEntryIndex: 3,
		failures: {
			[fingerprintId]: {
				fingerprint: { id: fingerprintId, kind: "tool_error", message: "bash exited with #" },
				count: 3,
				firstSeenTurn: 1,
				lastSeenTurn: 3,
				firstSeenAt: "2026-01-01T00:00:00.000Z",
				lastSeenAt: "2026-01-01T00:01:00.000Z",
				excerpt: "bash exited with 1",
				addressedByProposalIds: [],
			},
		},
	};
}

async function harness(
	script: Script,
	initial: HarnessState = harnessState(),
	extra: Partial<RavoRunServiceDeps> = {},
) {
	const harnessDir = await mkdtemp(path.join(tmpdir(), "ravo-run-service-"));
	let state = initial;
	const updates: RavoRunStatus[] = [];
	const saveState = vi.fn((_scope: HarnessScope, next: HarnessState) => {
		state = structuredClone(next);
	});
	const fake = fakeRunAgent(script);
	const deps: RavoRunServiceDeps = {
		runAgent: fake.runAgent,
		harnessDir,
		loadState: () => structuredClone(state),
		saveState,
		withStateLock: (_scope, fn) => fn(),
		onUpdate: (status) => updates.push(status),
		...extra,
	};
	const service = new RavoRunService(deps);
	return { service, deps, updates, saveState, calls: fake.calls, harnessDir, state: () => state };
}

const request: RavoRunRequest = { task: "remember the deploy barrier", maxRounds: 3, maxRepairs: 2 };
const phases = (updates: RavoRunStatus[]) =>
	updates.filter((status) => status.lastEvent?.type === "phase").map((status) => status.phase);

describe("RavoRunService", () => {
	it("commits a passing proposal once and grows the persisted lineage by one", async () => {
		const seeded = harnessState({ ravo: seededRavo(60) });
		const { service, updates, saveState, calls, harnessDir, state } = await harness({}, seeded);
		let checkpointSeen = false;
		const runId = () => service.status()?.runId ?? "";
		const original = updates.push.bind(updates);
		updates.push = (...items: RavoRunStatus[]) => {
			if (items[0]?.phase === "evaluate") {
				checkpointSeen = existsSync(path.join(harnessDir, "ravo", "runs", `${runId()}.json`));
			}
			return original(...items);
		};
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.phase).toBe("accepted");
		expect(terminal.repairs).toBe(0);
		expect(terminal.round).toBe(1);
		expect(phases(updates)).toEqual(["inspect", "plan", "implement", "evaluate", "commit_gate"]);
		expect(updates.at(-1)).toEqual(terminal);
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", screenScore: 100, deepScore: 80, missed: [] });
		expect(saveState).toHaveBeenCalledOnce();
		expect(calls).toMatchObject({ inspect: 1, plan: 1, implement: 1, repair: 0, judge: 1, supervisor: 0 });
		const persisted = state();
		expect(persisted.ravo?.lineage).toHaveLength(2);
		expect(persisted.ravo?.lineage.at(-1)).toMatchObject({
			proposalId: terminal.candidateId,
			parentId: "seed",
			score: 80,
		});
		expect(Object.values(persisted.entries.memory)).toHaveLength(1);
		expect(Object.values(persisted.entries.memory)[0]).toMatchObject({ title: "Deploy barrier", scope: "local" });
		expect(persisted.refinements).toHaveLength(1);
		expect(checkpointSeen).toBe(true);
		expect(existsSync(path.join(harnessDir, "ravo", "runs", `${terminal.runId}.json`))).toBe(false);
		expect(existsSync(path.join(harnessDir, "ravo", "archive", "events.jsonl"))).toBe(true);
		expect(service.running).toBe(false);
	});

	it("repairs a deep rejection and ends accepted with one repair", async () => {
		const { service, updates, saveState, calls, state } = await harness(
			{ judge: (call) => judgeVerdict(call === 1 ? 30 : 90) },
			harnessState({ ravo: seededRavo(80) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.repairs).toBe(1);
		expect(terminal.round).toBe(2);
		expect(calls).toMatchObject({ implement: 1, repair: 1, judge: 2 });
		const certificates = updates
			.filter((status) => status.lastEvent?.type === "evaluation")
			.map((s) => s.lastCertificate);
		expect(certificates.map((certificate) => certificate?.status)).toEqual(["reject_deep", "commit"]);
		expect(phases(updates)).toEqual([
			"inspect",
			"plan",
			"implement",
			"evaluate",
			"diagnose",
			"inspect",
			"plan",
			"repair",
			"evaluate",
			"commit_gate",
		]);
		expect(saveState).toHaveBeenCalledOnce();
		expect(state().ravo?.lineage).toHaveLength(2);
		expect(state().ravo?.evaluatedProposalIds).toHaveLength(3);
	});

	it("rejects a proposal that ignores a recurring failure fingerprint", async () => {
		const { service, saveState, harnessDir } = await harness(
			{ judge: () => judgeVerdict(85, ["evidence"]) },
			harnessState({ failures: failures("abc123") }),
		);
		const terminal = await service.start({ ...request, maxRepairs: 0 });
		expect(terminal.stopReason).toBe("repair_limit");
		expect(terminal.phase).toBe("stopped");
		expect(terminal.lastCertificate?.status).toBe("reject_criteria");
		expect(terminal.lastCertificate?.missed).toEqual(["evidence", "failure:abc123"]);
		expect(saveState).not.toHaveBeenCalled();
		expect(existsSync(path.join(harnessDir, "ravo", "runs", `${terminal.runId}.json`))).toBe(true);
	});

	it("passes a recurring failure opponent when the proposal claims the fingerprint", async () => {
		const { service, state } = await harness(
			{ implement: () => proposal(["abc123"]) },
			harnessState({ failures: failures("abc123") }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate?.missed).toEqual([]);
		expect(state().ravo?.lineage.at(-1)).toMatchObject({ claimedFingerprints: ["abc123"] });
		expect(state().ravo?.opponents.criteria.map((criterion) => criterion.id)).toContain("failure:abc123");
	});

	it("does not charge a judge run for criteria persisted by an external-evaluator run", async () => {
		const ravo = seededRavo(60);
		ravo.opponents = {
			criteria: [
				...ravo.opponents.criteria,
				{ id: "arc:all-levels", seedWeight: 1, currentWeight: 2 },
				{ id: "arc:no-crash", seedWeight: 1, currentWeight: 1 },
			],
		};
		const { service, state } = await harness({}, harnessState({ ravo }));
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate?.missed).toEqual([]);
		expect(
			state().ravo?.opponents.criteria.find((criterion) => criterion.id === "arc:all-levels")?.currentWeight,
		).toBe(2);
	});

	it("persists a referee opponent only when a replay adjudicated the committed claim", async () => {
		const ledger = failures("abc123");
		ledger.failures.abc123 = {
			...ledger.failures.abc123,
			replayCases: [
				{
					language: "python",
					source: "import prime_agent_absent_module",
					exceptionClass: "ModuleNotFoundError",
					verifiedAt: "2026-01-01T00:02:00.000Z",
				},
			],
		};
		// A memory-only fix: no replay can observe it, so no subprocess runs and
		// the referee opponent the run carried is not written back.
		const { service, state } = await harness(
			{ implement: () => proposal(["abc123"]) },
			harnessState({ failures: ledger }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate?.missed).toEqual([]);
		const ids = state().ravo?.opponents.criteria.map((criterion) => criterion.id);
		expect(ids).toContain("failure:abc123");
		expect(ids).not.toContain("referee:abc123");
		expect(state().ravo?.lineage.at(-1)).toMatchObject({ claimedFingerprints: ["abc123"] });
	});

	it("adds a referee opponent and lists replay evidence only for a probe adjudication can apply", async () => {
		const verifiedAt = "2026-01-01T00:02:00.000Z";
		const run = async (replayCases: NonNullable<FailureLedger["failures"][string]["replayCases"]>) => {
			const ledger = failures("abc123");
			ledger.failures.abc123 = { ...ledger.failures.abc123, replayCases };
			const judgePrompts: string[] = [];
			const { service, harnessDir, saveState } = await harness(
				{
					implement: () => proposal(["abc123"]),
					judge: (_call, prompt) => {
						judgePrompts.push(prompt);
						return { ...judgeVerdict(80), verdict: "fail" };
					},
				},
				harnessState({ failures: ledger }),
			);
			const terminal = await service.start({ ...request, maxRepairs: 0 });
			expect(terminal.lastCertificate?.status).toBe("reject_deep");
			expect(saveState).not.toHaveBeenCalled();
			const checkpoint = JSON.parse(
				readFileSync(path.join(harnessDir, "ravo", "runs", `${terminal.runId}.json`), "utf8"),
			) as { state: RavoState<JsonValue> };
			return {
				opponents: checkpoint.state.opponents.criteria.map((criterion) => criterion.id),
				judgePrompt: judgePrompts.join("\n"),
			};
		};

		const module = await run([
			{
				language: "python",
				source: "import prime_agent_absent_module",
				exceptionClass: "ModuleNotFoundError",
				verifiedAt,
			},
		]);
		expect(module.opponents).toEqual(expect.arrayContaining(["failure:abc123", "referee:abc123"]));
		expect(module.judgePrompt).toContain("replay case (re-run to check the fix): import prime_agent_absent_module");

		// Cases a ledger stored before attribute, name and executable probes were retired, verified at the time.
		const retired = await run([
			{
				language: "python",
				source: "from json import load_string",
				exceptionClass: "ImportError",
				verifiedAt,
			},
			{
				language: "python",
				source: 'import json\ngetattr(json, "load_string")',
				exceptionClass: "AttributeError",
				verifiedAt,
			},
			{
				language: "python",
				source: 'import shutil\nif shutil.which("build") is None:\n    raise FileNotFoundError("build")',
				exceptionClass: "FileNotFoundError",
				verifiedAt,
			},
		]);
		expect(retired.opponents).toContain("failure:abc123");
		expect(retired.opponents).not.toContain("referee:abc123");
		expect(retired.judgePrompt).toContain("- failure:abc123 [tool_error]");
		expect(retired.judgePrompt).not.toContain("replay case (re-run");
		expect(retired.judgePrompt).not.toMatch(/turns=\S+ replay=verified/);
		expect(retired.judgePrompt).not.toContain("load_string");
	});

	it("cancels a running run and rejects a concurrent start", async () => {
		const { service, updates } = await harness({
			implement: (_call, _prompt, options) =>
				new Promise((resolve) => {
					options?.signal?.addEventListener("abort", () => resolve(proposal()), { once: true });
				}),
		});
		const pending = service.start(request);
		expect(service.running).toBe(true);
		expect(service.status()?.runId).toMatch(/^ravo_/);
		await expect(service.start(request)).rejects.toThrow(/already running/);
		await vi.waitFor(() => expect(updates.some((status) => status.phase === "implement")).toBe(true));
		expect(service.cancel()).toBe(true);
		const terminal = await pending;
		expect(terminal.stopReason).toBe("cancelled");
		expect(terminal.phase).toBe("stopped");
		expect(service.running).toBe(false);
		expect(service.cancel()).toBe(false);
	});

	it("reports an unexpected child failure and rethrows", async () => {
		const { service } = await harness({
			inspect: () => {
				throw new Error("provider down");
			},
		});
		await expect(service.start(request)).rejects.toThrow(/provider down/);
		expect(service.status()).toMatchObject({ phase: "stopped", error: "provider down" });
		expect(service.running).toBe(false);
	});

	it("retries a child that returned malformed structured output", async () => {
		const { service, calls } = await harness({ implement: (call) => (call === 1 ? "not a proposal" : proposal()) });
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(calls.implement).toBe(2);
		expect(terminal.repairs).toBe(0);
	});

	it("continues the same retained worker for the repair when a retained runtime is provided", async () => {
		const requests: RetainedWorkerRequest[] = [];
		const spawn = vi.fn(async (req: RetainedWorkerRequest) => {
			requests.push(req);
			return { handle: "worker-1" };
		});
		const cont = vi.fn(async (_handle: string, req: RetainedWorkerRequest) => {
			requests.push(req);
		});
		const runtime: RetainedWorkerRuntime = {
			spawn,
			continue: cont,
			wait: async (handle) => {
				expect(handle).toBe("worker-1");
				return { status: "completed", result: proposal(), tokens: 12 };
			},
		};
		const { service, calls, state } = await harness(
			{ judge: (call) => judgeVerdict(call === 1 ? 30 : 90) },
			harnessState({ ravo: seededRavo(80) }),
			{ retainedRuntime: runtime },
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.repairs).toBe(1);
		expect(spawn).toHaveBeenCalledOnce();
		expect(cont).toHaveBeenCalledOnce();
		expect(cont.mock.calls[0]?.[0]).toBe("worker-1");
		expect(requests.map((req) => req.role)).toEqual(["implement", "repair"]);
		expect(requests[1]?.prompt).toMatch(/^# RAVO repair/);
		expect(calls).toMatchObject({ implement: 0, repair: 0, judge: 2 });
		expect(state().ravo?.lineage.at(-1)).toMatchObject({ parentId: "seed", score: 90 });
	});
});

describe("ravo.run outcome records", () => {
	let entries: LogEntry[];

	beforeEach(() => {
		entries = [];
		setLogSink((entry) => entries.push(entry));
	});

	afterEach(() => {
		setLogSink(undefined);
	});

	const outcomeLines = () => entries.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT);
	const judgeNaming = (score: number, addressedFingerprints: string[]) => () => ({
		...judgeVerdict(score),
		addressedFingerprints,
	});
	const claimedAndNamed = (fingerprintId: string): Script => ({
		implement: () => proposal([fingerprintId]),
		judge: judgeNaming(80, [fingerprintId]),
	});

	it("logs refinement.committed for a commit whose claim the judge also named", async () => {
		const { service, state } = await harness(
			{
				implement: () => proposal(["abc123", "not-recurring"]),
				judge: judgeNaming(80, ["abc123", "not-recurring"]),
			},
			harnessState({ failures: failures("abc123") }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		const lines = outcomeLines();
		expect(lines).toHaveLength(1);
		expect(lines[0]).toMatchObject({
			msg: REFINEMENT_COMMITTED_MSG,
			proposalId: terminal.candidateId,
			addressed: ["abc123"],
			deepScore: 80,
			missed: 0,
			reason: "ravo_run",
			scope: "local",
		});
		expect(lines[0]).not.toHaveProperty("decision");
		expect(lines[0]).not.toHaveProperty("claimed");
		expect(state().refinements.at(-1)).toMatchObject({ id: terminal.candidateId, reason: "ravo_run" });
	});

	it("accepts a judge that names the fingerprint in its opponent form", async () => {
		const { service } = await harness(
			{ implement: () => proposal(["abc123"]), judge: judgeNaming(80, ["failure:abc123"]) },
			harnessState({ failures: failures("abc123") }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: REFINEMENT_COMMITTED_MSG,
				proposalId: terminal.candidateId,
				addressed: ["abc123"],
			}),
		]);
	});

	it("logs a commit claimed only by the proposal as refinement.applied_unmeasured", async () => {
		const { service } = await harness(
			{ implement: () => proposal(["abc123"]) },
			harnessState({ failures: failures("abc123") }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		const lines = outcomeLines();
		expect(lines).toEqual([
			expect.objectContaining({
				msg: REFINEMENT_APPLIED_UNMEASURED_MSG,
				proposalId: terminal.candidateId,
				deepScore: 80,
				reason: "ravo_run",
				scope: "local",
			}),
		]);
		expect(lines[0]).not.toHaveProperty("addressed");
	});

	it("drops a claim the certificate charged even when the proposal commits", async () => {
		const ledger = failures("abc123");
		// Never verified, so the referee has no evidence and the claim misses failure:abc123 alone, within epsilon.
		ledger.failures.abc123 = {
			...ledger.failures.abc123,
			replayCases: [
				{ language: "python", source: "import prime_agent_absent_module", exceptionClass: "ModuleNotFoundError" },
			],
		};
		const probe = {
			action: "create",
			kind: "skill",
			title: "Absent module probe",
			content: "Calls the module the recorded failure could not import.",
			path: "deploy/probe",
			reference: { type: "python", import: "prime_agent_absent_module", callable: "run" },
			arguments: {},
		};
		const { service } = await harness(
			{
				implement: () => ({ ...proposal(["abc123"]), edits: [...proposal().edits, probe] }),
				judge: judgeNaming(80, ["abc123"]),
			},
			harnessState({ failures: ledger }),
		);
		const terminal = await service.start({ ...request, maxRounds: 1, maxRepairs: 0 });
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", missed: ["failure:abc123"] });
		const lines = outcomeLines();
		expect(lines).toEqual([expect.objectContaining({ msg: REFINEMENT_APPLIED_UNMEASURED_MSG, reason: "ravo_run" })]);
	});

	it("logs each rejected proposal once, in order, before the commit", async () => {
		const { service, updates } = await harness(
			{ judge: (call) => judgeVerdict(call === 1 ? 30 : 90) },
			harnessState({ ravo: seededRavo(80) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		const evaluated = updates
			.filter((status) => status.lastEvent?.type === "evaluation")
			.map((status) => status.lastCertificate?.proposalId);
		expect(evaluated).toHaveLength(2);
		expect(evaluated[1]).toBe(terminal.candidateId);
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: REFINEMENT_REJECTED_MSG,
				proposalId: evaluated[0],
				decision: "reject_deep",
				deepScore: 30,
				missed: 0,
				claimed: 0,
				reason: "ravo_run",
				scope: "local",
				cause: "gate",
			}),
			expect.objectContaining({ msg: REFINEMENT_APPLIED_UNMEASURED_MSG, proposalId: evaluated[1], deepScore: 90 }),
		]);
	});

	it("logs a repair-limited run's rejection and no apply record", async () => {
		const { service, saveState } = await harness(
			{ judge: () => judgeVerdict(85, ["evidence"]) },
			harnessState({ failures: failures("abc123") }),
		);
		const terminal = await service.start({ ...request, maxRepairs: 0 });
		expect(terminal.stopReason).toBe("repair_limit");
		expect(outcomeLines()).toEqual([
			expect.objectContaining({
				msg: REFINEMENT_REJECTED_MSG,
				decision: "reject_criteria",
				missed: 2,
				claimed: 0,
				deepScore: 85,
				cause: "gate",
			}),
		]);
		expect(saveState).not.toHaveBeenCalled();
	});

	it("logs an edit set that fails to apply as partial, never as a commit", async () => {
		const initial = harnessState();
		applyRefinementProposal(initial, normalizeRefinementProposal(proposal()), { id: "seed" });
		expect(initial.entries.memory.deploy_barrier).toBeDefined();
		const { service, saveState } = await harness({}, initial);
		const terminal = await service.start({ ...request, maxRepairs: 0 });
		expect(terminal.stopReason).toBe("repair_limit");
		expect(terminal.lastCertificate?.status).toBe("commit");
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: REFINEMENT_REJECTED_MSG, decision: "partial", reason: "ravo_run" }),
		]);
		expect(outcomeLines()[0]).not.toHaveProperty("cause");
		expect(saveState).not.toHaveBeenCalled();
	});

	it("records what rejected a proposal the screen stopped or no judge answered", async () => {
		const noEdits = () => ({ ...proposal(), edits: [] });
		const screened = await harness({ implement: noEdits, repair: noEdits });
		expect((await screened.service.start({ ...request, maxRepairs: 0 })).lastCertificate?.status).toBe(
			"reject_screen",
		);
		const unanswered = await harness({ judge: () => ({ verdict: "pass" }) });
		expect((await unanswered.service.start({ ...request, maxRepairs: 0 })).lastCertificate?.status).toBe(
			"reject_deep",
		);
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: REFINEMENT_REJECTED_MSG, decision: "reject_screen", cause: "screen" }),
			expect.objectContaining({ msg: REFINEMENT_REJECTED_MSG, decision: "reject_deep", cause: "judge_unavailable" }),
		]);
	});

	it("credits only judged, recurring, uncharged claims", () => {
		const artifact = { addressedFingerprints: ["b", "a", "a", "x"] };
		expect(judgedRavoRunClaims(artifact, ["a", "b"], ["a", "failure:b", "x"])).toEqual(["a", "b"]);
		expect(judgedRavoRunClaims(artifact, ["a", "b"], [])).toEqual([]);
		expect(judgedRavoRunClaims({ addressedFingerprints: ["a"] }, ["a", "b"], ["a", "b"])).toEqual(["a"]);
		const certificate = { missedCriterionIds: ["failure:a", "referee:b"] } as RavoGateCertificate;
		expect(creditedRavoRunClaims(["a", "b", "c"], certificate)).toEqual(["c"]);
	});

	it("writes no outcome record when the committed state cannot be saved", async () => {
		const { service } = await harness(claimedAndNamed("abc123"), harnessState({ failures: failures("abc123") }), {
			saveState: () => {
				throw new Error("disk full");
			},
		});
		await expect(service.start(request)).rejects.toThrow(/disk full/);
		expect(outcomeLines()).toEqual([]);
	});

	it("joins each outcome record to the span that decided it", async () => {
		installAsyncTraceContextStorage(new AsyncLocalStorage());
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		try {
			const { service } = await harness(
				{ judge: (call) => judgeVerdict(call === 1 ? 30 : 90) },
				harnessState({ ravo: seededRavo(80) }),
			);
			expect((await service.start(request)).stopReason).toBe("accepted");
		} finally {
			unsubscribe();
		}
		const run = spans.find((span) => span.name === "ravo.run");
		const firstRound = spans.find((span) => span.name === "ravo.round" && span.attrs["ravo.round"] === 1);
		const gate = spans.find(
			(span) => span.name === "ravo.evaluation" && span.attrs["ravo.evaluator"] === "commit_gate",
		);
		expect(run && firstRound && gate).toBeTruthy();
		const [rejected, committed] = outcomeLines();
		expect(rejected).toMatchObject({
			msg: REFINEMENT_REJECTED_MSG,
			traceId: run?.traceId,
			spanId: firstRound?.spanId,
		});
		expect(committed).toMatchObject({
			msg: REFINEMENT_APPLIED_UNMEASURED_MSG,
			traceId: run?.traceId,
			spanId: gate?.spanId,
			proposalId: gate?.attrs["ravo.proposal_id"],
		});
	});

	it("rolls a ravo.run commit into the learning index as a treated commit", async () => {
		const committed = await harness(claimedAndNamed("abc123"), harnessState({ failures: failures("abc123") }));
		const terminal = await committed.service.start(request);
		const unmeasured = await harness(
			{ implement: () => proposal(["abc123"]) },
			harnessState({ failures: failures("abc123") }),
		);
		await unmeasured.service.start(request);
		const [commitLine, unmeasuredLine] = outcomeLines();
		expect([commitLine?.msg, unmeasuredLine?.msg]).toEqual([
			REFINEMENT_COMMITTED_MSG,
			REFINEMENT_APPLIED_UNMEASURED_MSG,
		]);
		const dir = await mkdtemp(path.join(tmpdir(), "ravo-run-learning-"));
		const logOf = async (name: string, line: LogEntry | undefined): Promise<string> => {
			const file = path.join(dir, name);
			await writeFile(file, `${JSON.stringify({ ...line, ts: "2026-09-01T12:00:00.000Z" })}\n`, "utf8");
			return file;
		};
		const nowMs = Date.parse("2026-09-10T00:00:00.000Z");
		const treated = rollUpLearningDays([await logOf("committed.jsonl", commitLine)], nowMs);
		expect(treated.days.flatMap((day) => day.commits)).toEqual([
			expect.objectContaining({ proposalId: terminal.candidateId, addressed: ["abc123"] }),
		]);
		const untreated = rollUpLearningDays([await logOf("unmeasured.jsonl", unmeasuredLine)], nowMs);
		expect(untreated.days.flatMap((day) => day.commits)).toEqual([]);
	});
});

/**
 * Another session's ledger flush, run on a thread so it can hold the global
 * store lock while the run's commit waits for it: lock, read, hold until the
 * run reaches its commit gate (plus a margin), then fold one failure record in,
 * write and release.
 */
const LEDGER_FLUSH_WORKER = `
const { parentPort, workerData } = require("node:worker_threads");
const { readFileSync, writeFileSync } = require("node:fs");
const { lockSync } = require(workerData.lockfile);
const release = lockSync(workerData.statePath, { realpath: false, stale: 10000 });
const state = JSON.parse(readFileSync(workerData.statePath, "utf8"));
parentPort.postMessage("locked");
Atomics.wait(workerData.commitGate, 0, 0, 10000);
Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, workerData.marginMs);
state.failures.failures[workerData.record.fingerprint.id] = workerData.record;
writeFileSync(workerData.statePath, JSON.stringify(state));
release();
`;

describe("RavoRunService harness stores", () => {
	async function stores() {
		const root = await mkdtemp(path.join(tmpdir(), "ravo-run-stores-"));
		const localDir = path.join(root, "session", "harness");
		const globalDir = path.join(root, "agent", "harness");
		const deps: Partial<RavoRunServiceDeps> = {
			harnessDir: localDir,
			globalHarnessDir: globalDir,
			...ravoRunHarnessStores(localDir, globalDir),
		};
		return { localDir, globalDir, deps };
	}
	const claimAbc: Script = {
		implement: () => proposal(["abc123"]),
		judge: () => ({ ...judgeVerdict(80), addressedFingerprints: ["abc123"] }),
	};
	/** A seed champion that claimed abc123 and is still inside its observation window. */
	const provisionalSeed = (): RavoState<JsonValue> => {
		const ravo = seededRavo(60);
		return {
			...ravo,
			lineage: ravo.lineage.map((champion) => ({
				...champion,
				claimedFingerprints: ["abc123"],
				provisional: { committedTurn: 0, untilTurn: 100, clock: "ordinal" as const },
			})),
		};
	};
	/** Another writer's locked read-modify-write of a store, run while the judge is deciding. */
	const writeStoreDuringJudge = (
		dir: string,
		scope: HarnessScope,
		change: (stored: HarnessState) => void,
	): Script => ({
		implement: () => proposal(["abc123"]),
		judge: (call) => {
			if (call === 1) {
				withHarnessStateLock(dir, () => {
					const stored = loadHarnessState(dir, scope);
					change(stored);
					saveHarnessState(dir, stored);
				});
			}
			return { ...judgeVerdict(80), addressedFingerprints: ["abc123"] };
		},
	});

	it("commits a global run into the global store and leaves the session store alone", async () => {
		const { localDir, globalDir, deps } = await stores();
		saveHarnessState(globalDir, harnessState({ failures: failures("abc123") }));
		saveHarnessState(localDir, harnessState({ failures: failures("local9") }));
		const entries: LogEntry[] = [];
		setLogSink((entry) => entries.push(entry));
		let terminal: RavoRunStatus;
		try {
			const { service } = await harness(claimAbc, harnessState(), deps);
			terminal = await service.start({ ...request, global: true });
		} finally {
			setLogSink(undefined);
		}
		expect(terminal.stopReason).toBe("accepted");
		// The run charged the global ledger, not the session's.
		expect(terminal.lastCertificate?.missed).toEqual([]);
		const global = loadHarnessState(globalDir, "global");
		expect(Object.values(global.entries.memory)).toEqual([
			expect.objectContaining({ title: "Deploy barrier", scope: "global" }),
		]);
		expect(global.refinements.at(-1)).toMatchObject({ id: terminal.candidateId, reason: "ravo_run" });
		expect(global.ravo?.lineage.at(-1)).toMatchObject({
			proposalId: terminal.candidateId,
			claimedFingerprints: ["abc123"],
		});
		expect(global.failures?.failures.abc123).toBeDefined();
		const local = loadHarnessState(localDir, "local");
		expect(local.entries.memory).toEqual({});
		expect(local.refinements).toEqual([]);
		expect(local.ravo).toBeUndefined();
		expect(entries.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT)).toEqual([
			expect.objectContaining({ msg: REFINEMENT_COMMITTED_MSG, addressed: ["abc123"], scope: "global" }),
		]);
		expect(existsSync(path.join(globalDir, "ravo", "archive", "events.jsonl"))).toBe(true);
	});

	it("commits a local run into the session store and leaves the global store alone", async () => {
		const { localDir, globalDir, deps } = await stores();
		saveHarnessState(globalDir, harnessState({ failures: failures("global9") }));
		saveHarnessState(localDir, harnessState({ failures: failures("abc123") }));
		const { service } = await harness(claimAbc, harnessState(), deps);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate?.missed).toEqual([]);
		expect(loadHarnessState(localDir, "local").entries.memory.deploy_barrier).toMatchObject({ scope: "local" });
		const global = loadHarnessState(globalDir, "global");
		expect(global.entries.memory).toEqual({});
		expect(global.refinements).toEqual([]);
		expect(global.ravo).toBeUndefined();
	});

	it.each([
		{ held: "briefly", marginMs: 150 },
		{ held: "past the synchronous lock budget", marginMs: 1_500 },
	])(
		"keeps a ledger flush that holds the global store lock $held while the commit waits for it",
		async ({ marginMs }) => {
			const { globalDir, deps } = await stores();
			saveHarnessState(globalDir, harnessState({ failures: failures("abc123") }));
			const commitGate = new Int32Array(new SharedArrayBuffer(4));
			let flusher: Worker | undefined;
			let flushed: Promise<unknown[]> | undefined;
			const { service } = await harness(
				{
					implement: () => proposal(["abc123"]),
					// The flush takes the lock after the run read the store, and holds it into the commit gate.
					judge: async () => {
						flusher = new Worker(LEDGER_FLUSH_WORKER, {
							eval: true,
							workerData: {
								lockfile: createRequire(import.meta.url).resolve("proper-lockfile"),
								statePath: getHarnessStatePath(globalDir),
								record: failures("flushed1").failures.flushed1,
								commitGate,
								marginMs,
							},
						});
						flushed = once(flusher, "exit");
						await once(flusher, "message");
						return { ...judgeVerdict(80), addressedFingerprints: ["abc123"] };
					},
				},
				harnessState(),
				{
					...deps,
					onUpdate: (status) => {
						if (status.phase !== "commit_gate") return;
						Atomics.store(commitGate, 0, 1);
						Atomics.notify(commitGate, 0);
					},
				},
			);
			let terminal: RavoRunStatus;
			try {
				terminal = await service.start({ ...request, global: true });
				await flushed;
			} finally {
				await flusher?.terminate();
			}
			expect(terminal.stopReason).toBe("accepted");
			const global = loadHarnessState(globalDir, "global");
			expect(Object.keys(global.failures?.failures ?? {}).sort()).toEqual(["abc123", "flushed1"]);
			expect(global.entries.memory.deploy_barrier).toMatchObject({ scope: "global" });
			expect(global.refinements.map((event) => event.id)).toEqual([terminal.candidateId]);
			expect(global.ravo?.lineage.at(-1)?.proposalId).toBe(terminal.candidateId);
		},
	);

	it.each(["global", "local"] as const)(
		"keeps a regression recorded on the %s lineage during the run",
		async (scope) => {
			const { localDir, globalDir, deps } = await stores();
			const dir = scope === "global" ? globalDir : localDir;
			saveHarnessState(dir, harnessState({ failures: failures("abc123"), ravo: provisionalSeed() }));
			const regression = { championId: "seed", fingerprints: ["abc123"], committedTurn: 0, untilTurn: 100 };
			const { service } = await harness(
				writeStoreDuringJudge(dir, scope, (stored) => {
					stored.ravo = recordProvisionalRegressions(stored.ravo ?? provisionalSeed(), [regression], 5);
				}),
				harnessState(),
				deps,
			);
			const terminal = await service.start({ ...request, global: scope === "global" });
			expect(terminal.stopReason).toBe("accepted");
			const stored = loadHarnessState(dir, scope);
			expect(stored.ravo?.lineage.map((champion) => champion.proposalId)).toEqual(["seed", terminal.candidateId]);
			expect(stored.ravo?.lineage[0]?.provisional?.observedRecurrence).toEqual({
				turn: 5,
				fingerprints: ["abc123"],
			});
			expect(stored.entries.memory.deploy_barrier).toMatchObject({ scope });
		},
	);

	it("stops as stale instead of overwriting a champion another refinement committed during the run", async () => {
		const { globalDir, deps } = await stores();
		saveHarnessState(globalDir, harnessState({ failures: failures("abc123"), ravo: seededRavo(60) }));
		const entries: LogEntry[] = [];
		setLogSink((entry) => entries.push(entry));
		let terminal: RavoRunStatus;
		let repairs: number;
		try {
			const { service, calls } = await harness(
				writeStoreDuringJudge(globalDir, "global", (stored) => {
					const ravo = stored.ravo ?? seededRavo(60);
					stored.ravo = {
						...ravo,
						lineage: [
							...ravo.lineage,
							{ proposalId: "other", parentId: "seed", score: 95, artifact: null, missedCriterionIds: [] },
						],
						championId: "other",
						evaluatedProposalIds: [...ravo.evaluatedProposalIds, "other"],
					};
				}),
				harnessState(),
				deps,
			);
			terminal = await service.start({ ...request, global: true });
			repairs = calls.repair;
		} finally {
			setLogSink(undefined);
		}
		expect(terminal.stopReason).toBe("stale_cas");
		expect(repairs).toBe(0);
		const global = loadHarnessState(globalDir, "global");
		expect(global.ravo?.championId).toBe("other");
		expect(global.ravo?.lineage.map((champion) => champion.proposalId)).toEqual(["seed", "other"]);
		expect(global.entries.memory).toEqual({});
		expect(global.refinements).toEqual([]);
		expect(entries.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT)).toEqual([
			expect.objectContaining({
				msg: REFINEMENT_REJECTED_MSG,
				proposalId: terminal.candidateId,
				decision: "reject_deep",
				cause: "baseline_changed",
				claimed: 1,
				reason: "ravo_run",
				scope: "global",
			}),
		]);
	});
});

const ARC_AGENT_SOURCE = [
	"from arcengine import FrameData, GameAction, GameState",
	"from ..agent import Agent",
	"class RavoTestAgent(Agent):",
	"    MAX_ACTIONS = 8",
	"    def is_done(self, frames, latest_frame):",
	"        return latest_frame.state is GameState.WIN",
	"    def choose_action(self, frames, latest_frame):",
	"        return GameAction.RESET",
].join("\n");

function arcScorecard(levelsCompleted: number, totalLevels: number, actions: number): string {
	return [
		"2026-09-08 13:46:02,022 | INFO | --- FINAL SCORECARD REPORT ---",
		"2026-09-08 13:46:02,022 | INFO | {",
		`  "environments": [{"game_id": "ls20", "levels_completed": ${levelsCompleted}, "number_of_levels": ${totalLevels}, "total_actions": ${actions}, "state": "GAME_OVER"}],`,
		`  "total_levels_completed": ${levelsCompleted},`,
		`  "total_levels": ${totalLevels},`,
		`  "total_actions": ${actions}`,
		"}",
	].join("\n");
}

const arcProposal = (source = ARC_AGENT_SOURCE, agentName = "ravo_test_agent") => ({
	summary: "Candidate ARC agent",
	rationale: "Play the game.",
	expectedOutcome: "More levels.",
	addressedFingerprints: [],
	edits: [],
	arcAgent: { agentName, source },
});

function fakeArcRunner(outcomes: Array<{ levels: number; total: number; actions: number; traceback?: string }>) {
	const runs: string[] = [];
	const runner: ArcRunner = async ({ args, cwd }) => {
		runs.push(`${cwd} ${args.join(" ")}`);
		const outcome = outcomes[Math.min(runs.length - 1, outcomes.length - 1)];
		const stdout = arcScorecard(outcome.levels, outcome.total, outcome.actions);
		return outcome.traceback
			? { exitCode: 0, stdout, stderr: `Traceback (most recent call last):\n  File "x"\n${outcome.traceback}\n` }
			: { exitCode: 0, stdout, stderr: "" };
	};
	return { runner, runs };
}

describe("RavoRunService with the ARC-AGI-3 outcome evaluator", () => {
	const arcRequest = (repoDir: string): RavoRunRequest => ({
		task: "play ls20",
		maxRounds: 3,
		maxRepairs: 2,
		evaluator: { kind: "arc-agi", repoDir, game: "ls20" },
	});

	it("plays one game per candidate, scores by levels, and never consults the judge", async () => {
		const repoDir = await mkdtemp(path.join(tmpdir(), "arc-repo-"));
		await mkdir(path.join(repoDir, "agents", "templates"), { recursive: true });
		await writeFile(path.join(repoDir, "agents", "__init__.py"), "AVAILABLE_AGENTS = {}\n", "utf8");
		const arc = fakeArcRunner([{ levels: 3, total: 7, actions: 40 }]);
		const { service, calls, state, harnessDir } = await harness({ implement: () => arcProposal() }, harnessState(), {
			arcRunner: arc.runner,
		});
		const terminal = await service.start(arcRequest(repoDir));
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate).toMatchObject({
			status: "commit",
			screenScore: 100,
			deepScore: 43,
			missed: ["arc:all-levels"],
		});
		expect(calls.judge).toBe(0);
		expect(arc.runs).toEqual([`${repoDir} run main.py --agent=ravo_test_agent --game=ls20`]);
		expect(readFileSync(path.join(repoDir, "agents", "templates", "ravo_test_agent.py"), "utf8")).toContain(
			"class RavoTestAgent(Agent)",
		);
		const persisted = state();
		expect(persisted.ravo?.lineage.at(-1)).toMatchObject({ score: 43, missedCriterionIds: ["arc:all-levels"] });
		expect(persisted.ravo?.opponents.criteria.find((c) => c.id === "arc:all-levels")?.currentWeight).toBe(2);
		expect(persisted.ravo?.opponents.criteria.find((c) => c.id === "arc:no-crash")?.currentWeight).toBe(1);
		expect(existsSync(path.join(harnessDir, "ravo", "arc", `${terminal.runId}-ravo_test_agent.py`))).toBe(true);
	});

	it("rejects a syntactically broken agent at the fast screen without playing", async () => {
		const repoDir = await mkdtemp(path.join(tmpdir(), "arc-repo-"));
		const arc = fakeArcRunner([{ levels: 7, total: 7, actions: 10 }]);
		const { service } = await harness(
			{
				implement: () => arcProposal("class RavoTestAgent(Agent):\n  def broken(:\n"),
				repair: () => arcProposal("class RavoTestAgent(Agent):\n  def broken(:\n"),
			},
			harnessState(),
			{ arcRunner: arc.runner },
		);
		const terminal = await service.start({ ...arcRequest(repoDir), maxRepairs: 1 });
		expect(terminal.stopReason).toBe("repair_limit");
		expect(terminal.lastCertificate?.status).toBe("reject_screen");
		expect(arc.runs).toEqual([]);
	});

	it("treats a crashing agent as a deep failure and repairs it", async () => {
		const repoDir = await mkdtemp(path.join(tmpdir(), "arc-repo-"));
		await mkdir(path.join(repoDir, "agents", "templates"), { recursive: true });
		await writeFile(path.join(repoDir, "agents", "__init__.py"), "AVAILABLE_AGENTS = {}\n", "utf8");
		const arc = fakeArcRunner([
			{ levels: 0, total: 7, actions: 0, traceback: "KeyError: 'frame'" },
			{ levels: 7, total: 7, actions: 30 },
		]);
		const { service, calls } = await harness(
			{ implement: () => arcProposal(), repair: () => arcProposal(ARC_AGENT_SOURCE, "ravo_test_agent_v2") },
			harnessState(),
			{ arcRunner: arc.runner },
		);
		const terminal = await service.start(arcRequest(repoDir));
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.repairs).toBe(1);
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", deepScore: 100, missed: [] });
		expect(calls.repair).toBe(1);
		expect(arc.runs).toHaveLength(2);
	});

	it("never logs refinement.committed for an outcome-evaluator commit", async () => {
		const repoDir = await mkdtemp(path.join(tmpdir(), "arc-repo-"));
		await mkdir(path.join(repoDir, "agents", "templates"), { recursive: true });
		await writeFile(path.join(repoDir, "agents", "__init__.py"), "AVAILABLE_AGENTS = {}\n", "utf8");
		const arc = fakeArcRunner([{ levels: 3, total: 7, actions: 40 }]);
		const entries: LogEntry[] = [];
		setLogSink((entry) => entries.push(entry));
		try {
			const { service, calls } = await harness(
				{ implement: () => ({ ...arcProposal(), addressedFingerprints: ["abc123"] }) },
				harnessState({ failures: failures("abc123") }),
				{ arcRunner: arc.runner },
			);
			const terminal = await service.start(arcRequest(repoDir));
			expect(terminal.stopReason).toBe("accepted");
			expect(calls.judge).toBe(0);
		} finally {
			setLogSink(undefined);
		}
		expect(entries.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT)).toEqual([
			expect.objectContaining({ msg: REFINEMENT_APPLIED_UNMEASURED_MSG, reason: "ravo_run" }),
		]);
	});
});
