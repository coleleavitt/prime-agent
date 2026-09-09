import type { RefinementEdit, RefinementProposal } from "../refinement/refinement.js";
import type { SkillCounterexampleRun, SkillModuleSource } from "../refinement/skill-dry-run.js";
import type { BoundedContextView, ContextViewItem } from "./context-view.js";
import type { ChildCall, ControllerProposal, EvaluationAdapter, RavoChildCallOptions } from "./controller.js";
import type { GateStatus, JsonValue } from "./reducer.js";
import type { ChildRuntimeScope, StructuredChildSpec } from "./runtime-adapter.js";

/**
 * Refereed opponent (after jfc's `validation.rs` refereed game): an opponent
 * whose evidence is EXECUTABLE rather than an opinion.
 *
 * For a proposal that creates or updates a skill, one sealed child (the
 * referee) writes a counter-example: a self-contained Python test that imports
 * the skill exactly as the fast-screen dry-run does and asserts the documented
 * contract on a concrete input. The test is then run in the kernel python by
 * `runSkillCounterexample`, and the verdict is mechanistic:
 *
 * - FlawUpheld  ⇔ the test FAILS (nonzero exit / assertion) ⇒ criterion "fail".
 * - NoFlawFound ⇔ no test was produced OR the test passes ⇒ criterion "pass".
 *   A referee that returns prose without an executable test contributes no
 *   evidence; the proposal is not charged for it.
 * - A proposal that touches no skill is out of the referee's jurisdiction.
 *   The reducer counts a literal "abstain" as a conservative miss (which
 *   would pressure the referee on every memory-only commit and then block
 *   them), so this is reported as "pass" with an `abstain:` detail, exactly
 *   like dormant failure opponents and the ARC not-applicable hygiene opponents.
 *
 * Sealing: the referee child receives only the proposal artifact, the skill
 * edits with their resolved module source, and the current task text. It never
 * sees the judge's output or any other opponent's verdict.
 */

export const REFEREE_CRITERION_ID = "referee:counterexample";
export const REFEREE_OPPONENT_ID = `opponent:${REFEREE_CRITERION_ID}`;
export const REFEREE_ROLE = "referee";

/** Characters of test source and stderr carried into the certificate detail. */
export const REFEREE_STDERR_DETAIL_CHARS = 400;
export const REFEREE_TEST_DETAIL_CHARS = 6_000;
const REFEREE_SOURCE_PROMPT_CHARS = 16_000;
const REFEREE_TASK_PROMPT_CHARS = 4_000;
const JSON_ONLY = "Return exactly one JSON object and nothing else: no prose, no code fences.";

export type RefereeVerdict = "flaw_upheld" | "no_flaw_found" | "not_applicable";

/** Structured output of the referee child. `flaw === null` means it found nothing to challenge. */
export interface RefereeChallenge {
	flaw: string | null;
	confidence: number;
	/** Self-contained Python test source; required whenever `flaw` is set. */
	test?: string;
	/** Index into `proposal.edits` of the skill edit the test targets; defaults to the first skill edit. */
	editIndex?: number;
}

export interface RefereeSkillTarget {
	editIndex: number;
	edit: RefinementEdit;
	source?: SkillModuleSource;
}

/** Everything the sealed referee child may see. */
export interface RefereeInput {
	proposal: ControllerProposal<JsonValue>;
	task: string;
	skills: readonly RefereeSkillTarget[];
}

export interface RefereeResult {
	verdict: RefereeVerdict;
	status: GateStatus;
	detail: string;
	challenge?: RefereeChallenge;
	run?: SkillCounterexampleRun;
	tokens: number;
}

export type RefereeRunner = (
	edit: RefinementEdit,
	testSource: string,
	options: { signal: AbortSignal },
) => Promise<SkillCounterexampleRun>;

export type RefereeSourceReader = (
	edit: RefinementEdit,
	options: { signal: AbortSignal },
) => Promise<SkillModuleSource>;

