/**
 * Workflow V2 Slice 3 — at-most-once physical-dispatch guard (capability UNAVAILABLE).
 *
 * This is the executor's sole authorization point for the ONE physical provider
 * call of an admitted, materialized turn (docs/WORKFLOW-V2-SLICE3.md §5, §5.1,
 * §8.1). It is dormant: no route, daemon path, or capability negotiation
 * reaches it (workflow-v2-capability.ts returns CAPABILITY_UNAVAILABLE).
 *
 * Why a runtime guard and not the tools-none profile: AgentSession overrides
 * the low-level Agent `shouldStopAfterTurn`, so "one turn" is NOT observable at
 * that option alone. At-most-once dispatch must therefore be enforced by a hard
 * runtime gate tied to the composite admission and a durable journal fact — not
 * by a profile flag.
 *
 * Invariant (§5.1): the single physical provider call is reachable ONLY
 * immediately after a freshly-committed `dispatching` journal fact, and a
 * `dispatching` fact for one admitted turn is committed AT MOST ONCE (the
 * journal claims the exact materialization outbox and asserts no prior
 * dispatching/provider-entered fact inside one generation-fenced transaction).
 * Therefore, across every attempt, restart, and relaunch, the provider is
 * dispatched at most once per admitted turn. Once `dispatching` commits,
 * recovery NEVER invokes the provider again; the crash gap between that commit
 * and the network call is terminal `execution_unknown` (at-most-once over
 * duplicate remote effect — provider idempotency is not assumed).
 *
 * The durable dispatch facts live in the per-worker retained journal
 * (core/workflow-v2-retained-journal.ts). This guard consumes that journal
 * through the injected Slice3DispatchJournal seam so it carries no store of its
 * own; the journal remains the sole authority for dispatch facts.
 */

/** Identity of one admitted turn whose dispatch this guard authorizes. */
export interface Slice3DispatchBinding {
	rlmChildId: string;
	turnId: string;
	admissionReceiptDigest: string;
}

/** Closed, journal-proven dispatch facts for one binding (§5.1). */
export interface Slice3DispatchFacts {
	/** The capture slot armed and `capture_armed` committed. */
	captureArmed: boolean;
	/** An exact, unclaimed materialization outbox exists for this binding. */
	outboxUnclaimed: boolean;
	/** A `dispatching` fact for this binding is durably committed. */
	hasDispatching: boolean;
	/** A `provider_entered` evidence fact for this binding is durably committed. */
	hasProviderEntered: boolean;
}

export type Slice3DispatchClaimCode =
	| "OUTBOX_ALREADY_CLAIMED"
	| "DISPATCHING_ALREADY_COMMITTED"
	| "WORKER_STALE"
	| "ROUTE_STALE"
	| "STORE_CORRUPT";

export type Slice3DispatchClaim = { ok: true } | { ok: false; code: Slice3DispatchClaimCode };

/**
 * Per-worker retained-journal seam (owned by core/workflow-v2-retained-journal.ts),
 * injected so this guard never opens a second store. Every mutating call is a
 * generation-fenced transaction in the real implementation.
 */
export interface Slice3DispatchJournal {
	/** Read the closed dispatch facts for one binding (validated, generation-fenced). */
	readDispatchFacts(binding: Slice3DispatchBinding): Slice3DispatchFacts;
	/**
	 * Atomically: assert no `dispatching`/`provider_entered` fact for this
	 * binding, claim the exact materialization outbox, append and fsync one fresh
	 * `dispatching` fact — all inside one generation-fenced transaction. Returns
	 * a typed rejection instead of committing when the claim cannot be made
	 * exactly once. This is the ONE authorization for a physical provider call.
	 */
	claimAndCommitDispatching(binding: Slice3DispatchBinding): Slice3DispatchClaim;
	/** Commit `provider_entered` evidence for one binding (generation-fenced). */
	commitProviderEntered(binding: Slice3DispatchBinding): void;
}

