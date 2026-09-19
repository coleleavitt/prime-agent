/**
 * The generation-attempt abstraction the rollout driver runs against.
 *
 * The default `createLocalProposer` is pure, synchronous and zero-token: it
 * wraps `task.propose`, so an online rollout over a scored task spends no model
 * tokens and touches no network.
 *
 * An LLM proposer is an ASYNC implementation of `AsyncProposer<C>` that lives in
 * the flag-gated `llm.ts` (built by the in-session lane) and drives the child
 * agent through the existing `createRunAgentChildCall` path, emitting a
 * constrained JSON artifact parsed back through `task.deserialize`. The sync
 * rollout here refuses the LLM flag (see `rollout.ts`); the async sibling driver
 * awaits an `AsyncProposer`. Wiring the seam, never invoking it, keeps the
 * default path token-free.
 *
 * Provenance. An outcome says who generated its artifact (`origin`): the LLM
 * proposer marks an accepted child candidate `llm` and a local stand-in for a
 * rejected child output `local`; the local proposer leaves it unset, which the
 * tree reads as `local`. The proposer also keeps a `ProposalTally` per rollout —
 * child results examined, accepted, rejected by `ProposalRejectReason`, and the
 * attempts that fell back — so an experiment can report agent-GENERATED
 * candidates apart from handler calls and from fallbacks.
 */

import type { SeededRng } from "./rng.js";
import type { ProposeParams, ScoredTask } from "./task.js";
import type { CandidateOrigin } from "./types.js";

export interface ProposeOutcome<C> {
	artifact: C;
	tokens: number;
	/**
	 * Who generated `artifact`. Absent means the local proposer. The LLM proposer
	 * sets `"llm"` on an accepted child candidate and `"local"` when it fell back
	 * to the task's mutator after rejecting the child's output.
	 */
	origin?: CandidateOrigin;
}

/** Synchronous, zero-token generation attempt (the local path). */
export interface Proposer<C> {
	propose(parent: C | null, params: ProposeParams, rng: SeededRng, round: number): ProposeOutcome<C>;
}

/** Asynchronous generation attempt (the flag-gated LLM path implements this). */
export interface AsyncProposer<C> {
	propose(parent: C | null, params: ProposeParams, rng: SeededRng, round: number): Promise<ProposeOutcome<C>>;
}

export function createLocalProposer<C>(task: ScoredTask<C>): Proposer<C> {
	return {
		propose(parent, params, rng, round) {
			return { artifact: task.propose(parent, params, rng, round), tokens: 0 };
		},
	};
}

/**
 * Lift a synchronous `Proposer` into an `AsyncProposer`, preserving its outcome
 * (tokens and determinism) exactly. The async in-session driver
 * (`runDreamLoopWithAgent`) uses this so it has a single async driver whether or
 * not the LLM proposer is enabled: the local-only case still awaits, but each
 * attempt resolves to the identical zero-token outcome a sync rollout would
 * produce, so the grown tree stays byte-identical.
 */
export function asyncOf<C>(local: Proposer<C>): AsyncProposer<C> {
	return {
		async propose(parent, params, rng, round) {
			return local.propose(parent, params, rng, round);
		},
	};
}

/**
 * Why a child proposer result was rejected before it could enter the tree.
 *
 * - `parse`: no JSON value could be extracted from the child's output.
 * - `shape`: JSON parsed but is not the task's artifact shape (wrong keys, wrong
 *   `weights` length, non-numeric entries).
 * - `invalid-candidate`: the shape was right but `task.deserialize` refused it.
 * - `error`: the child finished with status `error`.
 * - `length`: the child's response was cut at its output cap.
 * - `aborted`: the child was aborted (the run stops; recorded for the span).
 * - `turn-limit` / `budget`: the child hit its turn limit or token budget.
 */
export const PROPOSAL_REJECT_REASONS = [
	"parse",
	"shape",
	"invalid-candidate",
	"error",
	"length",
	"aborted",
	"turn-limit",
	"budget",
] as const;
export type ProposalRejectReason = (typeof PROPOSAL_REJECT_REASONS)[number];

export function isProposalRejectReason(value: unknown): value is ProposalRejectReason {
	return typeof value === "string" && (PROPOSAL_REJECT_REASONS as readonly string[]).includes(value);
}

/**
 * Per-rollout proposer provenance. Identities the LLM proposer maintains:
 * `llmProposals === llmAccepted + sum(llmRejected)`; each accepted result is one
 * `origin: "llm"` node, so a completed rollout has `llmAccepted` agent-generated
 * probes; `localFallbacks` counts attempts whose LAST child result was rejected
 * (a rejected-then-retried-and-accepted attempt is one rejection and no
 * fallback), so `probes === llmAccepted + localFallbacks` on the LLM path. All
 * zero on the local path.
 */
export interface ProposalTally {
	/** Child proposer results examined (accepted + rejected, retries included). */
	llmProposals: number;
	/** Child results that parsed, deserialized and entered the tree as `origin: "llm"` nodes. */
	llmAccepted: number;
	/** Rejected child results by reason; every reason is present, 0 when unseen. */
	llmRejected: Record<ProposalRejectReason, number>;
	/** Attempts whose candidate came from the local mutator after the child's output was rejected. */
	localFallbacks: number;
}

export function zeroProposalTally(): ProposalTally {
	return {
		llmProposals: 0,
		llmAccepted: 0,
		llmRejected: {
			parse: 0,
			shape: 0,
			"invalid-candidate": 0,
			error: 0,
			length: 0,
			aborted: 0,
			"turn-limit": 0,
			budget: 0,
		},
		localFallbacks: 0,
	};
}

/** `into + from`, as a new tally; neither argument is mutated. */
export function addProposalTally(into: ProposalTally, from: ProposalTally): ProposalTally {
	const sum = zeroProposalTally();
	sum.llmProposals = into.llmProposals + from.llmProposals;
	sum.llmAccepted = into.llmAccepted + from.llmAccepted;
	sum.localFallbacks = into.localFallbacks + from.localFallbacks;
	for (const reason of PROPOSAL_REJECT_REASONS) {
		sum.llmRejected[reason] = (into.llmRejected[reason] ?? 0) + (from.llmRejected[reason] ?? 0);
	}
	return sum;
}

/** Record one accepted child result on `tally` (mutating). */
export function tallyAccepted(tally: ProposalTally): void {
	tally.llmProposals += 1;
	tally.llmAccepted += 1;
}

/** Record one rejected child result on `tally` (mutating); `fellBack` marks the attempt as a local fallback. */
export function tallyRejected(tally: ProposalTally, reason: ProposalRejectReason, fellBack: boolean): void {
	tally.llmProposals += 1;
	tally.llmRejected[reason] += 1;
	if (fellBack) tally.localFallbacks += 1;
}

/** Total rejected child results across every reason. */
export function totalRejected(tally: Pick<ProposalTally, "llmRejected">): number {
	let total = 0;
	for (const reason of PROPOSAL_REJECT_REASONS) total += tally.llmRejected[reason] ?? 0;
	return total;
}
