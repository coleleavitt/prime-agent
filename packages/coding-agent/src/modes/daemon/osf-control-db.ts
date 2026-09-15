/**
 * Workflow V2 Slice 3 — shared OS-fence control-DB contract (single authority).
 *
 * Normative authority: docs/WORKFLOW-V2-SLICE3-OSFENCE.md (pi-plugin-workflow, sha f63540e8),
 * closed shapes docs/api/workflow-v2-slice3-osfence.schema.json (sha ab08f952).
 *
 * This module is the ONE place that declares the {@link OsfControlDb} owner-only writer seam and the
 * closed control-DB row/route/acquisition/writer-fence vocabulary it exchanges. Both sides conform
 * to it: `daemon-osfence.ts` (the daemon-side CONSUMER, which owns Layer A possession, the handshake,
 * route resolution, and the fence-result surfaces) and `supervisor-control-db.ts` (the node:sqlite
 * IMPLEMENTATION, which owns the durable `BEGIN IMMEDIATE` generation CAS and `synchronous = FULL`
 * fsync). Extracting the seam here removes the two structurally-duplicated copies that previously
 * lived in each module.
 *
 * CAPABILITY: unavailable. These are type/shape declarations only; they authorize no implementation,
 * commit, or capability enablement. Only signals in {@link OsfenceAuthoritySignal} may authorize
 * ownership; proper-lockfile, PID/processStartId liveness, mtime, and TTL are diagnostics only (§0.1)
 * and this module imports none of them.
 */

/** The ONLY signals that may authorize ownership (§11 invariant 6). */
export type OsfenceAuthoritySignal = "endpoint_possession" | "control_db_generation_cas";

export type OsfencePlatform = "unix" | "win32";

export type OsfenceCapability = "available" | "unavailable";

export const OSFENCE_MAX_GENERATION = 9_007_199_254_740_991; // Number.MAX_SAFE_INTEGER, the closed-schema maximum.

export const OSFENCE_ACQUISITION_PROTOCOL = "prime.workflow.osfence-acquisition/v2-slice3" as const;
export const OSFENCE_WRITER_FENCE_PROTOCOL = "prime.workflow.osfence-writer-fence/v2-slice3" as const;

/** SQLite `application_id` tag for control.db ("W2SD"); asserted by the injected control DB (§3.2). */
export const OSFENCE_CONTROL_DB_APPLICATION_ID = 1_462_916_932;

// ---------------------------------------------------------------------------
// Closed shapes (mirror docs/api/workflow-v2-slice3-osfence.schema.json)
// ---------------------------------------------------------------------------

export interface EndpointIdentity {
	endpoint: string;
	/** null only on win32, where Slice 3 is unavailable (§11.5, BLOCK-4). */
	endpointDev: number | null;
	endpointIno: number | null;
	platform: OsfencePlatform;
}

export interface RouteTuple {
	rootSessionId: string;
	directParentSessionId: string;
	workerId: string;
	workerGeneration: number;
	routeRevision: number;
}

export type RouteState = "starting" | "ready" | "recovering" | "stopping" | "failed";

/**
 * The lean, consumer-facing worker route row returned by {@link OsfControlDb.readRoute}. The sqlite
 * implementation may return a richer row (e.g. carrying the diagnostic `updatedAt`) that structurally
 * extends this shape; the consumer only ever reads these fields (§5.3, R1/R2).
 */
export interface WorkerRouteRow {
	rootSessionId: string;
	directParentSessionId: string;
	workerId: string;
	workerGeneration: number;
	routeRevision: number;
	state: RouteState;
	descriptorDigest: string;
	updatedByGeneration: number;
}

export type AcquisitionKind = "first_init" | "takeover";

export type RevocationProof = "endpoint_released_kernel_confirmed" | "operator_fence" | "voluntary_release";

export interface AcquisitionRecord {
	protocol: typeof OSFENCE_ACQUISITION_PROTOCOL;
	kind: AcquisitionKind;
	priorGeneration: number;
	generation: number;
	endpointIdentity: EndpointIdentity;
	incarnationId: string;
	schemaDigest: string;
	authoritySignal: OsfenceAuthoritySignal;
	revocationProof: RevocationProof | null;
	capability: OsfenceCapability;
	acquiredAt: string;
}

export type WriterFenceCallSite = "pre_replay_verify" | "pre_durable_append";

export interface WriterFenceAssertion {
	protocol: typeof OSFENCE_WRITER_FENCE_PROTOCOL;
	form: "synchronous";
	endpointPossessed: boolean;
	endpointLeaseCompromised: boolean;
	boundEndpointIdentity: EndpointIdentity;
	observedEndpointIdentity: EndpointIdentity;
	adoptedGeneration: number;
	observedGeneration: number;
	callSite: WriterFenceCallSite;
	outcome: "pass" | "fail";
	assertedAt: string;
}

