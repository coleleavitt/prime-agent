import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { DaemonSocketClient } from "../src/modes/daemon/active-session-state.js";
import type { DaemonCommand, DaemonOutbound, DaemonResponse } from "../src/modes/daemon/daemon-protocol.js";
import { DaemonSupervisor } from "../src/modes/daemon/daemon-supervisor.js";

interface SupervisorHarness {
	clients: Set<DaemonSocketClient>;
	handleCommand(client: DaemonSocketClient, command: DaemonCommand): Promise<DaemonResponse | undefined>;
	handleWorkerFrame(worker: unknown, frame: unknown): void;
	writeSerialized(client: DaemonSocketClient, line: string | Buffer, message?: DaemonOutbound): boolean;
	forgetDreamRunStatusFor(worker: unknown): void;
}

const tempDirs: string[] = [];

afterEach(() => {
	for (const directory of tempDirs.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function createSupervisorHarness(): SupervisorHarness {
	const directory = mkdtempSync(join(tmpdir(), "prime-supervisor-dream-"));
	tempDirs.push(directory);
	return new DaemonSupervisor(join(directory, "daemon.sock"), {
		defaultSessionConfig: { agentDir: directory, cwd: directory },
		descriptorDir: join(directory, "workers"),
	}) as unknown as SupervisorHarness;
}

function client(rosterSubscribed: boolean): DaemonSocketClient {
	return { rosterSubscribed, socket: { destroyed: false } } as unknown as DaemonSocketClient;
}

function dreamFrame(sessionId: string, phase: string) {
	return {
		header: { kind: "outbound", outboundType: "dream_run_update" },
		payload: Buffer.from(
			JSON.stringify({
				type: "dream_run_update",
				sessionId,
				status: {
					runId: "d1",
					phase,
					task: "circle-packing",
					iteration: 1,
					bestNodeScore: 0.25,
					startedAt: 1,
					updatedAt: 2,
				},
			}),
		),
	};
}

const flush = () => new Promise<void>((resolve) => setImmediate(() => setImmediate(resolve)));

describe("daemon supervisor dream_run_update relay", () => {
	it("relays live pushes only to roster subscribers", () => {
		const supervisor = createSupervisorHarness();
		const subscribed = client(true);
		const other = client(false);
		supervisor.clients.add(subscribed);
		supervisor.clients.add(other);
		const writes: Array<[DaemonSocketClient, string]> = [];
		supervisor.writeSerialized = vi.fn((target, line) => {
			writes.push([target, line.toString()]);
			return true;
		});

		supervisor.handleWorkerFrame({ descriptor: { lifecycle: "ready" } }, dreamFrame("s1", "dreaming"));

		expect(writes).toHaveLength(1);
		expect(writes[0]?.[0]).toBe(subscribed);
		expect(JSON.parse(writes[0]?.[1] ?? "")).toMatchObject({ type: "dream_run_update", sessionId: "s1" });
	});

	it("replays the latest status per session to a new roster subscriber and forgets it with the worker", async () => {
		const supervisor = createSupervisorHarness();
		const worker = { descriptor: { lifecycle: "ready" } };
		supervisor.handleWorkerFrame(worker, dreamFrame("s1", "rollout"));
		supervisor.handleWorkerFrame(worker, dreamFrame("s1", "redeploying"));
		supervisor.handleWorkerFrame({ descriptor: { lifecycle: "ready" } }, dreamFrame("s2", "dreaming"));

		const late = client(false);
		supervisor.clients.add(late);
		const writes: string[] = [];
		supervisor.writeSerialized = vi.fn((_target, line) => {
			writes.push(line.toString());
			return true;
		});
		const response = await supervisor.handleCommand(late, { id: "sub", type: "roster_subscribe" });
		expect(response).toMatchObject({ success: true });
		expect(writes).toHaveLength(0);
		await flush();
		const replayed = writes.map((line) => JSON.parse(line) as { sessionId: string; status: { phase: string } });
		expect(replayed.map((m) => [m.sessionId, m.status.phase])).toEqual([
			["s1", "redeploying"],
			["s2", "dreaming"],
		]);

		supervisor.forgetDreamRunStatusFor(worker);
		const again = client(false);
		supervisor.clients.add(again);
		writes.length = 0;
		await supervisor.handleCommand(again, { id: "sub-2", type: "roster_subscribe" });
		await flush();
		expect(writes.map((line) => (JSON.parse(line) as { sessionId: string }).sessionId)).toEqual(["s2"]);
	});
});
