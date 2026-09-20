import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { addSpanSink, type SpanEndRecord } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { DEFAULT_POLICY } from "../src/core/dream/policy.js";
import { createLocalProposer, type Proposer } from "../src/core/dream/proposer.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import {
	attemptRngLabel,
	type ExploreResult,
	IMPROVE_EPS,
	improvementsOf,
	LlmProposerUnavailableError,
	runOnlineExploration,
} from "../src/core/dream/rollout.js";
import { listTrees, readTree } from "../src/core/dream/store.js";
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
	proposer?: Proposer<unknown>,
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
		...(proposer ? { proposer } : {}),
	});
}

/**
 * A token-free stand-in for the LLM proposer's OUTCOMES: the same local
 * candidates, but every `accept`-th attempt is stamped `origin: "llm"` (an
 * accepted child result) and the rest `origin: "local"` (a fallback after a
 * rejected one), each carrying the tokens a child would have spent. No handler,
 * no network; the rng stream is untouched, so the tree shape equals the local one.
 */
function stampedProposer(task: DreamTaskId, n: number | undefined, accept: number): Proposer<unknown> {
	const local = createLocalProposer(resolveTask({ task, n }));
	let attempt = 0;
	return {
		propose(parent, params, rng, round) {
			attempt += 1;
			const outcome = local.propose(parent, params, rng, round);
			return attempt % accept === 0
				? { ...outcome, tokens: 270, origin: "llm" }
				: { ...outcome, tokens: 600, origin: "local" };
		},
	};
}

