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
 *     {current} ∪ candidates among the candidates whose replay quality is no
 *     lower than current's, with current winning ties — so a worse, malformed or
 *     exploration-collapsing LLM policy can NEVER regress the deployed one;
 *   - a proposed artifact is validated by `task.deserialize` before it enters the
 *     tree, then re-scored by `task.evaluate`;
 *   - semantic guidance (the paper's ablation) is advisory TEXT prefixed to the
 *     proposer prompt; it is built from recorded artifacts and scalar scores only
 *     (never hidden tests) and the hidden tests still decide every score.
 * There is no eval/Function/spawn of any child output anywhere here.
 *
 * The LLM path is inherently non-deterministic, but its plumbing is deterministic
 * given recorded outcomes: tree ids, persistence, scoring and replay all reuse the
 * exact helpers the sync driver uses, so a recorded tree replays identically.
 *
 * `fixedPolicy` is the paper's "Recursive Fixed Exploration" control (see
 * `loop.ts`): the same loop and the same growing pool, but no dreaming at all.
 * `initialRollout` lets an experiment share one round-1 rollout across arms, and
 * the per-round `rounds` records count handler calls and tokens per role so an
 * experiment can report cost next to (never mixed into) the compute axis.
 */

import { existsSync } from "node:fs";
import { currentTraceContext, runWithTraceContext, type Span, startSpan, withSpan } from "@earendil-works/pi-ai";
import type { ChildCall } from "../ravo/controller.js";
import {
	type ChildRuntimeScope,
	createRunAgentChildCall,
	extractJsonValue,
	type JsonContainer,
	structuredChildRequest,
	structuredChildRunOptions,
} from "../ravo/runtime-adapter.js";
import type { RunAgentHandler, RunAgentResult, RunAgentStatus } from "../run-agent.js";
import { proposePolicies, runDreaming, selectBestPolicy } from "./improve.js";
import { type DreamHandlerCalls, type DreamLoopResult, type DreamRoundRecord, freezePool } from "./loop.js";
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
import {
	type AsyncProposer,
	addProposalTally,
	asyncOf,
	createLocalProposer,
	type ProposalRejectReason,
	type ProposalTally,
	type ProposeOutcome,
	tallyAccepted,
	tallyRejected,
	zeroProposalTally,
} from "./proposer.js";
import { excerptOf, RejectionLog, rejectionsPath } from "./rejections.js";
import { createSeededRng, type SeededRng } from "./rng.js";
import {
	applyRoundStop,
	attemptRng,
	beginRollout,
	commitAttempt,
	type ExploreOptions,
	type ExploreResult,
	exploreSpanAttrs,
	finishRollout,
	IMPROVE_EPS,
	selectRoundCells,
} from "./rollout.js";
import { DreamStoreError, type RecordedTree, treePath } from "./store.js";
import type { DreamTaskId, ProposeParams, ScoredTask } from "./task.js";
import type { DreamClock, DreamMode, NodeRecord } from "./types.js";

/** Per-attempt child token budget when a caller does not set one. */
export const DEFAULT_CHILD_TOKEN_BUDGET = 200_000;
/** Child results a proposer attempt examines at most: the first plus this many retries on a retryable rejection. */
export const PROPOSER_RETRIES = 1;
const JSON_OBJECT_ONLY = "Return exactly one JSON object and nothing else: no prose, no code fences.";
const JSON_ARRAY_ONLY = "Return exactly one JSON array and nothing else: no prose, no code fences.";

/**
 * The LAST line of every proposer prompt. A child that reasons out loud around
 * the object, fences it, or runs past its output cap is the measured failure
 * mode (81/83 fallbacks on one real run), so the instruction is explicit, names
 * every forbidden wrapper, and comes after everything else in the prompt.
 */
export const PROPOSER_JSON_ONLY = "Return ONLY the JSON object, no prose, no code fences, no text before or after it.";
const PROPOSER_OUTPUT_CONTRACT = [
	"Output contract:",
	"- Your entire reply is ONE JSON object: the improved candidate, in the exact shape the contract above describes (without a contract, the same keys as the current candidate; a derived field such as a score or peak may be omitted).",
	"- Do not think out loud, explain, or add any text before or after the object. Do not wrap it in markdown code fences.",
	"- Keep the reply as short as the object itself: a reply that runs past its output limit is discarded and replaced by a local mutation.",
].join("\n");

