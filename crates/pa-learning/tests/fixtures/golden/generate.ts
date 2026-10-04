// The generator of the TS goldens in this directory (`tests/golden.rs` replays
// every scenario in Rust and compares whole JSON values, exact command output
// and exact file bytes).
//
// To regenerate: from `perf/session-catalog-resume`, copy
// `packages/coding-agent/src/{core/learning-index,core/distill/trajectory-index,
// cli/learning-chart,cli/learning-command,core/ravo/failure-ledger,
// core/ravo/canonical-json,core/ravo/referee}.ts` into `<build>/src/` (keeping
// that layout), copy `stubs/config.ts` to `<build>/src/config.ts` and
// `stubs/core/refinement/harness-trust.ts` to the same path under `<build>/src/`,
// put this file at `<build>/`, then
//   esbuild generate.ts --bundle --platform=node --format=esm \
//     --alias:@earendil-works/pi-ai=./stubs/pi-ai.ts \
//     --alias:proper-lockfile=./stubs/proper-lockfile.ts --outfile=gen.mjs
//   node gen.mjs <this directory>
// (with `stubs/` copied next to it).
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { gzipSync } from "node:zlib";
import { renderAsciiChart } from "./src/cli/learning-chart.ts";
import { formatSignificance, runLearningCommand } from "./src/cli/learning-command.ts";
import {
	buildTrajectoryReport,
	formatTrajectoryLines,
	isoWeek,
	matchesSecurityClass,
	sealTrajectoryWindows,
	trajectoryClassForEntries,
	trajectoryInternalizedFingerprints,
} from "./src/core/distill/trajectory-index.ts";
import {
	buildLearningReport,
	type LearningDay,
	type LearningReport,
	learningLogFiles,
	mannWhitneyOneSided,
	normalCdf,
	rollUpLearningDays,
	sealLearningDays,
	spanFingerprintKey,
	writeLearningDay,
} from "./src/core/learning-index.ts";

const out = process.argv[2]!;
const write = (name: string, value: unknown) =>
	writeFileSync(join(out, name), `${JSON.stringify(value, null, 2)}\n`);
const scratch = mkdtempSync(join(tmpdir(), "learning-golden-"));

/** Replace a run's temp paths with `<dir>` so the output is stable. */
function run(args: string[], nowMs: number, dir: string): { code: number; stdout: string[]; stderr: string[] } {
	const stdout: string[] = [];
	const stderr: string[] = [];
	const code = runLearningCommand(args, {
		stdout: (line) => stdout.push(line.split(dir).join("<dir>")),
		stderr: (line) => stderr.push(line.split(dir).join("<dir>")),
		now: () => nowMs,
	});
	return { code, stdout, stderr };
}

// --- the TS test corpus ------------------------------------------------------
// 8 sessions x 1000 turns over 16 UTC days, 20 failure fingerprints, 10 named
// in a `refinement.committed` at turn 4000 (`test/learning-index.test.ts`).

const SESSIONS = 8;
const TOTAL_TURNS = 8000;
const TURNS_PER_DAY = 500;
const DAYS = TOTAL_TURNS / TURNS_PER_DAY;
const COMMIT_TURN = 4000;
const BASE_DAY_MS = Date.UTC(2026, 7, 1);
const AFTER_CORPUS_MS = BASE_DAY_MS + (DAYS + 4) * 86_400_000;
const ids = Array.from({ length: 20 }, (_unused, index) => `fp${String(index).padStart(2, "0")}`);
const treatedIds = ids.slice(0, 10);
const untreatedIds = ids.slice(10);

function isoAt(turn: number): string {
	const day = Math.floor(turn / TURNS_PER_DAY);
	return new Date(BASE_DAY_MS + day * 86_400_000 + (turn % TURNS_PER_DAY) * 150_000).toISOString();
}

function hex(seed: number, length: number): string {
	return seed.toString(16).padStart(length, "0").slice(-length);
}

