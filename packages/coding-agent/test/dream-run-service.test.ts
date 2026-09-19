import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { type ExperimentArmRunner, readExperimentResult } from "../src/core/dream/experiment.js";
import { runDreamLoop } from "../src/core/dream/loop.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import {
	type DreamExperimentLlmContext,
	type DreamExperimentRequest,
	type DreamRunRequest,
	DreamRunService,
	type DreamRunStatus,
} from "../src/core/dream/run-service.js";
import type { RunAgentHandler } from "../src/core/run-agent.js";

/**
 * Unit coverage of DreamRunService against a STUB RunAgentHandler that throws if
 * ever invoked: the default (local) path must never call it, so any call fails
 * the test and proves the run spent tokens. Determinism flows through the
 * injected SeededRng and clock.
 */

const failingRunAgent: RunAgentHandler = async () => {
	throw new Error("runAgent must not be called on the local Dream-RSI path");
};

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
		expect(contexts[0]!.runAgent).toBe(failingRunAgent);
		expect(contexts[0]!.scope).toMatchObject({ tools: "none", role: "dream" });
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