/**
 * The first line of every child prompt, by role. Each is a prompt's first
 * characters, so a caller holding only the `RunAgentRequest` (a test stub, an
 * accounting wrapper) can tell the roles apart without inspecting the body.
 */
export const PROPOSER_PROMPT_HEADER = "# Dream-RSI proposer";
export const DREAMER_PROMPT_HEADER = "# Dream-RSI policy dreamer";
export const GUIDANCE_PROMPT_HEADER = "# Dream-RSI guidance writer";

/** Top recorded candidates per tree in a guidance digest, and the per-artifact JSON cap. */
export const DEFAULT_GUIDANCE_TOP_K = 3;
export const DEFAULT_GUIDANCE_MAX_ARTIFACT_CHARS = 2000;

const GUIDANCE_PROMPT_PREFIX =
	"Directional insights from prior trajectories (advisory; the hidden tests, not these notes, decide the score):";

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
	/**
	 * Semantic guidance: directional insights from prior trajectories, inserted
	 * after the prompt header only when non-empty. Advisory text, never a score.
	 */
	guidance?: string;
	/**
	 * Per-rollout provenance the proposer records into (mutated through
	 * `tallyAccepted`/`tallyRejected`): every child result examined, accepted or
	 * rejected by reason, and the attempts that fell back to the local mutator.
	 * The driver snapshots it per round.
	 */
	tally?: ProposalTally;
	/** Where every rejected child result is written (reason, status, tokens, a bounded output excerpt). */
	rejections?: RejectionLog;
	/** The loop iteration the rollout belongs to, stamped on each rejection record; defaults to 0. */
	iteration?: number;
}

/** What became of one child proposer result before it could enter the tree. */
type ChildOutcome<T> =
	| { status: "accepted"; value: T; tokens: number; outputTokens: number }
	| {
			status: "rejected";
			reason: ProposalRejectReason;
			childStatus: RunAgentStatus;
			tokens: number;
			outputTokens: number;
			stopReason?: string;
			error?: string;
			excerpt: string;
	  };

/**
 * Rejections worth one more child call. A cap overrun (`length`) is not: the
 * retry would most likely run away again, doubling the most expensive failure.
 * A turn limit, budget or abort is terminal for the attempt by definition.
 */
const RETRYABLE_REJECTIONS: ReadonlySet<ProposalRejectReason> = new Set<ProposalRejectReason>([
	"error",
	"parse",
	"shape",
	"invalid-candidate",
]);

export interface LlmDreamerOptions {
	scope: ChildRuntimeScope;
	signal: AbortSignal;
	tokenBudget: number;
	/** Explicit labelled fork used only when the LLM dreamer fails or yields no in-bounds policy. */
	localFallbackRng: SeededRng;
}

/**
 * An `AsyncProposer` that generates each attempt through a child coding agent.
 * The child's output is searched for its JSON object (`extractJsonValue`: code
 * fences and prose around it are tolerated, since the object wrapped in an
 * explanation was the measured failure mode), then validated by
 * `task.deserialize` before it can enter the tree as an `origin: "llm"` node.
 * A retryable rejection gets `PROPOSER_RETRIES` more child calls. When the last
 * result is still rejected the proposer FALLS BACK to the task's local `propose`
 * (deterministic, zero extra tokens beyond what the child already spent) and the
 * node is `origin: "local"`; an abort throws `DreamAbortError` so the driver
 * stops. Every child result is tallied (`options.tally`) and every rejection is
 * logged (`options.rejections`) and put on the `dream.llm_propose` span, so a
 * fallback is never mistaken for the agent's work and its cause is recoverable.
 */
