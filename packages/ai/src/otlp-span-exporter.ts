import type { SpanAttributeValue, SpanEndRecord, SpanSink } from "./trace-context.js";

const DEFAULT_BATCH_SIZE = 128;
const DEFAULT_MAX_QUEUE_SIZE = 2048;
const DEFAULT_FLUSH_INTERVAL_MS = 5_000;
const DEFAULT_MAX_METRIC_SERIES = 256;
const DEFAULT_REQUEST_TIMEOUT_MS = 10_000;
const MAX_ATTRIBUTE_CHARS = 1_024;
const SENSITIVE_ATTRIBUTE_KEY =
	/(?:authorization|api[-_.]?key|access[-_.]?token|refresh[-_.]?token|secret|password|cookie)/i;

interface OtlpAnyValue {
	stringValue?: string;
	intValue?: string;
	doubleValue?: number;
	boolValue?: boolean;
}

export interface OtlpSpanExporterOptions {
	/** OTLP/HTTP base endpoint. `/v1/traces` and `/v1/metrics` are appended. */
	endpoint: string;
	headers?: Record<string, string>;
	serviceName?: string;
	serviceVersion?: string;
	batchSize?: number;
	maxQueueSize?: number;
	flushIntervalMs?: number;
	maxMetricSeries?: number;
	requestTimeoutMs?: number;
	fetch?: typeof fetch;
}

export interface SpanMetricSnapshot {
	name: string;
	count: number;
	errorCount: number;
	durationMs: number;
}

export interface OtlpSpanExporter {
	readonly sink: SpanSink;
	flush(): Promise<void>;
	shutdown(): Promise<void>;
	/** Abort pending requests after an external shutdown deadline. */
	abort(): void;
	metrics(): SpanMetricSnapshot[];
	stats(): { queued: number; dropped: number; exportErrors: number };
}

interface QueuedSpan {
	record: SpanEndRecord;
	endTimeUnixNano: string;
}

function positiveInteger(value: number | undefined, fallback: number): number {
	return value !== undefined && Number.isFinite(value) && value > 0 ? Math.floor(value) : fallback;
}

function endpoint(base: string, signal: "traces" | "metrics"): string {
	return `${base.replace(/\/+$/, "")}/v1/${signal}`;
}

function anyValue(value: SpanAttributeValue): OtlpAnyValue {
	if (typeof value === "boolean") return { boolValue: value };
	if (typeof value === "number") {
		return Number.isInteger(value) ? { intValue: String(value) } : { doubleValue: value };
	}
	return { stringValue: value ?? "" };
}

function attributes(values: Record<string, SpanAttributeValue>): Array<{ key: string; value: OtlpAnyValue }> {
	return Object.entries(values)
		.filter(
			(entry): entry is [string, Exclude<SpanAttributeValue, undefined>] =>
				entry[1] !== undefined && !SENSITIVE_ATTRIBUTE_KEY.test(entry[0]),
		)
		.map(([key, value]) => ({
			key,
			value: anyValue(typeof value === "string" ? value.slice(0, MAX_ATTRIBUTE_CHARS) : value),
		}));
}

function unixNano(ms: number): string {
	return String(BigInt(Math.max(0, Math.round(ms * 1_000_000))));
}

function resourceAttributes(options: OtlpSpanExporterOptions): Array<{ key: string; value: OtlpAnyValue }> {
	const attrs: Record<string, SpanAttributeValue> = {
		"service.name": options.serviceName ?? "prime-agent",
		"service.version": options.serviceVersion,
	};
	return attributes(attrs);
}

/**
 * Dependency-free OTLP/HTTP JSON adapter for the process-wide span sink.
 * It is inert until explicitly created and attached with `addSpanSink`.
 */
