import { canonicalJson, sha256 } from "./canonical-json.js";
import { FAILURE_OPPONENT_PREFIX } from "./failure-ledger.js";
import type {
	JsonValue,
	RavoGateCertificate,
	RavoOpponentPool,
	RavoProvisionalWindow,
	RavoState,
	RavoWindowClock,
} from "./reducer.js";
import {
	emptyRavoState,
	isRavoWindowClock,
	ravoExtendOpponents,
	ravoMarkProvisional,
	ravoStep,
	ravoW,
} from "./reducer.js";
import {
	failureOpponentPassed,
	isRefereeOpponentId,
	type RefereeVerdict,
	refereeDetail,
	refereeOpponentFingerprint,
	refereeOpponentId,
	refereeOpponentPassed,
	refereeVerdictIsEvidence,
} from "./referee.js";

export const ASSISTED_RAVO_CRITERIA = ["evidence", "scope", "minimality", "contracts", "novelty"] as const;

/** Default number of turns a committed champion stays provisional. */
export const DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS = 20;

/**
 * How the authority treats a commit that claims no failure fingerprint.
 *
 * - `measured` (default): the reducer's step stands; lineage, champion and
 *   weakness pressure all advance.
 * - `unmeasured`: the edits are authorized, but `nextState` is the input state.
 *   Nothing can later confirm or refute a commit that claimed nothing, so it
 *   must not raise the deep-gate bar or pressure a criterion.
 * - `reject`: the commit is refused with `rejection: "unclaimed"`; the
 *   evaluation is consumed exactly as any other rejection.
 */
export type UnclaimedCommitPolicy = "measured" | "unmeasured" | "reject";

export function isFailureOpponentId(criterionId: string): boolean {
	return criterionId.startsWith(FAILURE_OPPONENT_PREFIX) && criterionId.length > FAILURE_OPPONENT_PREFIX.length;
}

/** Fingerprint id carried by a failure opponent criterion id (`failure:<id>`). */
export function failureOpponentFingerprint(criterionId: string): string | undefined {
	return isFailureOpponentId(criterionId) ? criterionId.slice(FAILURE_OPPONENT_PREFIX.length) : undefined;
}

export interface AssistedRavoBinding {
	proposalDigest: string;
	baselineDigest: string;
}

export interface AssistedRavoCertificate extends RavoGateCertificate, AssistedRavoBinding {}

export interface AssistedRavoObservation {
	status: "pass" | "fail" | "abstain" | "error";
	score?: number;
	detail?: string;
	failedCriteria?: readonly string[];
	/** Failure fingerprint ids the judge accepted the proposal as addressing. */
	addressedFingerprints?: readonly string[];
}

export interface AssistedRavoAuthorization {
	authorized: boolean;
	certificate: AssistedRavoCertificate;
	proposalDigest: string;
	baselineDigest: string;
	nextState: RavoState<JsonValue>;
}

const opponents: RavoOpponentPool = {
	criteria: ASSISTED_RAVO_CRITERIA.map((id) => ({
		id,
		seedWeight: 1,
		currentWeight: 1,
	})),
};

export function emptyAssistedRavoState(): RavoState<JsonValue> {
	return emptyRavoState<JsonValue>({
		criteria: opponents.criteria.map((criterion) => ({ ...criterion })),
	});
}

/**
 * A provisional window stamped with a clock this build does not know (written
 * by another build) loses the stamp and reads as legacy, which never regresses.
 * Left in place, `ravoW` would reject the whole state and the lineage, weights
 * and evaluated ids would all be discarded for one field.
 */
function withKnownWindowClocks(state: RavoState<JsonValue>): RavoState<JsonValue> {
	if (!Array.isArray(state.lineage)) return state;
	return {
		...state,
		lineage: state.lineage.map((champion) => {
			const window: unknown = champion?.provisional;
			if (typeof window !== "object" || window === null || Array.isArray(window)) return champion;
			const { clock, ...rest } = window as RavoProvisionalWindow;
			return clock === undefined || isRavoWindowClock(clock) ? champion : { ...champion, provisional: rest };
		}),
	};
}

