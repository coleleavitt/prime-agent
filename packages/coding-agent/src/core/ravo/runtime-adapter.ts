import type { ThinkingLevel } from "@earendil-works/pi-agent-core";
import type { Usage } from "@earendil-works/pi-ai";
import type {
	RunAgentHandler,
	RunAgentOptions,
	RunAgentRequest,
	RunAgentStatus,
	RunAgentToolSelection,
} from "../run-agent.js";
import type { ChildCall, RavoChildCallOptions, RavoChildResult } from "./controller.js";

export interface ChildRuntimeScope {
	model?: string;
	tools?: RunAgentToolSelection;
	maxTurns?: number;
	tokenBudget?: number;
	/** Worker role label (`implement`, `repair`); retained runtimes use it to name the child session. */
	role?: string;
	/** Child thinking level (`RunAgentRequest.thinkingLevel`); absent, the child inherits the parent's. */
	thinkingLevel?: ThinkingLevel;
	/** Visible-answer cap per model call (`RunAgentOptions.maxOutputTokens`). */
	maxOutputTokens?: number;
}

/** The top-level JSON container a lenient extraction looks for. */
export type JsonContainer = "object" | "array";

export interface StructuredChildSpec<TInput, TOutput> {
	prompt: (input: TInput) => string;
	validate: (value: unknown) => TOutput;
	scope?: ChildRuntimeScope;
	/**
	 * Absent (RAVO's default), the child's whole output must be JSON. When set,
	 * the output is searched for the largest balanced JSON value of this kind
	 * (`extractJsonValue`), so markdown code fences and prose before or after the
	 * value are tolerated. Dream-RSI opts its children in; RAVO stays strict.
	 */
	extractJson?: JsonContainer;
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
			structuredChildRequest(spec.prompt(input), spec.scope),
			runOptions(spec.scope, options),
		);
		if (result.status !== "completed") return failed(result.status, result.usage.totalTokens, result.error);
		try {
			const value = spec.extractJson ? extractJsonValue(result.output, spec.extractJson) : parseJson(result.output);
			return completed(spec.validate(value), result.usage.totalTokens);
		} catch (error) {
			return failed("error", result.usage.totalTokens, errorMessage(error));
		}
	};
}

/** The `RunAgentRequest` a structured child is prompted with: the prompt plus the scope's model and thinking level, when any. */
export function structuredChildRequest(prompt: string, scope: ChildRuntimeScope | undefined): RunAgentRequest {
	return {
		prompt,
		...(scope?.model ? { model: scope.model } : {}),
		...(scope?.thinkingLevel === undefined ? {} : { thinkingLevel: scope.thinkingLevel }),
	};
}

/** The `RunAgentOptions` a structured child runs under: the scope's tools, turn and output caps, and the bounded token budget. */
export function structuredChildRunOptions(
	scope: ChildRuntimeScope | undefined,
	options: RavoChildCallOptions,
): RunAgentOptions {
	return runOptions(scope, options);
}

/**
 * Lenient JSON extraction for a child that was asked for one JSON value but may
 * have wrapped it in markdown code fences or prose. The whole output is tried
 * first; otherwise every `{` (or `[`) is a candidate start, the balanced close
 * is found by a string-aware scan, and the LARGEST candidate that parses wins,
 * so a small JSON fragment mentioned in an explanation never shadows the answer
 * and a nested value never shadows its parent. Throws when no value of the
 * requested kind parses (e.g. output truncated mid-object).
 */
export function extractJsonValue(output: string, container: JsonContainer): unknown {
	const text = output.trim();
	const isWanted = (value: unknown): boolean =>
		container === "array"
			? Array.isArray(value)
			: typeof value === "object" && value !== null && !Array.isArray(value);
	try {
		const whole = JSON.parse(text) as unknown;
		if (isWanted(whole)) return whole;
	} catch {
		// Not bare JSON; scan for an embedded value.
	}
	const opener = container === "array" ? "[" : "{";
	let best: { value: unknown; length: number } | undefined;
	let start = text.indexOf(opener);
	while (start !== -1) {
		const end = balancedJsonEnd(text, start);
		let next = start + 1;
		if (end !== -1) {
			try {
				const value = JSON.parse(text.slice(start, end)) as unknown;
				if (isWanted(value) && (best === undefined || end - start > best.length)) {
					best = { value, length: end - start };
				}
				next = end;
			} catch {
				// Not JSON from this opener; the next opener may be.
			}
		}
		start = text.indexOf(opener, next);
	}
	if (best === undefined) throw new Error(`child output contains no JSON ${container}`);
	return best.value;
}

/** Index just past the bracket that balances the one at `start`, skipping brackets inside strings; -1 when unbalanced. */
function balancedJsonEnd(text: string, start: number): number {
	let depth = 0;
	let inString = false;
	let escaped = false;
	for (let i = start; i < text.length; i++) {
		const ch = text[i];
		if (inString) {
			if (escaped) escaped = false;
			else if (ch === "\\") escaped = true;
			else if (ch === '"') inString = false;
			continue;
		}
		if (ch === '"') inString = true;
		else if (ch === "{" || ch === "[") depth += 1;
		else if (ch === "}" || ch === "]") {
			depth -= 1;
			if (depth === 0) return i + 1;
		}
	}
	return -1;
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
		...(scope?.maxOutputTokens === undefined ? {} : { maxOutputTokens: scope.maxOutputTokens }),
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
