/**
 * Dependency-free trace context shared by pi-ai and its consumers.
 *
 * One W3C `traceparent` (`00-<traceId>-<spanId>-<flags>`) identifies a user
 * turn across processes; every hop opens a child span. The API surface
 * deliberately mirrors OpenTelemetry (start/end, attributes, status) so an
 * OTel SDK can be bridged in later without touching call sites, but nothing
 * here depends on `@opentelemetry/*`.
 *
 * Span completion is reported through the shared structured logger as a
 * `component: "trace"` / `msg: "span_end"` entry, and every log entry emitted
 * while a span is active is stamped with `traceId`/`spanId`/`parentSpanId`
 * (see log.ts). Tracing must never throw into the caller.
 */

export const TRACEPARENT_ENV = "TRACEPARENT";
export const TRACE_LOG_COMPONENT = "trace";
export const SPAN_START_MSG = "span_start";
export const SPAN_END_MSG = "span_end";

export interface TraceContext {
	/** 32 lowercase hex chars, never all zeros. */
	traceId: string;
	/** 16 lowercase hex chars, never all zeros. */
	spanId: string;
	/** 2 lowercase hex chars; `01` = sampled. */
	flags: string;
	parentSpanId?: string;
}

export type SpanStatus = "ok" | "error";
export type SpanAttributeValue = string | number | boolean | undefined;
export type SpanAttributes = Record<string, SpanAttributeValue>;

export interface SpanEndRecord {
	name: string;
	traceId: string;
	spanId: string;
	parentSpanId?: string;
	durationMs: number;
	status: SpanStatus;
	attrs: SpanAttributes;
	error?: string;
}

export interface Span {
	readonly name: string;
	readonly context: TraceContext;
	readonly attrs: SpanAttributes;
	/** Merge attributes (undefined values are dropped). */
	setAttributes(attrs: SpanAttributes): void;
	/** Mark the span failed; `end()` will report `status: "error"`. */
	recordError(error: unknown): void;
	/** Enable or suppress the eventual span-end report. */
	setReportingEnabled(enabled: boolean): void;
	/** Idempotent. */
	end(status?: SpanStatus): void;
}

const TRACE_ID_RE = /^[0-9a-f]{32}$/;
const SPAN_ID_RE = /^[0-9a-f]{16}$/;
const FLAGS_RE = /^[0-9a-f]{2}$/;
const ZERO_TRACE_ID = "0".repeat(32);
const ZERO_SPAN_ID = "0".repeat(16);

/**
 * Minimal subset of `AsyncLocalStorage` so pi-ai stays browser-safe (no
 * `node:` imports at module top level). The default implementation is a
 * synchronous stack: correct for nested `withSpan` bodies, but an `await`
 * boundary loses the context. Node/Bun hosts swap in an AsyncLocalStorage
 * (see `installAsyncTraceContextStorage`, which coding-agent calls at
 * startup, and the best-effort auto-install below).
 */
export interface TraceContextStorage {
	getStore(): TraceContext | undefined;
	run<T>(context: TraceContext, fn: () => T): T;
	exit<T>(fn: () => T): T;
}

class SyncStackStorage implements TraceContextStorage {
	private readonly stack: Array<TraceContext | undefined> = [];
	getStore(): TraceContext | undefined {
		return this.stack.length > 0 ? this.stack[this.stack.length - 1] : undefined;
	}
	run<T>(context: TraceContext, fn: () => T): T {
		this.stack.push(context);
		try {
			return fn();
		} finally {
			this.stack.pop();
		}
	}
	exit<T>(fn: () => T): T {
		this.stack.push(undefined);
		try {
			return fn();
		} finally {
			this.stack.pop();
		}
	}
}

let storage: TraceContextStorage = new SyncStackStorage();
let asyncStorageInstalled = false;

/**
 * Replace the context storage (idempotent when an async storage is already
 * installed). Hosts pass `new AsyncLocalStorage<TraceContext>()`.
 */
export function installAsyncTraceContextStorage(next: TraceContextStorage): void {
	if (asyncStorageInstalled) return;
	storage = next;
	asyncStorageInstalled = true;
}

export function isAsyncTraceContextStorageInstalled(): boolean {
	return asyncStorageInstalled;
}

// NEVER convert to a top-level runtime import - breaks browser/Vite builds.
const NODE_ASYNC_HOOKS_SPECIFIER = "node:" + "async_hooks";
if (typeof process !== "undefined" && (process.versions?.node || process.versions?.bun)) {
	(import(NODE_ASYNC_HOOKS_SPECIFIER) as Promise<{ AsyncLocalStorage: new () => TraceContextStorage }>)
		.then((m) => installAsyncTraceContextStorage(new m.AsyncLocalStorage()))
		.catch(() => {
			// Keep the synchronous fallback.
		});
}

