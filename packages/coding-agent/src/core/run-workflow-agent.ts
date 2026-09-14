import { createHash } from "node:crypto";
import { Agent, type AgentEvent, type StreamFn } from "@earendil-works/pi-agent-core";
import {
	type AssistantMessage,
	createAssistantMessageEventStream,
	type Model,
	type SimpleStreamOptions,
	type Usage,
} from "@earendil-works/pi-ai";

const MAX_RESULT_UTF8_BYTES = 1024 * 1024;
const EMPTY_TOOLS = Object.freeze([]);
const EXECUTION_UNKNOWN_ERROR = "EXECUTION_UNKNOWN";

type FailureReason =
	| "provider_failed"
	| "result_missing"
	| "result_too_large"
	| "usage_invalid"
	| "unexpected_tool_call"
	| "host_failed";

type UnknownReason = "drain_timeout" | "terminal_capture_ambiguous";

export interface WorkflowAgentUsage {
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
	finality: "final" | "known_prefix";
}

export interface WorkflowAgentResultValue {
	text: string;
	utf8Bytes: number;
	sha256: string;
}

export type WorkflowAgentResult =
	| {
			outcome: "completed";
			stopReason: "completed";
			result: WorkflowAgentResultValue;
			usage: WorkflowAgentUsage;
			turnsStarted: 1;
	  }
	| { outcome: "failed"; reason: FailureReason; error: string; usage: WorkflowAgentUsage; turnsStarted: 0 | 1 }
	| { outcome: "cancelled"; usage: WorkflowAgentUsage; turnsStarted: 0 | 1 }
	| {
			outcome: "execution_unknown";
			reason: UnknownReason;
			error: typeof EXECUTION_UNKNOWN_ERROR;
			usage: WorkflowAgentUsage;
			turnsStarted: 0 | 1;
	  };

export interface RunWorkflowAgentInput {
	prompt: string;
	model: Model<any>;
	streamFn: StreamFn;
	getApiKey?: (provider: string) => Promise<string | undefined> | string | undefined;
	headers?: Record<string, string>;
	signal?: AbortSignal;
	drainTimeoutMs: number;
	maxResultUtf8Bytes?: number;
}

const zeroUsage = (finality: WorkflowAgentUsage["finality"]): WorkflowAgentUsage => ({
	inputTokens: 0,
	outputTokens: 0,
	cacheReadTokens: 0,
	cacheWriteTokens: 0,
	totalTokens: 0,
	costInput: 0,
	costOutput: 0,
	costCacheRead: 0,
	costCacheWrite: 0,
	costTotal: 0,
	completeness: "complete_host_observation",
	finality,
});

function finiteNonnegative(value: number): boolean {
	return Number.isFinite(value) && Number.isSafeInteger(value) && value >= 0;
}

function finiteNonnegativeCost(value: number): boolean {
	return Number.isFinite(value) && value >= 0 && value <= 1e15;
}

function captureUsage(
	source: Usage | undefined,
	finality: WorkflowAgentUsage["finality"],
): WorkflowAgentUsage | undefined {
	if (!source) return zeroUsage(finality);
	const tokens = [source.input, source.output, source.cacheRead, source.cacheWrite, source.totalTokens];
	const costs = [
		source.cost.input,
		source.cost.output,
		source.cost.cacheRead,
		source.cost.cacheWrite,
		source.cost.total,
	];
	if (!tokens.every(finiteNonnegative) || !costs.every(finiteNonnegativeCost)) return undefined;
	return {
		inputTokens: source.input,
		outputTokens: source.output,
		cacheReadTokens: source.cacheRead,
		cacheWriteTokens: source.cacheWrite,
		totalTokens: source.totalTokens,
		costInput: source.cost.input,
		costOutput: source.cost.output,
		costCacheRead: source.cost.cacheRead,
		costCacheWrite: source.cost.cacheWrite,
		costTotal: source.cost.total,
		completeness: "complete_host_observation",
		finality,
	};
}

