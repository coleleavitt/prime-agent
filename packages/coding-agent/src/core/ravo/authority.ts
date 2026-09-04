import { canonicalJson, sha256 } from "./canonical-json.js";
import type { JsonValue, RavoGateCertificate, RavoOpponentPool, RavoState } from "./reducer.js";
import { emptyRavoState, ravoStep, ravoW } from "./reducer.js";

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
	const state = input.state ?? emptyAssistedRavoState();
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
		{
			screenThreshold: input.screenThreshold ?? 50,
			epsilon: input.epsilon ?? 1,
		},
	);
	return {
		authorized: stepped.certificate.committed,
		certificate: { ...stepped.certificate, proposalDigest, baselineDigest },
		proposalDigest,
		baselineDigest,
		nextState: stepped.state,
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
