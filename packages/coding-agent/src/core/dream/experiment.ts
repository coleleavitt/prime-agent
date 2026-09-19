/**
 * Dream-RSI experiments: the paper's evidence, with its control arm.
 *
 * An experiment runs several ARMS of the loop from the SAME task instance, initial
 * policy, seed, clock and per-round budget, each into its own store under
 * `<dir>/experiments/<id>/<arm>`, and records one row per round per arm. The
 * "fixed" arm is the paper's Recursive Fixed Exploration control: identical in
 * every respect except that it never dreams (`fixedPolicy`), so round 1 is
 * identical to the dreaming arm by construction and every later difference is the
 * learned policy's doing. The headline multipliers compare each arm against it.
 *
 * Vocabulary. A ROUND is one rollout (`rounds = N` means N rollouts per arm; the
 * loop runs `iterations = N - 1`). PROBES (`tree.size - 1`, evaluated attempts) are
 * the discovery compute — the paper's "agent calls" — on every path and the only
 * input to the multipliers. Handler calls and tokens are cost and are never mixed
 * into that axis. A policy's own-pool replay score is an in-arm estimate and is
 * never compared across arms. A multiplier is reported only when defined (else
 * null, printed as "not reached"/"not comparable"); nothing is clamped.
 *
 * Two drivers share one plan/record/headline core:
 *   - `runExperiment` is the synchronous local runner behind the standalone CLI:
 *     an in-turn `dream.experiment` root with one `dream.experiment_arm` child per
 *     arm, each wrapping the arm's `dream.run`. Zero tokens, byte-deterministic
 *     under the injected seed and clock (python-speedup scores excepted).
 *   - `runExperimentWithRunner` is the asynchronous in-session core: a DETACHED
 *     ROOT `dream.experiment` carrying the launching turn's `trigger.trace_id`,
 *     arms run inside its context, and an `ExperimentArmRunner` decides how each
 *     arm's loop is driven. `createLocalArmRunner` wraps `runDreamLoop`; the LLM
 *     lane's runner (built on `runDreamLoopWithAgent` in `llm.ts`) serves the
 *     LLM and guided arms and may share round 1 across arms through `prepare`.
 *     This module never imports `llm.ts`, so the barrel stays token-free.
 */

import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { currentTraceContext, runWithTraceContext, type Span, startSpan, withSpan } from "@earendil-works/pi-ai";
import { type DreamHandlerCalls, type DreamLoopResult, type DreamRoundRecord, runDreamLoop } from "./loop.js";
import { DEFAULT_OBJECTIVE, type ReplayObjectiveConfig } from "./objective.js";
import { DEFAULT_POLICY, type ExplorationPolicy, policyId } from "./policy.js";
import { DreamStoreError, experimentArmDir, experimentDir, experimentResultPath } from "./store.js";
import type { DreamTaskId, ScoredTask } from "./task.js";
import { DEFAULT_CIRCLE_PACKING_N, resolveTask } from "./tasks/index.js";
import type { DreamClock, DreamMode } from "./types.js";

export const EXPERIMENT_SCHEMA = "prime-agent.dream.experiment/1";
export const EXPERIMENT_ARMS = ["dream", "fixed", "dream-guided", "fixed-guided"] as const;
/** The arms the token-free local runner can serve. */
export const LOCAL_EXPERIMENT_ARMS = ["dream", "fixed"] as const;
export type ExperimentArm = (typeof EXPERIMENT_ARMS)[number];

export const GUIDED_ARM_REJECTION_MESSAGE =
	"dream-guided/fixed-guided need the in-session LLM proposer; run dream.experiment(...) from the kernel skill or /dream experiment --llm-proposer";

/** Reaching the control's target is judged within this tolerance. */
const TARGET_EPS = 1e-9;
const DIR_MODE = 0o700;
const FILE_MODE = 0o600;
const PYTHON_SPEEDUP_NOTE =
	"python-speedup: evaluate is wall-clock timed; tree shapes and ids are deterministic, scores are not byte-deterministic";
