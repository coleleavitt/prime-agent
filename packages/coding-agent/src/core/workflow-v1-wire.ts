import { createHash } from "node:crypto";

export const WORKFLOW_RUN_AGENT_PROTOCOL = "prime.workflow.run-agent/v1" as const;
export const WORKFLOW_RUN_AGENT_RESULT_PROTOCOL = "prime.workflow.run-agent-result/v1" as const;
const SAFE = Number.MAX_SAFE_INTEGER;
const idPattern = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const modelPattern = /^\S+\/\S+$/;
const usageKeys = [
	"inputTokens",
	"outputTokens",
	"cacheReadTokens",
	"cacheWriteTokens",
	"totalTokens",
	"costInput",
	"costOutput",
	"costCacheRead",
	"costCacheWrite",
	"costTotal",
	"completeness",
	"finality",
] as const;
const replyKeys = [
	"protocol",
	"requestId",
	"nodeId",
	"resolvedModel",
	"turnsStarted",
	"durationMs",
	"budgetExhausted",
	"budgetOvershootTokens",
	"usage",
	"outcome",
	"stopReason",
	"result",
	"error",
] as const;
const failures: Record<string, string> = {
	model_resolution_failed: "MODEL_RESOLUTION_FAILED",
	provider_failed: "PROVIDER_FAILED",
	result_missing: "RESULT_MISSING",
	result_too_large: "RESULT_TOO_LARGE",
	usage_invalid: "USAGE_INVALID",
	unexpected_tool_call: "UNEXPECTED_TOOL_CALL",
	host_failed: "HOST_FAILED",
};
const unknowns = new Set(["host_connection_lost", "host_process_lost", "drain_timeout", "terminal_capture_ambiguous"]);

export interface WorkflowRunAgentRequest {
	protocol: typeof WORKFLOW_RUN_AGENT_PROTOCOL;
	requestId: string;
	nodeId: string;
	prompt: string;
	model: string | null;
	maxTurns: 1;
	softTokenBudget?: number | null;
	maxResultUtf8Bytes: number;
	drainTimeoutMs: number;
	tools: "none";
}
export type WorkflowUsageFinality = "final" | "known_prefix";
export interface WorkflowUsage {
	inputTokens: number;
	outputTokens: number;
	cacheReadTokens: number;
	cacheWriteTokens: number;
	totalTokens: number;
	costInput: number | null;
	costOutput: number | null;
	costCacheRead: number | null;
	costCacheWrite: number | null;
	costTotal: number | null;
	completeness: "complete_host_observation";
	finality: WorkflowUsageFinality;
}
export interface WorkflowRunAgentReply {
	protocol: typeof WORKFLOW_RUN_AGENT_RESULT_PROTOCOL;
	requestId: string;
	nodeId: string;
	resolvedModel: string | null;
	turnsStarted: 0 | 1;
	durationMs: number;
	budgetExhausted: boolean;
	budgetOvershootTokens: number;
	usage: WorkflowUsage;
	outcome: "completed" | "failed" | "cancelled" | "execution_unknown";
	stopReason: string;
	result: { text: string; utf8Bytes: number; sha256: string } | null;
	error: { code: string; message: string } | null;
}

