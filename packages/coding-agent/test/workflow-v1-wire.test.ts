import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { decodeWorkflowRunAgentReply, decodeWorkflowRunAgentRequest } from "../src/core/workflow-v1-wire.js";

const request = () =>
	decodeWorkflowRunAgentRequest({
		protocol: "prime.workflow.run-agent/v1",
		requestId: "r1",
		nodeId: "n1",
		prompt: "hi",
		model: null,
		maxTurns: 1,
		maxResultUtf8Bytes: 100,
		drainTimeoutMs: 100,
		tools: "none",
	});
const usage = (finality = "final") => ({
	inputTokens: 1,
	outputTokens: 1,
	cacheReadTokens: 0,
	cacheWriteTokens: 0,
	totalTokens: 2,
	costInput: null,
	costOutput: null,
	costCacheRead: null,
	costCacheWrite: null,
	costTotal: null,
	completeness: "complete_host_observation",
	finality,
});
const completed = () => ({
	protocol: "prime.workflow.run-agent-result/v1",
	requestId: "r1",
	nodeId: "n1",
	resolvedModel: "p/m",
	turnsStarted: 1,
	durationMs: 2,
	budgetExhausted: false,
	budgetOvershootTokens: 0,
	usage: usage(),
	outcome: "completed",
	stopReason: "completed",
	result: { text: "ok", utf8Bytes: 2, sha256: createHash("sha256").update("ok").digest("hex") },
	error: null,
});
describe("workflow v1 closed wire", () => {
	it("accepts the sole request and completed reply", () =>
		expect(decodeWorkflowRunAgentReply(completed(), request()).outcome).toBe("completed"));
	it.each([{ prompt: "" }, { maxTurns: 2 }, { protocol: "prime.workflow.run-child/v1" }, { extra: true }])(
		"rejects malformed requests",
		(patch) => expect(() => decodeWorkflowRunAgentRequest({ ...request(), ...patch })).toThrow(),
	);
	it("rejects digest, correlation, extras and incorrect finality", () => {
		for (const patch of [
			{ requestId: "other" },
			{ extra: true },
			{ usage: usage("known_prefix") },
			{ result: { ...completed().result, sha256: "0".repeat(64) } },
		])
			expect(() => decodeWorkflowRunAgentReply({ ...completed(), ...patch }, request())).toThrow();
	});
	it("accepts only known-prefix execution_unknown", () => {
		const value = {
			...completed(),
			outcome: "execution_unknown",
			stopReason: "host_connection_lost",
			result: null,
			error: { code: "EXECUTION_UNKNOWN", message: "lost" },
			usage: usage("known_prefix"),
		};
		expect(decodeWorkflowRunAgentReply(value, request()).outcome).toBe("execution_unknown");
		expect(() => decodeWorkflowRunAgentReply({ ...value, usage: usage() }, request())).toThrow();
	});

	it("rejects completed zero-turn and contradictory budget semantics", () => {
		const bounded = decodeWorkflowRunAgentRequest({ ...request(), softTokenBudget: 10 });
		const base = { ...completed(), usage: { ...usage(), totalTokens: 20 } };
		expect(() => decodeWorkflowRunAgentReply(base, bounded)).toThrow("budget semantics");
		expect(() =>
			decodeWorkflowRunAgentReply(
				{ ...base, budgetExhausted: true, budgetOvershootTokens: 10, turnsStarted: 0 },
				bounded,
			),
		).toThrow("one started turn");
	});
});
