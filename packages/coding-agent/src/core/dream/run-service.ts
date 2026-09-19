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
 * `currentTraceContext()` still supplies that trigger id. `startExperiment()`
 * likewise leaves the detached-root `dream.experiment` span to
 * `runExperimentWithRunner`.
 *
 * Soundness is unchanged from the local path: with `llmProposer`/`llmDreamer`
 * false (the default) the driver runs the local zero-token proposer and never
 * calls `runAgent`; the LLM path parses every dreamed policy through the strict
 * `parseExplorationPolicy`, which is DATA, never executed. Determinism flows
 * through the injected `SeededRng` and clock.
 *
 * An experiment shares the single run slot with a run. Its local arms run through
 * `createLocalArmRunner` (zero tokens); any arm that needs the LLM path (an
 * `llmProposer`/`llmDreamer` request, or a guided arm) is built by the injected
 * `llmExperimentRunner` factory, which the in-session caller supplies from the
 * LLM lane's runner. Without that factory such a request is rejected before any
 * child call.
 */

import { randomUUID } from "node:crypto";
import type { Model } from "@earendil-works/pi-ai";
import type { ChildRuntimeScope } from "../ravo/runtime-adapter.js";
import type { RunAgentHandler } from "../run-agent.js";
import {
	armSettings,
	createLocalArmRunner,
	EXPERIMENT_ARMS,
	type ExperimentArm,
	type ExperimentArmRunner,
	ExperimentArmUnavailableError,
	type ExperimentProgressEvent,
	type ExperimentSpec,
	experimentIdFor,
	isExperimentAbortError,
	LOCAL_EXPERIMENT_ARMS,
	runExperimentWithRunner,
} from "./experiment.js";
import { type DreamProgressEvent, isDreamAbortError, runDreamLoopWithAgent } from "./llm.js";
import type { DreamLoopResult } from "./loop.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import { experimentResultPath } from "./store.js";
import type { DreamTaskId } from "./task.js";
import { resolveTask, taskPromptContext } from "./tasks/index.js";
import type { DreamClock } from "./types.js";

/** Why a Dream-RSI run ended. `completed` covers both improved and no-improvement finishes. */
export type DreamStopReason = "completed" | "cancelled" | "error";

/**
 * A snapshot of one Dream-RSI run. Every field is a scalar so it is safe on a span
 * attr and a wire frame. The experiment fields are OPTIONAL and additive: the
 * `dream_run_update` payload stays backward-compatible (the daemon forwards it
 * opaquely and the Agents View reads only the run fields), so no protocol bump.
 */
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
	/** `run` (default) or `experiment`. */
	kind?: "run" | "experiment";
	experimentId?: string;
	/** The arm currently running. */
	arm?: string;
	armIndex?: number;
	armCount?: number;
	/** The current arm's round (1-based) and the experiment's rounds per arm. */
	round?: number;
	rounds?: number;
	/** The current arm's evaluated attempts so far. */
	cumulativeProbes?: number;
	/** Where `result.json` landed once the experiment completed. */
	resultPath?: string;
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

export interface DreamExperimentRequest {
	task: DreamTaskId;
	n?: number;
	seed?: number;
	/** Rollouts per arm; defaults to `DREAM_EXPERIMENT_DEFAULTS.rounds`. */
	rounds?: number;
	/** Distinct arms in run order; defaults to `dream,fixed`. */
	arms?: ExperimentArm[];
	workers?: number;
	k1?: number;
	k2?: number;
	dreams?: number;
	llmProposer?: boolean;
	llmDreamer?: boolean;
}

/** What the in-session LLM arm runner is built from. */
export interface DreamExperimentLlmContext {
	runAgent: RunAgentHandler;
	scope: ChildRuntimeScope;
	signal: AbortSignal;
	useLlmProposer: boolean;
	useLlmDreamer: boolean;
	/** The task's public contract for the proposer prompt (python-speedup only). */
	proposerPromptContext?: string;
}

