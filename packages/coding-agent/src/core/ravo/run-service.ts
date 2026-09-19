import { randomUUID } from "node:crypto";
import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import type { Model } from "@earendil-works/pi-ai";
import {
	type JudgeDeepVerdict,
	logRefinementOutcome,
	parseJudgeVerdict,
	RAVO_DEFAULT_CONFIG,
	RAVO_SEED_CRITERIA,
	type RefineFinalDecision,
	type RefinementRejectionCause,
	ravoFastScreen,
} from "../refinement/ravo.js";
import {
	applyRefinementProposal,
	carryObservedRecurrences,
	countValidRefinementEdits,
	formatHarnessStateForPrompt,
	type HarnessScope,
	type HarnessState,
	loadHarnessState,
	normalizeRefinementProposal,
	type RefinementProposal,
	saveHarnessState,
	withHarnessStateLockAsync,
	withoutObservedRecurrences,
} from "../refinement/refinement.js";
import { screenRefinementProposal } from "../refinement/skill-dry-run.js";
import type { RunAgentHandler } from "../run-agent.js";
import { toolforgeSrcRoots } from "../toolforge/ledger.js";
import { type ArcRunner, createArcEvaluatorSuite } from "./arc-agi-evaluator.js";
import { RavoArchive } from "./archive.js";
import { failureOpponentFingerprint, isFailureOpponentId, normalizeAssistedRavoState } from "./authority.js";
import { canonicalJson } from "./canonical-json.js";
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
import type { ExternalEvaluatorSuite } from "./external-evaluator.js";
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
	failureOpponentPassed,
	isRefereeOpponentId,
	type RefereeVerdict,
	refereeDetail,
	refereeOpponentFingerprint,
	refereeOpponentId,
	refereeOpponentPassed,
	refereeVerdictIsEvidence,
	replayCaseOf,
	skillImportsOf,
} from "./referee.js";
import { adjudicateFailureClaims } from "./referee-runner.js";
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
 * claims them in `addressedFingerprints` AND the referee fails to refute that
 * claim by re-running the recorded replay case. The commit gate applies the proposal
 * to the harness state and persists the stepped reducer state into
 * `HarnessState.ravo`, so lineage and weights continue from Assisted RAVO.
 * A local run reads and commits into the session store, a global run into the
 * global store, where the read, apply and save hold the harness state lock.
 * The commit keeps a regression recorded on the stored lineage meanwhile, and
 * refuses, stopping the run as `stale_cas`, when anything else in the stored
 * RAVO state changed since the run read it.
 * Each evaluated proposal reports its final decision once through
 * `logRefinementOutcome` with reason `ravo_run`; a commit is logged as
 * addressing a failure only for claims the judge named and the certificate did
 * not charge.
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
	/** Read the harness store of `scope`: the session store for a local run, the global store for a global one. */
	loadState: (scope: HarnessScope) => HarnessState;
	saveState: (scope: HarnessScope, state: HarnessState) => void;
	/**
	 * Run `fn` holding the cross-process lock of the `scope` store. The commit
	 * gate reads, applies and saves inside one synchronous `fn`, so nothing
	 * another writer saves lands between its read and its write.
	 */
	withStateLock: <T>(scope: HarnessScope, fn: () => T) => T | Promise<T>;
	onUpdate: (status: RavoRunStatus) => void;
	now?: () => number;
	/** Runs the ARC-AGI-3 harness for `evaluator: { kind: "arc-agi" }`; tests inject a fake. Defaults to `uv run main.py`. */
	arcRunner?: ArcRunner;
}

/**
 * The harness stores a RAVO run reads and commits into: `localDir` for a local
 * run, `globalDir` for a global one. Every session's ledger flush writes the
 * global store under the harness state lock, so a global commit takes it too;
 * the session store is left unlocked, as `/refine` leaves it.
 */
export function ravoRunHarnessStores(
	localDir: string,
	globalDir: string,
): Pick<RavoRunServiceDeps, "loadState" | "saveState" | "withStateLock"> {
	const dirOf = (scope: HarnessScope): string => (scope === "global" ? globalDir : localDir);
	return {
		loadState: (scope) => loadHarnessState(dirOf(scope), scope),
		saveState: (scope, state) => {
			saveHarnessState(dirOf(scope), state);
		},
		withStateLock: (scope, fn) => (scope === "global" ? withHarnessStateLockAsync(globalDir, fn) : fn()),
	};
}

/** JSON object produced by the implement/repair children: a RefinementProposal plus `addressedFingerprints`. */
export type RavoRunArtifact = { [key: string]: JsonValue };

