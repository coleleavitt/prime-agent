import { existsSync } from "node:fs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import type { Usage } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import type { FailureLedger } from "../src/core/ravo/failure-ledger.js";
import type { JsonValue, RavoState } from "../src/core/ravo/reducer.js";
import {
	type RavoRunRequest,
	RavoRunService,
	type RavoRunServiceDeps,
	type RavoRunStatus,
} from "../src/core/ravo/run-service.js";
import type { RetainedWorkerRequest, RetainedWorkerRuntime } from "../src/core/ravo/runtime-adapter.js";
import type { HarnessState } from "../src/core/refinement/refinement.js";
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
const judgeVerdict = (score: number, failedCriteria: string[] = []) => ({
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
	const saveState = vi.fn(async (next: HarnessState) => {
		state = structuredClone(next);
	});
	const fake = fakeRunAgent(script);
	const deps: RavoRunServiceDeps = {
		runAgent: fake.runAgent,
		harnessDir,
		loadState: async () => structuredClone(state),
		saveState,
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
