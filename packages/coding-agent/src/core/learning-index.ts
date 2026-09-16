import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { gunzipSync } from "node:zlib";
import { type LogEntry, SPAN_END_MSG, TRACE_LOG_COMPONENT } from "@earendil-works/pi-ai";
import { normalizeFailureMessage } from "./ravo/failure-ledger.js";

/**
 * Learning index: a day-partitioned roll-up of `span_end` records keyed by
 * failure fingerprint, plus the `refinement.committed` records that name the
 * fingerprints a committed refinement claimed to address.
 *
 * It exists because the raw structured log cannot answer the only question
 * that matters for a learning loop — "did the thing we changed stop
 * failing?". `agent.jsonl` rotates by size, and on a busy machine a single
 * day fills every retained generation, so the evidence for a multi-week trend
 * is deleted before the trend can form. The roll-up is written while the raw
 * lines still exist and is three orders of magnitude smaller, so it survives.
 *
 * The comparison is a cohort study, not an experiment: fingerprints named in
 * a `refinement.committed` are the treated cohort, every other observed
 * fingerprint is the control, and the statistic is a one-sided Mann-Whitney U
 * on the per-fingerprint change in failure rate. Nothing is randomized, so
 * this measures association. It refuses to report a p-value when either
 * cohort is smaller than `minCohortN`, because a number computed from three
 * fingerprints reads exactly like a number computed from three hundred and is
 * worse than no number at all.
 */

export const LEARNING_INDEX_SCHEMA = 1;
/** Component used by the `refinement.committed` emitter in `refinement/ravo.ts`. */
export const REFINEMENT_LOG_COMPONENT = "coding-agent.refinement";
/** Log message naming the fingerprint ids a committed refinement addressed. */
export const REFINEMENT_COMMITTED_MSG = "refinement.committed";
/** Exposure unit: one assistant turn of the agent loop. */
export const TURN_SPAN_NAME = "agent.turn";
/** Span attribute carrying an explicit failure-ledger fingerprint id, when one is known. */
export const FAILURE_FINGERPRINT_ATTR = "failure.fingerprint";
/** Cohort size below which a p-value is withheld rather than printed. */
export const DEFAULT_MIN_COHORT_N = 5;
/** Rates are reported per this many turns. */
export const RATE_DENOMINATOR = 1000;

const DAY_RE = /^\d{4}-\d{2}-\d{2}$/;
const MAX_DURATION_SAMPLES = 20_000;
const MAX_DECOMPRESSED_LOG_BYTES = 64 * 1024 * 1024;
const MAX_FINGERPRINTS_PER_DAY = 50_000;
const MAX_COMMITS_PER_DAY = 10_000;

export interface FingerprintDayStats {
	/** Stable key: the ledger fingerprint id when the span carries one, else a hash of name+status+message. */
	fingerprint: string;
	name: string;
	status: string;
	/** True when this key counts failures; only failures enter the cohort comparison. */
	failure: boolean;
	count: number;
	p50Ms: number;
	p95Ms: number;
	/** Set when durations were capped, so the percentiles are over a prefix. */
	sampled?: boolean;
	/** Normalized error text, kept so the CLI can name the fingerprint. */
	message?: string;
}

export interface RefinementCommit {
	at: string;
	proposalId: string;
	addressed: string[];
}

export interface LearningDay {
	schema: typeof LEARNING_INDEX_SCHEMA;
	/** UTC calendar day, `YYYY-MM-DD`. */
	day: string;
	sealedAt: string;
	/** `agent.turn` span ends on this day: the denominator for every rate. */
	turns: number;
	fingerprints: FingerprintDayStats[];
	commits: RefinementCommit[];
	parseErrors: number;
	sourceFiles: string[];
}

export interface FingerprintTrend {
	fingerprint: string;
	name: string;
	message: string;
	treated: boolean;
	beforeCount: number;
	afterCount: number;
	/** Failures per `RATE_DENOMINATOR` turns in the window. */
	beforeRate: number;
	afterRate: number;
	/** `afterRate - beforeRate`; negative means the failure got rarer. */
	delta: number;
}