function boundedError(value: unknown): string {
	const text = value instanceof Error ? value.message : String(value);
	return Buffer.from(text, "utf8").subarray(0, 1024).toString("utf8");
}

function captureAssistantText(
	message: AssistantMessage,
	limit: number,
): { text: string; utf8Bytes: number; sha256: string } | undefined {
	const hash = createHash("sha256");
	const chunks: string[] = [];
	let utf8Bytes = 0;
	for (const part of message.content) {
		if (part.type !== "text") continue;
		const bytes = Buffer.byteLength(part.text, "utf8");
		if (bytes > limit - utf8Bytes) return undefined;
		utf8Bytes += bytes;
		hash.update(part.text, "utf8");
		chunks.push(part.text);
	}
	return { text: chunks.join(""), utf8Bytes, sha256: hash.digest("hex") };
}

/** Package-private, process-local one-turn inference boundary for Workflow V1. */
export async function runWorkflowAgent(input: RunWorkflowAgentInput): Promise<WorkflowAgentResult> {
	const resultLimit = input.maxResultUtf8Bytes ?? MAX_RESULT_UTF8_BYTES;
	if (!Number.isSafeInteger(resultLimit) || resultLimit < 1 || resultLimit > MAX_RESULT_UTF8_BYTES) {
		throw new Error("maxResultUtf8Bytes must be a positive safe integer no greater than 1048576");
	}
	if (!Number.isSafeInteger(input.drainTimeoutMs) || input.drainTimeoutMs < 1) {
		throw new Error("drainTimeoutMs must be a positive safe integer");
	}

	let turnsStarted: 0 | 1 = 0;
	let terminal: AssistantMessage | undefined;
	let terminalAmbiguous = false;
	let unexpectedToolCall = false;
	let physicalRequests = 0;
	let providerDrain: Promise<void> = Promise.resolve();
	let winner: "terminal" | "abort" | undefined;
	const linkedAbort = new AbortController();
	const abort = () => {
		winner ??= "abort";
		linkedAbort.abort();
	};
	input.signal?.addEventListener("abort", abort, { once: true });
	if (input.signal?.aborted) abort();

	const streamFn: StreamFn = async (model, context, options) => {
		physicalRequests += 1;
		turnsStarted = 1;
		if (physicalRequests !== 1) throw new Error("Workflow V1 attempted more than one provider request");
		const providerOptions: SimpleStreamOptions & { maxRetries: 0 } = {
			...options,
			...(input.headers ? { headers: { ...options?.headers, ...input.headers } } : {}),
			maxRetries: 0,
		};
		const source = await input.streamFn(model, context, providerOptions);
		const guarded = createAssistantMessageEventStream();
		providerDrain = (async () => {
			for await (const event of source) {
				if (event.type !== "done" && event.type !== "error") {
					guarded.push(event);
					continue;
				}
				const message = event.type === "done" ? event.message : event.error;
				if (!message.content.some((part) => part.type === "toolCall")) {
					guarded.push(event);
					continue;
				}
				unexpectedToolCall = true;
				const rejected: AssistantMessage = {
					...message,
					content: message.content.filter((part) => part.type !== "toolCall"),
					stopReason: "error",
					errorMessage: "Unexpected tool call",
				};
				guarded.push({ type: "error", reason: "error", error: rejected });
			}
		})().catch((error: unknown) => {
			const failed: AssistantMessage = {
				role: "assistant",
				content: [],
				api: model.api,
				provider: model.provider,
				model: model.id,
				stopReason: "error",
				errorMessage: boundedError(error),
				usage: {
					input: 0,
					output: 0,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 0,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
				timestamp: Date.now(),
			};
			guarded.push({ type: "error", reason: "error", error: failed });
		});
		return guarded;
	};
	const agent = new Agent({
		initialState: { model: input.model, tools: EMPTY_TOOLS as unknown as [] },
		streamFn,
		getApiKey: input.getApiKey,
		shouldStopAfterTurn: () => true,
		toolCallPolicy: "reject",
	});
	Object.freeze(agent.state.tools);
	const unsubscribe = agent.subscribe((event: AgentEvent) => {
		if (event.type === "turn_start") turnsStarted = 1;
		if (event.type !== "message_end" || event.message.role !== "assistant") return;
		winner ??= "terminal";
		if (terminal) terminalAmbiguous = true;
		terminal = event.message;
		if (event.message.content.some((part) => part.type === "toolCall")) {
			unexpectedToolCall = true;
			agent.abort();
		}
	});

	let promptFailure: unknown;
	const prompt = (async () => {
		if (linkedAbort.signal.aborted) return;
		try {
			await agent.prompt(input.prompt);
		} catch (error) {
			promptFailure = error;
		}
		await agent.waitForIdle();
		await providerDrain;
	})();
	const onAbort = () => agent.abort();
	linkedAbort.signal.addEventListener("abort", onAbort, { once: true });

	let drained = false;
	let promptSettled = false;
	void prompt.finally(() => {
		promptSettled = true;
	});
	const abortObserved = new Promise<void>((resolve) => {
		if (linkedAbort.signal.aborted) resolve();
		else linkedAbort.signal.addEventListener("abort", () => resolve(), { once: true });
	});
	try {
		await Promise.race([prompt, abortObserved]);
		if (!promptSettled) {
			let timer: NodeJS.Timeout | undefined;
			await Promise.race([
				prompt,
				new Promise<void>((resolve) => {
					timer = setTimeout(resolve, input.drainTimeoutMs);
				}),
			]);
			if (timer) clearTimeout(timer);
		}
		drained = promptSettled;
	} finally {
		linkedAbort.signal.removeEventListener("abort", onAbort);
		input.signal?.removeEventListener("abort", abort);
		unsubscribe();
	}

	const provisionalUsage =
		captureUsage(terminal?.usage, drained ? "final" : "known_prefix") ??
		zeroUsage(drained ? "final" : "known_prefix");
	if (!drained) {
		agent.abort();
		void prompt.catch(() => undefined);
		return {
			outcome: "execution_unknown",
			reason: "drain_timeout",
			error: EXECUTION_UNKNOWN_ERROR,
			usage: provisionalUsage,
			turnsStarted,
		};
	}
	const usage = captureUsage(terminal?.usage, "final");
	if (!usage)
		return {
			outcome: "failed",
			reason: "usage_invalid",
			error: "Invalid provider usage",
			usage: zeroUsage("final"),
			turnsStarted,
		};
	if (terminalAmbiguous) {
		return {
			outcome: "execution_unknown",
			reason: "terminal_capture_ambiguous",
			error: EXECUTION_UNKNOWN_ERROR,
			usage: { ...usage, finality: "known_prefix" },
			turnsStarted,
		};
	}
	if (winner === "abort" || terminal?.stopReason === "aborted") return { outcome: "cancelled", usage, turnsStarted };
	if (promptFailure)
		return { outcome: "failed", reason: "host_failed", error: boundedError(promptFailure), usage, turnsStarted };
	if (!terminal)
		return {
			outcome: "failed",
			reason: "result_missing",
			error: "Authoritative terminal result missing",
			usage,
			turnsStarted,
		};
	if (unexpectedToolCall)
		return { outcome: "failed", reason: "unexpected_tool_call", error: "Unexpected tool call", usage, turnsStarted };
	if (terminal.stopReason === "error") {
		return {
			outcome: "failed",
			reason: "provider_failed",
			error: boundedError(terminal.errorMessage ?? "Provider failed"),
			usage,
			turnsStarted,
		};
	}
	const captured = captureAssistantText(terminal, resultLimit);
	if (!captured) {
		return {
			outcome: "failed",
			reason: "result_too_large",
			error: "Result exceeds maxResultUtf8Bytes",
			usage,
			turnsStarted,
		};
	}
	return { outcome: "completed", stopReason: "completed", result: captured, usage, turnsStarted: 1 };
}
