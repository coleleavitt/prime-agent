/**
 * Workflow V2 Slice 3 — OS-fence end-to-end integration (dormant behind the base-off guard).
 *
 * Normative authority: docs/WORKFLOW-V2-SLICE3-OSFENCE.md (§6 fence binding, §5.3 route/fence
 * fields) and docs/WORKFLOW-V2-SLICE3.md (§5 executor, §6 profile, §7 capture, §8 settlement).
 *
 * This module ties the already-built Slice 3 host lanes to the OS fence in ONE place:
 *   admission (RlmCompositeAdmissionLedger, writer-mode)
 *     -> at-most-once dispatch (Slice3RetainedDispatchExecutor, single physical provider call)
 *     -> tools-none profile (WORKFLOW_V2_TOOLS_NONE_PROFILE, maxTurns 1)
 *     -> exact terminal capture (TerminalCaptureSlot)
 *     -> pure settlement (reduceSettlement + validateSettlementCommit).
 *
 * The single Form-2 writer fence (§3.3: endpoint possession [Layer A] + control-DB generation
 * [Layer B check b]) gates BOTH the durable admit append and the provider-effect boundary, re-checked
 * at each write site so a preflight can never authorize a later write. The control-DB Form-1
 * (BEGIN IMMEDIATE compare-and-advance) transactions live entirely in the injected {@link OsfControlDb}
 * (acquire / reserveWorkerGeneration / writeRoute). The fence's route/generation fields on the admit
 * record are read from the control DB in the same critical section (§6.2).
 *
 * CAPABILITY: unavailable. {@link resolveWorkflowV2OsfencePipeline} resolves to disabled whenever the
 * V2 capability is unavailable (always, in production, via resolveOsfenceMode), so nothing here runs
 * on the live V1 path. Enablement additionally requires BLOCK-2..5 (§12) to clear; this module does
 * not flip any global authority.
 */

import {
	type Slice3DispatchBinding,
	type Slice3DispatchJournal,
	type Slice3DispatchOutcome,
	Slice3RetainedDispatchExecutor,
} from "../../core/workflow-v2-retained-executor.js";
import {
	WORKFLOW_V2_TOOLS_NONE_MAX_TURNS,
	WORKFLOW_V2_TOOLS_NONE_PROFILE,
	type WorkflowV2RetainedProfile,
} from "../../core/workflow-v2-retained-profile.js";
import {
	reduceSettlement,
	type SettlementInput,
	type SettlementReduction,
	validateSettlementCommit,
} from "../../core/workflow-v2-settlement.js";
import {
	assertWriterFence,
	type EndpointPossession,
	type OsfControlDb,
	type OsfenceMode,
	type OsfenceModeInput,
	type RouteResolution,
	type RouteTuple,
	resolveOsfenceMode,
	resolveRoute,
	type WriterFenceAssertion,
	type WriterFenceCallSite,
} from "./daemon-osfence.js";
import {
	type AdmitInput,
	type AdmitOutcome,
	RlmCompositeAdmissionLedger,
	type Slice3Fence,
	type Slice3LedgerCodec,
} from "./rlm-ledger.js";

/** The immutable tools-none profile binding every admitted retained turn carries (§6). */
export interface WorkflowV2BoundProfile {
	readonly profile: WorkflowV2RetainedProfile;
	readonly tools: "none";
	readonly maxTurns: typeof WORKFLOW_V2_TOOLS_NONE_MAX_TURNS;
}

/** The single bound tools-none profile; frozen so it cannot drift at a call site. */
export const WORKFLOW_V2_OSFENCE_BOUND_PROFILE: WorkflowV2BoundProfile = Object.freeze({
	profile: WORKFLOW_V2_TOOLS_NONE_PROFILE,
	tools: "none",
	maxTurns: WORKFLOW_V2_TOOLS_NONE_MAX_TURNS,
});

/** The worker identity this pipeline serves under the adopted supervisor generation (§5). */
export interface WorkflowV2OsfenceWorkerIdentity {
	readonly workerId: string;
	readonly workerGeneration: number;
	readonly workerIncarnationId: string;
}

