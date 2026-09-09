import { AsyncLocalStorage } from "node:async_hooks";
import { EventEmitter } from "node:events";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	installAsyncTraceContextStorage,
	installDefaultSpanSink,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
	setSpanSink,
	type TraceContext,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { KernelExitedError, ReplKernelManager } from "../src/core/kernel/index.js";

// Deterministic propagation across the awaits inside the manager (idempotent).
installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const CRASH_STDERR = "idalib: database open failed, calling exit(1)";
const BOOT_STDERR = "fake runtime died before ready";

/**
 * Fake runtime speaking protocol 3 whose `crash` cell writes its last words
 * to stderr and dies with a native-style exit(1), and whose `crash-signal`
 * cell is killed by SIGKILL. Journals spawns and restores like the protocol
 * corruption fake so the lazy reprovisioning path can be asserted.
 */
function writeFakeRuntime(path: string): void {
	writeFileSync(
		path,
		`#!/usr/bin/env node
const fs = require("node:fs");
const readline = require("node:readline");
const countPath = process.env.FAKE_REPL_SPAWN_COUNT;
const count = fs.existsSync(countPath) ? Number(fs.readFileSync(countPath, "utf8")) + 1 : 1;
fs.writeFileSync(countPath, String(count));
let state = {};
let boot = "missing";
const emit = (event) => process.stdout.write(JSON.stringify(event) + "\\n");
if (fs.existsSync(process.env.FAKE_REPL_DIE_ON_BOOT)) {
  process.stderr.write(${JSON.stringify(BOOT_STDERR)} + "\\n");
  process.exit(3);
}
emit({ event: "ready", protocol: 3, python: process.version });
const input = readline.createInterface({ input: process.stdin });
input.on("line", (line) => {
  const request = JSON.parse(line);
  if (request.type === "execute") {
    if (request.code === "crash") {
      emit({ event: "stdout", id: request.id, text: "about to crash" });
      process.stderr.write(${JSON.stringify(CRASH_STDERR)} + "\\n");
      process.exit(1);
    }
    if (request.code === "crash-signal") {
      process.kill(process.pid, "SIGKILL");
      return;
    }
    if (request.code === "bootstrap") boot = "live";
    if (request.code === "read-boot") emit({ event: "stdout", id: request.id, text: boot });
    if (request.code === "pid") emit({ event: "stdout", id: request.id, text: String(process.pid) });
    if (request.code === "seed") state.value = "persisted";
    if (request.code === "read") emit({ event: "stdout", id: request.id, text: state.value || "fresh" });
    emit({ event: "done", id: request.id, status: "ok" });
    return;
  }
  if (request.type === "snapshot") {
    fs.writeFileSync(request.path, JSON.stringify(state));
    fs.writeFileSync(request.manifest_path, "{}");
    emit({ event: "done", id: request.id, status: "ok", saved: Object.keys(state), skipped: [], bytes: 1 });
    return;
  }
  if (request.type === "restore") {
    fs.appendFileSync(process.env.FAKE_REPL_RESTORE_LOG, "r");
    state = fs.existsSync(request.path) ? JSON.parse(fs.readFileSync(request.path, "utf8")) : {};
    emit({ event: "done", id: request.id, status: "ok", restored: Object.keys(state), failed: [] });
    return;
  }
  if (request.type === "shutdown") {
    emit({ event: "done", id: request.id, status: "ok" });
    process.exit(0);
  }
});
`,
	);
	chmodSync(path, 0o755);
}

function spawnCount(path: string): number {
	return existsSync(path) ? Number(readFileSync(path, "utf8")) : 0;
}

function restoreCount(path: string): number {
	return existsSync(path) ? readFileSync(path, "utf8").length : 0;
}

type ManagerInternals = { state: string; pendingRestore: boolean; pendingRebootstrap: boolean };

function internals(manager: ReplKernelManager): ManagerInternals {
	return manager as unknown as ManagerInternals;
}

