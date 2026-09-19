import { randomBytes } from "node:crypto";
import {
	chmodSync,
	closeSync,
	existsSync,
	fsyncSync,
	mkdirSync,
	openSync,
	readdirSync,
	readFileSync,
	renameSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { getLogger } from "@earendil-works/pi-ai";
import { lockSync } from "proper-lockfile";
import { getLearningDir, getTrajectoryBackfillDir, getTrajectoryIndexPath } from "../../config.js";
import { type LearningDay, readLearningIndex } from "../learning-index.js";
import { DEFAULT_RECURRENCE_THRESHOLD } from "../ravo/failure-ledger.js";
import { parseHarnessEntryRef } from "../refinement/harness-trust.js";
import type { HarnessState } from "../refinement/refinement.js";

/**
 * Engineer Trajectory Index (ETI): a derived, off-turn-path DIFF layer over the
 * already-sealed learning-index day roll-ups. It buckets sealed days into
 * OBSERVED ISO-week windows, computes per-fingerprint NEW / DROPPED
 * (internalized) / PERSISTS (stable-gap) labels plus a new-minus-retired rate,
 * and feeds the stable, tool-invariant residue into the harness prompt.
 *
 * Two honesty guardrails are structural: every label carries a non-empty
 * `confounds[]` flag (`task-mix` always, plus `tool-surface` /
 * `measurement-instrument` when the span crosses a corpus boundary or a data
 * gap), and no NEW/DROPPED/PERSISTS label is emitted below a minimum
 * observed-window count (mirrors `buildLearningReport`'s `minCohortN` withhold).
 *
 * The single runtime input is the sealed `learning/days/*.json` files. ETI never
 * opens a failure ledger, a raw transcript, or the structured log on the turn
 * path: `readTrajectoryIndex` reads only the small sealed `trajectory.json`
 * through a stat cache, opening no span. Compute and seal happen off the turn
 * path (the `learning trajectory` CLI). Backfill (cross-tool) day files are read
 * only by the CLI and never written into `trajectory.json` or the prompt.
 */

const log = getLogger("coding-agent.trajectory-index");

export const TRAJECTORY_STORE_VERSION = 1;
/** Mirrors DEFAULT_MIN_COHORT_N discipline: no label below this many observed windows. */
export const DEFAULT_MIN_TRAJECTORY_WINDOWS = 4;
/** M: consecutive absent windows required before a recurring fingerprint counts as internalized. */
export const DEFAULT_TRAJECTORY_INTERNALIZED_GAP = 2;
/** bound() ceiling on the persisted window list. */
export const DEFAULT_MAX_TRAJECTORY_WINDOWS = 52;
export const TRAJECTORY_INDEX_ENV = "PRIME_AGENT_TRAJECTORY_INDEX";

/**
 * A fingerprint whose name/message/exceptionClass/source names a
 * security/credential class. Lever 2 never marks such a fingerprint
 * internalized, so its recurrence reminder always fires (fail closed).
 */
export const SECURITY_CLASS_FINGERPRINT = /auth|credential|secret|token|password|permission[- ]denied|unauthor/i;

export type TrajectoryCorpus = "prime" | `backfill:${string}`;
export type TrajectoryConfound = "task-mix" | "tool-surface" | "measurement-instrument";
export type TrajectoryLabelKind = "new" | "dropped" | "persists";

/** Order confounds render and store in, so the tag is deterministic. */
const CONFOUND_ORDER: readonly TrajectoryConfound[] = ["task-mix", "tool-surface", "measurement-instrument"];

/** Per (window, fingerprint) aggregate, lifted from LearningDay.fingerprints (failure===true only). */
export interface TrajectoryFingerprintWindow {
	fingerprint: string;
	name: string;
	message: string;
	failure: true;
	count: number;
	/** Running total of ALL failure counts through this window (== observationOrdinal on an equivalent ledger). */
	cumulativeOrdinal: number;
}

/** One sealed, immutable window. */
export interface TrajectoryWindow {
	schema: typeof TRAJECTORY_STORE_VERSION;
	window: string;
	sealedAt: string;
	days: string[];
	turns: number;
	corpus: TrajectoryCorpus;
	fingerprints: TrajectoryFingerprintWindow[];
}

/** The emitted diff for one fingerprint across the sealed windows. */
export interface TrajectoryLabel {
	fingerprint: string;
	name: string;
	message: string;
	corpus: TrajectoryCorpus;
	label: TrajectoryLabelKind | null;
	/** Reason a label was withheld; mutually exclusive with a non-null label. */
	withheld?: string;
	sinceWindow: string;
	lastWindow: string;
	windowsPresent: number;
	windowsRecurring: number;
	claimedByRefinement: boolean;
	domainActive: boolean;
	securityClass: boolean;
	/** NEVER empty; always contains at least "task-mix". */
	confounds: TrajectoryConfound[];
}

export interface TrajectoryRateWindow {
	window: string;
	appeared: number;
	/** Fingerprints whose absence began this window; null across an observed-week gap. */
	retired: number | null;
	/** appeared - retired; null across an observed-week gap. */
	newMinusRetired: number | null;
}

/** On disk: <agentDir>/learning/trajectory.json (mode 0600). */
export interface TrajectoryStoreFile {
	version: typeof TRAJECTORY_STORE_VERSION;
	sealedAt: string;
	/** K: the number of observed prime windows the labels were computed over. */
	windowsObserved: number;
	minWindows: number;
	windows: TrajectoryWindow[];
	labels: TrajectoryLabel[];
	rate: TrajectoryRateWindow[];
}

/** Global single file; mirrors ResolutionStore mechanics with no per-repo key. */
export interface TrajectoryStore {
	load(): TrajectoryStoreFile | undefined;
	save(file: TrajectoryStoreFile): void;
}

export interface SealTrajectoryOptions {
	days: readonly LearningDay[];
	/** Cross-tool day files, tagged by corpus; the CLI passes them with --include-backfill. Never persisted. */
	backfillDays?: readonly { corpus: TrajectoryCorpus; day: LearningDay }[];
	minWindows?: number;
	internalizedGap?: number;
	nowMs?: number;
}

// ---------------------------------------------------------------------------
// Kill switch
// ---------------------------------------------------------------------------

/** Enabled unless the kill switch is `0`, `off`, `false`, or `no` (copying globalFailureLedgerEnabled). */
export function isTrajectoryIndexEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
	const value = env[TRAJECTORY_INDEX_ENV]?.trim().toLowerCase();
	return !(value === "0" || value === "off" || value === "false" || value === "no");
}

