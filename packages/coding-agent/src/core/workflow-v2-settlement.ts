import {
	type AmbiguousCapture,
	type CaptureClosure,
	type Digest,
	digestValue,
	type ExactUsage,
	type ObservedCapture,
	type SafeError,
	sealWithDigest,
	type TerminalCapture,
	type TurnBinding,
} from "./workflow-v2-terminal-capture.js";

/**
 * Workflow V2 Slice 3 — pure settlement reducer.
 *
 * Ownership (docs/WORKFLOW-V2-SLICE3.md §7.1, §8, file-map §9): a closed,
 * total, I/O-free reduction from a discriminated `terminalCapture`, its
 * `captureClosure`, and durable dispatch/cancel/quiescence facts into one
 * `atomicSettlementCommit` bundling the canonical settlement, the exact
 * `TurnSettled` host event, the terminal replay receipt, and the successor host
 * cursor. It reads no AgentSession, transcript, roster, preview, session
 * message, agent-message, or parent usage aggregate. A possible provider effect
 * without complete durable terminal evidence reduces to `execution_unknown`;
 * recovery never relaunches it.
 */

export const SETTLEMENT_COMMIT_PROTOCOL = "prime.workflow.retained-settlement-commit/v2-slice3" as const;
export const RETAINED_EVENT_PROTOCOL = "prime.workflow.retained-event/v2" as const;
export const TERMINAL_RECEIPT_PROTOCOL = "prime.workflow.retained-terminal-receipt/v2-slice3" as const;

export interface Fence {
	supervisorGeneration: number;
	supervisorIncarnationId: string;
	workerId: string;
	workerGeneration: number;
	workerIncarnationId: string;
	routeRevision: number;
}

export type ProviderEntry = "observed" | "not_observed" | "uncertain";
export type QuiescenceStatus = "proved" | "unproved";

export interface CancelEvidence {
	requested: boolean;
	actuated: boolean;
	actuatedAt: string | null;
	/** Proven that the cancel was ordered strictly after durable completion. */
	orderedAfterCompletion: boolean;
}

export interface SettlementInput {
	binding: TurnBinding;
	fence: Fence;
	capture: TerminalCapture;
	captureClosure: CaptureClosure;
	dispatchingSequence: number;
	providerEntry: ProviderEntry;
	cancel: CancelEvidence;
	quiescence: QuiescenceStatus;
	topologyEvidenceDigest: Digest;
	startedAt: string | null;
	settledAt: string;
	hostCursor: string;
	nextHostCursor: string;
	hostEventId: string;
}

export type TerminalOutcome = "completed" | "failed" | "cancelled" | "execution_unknown";

export interface TurnSettledEvent {
	protocol: typeof RETAINED_EVENT_PROTOCOL;
	hostEventId: string;
	hostCursor: string;
	type: "TurnSettled";
	recordedAt: string;
	data: {
		requestId: string;
		rlmChildId: string;
		turnId: string;
		settlement: Record<string, unknown>;
		evidenceDigest: Digest;
	};
	digest: Digest;
}

export interface TerminalReceipt {
	protocol: typeof TERMINAL_RECEIPT_PROTOCOL;
	requestId: string;
	requestDigest: Digest;
	rlmChildId: string;
	turnId: string;
	hostCursor: string;
	settlementDigest: Digest;
}

export interface AtomicSettlementCommit {
	protocol: typeof SETTLEMENT_COMMIT_PROTOCOL;
	fence: Fence;
	binding: TurnBinding;
	capture: TerminalCapture;
	captureClosure: CaptureClosure;
	topologyEvidenceDigest: Digest;
	settlement: Record<string, unknown>;
	settlementDigest: Digest;
	event: TurnSettledEvent;
	nextHostCursor: string;
	receipt: TerminalReceipt;
	evidence: {
		dispatchingSequence: number;
		providerEntry: ProviderEntry;
		captureClosed: boolean;
		agentEndEvidenceDigest: Digest | null;
		cancelState: "not_requested" | "requested" | "actuated" | "uncertain";
		quiescence: QuiescenceStatus;
	};
	commitDigest: Digest;
}

