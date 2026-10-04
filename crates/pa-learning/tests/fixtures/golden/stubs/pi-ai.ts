// `@earendil-works/pi-ai` for the golden generator: the log-record
// constants, a no-op logger, and a `withSpan` that runs its body.
export const TRACE_LOG_COMPONENT = "trace";
export const SPAN_END_MSG = "span_end";
export type LogEntry = Record<string, unknown> & { msg: string; component: string; ts?: string };
export function getLogger(): { warn: () => void; info: () => void } {
	return { warn: () => undefined, info: () => undefined };
}
export function withSpan<T>(_name: string, _attributes: unknown, body: (span: { setAttributes: () => void }) => T): T {
	return body({ setAttributes: () => undefined });
}
