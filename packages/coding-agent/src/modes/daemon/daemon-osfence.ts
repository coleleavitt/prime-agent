/**
 * Workflow V2 Slice 3 — OS-fence ownership consumer (daemon side).
 *
 * Normative authority: docs/WORKFLOW-V2-SLICE3-OSFENCE.md (pi-plugin-workflow, sha f63540e8),
 * closed shapes docs/api/workflow-v2-slice3-osfence.schema.json (sha ab08f952).
 *
 * This module is the daemon-side CONSUMER of the OS fence. It does NOT implement the Control DB:
 * the durable `BEGIN IMMEDIATE` generation CAS, `synchronous = FULL` fsync, monotonic generation,
 * and STRICT-table storage live behind the injected {@link OsfControlDb} interface, owned by the
 * separate `supervisor-control-db.ts` slice (contract §3, §9). This module owns Layer A endpoint
 * possession (§2.1), the two bound writer-fence forms (§3.3), the acquisition/takeover ordering
 * (§4), the two-way nonce handshake (§5), route resolution (§5.3), and the typed zero-effect fence
 * result surfaces (§5.5).
 *
 * CAPABILITY: unavailable. Every entry point is dormant behind the base-off guard
 * ({@link resolveOsfenceMode}); the live V1 daemon acquisition/claim/hello/fence path is
 * byte-identical. Only signals in {@link OsfenceAuthoritySignal} may authorize ownership;
 * proper-lockfile, PID/processStartId liveness, mtime, and TTL are diagnostics only (§0.1) and this
 * module imports none of them.
 */

import { createHash, randomBytes } from "node:crypto";
import type { DaemonSocketIdentity } from "./daemon-socket.js";

// Shared OS-fence control-DB contract (single authority; both this consumer and
// supervisor-control-db.ts conform to it). Re-exported so existing importers of this module keep
// their names.
import type {
	AcquisitionRecord,
	AdmissionUnknownReason,
	EndpointIdentity,
	FenceResultAdmissionUnknown,
	FenceResultGenerationStale,
	FenceResultOk,
	FenceResultOwnerMismatch,
	FenceResultOwnerUnavailable,
	FenceResultRouteStale,
	OsfControlDb,
	OsfenceAuthoritySignal,
	RevocationProof,
	RouteTuple,
	WorkerRouteRow,
	WriterFenceAssertion,
	WriterFenceCallSite,
} from "./osf-control-db.js";
import {
	OSFENCE_ACQUISITION_PROTOCOL,
	OSFENCE_FENCE_RESULT_PROTOCOL,
	OSFENCE_MAX_GENERATION,
	OSFENCE_WRITER_FENCE_PROTOCOL,
} from "./osf-control-db.js";

export type {
	AcquisitionKind,
	AcquisitionRecord,
	AdmissionUnknownReason,
	EndpointIdentity,
	FenceResult,
	FenceResultAdmissionUnknown,
	FenceResultGenerationStale,
	FenceResultOk,
	FenceResultOwnerMismatch,
	FenceResultOwnerUnavailable,
	FenceResultRouteStale,
	OsfControlDb,
	OsfControlDbAcquireParams,
	OsfenceAuthoritySignal,
	OsfenceCapability,
	OsfencePlatform,
	RevocationProof,
	RouteState,
	RouteTuple,
	WorkerRouteRow,
	WriterFenceAssertion,
	WriterFenceCallSite,
} from "./osf-control-db.js";
export {
	OSFENCE_ACQUISITION_PROTOCOL,
	OSFENCE_CONTROL_DB_APPLICATION_ID,
	OSFENCE_FENCE_RESULT_PROTOCOL,
	OSFENCE_MAX_GENERATION,
	OSFENCE_WRITER_FENCE_PROTOCOL,
} from "./osf-control-db.js";

export const OSFENCE_OFFER_PROTOCOL = "prime.workflow.osfence-handshake-offer/v2-slice3" as const;
export const OSFENCE_ACK_PROTOCOL = "prime.workflow.osfence-handshake-ack/v2-slice3" as const;

