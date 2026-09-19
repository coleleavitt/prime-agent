import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, ToolResultMessage } from "@earendil-works/pi-ai";
import { canonicalJson, sha256 } from "./canonical-json.js";
import type { JsonValue, RavoState, RavoWindowClock } from "./reducer.js";
import {
	deriveReplayCase,
	mergeReplayCase,
	normalizeReplayCases,
	type ReplayCase,
	replayCasesOf,
	replayProbeOf,
	verifiedReplayCasesOf,
} from "./referee.js";

/**
 * Failure ledger: fingerprints observed runtime failures (Python tracebacks in
 * tool output, tool results flagged `isError`, provider errors) and counts them
 * so a recurring failure can trigger a deterministic /refine round that must
 * address it. Fingerprints double as RAVO opponent criterion ids
 * (`FAILURE_OPPONENT_PREFIX + id`).
 *
 * A record may also carry REPLAY CASES: executable reproductions of the
 * failure, derived here from the kernel's own traceback for an ipython cell
 * that raised (never from a traceback some tool merely printed) and confirmed
 * by the referee's self-check (`verifyObservedReplayCases`) before they count
 * as evidence. They are what lets a proposal's claim to have fixed an
 * environment failure be refuted by execution instead of believed. One
 * fingerprint can collapse many distinct reproductions (normalization erases
 * the quoted module and attribute names), so a record keeps a bounded list of
 * valid probes distinct by source; a stored case that is not one is dropped
 * when the ledger is next loaded or written.
 *
 * Not every failure is something a harness edit can prevent. A provider outage,
 * a user denying a fetch, or a flaky network is still counted (the observation
 * ordinal must stay monotone) but never recurs as a refine trigger or opponent;
 * see `isActionableFailure`. A record is non-actionable while a strict majority
 * of its occurrences classified that way (`nonActionableCount`).
 */

export type FailureKind = "python_exception" | "tool_error" | "provider_error";

export interface FailureFingerprint {
	/** First 16 hex chars of sha256(canonicalJson({kind, source, exceptionClass, message})). */
	id: string;
	kind: FailureKind;
	source?: string;
	exceptionClass?: string;
	/** Normalized message: lowercase, numbers "#", quoted strings "?", paths "<path>", hex ids "<hex>". */
	message: string;
}

export interface FailureObservation {
	fingerprint: FailureFingerprint;
	excerpt: string;
	entryIndex: number;
	turn: number;
	at: string;
	/** Derived reproduction; never verified at observation time. */
	replayCase?: ReplayCase;
}

export interface FailureRecord {
	fingerprint: FailureFingerprint;
	count: number;
	firstSeenTurn: number;
	lastSeenTurn: number;
	firstSeenAt: string;
	lastSeenAt: string;
	excerpt: string;
	addressedByProposalIds: string[];
	/**
	 * Distinct executable reproductions, oldest first, at most `MAX_REPLAY_CASES`,
	 * each a valid probe (`replayProbeOf`) as stored by this module. Only the
	 * verified ones are evidence (`verifiedReplayCasesOf`).
	 */
	replayCases?: ReplayCase[];
	/** Legacy single case. Read on load and folded into `replayCases`; never written. */
	replayCase?: ReplayCase;
	/**
	 * Occurrences that classified non-actionable; omitted when none did. One
	 * fingerprint collapses bodies that classify differently (the normalized
	 * message is capped, and a provider body is erased), so neither the latest
	 * excerpt nor any single occurrence decides: the record is non-actionable
	 * while these are a strict majority of `count`.
	 */
	nonActionableCount?: number;
	/** Legacy flag: every occurrence counts as non-actionable. Read on load and folded into `nonActionableCount`; never written. */
	nonActionable?: true;
}

export interface FailureLedger {
	schema: 1;
	failures: Record<string, FailureRecord>;
	lastScannedEntryIndex: number;
}

export type FailureTriggerReason = "recurrence" | "regression";

export interface ProvisionalRegression {
	championId: string;
	fingerprints: string[];
	committedTurn: number;
	untilTurn: number;
}

export const FAILURE_OPPONENT_PREFIX = "failure:";
export const DEFAULT_RECURRENCE_THRESHOLD = 2;

export const GLOBAL_FAILURE_LEDGER_ENV = "PRIME_AGENT_GLOBAL_LEDGER";

/**
 * Whether the failure ledger is kept in the GLOBAL harness state, so a
 * fingerprint observed in one session still counts towards recurrence in the
 * next one and a provisional champion can be seen to regress after the session
 * that committed it ended. On by default: a per-session ledger almost never
 * reaches the recurrence threshold, and the durable observation ordinal only
 * advances through the global one. `PRIME_AGENT_GLOBAL_LEDGER=0` (or off,
 * false, no) keeps it per-session.
 */
