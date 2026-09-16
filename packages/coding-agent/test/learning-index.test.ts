import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type * as PiAi from "@earendil-works/pi-ai";
import type { AssistantMessage, LogEntry, Model } from "@earendil-works/pi-ai";
import { setLogSink, stringifyLogEntry } from "@earendil-works/pi-ai";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { renderAsciiChart } from "../src/cli/learning-chart.js";
import { runLearningCommand } from "../src/cli/learning-command.js";
import {
	buildLearningReport,
	FAILURE_FINGERPRINT_ATTR,
	mannWhitneyOneSided,
	REFINEMENT_COMMITTED_MSG,
	REFINEMENT_LOG_COMPONENT,
	readLearningIndex,
	rollUpLearningDays,
	sealLearningDays,
	spanFingerprintKey,
	TURN_SPAN_NAME,
} from "../src/core/learning-index.js";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import type { FailureRecord } from "../src/core/ravo/failure-ledger.js";
import {
	countValidRefinementEdits,
	RAVO_DEFAULT_CONFIG,
	type RefinementProposal,
	ravoEvaluateProposal,
} from "../src/core/refinement/index.js";

const { completeSimpleMock } = vi.hoisted(() => ({ completeSimpleMock: vi.fn() }));

vi.mock("@earendil-works/pi-ai", async (importOriginal) => {
	const actual = await importOriginal<typeof PiAi>();
	return { ...actual, completeSimple: completeSimpleMock };
});

/**
 * Synthetic corpus, exactly as the plan's exit criterion specifies it:
 * 8 sessions x 1000 turns spread over 16 UTC days, 20 failure fingerprints,
 * 10 of them named in a `refinement.committed` at turn 4000, after which the
 * named ones emit at 1/1000 turns while the rest hold at 8/1000.
 *
 * Turn 4000 lands on day 8, which the report excludes as contaminated, so the
 * before window is days 0-7 (4000 turns) and the after window is days 9-15
 * (3500 turns).
 */
const SESSIONS = 8;
const TURNS_PER_SESSION = 1000;
const TOTAL_TURNS = SESSIONS * TURNS_PER_SESSION;
const TURNS_PER_DAY = 500;
const DAYS = TOTAL_TURNS / TURNS_PER_DAY;
const COMMIT_TURN = 4000;
const FINGERPRINT_COUNT = 20;
const TREATED_COUNT = 10;
const BASELINE_PERIOD = 125; // 8 failures per 1000 turns
const TREATED_PERIOD = 1000; // 1 failure per 1000 turns
const BASE_DAY_MS = Date.UTC(2026, 7, 1);
const AFTER_CORPUS_MS = BASE_DAY_MS + (DAYS + 4) * 86_400_000;

const fingerprintIds = Array.from(
	{ length: FINGERPRINT_COUNT },
	(_unused, index) => `fp${String(index).padStart(2, "0")}`,
);
const treatedIds = fingerprintIds.slice(0, TREATED_COUNT);
const untreatedIds = fingerprintIds.slice(TREATED_COUNT);
const offsetFor = (index: number): number => (index * 6) % BASELINE_PERIOD;

function isoAt(turn: number): string {
	const day = Math.floor(turn / TURNS_PER_DAY);
	const secondsIntoDay = (turn % TURNS_PER_DAY) * 150;
	return new Date(BASE_DAY_MS + day * 86_400_000 + secondsIntoDay * 1000).toISOString();
}

function hex(seed: number, length: number): string {
	return seed.toString(16).padStart(length, "0").slice(-length);
}

function spanEnd(fields: Record<string, unknown>): string {
	return JSON.stringify({ level: "info", component: "trace", msg: "span_end", ...fields });
}

/**
 * `improved` is the set of fingerprints whose rate actually drops after the
 * commit; `addressed` is the set the seeded `refinement.committed` claims.
 * They are separate arguments on purpose: the second exit criterion moves the
 * claim onto the fingerprints that did not improve while the data stays put.
 */