export type SettlementReduction =
	| { kind: "commit"; commit: AtomicSettlementCommit }
	| { kind: "rejected"; reason: SettlementRejection };

export type SettlementRejection = "correlation_mismatch" | "cursor_not_successor" | "invalid_input";

const PLACEHOLDER: Digest = `sha256:${"0".repeat(64)}`;

export function authorityScopeDigest(binding: TurnBinding): Digest {
	return digestValue({
		authorityId: binding.authorityId,
		parentSessionId: binding.parentSessionId,
		rootSessionId: binding.rootSessionId,
	});
}

function toKnownPrefix(usage: ExactUsage): ExactUsage {
	return { ...usage, finality: "known_prefix" };
}

interface OutcomePlan {
	outcome: TerminalOutcome;
	result: Record<string, unknown>;
	usage: ExactUsage;
	error: SafeError | null;
	cancelActuated: boolean;
	descendantsQuiescent: boolean;
	cancelState: "not_requested" | "requested" | "actuated" | "uncertain";
}

function unknownPlan(usagePrefix: ExactUsage, input: SettlementInput): OutcomePlan {
	const cancelState: OutcomePlan["cancelState"] = input.cancel.actuated
		? "actuated"
		: input.cancel.requested
			? "uncertain"
			: "not_requested";
	return {
		outcome: "execution_unknown",
		result: { kind: "none", reason: "unknown" },
		usage: toKnownPrefix(usagePrefix),
		error: { code: "EXECUTION_UNKNOWN", message: "execution outcome unknown", retryable: false },
		cancelActuated: input.cancel.actuated,
		descendantsQuiescent: input.quiescence === "proved",
		cancelState,
	};
}

function planOutcome(input: SettlementInput): OutcomePlan {
	const { capture, captureClosure, quiescence, cancel } = input;

	// Any ambiguity, unproven quiescence, or unresolved closure is terminal unknown.
	if (capture.kind === "ambiguous") {
		return unknownPlan((capture as AmbiguousCapture).usagePrefix, input);
	}
	if (captureClosure.kind === "ambiguous") {
		return unknownPlan(toKnownPrefix((capture as ObservedCapture).usage), input);
	}
	if (quiescence !== "proved") {
		return unknownPlan(toKnownPrefix((capture as ObservedCapture).usage), input);
	}

	const obs = capture as ObservedCapture;
	const usage = obs.usage;
	if (usage.finality !== "final") return unknownPlan(toKnownPrefix(usage), input);

	// Cancellation branch.
	if (obs.stopReason === "aborted") {
		if (cancel.actuated) {
			return {
				outcome: "cancelled",
				result: { kind: "none", reason: "cancelled" },
				usage,
				error: { code: "CANCELLED", message: "turn cancelled", retryable: false },
				cancelActuated: true,
				descendantsQuiescent: true,
				cancelState: "actuated",
			};
		}
		// Aborted terminal without correlated actuation is uncertain.
		return unknownPlan(toKnownPrefix(usage), input);
	}
	if (cancel.actuated && !cancel.orderedAfterCompletion) {
		// Actuated abort but a non-abort terminal with unproven ordering is uncertain.
		return unknownPlan(toKnownPrefix(usage), input);
	}

	const baseCancelState: OutcomePlan["cancelState"] = cancel.requested ? "requested" : "not_requested";

	// Result/stop-reason classification for a clean, quiescent, closed terminal.
	if (obs.result.kind === "too_large") {
		return {
			outcome: "failed",
			result: obs.result as unknown as Record<string, unknown>,
			usage,
			error: { code: "RESULT_INVALID", message: "result exceeds inline capacity", retryable: false },
			cancelActuated: false,
			descendantsQuiescent: true,
			cancelState: baseCancelState,
		};
	}
	if (obs.stopReason === "error") {
		const code =
			obs.safeError?.code === "AUTH_FAILED" || obs.safeError?.code === "MODEL_UNAVAILABLE"
				? obs.safeError.code
				: "PROVIDER_FAILED";
		return {
			outcome: "failed",
			result: { kind: "none", reason: "provider_error" },
			usage,
			error: { code, message: obs.safeError?.message ?? "provider error", retryable: false },
			cancelActuated: false,
			descendantsQuiescent: true,
			cancelState: baseCancelState,
		};
	}
	if (obs.stopReason === "stop" && obs.result.kind === "text") {
		return {
			outcome: "completed",
			result: obs.result as unknown as Record<string, unknown>,
			usage,
			error: null,
			cancelActuated: false,
			descendantsQuiescent: true,
			cancelState: baseCancelState,
		};
	}
	// Empty stop, length, tool_use, other, or malformed profile terminal.
	return {
		outcome: "failed",
		result: { kind: "none", reason: "no_assistant" },
		usage,
		error: { code: "RESULT_INVALID", message: `unusable terminal (${obs.stopReason})`, retryable: false },
		cancelActuated: false,
		descendantsQuiescent: true,
		cancelState: baseCancelState,
	};
}

