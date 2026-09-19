import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { DEFAULT_POLICY } from "../src/core/dream/policy.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import {
	attemptRngLabel,
	type ExploreResult,
	LlmProposerUnavailableError,
	runOnlineExploration,
} from "../src/core/dream/rollout.js";
import type { DreamTaskId } from "../src/core/dream/task.js";
import { resolveTask } from "../src/core/dream/tasks/index.js";
import { createSumDifferenceTask } from "../src/core/dream/tasks/sum-difference.js";

const FIXED_CLOCK = 1_700_000_000_000;

let dreamDir: string;
let priorDreamDir: string | undefined;
let priorAgentDir: string | undefined;

beforeEach(() => {
	dreamDir = mkdtempSync(join(tmpdir(), "dream-rollout-"));
	priorDreamDir = process.env.PRIME_AGENT_DREAM_DIR;
	priorAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	// Guard the live agent state even though `dir` is passed explicitly.
	process.env.PRIME_AGENT_DREAM_DIR = dreamDir;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = mkdtempSync(join(tmpdir(), "dream-agent-"));
});

afterEach(() => {
	if (priorDreamDir === undefined) delete process.env.PRIME_AGENT_DREAM_DIR;
	else process.env.PRIME_AGENT_DREAM_DIR = priorDreamDir;
	if (priorAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = priorAgentDir;
	rmSync(dreamDir, { recursive: true, force: true });
});

function explore(
	dir: string,
	seed: number,
	task: DreamTaskId = "circle-packing",
	n: number | undefined = 26,
	clockMs: number = FIXED_CLOCK,
) {
	return runOnlineExploration({
		task: resolveTask({ task, n }),
		taskId: task,
		n,
		seed,
		rng: createSeededRng(seed),
		clock: () => clockMs,
		workers: 4,
		k1: 12,
		dir,
		policy: DEFAULT_POLICY,
		iteration: 0,
	});
}

/** The clock-free shape of a grown tree: per node (parent seq, branch, round, score, valid), in seq order. */
function shape(result: ExploreResult): [number | null, number, number, number, boolean][] {
	return result.tree
		.allNodes()
		.map((node) => [
			node.parentId === null ? null : result.tree.nodeById(node.parentId)!.seq,
			node.branch,
			node.round,
			node.score,
			node.valid,
		]);
}

describe("runOnlineExploration (circle-packing)", () => {
	it("grows a tree whose best score improves over the root, at zero tokens", () => {
		const result = explore(dreamDir, 5);
		expect(result.tokens).toBe(0);
		expect(result.rounds).toBeGreaterThan(0);
		expect(result.revealedCount).toBeGreaterThan(0);
		expect(result.tree.size).toBe(result.revealedCount + 1);
		expect(result.bestScore).toBeGreaterThan(result.rootScore);
		expect(result.bestNodeId).not.toBe(`${result.treeId}-n0`);
	});

	it("persists a scalar-only JSONL tree with header, node and reveal lines plus blobs", () => {
		const result = explore(dreamDir, 5);
		const treeFile = join(dreamDir, "trees", `${result.treeId}.jsonl`);
		expect(existsSync(treeFile)).toBe(true);
		const lines = readFileSync(treeFile, "utf8")
			.trim()
			.split("\n")
			.map((line) => JSON.parse(line));
		expect(lines[0].type).toBe("tree");
		const nodes = lines.filter((line) => line.type === "node");
		const reveals = lines.filter((line) => line.type === "reveal");
		expect(nodes.length).toBe(result.tree.size);
		expect(reveals.length).toBe(result.rounds);
		for (const node of nodes) {
			expect(Number.isFinite(node.score)).toBe(true);
			for (const [key, value] of Object.entries(node)) {
				const scalar = value === null || ["string", "number", "boolean"].includes(typeof value);
				expect(scalar, `node.${key} must be scalar`).toBe(true);
			}
		}
		const blobDir = join(dreamDir, "trees", result.treeId, "blobs");
		expect(readdirSync(blobDir).length).toBe(result.tree.size);
	});

	it("is byte-for-byte reproducible for the same seed and clock", () => {
		const dirA = mkdtempSync(join(tmpdir(), "dream-repro-a-"));
		const dirB = mkdtempSync(join(tmpdir(), "dream-repro-b-"));
		try {
			const a = explore(dirA, 11);
			const b = explore(dirB, 11);
			expect(b.treeId).toBe(a.treeId);
			expect(b.bestScore).toBe(a.bestScore);
			expect(b.bestNodeId).toBe(a.bestNodeId);
			const fileA = readFileSync(join(dirA, "trees", `${a.treeId}.jsonl`), "utf8");
			const fileB = readFileSync(join(dirB, "trees", `${b.treeId}.jsonl`), "utf8");
			expect(fileB).toBe(fileA);
			const blobA = readFileSync(join(dirA, "trees", a.treeId, "blobs", "0.json"), "utf8");
			const blobB = readFileSync(join(dirB, "trees", b.treeId, "blobs", "0.json"), "utf8");
			expect(blobB).toBe(blobA);
		} finally {
			rmSync(dirA, { recursive: true, force: true });
			rmSync(dirB, { recursive: true, force: true });
		}
	});

	it("grows the same tree for the same seed under different clocks, with only the ids differing", () => {
		const dirA = mkdtempSync(join(tmpdir(), "dream-clock-a-"));
		const dirB = mkdtempSync(join(tmpdir(), "dream-clock-b-"));
		const dirC = mkdtempSync(join(tmpdir(), "dream-clock-c-"));
		try {
			const a = explore(dirA, 7, "circle-packing", 26, 1_789_842_143_996);
			const b = explore(dirB, 7, "circle-packing", 26, 1_789_842_143_997);
			const c = explore(dirC, 7, "circle-packing", 26, 1);
			expect(b.treeId).not.toBe(a.treeId);
			expect(c.treeId).not.toBe(a.treeId);
			expect(b.bestScore).toBe(a.bestScore);
			expect(c.bestScore).toBe(a.bestScore);
			expect(b.revealedCount).toBe(a.revealedCount);
			expect(b.rounds).toBe(a.rounds);
			expect(shape(b)).toEqual(shape(a));
			expect(shape(c)).toEqual(shape(a));
			// The blobs (artifacts) are clock-free too, so they are byte-identical.
			const blobA = readFileSync(join(dirA, "trees", a.treeId, "blobs", `${a.tree.size - 1}.json`), "utf8");
			const blobB = readFileSync(join(dirB, "trees", b.treeId, "blobs", `${b.tree.size - 1}.json`), "utf8");
			expect(blobB).toBe(blobA);
			// A different seed does change the tree.
			const other = explore(mkdtempSync(join(tmpdir(), "dream-clock-d-")), 8, "circle-packing", 26, 1);
			expect(shape(other)).not.toEqual(shape(a));
		} finally {
			rmSync(dirA, { recursive: true, force: true });
			rmSync(dirB, { recursive: true, force: true });
			rmSync(dirC, { recursive: true, force: true });
		}
	});

	it("labels every attempt fork by round, parent seq and child slot, never by an id", () => {
		expect(attemptRngLabel(3, 7, 1)).toBe("r3:p7:b1");
		expect(attemptRngLabel(3, 7, 1)).not.toContain(String(FIXED_CLOCK));
	});

	it("refuses the LLM proposer flag without spending tokens or writing a tree", () => {
		expect(() =>
			runOnlineExploration({
				task: resolveTask({ task: "circle-packing", n: 26 }),
				taskId: "circle-packing",
				n: 26,
				seed: 1,
				rng: createSeededRng(1),
				clock: () => FIXED_CLOCK,
				workers: 4,
				k1: 12,
				dir: dreamDir,
				policy: DEFAULT_POLICY,
				iteration: 0,
				useLlmProposer: true,
			}),
		).toThrow(LlmProposerUnavailableError);
		expect(existsSync(join(dreamDir, "trees"))).toBe(false);
	});
});

describe("runOnlineExploration (sum-difference)", () => {
	it("grows a valid, finite, zero-token tree over the integer-set task", () => {
		const result = explore(dreamDir, 3, "sum-difference", undefined);
		expect(result.tokens).toBe(0);
		expect(result.revealedCount).toBeGreaterThan(0);
		expect(Number.isFinite(result.bestScore)).toBe(true);
		expect(result.bestScore).toBeGreaterThanOrEqual(result.rootScore);
		const best = result.tree.bestNode();
		expect(best?.valid).toBe(true);
	});
});

describe("sum-difference scoring", () => {
	const task = createSumDifferenceTask();

	it("rejects degenerate sets without producing NaN/Infinity", () => {
		const singleton = task.evaluate({ set: [0] });
		expect(singleton.valid).toBe(false);
		expect(singleton.score).toBe(0);
		const empty = task.evaluate({ set: [] });
		expect(empty.valid).toBe(false);
		expect(empty.score).toBe(0);
	});

	it("scores a well-defined set with a finite Gamma", () => {
		const good = task.evaluate({ set: [0, 1, 2, 4, 9, 15] });
		expect(good.valid).toBe(true);
		expect(Number.isFinite(good.score)).toBe(true);
		expect(good.score).toBeGreaterThan(0);
		// An arithmetic progression is well defined: |A+A| = |A-A| = 2|A|-1, so Gamma = 1.
		const progression = task.evaluate({ set: [0, 1, 2, 3, 4] });
		expect(progression.valid).toBe(true);
		expect(progression.score).toBeCloseTo(1, 12);
	});
});
