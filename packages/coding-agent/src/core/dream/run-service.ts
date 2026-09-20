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
import type { ThinkingLevel } from "@earendil-works/pi-agent-core";
import type { Model } from "@earendil-works/pi-ai";
import type { ChildRuntimeScope } from "../ravo/runtime-adapter.js";
import type { RunAgentHandler } from "../run-agent.js";
import {
	armSettings,
	createLocalArmRunner,
	EXPERIMENT_ARMS,
	ExperimentAbortError,
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
import {
	DREAMER_PROMPT_HEADER,
	type DreamProgressEvent,
	GUIDANCE_PROMPT_HEADER,
	isDreamAbortError,
	PROPOSER_PROMPT_HEADER,
	runDreamLoopWithAgent,
} from "./llm.js";
import type { DreamLoopResult } from "./loop.js";
import type { ExplorationPolicy } from "./policy.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import { experimentResultPath } from "./store.js";
import type { DreamTaskId } from "./task.js";
import { resolveTask, resolveTaskN, taskPromptContext } from "./tasks/index.js";
import type { DreamClock } from "./types.js";

/** Why a Dream-RSI run ended. `completed` covers both improved and no-improvement finishes. */
export type DreamStopReason = "completed" | "cancelled" | "error";

/**
 * A snapshot of one Dream-RSI run. Every field but `resultPaths` is a scalar, so
 * a snapshot is safe on a wire frame and its scalar fields on a span attr. The
 * experiment and seed fields are OPTIONAL and additive: the `dream_run_update`
 * payload stays backward-compatible (the daemon forwards it opaquely and the
 * Agents View reads only the fields it knows), so no protocol bump.
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
	/** Where `result.json` landed once the experiment completed (the LAST completed seed's, with `seeds`). */
	resultPath?: string;
	/** The seed of the experiment currently running (or last completed), with its 0-based index and the seed count. */
	seed?: number;
	seedIndex?: number;
	seedCount?: number;
	/** Every completed seed's `result.json`, in run order; `resultPath` is its last entry. */
	resultPaths?: string[];
	/** Token total of the last COMPLETED seed (all arms), so spend is visible mid-run on the LLM path. */
	tokens?: number;
};

export type DreamRunEvent = { type: "dream_run_update"; status: DreamRunStatus };

/** The child-agent knobs a run or experiment may set for its proposer, dreamer and guidance children. */
export interface DreamChildOptions {
	/** Child model selector (`provider/id`); defaults to the session model the service was built with. */
	model?: string;
	/** Child thinking level for every dream role; defaults to `DREAM_CHILD_DEFAULTS.thinking`. */
	thinking?: ThinkingLevel;
	/** Visible-answer cap for every dream role; defaults per role (`DREAM_CHILD_DEFAULTS`). */
	maxOutputTokens?: number;
}

export interface DreamRunRequest extends DreamChildOptions {
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
	/** Pool priming: extra policies rolled out at iteration 0 (`PRIMING_DIVERSE` for `--priming diverse`). */
	primingPolicies?: ExplorationPolicy[];
}

export interface DreamExperimentRequest extends DreamChildOptions {
	task: DreamTaskId;
	n?: number;
	seed?: number;
	/**
	 * Several seeds, run sequentially under ONE run id with one `dream_run_update`
	 * stream and one `result.json` per seed (`experiments/<task>-s<seed>-...`).
	 * At most `DREAM_MAX_SEEDS`; exclusive with `seed`.
	 */
	seeds?: number[];
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
	/** Pool priming, part of every arm's shared round 1 (`PRIMING_DIVERSE` for `--priming diverse`). */
	primingPolicies?: ExplorationPolicy[];
}

/** The most seeds one in-session experiment may run: every LLM seed is a full experiment's spend. */
export const DREAM_MAX_SEEDS = 16;

