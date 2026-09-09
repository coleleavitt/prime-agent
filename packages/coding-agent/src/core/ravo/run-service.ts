import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import type { Model } from "@earendil-works/pi-ai";
import { RAVO_DEFAULT_CONFIG, RAVO_SEED_CRITERIA, ravoFastScreen } from "../refinement/ravo.js";
import {
	applyRefinementProposal,
	countValidRefinementEdits,
	formatHarnessStateForPrompt,
	type HarnessScope,
	type HarnessState,
	normalizeRefinementProposal,
	type RefinementProposal,
} from "../refinement/refinement.js";
import { resolveKernelPython, screenRefinementProposal } from "../refinement/skill-dry-run.js";
import type { RunAgentHandler } from "../run-agent.js";
import {
	type ArcAgentArtifact,
	type ArcEvaluationResult,
	type ArcRunner,
	evaluateArcAgent,
	validateArcArtifact,
} from "./arc-agi-evaluator.js";
import { RavoArchive } from "./archive.js";
import { failureOpponentFingerprint, isFailureOpponentId, normalizeAssistedRavoState } from "./authority.js";
import type { BoundedContextView, ContextArchive, ContextAtom, ContextViewLimits } from "./context-view.js";
import {
	type ChildCall,
	type ControllerProposal,
	type DiagnosticFeedback,
	type EvaluationAdapter,
	type InspectionFindings,
	type RavoChildCallOptions,
	type RavoChildResult,
	type RavoControllerCheckpoint,
	type RavoControllerOptions,
	type RavoControllerResult,
	type RavoPhase,
	type RavoPlan,
	type RavoProgressEvent,
	type RavoStopReason,
	runRavoController,
	type SupervisorSignal,
} from "./controller.js";
import { ErrorBudgetLedger } from "./error-budget-ledger.js";
import {
	emptyFailureLedger,
	type FailureRecord,
	failureOpponentId,
	formatFailureLedgerForPrompt,
	recurringFailures,
} from "./failure-ledger.js";
import { Rational } from "./rational.js";
import {
	type JsonValue,
	type RavoEvaluation,
	type RavoGateCertificate,
	type RavoState,
	ravoExtendOpponents,
	ravoMarkProvisional,
	ravoStep,
} from "./reducer.js";
import {
	type ChildRuntimeScope,
	createRetainedWorkerChildCall,
	createRunAgentChildCall,
	type RetainedWorkerRuntime,
	type StructuredChildSpec,
} from "./runtime-adapter.js";

/**
 * RavoRunService: the full RAVO controller loop over one continual-harness
 * refinement proposal. Children (inspect, plan, implement, repair, judge,
 * supervisor) are RunAgent calls with JSON-only prompts; the fast screen is
 * structural validity plus the skill dry-run; the deep gate and the five
 * hygiene opponents share ONE memoized judge call per proposal; recurring
 * failure fingerprints are deterministic opponents that pass iff the proposal
 * claims them in `addressedFingerprints`. The commit gate applies the proposal
 * to the harness state and persists the stepped reducer state into
 * `HarnessState.ravo`, so lineage and weights continue from Assisted RAVO.
 */

export interface RavoRunRequest {
	task: string;
	instructions?: string;
	global?: boolean;
	maxRounds?: number;
	maxRepairs?: number;
	deadlineMs?: number;
	tokenBudget?: number;
	evaluator?: "judge" | { kind: "arc-agi"; repoDir: string; game: string };
}

export type RavoCertificateStatus = "commit" | "reject_screen" | "reject_deep" | "reject_criteria";

export type RavoRunStatus = {
	runId: string;
	phase: RavoPhase | "idle";
	round: number;
	repairs: number;
	lastEvent?: RavoProgressEvent;
	stopReason?: RavoStopReason;
	startedAt: number;
	updatedAt: number;
	candidateId?: string;
	lastCertificate?: {
		proposalId: string;
		status: RavoCertificateStatus;
		screenScore: number;
		deepScore?: number;
		missed: string[];
	};
	/** Set when the run ended with an unexpected error instead of a stop reason. */
	error?: string;
};

export type RavoRunEvent = { type: "ravo_run_update"; status: RavoRunStatus };

export interface RavoRunServiceDeps {
	runAgent: RunAgentHandler;
	retainedRuntime?: RetainedWorkerRuntime;
	harnessDir: string;
	globalHarnessDir?: string;
	model?: Model<any>;
	loadState: () => Promise<HarnessState>;
	saveState: (state: HarnessState) => Promise<void>;
	onUpdate: (status: RavoRunStatus) => void;
	now?: () => number;
	/** Runs the ARC-AGI-3 harness for `evaluator: { kind: "arc-agi" }`; tests inject a fake. Defaults to `uv run main.py`. */
	arcRunner?: ArcRunner;
}

/** JSON object produced by the implement/repair children: a RefinementProposal plus `addressedFingerprints`. */
export type RavoRunArtifact = { [key: string]: JsonValue };

export const RAVO_RUN_DEFAULTS = {
	maxRounds: 4,
	maxRepairs: 3,
	deadlineMs: 20 * 60 * 1000,
	tokenBudget: 400_000,
	concurrency: 3,
	familyDelta: Rational.of(1, 20),
} as const;

