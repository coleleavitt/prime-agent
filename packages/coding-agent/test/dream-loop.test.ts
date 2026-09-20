import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { addSpanSink, type SpanEndRecord } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import {
	type DreamCandidateLine,
	type DreamProbationLine,
	type DreamStepLine,
	dreamsPath,
	readDreamsLog,
} from "../src/core/dream/dreams.js";
import {
	type DreamLoopOptions,
	type DreamLoopResult,
	type DreamRoundRecord,
	dreamRunId,
	judgeProbation,
	mergedRoundCurve,
	PROBATION_EPS,
	primingTreeId,
	runDreamLoop,
} from "../src/core/dream/loop.js";
import { DEFAULT_POLICY, type ExplorationPolicy, PRIMING_DIVERSE, policyId } from "../src/core/dream/policy.js";
import { listTrees, readTree } from "../src/core/dream/store.js";
import type { ScoredTask } from "../src/core/dream/task.js";
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

/**
 * sha256 of the sorted tree files (header/node/reveal lines and blobs) of a store.
 * The pinned digests below were produced by the loop at `15768af87`, BEFORE the
 * objective, verdict, lever-scan, dreams-log, runLabel and priming changes, for
 * exactly the `options()` configuration; a plain loop must still grow them.
 */
function treeDigest(dir: string): string {
	return createHash("sha256")
		.update(JSON.stringify(treeFiles(dir)))
		.digest("hex");
}
const PRE_CHANGE_TREE_DIGEST = "ef7ed6f1f3279f829d89a1ef60ec03d89930b505aefc35d00f2d2865cd51ff6b";

describe("runDreamLoop byte identity with the pre-change loop", () => {
	it("grows byte-identical tree files without runLabel or primingPolicies, dreaming or fixed", () => {
		for (const fixedPolicy of [true, false]) {
			const dir = scratch();
			const result = runDreamLoop(options(dir, { fixedPolicy }));
			expect(Object.keys(treeFiles(dir))).toHaveLength(34);
			expect(treeDigest(dir)).toBe(PRE_CHANGE_TREE_DIGEST);
			// The bare run id form and no priming trees.
			expect(result.runId).toBe(`sum-difference-s7-r${FIXED_CLOCK}`);
			expect(result.rounds[0]!.primingTreeIds).toBeUndefined();
			expect(result.rounds[0]!.primingProbes).toBeUndefined();
			// The dreams log lives beside trees/, never inside it, so the pool is unchanged.
			expect(listTrees(dir)).toHaveLength(3);
			expect(readdirSync(join(dir, "trees")).every((entry) => !entry.includes("dreams"))).toBe(true);
		}
	});
});

describe("runDreamLoop round curves, stoppedEarly and the dreaming record", () => {
	it("records the exact probe curve of every round and how many rollouts stopped before k1", () => {
		const dir = scratch();
		const result = runDreamLoop(options(dir));
		expect(result.stoppedEarly).toBe(result.rounds.filter((record) => record.decisionRounds < 5).length);
		for (const record of result.rounds) {
			const tree = readTree(record.treeId, dir);
			const bestSeq = tree.nodes
				.filter((node) => node.valid)
				.sort((a, b) => b.score - a.score || a.seq - b.seq)[0]!.seq;
			expect(record.probesToRoundBest).toBe(bestSeq);
			expect(record.improvements!.at(-1)).toEqual({ probe: bestSeq, score: record.roundBest });
			expect(record.improvements!.map((point) => point.score)).toEqual(
				[...record.improvements!.map((point) => point.score)].sort((a, b) => a - b),
			);
			expect(record.probesToRoundBest).toBeLessThanOrEqual(record.probes);
		}
	});

	it("fills the additive dreaming fields on every dreaming round", () => {
		const dir = scratch();
		const result = runDreamLoop(options(dir));
		for (const record of result.rounds.slice(1)) {
			const dreaming = record.dreaming!;
			expect(typeof dreaming.candidates).toBe("number");
			expect(dreaming.candidateVerdicts).toHaveLength(dreaming.candidates);
			expect(dreaming.dreamer).toBe("local");
			expect(dreaming.leverScan).not.toBeNull();
			expect(dreaming.leverScan!.gap).toBeGreaterThanOrEqual(0);
			expect(dreaming.candidateVerdicts!.every((verdict) => verdict.origin === "local")).toBe(true);
			const winners = dreaming.candidateVerdicts!.filter((verdict) => verdict.reason === "winner");
			expect(winners.length).toBe(dreaming.improved ? 1 : 0);
		}
		expect(result.finalSelection).toHaveLength(result.iterations);
	});
});