export function normalizeAssistedRavoState(value: unknown): RavoState<JsonValue> {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return emptyAssistedRavoState();
	const record = value as Record<string, unknown>;
	if (Array.isArray(record.lineage) && Array.isArray(record.evaluatedProposalIds) && record.opponents) {
		try {
			const state = withKnownWindowClocks(structuredClone(value) as RavoState<JsonValue>);
			if (ravoW(state)) return state;
		} catch {
			// Fall through to legacy migration or an empty state.
		}
	}

	const legacyEvaluator = record.evaluator as { criteria?: unknown } | undefined;
	if (!Array.isArray(record.lineage) || !Array.isArray(legacyEvaluator?.criteria)) return emptyAssistedRavoState();
	const legacyWeights = new Map<string, number>();
	for (const item of legacyEvaluator.criteria) {
		if (typeof item !== "object" || item === null) continue;
		const criterion = item as Record<string, unknown>;
		if (
			typeof criterion.id === "string" &&
			Number.isSafeInteger(criterion.weight) &&
			(criterion.weight as number) > 0
		) {
			legacyWeights.set(criterion.id, criterion.weight as number);
		}
	}
	const lineage: RavoState<JsonValue>["lineage"] = [];
	let parentId: string | null = null;
	for (const item of record.lineage) {
		if (typeof item !== "object" || item === null) continue;
		const entry = item as Record<string, unknown>;
		if (
			typeof entry.id !== "string" ||
			!entry.id ||
			!Number.isSafeInteger(entry.score) ||
			(entry.score as number) < 0
		)
			continue;
		const missedCriterionIds = Array.isArray(entry.missedCriteria)
			? entry.missedCriteria.filter((id): id is string => typeof id === "string")
			: [];
		lineage.push({
			proposalId: entry.id,
			parentId,
			score: entry.score as number,
			artifact: null,
			missedCriterionIds,
		});
		parentId = entry.id;
	}
	return {
		lineage,
		championId: parentId,
		opponents: {
			criteria: ASSISTED_RAVO_CRITERIA.map((id) => ({
				id,
				seedWeight: 1,
				currentWeight: Math.max(1, legacyWeights.get(id) ?? 1),
			})),
		},
		evaluatedProposalIds: lineage.map((entry) => entry.proposalId),
	};
}

export function ravoArtifactDigest(value: JsonValue): string {
	const normalized = JSON.parse(JSON.stringify(value)) as JsonValue;
	return sha256(canonicalJson(normalized));
}

function isAssistedCriterion(criterionId: string): boolean {
	return (ASSISTED_RAVO_CRITERIA as readonly string[]).includes(criterionId);
}

/**
 * Authorize one complete assisted edit set. The generic reducer is the sole
 * decision authority. Evaluation errors and abstentions are conservative
 * failures. The returned certificate is bound to both candidate and baseline.
 *
 * Pool criteria that /refine never observes (an `arc:*` criterion persisted by
 * an external-evaluator run, say) are dormant passes: with no observation the
 * reducer would count them as abstentions, and every later /refine would be
 * charged for a criterion it cannot evaluate.
 *
 * Recurring failure fingerprints join the pool as opponents
 * (`ravoExtendOpponents`, seed weight 1) before the step, so they participate
 * in the epsilon gate exactly like the five assisted criteria and are
 * pressured like them when missed. A failure opponent named in
 * `failureOpponents` passes iff the judge listed its fingerprint in
 * `addressedFingerprints`, did not also list it as failed, AND the referee's
 * verdict does not charge it (`failureOpponentPassed`: a claim a replay cannot
 * speak to stands, a derivable failure with no reproduced case fails closed).
 * Failure opponents already in the pool but absent from `failureOpponents` are
 * dormant (no longer recurring) and pass; their pressured weight is kept so a
 * later recurrence returns to the gate at full strength.
 *
 * Each fingerprint a replay actually adjudicated (`refereeVerdictIsEvidence`)
 * also joins the pool as `referee:<fingerprint>` (Rocq `Ravo.v` Section 16:
 * the referee is one more opponent, which can only tighten the gate). A claim
 * the referee refutes therefore misses two criteria, which is what carries it
 * past epsilon; an unclaimed or unadjudicated fingerprint is charged exactly
 * what it was charged before.
 *
 * A commit that claims fingerprints is provisional: the champion records them
 * and an observation window of `observationWindowTurns` starting at `turn`,
 * stamped with `turnClock`. A commit that claims nothing follows
 * `unclaimedCommit`.
 */
