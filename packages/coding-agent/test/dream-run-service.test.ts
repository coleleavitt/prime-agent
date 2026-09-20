import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { type ExperimentArmRunner, readExperimentResult } from "../src/core/dream/experiment.js";
import { createAgentExperimentRunner } from "../src/core/dream/experiment-llm.js";
import { DREAMER_PROMPT_HEADER, GUIDANCE_PROMPT_HEADER, PROPOSER_PROMPT_HEADER } from "../src/core/dream/llm.js";
import { runDreamLoop } from "../src/core/dream/loop.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import {
	DREAM_CHILD_DEFAULTS,
	DREAM_MAX_SEEDS,
	type DreamExperimentLlmContext,
	type DreamExperimentRequest,
	type DreamRunRequest,
	DreamRunService,
	type DreamRunStatus,
	dreamChildRole,
	dreamChildScope,
	roleCappedRunAgent,
	validateDreamSeeds,
} from "../src/core/dream/run-service.js";
import type { RunAgentHandler, RunAgentOptions, RunAgentRequest, RunAgentResult } from "../src/core/run-agent.js";

/**
 * Unit coverage of DreamRunService against a STUB RunAgentHandler that throws if
 * ever invoked: the default (local) path must never call it, so any call fails
 * the test and proves the run spent tokens. Determinism flows through the
 * injected SeededRng and clock.
 */

const failingRunAgent: RunAgentHandler = async () => {
	throw new Error("runAgent must not be called on the local Dream-RSI path");
};

/** A recording stub whose every answer is rejected (not JSON), so each child call falls back locally. */
function recordingRunAgent(): {
	runAgent: RunAgentHandler;
	calls: { request: RunAgentRequest; options?: RunAgentOptions }[];
} {
	const calls: { request: RunAgentRequest; options?: RunAgentOptions }[] = [];
	const runAgent: RunAgentHandler = async (request, options) => {
		calls.push({ request, options });
		const result: RunAgentResult = {
			status: "completed",
			output: "no json here",
			messages: [],
			model: "faux/stub",
			turns: 1,
			toolCalls: 0,
			usage: {
				input: 3,
				output: 2,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 5,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
		};
		return result;
	};
	return { runAgent, calls };
}

const dirs: string[] = [];

function tempDir(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-run-service-"));
	dirs.push(dir);
	return dir;
}

afterEach(() => {
	while (dirs.length > 0) {
		const dir = dirs.pop();
		if (dir) rmSync(dir, { recursive: true, force: true });
	}
});

describe("DreamRunService", () => {
	it("runs the local zero-token path without ever calling runAgent and emits ordered phases", async () => {
		const updates: DreamRunStatus[] = [];
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			rng: createSeededRng(5),
			onUpdate: (status) => updates.push(status),
		});
		const request: DreamRunRequest = {
			task: "circle-packing",
			iterations: 1,
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 5,
		};
		const status = await service.start(request);

		expect(status.stopReason).toBe("completed");
		expect(["accepted", "stopped"]).toContain(status.phase);
		expect(status.task).toBe("circle-packing");
		expect(typeof status.finalPolicyScore).toBe("number");
		expect(typeof status.improved).toBe("boolean");
		expect(service.running).toBe(false);

		const phases = updates.map((update) => update.phase);
		expect(phases[0]).toBe("rollout");
		expect(phases).toContain("dreaming");
		expect(phases).toContain("redeploying");
		expect(updates.at(-1)?.stopReason).toBe("completed");
	});

	it("status() returns a defensive clone", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			rng: createSeededRng(1),
			onUpdate: () => {},
		});
		await service.start({ task: "circle-packing", iterations: 0, workers: 1, k1: 2, k2: 4, dreams: 1, seed: 1 });
		const first = service.status();
		const second = service.status();
		expect(first).toEqual(second);
		expect(first).not.toBe(second);
	});

	it("a cancel mid-run resolves with stopReason cancelled and never rethrows", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			rng: createSeededRng(5),
			onUpdate: () => {},
		});
		const completion = service.start({
			task: "circle-packing",
			iterations: 5,
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 5,
		});
		expect(service.cancel()).toBe(true);
		const status = await completion;
		expect(status.stopReason).toBe("cancelled");
		expect(status.phase).toBe("stopped");
		expect(status.error).toBeUndefined();
		expect(service.running).toBe(false);
	});

	it("a non-abort failure sets phase stopped with an error message and rejects", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			onUpdate: () => {},
		});
		// circle-packing requires n >= 2; resolveTask throws before the loop starts.
		await expect(service.start({ task: "circle-packing", n: 1 })).rejects.toThrow(/n >= 2/);
		expect(service.status()?.phase).toBe("stopped");
		expect(service.status()?.error).toMatch(/n >= 2/);
		expect(service.running).toBe(false);
	});

	it("is deterministic: the same seed and clock yield the same scores", async () => {
		const run = async (): Promise<DreamRunStatus> => {
			const service = new DreamRunService({
				runAgent: failingRunAgent,
				dir: tempDir(),
				now: () => 1000,
				rng: createSeededRng(9),
				onUpdate: () => {},
			});
			return service.start({ task: "circle-packing", iterations: 2, workers: 2, k1: 3, k2: 6, dreams: 2, seed: 9 });
		};
		const first = await run();
		const second = await run();
		expect(second.finalPolicyScore).toBe(first.finalPolicyScore);
		expect(second.bestNodeScore).toBe(first.bestNodeScore);
		expect(second.improved).toBe(first.improved);
	});
});

