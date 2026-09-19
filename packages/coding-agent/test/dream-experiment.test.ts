import { AsyncLocalStorage } from "node:async_hooks";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	addSpanSink,
	installAsyncTraceContextStorage,
	type SpanEndRecord,
	type TraceContext,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import {
	buildArmResult,
	computeHeadline,
	createLocalArmRunner,
	EXPERIMENT_SCHEMA,
	type ExperimentArmResult,
	ExperimentArmUnavailableError,
	type ExperimentProgressEvent,
	type ExperimentResult,
	type ExperimentRoundRow,
	type ExperimentSpec,
	isExperimentAbortError,
	isExperimentResult,
	OBJECTIVE_NOTE,
	planExperiment,
	readExperimentResult,
	runExperiment,
	runExperimentWithRunner,
	taskScoring,
	timingScoringNote,
} from "../src/core/dream/experiment.js";
import { type DreamLoopResult, type DreamRoundRecord, runDreamLoop } from "../src/core/dream/loop.js";
import { DEFAULT_POLICY, policyId } from "../src/core/dream/policy.js";
import {
	addProposalTally,
	PROPOSAL_REJECT_REASONS,
	type ProposalTally,
	tallyAccepted,
	tallyRejected,
	totalRejected,
	zeroProposalTally,
} from "../src/core/dream/proposer.js";
import {
	DreamStoreError,
	experimentArmDir,
	experimentResultPath,
	listExperimentIds,
	listTrees,
} from "../src/core/dream/store.js";

/**
 * The experiment runner: the dreaming arm against the fixed-exploration control,
 * both from one policy, seed, clock and budget. Zero tokens, no network. Every
 * assertion here is on sum-difference (deterministic scores); python-speedup is
 * never run in a test.
 */

installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const FIXED_CLOCK = 1_700_000_000_000;
const scratchDirs: string[] = [];

function scratch(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-experiment-"));
	scratchDirs.push(dir);
	return dir;
}

