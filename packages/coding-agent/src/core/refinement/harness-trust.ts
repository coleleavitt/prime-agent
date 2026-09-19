import type { RefereeVerdictStatus } from "../ravo/referee.js";

/**
 * Trust and dormancy on continual-harness entries.
 *
 * Every entry carries a trust score. It moves on measured outcomes only:
 *
 * - a provisional window that closes with nothing recurring is `+5`;
 * - a MEASURED FAULT is `-15`. "Measured" means an upheld referee verdict
 *   (`ravo/referee.ts`), i.e. a recorded replay case re-executed in a
 *   subprocess and the recorded exception recurring. A judge's opinion, a
 *   proposal's own claim, and a heuristic all count for nothing here.
 *
 * Below {@link DORMANT_TRUST_THRESHOLD} an entry goes dormant: it is dropped
 * from the rendered prompt but never deleted, and stays fully readable and
 * editable through the harness CRUD surface. Eviction is from attention, not
 * from the store.
 *
 * ATTRIBUTION IS THE WHOLE DIFFICULTY. "Every entry that was in context when
 * something went wrong gets debited" is not a measurement, it is a rumour: the
 * prompt carries six entries on every turn and none of them caused the
 * traceback. `trustWindows` records at commit time what a refinement wrote:
 * proposal id, the `kind:id` of every entry it touched, the fingerprints the
 * gate accepted it as addressing, and the imports each skill it wrote names.
 *
 * - A replay probes an import, so it is evidence only about the skill entry it
 *   ran for. A debit is charged to that entry, once per window, and never to a
 *   memory, prompt or subagent entry the same commit wrote. A faulted window
 *   records the entries it charged (`faultedEntries`): another skill of the
 *   same commit whose own verdict lands in a later flush is still charged.
 * - A claimed failure that recurs inside the window without an upheld verdict
 *   closes the window `contested`: no credit, no debit.
 * - An entry whose imports changed since the commit, or that a newer
 *   overlapping window wrote with the same imports, is superseded for that
 *   window and never adjudicated against it. The imports are checked again
 *   when a verdict is recorded, since the skill can be rewritten while its
 *   replay runs.
 * - A late fault on a window that already closed clean leaves the `+5` the
 *   clean close granted to its co-touched entries.
 *
 * Rocq `Ravo.v` Section 15 requires the asymmetry to be strict: a measured
 * fault must leave an entry strictly less trusted than a clean window does,
 * at every reachable score. {@link faultIsStrictlyWorseThanCleanWindow} states
 * it; the clamp is monotone and the two deltas have opposite signs, so it holds
 * on all of [0, 100] rather than only in the interior.
 */

export const DEFAULT_ENTRY_TRUST = 50;
export const MIN_ENTRY_TRUST = 0;
export const MAX_ENTRY_TRUST = 100;
/** Credit for a provisional window that closed with nothing recurring. */
export const CLEAN_WINDOW_CREDIT = 5;
/** Debit for an upheld referee verdict attributable to the entry. */
export const MEASURED_FAULT_DEBIT = 15;
/** Strictly below this an entry is dormant: readable, but not rendered. */
export const DORMANT_TRUST_THRESHOLD = 30;
/** Replays recorded per (window, entry, fingerprint) before it is never re-run. */
export const MAX_TRUST_ADJUDICATION_RUNS = 3;

const MAX_TRUST_EVENTS = 20;
const MAX_SETTLED_TRUST_WINDOWS = 100;
const MAX_TRUST_ADJUDICATIONS_PER_WINDOW = 32;
const ENTRY_REF_SEPARATOR = ":";
const SKILL_REF_PREFIX = `skill${ENTRY_REF_SEPARATOR}`;

export type HarnessTrustReason = "clean_window" | "measured_fault";
/**
 * `unmeasured`: a window that claimed nothing, settled on load without credit.
 * `contested`: a claimed failure recurred in the window without an upheld verdict.
 */
export type HarnessTrustOutcome = "open" | "clean" | "contested" | "faulted" | "unmeasured";

export type TrustAdjudicationStatus = Extract<RefereeVerdictStatus, "upheld" | "cleared" | "unverifiable">;

export interface HarnessTrustEvent {
	reason: HarnessTrustReason;
	delta: number;
	/** Score after the delta was clamped and applied. */
	score: number;
	at: string;
	/** Refinement that committed the entry this event is attributed to. */
	proposalId: string;
	/** Fingerprint whose upheld verdict produced a measured fault. */
	fingerprintId?: string;
}