const CONTEXT_LIMITS: ContextViewLimits = {
	maxTokens: 24_000,
	maxBytes: 96_000,
	maxItems: 16,
	lineageDepth: 3,
	maxArtifactBytesPerItem: 32_000,
};

const CHILD_RETRIES = 1;
const JSON_ONLY = "Return exactly one JSON object and nothing else: no prose, no code fences.";

export class RavoRunService {
	readonly #deps: RavoRunServiceDeps;
	#status: RavoRunStatus | undefined;
	#abort: AbortController | undefined;
	#running = false;

	constructor(deps: RavoRunServiceDeps) {
		this.#deps = deps;
	}

	get running(): boolean {
		return this.#running;
	}

	status(): RavoRunStatus | undefined {
		return this.#status ? structuredClone(this.#status) : undefined;
	}

	cancel(): boolean {
		if (!this.#running || !this.#abort) return false;
		this.#abort.abort();
		return true;
	}

	async start(request: RavoRunRequest, signal?: AbortSignal): Promise<RavoRunStatus> {
		if (this.#running) throw new Error(`RAVO run ${this.#status?.runId ?? ""} is already running`.trim());
		if (!request.task.trim()) throw new Error("RAVO run requires a task");
		const now = this.#deps.now ?? Date.now;
		const runId = `ravo_${new Date(now())
			.toISOString()
			.replace(/[^0-9]/g, "")
			.slice(0, 14)}_${randomUUID().slice(0, 8)}`;
		const abort = new AbortController();
		const relay = (): void => abort.abort();
		if (signal?.aborted) abort.abort();
		else signal?.addEventListener("abort", relay, { once: true });
		this.#abort = abort;
		this.#running = true;
		const startedAt = now();
		this.#status = { runId, phase: "idle", round: 0, repairs: 0, startedAt, updatedAt: startedAt };
		try {
			return await this.#run(runId, request, abort.signal, now);
		} catch (error) {
			this.#update({ phase: "stopped", error: error instanceof Error ? error.message : String(error) });
			throw error;
		} finally {
			signal?.removeEventListener("abort", relay);
			this.#running = false;
			this.#abort = undefined;
		}
	}

	#update(patch: Partial<RavoRunStatus>, emit = true): RavoRunStatus {
		const current = this.#status;
		if (!current) throw new Error("no active RAVO run");
		const next: RavoRunStatus = { ...current, ...patch, updatedAt: (this.#deps.now ?? Date.now)() };
		this.#status = next;
		if (emit) {
			try {
				this.#deps.onUpdate(structuredClone(next));
			} catch {
				// Observers never abort the run.
			}
		}
		return next;
	}

	async #run(runId: string, request: RavoRunRequest, signal: AbortSignal, now: () => number): Promise<RavoRunStatus> {
		const deps = this.#deps;
		const scope: HarnessScope = request.global ? "global" : "local";
		const baseDir = request.global && deps.globalHarnessDir ? deps.globalHarnessDir : deps.harnessDir;
		const runsDir = path.join(baseDir, "ravo", "runs");
		const checkpointPath = path.join(runsDir, `${runId}.json`);
		await mkdir(runsDir, { recursive: true });
		const state = await deps.loadState();
		const recurring = recurringFailures(state.failures ?? emptyFailureLedger());
		const activeFailureIds = [...new Set(recurring.map((record) => failureOpponentId(record.fingerprint)))];
		const baseRavo = normalizeAssistedRavoState(state.ravo);
		const arc = request.evaluator !== undefined && request.evaluator !== "judge" ? request.evaluator : undefined;
		const initialState: RavoState<JsonValue> = {
			...baseRavo,
			opponents: ravoExtendOpponents(baseRavo.opponents, [...activeFailureIds, ...(arc ? ARC_OPPONENT_IDS : [])]),
		};
		const config = { ...RAVO_DEFAULT_CONFIG };
		const context = buildContextArchive(request, state, recurring, initialState);
		const model = deps.model ? `${deps.model.provider}/${deps.model.id}` : undefined;
		const scopeOf = (role: string, overrides: Partial<ChildRuntimeScope> = {}): ChildRuntimeScope => ({
			...(model ? { model } : {}),
			tools: "none",
			maxTurns: 2,
			role,
			...overrides,
		});
		const structured = <TInput, TOutput>(spec: StructuredChildSpec<TInput, TOutput>): ChildCall<TInput, TOutput> =>
			retrying(createRunAgentChildCall(deps.runAgent, spec), CHILD_RETRIES);
		let proposalCount = 0;
		let planCount = 0;
		const proposalSpec = (kind: "implement" | "repair") => ({
			prompt: (input: ProposalInput) => proposalPrompt(kind, input, recurring, arc !== undefined),
			validate: (value: unknown): ControllerProposal<JsonValue> => {
				proposalCount += 1;
				return {
					id: `${runId}-p${proposalCount}`,
					parentId: null,
					repairOf: null,
					artifact: validateArtifact(value),
				};
			},
			scope: scopeOf(kind, { maxTurns: 4 }),
		});
		const linkRepair =
			(
				call: ChildCall<ProposalInput, ControllerProposal<JsonValue>>,
			): ChildCall<RepairInput, ControllerProposal<JsonValue>> =>
			async (input, options) => {
				const result = await call(input, options);
				return mapResult(result, (proposal) => ({
					...proposal,
					parentId: input.candidate.id,
					repairOf: input.candidate.id,
				}));
			};
		const implementCall = deps.retainedRuntime
			? createRetainedWorkerChildCall(deps.retainedRuntime, proposalSpec("implement"))
			: structured(proposalSpec("implement"));
		const repairCall = linkRepair(
			deps.retainedRuntime
				? createRetainedWorkerChildCall(deps.retainedRuntime, proposalSpec("repair"))
				: structured(proposalSpec("repair")),
		);

		const judge = memoizedJudge(structured(judgeSpec(recurring, state, scopeOf("judge"))));
		const hygieneIds = RAVO_SEED_CRITERIA.map((criterion) => criterion.id);
		const arcRun = arc
			? memoizedArcRun({ ...arc, ...(deps.arcRunner ? { runner: deps.arcRunner } : {}) })
			: undefined;
		const hygieneOpponents: EvaluationAdapter<JsonValue>[] = arcRun
			? [...arcOpponents(arcRun), ...hygieneIds.map(notApplicableOpponent)]
			: hygieneIds.map(
					(criterionId): EvaluationAdapter<JsonValue> => ({
						id: `opponent:${criterionId}`,
						kind: "opponent",
						criterionId,
						evaluate: async (input, options) => {
							const judged = await judge(input.proposal, input.context, options);
							if (judged.status !== "completed") return judged;
							const failed = judged.value.failedCriteria.includes(criterionId);
							return {
								status: "completed",
								value: { status: failed ? "fail" : "pass", detail: judged.value.rationale },
								tokens: judged.tokens,
							};
						},
					}),
				);
		const opponents: EvaluationAdapter<JsonValue>[] = [
			...hygieneOpponents,
			...initialState.opponents.criteria
				.filter((criterion) => isFailureOpponentId(criterion.id))
				.map((criterion): EvaluationAdapter<JsonValue> => {
					const fingerprint = failureOpponentFingerprint(criterion.id) ?? "";
					const dormant = !activeFailureIds.includes(criterion.id);
					return {
						id: `opponent:${criterion.id}`,
						kind: "opponent",
						criterionId: criterion.id,
						evaluate: async ({ proposal }) => {
							if (dormant) {
								return {
									status: "completed",
									value: { status: "pass", detail: "dormant: fingerprint is not currently recurring" },
									tokens: 0,
								};
							}
							const claimed = addressedFingerprintsOf(proposal.artifact).includes(fingerprint);
							return {
								status: "completed",
								value: {
									status: claimed ? "pass" : "fail",
									detail: claimed
										? `proposal claims to address ${fingerprint}`
										: `recurring failure ${fingerprint} is not addressed (set addressedFingerprints)`,
								},
								tokens: 0,
							};
						},
					};
				}),
		];
		const fast: EvaluationAdapter<JsonValue> = arcRun
			? arcFastAdapter(config.screenThreshold)
			: {
					id: "fast:structural-dry-run",
					kind: "fast",
					evaluate: async ({ proposal }, options) => {
						const refinement = proposalOf(proposal.artifact);
						const screened = await screenRefinementProposal(refinement, { signal: options.signal });
						const score = ravoFastScreen(refinement, screened.validEdits);
						const failures = screened.dryRun
							.filter((item) => !item.ok)
							.map((item) => `edit ${item.editIndex}: ${item.detail}`);
						const invalid = refinement.edits.length - countValidRefinementEdits(refinement);
						const detail = [
							`${screened.validEdits}/${refinement.edits.length} edits pass the structural screen and dry-run`,
							...(invalid > 0 ? [`${invalid} structurally invalid`] : []),
							...failures,
						].join("; ");
						return {
							status: "completed",
							value: { status: score >= config.screenThreshold ? "pass" : "fail", score, detail },
							tokens: 0,
						};
					},
				};
		const deep: EvaluationAdapter<JsonValue> = arcRun
			? arcDeepAdapter(arcRun)
			: {
					id: "deep:judge",
					kind: "deep",
					evaluate: async (input, options) => {
						const judged = await judge(input.proposal, input.context, options);
						if (judged.status !== "completed") return judged;
						return {
							status: "completed",
							value: { status: "pass", score: judged.value.score, detail: judged.value.rationale },
							tokens: judged.tokens,
						};
					},
				};

		// Mirror of the controller's reducer state so the commit gate can replay
		// the exact step and persist the resulting state in the same save.
		let mirror: RavoState<JsonValue> = structuredClone(initialState);
		let rejectedSinceCommit = 0;
		const archive = new RavoArchive({ artifactRoot: baseDir, archivePath: "ravo/archive" });
		const tokenBudget = request.tokenBudget ?? RAVO_RUN_DEFAULTS.tokenBudget;
		const options: RavoControllerOptions<JsonValue> = {
			runId,
			context,
			contextLimits: CONTEXT_LIMITS,
			initialState,
			reducerConfig: config,
			archive,
			ledger: new ErrorBudgetLedger(RAVO_RUN_DEFAULTS.familyDelta),
			inspect: structured(inspectSpec(scopeOf("inspect"))),
			plan: structured({
				prompt: planPrompt,
				validate: (value: unknown): RavoPlan => {
					planCount += 1;
					return { id: `${runId}-plan${planCount}`, steps: stringList(objectRecord(value).steps) };
				},
				scope: scopeOf("plan"),
			}),
			implement: implementCall,
			repair: repairCall,
			evaluators: [fast, deep, ...opponents],
			supervisor: structured(supervisorSpec(scopeOf("supervisor"))),
			shouldConsultSupervisor: () => rejectedSinceCommit >= 2,
			commitGate: async ({ proposal, certificate }) => {
				const refinement = proposalOf(proposal.artifact);
				const stepped = ravoStep(
					mirror,
					{ id: proposal.id, artifact: proposal.artifact },
					evaluationFromCertificate(certificate),
					config,
				);
				if (!stepped.certificate.committed) {
					return {
						accepted: false,
						detail: `reducer replay rejected (${stepped.certificate.rejection ?? "unknown"})`,
					};
				}
				const current = await deps.loadState();
				const result = applyRefinementProposal(current, refinement, {
					id: proposal.id,
					scope,
					baselineState: state,
				});
				const failed = result.appliedEdits.filter((edit) => !edit.applied);
				if (failed.length > 0) {
					return {
						accepted: false,
						detail: failed
							.map((edit) => `${edit.action} ${edit.kind}:${edit.id}: ${edit.error ?? "not applied"}`)
							.join("; "),
					};
				}
				if (arc) await persistArcAgent(baseDir, runId, proposal.artifact);
				current.ravo = ravoMarkProvisional(stepped.state, proposal.id, {
					claimedFingerprints: addressedFingerprintsOf(proposal.artifact).filter((fingerprint) =>
						activeFailureIds.includes(failureOpponentId(fingerprint)),
					),
				});
				await deps.saveState(current);
				mirror = stepped.state;
				return { accepted: true, detail: `applied ${result.appliedEdits.length} edits` };
			},
			maxRounds: request.maxRounds ?? RAVO_RUN_DEFAULTS.maxRounds,
			maxRepairs: request.maxRepairs ?? RAVO_RUN_DEFAULTS.maxRepairs,
			deadlineMs: request.deadlineMs ?? RAVO_RUN_DEFAULTS.deadlineMs,
			tokenBudget,
			reservationPerCall: Math.max(1, Math.min(32_000, Math.floor(tokenBudget / 8))),
			concurrency: RAVO_RUN_DEFAULTS.concurrency,
			signal,
			now,
			onProgress: (event) => {
				if (event.type === "evaluation" && !event.certificate.committed) {
					rejectedSinceCommit += 1;
					mirror = {
						...mirror,
						evaluatedProposalIds: [...mirror.evaluatedProposalIds, event.proposalId],
					};
				}
				if (event.type === "stopped") {
					this.#update({ lastEvent: event }, false);
					return;
				}
				const repairs = this.#status?.repairs ?? 0;
				this.#update({
					...statusPatch(event),
					...(event.type === "phase" && event.phase === "diagnose" ? { repairs: repairs + 1 } : {}),
				});
			},
			onCheckpoint: async (checkpoint) => {
				await writeFile(checkpointPath, `${JSON.stringify(checkpointJson(checkpoint), null, 2)}\n`, "utf8");
			},
		};
		let result: RavoControllerResult<JsonValue>;
		try {
			result = await runRavoController(options);
		} catch (error) {
			// A child aborted mid-call surfaces as a child failure, not a controller stop.
			if (!signal.aborted) throw error;
			return this.#update({
				phase: "stopped",
				stopReason: "cancelled",
				lastEvent: { type: "stopped", reason: "cancelled" },
			});
		}
		if (result.reason === "accepted") await rm(checkpointPath, { force: true });
		return this.#update({
			phase: result.reason === "accepted" ? "accepted" : "stopped",
			stopReason: result.reason,
			round: result.checkpoint.round,
			repairs: result.checkpoint.repairs,
			lastEvent: { type: "stopped", reason: result.reason },
			...(result.checkpoint.candidate ? { candidateId: result.checkpoint.candidate.id } : {}),
		});
	}
}