function seedLog(addressed: readonly string[], improved: readonly string[] = treatedIds): string {
	const lines: string[] = [];
	for (let turn = 0; turn < TOTAL_TURNS; turn++) {
		const ts = isoAt(turn);
		const session = turn % SESSIONS;
		const traceId = `${hex(session, 8)}${hex(turn, 24)}`;
		lines.push(
			JSON.stringify({
				level: "info",
				component: "trace",
				msg: "span_end",
				ts,
				name: "agent.turn",
				traceId,
				spanId: hex(turn, 16),
				durationMs: 800 + (turn % 400) + (turn % 7) / 8,
				status: "ok",
				attrs: { "session.id": `s${session}`, "turn.index": Math.floor(turn / SESSIONS) },
			}),
		);
		if (turn === COMMIT_TURN) {
			lines.push(
				JSON.stringify({
					ts,
					level: "info",
					component: "coding-agent.refinement",
					msg: "refinement.committed",
					traceId,
					proposalId: "champion-1",
					addressed: [...addressed],
					deepScore: 88,
					missed: 0,
				}),
			);
		}
		for (let index = 0; index < ids.length; index++) {
			const id = ids[index]!;
			const period = turn >= COMMIT_TURN && improved.includes(id) ? 1000 : 125;
			if (turn % period !== (index * 6) % 125) continue;
			lines.push(
				JSON.stringify({
					level: "warn",
					component: "trace",
					msg: "span_end",
					ts,
					name: "tool.execute",
					traceId,
					spanId: hex(turn * 32 + index, 16),
					parentSpanId: hex(turn, 16),
					durationMs: 40 + index,
					status: "error",
					error: `AttributeError: object has no attribute '${id}'`,
					attrs: { "failure.fingerprint": id, "tool.name": "ipython" },
				}),
			);
		}
	}
	return `${lines.join("\n")}\n`;
}

function corpus(): unknown {
	const dir = join(scratch, "corpus");
	mkdirSync(dir, { recursive: true });
	const logFor = (name: string, text: string): string => {
		const path = join(dir, name, "agent.jsonl");
		mkdirSync(join(dir, name), { recursive: true });
		writeFileSync(path, text);
		return path;
	};
	const treatedLog = seedLog(treatedIds);
	const swappedLog = seedLog(untreatedIds, treatedIds);
	const noneLog = treatedLog
		.split("\n")
		.filter((raw) => !raw.includes("refinement.committed"))
		.join("\n");
	const treatedPath = logFor("treated", treatedLog);
	const rolled = rollUpLearningDays([treatedPath], AFTER_CORPUS_MS);
	const report = (text: string, name: string, minCohortN?: number): LearningReport =>
		buildLearningReport(rollUpLearningDays([logFor(name, text)], AFTER_CORPUS_MS).days, {
			nowMs: AFTER_CORPUS_MS,
			minCohortN,
		});

	// Sealing with "now" inside the last day: that day stays open.
	const sealDir = join(dir, "seal-days");
	const insideLastDay = BASE_DAY_MS + (DAYS - 1) * 86_400_000 + 3_600_000;
	const firstSeal = sealLearningDays({ files: [treatedPath], indexDir: sealDir, nowMs: insideLastDay });
	const secondSeal = sealLearningDays({ files: [treatedPath], indexDir: sealDir, nowMs: AFTER_CORPUS_MS });

	const commands: Record<string, unknown> = {};
	const commandRuns: [string, string[]][] = [
		["text", ["--log", treatedPath, "--index", join(dir, "cmd-text")]],
		["json", ["--log", treatedPath, "--index", join(dir, "cmd-json"), "--json"]],
		["withheld", ["--log", treatedPath, "--index", join(dir, "cmd-withheld"), "--min-n", "11", "--no-chart"]],
		["limited", ["--log", treatedPath, "--index", join(dir, "cmd-limited"), "--limit=3", "--min-n=2"]],
		["unknown", ["--nope"]],
		["operand", ["extra"]],
		["missing-value", ["--log"]],
		["bad-integer", ["--min-n", "0"]],
		["no-log", ["--log", join(dir, "absent", "agent.jsonl"), "--index", join(dir, "cmd-absent")]],
		["no-days", ["--no-seal", "--index", join(dir, "cmd-empty")]],
	];
	for (const [name, args] of commandRuns) commands[name] = run(args, AFTER_CORPUS_MS, dir);

	return {
		afterCorpusMs: AFTER_CORPUS_MS,
		insideLastDayMs: insideLastDay,
		logSha256: {
			treated: createHash("sha256").update(treatedLog).digest("hex"),
			swapped: createHash("sha256").update(swappedLog).digest("hex"),
		},
		rolledDays: rolled.days.map((rolledDay) => ({
			...rolledDay,
			sourceFiles: rolledDay.sourceFiles.map((file) => file.split(dir).join("<dir>")),
		})),
		parseErrors: rolled.parseErrors,
		reports: {
			treated: report(treatedLog, "r-treated"),
			swapped: report(swappedLog, "r-swapped"),
			strict: report(treatedLog, "r-strict", 11),
			none: report(noneLog, "r-none"),
		},
		seal: {
			first: firstSeal,
			second: secondSeal,
			firstDayFile: readFileSync(join(sealDir, "2026-08-01.json"), "utf8").split(dir).join("<dir>"),
			commitDayFile: readFileSync(join(sealDir, "2026-08-09.json"), "utf8").split(dir).join("<dir>"),
		},
		commands,
	};
}

