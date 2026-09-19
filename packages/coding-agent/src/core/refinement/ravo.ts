import type { Model } from "@earendil-works/pi-ai";
import { completeSimple, getLogger } from "@earendil-works/pi-ai";
import { REFINEMENT_COMMITTED_MSG, REFINEMENT_LOG_COMPONENT } from "../learning-index.js";
import {
	type AssistedRavoAuthorization,
	authorizeAssistedRavo,
	DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
	failureOpponentFingerprint,
	isFailureOpponentId,
} from "../ravo/authority.js";
import { type FailureRecord, failureOpponentId, formatFailureLedgerForPrompt } from "../ravo/failure-ledger.js";
import { type JsonValue, type RavoState, type RavoWindowClock, ravoExtendOpponents } from "../ravo/reducer.js";
import {
	isRefereeOpponentId,
	type RefereeVerdict,
	type RefereeVerdictStatus,
	refereeOpponentId,
	refereeVerdictIsEvidence,
	skillImportsOf,
} from "../ravo/referee.js";
import { adjudicateFailureClaims } from "../ravo/referee-runner.js";
import { toolforgeSrcRoots } from "../toolforge/ledger.js";
import type { RefineEvidenceDriftKind } from "./evidence-drift.js";
import type { RefinementProposal } from "./refinement.js";

/**
 * RAVO — Recursive Agentic Variation with co-evolving Opponents — applied to
 * continual-harness refinement.
 *
 * The refinement loop is an evolutionary search whose evolving artifact is the
 * harness state: the /refine model proposes a candidate edit set (the agentic
 * variation operator), and a gate architecture decides commits. The pure core
 * below mirrors a machine-checked formalization (Lean 4 `AvoRlm.Ravo`, Rocq
 * `Ravo.v`); each function cites the theorem that pins down its behavior:
 *
 * - `bestScore` / lineage: Lean Def 1.1. Append-only, so the best recorded
 *   deep score is monotone at any horizon (Lean `ravo_run_invariants` (1)).
 * - fast screen + deep gate: noise in the cheap screen can cause false
 *   rejections, never false commits (Lean `bestScore_screenedStep`).
 * - criteria pool = the "opponents": the epsilon-tolerant weighted champion
 *   gate (Rocq Def 3.3) commits only candidates whose failed criteria carry
 *   at most `epsilon` total weight (Rocq Thm 3.10, epsilon-succession).
 * - weakness pressure: doubling a failed criterion's weight preserves support
 *   (Rocq Prop 4.2 / Lean `pressure_support`), strictly increases its share
 *   (Rocq Prop 4.3), and only tightens future gates (Rocq Thm 7.5: weights
 *   only grow, so the seed-weight succession chain survives reweighting).
 * - pool extension: recurring failure fingerprints join the pool as opponents
 *   (`ravoExtendOpponents`). Adding an opponent can only raise missedWeight
 *   (`missedWeight_app`), so extension only tightens the gate — a candidate
 *   that ignores a recurring failure is charged its weight, never excused.
 * - the referee (Rocq `Ravo.v` Section 16): a claimed failure fingerprint whose
 *   verified missing-module or missing-distribution replay cases name what a
 *   skill of the proposal imports is adjudicated by re-executing those cases in
 *   a subprocess, not by reading the claim. `FlawUpheld`
 *   iff the recorded exception recurs (Thm 16.2); prose is not evidence
 *   (Thm 16.3); a fingerprint no replay can speak to is never upheld
 *   (`no_test_no_flaw`) and falls back to the claim. The referee is one more
 *   opponent, so it can only tighten.
 * - provisional commit: a champion that claims to address recurring failures
 *   stays provisional for an observation window; a claimed fingerprint that
 *   recurs inside the window is a measured fault (judge said pass, outcome
 *   said fail) and feeds a gated repair (`ravoObserveChampion`). The fault
 *   never bypasses the gate: the repair proposal is scored like any other.
 * - measurability: a commit that claims no fingerprint cannot be confirmed or
 *   refuted by anything later, so it applies its edits but leaves the RAVO
 *   state untouched. A failure-triggered refine that claims nothing is
 *   rejected outright (`reject_unclaimed`).
 *
 * Divergence from the verified spec, stated honestly: the deep gate (here and
 * in the generic reducer via `RavoConfig.deepTolerance`) allows
 * `deepTolerance` slack under the best recorded score, because judge scores
 * are noisy and a strict ratchet provably starves the loop (the flaw the Rocq
 * v2 development documents). Lineage monotonicity is unaffected — the lineage
 * is append-only, champion scores are recorded unslacked, and `bestScore` is
 * a running max.
 *
 * Implementation note (from the spec): the proposal is obtained ONCE per
 * iteration and threaded through all gates — an LLM session is not a pure
 * function and must not be re-invoked per gate.
 */