/** What the in-session LLM arm runner is built from. */
export interface DreamExperimentLlmContext {
	runAgent: RunAgentHandler;
	scope: ChildRuntimeScope;
	signal: AbortSignal;
	useLlmProposer: boolean;
	useLlmDreamer: boolean;
	/** The task's public contract for the proposer prompt (`taskPromptContext(task, resolveTaskN(spec))`; python-speedup and autocorrelation have one). */
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

/**
 * Per-role child defaults. Thinking is off for every role: on the measured run
 * (docs/dream-rsi.md) the thinking-off proposer answered in ~270 output tokens
 * with 93% acceptance, while thinking-on gave 2/83, and on adaptive models the
 * stream `max_tokens` covers thinking too. The caps bound the VISIBLE answer
 * (`RunAgentOptions.maxOutputTokens`): a numeric proposal is a few hundred
 * tokens, a python-speedup program more, a dreamer's policy list ~1.7k, and
 * guidance a handful of short insights.
 */
export const DREAM_CHILD_DEFAULTS = {
	thinking: "off" as ThinkingLevel,
	maxOutputTokens: { proposer: 4096, proposerPythonSpeedup: 8192, dreamer: 4096, guidance: 2048 },
} as const;

export type DreamChildRole = "proposer" | "dreamer" | "guidance";

/** The default visible-answer cap of one dream role for a task. */
export function dreamChildOutputCap(role: DreamChildRole, task: DreamTaskId): number {
	if (role === "proposer") {
		return task === "python-speedup"
			? DREAM_CHILD_DEFAULTS.maxOutputTokens.proposerPythonSpeedup
			: DREAM_CHILD_DEFAULTS.maxOutputTokens.proposer;
	}
	return DREAM_CHILD_DEFAULTS.maxOutputTokens[role];
}

/**
 * The `ChildRuntimeScope` a run's children share: the request's model (else the
 * session's), the thinking level and the PROPOSER's cap (the scope is what the
 * experiment records as the arm mode). The dreamer and guidance caps are applied
 * per call by `roleCappedRunAgent`. An explicit `maxOutputTokens` applies to
 * every role.
 */
export function dreamChildScope(
	task: DreamTaskId,
	options: DreamChildOptions,
	sessionModel: string | undefined,
): ChildRuntimeScope {
	const model = options.model ?? sessionModel;
	return {
		...(model ? { model } : {}),
		tools: "none",
		maxTurns: 8,
		role: "dream",
		thinkingLevel: options.thinking ?? DREAM_CHILD_DEFAULTS.thinking,
		maxOutputTokens: options.maxOutputTokens ?? dreamChildOutputCap("proposer", task),
	};
}

/** The dream role a child prompt belongs to, read from its first line (the headers `llm.ts` exports for this). */
export function dreamChildRole(prompt: string): DreamChildRole | undefined {
	if (prompt.startsWith(PROPOSER_PROMPT_HEADER)) return "proposer";
	if (prompt.startsWith(DREAMER_PROMPT_HEADER)) return "dreamer";
	if (prompt.startsWith(GUIDANCE_PROMPT_HEADER)) return "guidance";
	return undefined;
}

/**
 * The `RunAgentHandler` a run's children go through: the dreamer and guidance
 * children get their own default caps in place of the proposer's that the shared
 * scope carries. An explicit request cap is already on the scope for every role,
 * so the handler is then returned unwrapped.
 */
export function roleCappedRunAgent(
	runAgent: RunAgentHandler,
	task: DreamTaskId,
	options: DreamChildOptions,
): RunAgentHandler {
	if (options.maxOutputTokens !== undefined) return runAgent;
	return (request, runOptions) => {
		const role = dreamChildRole(request.prompt);
		if (role === undefined || role === "proposer") return runAgent(request, runOptions);
		return runAgent(request, { ...runOptions, maxOutputTokens: dreamChildOutputCap(role, task) });
	};
}

/** Validate an experiment's seed list: 1..`DREAM_MAX_SEEDS` distinct non-negative integers; `RangeError` otherwise. */
export function validateDreamSeeds(seeds: readonly number[]): number[] {
	if (seeds.length === 0) throw new RangeError("dream experiment seeds must not be empty");
	if (seeds.length > DREAM_MAX_SEEDS) {
		throw new RangeError(`dream experiment seeds must list at most ${DREAM_MAX_SEEDS} seeds (got ${seeds.length})`);
	}
	for (const seed of seeds) {
		if (!Number.isInteger(seed) || seed < 0) {
			throw new RangeError(`dream experiment seeds must be non-negative integers (got ${String(seed)})`);
		}
	}
	if (new Set(seeds).size !== seeds.length) throw new RangeError("dream experiment seeds must be distinct");
	return [...seeds];
}

type ExperimentSettings = DreamExperimentRequest & { rounds: number; arms: ExperimentArm[]; seeds: number[] };

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
		if (request.seed !== undefined && request.seeds !== undefined) {
			throw new Error("dream experiment takes either seed or seeds, not both");
		}
		const seeds = request.seeds ? validateDreamSeeds(request.seeds) : [request.seed ?? DREAM_RUN_DEFAULTS.seed];
		const settings: ExperimentSettings = {
			...request,
			rounds: request.rounds ?? DREAM_EXPERIMENT_DEFAULTS.rounds,
			arms: request.arms ? [...request.arms] : [...DREAM_EXPERIMENT_DEFAULTS.arms],
			seeds,
		};
		return this.#launch(
			{
				task: request.task,
				kind: "experiment",
				rounds: settings.rounds,
				armCount: settings.arms.length,
				seed: seeds[0],
				seedIndex: 0,
				seedCount: seeds.length,
			},
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

