import type { Usage } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import {
	createRetainedWorkerChildCall,
	createRunAgentChildCall,
	type RetainedWorkerRuntime,
} from "../src/core/ravo/runtime-adapter.js";
import type { RunAgentHandler, RunAgentResult, RunAgentStatus } from "../src/core/run-agent.js";

const usage = (totalTokens: number): Usage => ({
	input: totalTokens,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});

function result(output: string, status: RunAgentStatus = "completed"): RunAgentResult {
	return {
		status,
		output,
		messages: [],
		model: "faux/child",
		turns: 1,
		toolCalls: 0,
		usage: usage(7),
	};
}

function validateAnswer(value: unknown): { answer: number } {
	if (typeof value !== "object" || value === null || !("answer" in value) || typeof value.answer !== "number") {
		throw new Error("answer must be a number");
	}
	return { answer: value.answer };
}

const callOptions = (tokenBudget = 50) => ({
	signal: new AbortController().signal,
	tokenBudget,
});

describe("RAVO runtime adapters", () => {
	it("passes the caller prompt and execution scope to RunAgent and validates exact JSON", async () => {
		const runAgent = vi.fn<RunAgentHandler>(async () => result('{"answer":42}'));
		const call = createRunAgentChildCall(runAgent, {
			prompt: ({ role }: { role: string }) => `caller-defined ${role} prompt`,
			validate: validateAnswer,
			scope: {
				model: "faux/child",
				tools: { allow: ["read"] },
				maxTurns: 3,
				tokenBudget: 20,
			},
		});

		await expect(call({ role: "inspect" }, callOptions())).resolves.toEqual({
			status: "completed",
			value: { answer: 42 },
			tokens: 7,
		});
		expect(runAgent).toHaveBeenCalledWith(
			{ prompt: "caller-defined inspect prompt", model: "faux/child" },
			expect.objectContaining({
				tools: { allow: ["read"] },
				maxTurns: 3,
				tokenBudget: 20,
			}),
		);
	});

	it("rejects prose, fenced JSON, and schema-invalid output instead of scraping it", async () => {
		for (const output of ['result: {"answer":42}', '```json\n{"answer":42}\n```', '{"answer":"42"}']) {
			const call = createRunAgentChildCall(async () => result(output), {
				prompt: () => "evaluate exactly",
				validate: validateAnswer,
			});
			await expect(call({}, callOptions())).resolves.toMatchObject({
				status: "error",
				tokens: 7,
			});
		}
	});

	it("propagates terminal status and bounds the adapter token budget by the controller allocation", async () => {
		const runAgent = vi.fn<RunAgentHandler>(async () => result("", "turn_limit"));
		const call = createRunAgentChildCall(runAgent, {
			prompt: () => "plan exactly",
			validate: validateAnswer,
			scope: { tokenBudget: 100 },
		});
		await expect(call({}, callOptions(12))).resolves.toEqual({
			status: "turn_limit",
			tokens: 7,
		});
		expect(runAgent).toHaveBeenCalledWith(expect.anything(), expect.objectContaining({ tokenBudget: 12 }));
	});

	it("spawns then continues a retained worker and only accepts its structured terminal result", async () => {
		const runtime: RetainedWorkerRuntime = {
			spawn: vi.fn(async () => ({ handle: "worker-1" })),
			wait: vi.fn(async () => ({
				status: "completed" as const,
				result: { answer: 9 },
				tokens: 4,
			})),
			continue: vi.fn(async () => {}),
		};
		const call = createRetainedWorkerChildCall<{ instruction: string; workerHandle?: string }, { answer: number }>(
			runtime,
			{
				prompt: ({ instruction }) => instruction,
				validate: validateAnswer,
				scope: { tools: "active", model: "faux/worker", maxTurns: 2 },
			},
		);
		const spawned = await call({ instruction: "implement", workerHandle: undefined }, callOptions(11));
		expect(spawned).toMatchObject({ status: "deferred", handle: "worker-1" });
		if (spawned.status !== "deferred") throw new Error("expected deferred worker");
		await expect(spawned.wait(callOptions(11))).resolves.toEqual({
			status: "completed",
			value: { answer: 9 },
			tokens: 4,
		});
		expect(runtime.spawn).toHaveBeenCalledWith(
			expect.objectContaining({
				prompt: "implement",
				model: "faux/worker",
				tools: "active",
				maxTurns: 2,
				tokenBudget: 11,
			}),
		);

		const continued = await call({ instruction: "repair", workerHandle: "worker-1" }, callOptions(8));
		expect(runtime.continue).toHaveBeenCalledWith(
			"worker-1",
			expect.objectContaining({ prompt: "repair", tokenBudget: 8 }),
		);
		expect(continued).toMatchObject({ status: "deferred", handle: "worker-1" });
	});

	it("fails a retained worker that has no structured terminal artifact", async () => {
		const runtime: RetainedWorkerRuntime = {
			spawn: async () => ({ handle: "worker-1" }),
			wait: async () => ({ status: "completed", tokens: 3 }),
			continue: async () => {},
		};
		const call = createRetainedWorkerChildCall(runtime, {
			prompt: () => "evaluate",
			validate: validateAnswer,
		});
		const deferred = await call({ workerHandle: undefined }, callOptions());
		if (deferred.status !== "deferred") throw new Error("expected deferred worker");
		await expect(deferred.wait(callOptions())).resolves.toEqual({
			status: "error",
			tokens: 3,
			error: "retained worker completed without a structured result",
		});
	});
});
