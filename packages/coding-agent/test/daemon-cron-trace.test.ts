import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	currentTraceContext,
	installDefaultSpanSink,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
	setSpanSink,
	type TraceContext,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CreateAgentSessionRuntimeFactory } from "../src/core/agent-session-runtime.js";
import type { AgentCronJob, AgentCronJobStore } from "../src/core/cron-jobs.js";
import { SessionManager } from "../src/core/session-manager.js";
import type { ActiveSessionState } from "../src/modes/daemon/active-session-state.js";
import { AgentDaemon } from "../src/modes/daemon/daemon-mode.js";
import type { DaemonCommand } from "../src/modes/daemon/daemon-protocol.js";
import { DAEMON_WORKER_SUPERVISOR_SOCKET_ENV } from "../src/modes/daemon/daemon-worker-protocol.js";

const ended: SpanEndRecord[] = [];
beforeEach(() => {
	ended.length = 0;
	setSpanSink((record) => ended.push(record));
});
afterEach(() => installDefaultSpanSink());

interface CronDaemonInternals {
	cronStore: AgentCronJobStore;
	cronScheduler: { runDue(now: Date): Promise<number> };
	createRuntime(command: Extract<DaemonCommand, { type: "create" }>): Promise<ActiveSessionState>;
}

interface SessionActivityOverrides {
	isStreaming?: boolean;
	isCompacting?: boolean;
	isRetrying?: boolean;
	isBashRunning?: boolean;
	hasPendingSessionWork?: boolean;
	unfinishedActionCount?: number;
}

interface CronFixture {
	tempDir: string;
	sessionFile: string;
	internals: CronDaemonInternals;
	state: ActiveSessionState;
	promptUntilAccepted: ReturnType<typeof vi.fn>;
	promptHeartbeat: ReturnType<typeof vi.fn>;
	promptContexts: Array<TraceContext | undefined>;
}

async function createCronFixture(activity: SessionActivityOverrides = {}): Promise<CronFixture> {
	const tempDir = mkdtempSync(join(tmpdir(), "prime-agent-daemon-cron-trace-"));
	const sessionDir = join(tempDir, "sessions");
	const manager = SessionManager.create(tempDir, sessionDir);
	manager.newSession();
	const sessionFile = manager.getSessionFile();
	if (!sessionFile) throw new Error("Missing session file");
	const promptContexts: Array<TraceContext | undefined> = [];
	const promptUntilAccepted = vi.fn(async (_prompt: string, options?: { admissionCommitted?: () => void }) => {
		promptContexts.push(currentTraceContext());
		options?.admissionCommitted?.();
	});
	const promptHeartbeat = vi.fn(async (_job: AgentCronJob, options?: { admissionCommitted?: () => void }) => {
		promptContexts.push(currentTraceContext());
		options?.admissionCommitted?.();
	});
	const createRuntime = vi.fn(async (options: Parameters<CreateAgentSessionRuntimeFactory>[0]) => {
		const session = makeRuntimeSession(options.sessionManager);
		Object.assign(session, {
			isStreaming: false,
			isCompacting: false,
			isRetrying: false,
			isBashRunning: false,
			hasPendingSessionWork: false,
			unfinishedActionCount: 0,
			hasAcceptedPromptInFlight: false,
			sessionActions: { queuedCount: 0, steering: [], followUps: [] },
			promptUntilAccepted,
			promptHeartbeat,
			...activity,
		});
		return {
			session,
			extensionsResult: { extensions: [], errors: [], runtime: {} } as unknown as Awaited<
				ReturnType<CreateAgentSessionRuntimeFactory>
			>["extensionsResult"],
			services: { cwd: options.cwd, agentDir: options.agentDir } as Awaited<
				ReturnType<CreateAgentSessionRuntimeFactory>
			>["services"],
			diagnostics: [],
		};
	});
	const daemon = new AgentDaemon(join(tempDir, "daemon.sock"), {
		defaultSessionConfig: { agentDir: tempDir, cwd: tempDir, sessionDir },
		createRuntime,
	});
	const internals = daemon as unknown as CronDaemonInternals;
	const state = await internals.createRuntime({ type: "create", sessionPath: sessionFile });
	ended.length = 0;
	return { tempDir, sessionFile, internals, state, promptUntilAccepted, promptHeartbeat, promptContexts };
}

function cronSpans(): SpanEndRecord[] {
	return ended.filter((record) => record.name === "cron.job");
}