/** One committed candidate in the RAVO lineage (Lean Def 1.1). */
export interface RavoLineageEntry {
	id: string;
	score: number;
	summary: string;
	missedCriteria: string[];
	created_at: string;
}

/** An opponent in the evaluator pool: a judged criterion with a weight. */
export interface RavoCriterion {
	id: string;
	weight: number;
	description: string;
}

/** Co-evolving evaluator state persisted inside the harness state. */
export interface RavoEvaluator {
	criteria: RavoCriterion[];
}

/** RAVO state carried by the harness state file. */
export interface RavoHarnessState {
	lineage: RavoLineageEntry[];
	evaluator: RavoEvaluator;
}

export interface RavoConfig {
	/** Fast-screen threshold tau on the structural score in [0, 100]. */
	screenThreshold: number;
	/** Epsilon: total criterion weight a candidate may miss and still commit. */
	epsilon: number;
	/** Slack under bestScore tolerated by the deep gate (see header note). */
	deepTolerance: number;
}

const refinementLog = getLogger(REFINEMENT_LOG_COMPONENT);

export const REFINEMENT_REJECTED_MSG = "refinement.rejected";
export const REFINEMENT_APPLIED_UNMEASURED_MSG = "refinement.applied_unmeasured";

/** Why a refine ran. */
export type RefineReason =
	| "manual"
	| "refine_run"
	| "recurrence"
	| "regression"
	| "turn_interval"
	| "compact"
	| "rollback"
	| "ravo_run";

/**
 * What a refine is for. A `failure` refine exists to stop recorded failures and
 * must claim one; a `checkpoint` is periodic housekeeping; a `directed` refine
 * does what it was asked.
 */
export type RefineKind = "directed" | "checkpoint" | "failure";

export function refineKindOf(reason: RefineReason): RefineKind {
	switch (reason) {
		case "turn_interval":
		case "compact":
			return "checkpoint";
		case "recurrence":
		case "regression":
			return "failure";
		case "manual":
		case "refine_run":
		case "rollback":
		case "ravo_run":
			return "directed";
	}
}

/**
 * The decision a refinement actually ended with, after apply-time checks. It
 * differs from the gate's `RavoDecision`: a gate commit can still apply
 * unmeasured, fail to apply, or be a rollback that never met the gate.
 */
export type RefineFinalDecision =
	| "commit"
	| "commit_unmeasured"
	| "reject_screen"
	| "reject_deep"
	| "reject_criteria"
	| "reject_unclaimed"
	| "partial"
	| "rollback"
	| "no_edits";

/**
 * Why a proposal was rejected. Only `gate` and `stale_evidence` mean the judge
 * decided on the edit: `screen` is the structural screen or skill dry-run,
 * `judge_unavailable` never reached the judge, and `baseline_changed` is a gate
 * approval that no longer held when it applied.
 */
export type RefinementRejectionCause = "gate" | "screen" | "judge_unavailable" | "baseline_changed" | "stale_evidence";

