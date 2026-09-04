import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { assessRlmChildSettlement } from "../src/core/rlm-child-settlement.js";

function assistant(overrides: Partial<AssistantMessage>): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text: "done" }],
		api: "openai-completions",
		provider: "openwebui",
		model: "openai.gpt-5.6-sol",
		usage: {
			input: 1,
			output: 1,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 2,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop",
		timestamp: 1,
		...overrides,
	};
}

const spawn: AgentMessage = {
	role: "custom",
	customType: "agent_message",
	content: "[task from parent]",
	display: true,
	details: { id: "spawn:child-1" },
	timestamp: 1,
} as unknown as AgentMessage;

const emptyUsage = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

describe("assessRlmChildSettlement", () => {
	it("accepts a terminal turn that carries text", () => {
		expect(
			assessRlmChildSettlement({
				messages: [spawn, assistant({})],
				spawnMessageId: "spawn:child-1",
				repliedToParent: false,
			}),
		).toEqual({ ok: true });
	});

	it("accepts a terminal turn that carries only a tool call", () => {
		expect(
			assessRlmChildSettlement({
				messages: [
					spawn,
					assistant({
						content: [{ type: "toolCall", id: "c1", name: "python", arguments: {} }],
						stopReason: "toolUse",
					}),
				],
				spawnMessageId: "spawn:child-1",
				repliedToParent: false,
			}),
		).toEqual({ ok: true });
	});

	it("rejects the incident shape: content [] / stop / usage 0", () => {
		const verdict = assessRlmChildSettlement({
			messages: [spawn, assistant({ content: [], usage: emptyUsage })],
			spawnMessageId: "spawn:child-1",
			repliedToParent: false,
		});
		expect(verdict.ok).toBe(false);
		if (verdict.ok) throw new Error("unreachable");
		expect(verdict.reason).toContain("empty assistant response");
		expect(verdict.reason).toContain("stopReason=stop usage=0");
		expect(verdict.reason).toContain("openai.gpt-5.6-sol");
	});

	it("rejects whitespace-only and thinking-only terminal turns", () => {
		for (const content of [
			[{ type: "text" as const, text: "  \n" }],
			[{ type: "thinking" as const, thinking: "hmm", thinkingSignature: "reasoning" }],
		]) {
			const verdict = assessRlmChildSettlement({
				messages: [spawn, assistant({ content })],
				spawnMessageId: "spawn:child-1",
				repliedToParent: false,
			});
			expect(verdict.ok).toBe(false);
		}
	});

	it("rejects a provider error and an abort, quoting the recorded message", () => {
		const errored = assessRlmChildSettlement({
			messages: [
				spawn,
				assistant({
					content: [],
					stopReason: "error",
					errorMessage: "OpenWebUI stream reported an error: serviceUnavailableException",
				}),
			],
			spawnMessageId: "spawn:child-1",
			repliedToParent: true,
		});
		expect(errored).toEqual({
			ok: false,
			reason: "child ended with a provider error: OpenWebUI stream reported an error: serviceUnavailableException",
		});
		const aborted = assessRlmChildSettlement({
			messages: [spawn, assistant({ content: [], stopReason: "aborted", errorMessage: "Request was aborted" })],
			spawnMessageId: "spawn:child-1",
			repliedToParent: false,
		});
		expect(aborted).toEqual({ ok: false, reason: "child turn was aborted: Request was aborted" });
	});

	it("accepts an empty terminal turn when the child already replied to the parent", () => {
		expect(
			assessRlmChildSettlement({
				messages: [spawn, assistant({ content: [], usage: emptyUsage })],
				spawnMessageId: "spawn:child-1",
				repliedToParent: true,
			}),
		).toEqual({ ok: true });
	});

	it("judges only the turns after this run's spawn message on a reused worker", () => {
		const previousRun = assistant({ content: [{ type: "text", text: "earlier task answer" }] });
		const verdict = assessRlmChildSettlement({
			messages: [previousRun, spawn, assistant({ content: [], usage: emptyUsage })],
			spawnMessageId: "spawn:child-1",
			repliedToParent: false,
		});
		expect(verdict.ok).toBe(false);
		expect(
			assessRlmChildSettlement({
				messages: [previousRun, spawn],
				spawnMessageId: "spawn:child-1",
				repliedToParent: false,
			}),
		).toEqual({ ok: false, reason: "child produced no assistant response for the task" });
	});
});