export const RAVO_RUN_DEFAULTS = {
	maxRounds: 4,
	maxRepairs: 3,
	deadlineMs: 20 * 60 * 1000,
	// The only budget knob a user sets. Each child may spend whatever is still
	// unclaimed; a real AgentSession child costs ~50-100k tokens per call.
	tokenBudget: 1_500_000,
	// Admission floor per concurrent call (a child cannot start on scraps).
	reservationPerCall: 60_000,
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
		const state = deps.loadState(scope);
		const recurring = recurringFailures(state.failures ?? emptyFailureLedger());
		const activeFailureIds = [...new Set(recurring.map((record) => failureOpponentId(record.fingerprint)))];
		// A fingerprint with a verified reproduction gets a paired referee
		// opponent, so a claim the replay case refutes misses two criteria and
		// cannot slide through on epsilon. The controller's pool is fixed for the
		// run, so every candidate is added here; the commit gate keeps only the
		// ones the committed proposal's verdicts actually adjudicated.
		const refereeable = recurring.filter((record) => replayCaseOf(record) !== undefined);
		const refereeIds = [...new Set(refereeable.map((record) => refereeOpponentId(record.fingerprint)))];
		const baseRavo = normalizeAssistedRavoState(state.ravo);
		const ravoBaseline = (ravo: RavoState<JsonValue>): string => canonicalJson(withoutObservedRecurrences(ravo));
		const baseRavoBaseline = ravoBaseline(baseRavo);
		const config = { ...RAVO_DEFAULT_CONFIG };
		const arc = request.evaluator !== undefined && request.evaluator !== "judge" ? request.evaluator : undefined;
		// The only place the service names a benchmark: everything past this
		// point talks to the generic ExternalEvaluatorSuite.
		const suite: ExternalEvaluatorSuite | undefined = arc
			? createArcEvaluatorSuite({
					repoDir: arc.repoDir,
					game: arc.game,
					screenThreshold: config.screenThreshold,
					...(deps.arcRunner ? { runner: deps.arcRunner } : {}),
				})
			: undefined;
		const initialState: RavoState<JsonValue> = {
			...baseRavo,
			opponents: ravoExtendOpponents(baseRavo.opponents, [
				...activeFailureIds,
				...refereeIds,
				...(suite ? suite.criterionIds : []),
			]),
		};
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
		const evaluatorReference = suite ? await suite.promptSection() : undefined;
		const proposalSpec = (kind: "implement" | "repair") => ({
			prompt: (input: ProposalInput) => proposalPrompt(kind, input, recurring, evaluatorReference),
			validate: (value: unknown): ControllerProposal<JsonValue> => {
				proposalCount += 1;
				return {
					id: `${runId}-p${proposalCount}`,
					parentId: null,
					repairOf: null,
					artifact: validateArtifact(value, suite),
				};
			},
			scope: scopeOf(kind, { maxTurns: 8 }),
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

		const recurringIds = recurring.map((record) => record.fingerprint.id);
		const judgedClaims = new Map<string, string[]>();
		const judge = memoizedJudge(structured(judgeSpec(recurring, state, scopeOf("judge"))), (proposal, verdict) => {
			judgedClaims.set(
				proposal.id,
				judgedRavoRunClaims(proposal.artifact, recurringIds, verdict.addressedFingerprints),
			);
		});
		const logOutcome = (
			proposalId: string,
			decision: RefineFinalDecision,
			certificate: RavoGateCertificate,
			cause: RefinementRejectionCause = ravoRunRejectionCause(decision, certificate),
		): void => {
			const claimed = judgedClaims.get(proposalId) ?? [];
			logRefinementOutcome({
				proposalId,
				decision,
				addressed: decision === "commit" ? creditedRavoRunClaims(claimed, certificate) : claimed,
				deepScore: certificate.deep.score ?? 0,
				missed: certificate.missedCriterionIds.length,
				claimed: claimed.length,
				reason: "ravo_run",
				scope,
				cause,
			});
		};
		const hygieneIds = RAVO_SEED_CRITERIA.map((criterion) => criterion.id);
		const hygieneOpponents: EvaluationAdapter<JsonValue>[] = suite
			? [...suite.opponents, ...hygieneIds.map(notApplicableOpponent)]
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
		const referee = memoizedReferee(recurring);
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
						evaluate: async ({ proposal }, options) => {
							if (dormant) {
								return {
									status: "completed",
									value: { status: "pass", detail: "dormant: fingerprint is not currently recurring" },
									tokens: 0,
								};
							}
							const claimed = addressedFingerprintsOf(proposal.artifact).includes(fingerprint);
							const verdict = (await referee(proposal, options.signal)).get(fingerprint);
							const passed = failureOpponentPassed(claimed, verdict);
							return {
								status: "completed",
								value: {
									status: passed ? "pass" : "fail",
									detail:
										verdict && verdict.status !== "not_applicable"
											? `${fingerprint}: ${verdict.detail}`
											: claimed
												? `proposal claims to address ${fingerprint}`
												: `recurring failure ${fingerprint} is not addressed (set addressedFingerprints)`,
								},
								tokens: 0,
							};
						},
					};
				}),
			...initialState.opponents.criteria
				.filter((criterion) => isRefereeOpponentId(criterion.id))
				.map((criterion): EvaluationAdapter<JsonValue> => {
					const fingerprint = refereeOpponentFingerprint(criterion.id) ?? "";
					return {
						id: `opponent:${criterion.id}`,
						kind: "opponent",
						criterionId: criterion.id,
						evaluate: async ({ proposal }, options) => {
							const claimed = addressedFingerprintsOf(proposal.artifact).includes(fingerprint);
							const verdict = (await referee(proposal, options.signal)).get(fingerprint);
							return {
								status: "completed",
								value: {
									status: refereeOpponentPassed(claimed, verdict) ? "pass" : "fail",
									detail: refereeDetail(verdict, claimed),
								},
								tokens: 0,
							};
						},
					};
				}),
		];
		// A persisted criterion this run has no evaluator for (an `arc:*` opponent
		// on a judge run) would otherwise abstain and be charged on every proposal.
		const observed = new Set(opponents.map((adapter) => adapter.criterionId ?? adapter.id));
		for (const criterion of initialState.opponents.criteria) {
			if (!observed.has(criterion.id)) opponents.push(dormantOpponent(criterion.id));
		}
		const fast: EvaluationAdapter<JsonValue> = suite
			? suite.fast
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
		const deep: EvaluationAdapter<JsonValue> = suite
			? suite.deep
			: {
					id: "deep:judge",
					kind: "deep",
					evaluate: async (input, options) => {
						const judged = await judge(input.proposal, input.context, options);
						if (judged.status !== "completed") return judged;
						// The judge's own verdict, not a constant: a deep gate that
						// cannot fail reduces to comparing the judge's number against
						// the best number the judge ever gave.
						return {
							status: "completed",
							value: {
								status: judged.value.verdict,
								score: judged.value.score,
								detail: judged.value.rationale,
							},
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
					logOutcome(proposal.id, certificateSummary(stepped.certificate).status, stepped.certificate);
					return {
						accepted: false,
						detail: `reducer replay rejected (${stepped.certificate.rejection ?? "unknown"})`,
					};
				}
				// Pool membership for a referee opponent follows the verdict, as in
				// the assisted gate: one this run added whose verdict a replay did not
				// adjudicate passed without evidence and is not persisted.
				const verdicts = await referee(proposal, signal);
				const persistedIds = new Set(baseRavo.opponents.criteria.map((criterion) => criterion.id));
				const adjudicatedPool = {
					criteria: stepped.state.opponents.criteria.filter(
						(criterion) =>
							!isRefereeOpponentId(criterion.id) ||
							persistedIds.has(criterion.id) ||
							refereeVerdictIsEvidence(verdicts.get(refereeOpponentFingerprint(criterion.id) ?? "")),
					),
				};
				const claimedFingerprints = addressedFingerprintsOf(proposal.artifact).filter((fingerprint) =>
					activeFailureIds.includes(failureOpponentId(fingerprint)),
				);
				// No await between the read and the save: a write that lands in between would be overwritten.
				const applied = await deps.withStateLock(scope, () => {
					const current = deps.loadState(scope);
					// The certificate was earned against the lineage and weights read at run
					// start; another refinement's commit or evaluation since then voids it.
					if (ravoBaseline(normalizeAssistedRavoState(current.ravo)) !== baseRavoBaseline) {
						return { stale: true, failed: [], edits: 0 };
					}
					const result = applyRefinementProposal(current, refinement, {
						id: proposal.id,
						scope,
						baselineState: state,
						reason: "ravo_run",
					});
					const failed = result.appliedEdits.filter((edit) => !edit.applied);
					if (failed.length > 0) return { stale: false, failed, edits: 0 };
					current.ravo = carryObservedRecurrences(
						ravoMarkProvisional({ ...stepped.state, opponents: adjudicatedPool }, proposal.id, {
							claimedFingerprints,
						}),
						current.ravo,
					);
					deps.saveState(scope, current);
					return { stale: false, failed, edits: result.appliedEdits.length };
				});
				if (applied.stale) {
					logOutcome(proposal.id, "reject_deep", stepped.certificate, "baseline_changed");
					return { accepted: false, stale: true, detail: "the stored RAVO state changed during the run" };
				}
				if (applied.failed.length > 0) {
					logOutcome(proposal.id, "partial", stepped.certificate);
					return {
						accepted: false,
						detail: applied.failed
							.map((edit) => `${edit.action} ${edit.kind}:${edit.id}: ${edit.error ?? "not applied"}`)
							.join("; "),
					};
				}
				mirror = stepped.state;
				logOutcome(proposal.id, "commit", stepped.certificate);
				if (suite) await suite.persistCommitted(baseDir, runId, proposal.artifact);
				return { accepted: true, detail: `applied ${applied.edits} edits` };
			},
			maxRounds: request.maxRounds ?? RAVO_RUN_DEFAULTS.maxRounds,
			maxRepairs: request.maxRepairs ?? RAVO_RUN_DEFAULTS.maxRepairs,
			deadlineMs: request.deadlineMs ?? RAVO_RUN_DEFAULTS.deadlineMs,
			tokenBudget,
			reservationPerCall: Math.max(1, Math.min(RAVO_RUN_DEFAULTS.reservationPerCall, Math.floor(tokenBudget / 8))),
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
					logOutcome(event.proposalId, certificateSummary(event.certificate).status, event.certificate);
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
	verdict: JudgeDeepVerdict;
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
				: "Scope: local. Session progress and coordination facts for this session only. Record a transient condition (an open blocker, a pending rename, a service not yet registered) only together with how to re-check it, and update or delete it once it changes.",
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
	evaluatorReference: string | undefined,
): string {
	const fingerprints = recurring.map((record) => record.fingerprint.id);
	return [
		`# RAVO ${kind}`,
		"Everything you need is in this message. Do not search, browse, or call tools; write the answer directly.",
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
		...(evaluatorReference !== undefined ? [evaluatorReference] : []),
		`Return JSON with this shape:\n${PROPOSAL_SHAPE}\n${JSON_ONLY}`,
	].join("\n\n");
}