export function createLlmProposer(
	runAgent: RunAgentHandler,
	task: ScoredTask<unknown>,
	options: LlmProposerOptions,
): AsyncProposer<unknown> {
	const prompt = buildProposePrompt(task.id, options.promptContext, options.guidance);
	const tally = options.tally ?? zeroProposalTally();
	const iteration = options.iteration ?? 0;
	const child = (input: ProposeChildInput): Promise<ChildOutcome<unknown>> =>
		runStructuredChild(runAgent, prompt(input), options, "object", (value) => task.deserialize(value));
	return {
		propose(parent, params, rng, round): Promise<ProposeOutcome<unknown>> {
			return withSpan(
				"dream.llm_propose",
				{ "dream.round": round },
				async (span): Promise<ProposeOutcome<unknown>> => {
					const parentJson = parent === null ? null : task.serialize(parent);
					const input: ProposeChildInput = { parentJson, params, round };
					let tokens = 0;
					let outputTokens = 0;
					let attempts = 0;
					let last: ChildOutcome<unknown>;
					const reject = (outcome: ChildOutcome<unknown> & { status: "rejected" }, fellBack: boolean): void => {
						tallyRejected(tally, outcome.reason, fellBack);
						options.rejections?.append({
							iteration,
							round,
							attempt: attempts,
							reason: outcome.reason,
							status: outcome.childStatus,
							fellBack,
							tokens: outcome.tokens,
							outputTokens: outcome.outputTokens,
							...(outcome.stopReason === undefined ? {} : { stopReason: outcome.stopReason }),
							...(outcome.error === undefined ? {} : { error: outcome.error }),
							excerpt: outcome.excerpt,
						});
					};
					for (;;) {
						attempts += 1;
						last = await child(input);
						tokens += last.tokens;
						outputTokens += last.outputTokens;
						if (last.status === "accepted") break;
						const retry =
							RETRYABLE_REJECTIONS.has(last.reason) && attempts <= PROPOSER_RETRIES && !options.signal.aborted;
						if (!retry) break;
						reject(last, false);
					}
					span.setAttributes({
						"dream.tokens": tokens,
						"dream.llm_output_tokens": outputTokens,
						"dream.llm_attempts": attempts,
					});
					if (last.status === "accepted") {
						tallyAccepted(tally);
						span.setAttributes({ "dream.llm_fallback": false, "dream.origin": "llm" });
						return { artifact: last.value, tokens, origin: "llm" };
					}
					span.setAttributes({
						"dream.llm_reject_reason": last.reason,
						"dream.llm_status": last.childStatus,
						"dream.llm_reject_excerpt": last.excerpt,
					});
					if (last.reason === "aborted" || options.signal.aborted) {
						reject(last, false);
						span.setAttributes({ "dream.llm_fallback": false });
						throw new DreamAbortError("dream proposer child aborted");
					}
					// The child's output is rejected for good: fall back to the deterministic
					// local proposer, keeping the tokens the child already spent on the node.
					reject(last, true);
					const fallback = task.propose(parent, params, rng, round);
					span.setAttributes({ "dream.llm_fallback": true, "dream.origin": "local" });
					return { artifact: fallback, tokens, origin: "local" };
				},
			);
		},
	};
}

/**
 * One structured child call, classified. A non-completed child maps by status
 * (`error`, `turn-limit`, `budget`, `aborted`). A completed one must yield a
 * JSON value of the requested kind (`parse` otherwise) that `validate` accepts:
 * a `TypeError` is the tasks' structural refusal (`shape`: wrong keys, wrong
 * `weights` length, non-numeric entries), any other error is `invalid-candidate`.
 * When the child's final message stopped at its output cap, a rejection of that
 * output is `length` whatever the parser said, since the cap is the cause.
 */
async function runStructuredChild<T>(
	runAgent: RunAgentHandler,
	prompt: string,
	options: Pick<LlmProposerOptions, "scope" | "signal" | "tokenBudget">,
	container: JsonContainer,
	validate: (value: unknown) => T,
): Promise<ChildOutcome<T>> {
	const result = await runAgent(
		structuredChildRequest(prompt, options.scope),
		structuredChildRunOptions(options.scope, { signal: options.signal, tokenBudget: options.tokenBudget }),
	);
	const tokens = result.usage.totalTokens;
	const outputTokens = result.usage.output;
	const stopReason = lastAssistantStopReason(result);
	const rejected = (reason: ProposalRejectReason, error?: string): ChildOutcome<T> => ({
		status: "rejected",
		reason: stopReason === "length" && result.status === "completed" ? "length" : reason,
		childStatus: result.status,
		tokens,
		outputTokens,
		...(stopReason === undefined ? {} : { stopReason }),
		...(error === undefined ? {} : { error }),
		excerpt: excerptOf(result.output),
	});
	if (result.status !== "completed") return rejected(statusRejectReason(result.status), result.error);
	let value: unknown;
	try {
		value = extractJsonValue(result.output, container);
	} catch (error) {
		return rejected("parse", errorText(error));
	}
	try {
		return { status: "accepted", value: validate(value), tokens, outputTokens };
	} catch (error) {
		return rejected(error instanceof TypeError ? "shape" : "invalid-candidate", errorText(error));
	}
}

