import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, Usage } from "@earendil-works/pi-ai";
import { AGENT_MESSAGE_CUSTOM_TYPE, type AgentSessionMessage } from "../agent-messages.js";
import type { AgentSession, RetainedRlmChildHandle } from "../agent-session.js";
import { assessRlmChildSettlement } from "../rlm-child-settlement.js";
import {
	type RunAgentStatus,
	type RunAgentToolSelection,
	resolveRunAgentTools,
	runAgentSession,
} from "../run-agent.js";
import type {
	RetainedWorkerRequest,
	RetainedWorkerRuntime,
	RetainedWorkerTerminalReason,
	RetainedWorkerTerminalResult,
	RetainedWorkerWaitOptions,
} from "./runtime-adapter.js";

/**
 * Retained worker runtime backed by resident AgentSession RLM children.
 *
 * Each handle is one child session created through `AgentSession.createRetainedRlmChild`.
 * `spawn` and `continue` each run exactly one prompt turn in that same session, so a
 * repair turn sees the implement turn's transcript. Every turn is classified with
 * `assessRlmChildSettlement`: a provider error, an abort, or an empty nominal stop is
 * never reported as `completed`.
 */
export interface AgentSessionRetainedWorkerRuntime extends RetainedWorkerRuntime {
	/** Abort the in-flight turn, if any. The child stays resident; `wait` reports `aborted`. */
	cancel(handle: string, reason?: string): boolean;
	/** Re-attach a handle from a checkpoint after the runtime (or the parent process) restarted. */
	recover(handle: string): Promise<{ handle: string }>;
	/** Delete the child session and forget the handle. */
	release(handle: string): Promise<void>;
	/** Handles currently tracked by this runtime instance. */
	handles(): string[];
}

export interface RetainedWorkerRuntimeOptions {
	/**
	 * Turn the terminal assistant text into the structured result. Defaults to strict
	 * `JSON.parse`; throwing marks the turn `error`/`invalid_result`.
	 */
	parseResult?: (text: string) => unknown;
	/** Session name for a new worker. Defaults to `ravo-<role>-<n>` (or `ravo-worker-<n>`). */
	sessionName?: (input: { role?: string; ordinal: number }) => string;
}

interface ActiveTurn {
	promise: Promise<RetainedWorkerTerminalResult>;
	abort: AbortController;
	/** Extra token caps registered by waiters; the tightest one aborts the turn. */
	caps: number[];
	tokens: number;
	capHit: boolean;
	cancelReason?: string;
}

interface WorkerRecord {
	id: string;
	child: RetainedRlmChildHandle;
	/**
	 * Tools the spawn scope granted; a follow-up may only narrow them. Unknown after a
	 * disk reopen, where the next explicit follow-up scope becomes the grant.
	 */
	grantedTools?: string[];
	turn: number;
	lastMarkerId?: string;
	active?: ActiveTurn;
	last?: RetainedWorkerTerminalResult;
	released: boolean;
}

const RETAINED_WORKER_TASK_PREFIX = "[task from parent]\n\n";

