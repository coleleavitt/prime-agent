/**
 * The flag-gated LLM path for Dream-RSI: an ASYNC proposer and dreamer that drive
 * a child coding agent, plus the async twins of the rollout and full loop.
 *
 * This file is reached ONLY in-session, by a caller that imports `./llm.js`
 * directly and holds a `RunAgentHandler`. It is deliberately NOT re-exported by
 * `index.ts` (see the note there): a plain `import` of the dream barrel can never
 * pull the child-agent call path, so the default local path stays token-free and
 * socket-free. Every child call spends tokens; nothing else here does.
 *
 * Soundness is unchanged from the local path and rests on EXISTING trust
 * boundaries, never on the child's cooperation:
 *   - a dreamed policy is DATA parsed by the strict `parseExplorationPolicy`
 *     (unknown key / wrong type / out-of-range is rejected), then scored and
 *     selected by `selectBestPolicy`, which returns the argmax of
 *     {current} ∪ candidates with current winning ties — so a worse or malformed
 *     LLM policy can NEVER regress the deployed one;
 *   - a proposed artifact is validated by `task.deserialize` before it enters the
 *     tree, then re-scored by `task.evaluate`.
 * There is no eval/Function/spawn of any child output anywhere here.
 *
 * The LLM path is inherently non-deterministic, but its plumbing is deterministic
 * given recorded outcomes: tree ids, persistence, scoring and replay all reuse the
 * exact helpers the sync driver uses, so a recorded tree replays identically.
 */

import { currentTraceContext, runWithTraceContext, type Span, startSpan, withSpan } from "@earendil-works/pi-ai";
import type { ChildCall } from "../ravo/controller.js";
import { type ChildRuntimeScope, createRunAgentChildCall } from "../ravo/runtime-adapter.js";
import type { RunAgentHandler } from "../run-agent.js";
import { proposePolicies, runDreaming, selectBestPolicy } from "./improve.js";
import { type DreamLoopResult, freezePool } from "./loop.js";
import { DEFAULT_OBJECTIVE, type ReplayObjectiveConfig } from "./objective.js";
import {
	DEFAULT_POLICY,
	type ExplorationPolicy,
	POLICY_BOUNDS,
	parseExplorationPolicy,
	policyId,
	RECOVERY_POLICIES,
	SELECTION_RULES,
	STOP_RULES,
} from "./policy.js";
import { type AsyncProposer, asyncOf, createLocalProposer } from "./proposer.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import {
	applyRoundStop,
	beginRollout,
	commitAttempt,
	type ExploreOptions,
	type ExploreResult,
	exploreSpanAttrs,
	finishRollout,
	IMPROVE_EPS,
	selectRoundCells,
} from "./rollout.js";
import type { DreamTaskId, ProposeParams, ScoredTask } from "./task.js";
import type { DreamClock, DreamMode } from "./types.js";

/** Per-attempt child token budget when a caller does not set one. */
const DEFAULT_CHILD_TOKEN_BUDGET = 200_000;
const JSON_OBJECT_ONLY = "Return exactly one JSON object and nothing else: no prose, no code fences.";
const JSON_ARRAY_ONLY = "Return exactly one JSON array and nothing else: no prose, no code fences.";

/**
 * The sentinel a child abort surfaces as, so the driver stops the whole run
 * instead of silently falling back to the local proposer. `runDreamLoopWithAgent`
 * records it on the detached-root span and re-throws it to its caller.
 */
export class DreamAbortError extends Error {
	constructor(message = "dream run aborted") {
		super(message);
		this.name = "DreamAbortError";
	}
}

export function isDreamAbortError(error: unknown): error is DreamAbortError {
	return error instanceof DreamAbortError;
}

interface ProposeChildInput {
	parentJson: unknown;
	params: ProposeParams;
	round: number;
}

interface DreamChildInput {
	current: ExplorationPolicy;
	m: number;
}

