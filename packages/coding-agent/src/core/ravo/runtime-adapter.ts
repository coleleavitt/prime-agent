import type { Usage } from "@earendil-works/pi-ai";
import type { RunAgentHandler, RunAgentOptions, RunAgentStatus, RunAgentToolSelection } from "../run-agent.js";
import type { ChildCall, RavoChildCallOptions, RavoChildResult } from "./controller.js";

export interface ChildRuntimeScope {
	model?: string;
	tools?: RunAgentToolSelection;
	maxTurns?: number;
	tokenBudget?: number;
	/** Worker role label (`implement`, `repair`); retained runtimes use it to name the child session. */
	role?: string;
}

export interface StructuredChildSpec<TInput, TOutput> {
	prompt: (input: TInput) => string;
	validate: (value: unknown) => TOutput;
	scope?: ChildRuntimeScope;
}

export type RetainedWorkerTerminalReason =
	| "completed"
	| "empty_turn"
	| "provider_error"
	| "invalid_result"
	| "aborted"
	| "turn_limit"
	| "budget_exceeded"
	| "error";

export interface RetainedWorkerTerminalResult {
	status: RunAgentStatus;
	/** Structured artifact parsed from the terminal turn; absent unless the turn completed with one. */
	result?: unknown;
	tokens: number;
	error?: string;
	/** Terminal assistant text of the turn, when any was produced. */
	text?: string;
	usage?: Usage;
	reason?: RetainedWorkerTerminalReason;
}

export interface RetainedWorkerRuntime {
	spawn(request: RetainedWorkerRequest): Promise<{ handle: string }>;
	wait(handle: string, options: RetainedWorkerWaitOptions): Promise<RetainedWorkerTerminalResult>;
	continue(handle: string, request: RetainedWorkerRequest): Promise<void>;
}

export interface RetainedWorkerRequest extends RetainedWorkerWaitOptions {
	prompt: string;
	model?: string;
	tools: RunAgentToolSelection;
	maxTurns?: number;
	role?: string;
}

export interface RetainedWorkerWaitOptions {
	signal: AbortSignal;
	tokenBudget: number;
}

/** Adapt a terminal RunAgent call into a RAVO child call with strict JSON output. */
export function createRunAgentChildCall<TInput, TOutput>(
	runAgent: RunAgentHandler,
	spec: StructuredChildSpec<TInput, TOutput>,
): ChildCall<TInput, TOutput> {
	return async (input, options) => {
		const result = await runAgent(
			{
				prompt: spec.prompt(input),
				...(spec.scope?.model ? { model: spec.scope.model } : {}),
			},
			runOptions(spec.scope, options),
		);
		if (result.status !== "completed") return failed(result.status, result.usage.totalTokens, result.error);
		try {
			return completed(spec.validate(parseJson(result.output)), result.usage.totalTokens);
		} catch (error) {
			return failed("error", result.usage.totalTokens, errorMessage(error));
		}
	};
}

/**
 * Adapt a persistent worker runtime into a deferred RAVO child call.
 * The runtime must return a structured terminal result; transcript text is never inspected.
 */
export function createRetainedWorkerChildCall<TInput extends { workerHandle?: string }, TOutput>(
	runtime: RetainedWorkerRuntime,
	spec: StructuredChildSpec<TInput, TOutput>,
): ChildCall<TInput, TOutput> {
	return async (input, options) => {
		const request = retainedRequest(spec.prompt(input), spec.scope, options);
		const handle = input.workerHandle ?? (await runtime.spawn(request)).handle;
		if (input.workerHandle) await runtime.continue(handle, request);
		return {
			status: "deferred",
			handle,
			wait: (waitOptions) => waitForStructuredResult(runtime, handle, spec.validate, spec.scope, waitOptions),
		};
	};
}

async function waitForStructuredResult<T>(
	runtime: RetainedWorkerRuntime,
	handle: string,
	validate: (value: unknown) => T,
	scope: ChildRuntimeScope | undefined,
	options: RavoChildCallOptions,
): Promise<RavoChildResult<T>> {
	const terminal = await runtime.wait(handle, {
		signal: options.signal,
		tokenBudget: boundedTokenBudget(scope?.tokenBudget, options.tokenBudget),
	});
	if (terminal.status !== "completed") return failed(terminal.status, terminal.tokens, terminal.error);
	if (terminal.result === undefined) {
		return failed("error", terminal.tokens, "retained worker completed without a structured result");
	}
	try {
		return completed(validate(terminal.result), terminal.tokens);
	} catch (error) {
		return failed("error", terminal.tokens, errorMessage(error));
	}
}

function runOptions(scope: ChildRuntimeScope | undefined, options: RavoChildCallOptions): RunAgentOptions {
	return {
		tools: scope?.tools ?? "none",
		signal: options.signal,
		...(scope?.maxTurns === undefined ? {} : { maxTurns: scope.maxTurns }),
		tokenBudget: boundedTokenBudget(scope?.tokenBudget, options.tokenBudget),
	};
}

function retainedRequest(
	prompt: string,
	scope: ChildRuntimeScope | undefined,
	options: RavoChildCallOptions,
): RetainedWorkerRequest {
	return {
		prompt,
		tools: scope?.tools ?? "none",
		signal: options.signal,
		tokenBudget: boundedTokenBudget(scope?.tokenBudget, options.tokenBudget),
		...(scope?.model ? { model: scope.model } : {}),
		...(scope?.maxTurns === undefined ? {} : { maxTurns: scope.maxTurns }),
		...(scope?.role ? { role: scope.role } : {}),
	};
}

function boundedTokenBudget(configured: number | undefined, allocated: number): number {
	return configured === undefined ? allocated : Math.min(configured, allocated);
}

function parseJson(output: string): unknown {
	try {
		return JSON.parse(output) as unknown;
	} catch (error) {
		throw new Error(`child output is not valid JSON: ${errorMessage(error)}`);
	}
}

function completed<T>(value: T, tokens: number): RavoChildResult<T> {
	return { status: "completed", value, tokens };
}

function failed(status: Exclude<RunAgentStatus, "completed">, tokens: number, error?: string): RavoChildResult<never> {
	return { status, tokens, ...(error === undefined ? {} : { error }) };
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