export function globalFailureLedgerEnabled(env: Record<string, string | undefined> = process.env): boolean {
	const value = env[GLOBAL_FAILURE_LEDGER_ENV]?.trim().toLowerCase();
	return !(value === "0" || value === "off" || value === "false" || value === "no");
}

const MAX_NORMALIZED_MESSAGE_LENGTH = 200;
const MAX_EXCERPT_LENGTH = 400;
const TRACEBACK_HEADER = /Traceback \(most recent call last\)/;
const EXCEPTION_LINE = /^(\w+(?:\.\w+)*(?:Error|Exception|Warning)): (.*)$/m;
const BARE_EXCEPTION_LINE = /^([A-Z]\w*(?:\.[A-Z]\w*)*)$/;
const TRACEBACK_FRAME = /^\s*File "([^"]+)", line \d+/;

export function emptyFailureLedger(): FailureLedger {
	return { schema: 1, failures: {}, lastScannedEntryIndex: 0 };
}

export function normalizeFailureLedger(value: unknown): FailureLedger {
	const ledger = emptyFailureLedger();
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		return ledger;
	}
	const raw = value as Record<string, unknown>;
	if (typeof raw.lastScannedEntryIndex === "number" && Number.isSafeInteger(raw.lastScannedEntryIndex)) {
		ledger.lastScannedEntryIndex = Math.max(0, raw.lastScannedEntryIndex);
	}
	const failures = raw.failures;
	if (typeof failures !== "object" || failures === null || Array.isArray(failures)) {
		return ledger;
	}
	for (const [id, rawRecord] of Object.entries(failures as Record<string, unknown>)) {
		const record = normalizeFailureRecord(id, rawRecord);
		if (record) ledger.failures[id] = record;
	}
	return ledger;
}

function normalizeFailureRecord(id: string, value: unknown): FailureRecord | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const raw = value as Record<string, unknown>;
	const fp = raw.fingerprint;
	if (typeof fp !== "object" || fp === null || Array.isArray(fp)) return undefined;
	const rawFp = fp as Record<string, unknown>;
	const kind = rawFp.kind;
	if (kind !== "python_exception" && kind !== "tool_error" && kind !== "provider_error") return undefined;
	if (typeof rawFp.message !== "string") return undefined;
	const fingerprint: FailureFingerprint = {
		id: typeof rawFp.id === "string" && rawFp.id.length > 0 ? rawFp.id : id,
		kind,
		message: rawFp.message,
	};
	if (typeof rawFp.source === "string") fingerprint.source = rawFp.source;
	if (typeof rawFp.exceptionClass === "string") fingerprint.exceptionClass = rawFp.exceptionClass;
	const count = typeof raw.count === "number" && Number.isSafeInteger(raw.count) ? Math.max(0, raw.count) : 0;
	const replayCases = normalizeReplayCases(raw.replayCases, raw.replayCase);
	const excerpt = typeof raw.excerpt === "string" ? raw.excerpt : "";
	const record: FailureRecord = {
		fingerprint,
		count,
		firstSeenTurn: safeTurn(raw.firstSeenTurn),
		lastSeenTurn: safeTurn(raw.lastSeenTurn),
		firstSeenAt: typeof raw.firstSeenAt === "string" ? raw.firstSeenAt : "",
		lastSeenAt: typeof raw.lastSeenAt === "string" ? raw.lastSeenAt : "",
		excerpt,
		addressedByProposalIds: Array.isArray(raw.addressedByProposalIds)
			? raw.addressedByProposalIds.filter((item): item is string => typeof item === "string")
			: [],
		...(replayCases.length === 0 ? {} : { replayCases }),
	};
	return withNonActionableCount(
		record,
		nonActionableCountOf({
			fingerprint,
			excerpt,
			count,
			nonActionableCount: raw.nonActionableCount,
			nonActionable: raw.nonActionable,
		}),
	);
}

/** The record with its tally stored in `nonActionableCount` (omitted at zero) and the legacy flag dropped. */
function withNonActionableCount(record: FailureRecord, nonActionableCount: number): FailureRecord {
	const { nonActionableCount: _previous, nonActionable: _legacy, ...rest } = record;
	return nonActionableCount > 0 ? { ...rest, nonActionableCount } : rest;
}

/**
 * The occurrences of a record that classified non-actionable. When the
 * fingerprint alone (its normalized message or exception class) classifies
 * non-actionable, every occurrence did, whatever tally was stored. Otherwise
 * the stored tally counts; a record written before the tally existed counts
 * every occurrence under the legacy `nonActionable` flag, and otherwise one
 * when its stored excerpt classifies non-actionable (that excerpt is the one
 * occurrence known).
 */