// ---------------------------------------------------------------------------
// ISO week helpers (ISO-8601 week-year, UTC, Monday start)
// ---------------------------------------------------------------------------

const DAY_MS = 86_400_000;
const WEEK_KEY_RE = /^(\d{4})-W(\d{2})$/;

function utcMidnight(input: string | Date): Date {
	if (typeof input === "string") return new Date(`${input}T00:00:00.000Z`);
	return new Date(Date.UTC(input.getUTCFullYear(), input.getUTCMonth(), input.getUTCDate()));
}

/** ISO-8601 week key "YYYY-Www" using the WEEK-YEAR (not the calendar year), UTC, Monday-start. */
export function isoWeek(input: string | Date): string {
	const date = utcMidnight(input);
	// Shift to the Thursday of this week; its calendar year is the ISO week-year.
	const dayNum = (date.getUTCDay() + 6) % 7; // Mon=0 .. Sun=6
	const thursday = new Date(date.getTime() + (3 - dayNum) * DAY_MS);
	const isoYear = thursday.getUTCFullYear();
	const firstThursday = new Date(Date.UTC(isoYear, 0, 4));
	const firstThursdayDayNum = (firstThursday.getUTCDay() + 6) % 7;
	const week1Monday = new Date(firstThursday.getTime() - firstThursdayDayNum * DAY_MS);
	const week = Math.floor((thursday.getTime() - week1Monday.getTime()) / (7 * DAY_MS)) + 1;
	return `${isoYear}-W${String(week).padStart(2, "0")}`;
}

/** The UTC Monday that begins an ISO week key; used for gap detection between observed weeks. */
function isoWeekMonday(key: string): number | undefined {
	const match = WEEK_KEY_RE.exec(key);
	if (!match) return undefined;
	const isoYear = Number(match[1]);
	const week = Number(match[2]);
	const jan4 = new Date(Date.UTC(isoYear, 0, 4));
	const jan4DayNum = (jan4.getUTCDay() + 6) % 7;
	const week1Monday = jan4.getTime() - jan4DayNum * DAY_MS;
	return week1Monday + (week - 1) * 7 * DAY_MS;
}

/** Whether `later` is the ISO week immediately after `earlier` (exactly seven days on). */
function consecutiveWeeks(earlier: string, later: string): boolean {
	const a = isoWeekMonday(earlier);
	const b = isoWeekMonday(later);
	if (a === undefined || b === undefined) return false;
	return b - a === 7 * DAY_MS;
}

// ---------------------------------------------------------------------------
// Security class predicate
// ---------------------------------------------------------------------------

/** Whether any of a fingerprint's descriptive fields names a security/credential class. */
export function matchesSecurityClass(fields: {
	name?: string;
	message?: string;
	exceptionClass?: string;
	source?: string;
}): boolean {
	const text = [fields.name, fields.message, fields.exceptionClass, fields.source].filter(Boolean).join("\n");
	return text.length > 0 && SECURITY_CLASS_FINGERPRINT.test(text);
}