describe("runDreamLoop dreams log", () => {
	it("writes one candidate line per candidate per step plus a step line, and the final selection as iteration -1", () => {
		const dir = scratch();
		const result = runDreamLoop(options(dir, { dreamsLogContext: { experimentId: "exp-1", arm: "dream" } }));
		const path = dreamsPath(dir, result.runId);
		expect(path).toBe(join(dir, "dreams", `${result.runId}.jsonl`));
		expect(existsSync(path)).toBe(true);
		const lines = readDreamsLog(path);
		const steps = lines.filter((line): line is DreamStepLine => line.type === "step");
		const candidates = lines.filter((line): line is DreamCandidateLine => line.type === "candidate");
		expect(steps.map((step) => step.iteration)).toEqual([1, 2, -1]);
		expect(candidates.filter((line) => line.iteration === 1)).toHaveLength(4);
		expect(candidates.filter((line) => line.iteration === 2)).toHaveLength(4);
		expect(candidates.filter((line) => line.iteration === -1)).toHaveLength(2);
		for (const line of lines) {
			expect(line.ts).toBe(FIXED_CLOCK);
			expect(line.experimentId).toBe("exp-1");
			expect(line.arm).toBe("dream");
		}
		for (const step of steps.slice(0, 2)) {
			const record = result.rounds[step.iteration]!.dreaming!;
			expect(step.currentValue).toBe(record.currentScore);
			expect(step.improved).toBe(record.improved);
			expect(step.chosenPolicyId).toBe(result.rounds[step.iteration]!.policyId);
			expect(step.leverScan).toEqual(record.leverScan);
			expect(step.dreamer).toBe("local");
			expect(step.poolSize).toBe(record === result.rounds[1]!.dreaming ? 1 : 2);
		}
		const final = steps[2]!;
		expect(final.chosenPolicyId).toBe(result.finalPolicyId);
		expect(final.improved).toBe(result.improved);
		expect(final.leverScan).toBeNull();
		expect(final.poolSize).toBe(3);
		// A fixed-policy run never dreams, so it logs only the final selection (with no candidates).
		const fixedDir = scratch();
		const fixed = runDreamLoop(options(fixedDir, { fixedPolicy: true }));
		const fixedLines = readDreamsLog(dreamsPath(fixedDir, fixed.runId));
		expect(fixedLines).toHaveLength(1);
		expect(fixedLines[0]).toMatchObject({ type: "step", iteration: -1, improved: false });
	});
});

describe("runDreamLoop runLabel", () => {
	it("folds a clock-free label into the run id and the dreams-log key so arms under one clock differ", () => {
		const dreamDir = scratch();
		const fixedDir = scratch();
		const dream = runDreamLoop(options(dreamDir, { runLabel: "exp-1/dream" }));
		const fixed = runDreamLoop(options(fixedDir, { runLabel: "exp-1/fixed", fixedPolicy: true }));
		expect(dream.runId).toBe(`sum-difference-s7-r${FIXED_CLOCK}-exp-1_dream`);
		expect(fixed.runId).toBe(`sum-difference-s7-r${FIXED_CLOCK}-exp-1_fixed`);
		expect(dream.runId).not.toBe(fixed.runId);
		expect(existsSync(dreamsPath(dreamDir, dream.runId))).toBe(true);
		expect(existsSync(dreamsPath(fixedDir, fixed.runId))).toBe(true);
		// The label never reaches a tree: ids and files are those of the unlabelled loop.
		expect(dream.treeIds).toEqual(runDreamLoop(options(scratch())).treeIds);
		expect(treeDigest(dreamDir)).toBe(PRE_CHANGE_TREE_DIGEST);
		expect(dreamRunId("sum-difference", 7, 5, "a b/c")).toBe("sum-difference-s7-r5-a_b_c");
		expect(dreamRunId("sum-difference", 7, 5)).toBe("sum-difference-s7-r5");
		expect(dreamRunId("sum-difference", 7, 5, "")).toBe("sum-difference-s7-r5");
	});
});

