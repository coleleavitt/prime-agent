import { AsyncLocalStorage } from "node:async_hooks";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	addSpanSink,
	installAsyncTraceContextStorage,
	type SpanEndRecord,
	type TraceContext,
	type Usage,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import { type DreamCandidateLine, type DreamStepLine, dreamsPath, readDreamsLog } from "../src/core/dream/dreams.js";
import {
	type ExperimentArm,
	type ExperimentArmResult,
	ExperimentArmUnavailableError,
	type ExperimentProgressEvent,
	type ExperimentResult,
	type ExperimentSpec,
	isExperimentAbortError,
	isExperimentResult,
	planExperiment,
	readExperimentResult,
	runExperiment,
} from "../src/core/dream/experiment.js";
import { createAgentExperimentRunner, runExperimentWithAgent } from "../src/core/dream/experiment-llm.js";
import {
	DREAMER_PROMPT_HEADER,
	GUIDANCE_PROMPT_HEADER,
	isDreamAbortError,
	PROPOSER_PROMPT_HEADER,
} from "../src/core/dream/llm.js";
import { type DreamHandlerCalls, dreamRunId, primingTreeId } from "../src/core/dream/loop.js";
import { DEFAULT_POLICY, type ExplorationPolicy, PRIMING_DIVERSE, policyId } from "../src/core/dream/policy.js";
import { totalRejected, zeroProposalTally } from "../src/core/dream/proposer.js";
import { readRejections, rejectionsPath } from "../src/core/dream/rejections.js";
import { experimentArmDir, experimentResultPath, listTrees, readTree } from "../src/core/dream/store.js";
import type { ChildRuntimeScope } from "../src/core/ravo/runtime-adapter.js";
import type { RunAgentHandler, RunAgentResult } from "../src/core/run-agent.js";

/**
 * The agent-backed experiment runner against a STUB RunAgentHandler: all four
 * arms, the shared round 1, per-arm per-role accounting, the guidance ablation,
 * the span shape and the abort path. Zero real tokens; every assertion is on
 * sum-difference (deterministic scores). python-speedup is never run here.
 */

installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const FIXED_CLOCK = 1_700_000_000_000;
const SCOPE: ChildRuntimeScope = { tools: "none", maxTurns: 2, tokenBudget: 500_000 };
const ARTIFACT = JSON.stringify({ set: [0, 1, 2, 4, 9] });
const REVISED = JSON.stringify([{ ...DEFAULT_POLICY, stopRule: "never" } satisfies ExplorationPolicy]);
const INSIGHTS = "Wider gaps between set members raised the score; consecutive runs lowered it.";
const GUIDANCE_PREFIX = "Directional insights from prior trajectories";
const SHARED = "shared";

const scratchDirs: string[] = [];

function scratch(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-experiment-llm-"));
	scratchDirs.push(dir);
	return dir;
}