// ---------------------------------------------------------------------------
// Sealing (pure over its inputs; off the turn path)
// ---------------------------------------------------------------------------

interface CorpusResult {
	windows: TrajectoryWindow[];
	labels: TrajectoryLabel[];
	rate: TrajectoryRateWindow[];
}

function clampPositive(value: number | undefined, fallback: number): number {
	if (value === undefined || !Number.isFinite(value)) return fallback;
	return Math.max(1, Math.trunc(value));
}

/**
 * Compute windows, labels and rate for one corpus. Corpora are never summed
 * with each other; each is windowed and labelled on its own so a cross-tool
 * comparison is never presented as a clean derivative.
 */
function computeCorpus(
	corpus: TrajectoryCorpus,
	days: readonly LearningDay[],
	ctx: {
		currentWeek: string;
		minWindows: number;
		internalizedGap: number;
		sealedAt: string;
		indexMixesCorpora: boolean;
	},
): CorpusResult {
	// Bucket sealed days into observed weeks, leaving the current ISO week open.
	const byWeek = new Map<string, LearningDay[]>();
	const addressed = new Set<string>();
	for (const day of days) {
		for (const commit of day.commits) {
			for (const id of commit.addressed) addressed.add(id);
		}
		const week = isoWeek(day.day);
		if (week === ctx.currentWeek) continue;
		const bucket = byWeek.get(week);
		if (bucket) bucket.push(day);
		else byWeek.set(week, [day]);
	}
	const orderedWeeks = [...byWeek.keys()].sort();
	const K = orderedWeeks.length;

	// Per-window aggregation of failure fingerprints only.
	const windows: TrajectoryWindow[] = [];
	const perWindowCounts: Array<Map<string, number>> = [];
	const carried = new Map<string, { name: string; message: string }>();
	const carriedDay = new Map<string, string>();
	const windowTurns: number[] = [];
	let cumulative = 0;
	for (const week of orderedWeeks) {
		const dayList = [...byWeek.get(week)!].sort((a, b) => a.day.localeCompare(b.day));
		const counts = new Map<string, number>();
		let windowFailureTotal = 0;
		let turns = 0;
		for (const day of dayList) {
			turns += day.turns;
			for (const stat of day.fingerprints) {
				if (!stat.failure) continue;
				counts.set(stat.fingerprint, (counts.get(stat.fingerprint) ?? 0) + stat.count);
				windowFailureTotal += stat.count;
				// Carry name/message from the newest day in the window that names it.
				const seenDay = carriedDay.get(stat.fingerprint);
				if (seenDay === undefined || day.day >= seenDay) {
					carried.set(stat.fingerprint, { name: stat.name, message: stat.message ?? "" });
					carriedDay.set(stat.fingerprint, day.day);
				}
			}
		}
		cumulative += windowFailureTotal;
		const runningOrdinal = cumulative;
		const fingerprints: TrajectoryFingerprintWindow[] = [...counts.entries()]
			.sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
			.map(([fingerprint, count]) => {
				const label = carried.get(fingerprint) ?? { name: "", message: "" };
				return {
					fingerprint,
					name: label.name,
					message: label.message,
					failure: true as const,
					count,
					cumulativeOrdinal: runningOrdinal,
				};
			});
		windows.push({
			schema: TRAJECTORY_STORE_VERSION,
			window: week,
			sealedAt: ctx.sealedAt,
			days: dayList.map((day) => day.day),
			turns,
			corpus,
			fingerprints,
		});
		perWindowCounts.push(counts);
		windowTurns.push(turns);
	}

	// Adjacent-window gaps, so the confound stamper can flag a span that crosses one.
	const gapBefore: boolean[] = orderedWeeks.map((week, index) =>
		index === 0 ? false : !consecutiveWeeks(orderedWeeks[index - 1]!, week),
	);

	const threshold = DEFAULT_RECURRENCE_THRESHOLD;
	const allFingerprints = new Set<string>();
	for (const counts of perWindowCounts) {
		for (const fp of counts.keys()) allFingerprints.add(fp);
	}

	const labels: TrajectoryLabel[] = [];
	for (const fingerprint of [...allFingerprints].sort()) {
		const presence = perWindowCounts.map((counts) => counts.get(fingerprint) ?? 0);
		let firstIdx = -1;
		let lastIdx = -1;
		let windowsPresent = 0;
		let windowsRecurring = 0;
		for (let index = 0; index < presence.length; index++) {
			const count = presence[index]!;
			if (count <= 0) continue;
			if (firstIdx === -1) firstIdx = index;
			lastIdx = index;
			windowsPresent++;
			if (count >= threshold) windowsRecurring++;
		}
		if (firstIdx === -1) continue; // never a failure in an observed window
		const carriedLabel = carried.get(fingerprint) ?? { name: "", message: "" };
		const claimedByRefinement = addressed.has(fingerprint);
		const domainActive = lastMWindowTurns(windowTurns, ctx.internalizedGap) > 0;
		const securityClass = matchesSecurityClass({ name: carriedLabel.name, message: carriedLabel.message });
		const spanHasGap = gapBefore.some((gap, index) => gap && index > firstIdx && index <= lastIdx);
		const confounds = stampConfounds(corpus, ctx.indexMixesCorpora, spanHasGap);

		const base: TrajectoryLabel = {
			fingerprint,
			name: carriedLabel.name,
			message: carriedLabel.message,
			corpus,
			label: null,
			sinceWindow: orderedWeeks[firstIdx]!,
			lastWindow: orderedWeeks[lastIdx]!,
			windowsPresent,
			windowsRecurring,
			claimedByRefinement,
			domainActive,
			securityClass,
			confounds,
		};

		if (K < ctx.minWindows) {
			labels.push({ ...base, withheld: `fewer than ${ctx.minWindows} observed windows (${K})` });
			continue;
		}

		const absentForLastM = lastIdx <= K - 1 - ctx.internalizedGap;
		const recurred = windowsRecurring >= 1;
		if (recurred && absentForLastM) {
			// DROPPED / internalized (low confidence): recurred then gone for the last M windows.
			if (claimedByRefinement) {
				labels.push({ ...base, withheld: "absent but a committed refinement claimed it" });
			} else if (!domainActive) {
				labels.push({ ...base, withheld: "absent but domain inactive (task-mix)" });
			} else {
				labels.push({ ...base, label: "dropped" });
			}
			continue;
		}
		if (firstIdx === K - 1) {
			labels.push({ ...base, label: "new" });
			continue;
		}
		if (windowsRecurring * 2 > K) {
			labels.push({ ...base, label: "persists" });
			continue;
		}
		labels.push(base);
	}

	const rate = computeRate(orderedWeeks, perWindowCounts, gapBefore);
	return { windows, labels, rate };
}

