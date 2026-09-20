import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { formatCommandHelp, getCommandSpec } from "../src/cli/command-registry.js";
import {
	DREAM_USAGE,
	type DreamCommandIo,
	DreamCommandUsageError,
	parseDreamCommandArgs,
	runDreamCommand,
} from "../src/cli/dream-command.js";
import { DREAM_TASK_IDS } from "../src/core/dream/tasks/index.js";

/**
 * The `dream` CLI must run the whole Dream-RSI loop against a scratch store with
 * zero model tokens and no network, be deterministic under an injected seed and
 * clock, and never touch the live ~/.prime/agent. LLM flags are rejected without
 * spending a token or opening a socket.
 */

const FIXED_NOW = Date.parse("2026-09-18T00:00:00.000Z");
const LLM_REJECTION =
	"LLM proposer/dreamer run only in-session, where an agent handler exists: start them with /dream --llm-proposer or /dream --llm-dreamer. The standalone CLI has no handler, so it runs the default local proposer at zero tokens.";
const GUIDED_REJECTION =
	"dream-guided/fixed-guided need the in-session LLM proposer; run dream.experiment(...) from the kernel skill or /dream experiment --llm-proposer";
const EXPERIMENT_ARGS = [
	"--task",
	"sum-difference",
	"--rounds",
	"3",
	"--workers",
	"2",
	"--k1",
	"3",
	"--k2",
	"6",
	"--dreams",
	"2",
];

const scratchDirs: string[] = [];
let savedDreamDir: string | undefined;
let savedAgentDir: string | undefined;

function scratch(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-cli-"));
	scratchDirs.push(dir);
	return dir;
}

function makeIo(nowMs: number = FIXED_NOW): { io: DreamCommandIo; out: string[]; err: string[] } {
	const out: string[] = [];
	const err: string[] = [];
	return {
		io: { stdout: (line) => out.push(line), stderr: (line) => err.push(line), now: () => nowMs },
		out,
		err,
	};
}

function treeFiles(dir: string): Record<string, string> {
	const treesDir = join(dir, "trees");
	const files: Record<string, string> = {};
	for (const entry of readdirSync(treesDir)) {
		if (entry.endsWith(".jsonl")) {
			files[entry] = readFileSync(join(treesDir, entry), "utf-8");
		}
	}
	return files;
}

beforeEach(() => {
	savedDreamDir = process.env.PRIME_AGENT_DREAM_DIR;
	savedAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	// Fresh scratch dirs so nothing here can read or write the live agent state.
	process.env.PRIME_AGENT_DREAM_DIR = scratch();
	process.env.PRIME_AGENT_CODING_AGENT_DIR = scratch();
});