function seedLog(addressed: readonly string[], improved: readonly string[] = treatedIds): string {
	const lines: string[] = [];
	for (let turn = 0; turn < TOTAL_TURNS; turn++) {
		const ts = isoAt(turn);
		const session = turn % SESSIONS;
		const traceId = `${hex(session, 8)}${hex(turn, 24)}`;
		lines.push(
			spanEnd({
				ts,
				name: TURN_SPAN_NAME,
				traceId,
				spanId: hex(turn, 16),
				durationMs: 800 + (turn % 400),
				status: "ok",
				attrs: { "session.id": `s${session}`, "turn.index": Math.floor(turn / SESSIONS) },
			}),
		);
		if (turn === COMMIT_TURN) {
			lines.push(
				JSON.stringify({
					ts,
					level: "info",
					component: REFINEMENT_LOG_COMPONENT,
					msg: REFINEMENT_COMMITTED_MSG,
					traceId,
					proposalId: "champion-1",
					addressed: [...addressed],
					deepScore: 88,
					missed: 0,
				}),
			);
		}
		for (let index = 0; index < FINGERPRINT_COUNT; index++) {
			const id = fingerprintIds[index]!;
			const treated = turn >= COMMIT_TURN && improved.includes(id);
			const period = treated ? TREATED_PERIOD : BASELINE_PERIOD;
			if (turn % period !== offsetFor(index)) continue;
			lines.push(
				spanEnd({
					ts,
					name: "tool.execute",
					traceId,
					spanId: hex(turn * 32 + index, 16),
					parentSpanId: hex(turn, 16),
					durationMs: 40 + index,
					status: "error",
					error: `AttributeError: object has no attribute '${id}'`,
					attrs: { [FAILURE_FINGERPRINT_ATTR]: id, "tool.name": "ipython" },
				}),
			);
		}
	}
	return `${lines.join("\n")}\n`;
}

const tempDirs: string[] = [];
let previousAgentDir: string | undefined;
let liveAgentDir: string;

function tempDir(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	tempDirs.push(dir);
	return dir;
}

function writeLog(content: string): string {
	const path = join(tempDir("learning-log-"), "agent.jsonl");
	writeFileSync(path, content);
	return path;
}

beforeAll(() => {
	// Never read or write the developer's real ~/.prime/agent from a test.
	previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	liveAgentDir = mkdtempSync(join(tmpdir(), "learning-agent-dir-"));
	process.env.PRIME_AGENT_CODING_AGENT_DIR = liveAgentDir;
});

afterAll(() => {
	if (previousAgentDir === undefined) {
		delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	} else {
		process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	}
	rmSync(liveAgentDir, { recursive: true, force: true });
});

afterEach(() => {
	while (tempDirs.length > 0) rmSync(tempDirs.pop()!, { recursive: true, force: true });
});

function reportFor(addressed: readonly string[]) {
	const { days } = rollUpLearningDays([writeLog(seedLog(addressed))], AFTER_CORPUS_MS);
	return { days, report: buildLearningReport(days, { nowMs: AFTER_CORPUS_MS }) };
}

