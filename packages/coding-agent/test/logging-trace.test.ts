import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	currentTraceContext,
	formatTraceparent,
	getLogger,
	installDefaultSpanSink,
	type LogEntry,
	parseTraceparent,
	setLogSink,
	setSpanSink,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ENV_AGENT_DIR, getAgentLogPath } from "../src/config.js";
import { installFileLogSink, runWithLogContext, setLogContext, withInboundTraceContext } from "../src/core/logging.js";

const INBOUND = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

function readLines(): LogEntry[] {
	return readFileSync(getAgentLogPath(), "utf8")
		.split("\n")
		.filter((line) => line.trim())
		.map((line) => JSON.parse(line) as LogEntry);
}

describe("file log sink trace fields", () => {
	let agentDir = "";

	beforeEach(() => {
		agentDir = mkdtempSync(join(tmpdir(), "prime-agent-logging-trace-"));
		vi.stubEnv(ENV_AGENT_DIR, agentDir);
	});

	afterEach(() => {
		setLogSink(undefined);
		installDefaultSpanSink();
		vi.unstubAllEnvs();
		rmSync(agentDir, { recursive: true, force: true });
	});

	it("writes traceId/spanId, pid and the late-bound sessionId on every line", async () => {
		installFileLogSink({ mode: "test" });
		setLogContext({ sessionId: "session-1" });
		const log = getLogger("test");
		const context = await withSpan("agent.prompt", { "session.id": "session-1" }, async (span) => {
			log.info("inside", { detail: 1 });
			await Promise.resolve();
			log.warn("after await");
			return span.context;
		});
		log.info("outside");

		const lines = readLines();
		const inside = lines.find((l) => l.msg === "inside");
		const afterAwait = lines.find((l) => l.msg === "after await");
		const outside = lines.find((l) => l.msg === "outside");
		const spanEnd = lines.find((l) => l.component === "trace" && l.msg === "span_end");

		expect(inside).toMatchObject({
			component: "test",
			level: "info",
			detail: 1,
			pid: process.pid,
			mode: "test",
			sessionId: "session-1",
			traceId: context.traceId,
			spanId: context.spanId,
		});
		// The async storage installed by logging.ts carries the span across awaits.
		expect(afterAwait).toMatchObject({ traceId: context.traceId, spanId: context.spanId, sessionId: "session-1" });
		expect(outside).toMatchObject({ pid: process.pid, sessionId: "session-1" });
		expect(outside).not.toHaveProperty("traceId");
		expect(spanEnd).toMatchObject({
			name: "agent.prompt",
			traceId: context.traceId,
			spanId: context.spanId,
			status: "ok",
			attrs: { "session.id": "session-1" },
			sessionId: "session-1",
		});
	});

	it("never lets context fields overwrite the entry's trace ids or reserved keys", async () => {
		installFileLogSink();
		setLogContext({ traceId: "context-trace", spanId: "context-span", msg: "context-msg", level: "error" });
		const context = await withSpan("outer", async (span) => {
			getLogger("test").info("real");
			return span.context;
		});
		const real = readLines().find((l) => l.component === "test");
		expect(real).toMatchObject({ msg: "real", level: "info", traceId: context.traceId, spanId: context.spanId });
	});
});

describe("withInboundTraceContext", () => {
	const savedTraceparent = process.env.TRACEPARENT;
	const ended: string[] = [];

	beforeEach(() => {
		ended.length = 0;
		setSpanSink((record) => ended.push(`${record.name}:${record.traceId}:${record.parentSpanId ?? ""}`));
	});

	afterEach(() => {
		installDefaultSpanSink();
		if (savedTraceparent === undefined) delete process.env.TRACEPARENT;
		else process.env.TRACEPARENT = savedTraceparent;
	});

	it("parents the run to TRACEPARENT from the environment", async () => {
		process.env.TRACEPARENT = INBOUND;
		const inbound = parseTraceparent(INBOUND);
		await withInboundTraceContext(async () => {
			expect(formatTraceparent(currentTraceContext()!)).toBe(INBOUND);
			await withSpan("child", () => Promise.resolve());
		});
		expect(ended).toEqual([`child:${inbound?.traceId}:${inbound?.spanId}`]);
	});

	it("runs with no ambient context when TRACEPARENT is absent or malformed", () => {
		delete process.env.TRACEPARENT;
		expect(withInboundTraceContext(() => currentTraceContext())).toBeUndefined();
		process.env.TRACEPARENT = "00-garbage";
		expect(withInboundTraceContext(() => currentTraceContext())).toBeUndefined();
		expect(withInboundTraceContext(() => 42)).toBe(42);
	});
});

describe("scoped log context", () => {
	let agentDir = "";
	beforeEach(() => {
		agentDir = mkdtempSync(join(tmpdir(), "prime-agent-logging-scope-"));
		vi.stubEnv(ENV_AGENT_DIR, agentDir);
	});
	afterEach(() => {
		setLogSink(undefined);
		vi.unstubAllEnvs();
		rmSync(agentDir, { recursive: true, force: true });
	});

	it("scopes sessionId to the async flow so concurrent sessions in one worker never share it", async () => {
		installFileLogSink();
		const log = getLogger("test");
		const gate = new Promise<void>((resolve) => setTimeout(resolve, 5));
		await Promise.all([
			runWithLogContext({ sessionId: "session-a" }, async () => {
				log.info("a-before");
				await gate;
				log.info("a-after");
			}),
			runWithLogContext({ sessionId: "session-b" }, async () => {
				log.info("b-before");
				await gate;
				log.info("b-after");
			}),
		]);
		log.info("unscoped");
		const byMsg = new Map(readLines().map((l) => [l.msg, l]));
		expect(byMsg.get("a-before")?.sessionId).toBe("session-a");
		expect(byMsg.get("a-after")?.sessionId).toBe("session-a");
		expect(byMsg.get("b-before")?.sessionId).toBe("session-b");
		expect(byMsg.get("b-after")?.sessionId).toBe("session-b");
		expect(byMsg.get("unscoped")).not.toHaveProperty("sessionId");
	});

	it("lets a nested scope (a child session) override only inside itself", () => {
		installFileLogSink();
		const log = getLogger("test");
		runWithLogContext({ sessionId: "parent" }, () => {
			log.info("parent-1");
			runWithLogContext({ sessionId: "child" }, () => log.info("child-1"));
			log.info("parent-2");
		});
		const byMsg = new Map(readLines().map((l) => [l.msg, l]));
		expect(byMsg.get("parent-1")?.sessionId).toBe("parent");
		expect(byMsg.get("child-1")?.sessionId).toBe("child");
		expect(byMsg.get("parent-2")?.sessionId).toBe("parent");
	});
});