describe("ReplKernelManager unexpected kernel exit", () => {
	let tempDir = "";
	let manager: ReplKernelManager | undefined;
	const spans: SpanEndRecord[] = [];
	const entries: LogEntry[] = [];

	beforeEach(() => {
		tempDir = mkdtempSync(join(tmpdir(), "prime-agent-repl-exit-"));
		spans.length = 0;
		entries.length = 0;
		setSpanSink((record) => spans.push(record));
		setLogSink((entry) => entries.push(entry));
	});

	afterEach(async () => {
		await manager?.shutdown({ snapshot: true, drainHostRequests: true });
		manager = undefined;
		installDefaultSpanSink();
		setLogSink(undefined);
		if (tempDir) {
			rmSync(tempDir, { recursive: true, force: true });
			tempDir = "";
		}
	});

	function newManager(options: { snapshot?: boolean; bootstrapCode?: string } = {}): {
		manager: ReplKernelManager;
		countPath: string;
		dieOnBootPath: string;
		restoreLogPath: string;
		snapshotPath: string;
		stderrLogPath: string;
	} {
		const python = join(tempDir, "python");
		const countPath = join(tempDir, "spawn-count");
		const dieOnBootPath = join(tempDir, "die-on-boot");
		const restoreLogPath = join(tempDir, "restore-log");
		const snapshotPath = join(tempDir, "state.json");
		const stderrLogPath = join(tempDir, "kernel-stderr.log");
		writeFakeRuntime(python);
		manager = new ReplKernelManager({
			python,
			cwd: tempDir,
			env: {
				FAKE_REPL_DIE_ON_BOOT: dieOnBootPath,
				FAKE_REPL_RESTORE_LOG: restoreLogPath,
				FAKE_REPL_SPAWN_COUNT: countPath,
			},
			snapshot: options.snapshot
				? { path: snapshotPath, manifestPath: join(tempDir, "manifest.json"), debounceMs: 1 }
				: undefined,
			bootstrapCode: options.bootstrapCode,
			stderrLogPath,
		});
		return { manager, countPath, dieOnBootPath, restoreLogPath, snapshotPath, stderrLogPath };
	}

	async function crash(kernel: ReplKernelManager, code = "crash"): Promise<KernelExitedError> {
		const failed = kernel.execute(code);
		await expect(failed).rejects.toBeInstanceOf(KernelExitedError);
		const error = await failed.then(
			() => undefined,
			(rejection: unknown) => rejection,
		);
		if (!(error instanceof KernelExitedError)) throw new Error("expected KernelExitedError");
		return error;
	}

	function kernelExitEntries(): LogEntry[] {
		return entries.filter((entry) => entry.component === "kernel" && entry.msg === "kernel_exit");
	}

	it("fails the cell the kernel died in with KernelExitedError and settles for a respawn", async () => {
		const { manager: kernel, stderrLogPath } = newManager();
		expect((await kernel.execute("pid")).status).toBe("ok");
		// Date.now() has millisecond resolution; make the uptime unambiguous.
		await new Promise((r) => setTimeout(r, 5));

		const error = await crash(kernel);
		expect(error.exit).toMatchObject({ exitCode: 1, signal: null, requestType: "execute" });
		expect(error.exit.uptimeMs).toBeGreaterThan(0);
		expect(error.exit.requestId).toEqual(expect.any(String));
		expect(error.exit.at).toBeGreaterThan(0);
		expect(error.exit.stderrTail).toContain(CRASH_STDERR);
		expect(error.exit.stderrTail).toContain("unexpected exit code=1 signal=null");
		expect(error.message).toMatch(/exited unexpectedly \(exit code 1\)[^\n]*execute request/);
		expect(error.message).toContain("A fresh kernel starts on the next call");
		expect(error.message).toContain(CRASH_STDERR);

		expect(kernel.lastUnexpectedExit).toBe(error.exit);
		expect(kernel.isRunning).toBe(false);
		expect(internals(kernel).state).toBe("idle");
		expect(internals(kernel).pendingRestore).toBe(true);
		expect(internals(kernel).pendingRebootstrap).toBe(true);
		// The last words reached the on-disk stderr log too.
		expect(readFileSync(stderrLogPath, "utf8")).toContain(CRASH_STDERR);
	});

	it("reports a signal death as such", async () => {
		const { manager: kernel } = newManager();
		await kernel.start();
		const error = await crash(kernel, "crash-signal");
		expect(error.exit).toMatchObject({ exitCode: null, signal: "SIGKILL", requestType: "execute" });
		expect(error.message).toContain("killed by signal SIGKILL");
		expect(kernel.lastUnexpectedExit?.signal).toBe("SIGKILL");
	});

	it("spawns a fresh kernel on the next call and reprovisions it (restore, then bootstrap)", async () => {
		const {
			manager: kernel,
			countPath,
			restoreLogPath,
			snapshotPath,
		} = newManager({ snapshot: true, bootstrapCode: "bootstrap" });
		await kernel.execute("bootstrap");
		await kernel.execute("seed");
		await expect.poll(() => existsSync(snapshotPath)).toBe(true);
		const firstPid = (await kernel.execute("pid")).stdout;
		expect(spawnCount(countPath)).toBe(1);

		await crash(kernel);

		const secondPid = (await kernel.execute("pid")).stdout;
		expect(secondPid).not.toBe(firstPid);
		expect(kernel.isRunning).toBe(true);
		expect(spawnCount(countPath)).toBe(2);
		// Namespace revived from the last auto-snapshot, runtime bootstrap re-run.
		await expect(kernel.execute("read")).resolves.toMatchObject({ status: "ok", stdout: "persisted" });
		await expect(kernel.execute("read-boot")).resolves.toMatchObject({ status: "ok", stdout: "live" });
		expect(restoreCount(restoreLogPath)).toBe(1);
		expect(internals(kernel).pendingRestore).toBe(false);
		expect(internals(kernel).pendingRebootstrap).toBe(false);
		// The crash record stays readable after the respawn for the caller to report once.
		expect(kernel.lastUnexpectedExit?.exitCode).toBe(1);
	});

	it("runs a cell that was queued behind the crashing one on the replacement kernel", async () => {
		const { manager: kernel, countPath, snapshotPath } = newManager({ snapshot: true });
		await kernel.execute("seed");
		await expect.poll(() => existsSync(snapshotPath)).toBe(true);

		const crashing = kernel.execute("crash");
		const queued = kernel.execute("read");
		await expect(crashing).rejects.toBeInstanceOf(KernelExitedError);
		await expect(queued).resolves.toMatchObject({ status: "ok", stdout: "persisted" });
		expect(spawnCount(countPath)).toBe(2);
	});

	it("recovers from a crash of a kernel that had nothing to restore", async () => {
		const { manager: kernel, countPath } = newManager();
		await crash(kernel);
		await expect(kernel.execute("read")).resolves.toMatchObject({ status: "ok", stdout: "fresh" });
		expect(spawnCount(countPath)).toBe(2);
	});

	it("kill() and shutdown() owned teardowns stay shut down and record no unexpected exit", async () => {
		const killed = newManager().manager;
		await killed.start();
		await killed.kill();
		expect(internals(killed).state).toBe("shutdown");
		await expect(killed.execute("pid")).rejects.toThrow(/^Kernel has been shut down$/);
		expect(killed.lastUnexpectedExit).toBeUndefined();
		expect(internals(killed).state).toBe("shutdown");

		const { manager: graceful, countPath } = newManager();
		await graceful.start();
		await expect(graceful.shutdown({ snapshot: true, drainHostRequests: true })).resolves.toBe(true);
		expect(internals(graceful).state).toBe("shutdown");
		await expect(graceful.execute("pid")).rejects.toThrow(/^Kernel has been shut down$/);
		expect(graceful.lastUnexpectedExit).toBeUndefined();
		expect(kernelExitEntries()).toHaveLength(0);
		// One spawn per manager: neither teardown resurrected its kernel.
		expect(spawnCount(countPath)).toBe(2);
	});

	it("names the crash when a host teardown follows it before any respawn", async () => {
		const { manager: kernel } = newManager();
		await kernel.start();
		await crash(kernel);
		await kernel.kill();
		expect(internals(kernel).state).toBe("shutdown");
		await expect(kernel.execute("pid")).rejects.toThrow(
			/^Kernel has been shut down: Kernel process exited unexpectedly \(exit code 1\) while serving execute request [0-9a-f-]+$/,
		);
		expect(kernel.lastUnexpectedExit?.exitCode).toBe(1);
	});

	it("still rejects start() when the kernel exits before ready, and lets the next call retry", async () => {
		const { manager: kernel, countPath, dieOnBootPath } = newManager();
		writeFileSync(dieOnBootPath, "1");
		await expect(kernel.start()).rejects.toThrow(/Kernel exited before ready[\s\S]*fake runtime died before ready/);
		expect(kernel.lastUnexpectedExit).toBeUndefined();
		expect(kernel.isRunning).toBe(false);
		expect(kernelExitEntries()).toMatchObject([{ exitCode: 3, signal: null, uptimeMs: null }]);
		const startSpan = spans.find((s) => s.name === "kernel.start");
		expect(startSpan?.status).toBe("error");
		expect(startSpan?.attrs["kernel.start.outcome"]).toBe("failed");

		rmSync(dieOnBootPath);
		await expect(kernel.execute("read")).resolves.toMatchObject({ status: "ok", stdout: "fresh" });
		expect(spawnCount(countPath)).toBe(2);
	});

	it("traces the crash under the failing kernel.execute span and links it to the spawn", async () => {
		const sessionStore = new AsyncLocalStorage<string>();
		setLogSink((entry) => entries.push({ ...entry, sessionId: sessionStore.getStore() }));
		const { manager: kernel } = newManager();
		// Spawned in its own trace (a prewarm), like production: the exit must
		// land in the prompt's trace, not under this long-closed span.
		await kernel.start();

		const outer = await sessionStore.run("session-1", () =>
			withSpan("outer", async (span) => {
				await kernel.execute("pid");
				await new Promise((r) => setTimeout(r, 5));
				await crash(kernel);
				return span.context;
			}),
		);

		const startSpan = spans.find((s) => s.name === "kernel.start");
		expect(startSpan?.attrs).toMatchObject({ "kernel.start.outcome": "spawned" });
		expect(typeof startSpan?.attrs["kernel.pid"]).toBe("number");

		const executeSpans = spans.filter((s) => s.name === "kernel.execute");
		expect(executeSpans).toHaveLength(2);
		const failed = executeSpans[1] as SpanEndRecord;
		expect(failed).toMatchObject({ traceId: outer.traceId, parentSpanId: outer.spanId, status: "error" });
		expect(failed.attrs).toMatchObject({
			"kernel.request_type": "execute",
			"kernel.status": "error",
			"kernel.exit_code": 1,
		});
		expect(failed.attrs["kernel.signal"]).toBeUndefined();
		expect(failed.attrs["kernel.uptime_ms"]).toBeGreaterThan(0);

		const exitEntries = kernelExitEntries();
		expect(exitEntries).toHaveLength(1);
		const [exitEntry] = exitEntries as [LogEntry];
		// Logged in the failing request's context: its trace, its span, its session.
		expect(exitEntry).toMatchObject({
			level: "error",
			traceId: outer.traceId,
			spanId: failed.spanId,
			sessionId: "session-1",
			pid: startSpan?.attrs["kernel.pid"],
			exitCode: 1,
			signal: null,
			requestId: failed.attrs["kernel.request_id"],
			requestType: "execute",
			uptimeMs: failed.attrs["kernel.uptime_ms"],
			startTraceId: startSpan?.traceId,
			startSpanId: startSpan?.spanId,
		});
		expect(exitEntry.stderrTail).toContain(CRASH_STDERR);
		expect(startSpan?.traceId).not.toBe(outer.traceId);
	});

	it("marks a start that found the manager shut down as skipped, never as a healthy spawn", async () => {
		const { manager: kernel } = newManager();
		await kernel.start();
		await kernel.kill();
		await kernel.start();
		const startSpans = spans.filter((s) => s.name === "kernel.start");
		expect(startSpans.map((s) => s.attrs["kernel.start.outcome"])).toEqual(["spawned", "skipped"]);
		expect(startSpans[1]?.attrs).toMatchObject({ "kernel.state": "shutdown" });
		expect(startSpans[1]?.attrs["kernel.pid"]).toBeUndefined();

		// A respawn after a crash is a real spawn again.
		const { manager: crashed } = newManager();
		await crashed.start();
		await crash(crashed);
		await crashed.execute("pid");
		const respawns = spans.filter((s) => s.name === "kernel.start").slice(2);
		expect(respawns.map((s) => s.attrs["kernel.start.outcome"])).toEqual(["spawned", "spawned"]);
		expect(respawns[1]?.attrs).toMatchObject({ "kernel.bootstrapped": true });
	});
});