function statusRejectReason(status: Exclude<RunAgentStatus, "completed">): ProposalRejectReason {
	switch (status) {
		case "aborted":
			return "aborted";
		case "turn_limit":
			return "turn-limit";
		case "budget_exceeded":
			return "budget";
		case "error":
			return "error";
	}
}

/** The stop reason of the child's final assistant message, when the handler returned its transcript. */
function lastAssistantStopReason(result: RunAgentResult): string | undefined {
	for (let i = result.messages.length - 1; i >= 0; i--) {
		const message = result.messages[i];
		if (message?.role === "assistant") return message.stopReason;
	}
	return undefined;
}

function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
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
			extractJson: "array",
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

/** One recorded candidate in a guidance digest: its scalar score, round, and serialized artifact (truncated). */
export interface GuidanceNodeDigest {
	score: number;
	round: number;
	artifactJson: string;
}

/** One recorded tree in a guidance digest. Recorded artifacts and scalar scores only; never hidden tests. */
export interface GuidanceTreeDigest {
	treeId: string;
	policyId: string;
	bestScore: number;
	/** Revealed non-root nodes: evaluated attempts. */
	attempts: number;
	/** Online decision rounds the rollout took. */
	rounds: number;
	/** Distinct failure classes seen, sorted. */
	failClasses: string[];
	/** The top-k valid candidates by score (ties by seq), truncated to the artifact cap. */
	topNodes: GuidanceNodeDigest[];
}

/** The guidance writer's input: a bounded, deterministic digest of the frozen pool. */
export interface GuidanceInput {
	taskId: DreamTaskId;
	iteration: number;
	poolSize: number;
	trees: GuidanceTreeDigest[];
}

/** Insight text a guidance writer produced and the tokens it spent (0 for an injected writer). */
export interface GuidanceInsights {
	text: string;
	tokens: number;
}

/**
 * `true` asks a guidance-writer child for the insights (one call per iteration
 * >= 1, spending tokens); an injected `insights` function replaces that call
 * (test-only, token-free). Either way the text is advisory prompt content.
 */
export type SemanticGuidanceOption = true | { insights: (input: GuidanceInput) => Promise<GuidanceInsights> };

function truncateArtifact(json: string, maxChars: number): string {
	const cap = Math.max(0, Math.trunc(maxChars));
	return json.length <= cap ? json : `${json.slice(0, cap)}...`;
}

function artifactJsonOf(tree: RecordedTree, node: NodeRecord): string {
	try {
		return JSON.stringify(tree.loadBlob(node)) ?? "";
	} catch {
		return "";
	}
}

/**
 * Digest a frozen pool for the guidance writer. Deterministic: trees in tree-id
 * order, top nodes by score descending then seq ascending, bounded by `topK` and
 * `maxArtifactChars`. It reads recorded artifacts (through `tree.loadBlob`) and
 * scalar scores only — a task's hidden tests are never part of a tree.
 */
export function buildGuidanceInput(
	pool: readonly RecordedTree[],
	taskId: DreamTaskId,
	iteration: number,
	topK = DEFAULT_GUIDANCE_TOP_K,
	maxArtifactChars = DEFAULT_GUIDANCE_MAX_ARTIFACT_CHARS,
): GuidanceInput {
	const keep = Math.max(0, Math.trunc(topK));
	const trees = [...pool]
		.sort((a, b) => a.header.treeId.localeCompare(b.header.treeId))
		.map((tree): GuidanceTreeDigest => {
			const nonRoot = tree.nodes.filter((node) => node.parentId !== null);
			const valid = tree.nodes
				.filter((node) => node.valid && Number.isFinite(node.score))
				.sort((a, b) => b.score - a.score || a.seq - b.seq);
			const failClasses = [...new Set(nonRoot.flatMap((node) => (node.failClass ? [node.failClass] : [])))].sort();
			return {
				treeId: tree.header.treeId,
				policyId: tree.header.policyId,
				bestScore: valid.length > 0 ? valid[0]!.score : 0,
				attempts: nonRoot.length,
				rounds: nonRoot.reduce((max, node) => Math.max(max, node.round), 0),
				failClasses,
				topNodes: valid.slice(0, keep).map((node) => ({
					score: node.score,
					round: node.round,
					artifactJson: truncateArtifact(artifactJsonOf(tree, node), maxArtifactChars),
				})),
			};
		});
	return { taskId, iteration, poolSize: trees.length, trees };
}

