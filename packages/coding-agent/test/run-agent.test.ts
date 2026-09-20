import type { StreamFn } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, Model } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import type { AgentSession, AgentSessionEvent, AgentSessionEventListener } from "../src/core/agent-session.js";
import { cappedMaxTokens, capStreamFn, runAgentSession, THINKING_ALLOWANCE } from "../src/core/run-agent.js";

const model = {
	id: "child",
	name: "Child",
	provider: "faux",
	api: "faux",
	baseUrl: "http://localhost",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1000,
	maxTokens: 100,
} satisfies Model<"faux">;

function assistant(
	text: string,
	tokens: number,
	stopReason: AssistantMessage["stopReason"] = "stop",
): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		api: "faux",
		provider: "faux",
		model: "child",
		stopReason,
		timestamp: Date.now(),
		usage: {
			input: tokens,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: tokens,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
	};
}

/** The stream options the fake Agent's `streamFn` was called with, in call order. */
type StreamCall = Parameters<StreamFn>[2];

function fakeSession(
	run: (emit: (event: AgentSessionEvent) => void, session: AgentSession) => Promise<void>,
	streamCalls: StreamCall[] = [],
) {
	const listeners = new Set<AgentSessionEventListener>();
	const messages = [assistant("answer", 12)];
	const abort = vi.fn();
	// The real Agent hands its whole loop config to `streamFn`; the fake records
	// what arrives so a wrapped `maxTokens` is observable.
	const streamFn: StreamFn = ((_model, _context, options) => {
		streamCalls.push(options);
		return undefined as never;
	}) as StreamFn;
	const session = {
		messages,
		abort,
		agent: { streamFn },
		subscribe(listener: AgentSessionEventListener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
		promptAndWait: vi.fn(async () =>
			run((event) => {
				for (const listener of listeners) listener(event);
			}, session),
		),
	} as unknown as AgentSession;
	return session;
}

describe("runAgentSession", () => {
	it("reports a distinct turn limit and normalized progress", async () => {
		const message = assistant("answer", 12, "toolUse");
		const session = fakeSession(async (emit) => {
			emit({ type: "tool_execution_start", toolCallId: "tool-1", toolName: "read", args: {} });
			emit({ type: "tool_execution_end", toolCallId: "tool-1", toolName: "read", result: {}, isError: false });
			emit({ type: "turn_end", message, toolResults: [] });
		});
		const progress: string[] = [];
		const result = await runAgentSession({
			session,
			model,
			request: { prompt: "task" },
			options: { maxTurns: 1, onProgress: (event) => progress.push(event.type) },
		});
		expect(result).toMatchObject({ status: "turn_limit", turns: 1, toolCalls: 1 });
		expect(session.abort).toHaveBeenCalledOnce();
		expect(progress).toEqual(["started", "tool", "tool", "turn", "finished"]);
	});

	it("reports a distinct budget limit when the turn that crosses it wants to continue", async () => {
		const message = assistant("answer", 12, "toolUse");
		const session = fakeSession(async (emit) => emit({ type: "turn_end", message, toolResults: [] }));
		await expect(
			runAgentSession({ session, model, request: { prompt: "task" }, options: { tokenBudget: 10 } }),
		).resolves.toMatchObject({ status: "budget_exceeded", usage: { totalTokens: 12 } });
	});

	it("keeps a final answer that crosses the budget or turn limit as completed", async () => {
		// The historian regression: a single 30k+ token summary turn ended with
		// stopReason "stop" and was reported as budget_exceeded, so the caller
		// threw the answer away and retried on the next model.
		const message = assistant("answer", 12);
		const session = fakeSession(async (emit) => emit({ type: "turn_end", message, toolResults: [] }));
		const result = await runAgentSession({
			session,
			model,
			request: { prompt: "task" },
			options: { tokenBudget: 10, maxTurns: 1 },
		});
		expect(result).toMatchObject({ status: "completed", output: "answer", usage: { totalTokens: 12 } });
		expect(session.abort).not.toHaveBeenCalled();
	});

	it("honors a pre-aborted linked signal without prompting", async () => {
		const controller = new AbortController();
		controller.abort();
		const session = fakeSession(async () => {});
		const result = await runAgentSession({
			session,
			model,
			request: { prompt: "task" },
			options: { signal: controller.signal },
		});
		expect(result.status).toBe("aborted");
		expect(session.promptAndWait).not.toHaveBeenCalled();
	});

	it("caps every model call's maxTokens at maxOutputTokens plus the thinking allowance", async () => {
		const calls: StreamCall[] = [];
		const session = fakeSession(async (_emit, current) => {
			// What the Agent does on each turn: call the session's streamFn with its loop config.
			current.agent.streamFn(model, { messages: [] }, { reasoning: "off" });
			current.agent.streamFn(model, { messages: [] }, { maxTokens: 1000, reasoning: "off" });
			current.agent.streamFn(model, { messages: [] }, { maxTokens: 9000, reasoning: "off" });
			current.agent.streamFn(model, { messages: [] }, { reasoning: "low" });
			current.agent.streamFn(model, { messages: [] }, { maxTokens: 5000, reasoning: "high" });
		}, calls);
		const result = await runAgentSession({
			session,
			model,
			request: { prompt: "task" },
			options: { maxOutputTokens: 4096 },
		});
		expect(result.status).toBe("completed");
		expect(calls.map((options) => options?.maxTokens)).toEqual([
			4096, // no existing cap: the visible-answer cap
			1000, // a tighter existing cap wins
			4096, // a looser existing cap is brought down
			4096 + THINKING_ALLOWANCE.low, // thinking on: the allowance sits on top so the cap still means the answer
			5000, // min(existing 5000, 4096 + 16384)
		]);
		// The reasoning level itself is untouched by the wrap.
		expect(calls.map((options) => options?.reasoning)).toEqual(["off", "off", "off", "low", "high"]);
	});

	it("leaves the stream function alone without a cap and rejects a non-positive cap", async () => {
		const calls: StreamCall[] = [];
		const session = fakeSession(async (_emit, current) => {
			current.agent.streamFn(model, { messages: [] }, { reasoning: "off" });
		}, calls);
		const before = session.agent.streamFn;
		await runAgentSession({ session, model, request: { prompt: "task" } });
		expect(session.agent.streamFn).toBe(before);
		expect(calls.map((options) => options?.maxTokens)).toEqual([undefined]);
		for (const cap of [0, -1, 1.5]) {
			await expect(
				runAgentSession({ session, model, request: { prompt: "task" }, options: { maxOutputTokens: cap } }),
			).rejects.toThrow("maxOutputTokens must be a positive integer");
		}
	});

	it("cappedMaxTokens and capStreamFn mirror the provider thinking budgets", () => {
		expect(cappedMaxTokens(2048, undefined)).toBe(2048);
		expect(cappedMaxTokens(2048, "off")).toBe(2048);
		expect(cappedMaxTokens(2048, "minimal")).toBe(2048 + 1024);
		expect(cappedMaxTokens(2048, "medium")).toBe(2048 + 8192);
		expect(cappedMaxTokens(2048, "xhigh")).toBe(2048 + 16384);
		expect(cappedMaxTokens(2048, "max")).toBe(2048 + 16384);
		const inner = vi.fn<StreamFn>();
		capStreamFn(inner, 100)(model, { messages: [] }, { maxTokens: 50 });
		capStreamFn(inner, 100)(model, { messages: [] }, undefined);
		expect(inner.mock.calls.map((call) => call[2]?.maxTokens)).toEqual([50, 100]);
	});

	it("contains progress observer exceptions", async () => {
		const session = fakeSession(async () => {});
		await expect(
			runAgentSession({
				session,
				model,
				request: { prompt: "task" },
				options: {
					onProgress: () => {
						throw new Error("boom");
					},
				},
			}),
		).resolves.toMatchObject({ status: "completed", output: "answer" });
	});
});