function lastMWindowTurns(windowTurns: readonly number[], gap: number): number {
	let total = 0;
	for (let index = Math.max(0, windowTurns.length - gap); index < windowTurns.length; index++) {
		total += windowTurns[index]!;
	}
	return total;
}

function stampConfounds(
	corpus: TrajectoryCorpus,
	indexMixesCorpora: boolean,
	spanHasGap: boolean,
): TrajectoryConfound[] {
	const set = new Set<TrajectoryConfound>(["task-mix"]);
	const isBackfill = corpus.startsWith("backfill:");
	if (isBackfill || indexMixesCorpora) set.add("tool-surface");
	if (isBackfill || indexMixesCorpora || spanHasGap) set.add("measurement-instrument");
	return CONFOUND_ORDER.filter((confound) => set.has(confound));
}

function computeRate(
	orderedWeeks: readonly string[],
	perWindowCounts: ReadonlyArray<Map<string, number>>,
	gapBefore: readonly boolean[],
): TrajectoryRateWindow[] {
	const rate: TrajectoryRateWindow[] = [];
	for (let index = 0; index < orderedWeeks.length; index++) {
		const here = perWindowCounts[index]!;
		const previous = index === 0 ? undefined : perWindowCounts[index - 1]!;
		let appeared = 0;
		for (const fp of here.keys()) {
			if (!previous || !previous.has(fp)) {
				// First appearance requires it be absent from every earlier window, not just the last one.
				if (!seenBefore(perWindowCounts, index, fp)) appeared++;
			}
		}
		// A gap makes the transition uncountable: report null for the gap-adjacent step.
		if (index === 0) {
			rate.push({ window: orderedWeeks[index]!, appeared, retired: 0, newMinusRetired: appeared });
			continue;
		}
		if (gapBefore[index]) {
			rate.push({ window: orderedWeeks[index]!, appeared, retired: null, newMinusRetired: null });
			continue;
		}
		let retired = 0;
		for (const fp of previous!.keys()) {
			if (!here.has(fp)) retired++;
		}
		rate.push({ window: orderedWeeks[index]!, appeared, retired, newMinusRetired: appeared - retired });
	}
	return rate;
}

function seenBefore(perWindowCounts: ReadonlyArray<Map<string, number>>, index: number, fingerprint: string): boolean {
	for (let earlier = 0; earlier < index; earlier++) {
		if (perWindowCounts[earlier]!.has(fingerprint)) return true;
	}
	return false;
}

