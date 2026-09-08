/**
 * Pure RAVO v2 reducer.
 *
 * All scores and weights are non-negative safe integers. This keeps every gate
 * comparison exact and makes the complete state and certificate JSON-safe.
 */

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };
export type GateStatus = "pass" | "fail" | "abstain" | "error";

export interface RavoCriterion {
	id: string;
	seedWeight: number;
	currentWeight: number;
}

export interface RavoOpponentPool {
	criteria: RavoCriterion[];
}

/**
 * Observation window attached to a provisional commit. A champion that
 * claimed to address recurring failure fingerprints stays provisional until
 * `untilTurn`; a claimed fingerprint recurring inside the window is a measured
 * fault (the judge said pass, the outcome said fail) and is recorded here.
 */
export interface RavoProvisionalWindow {
	committedTurn: number;
	untilTurn: number;
	observedRecurrence?: { turn: number; fingerprints: string[] };
}

export interface RavoChampion<TArtifact extends JsonValue = JsonValue> {
	proposalId: string;
	parentId: string | null;
	score: number;
	artifact: TArtifact;
	missedCriterionIds: string[];
	/** Failure fingerprint ids the committed proposal claimed to address. */
	claimedFingerprints?: string[];
	provisional?: RavoProvisionalWindow;
}

export interface RavoState<TArtifact extends JsonValue = JsonValue> {
	lineage: RavoChampion<TArtifact>[];
	championId: string | null;
	opponents: RavoOpponentPool;
	evaluatedProposalIds: string[];
}

export interface RavoProposal<TArtifact extends JsonValue = JsonValue> {
	id: string;
	artifact: TArtifact;
}

export interface RavoScreenObservation {
	status: GateStatus;
	score?: number;
	detail?: string;
}

export interface RavoDeepObservation {
	status: GateStatus;
	score?: number;
	detail?: string;
}

export interface RavoCriterionObservation {
	criterionId: string;
	status: GateStatus;
	detail?: string;
}

/** A completed observation belongs to one proposal and is consumed once. */
export interface RavoEvaluation {
	proposalId: string;
	screen: RavoScreenObservation;
	deep: RavoDeepObservation;
	criteria: RavoCriterionObservation[];
}

export interface RavoConfig {
	screenThreshold: number;
	/** Maximum current opponent weight which may be missed. */
	epsilon: number;
	/**
	 * Slack under the best recorded score tolerated by the deep gate (default
	 * 0, the strict ratchet of the formalization). Champion scores are recorded
	 * unslacked, so `ravoBestScore` stays a running max either way.
	 */
	deepTolerance?: number;
}

export type RavoRejection = "already_evaluated" | "invalid_input" | "screen" | "deep" | "opponents";

export interface RavoCriterionCertificate {
	criterionId: string;
	status: GateStatus;
	seedWeight: number;
	currentWeight: number;
	countedAsMissed: boolean;
	detail?: string;
}

/** A deterministic, JSON-compatible witness for every gate decision. */
export interface RavoGateCertificate {
	proposalId: string;
	previousChampionId: string | null;
	previousBestScore: number;
	screenThreshold: number;
	epsilon: number;
	deepTolerance: number;
	screen: RavoScreenObservation;
	deep: RavoDeepObservation;
	criteria: RavoCriterionCertificate[];
	missedCriterionIds: string[];
	missedSeedWeight: number;
	missedCurrentWeight: number;
	committed: boolean;
	rejection?: RavoRejection;
}

export interface RavoStepResult<TArtifact extends JsonValue = JsonValue> {
	state: RavoState<TArtifact>;
	certificate: RavoGateCertificate;
}

function isSafeNatural(value: number): boolean {
	return Number.isSafeInteger(value) && value >= 0;
}

function checkedAdd(left: number, right: number): number | undefined {
	const sum = left + right;
	return Number.isSafeInteger(sum) ? sum : undefined;
}

function checkedDouble(value: number): number | undefined {
	return value <= Math.floor(Number.MAX_SAFE_INTEGER / 2) ? value * 2 : undefined;
}

function sortedUnique(values: readonly string[]): string[] {
	return [...new Set(values)].sort((left, right) => left.localeCompare(right));
}

export function ravoBestScore(lineage: readonly RavoChampion[]): number {
	let best = 0;
	for (const champion of lineage) best = Math.max(best, champion.score);
	return best;
}

export function emptyRavoState<TArtifact extends JsonValue>(opponents: RavoOpponentPool): RavoState<TArtifact> {
	return { lineage: [], championId: null, opponents, evaluatedProposalIds: [] };
}