export interface HarnessEntryTrust {
	score: number;
	updated_at: string;
	events: HarnessTrustEvent[];
}

/** The replays recorded for one touched skill entry and one claimed fingerprint of a window. */
export interface HarnessTrustAdjudication {
	/** A touched `skill:<id>`. */
	entry: string;
	/** A claimed fingerprint. */
	fingerprintId: string;
	/** Strongest verdict seen: upheld > cleared > unverifiable. */
	status: TrustAdjudicationStatus;
	/** Earliest in-window recurrence ordinal that prompted a run. */
	ordinal: number;
	/** When each run finished, sorted, at most {@link MAX_TRUST_ADJUDICATION_RUNS}. */
	runs: string[];
}

/**
 * The attribution record for one committed refinement. Keyed by proposal id in
 * `HarnessState.trustWindows`; the shape matches the orphaned `trustWindows`
 * key already present in session harness states on this machine.
 */
export interface HarnessTrustWindow {
	proposalId: string;
	/** `kind:id` of every entry the commit created or updated. */
	touched: string[];
	/** Fingerprints the gate accepted the commit as addressing. */
	claimedFingerprints: string[];
	committedTurn: number;
	untilTurn: number;
	outcome: HarnessTrustOutcome;
	settledTurn?: number;
	/** Claimed fingerprints an upheld referee verdict refuted. */
	faultedFingerprints?: string[];
	/**
	 * Skill entries a fault on this window has been charged to. Absent on a
	 * faulted window, every touched entry counts as charged, as older builds did.
	 */
	faultedEntries?: string[];
	/** Touched skill ref -> the imports the commit wrote for it. A skill without one is never adjudicated. */
	skillImports?: Record<string, string[]>;
	/** Claimed fingerprint -> earliest in-window ordinal it recurred at. */
	recurrences?: Record<string, number>;
	adjudications?: HarnessTrustAdjudication[];
}

export type HarnessTrustWindows = Record<string, HarnessTrustWindow>;

/** Evidence a session observed about a window, recorded at the next flush or apply of its scope. */
export type TrustWindowEvidence =
	| { type: "recurrence"; proposalId: string; fingerprintId: string; ordinal: number }
	| {
			type: "adjudication";
			proposalId: string;
			entry: string;
			fingerprintId: string;
			status: TrustAdjudicationStatus;
			ordinal: number;
			at: string;
	  };

/** Resolve an entry so a window can be settled against it; `undefined` when it is gone. */
export type TrustLookup = (kind: string, id: string) => { trust?: HarnessEntryTrust } | undefined;

export interface HarnessTrustAdjustment {
	kind: string;
	id: string;
	reason: HarnessTrustReason;
	delta: number;
	before: number;
	after: number;
	/** The record the caller stores back on the entry. */
	trust: HarnessEntryTrust;
	proposalId: string;
	fingerprintId?: string;
}

export interface TrustWindowSettlement {
	proposalId: string;
	from: HarnessTrustOutcome;
	outcome: "clean" | "contested" | "faulted";
	turn: number;
	/** Faulted: the upheld fingerprints. Contested: the recurred ones. Clean: every claimed one. */
	fingerprints: string[];
}

export interface TrustSettlement {
	windows: HarnessTrustWindows;
	adjustments: HarnessTrustAdjustment[];
	settled: TrustWindowSettlement[];
}

export function harnessEntryRef(kind: string, id: string): string {
	return `${kind}${ENTRY_REF_SEPARATOR}${id}`;
}

export function parseHarnessEntryRef(ref: string): { kind: string; id: string } | undefined {
	const separator = ref.indexOf(ENTRY_REF_SEPARATOR);
	if (separator <= 0 || separator === ref.length - 1) return undefined;
	return { kind: ref.slice(0, separator), id: ref.slice(separator + 1) };
}

export function clampTrust(score: number): number {
	if (!Number.isFinite(score)) return DEFAULT_ENTRY_TRUST;
	return Math.min(MAX_ENTRY_TRUST, Math.max(MIN_ENTRY_TRUST, Math.round(score)));
}

export function emptyEntryTrust(at: string): HarnessEntryTrust {
	return { score: DEFAULT_ENTRY_TRUST, updated_at: at, events: [] };
}