describe("day-partitioned roll-up", () => {
	it("keys span ends by fingerprint and records count, p50 and p95 per day", () => {
		const { days } = rollUpLearningDays([writeLog(seedLog(treatedIds))], AFTER_CORPUS_MS);
		expect(days).toHaveLength(DAYS);
		expect(days[0]!.day).toBe("2026-08-01");
		expect(days[DAYS - 1]!.day).toBe("2026-08-16");
		for (const day of days) expect(day.turns).toBe(TURNS_PER_DAY);

		const firstDay = days[0]!;
		const turnRow = firstDay.fingerprints.find((stat) => stat.name === TURN_SPAN_NAME)!;
		expect(turnRow.count).toBe(TURNS_PER_DAY);
		expect(turnRow.failure).toBe(false);
		expect(turnRow.p50Ms).toBeGreaterThan(0);
		expect(turnRow.p95Ms).toBeGreaterThanOrEqual(turnRow.p50Ms);

		const fp00 = firstDay.fingerprints.find((stat) => stat.fingerprint === "fp00")!;
		expect(fp00.failure).toBe(true);
		expect(fp00.status).toBe("error");
		// 500 turns a day at one failure per 125 turns.
		expect(fp00.count).toBe(4);
		expect(fp00.p50Ms).toBe(40);

		const commitDay = days.find((day) => day.commits.length > 0)!;
		expect(commitDay.day).toBe("2026-08-09");
		expect(commitDay.commits[0]!.addressed).toEqual(treatedIds);
	});

	it("derives a stable fingerprint for spans that carry no ledger id", () => {
		const entry = {
			ts: "2026-08-01T00:00:00.000Z",
			level: "warn" as const,
			component: "trace",
			msg: "span_end",
			name: "kernel.cell",
			status: "error",
			error: "ModuleNotFoundError: No module named 'paramiko'",
		};
		const first = spanFingerprintKey(entry);
		const second = spanFingerprintKey({ ...entry, ts: "2026-09-02T11:22:33.000Z" });
		const other = spanFingerprintKey({ ...entry, error: "AttributeError: 'Foo' object has no attribute 'bar'" });
		expect(first.fingerprint).toMatch(/^span:[0-9a-f]{16}$/);
		expect(first.failure).toBe(true);
		expect(second.fingerprint).toBe(first.fingerprint);
		expect(other.fingerprint).not.toBe(first.fingerprint);
		// Ledger normalization collapses quoted operands, so two spellings of the
		// same defect share one key. That is the point of a fingerprint.
		expect(spanFingerprintKey({ ...entry, error: "ModuleNotFoundError: No module named 'httpx'" }).fingerprint).toBe(
			first.fingerprint,
		);
		expect(spanFingerprintKey({ ...entry, status: "ok", error: undefined }).failure).toBe(false);
	});
});

describe("treated versus untreated cohorts", () => {
	it("detects the drop when the addressed fingerprints are the ones that improved", () => {
		const { report } = reportFor(treatedIds);
		expect(report.pivotDay).toBe("2026-08-09");
		expect(report.turns).toEqual({ before: 4000, after: 3500 });
		expect(report.cohorts.treated.n).toBe(TREATED_COUNT);
		expect(report.cohorts.untreated.n).toBe(FINGERPRINT_COUNT - TREATED_COUNT);
		expect(report.cohorts.treated.ids.slice().sort()).toEqual(treatedIds.slice().sort());

		expect(report.cohorts.treated.medianDelta).toBeLessThan(report.cohorts.untreated.medianDelta - 0.5);
		expect(report.pValue).toBeDefined();
		expect(report.pValue!).toBeLessThan(0.05);
		expect(report.insufficientEvidence).toBeUndefined();

		const treated = report.fingerprints.find((trend) => trend.fingerprint === "fp00")!;
		expect(treated.beforeRate).toBeCloseTo(8, 6);
		expect(treated.afterRate).toBeCloseTo(3 / 3.5, 6);
		const untreated = report.fingerprints.find((trend) => trend.fingerprint === "fp19")!;
		expect(untreated.beforeRate).toBeCloseTo(8, 6);
		expect(untreated.afterRate).toBeCloseTo(8, 6);
		expect(untreated.delta).toBeCloseTo(0, 6);
	});

	it("withholds significance when the addressed list names the fingerprints that did not improve", () => {
		// Identical corpus; only the claim moves. fp00-fp09 still improve.
		const { days } = rollUpLearningDays([writeLog(seedLog(untreatedIds, treatedIds))], AFTER_CORPUS_MS);
		const report = buildLearningReport(days, { nowMs: AFTER_CORPUS_MS });
		expect(report.cohorts.treated.n).toBe(TREATED_COUNT);
		expect(report.cohorts.treated.ids.slice().sort()).toEqual(untreatedIds.slice().sort());
		expect(report.cohorts.treated.medianDelta).toBeCloseTo(0, 6);
		expect(report.cohorts.untreated.medianDelta).toBeLessThan(-0.5);
		expect(report.pValue).toBeDefined();
		expect(report.pValue!).toBeGreaterThanOrEqual(0.05);
	});

	it("refuses to produce a p-value below the minimum cohort size", () => {
		const { days } = reportFor(treatedIds);
		const strict = buildLearningReport(days, { nowMs: AFTER_CORPUS_MS, minCohortN: 11 });
		expect(strict.pValue).toBeUndefined();
		expect(strict.u).toBeUndefined();
		expect(strict.insufficientEvidence).toContain("minimum 11 each");
		// The measurements are still reported; only the inference is withheld.
		expect(strict.cohorts.treated.medianDelta).toBeLessThan(-0.5);
	});

	it("refuses when nothing has ever been committed", () => {
		const log = seedLog(treatedIds)
			.split("\n")
			.filter((raw) => !raw.includes(REFINEMENT_COMMITTED_MSG))
			.join("\n");
		const { days } = rollUpLearningDays([writeLog(log)], AFTER_CORPUS_MS);
		const report = buildLearningReport(days, { nowMs: AFTER_CORPUS_MS });
		expect(report.pValue).toBeUndefined();
		expect(report.insufficientEvidence).toBe("no refinement.committed record in the index");
		expect(report.fingerprints).toEqual([]);
	});
});