interface GuidanceCallOptions {
	scope: ChildRuntimeScope;
	signal: AbortSignal;
	tokenBudget: number;
}

/**
 * Resolve one iteration's semantic guidance inside `dream.llm_guidance`. A child
 * failure falls back to EMPTY guidance (`dream.llm_fallback: true`) so the arm
 * still runs; an abort throws `DreamAbortError`; an injected writer is called in
 * place of the child.
 */
function resolveGuidance(
	runAgent: RunAgentHandler,
	option: SemanticGuidanceOption,
	input: GuidanceInput,
	options: GuidanceCallOptions,
): Promise<GuidanceInsights> {
	return withSpan(
		"dream.llm_guidance",
		{ "dream.iteration": input.iteration, "dream.pool_size": input.poolSize },
		async (span) => {
			if (option !== true) {
				const produced = await option.insights(input);
				span.setAttributes({ "dream.tokens": produced.tokens, "dream.llm_fallback": false });
				return produced;
			}
			const childCall = retrying(
				createRunAgentChildCall<GuidanceInput, string>(runAgent, {
					prompt: buildGuidancePrompt,
					validate: parseInsights,
					scope: options.scope,
					extractJson: "object",
				}),
				1,
			);
			const result = await childCall(input, { signal: options.signal, tokenBudget: options.tokenBudget });
			if (result.status === "deferred") {
				throw new Error("dream LLM guidance received an unexpected deferred child result");
			}
			if (result.status === "completed") {
				span.setAttributes({ "dream.tokens": result.tokens, "dream.llm_fallback": false });
				return { text: result.value, tokens: result.tokens };
			}
			if (result.status === "aborted" || options.signal.aborted) {
				throw new DreamAbortError("dream guidance child aborted");
			}
			span.setAttributes({ "dream.tokens": result.tokens, "dream.llm_fallback": true });
			return { text: "", tokens: result.tokens };
		},
	);
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
						const parentArtifact = state.artifacts.get(cell.nodeId) ?? null;
						const outcome = await proposer.propose(
							parentArtifact,
							state.params,
							attemptRng(state, cell, round),
							round,
						);
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

/**
 * A round-1 rollout performed elsewhere (once per experiment, then copied into
 * every arm's store). The loop builds its iteration-0 record from it and performs
 * no rollout of its own; the tree MUST already exist in `dir`.
 */
export interface DreamInitialRollout {
	treeId: string;
	bestScore: number;
	revealedCount: number;
	/** `ExploreResult.agentGeneratedCount` of the shared rollout; absent reads as 0 (untracked), never as `revealedCount`. */
	agentGeneratedCount?: number;
	/** The shared rollout's proposer tally; absent reads as all zero. */
	proposals?: ProposalTally;
	rounds: number;
	tokens: number;
	handlerCalls: DreamHandlerCalls;
}

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
	/** The hand-written policy iteration 0 rolls out with and dreaming starts from; defaults to `DEFAULT_POLICY`. */
	initialPolicy?: ExplorationPolicy;
	/**
	 * The fixed-exploration control: never dream. Every iteration redeploys
	 * `initialPolicy`; the pool still grows and every rollout is unchanged, so the
	 * control shares the dreaming run's iteration 0 byte for byte.
	 */
	fixedPolicy?: boolean;
	/** A shared round 1 (see `DreamInitialRollout`); when set, iteration 0 performs no rollout. */
	initialRollout?: DreamInitialRollout;
	/**
	 * The semantic-guidance ablation: on every iteration >= 1 the frozen pool is
	 * digested and directional insights are prefixed to the proposer prompt.
	 * Requires `useLlmProposer`. Iteration 0 has no pool, so its prompt is
	 * byte-identical with and without guidance.
	 */
	semanticGuidance?: SemanticGuidanceOption;
	/** When true, each attempt is generated by a child agent; otherwise the local zero-token proposer. */
	useLlmProposer: boolean;
	/** When true, each dreaming step's candidates come from a child agent; otherwise local search. */
	useLlmDreamer: boolean;
	/** Child runtime scope shared by the proposer, dreamer and guidance children. */
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
 * outliving its parent. Iteration 0 rolls out with the initial policy (or adopts
 * a shared `initialRollout`); each later iteration freezes the pool, resolves
 * candidates (LLM dreamer when enabled, else local search), selects a no-worse
 * policy through the existing `runDreaming` seam, and redeploys it — unless
 * `fixedPolicy`, which redeploys the initial policy without dreaming.
 * `useLlmProposer` and `useLlmDreamer` are INDEPENDENT toggles; one async driver
 * serves both, so the local-proposer path stays byte-identical.
 *
 * A configuration error (`semanticGuidance` without `useLlmProposer`) rejects
 * before any span, child call or tree file.
 */
export async function runDreamLoopWithAgent(options: DreamLoopWithAgentOptions): Promise<DreamLoopResult> {
	if (options.semanticGuidance && !options.useLlmProposer) {
		throw new Error("semanticGuidance requires useLlmProposer");
	}
	const trigger = currentTraceContext()?.traceId;
	const taskId: DreamTaskId = options.taskId ?? options.task.id;
	const iterations = Math.max(0, Math.trunc(options.iterations));
	const fixedPolicy = options.fixedPolicy === true;
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
			"dream.fixed_policy": fixedPolicy,
			...(trigger ? { "trigger.trace_id": trigger } : {}),
		}),
	);
	return runDreamRun(options, taskId, iterations, fixedPolicy, span);
}

