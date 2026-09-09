import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cleanupSessionResources } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExtensionContext } from "../src/core/extensions/types.js";
import type { KernelBootstrapProgressHandler } from "../src/core/kernel/bootstrap.js";
import {
	type ExecuteResult,
	KernelBusyAfterInterruptError,
	type KernelClient,
	KernelExitedError,
	type KernelUnexpectedExit,
	ReplKernelManager,
} from "../src/core/kernel/index.js";
import { createIpythonToolDefinition, IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

let tempDir = "";

function writeFakePython(opts: { sleepSeconds?: number } = {}): { python: string; countRuns: () => number } {
	const python = join(tempDir, "python");
	const countFile = join(tempDir, "runs");
	writeFileSync(
		python,
		[
			"#!/bin/sh",
			`echo run >> "${countFile}"`,
			...(opts.sleepSeconds ? [`sleep ${opts.sleepSeconds}`] : []),
			"exit 42",
			"",
		].join("\n"),
	);
	chmodSync(python, 0o755);
	const countRuns = () => {
		try {
			return readFileSync(countFile, "utf8").split("\n").filter(Boolean).length;
		} catch {
			return 0;
		}
	};
	return { python, countRuns };
}

function okExecuteResult(): ExecuteResult {
	return { stdout: "ok", stderr: "", status: "ok", durationMs: 1 };
}

function unexpectedExit(overrides: Partial<KernelUnexpectedExit> = {}): KernelUnexpectedExit {
	return {
		exitCode: 1,
		signal: null,
		uptimeMs: 1234,
		requestId: "req-crash",
		requestType: "execute",
		stderrTail: "fatal: native exit",
		at: 1_700_000_000_000,
		...overrides,
	};
}

function fakeProvisioner(ensure: () => Promise<KernelClient>): {
	provisioner: IpythonKernelProvisioner;
	ensure: ReturnType<typeof vi.fn>;
	kill: ReturnType<typeof vi.fn>;
} {
	const ensureMock = vi.fn(ensure);
	const kill = vi.fn(async () => {});
	const takeUnreportedExit = vi.fn(() => undefined);
	const provisioner = { ensure: ensureMock, kill, takeUnreportedExit } as unknown as IpythonKernelProvisioner;
	return { provisioner, ensure: ensureMock, kill };
}

/**
 * A protocol-3 kernel whose execute of a cell containing `CRASH` dies with
 * native exit(3) after writing to stderr, the way idalib's exit(1) kills a
 * real kernel. Every other cell (including the runtime bootstrap) succeeds.
 */
function writeCrashingReplRuntime(): { python: string; countRuns: () => number } {
	const python = join(tempDir, "python-crash");
	const countFile = join(tempDir, "crash-runs");
	writeFileSync(
		python,
		`#!/usr/bin/env node
const fs = require("node:fs");
const readline = require("node:readline");
fs.appendFileSync(${JSON.stringify(countFile)}, "run\\n");
const emit = (event) => process.stdout.write(JSON.stringify(event) + "\\n");
emit({ event: "ready", protocol: 3, python: process.version });
const input = readline.createInterface({ input: process.stdin });
input.on("line", (line) => {
	const request = JSON.parse(line);
	if (request.type === "restore") {
		emit({ event: "done", id: request.id, status: "ok", restored: [], failed: [] });
		return;
	}
	if (request.type === "snapshot") {
		emit({ event: "done", id: request.id, status: "ok", saved: [], skipped: [], bytes: 0 });
		return;
	}
	if (request.type === "execute") {
		if (String(request.code).includes("CRASH")) {
			fs.writeSync(2, "fatal: native exit from cell\\n");
			process.exit(3);
		}
		emit({ event: "stdout", id: request.id, text: "ran " + request.code });
		emit({ event: "done", id: request.id, status: "ok" });
		return;
	}
	if (request.type === "shutdown") {
		emit({ event: "done", id: request.id, status: "ok" });
		process.exit(0);
	}
});
`,
	);
	chmodSync(python, 0o755);
	const countRuns = () => {
		try {
			return readFileSync(countFile, "utf8").split("\n").filter(Boolean).length;
		} catch {
			return 0;
		}
	};
	return { python, countRuns };
}

function textOf(result: { content: Array<{ type: string; text?: string }> }): string {
	return result.content[0]?.type === "text" ? (result.content[0].text ?? "") : "";
}

/** The ipython tool flags failed cells with an `isError` the core result type does not declare. */
function isErrorOf(result: object): boolean | undefined {
	return (result as { isError?: boolean }).isError;
}

function createBusyKernelContext(
	select: (title: string, options: string[]) => Promise<string | undefined>,
	options: { throwWorkingMessage?: boolean } = {},
): {
	ctx: ExtensionContext;
	setWorkingMessage: ReturnType<typeof vi.fn>;
} {
	const setWorkingMessage = vi.fn(() => {
		if (options.throwWorkingMessage) {
			throw new Error("stale UI context");
		}
	});
	const ctx = {
		hasUI: true,
		ui: {
			select,
			setWorkingMessage,
		},
	} as unknown as ExtensionContext;
	return { ctx, setWorkingMessage };
}

function writeFakeReplRuntime(
	markerPath: string,
	options: { gatedExecute?: { startedPath: string; gatePath: string } } = {},
): string {
	const python = join(tempDir, "python-repl");
	const refuse = `emit({ event: "error", id: request.id, ename: "RuntimeError", evalue: "bootstrap refused", traceback: [] });
		emit({ event: "done", id: request.id, status: "error" });`;
	const executeBranch = options.gatedExecute
		? `fs.writeFileSync(${JSON.stringify(options.gatedExecute.startedPath)}, "1");
		const gate = setInterval(() => {
			if (!fs.existsSync(${JSON.stringify(options.gatedExecute.gatePath)})) return;
			clearInterval(gate);
			${refuse}
		}, 10);`
		: refuse;
	writeFileSync(
		python,
		`#!/usr/bin/env node
const fs = require("node:fs");
const readline = require("node:readline");
const emit = (event) => process.stdout.write(JSON.stringify(event) + "\\n");
emit({ event: "ready", protocol: 3, python: process.version });
const input = readline.createInterface({ input: process.stdin });
input.on("line", (line) => {
	const request = JSON.parse(line);
	if (request.type === "restore") {
		emit({ event: "done", id: request.id, status: "ok", restored: [], failed: [] });
		return;
	}
	if (request.type === "snapshot") {
		setTimeout(() => {
			fs.writeFileSync(${JSON.stringify(markerPath)}, "1");
			emit({ event: "done", id: request.id, status: "ok", saved: [], skipped: [], bytes: 0 });
		}, 150);
		return;
	}
	if (request.type === "execute") {
		${executeBranch}
		return;
	}
	if (request.type === "shutdown") {
		emit({ event: "done", id: request.id, status: "ok" });
		process.exit(0);
	}
});
`,
	);
	chmodSync(python, 0o755);
	return python;
}

describe("IpythonKernelProvisioner", () => {
	beforeEach(() => {
		tempDir = mkdtempSync(join(tmpdir(), "prime-agent-provisioner-"));
	});

	afterEach(() => {
		if (tempDir) {
			rmSync(tempDir, { recursive: true, force: true });
			tempDir = "";
		}
	});

	it("does not surface a failed startup before the kernel's final snapshot flush finished", async () => {
		const marker = join(tempDir, "snapshot-flushed");
		const snapshotDir = join(tempDir, "snapshots");
		mkdirSync(snapshotDir, { recursive: true });
		const python = writeFakeReplRuntime(marker);
		const provisioner = new IpythonKernelProvisioner(tempDir, { python, snapshotDir });
		try {
			await expect(provisioner.ensure()).rejects.toThrow(/Failed to initialize rlm runtime/);
			// The failed kernel's teardown (final snapshot flush included) completed
			// before the failure surfaced, so a replacement provisioner gated on this
			// one cannot race the still-flushing kernel over the same snapshot files.
			expect(existsSync(marker)).toBe(true);
		} finally {
			await provisioner.dispose();
		}
	});

	it("memoizes concurrent ensure() calls into one startup", async () => {
		const { python, countRuns } = writeFakePython();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });

		const [a, b] = await Promise.allSettled([provisioner.ensure(), provisioner.ensure()]);
		expect(a.status).toBe("rejected");
		expect(b.status).toBe("rejected");
		expect(countRuns()).toBe(1);
	});

	it("retries after a failed startup instead of caching the rejection", async () => {
		const { python, countRuns } = writeFakePython();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });

		await expect(provisioner.ensure()).rejects.toThrow(/Kernel exited before ready/);
		await expect(provisioner.ensure()).rejects.toThrow(/Kernel exited before ready/);
		expect(countRuns()).toBe(2);
	});

	it("prewarm() swallows the failure and the next ensure() starts fresh", async () => {
		const { python, countRuns } = writeFakePython();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });

		provisioner.prewarm();
		expect(provisioner.manager).toBeUndefined();

		// Once the prewarm startup settles, ensure() must launch a second attempt.
		await vi.waitFor(async () => {
			await expect(provisioner.ensure()).rejects.toThrow();
			expect(countRuns()).toBeGreaterThanOrEqual(2);
		});
	});

	it("replays the current startup stage to listeners attaching mid-flight", async () => {
		const { python } = writeFakePython({ sleepSeconds: 1 });
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });

		provisioner.prewarm();
		const messages: string[] = [];
		const joined = provisioner.ensure((message) => messages.push(message));
		expect(messages).toContain("Starting Python kernel...");
		await expect(joined).rejects.toThrow();
	});

	it("dispose() settles a startup that is still in flight", async () => {
		const { python } = writeFakePython();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });

		provisioner.prewarm();
		await provisioner.dispose();
		expect(provisioner.manager).toBeUndefined();
	});

	it("skips the snapshot when dispose({ snapshot: false }) aborts a startup in flight", async () => {
		const marker = join(tempDir, "snapshot-flushed");
		const executeStarted = join(tempDir, "execute-started");
		const executeGate = join(tempDir, "execute-gate");
		const snapshotDir = join(tempDir, "snapshots");
		mkdirSync(snapshotDir, { recursive: true });
		const python = writeFakeReplRuntime(marker, {
			gatedExecute: { startedPath: executeStarted, gatePath: executeGate },
		});
		const provisioner = new IpythonKernelProvisioner(tempDir, { python, snapshotDir });

		const started = provisioner.ensure().catch(() => undefined);
		await vi.waitFor(() => expect(existsSync(executeStarted)).toBe(true));
		const disposed = provisioner.dispose({ snapshot: false });
		writeFileSync(executeGate, "1");
		await Promise.all([disposed, started]);
		expect(existsSync(marker)).toBe(false);
	});

	it("dispose({ snapshot: false }) skips the kernel's final snapshot flush", async () => {
		const { python } = writeFakePython();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });
		const shutdown = vi.fn(async () => {});
		Reflect.set(provisioner, "managerPromise", Promise.resolve({ shutdown }));

		await provisioner.dispose({ snapshot: false });
		expect(shutdown).toHaveBeenCalledWith({ snapshot: false, drainHostRequests: true });
	});

	it("dispose() before the boot slot prevents the kernel from spawning", async () => {
		const { python, countRuns } = writeFakePython();
		let release: () => void = () => {};
		const gate = new Promise<void>((r) => {
			release = r;
		});
		const provisioner = new IpythonKernelProvisioner(tempDir, { python, readyGate: gate });

		const started = provisioner.ensure().catch(() => {});
		const disposed = provisioner.dispose(); // aborts while the boot waits on readyGate
		release();
		await Promise.all([started, disposed]);
		expect(countRuns()).toBe(0); // disposed boot must never spawn a kernel
	});

	it("aborting the startup owner before the boot slot prevents the kernel from spawning", async () => {
		const { python, countRuns } = writeFakePython();
		let release: () => void = () => {};
		const gate = new Promise<void>((r) => {
			release = r;
		});
		const provisioner = new IpythonKernelProvisioner(tempDir, { python, readyGate: gate });
		const controller = new AbortController();

		const started = provisioner.ensure(undefined, controller.signal);
		controller.abort();
		await expect(started).rejects.toThrow("Python execution aborted");
		release();
		await new Promise((r) => setTimeout(r, 50));

		expect(countRuns()).toBe(0);
		expect(provisioner.manager).toBeUndefined();
	});

	it("waits for readyGate before starting the kernel", async () => {
		const { python, countRuns } = writeFakePython();
		let release: () => void = () => {};
		const gate = new Promise<void>((r) => {
			release = r;
		});
		const provisioner = new IpythonKernelProvisioner(tempDir, { python, readyGate: gate });

		const started = provisioner.ensure().catch(() => {});
		await new Promise((r) => setTimeout(r, 50));
		expect(countRuns()).toBe(0); // gated: must not spawn the kernel yet

		release();
		await started;
		expect(countRuns()).toBe(1);
	});

	it("namespace maintenance returns null when no kernel is running", async () => {
		const provisioner = new IpythonKernelProvisioner(tempDir, {});
		expect(await provisioner.listNamespaceNames()).toBeNull();
		expect(await provisioner.pruneOversizedVariables()).toBeNull();
	});

	it("does not dispose a running kernel when an ensure caller is aborted", async () => {
		const provisioner = new IpythonKernelProvisioner(tempDir, {});
		const dispose = vi.fn(async () => {});
		const manager = { dispose, isRunning: true } as unknown as KernelClient;
		Object.assign(
			provisioner as unknown as {
				managerPromise: Promise<KernelClient>;
				startedManager: KernelClient;
			},
			{
				managerPromise: Promise.resolve(manager),
				startedManager: manager,
			},
		);
		const controller = new AbortController();
		controller.abort();

		await expect(provisioner.ensure(undefined, controller.signal)).rejects.toThrow("Python execution aborted");
		expect(dispose).not.toHaveBeenCalled();
		expect(provisioner.manager).toBe(manager);
	});

	it("removes startup progress listeners when an ensure caller is aborted", async () => {
		const provisioner = new IpythonKernelProvisioner(tempDir, {});
		Object.assign(
			provisioner as unknown as {
				managerPromise: Promise<KernelClient>;
			},
			{
				managerPromise: new Promise<KernelClient>(() => {}),
			},
		);
		const controller = new AbortController();
		const onProgress = vi.fn();

		const ensurePromise = provisioner.ensure(onProgress, controller.signal).catch(() => undefined);
		controller.abort();
		await ensurePromise;

		const internals = provisioner as unknown as {
			startupListeners: Set<KernelBootstrapProgressHandler>;
		};
		expect(internals.startupListeners.has(onProgress)).toBe(false);
	});

	it("surfaces backgroundOutput in details without changing model content", async () => {
		const execute = vi
			.fn<KernelClient["execute"]>()
			.mockResolvedValueOnce({ ...okExecuteResult(), backgroundOutput: "bg-line" });
		const manager = { execute } as unknown as KernelClient;
		const { provisioner } = fakeProvisioner(async () => manager);
		const tool = createIpythonToolDefinition(tempDir, { provisioner });

		const result = await tool.execute("tool-call", { code: "x = 1" }, undefined, undefined, {} as ExtensionContext);

		expect(result.details.backgroundOutput).toBe("bg-line");
		expect(result.content).toEqual([{ type: "text", text: "ok\n[background output (unattributed)]\nbg-line" }]);
	});

	it("lets the user wait when an interrupted kernel is still busy", async () => {
		const execute = vi
			.fn<KernelClient["execute"]>()
			.mockRejectedValueOnce(new KernelBusyAfterInterruptError())
			.mockResolvedValueOnce(okExecuteResult());
		const manager = { execute } as unknown as KernelClient;
		const { provisioner, ensure, kill } = fakeProvisioner(async () => manager);
		const select = vi.fn(async () => "Wait and preserve state");
		const { ctx, setWorkingMessage } = createBusyKernelContext(select, { throwWorkingMessage: true });
		const tool = createIpythonToolDefinition(tempDir, { provisioner });

		const result = await tool.execute("tool-call", { code: "x = 1" }, undefined, undefined, ctx);

		expect(result.details.status).toBe("ok");
		expect(result.details.kernelRestarted).toBe(false);
		expect(ensure).toHaveBeenCalledTimes(2);
		expect(kill).not.toHaveBeenCalled();
		expect(select).toHaveBeenCalledWith(
			expect.stringContaining("previous cell has not stopped"),
			["Wait and preserve state", "Kill kernel and restart"],
			{
				signal: undefined,
			},
		);
		expect(setWorkingMessage).toHaveBeenCalledWith("Waiting for Python kernel...");
		expect(setWorkingMessage).toHaveBeenLastCalledWith(undefined);
	});

	it("lets the user kill and restart a busy interrupted kernel", async () => {
		const busyManager = {
			execute: vi.fn<KernelClient["execute"]>().mockRejectedValueOnce(new KernelBusyAfterInterruptError()),
		} as unknown as KernelClient;
		const freshManager = {
			execute: vi.fn<KernelClient["execute"]>().mockResolvedValueOnce(okExecuteResult()),
		} as unknown as KernelClient;
		const { provisioner, ensure, kill } = fakeProvisioner(async () => {
			return ensure.mock.calls.length === 1 ? busyManager : freshManager;
		});
		const select = vi.fn(async () => "Kill kernel and restart");
		const { ctx, setWorkingMessage } = createBusyKernelContext(select, { throwWorkingMessage: true });
		const tool = createIpythonToolDefinition(tempDir, { provisioner });

		const result = await tool.execute("tool-call", { code: "x = 1" }, undefined, undefined, ctx);
		const text = result.content[0]?.type === "text" ? result.content[0].text : "";

		expect(result.details.status).toBe("ok");
		expect(result.details.kernelRestarted).toBe(true);
		expect(text).toContain("<ipython_kernel_reset>");
		expect(text).toContain("Variables, imports, async tasks, and open resources");
		expect(text).toContain("ok");
		expect(ensure).toHaveBeenCalledTimes(2);
		expect(kill).toHaveBeenCalledTimes(1);
		expect(freshManager.execute).toHaveBeenCalledWith("x = 1", expect.objectContaining({ signal: undefined }));
		expect(setWorkingMessage).toHaveBeenCalledWith("Restarting Python kernel...");
		expect(setWorkingMessage).toHaveBeenLastCalledWith(undefined);
	});

	it("reports a kernel that died mid-cell as a tool error with structured crash details", async () => {
		const exit = unexpectedExit();
		const execute = vi.fn<KernelClient["execute"]>().mockRejectedValueOnce(new KernelExitedError(exit));
		const manager = { execute } as unknown as KernelClient;
		const { provisioner, ensure, kill } = fakeProvisioner(async () => manager);
		const select = vi.fn(async () => "Kill kernel and restart");
		const { ctx } = createBusyKernelContext(select);
		const tool = createIpythonToolDefinition(tempDir, { provisioner });

		const result = await tool.execute("tool-call", { code: "import idapro" }, undefined, undefined, ctx);

		expect(isErrorOf(result)).toBe(true);
		expect(textOf(result)).toContain(
			"Kernel process exited unexpectedly (exit code 1) while serving execute request req-crash",
		);
		expect(textOf(result)).toContain("A fresh kernel starts on the next call");
		expect(textOf(result)).toContain("fatal: native exit");
		expect(result.details.status).toBe("error");
		expect(result.details.errorEname).toBe("KernelExitedError");
		expect(result.details.kernelRestarted).toBe(false);
		expect(result.details.kernelCrashed).toEqual({
			exitCode: 1,
			signal: null,
			uptimeMs: 1234,
			requestId: "req-crash",
			stderrTail: "fatal: native exit",
		});
		// A cell that kills the interpreter must never be re-run automatically.
		expect(execute).toHaveBeenCalledTimes(1);
		expect(ensure).toHaveBeenCalledTimes(1);
		expect(kill).not.toHaveBeenCalled();
		expect(select).not.toHaveBeenCalled();
	});

	it("prepends the crash-recovery notice to the first successful cell after an unexpected exit, once", async () => {
		const exit = unexpectedExit({ exitCode: 3, at: Date.UTC(2026, 0, 2, 3, 4, 5) });
		const execute = vi
			.fn<KernelClient["execute"]>()
			.mockResolvedValueOnce({ ...okExecuteResult(), stdout: "first" })
			.mockResolvedValueOnce({ ...okExecuteResult(), stdout: "second" });
		const manager = { execute, isRunning: true, lastUnexpectedExit: exit } as unknown as KernelClient;
		const provisioner = new IpythonKernelProvisioner(tempDir, {});
		Reflect.set(provisioner, "managerPromise", Promise.resolve(manager));
		Reflect.set(provisioner, "startedManager", manager);
		const tool = createIpythonToolDefinition(tempDir, { provisioner });

		const first = await tool.execute("call-1", { code: "x" }, undefined, undefined, {} as ExtensionContext);
		const firstText = textOf(first);
		expect(isErrorOf(first)).toBe(false);
		expect(firstText.startsWith("<ipython_kernel_reset>\n")).toBe(true);
		expect(firstText).toContain("exited unexpectedly (exit code 3) at 2026-01-02T03:04:05.000Z");
		expect(firstText).toContain("variables were revived from the last snapshot");
		expect(firstText.endsWith("\n\nfirst")).toBe(true);
		expect(firstText.split("exited unexpectedly").length - 1).toBe(1);

		const second = await tool.execute("call-2", { code: "y" }, undefined, undefined, {} as ExtensionContext);
		expect(textOf(second)).toBe("second");
	});

	it("reports a later unexpected exit again, keyed by its timestamp", async () => {
		const manager = {
			isRunning: true,
			lastUnexpectedExit: unexpectedExit({ at: 1 }),
		} as unknown as KernelClient & { lastUnexpectedExit: KernelUnexpectedExit | undefined };
		const provisioner = new IpythonKernelProvisioner(tempDir, {});
		Reflect.set(provisioner, "startedManager", manager);

		expect(provisioner.takeUnreportedExit()?.at).toBe(1);
		expect(provisioner.takeUnreportedExit()).toBeUndefined();
		manager.lastUnexpectedExit = unexpectedExit({ at: 2 });
		expect(provisioner.takeUnreportedExit()?.at).toBe(2);
		expect(provisioner.takeUnreportedExit()).toBeUndefined();
	});

	it("takeUnreportedExit() is empty without a started kernel", () => {
		const provisioner = new IpythonKernelProvisioner(tempDir, {});
		expect(provisioner.takeUnreportedExit()).toBeUndefined();
	});

	// The next two cases exercise the real ReplKernelManager: a kernel that
	// dies mid-cell must reject that cell with KernelExitedError, settle to
	// idle, and respawn on the next call without any provisioner intervention.
	it("surfaces a real kernel's mid-cell death as a crash result and respawns on the next call", async () => {
		const { python, countRuns } = writeCrashingReplRuntime();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });
		const tool = createIpythonToolDefinition(tempDir, { provisioner });
		try {
			const before = await tool.execute("call-0", { code: "x = 1" }, undefined, undefined, {} as ExtensionContext);
			expect(isErrorOf(before)).toBe(false);
			expect(textOf(before)).toBe("ran x = 1");
			expect(countRuns()).toBe(1);
			expect(provisioner.hasRunningKernel).toBe(true);

			const crash = await tool.execute("call-1", { code: "CRASH()" }, undefined, undefined, {} as ExtensionContext);
			expect(isErrorOf(crash)).toBe(true);
			expect(textOf(crash)).toContain("exited unexpectedly (exit code 3)");
			expect(textOf(crash)).toContain("fatal: native exit from cell");
			expect(crash.details.kernelCrashed).toMatchObject({
				exitCode: 3,
				signal: null,
				stderrTail: expect.stringContaining("native exit from cell"),
			});
			expect(crash.details.kernelCrashed?.uptimeMs).toBeGreaterThanOrEqual(0);
			// The manager settled instead of sticking in "shutdown"; the crashed
			// cell itself was not re-run.
			expect(provisioner.hasRunningKernel).toBe(false);
			expect(countRuns()).toBe(1);
			expect(provisioner.manager?.lastUnexpectedExit).toMatchObject({ exitCode: 3 });

			const after = await tool.execute("call-2", { code: "y = 2" }, undefined, undefined, {} as ExtensionContext);
			expect(isErrorOf(after)).toBe(false);
			const afterText = textOf(after);
			expect(afterText.startsWith("<ipython_kernel_reset>\n")).toBe(true);
			expect(afterText).toContain("restarted after it exited unexpectedly (exit code 3)");
			expect(afterText.endsWith("\n\nran y = 2")).toBe(true);
			expect(countRuns()).toBe(2);
			expect(provisioner.hasRunningKernel).toBe(true);
			expect(provisioner.manager).toBe(await provisioner.ensure());

			const again = await tool.execute("call-3", { code: "z = 3" }, undefined, undefined, {} as ExtensionContext);
			expect(textOf(again)).toBe("ran z = 3");
		} finally {
			await provisioner.dispose({ snapshot: false });
		}
	});

	it("does not repeat the crash notice and reports a second crash separately", async () => {
		const { python, countRuns } = writeCrashingReplRuntime();
		const provisioner = new IpythonKernelProvisioner(tempDir, { python });
		const tool = createIpythonToolDefinition(tempDir, { provisioner });
		try {
			const first = await tool.execute("call-1", { code: "CRASH()" }, undefined, undefined, {} as ExtensionContext);
			expect(first.details.kernelCrashed?.exitCode).toBe(3);
			const recovered = await tool.execute("call-2", { code: "a" }, undefined, undefined, {} as ExtensionContext);
			expect(textOf(recovered)).toContain("<ipython_kernel_reset>");

			const second = await tool.execute("call-3", { code: "CRASH()" }, undefined, undefined, {} as ExtensionContext);
			expect(isErrorOf(second)).toBe(true);
			expect(second.details.kernelCrashed?.exitCode).toBe(3);
			const recoveredAgain = await tool.execute(
				"call-4",
				{ code: "b" },
				undefined,
				undefined,
				{} as ExtensionContext,
			);
			expect(textOf(recoveredAgain)).toContain("<ipython_kernel_reset>");
			expect(textOf(recoveredAgain).split("exited unexpectedly").length - 1).toBe(1);
			expect(countRuns()).toBe(3);
		} finally {
			await provisioner.dispose({ snapshot: false });
		}
	});

	it("does not delete the on-disk snapshot (the kernel survives compaction)", async () => {
		const snapshotDir = join(tempDir, "artifacts");
		const provisioner = new IpythonKernelProvisioner(tempDir, { snapshotDir });
		const dill = join(snapshotDir, "kernel-state.dill");
		const manifest = join(snapshotDir, "kernel-state.json");
		mkdirSync(snapshotDir, { recursive: true });
		writeFileSync(dill, "payload");
		writeFileSync(manifest, "{}");

		// listing the namespace must never touch the on-disk snapshot
		await provisioner.listNamespaceNames();

		expect(existsSync(dill)).toBe(true);
		expect(existsSync(manifest)).toBe(true);
	});
});

describe("ReplKernelManager session cleanup during startup", () => {
	beforeEach(() => {
		tempDir = mkdtempSync(join(tmpdir(), "prime-agent-kernel-cleanup-"));
	});

	afterEach(() => {
		if (tempDir) {
			rmSync(tempDir, { recursive: true, force: true });
			tempDir = "";
		}
	});

	it("disposes a kernel that is still booting when its session is cleaned up", async () => {
		const python = join(tempDir, "python");
		// Never emits the ready line - stays in the booting phase until killed.
		writeFileSync(python, ["#!/bin/sh", "sleep 30", ""].join("\n"));
		chmodSync(python, 0o755);
		const sessionId = `provisioner-test-${Date.now()}`;
		const manager = new ReplKernelManager({ python, cwd: tempDir, sessionId });

		try {
			const startup = manager.start();
			cleanupSessionResources(sessionId);
			await expect(startup).rejects.toThrow(/Kernel exited before ready|disposed during startup/);
			expect(manager.isRunning).toBe(false);
		} finally {
			await manager.shutdown({ snapshot: true, drainHostRequests: true });
		}
	});
});
