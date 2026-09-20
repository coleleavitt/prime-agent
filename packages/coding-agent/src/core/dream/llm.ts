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
 *     lower than current's on every measured tree, with current winning ties —
 *     so a worse, malformed or exploration-collapsing LLM policy can never
 *     regress the deployed one ON REPLAY; and its first online rollout is a
 *     probation (`loop.ts` `judgeProbation`) that reverts and revokes it when
 *     the rollout misses the incumbent's lowest recorded best, since a replay
 *     win says nothing about branches the recording does not hold;
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
 * `initialRollout` lets an experiment share one round-1 rollout (with its
 * priming rollouts) across arms, and the per-round `rounds` records count
 * handler calls and tokens per role so an experiment can report cost next to
 * (never mixed into) the compute axis.
 *
 * Audit trail, identical to the sync loop's: the run id is `dreamRunId` (with
 * the experiment's `runLabel`), every dreaming step is written to
 * `<dir>/dreams/<runId>.jsonl` with one verdict per candidate and the post-hoc
 * final selection as iteration -1, the proposer's AND the dreamer's rejected
 * child results go to `<dir>/rejections/<runId>.jsonl` (the dreamer's with
 * `role: "dreamer"`), and `dream.llm_dream`, `dream.dream`, `dream.replay` and
 * `dream.candidate` all carry `dream.iteration`.
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
import { DreamsLog, type DreamsLogContext, dreamsPath } from "./dreams.js";
import {
	type CandidateInput,
	type DreamResult,
	dreamerKindOf,
	proposePolicies,
	runDreaming,
	selectBestPolicy,
	termsOnPool,
} from "./improve.js";
import {
	type DreamHandlerCalls,
	type DreamLoopResult,
	type DreamRoundRecord,
	dreamRunId,
	freezePool,
	judgeProbation,
	mergedRoundCurve,
	primingTreeId,
} from "./loop.js";
import { DEFAULT_OBJECTIVE, type ObjectiveScale, poolScoreScale, type ReplayObjectiveConfig } from "./objective.js";
import {
	DEFAULT_POLICY,
	type ExplorationPolicy,
	POLICY_BOUNDS,
	parseExplorationPolicy,
	policyId,
	RECOVERY_POLICIES,
	REPLAY_DEAD_FIELDS,
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
import { simulatePolicy } from "./replay.js";
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
	type ScoreImprovement,
	selectRoundCells,
} from "./rollout.js";
import { DreamStoreError, type RecordedTree, treePath } from "./store.js";
import type { DreamTaskId, ProposeParams, ScoredTask } from "./task.js";
import type {
	CandidateVerdict,
	DreamClock,
	DreamerKind,
	DreamMode,
	DreamProbationRecord,
	NodeRecord,
} from "./types.js";

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

/** One recorded tree's replay of the CURRENT policy, as the dreamer prompt shows it (scalars only). */
export interface DreamerPoolTreeDigest {
	treeId: string;
	/** Revealed non-root nodes the current policy's replay spent. */
	N: number;
	rounds: number;
	outOfSupportCells: number;
	bestScore: number;
	value: number;
	quality: number;
	anytime: number;
	cost: number;
	roundsSaved: number;
}

/** One earlier candidate's verdict, as the dreamer prompt shows it; a reverted adoption appears again as `revoked`. */
export interface DreamHistoryEntry {
	iteration: number;
	policyId: string;
	origin: CandidateVerdict["origin"];
	changed: string[];
	value: number;
	quality: number;
	reason: CandidateVerdict["reason"];
}

/**
 * Everything the dreamer child is told. Built by `buildDreamerInput`: the
 * current policy, the count contract, the objective and budget, the current
 * policy's per-tree replay on the frozen pool (scalars only) and the verdicts of
 * every earlier step. Without a pool the replay block is empty and the prompt
 * still states the rules.
 */
export interface DreamChildInput {
	current: ExplorationPolicy;
	m: number;
	iteration: number;
	objective: ReplayObjectiveConfig;
	budget: { workers: number; k1: number; k2: number };
	scale: ObjectiveScale;
	pool: DreamerPoolTreeDigest[];
	history: DreamHistoryEntry[];
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
	/**
	 * Explicit labelled fork for the local mutator: the whole set when the child
	 * fails or yields nothing usable, or the top-up that fills the `m` budget when
	 * the child returned fewer distinct new policies than asked.
	 */
	localFallbackRng: SeededRng;
	/** The loop iteration, stamped on the span, the prompt and any rejection line; defaults to 0. */
	iteration?: number;
	/** The frozen pool the candidates will be scored on; digested (scalars only) into the prompt. */
	pool?: readonly RecordedTree[];
	/** The scoring configuration the prompt states; defaults to `DEFAULT_OBJECTIVE`. */
	objective?: ReplayObjectiveConfig;
	/** Max parallelism W stated in the prompt (also the `batchSize` cap); defaults to the pool's max `header.w`, else 1. */
	workers?: number;
	k1?: number;
	k2?: number;
	/** Verdicts of earlier dreaming steps, shown to the child as history. */
	history?: readonly DreamHistoryEntry[];
	/** Where every rejected child result is written, as `role: "dreamer"` lines. */
	rejections?: RejectionLog;
}

