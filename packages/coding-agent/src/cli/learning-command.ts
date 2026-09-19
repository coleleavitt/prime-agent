import { withSpan } from "@earendil-works/pi-ai";
import { APP_NAME, getAgentLogPath, getLearningIndexDir, getTrajectoryIndexPath } from "../config.js";
import {
	buildTrajectoryReport,
	DEFAULT_MIN_TRAJECTORY_WINDOWS,
	DEFAULT_TRAJECTORY_INTERNALIZED_GAP,
	readBackfillDays,
	sealTrajectoryWindows,
	type TrajectoryReport,
	type TrajectoryStoreFile,
	writeTrajectoryIndex,
} from "../core/distill/trajectory-index.js";
import {
	buildLearningReport,
	DEFAULT_MIN_COHORT_N,
	type FingerprintTrend,
	type LearningReport,
	learningLogFiles,
	readLearningIndex,
	type SealResult,
	sealLearningDays,
} from "../core/learning-index.js";
import { renderAsciiChart } from "./learning-chart.js";

/**
 * `prime-agent learning` seals complete days of the structured log into the
 * day-partitioned learning index, then reports whether the fingerprints a
 * refinement claimed to address actually got rarer than the ones it did not.
 *
 * The sealing step is the point of the command: `agent.jsonl` rotates by size
 * and a busy day fills every retained generation, so the roll-up has to be
 * written while the raw lines are still on disk. Running the report is what
 * keeps that happening.
 */

const MAX_ROWS = 40;
const SMALLEST_REPORTABLE_P = 1e-6;

export interface LearningCommandOptions {
	logPath: string | undefined;
	indexDir: string | undefined;
	minCohortN: number;
	json: boolean;
	seal: boolean;
	chart: boolean;
	limit: number;
}

export interface LearningCommandIo {
	stdout(line: string): void;
	stderr(line: string): void;
	now?(): number;
}

export class LearningCommandUsageError extends Error {}

export function parseLearningCommandArgs(args: string[]): LearningCommandOptions {
	let logPath: string | undefined;
	let indexDir: string | undefined;
	let minCohortN = DEFAULT_MIN_COHORT_N;
	let json = false;
	let seal = true;
	let chart = true;
	let limit = MAX_ROWS;
	const takeValue = (index: number, option: string): string => {
		const value = args[index];
		if (value === undefined || value.startsWith("-")) {
			throw new LearningCommandUsageError(`${option} requires a value.`);
		}
		return value;
	};
	const positiveInteger = (raw: string, option: string): number => {
		const parsed = Number.parseInt(raw, 10);
		if (!Number.isSafeInteger(parsed) || parsed <= 0) {
			throw new LearningCommandUsageError(`${option} requires a positive integer.`);
		}
		return parsed;
	};
	for (let index = 0; index < args.length; index++) {
		const arg = args[index]!;
		if (arg === "--json") {
			json = true;
		} else if (arg === "--no-seal") {
			seal = false;
		} else if (arg === "--no-chart") {
			chart = false;
		} else if (arg === "--log") {
			logPath = takeValue(++index, "--log");
		} else if (arg.startsWith("--log=")) {
			logPath = arg.slice("--log=".length) || undefined;
			if (!logPath) throw new LearningCommandUsageError("--log requires a value.");
		} else if (arg === "--index") {
			indexDir = takeValue(++index, "--index");
		} else if (arg.startsWith("--index=")) {
			indexDir = arg.slice("--index=".length) || undefined;
			if (!indexDir) throw new LearningCommandUsageError("--index requires a value.");
		} else if (arg === "--min-n") {
			minCohortN = positiveInteger(takeValue(++index, "--min-n"), "--min-n");
		} else if (arg.startsWith("--min-n=")) {
			minCohortN = positiveInteger(arg.slice("--min-n=".length), "--min-n");
		} else if (arg === "--limit") {
			limit = positiveInteger(takeValue(++index, "--limit"), "--limit");
		} else if (arg.startsWith("--limit=")) {
			limit = positiveInteger(arg.slice("--limit=".length), "--limit");
		} else {
			throw new LearningCommandUsageError(
				arg.startsWith("-") ? `Unknown option for learning: ${arg}` : `learning takes no operands: ${arg}`,
			);
		}
	}
	return { logPath, indexDir, minCohortN, json, seal, chart, limit };
}

