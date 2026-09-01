import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { SessionSummary } from "../src/modes/daemon/daemon-session-list.js";
import { DaemonSupervisor } from "../src/modes/daemon/daemon-supervisor.js";

interface WorkerFixture {
	descriptor: {
		workerId: string;
		lifecycle: "starting" | "ready" | "recovering" | "failed";
		rootActiveSessionId: string;
		rootSessionId: string;
		pid: number;
		ownerClientId?: string;
		createCommand: { type: "create" };
	};
	client?: {
		request: ReturnType<typeof vi.fn>;
		requestWorker: ReturnType<typeof vi.fn>;
		close: ReturnType<typeof vi.fn>;
	};
	summaries: Map<string, SessionSummary>;
	intentionalStop: boolean;
	recovery?: Promise<void>;
	launchEnv?: Record<string, string>;
}

interface SupervisorInternals {
	workers: Map<string, WorkerFixture>;
	log: ReturnType<typeof vi.fn>;
	persistWorker: ReturnType<typeof vi.fn>;
	isWorkerRecoveryCancelled(worker: WorkerFixture): boolean;
	recoverWorker(worker: WorkerFixture): Promise<void>;
}

const tempDirs: string[] = [];

afterEach(() => {
	for (const directory of tempDirs.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function makeSupervisor(): SupervisorInternals {
	const directory = mkdtempSync(join(tmpdir(), "prime-supervisor-recovery-"));
	tempDirs.push(directory);
	mkdirSync(directory, { recursive: true });
	writeFileSync(join(directory, "settings.json"), JSON.stringify({}));
	const supervisor = new DaemonSupervisor(join(directory, "daemon.sock"), {
		defaultSessionConfig: { agentDir: directory, cwd: directory },
		descriptorDir: join(directory, "workers"),
	}) as unknown as SupervisorInternals;
	supervisor.log = vi.fn();
	supervisor.persistWorker = vi.fn();
	return supervisor;
}

function makeWorker(id: string): WorkerFixture {
	return {
		descriptor: {
			workerId: id,
			lifecycle: "recovering",
			rootActiveSessionId: `${id}-root`,
			rootSessionId: `${id}-root-session`,
			pid: process.pid,
			createCommand: { type: "create" },
		},
		client: { request: vi.fn(), requestWorker: vi.fn(), close: vi.fn() },
		summaries: new Map(),
		intentionalStop: false,
	};
}

describe("daemon supervisor worker recovery", () => {
	it("settles instead of rejecting when detached recovery work throws", async () => {
		const supervisor = makeSupervisor();
		const worker = makeWorker("busy");
		supervisor.workers.set(worker.descriptor.workerId, worker);

		// Recovery is started detached (`void this.recoverWorker(...)`), so a rejection
		// escaping its body reaches the supervisor's fatal unhandledRejection handler
		// and exits the process, taking every healthy session on the daemon with it.
		// The observed trigger was a worker too busy to answer `list` within its 5s budget.
		let calls = 0;
		supervisor.isWorkerRecoveryCancelled = () => {
			calls++;
			if (calls > 1) throw new Error("Timed out waiting for daemon worker response to list");
			return false;
		};

		await expect(supervisor.recoverWorker(worker)).resolves.toBeUndefined();
		expect(supervisor.log).toHaveBeenCalledWith(expect.stringContaining("recovery failed"));
	});

	it("clears the recovery slot so a later failure can retry", async () => {
		const supervisor = makeSupervisor();
		const worker = makeWorker("busy");
		supervisor.workers.set(worker.descriptor.workerId, worker);
		let calls = 0;
		supervisor.isWorkerRecoveryCancelled = () => {
			calls++;
			if (calls > 1) throw new Error("Timed out waiting for daemon worker response to list");
			return false;
		};

		await supervisor.recoverWorker(worker);

		expect(worker.recovery).toBeUndefined();
	});
});