describe("mannWhitneyOneSided", () => {
	it("is one-sided: perfect separation is significant in one direction only", () => {
		const low = [-7, -7, -7, -6, -6, -6, -5, -5, -5, -5];
		const high = [0, 0, 0, 1, 1, 1, 2, 2, 2, 2];
		const forward = mannWhitneyOneSided(low, high)!;
		const reversed = mannWhitneyOneSided(high, low)!;
		expect(forward.u).toBe(0);
		expect(forward.pValue).toBeLessThan(0.05);
		expect(reversed.u).toBe(100);
		expect(reversed.pValue).toBeGreaterThan(0.95);
	});

	it("returns undefined when a cohort is empty or every value is identical", () => {
		expect(mannWhitneyOneSided([], [1, 2])).toBeUndefined();
		expect(mannWhitneyOneSided([1], [1])).toBeUndefined();
	});
});

describe("sealing the index", () => {
	it("writes complete days, leaves the current day open, and is idempotent", () => {
		const logPath = writeLog(seedLog(treatedIds));
		const indexDir = join(tempDir("learning-index-"), "days");
		// "now" sits inside the last corpus day, so that day must stay open.
		const insideLastDay = BASE_DAY_MS + (DAYS - 1) * 86_400_000 + 3_600_000;
		const first = sealLearningDays({ files: [logPath], indexDir, nowMs: insideLastDay });
		expect(first.written).toHaveLength(DAYS - 1);
		expect(first.open).toEqual(["2026-08-16"]);
		expect(existsSync(join(indexDir, "2026-08-01.json"))).toBe(true);
		expect(existsSync(join(indexDir, "2026-08-16.json"))).toBe(false);

		const again = sealLearningDays({ files: [logPath], indexDir, nowMs: AFTER_CORPUS_MS });
		expect(again.skipped).toHaveLength(DAYS - 1);
		expect(again.written).toEqual(["2026-08-16"]);

		const days = readLearningIndex(indexDir);
		expect(days).toHaveLength(DAYS);
		expect(days.map((day) => day.day)).toEqual([...days.map((day) => day.day)].sort());
		expect(buildLearningReport(days, { nowMs: AFTER_CORPUS_MS }).pValue!).toBeLessThan(0.05);
		// No temp files survive the atomic rename.
		expect(readFileSync(join(indexDir, "2026-08-01.json"), "utf8").endsWith("\n")).toBe(true);
	});
});