/** An entry with no trust record is a fully trusted entry, not an untrusted one. */
export function entryTrustScore(trust: HarnessEntryTrust | undefined): number {
	return trust === undefined ? DEFAULT_ENTRY_TRUST : clampTrust(trust.score);
}

export function isDormantTrust(trust: HarnessEntryTrust | undefined): boolean {
	return entryTrustScore(trust) < DORMANT_TRUST_THRESHOLD;
}

/** Rocq `Ravo.v` Section 15: the fault must bite strictly harder than the credit rewards. */
export function faultIsStrictlyWorseThanCleanWindow(score: number): boolean {
	return clampTrust(score - MEASURED_FAULT_DEBIT) < clampTrust(score + CLEAN_WINDOW_CREDIT);
}

function normalizeTrustEvent(value: unknown): HarnessTrustEvent | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const raw = value as Record<string, unknown>;
	if (raw.reason !== "clean_window" && raw.reason !== "measured_fault") return undefined;
	if (typeof raw.delta !== "number" || !Number.isFinite(raw.delta)) return undefined;
	if (typeof raw.score !== "number" || !Number.isFinite(raw.score)) return undefined;
	return {
		reason: raw.reason,
		delta: Math.round(raw.delta),
		score: clampTrust(raw.score),
		at: typeof raw.at === "string" ? raw.at : "",
		proposalId: typeof raw.proposalId === "string" ? raw.proposalId : "",
		...(typeof raw.fingerprintId === "string" && raw.fingerprintId ? { fingerprintId: raw.fingerprintId } : {}),
	};
}

export function normalizeEntryTrust(value: unknown): HarnessEntryTrust | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const raw = value as Record<string, unknown>;
	if (typeof raw.score !== "number" || !Number.isFinite(raw.score)) return undefined;
	const events = Array.isArray(raw.events)
		? raw.events.map(normalizeTrustEvent).filter((event): event is HarnessTrustEvent => event !== undefined)
		: [];
	return {
		score: clampTrust(raw.score),
		updated_at: typeof raw.updated_at === "string" ? raw.updated_at : "",
		events: events.slice(-MAX_TRUST_EVENTS),
	};
}

function stringList(value: unknown): string[] {
	return Array.isArray(value)
		? value.filter((item): item is string => typeof item === "string" && item.length > 0)
		: [];
}

function sortedUnique(values: Iterable<string>): string[] {
	return [...new Set(values)].sort((left, right) => left.localeCompare(right));
}