interface ProposalInput {
	context: BoundedContextView;
	plan: RavoPlan;
	inspection?: InspectionFindings;
	candidate?: ControllerProposal<JsonValue>;
	feedback?: DiagnosticFeedback;
	workerHandle?: string;
}
interface RepairInput extends ProposalInput {
	candidate: ControllerProposal<JsonValue>;
	feedback: DiagnosticFeedback;
}
interface JudgeVerdict {
	score: number;
	failedCriteria: string[];
	addressedFingerprints: string[];
	rationale: string;
}
type JudgeInput = { proposal: ControllerProposal<JsonValue>; context: BoundedContextView };
type SettledChildResult<T> = Exclude<RavoChildResult<T>, { status: "deferred" }>;

function statusPatch(event: RavoProgressEvent): Partial<RavoRunStatus> {
	switch (event.type) {
		case "phase":
			return { lastEvent: event, phase: event.phase, round: event.round };
		case "proposal":
			return { lastEvent: event, candidateId: event.proposalId };
		case "evaluation":
			return { lastEvent: event, lastCertificate: certificateSummary(event.certificate) };
		case "supervisor":
			return { lastEvent: event };
		case "stopped":
			return {
				lastEvent: event,
				stopReason: event.reason,
				phase: event.reason === "accepted" ? "accepted" : "stopped",
			};
	}
}

