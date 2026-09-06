import type { AssistantMessage, Model } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import type { AgentSession, AgentSessionEvent, AgentSessionEventListener } from "../src/core/agent-session.js";
import { runAgentSession } from "../src/core/run-agent.js";

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

function fakeSession(run: (emit: (event: AgentSessionEvent) => void) => Promise<void>) {
	const listeners = new Set<AgentSessionEventListener>();
	const messages = [assistant("answer", 12)];
	const abort = vi.fn();
	return {
		messages,
		abort,
		subscribe(listener: AgentSessionEventListener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
		promptAndWait: vi.fn(async () =>
			run((event) => {
				for (const listener of listeners) listener(event);
			}),
		),
	} as unknown as AgentSession;
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