export interface RefinementOutcomeLog {
	proposalId: string;
	decision: RefineFinalDecision;
	addressed: readonly string[];
	deepScore: number;
	missed: number;
	claimed: number;
	reason: RefineReason;
	scope: "local" | "global";
	/** Logged on a `reject_*` decision only. */
	cause?: RefinementRejectionCause;
	/** A judge rejection made after the conversation moved while it was planned; see `isStaleEvidenceRejection`. */
	staleEvidence?: boolean;
	driftKind?: RefineEvidenceDriftKind;
	/** Messages the judge read that the proposer did not. */
	driftMessages?: number;
	/** Whether the stale rejection leaves its round open for one re-plan. */
	replanScheduled?: boolean;
	/** The stale-evidence rejection this refinement re-planned. */
	replanOf?: string;
}

/**
 * Emit exactly one structured line for a refinement's final decision. Call it
 * at apply time, never at gate time: the gate's commit can still be downgraded.
 *
 * - `refinement.committed` only for a measurable commit (`commit` with a
 *   non-empty `addressed`). The learning index treats these as its treated
 *   cohort, so an unmeasured commit must never appear here.
 * - `refinement.applied_unmeasured` for edits that applied without a claim:
 *   `commit_unmeasured`, a `commit` with nothing addressed, and `rollback`.
 * - `refinement.rejected` for everything that applied no edits, carrying
 *   `cause` on a `reject_*` decision when the caller classified one, and the
 *   drift and re-plan fields on a stale-evidence rejection.
 *
 * Every message carries `replanOf` when the refinement re-planned a
 * stale-evidence rejection.
 */
export function logRefinementOutcome(input: RefinementOutcomeLog): void {
	const { proposalId, decision, deepScore, reason, scope } = input;
	const replan = input.replanOf === undefined ? {} : { replanOf: input.replanOf };
	if (decision === "commit" && input.addressed.length > 0) {
		refinementLog.info(REFINEMENT_COMMITTED_MSG, {
			proposalId,
			addressed: [...input.addressed],
			deepScore,
			missed: input.missed,
			reason,
			scope,
			...replan,
		});
		return;
	}
	if (decision === "commit" || decision === "commit_unmeasured" || decision === "rollback") {
		refinementLog.info(REFINEMENT_APPLIED_UNMEASURED_MSG, { proposalId, deepScore, reason, scope, ...replan });
		return;
	}
	refinementLog.info(REFINEMENT_REJECTED_MSG, {
		proposalId,
		decision,
		deepScore,
		missed: input.missed,
		claimed: input.claimed,
		reason,
		scope,
		...(input.cause === undefined || !decision.startsWith("reject_") ? {} : { cause: input.cause }),
		...(input.staleEvidence === true
			? {
					staleEvidence: true,
					driftKind: input.driftKind,
					driftMessages: input.driftMessages,
					replanScheduled: input.replanScheduled === true,
				}
			: {}),
		...replan,
	});
}

export const RAVO_DEFAULT_CONFIG: RavoConfig = {
	screenThreshold: 50,
	epsilon: 1,
	deepTolerance: 10,
};

/** Seed opponent pool: the policy the refinement system prompt demands. */
export const RAVO_SEED_CRITERIA: RavoCriterion[] = [
	{
		id: "evidence",
		weight: 1,
		description: "Every edit is backed by concrete trajectory evidence.",
	},
	{
		id: "scope",
		weight: 1,
		description: "Edits match the requested scope (local vs global) policy.",
	},
	{
		id: "minimality",
		weight: 1,
		description: "Edits touch the smallest relevant components; no sprawling rewrites.",
	},
	{
		id: "contracts",
		weight: 1,
		description: "Skill edits carry a valid python reference and arguments contract.",
	},
	{
		id: "novelty",
		weight: 1,
		description: "Edits do not duplicate or overlap existing harness entries.",
	},
];

