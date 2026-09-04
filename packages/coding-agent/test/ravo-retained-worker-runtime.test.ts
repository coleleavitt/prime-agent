import { existsSync, rmSync } from "node:fs";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import { type AssistantMessage, fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import { isAgentSessionMessage } from "../src/core/agent-messages.js";
import { createAgentSessionRetainedWorkerRuntime } from "../src/core/ravo/retained-worker-runtime.js";
import { createRetainedWorkerChildCall } from "../src/core/ravo/runtime-adapter.js";
import type {
	CreateRlmSubagentRuntimeOptions,
	RlmSubagentRuntime,
	SubagentRuntimeHost,
} from "../src/core/rlm-runtime.js";
import { SessionManager } from "../src/core/session-manager.js";
import { createHarness, type Harness } from "./suite/harness.js";

const harnesses: Harness[] = [];
afterEach(() => {
	for (const harness of harnesses.splice(0)) harness.cleanup();
});

function tool(
	name: string,
	execute: AgentTool["execute"] = async () => ({ content: [], details: undefined }),
): AgentTool {
	return { name, label: name, description: name, parameters: { type: "object" }, execute };
}

function deferred<T = void>(): { promise: Promise<T>; resolve: (value: T) => void } {
	let resolve!: (value: T) => void;
	const promise = new Promise<T>((r) => {
		resolve = r;
	});
	return { promise, resolve };
}

const callOptions = (tokenBudget = 100_000) => ({ signal: new AbortController().signal, tokenBudget });

async function createHostedParent(options?: { tools?: AgentTool[] }) {
	const children: Harness[] = [];
	const creations: CreateRlmSubagentRuntimeOptions[] = [];
	const deleted: string[] = [];
	const host: SubagentRuntimeHost = {
		async createRlmSubagentRuntime(runtimeOptions): Promise<RlmSubagentRuntime> {
			creations.push(runtimeOptions);
			const child = await createHarness({
				tools: options?.tools,
				rlmDepth: runtimeOptions.rlmDepth,
				rlmMaxDepth: runtimeOptions.rlmMaxDepth,
				rlmSessionDir: runtimeOptions.sessionDir,
			});
			harnesses.push(child);
			children.push(child);
			child.session.setActiveToolsByName(runtimeOptions.activeToolNames);
			return { session: child.session };
		},
		async deleteRlmSubagentRuntime(childId, session) {
			deleted.push(childId);
			await session?.disposeAsync();
		},
	};
	const parent = await createHarness({ tools: options?.tools, subagentRuntimeHost: host, rlmMaxDepth: 3 });
	harnesses.push(parent);
	return { parent, children, creations, deleted };
}

function taskTexts(harness: Harness): string[] {
	return harness.session.messages.filter(isAgentSessionMessage).map((message) => message.details.message);
}

function assistantTexts(harness: Harness): string[] {
	return harness.session.messages
		.filter((message): message is AssistantMessage => message.role === "assistant")
		.map((message) =>
			message.content
				.filter((block): block is { type: "text"; text: string } => block.type === "text")
				.map((block) => block.text)
				.join(""),
		);
}

describe("AgentSession retained worker runtime", () => {
	it("spawns a scoped resident worker and settles its initial turn as a structured result", async () => {
		const hosted = await createHostedParent({ tools: [tool("read"), tool("write")] });
		hosted.parent.session.setActiveToolsByName(["read"]);
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);

		const spawned = runtime.spawn({
			prompt: "implement the plan",
			tools: { allow: ["read", "write"] },
			role: "implement",
			...callOptions(),
		});
		const { handle } = await spawned;
		const child = hosted.children[0]!;
		child.setResponses([fauxAssistantMessage('{"summary":"done"}')]);

		const terminal = await runtime.wait(handle, callOptions());
		expect(terminal).toMatchObject({
			status: "completed",
			reason: "completed",
			result: { summary: "done" },
			text: '{"summary":"done"}',
		});
		expect(terminal.tokens).toBeGreaterThan(0);
		expect(terminal.usage?.totalTokens).toBe(terminal.tokens);

		expect(hosted.creations[0]?.activeToolNames).toEqual(["read"]);
		expect(child.session.getActiveToolNames()).toEqual(["read"]);
		expect(child.session.sessionName).toBe("ravo-implement-1");
		expect(taskTexts(child)).toEqual(["implement the plan"]);
		expect(hosted.parent.session.messages).toEqual([]);
		const listed = await hosted.parent.session.listRlmSubagents();
		expect(listed.subagents.map((entry) => entry.rlm_child_id)).toEqual([handle]);
		expect(hosted.parent.session.getRlmChildSession(handle)).toBe(child.session);
		expect(runtime.handles()).toEqual([handle]);
	});

	it("continues follow-up turns in the same child and never widens its tool scope", async () => {
		const hosted = await createHostedParent({ tools: [tool("read"), tool("write")] });
		hosted.parent.session.setActiveToolsByName(["read", "write"]);
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const { handle } = await runtime.spawn({ prompt: "implement", tools: { allow: ["read"] }, ...callOptions() });
		const child = hosted.children[0]!;
		child.setResponses([fauxAssistantMessage('{"step":1}')]);
		await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({ status: "completed" });

		await expect(runtime.continue(handle, { prompt: "", tools: "none", ...callOptions() })).rejects.toThrow(
			"must not be empty",
		);

		await runtime.continue(handle, { prompt: "repair it", tools: { allow: ["read", "write"] }, ...callOptions() });
		expect(child.session.getActiveToolNames()).toEqual(["read"]);
		await expect(runtime.continue(handle, { prompt: "again", tools: "none", ...callOptions() })).rejects.toThrow(
			"already has a turn in flight",
		);
		child.setResponses([fauxAssistantMessage('{"step":2}')]);
		await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({
			status: "completed",
			result: { step: 2 },
		});
		expect(hosted.children).toHaveLength(1);
		expect(taskTexts(child)).toEqual(["implement", "repair it"]);
		expect(assistantTexts(child)).toEqual(['{"step":1}', '{"step":2}']);

		await runtime.continue(handle, { prompt: "narrow", tools: "none", ...callOptions() });
		expect(child.session.getActiveToolNames()).toEqual([]);
		child.setResponses([fauxAssistantMessage('{"step":3}')]);
		await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({ status: "completed" });

		await expect(
			runtime.continue(handle, { prompt: "switch", tools: "none", model: "other/model", ...callOptions() }),
		).rejects.toThrow("cannot switch");

		await runtime.release(handle);
		expect(hosted.deleted).toEqual([handle]);
		expect(runtime.handles()).toEqual([]);
		await expect(runtime.wait(handle, callOptions())).rejects.toThrow("Unknown retained worker");
		expect((await hosted.parent.session.listRlmSubagents()).subagents).toEqual([]);
	});

	it("drives the RAVO deferred child call through spawn, wait, and continue", async () => {
		const hosted = await createHostedParent();
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const call = createRetainedWorkerChildCall<{ task: string; workerHandle?: string }, { answer: number }>(runtime, {
			prompt: ({ task }) => task,
			validate: (value) => {
				if (
					typeof value !== "object" ||
					value === null ||
					typeof (value as { answer?: unknown }).answer !== "number"
				)
					throw new Error("answer must be a number");
				return { answer: (value as { answer: number }).answer };
			},
			scope: { tools: "none", role: "implement" },
		});
		const first = await call({ task: "first" }, callOptions());
		if (first.status !== "deferred") throw new Error("expected deferred");
		hosted.children[0]!.setResponses([fauxAssistantMessage('{"answer":1}')]);
		await expect(first.wait(callOptions())).resolves.toEqual({
			status: "completed",
			value: { answer: 1 },
			tokens: expect.any(Number),
		});

		const second = await call({ task: "second", workerHandle: first.handle }, callOptions());
		if (second.status !== "deferred") throw new Error("expected deferred");
		expect(second.handle).toBe(first.handle);
		hosted.children[0]!.setResponses([fauxAssistantMessage("not json")]);
		await expect(second.wait(callOptions())).resolves.toMatchObject({
			status: "error",
			error: expect.stringContaining("not valid JSON"),
		});
		expect(hosted.children).toHaveLength(1);
	});

	it("cancels an in-flight turn from the controller and reports it as aborted", async () => {
		const hosted = await createHostedParent();
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const started = deferred();
		const release = deferred();
		const { handle } = await runtime.spawn({ prompt: "long task", tools: "none", ...callOptions() });
		hosted.children[0]!.setResponses([
			async () => {
				started.resolve();
				await release.promise;
				return fauxAssistantMessage('{"late":true}');
			},
		]);
		await started.promise;
		expect(runtime.cancel(handle, "controller stop")).toBe(true);
		release.resolve();
		await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({
			status: "aborted",
			reason: "aborted",
			error: "controller stop",
		});
		expect(runtime.cancel(handle)).toBe(false);
		expect(hosted.parent.session.getRlmChildSession(handle)).toBeDefined();
	});

	it("aborts the turn when the controller signal passed to wait is aborted", async () => {
		const hosted = await createHostedParent();
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const started = deferred();
		const release = deferred();
		const { handle } = await runtime.spawn({ prompt: "long task", tools: "none", ...callOptions() });
		hosted.children[0]!.setResponses([
			async () => {
				started.resolve();
				await release.promise;
				return fauxAssistantMessage('{"late":true}');
			},
		]);
		const controller = new AbortController();
		const waiting = runtime.wait(handle, { signal: controller.signal, tokenBudget: 100_000 });
		await started.promise;
		controller.abort();
		release.resolve();
		await expect(waiting).resolves.toMatchObject({ status: "aborted" });
	});

	it("refuses to spawn under an aborted signal and links a pre-aborted turn", async () => {
		const hosted = await createHostedParent();
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const aborted = new AbortController();
		aborted.abort();
		await expect(
			runtime.spawn({ prompt: "task", tools: "none", signal: aborted.signal, tokenBudget: 10 }),
		).rejects.toThrow("cancelled");
		expect(hosted.children).toHaveLength(0);
	});

	it("never settles an empty or errored terminal turn as completed", async () => {
		const hosted = await createHostedParent();
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const { handle } = await runtime.spawn({ prompt: "task", tools: "none", ...callOptions() });
		const child = hosted.children[0]!;
		child.setResponses([fauxAssistantMessage([])]);
		await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({
			status: "error",
			reason: "empty_turn",
			error: expect.stringContaining("empty assistant response"),
		});

		await runtime.continue(handle, { prompt: "retry", tools: "none", ...callOptions() });
		child.setResponses([]);
		const errored = await runtime.wait(handle, callOptions());
		expect(errored.status).toBe("error");
		expect(errored.reason).not.toBe("completed");
		expect(errored.result).toBeUndefined();

		await runtime.continue(handle, { prompt: "retry again", tools: "none", ...callOptions() });
		child.setResponses([fauxAssistantMessage("   ")]);
		await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({
			status: "error",
			reason: "empty_turn",
		});
	});

	it("stops the turn at the wait token cap and reports budget_exceeded", async () => {
		const hosted = await createHostedParent();
		const runtime = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const { handle } = await runtime.spawn({ prompt: "task", tools: "none", ...callOptions() });
		hosted.children[0]!.setResponses([fauxAssistantMessage('{"ok":true}')]);
		await expect(runtime.wait(handle, callOptions(1))).resolves.toMatchObject({
			status: "budget_exceeded",
			reason: "budget_exceeded",
		});
	});

	it("recovers a resident handle by id into a fresh runtime instance", async () => {
		const hosted = await createHostedParent({ tools: [tool("read")] });
		hosted.parent.session.setActiveToolsByName(["read"]);
		const first = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		const { handle } = await first.spawn({ prompt: "task", tools: "none", ...callOptions() });
		const child = hosted.children[0]!;
		child.setResponses([fauxAssistantMessage('{"answer":1}')]);
		await expect(first.wait(handle, callOptions())).resolves.toMatchObject({ status: "completed" });

		const second = createAgentSessionRetainedWorkerRuntime(hosted.parent.session);
		await expect(second.recover("missing")).rejects.toThrow("Unknown retained worker");
		await expect(second.recover(handle)).resolves.toEqual({ handle });
		await expect(second.wait(handle, callOptions())).resolves.toMatchObject({
			status: "completed",
			result: { answer: 1 },
			tokens: 0,
		});
		await second.continue(handle, { prompt: "more", tools: { allow: ["read"] }, ...callOptions() });
		expect(child.session.getActiveToolNames()).toEqual([]);
		child.setResponses([fauxAssistantMessage('{"answer":2}')]);
		await expect(second.wait(handle, callOptions())).resolves.toMatchObject({
			status: "completed",
			result: { answer: 2 },
		});
		expect(taskTexts(child)).toEqual(["task", "more"]);
	});

	it("recovers a worker from its persisted transcript after a parent restart (inline mode)", async () => {
		const parent = await createHarness({ persistSession: true });
		let restarted: Harness | undefined;
		const tempDir = parent.tempDir;
		try {
			const runtime = createAgentSessionRetainedWorkerRuntime(parent.session);
			const { handle } = await runtime.spawn({ prompt: "task", tools: "none", ...callOptions() });
			parent.setResponses([fauxAssistantMessage('{"answer":1}')]);
			await expect(runtime.wait(handle, callOptions())).resolves.toMatchObject({ status: "completed" });
			const childDir = (await parent.session.listRlmSubagents()).subagents[0]?.session_dir;
			expect(childDir && existsSync(childDir)).toBe(true);
			parent.sessionManager.flushNow();
			const parentFile = parent.sessionManager.getSessionFile();
			if (!parentFile) throw new Error("parent session file missing");
			parent.session.dispose();
			parent.faux.unregister();

			restarted = await createHarness({
				sessionManager: SessionManager.open(parentFile, parent.sessionManager.getSessionDir(), tempDir),
			});
			expect(restarted.session.sessionId).toBe(parent.session.sessionId);
			const recovered = createAgentSessionRetainedWorkerRuntime(restarted.session);
			await expect(recovered.recover(handle)).resolves.toEqual({ handle });
			expect((await restarted.session.listRlmSubagents()).subagents.map((entry) => entry.rlm_child_id)).toEqual([
				handle,
			]);
			await expect(recovered.wait(handle, callOptions())).resolves.toMatchObject({
				status: "completed",
				result: { answer: 1 },
			});
			await recovered.continue(handle, { prompt: "more", tools: "none", ...callOptions() });
			restarted.setResponses([fauxAssistantMessage('{"answer":2}')]);
			await expect(recovered.wait(handle, callOptions())).resolves.toMatchObject({
				status: "completed",
				result: { answer: 2 },
			});
			const child = restarted.session.getRlmChildSession(handle);
			expect(child?.messages.filter(isAgentSessionMessage).map((message) => message.details.message)).toEqual([
				"task",
				"more",
			]);
		} finally {
			restarted?.session.dispose();
			restarted?.faux.unregister();
			rmSync(tempDir, { recursive: true, force: true, maxRetries: 40, retryDelay: 50 });
		}
	});
});