/**
 * Compute the full trajectory over sealed days. Pure given {days, backfillDays,
 * minWindows, internalizedGap, nowMs}. Prime windows drive the labels and rate
 * that reach the prompt; backfill corpora, when supplied, are computed
 * separately and flagged with all three confounds for the CLI table only.
 */
export function sealTrajectoryWindows(opts: SealTrajectoryOptions): TrajectoryStoreFile {
	const minWindows = clampPositive(opts.minWindows, DEFAULT_MIN_TRAJECTORY_WINDOWS);
	const internalizedGap = clampPositive(opts.internalizedGap, DEFAULT_TRAJECTORY_INTERNALIZED_GAP);
	const nowMs = opts.nowMs ?? Date.now();
	const sealedAt = new Date(nowMs).toISOString();
	const currentWeek = isoWeek(new Date(nowMs));
	const backfill = opts.backfillDays ?? [];
	const indexMixesCorpora = backfill.length > 0;

	const byCorpus = new Map<TrajectoryCorpus, LearningDay[]>();
	for (const { corpus, day } of backfill) {
		const bucket = byCorpus.get(corpus);
		if (bucket) bucket.push(day);
		else byCorpus.set(corpus, [day]);
	}

	const ctx = { currentWeek, minWindows, internalizedGap, sealedAt, indexMixesCorpora };
	const prime = computeCorpus("prime", opts.days, ctx);
	const windows = [...prime.windows];
	const labels = [...prime.labels];
	for (const corpus of [...byCorpus.keys()].sort()) {
		const result = computeCorpus(corpus, byCorpus.get(corpus)!, ctx);
		windows.push(...result.windows);
		labels.push(...result.labels);
	}

	return {
		version: TRAJECTORY_STORE_VERSION,
		sealedAt,
		windowsObserved: prime.windows.length,
		minWindows,
		windows,
		labels,
		rate: prime.rate,
	};
}

/** Keep only the prime portion, so backfill never reaches the persisted store or the prompt. */
export function primeTrajectoryOnly(file: TrajectoryStoreFile): TrajectoryStoreFile {
	return {
		...file,
		windows: file.windows.filter((window) => window.corpus === "prime"),
		labels: file.labels.filter((label) => label.corpus === "prime"),
	};
}

// ---------------------------------------------------------------------------
// Store (global single file; FileResolutionStore mechanics minus the cwd key)
// ---------------------------------------------------------------------------

const STORE_FILE_MODE = 0o600;
const STORE_DIR_MODE = 0o700;
const STORE_LOCK_ATTEMPTS = 40;
const STORE_LOCK_STALE_MS = 10_000;
/** A sealed diff over ~1000x-smaller day roll-ups is tiny; anything larger is treated as absent. */
const MAX_STORE_BYTES = 8 * 1024 * 1024;

/**
 * One global JSON file for the engineer. Writes are temp file + fsync + rename
 * under a cross-process lock; reads are guarded by a cached {mtimeMs,size} stat
 * so the turn path re-parses only when the file changed. Every failure degrades
 * to "no trajectory".
 */
class FileTrajectoryStore implements TrajectoryStore {
	private cached?: { mtimeMs: number; size: number; file: TrajectoryStoreFile | undefined };

	constructor(private readonly path: string) {}

	load(): TrajectoryStoreFile | undefined {
		const stat = this.statStore();
		if (!stat) return undefined;
		if (this.cached && this.cached.mtimeMs === stat.mtimeMs && this.cached.size === stat.size) {
			return this.cached.file;
		}
		const file = stat.size > MAX_STORE_BYTES ? this.tooLarge(stat.size) : this.read();
		this.cached = { mtimeMs: stat.mtimeMs, size: stat.size, file };
		return file;
	}

	private tooLarge(size: number): undefined {
		this.warn("trajectory store is larger than expected; ignoring it", new Error(`${size} bytes`));
		return undefined;
	}

	save(file: TrajectoryStoreFile): void {
		let release: (() => void) | undefined;
		try {
			mkdirSync(dirname(this.path), { recursive: true, mode: STORE_DIR_MODE });
			chmodSync(dirname(this.path), STORE_DIR_MODE);
			writeStoreGitignore(dirname(this.path));
			release = acquireStoreLock(this.path);
			if (!release) {
				log.warn("trajectory store is locked by another process; index not written", { path: this.path });
				return;
			}
			this.write(boundStoreFile(file));
		} catch (error) {
			this.warn("trajectory store write failed; index stays absent", error);
		} finally {
			try {
				release?.();
			} catch {
				// The lock goes stale on its own; a failed release must not break the caller.
			}
		}
	}

	private statStore(): { mtimeMs: number; size: number } | undefined {
		try {
			const stat = statSync(this.path, { throwIfNoEntry: false });
			return stat ? { mtimeMs: stat.mtimeMs, size: stat.size } : undefined;
		} catch (error) {
			this.warn("trajectory store cannot be stat'd", error);
			return undefined;
		}
	}

