import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	type DreamCommandIo,
	DreamCommandUsageError,
	parseDreamCommandArgs,
	runDreamCommand,
} from "../src/cli/dream-command.js";

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

	it("parses --beta1/--beta2 as finite non-negative numbers and defaults them to the objective defaults", () => {
		expect(parseDreamCommandArgs([]).objective).toEqual({ beta1: 0.05, beta2: 0.05 });
		expect(parseDreamCommandArgs(["--beta1", "0.2", "--beta2=0"]).objective).toEqual({ beta1: 0.2, beta2: 0 });
		expect(parseDreamCommandArgs(["experiment", "--beta1", "1e-3"]).objective.beta1).toBe(0.001);
		expect(() => parseDreamCommandArgs(["--beta1", "-0.1"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta2", "nan"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta1", "Infinity"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta1", ""])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--beta1"])).toThrow(DreamCommandUsageError);
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

	it("rejects an unknown flag, a bad subcommand, an out-of-range --n, and a non-integer count", () => {
		expect(() => parseDreamCommandArgs(["--nope"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["frobnicate"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--n", "10"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--task", "banana"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--iterations", "1.5"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["--workers", "0"])).toThrow(DreamCommandUsageError);
		expect(() => parseDreamCommandArgs(["loop", "rollout"])).toThrow(DreamCommandUsageError);
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
		expect(text).toContain("arm dream ");
		expect(text).toContain("arm fixed ");
		expect(text).toContain("round | best | cum best | probes | cum probes | policy");
		expect(text).toContain("headline vs fixed");
		expect(text).toContain("delta best");
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
		const fixed = result.arms.find((arm: { arm: string }) => arm.arm === "fixed");
		expect(fixed.finalPolicyId).toBe(fixed.initialPolicyId);
		expect(fixed.rounds.every((row: { dreaming: unknown }) => row.dreaming === null)).toBe(true);
		expect(fixed.storeDir).toBe(`experiments/${result.experimentId}/fixed`);
		expect(existsSync(join(dirA, "experiments", result.experimentId, "result.json"))).toBe(true);
	});

	it("records --beta1/--beta2 on the result and scores the dreaming step with them", () => {
		const dir = scratch();
		const { io, out } = makeIo();
		const args = ["experiment", ...EXPERIMENT_ARGS, "--beta1", "0.2", "--beta2", "0.1", "--dir", dir, "--json"];
		expect(runDreamCommand(args, io)).toBe(0);
		const result = JSON.parse(out.join("\n"));
		expect(result.objective).toEqual({ beta1: 0.2, beta2: 0.1 });
		const persisted = JSON.parse(readFileSync(join(dir, "experiments", result.experimentId, "result.json"), "utf8"));
		expect(persisted.objective).toEqual({ beta1: 0.2, beta2: 0.1 });
		expect(persisted.notes).toContain("objective: normalized (q in pool range, cost in budget fractions)");
		// The printed header names the betas too.
		const printed = makeIo();
		expect(
			runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--beta1", "0.2", "--dir", scratch()], printed.io),
		).toBe(0);
		expect(printed.out.join("\n")).toContain("beta1 0.2  beta2 0.05");
		// A different objective changes the dreaming step's scores but never the shared round 1.
		const defaults = makeIo();
		expect(runDreamCommand(["experiment", ...EXPERIMENT_ARGS, "--dir", scratch(), "--json"], defaults.io)).toBe(0);
		const base = JSON.parse(defaults.out.join("\n"));
		expect(base.objective).toEqual({ beta1: 0.05, beta2: 0.05 });
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

		const show = makeIo();
		expect(runDreamCommand(["inspect", "--tree", "latest", "--dir", dir], show.io)).toBe(0);
		expect(show.out.join("\n")).toContain("dream tree");
	});
});
