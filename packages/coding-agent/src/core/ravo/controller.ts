import { runWithTraceContext, type Span, type SpanAttributes, startSpan } from "@earendil-works/pi-ai";
import type { RunAgentStatus } from "../run-agent.js";
import { type RavoArchive, RavoStaleCommitError } from "./archive.js";
import { canonicalJson, sha256 } from "./canonical-json.js";
import {
	type BoundedContextView,
	buildBoundedContextView,
	type ContextArchive,
	type ContextViewLimits,
} from "./context-view.js";
import type { DecisionAllocation, ErrorBudgetLedger, ErrorBudgetLedgerSnapshot } from "./error-budget-ledger.js";
import {
	type GateStatus,
	type JsonValue,
	type RavoConfig,
	type RavoCriterionObservation,
	type RavoEvaluation,
	type RavoGateCertificate,
	type RavoState as ReducerState,
	ravoStep,
} from "./reducer.js";
import { type TokenReservationAdmission, TokenReservationLedger } from "./token-reservation-ledger.js";
import type { ChampionCas } from "./types.js";

export type RavoPhase =
	| "inspect"
	| "plan"
	| "implement"
	| "evaluate"
	| "diagnose"
	| "repair"
	| "commit_gate"
	| "accepted"
	| "stopped";
export type RavoStopReason =
	| "accepted"
	| "round_limit"
	| "repair_limit"
	| "deadline"
	| "budget"
	| "cancelled"
	| "stale_cas";

export interface ControllerProposal<T extends JsonValue = JsonValue> {
	readonly id: string;
	readonly parentId: string | null;
	readonly repairOf: string | null;
	readonly artifact: T;
}
export interface InspectionFindings {
	readonly summary: string;
	readonly facts: readonly string[];
}
export interface RavoPlan {
	readonly id: string;
	readonly steps: readonly string[];
	readonly supervisorAdvice?: string;
}
export interface DiagnosticFeedback {
	proposalId: string;
	rejection: string;
	findings: readonly { source: string; status: GateStatus; detail: string }[];
}
export interface RavoChildCallOptions {
	signal: AbortSignal;
	tokenBudget: number;
}
export type RavoChildResult<T> =
	| { status: "completed"; value: T; tokens: number }
	| { status: Exclude<RunAgentStatus, "completed">; tokens: number; error?: string }
	| { status: "deferred"; handle: string; wait: (options: RavoChildCallOptions) => Promise<RavoChildResult<T>> };
export type ChildCall<TInput, TOutput> = (
	input: TInput,
	options: RavoChildCallOptions,
) => Promise<RavoChildResult<TOutput>>;
export interface EvaluationAdapter<T extends JsonValue = JsonValue> {
	id: string;
	kind: "fast" | "deep" | "opponent";
	criterionId?: string;
	probabilistic?: boolean;
	allocation?: (input: { proposalId: string; round: number }) => DecisionAllocation;
	evaluate: ChildCall<
		{ proposal: ControllerProposal<T>; context: BoundedContextView },
		{ status: GateStatus; score?: number; detail?: string }
	>;
}
export interface SupervisorSignal<T extends JsonValue = JsonValue> {
	candidate?: ControllerProposal<T>;
	plan: RavoPlan;
	lastEvaluation?: RavoEvaluation;
	trajectory: readonly RavoGateCertificate[];
}
export interface RavoControllerCheckpoint<T extends JsonValue = JsonValue> {
	runId: string;
	phase: RavoPhase;
	round: number;
	repairs: number;
	state: ReducerState<T>;
	inspection?: InspectionFindings;
	plan?: RavoPlan;
	lastEvaluation?: RavoEvaluation;
	workerHandle?: string;
	candidate?: ControllerProposal<T>;
	feedback?: DiagnosticFeedback;
	certificates: RavoGateCertificate[];
	spentTokens: number;
	archiveBaseline?: ChampionCas;
	errorBudget: ErrorBudgetLedgerSnapshot;
}
export type RavoProgressEvent =
	| { type: "phase"; phase: RavoPhase; round: number }
	| { type: "proposal"; proposalId: string; parentId: string | null; repairOf: string | null }
	| { type: "evaluation"; proposalId: string; certificate: RavoGateCertificate }
	| { type: "supervisor"; intervened: boolean; detail?: string }
	| RavoCandidatesEvent
	| { type: "stopped"; reason: RavoStopReason };