export function emptyRavoState(): RavoHarnessState {
	return {
		lineage: [],
		evaluator: { criteria: RAVO_SEED_CRITERIA.map((c) => ({ ...c })) },
	};
}

/** Lean Def 1.1: best recorded deep score (0 for the empty lineage). */
export function ravoBestScore(lineage: readonly RavoLineageEntry[]): number {
	return lineage.reduce((acc, entry) => Math.max(acc, entry.score), 0);
}

/** Rocq Def 3.2: total weight of the criteria a candidate failed. */
export function ravoMissedWeight(evaluator: RavoEvaluator, missedCriteria: readonly string[]): number {
	const missed = new Set(missedCriteria);
	return evaluator.criteria.reduce((acc, c) => acc + (missed.has(c.id) ? c.weight : 0), 0);
}

/** Rocq Def 3.3: the epsilon-tolerant weighted champion gate. */
export function ravoClears(evaluator: RavoEvaluator, missedCriteria: readonly string[], epsilon: number): boolean {
	return ravoMissedWeight(evaluator, missedCriteria) <= epsilon;
}

/**
 * Rocq Def 7.2 (pressureW): double the weight of the criterion the committed
 * champion was weakest on. Weights only grow (Rocq Thm 7.5), and no criterion
 * is ever silenced (Rocq Prop 4.2).
 */
export function ravoPressure(evaluator: RavoEvaluator, weakId: string): RavoEvaluator {
	return {
		criteria: evaluator.criteria.map((c) => (c.id === weakId ? { ...c, weight: 2 * c.weight } : c)),
	};
}

export type RavoDecision = "commit" | "reject_screen" | "reject_deep" | "reject_criteria" | "reject_unclaimed";

export type RefereeCounts = Record<RefereeVerdictStatus, number>;

function emptyRefereeCounts(): RefereeCounts {
	return { cleared: 0, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 0 };
}

/** The gate report attached to a refinement plan and result. */
export interface RavoGateReport {
	decision: RavoDecision;
	fastScore: number;
	deepScore: number;
	bestScore: number;
	missedCriteria: string[];
	missedWeight: number;
	epsilon: number;
	screenThreshold: number;
	deepTolerance: number;
	rationale: string;
	/** Set when the deep judge call failed; evaluation then fails closed. */
	judgeError?: string;
	/** Generic-core authority decision, bound to the proposal and baseline. */
	authorization?: AssistedRavoAuthorization;
	/** Failure fingerprint ids the judge accepted the proposal as addressing. */
	addressedFingerprints: string[];
	/** Failure opponent criterion ids (`failure:<fingerprint>`) in this gate. */
	failureOpponents: string[];
	/** Referee verdicts on the claimed fingerprints, one per claimed fingerprint. */
	refereeVerdicts?: RefereeVerdict[];
	/**
	 * Whether the RAVO state learns from this decision: a commit that claims at
	 * least one fingerprint. An unmeasured commit applies its edits, but its
	 * `authorization.nextState` is the input state.
	 */
	measurable: boolean;
	refereeCounts: RefereeCounts;
}

/**
 * The pure RAVO decision (Lean `ravoStep` gate structure): fast screen, then
 * deep gate against the lineage bar, then the epsilon criteria gate. All three
 * must pass. Pure and deterministic given the scores — the verified safety
 * properties are properties of this gate architecture, not of the judge.
 */
export function ravoDecide(
	state: RavoHarnessState,
	config: RavoConfig,
	evaluation: {
		fastScore: number;
		deepScore: number;
		missedCriteria: string[];
	},
): RavoDecision {
	if (evaluation.fastScore < config.screenThreshold) {
		return "reject_screen";
	}
	const bar = ravoBestScore(state.lineage);
	if (evaluation.deepScore + config.deepTolerance < bar) {
		return "reject_deep";
	}
	if (!ravoClears(state.evaluator, evaluation.missedCriteria, config.epsilon)) {
		return "reject_criteria";
	}
	return "commit";
}

