import type { RefereeVerdict } from "../ravo/referee.js";

/**
 * Trust and dormancy on continual-harness entries.
 *
 * Every entry carries a trust score. It moves on measured outcomes only:
 *
 * - a provisional window that closes with nothing refuted is `+5`;
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
 * traceback. A debit is only charged to an entry a refinement WROTE while the
 * gate accepted it as addressing that exact fingerprint. `trustWindows` records
 * that link at commit time — proposal id, the `kind:id` of every entry the
 * commit touched, and the fingerprints it claimed — so a later upheld verdict
 * on one of those fingerprints has a defensible target and nothing else does.
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
/** Credit for a provisional window that closed with no upheld verdict. */
export const CLEAN_WINDOW_CREDIT = 5;
/** Debit for an upheld referee verdict attributable to the entry. */
export const MEASURED_FAULT_DEBIT = 15;
/** Strictly below this an entry is dormant: readable, but not rendered. */
export const DORMANT_TRUST_THRESHOLD = 30;

const MAX_TRUST_EVENTS = 20;
const MAX_SETTLED_TRUST_WINDOWS = 100;
const ENTRY_REF_SEPARATOR = ":";

export type HarnessTrustReason = "clean_window" | "measured_fault";
export type HarnessTrustOutcome = "open" | "clean" | "faulted";

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
}

export type HarnessTrustWindows = Record<string, HarnessTrustWindow>;

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

export interface TrustSettlement {
	windows: HarnessTrustWindows;
	adjustments: HarnessTrustAdjustment[];
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

function safeTurn(value: unknown): number {
	return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : 0;
}

function normalizeTrustWindow(key: string, value: unknown): HarnessTrustWindow | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const raw = value as Record<string, unknown>;
	const proposalId = typeof raw.proposalId === "string" && raw.proposalId ? raw.proposalId : key;
	if (!proposalId) return undefined;
	const committedTurn = safeTurn(raw.committedTurn);
	const untilTurn = Math.max(committedTurn, safeTurn(raw.untilTurn));
	const outcome: HarnessTrustOutcome =
		raw.outcome === "clean" || raw.outcome === "faulted" || raw.outcome === "open" ? raw.outcome : "open";
	const faulted = stringList(raw.faultedFingerprints);
	return {
		proposalId,
		touched: stringList(raw.touched),
		claimedFingerprints: stringList(raw.claimedFingerprints),
		committedTurn,
		untilTurn,
		outcome,
		...(typeof raw.settledTurn === "number" && Number.isSafeInteger(raw.settledTurn)
			? { settledTurn: Math.max(0, raw.settledTurn) }
			: {}),
		...(faulted.length > 0 ? { faultedFingerprints: faulted } : {}),
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

/**
 * Drop the oldest settled windows once the map grows past its cap. Open
 * windows are never pruned: an unsettled window is the only thing that can
 * still produce a debit.
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
 */
export function openTrustWindow(
	windows: HarnessTrustWindows | undefined,
	window: {
		proposalId: string;
		touched: readonly string[];
		claimedFingerprints: readonly string[];
		committedTurn: number;
		untilTurn: number;
	},
): HarnessTrustWindows {
	const committedTurn = safeTurn(window.committedTurn);
	const opened: HarnessTrustWindow = {
		proposalId: window.proposalId,
		touched: [...new Set(window.touched)].sort((left, right) => left.localeCompare(right)),
		claimedFingerprints: [...new Set(window.claimedFingerprints)].sort((left, right) => left.localeCompare(right)),
		committedTurn,
		untilTurn: Math.max(committedTurn, safeTurn(window.untilTurn)),
		outcome: "open",
	};
	return pruneTrustWindows({ ...(windows ?? {}), [window.proposalId]: opened });
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
 * Settle every open window the current turn and verdict set can decide.
 *
 * A window faults the moment an upheld verdict names one of the fingerprints
 * it claimed; each such fingerprint is a separate measured fault against every
 * entry the commit touched. A window with no upheld verdict settles clean once
 * the turn passes `untilTurn`. Anything else stays open — in particular a
 * `unverifiable` verdict settles nothing, mirroring `refereeOpponentPassed`:
 * a verification that could not be run is not a verification that succeeded,
 * and it is equally not evidence of a fault.
 *
 * Pure: the caller stores `adjustment.trust` back onto the entry and replaces
 * the window map.
 */
export function settleTrustWindows(
	windows: HarnessTrustWindows | undefined,
	lookup: TrustLookup,
	options: { verdicts?: readonly RefereeVerdict[]; turn: number; at?: string },
): TrustSettlement {
	const at = options.at ?? new Date().toISOString();
	const turn = safeTurn(options.turn);
	const upheld = new Set(
		(options.verdicts ?? []).filter((verdict) => verdict.status === "upheld").map((verdict) => verdict.fingerprintId),
	);
	const next: HarnessTrustWindows = {};
	const adjustments: HarnessTrustAdjustment[] = [];
	// Two windows settling in one call can touch the same entry; each must see
	// the score the previous one left behind, not the score on disk.
	const pending = new Map<string, HarnessEntryTrust>();

	const charge = (
		window: HarnessTrustWindow,
		reason: HarnessTrustReason,
		delta: number,
		fingerprintId: string | undefined,
	) => {
		for (const ref of window.touched) {
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

	for (const [key, window] of Object.entries(windows ?? {})) {
		if (window.outcome !== "open") {
			next[key] = window;
			continue;
		}
		const faulted = window.claimedFingerprints.filter((fingerprintId) => upheld.has(fingerprintId));
		if (faulted.length > 0) {
			for (const fingerprintId of faulted) {
				charge(window, "measured_fault", -MEASURED_FAULT_DEBIT, fingerprintId);
			}
			next[key] = { ...window, outcome: "faulted", settledTurn: turn, faultedFingerprints: faulted };
			continue;
		}
		if (turn > window.untilTurn) {
			charge(window, "clean_window", CLEAN_WINDOW_CREDIT, undefined);
			next[key] = { ...window, outcome: "clean", settledTurn: turn };
			continue;
		}
		next[key] = window;
	}
	return { windows: pruneTrustWindows(next), adjustments };
}