export function authorizeAssistedRavo(input: {
	proposalId: string;
	artifact: JsonValue;
	/**
	 * The state the certificate is bound to. Pass only the slice the proposal
	 * can be evaluated against (`refinementBaselineView` for harness state):
	 * every byte in here has to be byte-identical again at apply time, so
	 * append-only bookkeeping such as the failure ledger must be left out or a
	 * turn-boundary flush in this or any other process rejects the commit.
	 */
	baseline: JsonValue;
	fastScore: number;
	observation: AssistedRavoObservation;
	state?: RavoState<JsonValue>;
	screenThreshold?: number;
	epsilon?: number;
	/** Slack under the best recorded score tolerated by the deep gate (default 0). */
	deepTolerance?: number;
	/** Criterion ids (`failure:<fingerprint>`) of currently recurring failures. */
	failureOpponents?: readonly string[];
	/** Referee verdicts for the fingerprints the proposal claims (`adjudicateFailureClaims`). */
	refereeVerdicts?: readonly RefereeVerdict[];
	/** Turn at which the commit happens; enables the provisional window. */
	turn?: number;
	/** Clock `turn` is measured on; stamped on the provisional window. */
	turnClock?: RavoWindowClock;
	observationWindowTurns?: number;
	unclaimedCommit?: UnclaimedCommitPolicy;
}): AssistedRavoAuthorization {
	const proposalDigest = ravoArtifactDigest(input.artifact);
	const baselineDigest = ravoArtifactDigest(input.baseline);
	const failed = new Set(input.observation.failedCriteria ?? ASSISTED_RAVO_CRITERIA);
	const addressed = new Set(input.observation.addressedFingerprints ?? []);
	const failureOpponents = (input.failureOpponents ?? []).filter(isFailureOpponentId);
	const active = new Set(failureOpponents);
	const verdicts = new Map((input.refereeVerdicts ?? []).map((verdict) => [verdict.fingerprintId, verdict]));
	// Only a fingerprint a replay actually adjudicated joins the pool as a
	// referee opponent; no_evidence and not_applicable add no new criterion.
	const adjudicated = failureOpponents
		.map((id) => failureOpponentFingerprint(id) ?? "")
		.filter((fingerprint) => refereeVerdictIsEvidence(verdicts.get(fingerprint)))
		.map((fingerprint) => refereeOpponentId(fingerprint));
	const baseState = input.state ?? emptyAssistedRavoState();
	const state: RavoState<JsonValue> = {
		...baseState,
		opponents: ravoExtendOpponents(baseState.opponents, [...failureOpponents, ...adjudicated]),
	};
	const detail = input.observation.detail === undefined ? {} : { detail: input.observation.detail };
	// A judged miss under a passing deep observation is a "fail"; any non-pass
	// deep status propagates to every criterion as a conservative miss.
	const judged = (criterionId: string, passed: boolean) => ({
		criterionId,
		status:
			input.observation.status !== "pass"
				? input.observation.status
				: passed
					? ("pass" as const)
					: ("fail" as const),
		...detail,
	});
	const criteria = [
		...ASSISTED_RAVO_CRITERIA.map((criterionId) => judged(criterionId, !failed.has(criterionId))),
		...state.opponents.criteria
			.filter((criterion) => isFailureOpponentId(criterion.id))
			.map((criterion) => {
				const fingerprint = failureOpponentFingerprint(criterion.id) ?? "";
				const dormant = !active.has(criterion.id);
				const claimed = addressed.has(fingerprint);
				const verdict = verdicts.get(fingerprint);
				const passed = dormant || (failureOpponentPassed(claimed, verdict) && !failed.has(criterion.id));
				if (dormant) {
					return { ...judged(criterion.id, true), detail: "dormant: fingerprint is not currently recurring" };
				}
				return verdict && verdict.status !== "not_applicable"
					? { ...judged(criterion.id, passed), detail: verdict.detail }
					: judged(criterion.id, passed);
			}),
		...state.opponents.criteria
			.filter((criterion) => isRefereeOpponentId(criterion.id))
			.map((criterion) => {
				const fingerprint = refereeOpponentFingerprint(criterion.id) ?? "";
				const claimed = addressed.has(fingerprint);
				const verdict = verdicts.get(fingerprint);
				return {
					...judged(criterion.id, refereeOpponentPassed(claimed, verdict)),
					detail: refereeDetail(verdict, claimed),
				};
			}),
		...state.opponents.criteria
			.filter(
				(criterion) =>
					!isAssistedCriterion(criterion.id) &&
					!isFailureOpponentId(criterion.id) &&
					!isRefereeOpponentId(criterion.id),
			)
			.map((criterion) => ({ ...judged(criterion.id, true), detail: "dormant: not a /refine criterion" })),
	];
	const stepped = ravoStep(
		state,
		{ id: input.proposalId, artifact: input.artifact },
		{
			proposalId: input.proposalId,
			screen: {
				status: input.fastScore >= (input.screenThreshold ?? 50) ? "pass" : "fail",
				score: input.fastScore,
			},
			deep: {
				status: input.observation.status,
				...(input.observation.score === undefined ? {} : { score: input.observation.score }),
				...detail,
			},
			criteria,
		},
		{
			screenThreshold: input.screenThreshold ?? 50,
			epsilon: input.epsilon ?? 1,
			deepTolerance: input.deepTolerance ?? 0,
		},
	);
	let nextState = stepped.state;
	let certificate: RavoGateCertificate = stepped.certificate;
	if (stepped.certificate.committed) {
		const claimedFingerprints = failureOpponents
			.map((id) => failureOpponentFingerprint(id) ?? "")
			.filter((fingerprint) => addressed.has(fingerprint));
		const policy = claimedFingerprints.length > 0 ? "measured" : (input.unclaimedCommit ?? "measured");
		if (policy === "reject") {
			// The same consumed-evaluation state every other rejection leaves:
			// the proposal id is spent, nothing is appended or pressured.
			certificate = { ...stepped.certificate, committed: false, rejection: "unclaimed" };
			nextState = { ...state, evaluatedProposalIds: [...state.evaluatedProposalIds, input.proposalId] };
		} else if (policy === "unmeasured") {
			nextState = baseState;
		} else {
			const turn = input.turn;
			const windowTurns = input.observationWindowTurns ?? DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS;
			nextState = ravoMarkProvisional(nextState, input.proposalId, {
				claimedFingerprints,
				...(turn !== undefined && Number.isSafeInteger(turn) && turn >= 0
					? {
							window: {
								committedTurn: turn,
								untilTurn: turn + Math.max(0, windowTurns),
								...(input.turnClock === undefined ? {} : { clock: input.turnClock }),
							},
						}
					: {}),
			});
		}
	}
	return {
		authorized: certificate.committed,
		certificate: { ...certificate, proposalDigest, baselineDigest },
		proposalDigest,
		baselineDigest,
		nextState,
	};
}

export function assistedRavoBindingMatches(
	authorization: AssistedRavoAuthorization,
	artifact: JsonValue,
	baseline: JsonValue,
): boolean {
	return (
		authorization.proposalDigest === ravoArtifactDigest(artifact) &&
		authorization.baselineDigest === ravoArtifactDigest(baseline)
	);
}

export function assistedRavoCertificateMatches(
	authorization: AssistedRavoAuthorization,
	artifact: JsonValue,
	baseline: JsonValue,
): boolean {
	return authorization.authorized && assistedRavoBindingMatches(authorization, artifact, baseline);
}