/** The outcome of one LLM dreaming call: the candidate set with provenance, plus what became of the child's output. */
export interface DreamedCandidates {
	/** In order: the child's kept entries (identical/duplicate ones included, so they are audited), then any local top-up. */
	candidates: CandidateInput[];
	tokens: number;
	dreamer: DreamerKind;
	/** Entries in the child's JSON array (0 when no array was parsed). */
	returned: number;
	/** Entries the strict parser dropped, with the reason. */
	dropped: { index: number; reason: string }[];
	/** Distinct policies, different from current, the child contributed (at most `m`). */
	kept: number;
	/** Child entries beyond the `m` distinct ones, not passed on. */
	truncated: number;
	/** Local candidates appended to fill the budget (`m - kept`). */
	local: number;
	/** True when the child produced nothing usable and the whole set is local. */
	llmFallback: boolean;
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

/** Max `header.w` over a pool; 1 for an empty pool. */
function poolWorkers(pool: readonly RecordedTree[]): number {
	let workers = 1;
	for (const tree of pool) if (tree.header.w > workers) workers = Math.trunc(tree.header.w);
	return workers;
}

/**
 * Digest the frozen pool for the dreamer: the CURRENT policy's replay on every
 * tree (tree-id order) under the same simulator and objective the selection
 * uses, with the incumbent's spend charged raw as the selection charges it
 * (`termsOnPool`), so the child sees exactly the numbers its candidates must
 * beat. Scalars only; no artifact and never a hidden test. Rng-free and
 * zero-token.
 */
export function buildDreamerInput(
	current: ExplorationPolicy,
	m: number,
	options: Pick<LlmDreamerOptions, "iteration" | "pool" | "objective" | "workers" | "k1" | "k2" | "history">,
): DreamChildInput {
	const pool = [...(options.pool ?? [])].sort((a, b) => a.header.treeId.localeCompare(b.header.treeId));
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const workers = Math.max(1, Math.trunc(options.workers ?? poolWorkers(pool)));
	const k1 = Math.max(1, Math.trunc(options.k1 ?? 1));
	const k2 = Math.max(1, Math.trunc(options.k2 ?? k1));
	const scale = poolScoreScale(pool);
	const replays = pool.map((tree) => simulatePolicy(tree, current, { k2 }));
	const poolTerms = termsOnPool(replays, pool, { k1, k2, objective }, scale, "raw");
	const digest = pool.map((tree, index): DreamerPoolTreeDigest => {
		const replay = replays[index]!;
		const terms = poolTerms[index]!;
		return {
			treeId: tree.header.treeId,
			N: replay.N,
			rounds: replay.rounds,
			outOfSupportCells: replay.outOfSupportCells,
			bestScore: replay.bestScore,
			value: terms.value,
			quality: terms.quality,
			anytime: terms.anytime,
			cost: terms.cost,
			roundsSaved: terms.roundsSaved,
		};
	});
	return {
		current,
		m: Math.max(0, Math.trunc(m)),
		iteration: Math.max(0, Math.trunc(options.iteration ?? 0)),
		objective,
		budget: { workers, k1, k2 },
		scale,
		pool: digest,
		history: [...(options.history ?? [])],
	};
}

/** A `DreamHistoryEntry` per verdict of one finished dreaming step, for the next step's prompt. */
export function historyOf(iteration: number, verdicts: readonly CandidateVerdict[]): DreamHistoryEntry[] {
	return verdicts.map((verdict) => ({
		iteration,
		policyId: verdict.policyId,
		origin: verdict.origin,
		changed: [...verdict.changed],
		value: verdict.value,
		quality: verdict.quality,
		reason: verdict.reason,
	}));
}

/**
 * The history entry a reverted adoption adds after its `winner` entry: the same
 * policy, reason `revoked`, so the next prompt shows the probation outcome and
 * the dreamer does not propose it again.
 */
export function revokedHistoryEntry(
	iteration: number,
	winner: CandidateVerdict,
	probation: DreamProbationRecord,
): DreamHistoryEntry {
	return {
		iteration,
		policyId: probation.policyId,
		origin: winner.origin,
		changed: [...winner.changed],
		value: winner.value,
		quality: winner.quality,
		reason: "revoked",
	};
}

/**
 * The LLM dreamer. It asks a child agent for `m` revised policies with the
 * scoring rule, the pool's replay of the current policy and the earlier steps'
 * verdicts in the prompt, keeps only the entries the STRICT
 * `parseExplorationPolicy` accepts (recording why each other one was dropped),
 * takes the first `m` DISTINCT policies that differ from the current one (an
 * identical or duplicate entry is passed on so the selection records it as such,
 * but does not count toward `m`), and fills any shortfall from the local
 * `proposePolicies` on the labelled fallback fork, so the step's `dreamer` is
 * `llm`, `mixed` or `local`. A retryable rejection (`RETRYABLE_REJECTIONS`) gets
 * one more child call; every rejected result is written to `options.rejections`
 * as a `role: "dreamer"` line and put on the `dream.llm_dream` span; an abort
 * throws `DreamAbortError`. The candidates are then scored and selected by
 * `runDreaming` under the same no-regression rule, so a bad policy can never be
 * deployed.
 */
export async function proposePoliciesWithAgent(
	runAgent: RunAgentHandler,
	current: ExplorationPolicy,
	m: number,
	options: LlmDreamerOptions,
): Promise<DreamedCandidates> {
	const requested = Math.max(0, Math.trunc(m));
	const iteration = Math.max(0, Math.trunc(options.iteration ?? 0));
	const input = buildDreamerInput(current, requested, options);
	const prompt = buildDreamPrompt(input);
	let parsed: ParsedCandidates | undefined;
	const validate = (value: unknown): ParsedCandidates => {
		parsed = parseCandidateArray(value);
		if (parsed.kept.length === 0) {
			throw new TypeError(
				parsed.returned === 0
					? "the array holds no policy object"
					: `every entry was dropped: ${parsed.dropped.map((entry) => `[${entry.index}] ${entry.reason}`).join("; ")}`,
			);
		}
		return parsed;
	};
	return withSpan(
		"dream.llm_dream",
		{ "dream.candidates_requested": requested, "dream.iteration": iteration },
		async (span): Promise<DreamedCandidates> => {
			let tokens = 0;
			let attempts = 0;
			let last: ChildOutcome<ParsedCandidates> | undefined;
			const reject = (outcome: ChildOutcome<ParsedCandidates> & { status: "rejected" }, fellBack: boolean): void => {
				options.rejections?.append({
					role: "dreamer",
					iteration,
					round: 0,
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
			if (requested > 0) {
				for (;;) {
					attempts += 1;
					parsed = undefined;
					last = await runStructuredChild(runAgent, prompt, options, "array", validate);
					tokens += last.tokens;
					if (last.status === "accepted") break;
					const retry =
						RETRYABLE_REJECTIONS.has(last.reason) && attempts <= PROPOSER_RETRIES && !options.signal.aborted;
					if (!retry) break;
					reject(last, false);
				}
			}
			span.setAttributes({
				"dream.tokens": tokens,
				"dream.llm_attempts": attempts,
				"dream.candidates_returned": parsed?.returned ?? 0,
				"dream.candidates_dropped": parsed?.dropped.length ?? 0,
			});
			if (last !== undefined && last.status === "rejected") {
				span.setAttributes({
					"dream.llm_status": last.childStatus,
					"dream.llm_reject_reason": last.reason,
					"dream.llm_reject_excerpt": last.excerpt,
				});
				if (last.reason === "aborted" || options.signal.aborted) {
					reject(last, false);
					span.setAttributes({ "dream.llm_fallback": false });
					throw new DreamAbortError("dream dreamer child aborted");
				}
				reject(last, true);
			} else if (last !== undefined) {
				span.setAttributes({ "dream.llm_status": "completed" });
			}
			// Take the child's entries in order: the first `requested` DISTINCT new
			// policies count toward the budget; an identical or duplicate entry rides
			// along (the selection labels it) and anything past the budget is truncated.
			const fromChild = last?.status === "accepted" ? last.value.kept : [];
			const currentId = policyId(current);
			const seen = new Set<string>();
			const taken: CandidateInput[] = [];
			let truncated = 0;
			for (const policy of fromChild) {
				if (seen.size >= requested) {
					truncated += 1;
					continue;
				}
				const id = policyId(policy);
				if (id !== currentId) seen.add(id);
				taken.push({ policy, origin: "llm" });
			}
			const kept = seen.size;
			const shortfall = requested - kept;
			const local = proposePolicies(current, shortfall, options.localFallbackRng).map(
				(policy): CandidateInput => ({ policy, origin: "local" }),
			);
			const candidates = [...taken, ...local];
			const llmFallback = kept === 0 && requested > 0;
			const dreamer = dreamerKindOf(candidates);
			span.setAttributes({
				"dream.candidates_kept": kept,
				"dream.candidates_truncated": truncated,
				"dream.candidates_local": local.length,
				"dream.candidates": candidates.length,
				"dream.dreamer": dreamer,
				"dream.llm_fallback": llmFallback,
			});
			return {
				candidates,
				tokens,
				dreamer,
				returned: parsed?.returned ?? 0,
				dropped: parsed?.dropped ?? [],
				kept,
				truncated,
				local: local.length,
				llmFallback,
			};
		},
	);
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
	/** The round's best valid score: max over the initial rollout and any priming rollouts. */
	bestScore: number;
	/** The initial rollout's revealed non-root nodes; priming probes are `primingProbes`. */
	revealedCount: number;
	/** `ExploreResult.agentGeneratedCount` summed over the shared rollouts; absent reads as 0 (untracked), never as `revealedCount`. */
	agentGeneratedCount?: number;
	/** The shared rollouts' proposer tally; absent reads as all zero. */
	proposals?: ProposalTally;
	/** The initial rollout's online decision rounds. */
	rounds: number;
	tokens: number;
	handlerCalls: DreamHandlerCalls;
	/** The round-1 curve (`mergedRoundCurve` over the initial then each priming rollout); absent when the runner predates it. */
	probesToBest?: number;
	improvements?: ScoreImprovement[];
	/** Priming rollouts shared with round 1, when the experiment primed the pool. */
	primingTreeIds?: string[];
	primingProbes?: number;
}

/**
 * Fold the initial rollout and its priming rollouts into one round-1 record's
 * facts, exactly as `runDreamLoop` charges them: probes and tokens summed,
 * `bestScore` the max, `rounds` the initial rollout's, the curve merged over the
 * concatenated probes. Shared by the in-loop priming path and the experiment's
 * shared round 1, so both arms of an experiment record the same round 1.
 */
export function mergePrimedRollouts(
	initial: ExploreResult,
	primed: readonly ExploreResult[],
): Omit<DreamInitialRollout, "handlerCalls" | "proposals"> {
	const curve = mergedRoundCurve([initial, ...primed]);
	let bestScore = initial.bestScore;
	let primingProbes = 0;
	let tokens = initial.tokens;
	let agentGeneratedCount = initial.agentGeneratedCount;
	for (const prime of primed) {
		if (prime.bestScore > bestScore) bestScore = prime.bestScore;
		primingProbes += prime.revealedCount;
		tokens += prime.tokens;
		agentGeneratedCount += prime.agentGeneratedCount;
	}
	return {
		treeId: initial.treeId,
		bestScore,
		revealedCount: initial.revealedCount,
		agentGeneratedCount,
		rounds: initial.rounds,
		tokens,
		probesToBest: curve.probesToBest,
		improvements: curve.improvements,
		...(primed.length > 0 ? { primingTreeIds: primed.map((prime) => prime.treeId), primingProbes } : {}),
	};
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
	/**
	 * A clock-free label folded into the run id and every per-run log key
	 * (`rejections/`, `dreams/`); the experiment runner passes `<experimentId>/<arm>`
	 * so arms under one frozen clock no longer share a run id. See `dreamRunId`.
	 */
	runLabel?: string;
	/** Experiment provenance stamped on every dreams-log line. */
	dreamsLogContext?: DreamsLogContext;
	/**
	 * Policies rolled out once each at iteration 0 on forks `prime:<i>`, joining
	 * the pool with their probes and tokens charged to round 1 (see `runDreamLoop`).
	 * Ignored when `initialRollout` is set: a shared round 1 carries its own priming.
	 */
	primingPolicies?: readonly ExplorationPolicy[];
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
	const priming = options.initialRollout ? [] : (options.primingPolicies ?? []);
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
			"dream.priming_policies": priming.length + (options.initialRollout?.primingTreeIds?.length ?? 0),
			...(options.scope.model ? { "dream.child_model": options.scope.model } : {}),
			...(options.scope.thinkingLevel ? { "dream.child_thinking": options.scope.thinkingLevel } : {}),
			...(options.scope.maxOutputTokens === undefined
				? {}
				: { "dream.child_max_output_tokens": options.scope.maxOutputTokens }),
			...(trigger ? { "trigger.trace_id": trigger } : {}),
		}),
	);
	return runDreamRun(options, taskId, iterations, fixedPolicy, priming, span);
}

async function runDreamRun(
	options: DreamLoopWithAgentOptions,
	taskId: DreamTaskId,
	iterations: number,
	fixedPolicy: boolean,
	priming: readonly ExplorationPolicy[],
	span: Span,
): Promise<DreamLoopResult> {
	try {
		const value = await runWithTraceContext(span.context, () =>
			dreamLoopBody(options, taskId, iterations, fixedPolicy, priming, span),
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
	priming: readonly ExplorationPolicy[],
	runSpan: Span,
): Promise<DreamLoopResult> {
	const objective = options.objective ?? DEFAULT_OBJECTIVE;
	const rng = options.rng ?? createSeededRng(options.seed);
	const scoreCfg = { k1: options.k1, k2: options.k2, objective };
	const childTokenBudget = options.childTokenBudget ?? DEFAULT_CHILD_TOKEN_BUDGET;
	const initialPolicy = options.initialPolicy ?? DEFAULT_POLICY;
	const guidanceOption = options.semanticGuidance;
	const k1 = Math.max(1, Math.trunc(options.k1));
	const { runAgent, task, dir, signal } = options;
	const runId = dreamRunId(taskId, options.seed, options.clock(), options.runLabel);
	runSpan.setAttributes({ "dream.run_id": runId });

	// Per-round accounting. The wrappers count every actual handler invocation of
	// the current round by role (retries included, since `retrying` sits inside
	// them); the proposer tallies every child result it examines; `record`
	// snapshots and resets all three. Nothing here touches the rng, the tree or
	// persistence. The rejection log exists only on the LLM path (proposer or
	// dreamer), so the local path writes nothing new there; the dreams log is
	// written on every path, one step at a time, and touches neither rng nor tree.
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
	const rejections =
		options.useLlmProposer || options.useLlmDreamer
			? new RejectionLog(rejectionsPath(dir, runId), options.clock)
			: undefined;
	const dreamsLog = new DreamsLog(dreamsPath(dir, runId), options.clock, options.dreamsLogContext);

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

	const rollout = (
		policy: ExplorationPolicy,
		iteration: number,
		guidance: string,
		fork = `iter:${iteration}`,
		treeId?: string,
	): Promise<ExploreResult> => {
		if (signal.aborted) throw new DreamAbortError("dream run aborted before rollout");
		return runOnlineExplorationWithAgent(
			{
				task,
				taskId,
				...(options.n !== undefined ? { n: options.n } : {}),
				seed: options.seed,
				rng: rng.fork(fork),
				clock: options.clock,
				workers: options.workers,
				k1: options.k1,
				dir,
				policy,
				iteration,
				...(treeId !== undefined ? { treeId } : {}),
			},
			makeProposer(guidance, iteration),
			signal,
		);
	};

	const treeIds: string[] = [];
	const rounds: DreamRoundRecord[] = [];
	const chosenPolicies: ExplorationPolicy[] = [];
	const history: DreamHistoryEntry[] = [];
	let bestNodeScore = 0;
	let seenBest = false;
	let tokens = 0;
	let stoppedEarly = 0;
	const noteBest = (score: number): void => {
		if (!seenBest || score > bestNodeScore) {
			bestNodeScore = score;
			seenBest = true;
		}
	};
	const record = (
		result: Omit<DreamInitialRollout, "handlerCalls" | "proposals" | "tokens">,
		policy: ExplorationPolicy,
		iteration: number,
		poolSize: number,
		dreaming: DreamRoundRecord["dreaming"],
	): void => {
		treeIds.push(result.treeId);
		tokens += roleTokens.rollout + roleTokens.dreamer + roleTokens.guidance;
		noteBest(result.bestScore);
		if (result.rounds < k1) stoppedEarly += 1;
		rounds.push({
			iteration,
			treeId: result.treeId,
			policyId: policyId(policy),
			roundBest: result.bestScore,
			probes: result.revealedCount + (result.primingProbes ?? 0),
			agentGeneratedCalls: result.agentGeneratedCount ?? 0,
			proposals: addProposalTally(zeroProposalTally(), tally),
			decisionRounds: result.rounds,
			poolSize,
			tokens: { ...roleTokens },
			handlerCalls: { ...calls },
			dreaming,
			...(result.probesToBest === undefined ? {} : { probesToRoundBest: result.probesToBest }),
			...(result.improvements === undefined ? {} : { improvements: [...result.improvements] }),
			...(result.primingTreeIds === undefined
				? {}
				: { primingTreeIds: [...result.primingTreeIds], primingProbes: result.primingProbes ?? 0 }),
		});
		calls = zeroCalls();
		roleTokens = zeroTokens();
		tally = zeroProposalTally();
	};

	let primingCount = 0;
	if (options.initialRollout) {
		const shared = options.initialRollout;
		if (!existsSync(treePath(shared.treeId, dir))) {
			throw new DreamStoreError(`shared initial rollout ${shared.treeId} is not in the store ${dir}`);
		}
		for (const primeId of shared.primingTreeIds ?? []) {
			if (!existsSync(treePath(primeId, dir))) {
				throw new DreamStoreError(`shared priming rollout ${primeId} is not in the store ${dir}`);
			}
		}
		if (signal.aborted) throw new DreamAbortError("dream run aborted before rollout");
		primingCount = shared.primingTreeIds?.length ?? 0;
		calls = { ...shared.handlerCalls };
		roleTokens.rollout = shared.tokens;
		tally = addProposalTally(zeroProposalTally(), shared.proposals ?? zeroProposalTally());
		record(shared, initialPolicy, 0, 0, null);
	} else {
		const first = await rollout(initialPolicy, 0, "");
		const primed: ExploreResult[] = [];
		for (const [index, policy] of priming.entries()) {
			primed.push(
				await rollout(policy, 0, "", `prime:${index}`, primingTreeId(taskId, options.seed, index, options.clock())),
			);
		}
		primingCount = primed.length;
		const merged = mergePrimedRollouts(first, primed);
		roleTokens.rollout = merged.tokens;
		record(merged, initialPolicy, 0, 0, null);
	}
	options.onProgress?.({ type: "phase", phase: "rollout", iteration: 0, bestNodeScore, treeId: treeIds[0] });

	let current: ExplorationPolicy = initialPolicy;
	const revoked = new Set<string>();
	let probationReverts = 0;
	for (let iteration = 1; iteration <= iterations; iteration++) {
		if (signal.aborted) throw new DreamAbortError("dream run aborted before iteration");
		const incumbent = current;
		let adopted: DreamResult | undefined;
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
		let poolSize = pool ? pool.length : iteration + primingCount;
		let dreaming: DreamRoundRecord["dreaming"] = null;
		if (!fixedPolicy) {
			options.onProgress?.({ type: "phase", phase: "dreaming", iteration, bestNodeScore });
			pool ??= freezePool(dir, taskId);
			poolSize = pool.length;
			let resolved: CandidateInput[] | undefined;
			if (options.useLlmDreamer) {
				const dreamed = await proposePoliciesWithAgent(dreamerHandler, current, options.dreams, {
					scope: options.scope,
					signal,
					tokenBudget: childTokenBudget,
					localFallbackRng: rng.fork(`dream-fallback:${iteration}`),
					iteration,
					pool,
					objective,
					workers: options.workers,
					k1: options.k1,
					k2: options.k2,
					history,
					...(rejections ? { rejections } : {}),
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
				iteration,
				revoked,
				...(resolved ? { proposeCandidates: () => resolved as CandidateInput[] } : {}),
			});
			dreamsLog.recordStep({
				iteration,
				poolSize,
				selection: {
					candidates: dream.candidates,
					currentScore: dream.currentScore,
					chosenPolicy: dream.chosenPolicy,
					improved: dream.improved,
					dreamer: dream.dreamer,
					measuredTrees: dream.measuredTrees,
				},
				leverScan: dream.leverScan,
			});
			history.push(...historyOf(iteration, dream.candidates));
			current = dream.chosenPolicy;
			chosenPolicies.push(current);
			if (dream.improved) adopted = dream;
			dreaming = {
				currentScore: dream.currentScore,
				chosenScore: dream.chosenScore,
				improved: dream.improved,
				candidates: dream.candidatePolicyIds.length,
				candidateVerdicts: dream.candidates,
				dreamer: dream.dreamer,
				leverScan: dream.leverScan,
				measuredTrees: dream.measuredTrees,
			};
		}
		if (signal.aborted) throw new DreamAbortError("dream run aborted before redeploy");
		const deployed = current;
		const redeployed = await withSpan(
			"dream.redeploy",
			{
				"dream.policy_id": policyId(deployed),
				"dream.k1": options.k1,
				"dream.workers": options.workers,
				"dream.iteration": iteration,
				"dream.fixed_policy": fixedPolicy,
				"dream.probation": adopted !== undefined,
			},
			async (redeploySpan) => {
				const result = await rollout(deployed, iteration, guidance);
				redeploySpan.setAttributes({ "dream.tree_id": result.treeId });
				const probation = adopted ? judgeProbation(adopted, incumbent, result) : undefined;
				if (probation) {
					redeploySpan.setAttributes({
						"dream.probation_floor": probation.floor,
						"dream.reverted": probation.reverted,
					});
				}
				return { result, probation };
			},
		);
		if (redeployed.probation && dreaming && adopted) {
			dreaming.probation = redeployed.probation;
			dreamsLog.recordProbation(iteration, redeployed.probation);
			if (redeployed.probation.reverted) {
				revoked.add(redeployed.probation.policyId);
				current = incumbent;
				probationReverts += 1;
				const winner = adopted.candidates.find((verdict) => verdict.reason === "winner");
				if (winner) history.push(revokedHistoryEntry(iteration, winner, redeployed.probation));
			}
		}
		roleTokens.rollout = redeployed.result.tokens;
		record(redeployed.result, deployed, iteration, poolSize, dreaming);
		options.onProgress?.({
			type: "phase",
			phase: "redeploying",
			iteration,
			bestNodeScore,
			treeId: redeployed.result.treeId,
		});
	}

	const finalPool = freezePool(dir, taskId);
	const selection = selectBestPolicy(initialPolicy, chosenPolicies, finalPool, scoreCfg, revoked);
	dreamsLog.recordStep({ iteration: -1, poolSize: finalPool.length, selection, leverScan: null });
	options.onProgress?.({
		type: "completed",
		iteration: iterations,
		bestNodeScore,
		finalPolicyScore: selection.chosenScore,
		improved: selection.improved,
	});
	return {
		runId,
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
		stoppedEarly,
		finalSelection: selection.candidates,
		probationReverts,
	};
}

/** The strict per-entry parse of a dreamer array: what survived and why each other entry was dropped. */
export interface ParsedCandidates {
	kept: ExplorationPolicy[];
	dropped: { index: number; reason: string }[];
	/** Entries in the array (0 for a non-array). */
	returned: number;
}

/**
 * Map the returned JSON array through the STRICT policy parser per entry,
 * keeping every accepted policy and recording the parser's reason for every
 * dropped one (unknown or missing field, wrong type, out-of-range value). Never
 * throws: a non-array is zero entries.
 */
export function parseCandidateArray(value: unknown): ParsedCandidates {
	if (!Array.isArray(value)) return { kept: [], dropped: [], returned: 0 };
	const kept: ExplorationPolicy[] = [];
	const dropped: { index: number; reason: string }[] = [];
	value.forEach((entry, index) => {
		try {
			kept.push(parseExplorationPolicy(entry));
		} catch (error) {
			dropped.push({ index, reason: errorText(error) });
		}
	});
	return { kept, dropped, returned: value.length };
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

function fmt6(value: number): string {
	return Number.isInteger(value) ? String(value) : value.toFixed(6);
}

/**
 * The per-field semantics of a policy as the replay reads them, copied from
 * `interpreter.ts` so the dreamer knows which fields can change a replay score,
 * which are read only under one rule, and which replay never reads at all.
 */
function policySemanticsText(workers: number): string {
	return [
		"Field semantics (what replay reads):",
		"- selectionRule ranks the eligible cells (the root plus every revealed leaf) each round: best-first by score descending; explore-root puts the root first, then the rest by score; round-robin by node id; weighted by score plus explorationBias for every cell scoring at least promisingThreshold times the current best.",
		`- batchSize: cells probed per round, capped at W = ${workers} at runtime, so a value above ${workers} changes nothing. A batch never holds a node together with its parent.`,
		"- stopRule: patience stops after beta consecutive rounds without improving the best score; fixed-rounds stops after beta rounds; threshold stops once the best score reaches targetScore; never runs to the round cap.",
		"- beta is read only under patience and fixed-rounds; targetScore only under threshold; promisingThreshold and explorationBias only under weighted. Changing a field the current rules do not read changes nothing.",
		`- ${REPLAY_DEAD_FIELDS.join(", ")} are never read by replay (they shape only how new candidates are generated online): a policy that differs from the current one only in them replays identically and cannot win. Keep them at the current values.`,
	].join("\n");
}

function objectiveText(input: DreamChildInput): string {
	const { beta1, beta2, beta3 } = input.objective;
	const { workers, k1, k2 } = input.budget;
	return [
		`How a candidate is judged. Each candidate is replayed on ${input.pool.length} recorded discovery tree${input.pool.length === 1 ? "" : "s"} (W = ${workers} cells per round, online round cap k1 = ${k1}, replay round cap k2 = ${k2}) and its mean V is compared with the current policy's mean V on the same trees. `,
		`V = (1 - beta3) * q + beta3 * anytime - beta1 * S / (W * k1) + beta2 * (1 - rounds / k1), with q the best revealed score normalized to the pool's score range [${fmt6(input.scale.scoreMin)}, ${fmt6(input.scale.scoreMax)}], anytime the mean normalized best-so-far over the probe budget W * k1 (rewards reaching the best early), S the charged selections (revealed nodes plus out-of-support selections) and rounds the replay decision rounds; beta1 = ${beta1}, beta2 = ${beta2}, beta3 = ${beta3}. `,
		"Selection rule: a candidate whose q is below the current policy's on ANY of these trees is excluded; among the rest the highest mean V wins, and the current policy wins every tie, so only a STRICTLY higher mean V on these recorded trees is accepted. ",
		"Evidence-backed spend: a candidate's stop-early credit (fewer charged probes, fewer rounds) is charged at the latest probe and round at which the same candidate was still improving on the OTHER recorded trees, so on a single tree there is none (a candidate is charged the full budget W * k1 and k1 rounds), the current policy is charged exactly what it spent, and no-worse quality must hold on every tree. ",
		"Probation: an adopted policy's first online rollout must reach at least the current policy's lowest recorded best on these trees, or the adoption is reverted and the policy is revoked for the rest of the run; a replay win on recorded trees is not an online result. ",
		"Replay mechanics: a candidate re-walks each recorded tree, selecting cells by its own rules and revealing the recorded child of each selected cell; nothing new is ever generated. A selected cell whose recorded children are all revealed is out of support: it reveals nothing but is charged as a probe.",
	].join("");
}

function poolText(input: DreamChildInput): string {
	if (input.pool.length === 0) return "Current policy on the pool: no recorded trees yet.";
	const rows = input.pool.map(
		(tree) =>
			`- ${tree.treeId}: N ${tree.N}, rounds ${tree.rounds}, out-of-support ${tree.outOfSupportCells}, best ${fmt6(tree.bestScore)}, V ${fmt6(tree.value)} (q ${fmt6(tree.quality)}, anytime ${fmt6(tree.anytime)}, cost ${fmt6(tree.cost)}, rounds saved ${fmt6(tree.roundsSaved)})`,
	);
	const meanV = input.pool.reduce((sum, tree) => sum + tree.value, 0) / input.pool.length;
	return [
		`Current policy on the pool (its replay per tree; mean V ${fmt6(meanV)} is the value to beat):`,
		...rows,
	].join("\n");
}

function historyText(input: DreamChildInput): string[] {
	if (input.history.length === 0) return [];
	const rows = input.history.map(
		(entry) =>
			`- iteration ${entry.iteration}: ${entry.policyId} (${entry.origin}; changed ${entry.changed.length > 0 ? entry.changed.join(", ") : "nothing"}) -> V ${fmt6(entry.value)}, q ${fmt6(entry.quality)}: ${entry.reason}`,
	);
	return [
		["Earlier candidates and their verdicts (do not repeat a losing or revoked one unchanged):", ...rows].join("\n"),
	];
}

/**
 * The dreamer prompt. Its first line is `DREAMER_PROMPT_HEADER`; it then states
 * the count contract, the policy schema, how V is computed and how the winner is
 * selected (one paragraph), the replay mechanics (two sentences), the per-field
 * semantics including the replay-dead fields and the `batchSize` cap at W, the
 * current policy with its per-tree replay on the pool, the earlier steps'
 * verdicts, and ends with the JSON-only instruction.
 */
export function buildDreamPrompt(input: DreamChildInput): string {
	return [
		DREAMER_PROMPT_HEADER,
		`Propose up to ${input.m} revised exploration policies that should score better than the current one on replay (dreaming step ${input.iteration}). A policy is DATA: a flat JSON object with exactly these fields.`,
		policySchemaText(),
		objectiveText(input),
		policySemanticsText(input.budget.workers),
		`Current policy:\n${JSON.stringify(input.current)}`,
		poolText(input),
		...historyText(input),
		`Return at most ${input.m} policy objects that are pairwise distinct and each differ from the current policy in at least one field replay reads; a duplicate or a copy of the current policy is discarded, as is any object with an unknown or missing field, a wrong type, or an out-of-range value. ${JSON_ARRAY_ONLY}`,
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