export function createAgentSessionRetainedWorkerRuntime(
	parent: AgentSession,
	options: RetainedWorkerRuntimeOptions = {},
): AgentSessionRetainedWorkerRuntime {
	const workers = new Map<string, WorkerRecord>();
	const parseResult = options.parseResult ?? parseJsonResult;
	let spawned = 0;

	const record = (handle: string): WorkerRecord => {
		const worker = workers.get(handle);
		if (!worker || worker.released) throw new Error(`Unknown retained worker: ${handle}`);
		return worker;
	};

	const startTurn = (worker: WorkerRecord, request: RetainedWorkerRequest): ActiveTurn => {
		if (worker.active) throw new Error(`Retained worker ${worker.id} already has a turn in flight`);
		validateBudget(request.tokenBudget, "tokenBudget");
		if (!request.prompt.trim()) throw new Error("retained worker prompt must not be empty");
		const child = worker.child.session;
		worker.turn += 1;
		const markerId = worker.turn === 1 ? `spawn:${worker.id}` : `retained:${worker.id}:${worker.turn}`;
		worker.lastMarkerId = markerId;
		const abort = new AbortController();
		const active: ActiveTurn = { promise: Promise.resolve(EMPTY_RESULT), abort, caps: [], tokens: 0, capHit: false };
		const unlink = linkAbort(abort, [request.signal, parent.agent.signal]);
		const message: AgentSessionMessage = {
			role: "custom",
			customType: AGENT_MESSAGE_CUSTOM_TYPE,
			content: `${RETAINED_WORKER_TASK_PREFIX}${request.prompt}`,
			display: true,
			details: {
				id: markerId,
				message: request.prompt,
				from: { sessionId: parent.sessionId, sessionName: parent.sessionName },
				fromRelationship: "parent",
			},
			timestamp: Date.now(),
		};
		active.promise = (async (): Promise<RetainedWorkerTerminalResult> => {
			try {
				const run = await runAgentSession({
					session: child,
					model: worker.child.model,
					request: { prompt: message.content },
					promptMessage: message,
					options: {
						signal: abort.signal,
						tokenBudget: request.tokenBudget,
						...(request.maxTurns === undefined ? {} : { maxTurns: request.maxTurns }),
						onProgress: (progress) => {
							if (progress.type !== "turn") return;
							active.tokens = progress.tokens;
							if (active.caps.some((cap) => progress.tokens >= cap)) {
								active.capHit = true;
								abort.abort();
							}
						},
					},
				});
				const result = classifyTurn(child, markerId, run.status, run.usage, run.error, active, parseResult);
				worker.last = result;
				return result;
			} finally {
				unlink();
				worker.active = undefined;
			}
		})();
		worker.active = active;
		return active;
	};

	const attach = (child: RetainedRlmChildHandle, grantedTools: string[] | undefined): WorkerRecord => {
		const worker: WorkerRecord = {
			id: child.id,
			child,
			...(grantedTools ? { grantedTools } : {}),
			turn: countMarkers(child.session.messages, child.id),
			released: false,
		};
		worker.lastMarkerId = worker.turn === 0 ? undefined : markerFor(child.id, worker.turn);
		workers.set(child.id, worker);
		return worker;
	};

	return {
		async spawn(request) {
			validateBudget(request.tokenBudget, "tokenBudget");
			if (!request.prompt.trim()) throw new Error("retained worker prompt must not be empty");
			if (request.signal.aborted) throw new Error("retained worker spawn was cancelled");
			spawned += 1;
			const sessionName =
				options.sessionName?.({ ...(request.role ? { role: request.role } : {}), ordinal: spawned }) ??
				`ravo-${request.role ?? "worker"}-${spawned}`;
			const child = await parent.createRetainedRlmChild({
				prompt: request.prompt,
				tools: request.tools,
				sessionName,
				...(request.model ? { model: request.model } : {}),
			});
			const worker = attach(child, child.session.getActiveToolNames());
			try {
				startTurn(worker, request);
			} catch (error) {
				await release(worker).catch(() => undefined);
				throw error;
			}
			return { handle: worker.id };
		},

		async continue(handle, request) {
			const worker = record(handle);
			if (worker.active) throw new Error(`Retained worker ${handle} already has a turn in flight`);
			if (request.model !== undefined) assertSameModel(worker, request.model);
			applyScope(parent, worker, request.tools);
			startTurn(worker, request);
		},

		async wait(handle, waitOptions) {
			validateBudget(waitOptions.tokenBudget, "tokenBudget");
			const worker = record(handle);
			const active = worker.active;
			if (active) {
				active.caps.push(waitOptions.tokenBudget);
				if (active.tokens >= waitOptions.tokenBudget) {
					active.capHit = true;
					active.abort.abort();
				}
				const unlink = linkAbort(active.abort, [waitOptions.signal]);
				try {
					return await active.promise;
				} finally {
					unlink();
				}
			}
			if (worker.last) return worker.last;
			if (worker.lastMarkerId) return settleDetached(worker, waitOptions, parseResult);
			throw new Error(`Retained worker ${handle} has no turn to wait for`);
		},

		cancel(handle, reason) {
			const worker = workers.get(handle);
			const active = worker?.active;
			if (!worker || worker.released || !active) return false;
			active.cancelReason = reason ?? "Cancelled by controller";
			active.abort.abort();
			return true;
		},

		async recover(handle) {
			const existing = workers.get(handle);
			if (existing && !existing.released) return { handle };
			const child = await parent.reopenRetainedRlmChild(handle);
			if (!child) {
				const listed = (await parent.listRlmSubagents()).subagents.some((entry) => entry.rlm_child_id === handle);
				throw new Error(
					listed
						? `Retained worker ${handle} is listed but not resident; hydrate it before recovery`
						: `Unknown retained worker: ${handle}`,
				);
			}
			attach(child, child.resident ? child.session.getActiveToolNames() : undefined);
			return { handle };
		},

		async release(handle) {
			const worker = workers.get(handle);
			if (!worker || worker.released) return;
			await release(worker);
		},

		handles() {
			return [...workers.values()].filter((worker) => !worker.released).map((worker) => worker.id);
		},
	};

	async function release(worker: WorkerRecord): Promise<void> {
		worker.released = true;
		const active = worker.active;
		if (active) {
			active.cancelReason = "Released by controller";
			active.abort.abort();
			await active.promise.catch(() => undefined);
		}
		workers.delete(worker.id);
		await parent.deleteRlmSubagent(worker.id);
	}
}