// --- a handcrafted log across rotated generations ----------------------------

function miscLog(): unknown {
	const dir = join(scratch, "misc");
	mkdirSync(dir, { recursive: true });
	const span = (fields: Record<string, unknown>) =>
		JSON.stringify({ level: "info", component: "trace", msg: "span_end", ...fields });
	const generations = {
		gz2: [
			span({ ts: "2026-09-01T10:00:00.000Z", name: "agent.turn", durationMs: 12.5 }),
			span({
				ts: "2026-09-01T10:00:01.000Z",
				name: "kernel.cell",
				status: "error",
				error: "ModuleNotFoundError: No module named 'paramiko' at /home/u/x.py line 12",
				durationMs: 3.25,
			}),
			"not json at all",
			JSON.stringify({ ts: "2026-09-01T10:00:02.000Z", msg: "no component" }),
		],
		gz1: [
			span({
				ts: "2026-09-01T23:59:59.999Z",
				name: "kernel.cell",
				status: "error",
				error: "ModuleNotFoundError: No module named 'httpx' at /tmp/y.py line 99",
				durationMs: 0.001,
			}),
			span({ ts: "2026-09-02T00:00:00.000Z", name: "agent.turn", durationMs: 7 }),
			span({ ts: "2026-09-02T00:00:00.500Z", name: "llm.request", status: "ok", durationMs: 1.1 }),
			span({ ts: "2026-09-02T00:00:00.600Z", name: "llm.request", status: "ok", durationMs: 2.2 }),
			span({ ts: "2026-09-02T00:00:00.700Z", name: "llm.request", status: "ok", durationMs: 0.3 }),
		],
		old: [
			JSON.stringify({
				ts: "2026-09-02T01:00:00.000Z",
				level: "info",
				component: "coding-agent.refinement",
				msg: "refinement.committed",
				proposalId: "p-2",
				addressed: ["abc0123456789def", 7, "fedcba9876543210"],
			}),
			JSON.stringify({
				ts: "2026-09-02T01:00:01.000Z",
				level: "info",
				component: "coding-agent.refinement",
				msg: "refinement.committed",
				proposalId: "p-claimless",
				addressed: [],
			}),
			span({
				ts: "2026-09-02T02:00:00.000Z",
				name: "tool.execute",
				status: "error",
				error: "boom",
				attrs: { "failure.fingerprint": "abc0123456789def" },
				durationMs: "12",
			}),
			span({ ts: "2026-09-02T02:00:01.000Z", name: "", durationMs: 1 }),
			JSON.stringify({ ts: "2026-09-02T02:00:02.000Z", level: "info", component: "other", msg: "span_end", name: "x" }),
			span({ ts: "bad-day", name: "agent.turn" }),
			"[1,2,3]",
		],
		live: [
			span({ ts: "2026-09-03T00:00:00.000Z", name: "agent.turn", durationMs: 5 }),
			span({ ts: "2026-09-03T00:00:01.000Z", name: "tool.execute", status: "error", attrs: { "failure.fingerprint": "" } }),
			"",
		],
	};
	const log = join(dir, "agent.jsonl");
	writeFileSync(`${log}.old.2.gz`, gzipSync(`${generations.gz2.join("\n")}\n`));
	writeFileSync(`${log}.old.1.gz`, gzipSync(`${generations.gz1.join("\n")}\n`));
	writeFileSync(`${log}.old`, `${generations.old.join("\n")}\n`);
	writeFileSync(log, generations.live.join("\n"));
	const files = learningLogFiles(log);
	const rolled = rollUpLearningDays(files, Date.UTC(2026, 8, 10));
	const relabel = (days: LearningDay[]) =>
		days.map((day) => ({ ...day, sourceFiles: day.sourceFiles.map((file) => basename(file)) }));
	return {
		generations,
		files: files.map((file) => basename(file)),
		nowMs: Date.UTC(2026, 8, 10),
		days: relabel(rolled.days),
		parseErrors: rolled.parseErrors,
		keys: [
			{ name: "kernel.cell", status: "error", error: "ModuleNotFoundError: No module named 'paramiko'" },
			{ name: "kernel.cell", status: "error", error: "ModuleNotFoundError: No module named 'httpx'" },
			{ name: "kernel.cell", status: "ok" },
			{ name: "tool.execute", error: "x", attrs: { "failure.fingerprint": "0011223344556677" } },
			{ name: "tool.execute", status: "error", error: "Error: ENOENT /var/tmp/a.txt 0xdeadbeef 42" },
		].map((entry) => ({ entry, key: spanFingerprintKey({ ts: "", level: "info", component: "trace", msg: "span_end", ...entry }) })),
	};
}