export interface LlmProposerOptions {
	/** Child runtime scope: `{ model?, tools: "none", maxTurns, tokenBudget }`. */
	scope: ChildRuntimeScope;
	/** Cancels the child; an aborted child surfaces as `DreamAbortError`. */
	signal: AbortSignal;
	/** Per-attempt token budget handed to the child call. */
	tokenBudget: number;
	/** Task-specific guidance appended to the prompt (the contract and PUBLIC examples; never hidden tests). */
	promptContext?: string;
}

export interface LlmDreamerOptions {
	scope: ChildRuntimeScope;
	signal: AbortSignal;
	tokenBudget: number;
	/** Explicit labelled fork used only when the LLM dreamer fails or yields no in-bounds policy. */
	localFallbackRng: SeededRng;
}

/**
 * An `AsyncProposer` that generates each attempt through a child coding agent.
 * The child emits one JSON artifact, validated by `task.deserialize` before it
 * can enter the tree. On a non-abort child failure the proposer FALLS BACK to the
 * task's local `propose` (deterministic, zero extra tokens beyond what the child
 * already spent); on an abort it throws `DreamAbortError` so the driver stops.
 */
export function createLlmProposer(
	runAgent: RunAgentHandler,
	task: ScoredTask<unknown>,
	options: LlmProposerOptions,
): AsyncProposer<unknown> {
	const childCall = retrying(
		createRunAgentChildCall<ProposeChildInput, unknown>(runAgent, {
			prompt: buildProposePrompt(task.id, options.promptContext),
			validate: (value) => task.deserialize(value),
			scope: options.scope,
		}),
		1,
	);
	return {
		propose(parent, params, rng, round) {
			return withSpan("dream.llm_propose", { "dream.round": round }, async (span) => {
				const parentJson = parent === null ? null : task.serialize(parent);
				const result = await childCall(
					{ parentJson, params, round },
					{ signal: options.signal, tokenBudget: options.tokenBudget },
				);
				if (result.status === "deferred") {
					throw new Error("dream LLM proposer received an unexpected deferred child result");
				}
				if (result.status === "completed") {
					span.setAttributes({ "dream.tokens": result.tokens, "dream.llm_fallback": false });
					return { artifact: result.value, tokens: result.tokens };
				}
				if (result.status === "aborted" || options.signal.aborted) {
					throw new DreamAbortError("dream proposer child aborted");
				}
				// error / turn_limit / budget_exceeded: fall back to the deterministic local
				// proposer, keeping the tokens the child already spent.
				const fallback = task.propose(parent, params, rng, round);
				span.setAttributes({ "dream.tokens": result.tokens, "dream.llm_fallback": true });
				return { artifact: fallback, tokens: result.tokens };
			});
		},
	};
}

/**
 * The LLM dreamer: ask a child agent for `m` revised policies, keep only the ones
 * the STRICT `parseExplorationPolicy` accepts (dropping any with an unknown field,
 * wrong type, or out-of-range value), and fall back to the local `proposePolicies`
 * when the call fails or every candidate is dropped. The survivors are handed to
 * `runDreaming`, which scores and selects them under the same no-regression rule,
 * so a bad policy can never be deployed.
 */
export async function proposePoliciesWithAgent(
	runAgent: RunAgentHandler,
	current: ExplorationPolicy,
	m: number,
	options: LlmDreamerOptions,
): Promise<{ candidates: ExplorationPolicy[]; tokens: number }> {
	const requested = Math.max(0, Math.trunc(m));
	const childCall = retrying(
		createRunAgentChildCall<DreamChildInput, ExplorationPolicy[]>(runAgent, {
			prompt: buildDreamPrompt,
			validate: parseCandidateArray,
			scope: options.scope,
		}),
		1,
	);
	return withSpan("dream.llm_dream", { "dream.candidates_requested": requested }, async (span) => {
		const result = await childCall(
			{ current, m: requested },
			{ signal: options.signal, tokenBudget: options.tokenBudget },
		);
		if (result.status === "deferred") {
			throw new Error("dream LLM dreamer received an unexpected deferred child result");
		}
		if (result.status === "completed" && result.value.length > 0) {
			span.setAttributes({
				"dream.candidates_kept": result.value.length,
				"dream.llm_fallback": false,
				"dream.tokens": result.tokens,
			});
			return { candidates: result.value, tokens: result.tokens };
		}
		// The call failed, was aborted, or every candidate was dropped: fall back to the
		// local search. runDreamLoopWithAgent's own signal checks stop an aborted run.
		const candidates = proposePolicies(current, requested, options.localFallbackRng);
		span.setAttributes({
			"dream.candidates_kept": candidates.length,
			"dream.llm_fallback": true,
			"dream.tokens": result.tokens,
		});
		return { candidates, tokens: result.tokens };
	});
}

