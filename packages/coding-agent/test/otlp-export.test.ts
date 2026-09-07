import { type OtlpSpanExporter, type SpanEndRecord, startSpan } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
	DEFAULT_OTLP_SHUTDOWN_TIMEOUT_MS,
	installOtlpExporterFromEnv,
	parseOtlpHeaders,
} from "../src/core/otlp-export.js";

function fakeExporter(overrides: Partial<OtlpSpanExporter> = {}): OtlpSpanExporter & { spans: SpanEndRecord[] } {
	const spans: SpanEndRecord[] = [];
	return {
		sink: (span) => spans.push(span),
		flush: async () => {},
		shutdown: async () => {},
		abort: () => {},
		metrics: () => [],
		stats: () => ({ queued: 0, dropped: 0, exportErrors: 0 }),
		spans,
		...overrides,
	};
}

afterEach(() => vi.useRealTimers());

describe("coding-agent OTLP setup", () => {
	it("does nothing when the standard endpoint environment variable is absent", () => {
		const createExporter = vi.fn();
		expect(installOtlpExporterFromEnv({ version: "1.2.3", env: {}, createExporter })).toBeUndefined();
		expect(createExporter).not.toHaveBeenCalled();
	});

	it("installs an additive exporter with service identity and parsed standard headers", async () => {
		const exporter = fakeExporter();
		const createExporter = vi.fn(() => exporter);
		const installed = installOtlpExporterFromEnv({
			version: "1.2.3",
			env: {
				OTEL_EXPORTER_OTLP_ENDPOINT: " https://collector.example/otlp ",
				OTEL_EXPORTER_OTLP_HEADERS: "authorization=Bearer token, x-tenant = acme,invalid",
			},
			createExporter,
		});

		expect(createExporter).toHaveBeenCalledWith({
			endpoint: "https://collector.example/otlp",
			headers: { authorization: "Bearer token", "x-tenant": "acme" },
			serviceName: "prime-agent",
			serviceVersion: "1.2.3",
		});
		startSpan("exported").end();
		expect(exporter.spans.map((span) => span.name)).toContain("exported");
		await installed?.shutdown();
		startSpan("after-shutdown").end();
		expect(exporter.spans.map((span) => span.name)).not.toContain("after-shutdown");
	});

	it("bounds orderly shutdown when the collector never settles", async () => {
		vi.useFakeTimers();
		const exporter = fakeExporter({ shutdown: () => new Promise(() => {}) });
		const installed = installOtlpExporterFromEnv({
			version: "1.2.3",
			env: { OTEL_EXPORTER_OTLP_ENDPOINT: "http://collector" },
			createExporter: () => exporter,
		});
		let finished = false;
		const shutdown = installed?.shutdown().then(() => {
			finished = true;
		});
		await vi.advanceTimersByTimeAsync(DEFAULT_OTLP_SHUTDOWN_TIMEOUT_MS - 1);
		expect(finished).toBe(false);
		await vi.advanceTimersByTimeAsync(1);
		await shutdown;
		expect(finished).toBe(true);
	});
});

describe("parseOtlpHeaders", () => {
	it("keeps equals signs in values and ignores malformed entries", () => {
		expect(parseOtlpHeaders("authorization=Basic%20a=b,broken, =empty")).toEqual({ authorization: "Basic a=b" });
		expect(parseOtlpHeaders("  ")).toBeUndefined();
	});
});