// --- statistics, significance and the chart ----------------------------------

function stats(): unknown {
	const samples: [number[], number[]][] = [
		[
			[-7, -7, -7, -6, -6, -6, -5, -5, -5, -5],
			[0, 0, 0, 1, 1, 1, 2, 2, 2, 2],
		],
		[
			[0, 0, 0, 1, 1, 1, 2, 2, 2, 2],
			[-7, -7, -7, -6, -6, -6, -5, -5, -5, -5],
		],
		[
			[-7.142857142857143, -7.142857142857143, -6.857142857142857, 0.25, 1],
			[0, 0, -0.25, 0.5, 0, 0.125, 3],
		],
		[[1], [1]],
		[[], [1, 2]],
		[[0.1, 0.2, 0.3], [0.15, 0.25, 0.35, 0.45]],
	];
	const report = (pValue: number | undefined, u: number | undefined, insufficientEvidence?: string) =>
		({ pValue, u, insufficientEvidence }) as LearningReport;
	return {
		mannWhitney: samples.map(([lower, higher]) => ({ lower, higher, result: mannWhitneyOneSided(lower, higher) ?? null })),
		normalCdf: [-8, -3.2, -1.96, -0.5, 0, 0.3, 1.2345, 2.5, 6, 40].map((z) => ({ z, p: normalCdf(z) })),
		significance: [
			report(0.5, 12),
			report(0.0123456, 3.5),
			report(0.99951, 99),
			report(0.000123456, 0),
			report(0.00099996, 1),
			report(1e-8, 0),
			report(undefined, undefined, "cohorts are too small (treated 1, untreated 2, minimum 5 each)"),
			report(undefined, undefined),
		].map((input) => ({ pValue: input.pValue ?? null, u: input.u ?? null, insufficientEvidence: input.insufficientEvidence ?? null, text: formatSignificance(input) })),
		charts: [
			{
				series: [
					{ label: "treated", mark: "T", points: [8, 8, 1, 1] },
					{ label: "control", mark: "u", points: [8, 8, 8, 8] },
				],
				options: { xLabels: ["2026-08-01", "2026-08-04"], markerIndex: 2, height: 5, width: 8 },
			},
			{
				series: [
					{ label: "a", mark: "a", points: Array.from({ length: 130 }, (_u, index) => Math.sin(index / 9) * 50 + (index % 13)) },
					{ label: "b", mark: "b", points: Array.from({ length: 130 }, (_u, index) => (index % 17 === 0 ? undefined : -index / 3)) },
				],
				options: { xLabels: ["2026-01-01", "2026-05-10"], markerIndex: 129, valueLabel: "values" },
			},
			{ series: [{ label: "flat", mark: "f", points: [0, 0, 0] }], options: { markerIndex: 5 } },
			{ series: [{ label: "big", mark: "g", points: [150, 1234.5, 99.995] }], options: { height: 2 } },
			{ series: [{ label: "none", mark: "n", points: [undefined, undefined] }], options: {} },
			{ series: [], options: {} },
		].map((input) => ({ ...input, lines: renderAsciiChart(input.series, input.options) })),
	};
}