export interface RefereeOpponentDeps {
	/** The sealed referee child; run-service builds it with the same ChildCall plumbing as the judge. */
	challenge: ChildCall<RefereeInput, RefereeChallenge>;
	/** Executes the counter-example in the kernel (`runSkillCounterexample`). */
	run: RefereeRunner;
	/** Resolves the skill module source for the prompt (`readSkillModuleSource`); optional. */
	readSource?: RefereeSourceReader;
	/** Normalizes the proposal artifact; run-service passes `proposalOf`. */
	proposalOf: (artifact: JsonValue) => RefinementProposal;
}

export function isSkillWriteEdit(edit: RefinementEdit): boolean {
	return edit.kind === "skill" && (edit.action === "create" || edit.action === "update");
}

/** Skill edits the referee has jurisdiction over: create/update with a python reference object. */
export function refereeSkillEdits(proposal: RefinementProposal): { editIndex: number; edit: RefinementEdit }[] {
	return proposal.edits.flatMap((edit, editIndex) =>
		isSkillWriteEdit(edit) && edit.reference !== undefined && edit.reference.type === "python"
			? [{ editIndex, edit }]
			: [],
	);
}

function clip(text: string, maxChars: number): string {
	return text.length > maxChars ? `${text.slice(0, maxChars)}\n# ... clipped` : text;
}

/** Only the current task text crosses the seal; harness overview, ledger, and champion stay outside. */
export function refereeTaskText(context: BoundedContextView): string {
	const task = context.items.find((item: ContextViewItem) => item.kind === "current_task")?.content ?? "";
	return clip(task, REFEREE_TASK_PROMPT_CHARS);
}

export function refereePrompt(input: RefereeInput): string {
	const skills = input.skills.map(({ editIndex, edit, source }) => {
		const reference = edit.reference ?? {};
		const lines = [
			`<skill editIndex="${editIndex}" action="${edit.action}"${edit.id ? ` id="${edit.id}"` : ""}>`,
			`title: ${edit.title ?? ""}`,
			`reference: ${JSON.stringify(reference)}`,
			`arguments: ${JSON.stringify(edit.arguments ?? {})}`,
			`<documentation>\n${edit.content ?? ""}\n</documentation>`,
			source?.source !== undefined
				? `<module_source path="${source.origin ?? ""}"${source.truncated ? ' truncated="true"' : ""}>\n${clip(source.source, REFEREE_SOURCE_PROMPT_CHARS)}\n</module_source>`
				: `<module_source unavailable="true">${source?.detail ?? "not resolved"}</module_source>`,
			"</skill>",
		];
		return lines.join("\n");
	});
	return [
		`# RAVO ${REFEREE_ROLE}`,
		"You are the RAVO referee. Your only admissible evidence is an executable counter-example; prose is not evidence and is discarded. Everything you need is in this message. Do not search, browse, or call tools; write the answer directly.",
		`<task>\n${input.task}\n</task>`,
		`<proposal>\n${JSON.stringify(input.proposal.artifact)}\n</proposal>`,
		skills.join("\n\n"),
		[
			"Write ONE self-contained Python test that demonstrates a flaw in a skill above, if you can find one:",
			"- Import the skill exactly by its reference import (`import <reference.import>` or `from <reference.import> import <callable>`); sys.path is prepared the same way the kernel dry-run prepares it.",
			"- Call the documented callable on ONE concrete input drawn from the `arguments` contract and assert the documented result with plain `assert` statements (no pytest/unittest). Call async callables via `asyncio.run(...)`.",
			"- The test must FAIL (raise / exit nonzero) exactly when the flaw is real and PASS when the skill honors its documented contract. A test that fails for every implementation, needs the network, writes outside a temp dir, or runs longer than a few seconds is invalid.",
			"- If the documented contract is honored as far as you can test it, return no flaw.",
			'Return JSON: { "flaw": "one sentence naming the contract violation", "confidence": 0.0-1.0, "test": "python source", "editIndex": <skill editIndex> } or { "flaw": null }.',
			JSON_ONLY,
		].join("\n"),
	].join("\n\n");
}

