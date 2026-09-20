import type { Usage } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import {
	createRetainedWorkerChildCall,
	createRunAgentChildCall,
	extractJsonValue,
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

	it("threads the scope's thinking level onto the request and its output cap onto the options, and omits both when unset", async () => {
		const runAgent = vi.fn<RunAgentHandler>(async () => result('{"answer":1}'));
		const call = createRunAgentChildCall(runAgent, {
			prompt: () => "capped child",
			validate: validateAnswer,
			scope: { model: "faux/child", tools: "none", maxTurns: 8, thinkingLevel: "off", maxOutputTokens: 4096 },
		});
		await call({}, callOptions(200_000));
		expect(runAgent).toHaveBeenCalledWith(
			{ prompt: "capped child", model: "faux/child", thinkingLevel: "off" },
			{ tools: "none", signal: expect.any(AbortSignal), maxTurns: 8, tokenBudget: 200_000, maxOutputTokens: 4096 },
		);
		// Without the knobs neither key appears, so older callers' exact-equality assertions still hold.
		const plain = vi.fn<RunAgentHandler>(async () => result('{"answer":1}'));
		await createRunAgentChildCall(plain, {
			prompt: () => "plain",
			validate: validateAnswer,
			scope: { tools: "none" },
		})({}, callOptions(10));
		expect(plain.mock.calls[0]![0]).toEqual({ prompt: "plain" });
		expect(plain.mock.calls[0]![1]).not.toHaveProperty("maxOutputTokens");
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

	it("opts into lenient extraction only when asked: fences and prose around the object are then tolerated", async () => {
		for (const output of [
			'result: {"answer":42}',
			'```json\n{"answer":42}\n```',
			'Between {"answer":1} and\n{"answer":42, "note":"x"}\nwe pick the latter.',
		]) {
			const call = createRunAgentChildCall(async () => result(output), {
				prompt: () => "evaluate leniently",
				validate: validateAnswer,
				extractJson: "object",
			});
			await expect(call({}, callOptions())).resolves.toEqual({
				status: "completed",
				value: { answer: 42 },
				tokens: 7,
			});
		}
		// Schema-invalid and truncated outputs still fail, and so does output with no object at all.
		for (const output of ['{"answer":"42"}', 'partial {"answer": 4', "[1, 2, 3]", "no json here"]) {
			const call = createRunAgentChildCall(async () => result(output), {
				prompt: () => "evaluate leniently",
				validate: validateAnswer,
				extractJson: "object",
			});
			await expect(call({}, callOptions())).resolves.toMatchObject({ status: "error", tokens: 7 });
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

describe("extractJsonValue", () => {
	it("returns bare JSON of the requested kind as is", () => {
		expect(extractJsonValue('{"a": 1}', "object")).toEqual({ a: 1 });
		expect(extractJsonValue("  [1, 2]\n", "array")).toEqual([1, 2]);
	});

	it("strips markdown fences and prose before or after the value", () => {
		expect(
			extractJsonValue('Here you go:\n```json\n{"n": 4, "weights": [1, 2, 3, 4]}\n```\nHope this helps.', "object"),
		).toEqual({
			n: 4,
			weights: [1, 2, 3, 4],
		});
		expect(extractJsonValue('Policies: [{"a": 1}, {"a": 2}] as requested.', "array")).toEqual([{ a: 1 }, { a: 2 }]);
	});

	it("picks the largest parseable value of the requested kind, so prose fragments and nested values never shadow the answer", () => {
		const text =
			'I moved mass from bins [1, 2] to the edges. Compared with {"n": 4} it is flatter: {"n": 4, "weights": [3, 1, 1, 3]} (see {"peak": 1.9}).';
		expect(extractJsonValue(text, "object")).toEqual({ n: 4, weights: [3, 1, 1, 3] });
		// Asked for an array, the largest one anywhere (here the nested weights) wins over the prose fragment.
		expect(extractJsonValue(text, "array")).toEqual([3, 1, 1, 3]);
		expect(extractJsonValue("Two lists: [1, 2] then [1, 2, 3, 4, 5].", "array")).toEqual([1, 2, 3, 4, 5]);
		// An object wrapped in a stray array is still found.
		expect(extractJsonValue('[{"answer": 42}]', "object")).toEqual({ answer: 42 });
		expect(extractJsonValue('{"outer": {"inner": 1}}', "object")).toEqual({ outer: { inner: 1 } });
	});

	it("is string-aware: brackets and escaped quotes inside strings do not unbalance the scan", () => {
		expect(extractJsonValue('note: {"text": "a } b ] \\" c", "k": [1]}', "object")).toEqual({
			text: 'a } b ] " c',
			k: [1],
		});
	});

	it("throws when nothing of the requested kind parses, including a value truncated at an output cap", () => {
		expect(() => extractJsonValue("no json here", "object")).toThrow(/no JSON object/);
		expect(() => extractJsonValue('Reasoning... {"n": 4, "weights": [1, 2,', "object")).toThrow(/no JSON object/);
		expect(() => extractJsonValue('{"a": 1}', "array")).toThrow(/no JSON array/);
		expect(() => extractJsonValue("[1, 2]", "object")).toThrow(/no JSON object/);
		expect(() => extractJsonValue("42", "object")).toThrow(/no JSON object/);
	});
});