describe("prime-agent learning", () => {
	function run(args: string[]): { code: number; stdout: string; stderr: string } {
		const stdout: string[] = [];
		const stderr: string[] = [];
		const code = runLearningCommand(args, {
			stdout: (line) => stdout.push(line),
			stderr: (line) => stderr.push(line),
			now: () => AFTER_CORPUS_MS,
		});
		return { code, stdout: stdout.join("\n"), stderr: stderr.join("\n") };
	}

	it("seals, prints the table, the cohorts and a chart", () => {
		const logPath = writeLog(seedLog(treatedIds));
		const indexDir = join(tempDir("learning-cli-"), "days");
		const { code, stdout, stderr } = run(["--log", logPath, "--index", indexDir]);
		expect(stderr).toBe("");
		expect(code).toBe(0);
		expect(stdout).toContain("16 sealed days (2026-08-01 .. 2026-08-16)");
		expect(stdout).toContain("pivot 2026-08-09T");
		expect(stdout).toContain("fp00");
		expect(stdout).toContain("treated");
		expect(stdout).toContain("Mann-Whitney U (one-sided, treated < untreated)");
		expect(stdout).toContain("T treated");
		expect(stdout).not.toMatch(/p-value: withheld/);
	});

	it("prints the reason instead of a p-value when the cohorts are too small", () => {
		const logPath = writeLog(seedLog(treatedIds));
		const indexDir = join(tempDir("learning-cli-"), "days");
		const { code, stdout } = run(["--log", logPath, "--index", indexDir, "--min-n", "11", "--no-chart"]);
		expect(code).toBe(2);
		expect(stdout).toContain("p-value: withheld - cohorts are too small (treated 10, untreated 10, minimum 11 each)");
		expect(stdout).not.toMatch(/p [<=] /);
	});

	it("emits the raw report under --json and rejects unknown options", () => {
		const logPath = writeLog(seedLog(treatedIds));
		const indexDir = join(tempDir("learning-cli-"), "days");
		const { code, stdout } = run(["--log", logPath, "--index", indexDir, "--json"]);
		expect(code).toBe(0);
		const parsed = JSON.parse(stdout);
		expect(parsed.cohorts.treated.n).toBe(TREATED_COUNT);
		expect(parsed.pValue).toBeLessThan(0.05);
		expect(parsed.indexDir).toBe(indexDir);

		const bad = run(["--nope"]);
		expect(bad.code).toBe(1);
		expect(bad.stderr).toContain("Unknown option for learning: --nope");
	});
});

describe("renderAsciiChart", () => {
	it("draws both series, marks shared cells and labels the axis", () => {
		const lines = renderAsciiChart(
			[
				{ label: "treated", mark: "T", points: [8, 8, 1, 1] },
				{ label: "control", mark: "u", points: [8, 8, 8, 8] },
			],
			{ xLabels: ["2026-08-01", "2026-08-04"], markerIndex: 2, height: 5, width: 8 },
		);
		const text = lines.join("\n");
		expect(text).toContain("T");
		expect(text).toContain("u");
		expect(text).toContain("*");
		expect(text).toContain("^");
		expect(text).toContain("2026-08-01");
		expect(renderAsciiChart([], {})).toEqual(["(no data to chart)"]);
	});
});

