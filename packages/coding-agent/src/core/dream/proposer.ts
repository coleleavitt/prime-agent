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
 */

import type { SeededRng } from "./rng.js";
import type { ProposeParams, ScoredTask } from "./task.js";

export interface ProposeOutcome<C> {
	artifact: C;
	tokens: number;
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
