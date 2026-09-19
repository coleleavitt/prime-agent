/**
 * DreamRunService: the in-session, background wrapper around the async Dream-RSI
 * driver, modelled on `RavoRunService`. It owns a single run at a time, relays
 * an external abort into the run, and pushes a `DreamRunStatus` snapshot through
 * `onUpdate` at each phase boundary so the human `/dream` command, the bundled
 * `dream` skill, and the Agents View all observe the same run.
 *
 * It opens NO span of its own: `runDreamLoopWithAgent` already mints the
 * detached-root `dream.run` span (carrying the launching turn's
 * `trigger.trace_id`) and ends it on success, abort and error, so the loop
 * outliving the turn is already a detached root, not a child that outlives its
 * parent. `start()` is invoked synchronously inside the turn so
 * `currentTraceContext()` still supplies that trigger id.
 *
 * Soundness is unchanged from the local path: with `llmProposer`/`llmDreamer`
 * false (the default) the driver runs the local zero-token proposer and never
 * calls `runAgent`; the LLM path parses every dreamed policy through the strict
 * `parseExplorationPolicy`, which is DATA, never executed. Determinism flows
 * through the injected `SeededRng` and clock.
 */

import { randomUUID } from "node:crypto";
import type { Model } from "@earendil-works/pi-ai";
import type { ChildRuntimeScope } from "../ravo/runtime-adapter.js";
import type { RunAgentHandler } from "../run-agent.js";
import { type DreamProgressEvent, isDreamAbortError, runDreamLoopWithAgent } from "./llm.js";
import type { DreamLoopResult } from "./loop.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import type { DreamTaskId } from "./task.js";
import { resolveTask } from "./tasks/index.js";

/** Why a Dream-RSI run ended. `completed` covers both improved and no-improvement finishes. */
export type DreamStopReason = "completed" | "cancelled" | "error";

/** A snapshot of one Dream-RSI run. Every field is a scalar so it is safe on a span attr and a wire frame. */
export type DreamRunStatus = {
	runId: string;
	phase: "idle" | "rollout" | "dreaming" | "redeploying" | "accepted" | "stopped";
	task: DreamTaskId;
	iteration: number;
	bestNodeScore: number;
	finalPolicyScore?: number;
	improved?: boolean;
	stopReason?: DreamStopReason;
	startedAt: number;
	updatedAt: number;
	/** Set when the run ended with an unexpected error instead of a clean stop. */
	error?: string;
};

export type DreamRunEvent = { type: "dream_run_update"; status: DreamRunStatus };

export interface DreamRunRequest {
	task: DreamTaskId;
	n?: number;
	seed?: number;
	workers?: number;
	k1?: number;
	k2?: number;
	dreams?: number;
	iterations?: number;
	llmProposer?: boolean;
	llmDreamer?: boolean;
}

export interface DreamRunServiceDeps {
	runAgent: RunAgentHandler;
	model?: Model<any>;
	/** The dream store directory trees and blobs are written under. */
	dir: string;
	now?: () => number;
	/** Injected rng; defaults to a fresh seeded rng from the request seed. */
	rng?: SeededRng;
	onUpdate: (status: DreamRunStatus) => void;
}

/** Defaults mirror the standalone `prime-agent dream` CLI so `/dream` and the CLI agree. */
export const DREAM_RUN_DEFAULTS = {
	seed: 1,
	workers: 4,
	k1: 12,
	k2: 24,
	dreams: 16,
	iterations: 3,
} as const;

export class DreamRunService {
	readonly #deps: DreamRunServiceDeps;
	#status: DreamRunStatus | undefined;
	#abort: AbortController | undefined;
	#running = false;

	constructor(deps: DreamRunServiceDeps) {
		this.#deps = deps;
	}

	get running(): boolean {
		return this.#running;
	}

	status(): DreamRunStatus | undefined {
		return this.#status ? structuredClone(this.#status) : undefined;
	}