describe("cron job trace spans", () => {
	it("wraps a scheduled prompt in a cron.job span that parents the prompt dispatch", async () => {
		const fixture = await createCronFixture();
		try {
			const { internals, state, sessionFile, tempDir } = fixture;
			const job = internals.cronStore.create({
				activeSessionId: state.activeSessionId,
				sessionId: state.runtime.session.sessionId,
				sessionFile,
				cwd: tempDir,
				label: "nightly report",
				scheduleText: "every 5m",
				prompt: "scheduled work",
			});

			expect(await internals.cronScheduler.runDue(new Date(Date.now() + 10 * 60 * 1000))).toBe(1);
			expect(fixture.promptUntilAccepted).toHaveBeenCalledOnce();

			const spans = cronSpans();
			expect(spans).toHaveLength(1);
			const span = spans[0]!;
			expect(span).toMatchObject({
				status: "ok",
				attrs: {
					"cron.job_id": job.id,
					"cron.name": "nightly report",
					"cron.kind": "cron",
					"cron.session_id": state.activeSessionId,
					"cron.delivery": "prompt",
					"cron.result": "ran",
				},
			});
			expect(span.attrs).not.toHaveProperty("cron.deferred");
			expect(span.error).toBeUndefined();
			// The prompt path ran under the cron.job span, so agent.prompt nests under it.
			expect(fixture.promptContexts).toHaveLength(1);
			expect(fixture.promptContexts[0]).toMatchObject({ traceId: span.traceId, spanId: span.spanId });
		} finally {
			rmSync(fixture.tempDir, { recursive: true, force: true });
		}
	});

	it("wraps a delivered heartbeat in a cron.job span with heartbeat attributes", async () => {
		const fixture = await createCronFixture();
		try {
			const { internals, state, sessionFile, tempDir } = fixture;
			const heartbeat = internals.cronStore.createRlmHeartbeat({
				activeSessionId: state.activeSessionId,
				sessionId: state.runtime.session.sessionId,
				sessionFile,
				cwd: tempDir,
				runtimeKind: "top-level",
				scheduleText: "every 5m",
				prompt: "report status",
			});

			expect(await internals.cronScheduler.runDue(new Date(Date.now() + 10 * 60 * 1000))).toBe(1);
			expect(fixture.promptHeartbeat).toHaveBeenCalledOnce();

			const spans = cronSpans();
			expect(spans).toHaveLength(1);
			expect(spans[0]).toMatchObject({
				status: "ok",
				attrs: {
					"cron.job_id": heartbeat.id,
					"cron.kind": "rlm_heartbeat",
					"cron.runtime_kind": "top-level",
					"cron.session_id": state.activeSessionId,
					"cron.delivery": "heartbeat",
					"cron.result": "ran",
				},
			});
			expect(fixture.promptContexts[0]).toMatchObject({ traceId: spans[0]!.traceId, spanId: spans[0]!.spanId });
		} finally {
			rmSync(fixture.tempDir, { recursive: true, force: true });
		}
	});

	it("ends a deferred heartbeat ok with cron.deferred=true and no delivery", async () => {
		const fixture = await createCronFixture({ isBashRunning: true });
		try {
			const { internals, state, sessionFile, tempDir } = fixture;
			const heartbeat = internals.cronStore.createRlmHeartbeat({
				activeSessionId: state.activeSessionId,
				sessionId: state.runtime.session.sessionId,
				sessionFile,
				cwd: tempDir,
				runtimeKind: "top-level",
				scheduleText: "every 5m",
				prompt: "report status",
			});

			expect(await internals.cronScheduler.runDue(new Date(Date.now() + 10 * 60 * 1000))).toBe(0);
			expect(fixture.promptHeartbeat).not.toHaveBeenCalled();
			expect(fixture.promptUntilAccepted).not.toHaveBeenCalled();

			const spans = cronSpans();
			expect(spans).toHaveLength(1);
			const span = spans[0]!;
			expect(span).toMatchObject({
				status: "ok",
				attrs: {
					"cron.job_id": heartbeat.id,
					"cron.kind": "rlm_heartbeat",
					"cron.session_id": state.activeSessionId,
					"cron.deferred": true,
					"cron.result": "skipped",
				},
			});
			expect(span.attrs).not.toHaveProperty("cron.delivery");
			expect(span.error).toBeUndefined();
		} finally {
			rmSync(fixture.tempDir, { recursive: true, force: true });
		}
	});

	it("records a failed dispatch on the cron.job span without swallowing the error", async () => {
		const fixture = await createCronFixture();
		try {
			const { internals, state, sessionFile, tempDir } = fixture;
			fixture.promptUntilAccepted.mockImplementationOnce(async () => {
				throw new Error("prompt rejected");
			});
			const job = internals.cronStore.create({
				activeSessionId: state.activeSessionId,
				sessionId: state.runtime.session.sessionId,
				sessionFile,
				cwd: tempDir,
				scheduleText: "every 5m",
				prompt: "scheduled work",
			});

			// The scheduler reports the failure through its onError hook; runDue does not throw.
			await internals.cronScheduler.runDue(new Date(Date.now() + 10 * 60 * 1000));
			expect(fixture.promptUntilAccepted).toHaveBeenCalledOnce();

			const spans = cronSpans();
			expect(spans).toHaveLength(1);
			expect(spans[0]).toMatchObject({
				status: "error",
				error: "prompt rejected",
				attrs: { "cron.job_id": job.id, "cron.kind": "cron", "cron.delivery": "prompt" },
			});
			expect(spans[0]!.attrs).not.toHaveProperty("cron.result");
		} finally {
			rmSync(fixture.tempDir, { recursive: true, force: true });
		}
	});
});