const NO_FIXED_ARM_NOTE = "no fixed arm ran: the headline multipliers are undefined";
/**
 * Appended to every result so a reader can tell a file scored by the normalized
 * objective from one scored by the raw-scale objective it replaced; the result
 * shape is unchanged, so `EXPERIMENT_SCHEMA` is not bumped.
 */
export const OBJECTIVE_NOTE = "objective: normalized (q in pool range, cost in budget fractions)";

export interface ExperimentBudget {
	workers: number;
	k1: number;
	k2: number;
	dreams: number;
}

export interface ExperimentSpec {
	task: DreamTaskId;
	/** Task size parameter (circle count); defaults to the paper's 26 for circle-packing. */
	n?: number;
	seed: number | string;
	/** Rollouts per arm (N >= 1); the loop runs `N - 1` iterations. */
	rounds: number;
	budget: ExperimentBudget;
	/** Distinct arms, run in this order. */
	arms: readonly ExperimentArm[];
	objective?: ReplayObjectiveConfig;
	/** The hand-written policy every arm starts from; defaults to `DEFAULT_POLICY`. */
	initialPolicy?: ExplorationPolicy;
}

export interface ExperimentRunOptions {
	/** The dream dir; the experiment writes only under `<dir>/experiments/<id>`. */
	dir: string;
	clock: DreamClock;
	/** Free-text notes appended to the result. */
	notes?: string[];
	/**
	 * Replace an existing experiment directory for the same id (its arm stores and
	 * result) instead of refusing. An experiment always starts from empty arm
	 * stores, so a leftover pool can never leak into a new run.
	 */
	overwrite?: boolean;
}

export interface ExperimentRoundRow {
	/** 1-based; round r is loop iteration r - 1. */
	round: number;
	treeId: string;
	policyId: string;
	roundBest: number;
	/** max(roundBest) over rounds 1..r. */
	cumulativeBest: number;
	/** Evaluated attempts this round: the compute axis. */
	probes: number;
	cumulativeProbes: number;
	decisionRounds: number;
	poolSize: number;
	handlerCalls: DreamHandlerCalls;
	cumulativeHandlerCalls: number;
	tokens: number;
	cumulativeTokens: number;
	dreaming: DreamRoundRecord["dreaming"];
}

export interface ExperimentArmMode {
	proposer: DreamMode;
	dreamer: DreamMode;
	/** The child model selector on the LLM path. */
	model?: string;
}

export interface ExperimentArmResult {
	arm: ExperimentArm;
	fixedPolicy: boolean;
	guided: boolean;
	mode: ExperimentArmMode;
	/** Relative to the dream dir: `experiments/<id>/<arm>`. */
	storeDir: string;
	runId: string;
	initialPolicyId: string;
	finalPolicyId: string;
	/** In-arm replay estimates on this arm's own final pool; never compared across arms. */
	policyScoreOnOwnPool: { initial: number; final: number };
	/** Rounds whose policy differs from the previous round's (Fig 6b adaptivity). */
	policyChanges: number;
	rounds: ExperimentRoundRow[];
	totals: { probes: number; handlerCalls: number; tokens: number; finalBest: number };
}

export interface ExperimentHeadline {
	reference: "fixed";
	/** The fixed arm's final cumulativeBest. */
	target: number;
	/** Per arm: cumulativeProbes at the end of the first round with cumulativeBest >= target, else null. */
	probesToTarget: Record<string, number | null>;
	/** probesToTarget.fixed / probesToTarget[arm]; null when either is null or the arm's is 0. */
	callsMultiplier: Record<string, number | null>;
	/** min over arms of totals.probes. */
	equalBudget: number;
	/** Per arm: cumulativeBest at the last round with cumulativeProbes <= equalBudget, else null. */
	bestAtBudget: Record<string, number | null>;
	/** bestAtBudget[arm] / bestAtBudget.fixed; null when either is null or fixed's is 0. */
	scoreMultiplier: Record<string, number | null>;
	/** finalBest[arm] - finalBest.fixed. */
	deltaBest: Record<string, number>;
}