/** Emitted once per best-of-n implement round, before the winning `proposal` event. */
export interface RavoCandidatesEvent {
	type: "candidates";
	round: number;
	/** `implementCandidates` as configured. */
	requested: number;
	/** Candidates that were admitted by the token ledger, implemented and screened. */
	considered: number;
	selected: string;
	candidates: readonly {
		proposalId: string;
		status: GateStatus;
		score?: number;
		/** Tokens the implement call for this candidate reported (its fast screen is billed separately). */
		tokens: number;
	}[];
}

export interface RavoControllerOptions<T extends JsonValue = JsonValue> {
	runId: string;
	context: ContextArchive;
	contextLimits: ContextViewLimits;
	initialState: ReducerState<T>;
	reducerConfig: RavoConfig;
	archive: RavoArchive;
	ledger: ErrorBudgetLedger;
	inspect: ChildCall<{ context: BoundedContextView }, InspectionFindings>;
	plan: ChildCall<
		{ context: BoundedContextView; inspection: InspectionFindings; feedback?: DiagnosticFeedback },
		RavoPlan
	>;
	implement: ChildCall<
		{ context: BoundedContextView; inspection: InspectionFindings; plan: RavoPlan; workerHandle?: string },
		ControllerProposal<T>
	>;
	repair: ChildCall<
		{
			context: BoundedContextView;
			candidate: ControllerProposal<T>;
			feedback: DiagnosticFeedback;
			plan: RavoPlan;
			workerHandle?: string;
		},
		ControllerProposal<T>
	>;
	evaluators: readonly EvaluationAdapter<T>[];
	commitGate: (input: {
		proposal: ControllerProposal<T>;
		certificate: RavoGateCertificate;
		signal: AbortSignal;
	}) => Promise<{ accepted: boolean; detail?: string }>;
	supervisor?: ChildCall<SupervisorSignal<T>, { intervene: boolean; advice?: string }>;
	shouldConsultSupervisor?: (signal: SupervisorSignal<T>) => boolean;
	maxRounds: number;
	maxRepairs: number;
	deadlineMs: number;
	tokenBudget: number;
	reservationPerCall: number;
	concurrency: number;
	/**
	 * Number of implement candidates to fan out per implement round (default 1,
	 * max 8). Each is screened by the fast evaluator and only the best one is
	 * evaluated further; repair rounds always produce a single candidate.
	 */
	implementCandidates?: number;
	signal?: AbortSignal;
	now?: () => number;
	onProgress?: (event: RavoProgressEvent) => void;
	checkpoint?: RavoControllerCheckpoint<T>;
	onCheckpoint?: (checkpoint: RavoControllerCheckpoint<T>) => void | Promise<void>;
}
export interface RavoControllerResult<T extends JsonValue = JsonValue> {
	reason: RavoStopReason;
	checkpoint: RavoControllerCheckpoint<T>;
	certificate?: RavoGateCertificate;
	gateCertificateDigest?: string;
}

export async function runRavoController<T extends JsonValue>(
	options: RavoControllerOptions<T>,
): Promise<RavoControllerResult<T>> {
	return inRavoSpan(
		"ravo.run",
		{ "ravo.run_id": options.runId, "ravo.resumed": Boolean(options.checkpoint) },
		(runSpan) => runController(options, runSpan),
	);
}