/**
 * Reduce durable capture/closure/evidence facts into one atomic settlement
 * commit. Pure and deterministic: identical input yields byte-identical
 * settlement, event, receipt, cursor, and every digest.
 */
export function reduceSettlement(input: SettlementInput): SettlementReduction {
	const { binding, capture, captureClosure } = input;

	// Correlation: capture, closure, and the outer binding must name one turn.
	if (
		capture.binding.rlmChildId !== binding.rlmChildId ||
		capture.binding.turnId !== binding.turnId ||
		capture.binding.requestId !== binding.requestId ||
		captureClosure.binding.rlmChildId !== binding.rlmChildId ||
		captureClosure.binding.turnId !== binding.turnId ||
		capture.invocationId !== captureClosure.invocationId
	) {
		return { kind: "rejected", reason: "correlation_mismatch" };
	}
	if (input.hostCursor === input.nextHostCursor) {
		return { kind: "rejected", reason: "cursor_not_successor" };
	}
	if (!Number.isSafeInteger(input.dispatchingSequence) || input.dispatchingSequence < 1) {
		return { kind: "rejected", reason: "invalid_input" };
	}

	const plan = planOutcome(input);

	const settlement = sealWithDigest(
		{
			authorityScope: authorityScopeDigest(binding),
			parentId: binding.parentSessionId,
			requestId: binding.requestId,
			nodeId: binding.nodeId,
			attemptId: binding.attemptId,
			rlmChildId: binding.rlmChildId,
			turnId: binding.turnId,
			admittedAt: binding.admittedAt,
			startedAt: input.startedAt,
			settledAt: input.settledAt,
			cancelActuated: plan.cancelActuated,
			descendantsQuiescent: plan.descendantsQuiescent,
			hostCursor: input.hostCursor,
			settlementDigest: PLACEHOLDER,
			outcome: plan.outcome,
			result: plan.result,
			usage: { ...plan.usage },
			error: plan.error,
			workflowChildId: binding.workflowChildId,
			requestDigest: binding.requestDigest,
		} as Record<string, unknown>,
		"settlementDigest",
	);
	const settlementDigest = settlement.settlementDigest as Digest;

	const evidenceDigest = digestValue({
		capture,
		captureClosure,
		cancel: input.cancel,
		quiescence: input.quiescence,
		dispatch: { dispatchingSequence: input.dispatchingSequence, providerEntry: input.providerEntry },
		topologyEvidenceDigest: input.topologyEvidenceDigest,
	});

	const event = sealWithDigest(
		{
			protocol: RETAINED_EVENT_PROTOCOL,
			hostEventId: input.hostEventId,
			hostCursor: input.hostCursor,
			type: "TurnSettled",
			recordedAt: input.settledAt,
			data: {
				requestId: binding.requestId,
				rlmChildId: binding.rlmChildId,
				turnId: binding.turnId,
				settlement,
				evidenceDigest,
			},
			digest: PLACEHOLDER,
		} as Record<string, unknown>,
		"digest",
	) as unknown as TurnSettledEvent;

	const receipt: TerminalReceipt = {
		protocol: TERMINAL_RECEIPT_PROTOCOL,
		requestId: binding.requestId,
		requestDigest: binding.requestDigest,
		rlmChildId: binding.rlmChildId,
		turnId: binding.turnId,
		hostCursor: input.hostCursor,
		settlementDigest,
	};

	const agentEndEvidenceDigest: Digest | null =
		captureClosure.kind === "observed" ? captureClosure.closureDigest : null;

	const commit = sealWithDigest(
		{
			protocol: SETTLEMENT_COMMIT_PROTOCOL,
			fence: input.fence,
			binding,
			capture,
			captureClosure,
			topologyEvidenceDigest: input.topologyEvidenceDigest,
			settlement,
			settlementDigest,
			event,
			nextHostCursor: input.nextHostCursor,
			receipt,
			evidence: {
				dispatchingSequence: input.dispatchingSequence,
				providerEntry: input.providerEntry,
				captureClosed: captureClosure.kind === "observed",
				agentEndEvidenceDigest,
				cancelState: plan.cancelState,
				quiescence: input.quiescence,
			},
			commitDigest: PLACEHOLDER,
		} as unknown as AtomicSettlementCommit,
		"commitDigest",
	);

	return { kind: "commit", commit };
}