export interface SupervisorClaim {
	/** Canonical decimal generation string (§5.1). */
	supervisorGeneration: string;
	supervisorIncarnationId: string;
	supervisorPid: number | null;
	supervisorProcessStartId: string | null;
	supervisorSocketPath: string;
	endpointIdentity: EndpointIdentity;
	schemaDigest: string;
}

export interface HandshakeOffer {
	protocol: typeof OSFENCE_OFFER_PROTOCOL;
	claim: SupervisorClaim;
	route: RouteTuple;
	workerIncarnationId: string;
	capabilityDigest: string;
	supervisorNonce: string;
	offeredAt: string;
}

export type HandshakeDecision = "adopt" | "reconnect" | "reject_stale" | "reject_incarnation";

export interface HandshakeAck {
	protocol: typeof OSFENCE_ACK_PROTOCOL;
	acceptedGeneration: string;
	workerId: string;
	workerGeneration: number;
	workerIncarnationId: string;
	route: RouteTuple;
	schemaDigest: string;
	capabilityDigest: string;
	supervisorNonce: string;
	workerNonce: string;
	channelBindingDigest: string;
	decision: HandshakeDecision;
	ackedAt: string;
}

// FenceResult surfaces are imported and re-exported from ./osf-control-db.js (shared authority).

export class SupervisorWriterFenceError extends Error {
	readonly code = "supervisor_generation_stale" as const;

	constructor(reason: string) {
		super(`Supervisor writer fence rejected a native-topology append: ${reason}`);
		this.name = "SupervisorWriterFenceError";
	}
}

// ---------------------------------------------------------------------------
// Base-off guard (§7.1). Dormant unless V2 capability is available AND every fence precondition holds.
// ---------------------------------------------------------------------------

export interface OsfenceModeInput {
	/** From negotiateWorkflowV2Capability(...); false keeps the fence dormant (unreachable V2 path). */
	capabilityAvailable: boolean;
	platform: NodeJS.Platform;
	controlDb: OsfControlDb | undefined;
	/** Result of the node:sqlite capability probe (§7.4, BLOCK-1). */
	sqliteProbeOk: boolean;
	/** Local-filesystem control root (§ BLOCK-5). A network FS fails closed. */
	controlRootIsLocal: boolean;
}

export type OsfenceMode =
	| { enabled: true; capability: "available"; controlDb: OsfControlDb }
	| { enabled: false; capability: "unavailable"; reason: string };

/**
 * The base-off guard. Returns `enabled: false` whenever the V2 capability is unavailable, the
 * platform is win32 (BLOCK-4), the control DB is absent, the node:sqlite probe failed (BLOCK-1), or
 * the control root is not local (BLOCK-5). It NEVER falls back to a timing/PID/lock path. Because
 * negotiateWorkflowV2Capability always returns CAPABILITY_UNAVAILABLE, this resolves to disabled in
 * production, so the fence stays dormant and the V1 path is byte-identical.
 */
export function resolveOsfenceMode(input: OsfenceModeInput): OsfenceMode {
	if (!input.capabilityAvailable) {
		return { enabled: false, capability: "unavailable", reason: "workflow_v2_capability_unavailable" };
	}
	if (input.platform === "win32") {
		return { enabled: false, capability: "unavailable", reason: "win32_first_instance_pipe_unproven" };
	}
	if (!input.sqliteProbeOk) {
		return { enabled: false, capability: "unavailable", reason: "node_sqlite_probe_failed" };
	}
	if (!input.controlRootIsLocal) {
		return { enabled: false, capability: "unavailable", reason: "control_root_not_local_filesystem" };
	}
	if (!input.controlDb) {
		return { enabled: false, capability: "unavailable", reason: "control_db_unavailable" };
	}
	return { enabled: true, capability: "available", controlDb: input.controlDb };
}

// ---------------------------------------------------------------------------
// Generation codec (§5.1). Monotonic integer <-> canonical decimal string on the wire.
// ---------------------------------------------------------------------------

const CANONICAL_GENERATION_RE = /^[1-9][0-9]{0,15}$/;

export function isCanonicalGenerationString(value: string): boolean {
	if (!CANONICAL_GENERATION_RE.test(value)) {
		return false;
	}
	const parsed = Number(value);
	return Number.isInteger(parsed) && parsed >= 1 && parsed <= OSFENCE_MAX_GENERATION;
}

