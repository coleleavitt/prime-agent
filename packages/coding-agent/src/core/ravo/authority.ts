import { canonicalJson, sha256 } from "./canonical-json.js";
import { FAILURE_OPPONENT_PREFIX } from "./failure-ledger.js";
import type { JsonValue, RavoGateCertificate, RavoOpponentPool, RavoState } from "./reducer.js";
import { emptyRavoState, ravoExtendOpponents, ravoMarkProvisional, ravoStep, ravoW } from "./reducer.js";

export const ASSISTED_RAVO_CRITERIA = ["evidence", "scope", "minimality", "contracts", "novelty"] as const;

/** Default number of turns a committed champion stays provisional. */
export const DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS = 20;

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

export function normalizeAssistedRavoState(value: unknown): RavoState<JsonValue> {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return emptyAssistedRavoState();
	const record = value as Record<string, unknown>;
	if (Array.isArray(record.lineage) && Array.isArray(record.evaluatedProposalIds) && record.opponents) {
		try {
			const state = structuredClone(value) as RavoState<JsonValue>;
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

/**
 * Authorize one complete assisted edit set. The generic reducer is the sole
 * decision authority. Evaluation errors and abstentions are conservative
 * failures. The returned certificate is bound to both candidate and baseline.
 *
 * Recurring failure fingerprints join the pool as opponents
 * (`ravoExtendOpponents`, seed weight 1) before the step, so they participate
 * in the epsilon gate exactly like the five assisted criteria and are
 * pressured like them when missed. A failure opponent named in
 * `failureOpponents` passes iff the judge listed its fingerprint in
 * `addressedFingerprints` and did not also list it as failed. Failure
 * opponents already in the pool but absent from `failureOpponents` are
 * dormant (no longer recurring) and pass; their pressured weight is kept so a
 * later recurrence returns to the gate at full strength.
 *
 * A commit is provisional: the champion records the claimed fingerprints and
 * an observation window of `observationWindowTurns` turns starting at `turn`
 * (see `ravoObserveChampion`).
 */
export function authorizeAssistedRavo(input: {
	proposalId: string;
	artifact: JsonValue;
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
	/** Turn at which the commit happens; enables the provisional window. */
	turn?: number;
	observationWindowTurns?: number;
}): AssistedRavoAuthorization {
	const proposalDigest = ravoArtifactDigest(input.artifact);
	const baselineDigest = ravoArtifactDigest(input.baseline);
	const failed = new Set(input.observation.failedCriteria ?? ASSISTED_RAVO_CRITERIA);
	const addressed = new Set(input.observation.addressedFingerprints ?? []);
	const failureOpponents = (input.failureOpponents ?? []).filter(isFailureOpponentId);
	const active = new Set(failureOpponents);
	const baseState = input.state ?? emptyAssistedRavoState();
	const state: RavoState<JsonValue> = {
		...baseState,
		opponents: ravoExtendOpponents(baseState.opponents, failureOpponents),
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
				const passed = dormant || (addressed.has(fingerprint) && !failed.has(criterion.id));
				return dormant
					? { ...judged(criterion.id, true), detail: "dormant: fingerprint is not currently recurring" }
					: judged(criterion.id, passed);
			}),
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
	if (stepped.certificate.committed) {
		const claimedFingerprints = failureOpponents
			.map((id) => failureOpponentFingerprint(id) ?? "")
			.filter((fingerprint) => addressed.has(fingerprint));
		const turn = input.turn;
		const windowTurns = input.observationWindowTurns ?? DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS;
		nextState = ravoMarkProvisional(nextState, input.proposalId, {
			claimedFingerprints,
			...(turn !== undefined && Number.isSafeInteger(turn) && turn >= 0
				? { window: { committedTurn: turn, untilTurn: turn + Math.max(0, windowTurns) } }
				: {}),
		});
	}
	return {
		authorized: stepped.certificate.committed,
		certificate: { ...stepped.certificate, proposalDigest, baselineDigest },
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