function randomHexBytes(bytes: number): string {
	const buffer = new Uint8Array(bytes);
	const webCrypto = (globalThis as { crypto?: { getRandomValues?: (array: Uint8Array) => Uint8Array } }).crypto;
	if (webCrypto?.getRandomValues) {
		webCrypto.getRandomValues(buffer);
	} else {
		for (let i = 0; i < bytes; i++) buffer[i] = Math.floor(Math.random() * 256);
	}
	let out = "";
	for (const byte of buffer) out += byte.toString(16).padStart(2, "0");
	return out;
}

function randomHex(bytes: number, zero: string): string {
	for (let attempt = 0; attempt < 8; attempt++) {
		const value = randomHexBytes(bytes);
		if (value !== zero) return value;
	}
	// Astronomically unlikely; keep the invariant (never all zeros) regardless.
	return `${"0".repeat(zero.length - 1)}1`;
}

export function newTraceId(): string {
	return randomHex(16, ZERO_TRACE_ID);
}

export function newSpanId(): string {
	return randomHex(8, ZERO_SPAN_ID);
}

/** Strict W3C parse: version 00, lowercase hex, non-zero ids. Returns undefined instead of throwing. */
export function parseTraceparent(value: unknown): TraceContext | undefined {
	if (typeof value !== "string") return undefined;
	const parts = value.trim().split("-");
	if (parts.length !== 4) return undefined;
	const [version, traceId, spanId, flags] = parts as [string, string, string, string];
	if (version !== "00") return undefined;
	if (!TRACE_ID_RE.test(traceId) || traceId === ZERO_TRACE_ID) return undefined;
	if (!SPAN_ID_RE.test(spanId) || spanId === ZERO_SPAN_ID) return undefined;
	if (!FLAGS_RE.test(flags)) return undefined;
	return { traceId, spanId, flags };
}

export function formatTraceparent(context: TraceContext): string {
	return `00-${context.traceId}-${context.spanId}-${context.flags}`;
}

/** The context of the innermost active span, if any. */
export function currentTraceContext(): TraceContext | undefined {
	return storage.getStore();
}

/** `traceparent` for the active span, for injection into frames/env/headers. */
export function currentTraceparent(): string | undefined {
	const context = storage.getStore();
	return context ? formatTraceparent(context) : undefined;
}

/** Mint a child of `parent` (or a brand-new root when `parent` is undefined). */
export function childContext(parent: TraceContext | undefined): TraceContext {
	if (!parent) {
		return { traceId: newTraceId(), spanId: newSpanId(), flags: "01" };
	}
	return { traceId: parent.traceId, spanId: newSpanId(), flags: parent.flags, parentSpanId: parent.spanId };
}

/** Run `fn` with `context` as the active trace context (no span is opened or reported). */
export function runWithTraceContext<T>(context: TraceContext | undefined, fn: () => T): T {
	return context ? storage.run(context, fn) : storage.exit(fn);
}

/** Bind a callback so it later executes under the trace context active now. */
export function bindTraceContext<T extends (...args: never[]) => unknown>(fn: T): T {
	const context = storage.getStore();
	if (!context) return fn;
	return ((...args: Parameters<T>) => storage.run(context, () => fn(...args))) as T;
}

/** Read an inbound context from `env.TRACEPARENT` (used once at process start). */
export function traceContextFromEnv(env: NodeJS.ProcessEnv = process.env): TraceContext | undefined {
	return parseTraceparent(env[TRACEPARENT_ENV]);
}

/** Copy of `env` with `TRACEPARENT` set from the active span; unchanged when no span is active. */
export function injectTraceparentEnv<T extends Record<string, string | undefined>>(env: T): T {
	const traceparent = currentTraceparent();
	if (!traceparent) return env;
	return { ...env, [TRACEPARENT_ENV]: traceparent };
}

export interface SpanStartRecord {
	name: string;
	traceId: string;
	spanId: string;
	parentSpanId?: string;
	attrs: SpanAttributes;
}

export type SpanSink = (record: SpanEndRecord) => void;
type SpanStartSink = (record: SpanStartRecord) => void;
let spanSink: SpanSink | undefined;
let spanStartSink: SpanStartSink | undefined;
const additionalSpanSinks = new Set<SpanSink>();

/**
 * Install the process-wide span reporter. log.ts installs the default that
 * writes `span_end` entries through the structured logger; tests or an OTel
 * bridge can replace it. Pass undefined to drop reports.
 */
export function setSpanSink(next: SpanSink | undefined): void {
	spanSink = next;
}

/** Subscribe without replacing the structured-log span sink. Returns an idempotent unsubscribe function. */
export function addSpanSink(next: SpanSink): () => void {
	additionalSpanSinks.add(next);
	return () => additionalSpanSinks.delete(next);
}

/** Install a reporter for span starts. Kept separate so existing span-end sinks remain compatible. */
export function setSpanStartSink(next: SpanStartSink | undefined): void {
	spanStartSink = next;
}

function errorText(error: unknown): string {
	if (error instanceof Error) return error.message;
	try {
		return String(error);
	} catch {
		return "unknown error";
	}
}