/** Current weights are positive safe integers and dominate their seed weights. */
export function validRavoWeights(pool: RavoOpponentPool): boolean {
	const ids = new Set<string>();
	return pool.criteria.every((criterion) => {
		if (!criterion.id || ids.has(criterion.id)) return false;
		ids.add(criterion.id);
		return (
			Number.isSafeInteger(criterion.seedWeight) &&
			criterion.seedWeight > 0 &&
			Number.isSafeInteger(criterion.currentWeight) &&
			criterion.currentWeight >= criterion.seedWeight
		);
	});
}

function validProvisionalWindow(window: RavoProvisionalWindow | undefined): boolean {
	if (window === undefined) return true;
	if (!isSafeNatural(window.committedTurn) || !isSafeNatural(window.untilTurn)) return false;
	if (window.untilTurn < window.committedTurn) return false;
	const observed = window.observedRecurrence;
	if (observed === undefined) return true;
	return (
		isSafeNatural(observed.turn) &&
		observed.turn >= window.committedTurn &&
		observed.turn <= window.untilTurn &&
		observed.fingerprints.every((id) => typeof id === "string" && id.length > 0)
	);
}

/** `ravoW`: all serializable reducer invariants, including the champion chain. */
export function ravoW(state: RavoState): boolean {
	if (!validRavoWeights(state.opponents)) return false;
	if (new Set(state.evaluatedProposalIds).size !== state.evaluatedProposalIds.length) return false;
	let parent: string | null = null;
	const committed = new Set<string>();
	for (const champion of state.lineage) {
		if (!champion.proposalId || committed.has(champion.proposalId) || champion.parentId !== parent) return false;
		if (!isSafeNatural(champion.score)) return false;
		if (!validProvisionalWindow(champion.provisional)) return false;
		if (champion.claimedFingerprints?.some((id) => typeof id !== "string" || id.length === 0)) return false;
		committed.add(champion.proposalId);
		parent = champion.proposalId;
	}
	if (state.championId !== parent) return false;
	return state.lineage.every((entry) => state.evaluatedProposalIds.includes(entry.proposalId));
}

/** Exact weighted opponent gate (`F_gatekeeper`). */
export function ravoGatekeeper(missedWeight: number, epsilon: number): boolean {
	return isSafeNatural(missedWeight) && isSafeNatural(epsilon) && missedWeight <= epsilon;
}

/** Alias matching the formalization's predicate name. */
export const F_gatekeeper = ravoGatekeeper;

/**
 * Apply weakness pressure. Unknown ids make this a no-op. Overflow also makes
 * this a no-op, preserving a valid JSON-safe state rather than wrapping.
 */
export function ravoPressure(pool: RavoOpponentPool, weakCriterionIds: readonly string[]): RavoOpponentPool {
	const weak = new Set(weakCriterionIds);
	const replacements = new Map<string, number>();
	for (const criterion of pool.criteria) {
		if (!weak.has(criterion.id)) continue;
		const doubled = checkedDouble(criterion.currentWeight);
		if (doubled === undefined) return pool;
		replacements.set(criterion.id, doubled);
	}
	if (replacements.size === 0) return pool;
	return {
		criteria: pool.criteria.map((criterion) => {
			const currentWeight = replacements.get(criterion.id);
			return currentWeight === undefined ? criterion : { ...criterion, currentWeight };
		}),
	};
}

/**
 * Extend the opponent pool with new criteria at seed weight 1. Existing ids
 * are kept untouched, so this is idempotent and never lowers a weight. Adding
 * an opponent can only raise missedWeight (`missedWeight_app`), so extension
 * only tightens the gate: a proposal that clears the extended pool clears the
 * original one. `ravoW` is preserved because every added criterion satisfies
 * `validRavoWeights` and the lineage is untouched.
 */
export function ravoExtendOpponents(pool: RavoOpponentPool, criterionIds: readonly string[]): RavoOpponentPool {
	const existing = new Set(pool.criteria.map((criterion) => criterion.id));
	const added: RavoCriterion[] = [];
	for (const id of criterionIds) {
		if (!id || existing.has(id)) continue;
		existing.add(id);
		added.push({ id, seedWeight: 1, currentWeight: 1 });
	}
	if (added.length === 0) return pool;
	return { criteria: [...pool.criteria, ...added] };
}

/**
 * Mark a committed champion provisional: record the fingerprints it claimed
 * to address and the observation window `[committedTurn, untilTurn]`. Unknown
 * champion ids and invalid windows are no-ops. Lineage order and scores are
 * never touched, so `ravoBestScore` monotonicity is unaffected.
 */