	cancel(): boolean {
		if (!this.#running || !this.#abort) return false;
		this.#abort.abort();
		return true;
	}

	async start(request: DreamRunRequest, signal?: AbortSignal): Promise<DreamRunStatus> {
		if (this.#running) throw new Error(`Dream-RSI run ${this.#status?.runId ?? ""} is already running`.trim());
		const now = this.#deps.now ?? Date.now;
		const runId = `dream_${new Date(now())
			.toISOString()
			.replace(/[^0-9]/g, "")
			.slice(0, 14)}_${randomUUID().slice(0, 8)}`;
		const abort = new AbortController();
		const relay = (): void => abort.abort();
		if (signal?.aborted) abort.abort();
		else signal?.addEventListener("abort", relay, { once: true });
		this.#abort = abort;
		this.#running = true;
		const startedAt = now();
		// Set synchronously before the first await so a caller reading status()
		// immediately after start() sees the run id.
		this.#status = {
			runId,
			phase: "idle",
			task: request.task,
			iteration: 0,
			bestNodeScore: 0,
			startedAt,
			updatedAt: startedAt,
		};
		try {
			return await this.#run(request, abort.signal, now);
		} catch (error) {
			this.#update({ phase: "stopped", error: error instanceof Error ? error.message : String(error) });
			throw error;
		} finally {
			signal?.removeEventListener("abort", relay);
			this.#running = false;
			this.#abort = undefined;
		}
	}

	#update(patch: Partial<DreamRunStatus>, emit = true): DreamRunStatus {
		const current = this.#status;
		if (!current) throw new Error("no active Dream-RSI run");
		const next: DreamRunStatus = { ...current, ...patch, updatedAt: (this.#deps.now ?? Date.now)() };
		this.#status = next;
		if (emit) {
			try {
				this.#deps.onUpdate(structuredClone(next));
			} catch {
				// Observers never abort the run.
			}
		}
		return next;
	}

	async #run(request: DreamRunRequest, signal: AbortSignal, now: () => number): Promise<DreamRunStatus> {
		const deps = this.#deps;
		const seed = request.seed ?? DREAM_RUN_DEFAULTS.seed;
		const rng = deps.rng ?? createSeededRng(seed);
		const task = resolveTask({ task: request.task, ...(request.n !== undefined ? { n: request.n } : {}) });
		const model = deps.model ? `${deps.model.provider}/${deps.model.id}` : undefined;
		const scope: ChildRuntimeScope = {
			...(model ? { model } : {}),
			tools: "none",
			maxTurns: 8,
			role: "dream",
		};
		try {
			const result: DreamLoopResult = await runDreamLoopWithAgent({
				runAgent: deps.runAgent,
				task,
				taskId: request.task,
				...(request.n !== undefined ? { n: request.n } : {}),
				seed,
				clock: () => now(),
				workers: request.workers ?? DREAM_RUN_DEFAULTS.workers,
				k1: request.k1 ?? DREAM_RUN_DEFAULTS.k1,
				k2: request.k2 ?? DREAM_RUN_DEFAULTS.k2,
				dreams: request.dreams ?? DREAM_RUN_DEFAULTS.dreams,
				iterations: request.iterations ?? DREAM_RUN_DEFAULTS.iterations,
				dir: deps.dir,
				rng,
				useLlmProposer: request.llmProposer === true,
				useLlmDreamer: request.llmDreamer === true,
				scope,
				signal,
				onProgress: (event) => this.#update(statusPatch(event)),
			});
			return this.#update({
				phase: result.improved ? "accepted" : "stopped",
				stopReason: "completed",
				finalPolicyScore: result.finalPolicyScore,
				improved: result.improved,
				bestNodeScore: result.bestNodeScore,
				iteration: result.iterations,
			});
		} catch (error) {
			// A cancel surfaces as DreamAbortError (or as the relayed abort); anything
			// else is a real failure the caller's start() records and re-throws.
			if (!isDreamAbortError(error) && !signal.aborted) throw error;
			return this.#update({ phase: "stopped", stopReason: "cancelled" });
		}
	}
}

/** Map an observability progress event onto the fields of the run status it changes. */
function statusPatch(event: DreamProgressEvent): Partial<DreamRunStatus> {
	if (event.type === "completed") {
		return {
			finalPolicyScore: event.finalPolicyScore,
			improved: event.improved,
			bestNodeScore: event.bestNodeScore,
			iteration: event.iteration,
		};
	}
	return { phase: event.phase, iteration: event.iteration, bestNodeScore: event.bestNodeScore };
}