const JUDGE_HEADER = `# RAVO judge
You are the RAVO deep evaluator for Prime Agent's /refine subsystem. Score a proposed continual-harness refinement against the evidence. Judge the QUALITY OF THE RESULTING HARNESS STATE, not prose style.
Each criterion below is an opponent; list the ids the proposal fails in "failedCriteria". A recurring failure counts as addressed only if the edits would plausibly prevent that exact failure from recurring; never list a fingerprint the proposal merely mentions, and never one whose failure is outside the harness's control (a provider outage, a user denial, a flaky network). A fingerprint marked replay=verified is re-executed after you answer when a skill the proposal writes imports the module its replay case probes, so a claim on one whose failure has not actually stopped loses the gate.
"verdict" is your decision on the deep gate: "pass" if this candidate is at least as good a harness state as the current champion, "fail" if it is worse, "abstain" if the evidence given cannot decide. A non-pass verdict rejects the candidate whatever it scored.`;

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
				`Return JSON: { "verdict": "pass"|"fail"|"abstain", "score": 0-100, "failedCriteria": ["id"], "addressedFingerprints": ["fingerprint id"], "rationale": "one or two sentences" }. ${JSON_ONLY}`,
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
export function validateArtifact(value: unknown, suite?: ExternalEvaluatorSuite): RavoRunArtifact {
	const record = objectRecord(value);
	if (!Array.isArray(record.edits)) throw new Error("proposal output requires an edits array");
	const proposal = normalizeRefinementProposal(record);
	const artifact: Record<string, unknown> = {
		...proposal,
		addressedFingerprints: stringList(record.addressedFingerprints),
		...(suite?.artifactFields(record) ?? {}),
	};
	return JSON.parse(JSON.stringify(artifact)) as RavoRunArtifact;
}