export function encodeGeneration(generation: number): string {
	if (!Number.isInteger(generation) || generation < 1 || generation > OSFENCE_MAX_GENERATION) {
		throw new RangeError(`Generation out of bounds: ${generation}`);
	}
	return String(generation);
}

export function decodeGeneration(value: string): number {
	if (!isCanonicalGenerationString(value)) {
		throw new RangeError(`Noncanonical generation string: ${JSON.stringify(value)}`);
	}
	return Number(value);
}

// ---------------------------------------------------------------------------
// Layer A — exclusive endpoint possession (§2.1). Death-released listening-fd token.
// ---------------------------------------------------------------------------

export interface EndpointLease {
	/** True once proper-lockfile reported the advisory lease compromised; diagnostic + defense-in-depth. */
	readonly compromised: boolean;
}

export interface EndpointPossessionInput {
	socketPath: string;
	boundIdentity: DaemonSocketIdentity;
	platform: NodeJS.Platform;
	lease: EndpointLease | undefined;
	/** getDaemonSocketIdentity(socketPath); re-read at each fence to detect a recreated path. */
	readIdentity: (socketPath: string) => DaemonSocketIdentity | undefined;
}

/**
 * Layer A possession over the listening endpoint fd. The kernel grants a single exclusive
 * `bind()`+`listen()` per Unix domain socket path; `SIGSTOP` does NOT release the fd and process
 * death closes it, so possession is both an instantaneous singleton and death-released — the two
 * properties the possession invariant (§3.1) needs. This class does not own the fd (the daemon's
 * net.Server does); it captures the bound `(dev, ino)` post-listen and re-verifies possession at each
 * write boundary. `supervisor.lock`/proper-lockfile are advisory only (§2.1, BLOCK-2).
 */
export class EndpointPossession {
	readonly identity: EndpointIdentity;
	private lost = false;

	constructor(private readonly input: EndpointPossessionInput) {
		if (input.platform === "win32") {
			// Windows first-instance pipe exclusivity is unproven; Slice 3 is unavailable there (BLOCK-4).
			throw new SupervisorWriterFenceError("win32 endpoint possession is unavailable (BLOCK-4)");
		}
		this.identity = {
			endpoint: input.socketPath,
			endpointDev: input.boundIdentity.dev,
			endpointIno: input.boundIdentity.ino,
			platform: "unix",
		};
	}

	/** Marks possession permanently lost (socket lease compromised / server closed); forces self-fence. */
	relinquish(): void {
		this.lost = true;
	}

	/**
	 * The Layer A fd token alone (§3.1): true while this process still holds the listening fd (not
	 * relinquished). `SIGSTOP` does not clear it; process death does. This is deliberately independent
	 * of the advisory lease and of the endpoint inode identity, which the writer fence checks
	 * separately (schema writerFenceAssertion treats endpointPossessed, endpointLeaseCompromised, and
	 * identity as distinct signals, §3.3).
	 */
	isHeld(): boolean {
		return !this.lost;
	}

	/** True when the fd is held, the endpoint inode is unchanged, and the advisory lease is intact. */
	isPossessed(): boolean {
		if (this.lost || this.input.lease?.compromised) {
			return false;
		}
		return this.identityMatches();
	}

	/** True when the current endpoint inode matches the inode bound at listen() (§2.1). */
	identityMatches(): boolean {
		const observed = this.input.readIdentity(this.input.socketPath);
		return (
			observed !== undefined &&
			observed.dev === this.input.boundIdentity.dev &&
			observed.ino === this.input.boundIdentity.ino
		);
	}

	observedIdentity(): EndpointIdentity {
		const observed = this.input.readIdentity(this.input.socketPath);
		if (!observed) {
			return { endpoint: this.input.socketPath, endpointDev: null, endpointIno: null, platform: "unix" };
		}
		return {
			endpoint: this.input.socketPath,
			endpointDev: observed.dev,
			endpointIno: observed.ino,
			platform: "unix",
		};
	}
}

// ---------------------------------------------------------------------------
// Writer fence Form 2 (§3.3) — synchronous, sound only under the death-released possession invariant.
// ---------------------------------------------------------------------------