function nonActionableCountOf(record: {
	fingerprint: FailureFingerprint;
	excerpt: string;
	count: number;
	nonActionableCount?: unknown;
	nonActionable?: unknown;
}): number {
	const { count, nonActionableCount } = record;
	if (count <= 0) return 0;
	if (classifiesNonActionable({ fingerprint: record.fingerprint, excerpt: "" })) return count;
	if (typeof nonActionableCount === "number" && Number.isSafeInteger(nonActionableCount)) {
		return Math.min(count, Math.max(0, nonActionableCount));
	}
	if (record.nonActionable === true) return count;
	return classifiesNonActionable(record) ? 1 : 0;
}

/**
 * The record with its valid-probe cases (`replayProbeOf`) stored in
 * `replayCases` and the legacy single field dropped. Every write of a record's
 * cases goes through here.
 */
function withReplayCases(record: FailureRecord, cases: readonly ReplayCase[]): FailureRecord {
	const { replayCase: _legacy, replayCases: _previous, ...rest } = record;
	const live = cases.filter((replay) => replayProbeOf(replay) !== undefined);
	return live.length === 0 ? rest : { ...rest, replayCases: live };
}

function safeTurn(value: unknown): number {
	return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : 0;
}

/**
 * Canonicalize a raw error message so the same fault reported with different
 * numbers, paths, ids, or quoted values maps to one fingerprint.
 */
