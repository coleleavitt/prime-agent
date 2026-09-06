import { AsyncLocalStorage } from "node:async_hooks";
import {
	installAsyncTraceContextStorage,
	type LogEntry,
	runWithTraceContext,
	setLogSink,
	stringifyLogEntry,
	type TraceContext,
	traceContextFromEnv,
} from "@earendil-works/pi-ai";
import { appendRotatingLog, getAgentLogPath } from "../config.js";

const AGENT_LOG_MAX_BYTES = 20 * 1024 * 1024;

// pi-ai auto-installs an AsyncLocalStorage through a dynamic import that only
// resolves after the current microtask; installing eagerly here guarantees a
// span opened during startup (before that import settles) still propagates
// across await boundaries. installAsyncTraceContextStorage is idempotent, so
// the late auto-install becomes a no-op.
installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

let context: Record<string, unknown> = {};

/**
 * Fields that belong to one asynchronous flow rather than to the process. A
 * daemon worker hosts several sessions at once (a root session plus RLM and
 * runAgent children), so a process-global sessionId would label another
 * session's lines with whichever session called setLogContext last.
 */
const scopedContext = new AsyncLocalStorage<Record<string, unknown>>();

/** Merge late-bound process-wide fields (e.g. mode) into every subsequent log entry. */
export function setLogContext(fields: Record<string, unknown>): void {
	Object.assign(context, fields);
}

/**
 * Run `fn` with `fields` (typically `{ sessionId }`) merged into every log
 * entry emitted inside it, including entries from awaited continuations.
 * Nested calls layer on top of the enclosing scope. Prefer this over
 * setLogContext for anything that varies per session or per run.
 */
export function runWithLogContext<T>(fields: Record<string, unknown>, fn: () => T): T {
	return scopedContext.run({ ...scopedContext.getStore(), ...fields }, fn);
}

/**
 * Route all structured logging (coding-agent and pi-ai) to the shared JSONL
 * log at ~/.prime/agent/logs/agent.jsonl. One master file, filterable by the
 * pid/context fields; writes are best-effort and size-bounded.
 */
export function installFileLogSink(fields?: Record<string, unknown>): void {
	context = { pid: process.pid, ...fields };
	setLogSink((entry: LogEntry) => {
		// Context fields are defaults: the entry's own keys win so the reserved
		// ts/level/component/msg and the traceId/spanId/parentSpanId that pi-ai
		// stamps from the active span can never be overwritten by a context field.
		appendRotatingLog(
			getAgentLogPath(),
			stringifyLogEntry({ ...context, ...scopedContext.getStore(), ...entry }),
			AGENT_LOG_MAX_BYTES,
		);
	});
}

/**
 * Run `fn` under the trace context an external caller handed this process via
 * the `TRACEPARENT` environment variable, so every span the run opens becomes a
 * child of the caller's span. Without (or with a malformed) `TRACEPARENT` the
 * run proceeds with no ambient context, exactly as before.
 */
export function withInboundTraceContext<T>(fn: () => T): T {
	const inbound = traceContextFromEnv();
	return inbound ? runWithTraceContext(inbound, fn) : fn();
}