describe("runDreamLoop primingPolicies", () => {
	it("rolls out each priming policy at iteration 0, pools the trees and charges them to round 1", () => {
		const dir = scratch();
		const plainDir = scratch();
		const plain = runDreamLoop(options(plainDir));
		const result = runDreamLoop(options(dir, { primingPolicies: PRIMING_DIVERSE }));
		const first = result.rounds[0]!;
		const primingIds = PRIMING_DIVERSE.map((_, index) => primingTreeId("sum-difference", 7, index, FIXED_CLOCK));
		expect(first.primingTreeIds).toEqual(primingIds);
		expect(primingIds).toEqual(["sum-difference-s7-i0p0-1700000000000", "sum-difference-s7-i0p1-1700000000000"]);
		// The initial rollout is unchanged: same tree id and file as the plain loop.
		expect(result.treeIds).toEqual(plain.treeIds);
		const initialFile = `${plain.treeIds[0]}.jsonl`;
		expect(treeFiles(dir)[initialFile]).toBe(treeFiles(plainDir)[initialFile]);
		// Later rollouts differ once the pool holds priming trees only if dreaming chose another policy.
		expect(result.rounds[0]!.policyId).toBe(plain.rounds[0]!.policyId);
		const summaries = new Map(listTrees(dir).map((summary) => [summary.treeId, summary]));
		expect(summaries.size).toBe(3 + PRIMING_DIVERSE.length);
		let primingProbes = 0;
		primingIds.forEach((treeId, index) => {
			const summary = summaries.get(treeId)!;
			expect(summary.iteration).toBe(0);
			expect(summary.policyId).toBe(policyId(PRIMING_DIVERSE[index]!));
			primingProbes += summary.nodeCount - 1;
		});
		expect(first.primingProbes).toBe(primingProbes);
		expect(first.probes).toBe(plain.rounds[0]!.probes + primingProbes);
		expect(first.roundBest).toBe(
			Math.max(plain.rounds[0]!.roundBest, ...primingIds.map((id) => summaries.get(id)!.bestScore)),
		);
		expect(first.probesToRoundBest).toBeLessThanOrEqual(first.probes);
		expect(first.improvements!.at(-1)!.score).toBe(first.roundBest);
		// Priming rollouts are not counted as stopped-early rollouts and their tokens are zero here.
		expect(result.stoppedEarly).toBeLessThanOrEqual(result.rounds.length);
		expect(first.tokens.rollout).toBe(0);
		// The pool the first dreaming step froze holds the priming trees.
		expect(result.rounds[1]!.poolSize).toBe(1 + PRIMING_DIVERSE.length);
		expect(result.rounds[1]!.dreaming!.candidateVerdicts).toHaveLength(4);
		// The fixed control charges the same priming to round 1 and reports the same pool size.
		const fixed = runDreamLoop(options(scratch(), { primingPolicies: PRIMING_DIVERSE, fixedPolicy: true }));
		expect(fixed.rounds[0]).toEqual(first);
		expect(fixed.rounds[1]!.poolSize).toBe(1 + PRIMING_DIVERSE.length);
	});

	it("merges the round-1 curve over the initial rollout and then each priming tree", () => {
		const dir = scratch();
		const result = runDreamLoop(options(dir, { primingPolicies: PRIMING_DIVERSE, iterations: 0 }));
		const first = result.rounds[0]!;
		const trees = [result.treeIds[0]!, ...first.primingTreeIds!].map((treeId) => readTree(treeId, dir));
		// Every root (each rollout seeds its own) is known before the first probe: the best valid one is probe 0.
		const roots = trees.flatMap((tree) => tree.nodes.filter((node) => node.parentId === null && node.valid));
		let best = Math.max(...roots.map((node) => node.score));
		const expected: { probe: number; score: number }[] = [{ probe: 0, score: best }];
		let offset = 0;
		for (const tree of trees) {
			for (const node of tree.nodes) {
				if (!node.valid || node.parentId === null) continue;
				if (node.score > best) {
					best = node.score;
					expected.push({ probe: offset + node.seq, score: node.score });
				}
			}
			offset += tree.nodes.length - 1;
		}
		expect(first.improvements).toEqual(expected);
		expect(first.probesToRoundBest).toBe(expected.at(-1)!.probe);
		expect(expected.at(-1)!.score).toBe(first.roundBest);
		expect(mergedRoundCurve([])).toEqual({ probesToBest: 0, improvements: [] });
	});
});

