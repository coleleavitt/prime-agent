import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
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