export function certificateSummary(certificate: RavoGateCertificate): NonNullable<RavoRunStatus["lastCertificate"]> {
	const status: RavoCertificateStatus = certificate.committed
		? "commit"
		: certificate.rejection === "screen"
			? "reject_screen"
			: certificate.rejection === "deep"
				? "reject_deep"
				: "reject_criteria";
	return {
		proposalId: certificate.proposalId,
		status,
		screenScore: certificate.screen.score ?? 0,
		...(certificate.deep.score === undefined ? {} : { deepScore: certificate.deep.score }),
		missed: [...certificate.missedCriterionIds],
	};
}

function evaluationFromCertificate(certificate: RavoGateCertificate): RavoEvaluation {
	return {
		proposalId: certificate.proposalId,
		screen: certificate.screen,
		deep: certificate.deep,
		criteria: certificate.criteria.map((item) => ({
			criterionId: item.criterionId,
			status: item.status,
			...(item.detail === undefined ? {} : { detail: item.detail }),
		})),
	};
}

function buildContextArchive(
	request: RavoRunRequest,
	state: HarnessState,
	recurring: readonly FailureRecord[],
	ravo: RavoState<JsonValue>,
): ContextArchive {
	const champion = ravo.lineage.at(-1);
	const constraints: ContextAtom[] = [
		{
			id: "scope-policy",
			kind: "constraint",
			text: request.global
				? "Scope: global. Only stable cross-session lessons, durable user preferences, reusable skills/subagents, or project-qualified facts."
				: "Scope: local. Session progress, temporary blockers, and coordination facts for this session only.",
		},
		{
			id: "harness-overview",
			kind: "constraint",
			text: formatHarnessStateForPrompt(state, { includeIpythonExamples: false, includeShellExamples: false }),
		},
		{
			id: "failure-ledger",
			kind: "constraint",
			text: `Recurring failures (opponent criteria failure:<fingerprint>):\n${formatFailureLedgerForPrompt(recurring)}`,
		},
	];
	return {
		currentTask: {
			id: "task",
			kind: "current_task",
			text: [
				request.task.trim(),
				...(request.instructions ? [`Instructions: ${request.instructions.trim()}`] : []),
			].join("\n"),
		},
		...(champion
			? {
					champion: {
						id: `champion:${champion.proposalId}`,
						kind: "champion" as const,
						text: `Current champion ${champion.proposalId} (score ${champion.score}): ${summaryOf(champion.artifact)}`,
					},
				}
			: {}),
		constraints,
	};
}