const EMPTY_RESULT: RetainedWorkerTerminalResult = { status: "error", tokens: 0, error: "turn did not start" };

function markerFor(id: string, turn: number): string {
	return turn === 1 ? `spawn:${id}` : `retained:${id}:${turn}`;
}

function countMarkers(messages: readonly AgentMessage[], id: string): number {
	let count = 0;
	for (const message of messages) {
		if (message.role !== "custom" || message.customType !== AGENT_MESSAGE_CUSTOM_TYPE) continue;
		const markerId = (message as { details?: { id?: unknown } }).details?.id;
		if (markerId === `spawn:${id}` || (typeof markerId === "string" && markerId.startsWith(`retained:${id}:`))) {
			count += 1;
		}
	}
	return count;
}

function applyScope(parent: AgentSession, worker: WorkerRecord, selection: RunAgentToolSelection): void {
	const requested = resolveRunAgentTools(parent.getActiveToolNames(), selection);
	const granted = worker.grantedTools;
	const effective = granted ? requested.filter((name) => granted.includes(name)) : requested;
	worker.grantedTools = granted ?? effective;
	const current = worker.child.session.getActiveToolNames();
	if (current.length === effective.length && current.every((name) => effective.includes(name))) return;
	worker.child.session.setActiveToolsByName(effective);
}

function assertSameModel(worker: WorkerRecord, requested: string): void {
	const model = worker.child.model;
	const current = `${model.provider}/${model.id}`;
	if (requested.toLowerCase() !== current.toLowerCase() && requested.toLowerCase() !== model.id.toLowerCase()) {
		throw new Error(`Retained worker ${worker.id} runs ${current}; a follow-up cannot switch it to ${requested}`);
	}
}

/**
 * Settle a recovered worker whose turn promise this runtime never owned: wait for the
 * child to go idle, then judge its transcript after the last marker exactly like a
 * live turn. Usage is unknown on this path and reported as zero tokens.
 */
async function settleDetached(
	worker: WorkerRecord,
	waitOptions: RetainedWorkerWaitOptions,
	parseResult: (text: string) => unknown,
): Promise<RetainedWorkerTerminalResult> {
	const child = worker.child.session;
	const markerId = worker.lastMarkerId;
	if (!markerId) throw new Error(`Retained worker ${worker.id} has no turn to wait for`);
	if (child.isSessionActive) {
		if (waitOptions.signal.aborted) void child.abort();
		const onAbort = () => void child.abort();
		waitOptions.signal.addEventListener("abort", onAbort, { once: true });
		try {
			await child.waitForHeadlessIdle();
		} finally {
			waitOptions.signal.removeEventListener("abort", onAbort);
		}
	}
	const status: RunAgentStatus = waitOptions.signal.aborted ? "aborted" : "completed";
	const result = classifyTurn(child, markerId, status, undefined, undefined, undefined, parseResult);
	worker.last = result;
	return result;
}