export interface WriterFenceState {
	possession: EndpointPossession;
	leaseCompromised: boolean;
	controlDb: Pick<OsfControlDb, "readGenerationUnchecked">;
	adoptedGeneration: number;
	now: () => string;
}

/** Pure evaluation of the Form-2 writer fence; returns the audit record without throwing. */
export function evaluateWriterFence(state: WriterFenceState, callSite: WriterFenceCallSite): WriterFenceAssertion {
	const bound = state.possession.identity;
	const observed = state.possession.observedIdentity();
	// (a) Layer A: the fd token, the advisory lease, and the endpoint inode are distinct signals.
	const possessed = state.possession.isHeld();
	const identityMatches =
		bound.endpointDev === observed.endpointDev &&
		bound.endpointIno === observed.endpointIno &&
		bound.endpoint === observed.endpoint &&
		bound.platform === observed.platform;
	// (b) Layer B durable cross-check: defense in depth, cannot fail while (a) holds (§3.3).
	const observedGeneration = state.controlDb.readGenerationUnchecked();
	const pass =
		possessed && !state.leaseCompromised && identityMatches && observedGeneration === state.adoptedGeneration;
	return {
		protocol: OSFENCE_WRITER_FENCE_PROTOCOL,
		form: "synchronous",
		endpointPossessed: possessed,
		endpointLeaseCompromised: state.leaseCompromised,
		boundEndpointIdentity: bound,
		observedEndpointIdentity: observed,
		adoptedGeneration: state.adoptedGeneration,
		observedGeneration,
		callSite,
		outcome: pass ? "pass" : "fail",
		assertedAt: state.now(),
	};
}

/**
 * The synchronous `assertWriterFence()` gate (§6). Called at BOTH native-topology append boundaries
 * (pre-replay-verify and pre-durable-append) so a preflight check can never authorize a later write.
 * Throws {@link SupervisorWriterFenceError} on any failed precondition; returns the pass record
 * otherwise. Consumed by the composite admission writer; workers open the ledger reader-only.
 */
export function assertWriterFence(state: WriterFenceState, callSite: WriterFenceCallSite): WriterFenceAssertion {
	const assertion = evaluateWriterFence(state, callSite);
	if (assertion.outcome !== "pass") {
		if (!assertion.endpointPossessed) {
			throw new SupervisorWriterFenceError("endpoint possession lost");
		}
		if (assertion.endpointLeaseCompromised) {
			throw new SupervisorWriterFenceError("endpoint lease compromised");
		}
		if (
			assertion.boundEndpointIdentity.endpointDev !== assertion.observedEndpointIdentity.endpointDev ||
			assertion.boundEndpointIdentity.endpointIno !== assertion.observedEndpointIdentity.endpointIno
		) {
			throw new SupervisorWriterFenceError("endpoint identity changed");
		}
		throw new SupervisorWriterFenceError("supervisor generation advanced");
	}
	return assertion;
}

// ---------------------------------------------------------------------------
// Fence result surfaces (§5.5). Every reject variant is zero-effect and carries audit evidence.
// ---------------------------------------------------------------------------

export function fenceResultOk(
	observedGeneration: number,
	adoptedGeneration: number,
	route: RouteTuple,
	authoritySignal: OsfenceAuthoritySignal,
	now: string,
): FenceResultOk {
	return {
		protocol: OSFENCE_FENCE_RESULT_PROTOCOL,
		code: "OK",
		zeroEffect: false,
		capability: "available",
		resolvedAt: now,
		observedGeneration,
		adoptedGeneration,
		route,
		authoritySignal,
	};
}

export function fenceGenerationStale(
	observedGeneration: number,
	adoptedGeneration: number,
	now: string,
): FenceResultGenerationStale {
	return {
		protocol: OSFENCE_FENCE_RESULT_PROTOCOL,
		code: "GENERATION_STALE",
		zeroEffect: true,
		capability: "available",
		resolvedAt: now,
		observedGeneration,
		adoptedGeneration,
	};
}

export function fenceRouteStale(route: RouteTuple, currentRouteRevision: number, now: string): FenceResultRouteStale {
	return {
		protocol: OSFENCE_FENCE_RESULT_PROTOCOL,
		code: "ROUTE_STALE",
		zeroEffect: true,
		capability: "available",
		resolvedAt: now,
		route,
		currentRouteRevision,
	};
}

