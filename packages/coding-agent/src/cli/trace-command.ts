import { existsSync, readdirSync, readFileSync } from "node:fs";
import { basename, dirname } from "node:path";
import { gunzipSync } from "node:zlib";
import {
	type LogEntry,
	parseTraceparent,
	SPAN_END_MSG,
	SPAN_START_MSG,
	TRACE_LOG_COMPONENT,
} from "@earendil-works/pi-ai";
import { APP_NAME, getAgentLogPath } from "../config.js";

/**
 * `prime-agent trace <traceId|traceparent>` reconstructs one trace from the
 * shared structured log (`~/.prime/agent/logs/agent.jsonl`). It is a pure
 * reader over the logging contract in docs/observability.md: spans are the
 * `component: "trace"` / `msg: "span_end"` entries, everything else is a log
 * line attributed to the span that was active when it was emitted. Keeping
 * this dependency-free (no OTel, no daemon round trip) means it works on a
 * cold machine against a log copied from somewhere else.
 */

const TRACE_ID_RE = /^[0-9a-f]{32}$/;
/** Fields already rendered structurally; everything else is printed as `key=value`. */
const RESERVED_LOG_FIELDS = new Set(["ts", "level", "component", "msg", "traceId", "spanId", "parentSpanId"]);
const MAX_FIELD_VALUE_CHARS = 120;
const MAX_RETAINED_TRACE_LINES = 200_000;
const MAX_DECOMPRESSED_LOG_BYTES = 64 * 1024 * 1024;

export interface TraceCommandOptions {
	traceId: string;
	logPath: string | undefined;
	json: boolean;
}

export interface TraceLogLine {
	/** Original line text, kept so `--json` prints exactly what is on disk. */
	raw: string;
	entry: LogEntry;
	/** Source file, so a hit in the rotated `.old` file is attributable. */
	file: string;
}

export interface SpanNode {
	spanId: string;
	parentSpanId: string | undefined;
	name: string;
	/** Undefined for a placeholder: the span was referenced but never ended (still running, or its end was rotated away). */
	end: LogEntry | undefined;
	/** Epoch ms used only for ordering siblings; derived from `ts - durationMs`. */
	startMs: number;
	logs: TraceLogLine[];
	children: SpanNode[];
}

export interface TraceTree {
	traceId: string;
	roots: SpanNode[];
	/** Log lines that carry the trace id but no span id (emitted under a bare context). */
	unattributed: TraceLogLine[];
	spanCount: number;
	logCount: number;
}

export interface TraceCommandIo {
	stdout(line: string): void;
	stderr(line: string): void;
}

export class TraceCommandUsageError extends Error {}

/** Parse `<traceId|traceparent> [--log <path>] [--json]`; throws a usage error with the reason. */
export function parseTraceCommandArgs(args: string[]): TraceCommandOptions {
	let target: string | undefined;
	let logPath: string | undefined;
	let json = false;
	for (let index = 0; index < args.length; index++) {
		const arg = args[index]!;
		if (arg === "--json") {
			json = true;
		} else if (arg === "--log") {
			logPath = args[++index];
			if (logPath === undefined || logPath.startsWith("-")) {
				throw new TraceCommandUsageError("--log requires a path.");
			}
		} else if (arg.startsWith("--log=")) {
			logPath = arg.slice("--log=".length);
			if (logPath.length === 0) throw new TraceCommandUsageError("--log requires a path.");
		} else if (arg.startsWith("-")) {
			throw new TraceCommandUsageError(`Unknown option for trace: ${arg}`);
		} else if (target === undefined) {
			target = arg;
		} else {
			throw new TraceCommandUsageError("trace accepts exactly one trace id or traceparent.");
		}
	}
	if (target === undefined) {
		throw new TraceCommandUsageError("Missing trace id.");
	}
	return { traceId: normalizeTraceId(target), logPath, json };
}

/**
 * Accept either a bare 32-hex trace id or a full W3C `traceparent` (as found
 * in `TRACEPARENT` or a kernel frame) so whatever the user copied works.
 */