export type Slice3DispatchOutcome<T> =
	/** The single physical provider call was made exactly once and returned. */
	| { outcome: "dispatched"; value: T }
	/** The physical call was made exactly once and threw; the provider was entered (one effect). */
	| { outcome: "provider_error"; error: unknown }
	/**
	 * A `dispatching`/`provider_entered` fact already exists: the turn was (or may
	 * have been) dispatched. Recovery NEVER relaunches; the caller settles this as
	 * `execution_unknown` unless exact terminal evidence exists. No physical call.
	 */
	| { outcome: "already_dispatched" }
	/** Not armed / no claimable outbox: cannot dispatch from this state. No physical call. */
	| { outcome: "not_ready"; reason: "not_armed" | "outbox_unavailable" }
	/** Lost the atomic outbox/dispatching claim to a fence or race. No physical call. */
	| { outcome: "claim_lost"; code: Slice3DispatchClaimCode }
	/**
	 * `dispatching` committed but the writer fence was lost at the provider-effect
	 * boundary before the physical call: terminal `execution_unknown`, no physical
	 * call, and no relaunch (the committed `dispatching` fact blocks it forever).
	 */
	| { outcome: "fence_lost_after_dispatch" };

/**
 * Executor adapter that authorizes and performs the single physical provider
 * dispatch for an admitted turn. Construct one per turn attempt; it holds no
 * mutable dispatch state (the journal is authoritative), so a fresh instance
 * after restart reaches the exact same at-most-once decision.
 */
export class Slice3RetainedDispatchExecutor {
	constructor(
		private readonly journal: Slice3DispatchJournal,
		/**
		 * Proves the worker still holds its current generation and adopted
		 * supervisor generation. Throws to fence. Re-checked at the provider-effect
		 * boundary; a preflight check cannot authorize a later provider call.
		 */
		private readonly assertFence: () => void = () => {},
	) {}

	/**
	 * Authorize and perform the ONE physical provider call for `binding`, or
	 * refuse without a call. `physicalCall` is invoked at most once per admitted
	 * turn across all attempts/restarts: only a freshly-committed `dispatching`
	 * fact reaches it, and that fact commits at most once.
	 */
	async dispatchOnce<T>(
		binding: Slice3DispatchBinding,
		physicalCall: () => Promise<T>,
	): Promise<Slice3DispatchOutcome<T>> {
		// Read committed facts. A prior dispatching/provider-entry fact means the
		// turn was (or may have been) dispatched: recovery never relaunches.
		let facts: Slice3DispatchFacts;
		try {
			facts = this.journal.readDispatchFacts(binding);
		} catch {
			return { outcome: "already_dispatched" };
		}
		if (facts.hasDispatching || facts.hasProviderEntered) {
			return { outcome: "already_dispatched" };
		}
		if (!facts.captureArmed) {
			return { outcome: "not_ready", reason: "not_armed" };
		}
		if (!facts.outboxUnclaimed) {
			return { outcome: "not_ready", reason: "outbox_unavailable" };
		}

		// The sole authorization: atomically claim the exact outbox and commit one
		// `dispatching` fact. Fails closed if any dispatching/provider fact raced in.
		const claim = this.journal.claimAndCommitDispatching(binding);
		if (!claim.ok) {
			return { outcome: "claim_lost", code: claim.code };
		}

		// Fence recheck AT the provider-effect boundary. If it fails now,
		// `dispatching` is already durable, so this is terminal execution_unknown
		// with zero physical calls — never a relaunch.
		try {
			this.assertFence();
		} catch {
			return { outcome: "fence_lost_after_dispatch" };
		}

		// Provider-entry evidence commits immediately before the one physical call
		// (§5.1). Even if the call is never observed to return, the committed
		// dispatching/provider-entered facts guarantee no second call.
		this.journal.commitProviderEntered(binding);

		try {
			const value = await physicalCall();
			return { outcome: "dispatched", value };
		} catch (error) {
			return { outcome: "provider_error", error };
		}
	}
}