/** Lenient on shape, strict on evidence: a claimed flaw without a test is invalid output (worth one retry). */
export function validateRefereeChallenge(value: unknown): RefereeChallenge {
	const record = objectRecord(value);
	const flaw = typeof record.flaw === "string" && record.flaw.trim() ? record.flaw.trim() : null;
	const rawConfidence = typeof record.confidence === "number" ? record.confidence : Number(record.confidence);
	const confidence = Number.isFinite(rawConfidence) ? Math.min(1, Math.max(0, rawConfidence)) : 0;
	const test = typeof record.test === "string" && record.test.trim() ? record.test : undefined;
	const editIndex =
		typeof record.editIndex === "number" && Number.isSafeInteger(record.editIndex) && record.editIndex >= 0
			? record.editIndex
			: undefined;
	if (flaw === null) return { flaw: null, confidence };
	if (test === undefined) throw new Error("referee flaw requires an executable test");
	return { flaw, confidence, test, ...(editIndex === undefined ? {} : { editIndex }) };
}

export function refereeSpec(scope: ChildRuntimeScope): StructuredChildSpec<RefereeInput, RefereeChallenge> {
	return { prompt: refereePrompt, validate: validateRefereeChallenge, scope };
}

function stderrExcerpt(run: SkillCounterexampleRun): string {
	const text = (run.stderr.trim() || run.stdout.trim()).slice(0, REFEREE_STDERR_DETAIL_CHARS);
	return text;
}

/**
 * The mechanistic adjudication. Pure given the child result and the run:
 * only a run whose `outcome` is "failed" upholds the flaw.
 */
export function adjudicateReferee(
	challenge: RefereeChallenge | undefined,
	run: SkillCounterexampleRun | undefined,
	tokens: number,
	noTestReason?: string,
): RefereeResult {
	if (!challenge || challenge.flaw === null || challenge.test === undefined || !run) {
		const reason =
			noTestReason ?? (challenge?.flaw === null ? "referee found no flaw" : "no executable counter-example");
		return {
			verdict: "no_flaw_found",
			status: "pass",
			detail: reason,
			...(challenge ? { challenge } : {}),
			tokens,
		};
	}
	const confidence = `confidence ${challenge.confidence.toFixed(2)}`;
	if (run.outcome === "failed") {
		const detail = [
			`flaw upheld (${confidence}): ${challenge.flaw}`,
			stderrExcerpt(run),
			`<counterexample>\n${clip(challenge.test, REFEREE_TEST_DETAIL_CHARS)}\n</counterexample>`,
		]
			.filter((part) => part.length > 0)
			.join("\n");
		return { verdict: "flaw_upheld", status: "fail", detail, challenge, run, tokens };
	}
	const detail =
		run.outcome === "passed"
			? `counter-example passed; claimed flaw not upheld (${confidence}): ${challenge.flaw}`
			: `no executable counter-example (${run.outcome}: ${run.detail}); claimed flaw not upheld: ${challenge.flaw}`;
	return { verdict: "no_flaw_found", status: "pass", detail, challenge, run, tokens };
}

/**
 * Evaluate one proposal end to end: jurisdiction, sealed challenge, kernel run,
 * adjudication. Child failures of status "error" (malformed output after the
 * retry, provider error) are no evidence and yield NoFlawFound; other
 * non-completed statuses (aborted, budget) propagate to the controller.
 */
export async function evaluateReferee(
	deps: RefereeOpponentDeps,
	input: { proposal: ControllerProposal<JsonValue>; context: BoundedContextView },
	options: RavoChildCallOptions,
): Promise<
	| { status: "completed"; value: RefereeResult; tokens: number }
	| { status: "aborted" | "turn_limit" | "budget_exceeded"; tokens: number; error?: string }