/**
 * The async twin of `runOnlineExploration`. It reuses the exact rollout helpers,
 * so tree ids, the `dream.explore`/`dream.round`/`dream.attempt` spans and scalar
 * attrs, persistence, scoring and the stop rule are identical; the only difference
 * is `await proposer.propose(...)`. When the proposer is the LLM one, its
 * `dream.llm_propose` span therefore nests under `dream.round` as a sibling of
 * `dream.attempt`; the local `asyncOf` proposer opens no span, so a local-proposer
 * rollout is byte-identical to the fully synchronous one.
 */
export function runOnlineExplorationWithAgent(
	options: ExploreOptions,
	proposer: AsyncProposer<unknown>,
	signal?: AbortSignal,
): Promise<ExploreResult> {
	return withSpan(options.spanName ?? "dream.explore", exploreSpanAttrs(options), async (span) => {
		const state = beginRollout(options);
		span.setAttributes({ "dream.tree_id": state.treeId });

		let rounds = 0;
		let bestScore = state.tree.bestScore();
		let lastImproveRound = 0;
		let tokens = 0;

		for (let round = 1; round <= state.k1; round++) {
			if (signal?.aborted) break;
			const cells = selectRoundCells(state, round);
			if (cells.length === 0) break;
			rounds = round;
			const revealedThisRound: string[] = [];
			const revealedBefore = state.tree.size - 1;
			await withSpan(
				"dream.round",
				{
					"dream.round": round,
					"dream.batch_size": cells.length,
					"dream.revealed_count": revealedBefore,
					"dream.best_score": bestScore,
				},
				async () => {
					for (const cell of cells) {
						const branch = state.tree.children(cell.nodeId).length;
						const attemptRng = state.rng.fork(`r${round}:${cell.nodeId}:b${branch}`);
						const parentArtifact = state.artifacts.get(cell.nodeId) ?? null;
						const outcome = await proposer.propose(parentArtifact, state.params, attemptRng, round);
						const committed = commitAttempt(state, cell, round, outcome);
						revealedThisRound.push(committed.node.id);
						tokens += committed.tokens;
					}
				},
			);
			state.writer.appendReveal({ type: "reveal", round, ids: revealedThisRound });
			const roundBest = state.tree.bestScore();
			if (roundBest > bestScore + IMPROVE_EPS) {
				bestScore = roundBest;
				lastImproveRound = round;
			}
			if (applyRoundStop(state, round, bestScore, lastImproveRound)) break;
		}

		return finishRollout(state, rounds, tokens);
	});
}

/**
 * Observability-only progress the async driver emits as each phase of the loop
 * begins or the run completes. The callback never touches the rng, tree,
 * scoring or persistence, so a run with `onProgress` grows byte-identical trees
 * to one without it. `DreamRunService` maps these onto `DreamRunStatus`; the
 * terminal `accepted`/`stopped` phase is decided from the `DreamLoopResult`, not
 * from a progress event.
 */
export type DreamProgressEvent =
	| {
			type: "phase";
			phase: "rollout" | "dreaming" | "redeploying";
			iteration: number;
			bestNodeScore: number;
			treeId?: string;
	  }
	| { type: "completed"; iteration: number; bestNodeScore: number; finalPolicyScore: number; improved: boolean };

