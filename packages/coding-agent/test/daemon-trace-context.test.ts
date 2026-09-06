import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	type Api,
	currentTraceContext,
	installDefaultSpanSink,
	type Model,
	type SpanEndRecord,
	setSpanSink,
	type TraceContext,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CreateAgentSessionRuntimeFactory } from "../src/core/agent-session-runtime.js";
import type { CreateRlmSubagentRuntimeOptions } from "../src/core/rlm-runtime.js";
import { SessionManager } from "../src/core/session-manager.js";
import type { ActiveSessionState, DaemonSocketClient } from "../src/modes/daemon/active-session-state.js";
import { AgentDaemon } from "../src/modes/daemon/daemon-mode.js";
import {
	createDaemonCommandEnvelope,
	type DaemonCommand,
	isDaemonCommandEnvelope,
} from "../src/modes/daemon/daemon-protocol.js";
import { DaemonSupervisor } from "../src/modes/daemon/daemon-supervisor.js";
import { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";
import { type DaemonWorkerFrameHeader, isDaemonWorkerFrameHeader } from "../src/modes/daemon/daemon-worker-protocol.js";

const TRACEPARENT = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

const ended: SpanEndRecord[] = [];
beforeEach(() => {
	ended.length = 0;
	setSpanSink((record) => ended.push(record));
});
afterEach(() => installDefaultSpanSink());

describe("daemon worker frame header trace context", () => {
	const base = { kind: "command", requestId: "worker_1", commandType: "list" };

	it("accepts a command header with and without traceparent", () => {
		expect(isDaemonWorkerFrameHeader(base)).toBe(true);
		expect(isDaemonWorkerFrameHeader({ ...base, traceparent: TRACEPARENT })).toBe(true);
	});

	it("rejects a non-string traceparent", () => {
		expect(isDaemonWorkerFrameHeader({ ...base, traceparent: 42 })).toBe(false);
		expect(isDaemonWorkerFrameHeader({ ...base, traceparent: null })).toBe(false);
		expect(isDaemonWorkerFrameHeader({ ...base, traceparent: { traceId: "x" } })).toBe(false);
	});
});

describe("daemon command envelope trace context", () => {
	const command: DaemonCommand = { id: "daemon_1", type: "list" };

	it("omits traceparent outside a span and accepts the legacy shape", () => {
		const envelope = createDaemonCommandEnvelope(command, "daemon_1", "client-1");
		expect(envelope).not.toHaveProperty("traceparent");
		expect(isDaemonCommandEnvelope(envelope)).toBe(true);
	});

	it("carries the active span's traceparent", () => {
		let active: TraceContext | undefined;
		const envelope = withSpan("test.client", () => {
			active = currentTraceContext();
			return createDaemonCommandEnvelope(command, "daemon_1", "client-1");
		});
		expect(envelope.traceparent).toBe(`00-${active!.traceId}-${active!.spanId}-${active!.flags}`);
		expect(isDaemonCommandEnvelope(envelope)).toBe(true);
	});

	it("rejects a non-string traceparent", () => {
		const envelope = createDaemonCommandEnvelope(command, "daemon_1", "client-1");
		expect(isDaemonCommandEnvelope({ ...envelope, traceparent: 42 })).toBe(false);
	});
});

interface WorkerClientInternals {
	socket?: { destroyed: boolean; destroy: () => void };
	channel?: { send: (header: DaemonWorkerFrameHeader, payload: Buffer) => Promise<void>; close: () => void };
	request(command: { type: string }, timeoutMs: number): Promise<unknown>;
	close(): void;
}

function makeWorkerClient(): { client: WorkerClientInternals; headers: DaemonWorkerFrameHeader[] } {
	const headers: DaemonWorkerFrameHeader[] = [];
	const client = new DaemonWorkerClient("/tmp/prime-agent-test-never.sock") as unknown as WorkerClientInternals;
	client.socket = { destroyed: false, destroy: () => {} };
	client.channel = {
		send: async (header) => {
			headers.push(header);
		},
		close: () => {},
	};
	return { client, headers };
}

describe("daemon worker client trace context", () => {
	it("stamps the frame header with the active span's traceparent", async () => {
		const { client, headers } = makeWorkerClient();
		let active: TraceContext | undefined;
		const pending = withSpan("test.supervisor", () => {
			active = currentTraceContext();
			return client.request({ type: "list" }, 5000);
		});
		client.close();
		await expect(pending).rejects.toThrow("Daemon worker client closed");
		expect(headers).toHaveLength(1);
		expect(headers[0]).toEqual({
			kind: "command",
			requestId: "worker_1",
			commandType: "list",
			traceparent: `00-${active!.traceId}-${active!.spanId}-${active!.flags}`,
		});
	});

	it("omits traceparent from the frame header outside a span", async () => {
		const { client, headers } = makeWorkerClient();
		const pending = client.request({ type: "list" }, 5000);
		client.close();
		await expect(pending).rejects.toThrow("Daemon worker client closed");
		expect(headers).toEqual([{ kind: "command", requestId: "worker_1", commandType: "list" }]);
	});
});

interface DaemonInternals {
	handleCommandFrame(
		client: DaemonSocketClient,
		header: Extract<DaemonWorkerFrameHeader, { kind: "command" }>,
		line: string,
	): Promise<void>;
	handleLine(client: DaemonSocketClient, line: string): Promise<void>;
	handleClientLine(client: DaemonSocketClient, line: string): Promise<void>;
}

function makeSocketClient(): DaemonSocketClient {
	return {
		id: "client-1",
		socket: { write: vi.fn(() => true), end: vi.fn(), destroy: vi.fn(), destroyed: false },
		attachedActiveSessionIds: new Set(),
		catchupActiveSessionIds: new Set(),
		backpressured: false,
		authenticated: true,
		transport: "private-framed",
		detachInput: () => {},
		supportsExtensionUi: false,
		capabilities: new Set(),
	} as unknown as DaemonSocketClient;
}

describe("daemon worker command frames", () => {
	function makeDaemon(): { internals: DaemonInternals; seen: Array<TraceContext | undefined> } {
		const daemon = new AgentDaemon("/tmp/prime-agent-test-never.sock", {
			defaultSessionConfig: { agentDir: "/tmp", cwd: "/tmp" },
			createRuntime: vi.fn(),
			worker: { authenticationToken: "token", workerInstanceId: "worker-instance" },
		});
		const internals = daemon as unknown as DaemonInternals;
		const seen: Array<TraceContext | undefined> = [];
		// Replace the body so the test observes only the context handleLine runs under.
		internals.handleLine = vi.fn(async () => {
			seen.push(currentTraceContext());
		});
		return { internals, seen };
	}

	it("handles a frame inside a daemon.command span parented to the header traceparent", async () => {
		const { internals, seen } = makeDaemon();
		await internals.handleCommandFrame(
			makeSocketClient(),
			{ kind: "command", requestId: "worker_7", commandType: "list", traceparent: TRACEPARENT },
			JSON.stringify({ id: "worker_7", type: "list" }),
		);
		expect(seen).toHaveLength(1);
		expect(seen[0]).toMatchObject({
			traceId: "0af7651916cd43dd8448eb211c80319c",
			parentSpanId: "b7ad6b7169203331",
		});
		expect(ended).toHaveLength(1);
		expect(ended[0]).toMatchObject({
			name: "daemon.command",
			traceId: "0af7651916cd43dd8448eb211c80319c",
			spanId: seen[0]!.spanId,
			parentSpanId: "b7ad6b7169203331",
			status: "ok",
			attrs: { "daemon.request_id": "worker_7", "daemon.command_type": "list" },
		});
	});

	it("roots a new trace for a legacy frame without traceparent", async () => {
		const { internals, seen } = makeDaemon();
		await internals.handleCommandFrame(
			makeSocketClient(),
			{ kind: "command", requestId: "worker_8", commandType: "list" },
			JSON.stringify({ id: "worker_8", type: "list" }),
		);
		expect(seen[0]).toBeDefined();
		expect(seen[0]?.parentSpanId).toBeUndefined();
		expect(ended[0]).toMatchObject({ name: "daemon.command", spanId: seen[0]!.spanId, status: "ok" });
		expect(ended[0]?.parentSpanId).toBeUndefined();
	});

	it("ignores a malformed traceparent instead of failing the command", async () => {
		const { internals, seen } = makeDaemon();
		await internals.handleCommandFrame(
			makeSocketClient(),
			{ kind: "command", requestId: "worker_9", commandType: "list", traceparent: "garbage" },
			JSON.stringify({ id: "worker_9", type: "list" }),
		);
		expect(seen).toHaveLength(1);
		expect(seen[0]?.parentSpanId).toBeUndefined();
		expect(ended[0]).toMatchObject({ name: "daemon.command", status: "ok" });
	});

	it("records a failing handler on the span and rethrows", async () => {
		const { internals } = makeDaemon();
		internals.handleLine = vi.fn(async () => {
			throw new Error("boom");
		});
		await expect(
			internals.handleCommandFrame(
				makeSocketClient(),
				{ kind: "command", requestId: "worker_10", commandType: "list" },
				JSON.stringify({ id: "worker_10", type: "list" }),
			),
		).rejects.toThrow("boom");
		expect(ended[0]).toMatchObject({ name: "daemon.command", status: "error", error: "boom" });
	});
});

describe("rlm child admission trace context", () => {
	it("creates the child runtime inside an rlm.child span nested under the parent context", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "prime-agent-daemon-rlm-trace-"));
		try {
			const sessionDir = join(tempDir, "sessions");
			const parentManager = SessionManager.create(tempDir, sessionDir);
			parentManager.newSession();
			parentManager.appendSessionInfo("parent");
			const parentSessionFile = parentManager.getSessionFile();
			if (!parentSessionFile) throw new Error("Missing parent session file");
			const childSessionDir = join(parentManager.getSessionArtifactDir()!, "child-1");
			const runtimeContexts: Array<TraceContext | undefined> = [];
			const createRuntime = vi.fn(async (options: Parameters<CreateAgentSessionRuntimeFactory>[0]) => {
				runtimeContexts.push(currentTraceContext());
				return {
					session: makeRuntimeSession(options.sessionManager),
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
			const internals = daemon as unknown as {
				createRuntime(command: Extract<DaemonCommand, { type: "create" }>): Promise<ActiveSessionState>;
				createRlmSubagentRuntime(
					parentState: ActiveSessionState,
					options: CreateRlmSubagentRuntimeOptions,
				): Promise<ActiveSessionState["runtime"]>;
			};
			const parentState = await internals.createRuntime({ type: "create", sessionPath: parentSessionFile });
			runtimeContexts.length = 0;
			ended.length = 0;

			let parentContext: TraceContext | undefined;
			await withSpan("test.parent_turn", async () => {
				parentContext = currentTraceContext();
				await internals.createRlmSubagentRuntime(parentState, {
					parentSession: parentState.runtime.session,
					id: "child-1",
					prompt: "trace me",
					sessionName: "traced-worker",
					sessionDir: childSessionDir,
					model: { provider: "test", id: "model" } as Model<Api>,
					thinkingLevel: "off",
					serviceTier: null,
					scopedModels: [],
					activeToolNames: [],
					customTools: [],
					includeGoals: false,
					includeCompactSkill: false,
					rlmDepth: 1,
					rlmMaxDepth: 4,
					rlmParentNodeId: "child-1",
				});
			});

			const childSpan = ended.find((record) => record.name === "rlm.child");
			expect(childSpan).toMatchObject({
				traceId: parentContext!.traceId,
				parentSpanId: parentContext!.spanId,
				status: "ok",
				attrs: { "rlm.child_id": "child-1", "rlm.depth": 1 },
			});
			// The child runtime factory ran under the rlm.child span, so anything it
			// spawns or logs is attributed to the child rather than the parent turn.
			expect(runtimeContexts).toHaveLength(1);
			expect(runtimeContexts[0]).toMatchObject({ traceId: parentContext!.traceId, spanId: childSpan!.spanId });
		} finally {
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

describe("daemon client-socket command lines", () => {
	function makeDaemon(): { internals: DaemonInternals; seen: Array<TraceContext | undefined> } {
		const daemon = new AgentDaemon("/tmp/prime-agent-test-never.sock", {
			defaultSessionConfig: { agentDir: "/tmp", cwd: "/tmp" },
			createRuntime: vi.fn(),
		});
		const internals = daemon as unknown as DaemonInternals;
		const seen: Array<TraceContext | undefined> = [];
		internals.handleLine = vi.fn(async () => {
			seen.push(currentTraceContext());
		});
		return { internals, seen };
	}

	it("adopts the CLI envelope's traceparent so relayed worker commands continue the caller's trace", async () => {
		const { internals, seen } = makeDaemon();
		const envelope = {
			...createDaemonCommandEnvelope({ id: "daemon_3", type: "list" }, "daemon_3", "client-1"),
			traceparent: TRACEPARENT,
		};
		await internals.handleClientLine(makeSocketClient(), JSON.stringify(envelope));
		expect(seen[0]).toMatchObject({ traceId: "0af7651916cd43dd8448eb211c80319c", parentSpanId: "b7ad6b7169203331" });
		expect(ended[0]).toMatchObject({
			name: "daemon.command",
			parentSpanId: "b7ad6b7169203331",
			attrs: { "daemon.request_id": "daemon_3", "daemon.command_type": "list" },
		});
	});

	it("roots a fresh trace for a legacy line and still reaches handleLine for garbage", async () => {
		const { internals, seen } = makeDaemon();
		await internals.handleClientLine(makeSocketClient(), JSON.stringify({ id: "daemon_4", type: "list" }));
		await internals.handleClientLine(makeSocketClient(), "{not json");
		expect(seen).toHaveLength(2);
		expect(seen[0]?.parentSpanId).toBeUndefined();
		expect(ended.map((r) => r.attrs["daemon.command_type"])).toEqual(["list", "unknown"]);
	});
});

describe("daemon command context inheritance", () => {
	it("inherits the ambient (inbound TRACEPARENT) context when the line carries none", async () => {
		const daemon = new AgentDaemon("/tmp/prime-agent-test-never.sock", {
			defaultSessionConfig: { agentDir: "/tmp", cwd: "/tmp" },
			createRuntime: vi.fn(),
		});
		const internals = daemon as unknown as DaemonInternals;
		const seen: Array<TraceContext | undefined> = [];
		internals.handleLine = vi.fn(async () => {
			seen.push(currentTraceContext());
		});
		await withSpan("process.inbound", async (ambient) => {
			await internals.handleClientLine(makeSocketClient(), JSON.stringify({ id: "daemon_9", type: "list" }));
			expect(seen[0]).toMatchObject({ traceId: ambient.context.traceId, parentSpanId: ambient.context.spanId });
		});
	});
});

describe("daemon supervisor client-socket command lines", () => {
	interface SupervisorInternals {
		handleClientLine(client: DaemonSocketClient, line: string): Promise<void>;
		handleLine(client: DaemonSocketClient, line: string): Promise<void>;
	}
	// Constructor-bypass harness (see daemon-supervisor-admission.test.ts): only
	// the context handleLine runs under is observed.
	function makeSupervisor(): { internals: SupervisorInternals; seen: Array<TraceContext | undefined> } {
		const internals = Object.create(DaemonSupervisor.prototype) as SupervisorInternals;
		const seen: Array<TraceContext | undefined> = [];
		internals.handleLine = vi.fn(async () => {
			seen.push(currentTraceContext());
		});
		return { internals, seen };
	}

	it("adopts the CLI envelope's traceparent before relaying", async () => {
		const { internals, seen } = makeSupervisor();
		const envelope = {
			...createDaemonCommandEnvelope({ id: "daemon_5", type: "list" }, "daemon_5", "client-1"),
			traceparent: TRACEPARENT,
		};
		await internals.handleClientLine(makeSocketClient(), JSON.stringify(envelope));
		expect(seen[0]).toMatchObject({ traceId: "0af7651916cd43dd8448eb211c80319c", parentSpanId: "b7ad6b7169203331" });
		expect(ended[0]).toMatchObject({
			name: "daemon.command",
			attrs: { "daemon.request_id": "daemon_5", "daemon.command_type": "list" },
		});
	});

	it("inherits the ambient context when the envelope carries none", async () => {
		const { internals, seen } = makeSupervisor();
		await withSpan("process.inbound", async (ambient) => {
			await internals.handleClientLine(
				makeSocketClient(),
				JSON.stringify(createDaemonCommandEnvelope({ id: "daemon_6", type: "list" }, "daemon_6")),
			);
			expect(seen[0]).toMatchObject({ traceId: ambient.context.traceId, parentSpanId: ambient.context.spanId });
		});
	});
});

describe("daemon command failure envelopes", () => {
	it("marks the daemon.command span failed when the handler converts a throw into a failure envelope", async () => {
		const daemon = new AgentDaemon("/tmp/prime-agent-test-never.sock", {
			defaultSessionConfig: { agentDir: "/tmp", cwd: "/tmp" },
			createRuntime: vi.fn(),
		});
		const internals = daemon as unknown as DaemonInternals & {
			handleCommand: (...args: unknown[]) => Promise<unknown>;
		};
		internals.handleCommand = vi.fn(async () => {
			throw new Error("Worker authentication failed");
		});
		const client = makeSocketClient();
		await internals.handleClientLine(client, JSON.stringify({ id: "daemon_11", type: "list" }));
		const written = (client.socket.write as ReturnType<typeof vi.fn>).mock.calls.map((c) => String(c[0]));
		expect(written.some((line) => line.includes("Worker authentication failed"))).toBe(true);
		expect(ended[0]).toMatchObject({
			name: "daemon.command",
			status: "error",
			error: "Worker authentication failed",
			attrs: { "daemon.command_type": "list" },
		});
	});
});