/** Everything the supervisor-side writer pipeline needs; all durable authority is injected. */
export interface WorkflowV2OsfencePipelineInput {
	/** Layer A possession of the listening endpoint fd (§2.1). */
	readonly possession: EndpointPossession;
	/** Owner-only control DB (Layer B, Form-1 transactions). */
	readonly controlDb: OsfControlDb;
	/** The adopted, control-DB-monotonic supervisor generation (§4.2). */
	readonly adoptedGeneration: number;
	readonly supervisorIncarnationId: string;
	readonly worker: WorkflowV2OsfenceWorkerIdentity;
	readonly agentDir: string;
	readonly sessionsDir: string;
	readonly codec: Slice3LedgerCodec;
	readonly dispatchJournal: Slice3DispatchJournal;
	/** Advisory lease-compromise signal (diagnostic + defense in depth, §2.1). */
	readonly leaseCompromised?: () => boolean;
	readonly now?: () => string;
	readonly log?: (message: string) => void;
}

/**
 * Supervisor-side, writer-mode OS-fence pipeline. Constructed only on the enabled fence path
 * ({@link resolveWorkflowV2OsfencePipeline}); workers open the ledger reader-only and never admit
 * (§6.1). Holds no ownership authority of its own — the endpoint fd (Layer A) and the control-DB
 * generation (Layer B) are the only authorities, re-read at every write boundary.
 */
export class WorkflowV2OsfencePipeline {
	private readonly admissionLedger: RlmCompositeAdmissionLedger;
	private readonly executor: Slice3RetainedDispatchExecutor;
	private readonly now: () => string;

	constructor(private readonly input: WorkflowV2OsfencePipelineInput) {
		this.now = input.now ?? (() => new Date().toISOString());
		// ONE Form-2 fence closure, shared by the admit append and the provider-effect boundary. It is
		// re-evaluated on every call (never cached across an await), so a preflight cannot authorize a
		// later durable write (§3.3, §6.1).
		const assertVoidFence = () => {
			this.assertWriterFence("pre_durable_append");
		};
		this.admissionLedger = new RlmCompositeAdmissionLedger(input.agentDir, input.sessionsDir, {
			mode: "writer",
			codec: input.codec,
			assertWriterFence: assertVoidFence,
			...(input.log ? { log: input.log } : {}),
		});
		this.executor = new Slice3RetainedDispatchExecutor(input.dispatchJournal, assertVoidFence);
	}

	/** The single bound tools-none profile (§6). */
	get boundProfile(): WorkflowV2BoundProfile {
		return WORKFLOW_V2_OSFENCE_BOUND_PROFILE;
	}

	/**
	 * The synchronous Form-2 writer fence (§3.3). Throws {@link SupervisorWriterFenceError} on lost
	 * possession, a changed endpoint inode, a compromised lease, or an advanced control-DB generation;
	 * returns the pass audit record otherwise. Layer A (possession) is the sufficient proof; the
	 * control-DB generation read is defense in depth.
	 */
	assertWriterFence(callSite: WriterFenceCallSite): WriterFenceAssertion {
		return assertWriterFence(
			{
				possession: this.input.possession,
				leaseCompromised: this.input.leaseCompromised?.() ?? false,
				controlDb: this.input.controlDb,
				adoptedGeneration: this.input.adoptedGeneration,
				now: this.now,
			},
			callSite,
		);
	}

	/** Exact-tuple route resolution against the control DB (§5.3, R1/R2/W3). No worker scan. */
	resolveRoute(requested: RouteTuple): RouteResolution {
		return resolveRoute(this.input.controlDb, requested, this.now());
	}

	/**
	 * Build the admit-record fence fields from control-DB values read in this critical section (§6.2):
	 * the current adopted supervisor generation and the exact route's revision. Fails closed if the
	 * route does not resolve to this pipeline's worker at a non-stale revision.
	 */
	buildFenceForRoute(route: RouteTuple): Slice3Fence {
		const resolution = this.resolveRoute(route);
		if (resolution.code !== "OK") {
			throw new WorkflowV2OsfencePipelineError(`route not current for admit fence: ${resolution.code}`);
		}
		return {
			supervisorGeneration: this.input.adoptedGeneration,
			supervisorIncarnationId: this.input.supervisorIncarnationId,
			workerId: this.input.worker.workerId,
			workerGeneration: this.input.worker.workerGeneration,
			workerIncarnationId: this.input.worker.workerIncarnationId,
			routeRevision: resolution.row.routeRevision,
		};
	}