type RaceInternals = {
	state: string;
	readyAt?: number;
	child: EventEmitter & {
		exitCode: number | null;
		signalCode: NodeJS.Signals | null;
		kill: (signal?: NodeJS.Signals | number) => boolean;
		pid?: number;
		stdin: { destroyed: boolean; destroy: () => void };
		stdout: { destroy: () => void; on: (event: string, listener: (...args: unknown[]) => void) => void };
		stderr: EventEmitter & { closed: boolean; destroy: () => void };
	};
	writeLine: (request: Record<string, unknown>) => Promise<void>;
	wireChild: (child: RaceInternals["child"]) => void;
};

describe("ReplKernelManager unexpected exit racing a host teardown", () => {
	it("lets a graceful shutdown that starts before the stderr drain keep ownership of the state", async () => {
		const manager = new ReplKernelManager({ cwd: process.cwd() });
		const internals = manager as unknown as RaceInternals;
		const stderr = Object.assign(new EventEmitter(), { closed: false, destroy: vi.fn() });
		const child = Object.assign(new EventEmitter(), {
			exitCode: null as number | null,
			signalCode: null as NodeJS.Signals | null,
			kill: vi.fn(() => false),
			pid: 4242,
			stdin: { destroyed: false, destroy: vi.fn() },
			stdout: { destroy: vi.fn(), on: vi.fn() },
			stderr,
		});
		Object.assign(internals, {
			state: "running",
			readyAt: Date.now() - 50,
			writeLine: vi.fn(async () => {}),
			child,
		});
		internals.wireChild(child);

		// The kernel dies while running; its stderr pipe has not drained yet.
		child.exitCode = 1;
		child.emit("exit", 1, null);
		expect(internals.state).toBe("running");
		expect(manager.lastUnexpectedExit?.exitCode).toBe(1);

		// A graceful shutdown begins before the drain finishes and owns the teardown.
		const shutdown = manager.shutdown();
		expect(internals.state).toBe("shutdown");
		stderr.closed = true;
		stderr.emit("close");
		await expect(shutdown).resolves.toBe(true);
		expect(internals.state).toBe("shutdown");
		expect(internals.child).toBeUndefined();
		await expect(manager.execute("noop")).rejects.toThrow(
			/^Kernel has been shut down: Kernel process exited unexpectedly/,
		);
	});

	it("settles at idle when nothing else claimed the dead child by the time stderr drained", async () => {
		const manager = new ReplKernelManager({ cwd: process.cwd() });
		const internals = manager as unknown as RaceInternals;
		const stderr = Object.assign(new EventEmitter(), { closed: false, destroy: vi.fn() });
		const child = Object.assign(new EventEmitter(), {
			exitCode: null as number | null,
			signalCode: null as NodeJS.Signals | null,
			kill: vi.fn(() => false),
			pid: 4243,
			stdin: { destroyed: false, destroy: vi.fn() },
			stdout: { destroy: vi.fn(), on: vi.fn() },
			stderr,
		});
		Object.assign(internals, {
			state: "running",
			readyAt: Date.now() - 50,
			writeLine: vi.fn(async () => {}),
			child,
		});
		internals.wireChild(child);

		child.signalCode = "SIGSEGV";
		child.emit("exit", null, "SIGSEGV");
		stderr.closed = true;
		stderr.emit("close");
		expect(internals.state).toBe("idle");
		expect(internals.child).toBeUndefined();
		expect(manager.lastUnexpectedExit).toMatchObject({ exitCode: null, signal: "SIGSEGV" });
		expect(manager.lastUnexpectedExit?.uptimeMs).toBeGreaterThanOrEqual(50);
	});
});