export function normalizeTraceId(value: string): string {
	const trimmed = value.trim();
	const fromTraceparent = parseTraceparent(trimmed.toLowerCase());
	if (fromTraceparent) return fromTraceparent.traceId;
	const lowered = trimmed.toLowerCase();
	if (TRACE_ID_RE.test(lowered) && lowered !== "0".repeat(32)) return lowered;
	throw new TraceCommandUsageError(
		`Not a trace id or traceparent: ${JSON.stringify(value)} (expected 32 hex chars or 00-<traceId>-<spanId>-<flags>).`,
	);
}

/**
 * The files to read for one log path, oldest first, matching the single
 * generation rotation in `appendRotatingLog` (`<path>.old` then `<path>`).
 * Only existing files are returned; a fresh install has neither.
 */
export function traceLogFiles(logPath: string): string[] {
	const directory = dirname(logPath);
	const prefix = `${basename(logPath)}.old.`;
	let compressed: string[] = [];
	try {
		compressed = readdirSync(directory)
			.map((name) => ({
				name,
				match: new RegExp(`^${prefix.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(\\d+)\\.gz$`).exec(name),
			}))
			.filter((entry): entry is { name: string; match: RegExpExecArray } => entry.match !== null)
			.sort((left, right) => Number(right.match[1]) - Number(left.match[1]))
			.map((entry) => `${directory}/${entry.name}`);
	} catch {
		// A missing or unreadable directory is handled by the existing no-log path.
	}
	return [...compressed, `${logPath}.old`, logPath].filter((file) => existsSync(file));
}

/** Read every well-formed entry for `traceId` across `files`, keeping file order (oldest first). */
export function readTraceLogLines(files: string[], traceId: string): TraceLogLine[] {
	const lines: TraceLogLine[] = [];
	for (const file of files) {
		// A quick substring test skips JSON.parse for the vast majority of lines
		// that belong to other traces; the log can be tens of megabytes.
		const content = file.endsWith(".gz")
			? gunzipSync(readFileSync(file), { maxOutputLength: MAX_DECOMPRESSED_LOG_BYTES }).toString("utf8")
			: readFileSync(file, "utf8");
		for (const raw of content.split("\n")) {
			if (!raw.includes(traceId)) continue;
			const entry = parseLogEntry(raw);
			if (entry?.traceId === traceId) {
				lines.push({ raw, entry, file });
				if (lines.length > MAX_RETAINED_TRACE_LINES) lines.shift();
			}
		}
	}
	return lines;
}

function parseLogEntry(raw: string): LogEntry | undefined {
	try {
		const parsed: unknown = JSON.parse(raw);
		if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return undefined;
		const entry = parsed as Partial<LogEntry>;
		if (typeof entry.msg !== "string" || typeof entry.component !== "string") return undefined;
		return entry as LogEntry;
	} catch {
		return undefined;
	}
}

function isSpanEnd(entry: LogEntry): boolean {
	return (
		entry.component === TRACE_LOG_COMPONENT &&
		entry.msg === SPAN_END_MSG &&
		typeof entry.spanId === "string" &&
		typeof entry.name === "string"
	);
}

function isSpanStart(entry: LogEntry): boolean {
	return (
		entry.component === TRACE_LOG_COMPONENT &&
		entry.msg === SPAN_START_MSG &&
		typeof entry.spanId === "string" &&
		typeof entry.name === "string"
	);
}

function timestampMs(entry: LogEntry): number {
	const parsed = Date.parse(entry.ts);
	return Number.isNaN(parsed) ? 0 : parsed;
}

/**
 * Build the span tree. Placeholder nodes stand in for spans that are referenced
 * (as a parent, or by a log line) but have no `span_end` yet: a trace is
 * usually inspected while the turn is still running, and hiding those lines
 * would make the tool useless for exactly that case.
 */