export function fenceOwnerMismatch(
	requestedRoute: RouteTuple,
	resolvedWorkerId: string,
	resolvedWorkerGeneration: number,
	now: string,
): FenceResultOwnerMismatch {
	return {
		protocol: OSFENCE_FENCE_RESULT_PROTOCOL,
		code: "OWNER_MISMATCH",
		zeroEffect: true,
		capability: "available",
		resolvedAt: now,
		requestedRoute,
		resolvedWorkerId,
		resolvedWorkerGeneration,
	};
}

export function fenceOwnerUnavailable(route: RouteTuple, now: string): FenceResultOwnerUnavailable {
	return {
		protocol: OSFENCE_FENCE_RESULT_PROTOCOL,
		code: "OWNER_UNAVAILABLE",
		zeroEffect: true,
		capability: "available",
		resolvedAt: now,
		route,
		routeState: "recovering",
	};
}

export function fenceAdmissionUnknown(
	reason: AdmissionUnknownReason,
	evidenceDigest: string,
	now: string,
): FenceResultAdmissionUnknown {
	return {
		protocol: OSFENCE_FENCE_RESULT_PROTOCOL,
		code: "ADMISSION_UNKNOWN",
		zeroEffect: true,
		capability: "available",
		resolvedAt: now,
		reason,
		evidenceDigest,
	};
}

// ---------------------------------------------------------------------------
// Route resolution (§5.3, R1/R2). Exact tuple only; no worker scan is representable.
// ---------------------------------------------------------------------------

export type RouteResolution =
	| { code: "OK"; row: WorkerRouteRow }
	| { code: "ROUTE_STALE"; result: FenceResultRouteStale }
	| { code: "OWNER_MISMATCH"; result: FenceResultOwnerMismatch }
	| { code: "OWNER_UNAVAILABLE"; result: FenceResultOwnerUnavailable };

/**
 * Resolves a command's exact route tuple against the Control DB (§5.3). A revision mismatch is
 * ROUTE_STALE (R1); a foreign worker at the same (root, directParent) is OWNER_MISMATCH (R2); a
 * `recovering` route is OWNER_UNAVAILABLE (W3). No roster scan and no blind forward.
 */
export function resolveRoute(
	controlDb: Pick<OsfControlDb, "readRoute">,
	requested: RouteTuple,
	now: string,
): RouteResolution {
	const row = controlDb.readRoute(requested.rootSessionId, requested.directParentSessionId);
	if (!row) {
		return {
			code: "OWNER_MISMATCH",
			result: fenceOwnerMismatch(requested, requested.workerId, requested.workerGeneration, now),
		};
	}
	if (row.workerId !== requested.workerId) {
		return {
			code: "OWNER_MISMATCH",
			result: fenceOwnerMismatch(requested, row.workerId, row.workerGeneration, now),
		};
	}
	if (row.state === "recovering") {
		return { code: "OWNER_UNAVAILABLE", result: fenceOwnerUnavailable(requested, now) };
	}
	if (row.routeRevision !== requested.routeRevision) {
		return { code: "ROUTE_STALE", result: fenceRouteStale(requested, row.routeRevision, now) };
	}
	return { code: "OK", row };
}

// ---------------------------------------------------------------------------
// Acquisition (§4). Delegates the durable generation advance to the injected control DB.
// ---------------------------------------------------------------------------

export interface SupervisorAcquisitionInput {
	controlDb: OsfControlDb;
	endpointIdentity: EndpointIdentity;
	incarnationId: string;
	schemaDigest: string;
	/** Null for first_init; a proven §4.3 revocation for a takeover. */
	revocationProof: RevocationProof | null;
}

/**
 * Runs the Control-DB acquisition transaction (§4.2) after Layer A possession is held. The monotonic
 * generation, the priorGeneration+1 CAS, and the fsync all happen inside the injected control DB;
 * this consumer validates the returned record's monotonicity and authority signal before trusting it.
 */