function renderContext(view: BoundedContextView): string {
	const items = view.items
		.filter((item) => item.content)
		.map((item) => `<${item.kind} id="${item.id}">\n${item.content}\n</${item.kind}>`);
	return items.join("\n\n");
}

const PROPOSAL_SHAPE = `{
  "summary": "one line",
  "rationale": "why, citing concrete evidence",
  "expectedOutcome": "one line",
  "addressedFingerprints": ["recurring failure fingerprint ids this proposal genuinely fixes"],
  "edits": [{ "action": "create|update|delete", "kind": "prompt|memory|skill|subagent", "id": "existing id (update/delete)", "title": "...", "content": "...", "path": "area/topic", "reference": { "type": "python", "import": "module", "callable": "fn" }, "arguments": {}, "reason": "..." }]
}
Rules: cite evidence for every edit; keep edits minimal and non-duplicating; skill edits need a python reference and arguments; never edit base_system_prompt; an empty edits list is a non-candidate.`;

function inspectSpec(
	scope: ChildRuntimeScope,
): StructuredChildSpec<{ context: BoundedContextView }, InspectionFindings> {
	return {
		prompt: ({ context }) =>
			[
				"# RAVO inspect",
				"Inspect the continual harness state and failure ledger below. Report what relates to the task: relevant entries, gaps, duplicates, and recurring failures.",
				renderContext(context),
				`Return JSON: { "summary": "one paragraph", "facts": ["short concrete fact", ...] }. ${JSON_ONLY}`,
			].join("\n\n"),
		validate: (value: unknown): InspectionFindings => {
			const record = objectRecord(value);
			const summary = typeof record.summary === "string" ? record.summary.trim() : "";
			if (!summary) throw new Error("inspect output requires a summary");
			return { summary, facts: stringList(record.facts) };
		},
		scope,
	};
}