export interface ExperimentResult {
	schema: typeof EXPERIMENT_SCHEMA;
	experimentId: string;
	task: DreamTaskId;
	n?: number;
	seed: number | string;
	rounds: number;
	budget: ExperimentBudget;
	objective: ReplayObjectiveConfig;
	initialPolicyId: string;
	initialPolicy: ExplorationPolicy;
	arms: ExperimentArmResult[];
	/** null when no fixed arm ran. */
	headline: ExperimentHeadline | null;
	/** True when round 1 was rolled out once and copied into every arm (the LLM path). */
	sharedInitialRollout: boolean;
	createdTs: number;
	notes: string[];
}

/** Thrown when a spec names an arm the chosen runner cannot serve (e.g. a guided arm on the local CLI). */
export class ExperimentArmUnavailableError extends Error {
	constructor(message = GUIDED_ARM_REJECTION_MESSAGE) {
		super(message);
		this.name = "ExperimentArmUnavailableError";
	}
}

/** Thrown by `runExperimentWithRunner` when its signal aborts between arms; a runner's own abort error is honoured too. */
export class ExperimentAbortError extends Error {
	constructor(message = "dream experiment aborted") {
		super(message);
		this.name = "ExperimentAbortError";
	}
}

export function isExperimentAbortError(error: unknown): error is ExperimentAbortError {
	return error instanceof ExperimentAbortError;
}

export function isExperimentArm(value: unknown): value is ExperimentArm {
	return typeof value === "string" && (EXPERIMENT_ARMS as readonly string[]).includes(value);
}

export function armSettings(arm: ExperimentArm): { fixedPolicy: boolean; guided: boolean } {
	return { fixedPolicy: arm === "fixed" || arm === "fixed-guided", guided: arm.endsWith("-guided") };
}

export function experimentIdFor(spec: ExperimentSpec, clock: DreamClock): string {
	return `${spec.task}-s${spec.seed}-n${spec.rounds}-${clock()}`;
}

/** The loop options one arm runs with; structurally a `DreamLoopOptions` without an injected rng (each arm seeds itself). */
export interface ExperimentArmLoopOptions {
	task: ScoredTask<unknown>;
	taskId: DreamTaskId;
	n?: number;
	seed: number | string;
	clock: DreamClock;
	workers: number;
	k1: number;
	k2: number;
	dreams: number;
	iterations: number;
	dir: string;
	objective: ReplayObjectiveConfig;
	initialPolicy: ExplorationPolicy;
	fixedPolicy: boolean;
}

export interface ExperimentArmPlan {
	arm: ExperimentArm;
	index: number;
	fixedPolicy: boolean;
	guided: boolean;
	/** Absolute per-arm store directory. */
	dir: string;
	/** The same directory relative to the dream dir: `experiments/<id>/<arm>`. */
	storeDir: string;
	loop: ExperimentArmLoopOptions;
}

export interface ExperimentPlan {
	experimentId: string;
	task: ScoredTask<unknown>;
	taskId: DreamTaskId;
	n: number | undefined;
	seed: number | string;
	rounds: number;
	budget: ExperimentBudget;
	objective: ReplayObjectiveConfig;
	initialPolicy: ExplorationPolicy;
	initialPolicyId: string;
	/** The dream dir the experiment lives under. */
	dir: string;
	resultPath: string;
	arms: ExperimentArmPlan[];
	createdTs: number;
	notes: string[];
}

/**
 * Validate a spec, resolve the task ONCE (shared by every arm), and lay out the
 * per-arm stores. Refuses an arm outside `allowedArms` and an existing experiment
 * directory (unless `overwrite`) before anything is created; the runners then
 * call `resetExperimentDir` so every arm starts from an empty store.
 */
