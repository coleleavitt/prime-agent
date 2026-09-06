import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { installDefaultSpanSink, runWithTraceContext, setSpanSink, withSpan } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadEntriesFromFile, type SessionEntry, SessionManager } from "../src/core/session-manager.js";

const tempDirs: string[] = [];

function createTempDir(): string {
	const dir = mkdtempSync(join(tmpdir(), "session-manager-trace-"));
	tempDirs.push(dir);
	return dir;
}

function assistantMessage(text: string) {
	return {
		role: "assistant" as const,
		content: [{ type: "text" as const, text }],
		api: "anthropic-messages" as const,
		provider: "anthropic",
		model: "test",
		usage: {
			input: 1,
			output: 1,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 2,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop" as const,
		timestamp: 2,
	};
}

describe("session record trace stamping", () => {
	beforeEach(() => {
		// Spans opened here must not leak span_end log lines into the shared log.
		setSpanSink(undefined);
	});
	afterEach(() => {
		installDefaultSpanSink();
		while (tempDirs.length > 0) {
			rmSync(tempDirs.pop()!, { recursive: true, force: true });
		}
	});

	it("stamps traceId and spanId from the active span onto every appended record", () => {
		const session = SessionManager.inMemory();
		const ids = withSpan("agent.turn", (span) => {
			session.appendMessage({ role: "user", content: "hello", timestamp: 1 });
			session.appendCustomEntry("my_data", { foo: "bar" });
			return span.context;
		});
		for (const entry of session.getEntries()) {
			expect(entry.traceId).toBe(ids.traceId);
			expect(entry.spanId).toBe(ids.spanId);
			expect("parentSpanId" in entry).toBe(false);
		}
	});

	it("follows the innermost span when spans nest", () => {
		const session = SessionManager.inMemory();
		const seen: Array<{ entryId: string; spanId: string }> = [];
		withSpan("outer", (outer) => {
			seen.push({
				entryId: session.appendMessage({ role: "user", content: "a", timestamp: 1 }),
				spanId: outer.context.spanId,
			});
			withSpan("inner", (inner) => {
				seen.push({ entryId: session.appendCustomEntry("x", {}), spanId: inner.context.spanId });
			});
			seen.push({ entryId: session.appendCustomEntry("y", {}), spanId: outer.context.spanId });
		});
		const byId = new Map(session.getEntries().map((entry) => [entry.id, entry]));
		for (const { entryId, spanId } of seen) {
			expect(byId.get(entryId)?.spanId).toBe(spanId);
		}
		const traceIds = new Set(session.getEntries().map((entry) => entry.traceId));
		expect(traceIds.size).toBe(1);
	});

	it("writes nothing trace-related when no span is active", () => {
		const session = SessionManager.inMemory();
		runWithTraceContext(undefined, () => {
			session.appendMessage({ role: "user", content: "hello", timestamp: 1 });
			session.appendCustomEntry("my_data", { foo: "bar" });
		});
		for (const entry of session.getEntries()) {
			expect("traceId" in entry).toBe(false);
			expect("spanId" in entry).toBe(false);
		}
	});

	it("persists the ids to disk and reads them back on reopen", () => {
		const dir = createTempDir();
		const session = SessionManager.create(dir, dir);
		const context = withSpan("agent.turn", (span) => {
			session.appendMessage({ role: "user", content: "hello", timestamp: 1 });
			session.appendMessage(assistantMessage("hi"));
			return span.context;
		});
		// Appended outside any span after the file exists: must not inherit ids.
		session.appendCustomEntry("after", {});
		session.flushNow();
		const file = session.getSessionFile()!;
		expect(existsSync(file)).toBe(true);

		const onDisk = loadEntriesFromFile(file).filter((entry) => entry.type !== "session") as SessionEntry[];
		expect(onDisk.map((entry) => entry.traceId)).toEqual([context.traceId, context.traceId, undefined]);
		expect(onDisk.map((entry) => entry.spanId)).toEqual([context.spanId, context.spanId, undefined]);
		// The join key is plain JSON on the line, so `grep <traceId>` finds the session.
		expect(readFileSync(file, "utf8")).toContain(`"traceId":"${context.traceId}"`);

		const reopened = SessionManager.open(file, dir);
		expect(reopened.getEntries().map((entry) => entry.traceId)).toEqual([
			context.traceId,
			context.traceId,
			undefined,
		]);
	});
});