/**
 * Commit a champion: append to the lineage (append-only, so `ravoBestScore`
 * is monotone — Lean `ravo_run_invariants` (1)) and apply weakness pressure
 * to the criterion the champion was weakest on, if any (Rocq Thm 7.5).
 */
export function ravoCommit(
	state: RavoHarnessState,
	champion: { id: string; summary: string },
	report: RavoGateReport,
): RavoHarnessState {
	const entry: RavoLineageEntry = {
		id: champion.id,
		score: report.deepScore,
		summary: champion.summary,
		missedCriteria: report.missedCriteria,
		created_at: new Date().toISOString(),
	};
	const weakId = report.missedCriteria[0];
	return {
		lineage: [...state.lineage, entry],
		evaluator: weakId ? ravoPressure(state.evaluator, weakId) : state.evaluator,
	};
}

/**
 * Fast screen (structural, deterministic, no LLM): fraction of edits that are
 * well-formed, in [0, 100]. An empty proposal screens at 0 — nothing to
 * commit. Cheap and noisy by design; by Lean `bestScore_screenedStep` its
 * noise can only cause false rejections, never false commits.
 */
export function ravoFastScreen(proposal: RefinementProposal, validEdits: number): number {
	if (proposal.edits.length === 0) {
		return 0;
	}
	return Math.round((100 * validEdits) / proposal.edits.length);
}

const RAVO_JUDGE_SYSTEM_PROMPT = `You are the RAVO deep evaluator for Prime Agent's /refine subsystem.

Score a proposed continual-harness refinement against the trajectory evidence.
Judge the QUALITY OF THE RESULTING HARNESS STATE, not prose style.

When <recurring_failures> is present, each listed failure is an opponent
criterion (id "failure:<fingerprint>"). The proposal ADDRESSES a fingerprint
only if its edits would plausibly prevent that exact failure from recurring
(a memory, prompt note, skill fix, or subagent change that targets its cause).
List the fingerprint ids the proposal genuinely addresses in
"addressedFingerprints"; a fingerprint not listed there counts as a missed
opponent. Never list a fingerprint the proposal merely mentions. A fingerprint
marked replay=verified is re-executed after you answer when a skill the proposal
writes imports the module its replay case probes, so listing one whose failure
has not actually stopped costs the proposal the gate. Do not list a fingerprint
whose failure is outside the harness's control (a provider outage, a user
denial, a flaky network).

"verdict" is your own decision on the deep gate: "pass" if this candidate is at
least as good a harness state as the current champion, "fail" if it is worse,
"abstain" if you cannot tell from the evidence given. It is not a summary of
"score"; a non-pass verdict rejects the candidate regardless of the number.

Return JSON only:
{
  "verdict": "pass" | "fail" | "abstain",
  "score": 0-100,
  "failedCriteria": ["criterion ids that the proposal fails"],
  "addressedFingerprints": ["recurring failure fingerprint ids the proposal addresses"],
  "rationale": "one or two sentences"
}`;

const RAVO_JUDGE_MAX_OUTPUT_TOKENS = 2_048;

function stringList(value: unknown): string[] {
	return Array.isArray(value) ? value.filter((id): id is string => typeof id === "string") : [];
}

/** The judge's own decision on the deep gate. Only an explicit pass token passes. */
export type JudgeDeepVerdict = "pass" | "fail" | "abstain";

export function parseJudgeVerdict(value: unknown): JudgeDeepVerdict {
	const text = typeof value === "string" ? value.trim().toLowerCase() : "";
	if (text === "pass" || text === "accept" || text === "passed" || text === "true") return "pass";
	if (text === "fail" || text === "reject" || text === "false") return "fail";
	// Absence is not consent. This used to return "pass", so a judge that omitted
	// the field -- the behaviour of any model that drops one clause of a long
	// prompt -- silently authorized the candidate, and the deep gate could not
	// fail in the one direction that matters. "abstain" propagates through
	// authorizeAssistedRavo as a conservative miss on every criterion.
	return "abstain";
}