async function runDreamRun(
	options: DreamLoopWithAgentOptions,
	taskId: DreamTaskId,
	iterations: number,
	fixedPolicy: boolean,
	span: Span,
): Promise<DreamLoopResult> {
	try {
		const value = await runWithTraceContext(span.context, () =>
			dreamLoopBody(options, taskId, iterations, fixedPolicy),
		);
		span.end();
		return value;
	} catch (error) {
		if (isDreamAbortError(error)) span.setAttributes({ "dream.stopped": "aborted" });
		else span.recordError(error);
		span.end();
		throw error;
	}
}

type HandlerRole = keyof DreamHandlerCalls;
type RoleTokens = DreamRoundRecord["tokens"];

function zeroCalls(): DreamHandlerCalls {
	return { proposer: 0, dreamer: 0, guidance: 0 };
}

function zeroTokens(): RoleTokens {
	return { rollout: 0, dreamer: 0, guidance: 0 };
}

async function dreamLoopBody(
	options: DreamLoopWithAgentOptions,
	taskId: DreamTaskId,
	iterations: number,
	fixedPolicy: boolean,
): Promise<DreamLoopResult> {
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const rng = options.rng ?? createSeededRng(options.seed);
	const scoreCfg = { k1: options.k1, k2: options.k2, objective };
	const childTokenBudget = options.childTokenBudget ?? DEFAULT_CHILD_TOKEN_BUDGET;
	const initialPolicy = options.initialPolicy ?? DEFAULT_POLICY;
	const guidanceOption = options.semanticGuidance;
	const { runAgent, task, dir, signal } = options;

	// Per-round accounting. The wrappers count every actual handler invocation of
	// the current round by role (retries included, since `retrying` sits inside
	// them); the proposer tallies every child result it examines; `record`
	// snapshots and resets all three. Nothing here touches the rng, the tree or
	// persistence. The rejection log exists only on the LLM-proposer path, so the
	// local path writes nothing new and reads the clock no more than before.
	let calls = zeroCalls();
	let roleTokens = zeroTokens();
	let tally = zeroProposalTally();
	const counting =
		(role: HandlerRole): RunAgentHandler =>
		(request, callOptions) => {
			calls[role] += 1;
			return runAgent(request, callOptions);
		};
	const proposerHandler = counting("proposer");
	const dreamerHandler = counting("dreamer");
	const guidanceHandler = counting("guidance");
	const rejections = options.useLlmProposer
		? new RejectionLog(rejectionsPath(dir, `${taskId}-s${options.seed}-r${options.clock()}`), options.clock)
		: undefined;

	const makeProposer = (guidance: string, iteration: number): AsyncProposer<unknown> =>
		options.useLlmProposer
			? createLlmProposer(proposerHandler, task, {
					scope: options.scope,
					signal,
					tokenBudget: childTokenBudget,
					tally,
					iteration,
					...(rejections ? { rejections } : {}),
					...(options.proposerPromptContext ? { promptContext: options.proposerPromptContext } : {}),
					...(guidance.length > 0 ? { guidance } : {}),
				})
			: asyncOf(createLocalProposer(task));

	const rollout = (policy: ExplorationPolicy, iteration: number, guidance: string): Promise<ExploreResult> => {
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
			makeProposer(guidance, iteration),
			signal,
		);
	};

	const treeIds: string[] = [];
	const rounds: DreamRoundRecord[] = [];
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
	const record = (
		result: Pick<ExploreResult, "treeId" | "bestScore" | "revealedCount" | "rounds" | "agentGeneratedCount">,
		policy: ExplorationPolicy,
		iteration: number,
		poolSize: number,
		dreaming: DreamRoundRecord["dreaming"],
	): void => {
		treeIds.push(result.treeId);
		tokens += roleTokens.rollout + roleTokens.dreamer + roleTokens.guidance;
		noteBest(result.bestScore);
		rounds.push({
			iteration,
			treeId: result.treeId,
			policyId: policyId(policy),
			roundBest: result.bestScore,
			probes: result.revealedCount,
			agentGeneratedCalls: result.agentGeneratedCount,
			proposals: addProposalTally(zeroProposalTally(), tally),
			decisionRounds: result.rounds,
			poolSize,
			tokens: { ...roleTokens },
			handlerCalls: { ...calls },
			dreaming,
		});
		calls = zeroCalls();
		roleTokens = zeroTokens();
		tally = zeroProposalTally();
	};

	if (options.initialRollout) {
		const shared = options.initialRollout;
		if (!existsSync(treePath(shared.treeId, dir))) {
			throw new DreamStoreError(`shared initial rollout ${shared.treeId} is not in the store ${dir}`);
		}
		if (signal.aborted) throw new DreamAbortError("dream run aborted before rollout");
		calls = { ...shared.handlerCalls };
		roleTokens.rollout = shared.tokens;
		tally = addProposalTally(zeroProposalTally(), shared.proposals ?? zeroProposalTally());
		record({ ...shared, agentGeneratedCount: shared.agentGeneratedCount ?? 0 }, initialPolicy, 0, 0, null);
	} else {
		const first = await rollout(initialPolicy, 0, "");
		roleTokens.rollout = first.tokens;
		record(first, initialPolicy, 0, 0, null);
	}
	options.onProgress?.({ type: "phase", phase: "rollout", iteration: 0, bestNodeScore, treeId: treeIds[0] });

	let current: ExplorationPolicy = initialPolicy;
	for (let iteration = 1; iteration <= iterations; iteration++) {
		if (signal.aborted) throw new DreamAbortError("dream run aborted before iteration");
		let pool: RecordedTree[] | undefined;
		let guidance = "";
		if (guidanceOption) {
			pool = freezePool(dir, taskId);
			const produced = await resolveGuidance(
				guidanceHandler,
				guidanceOption,
				buildGuidanceInput(pool, taskId, iteration),
				{ scope: options.scope, signal, tokenBudget: childTokenBudget },
			);
			guidance = produced.text;
			roleTokens.guidance = produced.tokens;
		}
		let poolSize = pool ? pool.length : iteration;
		let dreaming: DreamRoundRecord["dreaming"] = null;
		if (!fixedPolicy) {
			options.onProgress?.({ type: "phase", phase: "dreaming", iteration, bestNodeScore });
			pool ??= freezePool(dir, taskId);
			poolSize = pool.length;
			let resolved: ExplorationPolicy[] | undefined;
			if (options.useLlmDreamer) {
				const dreamed = await proposePoliciesWithAgent(dreamerHandler, current, options.dreams, {
					scope: options.scope,
					signal,
					tokenBudget: childTokenBudget,
					localFallbackRng: rng.fork(`dream-fallback:${iteration}`),
				});
				resolved = dreamed.candidates;
				roleTokens.dreamer = dreamed.tokens;
			}
			const dream = runDreaming({
				current,
				pool,
				dreams: options.dreams,
				k1: options.k1,
				k2: options.k2,
				rng: rng.fork(`dream:${iteration}`),
				objective,
				...(resolved ? { proposeCandidates: () => resolved as ExplorationPolicy[] } : {}),
			});
			current = dream.chosenPolicy;
			chosenPolicies.push(current);
			dreaming = {
				currentScore: dream.currentScore,
				chosenScore: dream.chosenScore,
				improved: dream.improved,
				candidates: dream.candidatePolicyIds.length,
			};
		}
		if (signal.aborted) throw new DreamAbortError("dream run aborted before redeploy");
		const redeployed = await withSpan(
			"dream.redeploy",
			{
				"dream.policy_id": policyId(current),
				"dream.k1": options.k1,
				"dream.workers": options.workers,
				"dream.iteration": iteration,
				"dream.fixed_policy": fixedPolicy,
			},
			async (redeploySpan) => {
				const result = await rollout(current, iteration, guidance);
				redeploySpan.setAttributes({ "dream.tree_id": result.treeId });
				return result;
			},
		);
		roleTokens.rollout = redeployed.tokens;
		record(redeployed, current, iteration, poolSize, dreaming);
		options.onProgress?.({
			type: "phase",
			phase: "redeploying",
			iteration,
			bestNodeScore,
			treeId: redeployed.treeId,
		});
	}

	const finalPool = freezePool(dir, taskId);
	const selection = selectBestPolicy(initialPolicy, chosenPolicies, finalPool, scoreCfg);
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
		fixedPolicy,
		treeIds,
		rounds,
		initialPolicyId: policyId(initialPolicy),
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