export function normalizeFailureMessage(raw: string): string {
	let text = raw.toLowerCase();
	text = text.replace(/(?:~|[a-z]:)?(?:\/[^\s'"`,;()[\]<>]+)+/g, "<path>");
	text = text.replace(/(["'`])(?:(?!\1).){0,400}\1/g, "?");
	text = text.replace(/\b0x[0-9a-f]+\b/g, "<hex>");
	text = text.replace(/\b[0-9a-f]{8,}(?:-[0-9a-f]{4,}){0,4}\b/g, "<hex>");
	text = text.replace(/[-+]?\d+(?:\.\d+)?/g, "#");
	text = text.replace(/\s+/g, " ").trim();
	return text.length > MAX_NORMALIZED_MESSAGE_LENGTH ? text.slice(0, MAX_NORMALIZED_MESSAGE_LENGTH) : text;
}

export function fingerprintFailure(
	kind: FailureKind,
	source: string | undefined,
	exceptionClass: string | undefined,
	rawMessage: string,
): FailureFingerprint {
	const message = normalizeFailureMessage(rawMessage);
	const id = sha256(
		canonicalJson({ kind, source: source ?? null, exceptionClass: exceptionClass ?? null, message }),
	).slice(0, 16);
	const fingerprint: FailureFingerprint = { id, kind, message };
	if (source !== undefined) fingerprint.source = source;
	if (exceptionClass !== undefined) fingerprint.exceptionClass = exceptionClass;
	return fingerprint;
}

/**
 * A durable, monotone, cross-session observation ordinal.
 *
 * Trust windows and provisional regressions need to say "N units of observed
 * work after the commit". Per-session assistant-turn numbers cannot: they
 * restart at 0 every session, so a window opened at turn 40 in session A is
 * asking about turns 40..60 while session B is at turn 2, and the window can
 * never match except by coincidence.
 *
 * Wall-clock is the other obvious candidate and is worse: a window would
 * expire while the operator was away from the machine and credit a clean
 * window that nothing ever tested, which is a fail-open.
 *
 * The ledger's own occurrence total is the right ordinal and is free. It is
 * monotone by construction -- `count` only ever increments (see
 * `updateFailureLedger`) and nothing evicts a record -- it is already durable
 * and already loaded, and its units are exactly what the window means: "the
 * machine observed this many more failures and none of them was yours".
 */
export function observationOrdinal(ledger: FailureLedger | undefined): number {
	if (!ledger) return 0;
	let total = 0;
	for (const record of Object.values(ledger.failures)) {
		total += record.count;
	}
	return total;
}

export function failureOpponentId(fp: FailureFingerprint | string): string {
	const id = typeof fp === "string" ? fp : fp.id;
	return id.startsWith(FAILURE_OPPONENT_PREFIX) ? id : `${FAILURE_OPPONENT_PREFIX}${id}`;
}

/** Bare exception classes that are timeouts or transport failures (`httpx.ConnectError`). */
const NON_ACTIONABLE_EXCEPTION_CLASSES = new Set(["TimeoutError", "ConnectError"]);

/** Outside the harness whatever the failure kind: aborts, denials, a dead kernel, the network, timeouts, a lock. */
const NON_ACTIONABLE_ANY_KIND: readonly RegExp[] = [
	/\b(?:request|operation) was aborted\b/,
	/\bwas not approved\b/,
	/\b(?:denied|declined|rejected) by (?:the )?user\b/,
	/\bkernel has been shut ?down\b/,
	/\bfetch failed\b/,
	/\bfetch http (?:\d{3}|#)(?!\w)/,
	/\b(?:enotfound|eai_again|econnrefused|econnreset|etimedout|ehostunreach|enetunreach)\b/,
	/\bname or service not known\b/,
	/\bsocket hang up\b/,
	/\btimed out\b/,
	/\bdatabase is locked\b/,
];

/** Provider-side conditions: capacity, transport, refusal, and empty completions. */
const NON_ACTIONABLE_PROVIDER: readonly RegExp[] = [
	/\brate[\s_-]*limit/,
	/\btoo many requests\b/,
	/\b429\b/,
	/\boverloaded/,
	/\b(?:over|at|insufficient) capacity\b/,
	/\bcapacity (?:exceeded|constraint)/,
	/\bservice[\s_-]*unavailable/,
	/\binternal[\s_-]*server[\s_-]*(?:error|exception)/,
	/\bbad gateway\b/,
	/\bgateway[\s_-]*time-?out\b/,
	/\b(?:http|status(?: code)?)[\s:=]*5\d\d\b/,
	/\bterminated\b/,
	/\bconnection error\b/,
	/\brefus(?:ed|al)\b/,
	/\bcontent[\s_-]*filter/,
	/\bempty (?:completion|response)\b/,
];

/**
 * Whether one occurrence is outside what a harness edit could prevent.
 * Classification reads the raw excerpt as well as the normalized message: some
 * providers put the cause only in a body the fingerprint normalization erases.
 *
 * Timeouts count only as `TimeoutError` or "timed out"; the bare word
 * "timeout" is mostly the agent passing `timeout=` to an API that has no such
 * keyword, which is exactly the kind of failure a note can prevent.
 */
function classifiesNonActionable(occurrence: Pick<FailureRecord, "fingerprint" | "excerpt">): boolean {
	const { fingerprint } = occurrence;
	const exceptionClass = fingerprint.exceptionClass?.split(".").at(-1);
	if (exceptionClass && NON_ACTIONABLE_EXCEPTION_CLASSES.has(exceptionClass)) return true;
	const text = `${fingerprint.message}\n${occurrence.excerpt}`.toLowerCase();
	if (NON_ACTIONABLE_ANY_KIND.some((pattern) => pattern.test(text))) return true;
	return fingerprint.kind === "provider_error" && NON_ACTIONABLE_PROVIDER.some((pattern) => pattern.test(text));
}

/**
 * Whether a harness edit could plausibly prevent this failure. An occurrence
 * (anything without a `count`) is classified by its own excerpt. A record is
 * actionable unless a strict majority of its occurrences classified
 * non-actionable (`nonActionableCount`), so one timeout among harness-fixable
 * occurrences does not mute a fingerprint, and one fixable body among outages
 * does not arm it.
 */
export function isActionableFailure(
	record: Pick<FailureRecord, "fingerprint" | "excerpt"> &
		Partial<Pick<FailureRecord, "count" | "nonActionableCount" | "nonActionable">>,
): boolean {
	if (record.count === undefined) return !classifiesNonActionable(record);
	return nonActionableCountOf({ ...record, count: record.count }) * 2 <= record.count;
}

function messageText(message: ToolResultMessage): string {
	return message.content
		.filter((part): part is { type: "text"; text: string } => part.type === "text" && typeof part.text === "string")
		.map((part) => part.text)
		.join("\n");
}

function clipExcerpt(text: string): string {
	const trimmed = text.trim();
	return trimmed.length > MAX_EXCERPT_LENGTH ? `${trimmed.slice(0, MAX_EXCERPT_LENGTH)}...` : trimmed;
}

interface ParsedTraceback {
	exceptionClass: string;
	message: string;
	excerpt: string;
	skillName?: string;
}

/**
 * Parse the LAST Python traceback in a block of tool output. The exception
 * class comes from the final `Class: message` line after the header; a bare
 * class line (e.g. `KeyboardInterrupt`) is accepted as a fallback.
 */
export function parsePythonTraceback(text: string): ParsedTraceback | undefined {
	const headerIndex = text.search(TRACEBACK_HEADER);
	if (headerIndex === -1) return undefined;
	const lastHeader = text.lastIndexOf("Traceback (most recent call last)");
	const block = text.slice(lastHeader);
	const lines = block.split(/\r?\n/);
	let exceptionClass: string | undefined;
	let message = "";
	let lastFrame: string | undefined;
	let skillName: string | undefined;
	for (const line of lines) {
		const frame = TRACEBACK_FRAME.exec(line);
		if (frame) {
			lastFrame = line.trim();
			const skill = skillNameFromPath(frame[1]);
			if (skill) skillName = skill;
			continue;
		}
		const match = EXCEPTION_LINE.exec(line);
		if (match) {
			exceptionClass = match[1];
			message = match[2].trim();
			continue;
		}
		const bare = BARE_EXCEPTION_LINE.exec(line.trim());
		if (bare && lastFrame !== undefined && exceptionClass === undefined) {
			exceptionClass = bare[1];
			message = "";
		}
	}
	if (!exceptionClass) return undefined;
	const exceptionLine = message ? `${exceptionClass}: ${message}` : exceptionClass;
	const excerpt = clipExcerpt(lastFrame ? `${lastFrame}\n${exceptionLine}` : exceptionLine);
	const parsed: ParsedTraceback = { exceptionClass, message, excerpt };
	if (skillName) parsed.skillName = skillName;
	return parsed;
}

function skillNameFromPath(path: string): string | undefined {
	const parts = path.split(/[\\/]+/);
	const index = parts.lastIndexOf("skills");
	if (index === -1 || index + 1 >= parts.length) return undefined;
	const name = parts[index + 1];
	return name && !name.includes(".") ? name : undefined;
}

export function extractFailures(
	messages: readonly AgentMessage[],
	opts: { fromEntryIndex: number; turn: number; now?: () => string },
): FailureObservation[] {
	const now = opts.now ?? (() => new Date().toISOString());
	const observations: FailureObservation[] = [];
	for (let entryIndex = Math.max(0, opts.fromEntryIndex); entryIndex < messages.length; entryIndex++) {
		const message = messages[entryIndex];
		if (message.role === "toolResult") {
			const observation = observeToolResult(message as ToolResultMessage, entryIndex, opts.turn, now);
			if (observation) observations.push(observation);
		} else if (message.role === "assistant") {
			const observation = observeAssistant(message as AssistantMessage, entryIndex, opts.turn, now);
			if (observation) observations.push(observation);
		}
	}
	return observations;
}

/**
 * The fingerprint a tool result carries, or undefined when it is not a failure.
 *
 * Extracted so the `tool.execute` span attribute and the ledger cannot drift:
 * the whole point of stamping the fingerprint on the span is that the span key
 * and the ledger key are the same string, and two copies of these rules would
 * silently stop being the same string.
 */
export function fingerprintToolResultText(
	toolName: string | undefined,
	text: string,
	isError: boolean,
): FailureFingerprint | undefined {
	const traceback = parsePythonTraceback(text);
	if (traceback) {
		return fingerprintFailure(
			"python_exception",
			traceback.skillName ?? toolName,
			traceback.exceptionClass,
			traceback.message,
		);
	}
	if (isError) {
		return fingerprintFailure(
			"tool_error",
			toolName,
			undefined,
			text.trim() || "tool returned an error without output",
		);
	}
	return undefined;
}

/** The kernel tool: its error details carry the traceback of the exception a cell itself raised. */
const IPYTHON_TOOL_NAME = "ipython";

/**
 * The traceback of the exception an ipython cell raised, read from the kernel's
 * error details rather than from the output text, and only when it fingerprints
 * the same failure the text does. Anything a cell printed, a page a tool
 * fetched, or a file it read can contain a traceback; none of it may name what
 * a replay case imports.
 */
function kernelCellTraceback(message: ToolResultMessage, fingerprint: FailureFingerprint): ParsedTraceback | undefined {
	if (message.toolName !== IPYTHON_TOOL_NAME) return undefined;
	const details: unknown = message.details;
	if (typeof details !== "object" || details === null || Array.isArray(details)) return undefined;
	const { status, error } = details as { status?: unknown; error?: unknown };
	if (status !== "error" || typeof error !== "object" || error === null || Array.isArray(error)) return undefined;
	const { ename, traceback } = error as { ename?: unknown; traceback?: unknown };
	if (typeof ename !== "string" || !Array.isArray(traceback)) return undefined;
	if (!traceback.every((line): line is string => typeof line === "string")) return undefined;
	const parsed = parsePythonTraceback(traceback.join("\n"));
	if (!parsed || parsed.exceptionClass.split(".").at(-1) !== ename) return undefined;
	const kernel = fingerprintFailure(
		"python_exception",
		parsed.skillName ?? message.toolName,
		parsed.exceptionClass,
		parsed.message,
	);
	return kernel.id === fingerprint.id ? parsed : undefined;
}

function observeToolResult(
	message: ToolResultMessage,
	entryIndex: number,
	turn: number,
	now: () => string,
): FailureObservation | undefined {
	const text = messageText(message);
	const traceback = parsePythonTraceback(text);
	if (traceback) {
		const fingerprint = fingerprintFailure(
			"python_exception",
			traceback.skillName ?? message.toolName,
			traceback.exceptionClass,
			traceback.message,
		);
		const kernelTraceback = kernelCellTraceback(message, fingerprint);
		const replayCase = kernelTraceback ? deriveReplayCase(fingerprint, kernelTraceback.excerpt) : undefined;
		return {
			fingerprint,
			excerpt: traceback.excerpt,
			entryIndex,
			turn,
			at: now(),
			...(replayCase === undefined ? {} : { replayCase }),
		};
	}
	if (message.isError) {
		const raw = text.trim() || "tool returned an error without output";
		return {
			fingerprint: fingerprintFailure("tool_error", message.toolName, undefined, raw),
			excerpt: clipExcerpt(raw),
			entryIndex,
			turn,
			at: now(),
		};
	}
	return undefined;
}

function observeAssistant(
	message: AssistantMessage,
	entryIndex: number,
	turn: number,
	now: () => string,
): FailureObservation | undefined {
	if (message.stopReason !== "error") return undefined;
	const raw = message.errorMessage?.trim() || message.stopReasonRaw || "provider returned an error";
	const source = typeof message.provider === "string" && message.provider ? message.provider : undefined;
	return {
		fingerprint: fingerprintFailure("provider_error", source, undefined, raw),
		excerpt: clipExcerpt(raw),
		entryIndex,
		turn,
		at: now(),
	};
}

/**
 * Fold observations into a ledger. `newlyRecurring` holds the records this
 * update moved into the recurring set (`recurringFailures`): at or over the
 * threshold and actionable after it, not both before it. A record can enter by
 * crossing the threshold or, already over it, by its occurrences turning into
 * an actionable majority.
 */
export function updateFailureLedger(
	ledger: FailureLedger,
	observations: readonly FailureObservation[],
	opts: { threshold?: number; scannedThroughEntryIndex?: number } = {},
): { ledger: FailureLedger; newlyRecurring: FailureRecord[] } {
	const threshold = Math.max(1, opts.threshold ?? DEFAULT_RECURRENCE_THRESHOLD);
	const failures: Record<string, FailureRecord> = {};
	for (const [id, record] of Object.entries(ledger.failures)) {
		// The tally is derived before an observation overwrites the excerpt a legacy record is read from.
		failures[id] = withNonActionableCount(
			withReplayCases(
				{ ...record, addressedByProposalIds: [...record.addressedByProposalIds] },
				replayCasesOf(record),
			),
			nonActionableCountOf(record),
		);
	}
	const recurringBefore = new Map<string, boolean>();
	let lastScannedEntryIndex = ledger.lastScannedEntryIndex;
	for (const observation of observations) {
		const id = observation.fingerprint.id;
		const existing = failures[id];
		if (!recurringBefore.has(id)) recurringBefore.set(id, existing !== undefined && isRecurring(existing, threshold));
		const nonActionable = classifiesNonActionable(observation);
		if (existing) {
			existing.count += 1;
			existing.lastSeenTurn = Math.max(existing.lastSeenTurn, observation.turn);
			existing.lastSeenAt = observation.at;
			existing.excerpt = observation.excerpt;
			if (nonActionable) existing.nonActionableCount = (existing.nonActionableCount ?? 0) + 1;
			if (observation.replayCase) {
				failures[id] = withReplayCases(
					existing,
					mergeReplayCase(existing.replayCases ?? [], observation.replayCase),
				);
			}
		} else {
			failures[id] = withReplayCases(
				{
					fingerprint: { ...observation.fingerprint },
					count: 1,
					firstSeenTurn: observation.turn,
					lastSeenTurn: observation.turn,
					firstSeenAt: observation.at,
					lastSeenAt: observation.at,
					excerpt: observation.excerpt,
					addressedByProposalIds: [],
					...(nonActionable ? { nonActionableCount: 1 } : {}),
				},
				observation.replayCase === undefined ? [] : [observation.replayCase],
			);
		}
		lastScannedEntryIndex = Math.max(lastScannedEntryIndex, observation.entryIndex + 1);
	}
	if (opts.scannedThroughEntryIndex !== undefined) {
		lastScannedEntryIndex = Math.max(lastScannedEntryIndex, opts.scannedThroughEntryIndex);
	}
	const newlyRecurring: FailureRecord[] = [];
	for (const [id, before] of recurringBefore) {
		const record = failures[id];
		if (!before && isRecurring(record, threshold)) newlyRecurring.push(record);
	}
	sortRecords(newlyRecurring);
	return { ledger: { schema: 1, failures, lastScannedEntryIndex }, newlyRecurring };
}

/**
 * Fold observations into a ledger that spans sessions. Counts add and turn
 * bounds widen exactly as in `updateFailureLedger`, but `lastScannedEntryIndex`
 * is a cursor into one session's branch and stays with the session that owns
 * it. Callers hold the harness state lock around the surrounding
 * read-modify-write so a concurrent flush cannot drop the other's records.
 */
export function mergeFailureObservations(
	ledger: FailureLedger,
	observations: readonly FailureObservation[],
	opts: { threshold?: number } = {},
): { ledger: FailureLedger; newlyRecurring: FailureRecord[] } {
	const updated = updateFailureLedger(ledger, observations, opts);
	return {
		ledger: { ...updated.ledger, lastScannedEntryIndex: ledger.lastScannedEntryIndex },
		newlyRecurring: updated.newlyRecurring,
	};
}

function sortRecords(records: FailureRecord[]): void {
	records.sort(
		(a, b) =>
			b.count - a.count || b.lastSeenTurn - a.lastSeenTurn || a.fingerprint.id.localeCompare(b.fingerprint.id),
	);
}

function isRecurring(record: FailureRecord, threshold: number): boolean {
	return record.count >= threshold && isActionableFailure(record);
}

/** Actionable failures at or over the threshold: the refine triggers and the gate's failure opponents. */
export function recurringFailures(ledger: FailureLedger, threshold = DEFAULT_RECURRENCE_THRESHOLD): FailureRecord[] {
	const records = Object.values(ledger.failures).filter((record) => isRecurring(record, threshold));
	sortRecords(records);
	return records;
}

export function formatFailureLedgerForPrompt(records: readonly FailureRecord[], limit = 12): string {
	if (records.length === 0) return "None.";
	const lines = records.slice(0, Math.max(0, limit)).map((record) => {
		const fp = record.fingerprint;
		const parts = [`- ${failureOpponentId(fp)} [${fp.kind}]`];
		if (fp.source) parts.push(`source=${fp.source}`);
		if (fp.exceptionClass) parts.push(`class=${fp.exceptionClass}`);
		parts.push(`count=${record.count}`, `turns=${record.firstSeenTurn}..${record.lastSeenTurn}`);
		const verified = verifiedReplayCasesOf(record);
		const replay = verified.at(-1);
		if (replay) parts.push("replay=verified");
		const excerpt = record.excerpt.replace(/\s+/g, " ").trim();
		const lines = [`${parts.join(" ")}\n  ${excerpt.length > 240 ? `${excerpt.slice(0, 240)}...` : excerpt}`];
		if (replay) lines.push(`  replay case (re-run to check the fix): ${replay.source.replace(/\n/g, "; ")}`);
		return lines.join("\n");
	});
	if (records.length > limit) lines.push(`- ... ${records.length - limit} more`);
	return lines.join("\n");
}

interface ProvisionalChampionView {
	proposalId?: unknown;
	claimedFingerprints?: unknown;
	provisional?: unknown;
}

/**
 * Find provisional champions whose claimed fingerprints recurred at `ordinal`
 * inside their observation window. Reads the lineage defensively so a state
 * written before the provisional fields existed is treated as non-provisional.
 *
 * `ordinal` is read off `clock`, and only windows stamped with that same clock
 * are examined: the global and local ledger ordinals advance independently, so
 * a window opened on one says nothing about a position on the other. A legacy
 * window was opened on a per-session turn count that restarts every session, so
 * comparing it with anything is noise and it never regresses.
 */
export function findProvisionalRegressions(
	ravo: RavoState<JsonValue> | undefined,
	recurredFingerprintIds: readonly string[],
	ordinal: number,
	clock: RavoWindowClock = "ordinal",
): ProvisionalRegression[] {
	if (!ravo || !Array.isArray(ravo.lineage) || recurredFingerprintIds.length === 0) return [];
	const recurred = new Set(recurredFingerprintIds);
	const regressions: ProvisionalRegression[] = [];
	for (const champion of ravo.lineage as ProvisionalChampionView[]) {
		if (typeof champion.proposalId !== "string") continue;
		const window = champion.provisional;
		if (typeof window !== "object" || window === null || Array.isArray(window)) continue;
		const {
			committedTurn,
			untilTurn,
			clock: windowClock,
		} = window as {
			committedTurn?: unknown;
			untilTurn?: unknown;
			clock?: unknown;
		};
		if (windowClock !== clock || typeof committedTurn !== "number" || typeof untilTurn !== "number") continue;
		if (ordinal < committedTurn || ordinal > untilTurn) continue;
		const claimed = Array.isArray(champion.claimedFingerprints)
			? champion.claimedFingerprints.filter((id): id is string => typeof id === "string")
			: [];
		const fingerprints = [...new Set(claimed.filter((id) => recurred.has(id)))].sort();
		if (fingerprints.length === 0) continue;
		regressions.push({ championId: champion.proposalId, fingerprints, committedTurn, untilTurn });
	}
	return regressions;
}

/** Record an observed recurrence on the regressed champions without touching lineage order or scores. */
export function recordProvisionalRegressions(
	ravo: RavoState<JsonValue>,
	regressions: readonly ProvisionalRegression[],
	turn: number,
): RavoState<JsonValue> {
	if (regressions.length === 0) return ravo;
	const byChampion = new Map(regressions.map((regression) => [regression.championId, regression.fingerprints]));
	const lineage = ravo.lineage.map((champion) => {
		const fingerprints = byChampion.get(champion.proposalId);
		const window = (champion as ProvisionalChampionView).provisional;
		if (!fingerprints || typeof window !== "object" || window === null || Array.isArray(window)) return champion;
		const { committedTurn, untilTurn } = window as { committedTurn?: unknown; untilTurn?: unknown };
		if (typeof committedTurn !== "number" || typeof untilTurn !== "number") return champion;
		return {
			...champion,
			provisional: { ...window, committedTurn, untilTurn, observedRecurrence: { turn, fingerprints } },
		};
	});
	return { ...ravo, lineage };
}

export function formatRecurrenceRefineInstructions(records: readonly FailureRecord[]): string {
	return [
		"Automatic refine triggered by recurrence: the failure fingerprints below recurred (count >= threshold).",
		"Your edits must target the cause: fix the skill, prompt note, or memory that lets the failure repeat. The evaluator decides which fingerprints your edits address; a proposal that addresses none of them is rejected.",
		"A fingerprint marked replay=verified is re-checked by re-running its replay case when a skill you write imports the module that case probes, and every claimed fix is checked by whether the failure recurs afterwards.",
		"Do not propose unrelated or speculative edits. Do not promote anything global unless explicitly requested.",
		"Recurring failures:",
		formatFailureLedgerForPrompt(records),
	].join("\n");
}

export function formatRegressionRefineInstructions(
	regressions: readonly ProvisionalRegression[],
	records: readonly FailureRecord[],
): string {
	const lines = regressions.map(
		(regression) =>
			`- proposal ${regression.championId} (observation window ${regression.committedTurn}..${regression.untilTurn}) claimed to address: ${regression.fingerprints.map((id) => failureOpponentId(id)).join(", ")}`,
	);
	return [
		"Automatic refine triggered by regression: a provisional refinement claimed to address failure fingerprints, but they recurred inside its observation window. The judge said pass; the outcome says fail.",
		"Propose a repair: correct or replace the committed edits so the fingerprints below stop recurring. The evaluator decides which fingerprints the repair addresses, so the edits must target the cause. This repair goes through the normal gate; never bypass it.",
		"Regressed provisional refinements:",
		...lines,
		"Recurred failures:",
		formatFailureLedgerForPrompt(records),
	].join("\n");
}

/** A replay case that reproduced its recorded exception when the self-check ran it. */
export interface ReplayVerification {
	fingerprintId: string;
	source: string;
	verifiedAt: string;
}

/**
 * Mark the matching unverified cases verified. Pure; a verification whose
 * record or case is gone (evicted, dropped as not a valid probe, or never
 * merged into this ledger) is ignored, and an already verified case keeps its
 * original timestamp. Only a record it rewrites is pruned, and the same ledger
 * comes back when nothing matched.
 */
export function applyReplayVerifications(
	ledger: FailureLedger,
	verifications: readonly ReplayVerification[],
): FailureLedger {
	let failures: Record<string, FailureRecord> | undefined;
	for (const verification of verifications) {
		const record = (failures ?? ledger.failures)[verification.fingerprintId];
		if (!record) continue;
		const cases = replayCasesOf(record);
		const index = cases.findIndex((replay) => replay.source === verification.source && !replay.verifiedAt);
		if (index === -1) continue;
		cases[index] = { ...cases[index], verifiedAt: verification.verifiedAt };
		failures ??= { ...ledger.failures };
		failures[verification.fingerprintId] = withReplayCases(record, cases);
	}
	return failures ? { ...ledger, failures } : ledger;
}