function isSafeNatural(value: unknown): value is number {
	return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function safeTurn(value: unknown): number {
	return isSafeNatural(value) ? value : 0;
}

function ownRecord(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function isAdjudicationStatus(value: unknown): value is TrustAdjudicationStatus {
	return value === "upheld" || value === "cleared" || value === "unverifiable";
}

const ADJUDICATION_RANK: Record<TrustAdjudicationStatus, number> = { unverifiable: 0, cleared: 1, upheld: 2 };

function inWindow(window: Pick<HarnessTrustWindow, "committedTurn" | "untilTurn">, ordinal: number): boolean {
	return isSafeNatural(ordinal) && ordinal >= window.committedTurn && ordinal <= window.untilTurn;
}

function isAdjudicableEntry(window: Pick<HarnessTrustWindow, "touched">, entry: string): boolean {
	return entry.startsWith(SKILL_REF_PREFIX) && window.touched.includes(entry);
}

function isChargedEntry(window: Pick<HarnessTrustWindow, "outcome" | "faultedEntries">, entry: string): boolean {
	return window.outcome === "faulted" && (window.faultedEntries?.includes(entry) ?? true);
}

function sameModules(left: readonly string[], right: readonly string[]): boolean {
	const modules = new Set(left);
	return modules.size === new Set(right).size && right.every((module) => modules.has(module));
}

function normalizeSkillImports(touched: readonly string[], value: unknown): Record<string, string[]> | undefined {
	const raw = ownRecord(value);
	if (!raw) return undefined;
	const imports: Record<string, string[]> = {};
	for (const [ref, list] of Object.entries(raw)) {
		if (!ref.startsWith(SKILL_REF_PREFIX) || !touched.includes(ref)) continue;
		const modules = sortedUnique(stringList(list));
		if (modules.length > 0) imports[ref] = modules;
	}
	return Object.keys(imports).length > 0 ? imports : undefined;
}

/**
 * Fold runs into an adjudication: the higher status, the earlier ordinal, and
 * the earliest runs of the union, so the result does not depend on the order
 * runs were recorded in. A status upgrade is kept even once the runs are full.
 */
function mergeAdjudication(
	existing: HarnessTrustAdjudication | undefined,
	incoming: HarnessTrustAdjudication,
): HarnessTrustAdjudication {
	const base = existing ?? incoming;
	return {
		entry: base.entry,
		fingerprintId: base.fingerprintId,
		status: ADJUDICATION_RANK[incoming.status] > ADJUDICATION_RANK[base.status] ? incoming.status : base.status,
		ordinal: Math.min(base.ordinal, incoming.ordinal),
		runs: sortedUnique([...(existing?.runs ?? []), ...incoming.runs]).slice(0, MAX_TRUST_ADJUDICATION_RUNS),
	};
}

function compareAdjudications(left: HarnessTrustAdjudication, right: HarnessTrustAdjudication): number {
	return left.entry.localeCompare(right.entry) || left.fingerprintId.localeCompare(right.fingerprintId);
}

/** Merge a run into a window's list, keyed by (entry, fingerprint); a new pair past the cap is dropped. */
function withAdjudication(
	adjudications: readonly HarnessTrustAdjudication[],
	incoming: HarnessTrustAdjudication,
): HarnessTrustAdjudication[] {
	const index = adjudications.findIndex(
		(item) => item.entry === incoming.entry && item.fingerprintId === incoming.fingerprintId,
	);
	if (index === -1) {
		if (adjudications.length >= MAX_TRUST_ADJUDICATIONS_PER_WINDOW) return [...adjudications];
		return [...adjudications, mergeAdjudication(undefined, incoming)].sort(compareAdjudications);
	}
	const next = [...adjudications];
	next[index] = mergeAdjudication(adjudications[index], incoming);
	return next;
}

function normalizeAdjudications(
	window: Pick<HarnessTrustWindow, "touched" | "claimedFingerprints" | "committedTurn" | "untilTurn">,
	value: unknown,
): HarnessTrustAdjudication[] {
	if (!Array.isArray(value)) return [];
	let adjudications: HarnessTrustAdjudication[] = [];
	for (const item of value) {
		const raw = ownRecord(item);
		if (!raw) continue;
		const { entry, fingerprintId, status, ordinal } = raw;
		if (typeof entry !== "string" || !isAdjudicableEntry(window, entry)) continue;
		if (typeof fingerprintId !== "string" || !window.claimedFingerprints.includes(fingerprintId)) continue;
		if (!isAdjudicationStatus(status) || typeof ordinal !== "number" || !inWindow(window, ordinal)) continue;
		if (!Array.isArray(raw.runs) || raw.runs.length === 0) continue;
		if (!raw.runs.every((at): at is string => typeof at === "string" && at.length > 0)) continue;
		adjudications = withAdjudication(adjudications, { entry, fingerprintId, status, ordinal, runs: raw.runs });
	}
	return adjudications;
}

/**
 * A window that claims no fingerprint can never be refuted, so letting it
 * close would credit an entry for nothing. Windows written before claims were
 * recorded (outcome `success`, no `claimedFingerprints`) are exactly that:
 * they load settled, earn nothing, and never reopen. Malformed attribution or
 * evidence is dropped piecewise; the window itself survives.
 */
function normalizeTrustWindow(key: string, value: unknown): HarnessTrustWindow | undefined {
	const raw = ownRecord(value);
	if (!raw) return undefined;
	const proposalId = typeof raw.proposalId === "string" && raw.proposalId ? raw.proposalId : key;
	if (!proposalId) return undefined;
	const committedTurn = safeTurn(raw.committedTurn);
	const untilTurn = Math.max(committedTurn, safeTurn(raw.untilTurn));
	const claimedFingerprints = stringList(raw.claimedFingerprints);
	const touched = stringList(raw.touched);
	const recorded: HarnessTrustOutcome =
		raw.outcome === "clean" ||
		raw.outcome === "contested" ||
		raw.outcome === "faulted" ||
		raw.outcome === "open" ||
		raw.outcome === "unmeasured"
			? raw.outcome
			: "open";
	const outcome: HarnessTrustOutcome =
		recorded === "open" && claimedFingerprints.length === 0 ? "unmeasured" : recorded;
	const faulted = stringList(raw.faultedFingerprints);
	const bounds = { touched, claimedFingerprints, committedTurn, untilTurn };
	const skillImports = normalizeSkillImports(touched, raw.skillImports);
	const recurrences: Record<string, number> = {};
	for (const [fingerprintId, ordinal] of Object.entries(ownRecord(raw.recurrences) ?? {})) {
		if (claimedFingerprints.includes(fingerprintId) && typeof ordinal === "number" && inWindow(bounds, ordinal)) {
			recurrences[fingerprintId] = ordinal;
		}
	}
	const adjudications = normalizeAdjudications(bounds, raw.adjudications);
	const faultedEntries =
		outcome === "faulted"
			? sortedUnique(stringList(raw.faultedEntries).filter((entry) => isAdjudicableEntry(bounds, entry)))
			: [];
	return {
		proposalId,
		touched,
		claimedFingerprints,
		committedTurn,
		untilTurn,
		outcome,
		...(typeof raw.settledTurn === "number" && Number.isSafeInteger(raw.settledTurn)
			? { settledTurn: Math.max(0, raw.settledTurn) }
			: {}),
		...(faulted.length > 0 ? { faultedFingerprints: faulted } : {}),
		...(faultedEntries.length > 0 ? { faultedEntries } : {}),
		...(skillImports ? { skillImports } : {}),
		...(Object.keys(recurrences).length > 0 ? { recurrences } : {}),
		...(adjudications.length > 0 ? { adjudications } : {}),
	};
}

export function normalizeTrustWindows(value: unknown): HarnessTrustWindows | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const windows: HarnessTrustWindows = {};
	for (const [key, raw] of Object.entries(value as Record<string, unknown>)) {
		const window = normalizeTrustWindow(key, raw);
		if (window) windows[key] = window;
	}
	return windows;
}

export function hasOpenTrustWindows(windows: HarnessTrustWindows | undefined): boolean {
	return Object.values(windows ?? {}).some((window) => window.outcome === "open");
}

/**
 * Drop the oldest settled windows once the map grows past its cap. Open
 * windows are never pruned: an unsettled window is the only thing that can
 * still produce a credit or a contested close.
 */
function pruneTrustWindows(windows: HarnessTrustWindows): HarnessTrustWindows {
	const settled = Object.entries(windows).filter(([, window]) => window.outcome !== "open");
	if (settled.length <= MAX_SETTLED_TRUST_WINDOWS) return windows;
	const drop = new Set(
		settled
			.sort(
				(left, right) =>
					(left[1].settledTurn ?? 0) - (right[1].settledTurn ?? 0) || left[0].localeCompare(right[0]),
			)
			.slice(0, settled.length - MAX_SETTLED_TRUST_WINDOWS)
			.map(([key]) => key),
	);
	const kept: HarnessTrustWindows = {};
	for (const [key, window] of Object.entries(windows)) {
		if (!drop.has(key)) kept[key] = window;
	}
	return kept;
}

/**
 * Record the attribution for one committed refinement. Nothing is scored here:
 * a commit is neither trusted nor distrusted until its window settles.
 * `skillImports` is what each touched skill imports as the commit wrote it; a
 * later replay is adjudicated against that and nothing else.
 */
export function openTrustWindow(
	windows: HarnessTrustWindows | undefined,
	window: {
		proposalId: string;
		touched: readonly string[];
		claimedFingerprints: readonly string[];
		committedTurn: number;
		untilTurn: number;
		skillImports?: Readonly<Record<string, readonly string[]>>;
	},
): HarnessTrustWindows {
	const committedTurn = safeTurn(window.committedTurn);
	const touched = sortedUnique(window.touched);
	const skillImports = normalizeSkillImports(touched, window.skillImports);
	const opened: HarnessTrustWindow = {
		proposalId: window.proposalId,
		touched,
		claimedFingerprints: sortedUnique(window.claimedFingerprints),
		committedTurn,
		untilTurn: Math.max(committedTurn, safeTurn(window.untilTurn)),
		outcome: "open",
		...(skillImports ? { skillImports } : {}),
	};
	return pruneTrustWindows({ ...(windows ?? {}), [window.proposalId]: opened });
}

/**
 * Record recurrences and replay verdicts on the windows they speak to. Pure,
 * idempotent and order-insensitive: re-recording evidence already on a window
 * changes nothing, so a caller may fold the same pending list in more than once.
 *
 * Evidence is ignored for an unknown proposal, an `unmeasured` window, a
 * fingerprint the window did not claim, and an ordinal outside the window. A
 * verdict is also ignored for an entry that is not a skill the window touched,
 * and on a `faulted` window for an entry the fault was already charged to; a
 * recurrence on a `faulted` window changes nothing.
 *
 * `currentSkillImports` resolves what a skill imports now (`undefined` once it
 * is gone). With it, a verdict for a skill rewritten to import something other
 * than what the window recorded is not attributable to the window and dropped,
 * however recently the replay that produced it was planned.
 */
export function recordTrustWindowEvidence(
	windows: HarnessTrustWindows | undefined,
	evidence: readonly TrustWindowEvidence[],
	currentSkillImports?: (entry: string) => readonly string[] | undefined,
): HarnessTrustWindows | undefined {
	if (windows === undefined) return undefined;
	let next: HarnessTrustWindows | undefined;
	for (const item of evidence) {
		const current = next ?? windows;
		if (!Object.hasOwn(current, item.proposalId)) continue;
		const window = current[item.proposalId];
		if (window.outcome === "unmeasured") continue;
		if (window.outcome === "faulted" && (item.type === "recurrence" || isChargedEntry(window, item.entry))) continue;
		if (!window.claimedFingerprints.includes(item.fingerprintId) || !inWindow(window, item.ordinal)) continue;
		let updated: HarnessTrustWindow = window;
		if (item.type === "adjudication") {
			if (!isAdjudicableEntry(window, item.entry) || !isAdjudicationStatus(item.status) || !item.at) continue;
			const imports = currentSkillImports?.(item.entry);
			if (imports !== undefined && !sameModules(imports, window.skillImports?.[item.entry] ?? [])) continue;
			const adjudications = withAdjudication(window.adjudications ?? [], {
				entry: item.entry,
				fingerprintId: item.fingerprintId,
				status: item.status,
				ordinal: item.ordinal,
				runs: [item.at],
			});
			if (JSON.stringify(adjudications) !== JSON.stringify(window.adjudications ?? [])) {
				updated = { ...updated, adjudications };
			}
		}
		const recurrences = window.recurrences ?? {};
		const earliest = Object.hasOwn(recurrences, item.fingerprintId) ? recurrences[item.fingerprintId] : undefined;
		if (earliest === undefined || item.ordinal < earliest) {
			updated = { ...updated, recurrences: { ...recurrences, [item.fingerprintId]: item.ordinal } };
		}
		if (updated !== window) next = { ...current, [item.proposalId]: updated };
	}
	return next ?? windows;
}

function applyTrustDelta(
	trust: HarnessEntryTrust,
	event: { reason: HarnessTrustReason; delta: number; at: string; proposalId: string; fingerprintId?: string },
): HarnessEntryTrust {
	const score = clampTrust(entryTrustScore(trust) + event.delta);
	const record: HarnessTrustEvent = {
		reason: event.reason,
		delta: event.delta,
		score,
		at: event.at,
		proposalId: event.proposalId,
		...(event.fingerprintId === undefined ? {} : { fingerprintId: event.fingerprintId }),
	};
	return { score, updated_at: event.at, events: [...trust.events, record].slice(-MAX_TRUST_EVENTS) };
}

/**
 * Settle every window the current ordinal and its recorded evidence decide.
 *
 * - A window with an upheld verdict recorded for one of its skill entries
 *   faults, whatever the ordinal and whether it was still open, closed clean,
 *   or closed contested: each such entry is charged `-15` once, and nothing
 *   else the commit wrote is.
 * - A window that already faulted charges an upheld verdict for a skill entry
 *   it has not charged yet, and settles nothing: its outcome does not change.
 * - An open window past `untilTurn` closes `contested`, with no adjustment,
 *   when a claimed failure recurred in it, and `clean` otherwise, crediting
 *   every touched entry `+5`.
 * - Anything else stays as it is. A `cleared` or `unverifiable` verdict
 *   settles nothing: a verification that could not be run is not a
 *   verification that succeeded, and it is equally not evidence of a fault.
 *
 * Pure: the caller stores `adjustment.trust` back onto the entry and replaces
 * the window map.
 */
export function settleTrustWindows(
	windows: HarnessTrustWindows | undefined,
	lookup: TrustLookup,
	options: { turn: number; at?: string },
): TrustSettlement {
	const at = options.at ?? new Date().toISOString();
	const turn = safeTurn(options.turn);
	const next: HarnessTrustWindows = {};
	const adjustments: HarnessTrustAdjustment[] = [];
	const settled: TrustWindowSettlement[] = [];
	// Two windows settling in one call can touch the same entry; each must see
	// the score the previous one left behind, not the score on disk.
	const pending = new Map<string, HarnessEntryTrust>();

	const charge = (
		window: HarnessTrustWindow,
		refs: readonly string[],
		reason: HarnessTrustReason,
		delta: number,
		fingerprintId: string | undefined,
	) => {
		for (const ref of refs) {
			const parsed = parseHarnessEntryRef(ref);
			if (!parsed) continue;
			const entry = lookup(parsed.kind, parsed.id);
			if (!entry) continue;
			const current = pending.get(ref) ?? entry.trust ?? emptyEntryTrust(at);
			const before = entryTrustScore(current);
			const trust = applyTrustDelta(current, {
				reason,
				delta,
				at,
				proposalId: window.proposalId,
				...(fingerprintId === undefined ? {} : { fingerprintId }),
			});
			pending.set(ref, trust);
			adjustments.push({
				kind: parsed.kind,
				id: parsed.id,
				reason,
				delta,
				before,
				after: trust.score,
				trust,
				proposalId: window.proposalId,
				...(fingerprintId === undefined ? {} : { fingerprintId }),
			});
		}
	};

	/** Charge each entry of `upheld` once, attributed to its lowest upheld fingerprint. */
	const chargeFaults = (window: HarnessTrustWindow, upheld: readonly HarnessTrustAdjudication[]) => {
		const entries = sortedUnique(upheld.map((adjudication) => adjudication.entry));
		for (const entry of entries) {
			const fingerprintId = sortedUnique(
				upheld.filter((adjudication) => adjudication.entry === entry).map((item) => item.fingerprintId),
			)[0];
			charge(window, [entry], "measured_fault", -MEASURED_FAULT_DEBIT, fingerprintId);
		}
		return { entries, fingerprints: sortedUnique(upheld.map((adjudication) => adjudication.fingerprintId)) };
	};

	for (const [key, window] of Object.entries(windows ?? {})) {
		if (window.outcome === "unmeasured") {
			next[key] = window;
			continue;
		}
		const upheld = (window.adjudications ?? []).filter(
			(adjudication) =>
				adjudication.status === "upheld" &&
				isAdjudicableEntry(window, adjudication.entry) &&
				!isChargedEntry(window, adjudication.entry) &&
				window.claimedFingerprints.includes(adjudication.fingerprintId) &&
				inWindow(window, adjudication.ordinal),
		);
		if (window.outcome === "faulted") {
			if (upheld.length === 0) {
				next[key] = window;
				continue;
			}
			const charged = chargeFaults(window, upheld);
			next[key] = {
				...window,
				faultedFingerprints: sortedUnique([...(window.faultedFingerprints ?? []), ...charged.fingerprints]),
				faultedEntries: sortedUnique([...(window.faultedEntries ?? []), ...charged.entries]),
			};
			continue;
		}
		if (upheld.length > 0) {
			const charged = chargeFaults(window, upheld);
			next[key] = {
				...window,
				outcome: "faulted",
				settledTurn: turn,
				faultedFingerprints: charged.fingerprints,
				faultedEntries: charged.entries,
			};
			settled.push({
				proposalId: window.proposalId,
				from: window.outcome,
				outcome: "faulted",
				turn,
				fingerprints: charged.fingerprints,
			});
			continue;
		}
		if (window.outcome === "open" && turn > window.untilTurn) {
			const recurred = sortedUnique(Object.keys(window.recurrences ?? {}));
			if (recurred.length > 0) {
				next[key] = { ...window, outcome: "contested", settledTurn: turn };
				settled.push({
					proposalId: window.proposalId,
					from: "open",
					outcome: "contested",
					turn,
					fingerprints: recurred,
				});
				continue;
			}
			charge(window, window.touched, "clean_window", CLEAN_WINDOW_CREDIT, undefined);
			next[key] = { ...window, outcome: "clean", settledTurn: turn };
			settled.push({
				proposalId: window.proposalId,
				from: "open",
				outcome: "clean",
				turn,
				fingerprints: [...window.claimedFingerprints],
			});
			continue;
		}
		next[key] = window;
	}
	return { windows: pruneTrustWindows(next), adjustments, settled };
}