// --- the Engineer Trajectory Index -------------------------------------------

const NOW_MS = Date.UTC(2025, 2, 10);
type Fp = { fingerprint: string; count: number; failure?: boolean; name?: string; message?: string };
const fp = (spec: Fp) => ({
	fingerprint: spec.fingerprint,
	name: spec.name ?? spec.fingerprint,
	status: spec.failure === false ? "ok" : "error",
	failure: spec.failure ?? true,
	count: spec.count,
	p50Ms: 0,
	p95Ms: 0,
	message: spec.message ?? "",
});
const day = (dayStr: string, turns: number, fps: Fp[], commits: LearningDay["commits"] = []): LearningDay => ({
	schema: 1,
	day: dayStr,
	sealedAt: "",
	turns,
	fingerprints: fps.map(fp),
	commits,
	parseErrors: 0,
	sourceFiles: [],
});
const W = ["2025-01-06", "2025-01-13", "2025-01-20", "2025-01-27", "2025-02-03", "2025-02-10"];

function trajectory(): unknown {
	const stay = { fingerprint: "fpStay", count: 2 };
	const drop = (count: number) => ({ fingerprint: "a1b2c3d4e5f60718", count, name: "tool_error: git push rejected" });
	const sec = (count: number) => ({ fingerprint: "00ffeeddccbbaa99", count, name: "kernel.cell", message: "token expired for the api" });
	const scenarios: Record<string, { days: LearningDay[]; backfill?: { corpus: string; day: LearningDay }[]; minWindows?: number; gap?: number }> = {
		windowing: {
			days: [
				day(W[0]!, 5, [{ fingerprint: "fpA", count: 1 }]),
				day("2025-01-08", 3, [{ fingerprint: "fpA", count: 4 }, { fingerprint: "fpZ", count: 1, name: "older" }]),
				day("2025-01-09", 1, [{ fingerprint: "fpZ", count: 1, name: "newer", message: "m" }]),
				day(W[1]!, 5, [{ fingerprint: "fpA", count: 2 }]),
				day(W[2]!, 5, [{ fingerprint: "fpA", count: 1 }, { fingerprint: "ok", count: 9, failure: false }]),
				day(W[3]!, 5, [{ fingerprint: "fpA", count: 3 }]),
				day(W[4]!, 5, [{ fingerprint: "fpA", count: 1 }]),
				day("2025-03-10", 9, [{ fingerprint: "fpA", count: 99 }]),
			],
		},
		labels: {
			days: [
				day(W[0]!, 5, [stay, drop(2), sec(2), { fingerprint: "span:0123456789abcdef", count: 3 }]),
				day(W[1]!, 5, [stay, drop(2), sec(2), { fingerprint: "span:0123456789abcdef", count: 3 }]),
				day(W[2]!, 5, [stay, { fingerprint: "fpOnce", count: 1 }, { fingerprint: "span:0123456789abcdef", count: 3 }]),
				day(W[3]!, 5, [stay, { fingerprint: "persistent", count: 2, name: "", message: "x" }]),
				day(W[4]!, 5, [stay, { fingerprint: "fpNew", count: 1 }, { fingerprint: "persistent", count: 2, name: "" }]),
			],
		},
		claimed: {
			days: [
				day(W[0]!, 5, [stay, drop(2)], [{ at: "2025-01-08T00:00:00Z", proposalId: "p1", addressed: ["a1b2c3d4e5f60718"] }]),
				day(W[1]!, 5, [stay, drop(2)]),
				day(W[2]!, 5, [stay]),
				day(W[3]!, 5, [stay]),
				day(W[4]!, 5, [stay]),
			],
		},
		inactive: {
			days: [
				day(W[0]!, 5, [stay, drop(2)]),
				day(W[1]!, 5, [stay, drop(2)]),
				day(W[2]!, 5, [stay]),
				day(W[3]!, 0, [stay]),
				day(W[4]!, 0, [stay]),
			],
		},
		withheld: { days: [day(W[0]!, 5, [stay]), day(W[1]!, 5, [stay]), day(W[2]!, 5, [stay])] },
		gap: {
			days: [day(W[0]!, 5, [{ fingerprint: "fpG", count: 2 }]), day(W[1]!, 5, [{ fingerprint: "fpG", count: 2 }]), day(W[4]!, 5, [{ fingerprint: "fpG", count: 2 }]), day(W[5]!, 5, [{ fingerprint: "fpG", count: 2 }, { fingerprint: "fpH", count: 1 }])],
			minWindows: 3,
			gap: 1,
		},
		rate: {
			days: [
				day(W[0]!, 5, [{ fingerprint: "fpX", count: 1 }, { fingerprint: "fpZ", count: 1 }]),
				day(W[1]!, 5, [{ fingerprint: "fpX", count: 1 }, { fingerprint: "fpY", count: 1 }]),
				day(W[2]!, 5, [{ fingerprint: "fpY", count: 1 }]),
				day(W[3]!, 5, [{ fingerprint: "fpY", count: 1 }]),
			],
		},
		backfill: {
			days: [day(W[0]!, 5, [stay]), day(W[1]!, 5, [stay]), day(W[2]!, 5, [stay]), day(W[3]!, 5, [stay])],
			backfill: [
				{ corpus: "backfill:opencode", day: day(W[0]!, 3, [{ fingerprint: "bf", count: 2, name: "exc:ValueError" }]) },
				{ corpus: "backfill:opencode", day: day(W[1]!, 3, [{ fingerprint: "bf", count: 2, name: "exc:ValueError" }]) },
				{ corpus: "backfill:claude", day: day(W[2]!, 1, [{ fingerprint: "cl", count: 1, name: "sh:permission-denied" }]) },
			],
		},
	};
	const sealed: Record<string, unknown> = {};
	for (const [name, scenario] of Object.entries(scenarios)) {
		const file = sealTrajectoryWindows({
			days: scenario.days,
			backfillDays: scenario.backfill as never,
			minWindows: scenario.minWindows,
			internalizedGap: scenario.gap,
			nowMs: NOW_MS,
		});
		sealed[name] = {
			input: scenario,
			file,
			report: buildTrajectoryReport(file),
			lines: formatTrajectoryLines(file, 3),
			internalized: [...trajectoryInternalizedFingerprints(file)].sort(),
		};
	}
	const labelsFile = sealTrajectoryWindows({ days: scenarios.labels!.days, nowMs: NOW_MS });
	const state = {
		schema: 1,
		entries: {},
		refinements: [],
		trustWindows: {
			p1: { proposalId: "p1", touched: ["memory:keep", "skill:sec-skill", "bad", ":x", "y:"], claimedFingerprints: ["fpStay"] },
			p2: { proposalId: "p2", touched: ["prompt:keep", "memory:dropme"], claimedFingerprints: ["a1b2c3d4e5f60718"] },
			p3: { proposalId: "p3", touched: ["memory:sec"], claimedFingerprints: ["00ffeeddccbbaa99"] },
			p4: { proposalId: "p4", touched: ["memory:via-ledger", "memory:new-one"], claimedFingerprints: [] },
		},
		failures: { failures: { fpNew: { addressedByProposalIds: ["p4", "absent"] }, fpStay: { addressedByProposalIds: [] } } },
	};
	const classOf = trajectoryClassForEntries(labelsFile, state as never);
	return {
		nowMs: NOW_MS,
		isoWeeks: ["2025-01-06", "2021-01-01", "2020-12-28", "2019-12-30", "2026-08-01", "2024-12-30", "2027-01-03"].map((d) => ({ day: d, week: isoWeek(d) })),
		security: [
			["authentication error", "invalid token"],
			["kernel.cell", "Token expired"],
			["tool.execute", "Permission Denied"],
			["tool.execute", "permission_denied"],
			["UnAuthorized", ""],
			["", ""],
			["bash", "exit 1"],
		].map(([name, message]) => ({ name, message, matches: matchesSecurityClass({ name, message }) })),
		sealed,
		classState: state,
		classOf: Object.fromEntries([...classOf.entries()].sort()),
	};
}