describe("DreamRunService.startExperiment", () => {
	const request: DreamExperimentRequest = {
		task: "sum-difference",
		rounds: 2,
		arms: ["dream", "fixed"],
		workers: 2,
		k1: 3,
		k2: 6,
		dreams: 2,
		seed: 5,
	};

	it("runs the local arms without ever calling runAgent and reports arm and round progress", async () => {
		const dir = tempDir();
		const updates: DreamRunStatus[] = [];
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir,
			now: () => 1000,
			onUpdate: (status) => updates.push(status),
		});
		const completion = service.startExperiment(request);
		const initial = service.status();
		expect(initial?.runId).toMatch(/^dream_/);
		expect(initial?.kind).toBe("experiment");
		expect(initial?.rounds).toBe(2);
		expect(initial?.armCount).toBe(2);
		expect(service.running).toBe(true);

		const status = await completion;
		expect(status.kind).toBe("experiment");
		expect(status.phase).toBe("stopped");
		expect(status.stopReason).toBe("completed");
		expect(status.error).toBeUndefined();
		expect(status.experimentId).toBe("sum-difference-s5-n2-1000");
		expect(status.resultPath).toBe(join(dir, "experiments", "sum-difference-s5-n2-1000", "result.json"));
		expect(existsSync(status.resultPath!)).toBe(true);
		expect(service.running).toBe(false);

		const arms = updates
			.filter((update) => update.round === 0)
			.map((update) => [update.arm, update.armIndex, update.armCount]);
		expect(arms).toEqual([
			["dream", 0, 2],
			["fixed", 1, 2],
		]);
		const rounds = updates
			.filter((update) => update.arm === "fixed" && (update.round ?? 0) > 0)
			.map((update) => update.round);
		expect(rounds).toContain(1);
		expect(rounds).toContain(2);
		expect(updates.every((update) => update.kind === "experiment")).toBe(true);
		expect(updates.at(-1)?.resultPath).toBe(status.resultPath);
		// bestNodeScore never goes down while the arms advance.
		for (let index = 1; index < updates.length; index++) {
			expect(updates[index]!.bestNodeScore).toBeGreaterThanOrEqual(updates[index - 1]!.bestNodeScore);
		}
		// The experiment lives beside the pool, never in it.
		expect(existsSync(join(dir, "trees"))).toBe(false);
		const result = readExperimentResult(dir, status.experimentId!);
		expect(result.arms.map((arm) => arm.arm)).toEqual(["dream", "fixed"]);
		expect(result.arms.every((arm) => arm.totals.tokens === 0)).toBe(true);
	});

	it("marks plain runs with kind run and shares the single slot with an experiment", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			onUpdate: () => {},
		});
		const completion = service.startExperiment(request);
		await expect(service.start({ task: "circle-packing" })).rejects.toThrow(/already running/);
		await completion;
		const run = await service.start({ task: "circle-packing", iterations: 0, workers: 1, k1: 2, k2: 4, dreams: 1 });
		expect(run.kind).toBe("run");
		expect(run.experimentId).toBeUndefined();
	});

	it("rejects guided arms without llmProposer and LLM arms without an injected runner, before any call", async () => {
		const dir = tempDir();
		const service = new DreamRunService({ runAgent: failingRunAgent, dir, now: () => 1000, onUpdate: () => {} });
		await expect(service.startExperiment({ ...request, arms: ["dream-guided"] })).rejects.toThrow(
			/require llmProposer/,
		);
		expect(service.status()?.phase).toBe("stopped");
		expect(service.status()?.error).toMatch(/llmProposer/);

		await expect(service.startExperiment({ ...request, llmProposer: true })).rejects.toThrow(/llmExperimentRunner/);
		await expect(service.startExperiment({ ...request, arms: ["dream-guided"], llmProposer: true })).rejects.toThrow(
			/llmExperimentRunner/,
		);
		expect(service.status()?.phase).toBe("stopped");
		expect(existsSync(join(dir, "experiments"))).toBe(false);
		expect(service.running).toBe(false);
	});

	it("builds LLM arms through the injected runner factory with the request's flags and the session scope", async () => {
		const dir = tempDir();
		const contexts: DreamExperimentLlmContext[] = [];
		const runnerCalls: string[] = [];
		const runner: ExperimentArmRunner = {
			mode: (arm) => ({ proposer: "llm", dreamer: arm.fixedPolicy ? "local" : "llm", model: "faux/stub" }),
			run: async (arm) => {
				runnerCalls.push(`${arm.arm}:${arm.fixedPolicy}:${arm.guided}`);
				return runDreamLoop(arm.loop);
			},
		};
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir,
			now: () => 1000,
			llmExperimentRunner: (context) => {
				contexts.push(context);
				return runner;
			},
			onUpdate: () => {},
		});
		const status = await service.startExperiment({
			...request,
			arms: ["dream", "fixed-guided"],
			llmProposer: true,
			llmDreamer: true,
		});
		expect(status.stopReason).toBe("completed");
		expect(contexts).toHaveLength(1);
		expect(contexts[0]!.useLlmProposer).toBe(true);
		expect(contexts[0]!.useLlmDreamer).toBe(true);
		// The handler is the session's, wrapped for per-role caps; a non-dream prompt passes straight through.
		await expect(contexts[0]!.runAgent({ prompt: "plain" })).rejects.toThrow(/must not be called/);
		expect(contexts[0]!.scope).toEqual({
			tools: "none",
			maxTurns: 8,
			role: "dream",
			thinkingLevel: "off",
			maxOutputTokens: DREAM_CHILD_DEFAULTS.maxOutputTokens.proposer,
		});
		expect(contexts[0]!.proposerPromptContext).toBeUndefined();
		expect(runnerCalls).toEqual(["dream:false:false", "fixed-guided:true:true"]);
		const result = readExperimentResult(dir, status.experimentId!);
		expect(result.arms.map((arm) => arm.mode.proposer)).toEqual(["llm", "llm"]);
		expect(result.arms[1]!.mode.dreamer).toBe("local");
		expect(result.arms[1]!.guided).toBe(true);
	});

	it("hands the python-speedup contract to the LLM runner as the proposer prompt context", async () => {
		// The stub runner stops before any rollout, so no python subprocess ever runs in a test.
		const contexts: DreamExperimentLlmContext[] = [];
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			llmExperimentRunner: (context) => {
				contexts.push(context);
				return {
					mode: () => ({ proposer: "llm", dreamer: "local" }),
					run: async () => {
						throw new Error("stop before rollout");
					},
				};
			},
			onUpdate: () => {},
		});
		await expect(service.startExperiment({ ...request, task: "python-speedup", llmProposer: true })).rejects.toThrow(
			"stop before rollout",
		);
		expect(contexts).toHaveLength(1);
		expect(contexts[0]!.proposerPromptContext).toContain("Contract:");
		expect(contexts[0]!.proposerPromptContext).not.toContain("hidden");
		// python-speedup answers are programs: the proposer cap doubles.
		expect(contexts[0]!.scope.maxOutputTokens).toBe(DREAM_CHILD_DEFAULTS.maxOutputTokens.proposerPythonSpeedup);
		expect(service.status()?.phase).toBe("stopped");
		expect(service.status()?.error).toBe("stop before rollout");
	});

	it("a cancel before the first arm resolves cancelled and writes no result", async () => {
		const dir = tempDir();
		const service = new DreamRunService({ runAgent: failingRunAgent, dir, now: () => 1000, onUpdate: () => {} });
		const completion = service.startExperiment({ ...request, rounds: 4 });
		expect(service.cancel()).toBe(true);
		const status = await completion;
		expect(status.stopReason).toBe("cancelled");
		expect(status.phase).toBe("stopped");
		expect(status.error).toBeUndefined();
		expect(status.resultPath).toBeUndefined();
		expect(service.running).toBe(false);
		expect(existsSync(join(dir, "experiments", "sum-difference-s5-n4-1000", "result.json"))).toBe(false);
		let leftovers: string[] = [];
		try {
			leftovers = readdirSync(join(dir, "experiments"));
		} catch {
			leftovers = [];
		}
		expect(leftovers.every((name) => !existsSync(join(dir, "experiments", name, "result.json")))).toBe(true);
	});
});

