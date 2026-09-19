import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { runLearningCommand } from "../src/cli/learning-command.js";
import { getLearningIndexDir, getTrajectoryBackfillDir, getTrajectoryIndexPath } from "../src/config.js";
import { type FingerprintDayStats, type LearningDay, writeLearningDay } from "../src/core/learning-index.js";

const NOW_MS = Date.UTC(2025, 2, 10); // 2025-03-10, ISO 2025-W11 (past every fixture week)

let agentDir = "";
let previousAgentDir: string | undefined;

function fp(fingerprint: string, count: number, name = fingerprint): FingerprintDayStats {
	return { fingerprint, name, status: "error", failure: true, count, p50Ms: 0, p95Ms: 0, message: "" };
}

function day(dayStr: string, fingerprints: FingerprintDayStats[]): LearningDay {
	return {
		schema: 1,
		day: dayStr,
		sealedAt: "",
		turns: 5,
		fingerprints,
		commits: [],
		parseErrors: 0,
		sourceFiles: [],
	};
}

beforeEach(() => {
	agentDir = realpathSync(mkdtempSync(join(tmpdir(), "trajectory-cli-")));
	previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
});

afterEach(() => {
	if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	rmSync(agentDir, { recursive: true, force: true });
});

function seedDays(): void {
	const indexDir = getLearningIndexDir();
	const stay = fp("fpStay", 2);
	for (const dayStr of ["2025-01-06", "2025-01-13", "2025-01-20", "2025-01-27"]) {
		writeLearningDay(indexDir, day(dayStr, [stay]));
	}
}

function capture(args: string[]): { code: number; out: string; err: string } {
	const out: string[] = [];
	const err: string[] = [];
	const code = runLearningCommand(args, {
		stdout: (line) => out.push(line),
		stderr: (line) => err.push(line),
		now: () => NOW_MS,
	});
	return { code, out: out.join("\n"), err: err.join("\n") };
}

describe("learning trajectory subcommand", () => {
	it("seals the trajectory store and prints the PERSISTS table", () => {
		seedDays();
		const { code, out } = capture(["trajectory", "--no-seal"]);
		expect(code).toBe(0);
		expect(out).toContain("engineer trajectory");
		expect(out).toContain("fpStay");
		expect(out).toContain("persists");
		expect(out).toContain("task-mix");
		expect(existsSync(getTrajectoryIndexPath())).toBe(true);
	});

	it("emits JSON and exits 2 when everything is withheld below the window floor", () => {
		const indexDir = getLearningIndexDir();
		// Only two observed windows -> below the default min of 4 -> all withheld.
		writeLearningDay(indexDir, day("2025-01-06", [fp("fpA", 2)]));
		writeLearningDay(indexDir, day("2025-01-13", [fp("fpA", 2)]));
		const { code, out } = capture(["trajectory", "--no-seal", "--json"]);
		expect(code).toBe(2);
		const parsed = JSON.parse(out) as { allWithheld: boolean; windowsObserved: number };
		expect(parsed.allWithheld).toBe(true);
		expect(parsed.windowsObserved).toBe(2);
	});

	it("errors when there are no sealed days", () => {
		const { code, err } = capture(["trajectory", "--no-seal"]);
		expect(code).toBe(1);
		expect(err).toContain("no sealed days");
	});

	it("rejects an unknown flag", () => {
		const { code, err } = capture(["trajectory", "--bogus"]);
		expect(code).toBe(1);
		expect(err).toContain("Unknown option for learning trajectory");
	});

	it("folds in confound-flagged backfill days with --include-backfill but never persists them", () => {
		seedDays();
		// Pre-generated backfill day files under learning/backfill/<corpus>/<day>.json.
		const opencodeDir = join(getTrajectoryBackfillDir(), "opencode");
		for (const dayStr of ["2025-01-06", "2025-01-13", "2025-01-20", "2025-01-27"]) {
			writeLearningDay(opencodeDir, day(dayStr, [fp("bf-token", 2, "exc:ValueError")]));
		}
		const { code, out } = capture(["trajectory", "--no-seal", "--include-backfill", "--json"]);
		expect(code).toBe(0);
		const parsed = JSON.parse(out) as {
			rows: { fingerprint: string; corpus: string; confounds: string }[];
		};
		const backfillRow = parsed.rows.find((row) => row.corpus === "backfill:opencode");
		expect(backfillRow).toBeDefined();
		expect(backfillRow?.confounds).toContain("tool-surface");
		expect(backfillRow?.confounds).toContain("measurement-instrument");
		// The persisted store is prime-only: no backfill datum ever reaches disk.
		expect(existsSync(getTrajectoryIndexPath())).toBe(true);
		expect(readFileSync(getTrajectoryIndexPath(), "utf-8")).not.toContain("backfill:");
	});
});
