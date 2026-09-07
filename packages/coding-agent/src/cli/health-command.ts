import { existsSync, readdirSync, readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { gunzipSync } from "node:zlib";
import { APP_NAME, getAgentLogPath } from "../config.js";

const DEFAULT_SINCE_MS = 24 * 60 * 60 * 1000;
const DEFAULT_STUCK_AFTER_MS = 10 * 60 * 1000;
const DEFAULT_LIMIT = 20;
const MAX_LIMIT = 200;
const MAX_HEALTH_ENTRIES = 100_000;
const STUCK_TURN_SPANS = new Set(["client.turn", "agent.prompt"]);
const DURATION_RE = /^(\d+)(ms|s|m|h|d)$/;
const DAEMON_RECOVERY_RE = /\b(recover(?:y|ing|ed)?|restart(?:ed|ing)?)\b/i;
const DAEMON_FAILURE_RE = /\b(fail(?:ed|ure)?|interrupt(?:ed)?|cancel(?:led)?|could not|did not answer|uncertain)\b/i;

export type HealthCategory = "historian" | "provider" | "stuck_turn" | "daemon_recovery";

export interface HealthCommandOptions {
	logPath: string | undefined;
	sinceMs: number;
	stuckAfterMs: number;
	limit: number;
	json: boolean;
}

interface HealthLogEntry extends Record<string, unknown> {
	ts: string;
	component: string;
	msg: string;
	traceId?: string;
	spanId?: string;
	parentSpanId?: string;
	_beforeWindow?: boolean;
}

export interface HealthIncident {
	category: HealthCategory;
	ts: string;
	summary: string;
	traceId?: string;
	sessionId?: string;
}

export interface HealthSummary {
	generatedAt: string;
	since: string;
	files: string[];
	counts: Record<HealthCategory, number>;
	incidents: HealthIncident[];
	truncated: boolean;
}

export interface HealthCommandIo {
	stdout(line: string): void;
	stderr(line: string): void;
	now?(): number;
}

export class HealthCommandUsageError extends Error {}

function parseDuration(value: string, option: string): number {
	const match = DURATION_RE.exec(value);
	if (!match) throw new HealthCommandUsageError(`${option} requires a duration such as 30m, 6h, or 2d.`);
	const amount = Number(match[1]);
	const multiplier = { ms: 1, s: 1000, m: 60_000, h: 3_600_000, d: 86_400_000 }[match[2]!]!;
	const result = amount * multiplier;
	if (!Number.isSafeInteger(result) || result <= 0) throw new HealthCommandUsageError(`${option} must be positive.`);
	return result;
}

export function parseHealthCommandArgs(args: string[]): HealthCommandOptions {
	let logPath: string | undefined;
	let sinceMs = DEFAULT_SINCE_MS;
	let stuckAfterMs = DEFAULT_STUCK_AFTER_MS;
	let limit = DEFAULT_LIMIT;
	let json = false;
	const takeValue = (index: number, option: string): string => {
		const value = args[index + 1];
		if (value === undefined || value.startsWith("-"))
			throw new HealthCommandUsageError(`${option} requires a value.`);
		return value;
	};
	for (let index = 0; index < args.length; index++) {
		const arg = args[index]!;
		if (arg === "--json") json = true;
		else if (arg === "--log") logPath = takeValue(index++, "--log");
		else if (arg.startsWith("--log=")) logPath = arg.slice(6);
		else if (arg === "--since") sinceMs = parseDuration(takeValue(index++, "--since"), "--since");
		else if (arg.startsWith("--since=")) sinceMs = parseDuration(arg.slice(8), "--since");
		else if (arg === "--stuck-after")
			stuckAfterMs = parseDuration(takeValue(index++, "--stuck-after"), "--stuck-after");
		else if (arg.startsWith("--stuck-after=")) stuckAfterMs = parseDuration(arg.slice(14), "--stuck-after");
		else if (arg === "--limit") limit = Number(takeValue(index++, "--limit"));
		else if (arg.startsWith("--limit=")) limit = Number(arg.slice(8));
		else throw new HealthCommandUsageError(`Unknown option for health: ${arg}`);
	}
	if (logPath === "") throw new HealthCommandUsageError("--log requires a path.");
	if (!Number.isInteger(limit) || limit <= 0 || limit > MAX_LIMIT) {
		throw new HealthCommandUsageError(`--limit must be an integer from 1 to ${MAX_LIMIT}.`);
	}
	return { logPath, sinceMs, stuckAfterMs, limit, json };
}

export function healthLogFiles(logPath: string): string[] {
	const directory = dirname(logPath);
	const escapedPrefix = `${basename(logPath)}.old.`.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
	let compressed: Array<{ path: string; generation: number }> = [];
	try {
		compressed = readdirSync(directory).flatMap((name) => {
			const match = new RegExp(`^${escapedPrefix}(\\d+)\\.gz$`).exec(name);
			return match ? [{ path: join(directory, name), generation: Number(match[1]) }] : [];
		});
	} catch {
		// A missing or unreadable directory is handled by the existing no-log path.
	}
	return [
		...compressed.sort((left, right) => right.generation - left.generation).map((entry) => entry.path),
		`${logPath}.old`,
		logPath,
	].filter((file) => existsSync(file));
}

function parseEntry(raw: string): HealthLogEntry | undefined {
	try {
		const value: unknown = JSON.parse(raw);
		if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
		const entry = value as Partial<HealthLogEntry>;
		if (typeof entry.ts !== "string" || typeof entry.component !== "string" || typeof entry.msg !== "string")
			return undefined;
		return entry as HealthLogEntry;
	} catch {
		return undefined;
	}
}

export function readHealthLogEntries(files: readonly string[], cutoffMs: number): HealthLogEntry[] {
	const entries: HealthLogEntry[] = [];
	for (const file of files) {
		const content = file.endsWith(".gz")
			? gunzipSync(readFileSync(file), { maxOutputLength: 64 * 1024 * 1024 }).toString("utf8")
			: readFileSync(file, "utf8");
		for (const raw of content.split("\n")) {
			const entry = parseEntry(raw);
			if (!entry) continue;
			const at = Date.parse(entry.ts);
			if (!Number.isNaN(at)) {
				const lifecycle = entry.component === "trace" && (entry.msg === "span_start" || entry.msg === "span_end");
				if (at >= cutoffMs || lifecycle) {
					entry._beforeWindow = at < cutoffMs;
					entries.push(entry);
					if (entries.length > MAX_HEALTH_ENTRIES) entries.shift();
				}
			}
		}
	}
	return entries.sort((left, right) => Date.parse(left.ts) - Date.parse(right.ts));
}

function stringField(entry: HealthLogEntry, key: string): string | undefined {
	const value = entry[key];
	return typeof value === "string" && value.length > 0 ? value : undefined;
}

function spanAttrs(entry: HealthLogEntry): Record<string, unknown> {
	const attrs = entry.attrs;
	return typeof attrs === "object" && attrs !== null && !Array.isArray(attrs)
		? (attrs as Record<string, unknown>)
		: {};
}

function incident(category: HealthCategory, entry: HealthLogEntry, summary: string): HealthIncident {
	const attrs = spanAttrs(entry);
	const sessionId =
		stringField(entry, "sessionId") ??
		(typeof attrs["session.id"] === "string" ? attrs["session.id"] : undefined) ??
		(typeof attrs["historian.session_id"] === "string" ? attrs["historian.session_id"] : undefined);
	return {
		category,
		ts: entry.ts,
		summary,
		...(entry.traceId ? { traceId: entry.traceId } : {}),
		...(sessionId ? { sessionId } : {}),
	};
}

function spanFailed(entry: HealthLogEntry): boolean {
	const attrs = spanAttrs(entry);
	return (
		entry.status === "error" ||
		attrs["historian.valid"] === false ||
		attrs["historian.outcome"] === "failed" ||
		attrs["historian.outcome"] === "failure" ||
		attrs["historian.outcome"] === "error"
	);
}

function detail(entry: HealthLogEntry, fallback: string): string {
	const attrs = spanAttrs(entry);
	return (
		stringField(entry, "error") ??
		stringField(entry, "message") ??
		(typeof attrs["historian.failure_reason"] === "string" ? attrs["historian.failure_reason"] : fallback)
	);
}

export function summarizeHealth(
	entries: readonly HealthLogEntry[],
	nowMs: number,
	options: Pick<HealthCommandOptions, "stuckAfterMs" | "limit">,
	files: string[],
	since: string,
): HealthSummary {
	const incidents: HealthIncident[] = [];
	const spanKey = (entry: HealthLogEntry): string | undefined =>
		entry.traceId && entry.spanId ? `${entry.traceId}:${entry.spanId}` : undefined;
	const providerLogSpans = new Set(
		entries
			.filter((entry) => entry.component === "ai.provider" && entry.msg === "provider stream failure")
			.map(spanKey)
			.filter((value): value is string => value !== undefined),
	);
	const endedSpanIds = new Set(
		entries
			.filter((entry) => entry.component === "trace" && entry.msg === "span_end")
			.map(spanKey)
			.filter((value): value is string => value !== undefined),
	);
	const activeStarts = entries.filter(
		(entry) =>
			entry.component === "trace" &&
			entry.msg === "span_start" &&
			typeof entry.spanId === "string" &&
			STUCK_TURN_SPANS.has(stringField(entry, "name") ?? "") &&
			!endedSpanIds.has(spanKey(entry) ?? ""),
	);
	const stuckCandidates = new Map<string, HealthLogEntry>();

	for (const entry of entries) {
		const name = stringField(entry, "name");
		if (entry._beforeWindow) continue;
		if (
			entry.component === "trace" &&
			entry.msg === "span_end" &&
			name?.startsWith("historian.") &&
			spanFailed(entry)
		) {
			incidents.push(incident("historian", entry, `${name}: ${detail(entry, "failed")}`));
		}
		const providerLog = entry.component === "ai.provider" && entry.msg === "provider stream failure";
		const providerSpan =
			entry.component === "trace" && entry.msg === "span_end" && name === "llm.request" && entry.status === "error";
		if (providerLog || (providerSpan && (!spanKey(entry) || !providerLogSpans.has(spanKey(entry)!)))) {
			const provider =
				stringField(entry, "provider") ??
				(typeof spanAttrs(entry)["llm.provider"] === "string"
					? String(spanAttrs(entry)["llm.provider"])
					: "provider");
			incidents.push(incident("provider", entry, `${provider}: ${detail(entry, "request failed")}`));
		}

		if (
			entry.component.includes("daemon") &&
			DAEMON_RECOVERY_RE.test(entry.msg) &&
			DAEMON_FAILURE_RE.test(entry.msg)
		) {
			incidents.push(incident("daemon_recovery", entry, entry.msg));
		}
	}
	for (const entry of activeStarts) {
		stuckCandidates.set(spanKey(entry)!, entry);
	}
	for (const [spanId, entry] of stuckCandidates) {
		const ageMs = nowMs - Date.parse(entry.ts);
		if (ageMs >= options.stuckAfterMs) {
			const name = stringField(entry, "name") ?? "turn";
			incidents.push(
				incident(
					"stuck_turn",
					entry,
					`${name} span ${entry.spanId ?? spanId} has no completion after ${formatDuration(ageMs)}`,
				),
			);
		}
	}
	incidents.sort((left, right) => Date.parse(right.ts) - Date.parse(left.ts));
	const counts: Record<HealthCategory, number> = { historian: 0, provider: 0, stuck_turn: 0, daemon_recovery: 0 };
	for (const item of incidents) counts[item.category]++;
	return {
		generatedAt: new Date(nowMs).toISOString(),
		since,
		files,
		counts,
		incidents: incidents.slice(0, options.limit),
		truncated: incidents.length > options.limit,
	};
}

function terminalSafe(value: string): string {
	return value.replace(/[\r\n\t]/g, " ").replace(/[\u001b\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "?");
}

function formatDuration(ms: number): string {
	if (ms >= 86_400_000) return `${Math.floor(ms / 86_400_000)}d`;
	if (ms >= 3_600_000) return `${Math.floor(ms / 3_600_000)}h`;
	if (ms >= 60_000) return `${Math.floor(ms / 60_000)}m`;
	return `${Math.floor(ms / 1000)}s`;
}

function formatHealthSummary(summary: HealthSummary): string {
	const total = Object.values(summary.counts).reduce((sum, count) => sum + count, 0);
	const out = [
		`health since ${summary.since}  (${total} incident${total === 1 ? "" : "s"}; ${summary.files.join(", ")})`,
	];
	const labels: Array<[HealthCategory, string]> = [
		["historian", "Historian failures"],
		["provider", "Provider errors"],
		["stuck_turn", "Stuck turns"],
		["daemon_recovery", "Daemon recovery failures"],
	];
	for (const [category, label] of labels) {
		out.push(`${label}: ${summary.counts[category]}`);
		for (const item of summary.incidents.filter((candidate) => candidate.category === category)) {
			const context = [
				item.sessionId ? `session=${item.sessionId}` : undefined,
				item.traceId ? `trace=${item.traceId}` : undefined,
			]
				.filter(Boolean)
				.join(" ");
			out.push(`  ${item.ts}  ${terminalSafe(item.summary)}${context ? `  ${terminalSafe(context)}` : ""}`);
		}
	}
	if (summary.truncated) out.push("Recent incident details truncated; increase --limit to show more.");
	return out.join("\n");
}

export function runHealthCommand(args: string[], io: HealthCommandIo): number {
	let options: HealthCommandOptions;
	try {
		options = parseHealthCommandArgs(args);
	} catch (error) {
		if (!(error instanceof HealthCommandUsageError)) throw error;
		io.stderr(`Error: ${error.message}`);
		io.stderr(
			`Usage: ${APP_NAME} health [--since <duration>] [--stuck-after <duration>] [--limit <n>] [--log <path>] [--json]`,
		);
		return 1;
	}
	const nowMs = io.now?.() ?? Date.now();
	const logPath = options.logPath ?? getAgentLogPath();
	const files = healthLogFiles(logPath);
	if (files.length === 0) {
		io.stderr(`Error: no log file at ${logPath}`);
		return 1;
	}
	try {
		const cutoffMs = nowMs - options.sinceMs;
		const entries = readHealthLogEntries(files, cutoffMs);
		const summary = summarizeHealth(entries, nowMs, options, files, new Date(cutoffMs).toISOString());
		io.stdout(options.json ? JSON.stringify(summary, undefined, 2) : formatHealthSummary(summary));
		return 0;
	} catch (error) {
		io.stderr(`Error: could not read ${files.join(", ")}: ${error instanceof Error ? error.message : String(error)}`);
		return 1;
	}
}