export interface CohortStats {
	ids: string[];
	n: number;
	medianDelta: number;
	meanDelta: number;
	deltas: number[];
}

export interface LearningDayPoint {
	day: string;
	turns: number;
	window: "before" | "pivot" | "after";
	/** Mean per-fingerprint failure rate across the cohort, per `RATE_DENOMINATOR` turns. */
	treatedRate: number;
	untreatedRate: number;
}

export interface LearningReport {
	schema: typeof LEARNING_INDEX_SCHEMA;
	generatedAt: string;
	days: string[];
	/** Commit used to split before/after: the median `refinement.committed` by time. */
	pivotAt: string | undefined;
	pivotDay: string | undefined;
	commits: number;
	turns: { before: number; after: number };
	fingerprints: FingerprintTrend[];
	/** Fingerprints named in a commit that were never observed in the index. */
	unobservedTreated: string[];
	cohorts: { treated: CohortStats; untreated: CohortStats };
	series: LearningDayPoint[];
	/** One-sided Mann-Whitney U statistic for the treated cohort; undefined when withheld. */
	u: number | undefined;
	/** P(treated deltas are not lower than untreated by chance); undefined when withheld. */
	pValue: number | undefined;
	/** Why the p-value was withheld. Mutually exclusive with `pValue`. */
	insufficientEvidence: string | undefined;
	minCohortN: number;
	rateDenominator: number;
}

export interface LearningReportOptions {
	minCohortN?: number;
	nowMs?: number;
}

// ---------------------------------------------------------------------------
// Log reading
// ---------------------------------------------------------------------------

/**
 * The files backing one log path, oldest first, matching the rotation in
 * `appendRotatingLog` (`<path>.old.<n>.gz`, then `<path>.old`, then `<path>`).
 */
export function learningLogFiles(logPath: string): string[] {
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
			.map((entry) => join(directory, entry.name));
	} catch {
		// A missing or unreadable log directory is reported by the caller as "no log".
	}
	return [...compressed, `${logPath}.old`, logPath].filter((file) => existsSync(file));
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

function entryDay(entry: LogEntry): string | undefined {
	if (typeof entry.ts !== "string" || entry.ts.length < 10) return undefined;
	const day = entry.ts.slice(0, 10);
	return DAY_RE.test(day) ? day : undefined;
}

function attrsOf(entry: LogEntry): Record<string, unknown> | undefined {
	const attrs = entry.attrs;
	if (typeof attrs !== "object" || attrs === null || Array.isArray(attrs)) return undefined;
	return attrs as Record<string, unknown>;
}

function isSpanEnd(entry: LogEntry): boolean {
	return (
		entry.component === TRACE_LOG_COMPONENT &&
		entry.msg === SPAN_END_MSG &&
		typeof entry.name === "string" &&
		entry.name.length > 0
	);
}