function cleanAttrs(attrs: SpanAttributes | undefined): SpanAttributes {
	const out: SpanAttributes = {};
	if (!attrs) return out;
	for (const [key, value] of Object.entries(attrs)) {
		if (value !== undefined) out[key] = value;
	}
	return out;
}

class SpanImpl implements Span {
	readonly attrs: SpanAttributes;
	private readonly started = performance.now();
	private ended = false;
	private reportingEnabled = true;
	private failure: string | undefined;

	constructor(
		readonly name: string,
		readonly context: TraceContext,
		attrs: SpanAttributes | undefined,
	) {
		this.attrs = cleanAttrs(attrs);
	}

	setAttributes(attrs: SpanAttributes): void {
		if (!this.ended) Object.assign(this.attrs, cleanAttrs(attrs));
	}

	recordError(error: unknown): void {
		if (!this.ended) this.failure = errorText(error);
	}

	setReportingEnabled(enabled: boolean): void {
		if (!this.ended) this.reportingEnabled = enabled;
	}

	end(status?: SpanStatus): void {
		if (this.ended) return;
		this.ended = true;
		const record: SpanEndRecord = {
			name: this.name,
			traceId: this.context.traceId,
			spanId: this.context.spanId,
			parentSpanId: this.context.parentSpanId,
			durationMs: Math.round((performance.now() - this.started) * 1000) / 1000,
			status: status ?? (this.failure === undefined ? "ok" : "error"),
			attrs: { ...this.attrs },
			error: this.failure,
		};
		if (!this.reportingEnabled) return;
		try {
			spanSink?.(record);
		} catch {
			// Reporting must never break the traced operation.
		}
		for (const additionalSink of additionalSpanSinks) {
			try {
				additionalSink(record);
			} catch {
				// One diagnostic subscriber must not suppress the others.
			}
		}
	}
}

/** Long-running operations whose start is useful when a process dies or hangs before span_end. */
const ACTIVE_OPERATION_SPANS = new Set([
	"client.turn",
	"agent.prompt",
	"historian.run",
	"kernel.start",
	"session.compact",
	"cron.job",
	"ravo.run",
	"update.self",
	"child.passivate",
	"child.delete",
]);

/**
 * Open a child span of the active context (or a new root) without changing
 * the active context. Prefer `withSpan`; use this for spans that outlive a
 * single callback (e.g. a streaming request ended from a `finally`).
 */
export function startSpan(name: string, attrs?: SpanAttributes, parent?: TraceContext): Span {
	const span = new SpanImpl(name, childContext(parent ?? storage.getStore()), attrs);
	spansByContext.set(span.context, span);
	try {
		if (ACTIVE_OPERATION_SPANS.has(name))
			spanStartSink?.({
				name: span.name,
				traceId: span.context.traceId,
				spanId: span.context.spanId,
				parentSpanId: span.context.parentSpanId,
				attrs: span.attrs,
			});
	} catch {
		// Reporting must never break the traced operation.
	}
	return span;
}

/** Span objects keyed by their (unique) context, so code deep inside a
 * `withSpan` callback can mark the active span failed without threading the
 * Span through every call — e.g. a command handler that converts a thrown
 * error into a failure envelope instead of re-throwing. */
const spansByContext = new WeakMap<TraceContext, Span>();

/** The span whose context is active, if it was started by this module. */
export function currentSpan(): Span | undefined {
	const context = storage.getStore();
	return context ? spansByContext.get(context) : undefined;
}

/**
 * Run `fn` inside a new child span that is the active context for its
 * duration. Sync and async `fn` are both supported; the span ends when the
 * returned value settles. Errors are recorded and re-thrown.
 */
export function withSpan<T>(name: string, attrs: SpanAttributes | undefined, fn: (span: Span) => T): T;
export function withSpan<T>(name: string, fn: (span: Span) => T): T;
export function withSpan<T>(
	name: string,
	attrsOrFn: SpanAttributes | undefined | ((span: Span) => T),
	maybeFn?: (span: Span) => T,
): T {
	const fn = (typeof attrsOrFn === "function" ? attrsOrFn : maybeFn) as (span: Span) => T;
	const attrs = typeof attrsOrFn === "function" ? undefined : attrsOrFn;
	const span = startSpan(name, attrs);
	return storage.run(span.context, () => {
		let result: T;
		try {
			result = fn(span);
		} catch (error) {
			span.recordError(error);
			span.end();
			throw error;
		}
		if (result instanceof Promise) {
			return result.then(
				(value) => {
					span.end();
					return value;
				},
				(error: unknown) => {
					span.recordError(error);
					span.end();
					throw error;
				},
			) as T;
		}
		span.end();
		return result;
	});
}

/** Fields merged into every log entry while a span is active. */
export function currentTraceLogFields(): Record<string, string> {
	const context = storage.getStore();
	if (!context) return {};
	const fields: Record<string, string> = { traceId: context.traceId, spanId: context.spanId };
	if (context.parentSpanId) fields.parentSpanId = context.parentSpanId;
	return fields;
}