export interface DreamLoopWithAgentOptions {
	runAgent: RunAgentHandler;
	task: ScoredTask<unknown>;
	/** Task id recorded on every tree header; defaults to `task.id`. */
	taskId?: DreamTaskId;
	/** Task size parameter (e.g. circle count) recorded on the header. */
	n?: number;
	seed: number | string;
	/** The one injected clock; the only clock-derived ids are the tree ids and the run id. */
	clock: DreamClock;
	workers: number;
	k1: number;
	k2: number;
	dreams: number;
	iterations: number;
	dir: string;
	objective?: ReplayObjectiveConfig;
	/** Injected rng; defaults to a fresh seeded rng from `seed`. */
	rng?: SeededRng;
	/** When true, each attempt is generated by a child agent; otherwise the local zero-token proposer. */
	useLlmProposer: boolean;
	/** When true, each dreaming step's candidates come from a child agent; otherwise local search. */
	useLlmDreamer: boolean;
	/** Child runtime scope shared by the proposer and dreamer children. */
	scope: ChildRuntimeScope;
	/** The session abort signal; a cancel stops the run and re-throws `DreamAbortError`. */
	signal: AbortSignal;
	/** Task-specific guidance appended to the proposer prompt (contract + PUBLIC examples). */
	proposerPromptContext?: string;
	/** Per-attempt token budget handed to each child call; defaults to a bounded value. */
	childTokenBudget?: number;
	/** Observability-only per-phase progress; never affects the grown tree or scoring. */
	onProgress?: (event: DreamProgressEvent) => void;
}

/**
 * The async in-session orchestrator. Because dreaming runs past the user turn, it
 * mints a DETACHED-ROOT `dream.run` span (a fresh trace carrying `trigger.trace_id`
 * of the turn that launched it), so the loop outlives the turn without a child
 * outliving its parent. Iteration 0 rolls out with the default policy; each later
 * iteration freezes the pool, resolves candidates (LLM dreamer when enabled, else
 * local search), selects a no-worse policy through the existing `runDreaming` seam,
 * and redeploys it. `useLlmProposer` and `useLlmDreamer` are INDEPENDENT toggles;
 * one async driver serves both, so the local-proposer path stays byte-identical.
 */
export function runDreamLoopWithAgent(options: DreamLoopWithAgentOptions): Promise<DreamLoopResult> {
	const trigger = currentTraceContext()?.traceId;
	const taskId: DreamTaskId = options.taskId ?? options.task.id;
	const iterations = Math.max(0, Math.trunc(options.iterations));
	const span = runWithTraceContext(undefined, () =>
		startSpan("dream.run", {
			"dream.task": taskId,
			"dream.seed": options.seed,
			"dream.workers": options.workers,
			"dream.k1": options.k1,
			"dream.k2": options.k2,
			"dream.dreams": options.dreams,
			"dream.iterations": iterations,
			"dream.mode": "llm" satisfies DreamMode,
			...(trigger ? { "trigger.trace_id": trigger } : {}),
		}),
	);
	return runDreamRun(options, taskId, iterations, span);
}

async function runDreamRun(
	options: DreamLoopWithAgentOptions,
	taskId: DreamTaskId,
	iterations: number,
	span: Span,
): Promise<DreamLoopResult> {
	try {
		const value = await runWithTraceContext(span.context, () => dreamLoopBody(options, taskId, iterations));
		span.end();
		return value;
	} catch (error) {
		if (isDreamAbortError(error)) span.setAttributes({ "dream.stopped": "aborted" });
		else span.recordError(error);
		span.end();
		throw error;
	}
}