function fixed(value: number, digits = 2): string {
	return Number.isFinite(value) ? value.toFixed(digits) : "-";
}

/**
 * Render the p-value, or the reason it is withheld. The approximation behind
 * it is good to roughly 1e-7 absolute, so anything smaller is reported as a
 * bound rather than as a number nobody can defend.
 */
export function formatSignificance(report: LearningReport): string {
	if (report.insufficientEvidence !== undefined || report.pValue === undefined) {
		return `p-value: withheld - ${report.insufficientEvidence ?? "no test was run"}`;
	}
	const p =
		report.pValue < SMALLEST_REPORTABLE_P
			? `< ${SMALLEST_REPORTABLE_P.toExponential(0)}`
			: `= ${report.pValue < 0.001 ? report.pValue.toExponential(2) : report.pValue.toPrecision(3)}`;
	return `Mann-Whitney U (one-sided, treated < untreated): U = ${fixed(report.u ?? 0, 1)}, p ${p}`;
}

function formatRows(trends: readonly FingerprintTrend[], limit: number): string[] {
	const shown = trends.slice(0, limit);
	if (shown.length === 0) return ["  (no failure fingerprints observed in either window)"];
	const idWidth = Math.max(11, ...shown.map((trend) => trend.fingerprint.length));
	const nameWidth = Math.max(4, ...shown.map((trend) => trend.name.length));
	const header = `  ${"fingerprint".padEnd(idWidth)}  ${"span".padEnd(nameWidth)}  ${"cohort".padEnd(7)}  ${"before/1k".padStart(9)}  ${"after/1k".padStart(9)}  ${"delta".padStart(9)}`;
	const rows = shown.map(
		(trend) =>
			`  ${trend.fingerprint.padEnd(idWidth)}  ${trend.name.padEnd(nameWidth)}  ${(trend.treated ? "treated" : "control").padEnd(7)}  ${fixed(trend.beforeRate).padStart(9)}  ${fixed(trend.afterRate).padStart(9)}  ${fixed(trend.delta).padStart(9)}`,
	);
	const lines = [header, `  ${"-".repeat(header.length - 2)}`, ...rows];
	if (trends.length > shown.length) lines.push(`  ... ${trends.length - shown.length} more (raise --limit)`);
	return lines;
}

export function formatLearningReport(
	report: LearningReport,
	indexDir: string,
	seal: SealResult | undefined,
	limit: number = MAX_ROWS,
): string {
	const out: string[] = [];
	out.push(`learning index  ${indexDir}`);
	const range = report.days.length > 0 ? ` (${report.days[0]} .. ${report.days[report.days.length - 1]})` : "";
	out.push(
		`  ${report.days.length} sealed day${report.days.length === 1 ? "" : "s"}${range}, ${report.turns.before + report.turns.after} turns in the compared windows, ${report.commits} refinement commit${report.commits === 1 ? "" : "s"}`,
	);
	if (seal) {
		out.push(
			`  sealed ${seal.written.length} new day${seal.written.length === 1 ? "" : "s"}, kept ${seal.skipped.length}, left ${seal.open.length} open${seal.parseErrors > 0 ? `, ${seal.parseErrors} unparsable log lines` : ""}`,
		);
	}
	if (report.pivotDay) {
		out.push(
			`  pivot ${report.pivotAt} (day ${report.pivotDay} excluded from both windows), before ${report.turns.before} turns / after ${report.turns.after} turns`,
		);
	}
	if (report.unobservedTreated.length > 0) {
		out.push(`  ${report.unobservedTreated.length} addressed fingerprint(s) were never observed and are not scored`);
	}
	out.push("");
	out.push(...formatRows(report.fingerprints, limit));
	out.push("");
	const cohortRow = (label: string, n: number, medianDelta: number, meanDelta: number) =>
		`  ${label.padEnd(9)}  ${String(n).padStart(3)}  ${fixed(medianDelta).padStart(13)}  ${fixed(meanDelta).padStart(11)}`;
	out.push(`  ${"cohort".padEnd(9)}    n   median delta   mean delta`);
	out.push(
		cohortRow(
			"treated",
			report.cohorts.treated.n,
			report.cohorts.treated.medianDelta,
			report.cohorts.treated.meanDelta,
		),
	);
	out.push(
		cohortRow(
			"control",
			report.cohorts.untreated.n,
			report.cohorts.untreated.medianDelta,
			report.cohorts.untreated.meanDelta,
		),
	);
	out.push("");
	out.push(`  ${formatSignificance(report)}`);
	return out.join("\n");
}

