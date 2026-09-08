import { AsyncLocalStorage } from "node:async_hooks";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	currentTraceContext,
	formatTraceparent,
	installAsyncTraceContextStorage,
	installDefaultSpanSink,
	type LogEntry,
	parseTraceparent,
	type SpanEndRecord,
	setLogSink,
	setSpanSink,
	type TraceContext,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { type HostRequestHandlers, ReplKernelManager } from "../src/core/kernel/index.js";

// Deterministic propagation across the awaits inside the manager regardless of
// whether pi-ai's best-effort async install has resolved yet (idempotent).
installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const HOST_SPAN_ID = "b7ad6b7169203331";

/**
 * Fake runtime speaking protocol 3: journals every request frame and its boot
 * env, and replays trace/host_request frames on demand so the host-side
 * bridge can be exercised without a Python interpreter.
 */
function writeFakeRuntime(path: string): void {
	writeFileSync(
		path,
		`#!/usr/bin/env node
const fs = require("node:fs");
const readline = require("node:readline");
fs.writeFileSync(process.env.FAKE_REPL_ENV_LOG, process.env.TRACEPARENT ?? "");
if (process.env.FAKE_REPL_ENV_JSON) {
  const internal = Object.fromEntries(Object.entries(process.env).filter(([k]) => k.startsWith("PRIME_AGENT_INTERNAL_")));
  fs.writeFileSync(process.env.FAKE_REPL_ENV_JSON, JSON.stringify(internal));
}
const emit = (event) => process.stdout.write(JSON.stringify(event) + "\\n");
const pendingHost = new Map();
emit({ event: "ready", protocol: 3, python: process.version });
const input = readline.createInterface({ input: process.stdin });
input.on("line", (line) => {
  const request = JSON.parse(line);
  fs.appendFileSync(process.env.FAKE_REPL_REQUEST_LOG, line + "\\n");
  if (request.type === "interrupt") {
    emit({ event: "error", id: request.id, ename: "KeyboardInterrupt", evalue: "", traceback: [] });
    emit({ event: "done", id: request.id, status: "error" });
    return;
  }
  if (request.type === "host_reply") {
    const pending = pendingHost.get(request.id);
    pendingHost.delete(request.id);
    if (pending) pending(request.data);
    return;
  }
  if (request.type === "execute") {
    const parts = typeof request.traceparent === "string" ? request.traceparent.split("-") : [];
    const traceId = parts[1] ?? "0af7651916cd43dd8448eb211c80319c";
    const parentSpanId = parts[2];
    const span = {
      event: "trace",
      id: request.id,
      msg: "span_end",
      name: "kernel.cell",
      traceId,
      spanId: "${HOST_SPAN_ID}",
      parentSpanId,
      durationMs: 1.5,
      status: "ok",
      attrs: { "kernel.request_id": request.id, "kernel.request_type": "execute" },
    };
    if (request.code === "hang") return;
    if (request.code === "emit-trace") {
      emit({ ...span, msg: "span_start", durationMs: undefined, status: undefined });
      emit({ event: "trace", id: request.id, component: "bash", msg: "cargo_lock_wait", traceId, spanId: "${HOST_SPAN_ID}", attrs: { "bash.pid": 42, "bash.wait_reason": "cargo_build_lock" } });
      emit(span);
    }
    if (request.code === "emit-error-trace") {
      emit({ ...span, status: "error", attrs: { ...span.attrs, error: "ValueError: nope" } });
    }
    if (request.code === "emit-bad-trace") {
      emit({ event: "trace", id: null, msg: "span_end" });
      emit({ event: "trace", msg: "span_start", name: "x", traceId: 7, spanId: "${HOST_SPAN_ID}" });
      emit({ event: "trace", msg: "span_end", name: 7, traceId, spanId: "${HOST_SPAN_ID}" });
    }
    if (request.code.startsWith("host-request")) {
      const frame = { event: "host_request", id: "hr-" + request.id, data: { type: "test.ctx", value: 1 } };
      if (request.code === "host-request") frame.traceparent = "00-" + traceId + "-${HOST_SPAN_ID}-01";
      if (request.code === "host-request-bad") frame.traceparent = "00-not-a-traceparent";
      pendingHost.set(frame.id, (reply) => {
        emit({ event: "stdout", id: request.id, text: JSON.stringify(reply) });
        emit({ event: "done", id: request.id, status: "ok" });
      });
      emit(frame);
      return;
    }
    emit({ event: "done", id: request.id, status: "ok" });
    return;
  }
  if (request.type === "snapshot") {
    fs.writeFileSync(request.path, "{}");
    fs.writeFileSync(request.manifest_path, "{}");
    emit({ event: "done", id: request.id, status: "ok", saved: [], skipped: [], bytes: 2 });
    return;
  }
  if (request.type === "restore") {
    emit({ event: "done", id: request.id, status: "ok", restored: [], failed: [] });
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

describe("ReplKernelManager trace propagation", () => {
	let tempDir = "";
	let requestLogPath = "";
	let envLogPath = "";
	let manager: ReplKernelManager | undefined;
	const spans: SpanEndRecord[] = [];
	const entries: LogEntry[] = [];
	const savedTraceparent = process.env.TRACEPARENT;

	beforeEach(() => {
		tempDir = mkdtempSync(join(tmpdir(), "prime-agent-repl-trace-"));
		requestLogPath = join(tempDir, "requests.jsonl");
		envLogPath = join(tempDir, "env-traceparent");
		spans.length = 0;
		entries.length = 0;
		setSpanSink((record) => spans.push(record));
		setLogSink((entry) => entries.push(entry));
		delete process.env.TRACEPARENT;
	});

	afterEach(async () => {
		await manager?.shutdown({ snapshot: true, drainHostRequests: true });
		manager = undefined;
		installDefaultSpanSink();
		setLogSink(undefined);
		if (savedTraceparent !== undefined) process.env.TRACEPARENT = savedTraceparent;
		if (tempDir) {
			rmSync(tempDir, { recursive: true, force: true });
			tempDir = "";
		}
	});

	function newManager(options: { hostHandlers?: HostRequestHandlers; snapshot?: boolean } = {}): ReplKernelManager {
		const python = join(tempDir, "python");
		writeFakeRuntime(python);
		manager = new ReplKernelManager({
			python,
			cwd: tempDir,
			env: {
				FAKE_REPL_REQUEST_LOG: requestLogPath,
				FAKE_REPL_ENV_LOG: envLogPath,
				FAKE_REPL_ENV_JSON: join(tempDir, "env.json"),
			},
			hostHandlers: options.hostHandlers,
			snapshot: options.snapshot
				? { path: join(tempDir, "state.json"), manifestPath: join(tempDir, "manifest.json"), debounceMs: 1 }
				: undefined,
		});
		return manager;
	}

	function requestFrames(): Record<string, unknown>[] {
		if (!existsSync(requestLogPath)) return [];
		return readFileSync(requestLogPath, "utf8")
			.split("\n")
			.filter((line) => line.trim())
			.map((line) => JSON.parse(line) as Record<string, unknown>);
	}

	function frameFor(predicate: (frame: Record<string, unknown>) => boolean): Record<string, unknown> {
		const frame = requestFrames().find(predicate);
		expect(frame).toBeDefined();
		return frame as Record<string, unknown>;
	}

	it("does not hand the daemon worker identity to the kernel", async () => {
		const injected = {
			PRIME_AGENT_INTERNAL_DAEMON_WORKER: "1",
			PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN: "worker-secret",
			PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET: "/tmp/nope.sock",
			PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID: "owner",
			PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL: join(tempDir, "journal"),
		};
		const saved = Object.fromEntries(Object.keys(injected).map((key) => [key, process.env[key]]));
		Object.assign(process.env, injected);
		try {
			const kernel = newManager();
			expect((await kernel.execute("noop")).status).toBe("ok");
		} finally {
			for (const [key, value] of Object.entries(saved)) {
				if (value === undefined) delete process.env[key];
				else process.env[key] = value;
			}
		}
		const seen = JSON.parse(readFileSync(join(tempDir, "env.json"), "utf8")) as Record<string, string>;
		// A `prime-agent` or vitest run started from a cell must not present the
		// live worker's token to the supervisor or believe it is a worker itself.
		expect(Object.keys(seen).filter((key) => key.includes("DAEMON") || key.includes("SESSION_LEASE"))).toEqual([]);
		// bash.py still enrols its process groups in the orphan journal.
		expect(seen.PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL).toBe(
			injected.PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL,
		);
	});

	it("stamps execute frames with the kernel.execute span, a child of the caller's span", async () => {
		const kernel = newManager();
		const outer = await withSpan("outer", async (span) => {
			const result = await kernel.execute("noop");
			expect(result.status).toBe("ok");
			return span.context;
		});

		const frame = frameFor((f) => f.type === "execute" && f.code === "noop");
		const frameContext = parseTraceparent(frame.traceparent);
		expect(frameContext).toBeDefined();
		expect(frameContext?.traceId).toBe(outer.traceId);

		const executeSpan = spans.find((s) => s.name === "kernel.execute" && s.attrs["kernel.request_id"] === frame.id);
		expect(executeSpan).toBeDefined();
		expect(executeSpan?.spanId).toBe(frameContext?.spanId);
		expect(executeSpan?.parentSpanId).toBe(outer.spanId);
		expect(executeSpan?.traceId).toBe(outer.traceId);
		expect(executeSpan?.status).toBe("ok");
		expect(executeSpan?.attrs).toMatchObject({ "kernel.request_type": "execute", "kernel.status": "ok" });
		// The caller's span outlives the request it wrapped.
		expect(spans.map((s) => s.name).slice(-2)).toEqual(["kernel.execute", "outer"]);
	});

	it("still traces a request with no ambient context as its own root, so no frame is untraceable", async () => {
		const kernel = newManager();
		expect(currentTraceContext()).toBeUndefined();
		await kernel.execute("noop");
		const frame = frameFor((f) => f.type === "execute" && f.code === "noop");
		const frameContext = parseTraceparent(frame.traceparent);
		expect(frameContext).toBeDefined();
		const executeSpan = spans.find((s) => s.name === "kernel.execute" && s.attrs["kernel.request_id"] === frame.id);
		expect(executeSpan?.spanId).toBe(frameContext?.spanId);
		expect(executeSpan?.parentSpanId).toBeUndefined();
	});

	it("stamps state requests (snapshot) too and reports aborted outcomes as kernel.status", async () => {
		const kernel = newManager({ snapshot: true });
		await kernel.execute("noop");
		await kernel.shutdown({ snapshot: true, drainHostRequests: true });
		const snapshot = frameFor((f) => f.type === "snapshot");
		expect(parseTraceparent(snapshot.traceparent)).toBeDefined();
		const snapshotSpan = spans.find(
			(s) => s.name === "kernel.execute" && s.attrs["kernel.request_id"] === snapshot.id,
		);
		expect(snapshotSpan?.attrs).toMatchObject({ "kernel.request_type": "snapshot", "kernel.status": "ok" });

		const aborted = newManager();
		const controller = new AbortController();
		const pending = aborted.execute("hang", { signal: controller.signal });
		await expect.poll(() => requestFrames().some((f) => f.code === "hang")).toBe(true);
		controller.abort();
		expect((await pending).status).toBe("aborted");
		const hang = frameFor((f) => f.code === "hang");
		const hangSpan = spans.find((s) => s.name === "kernel.execute" && s.attrs["kernel.request_id"] === hang.id);
		expect(hangSpan?.attrs["kernel.status"]).toBe("aborted");
	});

	it("forwards runtime trace events into the shared logger with the TS span_end shape", async () => {
		const kernel = newManager();
		const outer = await withSpan("outer", async (span) => {
			expect((await kernel.execute("emit-trace")).status).toBe("ok");
			expect((await kernel.execute("emit-error-trace")).status).toBe("ok");
			return span.context;
		});
		const forwarded = entries.filter((e) => e.component === "trace" && e.name === "kernel.cell");
		expect(forwarded).toHaveLength(3);
		const [started, ok, failed] = forwarded as [LogEntry, LogEntry, LogEntry];
		expect(started).toMatchObject({ level: "info", msg: "span_start", traceId: outer.traceId, spanId: HOST_SPAN_ID });
		const cargoWait = entries.find((entry) => entry.msg === "cargo_lock_wait");
		expect(cargoWait).toMatchObject({ level: "warn", traceId: outer.traceId, spanId: HOST_SPAN_ID });
		const okFrame = frameFor((f) => f.code === "emit-trace");
		expect(ok).toMatchObject({
			level: "info",
			msg: "span_end",
			name: "kernel.cell",
			traceId: outer.traceId,
			spanId: HOST_SPAN_ID,
			parentSpanId: parseTraceparent(okFrame.traceparent)?.spanId,
			durationMs: 1.5,
			status: "ok",
			attrs: { "kernel.request_id": okFrame.id, "kernel.request_type": "execute" },
		});
		expect(ok).not.toHaveProperty("event");
		expect(ok).not.toHaveProperty("id");
		expect(failed.level).toBe("warn");
		expect(failed.status).toBe("error");
		expect(failed.attrs).toMatchObject({ error: "ValueError: nope" });
	});

	it("ignores malformed trace events without disturbing the execution", async () => {
		const kernel = newManager();
		const result = await kernel.execute("emit-bad-trace");
		expect(result.status).toBe("ok");
		expect(entries.filter((e) => e.component === "trace" && e.name !== "kernel.start")).toHaveLength(0);
		// A later cell proves the child was not torn down as corrupt.
		expect((await kernel.execute("noop")).status).toBe("ok");
	});

	it("runs host request handlers under the frame's traceparent inside a kernel.host_request span", async () => {
		const seen: TraceContext[] = [];
		const kernel = newManager({
			hostHandlers: {
				"test.ctx": async (payload) => {
					const context = currentTraceContext();
					if (context) seen.push(context);
					return { value: payload.value, traceparent: context ? formatTraceparent(context) : null };
				},
			},
		});
		const result = await withSpan("outer", () => kernel.execute("host-request"));
		expect(result.status).toBe("ok");
		const frame = frameFor((f) => f.code === "host-request");
		const frameContext = parseTraceparent(frame.traceparent) as TraceContext;
		const reply = JSON.parse(result.stdout) as { status: string; result: { traceparent: string } };
		expect(reply.status).toBe("ok");

		expect(seen).toHaveLength(1);
		const [handlerContext] = seen as [TraceContext];
		expect(handlerContext.traceId).toBe(frameContext.traceId);
		expect(handlerContext.parentSpanId).toBe(HOST_SPAN_ID);
		expect(reply.result.traceparent).toBe(formatTraceparent(handlerContext));

		const hostSpan = spans.find((s) => s.name === "kernel.host_request");
		expect(hostSpan).toMatchObject({
			traceId: frameContext.traceId,
			spanId: handlerContext.spanId,
			parentSpanId: HOST_SPAN_ID,
			status: "ok",
			attrs: { "host_request.rid": `hr-${frame.id}`, "host_request.type": "test.ctx" },
		});
	});

	it("falls back to the ambient context when the host request traceparent is missing or invalid", async () => {
		const seen: (TraceContext | undefined)[] = [];
		const kernel = newManager({
			hostHandlers: {
				"test.ctx": async () => {
					seen.push(currentTraceContext());
					return {};
				},
			},
		});
		for (const code of ["host-request-plain", "host-request-bad"]) {
			const result = await kernel.execute(code);
			expect(result.status).toBe("ok");
			expect((JSON.parse(result.stdout) as { status: string }).status).toBe("ok");
		}
		expect(seen).toHaveLength(2);
		for (const context of seen) {
			// The handler always runs inside its own kernel.host_request span.
			expect(context).toBeDefined();
			expect(context?.parentSpanId).not.toBe(HOST_SPAN_ID);
			expect(context?.traceId).not.toBe("not");
		}
		expect(spans.filter((s) => s.name === "kernel.host_request")).toHaveLength(2);
	});

	it("spawns the kernel under a kernel.start span that is a child of the caller's span", async () => {
		const kernel = newManager({ snapshot: true });
		const boot = await withSpan("boot", async (span) => {
			await kernel.start();
			return span.context;
		});
		const startSpan = spans.find((s) => s.name === "kernel.start");
		expect(startSpan).toBeDefined();
		expect(startSpan?.traceId).toBe(boot.traceId);
		expect(startSpan?.parentSpanId).toBe(boot.spanId);
		expect(startSpan?.status).toBe("ok");
		expect(startSpan?.attrs).toMatchObject({
			"kernel.python": join(tempDir, "python"),
			"kernel.restore": true,
			"kernel.bootstrapped": false,
		});
		expect(typeof startSpan?.attrs["kernel.pid"]).toBe("number");
		// The child inherits the kernel.start span (not the caller's), so Python-side
		// roots parent under the spawn.
		const inherited = parseTraceparent(readFileSync(envLogPath, "utf8"));
		expect(inherited?.traceId).toBe(boot.traceId);
		expect(inherited?.spanId).toBe(startSpan?.spanId);
		// The caller's span outlives the start it wrapped.
		expect(spans.map((s) => s.name).slice(-2)).toEqual(["kernel.start", "boot"]);
	});

	it("still opens kernel.start as its own root when spawned outside any span", async () => {
		const kernel = newManager();
		await kernel.start();
		const startSpan = spans.find((s) => s.name === "kernel.start");
		expect(startSpan).toBeDefined();
		expect(startSpan?.parentSpanId).toBeUndefined();
		expect(startSpan?.attrs).toMatchObject({ "kernel.restore": false, "kernel.bootstrapped": false });
		const inherited = parseTraceparent(readFileSync(envLogPath, "utf8"));
		expect(inherited).toBeDefined();
		expect(formatTraceparent(inherited as TraceContext)).toBe(
			formatTraceparent({ traceId: startSpan?.traceId ?? "", spanId: startSpan?.spanId ?? "", flags: "01" }),
		);
	});

	it("records a failed spawn on the kernel.start span", async () => {
		manager = new ReplKernelManager({ python: join(tempDir, "missing-python"), cwd: tempDir });
		await expect(manager.start()).rejects.toThrow();
		const startSpan = spans.find((s) => s.name === "kernel.start");
		expect(startSpan?.status).toBe("error");
		expect(startSpan?.error).toBeTruthy();
		expect(startSpan?.attrs["kernel.python"]).toBe(join(tempDir, "missing-python"));
	});
});