afterEach(() => {
	for (const dir of scratchDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

const SPEC: ExperimentSpec = {
	task: "sum-difference",
	seed: 7,
	rounds: 2,
	budget: { workers: 3, k1: 5, k2: 10, dreams: 4 },
	arms: ["dream", "fixed", "dream-guided", "fixed-guided"],
};

function usage(totalTokens: number): Usage {
	return {
		input: totalTokens,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
}

type Role = keyof DreamHandlerCalls;
type Answer = { output?: string; status?: RunAgentResult["status"]; tokens?: number };

function roleOf(prompt: string): Role {
	if (prompt.startsWith(GUIDANCE_PROMPT_HEADER)) return "guidance";
	if (prompt.startsWith(DREAMER_PROMPT_HEADER)) return "dreamer";
	if (prompt.startsWith(PROPOSER_PROMPT_HEADER)) return "proposer";
	throw new Error(`unclassified child prompt: ${prompt.slice(0, 60)}`);
}

interface Tally {
	calls: DreamHandlerCalls;
	tokens: number;
	prompts: Record<Role, string[]>;
}

function emptyTally(): Tally {
	return {
		calls: { proposer: 0, dreamer: 0, guidance: 0 },
		tokens: 0,
		prompts: { proposer: [], dreamer: [], guidance: [] },
	};
}

/**
 * A stub handler that attributes every call to the arm currently running (the
 * shared round 1 runs before any arm starts and is tagged `shared`), so a test
 * can compare the result's per-arm accounting against what the stub really saw.
 */
function makeArmStub(answers: {
	proposer?: (call: number) => Answer;
	dreamer?: (call: number) => Answer;
	guidance?: (call: number) => Answer;
}): {
	handler: RunAgentHandler;
	onProgress: (event: ExperimentProgressEvent) => void;
	tallies: Map<string, Tally>;
	events: ExperimentProgressEvent[];
	total: () => number;
	calls: () => number;
} {
	let current = SHARED;
	let total = 0;
	let calls = 0;
	const tallies = new Map<string, Tally>();
	const events: ExperimentProgressEvent[] = [];
	const tally = (arm: string): Tally => {
		const existing = tallies.get(arm);
		if (existing) return existing;
		const fresh = emptyTally();
		tallies.set(arm, fresh);
		return fresh;
	};
	const handler: RunAgentHandler = async (request) => {
		calls += 1;
		const role = roleOf(request.prompt);
		const entry = tally(current);
		entry.calls[role] += 1;
		entry.prompts[role].push(request.prompt);
		let answer: Answer | undefined;
		if (role === "proposer") answer = answers.proposer?.(entry.calls.proposer);
		else if (role === "dreamer") answer = answers.dreamer?.(entry.calls.dreamer);
		else answer = answers.guidance?.(entry.calls.guidance) ?? { output: JSON.stringify({ insights: INSIGHTS }) };
		const tokens = answer?.tokens ?? 100;
		total += tokens;
		entry.tokens += tokens;
		return {
			status: answer?.status ?? "completed",
			output: answer?.output ?? "",
			messages: [],
			model: "faux/stub",
			turns: 1,
			toolCalls: 0,
			usage: usage(tokens),
		};
	};
	return {
		handler,
		onProgress: (event) => {
			events.push(event);
			if (event.type === "arm_start") current = event.arm;
		},
		tallies,
		events,
		total: () => total,
		calls: () => calls,
	};
}

const failingHandler: RunAgentHandler = async () => {
	throw new Error("runAgent must not be called on the local path");
};

function treeFiles(dir: string, treeId: string): Record<string, string> {
	const files: Record<string, string> = { tree: readFileSync(join(dir, "trees", `${treeId}.jsonl`), "utf8") };
	const blobDir = join(dir, "trees", treeId, "blobs");
	for (const name of readdirSync(blobDir).sort()) files[`blob/${name}`] = readFileSync(join(blobDir, name), "utf8");
	return files;
}

function arm(result: ExperimentResult, name: ExperimentArm): ExperimentArmResult {
	const found = result.arms.find((candidate) => candidate.arm === name);
	if (!found) throw new Error(`no arm ${name}`);
	return found;
}

async function captureSpans<T>(run: () => Promise<T>): Promise<{ value: T; spans: SpanEndRecord[] }> {
	const spans: SpanEndRecord[] = [];
	const unsubscribe = addSpanSink((record) => spans.push(record));
	try {
		return { value: await run(), spans };
	} finally {
		unsubscribe();
	}
}

function llmAnswers(): Parameters<typeof makeArmStub>[0] {
	return {
		proposer: () => ({ output: ARTIFACT, tokens: 10 }),
		dreamer: () => ({ output: REVISED, tokens: 200 }),
		guidance: () => ({ output: JSON.stringify({ insights: INSIGHTS }), tokens: 70 }),
	};
}

describe("runExperimentWithAgent (stub handler, four arms)", () => {
	it("shares round 1 across every arm and attributes each arm's calls and tokens to its own rounds", async () => {
		const dir = scratch();
		const stub = makeArmStub(llmAnswers());
		const result = await runExperimentWithAgent(SPEC, {
			dir,
			clock: () => FIXED_CLOCK,
			runAgent: stub.handler,
			scope: SCOPE,
			signal: new AbortController().signal,
			useLlmProposer: true,
			useLlmDreamer: true,
			onProgress: stub.onProgress,
		});
		expect(isExperimentResult(result)).toBe(true);
		expect(readExperimentResult(dir, result.experimentId)).toEqual(result);
		expect(result.sharedInitialRollout).toBe(true);
		expect(result.arms.map((a) => a.arm)).toEqual([...SPEC.arms]);
		expect(result.arms.every((a) => a.rounds.length === 2)).toBe(true);
		expect(result.arms.every((a) => a.mode.proposer === "llm" && a.mode.dreamer === "llm")).toBe(true);
		expect(result.headline?.reference).toBe("fixed");
		expect(existsSync(join(dir, "trees"))).toBe(false);

		// Round 1: one record, one tree, copied byte for byte into every arm's store, with its exact curve.
		const first = result.arms[0]!.rounds[0]!;
		const firstFiles = treeFiles(experimentArmDir(dir, result.experimentId, "dream"), first.treeId);
		expect(Object.keys(firstFiles).length).toBeGreaterThan(1);
		expect(first.probesToRoundBest).toBeLessThanOrEqual(first.probes);
		expect(first.improvements!.at(-1)!.score).toBe(first.roundBest);
		expect(first.primingTreeIds).toBeUndefined();
		for (const a of result.arms) {
			expect(a.rounds[0]).toEqual(first);
			expect(treeFiles(experimentArmDir(dir, result.experimentId, a.arm), first.treeId)).toEqual(firstFiles);
			expect(listTrees(experimentArmDir(dir, result.experimentId, a.arm))).toHaveLength(2);
			expect(a.rounds[1]!.improvements).toBeDefined();
			expect(a.stoppedEarly).toBeDefined();
		}
		// Distinct run ids per arm under the one frozen clock; the exact headline is recorded for every arm.
		expect(new Set(result.arms.map((a) => a.runId)).size).toBe(result.arms.length);
		expect(result.arms.every((a) => a.runId.endsWith(`-${result.experimentId}_${a.arm}`))).toBe(true);
		for (const a of result.arms) expect(typeof result.headline!.probesToTargetExact[a.arm]).toBe("number");
		const shared = stub.tallies.get(SHARED)!;
		expect(shared.calls).toEqual({ proposer: first.handlerCalls.proposer, dreamer: 0, guidance: 0 });
		expect(shared.calls.proposer).toBe(first.probes);
		expect(shared.tokens).toBe(first.tokens);
		// Every stub answer was accepted: the shared round is entirely agent-generated, nothing fell back.
		expect(first.agentGeneratedCalls).toBe(first.probes);
		expect(first.llmProposals).toBe(first.probes);
		expect(first.llmAccepted).toBe(first.probes);
		expect(first.localFallbacks).toBe(0);
		expect(first.llmRejected).toEqual(zeroProposalTally().llmRejected);
		expect(
			existsSync(
				rejectionsPath(experimentArmDir(dir, result.experimentId, "dream"), `${result.experimentId}-shared`),
			),
		).toBe(false);
		for (const a of result.arms) {
			expect(a.totals.agentGeneratedCalls).toBe(a.totals.probes);
			expect(a.totals.localFallbacks).toBe(0);
			expect(a.rounds.every((row) => row.agentGeneratedCalls === row.llmAccepted)).toBe(true);
		}
		// Round 1 cost the stub exactly one rollout's worth, however many arms ran.
		const roundOneCalls = [...stub.tallies.entries()]
			.filter(([name]) => name !== SHARED)
			.reduce((sum, [, tally]) => sum + tally.calls.proposer, 0);
		expect(stub.calls()).toBe(
			shared.calls.proposer + roundOneCalls + sumRole(stub, "dreamer") + sumRole(stub, "guidance"),
		);

		// Every arm's round-2 record is exactly what the stub saw while that arm ran.
		for (const a of result.arms) {
			const tally = stub.tallies.get(a.arm)!;
			expect(tally, `${a.arm} ran`).toBeDefined();
			expect(a.rounds[1]!.handlerCalls).toEqual(tally.calls);
			expect(a.rounds[1]!.tokens).toBe(tally.tokens);
			expect(a.totals.tokens).toBe(shared.tokens + tally.tokens);
			expect(a.totals.handlerCalls).toBe(
				shared.calls.proposer + tally.calls.proposer + tally.calls.dreamer + tally.calls.guidance,
			);
		}

		// Fixed arms never dream; dreaming arms record the step and its LLM dreamer call.
		for (const name of ["fixed", "fixed-guided"] as const) {
			const fixed = arm(result, name);
			expect(fixed.fixedPolicy).toBe(true);
			expect(fixed.rounds.every((row) => row.dreaming === null)).toBe(true);
			expect(fixed.finalPolicyId).toBe(fixed.initialPolicyId);
			expect(fixed.policyChanges).toBe(0);
			expect(stub.tallies.get(name)!.calls.dreamer).toBe(0);
		}
		for (const name of ["dream", "dream-guided"] as const) {
			const dreaming = arm(result, name);
			expect(dreaming.fixedPolicy).toBe(false);
			expect(dreaming.rounds[1]!.dreaming).not.toBeNull();
			expect(dreaming.rounds[1]!.dreaming!.candidates).toBeGreaterThan(0);
			expect(stub.tallies.get(name)!.calls.dreamer).toBe(1);
		}

		// Guidance: one writer call in round 2 of a guided arm, its insights only in that arm's later prompts.
		for (const a of result.arms) {
			const tally = stub.tallies.get(a.arm)!;
			expect(a.guided).toBe(a.arm.endsWith("-guided"));
			expect(tally.calls.guidance).toBe(a.guided ? 1 : 0);
			expect(a.rounds[1]!.handlerCalls.guidance).toBe(a.guided ? 1 : 0);
			expect(tally.prompts.proposer.length).toBeGreaterThan(0);
			expect(tally.prompts.proposer.every((prompt) => prompt.includes(GUIDANCE_PREFIX))).toBe(a.guided);
			expect(tally.prompts.proposer.every((prompt) => prompt.includes(INSIGHTS))).toBe(a.guided);
		}
		expect(shared.prompts.proposer.some((prompt) => prompt.includes(GUIDANCE_PREFIX))).toBe(false);
		// sum-difference has no task prompt context, so no proposer prompt carries a contract block.
		const everyProposerPrompt = [...stub.tallies.values()].flatMap((tally) => tally.prompts.proposer);
		expect(everyProposerPrompt.length).toBeGreaterThan(0);
		expect(everyProposerPrompt.some((prompt) => prompt.includes("Contract:"))).toBe(false);

		// Progress: one arm_start per arm in order, rounds after each arm, completed last.
		const starts = stub.events
			.filter((event) => event.type === "arm_start")
			.map((event) => event.type === "arm_start" && event.arm);
		expect(starts).toEqual([...SPEC.arms]);
		expect(stub.events.at(-1)?.type).toBe("completed");
	});

	it("reports rejected child results apart from agent-generated candidates, per round and in the rejection logs", async () => {
		const dir = scratch();
		// Every other proposer result has the wrong shape: rejected, retried, then a local fallback.
		let proposerCalls = 0;
		const stub = makeArmStub({
			...llmAnswers(),
			proposer: () => {
				proposerCalls += 1;
				return proposerCalls % 3 === 1
					? { output: ARTIFACT, tokens: 10 }
					: { output: JSON.stringify({ notASet: true }), tokens: 10 };
			},
		});
		const result = await runExperimentWithAgent(
			{ ...SPEC, arms: ["fixed", "dream"] },
			{
				dir,
				clock: () => FIXED_CLOCK,
				runAgent: stub.handler,
				scope: SCOPE,
				signal: new AbortController().signal,
				useLlmProposer: true,
				useLlmDreamer: false,
				onProgress: stub.onProgress,
			},
		);
		const first = result.arms[0]!.rounds[0]!;
		expect(first.llmProposals).toBe(first.llmAccepted + totalRejected(first));
		expect(first.probes).toBe(first.llmAccepted + first.localFallbacks);
		expect(first.agentGeneratedCalls).toBe(first.llmAccepted);
		expect(first.localFallbacks).toBeGreaterThan(0);
		expect(first.llmRejected.shape).toBe(totalRejected(first));
		expect(first.handlerCalls.proposer).toBe(first.llmProposals);
		// The shared round's rejections are logged once, under the first arm's store.
		const firstDir = experimentArmDir(dir, result.experimentId, "fixed");
		const sharedLog = readRejections(rejectionsPath(firstDir, `${result.experimentId}-shared`));
		expect(sharedLog).toHaveLength(totalRejected(first));
		expect(sharedLog.filter((record) => record.fellBack)).toHaveLength(first.localFallbacks);
		expect(sharedLog.every((record) => record.iteration === 0 && record.reason === "shape")).toBe(true);
		expect(
			existsSync(
				rejectionsPath(experimentArmDir(dir, result.experimentId, "dream"), `${result.experimentId}-shared`),
			),
		).toBe(false);
		for (const a of result.arms) {
			const second = a.rounds[1]!;
			expect(second.llmProposals).toBe(second.llmAccepted + totalRejected(second));
			expect(second.probes).toBe(second.llmAccepted + second.localFallbacks);
			expect(second.agentGeneratedCalls).toBe(second.llmAccepted);
			expect(second.cumulativeAgentGeneratedCalls).toBe(first.agentGeneratedCalls + second.agentGeneratedCalls);
			expect(a.totals.agentGeneratedCalls).toBe(first.agentGeneratedCalls + second.agentGeneratedCalls);
			expect(a.totals.localFallbacks).toBe(first.localFallbacks + second.localFallbacks);
			expect(a.totals.llmProposals).toBe(first.llmProposals + second.llmProposals);
			expect(a.totals.agentGeneratedCalls).toBeLessThan(a.totals.probes);
			// The arm's own loop logs its later rounds under its labelled run key, in its own store.
			expect(a.runId).toBe(dreamRunId("sum-difference", 7, FIXED_CLOCK, `${result.experimentId}/${a.arm}`));
			const armLog = readRejections(rejectionsPath(experimentArmDir(dir, result.experimentId, a.arm), a.runId));
			expect(armLog).toHaveLength(totalRejected(second));
			expect(armLog.every((record) => record.iteration === 1)).toBe(true);
			// The persisted trees carry the same split.
			const tree = readTree(second.treeId, experimentArmDir(dir, result.experimentId, a.arm));
			expect(tree.nodes.filter((node) => node.origin === "llm")).toHaveLength(second.agentGeneratedCalls);
			expect(tree.nodes.filter((node) => node.origin === "local")).toHaveLength(second.localFallbacks);
		}
		expect(isExperimentResult(readExperimentResult(dir, result.experimentId))).toBe(true);
	});

	it("with the local proposer and dreamer never calls the handler and matches the sync runner arm for arm", async () => {
		const syncDir = scratch();
		const agentDir = scratch();
		const localSpec: ExperimentSpec = { ...SPEC, rounds: 3, arms: ["fixed", "dream"] };
		const expected = runExperiment(localSpec, { dir: syncDir, clock: () => FIXED_CLOCK });
		const { value: result, spans } = await captureSpans(() =>
			runExperimentWithAgent(localSpec, {
				dir: agentDir,
				clock: () => FIXED_CLOCK,
				runAgent: failingHandler,
				scope: SCOPE,
				signal: new AbortController().signal,
				useLlmProposer: false,
				useLlmDreamer: false,
			}),
		);
		expect(result.sharedInitialRollout).toBe(true);
		expect(JSON.stringify(result.arms)).toBe(JSON.stringify(expected.arms));
		expect(JSON.stringify(result.headline)).toBe(JSON.stringify(expected.headline));
		for (const name of localSpec.arms) {
			const treeId = arm(result, name).rounds[0]!.treeId;
			expect(treeFiles(experimentArmDir(agentDir, result.experimentId, name), treeId)).toEqual(
				treeFiles(experimentArmDir(syncDir, expected.experimentId, name), treeId),
			);
		}
		expect(spans.find((span) => span.name === "dream.experiment")!.attrs["dream.mode"]).toBe("local");
		// Turning the shared rollout off gives each arm its own round 1, still identical on the local path.
		const own = await runExperimentWithAgent(localSpec, {
			dir: scratch(),
			clock: () => FIXED_CLOCK,
			runAgent: failingHandler,
			scope: SCOPE,
			signal: new AbortController().signal,
			useLlmProposer: false,
			useLlmDreamer: false,
			shareInitialRollout: false,
		});
		expect(own.sharedInitialRollout).toBe(false);
		expect(JSON.stringify(own.arms)).toBe(JSON.stringify(expected.arms));
	});

	it("rejects a guided arm without the LLM proposer before creating anything, in both entry points", async () => {
		const dir = scratch();
		const stub = makeArmStub(llmAnswers());
		await expect(
			runExperimentWithAgent(
				{ ...SPEC, arms: ["fixed", "dream-guided"] },
				{
					dir,
					clock: () => FIXED_CLOCK,
					runAgent: stub.handler,
					scope: SCOPE,
					signal: new AbortController().signal,
					useLlmProposer: false,
					useLlmDreamer: true,
				},
			),
		).rejects.toBeInstanceOf(ExperimentArmUnavailableError);
		expect(stub.calls()).toBe(0);
		expect(existsSync(join(dir, "experiments"))).toBe(false);

		const runner = createAgentExperimentRunner({
			runAgent: stub.handler,
			scope: SCOPE,
			signal: new AbortController().signal,
			useLlmProposer: false,
			useLlmDreamer: false,
		});
		const plan = planExperiment({ ...SPEC, arms: ["fixed-guided"] }, { dir, clock: () => FIXED_CLOCK });
		await expect(runner.prepare!(plan)).rejects.toThrow(/fixed-guided require useLlmProposer/);
		expect(stub.calls()).toBe(0);
		expect(runner.mode(plan.arms[0]!)).toEqual({ proposer: "local", dreamer: "local" });
	});

	it("passes the proposer prompt context and the child model through to every proposer call", async () => {
		const stub = makeArmStub(llmAnswers());
		const result = await runExperimentWithAgent(
			{ ...SPEC, arms: ["fixed"] },
			{
				dir: scratch(),
				clock: () => FIXED_CLOCK,
				runAgent: stub.handler,
				scope: { ...SCOPE, model: "faux/stub-model" },
				signal: new AbortController().signal,
				useLlmProposer: true,
				useLlmDreamer: false,
				proposerPromptContext: "Contract: public examples only.",
				onProgress: stub.onProgress,
			},
		);
		expect(result.arms[0]!.mode).toEqual({ proposer: "llm", dreamer: "local", model: "faux/stub-model" });
		const prompts = [...stub.tallies.values()].flatMap((tally) => tally.prompts.proposer);
		expect(prompts.length).toBe(result.arms[0]!.totals.probes);
		expect(prompts.every((prompt) => prompt.includes("Contract: public examples only."))).toBe(true);
		expect(result.arms[0]!.totals.tokens).toBe(stub.total());
	});

	it("links the spans: a detached-root experiment carrying the turn, detached-root arm runs carrying the experiment", async () => {
		const dir = scratch();
		const stub = makeArmStub(llmAnswers());
		const { value: result, spans } = await captureSpans(() =>
			withSpan("test.turn", {}, () =>
				runExperimentWithAgent(
					{ ...SPEC, arms: ["fixed", "dream-guided"] },
					{
						dir,
						clock: () => FIXED_CLOCK,
						runAgent: stub.handler,
						scope: SCOPE,
						signal: new AbortController().signal,
						useLlmProposer: true,
						useLlmDreamer: true,
						onProgress: stub.onProgress,
					},
				),
			),
		);
		const turn = spans.find((span) => span.name === "test.turn")!;
		const experiment = spans.find((span) => span.name === "dream.experiment")!;
		expect(experiment.parentSpanId).toBeUndefined();
		expect(experiment.traceId).not.toBe(turn.traceId);
		expect(experiment.attrs["trigger.trace_id"]).toBe(turn.traceId);
		expect(experiment.attrs["dream.mode"]).toBe("llm");
		expect(experiment.attrs["dream.arms"]).toBe("fixed,dream-guided");
		expect(experiment.status).toBe("ok");

		const experimentTrace = spans.filter((span) => span.traceId === experiment.traceId);
		const experimentIds = new Set(experimentTrace.map((span) => span.spanId));
		for (const span of experimentTrace) {
			if (span.name === "dream.experiment") continue;
			expect(span.parentSpanId, `${span.name} must have a parent`).toBeDefined();
			expect(experimentIds.has(span.parentSpanId!), `${span.name} parent must be in the trace`).toBe(true);
		}
		const armSpans = experimentTrace.filter((span) => span.name === "dream.experiment_arm");
		expect(armSpans).toHaveLength(2);
		expect(armSpans.every((span) => span.parentSpanId === experiment.spanId)).toBe(true);
		expect(armSpans.map((span) => span.attrs["dream.fixed_policy"])).toEqual([true, false]);
		expect(armSpans.map((span) => span.attrs["dream.guided"])).toEqual([false, true]);
		expect(armSpans.every((span) => typeof span.attrs["dream.run_id"] === "string")).toBe(true);
		// The shared round 1 is the experiment's own child; the arm loops adopt it.
		const sharedExplore = experimentTrace.filter((span) => span.name === "dream.explore");
		expect(sharedExplore).toHaveLength(1);
		expect(sharedExplore[0]!.parentSpanId).toBe(experiment.spanId);
		expect(sharedExplore[0]!.attrs["dream.tree_id"]).toBe(result.arms[0]!.rounds[0]!.treeId);

		const runs = spans.filter((span) => span.name === "dream.run");
		expect(runs).toHaveLength(2);
		for (const run of runs) {
			expect(run.parentSpanId).toBeUndefined();
			expect(run.traceId).not.toBe(experiment.traceId);
			expect(run.attrs["trigger.trace_id"]).toBe(experiment.traceId);
			const trace = spans.filter((span) => span.traceId === run.traceId);
			const ids = new Set(trace.map((span) => span.spanId));
			for (const span of trace) {
				if (span.name === "dream.run") continue;
				expect(span.parentSpanId, `${span.name} must have a parent`).toBeDefined();
				expect(ids.has(span.parentSpanId!), `${span.name} parent must be in the run trace`).toBe(true);
			}
			// No round-1 rollout inside an arm: the only explore is the one the redeploy wraps.
			const redeploys = trace.filter((span) => span.name === "dream.redeploy");
			const explores = trace.filter((span) => span.name === "dream.explore");
			expect(redeploys).toHaveLength(1);
			expect(explores).toHaveLength(1);
			expect(explores[0]!.parentSpanId).toBe(redeploys[0]!.spanId);
			expect(explores[0]!.attrs["dream.iteration"]).toBe(1);
		}
		const fixedRun = runs.find((span) => span.attrs["dream.fixed_policy"] === true)!;
		const guidedRun = runs.find((span) => span.attrs["dream.fixed_policy"] === false)!;
		expect(
			spans.filter((span) => span.traceId === fixedRun.traceId && span.name === "dream.llm_guidance"),
		).toHaveLength(0);
		expect(
			spans.filter((span) => span.traceId === guidedRun.traceId && span.name === "dream.llm_guidance"),
		).toHaveLength(1);
		expect(
			spans.filter((span) => span.traceId === guidedRun.traceId && span.name === "dream.llm_dream"),
		).toHaveLength(1);
	});

	it("writes no result and marks the experiment aborted on a pre-aborted signal or an aborted child", async () => {
		const pre = scratch();
		const controller = new AbortController();
		controller.abort();
		const stub = makeArmStub(llmAnswers());
		const { value: caught, spans } = await captureSpans(async () => {
			try {
				await runExperimentWithAgent(SPEC, {
					dir: pre,
					clock: () => FIXED_CLOCK,
					runAgent: stub.handler,
					scope: SCOPE,
					signal: controller.signal,
					useLlmProposer: true,
					useLlmDreamer: true,
				});
				return undefined;
			} catch (error) {
				return error;
			}
		});
		expect(isExperimentAbortError(caught) || isDreamAbortError(caught)).toBe(true);
		expect(stub.calls()).toBe(0);
		expect(spans.find((span) => span.name === "dream.experiment")!.attrs["dream.stopped"]).toBe("aborted");
		expect(existsSync(experimentResultPath(pre, `sum-difference-s7-n2-${FIXED_CLOCK}`))).toBe(false);

		// A child that reports `aborted` mid-way (here: in the second arm) stops the whole experiment.
		const mid = scratch();
		let armsStarted = 0;
		const aborting = makeArmStub({
			...llmAnswers(),
			proposer: () => (armsStarted >= 2 ? { status: "aborted", tokens: 1 } : { output: ARTIFACT, tokens: 10 }),
		});
		const { value: midCaught, spans: midSpans } = await captureSpans(async () => {
			try {
				await runExperimentWithAgent(SPEC, {
					dir: mid,
					clock: () => FIXED_CLOCK,
					runAgent: aborting.handler,
					scope: SCOPE,
					signal: new AbortController().signal,
					useLlmProposer: true,
					useLlmDreamer: true,
					onProgress: (event) => {
						aborting.onProgress(event);
						if (event.type === "arm_start") armsStarted += 1;
					},
				});
				return undefined;
			} catch (error) {
				return error;
			}
		});
		expect(isDreamAbortError(midCaught)).toBe(true);
		expect(aborting.tallies.get("dream")).toBeDefined();
		expect(aborting.tallies.get("dream-guided")).toBeUndefined();
		const experiment = midSpans.find((span) => span.name === "dream.experiment")!;
		expect(experiment.attrs["dream.stopped"]).toBe("aborted");
		expect(experiment.status).toBe("ok");
		// Both arms that started ended their detached-root run: the first cleanly, the second as aborted.
		const midRuns = midSpans.filter((span) => span.name === "dream.run");
		expect(midRuns).toHaveLength(2);
		expect(midRuns.map((span) => span.attrs["dream.stopped"])).toEqual([undefined, "aborted"]);
		expect(existsSync(experimentResultPath(mid, `sum-difference-s7-n2-${FIXED_CLOCK}`))).toBe(false);
	});
});

describe("runExperimentWithAgent: priming, verdicts and the dreams log", () => {
	it("shares the priming rollouts with round 1 of every arm and charges their probes and calls to it", async () => {
		const dir = scratch();
		const plainDir = scratch();
		const spec: ExperimentSpec = { ...SPEC, arms: ["fixed", "dream"] };
		const plainStub = makeArmStub(llmAnswers());
		const plain = await runExperimentWithAgent(spec, {
			dir: plainDir,
			clock: () => FIXED_CLOCK,
			runAgent: plainStub.handler,
			scope: SCOPE,
			signal: new AbortController().signal,
			useLlmProposer: true,
			useLlmDreamer: true,
			onProgress: plainStub.onProgress,
		});
		const stub = makeArmStub(llmAnswers());
		const result = await runExperimentWithAgent(
			{ ...spec, primingPolicies: PRIMING_DIVERSE },
			{
				dir,
				clock: () => FIXED_CLOCK,
				runAgent: stub.handler,
				scope: SCOPE,
				signal: new AbortController().signal,
				useLlmProposer: true,
				useLlmDreamer: true,
				onProgress: stub.onProgress,
			},
		);
		const first = result.arms[0]!.rounds[0]!;
		const primingIds = PRIMING_DIVERSE.map((_, index) => primingTreeId("sum-difference", 7, index, FIXED_CLOCK));
		expect(first.treeId).toBe(plain.arms[0]!.rounds[0]!.treeId);
		expect(first.primingTreeIds).toEqual(primingIds);
		expect(first.primingProbes).toBeGreaterThan(0);
		expect(first.probes).toBe(plain.arms[0]!.rounds[0]!.probes + first.primingProbes!);
		// Every stub answer was accepted: the shared round's calls and agent-generated probes cover the priming too.
		const shared = stub.tallies.get(SHARED)!;
		expect(shared.calls.proposer).toBe(first.probes);
		expect(first.handlerCalls.proposer).toBe(first.probes);
		expect(first.agentGeneratedCalls).toBe(first.probes);
		expect(first.llmAccepted).toBe(first.probes);
		expect(first.tokens).toBe(first.probes * 10);
		expect(first.improvements!.at(-1)!.score).toBe(first.roundBest);
		expect(first.roundBest).toBeGreaterThanOrEqual(plain.arms[0]!.rounds[0]!.roundBest);
		for (const a of result.arms) {
			expect(a.rounds[0]).toEqual(first);
			const armDir = experimentArmDir(dir, result.experimentId, a.arm);
			expect(
				listTrees(armDir)
					.map((tree) => tree.treeId)
					.sort(),
			).toEqual([first.treeId, ...primingIds, a.rounds[1]!.treeId].sort());
			for (const treeId of primingIds) {
				expect(treeFiles(armDir, treeId)).toEqual(
					treeFiles(experimentArmDir(dir, result.experimentId, "fixed"), treeId),
				);
			}
			// The dreaming arm's first pool held the shared tree and both priming trees.
			expect(a.rounds[1]!.poolSize).toBe(1 + PRIMING_DIVERSE.length);
			expect(a.totals.probes).toBe(first.probes + a.rounds[1]!.probes);
		}
		expect(stub.tallies.get("dream")!.calls.dreamer).toBe(1);
	});

	it("runs autocorrelation n=32 with a stub handler, recording candidate verdicts, the final selection and a dreams log", async () => {
		const dir = scratch();
		const weights = Array.from({ length: 32 }, (_, index) => 1 + 0.05 * Math.abs(16 - index));
		const revised = JSON.stringify([
			{ ...DEFAULT_POLICY, stopRule: "never" } satisfies ExplorationPolicy,
			{ ...DEFAULT_POLICY, selectionRule: "explore-root" } satisfies ExplorationPolicy,
		]);
		const stub = makeArmStub({
			proposer: () => ({ output: JSON.stringify({ n: 32, weights }), tokens: 12 }),
			dreamer: () => ({ output: revised, tokens: 300 }),
		});
		const spec: ExperimentSpec = {
			task: "autocorrelation",
			n: 32,
			seed: 7,
			rounds: 2,
			budget: { workers: 3, k1: 6, k2: 12, dreams: 4 },
			arms: ["fixed", "dream"],
		};
		const result = await runExperimentWithAgent(spec, {
			dir,
			clock: () => FIXED_CLOCK,
			runAgent: stub.handler,
			scope: SCOPE,
			signal: new AbortController().signal,
			useLlmProposer: true,
			useLlmDreamer: true,
			onProgress: stub.onProgress,
		});
		expect(result.task).toBe("autocorrelation");
		expect(result.n).toBe(32);
		expect(result.notes.some((note) => note.startsWith("k1 6 <= initialPolicy.beta 6"))).toBe(true);
		expect(isExperimentResult(readExperimentResult(dir, result.experimentId))).toBe(true);
		// The proposer's prompt carries the exact bin count; every stub answer was accepted.
		const prompts = [...stub.tallies.values()].flatMap((tally) => tally.prompts.proposer);
		expect(prompts.every((prompt) => prompt.includes("exactly 32 weights"))).toBe(true);
		expect(result.arms.every((a) => a.totals.localFallbacks === 0)).toBe(true);

		const dream = arm(result, "dream");
		const step = dream.rounds[1]!.dreaming!;
		// Two child policies plus two local top-ups fill M = 4: a mixed step with one verdict per candidate.
		expect(step.candidates).toBe(4);
		expect(step.dreamer).toBe("mixed");
		expect(step.candidateVerdicts).toHaveLength(4);
		expect(step.candidateVerdicts!.map((verdict) => verdict.origin)).toEqual(["llm", "llm", "local", "local"]);
		expect(step.candidateVerdicts!.map((verdict) => verdict.index)).toEqual([0, 1, 2, 3]);
		expect(
			step.candidateVerdicts!.every((verdict) => typeof verdict.value === "number" && verdict.changed.length > 0),
		).toBe(true);
		expect(step.candidateVerdicts!.filter((verdict) => verdict.reason === "winner")).toHaveLength(
			step.improved ? 1 : 0,
		);
		expect(step.leverScan).not.toBeNull();
		expect(step.leverScan!.gap).toBeGreaterThanOrEqual(0);
		expect(dream.finalSelection).toHaveLength(1);
		expect(dream.finalSelection![0]!.policyId).toBe(dream.rounds[1]!.policyId);
		expect(dream.stoppedEarly).toBe(0);
		expect(arm(result, "fixed").finalSelection).toEqual([]);
		expect(arm(result, "fixed").rounds.every((row) => row.dreaming === null)).toBe(true);

		// The dreams log of the dream arm: four candidate lines and a step line for iteration 1, then the final selection.
		const dreamDir = experimentArmDir(dir, result.experimentId, "dream");
		const lines = readDreamsLog(dreamsPath(dreamDir, dream.runId));
		expect(lines.map((line) => [line.type, line.iteration])).toEqual([
			["candidate", 1],
			["candidate", 1],
			["candidate", 1],
			["candidate", 1],
			["step", 1],
			["candidate", -1],
			["step", -1],
		]);
		expect(lines.every((line) => line.experimentId === result.experimentId && line.arm === "dream")).toBe(true);
		const stepLine = lines[4] as DreamStepLine;
		expect(stepLine.dreamer).toBe("mixed");
		expect(stepLine.poolSize).toBe(1);
		expect(stepLine.chosenPolicyId).toBe(dream.rounds[1]!.policyId);
		expect(stepLine.improved).toBe(step.improved);
		expect((lines[0] as DreamCandidateLine).policyId).toBe(step.candidateVerdicts![0]!.policyId);
		// The fixed arm never dreamed: its log holds only the post-hoc final selection over {initial}.
		const fixedLines = readDreamsLog(
			dreamsPath(experimentArmDir(dir, result.experimentId, "fixed"), arm(result, "fixed").runId),
		);
		expect(fixedLines.map((line) => [line.type, line.iteration])).toEqual([["step", -1]]);
	});
});

function sumRole(stub: ReturnType<typeof makeArmStub>, role: Role): number {
	let sum = 0;
	for (const tally of stub.tallies.values()) sum += tally.calls[role];
	return sum;
}

describe("createAgentExperimentRunner", () => {
	it("reports the configured modes identically for every arm and round-trips the initial policy id", async () => {
		const dir = scratch();
		const stub = makeArmStub(llmAnswers());
		const runner = createAgentExperimentRunner({
			runAgent: stub.handler,
			scope: SCOPE,
			signal: new AbortController().signal,
			useLlmProposer: true,
			useLlmDreamer: false,
		});
		const plan = planExperiment(SPEC, { dir, clock: () => FIXED_CLOCK });
		expect(plan.arms.map((a) => runner.mode(a))).toEqual(
			plan.arms.map(() => ({ proposer: "llm", dreamer: "local" })),
		);
		const shared = await runner.prepare!(plan);
		expect(shared).toBeDefined();
		expect(shared!.treeId).toBe(`sum-difference-s7-i0-${FIXED_CLOCK}`);
		expect(shared!.handlerCalls).toEqual({ proposer: shared!.revealedCount, dreamer: 0, guidance: 0 });
		expect(shared!.tokens).toBe(shared!.revealedCount * 10);
		for (const a of plan.arms) expect(listTrees(a.dir).map((tree) => tree.treeId)).toEqual([shared!.treeId]);
		const loop = await runner.run(plan.arms[1]!, shared, () => {});
		expect(loop.fixedPolicy).toBe(true);
		expect(loop.initialPolicyId).toBe(policyId(DEFAULT_POLICY));
		expect(loop.treeIds[0]).toBe(shared!.treeId);
		expect(loop.rounds[0]!.handlerCalls).toEqual(shared!.handlerCalls);
	});
});