describe("worker supervisor peer listing diagnostics", () => {
	it("warns once per distinct failure and still returns no peers", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "prime-agent-peer-warning-"));
		const previousSupervisorSocket = process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV];
		const entries: LogEntry[] = [];
		setLogSink((entry) => entries.push(entry));
		try {
			process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV] = join(tempDir, "missing.sock");
			const daemon = new AgentDaemon(join(tempDir, "worker.sock"), {
				defaultSessionConfig: { agentDir: tempDir, cwd: tempDir },
				createRuntime: vi.fn(),
				worker: { authenticationToken: "worker-token" },
			});
			const listSupervisorAgentPeers = (
				daemon as unknown as { listSupervisorAgentPeers(): Promise<unknown[]> }
			).listSupervisorAgentPeers.bind(daemon);

			await expect(listSupervisorAgentPeers()).resolves.toEqual([]);
			await expect(listSupervisorAgentPeers()).resolves.toEqual([]);

			const warnings = entries.filter(
				(entry) => entry.component === "coding-agent.daemon" && /list_agent_peers/.test(entry.msg),
			);
			expect(warnings).toHaveLength(1);
			expect(warnings[0]).toMatchObject({ level: "warn", supervisorSocketPath: join(tempDir, "missing.sock") });
			expect(typeof warnings[0]?.error).toBe("string");
			expect((warnings[0]?.error as string).length).toBeGreaterThan(0);
		} finally {
			setLogSink(undefined);
			if (previousSupervisorSocket === undefined) delete process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV];
			else process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV] = previousSupervisorSocket;
			rmSync(tempDir, { recursive: true, force: true });
		}
	});
});

function makeRuntimeSession(
	sessionManager: Parameters<CreateAgentSessionRuntimeFactory>[0]["sessionManager"],
): Awaited<ReturnType<CreateAgentSessionRuntimeFactory>>["session"] {
	return {
		sessionManager,
		messages: [],
		extensionRunner: {
			hasHandlers: vi.fn(() => false),
			emit: vi.fn(async () => {}),
		},
		sessionFile: sessionManager.getSessionFile(),
		sessionId: sessionManager.getSessionId(),
		get sessionName() {
			return sessionManager.getSessionName();
		},
		setSubagentRuntimeHost: vi.fn(),
		getRlmChildRunStatus: vi.fn(() => "running"),
		getRlmChildSnapshots: vi.fn(() => []),
		registerRlmChildSession: vi.fn(() => true),
		releaseRlmChildSession: vi.fn(() => vi.fn()),
		subscribe: vi.fn(() => vi.fn()),
		bindExtensions: vi.fn(async () => {}),
		setExecEnvProvider: vi.fn(),
		getAvailableThinkingLevels: vi.fn(() => []),
		scopedModels: [],
		getActiveToolNames: vi.fn(() => []),
		getContextUsage: vi.fn(() => undefined),
		setSessionName: vi.fn((name: string) => sessionManager.appendSessionInfo(name)),
		dispose: vi.fn(),
		disposeAsync: vi.fn(async () => {}),
		abort: vi.fn(async () => {}),
	} as unknown as Awaited<ReturnType<CreateAgentSessionRuntimeFactory>>["session"];
}
