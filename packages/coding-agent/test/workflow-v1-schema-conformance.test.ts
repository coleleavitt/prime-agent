import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { decodeWorkflowRunAgentReply, decodeWorkflowRunAgentRequest } from "../src/core/workflow-v1-wire.js";

const schema = JSON.parse(
	readFileSync(new URL("../../../scripts/fixtures/workflow-native-host-v1.schema.json", import.meta.url), "utf8"),
);
const requestSchema = schema.$defs.runAgentRequest.properties;
const replyVariants = schema.$defs.runAgentReply.oneOf;
const requestValue = () => ({
	protocol: requestSchema.protocol.const,
	requestId: "r1",
	nodeId: "n1",
	prompt: "x",
	model: null,
	maxTurns: requestSchema.maxTurns.const,
	maxResultUtf8Bytes: 10,
	drainTimeoutMs: 10,
	tools: requestSchema.tools.const,
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
	durationMs: 1,
	budgetExhausted: false,
	budgetOvershootTokens: 0,
	usage: usage(),
	outcome: "completed",
	stopReason: "completed",
	result: { text: "ok", utf8Bytes: 2, sha256: createHash("sha256").update("ok").digest("hex") },
	error: null,
});
const rejectRequest = (patch: Record<string, unknown>) =>
	expect(() => decodeWorkflowRunAgentRequest({ ...requestValue(), ...patch })).toThrow();

describe("normative Workflow V1 schema conformance", () => {
	it("copies normative protocol, policy, and request bounds", () => {
		expect(requestSchema.protocol.const).toBe("prime.workflow.run-agent/v1");
		expect(requestSchema.maxTurns.const).toBe(1);
		expect(requestSchema.tools.const).toBe("none");
		expect(requestSchema.prompt).toMatchObject({ minLength: 1, maxLength: 262144 });
		expect(requestSchema.softTokenBudget.oneOf[1]).toMatchObject({ minimum: 1, maximum: 1_000_000 });
		expect(requestSchema.maxResultUtf8Bytes).toMatchObject({ minimum: 1, maximum: 1_048_576 });
		expect(requestSchema.drainTimeoutMs).toMatchObject({ minimum: 1, maximum: 30_000 });
		expect(new Set(replyVariants.map((v: any) => v.properties.outcome.const))).toEqual(
			new Set(["completed", "failed", "cancelled", "execution_unknown"]),
		);
	});

	it("rejects mutations of protocol, maxTurns, tool policy, and every request bound", () => {
		for (const patch of [
			{ protocol: "prime.workflow.run-agent/v2" },
			{ maxTurns: 0 },
			{ maxTurns: 2 },
			{ tools: "all" },
			{ prompt: "" },
			{ prompt: "x".repeat(262145) },
			{ softTokenBudget: 0 },
			{ softTokenBudget: 1_000_001 },
			{ maxResultUtf8Bytes: 0 },
			{ maxResultUtf8Bytes: 1_048_577 },
			{ drainTimeoutMs: 0 },
			{ drainTimeoutMs: 30_001 },
		])
			rejectRequest(patch);
	});

	it("rejects digest, finality, outcome, and execution_unknown mutations", () => {
		const req = decodeWorkflowRunAgentRequest(requestValue());
		const done = completed();
		for (const value of [
			{ ...done, protocol: "prime.workflow.run-agent-result/v2" },
			{ ...done, usage: usage("known_prefix") },
			{ ...done, outcome: "mystery" },
			{ ...done, result: { ...done.result, utf8Bytes: 1 } },
			{ ...done, result: { ...done.result, sha256: "0".repeat(64) } },
		])
			expect(() => decodeWorkflowRunAgentReply(value, req)).toThrow();
		const unknown = {
			...done,
			outcome: "execution_unknown",
			stopReason: "host_connection_lost",
			result: null,
			error: { code: "EXECUTION_UNKNOWN", message: "lost" },
			usage: usage("known_prefix"),
		};
		expect(decodeWorkflowRunAgentReply(unknown, req).outcome).toBe("execution_unknown");
		for (const patch of [
			{ usage: usage("final") },
			{ stopReason: "provider_failed" },
			{ error: { code: "HOST_FAILED", message: "lost" } },
			{ result: done.result },
		])
			expect(() => decodeWorkflowRunAgentReply({ ...unknown, ...patch }, req)).toThrow();
	});

	it("enforces every normative terminal outcome variant", () => {
		const req = decodeWorkflowRunAgentRequest(requestValue());
		const done = completed();
		const failedReasons = replyVariants
			.filter((v: any) => v.properties.outcome.const === "failed")
			.map((v: any) => [v.properties.stopReason.const, v.properties.error.properties.code.const]);
		for (const [reason, code] of failedReasons) {
			const value = {
				...done,
				outcome: "failed",
				stopReason: reason,
				result: null,
				error: { code, message: "failure" },
			};
			expect(decodeWorkflowRunAgentReply(value, req).outcome).toBe("failed");
			expect(() =>
				decodeWorkflowRunAgentReply({ ...value, error: { code: `${code}_MUTATED`, message: "failure" } }, req),
			).toThrow();
		}
		const cancelled = { ...done, outcome: "cancelled", stopReason: "caller_aborted", result: null, error: null };
		expect(decodeWorkflowRunAgentReply(cancelled, req).outcome).toBe("cancelled");
		expect(() => decodeWorkflowRunAgentReply({ ...cancelled, result: done.result }, req)).toThrow();
	});
});