async function dreamLoopBody(
	options: DreamLoopWithAgentOptions,
	taskId: DreamTaskId,
	iterations: number,
): Promise<DreamLoopResult> {
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const rng = options.rng ?? createSeededRng(options.seed);
	const scoreCfg = { k2: options.k2, objective };
	const childTokenBudget = options.childTokenBudget ?? DEFAULT_CHILD_TOKEN_BUDGET;
	const { runAgent, task, dir, signal } = options;

	const makeProposer = (): AsyncProposer<unknown> =>
		options.useLlmProposer
			? createLlmProposer(runAgent, task, {
					scope: options.scope,
					signal,
					tokenBudget: childTokenBudget,
					...(options.proposerPromptContext ? { promptContext: options.proposerPromptContext } : {}),
				})
			: asyncOf(createLocalProposer(task));

	const rollout = (policy: ExplorationPolicy, iteration: number): Promise<ExploreResult> => {
		if (signal.aborted) throw new DreamAbortError("dream run aborted before rollout");
		return runOnlineExplorationWithAgent(
			{
				task,
				taskId,
				...(options.n !== undefined ? { n: options.n } : {}),
				seed: options.seed,
				rng: rng.fork(`iter:${iteration}`),
				clock: options.clock,
				workers: options.workers,
				k1: options.k1,
				dir,
				policy,
				iteration,
			},
			makeProposer(),
			signal,
		);
	};

	const treeIds: string[] = [];
	const chosenPolicies: ExplorationPolicy[] = [];
	let bestNodeScore = 0;
	let seenBest = false;
	let tokens = 0;
	const noteBest = (score: number): void => {
		if (!seenBest || score > bestNodeScore) {
			bestNodeScore = score;
			seenBest = true;
		}
	};

	const first = await rollout(DEFAULT_POLICY, 0);
	treeIds.push(first.treeId);
	tokens += first.tokens;
	noteBest(first.bestScore);
	options.onProgress?.({ type: "phase", phase: "rollout", iteration: 0, bestNodeScore, treeId: first.treeId });

	let current: ExplorationPolicy = DEFAULT_POLICY;
	for (let iteration = 1; iteration <= iterations; iteration++) {
		if (signal.aborted) throw new DreamAbortError("dream run aborted before iteration");
		options.onProgress?.({ type: "phase", phase: "dreaming", iteration, bestNodeScore });
		const pool = freezePool(dir, taskId);
		let resolved: ExplorationPolicy[] | undefined;
		if (options.useLlmDreamer) {
			const dreamed = await proposePoliciesWithAgent(runAgent, current, options.dreams, {
				scope: options.scope,
				signal,
				tokenBudget: childTokenBudget,
				localFallbackRng: rng.fork(`dream-fallback:${iteration}`),
			});
			resolved = dreamed.candidates;
			tokens += dreamed.tokens;
		}
		const dream = runDreaming({
			current,
			pool,
			dreams: options.dreams,
			k2: options.k2,
			rng: rng.fork(`dream:${iteration}`),
			objective,
			...(resolved ? { proposeCandidates: () => resolved as ExplorationPolicy[] } : {}),
		});
		current = dream.chosenPolicy;
		chosenPolicies.push(current);
		if (signal.aborted) throw new DreamAbortError("dream run aborted before redeploy");
		const redeployed = await withSpan(
			"dream.redeploy",
			{
				"dream.policy_id": policyId(current),
				"dream.k1": options.k1,
				"dream.workers": options.workers,
				"dream.iteration": iteration,
			},
			async (redeploySpan) => {
				const result = await rollout(current, iteration);
				redeploySpan.setAttributes({ "dream.tree_id": result.treeId });
				return result;
			},
		);
		treeIds.push(redeployed.treeId);
		tokens += redeployed.tokens;
		noteBest(redeployed.bestScore);
		options.onProgress?.({
			type: "phase",
			phase: "redeploying",
			iteration,
			bestNodeScore,
			treeId: redeployed.treeId,
		});
	}

	const finalPool = freezePool(dir, taskId);
	const selection = selectBestPolicy(DEFAULT_POLICY, chosenPolicies, finalPool, scoreCfg);
	options.onProgress?.({
		type: "completed",
		iteration: iterations,
		bestNodeScore,
		finalPolicyScore: selection.chosenScore,
		improved: selection.improved,
	});
	return {
		runId: `${taskId}-s${options.seed}-r${options.clock()}`,
		task: taskId,
		seed: options.seed,
		mode: "llm" satisfies DreamMode,
		iterations,
		treeIds,
		initialPolicyId: policyId(DEFAULT_POLICY),
		initialPolicyScore: selection.currentScore,
		finalPolicy: selection.chosenPolicy,
		finalPolicyId: policyId(selection.chosenPolicy),
		finalPolicyScore: selection.chosenScore,
		improved: selection.improved,
		bestNodeScore,
		tokens,
	};
}

