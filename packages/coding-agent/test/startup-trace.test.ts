import { AsyncLocalStorage } from "node:async_hooks";
import { mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import {
	installAsyncTraceContextStorage,
	installDefaultSpanSink,
	type SpanEndRecord,
	setSpanSink,
	type TraceContext,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadExtensions } from "../src/core/extensions/loader.js";
import { SessionManager } from "../src/core/session-manager.js";

// Deterministic propagation across the awaits inside the loaders regardless of
// whether pi-ai's best-effort async install has resolved yet (idempotent).
installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const spans: SpanEndRecord[] = [];
let tempDir = "";

beforeEach(() => {
	tempDir = mkdtempSync(join(tmpdir(), "prime-agent-startup-trace-"));
	spans.length = 0;
	setSpanSink((record) => spans.push(record));
});

afterEach(() => {
	installDefaultSpanSink();
	if (tempDir) {
		rmSync(tempDir, { recursive: true, force: true });
		tempDir = "";
	}
});

function spansNamed(name: string): SpanEndRecord[] {
	return spans.filter((s) => s.name === name);
}

describe("extensions.load span", () => {
	const FAST_EXTENSION = `
		export default function (pi) {
			pi.registerCommand("fast", { handler: async () => {} });
		}
	`;
	// The import itself is slow: top-level await before the factory is exported.
	const SLOW_EXTENSION = `
		await new Promise((resolve) => setTimeout(resolve, 120));
		export default function (pi) {
			pi.registerCommand("slow", { handler: async () => {} });
		}
	`;
	const BROKEN_EXTENSION = `
		throw new Error("boom at import");
	`;

	it("records count, per-extension timing for slow imports, the slowest one and the error count", async () => {
		const extDir = join(tempDir, "extensions");
		mkdirSync(extDir);
		const fastPath = join(extDir, "fast-ext.ts");
		const slowPath = join(extDir, "slow-ext", "index.ts");
		const brokenPath = join(extDir, "broken-ext.ts");
		mkdirSync(join(extDir, "slow-ext"));
		writeFileSync(fastPath, FAST_EXTENSION);
		writeFileSync(slowPath, SLOW_EXTENSION);
		writeFileSync(brokenPath, BROKEN_EXTENSION);

		const outer = await withSpan("outer", async (span) => {
			const result = await loadExtensions([fastPath, slowPath, brokenPath], tempDir);
			// Behaviour is unchanged: failures are collected, not thrown.
			expect(result.extensions).toHaveLength(2);
			expect(result.errors).toHaveLength(1);
			expect(result.errors[0]?.path).toBe(brokenPath);
			expect(result.errors[0]?.error).toContain("boom at import");
			return span.context;
		});

		const loadSpans = spansNamed("extensions.load");
		expect(loadSpans).toHaveLength(1);
		const [load] = loadSpans as [SpanEndRecord];
		expect(load.status).toBe("ok");
		expect(load.traceId).toBe(outer.traceId);
		expect(load.parentSpanId).toBe(outer.spanId);
		expect(load.attrs["extensions.count"]).toBe(3);
		expect(load.attrs["extensions.errors"]).toBe(1);
		// jiti + bundled host modules are warmed before the first import so their
		// one-time cost is not attributed to whichever extension comes first.
		expect(typeof load.attrs["extensions.loader_ms"]).toBe("number");
		// `slow-ext/index.ts` labels as its directory (index is generic).
		expect(load.attrs["extensions.slowest"]).toBe("slow-ext");
		expect(load.attrs["extensions.slowest_ms"]).toBeGreaterThanOrEqual(100);
		expect(load.attrs["extensions.slow-ext_ms"]).toBeGreaterThanOrEqual(100);
		expect(load.attrs["extensions.slow-ext_ms"]).toBe(load.attrs["extensions.slowest_ms"]);
		// The fast import may also cross the reporting threshold on a loaded CI
		// host; the deterministic contract is that the deliberately slow import is reported.
		expect(load.durationMs).toBeGreaterThanOrEqual(load.attrs["extensions.slowest_ms"] as number);
	});

	it("emits one span with zero errors when there is nothing to load", async () => {
		const result = await loadExtensions([], tempDir);
		expect(result.extensions).toHaveLength(0);
		const [load] = spansNamed("extensions.load") as [SpanEndRecord];
		expect(load).toBeDefined();
		expect(load.attrs).toEqual({ "extensions.count": 0, "extensions.errors": 0 });
		expect(load.parentSpanId).toBeUndefined();
	});
});

describe("session.load span", () => {
	function writeSession(): string {
		const sessionDir = join(tempDir, "sessions");
		const session = SessionManager.create(tempDir, sessionDir);
		session.appendMessage({ role: "user", content: "hello", timestamp: 1 });
		session.appendMessage({ role: "user", content: "again", timestamp: 2 });
		session.appendCustomEntry("note", { ok: true });
		session.flushNow();
		const path = session.getSessionFile();
		expect(path).toBeDefined();
		// Creating a fresh session is not a load.
		expect(spansNamed("session.load")).toHaveLength(0);
		return path as string;
	}

	it("open() reports the file name, its size and the parsed entry count", () => {
		const path = writeSession();
		const size = statSync(path).size;
		const outer = withSpan("outer", (span) => {
			const reopened = SessionManager.open(path);
			expect(reopened.getEntries()).toHaveLength(3);
			return span.context;
		});
		const [load] = spansNamed("session.load") as [SpanEndRecord];
		expect(load).toBeDefined();
		expect(load.status).toBe("ok");
		expect(load.parentSpanId).toBe(outer.spanId);
		expect(load.attrs).toEqual({
			"session.path": basename(path),
			"session.bytes": size,
			"session.entries": 4, // header + 3 records
		});
		// The file name only: the directory must not leak into the span.
		expect(load.attrs["session.path"]).not.toContain(tempDir);
	});

	it("openAsync() reports the same attributes in exactly one span", async () => {
		const path = writeSession();
		const size = statSync(path).size;
		const reopened = await SessionManager.openAsync(path);
		expect(reopened.getEntries()).toHaveLength(3);
		const loads = spansNamed("session.load");
		expect(loads).toHaveLength(1);
		expect(loads[0]?.attrs).toEqual({
			"session.path": basename(path),
			"session.bytes": size,
			"session.entries": 4,
		});
	});

	it("openAsync() on a missing file falls back to open() without a nested span", async () => {
		const sessionDir = join(tempDir, "sessions");
		mkdirSync(sessionDir);
		const path = join(sessionDir, "brand-new.jsonl");
		const opened = await SessionManager.openAsync(path, sessionDir, tempDir);
		expect(opened.getSessionFile()).toBe(path);
		const loads = spansNamed("session.load");
		expect(loads).toHaveLength(1);
		expect(loads[0]?.attrs).toEqual({ "session.path": "brand-new.jsonl", "session.entries": 1 });
	});
});