	/**
	 * Admit one child.admit through the writer-mode composite ledger. The Form-2 fence is re-checked
	 * inside the ledger at the append boundary; a stale writer throws and writes nothing (§6.1).
	 */
	admit(input: AdmitInput): Promise<AdmitOutcome> {
		return this.admissionLedger.admit(input);
	}

	/**
	 * Authorize and perform the ONE physical provider call for an admitted turn (§5.1). The executor
	 * commits a `dispatching` fact under the control-DB fence, re-checks the fence at the
	 * provider-effect boundary, and never relaunches once `dispatching` is durable.
	 */
	dispatchOnce<T>(binding: Slice3DispatchBinding, physicalCall: () => Promise<T>): Promise<Slice3DispatchOutcome<T>> {
		return this.executor.dispatchOnce(binding, physicalCall);
	}

	/**
	 * Reduce a captured terminal + closure + durable dispatch/cancel/quiescence facts into one atomic
	 * settlement commit (§8). Pure and total; a possible provider effect without complete terminal
	 * evidence reduces to `execution_unknown` and is never relaunched.
	 */
	settle(settlement: SettlementInput): SettlementReduction {
		const reduction = reduceSettlement(settlement);
		if (reduction.kind === "commit") {
			const validation = validateSettlementCommit(reduction.commit);
			if (!validation.ok) {
				throw new WorkflowV2OsfencePipelineError(`settlement commit failed validation: ${validation.reason}`);
			}
		}
		return reduction;
	}

	/** The composite admission ledger path, for reconciliation/inspection. */
	get ledgerPath(): string {
		return this.admissionLedger.ledgerPath;
	}
}

/** Raised on an integration-level fail-closed condition (never an ownership authority). */
export class WorkflowV2OsfencePipelineError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "WorkflowV2OsfencePipelineError";
	}
}

/** Runtime dependencies supplied only when the fence resolves enabled (never on the live path). */
export interface WorkflowV2OsfencePipelineDeps {
	readonly possession: EndpointPossession;
	readonly adoptedGeneration: number;
	readonly supervisorIncarnationId: string;
	readonly worker: WorkflowV2OsfenceWorkerIdentity;
	readonly agentDir: string;
	readonly sessionsDir: string;
	readonly codec: Slice3LedgerCodec;
	readonly dispatchJournal: Slice3DispatchJournal;
	readonly leaseCompromised?: () => boolean;
	readonly now?: () => string;
	readonly log?: (message: string) => void;
}

export type WorkflowV2OsfencePipelineResolution =
	| { readonly enabled: false; readonly capability: "unavailable"; readonly reason: string }
	| { readonly enabled: true; readonly capability: "available"; readonly pipeline: WorkflowV2OsfencePipeline };

/**
 * The base-off guard for the whole integration (§7.1). Delegates to {@link resolveOsfenceMode}: when
 * the V2 capability is unavailable, the platform is win32, the node:sqlite probe failed, the control
 * root is non-local, or the control DB is absent, it returns disabled and constructs NOTHING. Because
 * negotiateWorkflowV2Capability always returns CAPABILITY_UNAVAILABLE, this is disabled in production
 * and the pipeline never exists on the live path. It never falls back to a timing/PID/lock path.
 */
export function resolveWorkflowV2OsfencePipeline(
	modeInput: OsfenceModeInput,
	deps: WorkflowV2OsfencePipelineDeps,
): WorkflowV2OsfencePipelineResolution {
	const mode: OsfenceMode = resolveOsfenceMode(modeInput);
	if (!mode.enabled) {
		return { enabled: false, capability: "unavailable", reason: mode.reason };
	}
	const pipeline = new WorkflowV2OsfencePipeline({
		possession: deps.possession,
		controlDb: mode.controlDb,
		adoptedGeneration: deps.adoptedGeneration,
		supervisorIncarnationId: deps.supervisorIncarnationId,
		worker: deps.worker,
		agentDir: deps.agentDir,
		sessionsDir: deps.sessionsDir,
		codec: deps.codec,
		dispatchJournal: deps.dispatchJournal,
		...(deps.leaseCompromised ? { leaseCompromised: deps.leaseCompromised } : {}),
		...(deps.now ? { now: deps.now } : {}),
		...(deps.log ? { log: deps.log } : {}),
	});
	return { enabled: true, capability: "available", pipeline };
}