afterEach(() => {
	for (const dir of scratchDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

const SPEC: ExperimentSpec = {
	task: "sum-difference",
	seed: 7,
	rounds: 3,
	budget: { workers: 3, k1: 5, k2: 10, dreams: 4 },
	arms: ["fixed", "dream"],
};

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

function arm(result: ExperimentResult, name: string): ExperimentArmResult {
	const found = result.arms.find((candidate) => candidate.arm === name);
	if (!found) throw new Error(`no arm ${name}`);
	return found;
}

describe("runExperiment (local)", () => {
	it("gives every arm an identical round 1 and its own store, and never touches the pool", () => {
		const dir = scratch();
		const result = runExperiment(SPEC, { dir, clock: () => FIXED_CLOCK });
		expect(result.schema).toBe(EXPERIMENT_SCHEMA);
		expect(result.arms.map((a) => a.arm)).toEqual(["fixed", "dream"]);
		expect(result.sharedInitialRollout).toBe(false);
		expect(result.scoring).toBe("deterministic");
		expect(result.initialPolicyId).toBe(policyId(DEFAULT_POLICY));
		expect(result.experimentId).toBe(`sum-difference-s7-n3-${FIXED_CLOCK}`);

		const fixed = arm(result, "fixed");
		const dream = arm(result, "dream");
		expect(fixed.rounds[0]).toEqual(dream.rounds[0]);
		expect(fixed.rounds.map((row) => row.treeId)).toEqual(dream.rounds.map((row) => row.treeId));

		const fixedDir = experimentArmDir(dir, result.experimentId, "fixed");
		const dreamDir = experimentArmDir(dir, result.experimentId, "dream");
		expect(fixed.storeDir).toBe(`experiments/${result.experimentId}/fixed`);
		expect(join(dir, fixed.storeDir)).toBe(fixedDir);
		const fixedFiles = treeFiles(fixedDir);
		const dreamFiles = treeFiles(dreamDir);
		const firstTree = fixed.rounds[0]!.treeId;
		const firstFiles = Object.keys(fixedFiles).filter((name) => name.startsWith(firstTree));
		expect(firstFiles.length).toBeGreaterThan(1);
		for (const name of firstFiles) expect(dreamFiles[name]).toBe(fixedFiles[name]);

		expect(existsSync(join(dir, "trees"))).toBe(false);
		expect(listTrees(dir)).toEqual([]);
		expect(listTrees(fixedDir)).toHaveLength(3);
		expect(listTrees(dreamDir)).toHaveLength(3);
		expect(listExperimentIds(dir)).toEqual([result.experimentId]);
		expect(statSync(experimentResultPath(dir, result.experimentId)).mode & 0o777).toBe(0o600);
	});

	it("keeps the fixed arm fixed and lets the dream arm dream under the no-worse rule", () => {
		const dir = scratch();
		const result = runExperiment(SPEC, { dir, clock: () => FIXED_CLOCK });
		const fixed = arm(result, "fixed");
		expect(fixed.fixedPolicy).toBe(true);
		expect(fixed.guided).toBe(false);
		expect(fixed.mode).toEqual({ proposer: "local", dreamer: "local" });
		expect(fixed.policyChanges).toBe(0);
		expect(fixed.finalPolicyId).toBe(fixed.initialPolicyId);
		expect(fixed.selectedPolicyId).toBe(fixed.initialPolicyId);
		expect(fixed.policyScoreOnOwnPool.final).toBe(fixed.policyScoreOnOwnPool.initial);
		expect(fixed.rounds.every((row) => row.dreaming === null)).toBe(true);
		expect(fixed.rounds.every((row) => row.policyId === fixed.initialPolicyId)).toBe(true);
		for (let index = 1; index < fixed.rounds.length; index++) {
			expect(fixed.rounds[index]!.cumulativeProbes).toBeGreaterThan(fixed.rounds[index - 1]!.cumulativeProbes);
			expect(fixed.rounds[index]!.cumulativeBest).toBeGreaterThanOrEqual(fixed.rounds[index - 1]!.cumulativeBest);
		}
		expect(fixed.totals.handlerCalls).toBe(0);
		expect(fixed.totals.tokens).toBe(0);
		expect(fixed.totals.probes).toBe(fixed.rounds.at(-1)!.cumulativeProbes);
		expect(fixed.totals.finalBest).toBe(fixed.rounds.at(-1)!.cumulativeBest);
		// The local path generates nothing through an agent: every probe is a local candidate, no fallback.
		for (const armResult of result.arms) {
			expect(armResult.totals.agentGeneratedCalls).toBe(0);
			expect(armResult.totals.localFallbacks).toBe(0);
			expect(armResult.totals.llmProposals).toBe(0);
			expect(armResult.totals.llmAccepted).toBe(0);
			expect(armResult.totals.llmRejected).toEqual(zeroProposalTally().llmRejected);
			for (const row of armResult.rounds) {
				expect(row.probes).toBeGreaterThan(0);
				expect(row.agentGeneratedCalls).toBe(0);
				expect(row.cumulativeAgentGeneratedCalls).toBe(0);
				expect(row.localFallbacks).toBe(0);
				expect(row.llmProposals).toBe(0);
				expect(row.llmAccepted).toBe(0);
				expect(totalRejected(row)).toBe(0);
			}
		}

		const dream = arm(result, "dream");
		expect(dream.fixedPolicy).toBe(false);
		expect(dream.rounds[0]!.dreaming).toBeNull();
		expect(dream.rounds[1]!.dreaming).not.toBeNull();
		expect(dream.rounds[1]!.dreaming!.candidates).toBe(4);
		expect(dream.rounds[1]!.dreaming!.chosenScore).toBeGreaterThanOrEqual(dream.rounds[1]!.dreaming!.currentScore);
		expect(dream.policyScoreOnOwnPool.final).toBeGreaterThanOrEqual(dream.policyScoreOnOwnPool.initial);
		expect(dream.rounds.map((row) => row.round)).toEqual([1, 2, 3]);
		expect(dream.rounds.map((row) => row.poolSize)).toEqual([0, 1, 2]);
		// The final policy is what the arm last ran; the selected one is a post-hoc pick from {initial} + the dreamed ones.
		expect(dream.finalPolicyId).toBe(dream.rounds.at(-1)!.policyId);
		expect(dream.rounds.map((row) => row.policyId)).toContain(dream.selectedPolicyId);

		expect(result.headline).not.toBeNull();
		expect(result.headline!.reference).toBe("fixed");
		expect(result.headline!.target).toBe(fixed.totals.finalBest);
		expect(result.headline!.callsMultiplier.fixed).toBe(1);
		expect(result.headline!.scoreMultiplier.fixed).toBe(1);
		expect(result.headline!.deltaBest.fixed).toBe(0);
		expect(result.notes).toEqual([OBJECTIVE_NOTE]);
		expect(result.objective).toEqual({ beta1: 0.05, beta2: 0.05 });
	});

	it("persists a result that round-trips, validates by schema, and refuses to be overwritten silently", () => {
		const dir = scratch();
		const result = runExperiment(SPEC, { dir, clock: () => FIXED_CLOCK });
		const path = experimentResultPath(dir, result.experimentId);
		expect(existsSync(path)).toBe(true);
		expect(readExperimentResult(dir, result.experimentId)).toEqual(result);
		expect(isExperimentResult(JSON.parse(readFileSync(path, "utf8")))).toBe(true);
		expect(isExperimentResult({ ...result, schema: "x/0" })).toBe(false);
		expect(isExperimentResult(null)).toBe(false);
		// `scoring` and `selectedPolicyId` are additive: a file written before them still validates, a malformed one does not.
		const { scoring: _scoring, ...legacy } = result;
		expect(
			isExperimentResult({
				...legacy,
				arms: result.arms.map(({ selectedPolicyId: _selected, ...armRest }) => armRest),
			}),
		).toBe(true);
		expect(isExperimentResult({ ...result, scoring: "noisy" })).toBe(false);
		expect(isExperimentResult({ ...result, arms: [{ ...result.arms[0]!, selectedPolicyId: 1 }] })).toBe(false);
		// Provenance totals are additive too: a file without them validates, a malformed one does not.
		const {
			agentGeneratedCalls: _agent,
			localFallbacks: _fallbacks,
			llmProposals: _proposals,
			llmAccepted: _accepted,
			llmRejected: _rejected,
			...legacyTotals
		} = result.arms[0]!.totals;
		expect(isExperimentResult({ ...result, arms: [{ ...result.arms[0]!, totals: legacyTotals }] })).toBe(true);
		expect(
			isExperimentResult({
				...result,
				arms: [{ ...result.arms[0]!, totals: { ...result.arms[0]!.totals, agentGeneratedCalls: "3" } }],
			}),
		).toBe(false);
		expect(
			isExperimentResult({
				...result,
				arms: [{ ...result.arms[0]!, totals: { ...result.arms[0]!.totals, llmRejected: { parse: "1" } } }],
			}),
		).toBe(false);
		expect(() => readExperimentResult(dir, "missing")).toThrow(DreamStoreError);

		expect(() => runExperiment(SPEC, { dir, clock: () => FIXED_CLOCK })).toThrow(/already exists/);
		// Overwrite replaces the whole experiment directory, so the re-run starts from empty pools and is identical.
		const again = runExperiment(SPEC, { dir, clock: () => FIXED_CLOCK, overwrite: true });
		expect(JSON.stringify(again)).toBe(JSON.stringify(result));
		expect(listTrees(experimentArmDir(dir, again.experimentId, "dream"))).toHaveLength(3);
	});

	it("is deterministic across stores: JSON-equal results and byte-identical trees", () => {
		const a = scratch();
		const b = scratch();
		const first = runExperiment(SPEC, { dir: a, clock: () => FIXED_CLOCK });
		const second = runExperiment(SPEC, { dir: b, clock: () => FIXED_CLOCK });
		expect(JSON.stringify(second)).toBe(JSON.stringify(first));
		for (const name of SPEC.arms) {
			expect(treeFiles(experimentArmDir(b, second.experimentId, name))).toEqual(
				treeFiles(experimentArmDir(a, first.experimentId, name)),
			);
		}
	});

	it("yields identical round tables for one seed under two clocks, with only the ids differing", () => {
		const a = scratch();
		const b = scratch();
		const first = runExperiment(SPEC, { dir: a, clock: () => 1_789_842_143_996 });
		const second = runExperiment(SPEC, { dir: b, clock: () => 1 });
		expect(second.experimentId).not.toBe(first.experimentId);
		expect(second.createdTs).not.toBe(first.createdTs);
		const table = (result: ExperimentResult) =>
			result.arms.map((armResult) => ({
				arm: armResult.arm,
				initialPolicyId: armResult.initialPolicyId,
				finalPolicyId: armResult.finalPolicyId,
				selectedPolicyId: armResult.selectedPolicyId,
				policyScoreOnOwnPool: armResult.policyScoreOnOwnPool,
				policyChanges: armResult.policyChanges,
				totals: armResult.totals,
				rounds: armResult.rounds.map(({ treeId: _treeId, ...row }) => row),
			}));
		expect(table(second)).toEqual(table(first));
		expect(second.headline).toEqual(first.headline);
		expect(second.arms.map((armResult) => armResult.rounds.map((row) => row.treeId))).not.toEqual(
			first.arms.map((armResult) => armResult.rounds.map((row) => row.treeId)),
		);
	});

	it("marks the task's scoring, qualifies round 1 for a timing task, and notes the missing control", () => {
		const dir = scratch();
		const onlyDream = planExperiment({ ...SPEC, arms: ["dream"] }, { dir, clock: () => FIXED_CLOCK });
		expect(onlyDream.scoring).toBe("deterministic");
		expect(onlyDream.notes).toEqual(["no fixed arm ran: the headline multipliers are undefined", OBJECTIVE_NOTE]);
		expect(taskScoring("circle-packing")).toBe("deterministic");
		expect(taskScoring("sum-difference")).toBe("deterministic");
		expect(taskScoring("python-speedup")).toBe("timing");
		// Planning never runs a task, so python-speedup is safe to plan here; it is never rolled out in a test.
		const speedup = planExperiment({ ...SPEC, task: "python-speedup" }, { dir, clock: () => FIXED_CLOCK });
		expect(speedup.scoring).toBe("timing");
		expect(speedup.notes).toEqual([timingScoringNote("python-speedup"), OBJECTIVE_NOTE]);
		const note = speedup.notes[0]!;
		expect(note.startsWith("python-speedup:")).toBe(true);
		expect(note).toContain("wall-clock timed");
		expect(note).toContain("round 1");
		expect(note).toContain("timing noise");
		expect(speedup.notes.at(-1)).toBe(OBJECTIVE_NOTE);
		// A caller's objective is recorded on the plan and the result.
		const tuned = planExperiment(
			{ ...SPEC, objective: { beta1: 0.2, beta2: 0.1 } },
			{ dir, clock: () => FIXED_CLOCK },
		);
		expect(tuned.objective).toEqual({ beta1: 0.2, beta2: 0.1 });
		expect(tuned.arms.every((arm) => arm.loop.objective.beta1 === 0.2)).toBe(true);
		const result = runExperiment({ ...SPEC, arms: ["dream"] }, { dir, clock: () => FIXED_CLOCK });
		expect(result.headline).toBeNull();
	});

	it("rejects guided arms, bad rounds and duplicate arms before creating anything", () => {
		const dir = scratch();
		const clock = (): number => FIXED_CLOCK;
		expect(() => runExperiment({ ...SPEC, arms: ["dream", "dream-guided"] }, { dir, clock })).toThrow(
			ExperimentArmUnavailableError,
		);
		expect(() => runExperiment({ ...SPEC, arms: ["fixed-guided"] }, { dir, clock })).toThrow(
			/in-session LLM proposer/,
		);
		expect(() => runExperiment({ ...SPEC, rounds: 0 }, { dir, clock })).toThrow(RangeError);
		expect(() => runExperiment({ ...SPEC, rounds: 1.5 }, { dir, clock })).toThrow(RangeError);
		expect(() => runExperiment({ ...SPEC, arms: ["dream", "dream"] }, { dir, clock })).toThrow(RangeError);
		expect(() => runExperiment({ ...SPEC, arms: [] }, { dir, clock })).toThrow(RangeError);
		expect(() => runExperiment({ ...SPEC, budget: { ...SPEC.budget, workers: 0 } }, { dir, clock })).toThrow(
			RangeError,
		);
		expect(existsSync(join(dir, "experiments"))).toBe(false);
	});

	it("runs a single round as a one-row experiment where every arm ties", () => {
		const dir = scratch();
		const result = runExperiment({ ...SPEC, rounds: 1 }, { dir, clock: () => FIXED_CLOCK });
		expect(result.arms.every((a) => a.rounds.length === 1)).toBe(true);
		expect(result.headline!.callsMultiplier.dream).toBe(1);
		expect(result.headline!.scoreMultiplier.dream).toBe(1);
		expect(result.headline!.deltaBest.dream).toBe(0);
	});
});

function row(
	over: Partial<ExperimentRoundRow> & Pick<ExperimentRoundRow, "round" | "cumulativeBest" | "cumulativeProbes">,
): ExperimentRoundRow {
	return {
		treeId: `t${over.round}`,
		policyId: "p",
		roundBest: over.cumulativeBest,
		probes: 10,
		agentGeneratedCalls: 0,
		cumulativeAgentGeneratedCalls: 0,
		localFallbacks: 0,
		llmProposals: 0,
		llmAccepted: 0,
		llmRejected: zeroProposalTally().llmRejected,
		decisionRounds: 1,
		poolSize: over.round - 1,
		handlerCalls: { proposer: 0, dreamer: 0, guidance: 0 },
		cumulativeHandlerCalls: 0,
		tokens: 0,
		cumulativeTokens: 0,
		dreaming: null,
		...over,
	};
}

function syntheticArm(name: ExperimentArmResult["arm"], rows: ExperimentRoundRow[]): ExperimentArmResult {
	const last = rows.at(-1)!;
	return {
		arm: name,
		fixedPolicy: name.startsWith("fixed"),
		guided: name.endsWith("-guided"),
		mode: { proposer: "local", dreamer: "local" },
		storeDir: `experiments/x/${name}`,
		runId: "r",
		initialPolicyId: "p",
		finalPolicyId: "p",
		selectedPolicyId: "p",
		policyScoreOnOwnPool: { initial: 0, final: 0 },
		policyChanges: 0,
		rounds: rows,
		totals: {
			probes: last.cumulativeProbes,
			agentGeneratedCalls: last.cumulativeAgentGeneratedCalls,
			localFallbacks: 0,
			llmProposals: 0,
			llmAccepted: 0,
			llmRejected: zeroProposalTally().llmRejected,
			handlerCalls: 0,
			tokens: 0,
			finalBest: last.cumulativeBest,
		},
	};
}

describe("computeHeadline", () => {
	const fixed = syntheticArm("fixed", [
		row({ round: 1, cumulativeBest: 1.0, cumulativeProbes: 15 }),
		row({ round: 2, cumulativeBest: 1.5, cumulativeProbes: 30 }),
		row({ round: 3, cumulativeBest: 1.5, cumulativeProbes: 45 }),
	]);

	it("takes the target from the control and the compute-to-target from the FIRST reaching round", () => {
		const dream = syntheticArm("dream", [
			row({ round: 1, cumulativeBest: 1.0, cumulativeProbes: 15 }),
			row({ round: 2, cumulativeBest: 1.6, cumulativeProbes: 27 }),
			row({ round: 3, cumulativeBest: 1.7, cumulativeProbes: 39 }),
		]);
		const headline = computeHeadline([fixed, dream])!;
		expect(headline.target).toBe(1.5);
		expect(headline.probesToTarget).toEqual({ fixed: 30, dream: 27 });
		expect(headline.callsMultiplier.fixed).toBe(1);
		expect(headline.callsMultiplier.dream).toBeCloseTo(30 / 27, 12);
		expect(headline.equalBudget).toBe(39);
		expect(headline.bestAtBudget).toEqual({ fixed: 1.5, dream: 1.7 });
		expect(headline.scoreMultiplier.fixed).toBe(1);
		expect(headline.scoreMultiplier.dream).toBeCloseTo(1.7 / 1.5, 12);
		expect(headline.deltaBest.fixed).toBe(0);
		expect(headline.deltaBest.dream).toBeCloseTo(0.2, 12);
	});

	it("reports null, never a clamp, when the target is not reached", () => {
		const dream = syntheticArm("dream", [
			row({ round: 1, cumulativeBest: 1.0, cumulativeProbes: 15 }),
			row({ round: 2, cumulativeBest: 1.2, cumulativeProbes: 30 }),
			row({ round: 3, cumulativeBest: 1.4, cumulativeProbes: 45 }),
		]);
		const headline = computeHeadline([fixed, dream])!;
		expect(headline.probesToTarget.dream).toBeNull();
		expect(headline.callsMultiplier.dream).toBeNull();
		expect(headline.bestAtBudget.dream).toBe(1.4);
		expect(headline.scoreMultiplier.dream).toBeCloseTo(1.4 / 1.5, 12);
		expect(headline.deltaBest.dream).toBeCloseTo(-0.1, 12);
	});

	it("reports a multiplier below 1 as-is when the arm needed more compute", () => {
		const dream = syntheticArm("dream", [
			row({ round: 1, cumulativeBest: 1.0, cumulativeProbes: 15 }),
			row({ round: 2, cumulativeBest: 1.2, cumulativeProbes: 40 }),
			row({ round: 3, cumulativeBest: 1.5, cumulativeProbes: 60 }),
		]);
		const headline = computeHeadline([fixed, dream])!;
		expect(headline.probesToTarget.dream).toBe(60);
		expect(headline.callsMultiplier.dream).toBe(0.5);
		// Equal budget is the smaller total (45): dream's round 2 (40 probes) is the last that fits.
		expect(headline.equalBudget).toBe(45);
		expect(headline.bestAtBudget.dream).toBe(1.2);
	});

	it("leaves the equal-budget comparison null when no round of an arm fits, and when the control scored 0", () => {
		const big = syntheticArm("dream", [
			row({ round: 1, cumulativeBest: 2.0, cumulativeProbes: 50 }),
			row({ round: 2, cumulativeBest: 2.0, cumulativeProbes: 100 }),
		]);
		const small = syntheticArm("fixed", [row({ round: 1, cumulativeBest: 1.0, cumulativeProbes: 20 })]);
		const headline = computeHeadline([small, big])!;
		expect(headline.equalBudget).toBe(20);
		expect(headline.bestAtBudget.dream).toBeNull();
		expect(headline.scoreMultiplier.dream).toBeNull();

		const zero = syntheticArm("fixed", [row({ round: 1, cumulativeBest: 0, cumulativeProbes: 10 })]);
		const some = syntheticArm("dream", [row({ round: 1, cumulativeBest: 0.5, cumulativeProbes: 10 })]);
		const zeroHeadline = computeHeadline([zero, some])!;
		expect(zeroHeadline.scoreMultiplier.dream).toBeNull();
		expect(zeroHeadline.deltaBest.dream).toBe(0.5);
		expect(zeroHeadline.callsMultiplier.dream).toBe(1);
		// Zero probes to target is an undefined ratio, not infinity.
		const free = syntheticArm("dream", [row({ round: 1, cumulativeBest: 0.5, probes: 0, cumulativeProbes: 0 })]);
		expect(computeHeadline([zero, free])!.callsMultiplier.dream).toBeNull();
		expect(computeHeadline([zero, free])!.probesToTarget.dream).toBe(0);
	});

	it("is null without a fixed arm and includes the guided arms when present", () => {
		expect(
			computeHeadline([syntheticArm("dream", [row({ round: 1, cumulativeBest: 1, cumulativeProbes: 1 })])]),
		).toBe(null);
		const guided = syntheticArm("dream-guided", [
			row({ round: 1, cumulativeBest: 1.0, cumulativeProbes: 15 }),
			row({ round: 2, cumulativeBest: 1.3, cumulativeProbes: 30 }),
			row({ round: 3, cumulativeBest: 1.4, cumulativeProbes: 45 }),
		]);
		const headline = computeHeadline([fixed, guided])!;
		expect(headline.deltaBest["dream-guided"]).toBeCloseTo(-0.1, 12);
		expect(headline.probesToTarget["dream-guided"]).toBeNull();
	});
});

describe("buildArmResult", () => {
	it("derives cumulative fields and policy changes from the loop records", () => {
		const dir = scratch();
		const loop = runDreamLoop({
			task: planExperiment(SPEC, { dir, clock: () => FIXED_CLOCK }).task,
			taskId: "sum-difference",
			seed: 7,
			clock: () => FIXED_CLOCK,
			workers: 3,
			k1: 5,
			k2: 10,
			dreams: 4,
			iterations: 2,
			dir: join(dir, "loop"),
		});
		const result = buildArmResult(
			{ arm: "dream", fixedPolicy: false, guided: false, storeDir: "experiments/x/dream" },
			{ proposer: "local", dreamer: "local" },
			loop,
		);
		expect(result.rounds).toHaveLength(3);
		let probes = 0;
		let best = Number.NEGATIVE_INFINITY;
		for (const [index, record] of loop.rounds.entries()) {
			probes += record.probes;
			best = Math.max(best, record.roundBest);
			expect(result.rounds[index]!.cumulativeProbes).toBe(probes);
			expect(result.rounds[index]!.cumulativeBest).toBe(best);
			expect(result.rounds[index]!.treeId).toBe(record.treeId);
		}
		const changes = loop.rounds.filter(
			(record, index) => index > 0 && record.policyId !== loop.rounds[index - 1]!.policyId,
		);
		expect(result.policyChanges).toBe(changes.length);
		expect(result.totals.finalBest).toBe(loop.bestNodeScore);
		expect(result.runId).toBe(loop.runId);
		expect(result.finalPolicyId).toBe(loop.rounds.at(-1)!.policyId);
		expect(result.selectedPolicyId).toBe(loop.finalPolicyId);
	});

	it("reports the last deployed policy as final and the post-hoc pool winner as selected, so the table cannot contradict the line", () => {
		const record = (iteration: number, policy: string) => ({
			iteration,
			treeId: `t${iteration}`,
			policyId: policy,
			roundBest: 1 + iteration,
			probes: 4,
			decisionRounds: 2,
			poolSize: iteration,
			tokens: { rollout: 0, dreamer: 0, guidance: 0 },
			handlerCalls: { proposer: 0, dreamer: 0, guidance: 0 },
			dreaming: iteration === 0 ? null : { currentScore: 0.5, chosenScore: 0.6, improved: true, candidates: 2 },
		});
		// Round 3 ran p2, but on the final pool the loop's selection preferred the earlier p1.
		const loop: DreamLoopResult = {
			runId: "run",
			task: "sum-difference",
			seed: 1,
			mode: "local",
			iterations: 2,
			fixedPolicy: false,
			treeIds: ["t0", "t1", "t2"],
			rounds: [record(0, "p0"), record(1, "p1"), record(2, "p2")],
			initialPolicyId: "p0",
			initialPolicyScore: 0.4,
			finalPolicy: DEFAULT_POLICY,
			finalPolicyId: "p1",
			finalPolicyScore: 0.7,
			improved: true,
			bestNodeScore: 3,
			tokens: 0,
		};
		const result = buildArmResult(
			{ arm: "dream", fixedPolicy: false, guided: false, storeDir: "experiments/x/dream" },
			{ proposer: "local", dreamer: "local" },
			loop,
		);
		expect(result.finalPolicyId).toBe("p2");
		expect(result.rounds.at(-1)!.policyId).toBe(result.finalPolicyId);
		expect(result.policyChanges).toBe(2);
		expect(result.selectedPolicyId).toBe("p1");
		expect(result.policyScoreOnOwnPool).toEqual({ initial: 0.4, final: 0.7 });
		// A loop that never dreamed reports the initial policy on both axes.
		const fixed = buildArmResult(
			{ arm: "fixed", fixedPolicy: true, guided: false, storeDir: "experiments/x/fixed" },
			{ proposer: "local", dreamer: "local" },
			{
				...loop,
				fixedPolicy: true,
				rounds: [record(0, "p0"), record(1, "p0"), record(2, "p0")],
				finalPolicyId: "p0",
				finalPolicyScore: 0.4,
				improved: false,
			},
		);
		expect(fixed.finalPolicyId).toBe("p0");
		expect(fixed.selectedPolicyId).toBe("p0");
		expect(fixed.policyChanges).toBe(0);
	});

	it("reports agent-generated calls apart from probes and fallbacks, summing the proposer tally by reason", () => {
		// Round 1 (the measured defect's shape): 12 probes, 12 child results, 2 accepted, the rest
		// parse failures that fell back to the local mutator. Round 2: 12 probes, one retry
		// (13 results), 9 accepted, one shape rejection retried into an acceptance, 3 fallbacks.
		const round1 = zeroProposalTally();
		for (let index = 0; index < 12; index++) {
			if (index < 2) tallyAccepted(round1);
			else tallyRejected(round1, "parse", true);
		}
		const round2 = zeroProposalTally();
		for (let index = 0; index < 9; index++) tallyAccepted(round2);
		tallyRejected(round2, "shape", false);
		tallyRejected(round2, "length", true);
		tallyRejected(round2, "invalid-candidate", true);
		tallyRejected(round2, "error", true);
		expect(round1).toEqual({
			llmProposals: 12,
			llmAccepted: 2,
			llmRejected: { ...zeroProposalTally().llmRejected, parse: 10 },
			localFallbacks: 10,
		});
		expect(round2.llmProposals).toBe(13);
		expect(round2.llmAccepted).toBe(9);
		expect(totalRejected(round2)).toBe(4);
		expect(round2.localFallbacks).toBe(3);

		const record = (iteration: number, agentGeneratedCalls: number, proposals: ProposalTally): DreamRoundRecord => ({
			iteration,
			treeId: `t${iteration}`,
			policyId: "p",
			roundBest: 2 - iteration * 0.05,
			probes: 12,
			agentGeneratedCalls,
			proposals,
			decisionRounds: 3,
			poolSize: iteration,
			tokens: { rollout: 12 * 500, dreamer: 0, guidance: 0 },
			handlerCalls: { proposer: proposals.llmProposals, dreamer: 0, guidance: 0 },
			dreaming: null,
		});
		const loop: DreamLoopResult = {
			runId: "run",
			task: "autocorrelation",
			seed: 7,
			mode: "llm",
			iterations: 1,
			fixedPolicy: true,
			treeIds: ["t0", "t1"],
			rounds: [record(0, 2, round1), record(1, 9, round2)],
			initialPolicyId: "p",
			initialPolicyScore: 0.4,
			finalPolicy: DEFAULT_POLICY,
			finalPolicyId: "p",
			finalPolicyScore: 0.4,
			improved: false,
			bestNodeScore: 2,
			tokens: 24 * 500,
		};
		const result = buildArmResult(
			{ arm: "fixed", fixedPolicy: true, guided: false, storeDir: "experiments/x/fixed" },
			{ proposer: "llm", dreamer: "local", model: "faux/stub" },
			loop,
		);
		const [first, second] = result.rounds as [ExperimentRoundRow, ExperimentRoundRow];
		// Probes stay the compute axis; agent-generated calls are the subset the child actually produced.
		expect([first.probes, first.cumulativeProbes]).toEqual([12, 12]);
		expect([first.agentGeneratedCalls, first.cumulativeAgentGeneratedCalls]).toEqual([2, 2]);
		expect([first.llmProposals, first.llmAccepted, first.localFallbacks]).toEqual([12, 2, 10]);
		expect(first.llmRejected.parse).toBe(10);
		expect(first.probes).toBe(first.agentGeneratedCalls + first.localFallbacks);
		expect([second.probes, second.cumulativeProbes]).toEqual([12, 24]);
		expect([second.agentGeneratedCalls, second.cumulativeAgentGeneratedCalls]).toEqual([9, 11]);
		expect([second.llmProposals, second.llmAccepted, second.localFallbacks]).toEqual([13, 9, 3]);
		expect(second.llmRejected).toEqual({
			...zeroProposalTally().llmRejected,
			shape: 1,
			length: 1,
			"invalid-candidate": 1,
			error: 1,
		});
		expect(second.probes).toBe(second.agentGeneratedCalls + second.localFallbacks);
		// Handler calls are cost and stay separate from both compute counts.
		expect([first.cumulativeHandlerCalls, second.cumulativeHandlerCalls]).toEqual([12, 25]);
		expect(result.totals).toEqual({
			probes: 24,
			agentGeneratedCalls: 11,
			localFallbacks: 13,
			llmProposals: 25,
			llmAccepted: 11,
			llmRejected: {
				...zeroProposalTally().llmRejected,
				parse: 10,
				shape: 1,
				length: 1,
				"invalid-candidate": 1,
				error: 1,
			},
			handlerCalls: 25,
			tokens: 24 * 500,
			finalBest: 2,
		});
		expect(result.totals.llmRejected).toEqual(addProposalTally(round1, round2).llmRejected);
		// The rows hold copies: mutating the loop's tally afterwards changes nothing.
		tallyAccepted(round1);
		expect(result.rounds[0]!.llmAccepted).toBe(2);
		expect(result.totals.llmAccepted).toBe(11);
	});

	it("reads a record without provenance as zero agent-generated calls, never as every probe", () => {
		const untracked: DreamRoundRecord = {
			iteration: 0,
			treeId: "t0",
			policyId: "p",
			roundBest: 1,
			probes: 12,
			decisionRounds: 3,
			poolSize: 0,
			tokens: { rollout: 6000, dreamer: 0, guidance: 0 },
			handlerCalls: { proposer: 12, dreamer: 0, guidance: 0 },
			dreaming: null,
		};
		const result = buildArmResult(
			{ arm: "dream", fixedPolicy: false, guided: false, storeDir: "experiments/x/dream" },
			{ proposer: "llm", dreamer: "llm" },
			{
				runId: "run",
				task: "autocorrelation",
				seed: 1,
				mode: "llm",
				iterations: 0,
				fixedPolicy: false,
				treeIds: ["t0"],
				rounds: [untracked],
				initialPolicyId: "p",
				initialPolicyScore: 0,
				finalPolicy: DEFAULT_POLICY,
				finalPolicyId: "p",
				finalPolicyScore: 0,
				improved: false,
				bestNodeScore: 1,
				tokens: 6000,
			},
		);
		const only = result.rounds[0]!;
		expect(only.probes).toBe(12);
		expect(only.handlerCalls.proposer).toBe(12);
		expect(only.agentGeneratedCalls).toBe(0);
		expect(only.llmProposals).toBe(0);
		expect(only.localFallbacks).toBe(0);
		expect(only.llmRejected).toEqual(zeroProposalTally().llmRejected);
		expect(Object.keys(only.llmRejected).sort()).toEqual([...PROPOSAL_REJECT_REASONS].sort());
		expect(result.totals.agentGeneratedCalls).toBe(0);
	});
});

describe("runExperimentWithRunner", () => {
	it("matches the sync runner with the local arm runner, emits ordered progress and links the spans", async () => {
		const syncDir = scratch();
		const asyncDir = scratch();
		const expected = runExperiment(SPEC, { dir: syncDir, clock: () => FIXED_CLOCK });

		const events: ExperimentProgressEvent[] = [];
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let result: ExperimentResult;
		try {
			result = await withSpan("test.turn", {}, () =>
				runExperimentWithRunner(SPEC, {
					dir: asyncDir,
					clock: () => FIXED_CLOCK,
					runner: createLocalArmRunner(),
					onProgress: (event) => events.push(event),
				}),
			);
		} finally {
			unsubscribe();
		}
		expect(JSON.stringify(result)).toBe(JSON.stringify(expected));
		expect(readExperimentResult(asyncDir, result.experimentId)).toEqual(result);

		const kinds = events.map((event) => event.type);
		expect(kinds[0]).toBe("arm_start");
		expect(kinds.at(-1)).toBe("completed");
		expect(kinds.filter((kind) => kind === "arm_start")).toHaveLength(2);
		expect(kinds.filter((kind) => kind === "arm_end")).toHaveLength(2);
		expect(kinds.filter((kind) => kind === "round")).toHaveLength(6);
		const starts = events.filter((event) => event.type === "arm_start");
		expect(
			starts.map((event) => (event.type === "arm_start" ? [event.arm, event.armIndex, event.armCount] : [])),
		).toEqual([
			["fixed", 0, 2],
			["dream", 1, 2],
		]);
		const completed = events.at(-1);
		expect(completed?.type === "completed" && completed.resultPath).toBe(
			experimentResultPath(asyncDir, result.experimentId),
		);

		// dream.experiment is a DETACHED ROOT of a fresh trace carrying the trigger.
		const turn = spans.find((span) => span.name === "test.turn")!;
		const experiment = spans.find((span) => span.name === "dream.experiment")!;
		expect(experiment.parentSpanId).toBeUndefined();
		expect(experiment.traceId).not.toBe(turn.traceId);
		expect(experiment.attrs["trigger.trace_id"]).toBe(turn.traceId);
		expect(experiment.attrs["dream.mode"]).toBe("local");
		expect(experiment.attrs["dream.arms"]).toBe("fixed,dream");
		expect(experiment.status).toBe("ok");
		const trace = spans.filter((span) => span.traceId === experiment.traceId);
		const ids = new Set(trace.map((span) => span.spanId));
		for (const span of trace) {
			if (span.name === "dream.experiment") continue;
			expect(span.parentSpanId, `${span.name} must have a parent`).toBeDefined();
			expect(ids.has(span.parentSpanId!), `${span.name} parent must be in the trace`).toBe(true);
		}
		const arms = trace.filter((span) => span.name === "dream.experiment_arm");
		expect(arms).toHaveLength(2);
		expect(arms.every((span) => span.parentSpanId === experiment.spanId)).toBe(true);
		expect(arms.map((span) => span.attrs["dream.fixed_policy"])).toEqual([true, false]);
		expect(arms.every((span) => typeof span.attrs["dream.run_id"] === "string")).toBe(true);
		const runs = trace.filter((span) => span.name === "dream.run");
		expect(runs).toHaveLength(2);
		expect(runs.every((span) => arms.some((armSpan) => armSpan.spanId === span.parentSpanId))).toBe(true);
	});

	it("records a shared round 1 and the runner's mode, and refuses arms outside the allowed set", async () => {
		const dir = scratch();
		const seen: string[] = [];
		const result = await runExperimentWithRunner(SPEC, {
			dir,
			clock: () => FIXED_CLOCK,
			runner: {
				mode: (plan) => ({ proposer: "llm", dreamer: plan.fixedPolicy ? "local" : "llm", model: "faux/stub" }),
				prepare: async (plan) => {
					seen.push(`prepare:${plan.arms.length}`);
					return {
						treeId: "shared",
						bestScore: 1,
						revealedCount: 3,
						rounds: 1,
						tokens: 0,
						handlerCalls: { proposer: 3, dreamer: 0, guidance: 0 },
					};
				},
				run: async (plan, shared, onProgress) => {
					seen.push(`run:${plan.arm}:${shared?.treeId}`);
					onProgress({ phase: "rollout", iteration: 0, bestNodeScore: 0 });
					return runDreamLoop(plan.loop);
				},
			},
		});
		expect(seen).toEqual(["prepare:2", "run:fixed:shared", "run:dream:shared"]);
		expect(result.sharedInitialRollout).toBe(true);
		expect(arm(result, "dream").mode).toEqual({ proposer: "llm", dreamer: "llm", model: "faux/stub" });
		expect(arm(result, "fixed").mode).toEqual({ proposer: "llm", dreamer: "local", model: "faux/stub" });

		await expect(
			runExperimentWithRunner(
				{ ...SPEC, arms: ["dream-guided"] },
				{
					dir: scratch(),
					clock: () => FIXED_CLOCK,
					runner: createLocalArmRunner(),
					allowedArms: ["dream", "fixed"],
				},
			),
		).rejects.toBeInstanceOf(ExperimentArmUnavailableError);
	});

	it("aborts before the first arm, ends the span as aborted and writes no result", async () => {
		const dir = scratch();
		const controller = new AbortController();
		controller.abort();
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let caught: unknown;
		try {
			await runExperimentWithRunner(SPEC, {
				dir,
				clock: () => FIXED_CLOCK,
				runner: createLocalArmRunner(),
				signal: controller.signal,
			});
		} catch (error) {
			caught = error;
		} finally {
			unsubscribe();
		}
		expect(isExperimentAbortError(caught)).toBe(true);
		const experiment = spans.find((span) => span.name === "dream.experiment");
		expect(experiment?.attrs["dream.stopped"]).toBe("aborted");
		expect(experiment?.status).toBe("ok");
		expect(existsSync(join(dir, "experiments", `sum-difference-s7-n3-${FIXED_CLOCK}`, "result.json"))).toBe(false);
	});

	it("aborts between arms when the signal fires mid-experiment and records the error otherwise", async () => {
		const dir = scratch();
		const controller = new AbortController();
		let caught: unknown;
		try {
			await runExperimentWithRunner(SPEC, {
				dir,
				clock: () => FIXED_CLOCK,
				runner: createLocalArmRunner(),
				signal: controller.signal,
				onProgress: (event) => {
					if (event.type === "arm_end") controller.abort();
				},
			});
		} catch (error) {
			caught = error;
		}
		expect(isExperimentAbortError(caught)).toBe(true);
		expect(existsSync(experimentResultPath(dir, `sum-difference-s7-n3-${FIXED_CLOCK}`))).toBe(false);

		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		try {
			await expect(
				runExperimentWithRunner(SPEC, {
					dir: scratch(),
					clock: () => FIXED_CLOCK,
					runner: {
						mode: () => ({ proposer: "local", dreamer: "local" }),
						run: async () => {
							throw new Error("runner exploded");
						},
					},
				}),
			).rejects.toThrow("runner exploded");
		} finally {
			unsubscribe();
		}
		const experiment = spans.find((span) => span.name === "dream.experiment");
		expect(experiment?.status).toBe("error");
		expect(experiment?.error).toBe("runner exploded");
	});
});