// ---------------------------------------------------------------------------
// Closed re-validation (settlement/capture portion of the semantic validator)
// ---------------------------------------------------------------------------

export type CommitValidation = { ok: true } | { ok: false; reason: string };

/**
 * Re-derive every digest and cross-check every repeated binding/ID/cursor field
 * and outcome/usage rule of one atomic settlement commit. Any contradiction is
 * rejected before commit, hydration, replay, or projection. This is the
 * settlement/capture contribution to WorkflowV2Slice3SemanticValidator/v1.
 */
export function validateSettlementCommit(commit: AtomicSettlementCommit): CommitValidation {
	const fail = (reason: string): CommitValidation => ({ ok: false, reason });

	// Digest self-consistency.
	if (recomputeSealed(commit, "commitDigest") !== commit.commitDigest) return fail("commit_digest");
	if (recomputeSealed(commit.event as unknown as Record<string, unknown>, "digest") !== commit.event.digest)
		return fail("event_digest");
	if (recomputeSealed(commit.settlement, "settlementDigest") !== (commit.settlement.settlementDigest as string))
		return fail("settlement_digest");

	// Capture / closure digests.
	const capV = validateCaptureDigest(commit.capture);
	if (!capV.ok) return capV;
	const closeV = validateClosureDigest(commit.captureClosure);
	if (!closeV.ok) return closeV;

	// Repeated settlement-digest copies must agree.
	const sd = commit.settlement.settlementDigest as string;
	if (commit.settlementDigest !== sd) return fail("settlement_digest_copy");
	if (commit.receipt.settlementDigest !== sd) return fail("receipt_settlement_digest");

	// One host cursor names the settlement, event, and receipt.
	const cursor = commit.settlement.hostCursor as string;
	if (commit.event.hostCursor !== cursor) return fail("event_cursor");
	if (commit.receipt.hostCursor !== cursor) return fail("receipt_cursor");
	if (commit.nextHostCursor === cursor) return fail("next_cursor_equal");

	// Event carries the exact settlement object and correlation IDs.
	if (commit.event.data.settlement !== commit.settlement) return fail("event_settlement_ref");
	if (
		commit.event.data.requestId !== commit.settlement.requestId ||
		commit.event.data.rlmChildId !== commit.settlement.rlmChildId ||
		commit.event.data.turnId !== commit.settlement.turnId
	)
		return fail("event_correlation");

	// Binding equality across settlement, receipt, and binding.
	const b = commit.binding;
	if (
		commit.settlement.requestId !== b.requestId ||
		commit.settlement.rlmChildId !== b.rlmChildId ||
		commit.settlement.turnId !== b.turnId ||
		commit.settlement.workflowChildId !== b.workflowChildId ||
		commit.settlement.requestDigest !== b.requestDigest ||
		commit.settlement.parentId !== b.parentSessionId
	)
		return fail("binding_copy");
	if (commit.receipt.requestId !== b.requestId || commit.receipt.requestDigest !== b.requestDigest)
		return fail("receipt_binding");

	// Outcome table enforcement.
	const outcomeV = validateOutcome(commit);
	if (!outcomeV.ok) return outcomeV;

	// Evidence closed-field agreement.
	const closed = commit.captureClosure.kind === "observed";
	if (commit.evidence.captureClosed !== closed) return fail("evidence_capture_closed");
	if (commit.evidence.quiescence !== (commit.settlement.descendantsQuiescent ? "proved" : "unproved")) {
		// Only unknown may carry unproven quiescence; completed/failed/cancelled require proved.
		if (commit.settlement.outcome !== "execution_unknown") return fail("evidence_quiescence");
	}
	return { ok: true };
}

