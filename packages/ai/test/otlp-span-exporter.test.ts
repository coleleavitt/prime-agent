import { afterEach, describe, expect, it, vi } from "vitest";
import {
	addSpanSink,
	createOtlpSpanExporter,
	installDefaultSpanSink,
	type SpanEndRecord,
	setSpanSink,
	startSpan,
} from "../src/index.js";

function record(name: string, status: "ok" | "error" = "ok"): SpanEndRecord {
	return {
		name,
		traceId: "0af7651916cd43dd8448eb211c80319c",
		spanId: name.padEnd(16, "0").slice(0, 16),
		durationMs: 12.5,
		status,
		attrs: { model: "test", attempts: 2, cached: false },
		error: status === "error" ? "failed" : undefined,
	};
}

function response(): Response {
	return new Response(null, { status: 200 });
}

afterEach(() => {
	vi.useRealTimers();
	installDefaultSpanSink();
});

describe("OTLP span exporter", () => {
	it("is additive, batches OTLP JSON, and derives bounded span metrics", async () => {
		const primary: SpanEndRecord[] = [];
		const requests: Array<{ url: string; body: Record<string, unknown> }> = [];
		setSpanSink((span) => primary.push(span));
		const exporter = createOtlpSpanExporter({
			endpoint: "https://collector.example/otlp/",
			batchSize: 2,
			maxMetricSeries: 1,
			flushIntervalMs: 60_000,
			fetch: async (input, init) => {
				requests.push({ url: String(input), body: JSON.parse(String(init?.body)) as Record<string, unknown> });
				return response();
			},
		});
		const unsubscribe = addSpanSink(exporter.sink);

		exporter.sink(record("llm.request"));
		exporter.sink(record("llm.request", "error"));
		exporter.sink(record("ignored.series"));
		await exporter.flush();

		expect(primary).toEqual([]);
		const span = startSpan("primary");
		span.end();
		expect(primary).toHaveLength(1);
		expect(requests.map((request) => request.url)).toContain("https://collector.example/otlp/v1/traces");
		expect(requests.map((request) => request.url)).toContain("https://collector.example/otlp/v1/metrics");
		const traceRequest = requests.find((request) => request.url.endsWith("/v1/traces"));
		expect(traceRequest?.body).toMatchObject({
			resourceSpans: [{ scopeSpans: [{ spans: [{ name: "llm.request" }, { status: { code: 2 } }] }] }],
		});

		unsubscribe();
		await exporter.shutdown();
	});

	it("drops the oldest item at the queue bound and flushes the remainder on shutdown", async () => {
		const traceNames: string[][] = [];
		const exporter = createOtlpSpanExporter({
			endpoint: "http://collector",
			batchSize: 10,
			maxQueueSize: 2,
			flushIntervalMs: 60_000,
			fetch: async (input, init) => {
				if (String(input).endsWith("/v1/traces")) {
					const body = JSON.parse(String(init?.body)) as {
						resourceSpans: Array<{ scopeSpans: Array<{ spans: Array<{ name: string }> }> }>;
					};
					traceNames.push(body.resourceSpans[0]!.scopeSpans[0]!.spans.map((span) => span.name));
				}
				return response();
			},
		});

		exporter.sink(record("one"));
		exporter.sink(record("two"));
		exporter.sink(record("three"));
		expect(exporter.stats()).toEqual({ queued: 2, dropped: 1, exportErrors: 0 });
		await exporter.shutdown();
		expect(traceNames).toEqual([["two", "three"]]);
		expect(exporter.stats().queued).toBe(0);
	});

	it("force-flushes spans queued while an earlier batch is in flight", async () => {
		let releaseFirst!: () => void;
		const first = new Promise<void>((resolve) => {
			releaseFirst = resolve;
		});
		const traceNames: string[][] = [];
		let calls = 0;
		const exporter = createOtlpSpanExporter({
			endpoint: "http://collector",
			batchSize: 1,
			flushIntervalMs: 60_000,
			fetch: async (input, init) => {
				if (String(input).endsWith("/v1/traces")) {
					const body = JSON.parse(String(init?.body)) as {
						resourceSpans: Array<{ scopeSpans: Array<{ spans: Array<{ name: string }> }> }>;
					};
					traceNames.push(body.resourceSpans[0]!.scopeSpans[0]!.spans.map((span) => span.name));
				}
				if (calls++ < 2) await first;
				return response();
			},
		});
		exporter.sink(record("first"));
		exporter.sink(record("second"));
		const flushed = exporter.flush();
		releaseFirst();
		await flushed;
		expect(traceNames).toEqual([["first"], ["second"]]);
		await exporter.shutdown();
	});

	it("makes shutdown idempotent and reports rejected exports", async () => {
		const exporter = createOtlpSpanExporter({
			endpoint: "http://collector",
			fetch: async () => new Response(null, { status: 500 }),
		});
		exporter.sink(record("failed-export"));
		const first = exporter.shutdown();
		const second = exporter.shutdown();
		expect(second).toBe(first);
		await first;
		expect(exporter.stats().exportErrors).toBe(2);
	});

	it("redacts sensitive attributes and snapshots span records at end", async () => {
		let body: string | undefined;
		const exporter = createOtlpSpanExporter({
			endpoint: "http://collector",
			fetch: async (input, init) => {
				if (String(input).endsWith("/v1/traces")) body = String(init?.body);
				return response();
			},
		});
		const unsubscribe = addSpanSink(exporter.sink);
		const span = startSpan("safe", { authorization: "secret", ordinary: "kept" });
		span.end();
		span.setAttributes({ ordinary: "mutated" });
		await exporter.flush();
		expect(body).not.toContain("secret");
		expect(body).not.toContain("mutated");
		expect(body).toContain("kept");
		unsubscribe();
		await exporter.shutdown();
	});

	it("swallows transport failures and flushes on the interval", async () => {
		vi.useFakeTimers();
		const send = vi.fn(async () => {
			throw new Error("offline");
		});
		const exporter = createOtlpSpanExporter({
			endpoint: "http://collector",
			flushIntervalMs: 25,
			fetch: send,
		});
		exporter.sink(record("timer"));
		await vi.advanceTimersByTimeAsync(25);
		expect(send).toHaveBeenCalledTimes(2);
		expect(exporter.stats().queued).toBe(0);
		await exporter.shutdown();
	});
});