/**
 * A scripted task for the probation: every artifact is a number, the root is 0,
 * and a child's score depends only on the online round and on which rollout of
 * the run grows it (counted by `root` calls, so the test double can stage what
 * a fresh rollout finds). Trees 0, 1 and 3+: round 1 finds 1.0, later rounds
 * 0.5, so a patience incumbent records its best at the first probe. Tree 2, the
 * probation rollout of whatever the second dreaming step adopts: round 1 finds
 * `probationFirst`, round 2 and later 0.9 (the improvement a one-round policy
 * never sees online). Deterministic and rng-free by construction.
 */
function scriptedTask(probationFirst: number): ScoredTask<{ v: number }> {
	let trees = 0;
	const deserialize = (value: unknown): { v: number } => {
		if (typeof value !== "object" || value === null || typeof (value as { v?: unknown }).v !== "number") {
			throw new TypeError("expected { v: number }");
		}
		return { v: (value as { v: number }).v };
	};
	return {
		id: "sum-difference",
		root: () => {
			trees += 1;
			return { v: 0 };
		},
		propose: (_parent, _params, _rng, round) => {
			if (trees === 3) return { v: round === 1 ? probationFirst : 0.9 };
			return { v: round === 1 ? 1 : 0.5 };
		},
		evaluate: (candidate) => ({ valid: true, score: candidate.v }),
		serialize: (candidate) => ({ v: candidate.v }),
		deserialize,
	};
}

/** Run 3's dreamed collapse: one round of one probe. */
const COLLAPSE: ExplorationPolicy = { ...DEFAULT_POLICY, stopRule: "fixed-rounds", beta: 1 };

function scriptedOptions(dir: string, probationFirst: number): DreamLoopOptions {
	return options(dir, {
		task: scriptedTask(probationFirst),
		workers: 3,
		k1: 4,
		k2: 8,
		dreams: 1,
		iterations: 3,
		proposeCandidates: () => [COLLAPSE],
	});
}