function chartLines(report: LearningReport): string[] {
	if (report.series.length === 0) return [];
	const markerIndex = report.series.findIndex((point) => point.window === "pivot");
	return renderAsciiChart(
		[
			{ label: "treated", mark: "T", points: report.series.map((point) => point.treatedRate) },
			{ label: "control", mark: "u", points: report.series.map((point) => point.untreatedRate) },
		],
		{
			xLabels: report.series.map((point) => point.day),
			markerIndex: markerIndex === -1 ? undefined : markerIndex,
			valueLabel: "mean failures per 1000 turns, per fingerprint",
		},
	);
}

export function runLearningCommand(args: string[], io: LearningCommandIo): number {
	if (args[0] === "trajectory") {
		return runTrajectorySubcommand(args.slice(1), io);
	}
	let options: LearningCommandOptions;
	try {
		options = parseLearningCommandArgs(args);
	} catch (error) {
		if (!(error instanceof LearningCommandUsageError)) throw error;
		io.stderr(`Error: ${error.message}`);
		io.stderr(
			`Usage: ${APP_NAME} learning [--log <path>] [--index <dir>] [--min-n <n>] [--limit <n>] [--no-seal] [--no-chart] [--json]`,
		);
		return 1;
	}
	const indexDir = options.indexDir ?? getLearningIndexDir();
	const nowMs = io.now?.() ?? Date.now();
	let seal: SealResult | undefined;
	if (options.seal) {
		const logPath = options.logPath ?? getAgentLogPath();
		const files = learningLogFiles(logPath);
		if (files.length === 0 && options.logPath !== undefined) {
			io.stderr(`Error: no log file at ${logPath}`);
			return 1;
		}
		try {
			seal = sealLearningDays({ files, indexDir, nowMs });
		} catch (error) {
			io.stderr(`Error: could not seal ${logPath}: ${error instanceof Error ? error.message : String(error)}`);
			return 1;
		}
	}
	const days = readLearningIndex(indexDir);
	if (days.length === 0) {
		io.stderr(`Error: no sealed days in ${indexDir}`);
		return 1;
	}
	const report = buildLearningReport(days, { minCohortN: options.minCohortN, nowMs });
	if (options.json) {
		io.stdout(JSON.stringify({ ...report, indexDir, seal }, undefined, 2));
		return report.pValue === undefined ? 2 : 0;
	}
	io.stdout(formatLearningReport(report, indexDir, seal, options.limit));
	if (options.chart) {
		io.stdout("");
		for (const line of chartLines(report)) io.stdout(line);
	}
	return report.pValue === undefined ? 2 : 0;
}

// ---------------------------------------------------------------------------
// `learning trajectory` subcommand (Engineer Trajectory Index)
// ---------------------------------------------------------------------------

interface TrajectoryCommandOptions {
	indexDir: string | undefined;
	includeBackfill: boolean;
	minWindows: number;
	gap: number;
	seal: boolean;
	json: boolean;
	limit: number;
}