export function createOtlpSpanExporter(options: OtlpSpanExporterOptions): OtlpSpanExporter {
	const batchSize = positiveInteger(options.batchSize, DEFAULT_BATCH_SIZE);
	const maxQueueSize = positiveInteger(options.maxQueueSize, DEFAULT_MAX_QUEUE_SIZE);
	const flushIntervalMs = positiveInteger(options.flushIntervalMs, DEFAULT_FLUSH_INTERVAL_MS);
	const maxMetricSeries = positiveInteger(options.maxMetricSeries, DEFAULT_MAX_METRIC_SERIES);
	const requestTimeoutMs = positiveInteger(options.requestTimeoutMs, DEFAULT_REQUEST_TIMEOUT_MS);
	const send = options.fetch ?? globalThis.fetch;
	const queue: QueuedSpan[] = [];
	const metricSeries = new Map<string, SpanMetricSnapshot>();
	let dropped = 0;
	let exportErrors = 0;
	let stopped = false;
	let flushing: Promise<void> | undefined;
	let shutdownPromise: Promise<void> | undefined;
	const requests = new Set<AbortController>();

	const timer = setInterval(() => void flush(), flushIntervalMs);
	timer.unref?.();

	function recordMetric(record: SpanEndRecord): void {
		let metric = metricSeries.get(record.name);
		if (!metric) {
			if (metricSeries.size >= maxMetricSeries) return;
			metric = { name: record.name, count: 0, errorCount: 0, durationMs: 0 };
			metricSeries.set(record.name, metric);
		}
		metric.count += 1;
		metric.errorCount += record.status === "error" ? 1 : 0;
		metric.durationMs += record.durationMs;
	}

	const sink: SpanSink = (record) => {
		if (stopped) return;
		recordMetric(record);
		if (queue.length >= maxQueueSize) {
			queue.shift();
			dropped += 1;
		}
		queue.push({ record, endTimeUnixNano: unixNano(Date.now()) });
		if (queue.length >= batchSize) void flush();
	};

	function traceBody(batch: QueuedSpan[]): unknown {
		return {
			resourceSpans: [
				{
					resource: { attributes: resourceAttributes(options) },
					scopeSpans: [
						{
							scope: { name: "@earendil-works/pi-ai" },
							spans: batch.map(({ record, endTimeUnixNano }) => ({
								traceId: record.traceId,
								spanId: record.spanId,
								parentSpanId: record.parentSpanId,
								name: record.name,
								kind: 1,
								startTimeUnixNano: String(
									BigInt(endTimeUnixNano) - BigInt(Math.round(record.durationMs * 1_000_000)),
								),
								endTimeUnixNano,
								attributes: attributes(record.attrs),
								status: { code: record.status === "error" ? 2 : 1, message: record.error },
							})),
						},
					],
				},
			],
		};
	}

	function metricBody(metrics: SpanMetricSnapshot[], startTimeUnixNano: string, timeUnixNano: string): unknown {
		const dataPoints = (field: "count" | "errorCount" | "durationMs") =>
			metrics.map((metric) => ({
				attributes: attributes({ "span.name": metric.name }),
				startTimeUnixNano,
				timeUnixNano,
				...(field === "durationMs" ? { asDouble: metric[field] } : { asInt: String(metric[field]) }),
			}));
		return {
			resourceMetrics: [
				{
					resource: { attributes: resourceAttributes(options) },
					scopeMetrics: [
						{
							scope: { name: "@earendil-works/pi-ai" },
							metrics: [
								{
									name: "prime_agent.span.count",
									sum: { aggregationTemporality: 1, isMonotonic: true, dataPoints: dataPoints("count") },
								},
								{
									name: "prime_agent.span.error_count",
									sum: { aggregationTemporality: 1, isMonotonic: true, dataPoints: dataPoints("errorCount") },
								},
								{
									name: "prime_agent.span.duration_ms",
									sum: { aggregationTemporality: 1, isMonotonic: true, dataPoints: dataPoints("durationMs") },
								},
							],
						},
					],
				},
			],
		};
	}

	async function post(url: string, body: unknown): Promise<void> {
		const controller = new AbortController();
		requests.add(controller);
		const timeout = setTimeout(() => controller.abort(), requestTimeoutMs);
		timeout.unref?.();
		try {
			const response = await send(url, {
				method: "POST",
				headers: { "content-type": "application/json", ...options.headers },
				body: JSON.stringify(body),
				signal: controller.signal,
			});
			if (!response.ok) exportErrors += 1;
		} catch {
			exportErrors += 1;
		} finally {
			clearTimeout(timeout);
			requests.delete(controller);
		}
	}

	async function drain(): Promise<void> {
		while (queue.length > 0) {
			const batch = queue.splice(0, batchSize);
			const metrics = [...metricSeries.values()].map((metric) => ({ ...metric }));
			metricSeries.clear();
			const started = unixNano(Date.now() - Math.max(...batch.map(({ record }) => record.durationMs), 0));
			await Promise.all([
				post(endpoint(options.endpoint, "traces"), traceBody(batch)),
				post(endpoint(options.endpoint, "metrics"), metricBody(metrics, started, unixNano(Date.now()))),
			]);
		}
	}

	function flush(): Promise<void> {
		if (!flushing) {
			flushing = drain().finally(() => {
				flushing = undefined;
				if (queue.length >= batchSize) void flush();
			});
		}
		return flushing;
	}

	function abort(): void {
		for (const controller of requests) controller.abort();
	}

	function shutdown(): Promise<void> {
		shutdownPromise ??= (async () => {
			stopped = true;
			clearInterval(timer);
			while (flushing || queue.length > 0) {
				if (flushing) await flushing;
				else await flush();
			}
		})();
		return shutdownPromise;
	}

	return {
		sink,
		flush,
		shutdown,
		abort,
		metrics: () => [...metricSeries.values()].map((metric) => ({ ...metric })),
		stats: () => ({ queued: queue.length, dropped, exportErrors }),
	};
}