function validateJudge(value: unknown): JudgeVerdict {
	const record = objectRecord(value);
	const rawScore = typeof record.score === "number" ? record.score : Number(record.score);
	if (!Number.isFinite(rawScore)) throw new Error("judge output requires a numeric score");
	return {
		verdict: parseJudgeVerdict(record.verdict ?? record.status),
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

/** Fingerprint ids a ravo.run proposal is logged as claiming: claimed by the proposal, recurring in this run's ledger, and named by the judge. */
export function judgedRavoRunClaims(
	artifact: JsonValue,
	recurringFingerprintIds: readonly string[],
	judgeAddressed: readonly string[],
): string[] {
	const recurring = new Set(recurringFingerprintIds);
	// The judge sees recurring failures only as `failure:<fp>`.
	const judged = new Set(judgeAddressed.map((id) => failureOpponentFingerprint(id) ?? id));
	return [...new Set(addressedFingerprintsOf(artifact))]
		.filter((id) => recurring.has(id) && judged.has(id))
		.sort((left, right) => left.localeCompare(right));
}

/** The claims a certificate credited: none whose failure or referee criterion it counted as missed. */
export function creditedRavoRunClaims(claimed: readonly string[], certificate: RavoGateCertificate): string[] {
	const missed = new Set(certificate.missedCriterionIds);
	return claimed.filter((id) => !missed.has(failureOpponentId(id)) && !missed.has(refereeOpponentId(id)));
}

function summaryOf(artifact: JsonValue): string {
	const summary = objectRecord(artifact).summary;
	return typeof summary === "string" ? summary : "(no summary)";
}

/** What decided a rejected ravo.run proposal: the screen, a deep evaluator that never answered, or the gate. */
function ravoRunRejectionCause(
	decision: RefineFinalDecision,
	certificate: RavoGateCertificate,
): RefinementRejectionCause {
	if (decision === "reject_screen") return "screen";
	return certificate.deep.status === "error" ? "judge_unavailable" : "gate";
}

/** A pool criterion no evaluator in this run observes passes and keeps its weight. */
function dormantOpponent(criterionId: string): EvaluationAdapter<JsonValue> {
	return {
		id: `opponent:${criterionId}`,
		kind: "opponent",
		criterionId,
		evaluate: async () => ({
			status: "completed",
			value: { status: "pass", detail: "dormant: not evaluated by this run" },
			tokens: 0,
		}),
	};
}

/** Hygiene criteria judge harness prose; an external candidate has none, so they pass vacuously and keep their weights. */
function notApplicableOpponent(criterionId: string): EvaluationAdapter<JsonValue> {
	return {
		id: `opponent:${criterionId}`,
		kind: "opponent",
		criterionId,
		evaluate: async () => ({
			status: "completed",
			value: { status: "pass", detail: "not applicable to an external evaluator candidate" },
			tokens: 0,
		}),
	};
}

/**
 * One referee pass per proposal, shared by the paired `failure:<fp>` and
 * `referee:<fp>` opponents and the commit gate: the replay cases are
 * re-executed once and every reader sees the same verdicts. Only cases probing
 * a module a skill of the proposal imports are replayed, with the toolforge
 * roots on the path. Subprocesses cost no tokens.
 */
function memoizedReferee(
	records: readonly FailureRecord[],
): (proposal: ControllerProposal<JsonValue>, signal: AbortSignal) => Promise<Map<string, RefereeVerdict>> {
	const pending = new Map<string, Promise<Map<string, RefereeVerdict>>>();
	return (proposal, signal) => {
		let shared = pending.get(proposal.id);
		if (!shared) {
			shared = adjudicateFailureClaims(records, addressedFingerprintsOf(proposal.artifact), {
				signal,
				skillImports: skillImportsOf(proposalOf(proposal.artifact).edits),
				sysPath: toolforgeSrcRoots(),
			}).then((verdicts) => new Map(verdicts.map((verdict) => [verdict.fingerprintId, verdict])));
			pending.set(proposal.id, shared);
		}
		return shared;
	};
}

/**
 * One judge call per proposal, shared by the deep gate and the hygiene opponents. Tokens are reported once.
 * `onCompleted` sees each completed verdict before any reader of the shared call resumes.
 */
function memoizedJudge(
	call: ChildCall<JudgeInput, JudgeVerdict>,
	onCompleted?: (proposal: ControllerProposal<JsonValue>, verdict: JudgeVerdict) => void,
): (
	proposal: ControllerProposal<JsonValue>,
	context: BoundedContextView,
	options: RavoChildCallOptions,
) => Promise<SettledChildResult<JudgeVerdict>> {
	const pending = new Map<string, Promise<SettledChildResult<JudgeVerdict>>>();
	return async (proposal, context, options) => {
		let shared = pending.get(proposal.id);
		if (!shared) {
			shared = call({ proposal, context }, options).then((result): SettledChildResult<JudgeVerdict> => {
				if (result.status === "deferred") {
					return { status: "error", tokens: 0, error: "judge returned a deferred result" };
				}
				if (result.status === "completed") {
					try {
						onCompleted?.(proposal, result.value);
					} catch {
						// Recording a claim never decides the gate.
					}
				}
				return result;
			});
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
