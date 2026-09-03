import { canonicalJson, sha256 } from "./canonical-json.js";
import type { JsonValue, RavoGateCertificate, RavoOpponentPool, RavoState } from "./reducer.js";
import { emptyRavoState, ravoStep } from "./reducer.js";

export const ASSISTED_RAVO_CRITERIA = ["evidence", "scope", "minimality", "contracts", "novelty"] as const;

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
}

export interface AssistedRavoAuthorization {
	authorized: boolean;
	certificate: AssistedRavoCertificate;
	proposalDigest: string;
	baselineDigest: string;
	nextState: RavoState<JsonValue>;
}

const opponents: RavoOpponentPool = {
	criteria: ASSISTED_RAVO_CRITERIA.map((id) => ({ id, seedWeight: 1, currentWeight: 1 })),
};

export function ravoArtifactDigest(value: JsonValue): string {
	const normalized = JSON.parse(JSON.stringify(value)) as JsonValue;
	return sha256(canonicalJson(normalized));
}

/**
 * Authorize one complete assisted edit set. The generic reducer is the sole
 * decision authority. Evaluation errors and abstentions are conservative
 * failures. The returned certificate is bound to both candidate and baseline.
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
}): AssistedRavoAuthorization {
	const proposalDigest = ravoArtifactDigest(input.artifact);
	const baselineDigest = ravoArtifactDigest(input.baseline);
	const failed = new Set(input.observation.failedCriteria ?? ASSISTED_RAVO_CRITERIA);
	const state = input.state ?? emptyRavoState<JsonValue>(opponents);
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
				...(input.observation.detail === undefined ? {} : { detail: input.observation.detail }),
			},
			criteria: ASSISTED_RAVO_CRITERIA.map((criterionId) => ({
				criterionId,
				status: input.observation.status === "pass" && !failed.has(criterionId) ? "pass" : input.observation.status,
				...(input.observation.detail === undefined ? {} : { detail: input.observation.detail }),
			})),
		},
		{ screenThreshold: input.screenThreshold ?? 50, epsilon: input.epsilon ?? 1 },
	);
	return {
		authorized: stepped.certificate.committed,
		certificate: { ...stepped.certificate, proposalDigest, baselineDigest },
		proposalDigest,
		baselineDigest,
		nextState: stepped.state,
	};
}

export function assistedRavoCertificateMatches(
	authorization: AssistedRavoAuthorization,
	artifact: JsonValue,
	baseline: JsonValue,
): boolean {
	return (
		authorization.authorized &&
		authorization.proposalDigest === ravoArtifactDigest(artifact) &&
		authorization.baselineDigest === ravoArtifactDigest(baseline)
	);
}