	private read(): TrajectoryStoreFile | undefined {
		try {
			const parsed: unknown = JSON.parse(readFileSync(this.path, "utf-8"));
			if (!isTrajectoryStoreFile(parsed)) {
				this.warn("trajectory store has an unexpected shape; ignoring it", undefined);
				return undefined;
			}
			return parsed;
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
				this.warn("trajectory store is unreadable; ignoring it", error);
			}
			return undefined;
		}
	}

	private write(file: TrajectoryStoreFile): void {
		const temp = `${this.path}.${process.pid}.${randomBytes(4).toString("hex")}.tmp`;
		try {
			const fd = openSync(temp, "wx", STORE_FILE_MODE);
			try {
				writeFileSync(fd, `${JSON.stringify(file, null, 2)}\n`);
				fsyncSync(fd);
			} finally {
				closeSync(fd);
			}
			renameSync(temp, this.path);
		} catch (error) {
			rmSync(temp, { force: true });
			throw error;
		}
		chmodSync(this.path, STORE_FILE_MODE);
		this.cached = undefined;
	}

	private warn(message: string, error: unknown): void {
		log.warn(message, {
			path: this.path,
			...(error === undefined ? {} : { error: error instanceof Error ? error.message : String(error) }),
		});
	}
}

/**
 * `proper-lockfile` implements the lock as a directory, so nothing here creates
 * the store file to take it; `realpath: false` lets the lock precede the file's
 * first write.
 */
function acquireStoreLock(path: string): (() => void) | undefined {
	const wait = new Int32Array(new SharedArrayBuffer(4));
	for (let attempt = 0; attempt < STORE_LOCK_ATTEMPTS; attempt++) {
		try {
			return lockSync(path, { realpath: false, lockfilePath: `${path}.lock`, stale: STORE_LOCK_STALE_MS });
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ELOCKED") throw error;
			Atomics.wait(wait, 0, 0, 5);
		}
	}
	return undefined;
}

/**
 * Keep the store bounded and prime-only whatever the writer passed: newest
 * DEFAULT_MAX_TRAJECTORY_WINDOWS windows, and no backfill datum on disk.
 */
export function boundStoreFile(file: TrajectoryStoreFile): TrajectoryStoreFile {
	const prime = primeTrajectoryOnly(file);
	const windows = [...prime.windows]
		.sort((a, b) => a.window.localeCompare(b.window))
		.slice(-DEFAULT_MAX_TRAJECTORY_WINDOWS);
	return { ...prime, windows };
}

/** Local gitignore so trajectory.json and backfill/ stay out of any checkout the agent dir happens to be. */
function writeStoreGitignore(learningDir: string): void {
	try {
		const path = join(learningDir, ".gitignore");
		const wanted = ["trajectory.json", "backfill/"];
		let existing = "";
		if (existsSync(path)) existing = readFileSync(path, "utf-8");
		const lines = new Set(existing.split("\n").map((line) => line.trim()));
		const missing = wanted.filter((entry) => !lines.has(entry));
		if (missing.length === 0) return;
		const next = existing.length > 0 && !existing.endsWith("\n") ? `${existing}\n` : existing;
		writeFileSync(path, `${next}${missing.join("\n")}\n`, { mode: STORE_FILE_MODE });
	} catch {
		// A read-only or absent learning dir must never break sealing.
	}
}

const storeCache = new Map<string, FileTrajectoryStore>();

/** The global trajectory store for an agent dir, memoized so turn-path reads share one stat cache. */
export function openTrajectoryStore(agentDir?: string): TrajectoryStore {
	const path = getTrajectoryIndexPath(agentDir);
	let store = storeCache.get(path);
	if (!store) {
		store = new FileTrajectoryStore(path);
		storeCache.set(path, store);
	}
	return store;
}

/** Turn-path read: the small sealed file through the stat cache. Opens no span, scans no days. */
export function readTrajectoryIndex(agentDir?: string): TrajectoryStoreFile | undefined {
	try {
		return openTrajectoryStore(agentDir).load();
	} catch (error) {
		log.warn("trajectory index read failed; treating as absent", {
			error: error instanceof Error ? error.message : String(error),
		});
		return undefined;
	}
}

/** Persist a sealed file (prime-only, bounded). Off the turn path. */
export function writeTrajectoryIndex(file: TrajectoryStoreFile, agentDir?: string): void {
	openTrajectoryStore(agentDir).save(file);
}

// ---------------------------------------------------------------------------
// Validation (validate-on-read)
// ---------------------------------------------------------------------------