describe("DreamRunService child knobs", () => {
	it("builds the shared scope from the request's model, thinking and cap, defaulting to thinking off and the proposer cap", () => {
		expect(dreamChildScope("sum-difference", {}, "faux/session")).toEqual({
			model: "faux/session",
			tools: "none",
			maxTurns: 8,
			role: "dream",
			thinkingLevel: "off",
			maxOutputTokens: 4096,
		});
		expect(dreamChildScope("python-speedup", {}, undefined)).toMatchObject({ maxOutputTokens: 8192 });
		expect(
			dreamChildScope(
				"python-speedup",
				{ model: "faux/child", thinking: "low", maxOutputTokens: 512 },
				"faux/session",
			),
		).toEqual({
			model: "faux/child",
			tools: "none",
			maxTurns: 8,
			role: "dream",
			thinkingLevel: "low",
			maxOutputTokens: 512,
		});
	});

	it("tells the roles apart by prompt header and applies each role's default cap per call", async () => {
		expect(dreamChildRole(`${PROPOSER_PROMPT_HEADER}\nbody`)).toBe("proposer");
		expect(dreamChildRole(`${DREAMER_PROMPT_HEADER}\nbody`)).toBe("dreamer");
		expect(dreamChildRole(`${GUIDANCE_PROMPT_HEADER}\nbody`)).toBe("guidance");
		expect(dreamChildRole("# something else")).toBeUndefined();

		const stub = recordingRunAgent();
		const capped = roleCappedRunAgent(stub.runAgent, "python-speedup", {});
		const scopeOptions: RunAgentOptions = { tools: "none", maxOutputTokens: 8192, tokenBudget: 1000 };
		await capped({ prompt: `${PROPOSER_PROMPT_HEADER}\np` }, scopeOptions);
		await capped({ prompt: `${DREAMER_PROMPT_HEADER}\nd` }, scopeOptions);
		await capped({ prompt: `${GUIDANCE_PROMPT_HEADER}\ng` }, scopeOptions);
		await capped({ prompt: "unrelated" }, scopeOptions);
		expect(stub.calls.map((call) => call.options?.maxOutputTokens)).toEqual([8192, 4096, 2048, 8192]);
		expect(stub.calls.every((call) => call.options?.tokenBudget === 1000)).toBe(true);
		// An explicit cap is already on the scope for every role: nothing to wrap.
		expect(roleCappedRunAgent(stub.runAgent, "python-speedup", { maxOutputTokens: 300 })).toBe(stub.runAgent);
	});

	it("reaches every child role through the real LLM runner with thinking off and the role's cap", async () => {
		const stub = recordingRunAgent();
		const service = new DreamRunService({
			runAgent: stub.runAgent,
			dir: tempDir(),
			now: () => 1000,
			llmExperimentRunner: createAgentExperimentRunner,
			onUpdate: () => {},
		});
		const status = await service.startExperiment({
			task: "sum-difference",
			rounds: 2,
			arms: ["dream-guided"],
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 3,
			llmProposer: true,
			llmDreamer: true,
		});
		expect(status.stopReason).toBe("completed");
		expect(typeof status.tokens).toBe("number");
		const byRole = new Map<string, { request: RunAgentRequest; options?: RunAgentOptions }[]>();
		for (const call of stub.calls) {
			const role = dreamChildRole(call.request.prompt) ?? "unknown";
			byRole.set(role, [...(byRole.get(role) ?? []), call]);
		}
		expect([...byRole.keys()].sort()).toEqual(["dreamer", "guidance", "proposer"]);
		for (const call of stub.calls) expect(call.request.thinkingLevel).toBe("off");
		expect(byRole.get("proposer")!.every((call) => call.options?.maxOutputTokens === 4096)).toBe(true);
		expect(byRole.get("dreamer")!.every((call) => call.options?.maxOutputTokens === 4096)).toBe(true);
		expect(byRole.get("guidance")!.every((call) => call.options?.maxOutputTokens === 2048)).toBe(true);
		expect(stub.calls.every((call) => call.options?.tools === "none" && call.options?.maxTurns === 8)).toBe(true);
	});

	it("passes an explicit model, thinking level and cap to every role and records them in the arm mode", async () => {
		const stub = recordingRunAgent();
		const dir = tempDir();
		const service = new DreamRunService({
			runAgent: stub.runAgent,
			dir,
			now: () => 1000,
			llmExperimentRunner: createAgentExperimentRunner,
			onUpdate: () => {},
		});
		const status = await service.startExperiment({
			task: "sum-difference",
			rounds: 2,
			arms: ["dream"],
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 3,
			llmProposer: true,
			model: "faux/child",
			thinking: "low",
			maxOutputTokens: 777,
		});
		expect(stub.calls.length).toBeGreaterThan(0);
		for (const call of stub.calls) {
			expect(call.request.model).toBe("faux/child");
			expect(call.request.thinkingLevel).toBe("low");
			expect(call.options?.maxOutputTokens).toBe(777);
		}
		const result = readExperimentResult(dir, status.experimentId!);
		expect(result.arms[0]!.mode).toEqual({
			proposer: "llm",
			dreamer: "local",
			model: "faux/child",
			thinking: "low",
			maxOutputTokens: 777,
		});
	});
});