export function buildTraceTree(traceId: string, lines: TraceLogLine[]): TraceTree {
	const nodes = new Map<string, SpanNode>();
	const placeholder = (spanId: string, parentSpanId: string | undefined, atMs: number): SpanNode => {
		let node = nodes.get(spanId);
		if (!node) {
			node = { spanId, parentSpanId, name: "(open span)", end: undefined, startMs: atMs, logs: [], children: [] };
			nodes.set(spanId, node);
		} else if (node.end === undefined) {
			node.parentSpanId ??= parentSpanId;
			node.startMs = Math.min(node.startMs, atMs);
		}
		return node;
	};

	let spanCount = 0;
	for (const line of lines) {
		const { entry } = line;
		if (!isSpanStart(entry)) continue;
		const spanId = entry.spanId as string;
		const parentSpanId = typeof entry.parentSpanId === "string" ? entry.parentSpanId : undefined;
		const node = placeholder(spanId, parentSpanId, timestampMs(entry));
		node.name = `(open) ${entry.name as string}`;
		node.parentSpanId = parentSpanId;
		node.startMs = timestampMs(entry);
	}
	for (const line of lines) {
		const { entry } = line;
		if (!isSpanEnd(entry)) continue;
		const spanId = entry.spanId as string;
		const parentSpanId = typeof entry.parentSpanId === "string" ? entry.parentSpanId : undefined;
		const durationMs = typeof entry.durationMs === "number" ? entry.durationMs : 0;
		const node = placeholder(spanId, parentSpanId, timestampMs(entry) - durationMs);
		if (node.end !== undefined) continue;
		spanCount++;
		node.name = entry.name as string;
		node.end = entry;
		node.parentSpanId = parentSpanId;
		node.startMs = timestampMs(entry) - durationMs;
	}

	const unattributed: TraceLogLine[] = [];
	let logCount = 0;
	for (const line of lines) {
		const { entry } = line;
		if (isSpanEnd(entry) || isSpanStart(entry)) continue;
		logCount++;
		if (typeof entry.spanId !== "string") {
			unattributed.push(line);
			continue;
		}
		const parentSpanId = typeof entry.parentSpanId === "string" ? entry.parentSpanId : undefined;
		placeholder(entry.spanId, parentSpanId, timestampMs(entry)).logs.push(line);
	}

	// A parent that never ended (still running, rotated away, or owned by an
	// external caller via TRACEPARENT) still groups its children.
	for (const node of [...nodes.values()]) {
		if (node.parentSpanId !== undefined && node.parentSpanId !== node.spanId) {
			placeholder(node.parentSpanId, undefined, node.startMs);
		}
	}
	const roots: SpanNode[] = [];
	for (const node of nodes.values()) {
		const parent = node.parentSpanId === undefined ? undefined : nodes.get(node.parentSpanId);
		if (parent && parent !== node) {
			parent.children.push(node);
		} else {
			roots.push(node);
		}
	}
	const byStart = (left: SpanNode, right: SpanNode) => left.startMs - right.startMs;
	const byTs = (left: TraceLogLine, right: TraceLogLine) => timestampMs(left.entry) - timestampMs(right.entry);
	for (const node of nodes.values()) {
		node.children.sort(byStart);
		node.logs.sort(byTs);
	}
	roots.sort(byStart);
	unattributed.sort(byTs);
	return { traceId, roots, unattributed, spanCount, logCount };
}