function isTrajectoryStoreFile(value: unknown): value is TrajectoryStoreFile {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
	const file = value as Partial<TrajectoryStoreFile>;
	return (
		file.version === TRAJECTORY_STORE_VERSION &&
		typeof file.sealedAt === "string" &&
		typeof file.windowsObserved === "number" &&
		typeof file.minWindows === "number" &&
		Array.isArray(file.windows) &&
		file.windows.every(isTrajectoryWindow) &&
		Array.isArray(file.labels) &&
		file.labels.every(isTrajectoryLabel) &&
		Array.isArray(file.rate)
	);
}

function isTrajectoryWindow(value: unknown): value is TrajectoryWindow {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
	const window = value as Partial<TrajectoryWindow>;
	return (
		typeof window.window === "string" &&
		typeof window.turns === "number" &&
		typeof window.corpus === "string" &&
		Array.isArray(window.days) &&
		Array.isArray(window.fingerprints)
	);
}

function isTrajectoryLabel(value: unknown): value is TrajectoryLabel {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
	const label = value as Partial<TrajectoryLabel>;
	return (
		typeof label.fingerprint === "string" &&
		(label.label === null || label.label === "new" || label.label === "dropped" || label.label === "persists") &&
		Array.isArray(label.confounds) &&
		label.confounds.length >= 1
	);
}

// ---------------------------------------------------------------------------
// Backfill reader (CLI only; never summed with live days)
// ---------------------------------------------------------------------------

/**
 * Read the pre-generated cross-tool backfill day files, one corpus per
 * subdirectory of `learning/backfill/`, each in the LearningDay schema. Strictly
 * a CLI helper; the result is never written to the store or the prompt.
 */
export function readBackfillDays(backfillDir: string = getTrajectoryBackfillDir()): {
	corpus: TrajectoryCorpus;
	day: LearningDay;
}[] {
	const out: { corpus: TrajectoryCorpus; day: LearningDay }[] = [];
	let entries: string[];
	try {
		entries = readdirSync(backfillDir);
	} catch {
		return out;
	}
	for (const name of entries.sort()) {
		const corpusDir = join(backfillDir, name);
		let isDir = false;
		try {
			isDir = statSync(corpusDir).isDirectory();
		} catch {
			isDir = false;
		}
		if (!isDir) continue;
		const corpus: TrajectoryCorpus = `backfill:${name}`;
		for (const day of readLearningIndex(corpusDir)) out.push({ corpus, day });
	}
	return out;
}

// ---------------------------------------------------------------------------
// Prompt-lane helpers (join, suppression set, surfaced lines)
// ---------------------------------------------------------------------------

/** Only explicit ledger fingerprints reach the prompt; span:<hash> keys drive the CLI table only. */
function isExplicitLedgerFingerprint(fingerprint: string): boolean {
	return !fingerprint.startsWith("span:");
}

/** Fingerprints labelled internalized (dropped), excluding security-class ones (which Lever 2 must never silence). */
export function trajectoryInternalizedFingerprints(file: TrajectoryStoreFile | undefined): Set<string> {
	const set = new Set<string>();
	if (!file) return set;
	for (const label of file.labels) {
		if (label.corpus !== "prime") continue;
		if (label.label !== "dropped") continue;
		if (label.securityClass) continue;
		if (!isExplicitLedgerFingerprint(label.fingerprint)) continue;
		set.add(label.fingerprint);
	}
	return set;
}

const CLASS_OF_LABEL: Record<TrajectoryLabelKind, "stable-gap" | "new" | "internalized"> = {
	persists: "stable-gap",
	new: "new",
	dropped: "internalized",
};

/** stable-gap surfaces first, then new, then internalized: lower rank wins a conflict. */
const CLASS_RANK: Record<"stable-gap" | "new" | "internalized", number> = {
	"stable-gap": 0,
	new: 1,
	internalized: 2,
};

/**
 * Map a labelled fingerprint to the harness entries that address it, via the
 * trust-window join in HarnessState (proposalId <-> claimedFingerprints <->
 * touched entry refs, and FailureRecord.addressedByProposalIds). The key is the
 * merged harness-entry id.
 */
