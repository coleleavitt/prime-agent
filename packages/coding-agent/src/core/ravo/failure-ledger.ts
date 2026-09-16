import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, ToolResultMessage } from "@earendil-works/pi-ai";
import { canonicalJson, sha256 } from "./canonical-json.js";
import type { JsonValue, RavoState } from "./reducer.js";
import { deriveReplayCase, normalizeReplayCase, type ReplayCase } from "./referee.js";

/**
 * Failure ledger: fingerprints observed runtime failures (Python tracebacks in
 * tool output, tool results flagged `isError`, provider errors) and counts them
 * per session so a recurring failure can trigger a deterministic /refine round
 * that must address it. Fingerprints double as RAVO opponent criterion ids
 * (`FAILURE_OPPONENT_PREFIX + id`).
 *
 * A record may also carry a REPLAY CASE: an executable reproduction of the
 * failure, derived from the traceback here and confirmed by the referee's
 * self-check (`referee-runner.ts`) before it counts as evidence. It is what
 * lets a proposal's claim to have fixed the failure be refuted by execution
 * instead of believed. The field is additive: ledgers written before it load
 * unchanged and simply have no evidence to adjudicate against.
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
	/** Derived reproduction; `verifiedAt` is set only by the referee's self-check. */
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
	/** Executable reproduction the referee adjudicates a claim against. */
	replayCase?: ReplayCase;
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
 * that committed it ended. Off unless `PRIME_AGENT_GLOBAL_LEDGER=1`: promoting
 * the ledger changes which sessions trigger a refine.
 */
export function globalFailureLedgerEnabled(env: Record<string, string | undefined> = process.env): boolean {
	const value = env[GLOBAL_FAILURE_LEDGER_ENV]?.trim().toLowerCase();
	return value === "1" || value === "on" || value === "true" || value === "yes";
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
	const replayCase = normalizeReplayCase(raw.replayCase);
	return {
		fingerprint,
		count,
		firstSeenTurn: safeTurn(raw.firstSeenTurn),
		lastSeenTurn: safeTurn(raw.lastSeenTurn),
		firstSeenAt: typeof raw.firstSeenAt === "string" ? raw.firstSeenAt : "",
		lastSeenAt: typeof raw.lastSeenAt === "string" ? raw.lastSeenAt : "",
		excerpt: typeof raw.excerpt === "string" ? raw.excerpt : "",
		addressedByProposalIds: Array.isArray(raw.addressedByProposalIds)
			? raw.addressedByProposalIds.filter((item): item is string => typeof item === "string")
			: [],
		...(replayCase === undefined ? {} : { replayCase }),
	};
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
		const replayCase = deriveReplayCase(fingerprint, traceback.excerpt);
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

export function updateFailureLedger(
	ledger: FailureLedger,
	observations: readonly FailureObservation[],
	opts: { threshold?: number; scannedThroughEntryIndex?: number } = {},
): { ledger: FailureLedger; newlyRecurring: FailureRecord[] } {
	const threshold = Math.max(1, opts.threshold ?? DEFAULT_RECURRENCE_THRESHOLD);
	const failures: Record<string, FailureRecord> = {};
	for (const [id, record] of Object.entries(ledger.failures)) {
		failures[id] = { ...record, addressedByProposalIds: [...record.addressedByProposalIds] };
	}
	const countBefore = new Map<string, number>();
	let lastScannedEntryIndex = ledger.lastScannedEntryIndex;
	for (const observation of observations) {
		const id = observation.fingerprint.id;
		const existing = failures[id];
		if (!countBefore.has(id)) countBefore.set(id, existing?.count ?? 0);
		if (existing) {
			existing.count += 1;
			existing.lastSeenTurn = Math.max(existing.lastSeenTurn, observation.turn);
			existing.lastSeenAt = observation.at;
			existing.excerpt = observation.excerpt;
			// A case that has already reproduced is evidence; never replace it
			// with a fresh derivation that has not been self-checked.
			if (observation.replayCase && !existing.replayCase?.verifiedAt) {
				existing.replayCase = observation.replayCase;
			}
		} else {
			failures[id] = {
				fingerprint: { ...observation.fingerprint },
				count: 1,
				firstSeenTurn: observation.turn,
				lastSeenTurn: observation.turn,
				firstSeenAt: observation.at,
				lastSeenAt: observation.at,
				excerpt: observation.excerpt,
				addressedByProposalIds: [],
				...(observation.replayCase === undefined ? {} : { replayCase: observation.replayCase }),
			};
		}
		lastScannedEntryIndex = Math.max(lastScannedEntryIndex, observation.entryIndex + 1);
	}
	if (opts.scannedThroughEntryIndex !== undefined) {
		lastScannedEntryIndex = Math.max(lastScannedEntryIndex, opts.scannedThroughEntryIndex);
	}
	const newlyRecurring: FailureRecord[] = [];
	for (const [id, before] of countBefore) {
		const record = failures[id];
		if (before < threshold && threshold <= record.count) newlyRecurring.push(record);
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

export function recurringFailures(ledger: FailureLedger, threshold = DEFAULT_RECURRENCE_THRESHOLD): FailureRecord[] {
	const records = Object.values(ledger.failures).filter((record) => record.count >= threshold);
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
		const replay = record.replayCase?.verifiedAt ? record.replayCase : undefined;
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
 * Find provisional champions whose claimed fingerprints recurred at `turn`
 * inside their observation window. Reads the lineage defensively so a state
 * written before the provisional fields existed is treated as non-provisional.
 */
export function findProvisionalRegressions(
	ravo: RavoState<JsonValue> | undefined,
	recurredFingerprintIds: readonly string[],
	turn: number,
): ProvisionalRegression[] {
	if (!ravo || !Array.isArray(ravo.lineage) || recurredFingerprintIds.length === 0) return [];
	const recurred = new Set(recurredFingerprintIds);
	const regressions: ProvisionalRegression[] = [];
	for (const champion of ravo.lineage as ProvisionalChampionView[]) {
		if (typeof champion.proposalId !== "string") continue;
		const window = champion.provisional;
		if (typeof window !== "object" || window === null || Array.isArray(window)) continue;
		const { committedTurn, untilTurn } = window as { committedTurn?: unknown; untilTurn?: unknown };
		if (typeof committedTurn !== "number" || typeof untilTurn !== "number") continue;
		if (turn < committedTurn || turn > untilTurn) continue;
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
		"Automatic refine triggered by recurrence: the failure fingerprints below recurred in this session (count >= threshold).",
		"Your proposal MUST address these fingerprints: fix the skill, prompt note, or memory that lets the failure repeat, and set addressedFingerprints to the ids you addressed.",
		"A fingerprint marked replay=verified is adjudicated by re-running its replay case: claiming it while the recorded exception still reproduces is a rejected proposal, not a stricter one.",
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
			`- proposal ${regression.championId} (window turns ${regression.committedTurn}..${regression.untilTurn}) claimed to address: ${regression.fingerprints.map((id) => failureOpponentId(id)).join(", ")}`,
	);
	return [
		"Automatic refine triggered by regression: a provisional refinement claimed to address failure fingerprints, but they recurred inside its observation window. The judge said pass; the outcome says fail.",
		"Propose a repair: correct or replace the committed edits so the fingerprints below stop recurring, and set addressedFingerprints to the ids you repaired. This repair goes through the normal gate; never bypass it.",
		"Regressed provisional refinements:",
		...lines,
		"Recurred failures:",
		formatFailureLedgerForPrompt(records),
	].join("\n");
}