describe("DreamRunService.startExperiment seeds", () => {
	const base: DreamExperimentRequest = {
		task: "sum-difference",
		rounds: 2,
		arms: ["dream", "fixed"],
		workers: 2,
		k1: 3,
		k2: 6,
		dreams: 2,
	};

	it("validates the seed list: non-empty, distinct, non-negative integers, at most DREAM_MAX_SEEDS", () => {
		expect(validateDreamSeeds([5, 6])).toEqual([5, 6]);
		expect(() => validateDreamSeeds([])).toThrow(RangeError);
		expect(() => validateDreamSeeds([1, 1])).toThrow(/distinct/);
		expect(() => validateDreamSeeds([1, -2])).toThrow(/non-negative/);
		expect(() => validateDreamSeeds([1.5])).toThrow(/non-negative integers/);
		expect(DREAM_MAX_SEEDS).toBe(16);
		expect(validateDreamSeeds(Array.from({ length: 16 }, (_, index) => index))).toHaveLength(16);
		expect(() => validateDreamSeeds(Array.from({ length: 17 }, (_, index) => index))).toThrow(/at most 16/);
	});

	it("runs the seeds sequentially under one run id, one result.json each, and reports every path", async () => {
		const dir = tempDir();
		const updates: DreamRunStatus[] = [];
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir,
			now: () => 1000,
			onUpdate: (status) => updates.push(status),
		});
		await expect(service.startExperiment({ ...base, seed: 1, seeds: [5, 6] })).rejects.toThrow(
			/either seed or seeds/,
		);
		const completion = service.startExperiment({ ...base, seeds: [5, 6, 7] });
		const initial = service.status();
		expect(initial).toMatchObject({ seed: 5, seedIndex: 0, seedCount: 3 });
		const runId = initial!.runId;

		const status = await completion;
		expect(status.stopReason).toBe("completed");
		expect(status.runId).toBe(runId);
		expect(updates.every((update) => update.runId === runId)).toBe(true);
		const expectedPaths = [5, 6, 7].map((seed) =>
			join(dir, "experiments", `sum-difference-s${seed}-n2-1000`, "result.json"),
		);
		expect(status.resultPaths).toEqual(expectedPaths);
		expect(status.resultPath).toBe(expectedPaths[2]);
		expect(status.experimentId).toBe("sum-difference-s7-n2-1000");
		expect(status.seed).toBe(7);
		expect(status.seedIndex).toBe(2);
		expect(status.seedCount).toBe(3);
		// The local path spends nothing, so no token total is reported.
		expect(status.tokens).toBeUndefined();
		for (const path of expectedPaths) expect(existsSync(path)).toBe(true);
		// Seeds advance monotonically and each seed's arms run in order.
		const seedIndexes = updates.map((update) => update.seedIndex);
		expect([...new Set(seedIndexes)]).toEqual([0, 1, 2]);
		for (let index = 1; index < seedIndexes.length; index++) {
			expect(seedIndexes[index]!).toBeGreaterThanOrEqual(seedIndexes[index - 1]!);
		}
		const pathCounts = updates.filter((update) => update.resultPaths).map((update) => update.resultPaths!.length);
		expect([...new Set(pathCounts)]).toEqual([1, 2, 3]);
		// Seeds are independent replicates: different seeds, different round-1 trees.
		const results = [5, 6, 7].map((seed) => readExperimentResult(dir, `sum-difference-s${seed}-n2-1000`));
		expect(new Set(results.map((result) => result.arms[0]!.rounds[0]!.treeId)).size).toBe(3);
		expect(results.map((result) => result.seed)).toEqual([5, 6, 7]);
	});

	it("a cancel after the first seed keeps its result and stops before the next", async () => {
		const dir = tempDir();
		let service: DreamRunService | undefined;
		service = new DreamRunService({
			runAgent: failingRunAgent,
			dir,
			now: () => 1000,
			onUpdate: (status) => {
				if (status.resultPaths?.length === 1) service?.cancel();
			},
		});
		const status = await service.startExperiment({ ...base, seeds: [5, 6, 7] });
		expect(status.stopReason).toBe("cancelled");
		expect(status.phase).toBe("stopped");
		expect(status.resultPaths).toEqual([join(dir, "experiments", "sum-difference-s5-n2-1000", "result.json")]);
		expect(existsSync(status.resultPaths![0]!)).toBe(true);
		expect(existsSync(join(dir, "experiments", "sum-difference-s6-n2-1000", "result.json"))).toBe(false);
		expect(existsSync(join(dir, "experiments", "sum-difference-s7-n2-1000"))).toBe(false);
	});
});