function nodeLines(dir: string, treeId: string): Record<string, unknown>[] {
	return readFileSync(join(dir, "trees", `${treeId}.jsonl`), "utf8")
		.trim()
		.split("\n")
		.map((line) => JSON.parse(line) as Record<string, unknown>)
		.filter((line) => line.type === "node");
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
		// Nothing on the local path is agent-generated.
		expect(result.agentGeneratedCount).toBe(0);
		expect(result.tree.originCounts()).toEqual({ root: 1, local: result.revealedCount, llm: 0 });
	});

	it("reports the probe at which the best arrived and the best-so-far curve at its improvements", () => {
		const result = explore(dreamDir, 5);
		const best = result.tree.bestNode()!;
		expect(result.probesToBest).toBe(best.seq);
		expect(result.probesToBest).toBeGreaterThan(0);
		expect(result.probesToBest).toBeLessThanOrEqual(result.revealedCount);
		const curve = result.improvements;
		expect(curve.length).toBeGreaterThan(1);
		expect(curve[0]).toEqual({ probe: 0, score: result.rootScore });
		expect(curve.at(-1)).toEqual({ probe: best.seq, score: result.bestScore });
		for (let index = 1; index < curve.length; index++) {
			expect(curve[index]!.probe).toBeGreaterThan(curve[index - 1]!.probe);
			expect(curve[index]!.score).toBeGreaterThan(curve[index - 1]!.score);
		}
		// Every improvement is a real node reached in reveal order, and nothing in between beat it.
		const nodes = result.tree.allNodes();
		for (const point of curve) expect(nodes[point.probe]!.score).toBe(point.score);
		expect(improvementsOf(nodes)).toEqual(curve);
		// A curve over an all-invalid list is empty; the root counts as probe 0 when it is valid.
		expect(improvementsOf([{ seq: 0, score: 1, valid: false }])).toEqual([]);
		expect(
			improvementsOf([
				{ seq: 0, score: 0.2, valid: true },
				{ seq: 1, score: 0.1, valid: true },
				{ seq: 2, score: 0.3, valid: true },
			]),
		).toEqual([
			{ probe: 0, score: 0.2 },
			{ probe: 2, score: 0.3 },
		]);
		expect(IMPROVE_EPS).toBe(1e-12);
	});

	it("persists origin on every node line: root, then local for the whole local path", () => {
		const result = explore(dreamDir, 5);
		const lines = nodeLines(dreamDir, result.treeId);
		expect(lines[0]!.origin).toBe("root");
		expect(lines.slice(1).every((line) => line.origin === "local")).toBe(true);
		const summary = listTrees(dreamDir).find((tree) => tree.treeId === result.treeId)!;
		expect(summary.nodeCount).toBe(result.tree.size);
		expect(summary.agentGeneratedCount).toBe(0);
	});

	it("records an injected outcome's origin on the node, its line and the dream.attempt span, and counts the agent's candidates", () => {
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let result: ExploreResult;
		try {
			result = explore(dreamDir, 5, "circle-packing", 26, FIXED_CLOCK, stampedProposer("circle-packing", 26, 3));
		} finally {
			unsubscribe();
		}
		const localDir = mkdtempSync(join(tmpdir(), "dream-origin-local-"));
		try {
			const local = explore(localDir, 5);
			// Provenance never reaches the rng: the stamped rollout grows the local tree's shape.
			expect(shape(result)).toEqual(shape(local));
			expect(result.treeId).toBe(local.treeId);
		} finally {
			rmSync(localDir, { recursive: true, force: true });
		}

		const nonRoot = result.tree.allNodes().filter((node) => node.parentId !== null);
		const llmNodes = nonRoot.filter((node) => node.origin === "llm");
		expect(llmNodes.length).toBe(Math.floor(nonRoot.length / 3));
		expect(result.agentGeneratedCount).toBe(llmNodes.length);
		expect(result.tree.originCounts()).toEqual({
			root: 1,
			local: nonRoot.length - llmNodes.length,
			llm: llmNodes.length,
		});
		// Every attempt cost child tokens whether or not the agent's output was accepted.
		expect(result.tokens).toBe(llmNodes.length * 270 + (nonRoot.length - llmNodes.length) * 600);
		expect(nonRoot.every((node) => node.tokens === (node.origin === "llm" ? 270 : 600))).toBe(true);

		const lines = nodeLines(dreamDir, result.treeId);
		expect(lines.map((line) => line.origin)).toEqual(result.tree.allNodes().map((node) => node.origin));
		expect(listTrees(dreamDir).find((tree) => tree.treeId === result.treeId)!.agentGeneratedCount).toBe(
			llmNodes.length,
		);
		const recorded = readTree(result.treeId, dreamDir);
		expect(recorded.nodes.map((node) => node.origin)).toEqual(result.tree.allNodes().map((node) => node.origin));

		const attempts = spans.filter((span) => span.name === "dream.attempt");
		expect(attempts).toHaveLength(nonRoot.length);
		const byNode = new Map(attempts.map((span) => [span.attrs["dream.node_id"], span.attrs["dream.origin"]]));
		for (const node of nonRoot) expect(byNode.get(node.id)).toBe(node.origin);
		expect(attempts.every((span) => typeof span.attrs["dream.origin"] === "string")).toBe(true);
	});

	it("reads a tree written before provenance as a root plus local nodes", () => {
		const result = explore(dreamDir, 5);
		const path = join(dreamDir, "trees", `${result.treeId}.jsonl`);
		const legacy = readFileSync(path, "utf8")
			.split("\n")
			.map((line) => {
				if (line.length === 0) return line;
				const parsed = JSON.parse(line) as Record<string, unknown>;
				delete parsed.origin;
				return JSON.stringify(parsed);
			})
			.join("\n");
		expect(legacy).not.toContain('"origin"');
		writeFileSync(path, legacy);
		const recorded = readTree(result.treeId, dreamDir);
		expect(recorded.nodes[0]!.origin).toBe("root");
		expect(recorded.nodes.slice(1).every((node) => node.origin === "local")).toBe(true);
		expect(listTrees(dreamDir).find((tree) => tree.treeId === result.treeId)!.agentGeneratedCount).toBe(0);
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
