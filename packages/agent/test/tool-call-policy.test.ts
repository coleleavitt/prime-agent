import { type AssistantMessage, type AssistantMessageEvent, EventStream } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { describe, expect, it, vi } from "vitest";
import { Agent, type AgentEvent, type AgentTool } from "../src/index.js";

class MockAssistantStream extends EventStream<AssistantMessageEvent, AssistantMessage> {
	constructor(message: AssistantMessage) {
		super(
			(event) => event.type === "done" || event.type === "error",
			(event) => {
				if (event.type === "done") return event.message;
				if (event.type === "error") return event.error;
				throw new Error("Unexpected event type");
			},
		);
		queueMicrotask(() => this.push({ type: "done", reason: "stop", message }));
	}
}

function assistant(content: AssistantMessage["content"], stopReason: AssistantMessage["stopReason"]): AssistantMessage {
	return {
		role: "assistant",
		content,
		api: "openai-responses",
		provider: "openai",
		model: "mock",
		usage: {
			input: 1,
			output: 1,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 2,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason,
		timestamp: Date.now(),
	};
}

function testTool(execute: AgentTool["execute"]): AgentTool {
	return {
		name: "tripwire",
		label: "Tripwire",
		description: "must not execute under reject policy",
		parameters: Type.Object({}),
		execute,
	};
}

describe("Agent toolCallPolicy", () => {
	it("rejects a malicious provider tool call before hooks or dispatch and ends after one turn", async () => {
		const execute = vi.fn(async () => ({ content: [{ type: "text" as const, text: "executed" }], details: {} }));
		const beforeToolCall = vi.fn(async () => undefined);
		const afterToolCall = vi.fn(async () => undefined);
		const streamFn = vi.fn(
			() =>
				new MockAssistantStream(
					assistant([{ type: "toolCall", id: "malicious-1", name: "tripwire", arguments: {} }], "toolUse"),
				),
		);
		const events: AgentEvent[] = [];
		const agent = new Agent({
			initialState: { tools: [testTool(execute)] },
			toolCallPolicy: "reject",
			streamFn,
			beforeToolCall,
			afterToolCall,
		});
		agent.subscribe((event) => {
			events.push(event);
		});

		// Runtime mutation cannot widen the construction-time policy.
		expect(() => Object.assign(agent, { toolCallPolicy: "execute" })).toThrow();
		await agent.prompt("try to call the tool");

		expect(streamFn).toHaveBeenCalledTimes(1);
		expect(execute).not.toHaveBeenCalled();
		expect(beforeToolCall).not.toHaveBeenCalled();
		expect(afterToolCall).not.toHaveBeenCalled();
		expect(events.filter((event) => event.type === "turn_start")).toHaveLength(1);
		expect(events.some((event) => event.type.startsWith("tool_execution_"))).toBe(false);
		const turnEnd = events.find((event) => event.type === "turn_end");
		expect(turnEnd).toMatchObject({
			type: "turn_end",
			toolResults: [],
			message: { stopReason: "error", errorMessage: "unexpected_tool_call" },
		});
		expect(agent.state.messages).toHaveLength(2);
		expect(agent.state.messages.some((message) => message.role === "toolResult")).toBe(false);
	});

	it("keeps ordinary tool execution and the follow-on provider turn unchanged", async () => {
		const execute = vi.fn(async () => ({ content: [{ type: "text" as const, text: "ok" }], details: {} }));
		const responses = [
			assistant([{ type: "toolCall", id: "ordinary-1", name: "tripwire", arguments: {} }], "toolUse"),
			assistant([{ type: "text", text: "done" }], "stop"),
		];
		const streamFn = vi.fn(() => new MockAssistantStream(responses.shift()!));
		const agent = new Agent({ initialState: { tools: [testTool(execute)] }, streamFn });

		await agent.prompt("call the tool");

		expect(execute).toHaveBeenCalledTimes(1);
		expect(streamFn).toHaveBeenCalledTimes(2);
		expect(agent.state.messages.some((message) => message.role === "toolResult")).toBe(true);
		expect(agent.state.messages.at(-1)).toMatchObject({ role: "assistant", stopReason: "stop" });
	});
});