function stringList(value: unknown): string[] {
	return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

/**
 * The key a span end rolls up under. An explicit ledger fingerprint wins so
 * the index and the failure ledger agree on identity; otherwise the key is a
 * hash of the span name, its status and the normalized error text, which is
 * stable across runs but never collides with a ledger id.
 */
export function spanFingerprintKey(entry: LogEntry): { fingerprint: string; failure: boolean; message: string } {
	const name = String(entry.name);
	const status = typeof entry.status === "string" ? entry.status : "ok";
	const attrs = attrsOf(entry);
	const explicit = attrs?.[FAILURE_FINGERPRINT_ATTR];
	const message = normalizeFailureMessage(typeof entry.error === "string" ? entry.error : "");
	if (typeof explicit === "string" && explicit.length > 0) {
		return { fingerprint: explicit, failure: true, message };
	}
	// KNOWN DOUBLE COUNT, left in deliberately. An in-cell traceback ends
	// `kernel.cell` as an error (repl.py) and is separately fingerprinted onto
	// the enclosing `tool.execute`, so one failure yields two rows under two
	// keys. Measured on 157,374 spans: 482 rows, which collapse to 1 shadow
	// fingerprint today and would fan out to ~53 once the kernel error text is
	// hoisted. Suppressing `kernel.cell` by name was tried and is worse -- the
	// 39 SyntaxErrors (no traceback header, so nothing fingerprints them) and
	// the 613 host-initiated snapshot errors have no covering ancestor, and
	// dropping real failures from the control cohort biases toward a false
	// positive. A shadow in the control only biases toward the null. The real
	// fix is a second pass over each sealed day that resolves parentSpanId and
	// drops a descendant whose ancestor already carries a fingerprint.
	const failure = status === "error";
	const digest = createHash("sha256").update(`${name} ${status} ${message}`).digest("hex").slice(0, 16);
	return { fingerprint: `span:${digest}`, failure, message };
}

interface DayAccumulator {
	day: string;
	turns: number;
	keys: Map<string, { stats: FingerprintDayStats; durations: number[] }>;
	commits: RefinementCommit[];
	parseErrors: number;
	files: Set<string>;
}

function percentile(sorted: readonly number[], fraction: number): number {
	if (sorted.length === 0) return 0;
	const rank = Math.ceil(fraction * sorted.length);
	return sorted[Math.min(sorted.length - 1, Math.max(0, rank - 1))]!;
}

function accumulate(accumulators: Map<string, DayAccumulator>, entry: LogEntry, file: string): void {
	const day = entryDay(entry);
	if (day === undefined) return;
	let accumulator = accumulators.get(day);
	if (!accumulator) {
		accumulator = { day, turns: 0, keys: new Map(), commits: [], parseErrors: 0, files: new Set() };
		accumulators.set(day, accumulator);
	}
	accumulator.files.add(file);

	if (entry.msg === REFINEMENT_COMMITTED_MSG) {
		const addressed = stringList(entry.addressed);
		if (accumulator.commits.length < MAX_COMMITS_PER_DAY) {
			accumulator.commits.push({
				at: String(entry.ts),
				proposalId: typeof entry.proposalId === "string" ? entry.proposalId : "",
				addressed,
			});
		}
		return;
	}
	if (!isSpanEnd(entry)) return;
	if (entry.name === TURN_SPAN_NAME) accumulator.turns++;

	const { fingerprint, failure, message } = spanFingerprintKey(entry);
	let bucket = accumulator.keys.get(fingerprint);
	if (!bucket) {
		if (accumulator.keys.size >= MAX_FINGERPRINTS_PER_DAY) return;
		bucket = {
			stats: {
				fingerprint,
				name: String(entry.name),
				status: typeof entry.status === "string" ? entry.status : "ok",
				failure,
				count: 0,
				p50Ms: 0,
				p95Ms: 0,
				message,
			},
			durations: [],
		};
		accumulator.keys.set(fingerprint, bucket);
	}
	bucket.stats.count++;
	const durationMs = typeof entry.durationMs === "number" && Number.isFinite(entry.durationMs) ? entry.durationMs : 0;
	if (bucket.durations.length < MAX_DURATION_SAMPLES) {
		bucket.durations.push(durationMs);
	} else {
		bucket.stats.sampled = true;
	}
}

function finish(accumulator: DayAccumulator, sealedAt: string): LearningDay {
	const fingerprints: FingerprintDayStats[] = [];
	for (const { stats, durations } of accumulator.keys.values()) {
		const sorted = [...durations].sort((left, right) => left - right);
		fingerprints.push({ ...stats, p50Ms: percentile(sorted, 0.5), p95Ms: percentile(sorted, 0.95) });
	}
	fingerprints.sort((left, right) => right.count - left.count || left.fingerprint.localeCompare(right.fingerprint));
	return {
		schema: LEARNING_INDEX_SCHEMA,
		day: accumulator.day,
		sealedAt,
		turns: accumulator.turns,
		fingerprints,
		commits: accumulator.commits,
		parseErrors: accumulator.parseErrors,
		sourceFiles: [...accumulator.files].sort(),
	};
}

export interface RollUpResult {
	days: LearningDay[];
	parseErrors: number;
}

/** Roll every `span_end` and `refinement.committed` line in `files` up into day partitions. */
export function rollUpLearningDays(files: readonly string[], nowMs: number = Date.now()): RollUpResult {
	const accumulators = new Map<string, DayAccumulator>();
	let parseErrors = 0;
	for (const file of files) {
		const content = file.endsWith(".gz")
			? gunzipSync(readFileSync(file), { maxOutputLength: MAX_DECOMPRESSED_LOG_BYTES }).toString("utf8")
			: readFileSync(file, "utf8");
		for (const raw of content.split("\n")) {
			if (raw.length === 0) continue;
			const entry = parseLogEntry(raw);
			if (!entry) {
				parseErrors++;
				continue;
			}
			accumulate(accumulators, entry, file);
		}
	}
	const sealedAt = new Date(nowMs).toISOString();
	const days = [...accumulators.values()]
		.sort((left, right) => left.day.localeCompare(right.day))
		.map((accumulator) => finish(accumulator, sealedAt));
	return { days, parseErrors };
}

// ---------------------------------------------------------------------------
// Index persistence
// ---------------------------------------------------------------------------

function normalizeDay(value: unknown): LearningDay | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const raw = value as Record<string, unknown>;
	if (typeof raw.day !== "string" || !DAY_RE.test(raw.day)) return undefined;
	const fingerprints: FingerprintDayStats[] = [];
	for (const item of Array.isArray(raw.fingerprints) ? raw.fingerprints : []) {
		if (typeof item !== "object" || item === null || Array.isArray(item)) continue;
		const stat = item as Record<string, unknown>;
		if (typeof stat.fingerprint !== "string" || typeof stat.count !== "number") continue;
		fingerprints.push({
			fingerprint: stat.fingerprint,
			name: typeof stat.name === "string" ? stat.name : "",
			status: typeof stat.status === "string" ? stat.status : "ok",
			failure: stat.failure === true,
			count: Math.max(0, Math.trunc(stat.count)),
			p50Ms: typeof stat.p50Ms === "number" ? stat.p50Ms : 0,
			p95Ms: typeof stat.p95Ms === "number" ? stat.p95Ms : 0,
			message: typeof stat.message === "string" ? stat.message : undefined,
		});
	}
	const commits: RefinementCommit[] = [];
	for (const item of Array.isArray(raw.commits) ? raw.commits : []) {
		if (typeof item !== "object" || item === null || Array.isArray(item)) continue;
		const commit = item as Record<string, unknown>;
		commits.push({
			at: typeof commit.at === "string" ? commit.at : raw.day,
			proposalId: typeof commit.proposalId === "string" ? commit.proposalId : "",
			addressed: stringList(commit.addressed),
		});
	}
	return {
		schema: LEARNING_INDEX_SCHEMA,
		day: raw.day,
		sealedAt: typeof raw.sealedAt === "string" ? raw.sealedAt : "",
		turns: typeof raw.turns === "number" ? Math.max(0, Math.trunc(raw.turns)) : 0,
		fingerprints,
		commits,
		parseErrors: typeof raw.parseErrors === "number" ? raw.parseErrors : 0,
		sourceFiles: stringList(raw.sourceFiles),
	};
}