function validateOutcome(commit: AtomicSettlementCommit): CommitValidation {
	const s = commit.settlement;
	const outcome = s.outcome as string;
	const usage = s.usage as ExactUsage;
	const fail = (reason: string): CommitValidation => ({ ok: false, reason });
	if (outcome === "execution_unknown") {
		if (usage.finality !== "known_prefix") return fail("unknown_usage_finality");
		if ((s.error as SafeError)?.code !== "EXECUTION_UNKNOWN") return fail("unknown_error");
	} else {
		if (usage.finality !== "final") return fail("final_usage_required");
		if (s.descendantsQuiescent !== true) return fail("quiescence_required");
	}
	if (outcome === "completed") {
		if (s.cancelActuated !== false) return fail("completed_cancel");
		if (s.error !== null) return fail("completed_error");
		if ((s.result as { kind?: string }).kind !== "text") return fail("completed_result");
	}
	if (outcome === "cancelled") {
		if (s.cancelActuated !== true) return fail("cancelled_flag");
		if ((s.error as SafeError)?.code !== "CANCELLED") return fail("cancelled_error");
	}
	return { ok: true };
}

function validateCaptureDigest(capture: TerminalCapture): CommitValidation {
	if (capture.kind === "observed") {
		if (recomputeSealed(capture as unknown as Record<string, unknown>, "captureDigest") !== capture.captureDigest)
			return { ok: false, reason: "capture_digest" };
		if (capture.observationCount !== 1) return { ok: false, reason: "capture_count" };
	} else {
		if (recomputeSealed(capture as unknown as Record<string, unknown>, "evidenceDigest") !== capture.evidenceDigest)
			return { ok: false, reason: "capture_evidence_digest" };
	}
	return { ok: true };
}

function validateClosureDigest(closure: CaptureClosure): CommitValidation {
	if (closure.kind === "observed") {
		if (recomputeSealed(closure as unknown as Record<string, unknown>, "closureDigest") !== closure.closureDigest)
			return { ok: false, reason: "closure_digest" };
	} else {
		if (recomputeSealed(closure as unknown as Record<string, unknown>, "evidenceDigest") !== closure.evidenceDigest)
			return { ok: false, reason: "closure_evidence_digest" };
	}
	return { ok: true };
}

function recomputeSealed(obj: object, field: string): string {
	const { [field]: _drop, ...rest } = obj as Record<string, unknown>;
	return digestValue(rest);
}