export function planExperiment(
	spec: ExperimentSpec,
	options: ExperimentRunOptions,
	allowedArms: readonly ExperimentArm[] = EXPERIMENT_ARMS,
): ExperimentPlan {
	if (!Number.isInteger(spec.rounds) || spec.rounds < 1) {
		throw new RangeError(`experiment rounds must be an integer >= 1 (got ${String(spec.rounds)})`);
	}
	if (spec.arms.length === 0) throw new RangeError("experiment needs at least one arm");
	if (new Set(spec.arms).size !== spec.arms.length) {
		throw new RangeError(`experiment arms must be distinct (got ${spec.arms.join(",")})`);
	}
	for (const arm of spec.arms) {
		if (!isExperimentArm(arm)) throw new RangeError(`unknown experiment arm: ${String(arm)}`);
		if (!allowedArms.includes(arm)) throw new ExperimentArmUnavailableError();
	}
	for (const [key, value] of Object.entries(spec.budget)) {
		if (!Number.isInteger(value) || value < 1) {
			throw new RangeError(`experiment budget ${key} must be an integer >= 1 (got ${String(value)})`);
		}
	}
	const experimentId = experimentIdFor(spec, options.clock);
	const resultPath = experimentResultPath(options.dir, experimentId);
	const existing = experimentDir(options.dir, experimentId);
	if (!options.overwrite && existsSync(existing)) {
		throw new DreamStoreError(
			`experiment ${experimentId} already exists at ${existing}; pass overwrite to replace it`,
		);
	}
	const n = spec.n ?? (spec.task === "circle-packing" ? DEFAULT_CIRCLE_PACKING_N : undefined);
	const task = resolveTask({ task: spec.task, ...(n !== undefined ? { n } : {}) });
	const objective = spec.objective ?? DEFAULT_OBJECTIVE;
	const initialPolicy = spec.initialPolicy ?? DEFAULT_POLICY;
	const notes = [...(options.notes ?? [])];
	if (spec.task === "python-speedup") notes.push(PYTHON_SPEEDUP_NOTE);
	if (!spec.arms.includes("fixed")) notes.push(NO_FIXED_ARM_NOTE);
	notes.push(OBJECTIVE_NOTE);
	const arms = spec.arms.map((arm, index): ExperimentArmPlan => {
		const settings = armSettings(arm);
		const dir = experimentArmDir(options.dir, experimentId, arm);
		return {
			arm,
			index,
			...settings,
			dir,
			storeDir: `experiments/${experimentId}/${arm}`,
			loop: {
				task,
				taskId: spec.task,
				...(n !== undefined ? { n } : {}),
				seed: spec.seed,
				clock: options.clock,
				workers: spec.budget.workers,
				k1: spec.budget.k1,
				k2: spec.budget.k2,
				dreams: spec.budget.dreams,
				iterations: spec.rounds - 1,
				dir,
				objective,
				initialPolicy,
				fixedPolicy: settings.fixedPolicy,
			},
		};
	});
	return {
		experimentId,
		task,
		taskId: spec.task,
		n,
		seed: spec.seed,
		rounds: spec.rounds,
		budget: { ...spec.budget },
		objective,
		initialPolicy,
		initialPolicyId: policyId(initialPolicy),
		dir: options.dir,
		resultPath,
		arms,
		createdTs: options.clock(),
		notes,
	};
}

/** Remove any earlier experiment directory of the same id so every arm starts from an empty store. */
function resetExperimentDir(plan: ExperimentPlan): void {
	rmSync(experimentDir(plan.dir, plan.experimentId), { recursive: true, force: true });
}