const TRAJECTORY_MAX_ROWS = 40;

function parseTrajectoryCommandArgs(args: string[]): TrajectoryCommandOptions {
	let indexDir: string | undefined;
	let includeBackfill = false;
	let minWindows = DEFAULT_MIN_TRAJECTORY_WINDOWS;
	let gap = DEFAULT_TRAJECTORY_INTERNALIZED_GAP;
	let seal = true;
	let json = false;
	let limit = TRAJECTORY_MAX_ROWS;
	const takeValue = (index: number, option: string): string => {
		const value = args[index];
		if (value === undefined || value.startsWith("-")) {
			throw new LearningCommandUsageError(`${option} requires a value.`);
		}
		return value;
	};
	const positiveInteger = (raw: string, option: string): number => {
		const parsed = Number.parseInt(raw, 10);
		if (!Number.isSafeInteger(parsed) || parsed <= 0) {
			throw new LearningCommandUsageError(`${option} requires a positive integer.`);
		}
		return parsed;
	};
	for (let index = 0; index < args.length; index++) {
		const arg = args[index]!;
		if (arg === "--json") {
			json = true;
		} else if (arg === "--no-seal") {
			seal = false;
		} else if (arg === "--include-backfill") {
			includeBackfill = true;
		} else if (arg === "--index") {
			indexDir = takeValue(++index, "--index");
		} else if (arg.startsWith("--index=")) {
			indexDir = arg.slice("--index=".length) || undefined;
			if (!indexDir) throw new LearningCommandUsageError("--index requires a value.");
		} else if (arg === "--min-windows") {
			minWindows = positiveInteger(takeValue(++index, "--min-windows"), "--min-windows");
		} else if (arg.startsWith("--min-windows=")) {
			minWindows = positiveInteger(arg.slice("--min-windows=".length), "--min-windows");
		} else if (arg === "--gap") {
			gap = positiveInteger(takeValue(++index, "--gap"), "--gap");
		} else if (arg.startsWith("--gap=")) {
			gap = positiveInteger(arg.slice("--gap=".length), "--gap");
		} else if (arg === "--limit") {
			limit = positiveInteger(takeValue(++index, "--limit"), "--limit");
		} else if (arg.startsWith("--limit=")) {
			limit = positiveInteger(arg.slice("--limit=".length), "--limit");
		} else {
			throw new LearningCommandUsageError(
				arg.startsWith("-")
					? `Unknown option for learning trajectory: ${arg}`
					: `learning trajectory takes no operands: ${arg}`,
			);
		}
	}
	return { indexDir, includeBackfill, minWindows, gap, seal, json, limit };
}

function formatTrajectoryReport(report: TrajectoryReport, indexDir: string, storePath: string, limit: number): string {
	const out: string[] = [];
	out.push(`engineer trajectory  ${storePath}`);
	out.push(`  learning days  ${indexDir}`);
	out.push(
		`  ${report.windowsObserved} observed window${report.windowsObserved === 1 ? "" : "s"} (min ${report.minWindows} to emit a label)`,
	);
	if (report.windows.length > 0) {
		const spans = report.windows.map(
			(window) => `${window.window}(${window.corpus}:${window.days}d/${window.turns}t)`,
		);
		out.push(`  windows: ${spans.join(" ")}`);
	}
	out.push("");
	const shown = report.rows.slice(0, limit);
	if (shown.length === 0) {
		out.push("  (no failure fingerprints observed in any sealed window)");
	} else {
		const idWidth = Math.max(11, ...shown.map((row) => row.fingerprint.length));
		const nameWidth = Math.max(4, ...shown.map((row) => row.name.length || 1));
		const labelWidth = Math.max(5, ...shown.map((row) => row.label.length));
		const header = `  ${"fingerprint".padEnd(idWidth)}  ${"span".padEnd(nameWidth)}  ${"label".padEnd(labelWidth)}  ${"window span".padEnd(21)}  ${"rec".padStart(3)}  sec  confounds`;
		out.push(header);
		out.push(`  ${"-".repeat(Math.max(0, header.length - 2))}`);
		for (const row of shown) {
			const windowSpan = `${row.sinceWindow}..${row.lastWindow}`;
			out.push(
				`  ${row.fingerprint.padEnd(idWidth)}  ${(row.name || "-").padEnd(nameWidth)}  ${row.label.padEnd(labelWidth)}  ${windowSpan.padEnd(21)}  ${String(row.windowsRecurring).padStart(3)}  ${row.securityClass ? "yes" : " - "}  ${row.confounds}`,
			);
		}
		if (report.rows.length > shown.length) {
			out.push(`  ... ${report.rows.length - shown.length} more (raise --limit)`);
		}
	}
	out.push("");
	out.push("  rate of change (new - retired per window):");
	if (report.rate.length === 0) {
		out.push("    (none)");
	} else {
		for (const step of report.rate) {
			const value = step.newMinusRetired === null ? "null (gap)" : String(step.newMinusRetired);
			const retired = step.retired === null ? "null" : String(step.retired);
			out.push(`    ${step.window}  appeared ${step.appeared}, retired ${retired}, net ${value}`);
		}
	}
	return out.join("\n");
}