afterEach(() => {
	if (savedDreamDir === undefined) delete process.env.PRIME_AGENT_DREAM_DIR;
	else process.env.PRIME_AGENT_DREAM_DIR = savedDreamDir;
	if (savedAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = savedAgentDir;
	for (const dir of scratchDirs.splice(0)) {
		rmSync(dir, { recursive: true, force: true });
	}
});

describe("parseDreamCommandArgs", () => {
	it("defaults to the loop subcommand and circle-packing", () => {
		const options = parseDreamCommandArgs([]);
		expect(options.subcommand).toBe("loop");
		expect(options.task).toBe("circle-packing");
		expect(options.seed).toBe(1);
		expect(options.iterations).toBe(3);
		expect(options.workers).toBe(4);
	});

	it("resolves subcommand aliases", () => {
		expect(parseDreamCommandArgs(["propose"]).subcommand).toBe("rollout");
		expect(parseDreamCommandArgs(["simulate"]).subcommand).toBe("replay");
		expect(parseDreamCommandArgs(["inspect"]).subcommand).toBe("show");
		expect(parseDreamCommandArgs(["rollout"]).subcommand).toBe("rollout");
		expect(parseDreamCommandArgs(["replay"]).subcommand).toBe("replay");
		expect(parseDreamCommandArgs(["show"]).subcommand).toBe("show");
	});

	it("accepts --opt value and --opt=value forms", () => {
		expect(parseDreamCommandArgs(["--seed", "7"]).seed).toBe(7);
		expect(parseDreamCommandArgs(["--seed=9"]).seed).toBe(9);
		expect(parseDreamCommandArgs(["rollout", "--task=sum-difference"]).task).toBe("sum-difference");
		expect(parseDreamCommandArgs(["--n", "32"]).n).toBe(32);
	});

	it("parses the experiment subcommand, its alias, --rounds, --arms and --seeds", () => {
		const options = parseDreamCommandArgs(["experiment", "--rounds", "3", "--arms", "dream,fixed"]);
		expect(options.subcommand).toBe("experiment");
		expect(options.rounds).toBe(3);
		expect(options.arms).toEqual(["dream", "fixed"]);
		expect(options.seeds).toBeUndefined();
		expect(options.overwrite).toBe(false);
		const defaults = parseDreamCommandArgs(["experiment"]);
		expect(defaults.rounds).toBe(4);
		expect(defaults.arms).toEqual(["dream", "fixed"]);
		expect(parseDreamCommandArgs(["compare"]).subcommand).toBe("experiment");
		expect(parseDreamCommandArgs(["experiment", "--arms=fixed"]).arms).toEqual(["fixed"]);
		expect(parseDreamCommandArgs(["experiment", "--seeds", "1,2,3"]).seeds).toEqual([1, 2, 3]);
		expect(parseDreamCommandArgs(["experiment", "--overwrite"]).overwrite).toBe(true);
		// The guided arms parse (the runner rejects them later, before anything runs).
		expect(parseDreamCommandArgs(["experiment", "--arms", "dream,dream-guided"]).arms).toEqual([
			"dream",
			"dream-guided",
		]);
	});

	it("parses --beta1/--beta2/--beta3 as finite non-negative numbers and defaults them to the objective defaults", () => {
		// DEFAULT_OBJECTIVE: beta2 > beta1 (one saved round outweighs the <= W probes it can cost), beta3 = 0.25.
		expect(parseDreamCommandArgs([]).objective).toEqual({ beta1: 0.05, beta2: 0.1, beta3: 0.25 });
		expect(parseDreamCommandArgs(["--beta1", "0.2", "--beta2=0"]).objective).toEqual({
			beta1: 0.2,
			beta2: 0,
			beta3: 0.25,
		});
		expect(parseDreamCommandArgs(["--beta3", "0"]).objective.beta3).toBe(0);
		expect(parseDreamCommandArgs(["--beta3=1"]).objective.beta3).toBe(1);
		expect(() => parseDreamCommandArgs(["--beta3", "1.5"])).toThrow(/\[0, 1\]/);
		expect(parseDreamCommandArgs(["experiment", "--beta1", "1e-3"]).objective.beta1).toBe(0.001);
		expect(() => parseDreamCommandArgs(["--beta1", "-0.1"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta2", "nan"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta1", "Infinity"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta1", ""])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta1"])).toThrow(DreamCommandUsageError);
	});

	it("parses --priming as none (the default) or diverse", () => {
		expect(parseDreamCommandArgs([]).priming).toBe("none");
		expect(parseDreamCommandArgs(["--priming", "none"]).priming).toBe("none");
		expect(parseDreamCommandArgs(["experiment", "--priming=diverse"]).priming).toBe("diverse");
		expect(() => parseDreamCommandArgs(["--priming", "lots"])).toThrow(/none or diverse/);
		expect(() => parseDreamCommandArgs(["--priming"])).toThrow(DreamCommandUsageError);
	});

	it("rejects malformed experiment options", () => {
		expect(() => parseDreamCommandArgs(["experiment", "--arms", "dream,nope"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["experiment", "--arms", "dream,dream"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["experiment", "--arms", ""])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["experiment", "--rounds", "0"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["experiment", "--rounds", "2.5"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["experiment", "--iterations", "2"])).toThrow(/takes --rounds/);
		expect(() => parseDreamCommandArgs(["experiment", "--seeds", "1,1"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["experiment", "--seeds", "1,-2"])).toThrow(DreamCommandUsageError);
		// --iterations stays legal for loop.
		expect(parseDreamCommandArgs(["loop", "--iterations", "2"]).iterations).toBe(2);
	});

	it("threads --n to every task that takes a size and rejects a size the task does not accept", () => {
		// The task decides the accepted sizes: circle-packing takes any integer >= 2 (as /dream
		// does), autocorrelation only its bin counts; the parser turns the task's RangeError into
		// a usage error so a bad size never reaches a run.
		expect(parseDreamCommandArgs(["--n", "10"]).n).toBe(10);
		expect(parseDreamCommandArgs(["experiment", "--task", "autocorrelation", "--n", "32"]).n).toBe(32);
		expect(parseDreamCommandArgs(["--n", "128", "--task", "autocorrelation"]).n).toBe(128);
		expect(() => parseDreamCommandArgs(["--task", "autocorrelation", "--n", "10"])).toThrow(/--n: autocorrelation/);
		expect(() => parseDreamCommandArgs(["--task", "autocorrelation", "--n", "10"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--n", "1"])).toThrow(/--n: circle-packing/);
		expect(() => parseDreamCommandArgs(["--n", "0"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--n", "2.5"])).toThrow(DreamCommandUsageError);
	});

	it("rejects an unknown flag, a bad subcommand, and a non-integer count", () => {
		expect(() => parseDreamCommandArgs(["--nope"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["frobnicate"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--task", "banana"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--iterations", "1.5"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--workers", "0"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["loop", "rollout"])).toThrow(DreamCommandUsageError);
	});
});

describe("dream usage and help", () => {
	const spec = getCommandSpec(["dream"])!;
	const usageFlags = [...new Set(DREAM_USAGE.match(/--[a-z0-9-]+/g) ?? [])];
	const optionRows = (spec.options ?? []).filter((row) => row.startsWith("--"));
	const rowFlags = optionRows.map((row) => row.match(/^--[a-z0-9-]+/)![0]);

	it("is one string shared by the parser's usage error and help dream, naming every registered task", () => {
		expect(spec.usage).toBe(DREAM_USAGE);
		expect(DREAM_USAGE).toContain(`--task <${DREAM_TASK_IDS.join("|")}>`);
		expect(usageFlags.length).toBeGreaterThan(10);
		const help = formatCommandHelp(["dream"])!;
		expect(help).toContain(DREAM_USAGE);
		for (const flag of usageFlags) expect(help).toContain(`  ${flag}`);
	});

	it("has an Options row for every flag in the usage, and no row for a flag the usage lacks", () => {
		expect([...rowFlags].sort()).toEqual([...usageFlags].sort());
		expect(rowFlags).toContain("--beta1");
		expect(rowFlags).toContain("--beta2");
		expect(rowFlags).toContain("--beta3");
		expect(rowFlags).toContain("--priming");
		expect(rowFlags).toContain("--seeds");
		expect(rowFlags).toContain("--overwrite");
		for (const taskId of DREAM_TASK_IDS) {
			expect(optionRows.find((row) => row.startsWith("--task "))).toContain(taskId);
		}
	});

	it("names only flags the parser accepts", () => {
		for (const flag of usageFlags) {
			const takesValue = DREAM_USAGE.includes(`[${flag} <`);
			try {
				parseDreamCommandArgs(takesValue ? [flag, "1"] : [flag]);
			} catch (error) {
				// A value the parser dislikes is fine here; an unknown flag is not.
				expect(error).toBeInstanceOf(DreamCommandUsageError);
				expect((error as Error).message).not.toMatch(/Unknown option/);
			}
		}
	});
});

describe("runDreamCommand loop", () => {
	it("runs the whole loop at zero tokens and prints a per-round summary", () => {
		const dir = scratch();
		const { io, out } = makeIo();
		const code = runDreamCommand(["loop", "--seed", "7", "--iterations", "3", "--dir", dir], io);
		expect(code).toBe(0);
		const text = out.join("\n");
		expect(text).toContain("dream loop");
		expect(text).toContain("round 0:");
		expect(text).toContain("round 1:");
		expect(text).toContain("initial policy");
		expect(text).toContain("final");
		// A tree was persisted per rollout.
		expect(Object.keys(treeFiles(dir)).length).toBeGreaterThanOrEqual(2);
	});

	it("emits parseable JSON with tokens 0 and a no-worse final policy", () => {
		const dir = scratch();
		const { io, out } = makeIo();
		const code = runDreamCommand(["--seed", "7", "--iterations", "3", "--dir", dir, "--json"], io);
		expect(code).toBe(0);
		const result = JSON.parse(out.join("\n"));
		expect(result.tokens).toBe(0);
		expect(result.mode).toBe("local");
		expect(Array.isArray(result.treeIds)).toBe(true);
		expect(result.treeIds.length).toBeGreaterThanOrEqual(2);
		expect(Array.isArray(result.rounds)).toBe(true);
		expect(result.rounds.length).toBe(result.treeIds.length);
		for (const round of result.rounds) {
			expect(Number.isFinite(round.bestScore)).toBe(true);
			expect(round.probes).toBeGreaterThanOrEqual(0);
		}
		// The no-worse selection guarantee: the deployed policy never regresses on replay.
		expect(result.finalPolicyScore).toBeGreaterThanOrEqual(result.initialPolicyScore);
		expect(Number.isFinite(result.bestNodeScore)).toBe(true);
	});

	it("is deterministic: same seed and clock produce identical output and byte-identical trees", () => {
		const dirA = scratch();
		const dirB = scratch();
		const a = makeIo();
		const b = makeIo();
		const codeA = runDreamCommand(["loop", "--seed", "5", "--iterations", "2", "--dir", dirA, "--json"], a.io);
		const codeB = runDreamCommand(["loop", "--seed", "5", "--iterations", "2", "--dir", dirB, "--json"], b.io);
		expect(codeA).toBe(0);
		expect(codeB).toBe(0);
		expect(a.out).toEqual(b.out.map((line) => line.replace(dirB, dirA)));
		const filesA = treeFiles(dirA);
		const filesB = treeFiles(dirB);
		expect(Object.keys(filesA).sort()).toEqual(Object.keys(filesB).sort());
		for (const [name, content] of Object.entries(filesA)) {
			expect(filesB[name]).toBe(content);
		}
	});
});

describe("runDreamCommand experiment", () => {
	it("runs the dream and fixed arms into per-arm stores, prints the tables and the headline, and never touches the pool", () => {
		const dir = scratch();
		const { io, out, err } = makeIo();
		const code = runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", dir], io);
		expect(code).toBe(0);
		expect(err).toHaveLength(0);
		const text = out.join("\n");
		expect(text).toContain("dream experiment  sum-difference-s1-n3-");
		expect(text).toContain("task sum-difference  scoring deterministic  seed 1");
		expect(text).toContain("arm dream ");
		expect(text).toContain("arm fixed ");
		expect(text).toContain("round | best | cum best | probes | agent | fallback | cum probes | policy");
		expect(text).toContain("headline vs fixed");
		expect(text).toContain("delta best");
		// Provenance is counts only: on the local path every probe is local and no LLM proposal exists.
		const provenance = out.filter((line) => line.includes("provenance:"));
		expect(provenance).toHaveLength(2);
		for (const line of provenance) {
			const match = line.match(
				/^ {4}provenance: (\d+) probes = 0 agent-generated \+ (\d+) local \(0 fallbacks\); local proposer, 0 LLM proposals$/,
			);
			expect(match, line).not.toBeNull();
			expect(match![1]).toBe(match![2]);
		}
		// The dream arm dreamed on rounds 2 and 3; the fixed arm never dreams and prints no dreaming summary.
		const dreamingLines = out.filter((line) => /^ {4}dreaming: /.test(line));
		expect(dreamingLines).toHaveLength(1);
		expect(dreamingLines[0]).toMatch(/^ {4}dreaming: 2 phases {2}improved [0-2]\/2 {2}policy changes \d+/);
		const changes = Number(dreamingLines[0]!.match(/policy changes (\d+)/)![1]);
		expect(dreamingLines[0]!.includes("INERT")).toBe(changes === 0);
		// The final-policy line names the last row's policy, so it can never contradict the table above it.
		const finalLines = out.filter((line) => line.includes("final policy"));
		expect(finalLines).toHaveLength(2);
		for (const line of finalLines) {
			const match = line.match(/final policy (\S+) {2}changes (\d+) {2}selected policy (\S+) {2}own-pool score/);
			expect(match, line).not.toBeNull();
			const rows = out.slice(0, out.indexOf(line)).filter((row) => /^ {4}\s*[123] \|/.test(row));
			const lastRowPolicy = rows.at(-1)!.split(" | ")[7]!.split("  ")[0];
			expect(match![1]).toBe(lastRowPolicy);
		}
		expect(text).toMatch(/results .*\/experiments\/sum-difference-s1-n3-\d+\/result\.json/);
		// One row per arm-round.
		expect(out.filter((line) => /^ {4}\s*[123] \|/.test(line))).toHaveLength(6);
		expect(existsSync(join(dir, "trees"))).toBe(false);
		const experiments = readdirSync(join(dir, "experiments"));
		expect(experiments).toHaveLength(1);
		expect(existsSync(join(dir, "experiments", experiments[0]!, "result.json"))).toBe(true);
		expect(readdirSync(join(dir, "experiments", experiments[0]!)).sort()).toEqual(["dream", "fixed", "result.json"]);
		expect(
			readdirSync(join(dir, "experiments", experiments[0]!, "dream", "trees")).filter((n) => n.endsWith(".jsonl")),
		).toHaveLength(3);
	});

	it("emits the versioned result as JSON, deterministically across two stores", () => {
		const dirA = scratch();
		const dirB = scratch();
		const a = makeIo();
		const b = makeIo();
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", dirA, "--json"], a.io)).toBe(0);
		expect(runDreamCommand(["compare", ...EXPERIMENT_ARGS, "--dir", dirB, "--json"], b.io)).toBe(0);
		expect(a.out).toEqual(b.out);
		const result = JSON.parse(a.out.join("\n"));
		expect(result.schema).toBe("prime-agent.dream.experiment/1");
		expect(result.arms.map((arm: { arm: string }) => arm.arm)).toEqual(["dream", "fixed"]);
		expect(result.headline).not.toBeNull();
		expect(result.headline.reference).toBe("fixed");
		expect(result.rounds).toBe(3);
		expect(
			result.arms.every((arm: { rounds: unknown[]; totals: { tokens: number } }) => arm.rounds.length === 3),
		).toBe(true);
		expect(result.arms.every((arm: { totals: { tokens: number } }) => arm.totals.tokens === 0)).toBe(true);
		expect(result.scoring).toBe("deterministic");
		const fixed = result.arms.find((arm: { arm: string }) => arm.arm === "fixed");
		expect(fixed.finalPolicyId).toBe(fixed.initialPolicyId);
		expect(fixed.selectedPolicyId).toBe(fixed.initialPolicyId);
		expect(fixed.rounds.every((row: { dreaming: unknown }) => row.dreaming === null)).toBe(true);
		expect(fixed.storeDir).toBe(`experiments/${result.experimentId}/fixed`);
		for (const arm of result.arms as { finalPolicyId: string; rounds: { policyId: string }[] }[]) {
			expect(arm.finalPolicyId).toBe(arm.rounds.at(-1)!.policyId);
		}
		expect(existsSync(join(dirA, "experiments", result.experimentId, "result.json"))).toBe(true);
	});

	it("builds an autocorrelation run with --n and records the size on the result, the trees and the header line", () => {
		const dir = scratch();
		const args = [
			"experiment",
			"--task",
			"autocorrelation",
			"--rounds",
			"1",
			"--workers",
			"2",
			"--k1",
			"2",
			"--k2",
			"4",
		];
		const sized = makeIo();
		expect(runDreamCommand([...args, "--n", "32", "--dreams", "1", "--arms", "fixed", "--dir", dir], sized.io)).toBe(
			0,
		);
		expect(sized.out[1]).toContain("task autocorrelation n 32 ");
		const resultPath = join(dir, "experiments", readdirSync(join(dir, "experiments"))[0]!, "result.json");
		const result = JSON.parse(readFileSync(resultPath, "utf-8")) as { n?: number; experimentId: string };
		expect(result.n).toBe(32);
		const trees = treeFiles(join(dir, "experiments", result.experimentId, "fixed"));
		for (const text of Object.values(trees)) {
			expect((JSON.parse(text.split("\n")[0]!) as { n?: number }).n).toBe(32);
		}
		// Without --n the record still names the size the run was built with (the task default).
		const defaulted = makeIo();
		const dirB = scratch();
		expect(
			runDreamCommand([...args, "--dreams", "1", "--arms", "fixed", "--dir", dirB, "--json"], defaulted.io),
		).toBe(0);
		expect((JSON.parse(defaulted.out.join("\n")) as { n?: number }).n).toBe(64);
	});

	it("records --beta1/--beta2 on the result and scores the dreaming step with them", () => {
		const dir = scratch();
		const { io, out } = makeIo();
		const args = ["experiment", ...EXPERIMENT_ARGS, "--beta1", "0.2", "--beta2", "0.1", "--dir", dir, "--json"];
		expect(runDreamCommand(args, io)).toBe(0);
		const result = JSON.parse(out.join("\n"));
		expect(result.objective).toEqual({ beta1: 0.2, beta2: 0.1, beta3: 0.25 });
		const persisted = JSON.parse(readFileSync(join(dir, "experiments", result.experimentId, "result.json"), "utf8"));
		expect(persisted.objective).toEqual({ beta1: 0.2, beta2: 0.1, beta3: 0.25 });
		expect(persisted.notes).toContain("objective: normalized (q in pool range, cost in budget fractions)");
		// The printed header names the betas too.
		const printed = makeIo();
		expect(
			runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--beta1", "0.2", "--dir", scratch()], printed.io),
		).toBe(0);
		expect(printed.out.join("\n")).toContain("beta1 0.2  beta2 0.1  beta3 0.25");
		// A different objective changes the dreaming step's scores but never the shared round 1.
		const defaults = makeIo();
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", scratch(), "--json"], defaults.io)).toBe(0);
		const base = JSON.parse(defaults.out.join("\n"));
		expect(base.objective).toEqual({ beta1: 0.05, beta2: 0.1, beta3: 0.25 });
		expect(result.arms[0].rounds[0].roundBest).toBe(base.arms[0].rounds[0].roundBest);
		const dream = (r: { arms: { arm: string; rounds: { dreaming: { currentScore: number } | null }[] }[] }) =>
			r.arms.find((arm) => arm.arm === "dream")!.rounds[1]!.dreaming!.currentScore;
		expect(dream(result)).not.toBe(dream(base));
	});

	it("prints identical round tables for one seed under two clocks, with only the ids differing", () => {
		const dirA = scratch();
		const dirB = scratch();
		const a = makeIo(FIXED_NOW);
		const b = makeIo(FIXED_NOW + 12_345);
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--seed", "7", "--dir", dirA], a.io)).toBe(0);
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--seed", "7", "--dir", dirB], b.io)).toBe(0);
		expect(a.out).not.toEqual(b.out);
		const rows = (lines: string[]) => lines.filter((line) => /^ {4}\s*\d+ \|/.test(line));
		expect(rows(a.out)).toHaveLength(6);
		expect(rows(b.out)).toEqual(rows(a.out));
		const finals = (lines: string[]) => lines.filter((line) => line.includes("final policy"));
		expect(finals(b.out)).toEqual(finals(a.out));
		const headline = (lines: string[]) =>
			lines.filter((line) => line.includes("headline") || line.includes("delta best"));
		expect(headline(b.out)).toEqual(headline(a.out));
		// What differs is exactly the clock-bearing identity: experiment id, run ids and store paths.
		const differing = a.out.filter((line, index) => line !== b.out[index]);
		expect(differing.length).toBeGreaterThan(0);
		expect(
			differing.every(
				(line) =>
					line.startsWith("dream experiment") ||
					line.includes("  run ") ||
					line.startsWith("  store ") ||
					line.startsWith("  results "),
			),
		).toBe(true);
	});

	it("runs one experiment per seed with --seeds and prints an array", () => {
		const dir = scratch();
		const { io, out } = makeIo();
		const code = runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--seeds", "1,2", "--dir", dir, "--json"], io);
		expect(code).toBe(0);
		const results = JSON.parse(out.join("\n"));
		expect(Array.isArray(results)).toBe(true);
		expect(results.map((result: { seed: number }) => result.seed)).toEqual([1, 2]);
		expect(readdirSync(join(dir, "experiments")).sort()).toEqual(
			results.map((result: { experimentId: string }) => result.experimentId).sort(),
		);
		const status = makeIo();
		expect(runDreamCommand(["status", "--dir", dir], status.io)).toBe(0);
		expect(status.out.join("\n")).toContain("experiments 2");
	});

	it("--priming none is byte-identical to no flag", () => {
		const plain = makeIo();
		const none = makeIo();
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", scratch(), "--json"], plain.io)).toBe(0);
		expect(
			runDreamCommand(
				["experiment", ...EXPERIMENT_ARGS, "--priming", "none", "--dir", scratch(), "--json"],
				none.io,
			),
		).toBe(0);
		expect(none.out).toEqual(plain.out);
	});

	it("refuses to overwrite an existing experiment unless asked", () => {
		const dir = scratch();
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", dir], makeIo().io)).toBe(0);
		const again = makeIo();
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", dir], again.io)).toBe(2);
		expect(again.err.join("\n")).toContain("already exists");
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", dir, "--overwrite"], makeIo().io)).toBe(0);
	});

	it("rejects guided arms and the LLM flags with exit 2 before writing anything", () => {
		const dir = scratch();
		const guided = makeIo();
		expect(runDreamCommand(["experiment", "--arms", "dream,dream-guided", "--dir", dir], guided.io)).toBe(2);
		expect(guided.out).toHaveLength(0);
		expect(guided.err.join("\n")).toContain(GUIDED_REJECTION);
		expect(existsSync(join(dir, "experiments"))).toBe(false);

		const llm = makeIo();
		expect(runDreamCommand(["experiment", "--llm-proposer", "--dir", dir], llm.io)).toBe(2);
		expect(llm.err.join("\n")).toContain(LLM_REJECTION);
		expect(existsSync(join(dir, "experiments"))).toBe(false);
	});
});

describe("runDreamCommand read subcommands and rejections", () => {
	it("returns exit 1 with usage on a bad flag", () => {
		const { io, out, err } = makeIo();
		const code = runDreamCommand(["--bogus"], io);
		expect(code).toBe(1);
		expect(out).toHaveLength(0);
		expect(err.join("\n")).toContain("Usage:");
	});

	it("returns exit 2 on status/show/replay/improve against an empty store", () => {
		for (const sub of ["status", "show", "replay", "improve"]) {
			const dir = scratch();
			const { io, err } = makeIo();
			const code = runDreamCommand([sub, "--dir", dir], io);
			expect(code).toBe(2);
			expect(err.join("\n").length).toBeGreaterThan(0);
		}
	});

	it("rejects --llm-proposer and --llm-dreamer with exit 2 and writes nothing", () => {
		for (const flag of ["--llm-proposer", "--llm-dreamer"]) {
			const dir = scratch();
			const { io, out, err } = makeIo();
			const code = runDreamCommand(["loop", "--dir", dir, flag], io);
			expect(code).toBe(2);
			expect(out).toHaveLength(0);
			expect(err.join("\n")).toContain(LLM_REJECTION);
			// No rollout ran: nothing was written to the store.
			let wrote = false;
			try {
				wrote = readdirSync(join(dir, "trees")).length > 0;
			} catch {
				wrote = false;
			}
			expect(wrote).toBe(false);
		}
	});

	it("rolls out, then replays and shows the recorded tree", () => {
		const dir = scratch();
		const rollout = makeIo();
		expect(runDreamCommand(["rollout", "--seed", "3", "--dir", dir], rollout.io)).toBe(0);

		const replay = makeIo();
		expect(runDreamCommand(["simulate", "--tree", "latest", "--dir", dir, "--json"], replay.io)).toBe(0);
		const replayResult = JSON.parse(replay.out.join("\n"));
		expect(Number.isFinite(replayResult.v)).toBe(true);
		expect(replayResult.N).toBeGreaterThanOrEqual(0);

		const replayText = makeIo();
		expect(runDreamCommand(["replay", "--tree", "latest", "--dir", dir], replayText.io)).toBe(0);
		expect(replayText.out.join("\n")).toMatch(/out-of-support \d+ {2}in-support \d\.\d{6} {2}probes to best \d+/);
		expect(replayText.out.join("\n")).toContain("beta3 0.25");

		const show = makeIo();
		expect(runDreamCommand(["inspect", "--tree", "latest", "--dir", dir], show.io)).toBe(0);
		const shown = show.out.join("\n");
		expect(shown).toContain("dream tree");
		// Every node prints its origin; a local rollout has no agent-generated node.
		expect(shown).toMatch(/ {2}agent-generated 0\/\d+/);
		const nodeLines = show.out.filter((line) => /^ {2}\S+ {2}parent /.test(line));
		expect(nodeLines.length).toBeGreaterThan(1);
		expect(
			nodeLines.every(
				(line) => / {2}origin (root|local)$/.test(line) || / {2}origin (root|local) {2}fail /.test(line),
			),
		).toBe(true);
		expect(nodeLines.filter((line) => line.includes("origin root"))).toHaveLength(1);
	});
});
