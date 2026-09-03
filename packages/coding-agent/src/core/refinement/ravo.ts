import type { Model } from "@earendil-works/pi-ai";
import { completeSimple } from "@earendil-works/pi-ai";
import { type AssistedRavoAuthorization, authorizeAssistedRavo } from "../ravo/authority.js";
import type { JsonValue } from "../ravo/reducer.js";
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
 *
 * Divergence from the verified spec, stated honestly: the archive gate below
 * allows `deepTolerance` slack under the best recorded score, because judge
 * scores are noisy and a strict ratchet provably starves the loop (the flaw
 * the Rocq v2 development documents). Lineage monotonicity is unaffected —
 * the lineage is append-only and `bestScore` is a running max.
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

export const RAVO_DEFAULT_CONFIG: RavoConfig = {
	screenThreshold: 50,
	epsilon: 1,
	deepTolerance: 10,
};

/** Seed opponent pool: the policy the refinement system prompt demands. */
export const RAVO_SEED_CRITERIA: RavoCriterion[] = [
	{ id: "evidence", weight: 1, description: "Every edit is backed by concrete trajectory evidence." },
	{ id: "scope", weight: 1, description: "Edits match the requested scope (local vs global) policy." },
	{ id: "minimality", weight: 1, description: "Edits touch the smallest relevant components; no sprawling rewrites." },
	{ id: "contracts", weight: 1, description: "Skill edits carry a valid python reference and arguments contract." },
	{ id: "novelty", weight: 1, description: "Edits do not duplicate or overlap existing harness entries." },
];

export function emptyRavoState(): RavoHarnessState {
	return { lineage: [], evaluator: { criteria: RAVO_SEED_CRITERIA.map((c) => ({ ...c })) } };
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

export type RavoDecision = "commit" | "reject_screen" | "reject_deep" | "reject_criteria";

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
	evaluation: { fastScore: number; deepScore: number; missedCriteria: string[] },
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

Return JSON only:
{
  "score": 0-100,
  "failedCriteria": ["criterion ids that the proposal fails"],
  "rationale": "one or two sentences"
}`;

const RAVO_JUDGE_MAX_OUTPUT_TOKENS = 2_048;

function extractJudgeJson(text: string): { score: number; failedCriteria: string[]; rationale: string } {
	const trimmed = text.trim();
	const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/);
	const candidate = fenced ? fenced[1].trim() : trimmed;
	const start = candidate.indexOf("{");
	const end = candidate.lastIndexOf("}");
	const parsed: unknown = JSON.parse(start !== -1 && end > start ? candidate.slice(start, end + 1) : candidate);
	const record = typeof parsed === "object" && parsed !== null ? (parsed as Record<string, unknown>) : {};
	const rawScore = typeof record.score === "number" ? record.score : Number(record.score);
	const score = Number.isFinite(rawScore) ? Math.min(100, Math.max(0, Math.round(rawScore))) : 0;
	const failedCriteria = Array.isArray(record.failedCriteria)
		? record.failedCriteria.filter((id): id is string => typeof id === "string")
		: [];
	return {
		score,
		failedCriteria,
		rationale: typeof record.rationale === "string" ? record.rationale : "",
	};
}

/**
 * Deep evaluation: one judge call scoring the candidate against the evaluator
 * criteria. Combines with `ravoDecide` into the full gate. Fails open on
 * judge errors (recorded in the report): the gate must not make refinement
 * less available than the ungated baseline when the judge is down.
 */
export async function ravoEvaluateProposal(
	proposal: RefinementProposal,
	options: {
		state: RavoHarnessState;
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
	},
): Promise<RavoGateReport> {
	const { state, config } = options;
	const fastScore = ravoFastScreen(proposal, options.validEdits);
	const bestScore = ravoBestScore(state.lineage);
	const base = {
		fastScore,
		bestScore,
		epsilon: config.epsilon,
		screenThreshold: config.screenThreshold,
		deepTolerance: config.deepTolerance,
	};
	if (fastScore < config.screenThreshold) {
		const rationale = `structural screen scored ${fastScore} below threshold ${config.screenThreshold}`;
		const authorization = authorizeAssistedRavo({
			proposalId: options.proposalId,
			artifact: proposal as unknown as JsonValue,
			baseline: options.baseline,
			fastScore,
			observation: { status: "abstain", detail: rationale },
			screenThreshold: config.screenThreshold,
			epsilon: config.epsilon,
		});
		return {
			...base,
			decision: "reject_screen",
			deepScore: 0,
			missedCriteria: [],
			missedWeight: 0,
			rationale,
			authorization,
		};
	}

	let deepScore = bestScore;
	let missedCriteria: string[] = [];
	let rationale = "";
	let judgeError: string | undefined;
	try {
		const criteriaText = state.evaluator.criteria
			.map((c) => `- ${c.id} (weight ${c.weight}): ${c.description}`)
			.join("\n");
		const userPrompt = [
			`<criteria>\n${criteriaText}\n</criteria>`,
			`<current_harness_state>\n${options.harnessOverview}\n</current_harness_state>`,
			`<proposal>\n${JSON.stringify(proposal, null, 2)}\n</proposal>`,
			`<conversation>\n${options.conversationText}\n</conversation>`,
			"Score the proposal and list any failed criterion ids. Return JSON only.",
		].join("\n\n");
		const response = await completeSimple(
			options.model,
			{
				systemPrompt: RAVO_JUDGE_SYSTEM_PROMPT,
				messages: [{ role: "user", content: [{ type: "text", text: userPrompt }], timestamp: Date.now() }],
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
		deepScore = judged.score;
		missedCriteria = judged.failedCriteria;
		rationale = judged.rationale;
	} catch (error) {
		judgeError = error instanceof Error ? error.message : String(error);
		rationale = `deep judge unavailable (${judgeError}); no harness edits were authorized; retry /refine when evaluation is available`;
	}

	const authorization = authorizeAssistedRavo({
		proposalId: options.proposalId,
		artifact: proposal as unknown as JsonValue,
		baseline: options.baseline,
		fastScore,
		observation: judgeError
			? { status: "error", detail: rationale }
			: { status: "pass", score: deepScore, detail: rationale, failedCriteria: missedCriteria },
		screenThreshold: config.screenThreshold,
		epsilon: config.epsilon,
	});
	const decision: RavoDecision = authorization.authorized
		? "commit"
		: authorization.certificate.rejection === "screen"
			? "reject_screen"
			: authorization.certificate.rejection === "opponents"
				? "reject_criteria"
				: "reject_deep";
	return {
		...base,
		decision,
		deepScore,
		missedCriteria,
		missedWeight: ravoMissedWeight(state.evaluator, missedCriteria),
		rationale,
		judgeError,
		authorization,
	};
}

/** Whether RAVO gating is enabled (default on; disable with PRIME_AGENT_RAVO=0). */
export function ravoEnabled(env: Record<string, string | undefined> = process.env): boolean {
	const value = env.PRIME_AGENT_RAVO?.trim().toLowerCase();
	return !(value === "0" || value === "off" || value === "false");
}