describe("refinement.committed reaches the index", () => {
	const judgeModel: Model<"openai-completions"> = {
		id: "openai/gpt-5.5",
		name: "GPT 5.5",
		api: "openai-completions",
		provider: "prime-inference",
		baseUrl: "https://inference.primeintellect.ai/v1",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 200000,
		maxTokens: 8192,
	};
	const proposal: RefinementProposal = {
		summary: "Guard websearch results",
		rationale: "The websearch skill raised KeyError three times.",
		expectedOutcome: "No more KeyError from websearch.",
		edits: [{ action: "create", kind: "memory", title: "websearch guard", content: "Check results key." }],
	};
	const recurring: FailureRecord = {
		fingerprint: {
			id: "fp1234567890abcd",
			kind: "python_exception",
			source: "websearch",
			exceptionClass: "KeyError",
			message: "keyerror: ?",
		},
		count: 3,
		firstSeenTurn: 2,
		lastSeenTurn: 6,
		firstSeenAt: "2026-08-01T00:00:00.000Z",
		lastSeenAt: "2026-08-01T00:01:00.000Z",
		excerpt: "KeyError: 'results'",
		addressedByProposalIds: [],
	};

	function assistantText(text: string): AssistantMessage {
		return {
			role: "assistant",
			content: [{ type: "text", text }],
			api: "openai-completions",
			provider: "prime-inference",
			model: "openai/gpt-5.5",
			usage: {
				input: 1,
				output: 1,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 2,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop",
			timestamp: Date.now(),
		};
	}

	async function evaluate(judge: Record<string, unknown>): Promise<LogEntry[]> {
		const captured: LogEntry[] = [];
		completeSimpleMock.mockReset();
		completeSimpleMock.mockResolvedValueOnce(assistantText(JSON.stringify(judge)));
		setLogSink((entry) => captured.push(entry));
		try {
			await ravoEvaluateProposal(proposal, {
				state: emptyAssistedRavoState(),
				config: RAVO_DEFAULT_CONFIG,
				validEdits: countValidRefinementEdits(proposal),
				conversationText: "",
				harnessOverview: "",
				baseline: null,
				proposalId: "refine_1",
				model: judgeModel,
				apiKey: "not-a-real-key",
				recurringFailures: [recurring],
				turn: 7,
			});
		} finally {
			setLogSink(undefined);
		}
		return captured.filter((entry) => entry.msg === REFINEMENT_COMMITTED_MSG);
	}

	it("emits the addressed fingerprints on commit and nothing on rejection", async () => {
		const committed = await evaluate({
			verdict: "pass",
			score: 80,
			failedCriteria: [],
			addressedFingerprints: [recurring.fingerprint.id],
		});
		expect(committed).toHaveLength(1);
		expect(committed[0]!.addressed).toEqual([recurring.fingerprint.id]);
		expect(committed[0]!.proposalId).toBe("refine_1");
		expect(committed[0]!.component).toBe(REFINEMENT_LOG_COMPONENT);

		// An unaddressed recurring failure is charged its opponent weight and rejected.
		expect(
			await evaluate({
				verdict: "pass",
				score: 80,
				failedCriteria: ["evidence", "scope"],
				addressedFingerprints: [],
			}),
		).toEqual([]);
	});

	it("rolls the emitted record up as a treated cohort member", async () => {
		const committed = await evaluate({
			verdict: "pass",
			score: 80,
			failedCriteria: [],
			addressedFingerprints: [recurring.fingerprint.id],
		});
		// Strip the seeded commit so the only one in the corpus is the record the
		// real gate just emitted.
		const log = [
			...seedLog([])
				.trimEnd()
				.split("\n")
				.filter((raw) => !raw.includes(REFINEMENT_COMMITTED_MSG)),
			stringifyLogEntry({ ...committed[0]!, ts: isoAt(COMMIT_TURN) }),
		].join("\n");
		const { days } = rollUpLearningDays([writeLog(`${log}\n`)], AFTER_CORPUS_MS);
		const report = buildLearningReport(days, { nowMs: AFTER_CORPUS_MS });
		expect(report.commits).toBe(1);
		// The fingerprint the real gate named is never observed in this corpus, so
		// it is reported as unscored rather than silently counted as an improvement.
		expect(report.unobservedTreated).toEqual([recurring.fingerprint.id]);
		expect(report.cohorts.treated.n).toBe(0);
		expect(report.cohorts.untreated.n).toBe(FINGERPRINT_COUNT);
		expect(report.pValue).toBeUndefined();
		expect(report.insufficientEvidence).toContain("treated 0");
	});
});