describe("runDreamLoop probation", () => {
	it("judges a probation rollout against the incumbent's lowest replay best", () => {
		const step = {
			chosenPolicyId: policyId(COLLAPSE),
			candidates: [],
			current: {
				value: 0.9,
				quality: 1,
				anytime: 1,
				cost: 0.5,
				roundsSaved: 0,
				N: 6,
				rounds: 4,
				outOfSupportCells: 0,
				inSupportMean: 1,
				inSupportMin: 1,
				chargedProbes: 6,
				chargedRounds: 4,
			},
			currentMinBest: 0.7,
			evidenceTrees: 1,
		};
		const kept = judgeProbation(step, DEFAULT_POLICY, { treeId: "t", bestScore: 0.7 });
		expect(kept).toEqual({
			policyId: policyId(COLLAPSE),
			incumbentPolicyId: policyId(DEFAULT_POLICY),
			treeId: "t",
			roundBest: 0.7,
			floor: 0.7,
			chargedProbes: 6,
			chargedRounds: 4,
			incumbentChargedProbes: 6,
			incumbentChargedRounds: 4,
			evidenceTrees: 1,
			reverted: false,
		});
		expect(judgeProbation(step, DEFAULT_POLICY, { treeId: "t", bestScore: 0.7 - PROBATION_EPS / 2 }).reverted).toBe(
			false,
		);
		expect(judgeProbation(step, DEFAULT_POLICY, { treeId: "t", bestScore: 0.7 - 2 * PROBATION_EPS }).reverted).toBe(
			true,
		);
	});

	it("reverts an adopted policy whose first rollout falls below the floor, revokes it, and logs the judgement", () => {
		const dir = scratch();
		const { result, spans } = captured(() => runDreamLoop(scriptedOptions(dir, 0.2)));
		const initialId = policyId(DEFAULT_POLICY);
		const collapseId = policyId(COLLAPSE);
		expect(result.rounds.map((record) => record.policyId)).toEqual([initialId, initialId, collapseId, initialId]);
		// Iteration 1: one measured tree, no evidence, the collapse is charged the whole budget and loses.
		const first = result.rounds[1]!.dreaming!;
		expect(first.improved).toBe(false);
		expect(first.candidateVerdicts![0]!.reason).toBe("worse");
		expect(first.probation).toBeUndefined();
		// Iteration 2: two incumbent-grown trees whose best is the first probe vouch for stopping after
		// round 1 (the run-3 collapse with evidenceTrees 1), so the collapse wins on replay and is deployed.
		const second = result.rounds[2]!.dreaming!;
		expect(second.improved).toBe(true);
		expect(second.measuredTrees).toBe(2);
		const winner = second.candidateVerdicts![0]!;
		expect(winner.reason).toBe("winner");
		expect(winner.chargedProbes).toBe(1);
		expect(winner.chargedRounds).toBe(1);
		// Its probation rollout found 0.2 in its one round; the incumbent's lowest recorded best is 1.0. The
		// incumbent is charged its raw 6 probes over 4 rounds (reveals 1, 1, 2, 2: a batch never holds a node
		// with its parent, so round 2 probes only n1 and rounds 3-4 the root plus one leaf).
		expect(result.rounds[2]!.roundBest).toBe(0.2);
		expect(result.rounds[2]!.probes).toBe(1);
		expect(result.rounds[1]!.probes).toBe(6);
		expect(second.probation).toEqual({
			policyId: collapseId,
			incumbentPolicyId: initialId,
			treeId: result.rounds[2]!.treeId,
			roundBest: 0.2,
			floor: 1,
			chargedProbes: 1,
			chargedRounds: 1,
			incumbentChargedProbes: 6,
			incumbentChargedRounds: 4,
			evidenceTrees: 1,
			reverted: true,
		});
		// Iteration 3 dreams from the restored incumbent; the collapse is proposed again and is 'revoked'.
		const third = result.rounds[3]!.dreaming!;
		expect(third.improved).toBe(false);
		expect(third.candidateVerdicts![0]!.reason).toBe("revoked");
		expect(third.candidateVerdicts![0]!.eligible).toBe(false);
		expect(third.probation).toBeUndefined();
		expect(result.rounds[3]!.roundBest).toBe(1);
		expect(result.probationReverts).toBe(1);
		// The final selection never picks the revoked policy either.
		expect(result.finalPolicyId).toBe(initialId);
		expect(result.finalSelection!.map((verdict) => verdict.reason)).toEqual(["identical", "revoked", "identical"]);

		const lines = readDreamsLog(dreamsPath(dir, result.runId));
		const probations = lines.filter((line): line is DreamProbationLine => line.type === "probation");
		expect(probations).toHaveLength(1);
		expect(probations[0]).toEqual({ type: "probation", ts: FIXED_CLOCK, iteration: 2, ...second.probation });
		// The probation line follows its step line and precedes the next step's candidates.
		const order = lines.map((line) => `${line.type}:${line.iteration}`);
		expect(order.indexOf("probation:2")).toBeGreaterThan(order.indexOf("step:2"));
		expect(order.indexOf("probation:2")).toBeLessThan(order.indexOf("step:3"));

		const redeploys = spans.filter((span) => span.name === "dream.redeploy");
		expect(redeploys.map((span) => span.attrs["dream.probation"])).toEqual([false, true, false]);
		expect(redeploys[1]!.attrs["dream.reverted"]).toBe(true);
		expect(redeploys[1]!.attrs["dream.probation_floor"]).toBe(1);
		expect(redeploys[0]!.attrs["dream.reverted"]).toBeUndefined();
	});

	it("keeps an adopted policy whose probation rollout reaches the floor", () => {
		const dir = scratch();
		const result = runDreamLoop(scriptedOptions(dir, 1));
		const collapseId = policyId(COLLAPSE);
		expect(result.rounds.map((record) => record.policyId)).toEqual([
			policyId(DEFAULT_POLICY),
			policyId(DEFAULT_POLICY),
			collapseId,
			collapseId,
		]);
		const probation = result.rounds[2]!.dreaming!.probation!;
		expect(probation).toMatchObject({ policyId: collapseId, roundBest: 1, floor: 1, reverted: false });
		expect(result.probationReverts).toBe(0);
		// Iteration 3 dreams from the collapse; the same candidate is now 'identical'.
		expect(result.rounds[3]!.dreaming!.candidateVerdicts![0]!.reason).toBe("identical");
		const probations = readDreamsLog(dreamsPath(dir, result.runId)).filter((line) => line.type === "probation");
		expect(probations).toHaveLength(1);
		expect((probations[0] as DreamProbationLine).reverted).toBe(false);
	});
});