export function runSupervisorAcquisition(input: SupervisorAcquisitionInput): AcquisitionRecord {
	if (input.endpointIdentity.platform === "win32") {
		throw new SupervisorWriterFenceError("win32 acquisition is unavailable (BLOCK-4)");
	}
	const record = input.controlDb.acquire({
		endpointIdentity: input.endpointIdentity,
		incarnationId: input.incarnationId,
		schemaDigest: input.schemaDigest,
		revocationProof: input.revocationProof,
	});
	assertAcquisitionRecordSound(record);
	return record;
}

/** Fails closed on any record the injected control DB returns that violates the §11 invariants. */
export function assertAcquisitionRecordSound(record: AcquisitionRecord): void {
	if (record.protocol !== OSFENCE_ACQUISITION_PROTOCOL) {
		throw new SupervisorWriterFenceError("acquisition protocol mismatch");
	}
	if (record.capability !== "available") {
		throw new SupervisorWriterFenceError("acquisition capability gate");
	}
	if (record.authoritySignal !== "endpoint_possession" && record.authoritySignal !== "control_db_generation_cas") {
		throw new SupervisorWriterFenceError("acquisition cited a diagnostic authority signal");
	}
	if (record.endpointIdentity.platform === "win32") {
		throw new SupervisorWriterFenceError("acquisition on win32 (BLOCK-4)");
	}
	if (record.endpointIdentity.endpointDev === null || record.endpointIdentity.endpointIno === null) {
		throw new SupervisorWriterFenceError("unix acquisition requires endpoint dev/ino");
	}
	if (record.generation < 1 || record.generation > OSFENCE_MAX_GENERATION) {
		throw new SupervisorWriterFenceError("acquisition generation out of bounds");
	}
	if (record.kind === "first_init") {
		if (record.priorGeneration !== 0 || record.generation !== 1 || record.revocationProof !== null) {
			throw new SupervisorWriterFenceError("illegal first_init acquisition");
		}
		return;
	}
	if (record.generation !== record.priorGeneration + 1 || record.revocationProof === null) {
		throw new SupervisorWriterFenceError("illegal takeover monotonicity/revocation");
	}
}

// ---------------------------------------------------------------------------
// Two-way nonce handshake (§5.2). Binds generation/incarnation/route/digests in both directions.
// ---------------------------------------------------------------------------

export function generateNonce(): string {
	return randomBytes(32).toString("hex");
}

export function digestOf(...parts: string[]): string {
	const hash = createHash("sha256");
	for (const part of parts) {
		hash.update(part, "utf8");
		hash.update("\u0000");
	}
	return `sha256:${hash.digest("hex")}`;
}

/**
 * Channel-binding digest over the full bound tuple (§5.2): supervisor generation/incarnation, worker
 * id/generation/incarnation, root/direct-parent identity, route revision, and schema/capability
 * digests, plus both nonces. Any divergence in any bound field changes this digest.
 */
export function computeChannelBinding(offer: HandshakeOffer, workerNonce: string, workerGeneration: number): string {
	return digestOf(
		offer.claim.supervisorGeneration,
		offer.claim.supervisorIncarnationId,
		offer.route.workerId,
		String(workerGeneration),
		offer.workerIncarnationId,
		offer.route.rootSessionId,
		offer.route.directParentSessionId,
		String(offer.route.routeRevision),
		offer.claim.schemaDigest,
		offer.capabilityDigest,
		offer.supervisorNonce,
		workerNonce,
	);
}

export interface SupervisorOfferInput {
	claim: SupervisorClaim;
	route: RouteTuple;
	workerIncarnationId: string;
	capabilityDigest: string;
	now: string;
	nonce?: string;
}

export function buildHandshakeOffer(input: SupervisorOfferInput): HandshakeOffer {
	if (!isCanonicalGenerationString(input.claim.supervisorGeneration)) {
		throw new RangeError("supervisor claim carries a noncanonical generation string");
	}
	return {
		protocol: OSFENCE_OFFER_PROTOCOL,
		claim: input.claim,
		route: input.route,
		workerIncarnationId: input.workerIncarnationId,
		capabilityDigest: input.capabilityDigest,
		supervisorNonce: input.nonce ?? generateNonce(),
		offeredAt: input.now,
	};
}