function runTrajectorySubcommand(args: string[], io: LearningCommandIo): number {
	let options: TrajectoryCommandOptions;
	try {
		options = parseTrajectoryCommandArgs(args);
	} catch (error) {
		if (!(error instanceof LearningCommandUsageError)) throw error;
		io.stderr(`Error: ${error.message}`);
		io.stderr(
			`Usage: ${APP_NAME} learning trajectory [--index <dir>] [--include-backfill] [--min-windows <n>] [--gap <m>] [--no-seal] [--limit <n>] [--json]`,
		);
		return 1;
	}
	const indexDir = options.indexDir ?? getLearningIndexDir();
	const nowMs = io.now?.() ?? Date.now();
	if (options.seal) {
		const files = learningLogFiles(getAgentLogPath());
		try {
			sealLearningDays({ files, indexDir, nowMs });
		} catch (error) {
			io.stderr(
				`Error: could not seal the learning index: ${error instanceof Error ? error.message : String(error)}`,
			);
			return 1;
		}
	}
	const days = readLearningIndex(indexDir);
	const backfillDays = options.includeBackfill ? readBackfillDays() : undefined;
	if (days.length === 0 && (!backfillDays || backfillDays.length === 0)) {
		io.stderr(`Error: no sealed days in ${indexDir}`);
		return 1;
	}

	const storePath = getTrajectoryIndexPath();
	const report = withSpan("trajectory.seal", {}, (span): TrajectoryReport => {
		// The persisted store is always prime-only, so backfill never lands on disk.
		const diskFile: TrajectoryStoreFile = sealTrajectoryWindows({
			days,
			minWindows: options.minWindows,
			internalizedGap: options.gap,
			nowMs,
		});
		writeTrajectoryIndex(diskFile);
		const displayFile: TrajectoryStoreFile = options.includeBackfill
			? sealTrajectoryWindows({
					days,
					backfillDays,
					minWindows: options.minWindows,
					internalizedGap: options.gap,
					nowMs,
				})
			: diskFile;
		const built = buildTrajectoryReport(displayFile);
		span.setAttributes({
			windows: built.windowsObserved,
			labelled: displayFile.labels.filter((label) => label.corpus === "prime" && label.label !== null).length,
			withheld: displayFile.labels.filter((label) => label.corpus === "prime" && label.withheld !== undefined)
				.length,
			backfill: options.includeBackfill,
		});
		return built;
	});

	if (options.json) {
		io.stdout(JSON.stringify({ ...report, indexDir, storePath }, undefined, 2));
		return report.allWithheld ? 2 : 0;
	}
	io.stdout(formatTrajectoryReport(report, indexDir, storePath, options.limit));
	return report.allWithheld ? 2 : 0;
}