function planPrompt(input: {
	context: BoundedContextView;
	inspection: InspectionFindings;
	feedback?: DiagnosticFeedback;
}): string {
	return [
		"# RAVO plan",
		"Plan a minimal continual-harness refinement for the task. Each step names the edit (action, kind, id) and the evidence for it.",
		renderContext(input.context),
		`<inspection>\n${input.inspection.summary}\n${input.inspection.facts.map((fact) => `- ${fact}`).join("\n")}\n</inspection>`,
		...(input.feedback
			? [`<rejection>\n${formatFeedback(input.feedback)}\n</rejection>\nThe plan must fix every finding above.`]
			: []),
		`Return JSON: { "steps": ["step", ...] }. ${JSON_ONLY}`,
	].join("\n\n");
}

function proposalPrompt(
	kind: "implement" | "repair",
	input: ProposalInput,
	recurring: readonly FailureRecord[],
	arc: boolean,
): string {
	const fingerprints = recurring.map((record) => record.fingerprint.id);
	return [
		`# RAVO ${kind}`,
		kind === "implement"
			? "Produce the refinement proposal that executes the plan."
			: "Repair the rejected proposal so every finding below is resolved. Keep what was right; change only what the findings require.",
		renderContext(input.context),
		...(input.inspection ? [`<inspection>\n${input.inspection.summary}\n</inspection>`] : []),
		`<plan>\n${[...input.plan.steps.map((step) => `- ${step}`), ...(input.plan.supervisorAdvice ? [`Supervisor: ${input.plan.supervisorAdvice}`] : [])].join("\n")}\n</plan>`,
		...(input.candidate
			? [`<rejected_proposal>\n${JSON.stringify(input.candidate.artifact)}\n</rejected_proposal>`]
			: []),
		...(input.feedback ? [`<rejection>\n${formatFeedback(input.feedback)}\n</rejection>`] : []),
		...(fingerprints.length > 0
			? [
					`Recurring failure fingerprints that must be addressed (list the ones you fix in addressedFingerprints): ${fingerprints.join(", ")}`,
				]
			: []),
		...(arc
			? [
					'Also include "arcAgent": { "agentName": "python_module_name", "source": "full agent module source" } for the ARC-AGI-3 evaluator.',
				]
			: []),
		`Return JSON with this shape:\n${PROPOSAL_SHAPE}\n${JSON_ONLY}`,
	].join("\n\n");
}

const JUDGE_HEADER = `# RAVO judge
You are the RAVO deep evaluator for Prime Agent's /refine subsystem. Score a proposed continual-harness refinement against the evidence. Judge the QUALITY OF THE RESULTING HARNESS STATE, not prose style.
Each criterion below is an opponent; list the ids the proposal fails in "failedCriteria". A recurring failure counts as addressed only if the edits would plausibly prevent that exact failure from recurring; never list a fingerprint the proposal merely mentions.`;

function judgeSpec(
	recurring: readonly FailureRecord[],
	state: HarnessState,
	scope: ChildRuntimeScope,
): StructuredChildSpec<JudgeInput, JudgeVerdict> {
	const criteria = RAVO_SEED_CRITERIA.map((criterion) => `- ${criterion.id}: ${criterion.description}`).join("\n");
	const overview = formatHarnessStateForPrompt(state, { includeIpythonExamples: false, includeShellExamples: false });
	return {
		prompt: ({ proposal, context }) =>
			[
				JUDGE_HEADER,
				`<criteria>\n${criteria}\n</criteria>`,
				...(recurring.length > 0
					? [`<recurring_failures>\n${formatFailureLedgerForPrompt(recurring)}\n</recurring_failures>`]
					: []),
				`<current_harness_state>\n${overview}\n</current_harness_state>`,
				renderContext(context),
				`<proposal>\n${JSON.stringify(proposal.artifact, null, 2)}\n</proposal>`,
				`Return JSON: { "score": 0-100, "failedCriteria": ["id"], "addressedFingerprints": ["fingerprint id"], "rationale": "one or two sentences" }. ${JSON_ONLY}`,
			].join("\n\n"),
		validate: validateJudge,
		scope,
	};
}

function supervisorSpec(
	scope: ChildRuntimeScope,
): StructuredChildSpec<SupervisorSignal<JsonValue>, { intervene: boolean; advice?: string }> {
	return {
		prompt: (signal) =>
			[
				"# RAVO supervisor",
				"Several proposals were rejected. Decide whether the plan needs redirecting and give one concrete piece of advice if so.",
				`<plan>\n${signal.plan.steps.map((step) => `- ${step}`).join("\n")}\n</plan>`,
				`<trajectory>\n${signal.trajectory
					.map(
						(certificate) =>
							`${certificate.proposalId}: ${certificate.committed ? "commit" : `rejected (${certificate.rejection ?? "gate"}) missed=${certificate.missedCriterionIds.join(",")}`}`,
					)
					.join("\n")}\n</trajectory>`,
				`Return JSON: { "intervene": true|false, "advice": "one or two sentences" }. ${JSON_ONLY}`,
			].join("\n\n"),
		validate: (value: unknown) => {
			const record = objectRecord(value);
			return {
				intervene: record.intervene === true,
				...(typeof record.advice === "string" && record.advice.trim() ? { advice: record.advice.trim() } : {}),
			};
		},
		scope,
	};
}

function formatFeedback(feedback: DiagnosticFeedback): string {
	return [
		`rejection: ${feedback.rejection}`,
		...feedback.findings.map((finding) => `- ${finding.source} [${finding.status}]: ${finding.detail}`),
	].join("\n");
}