/**
 * Map the returned JSON array through the STRICT policy parser per entry, dropping
 * any candidate the parser rejects. Never throws: a non-array yields `[]`, so the
 * caller falls back to the local search.
 */
function parseCandidateArray(value: unknown): ExplorationPolicy[] {
	if (!Array.isArray(value)) return [];
	const out: ExplorationPolicy[] = [];
	for (const entry of value) {
		try {
			out.push(parseExplorationPolicy(entry));
		} catch {
			// Drop a candidate with an unknown field, a wrong type, or an out-of-range value.
		}
	}
	return out;
}

function buildProposePrompt(taskId: DreamTaskId, promptContext?: string): (input: ProposeChildInput) => string {
	return (input) =>
		[
			`# Dream-RSI proposer (${taskId})`,
			"Improve the candidate below into a better one for this scored task. Everything you need is in this message; do not search, browse, or call tools.",
			`Current candidate (JSON), or null to start fresh:\n${input.parentJson === null ? "null" : JSON.stringify(input.parentJson)}`,
			`Generation hints: stepScale=${input.params.stepScale}, refineDepth=${input.params.refineDepth}, branchWidth=${input.params.branchWidth}; round ${input.round}.`,
			...(promptContext ? [promptContext] : []),
			`It must be a candidate this task can deserialize. ${JSON_OBJECT_ONLY}`,
		].join("\n\n");
}

function buildDreamPrompt(input: DreamChildInput): string {
	return [
		"# Dream-RSI policy dreamer",
		`Propose ${input.m} revised exploration policies that should score better than the current one on replay. A policy is DATA: a flat JSON object with exactly these fields.`,
		policySchemaText(),
		`Current policy:\n${JSON.stringify(input.current)}`,
		`${JSON_ARRAY_ONLY} It must hold ${input.m} policy objects. Any object with an unknown field, a wrong type, or an out-of-range value is discarded.`,
	].join("\n\n");
}

function policySchemaText(): string {
	const numeric = (Object.keys(POLICY_BOUNDS) as (keyof typeof POLICY_BOUNDS)[])
		.map((field) => {
			const bound = POLICY_BOUNDS[field];
			return `- ${field}: ${bound.integer ? "integer" : "number"} in [${bound.min}, ${bound.max}]`;
		})
		.join("\n");
	return [
		"Named-rule fields (use exactly one listed value each):",
		`- selectionRule: one of ${SELECTION_RULES.join(", ")}`,
		`- recoveryPolicy: one of ${RECOVERY_POLICIES.join(", ")}`,
		`- stopRule: one of ${STOP_RULES.join(", ")}`,
		"Numeric fields:",
		numeric,
	].join("\n");
}

/** Retry a structured child once on invalid output; token usage is summed across attempts. */
function retrying<TInput, TOutput>(call: ChildCall<TInput, TOutput>, retries: number): ChildCall<TInput, TOutput> {
	return async (input, options) => {
		let spent = 0;
		for (let attempt = 0; ; attempt += 1) {
			const result = await call(input, options);
			if (result.status === "deferred") return result;
			spent += result.tokens;
			if (result.status === "completed") return { ...result, tokens: spent };
			if (result.status !== "error" || attempt >= retries || options.signal.aborted) {
				return { ...result, tokens: spent };
			}
		}
	};
}
