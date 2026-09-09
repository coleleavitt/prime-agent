import { ravoChampionWindowOutcome } from "./ravo.js";
import type { HarnessEntry, HarnessState, RefinementKind } from "./refinement.js";

/**
 * Asymmetric trust on continual-harness entries.
 *
 * Every harness entry (prompt note, memory, skill, subagent spec) is injected
 * into the system prompt on every turn, so an entry that keeps being wrong is
 * a standing cost. Nothing else in the harness decays: an entry persists until
 * an LLM proposes deleting it. Trust gives the harness a measured, outcome-driven
 * decay instead.
 *
 * Rules (from jfc's `trust.rs`), exactly:
 * - trust is an integer in [0, 100]; an entry without a recorded trust is at 50
 * - a success adds {@link TRUST_SUCCESS_DELTA} (+5)
 * - a failure subtracts {@link TRUST_FAILURE_PENALTY} (15)
 * - tiers: restricted < 30, standard < 71, trusted >= 71
 *
 * One failure costs three successes, and a trusted entry drops to standard on a
 * single failure. An entry in the restricted tier is DORMANT: it is kept in the
 * state and remains visible to CRUD, but it is no longer rendered into the
 * system prompt. An explicit update of a dormant entry revives it at 50.
 *
 * Outcomes come from RAVO's provisional window: a committed /refine proposal
 * "touches" the entries it created or updated; when the champion's observation
 * window closes without a measured fault every touched entry succeeds, and when
 * a claimed failure fingerprint recurs inside the window every touched entry
 * fails. Each window settles at most once per entry (idempotent).
 */

export const TRUST_DEFAULT = 50;
export const TRUST_MIN = 0;
export const TRUST_MAX = 100;
export const TRUST_SUCCESS_DELTA = 5;
export const TRUST_FAILURE_PENALTY = 15;
/** Entries with trust strictly below this are dormant (restricted tier). */
export const TRUST_RESTRICTED_BELOW = 30;
/** Entries with trust at or above this are trusted. */
export const TRUST_TRUSTED_FROM = 71;

export type TrustOutcome = "success" | "failure";
export type TrustTier = "restricted" | "standard" | "trusted";

/** Trust ledger sidecar persisted inside {@link HarnessState.trustWindows}. */
export interface HarnessTrustWindow {
	/** The committed /refine proposal (RAVO champion) that opened this window. */
	proposalId: string;
	/** Entry keys (`<kind>:<id>`) the committed proposal created or updated. */
	touched: string[];
	/** Terminal outcome once the window settled; absent while it is open. */
	outcome?: TrustOutcome;
	/** Turn at which the window settled, when the settlement had a turn. */
	settledTurn?: number;
}

/** Stable entry key used by trust windows. */
export function harnessEntryKey(kind: RefinementKind, id: string): string {
	return `${kind}:${id}`;
}

function parseEntryKey(key: string): { kind: RefinementKind; id: string } | undefined {
	const separator = key.indexOf(":");
	if (separator <= 0 || separator === key.length - 1) return undefined;
	const kind = key.slice(0, separator);
	if (kind !== "prompt" && kind !== "memory" && kind !== "skill" && kind !== "subagent") return undefined;
	return { kind, id: key.slice(separator + 1) };
}

/** Clamp an arbitrary number into the integer trust range. */
export function clampTrust(value: number): number {
	if (!Number.isFinite(value)) return TRUST_DEFAULT;
	return Math.min(TRUST_MAX, Math.max(TRUST_MIN, Math.round(value)));
}

/**
 * Normalize an untrusted persisted trust value: integers in [0, 100] are kept,
 * anything else is treated as absent (the default 50). Used by the loader so a
 * hand-edited or foreign state file cannot smuggle an out-of-range trust.
 */
export function normalizeTrust(value: unknown): number | undefined {
	if (typeof value !== "number" || !Number.isInteger(value)) return undefined;
	return value >= TRUST_MIN && value <= TRUST_MAX ? value : undefined;
}

/** Effective trust of an entry: its recorded trust, or 50 when absent. */
export function trustOf(entry: Pick<HarnessEntry, "trust">): number {
	return normalizeTrust(entry.trust) ?? TRUST_DEFAULT;
}

/** Tier of a trust value: restricted < 30, standard < 71, trusted >= 71. */
export function trustTier(trust: number): TrustTier {
	if (trust < TRUST_RESTRICTED_BELOW) return "restricted";
	if (trust < TRUST_TRUSTED_FROM) return "standard";
	return "trusted";
}

/** Whether an entry is dormant (restricted tier): kept in state, not rendered into the prompt. */
export function isDormant(entry: Pick<HarnessEntry, "trust">): boolean {
	return trustTier(trustOf(entry)) === "restricted";
}

/** Pure trust arithmetic: +5 on success, -15 on failure, clamped to [0, 100]. */
export function nextTrust(trust: number, outcome: TrustOutcome): number {
	return clampTrust(outcome === "success" ? trust + TRUST_SUCCESS_DELTA : trust - TRUST_FAILURE_PENALTY);
}

/** Return a copy of `entry` with the outcome applied to its trust. */
export function applyTrustOutcome<T extends Pick<HarnessEntry, "trust">>(entry: T, outcome: TrustOutcome): T {
	return { ...entry, trust: nextTrust(trustOf(entry), outcome) };
}

/**
 * Revive an entry on an explicit update: a dormant entry returns to the default
 * trust; an active entry keeps its trust (an update is not a success).
 */
export function revivedTrust(before: Pick<HarnessEntry, "trust"> | undefined): number | undefined {
	if (before === undefined) return undefined;
	return isDormant(before) ? TRUST_DEFAULT : normalizeTrust(before.trust);
}