/** Validate the implement/repair child output; `undefined` fields are dropped so the artifact is pure JSON. */
export function validateArtifact(value: unknown): RavoRunArtifact {
	const record = objectRecord(value);
	if (!Array.isArray(record.edits)) throw new Error("proposal output requires an edits array");
	const proposal = normalizeRefinementProposal(record);
	const artifact: Record<string, unknown> = {
		...proposal,
		addressedFingerprints: stringList(record.addressedFingerprints),
	};
	const arcAgent = objectRecord(record.arcAgent);
	if (typeof arcAgent.agentName === "string" && typeof arcAgent.source === "string") {
		artifact.arcAgent = { agentName: arcAgent.agentName, source: arcAgent.source };
	}
	return JSON.parse(JSON.stringify(artifact)) as RavoRunArtifact;
}

function validateJudge(value: unknown): JudgeVerdict {
	const record = objectRecord(value);
	const rawScore = typeof record.score === "number" ? record.score : Number(record.score);
	if (!Number.isFinite(rawScore)) throw new Error("judge output requires a numeric score");
	return {
		score: Math.min(100, Math.max(0, Math.round(rawScore))),
		failedCriteria: stringList(record.failedCriteria),
		addressedFingerprints: stringList(record.addressedFingerprints),
		rationale: typeof record.rationale === "string" ? record.rationale : "",
	};
}

export function proposalOf(artifact: JsonValue): RefinementProposal {
	return normalizeRefinementProposal(artifact);
}

export function addressedFingerprintsOf(artifact: JsonValue): string[] {
	return stringList(objectRecord(artifact).addressedFingerprints);
}

function summaryOf(artifact: JsonValue): string {
	const summary = objectRecord(artifact).summary;
	return typeof summary === "string" ? summary : "(no summary)";
}

function arcArtifactOf(artifact: JsonValue): ArcAgentArtifact | undefined {
	const record = objectRecord(artifact);
	const arcAgent = objectRecord(record.arcAgent);
	if (typeof arcAgent.agentName === "string" && typeof arcAgent.source === "string") {
		return { agentName: arcAgent.agentName, source: arcAgent.source };
	}
	const skill = proposalOf(artifact).edits.find(
		(edit) => edit.kind === "skill" && edit.action !== "delete" && edit.content,
	);
	if (!skill?.content) return undefined;
	return { agentName: (skill.id ?? skill.title ?? "agent").replace(/[^a-zA-Z0-9_]+/g, "_"), source: skill.content };
}

const ARC_OPPONENT_IDS = ["arc:no-crash", "arc:all-levels"] as const;

type ArcRunFn = (proposal: ControllerProposal<JsonValue>, signal: AbortSignal) => Promise<ArcEvaluationResult>;

/** One real game per proposal, shared by the deep gate and the outcome opponents. */
function memoizedArcRun(options: { repoDir: string; game: string; runner?: ArcRunner }): ArcRunFn {
	const pending = new Map<string, Promise<ArcEvaluationResult>>();
	return (proposal, signal) => {
		let shared = pending.get(proposal.id);
		if (!shared) {
			const artifact = arcArtifactOf(proposal.artifact);
			shared = artifact
				? evaluateArcAgent(options, artifact, signal)
				: Promise.resolve({
						status: "error" as const,
						detail: "proposal carries no ARC agent source (arcAgent or a skill edit)",
					});
			pending.set(proposal.id, shared);
		}
		return shared;
	};
}

/** Deterministic screen for an ARC candidate: artifact shape plus a Python syntax check. No game is played. */
function arcFastAdapter(screenThreshold: number): EvaluationAdapter<JsonValue> {
	return {
		id: "fast:arc-artifact",
		kind: "fast",
		evaluate: async ({ proposal }, options) => {
			const artifact = arcArtifactOf(proposal.artifact);
			let detail: string;
			let ok = false;
			try {
				if (!artifact) throw new Error("proposal carries no ARC agent source (arcAgent or a skill edit)");
				validateArcArtifact(artifact);
				const syntax = await pythonSyntaxCheck(artifact.source, options.signal);
				ok = syntax.ok;
				detail = syntax.ok ? `agent ${artifact.agentName} parses` : `agent ${artifact.agentName}: ${syntax.detail}`;
			} catch (error) {
				detail = error instanceof Error ? error.message : String(error);
			}
			const score = ok ? 100 : 0;
			return {
				status: "completed",
				value: { status: score >= screenThreshold ? "pass" : "fail", score, detail },
				tokens: 0,
			};
		},
	};
}

function arcDeepAdapter(run: ArcRunFn): EvaluationAdapter<JsonValue> {
	return {
		id: "deep:arc-agi",
		kind: "deep",
		evaluate: async ({ proposal }, options) => {
			const result = await run(proposal, options.signal);
			return {
				status: "completed",
				value: {
					status: result.status,
					...(result.score === undefined ? {} : { score: result.score }),
					...(result.detail === undefined ? {} : { detail: result.detail }),
				},
				tokens: 0,
			};
		},
	};
}