// ---------------------------------------------------------------------------
// Injected Control DB seam (§3). Implemented by supervisor-control-db.ts.
// ---------------------------------------------------------------------------

export interface OsfControlDbAcquireParams {
	endpointIdentity: EndpointIdentity;
	incarnationId: string;
	schemaDigest: string;
	/** Non-null for a takeover; must be a proven §4.3 revocation. Null forces first_init. */
	revocationProof: RevocationProof | null;
}

/**
 * Owner-only Control DB (`control.db`) writer seam. The implementation MUST run every mutation in a
 * `BEGIN IMMEDIATE` compare-and-advance transaction with `synchronous = FULL` + WAL and fsync the WAL
 * and directory after each committed acquisition/advance (§3.3, §3.5). The consumer never opens the
 * DB itself; it depends entirely on these methods, so the durable-fsync and monotonic-generation
 * guarantees are delivered by the injected implementation and proven in that slice.
 */
export interface OsfControlDb {
	/** Form-1 acquisition transaction (§4.2): first_init (prior 0 -> 1) or takeover (prior -> prior+1). */
	acquire(params: OsfControlDbAcquireParams): AcquisitionRecord;
	/** Single-row SELECT, no transaction (§3.3 Form 2 check (b)). Never an authority by itself. */
	readGenerationUnchecked(): number;
	/** Form-1 worker-generation reservation (§5.3): last_generation + 1, self-fenced on adopted. */
	reserveWorkerGeneration(adoptedGeneration: number, workerId: string): number;
	/** Form-1 route upsert (§5.3): route_revision + 1, self-fenced on adopted. */
	writeRoute(adoptedGeneration: number, route: RouteTuple, state: RouteState, descriptorDigest: string): void;
	/** Exact-tuple route read (§5.3, R1/R2). No worker scan is representable. */
	readRoute(rootSessionId: string, directParentSessionId: string): WorkerRouteRow | undefined;
	/** Idempotently close the writer handle on ordered relinquish (§4.5). Never decrements the generation. */
	release(): void;
}

export const OSFENCE_FENCE_RESULT_PROTOCOL = "prime.workflow.osfence-fence-result/v2-slice3" as const;

// ---------------------------------------------------------------------------
// Fence-result surfaces (§5.5). Every reject variant is zero-effect and typed. Shared so the
// daemon-osfence consumer and the supervisor-control-db implementation carry ONE definition.
// ---------------------------------------------------------------------------

export interface FenceResultOk {
	protocol: typeof OSFENCE_FENCE_RESULT_PROTOCOL;
	code: "OK";
	zeroEffect: false;
	capability: OsfenceCapability;
	resolvedAt: string;
	observedGeneration: number;
	adoptedGeneration: number;
	route: RouteTuple;
	authoritySignal: OsfenceAuthoritySignal;
}

export interface FenceResultGenerationStale {
	protocol: typeof OSFENCE_FENCE_RESULT_PROTOCOL;
	code: "GENERATION_STALE";
	zeroEffect: true;
	capability: OsfenceCapability;
	resolvedAt: string;
	observedGeneration: number;
	adoptedGeneration: number;
}

export interface FenceResultRouteStale {
	protocol: typeof OSFENCE_FENCE_RESULT_PROTOCOL;
	code: "ROUTE_STALE";
	zeroEffect: true;
	capability: OsfenceCapability;
	resolvedAt: string;
	route: RouteTuple;
	currentRouteRevision: number;
}

export interface FenceResultOwnerMismatch {
	protocol: typeof OSFENCE_FENCE_RESULT_PROTOCOL;
	code: "OWNER_MISMATCH";
	zeroEffect: true;
	capability: OsfenceCapability;
	resolvedAt: string;
	requestedRoute: RouteTuple;
	resolvedWorkerId: string;
	resolvedWorkerGeneration: number;
}

export interface FenceResultOwnerUnavailable {
	protocol: typeof OSFENCE_FENCE_RESULT_PROTOCOL;
	code: "OWNER_UNAVAILABLE";
	zeroEffect: true;
	capability: OsfenceCapability;
	resolvedAt: string;
	route: RouteTuple;
	routeState: "recovering";
}

export type AdmissionUnknownReason = "torn_append" | "legacy_ambiguity" | "integrity_failure" | "route_unprovable";

export interface FenceResultAdmissionUnknown {
	protocol: typeof OSFENCE_FENCE_RESULT_PROTOCOL;
	code: "ADMISSION_UNKNOWN";
	zeroEffect: true;
	capability: OsfenceCapability;
	resolvedAt: string;
	reason: AdmissionUnknownReason;
	evidenceDigest: string;
}

export type FenceResult =
	| FenceResultOk
	| FenceResultGenerationStale
	| FenceResultRouteStale
	| FenceResultOwnerMismatch
	| FenceResultOwnerUnavailable
	| FenceResultAdmissionUnknown;