/** Split entries of one kind into the active listing and the dormant remainder. */
export function partitionDormant<T extends Pick<HarnessEntry, "trust">>(
	entries: readonly T[],
): { active: T[]; dormant: T[] } {
	const active: T[] = [];
	const dormant: T[] = [];
	for (const entry of entries) (isDormant(entry) ? dormant : active).push(entry);
	return { active, dormant };
}

/** Every dormant entry in the state, in kind order. */
export function dormantEntries(state: HarnessState): HarnessEntry[] {
	const result: HarnessEntry[] = [];
	for (const kind of Object.keys(state.entries) as RefinementKind[]) {
		for (const entry of Object.values(state.entries[kind])) {
			if (isDormant(entry)) result.push(entry);
		}
	}
	return result;
}

/** Normalize an untrusted persisted trust-window map; malformed records are dropped. */
export function normalizeTrustWindows(value: unknown): Record<string, HarnessTrustWindow> | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const windows: Record<string, HarnessTrustWindow> = {};
	for (const [proposalId, raw] of Object.entries(value as Record<string, unknown>)) {
		if (!proposalId || typeof raw !== "object" || raw === null || Array.isArray(raw)) continue;
		const record = raw as Record<string, unknown>;
		const touched = Array.isArray(record.touched)
			? [...new Set(record.touched.filter((key): key is string => typeof key === "string" && key.length > 0))]
			: [];
		const outcome = record.outcome === "success" || record.outcome === "failure" ? record.outcome : undefined;
		const settledTurn =
			typeof record.settledTurn === "number" && Number.isSafeInteger(record.settledTurn) && record.settledTurn >= 0
				? record.settledTurn
				: undefined;
		windows[proposalId] = {
			proposalId,
			touched,
			...(outcome === undefined ? {} : { outcome }),
			...(settledTurn === undefined ? {} : { settledTurn }),
		};
	}
	return Object.keys(windows).length > 0 ? windows : undefined;
}

/**
 * Open (or replace) the trust window for a committed proposal. Called by the
 * apply path with the entries the proposal created or updated. Re-opening a
 * window for the same proposal id is idempotent: a settled window is kept as
 * is, so an outcome can never be applied twice for one proposal.
 */
export function openHarnessTrustWindow(
	state: HarnessState,
	proposalId: string,
	touched: readonly string[],
): HarnessState {
	const keys = [...new Set(touched)];
	if (!proposalId || keys.length === 0) return state;
	const existing = state.trustWindows?.[proposalId];
	if (existing?.outcome !== undefined) return state;
	return {
		...state,
		trustWindows: { ...(state.trustWindows ?? {}), [proposalId]: { proposalId, touched: keys } },
	};
}

/**
 * Apply a window outcome to every entry the proposal touched, once. The pure
 * "HarnessState + window outcome -> HarnessState" step: a window with no record,
 * or one that already settled, leaves the state untouched (idempotent). Entries
 * deleted since the commit are skipped.
 */
export function applyHarnessTrustWindowOutcome(
	state: HarnessState,
	proposalId: string,
	outcome: TrustOutcome,
	options: { turn?: number } = {},
): HarnessState {
	const window = state.trustWindows?.[proposalId];
	if (!window || window.outcome !== undefined) return state;
	const entries = { ...state.entries };
	const seen = new Set<string>();
	for (const key of window.touched) {
		if (seen.has(key)) continue;
		seen.add(key);
		const parsed = parseEntryKey(key);
		if (!parsed) continue;
		const entry = entries[parsed.kind][parsed.id];
		if (!entry) continue;
		entries[parsed.kind] = { ...entries[parsed.kind], [parsed.id]: applyTrustOutcome(entry, outcome) };
	}
	const settled: HarnessTrustWindow = {
		...window,
		outcome,
		...(options.turn === undefined ? {} : { settledTurn: options.turn }),
	};
	return { ...state, entries, trustWindows: { ...(state.trustWindows ?? {}), [proposalId]: settled } };
}

/**
 * Settle every open trust window against the RAVO lineage at `turn`:
 * - a champion whose provisional window recorded a measured fault
 *   (`observedRecurrence`) fails its touched entries (-15, once);
 * - a champion whose window closed (`turn > untilTurn`) without a fault succeeds
 *   its touched entries (+5, once);
 * - windows whose proposal never became a champion (rejected, rolled back, or
 *   applied without RAVO) are dropped; windows for champions without a
 *   provisional window stay open.
 *
 * Pure: returns the same object when nothing changes, so callers can persist
 * only on change. Must not run while a /refine plan is in flight: the RAVO
 * certificate binds the baseline state digest, and settling mutates entries.
 */
export function settleHarnessTrustWindows(state: HarnessState, turn: number): HarnessState {
	const windows = state.trustWindows;
	if (!windows || !Number.isSafeInteger(turn) || turn < 0) return state;
	const champions = new Map(state.ravo?.lineage.map((champion) => [champion.proposalId, champion]) ?? []);
	let next = state;
	const dropped: string[] = [];
	for (const [proposalId, window] of Object.entries(windows)) {
		if (window.outcome !== undefined) continue;
		const champion = champions.get(proposalId);
		if (!champion) {
			dropped.push(proposalId);
			continue;
		}
		const outcome = ravoChampionWindowOutcome(champion, turn);
		if (outcome !== undefined) {
			next = applyHarnessTrustWindowOutcome(next, proposalId, outcome, { turn });
		}
	}
	if (dropped.length === 0) return next;
	const remaining = { ...(next.trustWindows ?? {}) };
	for (const proposalId of dropped) delete remaining[proposalId];
	return { ...next, trustWindows: Object.keys(remaining).length > 0 ? remaining : undefined };
}