/**
 * Builds the `ExperimentArmRunner` for arms that spend tokens. The runner drives
 * `runDreamLoopWithAgent` with each arm's `fixedPolicy`/guidance settings and may
 * share round 1 across arms through `prepare`.
 */
export type DreamExperimentLlmRunnerFactory = (context: DreamExperimentLlmContext) => ExperimentArmRunner;

export interface DreamRunServiceDeps {
	runAgent: RunAgentHandler;
	model?: Model<any>;
	/** The dream store directory trees and blobs are written under. */
	dir: string;
	now?: () => number;
	/** Injected rng; defaults to a fresh seeded rng from the request seed. Runs only; an experiment's arms seed themselves. */
	rng?: SeededRng;
	/** Absent, an experiment that needs the LLM path is rejected before any child call. */
	llmExperimentRunner?: DreamExperimentLlmRunnerFactory;
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

export const DREAM_EXPERIMENT_DEFAULTS = {
	rounds: 4,
	arms: LOCAL_EXPERIMENT_ARMS,
} as const;

type ExperimentSettings = DreamExperimentRequest & { rounds: number; arms: ExperimentArm[] };

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
		return this.#launch({ task: request.task, kind: "run" }, signal, (abortSignal, now) =>
			this.#run(request, abortSignal, now),
		);
	}

	/**
	 * Start an experiment (the dreaming arm against the fixed-exploration control,
	 * plus the optional LLM ablation arms) in the same background slot a run uses.
	 * Progress arrives as `DreamRunStatus` snapshots carrying the experiment fields;
	 * the terminal snapshot carries `resultPath`.
	 */
	async startExperiment(request: DreamExperimentRequest, signal?: AbortSignal): Promise<DreamRunStatus> {
		const settings: ExperimentSettings = {
			...request,
			rounds: request.rounds ?? DREAM_EXPERIMENT_DEFAULTS.rounds,
			arms: request.arms ? [...request.arms] : [...DREAM_EXPERIMENT_DEFAULTS.arms],
		};
		return this.#launch(
			{ task: request.task, kind: "experiment", rounds: settings.rounds, armCount: settings.arms.length },
			signal,
			(abortSignal, _now, startedAt) => this.#runExperiment(settings, abortSignal, startedAt),
		);
	}

	async #launch(
		initial: Pick<DreamRunStatus, "task" | "kind"> & Partial<DreamRunStatus>,
		signal: AbortSignal | undefined,
		body: (signal: AbortSignal, now: () => number, startedAt: number) => Promise<DreamRunStatus>,
	): Promise<DreamRunStatus> {
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
			...initial,
			runId,
			phase: "idle",
			iteration: 0,
			bestNodeScore: 0,
			startedAt,
			updatedAt: startedAt,
		};
		try {
			return await body(abort.signal, now, startedAt);
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

	#scope(): ChildRuntimeScope {
		const model = this.#deps.model ? `${this.#deps.model.provider}/${this.#deps.model.id}` : undefined;
		return {
			...(model ? { model } : {}),
			tools: "none",
			maxTurns: 8,
			role: "dream",
		};
	}

	async #run(request: DreamRunRequest, signal: AbortSignal, now: () => number): Promise<DreamRunStatus> {
		const deps = this.#deps;
		const seed = request.seed ?? DREAM_RUN_DEFAULTS.seed;
		const rng = deps.rng ?? createSeededRng(seed);
		const task = resolveTask({ task: request.task, ...(request.n !== undefined ? { n: request.n } : {}) });
		const promptContext = taskPromptContext(request.task);
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
				scope: this.#scope(),
				signal,
				...(promptContext ? { proposerPromptContext: promptContext } : {}),
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

	async #runExperiment(request: ExperimentSettings, signal: AbortSignal, startedAt: number): Promise<DreamRunStatus> {
		const deps = this.#deps;
		const useLlmProposer = request.llmProposer === true;
		const useLlmDreamer = request.llmDreamer === true;
		if (request.arms.some((arm) => armSettings(arm).guided) && !useLlmProposer) {
			throw new Error("dream-guided/fixed-guided require llmProposer");
		}
		const needsLlm = useLlmProposer || useLlmDreamer;
		let runner: ExperimentArmRunner;
		if (needsLlm) {
			const factory = deps.llmExperimentRunner;
			if (!factory) {
				throw new ExperimentArmUnavailableError(
					"LLM experiment arms need an in-session LLM arm runner (DreamRunServiceDeps.llmExperimentRunner); this session has none, so llmProposer/llmDreamer and the guided arms are unavailable",
				);
			}
			const promptContext = taskPromptContext(request.task);
			runner = factory({
				runAgent: deps.runAgent,
				scope: this.#scope(),
				signal,
				useLlmProposer,
				useLlmDreamer,
				...(promptContext ? { proposerPromptContext: promptContext } : {}),
			});
		} else {
			runner = createLocalArmRunner();
		}
		// One frozen clock per experiment, exactly like the CLI: every arm's tree ids
		// then share the timestamp, so the same round has the same id in every arm.
		const clock: DreamClock = () => startedAt;
		const spec: ExperimentSpec = {
			task: request.task,
			...(request.n !== undefined ? { n: request.n } : {}),
			seed: request.seed ?? DREAM_RUN_DEFAULTS.seed,
			rounds: request.rounds,
			budget: {
				workers: request.workers ?? DREAM_RUN_DEFAULTS.workers,
				k1: request.k1 ?? DREAM_RUN_DEFAULTS.k1,
				k2: request.k2 ?? DREAM_RUN_DEFAULTS.k2,
				dreams: request.dreams ?? DREAM_RUN_DEFAULTS.dreams,
			},
			arms: request.arms,
		};
		this.#update({ experimentId: experimentIdFor(spec, clock) }, false);
		try {
			const result = await runExperimentWithRunner(spec, {
				dir: deps.dir,
				clock,
				runner,
				signal,
				allowedArms: needsLlm ? EXPERIMENT_ARMS : LOCAL_EXPERIMENT_ARMS,
				onProgress: (event) => this.#update(this.#experimentPatch(event)),
			});
			return this.#update({
				phase: "stopped",
				stopReason: "completed",
				experimentId: result.experimentId,
				resultPath: experimentResultPath(deps.dir, result.experimentId),
				bestNodeScore: Math.max(0, ...result.arms.map((arm) => arm.totals.finalBest)),
			});
		} catch (error) {
			if (!isDreamAbortError(error) && !isExperimentAbortError(error) && !signal.aborted) throw error;
			return this.#update({ phase: "stopped", stopReason: "cancelled" });
		}
	}

	/** Map an experiment progress event onto the status fields it changes; `bestNodeScore` stays the max seen. */
	#experimentPatch(event: ExperimentProgressEvent): Partial<DreamRunStatus> {
		const best = this.#status?.bestNodeScore ?? 0;
		switch (event.type) {
			case "arm_start":
				return {
					phase: "rollout",
					arm: event.arm,
					armIndex: event.armIndex,
					armCount: event.armCount,
					round: 0,
					iteration: 0,
					cumulativeProbes: 0,
				};
			case "phase":
				return {
					phase: event.phase,
					iteration: event.iteration,
					round: event.iteration + 1,
					bestNodeScore: Math.max(best, event.bestNodeScore),
				};
			case "round":
				return {
					round: event.round,
					iteration: event.round - 1,
					cumulativeProbes: event.cumulativeProbes,
					bestNodeScore: Math.max(best, event.cumulativeBest),
				};
			case "arm_end":
				return { bestNodeScore: Math.max(best, event.finalBest) };
			case "completed":
				return { experimentId: event.experimentId, resultPath: event.resultPath };
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