function extractJudgeJson(text: string): {
	verdict: JudgeDeepVerdict;
	score: number;
	failedCriteria: string[];
	addressedFingerprints: string[];
	rationale: string;
} {
	const trimmed = text.trim();
	const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/);
	const candidate = fenced ? fenced[1].trim() : trimmed;
	const start = candidate.indexOf("{");
	const end = candidate.lastIndexOf("}");
	const parsed: unknown = JSON.parse(start !== -1 && end > start ? candidate.slice(start, end + 1) : candidate);
	const record = typeof parsed === "object" && parsed !== null ? (parsed as Record<string, unknown>) : {};
	const rawScore = typeof record.score === "number" ? record.score : Number(record.score);
	const score = Number.isFinite(rawScore) ? Math.min(100, Math.max(0, Math.round(rawScore))) : 0;
	return {
		verdict: parseJudgeVerdict(record.verdict ?? record.status),
		score,
		failedCriteria: stringList(record.failedCriteria),
		addressedFingerprints: stringList(record.addressedFingerprints),
		rationale: typeof record.rationale === "string" ? record.rationale : "",
	};
}

/**
 * Deep evaluation: one judge call scoring the candidate against the evaluator
 * criteria, then the referee re-running the applicable replay cases of the
 * fingerprints the judge accepted as addressed. The generic authority
 * (`authorizeAssistedRavo`) makes the decision. Judge errors are recorded in
 * the report and fail closed: an unevaluated proposal is never authorized, so
 * no harness edits apply until a retried /refine reaches the judge. The judge's
 * own `verdict` drives the deep gate, so a judge that says fail rejects the
 * candidate whatever it scored.
 *
 * `recurringFailures` become failure opponents in the pool; the judge must
 * name the fingerprints the proposal addresses, and an unaddressed recurring
 * failure is charged its opponent weight in the epsilon gate. A commit is
 * provisional for `observationWindowTurns` from `turn` (default 20), measured
 * on `turnClock`.
 *
 * `refineKind` decides what a claimless result means. For `failure`, a judged
 * proposal that addresses no fingerprint is `reject_unclaimed`. For any other
 * kind a claimless commit is authorized but unmeasured (`measurable: false`,
 * `authorization.nextState` is `state` unchanged).
 *
 * Nothing is logged here; the apply phase reports the final decision through
 * `logRefinementOutcome`.
 */