export function ravoMarkProvisional<TArtifact extends JsonValue>(
	state: RavoState<TArtifact>,
	championId: string,
	options: { claimedFingerprints: readonly string[]; window?: { committedTurn: number; untilTurn: number } },
): RavoState<TArtifact> {
	const index = state.lineage.findIndex((champion) => champion.proposalId === championId);
	if (index === -1) return state;
	const claimedFingerprints = sortedUnique(options.claimedFingerprints.filter((id) => id.length > 0));
	const provisional: RavoProvisionalWindow | undefined = options.window
		? { committedTurn: options.window.committedTurn, untilTurn: options.window.untilTurn }
		: undefined;
	if (!validProvisionalWindow(provisional)) return state;
	const lineage = state.lineage.map((champion, position) =>
		position === index
			? { ...champion, claimedFingerprints, ...(provisional === undefined ? {} : { provisional }) }
			: champion,
	);
	return { ...state, lineage };
}

/**
 * Observe recurred failure fingerprints against a champion. A regression is a
 * measured fault: the champion is provisional, `turn` lies inside its window,
 * and at least one claimed fingerprint recurred. The recurrence is recorded on
 * the champion; lineage order, scores, and opponent weights never change, so
 * the caller must route the fault through a gated repair, never a bypass.
 */
export function ravoObserveChampion<TArtifact extends JsonValue>(
	state: RavoState<TArtifact>,
	championId: string,
	recurredFingerprints: readonly string[],
	turn: number,
): { state: RavoState<TArtifact>; regression: boolean } {
	const index = state.lineage.findIndex((champion) => champion.proposalId === championId);
	if (index === -1) return { state, regression: false };
	const champion = state.lineage[index];
	const window = champion.provisional;
	if (!window || !isSafeNatural(turn) || turn < window.committedTurn || turn > window.untilTurn) {
		return { state, regression: false };
	}
	const claimed = new Set(champion.claimedFingerprints ?? []);
	const fingerprints = sortedUnique(recurredFingerprints.filter((id) => claimed.has(id)));
	if (fingerprints.length === 0) return { state, regression: false };
	const lineage = state.lineage.map((entry, position) =>
		position === index ? { ...entry, provisional: { ...window, observedRecurrence: { turn, fingerprints } } } : entry,
	);
	return { state: { ...state, lineage }, regression: true };
}

/** Exact comparison of a criterion's share without division. */
export function shareStrictlyIncreased(
	before: RavoOpponentPool,
	after: RavoOpponentPool,
	criterionId: string,
): boolean {
	const beforeCriterion = before.criteria.find((criterion) => criterion.id === criterionId);
	const afterCriterion = after.criteria.find((criterion) => criterion.id === criterionId);
	if (!beforeCriterion || !afterCriterion) return false;
	const beforeTotal = before.criteria.reduce((sum, criterion) => sum + criterion.currentWeight, 0);
	const afterTotal = after.criteria.reduce((sum, criterion) => sum + criterion.currentWeight, 0);
	if (!Number.isSafeInteger(beforeTotal) || !Number.isSafeInteger(afterTotal)) return false;
	// a/b < c/d iff a*d < c*b. Products can exceed the safe range, so BigInt is
	// used only transiently; no BigInt enters JSON-compatible domain state.
	return (
		BigInt(beforeCriterion.currentWeight) * BigInt(afterTotal) <
		BigInt(afterCriterion.currentWeight) * BigInt(beforeTotal)
	);
}

function invalidCertificate<TArtifact extends JsonValue>(
	state: RavoState<TArtifact>,
	proposal: RavoProposal<TArtifact>,
	evaluation: RavoEvaluation,
	config: RavoConfig,
	rejection: RavoRejection,
): RavoStepResult<TArtifact> {
	return {
		state,
		certificate: {
			proposalId: proposal.id,
			previousChampionId: state.championId,
			previousBestScore: ravoBestScore(state.lineage),
			screenThreshold: config.screenThreshold,
			epsilon: config.epsilon,
			deepTolerance: config.deepTolerance ?? 0,
			screen: evaluation.screen,
			deep: evaluation.deep,
			criteria: [],
			missedCriterionIds: [],
			missedSeedWeight: 0,
			missedCurrentWeight: 0,
			committed: false,
			rejection,
		},
	};
}

/**
 * Consume one proposal evaluation exactly once. Screen failures never reach the
 * deep commit gate. Missing, abstaining, and errored opponents are conservative
 * misses. A deep abstention/error is a conservative rejection.
 */
