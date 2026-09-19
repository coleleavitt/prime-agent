import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { addSpanSink, type SpanEndRecord } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import {
	type DreamLoopOptions,
	type DreamLoopResult,
	type DreamRoundRecord,
	runDreamLoop,
} from "../src/core/dream/loop.js";
import { DEFAULT_POLICY, type ExplorationPolicy, policyId } from "../src/core/dream/policy.js";
import { listTrees } from "../src/core/dream/store.js";
import { resolveTask } from "../src/core/dream/tasks/index.js";

/**
 * `runDreamLoop` with the fixed-exploration control (`fixedPolicy`), an explicit
 * `initialPolicy`, and the per-round records the experiment runner consumes.
 * Zero tokens, no network; determinism through the injected seed and clock.
 */

const FIXED_CLOCK = 1_700_000_000_000;
const scratchDirs: string[] = [];

function scratch(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-loop-"));
	scratchDirs.push(dir);
	return dir;
}

afterEach(() => {
	for (const dir of scratchDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function options(dir: string, over: Partial<DreamLoopOptions> = {}): DreamLoopOptions {
	return {
		task: resolveTask({ task: "sum-difference" }),
		taskId: "sum-difference",
		seed: 7,
		clock: () => FIXED_CLOCK,
		workers: 3,
		k1: 5,
		k2: 10,
		dreams: 4,
		iterations: 2,
		dir,
		...over,
	};
}

function treeFiles(dir: string): Record<string, string> {
	const treesDir = join(dir, "trees");
	const files: Record<string, string> = {};
	for (const entry of readdirSync(treesDir).sort()) {
		if (entry.endsWith(".jsonl")) files[entry] = readFileSync(join(treesDir, entry), "utf8");
		else {
			const blobDir = join(treesDir, entry, "blobs");
			for (const blob of readdirSync(blobDir).sort()) {
				files[`${entry}/blobs/${blob}`] = readFileSync(join(blobDir, blob), "utf8");
			}
		}
	}
	return files;
}

function captured(run: () => DreamLoopResult): { result: DreamLoopResult; spans: SpanEndRecord[] } {
	const spans: SpanEndRecord[] = [];
	const unsubscribe = addSpanSink((record) => spans.push(record));
	try {
		return { result: run(), spans };
	} finally {
		unsubscribe();
	}
}

describe("runDreamLoop fixedPolicy", () => {
	it("never dreams, redeploys the initial policy every iteration, and records null dreaming", () => {
		const dir = scratch();
		const { result, spans } = captured(() => runDreamLoop(options(dir, { fixedPolicy: true })));

		expect(result.fixedPolicy).toBe(true);
		expect(result.iterations).toBe(2);
		expect(result.rounds).toHaveLength(3);
		expect(result.treeIds).toHaveLength(3);
		expect(result.finalPolicyId).toBe(result.initialPolicyId);
		expect(result.initialPolicyId).toBe(policyId(DEFAULT_POLICY));
		expect(result.improved).toBe(false);
		expect(result.finalPolicyScore).toBe(result.initialPolicyScore);
		expect(result.tokens).toBe(0);
		for (const record of result.rounds) {
			expect(record.dreaming).toBeNull();
			expect(record.policyId).toBe(result.initialPolicyId);
			expect(record.handlerCalls).toEqual({ proposer: 0, dreamer: 0, guidance: 0 });
			expect(record.tokens).toEqual({ rollout: 0, dreamer: 0, guidance: 0 });
		}

		const run = spans.find((span) => span.name === "dream.run");
		expect(run?.attrs["dream.fixed_policy"]).toBe(true);
		const inRun = spans.filter((span) => span.traceId === run?.traceId);
		expect(inRun.filter((span) => span.name === "dream.dream")).toHaveLength(0);
		expect(inRun.filter((span) => span.name === "dream.replay")).toHaveLength(0);
		const redeploys = inRun.filter((span) => span.name === "dream.redeploy");
		expect(redeploys).toHaveLength(2);
		expect(redeploys.every((span) => span.attrs["dream.fixed_policy"] === true)).toBe(true);
		// Every tree header carries the initial policy.
		expect(listTrees(dir).every((summary) => summary.policyId === result.initialPolicyId)).toBe(true);
	});

	it("shares a byte-identical iteration 0 with the dreaming loop and the same tree ids", () => {
		const fixedDir = scratch();
		const dreamDir = scratch();
		const fixed = runDreamLoop(options(fixedDir, { fixedPolicy: true }));
		const dream = runDreamLoop(options(dreamDir));
		expect(dream.fixedPolicy).toBe(false);
		expect(dream.treeIds).toEqual(fixed.treeIds);
		expect(dream.rounds[0]).toEqual(fixed.rounds[0]);

		const fixedFiles = treeFiles(fixedDir);
		const dreamFiles = treeFiles(dreamDir);
		const firstTree = fixed.treeIds[0]!;
		const firstFiles = Object.keys(fixedFiles).filter((name) => name.startsWith(firstTree));
		expect(firstFiles.length).toBeGreaterThan(1);
		for (const name of firstFiles) expect(dreamFiles[name]).toBe(fixedFiles[name]);

		// The dreaming loop's dream.run attr is false and its later iterations record a dreaming step.
		const { spans } = captured(() => runDreamLoop(options(scratch())));
		expect(spans.find((span) => span.name === "dream.run")?.attrs["dream.fixed_policy"]).toBe(false);
		for (const record of dream.rounds.slice(1)) {
			expect(record.dreaming).not.toBeNull();
			expect(record.dreaming?.candidates).toBe(4);
			expect(record.dreaming!.chosenScore).toBeGreaterThanOrEqual(record.dreaming!.currentScore);
		}
	});
});

describe("runDreamLoop initialPolicy and round records", () => {
	it("honours an explicit initial policy on every tree header and in the result", () => {
		const dir = scratch();
		const initial: ExplorationPolicy = { ...DEFAULT_POLICY, batchSize: 2, beta: 3, stopRule: "fixed-rounds" };
		const result = runDreamLoop(options(dir, { initialPolicy: initial, fixedPolicy: true }));
		expect(result.initialPolicyId).toBe(policyId(initial));
		expect(result.finalPolicy).toEqual(initial);
		expect(listTrees(dir).every((summary) => summary.policyId === policyId(initial))).toBe(true);
	});

	it("records one row per rollout that agrees with the persisted trees", () => {
		const dir = scratch();
		const result = runDreamLoop(options(dir));
		const byId = new Map(listTrees(dir).map((summary) => [summary.treeId, summary]));
		expect(result.rounds.map((record) => record.treeId)).toEqual(result.treeIds);
		result.rounds.forEach((record, index) => {
			const summary = byId.get(record.treeId);
			expect(summary).toBeDefined();
			expect(record.iteration).toBe(index);
			expect(record.roundBest).toBe(summary!.bestScore);
			expect(record.probes).toBe(summary!.nodeCount - 1);
			expect(record.policyId).toBe(summary!.policyId);
			expect(record.poolSize).toBe(index);
			expect(record.decisionRounds).toBeGreaterThanOrEqual(1);
		});
		expect(result.bestNodeScore).toBe(Math.max(...result.rounds.map((record) => record.roundBest)));
		expect(result.rounds[0]!.dreaming).toBeNull();
	});

	it("leaves the unflagged loop byte-identical across two runs", () => {
		const a = scratch();
		const b = scratch();
		const first = runDreamLoop(options(a));
		const second = runDreamLoop(options(b));
		expect(JSON.stringify(second)).toBe(JSON.stringify(first));
		expect(treeFiles(b)).toEqual(treeFiles(a));
	});
});

/** A round record without its clock-derived tree id. */
function clockFree(record: DreamRoundRecord): Omit<DreamRoundRecord, "treeId"> {
	const { treeId: _treeId, ...rest } = record;
	return rest;
}

describe("runDreamLoop determinism across clocks", () => {
	it("yields identical round tables and final policies for one seed under two clocks; only the ids differ", () => {
		for (const fixedPolicy of [false, true]) {
			const a = scratch();
			const b = scratch();
			const first = runDreamLoop(options(a, { fixedPolicy, clock: () => 1_789_842_143_996 }));
			const second = runDreamLoop(options(b, { fixedPolicy, clock: () => 1 }));
			expect(second.treeIds).not.toEqual(first.treeIds);
			expect(second.runId).not.toBe(first.runId);
			expect(second.rounds.map(clockFree)).toEqual(first.rounds.map(clockFree));
			expect(second.rounds.map((record) => record.policyId)).toEqual(first.rounds.map((record) => record.policyId));
			expect(second.finalPolicyId).toBe(first.finalPolicyId);
			expect(second.finalPolicyScore).toBe(first.finalPolicyScore);
			expect(second.initialPolicyScore).toBe(first.initialPolicyScore);
			expect(second.bestNodeScore).toBe(first.bestNodeScore);
			expect(second.improved).toBe(first.improved);
			// The trees differ only in their ids and timestamps: the same node lines modulo id/ts.
			const strip = (files: Record<string, string>) =>
				Object.values(files)
					.filter((content) => content.startsWith('{"type":"tree"'))
					.map((content) =>
						content
							.split("\n")
							.filter((line) => line.startsWith('{"type":"node"'))
							.map((line) => {
								const node = JSON.parse(line) as Record<string, unknown>;
								return [node.seq, node.branch, node.round, node.score, node.valid, node.artifactRef];
							}),
					);
			expect(strip(treeFiles(b))).toEqual(strip(treeFiles(a)));
		}
	});

	it("changes the round table when the seed changes", () => {
		const first = runDreamLoop(options(scratch(), { seed: 7 }));
		const second = runDreamLoop(options(scratch(), { seed: 8 }));
		expect(second.rounds.map(clockFree)).not.toEqual(first.rounds.map(clockFree));
	});
});
