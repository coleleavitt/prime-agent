import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	bindTraceContext,
	childContext,
	complete,
	currentSpan,
	currentTraceContext,
	currentTraceparent,
	fauxAssistantMessage,
	formatTraceparent,
	getLogger,
	injectTraceparentEnv,
	installDefaultSpanSink,
	type LogEntry,
	parseTraceparent,
	registerFauxProvider,
	runWithTraceContext,
	type SpanEndRecord,
	setLogSink,
	setSpanSink,
	startSpan,
	traceContextFromEnv,
	withSpan,
} from "../src/index.js";

const VALID = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

describe("traceparent parsing", () => {
	it("round-trips a valid header", () => {
		const ctx = parseTraceparent(VALID);
		expect(ctx).toEqual({ traceId: "0af7651916cd43dd8448eb211c80319c", spanId: "b7ad6b7169203331", flags: "01" });
		expect(formatTraceparent(ctx!)).toBe(VALID);
	});

	it.each([
		["wrong version", "01-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"],
		["short trace id", "00-0af7651916cd43dd8448eb211c8031-b7ad6b7169203331-01"],
		["zero trace id", "00-00000000000000000000000000000000-b7ad6b7169203331-01"],
		["zero span id", "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01"],
		["uppercase", "00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01"],
		["missing flags", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331"],
		["not a string", 42],
	])("rejects %s", (_label, value) => {
		expect(parseTraceparent(value)).toBeUndefined();
	});
});

describe("span context propagation", () => {
	const ended: SpanEndRecord[] = [];
	beforeEach(() => {
		ended.length = 0;
		setSpanSink((record) => ended.push(record));
	});
	afterEach(() => installDefaultSpanSink());

	it("has no context outside a span", () => {
		expect(currentTraceContext()).toBeUndefined();
		expect(currentTraceparent()).toBeUndefined();
	});

	it("nests child spans under the active one and restores after exit", async () => {
		await withSpan("outer", { a: 1 }, async (outer) => {
			expect(currentTraceContext()?.spanId).toBe(outer.context.spanId);
			expect(outer.context.parentSpanId).toBeUndefined();
			await withSpan("inner", (inner) => {
				expect(inner.context.traceId).toBe(outer.context.traceId);
				expect(inner.context.parentSpanId).toBe(outer.context.spanId);
				expect(inner.context.spanId).not.toBe(outer.context.spanId);
			});
			expect(currentTraceContext()?.spanId).toBe(outer.context.spanId);
		});
		expect(currentTraceContext()).toBeUndefined();
		expect(ended.map((r) => r.name)).toEqual(["inner", "outer"]);
		expect(ended[1]?.attrs).toEqual({ a: 1 });
		expect(ended.every((r) => r.status === "ok")).toBe(true);
	});

	it("records errors and rethrows for sync and async bodies", async () => {
		expect(() =>
			withSpan("sync", () => {
				throw new Error("boom");
			}),
		).toThrow("boom");
		await expect(
			withSpan("async", async () => {
				throw new Error("later");
			}),
		).rejects.toThrow("later");
		expect(ended.map((r) => [r.name, r.status, r.error])).toEqual([
			["sync", "error", "boom"],
			["async", "error", "later"],
		]);
	});

	it("inherits an explicit parent context via runWithTraceContext", () => {
		const parent = parseTraceparent(VALID)!;
		runWithTraceContext(parent, () => {
			expect(currentTraceparent()).toBe(VALID);
			const span = startSpan("child");
			expect(span.context.traceId).toBe(parent.traceId);
			expect(span.context.parentSpanId).toBe(parent.spanId);
			span.end();
		});
		expect(currentTraceContext()).toBeUndefined();
	});

	it("binds callbacks to the context active at bind time", async () => {
		let seen: string | undefined;
		const bound = withSpan("owner", () =>
			bindTraceContext(() => {
				seen = currentTraceparent();
			}),
		);
		bound();
		expect(seen).toBeDefined();
		expect(currentTraceparent()).toBeUndefined();
	});

	it("ends a span once even if end() is called twice", () => {
		const span = startSpan("once");
		span.end();
		span.end("error");
		expect(ended).toHaveLength(1);
		expect(ended[0]?.status).toBe("ok");
	});

	it("reads and injects TRACEPARENT through the environment", () => {
		expect(traceContextFromEnv({ TRACEPARENT: VALID })).toEqual(parseTraceparent(VALID));
		expect(traceContextFromEnv({})).toBeUndefined();
		expect(injectTraceparentEnv({ PATH: "/bin" })).toEqual({ PATH: "/bin" });
		runWithTraceContext(childContext(undefined), () => {
			const env: Record<string, string | undefined> = injectTraceparentEnv({ PATH: "/bin" });
			expect(env.TRACEPARENT).toBe(currentTraceparent());
		});
	});
});

describe("log stamping", () => {
	const entries: LogEntry[] = [];
	beforeEach(() => {
		entries.length = 0;
		setLogSink((entry) => entries.push(entry));
	});
	afterEach(() => {
		setLogSink(undefined);
		installDefaultSpanSink();
	});

	it("stamps traceId/spanId/parentSpanId on entries emitted inside a span", async () => {
		const log = getLogger("test");
		log.info("outside");
		await withSpan("outer", async (outer) => {
			log.info("in outer");
			await withSpan("inner", (inner) => {
				log.warn("in inner", { extra: true });
				expect(entries.at(-1)).toMatchObject({
					msg: "in inner",
					extra: true,
					traceId: inner.context.traceId,
					spanId: inner.context.spanId,
					parentSpanId: outer.context.spanId,
				});
			});
		});
		const outside = entries.find((entry) => entry.msg === "outside");
		const inOuter = entries.find((entry) => entry.msg === "in outer");
		expect(outside).not.toHaveProperty("traceId");
		expect(inOuter).toHaveProperty("traceId");
		expect(inOuter).not.toHaveProperty("parentSpanId");
	});

	it("reports span starts and ends as structured trace entries by default", async () => {
		await withSpan("agent.prompt", { k: "v" }, async () => {});
		const start = entries.find((e) => e.component === "trace" && e.msg === "span_start");
		const end = entries.find((e) => e.component === "trace" && e.msg === "span_end");
		expect(start).toMatchObject({ level: "info", name: "agent.prompt", attrs: { k: "v" } });
		expect(end).toMatchObject({ level: "info", name: "agent.prompt", status: "ok", attrs: { k: "v" } });
		expect(start?.traceId).toBe(end?.traceId);
		expect(start?.spanId).toBe(end?.spanId);
		expect(typeof end?.durationMs).toBe("number");
	});

	it("persists starts for bounded child lifecycle operations", async () => {
		await withSpan("child.passivate", () => Promise.resolve());
		await withSpan("child.delete", () => Promise.resolve());
		expect(entries.filter((entry) => entry.msg === "span_start").map((entry) => entry.name)).toEqual([
			"child.passivate",
			"child.delete",
		]);
	});

	it("does not persist starts for high-volume spans", async () => {
		await withSpan("llm.request", () => Promise.resolve());
		await withSpan("tool.execute", () => Promise.resolve());
		await withSpan("extension.hooks", () => Promise.resolve());
		expect(entries.filter((entry) => entry.msg === "span_start")).toEqual([]);
	});

	it("allows callers to suppress a span end after the operation runs", () => {
		withSpan("extension.hooks", (span) => span.setReportingEnabled(false));
		expect(entries.filter((entry) => entry.msg === "span_end")).toEqual([]);
	});

	it("wraps provider calls in an llm.request span carrying base_url and stop reason", async () => {
		const registration = registerFauxProvider();
		try {
			registration.setResponses([fauxAssistantMessage("hello")]);
			const model = registration.getModel();
			await withSpan("agent.turn", async (turn) => {
				await complete(model, { messages: [{ role: "user", content: "hi", timestamp: Date.now() }] });
				const end = entries.find((e) => e.msg === "span_end" && e.name === "llm.request");
				expect(end).toMatchObject({
					traceId: turn.context.traceId,
					parentSpanId: turn.context.spanId,
					status: "ok",
					attrs: {
						"llm.provider": model.provider,
						"llm.api": model.api,
						"llm.model": model.id,
						"llm.base_url": model.baseUrl,
						"llm.stop_reason": "stop",
					},
				});
			});
		} finally {
			registration.unregister();
		}
	});
});

describe("currentSpan", () => {
	const ended: SpanEndRecord[] = [];
	beforeEach(() => {
		ended.length = 0;
		setSpanSink((record) => ended.push(record));
	});
	afterEach(() => setSpanSink(undefined));

	it("returns the active span so a handler that swallows an error can still fail it", async () => {
		await withSpan("daemon.command", async () => {
			try {
				throw new Error("Worker authentication failed");
			} catch (error) {
				currentSpan()?.recordError(error);
				// converted to a failure envelope; nothing re-thrown
			}
		});
		expect(ended).toHaveLength(1);
		expect(ended[0]?.status).toBe("error");
		expect(ended[0]?.error).toBe("Worker authentication failed");
	});

	it("is undefined outside any span and for a context that was only adopted", () => {
		expect(currentSpan()).toBeUndefined();
		runWithTraceContext(parseTraceparent(VALID), () => {
			expect(currentTraceContext()).toBeDefined();
			expect(currentSpan()).toBeUndefined();
		});
	});

	it("resolves to the innermost span", () => {
		withSpan("outer", (outer) => {
			expect(currentSpan()).toBe(outer);
			withSpan("inner", (inner) => expect(currentSpan()).toBe(inner));
			expect(currentSpan()).toBe(outer);
		});
	});
});