export function trajectoryClassForEntries(
	file: TrajectoryStoreFile,
	state: HarnessState,
): Map<string, "stable-gap" | "new" | "internalized"> {
	const classOf = new Map<string, "stable-gap" | "new" | "internalized">();
	const windows = state.trustWindows;
	if (!windows) return classOf;

	const fingerprintClass = new Map<string, "stable-gap" | "new" | "internalized">();
	for (const label of file.labels) {
		if (label.corpus !== "prime" || label.label === null) continue;
		if (!isExplicitLedgerFingerprint(label.fingerprint)) continue;
		// A security-class fingerprint is never demoted to "internalized" for prompt
		// ordering; its reminder must keep surfacing, matching the Lever-2 exemption.
		if (label.label === "dropped" && label.securityClass) continue;
		fingerprintClass.set(label.fingerprint, CLASS_OF_LABEL[label.label]);
	}
	if (fingerprintClass.size === 0) return classOf;

	const assign = (entryId: string, klass: "stable-gap" | "new" | "internalized"): void => {
		const existing = classOf.get(entryId);
		if (!existing || CLASS_RANK[klass] < CLASS_RANK[existing]) classOf.set(entryId, klass);
	};
	const applyWindow = (window: (typeof windows)[string], fingerprint: string): void => {
		const klass = fingerprintClass.get(fingerprint);
		if (!klass) return;
		for (const ref of window.touched) {
			const parsed = parseHarnessEntryRef(ref);
			if (parsed) assign(parsed.id, klass);
		}
	};

	for (const window of Object.values(windows)) {
		for (const fingerprint of window.claimedFingerprints) applyWindow(window, fingerprint);
	}
	// Also join through the failure ledger's addressedByProposalIds when present.
	const failures = state.failures?.failures;
	if (failures) {
		for (const [fingerprint, record] of Object.entries(failures)) {
			if (!fingerprintClass.has(fingerprint)) continue;
			for (const proposalId of record.addressedByProposalIds) {
				const window = windows[proposalId];
				if (window) applyWindow(window, fingerprint);
			}
		}
	}
	return classOf;
}

/**
 * Raw (unsanitized) confound-tagged lines for the stable-gap residue, most
 * recurring first. The caller runs each line through sanitizeRefinementPromptText.
 */
export function formatTrajectoryLines(file: TrajectoryStoreFile, max = 3): string[] {
	const K = file.windowsObserved;
	const stable = file.labels
		.filter((label) => label.corpus === "prime" && label.label === "persists")
		.filter((label) => isExplicitLedgerFingerprint(label.fingerprint))
		.sort((a, b) => b.windowsRecurring - a.windowsRecurring || a.fingerprint.localeCompare(b.fingerprint))
		.slice(0, Math.max(0, max));
	return stable.map((label) => {
		const name = label.name || label.fingerprint;
		const confounds = label.confounds.join(", ");
		return `stable-gap: ${name} recurs in ${label.windowsRecurring} of ${K} windows [confounds: ${confounds}]`;
	});
}

// ---------------------------------------------------------------------------
// CLI report
// ---------------------------------------------------------------------------

export interface TrajectoryReportRow {
	fingerprint: string;
	name: string;
	corpus: TrajectoryCorpus;
	label: string;
	sinceWindow: string;
	lastWindow: string;
	windowsRecurring: number;
	confounds: string;
	securityClass: boolean;
}

export interface TrajectoryReport {
	windowsObserved: number;
	minWindows: number;
	allWithheld: boolean;
	rows: TrajectoryReportRow[];
	rate: TrajectoryRateWindow[];
	windows: { window: string; corpus: TrajectoryCorpus; days: number; turns: number }[];
}

/** Shape the sealed file into printable rows for the CLI, newest signal first. */
export function buildTrajectoryReport(file: TrajectoryStoreFile): TrajectoryReport {
	const rank = (label: TrajectoryLabel): number => {
		if (label.label === "persists") return 0;
		if (label.label === "new") return 1;
		if (label.label === "dropped") return 2;
		return 3;
	};
	const rows = [...file.labels]
		.sort(
			(a, b) =>
				rank(a) - rank(b) ||
				b.windowsRecurring - a.windowsRecurring ||
				a.corpus.localeCompare(b.corpus) ||
				a.fingerprint.localeCompare(b.fingerprint),
		)
		.map((label) => ({
			fingerprint: label.fingerprint,
			name: label.name,
			corpus: label.corpus,
			label: label.label ?? (label.withheld ? `withheld: ${label.withheld}` : "-"),
			sinceWindow: label.sinceWindow,
			lastWindow: label.lastWindow,
			windowsRecurring: label.windowsRecurring,
			confounds: label.confounds.join(", "),
			securityClass: label.securityClass,
		}));
	const primeLabels = file.labels.filter((label) => label.corpus === "prime");
	const allWithheld = primeLabels.length === 0 || primeLabels.every((label) => label.label === null);
	return {
		windowsObserved: file.windowsObserved,
		minWindows: file.minWindows,
		allWithheld,
		rows,
		rate: file.rate,
		windows: file.windows.map((window) => ({
			window: window.window,
			corpus: window.corpus,
			days: window.days.length,
			turns: window.turns,
		})),
	};
}

/** The learning dir a store lives under, for callers that manage the gitignore themselves. */
export function trajectoryLearningDir(agentDir?: string): string {
	return getLearningDir(agentDir);
}