/** Derive an arm's rows and totals from the loop's per-round records. */
export function buildArmResult(
	arm: Pick<ExperimentArmPlan, "arm" | "fixedPolicy" | "guided" | "storeDir">,
	mode: ExperimentArmMode,
	loop: DreamLoopResult,
): ExperimentArmResult {
	let cumulativeBest = 0;
	let seenBest = false;
	let cumulativeProbes = 0;
	let cumulativeHandlerCalls = 0;
	let cumulativeTokens = 0;
	let policyChanges = 0;
	let previousPolicyId: string | undefined;
	const rounds = loop.rounds.map((record): ExperimentRoundRow => {
		if (!seenBest || record.roundBest > cumulativeBest) {
			cumulativeBest = record.roundBest;
			seenBest = true;
		}
		cumulativeProbes += record.probes;
		const handlerCalls = record.handlerCalls.proposer + record.handlerCalls.dreamer + record.handlerCalls.guidance;
		cumulativeHandlerCalls += handlerCalls;
		const tokens = record.tokens.rollout + record.tokens.dreamer + record.tokens.guidance;
		cumulativeTokens += tokens;
		if (previousPolicyId !== undefined && record.policyId !== previousPolicyId) policyChanges += 1;
		previousPolicyId = record.policyId;
		return {
			round: record.iteration + 1,
			treeId: record.treeId,
			policyId: record.policyId,
			roundBest: record.roundBest,
			cumulativeBest,
			probes: record.probes,
			cumulativeProbes,
			decisionRounds: record.decisionRounds,
			poolSize: record.poolSize,
			handlerCalls: { ...record.handlerCalls },
			cumulativeHandlerCalls,
			tokens,
			cumulativeTokens,
			dreaming: record.dreaming ? { ...record.dreaming } : null,
		};
	});
	return {
		arm: arm.arm,
		fixedPolicy: arm.fixedPolicy,
		guided: arm.guided,
		mode: { ...mode },
		storeDir: arm.storeDir,
		runId: loop.runId,
		initialPolicyId: loop.initialPolicyId,
		finalPolicyId: loop.finalPolicyId,
		policyScoreOnOwnPool: { initial: loop.initialPolicyScore, final: loop.finalPolicyScore },
		policyChanges,
		rounds,
		totals: {
			probes: cumulativeProbes,
			handlerCalls: cumulativeHandlerCalls,
			tokens: cumulativeTokens,
			finalBest: cumulativeBest,
		},
	};
}

function ratio(numerator: number | null, denominator: number | null): number | null {
	if (numerator === null || denominator === null || !(denominator > 0)) return null;
	return numerator / denominator;
}

/**
 * The headline comparison against the fixed-exploration control. Every number is
 * a plain double or null; nothing is clamped, so an arm that needed MORE compute
 * than the control shows a multiplier below 1. Returns null without a `fixed` arm.
 */