function record(value: unknown, name: string): Record<string, unknown> {
	if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${name} must be an object`);
	return value as Record<string, unknown>;
}
function closed(value: unknown, keys: readonly string[], name: string): Record<string, unknown> {
	const out = record(value, name);
	const actual = Object.keys(out).sort();
	const expected = [...keys].sort();
	if (actual.length !== expected.length || actual.some((key, i) => key !== expected[i]))
		throw new Error(`${name} must be closed`);
	return out;
}
function integer(value: unknown, name: string, min = 0, max = SAFE): number {
	if (!Number.isSafeInteger(value) || (value as number) < min || (value as number) > max)
		throw new Error(`${name} is invalid`);
	return value as number;
}
function id(value: unknown, name: string): string {
	if (typeof value !== "string" || !idPattern.test(value)) throw new Error(`${name} is invalid`);
	return value;
}
function model(value: unknown, name: string): string | null {
	if (value !== null && (typeof value !== "string" || value.length > 512 || !modelPattern.test(value)))
		throw new Error(`${name} is invalid`);
	return value as string | null;
}

export function decodeWorkflowRunAgentRequest(value: unknown): WorkflowRunAgentRequest {
	const data = record(value, "request");
	const required = [
		"protocol",
		"requestId",
		"nodeId",
		"prompt",
		"model",
		"maxTurns",
		"maxResultUtf8Bytes",
		"drainTimeoutMs",
		"tools",
	];
	const allowed = new Set([...required, "softTokenBudget"]);
	if (!required.every((k) => Object.hasOwn(data, k)) || Object.keys(data).some((k) => !allowed.has(k)))
		throw new Error("request has missing or unknown fields");
	if (data.protocol !== WORKFLOW_RUN_AGENT_PROTOCOL || data.tools !== "none" || data.maxTurns !== 1)
		throw new Error("unsupported protocol, tools, or maxTurns");
	id(data.requestId, "requestId");
	id(data.nodeId, "nodeId");
	model(data.model, "model");
	if (typeof data.prompt !== "string" || data.prompt.length < 1 || data.prompt.length > 262144)
		throw new Error("prompt is invalid");
	if (data.softTokenBudget !== undefined && data.softTokenBudget !== null)
		integer(data.softTokenBudget, "softTokenBudget", 1, 1_000_000);
	integer(data.maxResultUtf8Bytes, "maxResultUtf8Bytes", 1, 1_048_576);
	integer(data.drainTimeoutMs, "drainTimeoutMs", 1, 30_000);
	return { ...data } as unknown as WorkflowRunAgentRequest;
}

function usage(value: unknown, finality: WorkflowUsageFinality): WorkflowUsage {
	const out = closed(value, usageKeys, "usage");
	for (const k of usageKeys.slice(0, 5)) integer(out[k], `usage.${k}`);
	for (const k of usageKeys.slice(5, 10)) {
		const v = out[k];
		if (v !== null && (typeof v !== "number" || !Number.isFinite(v) || v < 0 || v > 1e15))
			throw new Error(`usage.${k} is invalid`);
	}
	if (out.completeness !== "complete_host_observation" || out.finality !== finality)
		throw new Error("usage finality is invalid");
	return out as unknown as WorkflowUsage;
}
export function decodeWorkflowRunAgentReply(value: unknown, request: WorkflowRunAgentRequest): WorkflowRunAgentReply {
	const out = closed(value, replyKeys, "reply");
	if (
		out.protocol !== WORKFLOW_RUN_AGENT_RESULT_PROTOCOL ||
		out.requestId !== request.requestId ||
		out.nodeId !== request.nodeId
	)
		throw new Error("reply correlation mismatch");
	model(out.resolvedModel, "resolvedModel");
	integer(out.turnsStarted, "turnsStarted", 0, 1);
	integer(out.durationMs, "durationMs");
	if (typeof out.budgetExhausted !== "boolean") throw new Error("budgetExhausted is invalid");
	integer(out.budgetOvershootTokens, "budgetOvershootTokens");
	const outcome = out.outcome;
	const decodedUsage = usage(out.usage, outcome === "execution_unknown" ? "known_prefix" : "final");
	const expectedExhausted = request.softTokenBudget != null && decodedUsage.totalTokens >= request.softTokenBudget;
	const expectedOvershoot =
		request.softTokenBudget == null ? 0 : Math.max(0, decodedUsage.totalTokens - request.softTokenBudget);
	if (out.budgetExhausted !== expectedExhausted || out.budgetOvershootTokens !== expectedOvershoot) {
		throw new Error("budget semantics mismatch");
	}
	if (outcome === "completed" && out.turnsStarted !== 1) throw new Error("completed requires one started turn");
	if (outcome === "completed") {
		if (out.stopReason !== "completed" || out.error !== null) throw new Error("invalid completed reply");
		const result = closed(out.result, ["text", "utf8Bytes", "sha256"], "result");
		if (typeof result.text !== "string") throw new Error("result text is invalid");
		const bytes = Buffer.byteLength(result.text);
		if (
			bytes > request.maxResultUtf8Bytes ||
			result.utf8Bytes !== bytes ||
			result.sha256 !== createHash("sha256").update(result.text).digest("hex")
		)
			throw new Error("result authority mismatch");
	} else if (outcome === "failed") {
		const code = failures[String(out.stopReason)];
		const err = closed(out.error, ["code", "message"], "error");
		if (
			!code ||
			out.result !== null ||
			err.code !== code ||
			typeof err.message !== "string" ||
			err.message.length > 512
		)
			throw new Error("invalid failed reply");
	} else if (outcome === "cancelled") {
		if (out.stopReason !== "caller_aborted" || out.result !== null || out.error !== null)
			throw new Error("invalid cancelled reply");
	} else if (outcome === "execution_unknown") {
		const err = closed(out.error, ["code", "message"], "error");
		if (
			!unknowns.has(String(out.stopReason)) ||
			out.result !== null ||
			err.code !== "EXECUTION_UNKNOWN" ||
			typeof err.message !== "string" ||
			err.message.length > 512
		)
			throw new Error("invalid unknown reply");
	} else throw new Error("unknown workflow outcome");
	return out as unknown as WorkflowRunAgentReply;
}