function formatValue(value: unknown): string {
	const raw = typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
	const text = raw.replace(/[\r\n\t]/g, " ").replace(/[\u001b\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "?");
	return text.length > MAX_FIELD_VALUE_CHARS ? `${text.slice(0, MAX_FIELD_VALUE_CHARS - 1)}…` : text;
}

function formatFields(fields: Record<string, unknown>, skip: ReadonlySet<string>): string {
	const parts: string[] = [];
	for (const [key, value] of Object.entries(fields)) {
		if (skip.has(key) || value === undefined) continue;
		parts.push(`${key}=${formatValue(value)}`);
	}
	return parts.join(" ");
}

function formatTime(entry: LogEntry): string {
	// ISO timestamps share the date within a trace; the time part is what distinguishes lines.
	return typeof entry.ts === "string" && entry.ts.length >= 23 ? entry.ts.slice(11, 23) : String(entry.ts ?? "?");
}

function formatSpanHeading(node: SpanNode): string {
	if (!node.end) return `${node.name} ${node.spanId}`;
	const { durationMs, status, attrs, error } = node.end;
	const parts = [node.name, typeof durationMs === "number" ? `${durationMs}ms` : "?ms", String(status ?? "?")];
	if (attrs && typeof attrs === "object") {
		const rendered = formatFields(attrs as Record<string, unknown>, new Set());
		if (rendered) parts.push(rendered);
	}
	if (typeof error === "string" && error.length > 0) parts.push(`error=${formatValue(error)}`);
	parts.push(`[${node.spanId}]`);
	return parts.join("  ");
}

function formatLogLine(line: TraceLogLine): string {
	const { entry } = line;
	const parts = [
		formatTime(entry),
		String(entry.level ?? "?").padEnd(5),
		formatValue(entry.component),
		formatValue(entry.msg),
	];
	const extra = formatFields(entry, RESERVED_LOG_FIELDS);
	if (extra) parts.push(extra);
	return parts.join("  ");
}

/** One row of the tree: a span (with its own subtree) or a log line, ordered by time among its siblings. */
interface TreeItem {
	atMs: number;
	render(prefix: string, last: boolean, out: string[]): void;
}

function branch(prefix: string, last: boolean): string {
	return `${prefix}${last ? "└─ " : "├─ "}`;
}

function childPrefix(prefix: string, last: boolean): string {
	return `${prefix}${last ? "   " : "│  "}`;
}

function renderItems(items: TreeItem[], prefix: string, out: string[]): void {
	items.sort((left, right) => left.atMs - right.atMs);
	for (let index = 0; index < items.length; index++) {
		items[index]!.render(prefix, index === items.length - 1, out);
	}
}

function logItem(line: TraceLogLine): TreeItem {
	return {
		atMs: timestampMs(line.entry),
		render: (prefix, last, out) => out.push(`${branch(prefix, last)}${formatLogLine(line)}`),
	};
}

function spanItem(node: SpanNode): TreeItem {
	return {
		atMs: node.startMs,
		render: (prefix, last, out) => {
			out.push(`${branch(prefix, last)}${formatSpanHeading(node)}`);
			// Logs and child spans interleave by time so a span reads as a timeline.
			renderItems([...node.logs.map(logItem), ...node.children.map(spanItem)], childPrefix(prefix, last), out);
		},
	};
}

/** Render the tree as text; deterministic for a given log so it can be diffed. */
export function formatTraceTree(tree: TraceTree, files: string[]): string {
	const out: string[] = [];
	const plural = (count: number, noun: string) => `${count} ${noun}${count === 1 ? "" : "s"}`;
	out.push(
		`trace ${tree.traceId}  (${plural(tree.spanCount, "span")}, ${plural(tree.logCount, "log line")}, ${files.join(", ")})`,
	);
	const items = tree.roots.map(spanItem);
	if (tree.unattributed.length > 0) {
		// Always last: these lines have no span, so they have no place on the timeline.
		items.push({
			atMs: Number.POSITIVE_INFINITY,
			render: (prefix, last, lines) => {
				lines.push(`${branch(prefix, last)}(no span)`);
				renderItems(tree.unattributed.map(logItem), childPrefix(prefix, last), lines);
			},
		});
	}
	renderItems(items, "", out);
	return out.join("\n");
}

/**
 * Run the command against `io` and return the process exit code. Kept free of
 * `process` side effects so tests can drive it against a fixture log.
 */
export function runTraceCommand(args: string[], io: TraceCommandIo): number {
	let options: TraceCommandOptions;
	try {
		options = parseTraceCommandArgs(args);
	} catch (error) {
		if (!(error instanceof TraceCommandUsageError)) throw error;
		io.stderr(`Error: ${error.message}`);
		io.stderr(`Usage: ${APP_NAME} trace <traceId|traceparent> [--log <path>] [--json]`);
		return 1;
	}
	const logPath = options.logPath ?? getAgentLogPath();
	const files = traceLogFiles(logPath);
	if (files.length === 0) {
		io.stderr(`Error: no log file at ${logPath}`);
		return 1;
	}
	let lines: TraceLogLine[];
	try {
		lines = readTraceLogLines(files, options.traceId);
	} catch (error) {
		io.stderr(`Error: could not read ${files.join(", ")}: ${error instanceof Error ? error.message : String(error)}`);
		return 1;
	}
	if (lines.length === 0) {
		io.stderr(`Error: no entries for trace ${options.traceId} in ${files.join(", ")}`);
		return 1;
	}
	if (options.json) {
		for (const line of lines) io.stdout(line.raw);
		return 0;
	}
	io.stdout(formatTraceTree(buildTraceTree(options.traceId, lines), files));
	return 0;
}