export function computeHeadline(
	arms: readonly ExperimentArmResult[],
	reference: "fixed" = "fixed",
): ExperimentHeadline | null {
	const control = arms.find((arm) => arm.arm === reference);
	if (!control || arms.length === 0) return null;
	const target = control.totals.finalBest;
	const equalBudget = Math.min(...arms.map((arm) => arm.totals.probes));
	const probesToTarget: Record<string, number | null> = {};
	const bestAtBudget: Record<string, number | null> = {};
	const deltaBest: Record<string, number> = {};
	for (const arm of arms) {
		const reached = arm.rounds.find((row) => row.cumulativeBest >= target - TARGET_EPS);
		probesToTarget[arm.arm] = reached ? reached.cumulativeProbes : null;
		let within: ExperimentRoundRow | undefined;
		for (const row of arm.rounds) {
			if (row.cumulativeProbes <= equalBudget) within = row;
		}
		bestAtBudget[arm.arm] = within ? within.cumulativeBest : null;
		deltaBest[arm.arm] = arm.totals.finalBest - target;
	}
	const callsMultiplier: Record<string, number | null> = {};
	const scoreMultiplier: Record<string, number | null> = {};
	for (const arm of arms) {
		callsMultiplier[arm.arm] = ratio(probesToTarget[reference] ?? null, probesToTarget[arm.arm] ?? null);
		scoreMultiplier[arm.arm] = ratio(bestAtBudget[arm.arm] ?? null, bestAtBudget[reference] ?? null);
	}
	return {
		reference,
		target,
		probesToTarget,
		callsMultiplier,
		equalBudget,
		bestAtBudget,
		scoreMultiplier,
		deltaBest,
	};
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Structural check of a parsed result file: the versioned schema plus the fields every reader relies on. */
export function isExperimentResult(value: unknown): value is ExperimentResult {
	if (!isRecord(value)) return false;
	if (value.schema !== EXPERIMENT_SCHEMA) return false;
	if (typeof value.experimentId !== "string" || typeof value.task !== "string") return false;
	if (typeof value.rounds !== "number" || !isRecord(value.budget)) return false;
	if (typeof value.initialPolicyId !== "string" || !isRecord(value.initialPolicy)) return false;
	if (!Array.isArray(value.arms) || !Array.isArray(value.notes)) return false;
	if (value.headline !== null && !isRecord(value.headline)) return false;
	if (typeof value.sharedInitialRollout !== "boolean" || typeof value.createdTs !== "number") return false;
	return value.arms.every(
		(arm) =>
			isRecord(arm) &&
			isExperimentArm(arm.arm) &&
			typeof arm.runId === "string" &&
			typeof arm.storeDir === "string" &&
			Array.isArray(arm.rounds) &&
			isRecord(arm.totals),
	);
}

/** Write `<dir>/experiments/<id>/result.json` (dir 0700, file 0600, 2-space JSON) and return its path. */
export function writeExperimentResult(dir: string, result: ExperimentResult): string {
	const path = experimentResultPath(dir, result.experimentId);
	mkdirSync(dirname(path), { recursive: true, mode: DIR_MODE });
	writeFileSync(path, `${JSON.stringify(result, undefined, 2)}\n`, { mode: FILE_MODE });
	return path;
}

/** Read and validate a persisted result; `DreamStoreError` when missing, unparsable or of another schema. */
export function readExperimentResult(dir: string, experimentId: string): ExperimentResult {
	const path = experimentResultPath(dir, experimentId);
	if (!existsSync(path)) throw new DreamStoreError(`no experiment ${experimentId} in ${dir}`);
	let parsed: unknown;
	try {
		parsed = JSON.parse(readFileSync(path, "utf8"));
	} catch {
		throw new DreamStoreError(`experiment result ${path} is not valid JSON`);
	}
	if (!isExperimentResult(parsed)) {
		throw new DreamStoreError(`experiment result ${path} is not a ${EXPERIMENT_SCHEMA} document`);
	}
	return parsed;
}

function assembleResult(
	plan: ExperimentPlan,
	arms: readonly ExperimentArmResult[],
	sharedInitialRollout: boolean,
): ExperimentResult {
	return {
		schema: EXPERIMENT_SCHEMA,
		experimentId: plan.experimentId,
		task: plan.taskId,
		...(plan.n !== undefined ? { n: plan.n } : {}),
		seed: plan.seed,
		rounds: plan.rounds,
		budget: { ...plan.budget },
		objective: { ...plan.objective },
		initialPolicyId: plan.initialPolicyId,
		initialPolicy: { ...plan.initialPolicy },
		arms: [...arms],
		headline: computeHeadline(arms),
		sharedInitialRollout,
		createdTs: plan.createdTs,
		notes: [...plan.notes],
	};
}

function experimentSpanAttrs(plan: ExperimentPlan, mode: DreamMode): Record<string, string | number> {
	return {
		"dream.experiment_id": plan.experimentId,
		"dream.task": plan.taskId,
		"dream.seed": plan.seed,
		"dream.rounds": plan.rounds,
		"dream.arms": plan.arms.map((arm) => arm.arm).join(","),
		"dream.mode": mode,
	};
}

function armSpanAttrs(plan: ExperimentPlan, arm: ExperimentArmPlan): Record<string, string | boolean> {
	return {
		"dream.experiment_id": plan.experimentId,
		"dream.arm": arm.arm,
		"dream.fixed_policy": arm.fixedPolicy,
		"dream.guided": arm.guided,
	};
}

const LOCAL_MODE: ExperimentArmMode = { proposer: "local", dreamer: "local" };

/**
 * The synchronous, zero-token local runner (the standalone CLI). Every arm runs
 * `runDreamLoop` with no injected rng, so each seeds itself identically and
 * round 1 is byte-identical across arms. Only `dream` and `fixed` are served.
 */
export function runExperiment(spec: ExperimentSpec, options: ExperimentRunOptions): ExperimentResult {
	const plan = planExperiment(spec, options, LOCAL_EXPERIMENT_ARMS);
	resetExperimentDir(plan);
	return withSpan("dream.experiment", experimentSpanAttrs(plan, "local"), () => {
		const arms: ExperimentArmResult[] = [];
		for (const arm of plan.arms) {
			const loop = withSpan("dream.experiment_arm", armSpanAttrs(plan, arm), (span) => {
				const result = runDreamLoop(arm.loop);
				span.setAttributes({ "dream.run_id": result.runId });
				return result;
			});
			arms.push(buildArmResult(arm, LOCAL_MODE, loop));
		}
		const result = assembleResult(plan, arms, false);
		writeExperimentResult(plan.dir, result);
		return result;
	});
}

/** Observability-only phase progress a runner may relay from inside an arm. */
export interface ExperimentArmProgress {
	phase: "rollout" | "dreaming" | "redeploying";
	iteration: number;
	bestNodeScore: number;
	treeId?: string;
}

/** The one round-1 rollout an LLM runner shares across arms (see `ExperimentArmRunner.prepare`). */
export interface ExperimentSharedRollout {
	treeId: string;
	bestScore: number;
	revealedCount: number;
	rounds: number;
	tokens: number;
	handlerCalls: DreamHandlerCalls;
}

/**
 * How `runExperimentWithRunner` drives one arm. The local runner wraps
 * `runDreamLoop`; the LLM lane's runner wraps `runDreamLoopWithAgent` with the
 * arm's `fixedPolicy`/guidance settings and, through `prepare`, may roll round 1
 * out once into `plan.arms[0].dir`, `copyTree` it into every other arm dir and
 * hand every arm the same `initialRollout`.
 */
export interface ExperimentArmRunner {
	/** The proposer/dreamer mode recorded on the arm's result. */
	mode(arm: ExperimentArmPlan): ExperimentArmMode;
	/** Optional shared round 1; undefined means every arm rolls out its own. */
	prepare?(plan: ExperimentPlan): Promise<ExperimentSharedRollout | undefined>;
	/** Run the arm's whole loop (`arm.loop.iterations + 1` rollouts) into `arm.dir`. */
	run(
		arm: ExperimentArmPlan,
		shared: ExperimentSharedRollout | undefined,
		onProgress: (event: ExperimentArmProgress) => void,
	): Promise<DreamLoopResult>;
}

export type ExperimentProgressEvent =
	| { type: "arm_start"; arm: ExperimentArm; armIndex: number; armCount: number }
	| {
			type: "phase";
			arm: ExperimentArm;
			phase: ExperimentArmProgress["phase"];
			iteration: number;
			bestNodeScore: number;
			treeId?: string;
	  }
	| {
			type: "round";
			arm: ExperimentArm;
			round: number;
			roundBest: number;
			cumulativeBest: number;
			cumulativeProbes: number;
			tokens: number;
	  }
	| { type: "arm_end"; arm: ExperimentArm; finalBest: number; totalProbes: number }
	| { type: "completed"; experimentId: string; resultPath: string };

export interface ExperimentRunnerOptions extends ExperimentRunOptions {
	runner: ExperimentArmRunner;
	/** Arms the runner can serve; a spec arm outside it is refused before anything runs. */
	allowedArms?: readonly ExperimentArm[];
	/** Checked before every arm; an abort leaves no partial result. */
	signal?: AbortSignal;
	onProgress?: (event: ExperimentProgressEvent) => void;
}

/** The in-session local runner: `runDreamLoop` per arm, zero tokens, no shared rollout. */
export function createLocalArmRunner(): ExperimentArmRunner {
	return {
		mode: () => ({ ...LOCAL_MODE }),
		run: async (arm, shared) => {
			if (shared) throw new Error("the local arm runner does not take a shared initial rollout");
			return runDreamLoop(arm.loop);
		},
	};
}

function isAbortError(error: unknown, signal: AbortSignal | undefined): boolean {
	if (signal?.aborted) return true;
	return error instanceof Error && (error.name === "ExperimentAbortError" || error.name === "DreamAbortError");
}

function yieldToEventLoop(): Promise<void> {
	return new Promise((resolve) => setImmediate(resolve));
}

/**
 * The asynchronous in-session core. Because an experiment runs past the user
 * turn, it mints a DETACHED-ROOT `dream.experiment` span (a fresh trace carrying
 * the launching turn's `trigger.trace_id`) and runs the arms inside its context,
 * so each arm's `dream.experiment_arm` is its child and an LLM arm's `dream.run`
 * (itself a detached root) links back through `trigger.trace_id`. An abort or
 * error ends the span (`dream.stopped: aborted` or the recorded error), writes no
 * result and rethrows.
 */
export async function runExperimentWithRunner(
	spec: ExperimentSpec,
	options: ExperimentRunnerOptions,
): Promise<ExperimentResult> {
	const plan = planExperiment(spec, options, options.allowedArms ?? EXPERIMENT_ARMS);
	resetExperimentDir(plan);
	const modes = plan.arms.map((arm) => options.runner.mode(arm));
	const mode: DreamMode = modes.some((m) => m.proposer === "llm" || m.dreamer === "llm") ? "llm" : "local";
	const trigger = currentTraceContext()?.traceId;
	const span = runWithTraceContext(undefined, () =>
		startSpan("dream.experiment", {
			...experimentSpanAttrs(plan, mode),
			...(trigger ? { "trigger.trace_id": trigger } : {}),
		}),
	);
	try {
		const result = await runWithTraceContext(span.context, () => runArms(plan, modes, options));
		span.end();
		return result;
	} catch (error) {
		if (isAbortError(error, options.signal)) span.setAttributes({ "dream.stopped": "aborted" });
		else span.recordError(error);
		span.end();
		throw error;
	}
}

async function runArms(
	plan: ExperimentPlan,
	modes: readonly ExperimentArmMode[],
	options: ExperimentRunnerOptions,
): Promise<ExperimentResult> {
	const { runner, signal, onProgress } = options;
	const checkAbort = (where: string): void => {
		if (signal?.aborted) throw new ExperimentAbortError(`dream experiment aborted before ${where}`);
	};
	checkAbort("the first arm");
	const shared = runner.prepare ? await runner.prepare(plan) : undefined;
	const arms: ExperimentArmResult[] = [];
	for (const arm of plan.arms) {
		await yieldToEventLoop();
		checkAbort(`arm ${arm.arm}`);
		onProgress?.({ type: "arm_start", arm: arm.arm, armIndex: arm.index, armCount: plan.arms.length });
		const loop = await withSpan("dream.experiment_arm", armSpanAttrs(plan, arm), async (armSpan: Span) => {
			const result = await runner.run(arm, shared, (event) =>
				onProgress?.({ type: "phase", arm: arm.arm, ...event }),
			);
			armSpan.setAttributes({ "dream.run_id": result.runId });
			return result;
		});
		const armResult = buildArmResult(arm, modes[arm.index] ?? LOCAL_MODE, loop);
		for (const row of armResult.rounds) {
			onProgress?.({
				type: "round",
				arm: arm.arm,
				round: row.round,
				roundBest: row.roundBest,
				cumulativeBest: row.cumulativeBest,
				cumulativeProbes: row.cumulativeProbes,
				tokens: row.tokens,
			});
		}
		onProgress?.({
			type: "arm_end",
			arm: arm.arm,
			finalBest: armResult.totals.finalBest,
			totalProbes: armResult.totals.probes,
		});
		arms.push(armResult);
	}
	checkAbort("writing the result");
	const result = assembleResult(plan, arms, shared !== undefined);
	const resultPath = writeExperimentResult(plan.dir, result);
	onProgress?.({ type: "completed", experimentId: plan.experimentId, resultPath });
	return result;
}