async function runController<T extends JsonValue>(
	options: RavoControllerOptions<T>,
	runSpan: Span,
): Promise<RavoControllerResult<T>> {
	validateOptions(options);
	const now = options.now ?? Date.now;
	const started = now();
	const context = buildBoundedContextView(options.context, options.contextLimits);
	const cp: RavoControllerCheckpoint<T> = options.checkpoint
		? structuredClone(options.checkpoint)
		: {
				runId: options.runId,
				phase: "inspect" as const,
				round: 0,
				repairs: 0,
				state: structuredClone(options.initialState),
				certificates: [],
				spentTokens: 0,
				errorBudget: options.ledger.toJSON(),
			};
	if (cp.runId !== options.runId) throw new Error("checkpoint runId mismatch");
	if (options.checkpoint) options.ledger.restore(cp.errorBudget);
	await options.archive.initialize();
	if (!options.checkpoint)
		await options.archive.append("run", { runId: options.runId, contextDigest: context.sha256 });
	let roundSpan: Span | undefined;
	const emit = (event: RavoProgressEvent): void => {
		try {
			options.onProgress?.(event);
		} catch {
			// Progress listeners are observers; a throwing listener must not abort the run.
		}
	};
	const persistCheckpoint = async (): Promise<void> => {
		cp.errorBudget = options.ledger.toJSON();
		const checkpoint = structuredClone(cp);
		await options.archive.append("run", { runId: options.runId, checkpoint: checkpointJson(checkpoint) });
		await options.onCheckpoint?.(checkpoint);
	};
	const setPhase = async (phase: RavoPhase): Promise<void> => {
		cp.phase = phase;
		roundSpan?.setAttributes({ "ravo.phase": phase });
		emit({ type: "phase", phase, round: cp.round });
		await persistCheckpoint();
	};
	const runSummary = (reason: RavoStopReason): SpanAttributes => ({
		"ravo.reason": reason,
		"ravo.rounds": cp.round,
		"ravo.repairs": cp.repairs,
		"ravo.spent_tokens": cp.spentTokens,
	});
	const stop = async (reason: RavoStopReason, certificate?: RavoGateCertificate): Promise<RavoControllerResult<T>> => {
		cp.phase = reason === "accepted" ? "accepted" : "stopped";
		runSpan.setAttributes(runSummary(reason));
		await options.archive.append("stop", { runId: options.runId, reason, round: cp.round });
		emit({ type: "stopped", reason });
		return { reason, checkpoint: cp, ...(certificate ? { certificate } : {}) };
	};
	const abort = new AbortController();
	const relayAbort = (): void => abort.abort();
	if (options.signal?.aborted) abort.abort();
	else options.signal?.addEventListener("abort", relayAbort, { once: true });
	// The reservation is only an admission floor for concurrent calls; a child may
	// spend everything that is still unclaimed, so the total budget is the one knob.
	// The ledger is rebuilt from the checkpoint so a resumed run keeps its spend.
	const tokens = new TokenReservationLedger(options.tokenBudget, cp.spentTokens);
	const admit = (): TokenReservationAdmission => {
		if (abort.signal.aborted) throw new Stop("cancelled");
		if (now() - started >= options.deadlineMs) throw new Stop("deadline");
		const gate = tokens.gate(options.reservationPerCall);
		if (!gate.ok) throw new Stop("budget");
		return gate;
	};
	/**
	 * Run one admitted child call and settle its token usage. A deferred (retained
	 * worker) result binds its handle to the checkpoint unless `bindHandle` is false,
	 * in which case the handle is only returned so the caller can bind the one it keeps.
	 */
	const settle = async <I, O>(
		admission: TokenReservationAdmission,
		fn: ChildCall<I, O>,
		input: I,
		bindHandle = true,
	): Promise<{ value: O; tokens: number; handle?: string }> => {
		let closed = false;
		let handle: string | undefined;
		try {
			const callOptions: RavoChildCallOptions = { signal: abort.signal, tokenBudget: admission.available };
			let result = await fn(input, callOptions);
			if (result.status === "deferred") {
				handle = result.handle;
				if (bindHandle) cp.workerHandle = handle;
				const resumed = await result.wait(callOptions);
				if (resumed.status === "deferred") throw new Error("nested deferred child result");
				result = resumed;
			}
			if (!Number.isSafeInteger(result.tokens) || result.tokens < 0)
				throw new Error("child returned invalid token usage");
			tokens.settle(admission.reservationId, result.tokens);
			closed = true;
			cp.spentTokens = tokens.spent;
			if (result.status !== "completed") {
				if (abort.signal.aborted) throw new Stop("cancelled");
				if (result.status === "budget_exceeded") throw new Stop("budget");
				throw new ChildFailure(result.status, result.error);
			}
			return { value: result.value, tokens: result.tokens, ...(handle ? { handle } : {}) };
		} finally {
			if (!closed) tokens.release(admission.reservationId);
		}
	};
	const call = async <I, O>(fn: ChildCall<I, O>, input: I): Promise<O> => (await settle(admit(), fn, input)).value;
	const proposeOne = (inspection: InspectionFindings, plan: RavoPlan): Promise<ProposalOutcome<T>> =>
		inRavoSpan(
			"ravo.proposal",
			{ "ravo.round": cp.round, "ravo.kind": cp.feedback && cp.candidate ? "repair" : "implement" },
			async (span) => {
				const spentBefore = cp.spentTokens;
				const proposal =
					cp.feedback && cp.candidate
						? await call(options.repair, {
								context,
								candidate: cp.candidate,
								feedback: cp.feedback,
								plan,
								...(cp.workerHandle ? { workerHandle: cp.workerHandle } : {}),
							})
						: await call(options.implement, {
								context,
								inspection,
								plan,
								...(cp.workerHandle ? { workerHandle: cp.workerHandle } : {}),
							});
				span.setAttributes({
					"ravo.proposal_id": proposal.id,
					"ravo.candidate_tokens": cp.spentTokens - spentBefore,
				});
				validateProposal(proposal, cp.candidate, Boolean(cp.feedback));
				return { proposal };
			},
		);
	const evaluateOne = async (
		adapter: EvaluationAdapter<T>,
		candidate: ControllerProposal<T>,
	): Promise<{ adapter: EvaluationAdapter<T>; result: { status: GateStatus; score?: number; detail?: string } }> => {
		let allocation: DecisionAllocation | undefined;
		if (adapter.kind === "deep" && adapter.probabilistic) {
			try {
				allocation = adapter.allocation?.({ proposalId: candidate.id, round: cp.round });
				if (!allocation)
					return {
						adapter,
						result: {
							status: "error" as const,
							detail: "probabilistic deep evaluation was not preallocated",
						},
					};
				options.ledger.allocate(allocation);
				await persistCheckpoint();
			} catch (error) {
				return {
					adapter,
					result: {
						status: "error" as const,
						detail: error instanceof Error ? error.message : String(error),
					},
				};
			}
		}
		try {
			let result = await call(adapter.evaluate, { proposal: candidate, context });
			if (allocation) {
				const record = options.ledger.recordEvaluation({
					decisionId: allocation.decisionId,
					passed: result.status === "pass",
					calibrationId: allocation.calibrationId,
				});
				await persistCheckpoint();
				if (!record.probabilisticallyAccepted)
					result = {
						status: "error",
						detail: record.rejectionReason ?? "probabilistic evaluation rejected",
					};
			}
			return { adapter, result };
		} catch (error) {
			if (error instanceof Stop) throw error;
			return {
				adapter,
				result: { status: "error" as const, detail: error instanceof Error ? error.message : String(error) },
			};
		}
	};
	const evaluateTraced = (adapter: EvaluationAdapter<T>, candidate: ControllerProposal<T>) =>
		inRavoSpan(
			"ravo.evaluation",
			{ "ravo.proposal_id": candidate.id, "ravo.evaluator": adapter.id, "ravo.evaluator_kind": adapter.kind },
			async (span) => {
				const observation = await evaluateOne(adapter, candidate);
				span.setAttributes({ "ravo.verdict": observation.result.status });
				return observation;
			},
		);
	/**
	 * Best-of-n implement: fan out `n` implement calls, screen each with the fast
	 * evaluator and keep the lexicographically best (pass ≻ non-pass, higher score,
	 * fewer tokens, lower index). Every candidate is admitted through the token
	 * ledger up front; when a later candidate cannot be admitted the round proceeds
	 * with the ones that were (the first refusal still stops the run on budget, as
	 * a single implement call would). The winner's fast screen is reused by the
	 * evaluate phase so the fast evaluator runs exactly once per candidate.
	 */
	const proposeBestOf = (
		inspection: InspectionFindings,
		plan: RavoPlan,
		requested: number,
	): Promise<ProposalOutcome<T>> =>
		inRavoSpan(
			"ravo.proposal",
			{ "ravo.round": cp.round, "ravo.kind": "implement", "ravo.candidates_requested": requested },
			async (span) => {
				const spentBefore = cp.spentTokens;
				const admissions: TokenReservationAdmission[] = [];
				try {
					for (let index = 0; index < requested; index += 1) admissions.push(admit());
				} catch (error) {
					if (!(error instanceof Stop && error.reason === "budget" && admissions.length > 0)) {
						for (const admission of admissions) tokens.release(admission.reservationId);
						throw error;
					}
				}
				// Only the first candidate may continue a retained worker: concurrent
				// `continue` calls on one handle would interleave, so the rest spawn fresh.
				const input = { context, inspection, plan };
				const retained = cp.workerHandle ? { ...input, workerHandle: cp.workerHandle } : input;
				const fast = options.evaluators.find((adapter) => adapter.kind === "fast");
				if (!fast) throw new Error("fast evaluator is required");
				const outcomes = await concurrentMap(
					admissions,
					options.concurrency,
					async (admission, index): Promise<ScreenedCandidate<T> | { error: unknown }> => {
						try {
							return await inRavoSpan(
								"ravo.candidate",
								{ "ravo.round": cp.round, "ravo.candidate_index": index },
								async (candidateSpan) => {
									const implemented = await settle(
										admission,
										options.implement,
										index === 0 ? retained : input,
										false,
									);
									validateProposal(implemented.value, cp.candidate, false);
									const screen = await evaluateTraced(fast, implemented.value);
									candidateSpan.setAttributes({
										"ravo.proposal_id": implemented.value.id,
										"ravo.candidate_tokens": implemented.tokens,
										"ravo.verdict": screen.result.status,
									});
									return {
										index,
										proposal: implemented.value,
										tokens: implemented.tokens,
										screen,
										...(implemented.handle ? { handle: implemented.handle } : {}),
									};
								},
							);
						} catch (error) {
							// Every candidate must settle before the round reacts, so failures are
							// collected here and rethrown once the fan-out has drained.
							return { error };
						}
					},
				);
				const failures = outcomes.filter((o): o is { error: unknown } => "error" in o).map((o) => o.error);
				const stop = failures.find((error): error is Stop => error instanceof Stop);
				if (stop) throw stop;
				if (failures.length > 0) throw failures[0];
				const screened = outcomes.filter((o): o is ScreenedCandidate<T> => !("error" in o));
				const ids = new Set(screened.map((c) => c.proposal.id));
				if (ids.size !== screened.length) throw new Error("implement candidates must have distinct proposal ids");
				const selected = [...screened].sort(compareCandidates)[0];
				if (!selected) throw new Stop("budget");
				if (selected.handle) cp.workerHandle = selected.handle;
				span.setAttributes({
					"ravo.proposal_id": selected.proposal.id,
					"ravo.candidate_tokens": selected.tokens,
					"ravo.candidates_considered": screened.length,
					"ravo.fan_out_tokens": cp.spentTokens - spentBefore,
				});
				return {
					proposal: selected.proposal,
					screen: selected.screen,
					candidates: {
						type: "candidates",
						round: cp.round,
						requested,
						considered: screened.length,
						selected: selected.proposal.id,
						candidates: screened.map((c) => ({
							proposalId: c.proposal.id,
							status: c.screen.result.status,
							...(c.screen.result.score !== undefined ? { score: c.screen.result.score } : {}),
							tokens: c.tokens,
						})),
					},
				};
			},
		);
	const propose = (inspection: InspectionFindings, plan: RavoPlan): Promise<ProposalOutcome<T>> => {
		const requested = options.implementCandidates ?? 1;
		return requested > 1 && !(cp.feedback && cp.candidate)
			? proposeBestOf(inspection, plan, requested)
			: proposeOne(inspection, plan);
	};
	const commitGate = (
		candidate: ControllerProposal<T>,
		certificate: RavoGateCertificate,
	): Promise<{ accepted: false; detail?: string } | { accepted: true; digest: string }> =>
		inRavoSpan(
			"ravo.evaluation",
			{ "ravo.proposal_id": candidate.id, "ravo.evaluator": "commit_gate", "ravo.evaluator_kind": "commit_gate" },
			async (span) => {
				const gate = await options.commitGate({ proposal: candidate, certificate, signal: abort.signal });
				span.setAttributes({ "ravo.verdict": gate.accepted ? "accepted" : "rejected" });
				if (!gate.accepted) return { accepted: false, ...(gate.detail ? { detail: gate.detail } : {}) };
				const digest = sha256(
					canonicalJson({
						proposal: candidate,
						certificate,
						errorBudget: JSON.parse(options.ledger.serialize()) as JsonValue,
					}),
				);
				span.setAttributes({ "ravo.certificate_digest": digest });
				return { accepted: true, digest };
			},
		);
	const runRound = async (span: Span): Promise<RavoControllerResult<T> | undefined> => {
		roundSpan = span;
		try {
			await setPhase("inspect");
			if (!cp.inspection) cp.inspection = await call(options.inspect, { context });
			await setPhase("plan");
			cp.plan = await call(options.plan, {
				context,
				inspection: cp.inspection,
				...(cp.feedback ? { feedback: cp.feedback } : {}),
			});
			const supervisorSignal: SupervisorSignal<T> = {
				candidate: cp.candidate,
				plan: cp.plan,
				...(cp.lastEvaluation ? { lastEvaluation: cp.lastEvaluation } : {}),
				trajectory: cp.certificates,
			};
			let advice: string | undefined;
			if (options.supervisor && (options.shouldConsultSupervisor?.(supervisorSignal) ?? false)) {
				try {
					const result = await call(options.supervisor, supervisorSignal);
					advice = result.intervene ? result.advice : undefined;
					if (advice) cp.plan = { ...cp.plan, supervisorAdvice: advice };
					emit({
						type: "supervisor",
						intervened: result.intervene,
						...(result.advice ? { detail: result.advice } : {}),
					});
				} catch (error) {
					if (error instanceof Stop) throw error;
					emit({ type: "supervisor", intervened: false, detail: "supervisor unavailable" });
				}
			} else emit({ type: "supervisor", intervened: false });
			await setPhase(cp.feedback ? "repair" : "implement");
			const proposed = await propose(cp.inspection, cp.plan);
			const candidate = proposed.proposal;
			cp.candidate = candidate;
			cp.feedback = undefined;
			if (proposed.candidates) emit(proposed.candidates);
			emit({
				type: "proposal",
				proposalId: candidate.id,
				parentId: candidate.parentId,
				repairOf: candidate.repairOf,
			});
			await options.archive.append("proposal", {
				runId: options.runId,
				proposalId: candidate.id,
				parentId: candidate.parentId,
				repairOf: candidate.repairOf,
			});
			const archiveState = await options.archive.recover();
			cp.archiveBaseline = {
				revision: archiveState.revision,
				championDigest: archiveState.championDigest,
			};
			await setPhase("evaluate");
			// A best-of-n winner was already screened while it was selected; only the
			// remaining evaluators run here so the fast evaluator is not paid twice.
			const screen = proposed.screen;
			const pending = screen
				? options.evaluators.filter((adapter) => adapter !== screen.adapter)
				: options.evaluators;
			const observations = [
				...(screen ? [screen] : []),
				...(await concurrentMap(pending, options.concurrency, (adapter) => evaluateTraced(adapter, candidate))),
			];
			const evaluation = assembleEvaluation(candidate.id, observations);
			cp.lastEvaluation = evaluation;
			const stepped = ravoStep(
				cp.state,
				{ id: candidate.id, artifact: candidate.artifact },
				evaluation,
				options.reducerConfig,
			);
			cp.certificates.push(stepped.certificate);
			emit({ type: "evaluation", proposalId: candidate.id, certificate: stepped.certificate });
			await options.archive.append("evaluation", {
				runId: options.runId,
				proposalId: candidate.id,
				certificate: stepped.certificate as unknown as JsonValue,
			});
			if (stepped.certificate.committed) {
				await setPhase("commit_gate");
				const gate = await commitGate(candidate, stepped.certificate);
				if (gate.accepted) {
					const baseline = cp.archiveBaseline;
					if (!baseline) throw new Error("archive CAS baseline was not bound before evaluation");
					const digest = gate.digest;
					try {
						await options.archive.accept(
							{ runId: options.runId, proposalId: candidate.id, certificateDigest: digest },
							baseline,
							digest,
						);
					} catch (error) {
						if (error instanceof RavoStaleCommitError) {
							span.setAttributes({ "ravo.outcome": "stopped", "ravo.reason": "stale_cas" });
							return stop("stale_cas", stepped.certificate);
						}
						throw error;
					}
					cp.state = stepped.state;
					cp.errorBudget = options.ledger.toJSON();
					cp.phase = "accepted";
					span.setAttributes({ "ravo.outcome": "accepted", "ravo.certificate_digest": digest });
					runSpan.setAttributes({ ...runSummary("accepted"), "ravo.certificate_digest": digest });
					emit({ type: "stopped", reason: "accepted" });
					return {
						reason: "accepted",
						checkpoint: cp,
						certificate: stepped.certificate,
						gateCertificateDigest: digest,
					};
				}
				cp.feedback = diagnostic(stepped.certificate, gate.detail ?? "external commit gate rejected");
			} else cp.feedback = diagnostic(stepped.certificate);
			span.setAttributes({ "ravo.outcome": "rejected" });
			cp.state = { ...cp.state, evaluatedProposalIds: stepped.state.evaluatedProposalIds };
			await options.archive.append("reject", {
				runId: options.runId,
				proposalId: candidate.id,
				feedback: cp.feedback as unknown as JsonValue,
			});
			await setPhase("diagnose");
			cp.repairs += 1;
			if (cp.repairs > options.maxRepairs) {
				span.setAttributes({ "ravo.reason": "repair_limit" });
				return stop("repair_limit", stepped.certificate);
			}
			return undefined;
		} catch (error) {
			if (error instanceof Stop) span.setAttributes({ "ravo.outcome": "stopped" });
			throw error;
		} finally {
			roundSpan = undefined;
		}
	};
	try {
		while (cp.round < options.maxRounds) {
			if (abort.signal.aborted) throw new Stop("cancelled");
			if (now() - started >= options.deadlineMs) throw new Stop("deadline");
			cp.round += 1;
			const result = await inRavoSpan("ravo.round", { "ravo.round": cp.round }, runRound);
			if (result) return result;
		}
		return stop("round_limit", cp.certificates.at(-1));
	} catch (error) {
		if (error instanceof Stop) return stop(error.reason, cp.certificates.at(-1));
		throw error;
	} finally {
		options.signal?.removeEventListener("abort", relayAbort);
		abort.abort();
	}
}