function trajectoryCommand(): unknown {
	const agentDir = join(scratch, "agent");
	process.env.GOLDEN_AGENT_DIR = agentDir;
	const indexDir = join(agentDir, "learning", "days");
	const runs: Record<string, unknown> = {};
	runs["no-days"] = run(["trajectory", "--no-seal"], NOW_MS, agentDir);
	runs.bogus = run(["trajectory", "--bogus"], NOW_MS, agentDir);
	runs.operand = run(["trajectory", "x"], NOW_MS, agentDir);
	const stay = { fingerprint: "fpStay", count: 2 };
	writeLearningDay(indexDir, day(W[0]!, 5, [{ fingerprint: "fpA", count: 2 }]));
	writeLearningDay(indexDir, day(W[1]!, 5, [{ fingerprint: "fpA", count: 2 }]));
	runs.withheld = run(["trajectory", "--no-seal", "--json"], NOW_MS, agentDir);
	for (const dayStr of W.slice(2, 5)) writeLearningDay(indexDir, day(dayStr, 5, [stay, { fingerprint: "a1b2c3d4e5f60718", count: 1, name: "git push rejected" }]));
	runs.table = run(["trajectory", "--no-seal"], NOW_MS, agentDir);
	const storeAfterTable = readFileSync(join(agentDir, "learning", "trajectory.json"), "utf8");
	const gitignore = readFileSync(join(agentDir, "learning", ".gitignore"), "utf8");
	for (const dayStr of W.slice(0, 4)) {
		writeLearningDay(join(agentDir, "learning", "backfill", "opencode"), day(dayStr, 3, [{ fingerprint: "bf-token", count: 2, name: "exc:ValueError" }]));
	}
	writeFileSync(join(agentDir, "learning", "backfill", "not-a-corpus.json"), "{}");
	runs.backfill = run(["trajectory", "--no-seal", "--include-backfill", "--json"], NOW_MS, agentDir);
	runs["backfill-table"] = run(["trajectory", "--no-seal", "--include-backfill", "--limit", "2", "--min-windows=2", "--gap", "1"], NOW_MS, agentDir);
	const storeAfterBackfill = readFileSync(join(agentDir, "learning", "trajectory.json"), "utf8");
	return { runs, storeAfterTable: storeAfterTable.split(agentDir).join("<dir>"), gitignore, storeAfterBackfill };
}

write("corpus.json", corpus());
write("misc-log.json", miscLog());
write("stats.json", stats());
write("trajectory.json", trajectory());
write("trajectory-command.json", trajectoryCommand());
rmSync(scratch, { recursive: true, force: true });