export function ravoStep<TArtifact extends JsonValue>(
	state: RavoState<TArtifact>,
	proposal: RavoProposal<TArtifact>,
	evaluation: RavoEvaluation,
	config: RavoConfig,
): RavoStepResult<TArtifact> {
	if (state.evaluatedProposalIds.includes(proposal.id)) {
		return invalidCertificate(state, proposal, evaluation, config, "already_evaluated");
	}
	if (
		!proposal.id ||
		evaluation.proposalId !== proposal.id ||
		!ravoW(state) ||
		!isSafeNatural(config.screenThreshold) ||
		!isSafeNatural(config.epsilon) ||
		!isSafeNatural(config.deepTolerance ?? 0)
	) {
		return invalidCertificate(state, proposal, evaluation, config, "invalid_input");
	}

	const evaluatedState = { ...state, evaluatedProposalIds: [...state.evaluatedProposalIds, proposal.id] };
	const screenPass =
		evaluation.screen.status === "pass" &&
		evaluation.screen.score !== undefined &&
		isSafeNatural(evaluation.screen.score) &&
		evaluation.screen.score >= config.screenThreshold;
	if (!screenPass) return invalidCertificate(evaluatedState, proposal, evaluation, config, "screen");

	const previousBestScore = ravoBestScore(state.lineage);
	const deepTolerance = config.deepTolerance ?? 0;
	const slackedScore =
		evaluation.deep.score !== undefined && isSafeNatural(evaluation.deep.score)
			? checkedAdd(evaluation.deep.score, deepTolerance)
			: undefined;
	const deepPass =
		evaluation.deep.status === "pass" && slackedScore !== undefined && slackedScore >= previousBestScore;
	if (!deepPass) return invalidCertificate(evaluatedState, proposal, evaluation, config, "deep");

	const observations = new Map<string, RavoCriterionObservation>();
	let duplicateObservation = false;
	for (const observation of evaluation.criteria) {
		if (observations.has(observation.criterionId)) duplicateObservation = true;
		observations.set(observation.criterionId, observation);
	}
	if (
		duplicateObservation ||
		evaluation.criteria.some((item) => !state.opponents.criteria.some((c) => c.id === item.criterionId))
	) {
		return invalidCertificate(evaluatedState, proposal, evaluation, config, "invalid_input");
	}

	let missedSeedWeight = 0;
	let missedCurrentWeight = 0;
	let overflow = false;
	const criteria = [...state.opponents.criteria]
		.sort((left, right) => left.id.localeCompare(right.id))
		.map((criterion): RavoCriterionCertificate => {
			const observation = observations.get(criterion.id);
			const status = observation?.status ?? "abstain";
			const countedAsMissed = status !== "pass";
			if (countedAsMissed) {
				const nextSeed = checkedAdd(missedSeedWeight, criterion.seedWeight);
				const nextCurrent = checkedAdd(missedCurrentWeight, criterion.currentWeight);
				if (nextSeed === undefined || nextCurrent === undefined) overflow = true;
				else {
					missedSeedWeight = nextSeed;
					missedCurrentWeight = nextCurrent;
				}
			}
			return {
				criterionId: criterion.id,
				status,
				seedWeight: criterion.seedWeight,
				currentWeight: criterion.currentWeight,
				countedAsMissed,
				...(observation?.detail === undefined ? {} : { detail: observation.detail }),
			};
		});
	if (overflow) return invalidCertificate(evaluatedState, proposal, evaluation, config, "invalid_input");

	const missedCriterionIds = sortedUnique(
		criteria.filter((item) => item.countedAsMissed).map((item) => item.criterionId),
	);
	const committed = ravoGatekeeper(missedCurrentWeight, config.epsilon);
	const certificate: RavoGateCertificate = {
		proposalId: proposal.id,
		previousChampionId: state.championId,
		previousBestScore,
		screenThreshold: config.screenThreshold,
		epsilon: config.epsilon,
		deepTolerance,
		screen: evaluation.screen,
		deep: evaluation.deep,
		criteria,
		missedCriterionIds,
		missedSeedWeight,
		missedCurrentWeight,
		committed,
		...(committed ? {} : { rejection: "opponents" as const }),
	};
	if (!committed) return { state: evaluatedState, certificate };

	const champion: RavoChampion<TArtifact> = {
		proposalId: proposal.id,
		parentId: state.championId,
		score: evaluation.deep.score as number,
		artifact: proposal.artifact,
		missedCriterionIds,
	};
	return {
		state: {
			lineage: [...state.lineage, champion],
			championId: proposal.id,
			opponents: ravoPressure(state.opponents, missedCriterionIds),
			evaluatedProposalIds: evaluatedState.evaluatedProposalIds,
		},
		certificate,
	};
}

export const ravoCoStep = ravoStep;
