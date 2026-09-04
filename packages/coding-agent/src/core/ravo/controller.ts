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
	| { type: "stopped"; reason: RavoStopReason };

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
	const emit = (event: RavoProgressEvent): void => {
		try {
			options.onProgress?.(event);
		} catch {}
	};
	const persistCheckpoint = async (): Promise<void> => {
		cp.errorBudget = options.ledger.toJSON();
		const checkpoint = structuredClone(cp);
		await options.archive.append("run", { runId: options.runId, checkpoint: checkpointJson(checkpoint) });
		await options.onCheckpoint?.(checkpoint);
	};
	const setPhase = async (phase: RavoPhase): Promise<void> => {
		cp.phase = phase;
		emit({ type: "phase", phase, round: cp.round });
		await persistCheckpoint();
	};
	const stop = async (reason: RavoStopReason, certificate?: RavoGateCertificate): Promise<RavoControllerResult<T>> => {
		cp.phase = reason === "accepted" ? "accepted" : "stopped";
		await options.archive.append("stop", { runId: options.runId, reason, round: cp.round });
		emit({ type: "stopped", reason });
		return { reason, checkpoint: cp, ...(certificate ? { certificate } : {}) };
	};
	const abort = new AbortController();
	const relayAbort = (): void => abort.abort();
	if (options.signal?.aborted) abort.abort();
	else options.signal?.addEventListener("abort", relayAbort, { once: true });
	let reserved = 0;
	const call = async <I, O>(fn: ChildCall<I, O>, input: I): Promise<O> => {
		if (abort.signal.aborted) throw new Stop("cancelled");
		if (now() - started >= options.deadlineMs) throw new Stop("deadline");
		if (cp.spentTokens + reserved + options.reservationPerCall > options.tokenBudget) throw new Stop("budget");
		reserved += options.reservationPerCall;
		try {
			let result = await fn(input, { signal: abort.signal, tokenBudget: options.reservationPerCall });
			if (result.status === "deferred") {
				cp.workerHandle = result.handle;
				const resumed = await result.wait({ signal: abort.signal, tokenBudget: options.reservationPerCall });
				if (resumed.status === "deferred") throw new Error("nested deferred child result");
				result = resumed;
			}
			if (!Number.isSafeInteger(result.tokens) || result.tokens < 0)
				throw new Error("child returned invalid token usage");
			cp.spentTokens += result.tokens;
			if (result.status !== "completed") throw new ChildFailure(result.status, result.error);
			return result.value;
		} finally {
			reserved -= options.reservationPerCall;
		}
	};
	try {
		while (cp.round < options.maxRounds) {
			if (abort.signal.aborted) throw new Stop("cancelled");
			if (now() - started >= options.deadlineMs) throw new Stop("deadline");
			cp.round += 1;
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
			const candidate =
				cp.feedback && cp.candidate
					? await call(options.repair, {
							context,
							candidate: cp.candidate,
							feedback: cp.feedback,
							plan: cp.plan,
							...(cp.workerHandle ? { workerHandle: cp.workerHandle } : {}),
						})
					: await call(options.implement, {
							context,
							inspection: cp.inspection,
							plan: cp.plan,
							...(cp.workerHandle ? { workerHandle: cp.workerHandle } : {}),
						});
			validateProposal(candidate, cp.candidate, Boolean(cp.feedback));
			cp.candidate = candidate;
			cp.feedback = undefined;
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
			const observations = await concurrentMap(options.evaluators, options.concurrency, async (adapter) => {
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
			});
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
				const gate = await options.commitGate({
					proposal: candidate,
					certificate: stepped.certificate,
					signal: abort.signal,
				});
				if (gate.accepted) {
					const baseline = cp.archiveBaseline;
					if (!baseline) throw new Error("archive CAS baseline was not bound before evaluation");
					const digest = sha256(
						canonicalJson({
							proposal: candidate,
							certificate: stepped.certificate,
							errorBudget: JSON.parse(options.ledger.serialize()) as JsonValue,
						}),
					);
					try {
						await options.archive.accept(
							{ runId: options.runId, proposalId: candidate.id, certificateDigest: digest },
							baseline,
							digest,
						);
					} catch (error) {
						if (error instanceof RavoStaleCommitError) return stop("stale_cas", stepped.certificate);
						throw error;
					}
					cp.state = stepped.state;
					cp.errorBudget = options.ledger.toJSON();
					cp.phase = "accepted";
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
			cp.state = { ...cp.state, evaluatedProposalIds: stepped.state.evaluatedProposalIds };
			await options.archive.append("reject", {
				runId: options.runId,
				proposalId: candidate.id,
				feedback: cp.feedback as unknown as JsonValue,
			});
			await setPhase("diagnose");
			cp.repairs += 1;
			if (cp.repairs > options.maxRepairs) return stop("repair_limit", stepped.certificate);
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
async function concurrentMap<I, O>(items: readonly I[], limit: number, fn: (item: I) => Promise<O>): Promise<O[]> {
	const results = new Array<O>(items.length);
	let cursor = 0;
	const worker = async (): Promise<void> => {
		while (cursor < items.length) {
			const index = cursor++;
			results[index] = await fn(items[index] as I);
		}
	};
	await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
	return results;
}