export async function ravoEvaluateProposal(
	proposal: RefinementProposal,
	options: {
		state: RavoState<JsonValue>;
		config: RavoConfig;
		validEdits: number;
		conversationText: string;
		harnessOverview: string;
		baseline: JsonValue;
		proposalId: string;
		model: Model<any>;
		apiKey: string;
		headers?: Record<string, string>;
		signal?: AbortSignal;
		recurringFailures?: readonly FailureRecord[];
		turn?: number;
		/** Clock `turn` is read off: `"ordinal"` for the global ledger's, `"local-ordinal"` for a session ledger's. */
		turnClock?: RavoWindowClock;
		observationWindowTurns?: number;
		refineKind?: RefineKind;
	},
): Promise<RavoGateReport> {
	const { state, config } = options;
	const refineKind = options.refineKind ?? "directed";
	const recurringFailures = options.recurringFailures ?? [];
	const failureOpponents = [...new Set(recurringFailures.map((record) => failureOpponentId(record.fingerprint)))];
	const observationWindowTurns = options.observationWindowTurns ?? DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS;
	const fastScore = ravoFastScreen(proposal, options.validEdits);
	const bestScore = state.lineage.reduce((best, entry) => Math.max(best, entry.score), 0);
	const base = {
		fastScore,
		bestScore,
		epsilon: config.epsilon,
		screenThreshold: config.screenThreshold,
		deepTolerance: config.deepTolerance,
		failureOpponents,
	};
	const authorityInput = {
		proposalId: options.proposalId,
		artifact: proposal as unknown as JsonValue,
		baseline: options.baseline,
		fastScore,
		state,
		screenThreshold: config.screenThreshold,
		epsilon: config.epsilon,
		deepTolerance: config.deepTolerance,
		failureOpponents,
		turn: options.turn,
		...(options.turnClock === undefined ? {} : { turnClock: options.turnClock }),
		observationWindowTurns,
		unclaimedCommit: refineKind === "failure" ? ("reject" as const) : ("unmeasured" as const),
	};
	if (fastScore < config.screenThreshold) {
		const rationale = `structural screen scored ${fastScore} below threshold ${config.screenThreshold}`;
		const authorization = authorizeAssistedRavo({
			...authorityInput,
			observation: { status: "abstain", detail: rationale },
		});
		return {
			...base,
			decision: "reject_screen",
			deepScore: 0,
			missedCriteria: [],
			missedWeight: 0,
			addressedFingerprints: [],
			rationale,
			authorization,
			measurable: false,
			refereeCounts: emptyRefereeCounts(),
		};
	}

	let deepScore = bestScore;
	let deepVerdict: JudgeDeepVerdict = "pass";
	let missedCriteria: string[] = [];
	let addressedFingerprints: string[] = [];
	let rationale = "";
	let judgeError: string | undefined;
	try {
		const descriptions = new Map(RAVO_SEED_CRITERIA.map((criterion) => [criterion.id, criterion.description]));
		const failureDescriptions = new Map(
			recurringFailures.map((record) => [
				failureOpponentId(record.fingerprint),
				`Recurring ${record.fingerprint.kind} (${record.count}x): ${record.fingerprint.message}`,
			]),
		);
		const judgedPool = ravoExtendOpponents(state.opponents, failureOpponents);
		// Referee criteria are mechanical: the judge cannot influence them and is
		// not invited to opine on them.
		const criteriaText = judgedPool.criteria
			.filter((criterion) => !isRefereeOpponentId(criterion.id))
			.filter((criterion) => !isFailureOpponentId(criterion.id) || failureDescriptions.has(criterion.id))
			.map(
				(criterion) =>
					`- ${criterion.id} (weight ${criterion.currentWeight}): ${descriptions.get(criterion.id) ?? failureDescriptions.get(criterion.id) ?? criterion.id}`,
			)
			.join("\n");
		const userPrompt = [
			`<criteria>\n${criteriaText}\n</criteria>`,
			...(recurringFailures.length > 0
				? [
						`<recurring_failures>\n${formatFailureLedgerForPrompt(recurringFailures)}\n</recurring_failures>`,
						`The proposal must address these recurring failures. Return the fingerprint ids it addresses in "addressedFingerprints" (candidates: ${recurringFailures.map((record) => record.fingerprint.id).join(", ")}).`,
					]
				: []),
			`<current_harness_state>\n${options.harnessOverview}\n</current_harness_state>`,
			`<proposal>\n${JSON.stringify(proposal, null, 2)}\n</proposal>`,
			`<conversation>\n${options.conversationText}\n</conversation>`,
			"Score the proposal and list any failed criterion ids. Return JSON only.",
		].join("\n\n");
		const response = await completeSimple(
			options.model,
			{
				systemPrompt: RAVO_JUDGE_SYSTEM_PROMPT,
				messages: [
					{
						role: "user",
						content: [{ type: "text", text: userPrompt }],
						timestamp: Date.now(),
					},
				],
			},
			{
				maxTokens: Math.min(options.model.maxTokens, RAVO_JUDGE_MAX_OUTPUT_TOKENS),
				signal: options.signal,
				apiKey: options.apiKey,
				headers: options.headers,
			},
		);
		if (response.stopReason === "error") {
			throw new Error(response.errorMessage || "judge call failed");
		}
		const text = response.content
			.filter((content): content is { type: "text"; text: string } => content.type === "text")
			.map((content) => content.text)
			.join("\n");
		const judged = extractJudgeJson(text);
		const knownFingerprints = new Set(recurringFailures.map((record) => record.fingerprint.id));
		deepScore = judged.score;
		deepVerdict = judged.verdict;
		missedCriteria = judged.failedCriteria;
		addressedFingerprints = judged.addressedFingerprints.filter((id) => knownFingerprints.has(id));
		rationale = judged.rationale;
	} catch (error) {
		judgeError = error instanceof Error ? error.message : String(error);
		rationale = `deep judge unavailable (${judgeError}); no harness edits were authorized; retry /refine when evaluation is available`;
	}

	// The referee re-executes the verified replay cases of the fingerprints the
	// judge accepted as addressed, where a skill the proposal writes imports what
	// a case probes, with the toolforge roots the fast screen imports from. It is
	// the only input to this gate the proposal did not write, and it runs before
	// the authority so a refuted claim is charged as a missed opponent rather
	// than believed.
	const refereeVerdicts = judgeError
		? []
		: await adjudicateFailureClaims(recurringFailures, addressedFingerprints, {
				...(options.signal ? { signal: options.signal } : {}),
				skillImports: skillImportsOf(proposal.edits),
				sysPath: toolforgeSrcRoots(),
			});
	const authorization = authorizeAssistedRavo({
		...authorityInput,
		refereeVerdicts,
		observation: judgeError
			? { status: "error", detail: rationale }
			: {
					status: deepVerdict,
					score: deepScore,
					detail: rationale,
					failedCriteria: missedCriteria,
					addressedFingerprints,
				},
	});
	// A failure-triggered refine exists to stop the listed failures; judged as
	// addressing none of them, whatever else the gate said, it is unclaimed.
	const unclaimed = refineKind === "failure" && !judgeError && addressedFingerprints.length === 0;
	const decision: RavoDecision = unclaimed
		? "reject_unclaimed"
		: authorization.authorized
			? "commit"
			: authorization.certificate.rejection === "screen"
				? "reject_screen"
				: authorization.certificate.rejection === "opponents"
					? "reject_criteria"
					: "reject_deep";
	// The certificate's missed set already charges unaddressed failure
	// opponents; it is empty when the step never reached the opponents gate,
	// so fall back to the judge's list plus the unaddressed fingerprints.
	const unaddressed = failureOpponents.filter((id) => {
		const fingerprint = failureOpponentFingerprint(id);
		return fingerprint !== undefined && !addressedFingerprints.includes(fingerprint);
	});
	const missed =
		authorization.certificate.criteria.length > 0
			? authorization.certificate.missedCriterionIds
			: [...new Set([...missedCriteria, ...unaddressed])];
	const pool = ravoExtendOpponents(state.opponents, [
		...failureOpponents,
		...refereeVerdicts.filter(refereeVerdictIsEvidence).map((verdict) => refereeOpponentId(verdict.fingerprintId)),
	]);
	const refereeCounts = emptyRefereeCounts();
	for (const verdict of refereeVerdicts) refereeCounts[verdict.status] += 1;
	return {
		...base,
		decision,
		deepScore,
		missedCriteria: missed,
		missedWeight: pool.criteria.reduce(
			(weight, criterion) => weight + (missed.includes(criterion.id) ? criterion.currentWeight : 0),
			0,
		),
		addressedFingerprints,
		refereeVerdicts,
		rationale,
		judgeError,
		authorization,
		measurable: decision === "commit" && addressedFingerprints.length > 0,
		refereeCounts,
	};
}

/** Whether RAVO gating is enabled (default on; disable with PRIME_AGENT_RAVO=0). */
export function ravoEnabled(env: Record<string, string | undefined> = process.env): boolean {
	const value = env.PRIME_AGENT_RAVO?.trim().toLowerCase();
	return !(value === "0" || value === "off" || value === "false");
}