/** The guidance writer must return `{"insights": "<non-empty text>"}`; anything else is invalid output. */
function parseInsights(value: unknown): string {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new TypeError("guidance must be a JSON object");
	}
	const insights = (value as Record<string, unknown>).insights;
	if (typeof insights !== "string" || insights.trim().length === 0) {
		throw new TypeError("guidance insights must be a non-empty string");
	}
	return insights.trim();
}

function buildProposePrompt(
	taskId: DreamTaskId,
	promptContext?: string,
	guidance?: string,
): (input: ProposeChildInput) => string {
	const guidanceBlock = guidance && guidance.length > 0 ? [`${GUIDANCE_PROMPT_PREFIX}\n${guidance}`] : [];
	// The task contract (with the exact size, e.g. the bin count) sits between the
	// candidate and the output contract; the JSON-only instruction is the last line.
	return (input) =>
		[
			`${PROPOSER_PROMPT_HEADER} (${taskId})`,
			...guidanceBlock,
			"Improve the candidate below into a better one for this scored task. Everything you need is in this message; do not search, browse, or call tools.",
			`Current candidate (JSON), or null to start fresh:\n${input.parentJson === null ? "null" : JSON.stringify(input.parentJson)}`,
			`Generation hints: stepScale=${input.params.stepScale}, refineDepth=${input.params.refineDepth}, branchWidth=${input.params.branchWidth}; round ${input.round}.`,
			...(promptContext ? [promptContext] : []),
			PROPOSER_OUTPUT_CONTRACT,
			PROPOSER_JSON_ONLY,
		].join("\n\n");
}