/**
 * Outcome opponents derived from the same game run: the agent must not crash,
 * and it must finish every level. Missing `arc:all-levels` costs one weight
 * unit at first; weakness pressure doubles it after a champion is accepted
 * without finishing, so later candidates cannot keep winning on partial games.
 */
function arcOpponents(run: ArcRunFn): EvaluationAdapter<JsonValue>[] {
	return [
		{
			id: "opponent:arc:no-crash",
			kind: "opponent",
			criterionId: "arc:no-crash",
			evaluate: async ({ proposal }, options) => {
				const result = await run(proposal, options.signal);
				return {
					status: "completed",
					value: { status: result.status === "pass" ? "pass" : "fail", detail: result.detail ?? result.status },
					tokens: 0,
				};
			},
		},
		{
			id: "opponent:arc:all-levels",
			kind: "opponent",
			criterionId: "arc:all-levels",
			evaluate: async ({ proposal }, options) => {
				const result = await run(proposal, options.signal);
				const card = result.scorecard;
				const done = card !== undefined && card.totalLevels > 0 && card.levelsCompleted === card.totalLevels;
				return {
					status: "completed",
					value: {
						status: done ? "pass" : "fail",
						detail: card
							? `${card.levelsCompleted}/${card.totalLevels} levels`
							: (result.detail ?? "no scorecard"),
					},
					tokens: 0,
				};
			},
		},
	];
}

/** Hygiene criteria judge harness prose; an ARC agent artifact has none, so they pass vacuously and keep their weights. */
function notApplicableOpponent(criterionId: string): EvaluationAdapter<JsonValue> {
	return {
		id: `opponent:${criterionId}`,
		kind: "opponent",
		criterionId,
		evaluate: async () => ({
			status: "completed",
			value: { status: "pass", detail: "not applicable to an ARC agent candidate" },
			tokens: 0,
		}),
	};
}

async function pythonSyntaxCheck(source: string, signal: AbortSignal): Promise<{ ok: boolean; detail: string }> {
	const python = resolveKernelPython() ?? "python3";
	return new Promise((resolve) => {
		const child = execFile(
			python,
			["-I", "-c", "import ast,sys; ast.parse(sys.stdin.read())"],
			{ timeout: 10_000, signal },
			(error, _stdout, stderr) => {
				if (!error) return resolve({ ok: true, detail: "ok" });
				const lines = String(stderr).trim().split("\n");
				resolve({ ok: false, detail: lines.at(-1) || error.message });
			},
		);
		child.stdin?.end(source);
	});
}

async function persistArcAgent(baseDir: string, runId: string, artifact: JsonValue): Promise<void> {
	const agent = arcArtifactOf(artifact);
	if (!agent) return;
	const dir = path.join(baseDir, "ravo", "arc");
	await mkdir(dir, { recursive: true });
	await writeFile(path.join(dir, `${runId}-${agent.agentName}.py`), agent.source, "utf8");
}

/** One judge call per proposal, shared by the deep gate and the hygiene opponents. Tokens are reported once. */
function memoizedJudge(
	call: ChildCall<JudgeInput, JudgeVerdict>,
): (
	proposal: ControllerProposal<JsonValue>,
	context: BoundedContextView,
	options: RavoChildCallOptions,
) => Promise<SettledChildResult<JudgeVerdict>> {
	const pending = new Map<string, Promise<SettledChildResult<JudgeVerdict>>>();
	return async (proposal, context, options) => {
		let shared = pending.get(proposal.id);
		if (!shared) {
			shared = call({ proposal, context }, options).then(
				(result): SettledChildResult<JudgeVerdict> =>
					result.status === "deferred"
						? { status: "error", tokens: 0, error: "judge returned a deferred result" }
						: result,
			);
			pending.set(proposal.id, shared);
			return shared;
		}
		const result = await shared;
		return { ...result, tokens: 0 };
	};
}

/** Retry a structured child once on invalid output; token usage is summed across attempts. */
function retrying<TInput, TOutput>(call: ChildCall<TInput, TOutput>, retries: number): ChildCall<TInput, TOutput> {
	return async (input, options) => {
		let spent = 0;
		for (let attempt = 0; ; attempt += 1) {
			const result = await call(input, options);
			if (result.status === "deferred") return result;
			spent += result.tokens;
			if (result.status === "completed") return { ...result, tokens: spent };
			if (result.status !== "error" || attempt >= retries || options.signal.aborted)
				return { ...result, tokens: spent };
		}
	};
}

function mapResult<A, B>(result: RavoChildResult<A>, fn: (value: A) => B): RavoChildResult<B> {
	if (result.status === "completed") return { status: "completed", value: fn(result.value), tokens: result.tokens };
	if (result.status === "deferred") {
		return {
			status: "deferred",
			handle: result.handle,
			wait: async (options) => mapResult(await result.wait(options), fn),
		};
	}
	return result;
}

function checkpointJson(checkpoint: RavoControllerCheckpoint<JsonValue>): JsonValue {
	return JSON.parse(JSON.stringify(checkpoint)) as JsonValue;
}

function objectRecord(value: unknown): Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: {};
}

function stringList(value: unknown): string[] {
	return Array.isArray(value)
		? value.filter((item): item is string => typeof item === "string" && item.length > 0)
		: [];
}
