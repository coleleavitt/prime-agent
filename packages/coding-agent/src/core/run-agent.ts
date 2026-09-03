import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, Model, Usage } from "@earendil-works/pi-ai";
import type { AgentSession, AgentSessionEvent } from "./agent-session.js";

export interface RunAgentRequest {
	prompt: string;
	/** Authenticated model selector (`provider/id`). Defaults to the current model. */
	model?: string;
}

export type RunAgentToolSelection = "none" | "active" | { allow: string[] };

export interface RunAgentOptions {
	/** Child tools. An allowlist is always intersected with the parent's active tools. Default: `none`. */
	tools?: RunAgentToolSelection;
	/** Cancels the child without aborting the parent session. */
	signal?: AbortSignal;
	/** Stop after this many completed model turns. */
	maxTurns?: number;
	/** Stop after assistant usage reaches this many total tokens. */
	tokenBudget?: number;
	onProgress?: (progress: RunAgentProgress) => void;
}

export type RunAgentProgress =
	| { type: "started"; model: string }
	| { type: "turn"; turn: number; tokens: number }
	| { type: "tool"; phase: "started" | "completed"; name: string; toolCallId: string }
	| { type: "finished"; status: RunAgentStatus };

export type RunAgentStatus = "completed" | "aborted" | "turn_limit" | "budget_exceeded" | "error";

export interface RunAgentResult {
	status: RunAgentStatus;
	output: string;
	messages: AgentMessage[];
	model: string;
	turns: number;
	toolCalls: number;
	usage: Usage;
	error?: string;
}

export type RunAgentHandler = (request: RunAgentRequest, options?: RunAgentOptions) => Promise<RunAgentResult>;

export interface RunAgentSessionInput {
	session: AgentSession;
	model: Model<any>;
	request: RunAgentRequest;
	options?: RunAgentOptions;
}

const emptyUsage = (): Usage => ({
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});

function addUsage(total: Usage, usage: Usage): void {
	total.input += usage.input;
	total.output += usage.output;
	total.cacheRead += usage.cacheRead;
	total.cacheWrite += usage.cacheWrite;
	total.totalTokens += usage.totalTokens;
	total.cost.input += usage.cost.input;
	total.cost.output += usage.cost.output;
	total.cost.cacheRead += usage.cost.cacheRead;
	total.cost.cacheWrite += usage.cost.cacheWrite;
	total.cost.total += usage.cost.total;
}

function assistantText(message: AssistantMessage): string {
	return message.content
		.filter((content): content is { type: "text"; text: string } => content.type === "text")
		.map((content) => content.text)
		.join("\n");
}

function validateLimit(value: number | undefined, name: string): void {
	if (value !== undefined && (!Number.isSafeInteger(value) || value < 1)) {
		throw new Error(`${name} must be a positive integer`);
	}
}

/** Run one child session to a terminal result while presenting stable extension-facing progress. */
export async function runAgentSession(input: RunAgentSessionInput): Promise<RunAgentResult> {
	const { session, model, request, options } = input;
	validateLimit(options?.maxTurns, "maxTurns");
	validateLimit(options?.tokenBudget, "tokenBudget");
	let turns = 0;
	let toolCalls = 0;
	let limitReached = false;
	const usage = emptyUsage();
	const modelSelector = `${model.provider}/${model.id}`;
	const emitProgress = (progress: RunAgentProgress): void => {
		try {
			options?.onProgress?.(progress);
		} catch {
			// Progress observers cannot affect child execution.
		}
	};
	emitProgress({ type: "started", model: modelSelector });

	const onEvent = (event: AgentSessionEvent): void => {
		if (event.type === "turn_end") {
			turns += 1;
			if (event.message.role === "assistant") addUsage(usage, event.message.usage);
			emitProgress({ type: "turn", turn: turns, tokens: usage.totalTokens });
			if (options?.maxTurns !== undefined && turns >= options.maxTurns) {
				limitReached = true;
				void session.abort();
			} else if (options?.tokenBudget !== undefined && usage.totalTokens >= options.tokenBudget) {
				limitReached = true;
				void session.abort();
			}
		} else if (event.type === "tool_execution_start") {
			toolCalls += 1;
			emitProgress({ type: "tool", phase: "started", name: event.toolName, toolCallId: event.toolCallId });
		} else if (event.type === "tool_execution_end") {
			emitProgress({ type: "tool", phase: "completed", name: event.toolName, toolCallId: event.toolCallId });
		}
	};
	const unsubscribe = session.subscribe(onEvent);
	const abort = () => void session.abort();
	options?.signal?.addEventListener("abort", abort, { once: true });

	const limitStatus = (): RunAgentStatus => {
		if (options?.maxTurns !== undefined && turns >= options.maxTurns) return "turn_limit";
		if (options?.tokenBudget !== undefined && usage.totalTokens >= options.tokenBudget) return "budget_exceeded";
		return "aborted";
	};
	let status: RunAgentStatus = "completed";
	let error: string | undefined;
	try {
		if (options?.signal?.aborted) {
			status = "aborted";
		} else {
			await session.promptAndWait(request.prompt, {
				expandPromptTemplates: false,
				source: "extension",
				signal: options?.signal,
			});
			if (options?.signal?.aborted || limitReached) status = options?.signal?.aborted ? "aborted" : limitStatus();
		}
	} catch (caught) {
		if (options?.signal?.aborted || limitReached) {
			status = options?.signal?.aborted ? "aborted" : limitStatus();
		} else {
			status = "error";
			error = caught instanceof Error ? caught.message : String(caught);
		}
	} finally {
		options?.signal?.removeEventListener("abort", abort);
		unsubscribe();
	}

	const messages = [...session.messages];
	const lastAssistant = messages
		.slice()
		.reverse()
		.find((message): message is AssistantMessage => message.role === "assistant");
	if (lastAssistant?.stopReason === "error" && status === "completed") {
		status = "error";
		error = lastAssistant.errorMessage ?? "Agent request failed";
	} else if (lastAssistant?.stopReason === "aborted" && status === "completed") {
		status = "aborted";
	}
	const result: RunAgentResult = {
		status,
		output: lastAssistant ? assistantText(lastAssistant) : "",
		messages,
		model: modelSelector,
		turns,
		toolCalls,
		usage,
		...(error === undefined ? {} : { error }),
	};
	emitProgress({ type: "finished", status });
	return result;
}