/**
 * Run `fn` inside a child span of the ambient trace context. A thrown
 * {@link Stop} is a normal terminal outcome (deadline/budget/cancel), so the
 * span ends `ok` carrying `ravo.reason`; any other throw ends it `error`.
 * Tracing never alters control flow: the value or error is passed through.
 */
async function inRavoSpan<T>(name: string, attrs: SpanAttributes, fn: (span: Span) => Promise<T>): Promise<T> {
	const span = startSpan(name, attrs);
	try {
		const value = await runWithTraceContext(span.context, () => fn(span));
		span.end();
		return value;
	} catch (error) {
		if (error instanceof Stop) span.setAttributes({ "ravo.reason": error.reason });
		else span.recordError(error);
		span.end();
		throw error;
	}
}

const MAX_IMPLEMENT_CANDIDATES = 8;
type Observation<T extends JsonValue> = {
	adapter: EvaluationAdapter<T>;
	result: { status: GateStatus; score?: number; detail?: string };
};
interface ScreenedCandidate<T extends JsonValue> {
	index: number;
	proposal: ControllerProposal<T>;
	tokens: number;
	screen: Observation<T>;
	/** Retained worker handle that produced the candidate, if the implement call deferred. */
	handle?: string;
}
interface ProposalOutcome<T extends JsonValue> {
	proposal: ControllerProposal<T>;
	/** Fast screen already taken for `proposal` during best-of-n selection. */
	screen?: Observation<T>;
	candidates?: RavoCandidatesEvent;
}
/** Lexicographic best-of-n order: fast pass ≻ non-pass, higher score, fewer tokens, lower index. */
function compareCandidates<T extends JsonValue>(a: ScreenedCandidate<T>, b: ScreenedCandidate<T>): number {
	const passA = a.screen.result.status === "pass" ? 0 : 1;
	const passB = b.screen.result.status === "pass" ? 0 : 1;
	if (passA !== passB) return passA - passB;
	const scoreA = a.screen.result.score ?? Number.NEGATIVE_INFINITY;
	const scoreB = b.screen.result.score ?? Number.NEGATIVE_INFINITY;
	if (scoreA !== scoreB) return scoreB > scoreA ? 1 : -1;
	if (a.tokens !== b.tokens) return a.tokens - b.tokens;
	return a.index - b.index;
}
class ChildFailure extends Error {
	constructor(
		readonly status: Exclude<RunAgentStatus, "completed">,
		detail?: string,
	) {
		super(detail ?? status);
	}
}
class Stop extends Error {
	constructor(readonly reason: Extract<RavoStopReason, "deadline" | "budget" | "cancelled">) {
		super(reason);
	}
}
function validateOptions<T extends JsonValue>(o: RavoControllerOptions<T>): void {
	for (const [name, value] of [
		["maxRounds", o.maxRounds],
		["maxRepairs", o.maxRepairs],
		["deadlineMs", o.deadlineMs],
		["tokenBudget", o.tokenBudget],
		["reservationPerCall", o.reservationPerCall],
		["concurrency", o.concurrency],
	] as const)
		if (!Number.isSafeInteger(value) || value < (name === "maxRepairs" ? 0 : 1))
			throw new RangeError(`${name} is invalid`);
	if (
		o.implementCandidates !== undefined &&
		(!Number.isSafeInteger(o.implementCandidates) ||
			o.implementCandidates < 1 ||
			o.implementCandidates > MAX_IMPLEMENT_CANDIDATES)
	)
		throw new RangeError("implementCandidates is invalid");
	if (
		o.evaluators.filter((e) => e.kind === "fast").length !== 1 ||
		o.evaluators.filter((e) => e.kind === "deep").length !== 1
	)
		throw new Error("exactly one fast and one deep evaluator are required");
}
function validateProposal<T extends JsonValue>(
	next: ControllerProposal<T>,
	prior: ControllerProposal<T> | undefined,
	repair: boolean,
): void {
	if (!next.id) throw new Error("proposal id is required");
	if (!repair && (next.parentId !== null || next.repairOf !== null))
		throw new Error("initial proposal links must be null");
	if (repair && (!prior || next.id === prior.id || next.parentId !== prior.id || next.repairOf !== prior.id))
		throw new Error("repair must have a new id and link to its parent");
}
function assembleEvaluation<T extends JsonValue>(
	proposalId: string,
	values: readonly {
		adapter: EvaluationAdapter<T>;
		result: { status: GateStatus; score?: number; detail?: string };
	}[],
): RavoEvaluation {
	const fast = values.find((v) => v.adapter.kind === "fast")?.result ?? { status: "error" as const };
	const deep = values.find((v) => v.adapter.kind === "deep")?.result ?? { status: "error" as const };
	const criteria: RavoCriterionObservation[] = values
		.filter((v) => v.adapter.kind === "opponent")
		.map(({ adapter, result }) => ({
			criterionId: adapter.criterionId ?? adapter.id,
			status: result.status,
			...(result.detail ? { detail: result.detail } : {}),
		}));
	return { proposalId, screen: fast, deep, criteria };
}
function diagnostic(c: RavoGateCertificate, extra?: string): DiagnosticFeedback {
	const findings = [
		...(c.screen.status === "pass"
			? []
			: [{ source: "fast", status: c.screen.status, detail: c.screen.detail ?? "fast screen rejected" }]),
		...(c.deep.status === "pass"
			? []
			: [{ source: "deep", status: c.deep.status, detail: c.deep.detail ?? "deep evaluator rejected" }]),
		...c.criteria
			.filter((x) => x.countedAsMissed)
			.map((x) => ({
				source: `opponent:${x.criterionId}`,
				status: x.status,
				detail: x.detail ?? "criterion missed",
			})),
		...(extra ? [{ source: "commit_gate", status: "fail" as const, detail: extra }] : []),
	];
	return { proposalId: c.proposalId, rejection: c.rejection ?? "commit_gate", findings };
}
function checkpointJson<T extends JsonValue>(checkpoint: RavoControllerCheckpoint<T>): JsonValue {
	return JSON.parse(JSON.stringify(checkpoint)) as JsonValue;
}
async function concurrentMap<I, O>(
	items: readonly I[],
	limit: number,
	fn: (item: I, index: number) => Promise<O>,
): Promise<O[]> {
	const results = new Array<O>(items.length);
	let cursor = 0;
	const worker = async (): Promise<void> => {
		while (cursor < items.length) {
			const index = cursor++;
			results[index] = await fn(items[index] as I, index);
		}
	};
	await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
	return results;
}