function buildDreamPrompt(input: DreamChildInput): string {
	return [
		DREAMER_PROMPT_HEADER,
		`Propose ${input.m} revised exploration policies that should score better than the current one on replay. A policy is DATA: a flat JSON object with exactly these fields.`,
		policySchemaText(),
		`Current policy:\n${JSON.stringify(input.current)}`,
		`The array must hold ${input.m} policy objects. Any object with an unknown field, a wrong type, or an out-of-range value is discarded. ${JSON_ARRAY_ONLY}`,
	].join("\n\n");
}

function buildGuidancePrompt(input: GuidanceInput): string {
	return [
		`${GUIDANCE_PROMPT_HEADER} (${input.taskId})`,
		`Below is a digest of ${input.poolSize} prior discovery trajectories for this scored task (iteration ${input.iteration}): per tree the exploration policy id, the best score, the evaluated attempts, the decision rounds, the failure classes seen, and the top-scoring recorded candidates. Write 3-8 short directional insights a proposer could use to improve its next candidates: which kinds of edits raised the score, what failed and why, and what remains unexplored. Everything you need is in this message; do not search, browse, or call tools.`,
		`Trajectory digest (JSON):\n${JSON.stringify(input)}`,
		`Return {"insights": "<the insights as plain text>"}. ${JSON_OBJECT_ONLY}`,
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