/** Durable worker identity used by the worker to decide adoption of a presenting supervisor (§5.4). */
export interface WorkerAdoptionState {
	workerId: string;
	workerGeneration: number;
	workerIncarnationId: string;
	adoptedSupervisorGeneration: number;
	adoptedSupervisorIncarnationId: string;
	schemaDigest: string;
	capabilityDigest: string;
	route: RouteTuple;
}

/**
 * Worker-side adoption decision (§5.4). A live worker adopts a presented generation only when it is
 * strictly greater than the adopted one AND the presented route names the exact worker id/generation/
 * route revision this worker holds AND schema/capability digests match. Equal generation + same
 * incarnation is a reconnect; equal generation + different incarnation is rejected; lower is stale.
 */
export function decideWorkerAdoption(
	offer: HandshakeOffer,
	worker: WorkerAdoptionState,
	now: string,
	workerNonce?: string,
): HandshakeAck {
	const presented = decodeGeneration(offer.claim.supervisorGeneration);
	const nonce = workerNonce ?? generateNonce();
	const channelBindingDigest = computeChannelBinding(offer, nonce, worker.workerGeneration);
	const ackBase = {
		protocol: OSFENCE_ACK_PROTOCOL,
		acceptedGeneration: offer.claim.supervisorGeneration,
		workerId: worker.workerId,
		workerGeneration: worker.workerGeneration,
		workerIncarnationId: worker.workerIncarnationId,
		route: worker.route,
		schemaDigest: worker.schemaDigest,
		capabilityDigest: worker.capabilityDigest,
		supervisorNonce: offer.supervisorNonce,
		workerNonce: nonce,
		channelBindingDigest,
		ackedAt: now,
	};
	const digestsMatch =
		offer.claim.schemaDigest === worker.schemaDigest && offer.capabilityDigest === worker.capabilityDigest;
	const routeMatches =
		offer.route.workerId === worker.workerId &&
		offer.route.workerGeneration === worker.workerGeneration &&
		offer.route.routeRevision === worker.route.routeRevision &&
		offer.route.rootSessionId === worker.route.rootSessionId &&
		offer.route.directParentSessionId === worker.route.directParentSessionId;
	let decision: HandshakeDecision;
	if (presented > worker.adoptedSupervisorGeneration) {
		decision = digestsMatch && routeMatches ? "adopt" : "reject_stale";
	} else if (presented === worker.adoptedSupervisorGeneration) {
		decision =
			offer.claim.supervisorIncarnationId === worker.adoptedSupervisorIncarnationId
				? "reconnect"
				: "reject_incarnation";
	} else {
		decision = "reject_stale";
	}
	return { ...ackBase, decision };
}

export interface SupervisorAckVerification {
	accepted: boolean;
	decision: HandshakeDecision;
	reason?: string;
}

/**
 * Supervisor-side verification of the worker ack (§5.2). The channel-binding digest must reproduce
 * from the offer + worker nonce + acked worker generation, the nonces must echo, and only `adopt`/
 * `reconnect` are accepted. A tampered binding or mismatched nonce is rejected with zero effect.
 */
export function verifyHandshakeAckOnSupervisor(offer: HandshakeOffer, ack: HandshakeAck): SupervisorAckVerification {
	if (ack.protocol !== OSFENCE_ACK_PROTOCOL) {
		return { accepted: false, decision: ack.decision, reason: "ack_protocol_mismatch" };
	}
	if (ack.supervisorNonce !== offer.supervisorNonce) {
		return { accepted: false, decision: ack.decision, reason: "supervisor_nonce_mismatch" };
	}
	if (ack.acceptedGeneration !== offer.claim.supervisorGeneration) {
		return { accepted: false, decision: ack.decision, reason: "accepted_generation_mismatch" };
	}
	const expectedBinding = computeChannelBinding(offer, ack.workerNonce, ack.workerGeneration);
	if (ack.channelBindingDigest !== expectedBinding) {
		return { accepted: false, decision: ack.decision, reason: "channel_binding_mismatch" };
	}
	if (ack.decision !== "adopt" && ack.decision !== "reconnect") {
		return { accepted: false, decision: ack.decision, reason: `worker_${ack.decision}` };
	}
	return { accepted: true, decision: ack.decision };
}