> {
	const proposal = deps.proposalOf(input.proposal.artifact);
	const targets = refereeSkillEdits(proposal);
	if (targets.length === 0) {
		return {
			status: "completed",
			value: {
				verdict: "not_applicable",
				status: "pass",
				detail: "abstain: proposal touches no skill edit",
				tokens: 0,
			},
			tokens: 0,
		};
	}
	const skills: RefereeSkillTarget[] = [];
	for (const target of targets) {
		let source: SkillModuleSource | undefined;
		if (deps.readSource) {
			try {
				source = await deps.readSource(target.edit, { signal: options.signal });
			} catch (error) {
				source = { truncated: false, detail: error instanceof Error ? error.message : String(error) };
			}
		}
		skills.push({ ...target, ...(source ? { source } : {}) });
	}
	const challenged = await deps.challenge(
		{ proposal: input.proposal, task: refereeTaskText(input.context), skills },
		options,
	);
	if (challenged.status === "deferred") {
		return {
			status: "completed",
			value: adjudicateReferee(
				undefined,
				undefined,
				0,
				"no executable counter-example (referee returned a deferred result)",
			),
			tokens: 0,
		};
	}
	if (challenged.status === "error") {
		return {
			status: "completed",
			value: adjudicateReferee(
				undefined,
				undefined,
				challenged.tokens,
				`no executable counter-example (${challenged.error ?? "referee output invalid"})`,
			),
			tokens: challenged.tokens,
		};
	}
	if (challenged.status !== "completed") {
		return {
			status: challenged.status,
			tokens: challenged.tokens,
			...(challenged.error === undefined ? {} : { error: challenged.error }),
		};
	}
	const challenge = challenged.value;
	if (challenge.flaw === null || challenge.test === undefined) {
		return {
			status: "completed",
			value: adjudicateReferee(challenge, undefined, challenged.tokens),
			tokens: challenged.tokens,
		};
	}
	const target = targets.find((item) => item.editIndex === challenge.editIndex) ?? targets[0];
	const run = await deps.run(target.edit, challenge.test, { signal: options.signal });
	return {
		status: "completed",
		value: adjudicateReferee(challenge, run, challenged.tokens),
		tokens: challenged.tokens,
	};
}

/** The opponent adapter run-service registers under criterion `referee:counterexample`. */
export function createRefereeOpponent(deps: RefereeOpponentDeps): EvaluationAdapter<JsonValue> {
	return {
		id: REFEREE_OPPONENT_ID,
		kind: "opponent",
		criterionId: REFEREE_CRITERION_ID,
		evaluate: async (input, options) => {
			const result = await evaluateReferee(deps, input, options);
			if (result.status !== "completed") return result;
			return {
				status: "completed",
				value: { status: result.value.status, detail: result.value.detail },
				tokens: result.tokens,
			};
		},
	};
}

/**
 * The persisted harness pool must not carry the referee criterion: Assisted
 * RAVO (`authorizeAssistedRavo`) observes only the five assisted criteria and
 * failure opponents, so any other id in `HarnessState.ravo.opponents` would be
 * an unobserved abstention and count as a miss on every /refine. The referee
 * therefore joins the pool for the run and leaves it before the save.
 */
export function withoutRefereeCriterion<T extends { opponents: { criteria: { id: string }[] } }>(state: T): T {
	if (!state.opponents.criteria.some((criterion) => criterion.id === REFEREE_CRITERION_ID)) return state;
	return {
		...state,
		opponents: {
			...state.opponents,
			criteria: state.opponents.criteria.filter((criterion) => criterion.id !== REFEREE_CRITERION_ID),
		},
	};
}

function objectRecord(value: unknown): Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: {};
}