/** Read every sealed day from `indexDir`, oldest first. Unreadable files are skipped. */
export function readLearningIndex(indexDir: string): LearningDay[] {
	let names: string[];
	try {
		names = readdirSync(indexDir);
	} catch {
		return [];
	}
	const days: LearningDay[] = [];
	for (const name of names.sort()) {
		if (!name.endsWith(".json") || !DAY_RE.test(name.slice(0, -".json".length))) continue;
		try {
			const parsed = normalizeDay(JSON.parse(readFileSync(join(indexDir, name), "utf8")));
			if (parsed) days.push(parsed);
		} catch {
			// A truncated roll-up is skipped; the raw log can always be re-sealed.
		}
	}
	return days.sort((left, right) => left.day.localeCompare(right.day));
}

/** Write one day's roll-up atomically (temp + rename), mirroring `saveHarnessState`. */
export function writeLearningDay(indexDir: string, day: LearningDay): string {
	mkdirSync(indexDir, { recursive: true, mode: 0o700 });
	const target = join(indexDir, `${day.day}.json`);
	const temp = `${target}.${process.pid}.tmp`;
	try {
		writeFileSync(temp, `${JSON.stringify(day, undefined, 2)}\n`, { mode: 0o600 });
		renameSync(temp, target);
	} catch (error) {
		try {
			unlinkSync(temp);
		} catch {
			// The temp file may never have been created.
		}
		throw error;
	}
	return target;
}