	#scope(task: DreamTaskId, options: DreamChildOptions): ChildRuntimeScope {
		const model = this.#deps.model ? `${this.#deps.model.provider}/${this.#deps.model.id}` : undefined;
		return dreamChildScope(task, options, model);
	}

	async #run(request: DreamRunRequest, signal: AbortSignal, now: () => number): Promise<DreamRunStatus> {
		const deps = this.#deps;
		const seed = request.seed ?? DREAM_RUN_DEFAULTS.seed;
		const rng = deps.rng ?? createSeededRng(seed);
		const taskSpec = { task: request.task, ...(request.n !== undefined ? { n: request.n } : {}) };
		const task = resolveTask(taskSpec);
		const promptContext = taskPromptContext(request.task, resolveTaskN(taskSpec));
		try {
			const result: DreamLoopResult = await runDreamLoopWithAgent({
				runAgent: roleCappedRunAgent(deps.runAgent, request.task, request),
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
				scope: this.#scope(request.task, request),
				signal,
				...(promptContext ? { proposerPromptContext: promptContext } : {}),
				...(request.primingPolicies ? { primingPolicies: [...request.primingPolicies] } : {}),
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
			const promptContext = taskPromptContext(
				request.task,
				resolveTaskN({ task: request.task, ...(request.n !== undefined ? { n: request.n } : {}) }),
			);
			runner = factory({
				runAgent: roleCappedRunAgent(deps.runAgent, request.task, request),
				scope: this.#scope(request.task, request),
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
		// Seeds run sequentially under this one run slot; each is an independent
		// replicate with its own experiment id (`-s<seed>`), stores and result.json,
		// and a cancel keeps every seed that already completed.
		const clock: DreamClock = () => startedAt;
		const resultPaths: string[] = [];
		let best = 0;
		try {
			for (const [seedIndex, seed] of request.seeds.entries()) {
				if (signal.aborted) throw new ExperimentAbortError(`dream experiment aborted before seed ${seed}`);
				const spec: ExperimentSpec = {
					task: request.task,
					...(request.n !== undefined ? { n: request.n } : {}),
					seed,
					rounds: request.rounds,
					budget: {
						workers: request.workers ?? DREAM_RUN_DEFAULTS.workers,
						k1: request.k1 ?? DREAM_RUN_DEFAULTS.k1,
						k2: request.k2 ?? DREAM_RUN_DEFAULTS.k2,
						dreams: request.dreams ?? DREAM_RUN_DEFAULTS.dreams,
					},
					arms: request.arms,
					...(request.primingPolicies ? { primingPolicies: [...request.primingPolicies] } : {}),
				};
				this.#update(
					{
						seed,
						seedIndex,
						seedCount: request.seeds.length,
						experimentId: experimentIdFor(spec, clock),
						arm: undefined,
						armIndex: undefined,
						round: 0,
						iteration: 0,
						cumulativeProbes: 0,
					},
					seedIndex > 0,
				);
				const result = await runExperimentWithRunner(spec, {
					dir: deps.dir,
					clock,
					runner,
					signal,
					allowedArms: needsLlm ? EXPERIMENT_ARMS : LOCAL_EXPERIMENT_ARMS,
					onProgress: (event) => this.#update(this.#experimentPatch(event)),
				});
				resultPaths.push(experimentResultPath(deps.dir, result.experimentId));
				best = Math.max(best, ...result.arms.map((arm) => arm.totals.finalBest));
				this.#update({
					experimentId: result.experimentId,
					resultPath: resultPaths.at(-1),
					resultPaths: [...resultPaths],
					bestNodeScore: best,
					...(needsLlm ? { tokens: result.arms.reduce((sum, arm) => sum + arm.totals.tokens, 0) } : {}),
				});
			}
			return this.#update({ phase: "stopped", stopReason: "completed", resultPaths: [...resultPaths] });
		} catch (error) {
			if (!isDreamAbortError(error) && !isExperimentAbortError(error) && !signal.aborted) throw error;
			return this.#update({ phase: "stopped", stopReason: "cancelled", resultPaths: [...resultPaths] });
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