function classifyTurn(
	child: AgentSession,
	markerId: string,
	status: RunAgentStatus,
	usage: Usage | undefined,
	runError: string | undefined,
	active: ActiveTurn | undefined,
	parseResult: (text: string) => unknown,
): RetainedWorkerTerminalResult {
	const tokens = usage?.totalTokens ?? 0;
	const terminal = terminalAssistant(child.messages, markerId);
	const text = assistantText(terminal);
	const base = { tokens, ...(usage ? { usage } : {}), ...(text === undefined ? {} : { text }) };
	if (status !== "completed") {
		const finalStatus: RunAgentStatus = active?.capHit && status === "aborted" ? "budget_exceeded" : status;
		const reason: RetainedWorkerTerminalReason = finalStatus;
		const fallback =
			finalStatus === "budget_exceeded"
				? "retained worker exceeded its token budget"
				: finalStatus === "turn_limit"
					? "retained worker reached its turn limit"
					: "retained worker turn was aborted";
		return { ...base, status: finalStatus, reason, error: runError ?? active?.cancelReason ?? fallback };
	}
	const settlement = assessRlmChildSettlement({
		messages: child.messages,
		spawnMessageId: markerId,
		repliedToParent: false,
	});
	if (!settlement.ok) {
		const reason: RetainedWorkerTerminalReason =
			terminal?.stopReason === "error"
				? "provider_error"
				: terminal?.stopReason === "aborted"
					? "aborted"
					: "empty_turn";
		return {
			...base,
			status: reason === "aborted" ? "aborted" : "error",
			reason,
			error: settlement.reason,
		};
	}
	if (text === undefined || text.trim().length === 0) {
		return { ...base, status: "error", reason: "empty_turn", error: "retained worker ended without terminal text" };
	}
	try {
		return { ...base, status: "completed", reason: "completed", result: parseResult(text) };
	} catch (error) {
		return {
			...base,
			status: "error",
			reason: "invalid_result",
			error: error instanceof Error ? error.message : String(error),
		};
	}
}

function terminalAssistant(messages: readonly AgentMessage[], markerId: string): AssistantMessage | undefined {
	let start = -1;
	for (let i = messages.length - 1; i >= 0; i--) {
		const message = messages[i];
		if (message?.role === "custom" && (message as { details?: { id?: unknown } }).details?.id === markerId) {
			start = i;
			break;
		}
	}
	for (let i = messages.length - 1; i > start; i--) {
		const message = messages[i];
		if (message?.role === "assistant") return message as AssistantMessage;
	}
	return undefined;
}

function assistantText(message: AssistantMessage | undefined): string | undefined {
	if (!message) return undefined;
	return message.content
		.filter((block): block is { type: "text"; text: string } => block.type === "text")
		.map((block) => block.text)
		.join("\n");
}

function parseJsonResult(text: string): unknown {
	try {
		return JSON.parse(text) as unknown;
	} catch (error) {
		throw new Error(
			`retained worker output is not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
}

function validateBudget(value: number, name: string): void {
	if (!Number.isSafeInteger(value) || value < 1) throw new Error(`${name} must be a positive integer`);
}

function linkAbort(target: AbortController, sources: readonly (AbortSignal | undefined)[]): () => void {
	const forward = () => target.abort();
	const linked: AbortSignal[] = [];
	for (const signal of sources) {
		if (!signal) continue;
		if (signal.aborted) {
			target.abort();
			continue;
		}
		signal.addEventListener("abort", forward, { once: true });
		linked.push(signal);
	}
	return () => {
		for (const signal of linked) signal.removeEventListener("abort", forward);
	};
}