export interface SealOptions {
	files: readonly string[];
	indexDir: string;
	nowMs?: number;
	/** Rewrite days that already have a roll-up on disk. */
	force?: boolean;
}

export interface SealResult {
	written: string[];
	skipped: string[];
	/** Days still accumulating (the current UTC day), deliberately not sealed. */
	open: string[];
	parseErrors: number;
}

/**
 * Seal every complete day found in `files` into `indexDir`. The current UTC
 * day is left open because more lines will land in it; every earlier day is
 * final, which is what makes the roll-up safe to write before the raw lines
 * rotate away.
 */
export function sealLearningDays(options: SealOptions): SealResult {
	const nowMs = options.nowMs ?? Date.now();
	const today = new Date(nowMs).toISOString().slice(0, 10);
	const { days, parseErrors } = rollUpLearningDays(options.files, nowMs);
	const result: SealResult = { written: [], skipped: [], open: [], parseErrors };
	for (const day of days) {
		if (day.day >= today) {
			result.open.push(day.day);
			continue;
		}
		if (!options.force && existsSync(join(options.indexDir, `${day.day}.json`))) {
			result.skipped.push(day.day);
			continue;
		}
		writeLearningDay(options.indexDir, day);
		result.written.push(day.day);
	}
	return result;
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/** Abramowitz & Stegun 7.1.26; absolute error below 1.5e-7, which the 0.05 decision never notices. */
function erf(x: number): number {
	const sign = x < 0 ? -1 : 1;
	const z = Math.abs(x);
	const t = 1 / (1 + 0.3275911 * z);
	const poly = ((((1.061405429 * t - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t;
	return sign * (1 - poly * Math.exp(-z * z));
}

export function normalCdf(z: number): number {
	return Math.min(1, Math.max(0, 0.5 * (1 + erf(z / Math.SQRT2))));
}

export interface MannWhitneyResult {
	u: number;
	z: number;
	pValue: number;
}

/**
 * One-sided Mann-Whitney U with midranks and the tie correction, testing
 * "values in `lower` are smaller than values in `higher`". The normal
 * approximation with a continuity correction is used throughout: the deltas
 * here are heavily tied (a fingerprint that never recurs has delta exactly
 * equal to its old rate), and an exact enumeration is not valid under ties.
 */
export function mannWhitneyOneSided(
	lower: readonly number[],
	higher: readonly number[],
): MannWhitneyResult | undefined {
	const n1 = lower.length;
	const n2 = higher.length;
	if (n1 === 0 || n2 === 0) return undefined;
	const total = n1 + n2;
	const combined = [
		...lower.map((value) => ({ value, first: true })),
		...higher.map((value) => ({ value, first: false })),
	];
	combined.sort((left, right) => left.value - right.value);
	let rankSumFirst = 0;
	let tieSum = 0;
	for (let index = 0; index < combined.length; ) {
		let end = index;
		while (end + 1 < combined.length && combined[end + 1]!.value === combined[index]!.value) end++;
		const size = end - index + 1;
		const midRank = (index + 1 + (end + 1)) / 2;
		for (let position = index; position <= end; position++) {
			if (combined[position]!.first) rankSumFirst += midRank;
		}
		tieSum += size ** 3 - size;
		index = end + 1;
	}
	const u = rankSumFirst - (n1 * (n1 + 1)) / 2;
	const mean = (n1 * n2) / 2;
	const variance = ((n1 * n2) / 12) * (total + 1 - tieSum / (total * (total - 1)));
	if (!(variance > 0)) return undefined;
	const z = (u - mean + 0.5) / Math.sqrt(variance);
	return { u, z, pValue: normalCdf(z) };
}

function median(values: readonly number[]): number {
	if (values.length === 0) return 0;
	const sorted = [...values].sort((left, right) => left - right);
	const middle = sorted.length >> 1;
	return sorted.length % 2 === 1 ? sorted[middle]! : (sorted[middle - 1]! + sorted[middle]!) / 2;
}

function mean(values: readonly number[]): number {
	return values.length === 0 ? 0 : values.reduce((sum, value) => sum + value, 0) / values.length;
}

function cohort(trends: readonly FingerprintTrend[]): CohortStats {
	const deltas = trends.map((trend) => trend.delta);
	return {
		ids: trends.map((trend) => trend.fingerprint),
		n: trends.length,
		medianDelta: median(deltas),
		meanDelta: mean(deltas),
		deltas,
	};
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

function emptyCohort(): CohortStats {
	return { ids: [], n: 0, medianDelta: 0, meanDelta: 0, deltas: [] };
}

function rate(count: number, turns: number): number {
	return turns === 0 ? 0 : (count * RATE_DENOMINATOR) / turns;
}

/**
 * Compare the change in per-fingerprint failure rate between the fingerprints
 * a refinement claimed to address and every other observed fingerprint.
 *
 * The split point is the median `refinement.committed` in the index. The
 * pivot day itself is excluded from both windows: the index is partitioned by
 * day, the commit lands inside one, and counting that day on either side
 * mixes treated and untreated exposure.
 */
export function buildLearningReport(days: readonly LearningDay[], options: LearningReportOptions = {}): LearningReport {
	const minCohortN = Math.max(1, Math.trunc(options.minCohortN ?? DEFAULT_MIN_COHORT_N));
	const sorted = [...days].sort((left, right) => left.day.localeCompare(right.day));
	const generatedAt = new Date(options.nowMs ?? Date.now()).toISOString();
	const commits = sorted.flatMap((day) => day.commits).sort((left, right) => left.at.localeCompare(right.at));
	const base: LearningReport = {
		schema: LEARNING_INDEX_SCHEMA,
		generatedAt,
		days: sorted.map((day) => day.day),
		pivotAt: undefined,
		pivotDay: undefined,
		commits: commits.length,
		turns: { before: 0, after: 0 },
		fingerprints: [],
		unobservedTreated: [],
		cohorts: { treated: emptyCohort(), untreated: emptyCohort() },
		series: [],
		u: undefined,
		pValue: undefined,
		insufficientEvidence: undefined,
		minCohortN,
		rateDenominator: RATE_DENOMINATOR,
	};
	if (commits.length === 0) {
		return { ...base, insufficientEvidence: "no refinement.committed record in the index" };
	}

	const pivot = commits[Math.floor((commits.length - 1) / 2)]!;
	const pivotDay = pivot.at.slice(0, 10);
	const treatedIds = new Set(commits.flatMap((commit) => commit.addressed));

	let beforeTurns = 0;
	let afterTurns = 0;
	const beforeCounts = new Map<string, number>();
	const afterCounts = new Map<string, number>();
	const labels = new Map<string, { name: string; message: string }>();
	const perDay: Array<{
		day: string;
		turns: number;
		window: LearningDayPoint["window"];
		counts: Map<string, number>;
	}> = [];
	for (const day of sorted) {
		const window = day.day < pivotDay ? "before" : day.day > pivotDay ? "after" : "pivot";
		if (window === "before") beforeTurns += day.turns;
		if (window === "after") afterTurns += day.turns;
		const counts = new Map<string, number>();
		for (const stat of day.fingerprints) {
			if (!stat.failure) continue;
			labels.set(stat.fingerprint, { name: stat.name, message: stat.message ?? "" });
			counts.set(stat.fingerprint, (counts.get(stat.fingerprint) ?? 0) + stat.count);
			const target = window === "before" ? beforeCounts : window === "after" ? afterCounts : undefined;
			if (target) target.set(stat.fingerprint, (target.get(stat.fingerprint) ?? 0) + stat.count);
		}
		perDay.push({ day: day.day, turns: day.turns, window, counts });
	}

	const fingerprints: FingerprintTrend[] = [];
	for (const [fingerprint, label] of labels) {
		const beforeCount = beforeCounts.get(fingerprint) ?? 0;
		const afterCount = afterCounts.get(fingerprint) ?? 0;
		if (beforeCount === 0 && afterCount === 0) continue;
		const beforeRate = rate(beforeCount, beforeTurns);
		const afterRate = rate(afterCount, afterTurns);
		fingerprints.push({
			fingerprint,
			name: label.name,
			message: label.message,
			treated: treatedIds.has(fingerprint),
			beforeCount,
			afterCount,
			beforeRate,
			afterRate,
			delta: afterRate - beforeRate,
		});
	}
	fingerprints.sort((left, right) => left.delta - right.delta || left.fingerprint.localeCompare(right.fingerprint));

	const observed = new Set(fingerprints.map((trend) => trend.fingerprint));
	const unobservedTreated = [...treatedIds].filter((id) => !observed.has(id)).sort();
	const cohorts = {
		treated: cohort(fingerprints.filter((trend) => trend.treated)),
		untreated: cohort(fingerprints.filter((trend) => !trend.treated)),
	};
	// The cohort denominators are the full membership, not the fingerprints
	// that happened to fire that day: dividing by the latter would make a quiet
	// day look as bad as a loud one.
	const cohortRate = (counts: Map<string, number>, turns: number, ids: readonly string[]): number =>
		ids.length === 0
			? 0
			: rate(
					ids.reduce((sum, id) => sum + (counts.get(id) ?? 0), 0),
					turns,
				) / ids.length;
	const series: LearningDayPoint[] = perDay.map((entry) => ({
		day: entry.day,
		turns: entry.turns,
		window: entry.window,
		treatedRate: cohortRate(entry.counts, entry.turns, cohorts.treated.ids),
		untreatedRate: cohortRate(entry.counts, entry.turns, cohorts.untreated.ids),
	}));
	const report: LearningReport = {
		...base,
		pivotAt: pivot.at,
		pivotDay,
		turns: { before: beforeTurns, after: afterTurns },
		fingerprints,
		unobservedTreated,
		cohorts,
		series,
	};

	if (beforeTurns === 0 || afterTurns === 0) {
		return {
			...report,
			insufficientEvidence: `no ${TURN_SPAN_NAME} exposure ${beforeTurns === 0 ? "before" : "after"} the pivot day`,
		};
	}
	if (cohorts.treated.n < minCohortN || cohorts.untreated.n < minCohortN) {
		return {
			...report,
			insufficientEvidence: `cohorts are too small (treated ${cohorts.treated.n}, untreated ${cohorts.untreated.n}, minimum ${minCohortN} each)`,
		};
	}
	const test = mannWhitneyOneSided(cohorts.treated.deltas, cohorts.untreated.deltas);
	if (!test) {
		return {
			...report,
			insufficientEvidence: "every fingerprint changed by the same amount; the test has no variance",
		};
	}
	return { ...report, u: test.u, pValue: test.pValue };
}
