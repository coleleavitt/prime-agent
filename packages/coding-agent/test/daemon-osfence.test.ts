import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import _Ajv2020 from "ajv/dist/2020.js";
import _addFormats from "ajv-formats";

// ajv/ajv-formats are CJS with a default export; normalize the interop shape across tsgo and the
// test runtime, and type only the tiny surface this test uses so the exact ajv typings are irrelevant.
type AjvLike = { compile(schema: unknown): ((data: unknown) => boolean) & { errors?: unknown } };
type AjvCtor = new (opts?: Record<string, unknown>) => AjvLike;
const Ajv2020 = ((_Ajv2020 as { default?: unknown }).default ?? _Ajv2020) as unknown as AjvCtor;
const addFormats = ((_addFormats as { default?: unknown }).default ?? _addFormats) as unknown as (
	ajv: AjvLike,
) => AjvLike;

import { describe, expect, it } from "vitest";
import {
	type AcquisitionRecord,
	assertAcquisitionRecordSound,
	assertWriterFence,
	buildHandshakeOffer,
	computeChannelBinding,
	decideWorkerAdoption,
	decodeGeneration,
	digestOf,
	type EndpointIdentity,
	EndpointPossession,
	encodeGeneration,
	evaluateWriterFence,
	fenceAdmissionUnknown,
	fenceGenerationStale,
	fenceOwnerMismatch,
	fenceOwnerUnavailable,
	fenceResultOk,
	fenceRouteStale,
	generateNonce,
	type HandshakeOffer,
	isCanonicalGenerationString,
	OSFENCE_ACQUISITION_PROTOCOL,
	OSFENCE_MAX_GENERATION,
	type OsfControlDb,
	type RouteTuple,
	resolveOsfenceMode,
	resolveRoute,
	runSupervisorAcquisition,
	SupervisorWriterFenceError,
	verifyHandshakeAckOnSupervisor,
	type WorkerAdoptionState,
	type WorkerRouteRow,
} from "../src/modes/daemon/daemon-osfence.js";

const NOW = "2026-09-15T00:00:00.000Z";
const DIGEST = `sha256:${"a".repeat(64)}`;
const DIGEST_B = `sha256:${"b".repeat(64)}`;

function unixEndpoint(): EndpointIdentity {
	return { endpoint: "/run/prime/daemon.sock", endpointDev: 66310, endpointIno: 4242, platform: "unix" };
}

function route(overrides: Partial<RouteTuple> = {}): RouteTuple {
	return {
		rootSessionId: "root-1",
		directParentSessionId: "parent-1",
		workerId: "worker-1",
		workerGeneration: 1,
		routeRevision: 1,
		...overrides,
	};
}

function routeRow(overrides: Partial<WorkerRouteRow> = {}): WorkerRouteRow {
	return {
		rootSessionId: "root-1",
		directParentSessionId: "parent-1",
		workerId: "worker-1",
		workerGeneration: 1,
		routeRevision: 1,
		state: "ready",
		descriptorDigest: DIGEST,
		updatedByGeneration: 5,
		...overrides,
	};
}

/** A minimal fake OsfControlDb implementing the injected seam; the real one is the sibling SQLite module. */
class FakeControlDb implements OsfControlDb {
	generation: number;
	private readonly routes = new Map<string, WorkerRouteRow>();
	released = false;

	constructor(generation = 5) {
		this.generation = generation;
	}

	acquire(): AcquisitionRecord {
		throw new Error("not used in these tests");
	}
	readGenerationUnchecked(): number {
		return this.generation;
	}
	reserveWorkerGeneration(_adopted: number, _workerId: string): number {
		return 1;
	}
	writeRoute(_adopted: number, r: RouteTuple, state: WorkerRouteRow["state"], digest: string): void {
		this.routes.set(`${r.rootSessionId}\u0000${r.directParentSessionId}`, {
			...r,
			state,
			descriptorDigest: digest,
			updatedByGeneration: this.generation,
		});
	}
	setRoute(row: WorkerRouteRow): void {
		this.routes.set(`${row.rootSessionId}\u0000${row.directParentSessionId}`, row);
	}
	readRoute(rootSessionId: string, directParentSessionId: string): WorkerRouteRow | undefined {
		return this.routes.get(`${rootSessionId}\u0000${directParentSessionId}`);
	}
	release(): void {
		this.released = true;
	}
}

function possession(
	identity: EndpointIdentity,
	readIdentity?: (p: string) => { dev: number; ino: number } | undefined,
) {
	return new EndpointPossession({
		socketPath: identity.endpoint,
		boundIdentity: { dev: identity.endpointDev as number, ino: identity.endpointIno as number },
		platform: "linux",
		lease: undefined,
		readIdentity:
			readIdentity ?? (() => ({ dev: identity.endpointDev as number, ino: identity.endpointIno as number })),
	});
}

// --------------------------------------------------------------------------- generation codec

describe("generation codec (§5.1)", () => {
	it("accepts only canonical decimal strings", () => {
		for (const good of ["1", "5", "42", "9007199254740991"]) {
			expect(isCanonicalGenerationString(good)).toBe(true);
		}
		for (const bad of ["0", "01", "+1", " 1", "1 ", "1.0", "-1", "", "00", "9".repeat(17)]) {
			expect(isCanonicalGenerationString(bad)).toBe(false);
		}
	});
	it("round-trips within bounds and rejects out-of-range", () => {
		expect(decodeGeneration(encodeGeneration(1))).toBe(1);
		expect(decodeGeneration(encodeGeneration(OSFENCE_MAX_GENERATION))).toBe(OSFENCE_MAX_GENERATION);
		expect(() => encodeGeneration(0)).toThrow();
		expect(() => encodeGeneration(OSFENCE_MAX_GENERATION + 1)).toThrow();
		expect(() => decodeGeneration("0")).toThrow();
	});
});

// --------------------------------------------------------------------------- base-off guard

describe("resolveOsfenceMode base-off guard (§7.1)", () => {
	const base = {
		capabilityAvailable: true as boolean,
		platform: "linux" as NodeJS.Platform,
		controlDb: new FakeControlDb(),
		sqliteProbeOk: true,
		controlRootIsLocal: true,
	};
	it("is disabled whenever V2 capability is unavailable", () => {
		const mode = resolveOsfenceMode({ ...base, capabilityAvailable: false });
		expect(mode.enabled).toBe(false);
		expect(mode.capability).toBe("unavailable");
	});
	it("is disabled on win32 (BLOCK-4), missing sqlite (BLOCK-1), network FS (BLOCK-5), no controlDb", () => {
		expect(resolveOsfenceMode({ ...base, platform: "win32" }).enabled).toBe(false);
		expect(resolveOsfenceMode({ ...base, sqliteProbeOk: false }).enabled).toBe(false);
		expect(resolveOsfenceMode({ ...base, controlRootIsLocal: false }).enabled).toBe(false);
		expect(resolveOsfenceMode({ ...base, controlDb: undefined }).enabled).toBe(false);
	});
	it("is enabled only when every precondition holds", () => {
		const mode = resolveOsfenceMode(base);
		expect(mode.enabled).toBe(true);
		expect(mode.capability).toBe("available");
	});
});

// --------------------------------------------------------------------------- Layer A possession

describe("EndpointPossession Layer A (§2.1, §3.1)", () => {
	it("is possessed while the bound inode is unchanged and the lease is intact", () => {
		expect(possession(unixEndpoint()).isPossessed()).toBe(true);
	});
	it("loses possession on relinquish, identity change, missing endpoint, or compromised lease", () => {
		const relinquished = possession(unixEndpoint());
		relinquished.relinquish();
		expect(relinquished.isPossessed()).toBe(false);

		expect(possession(unixEndpoint(), () => ({ dev: 1, ino: 2 })).isPossessed()).toBe(false);
		expect(possession(unixEndpoint(), () => undefined).isPossessed()).toBe(false);

		const withLease = new EndpointPossession({
			socketPath: "/run/prime/daemon.sock",
			boundIdentity: { dev: 66310, ino: 4242 },
			platform: "linux",
			lease: { compromised: true },
			readIdentity: () => ({ dev: 66310, ino: 4242 }),
		});
		expect(withLease.isPossessed()).toBe(false);
	});
	it("refuses win32 (BLOCK-4)", () => {
		expect(
			() =>
				new EndpointPossession({
					socketPath: "\\\\.\\pipe\\x",
					boundIdentity: { dev: 0, ino: 0 },
					platform: "win32",
					lease: undefined,
					readIdentity: () => undefined,
				}),
		).toThrow(SupervisorWriterFenceError);
	});
});

// --------------------------------------------------------------------------- writer fence Form 2

describe("writer fence Form 2 (§3.3)", () => {
	function state(overrides: Partial<Parameters<typeof evaluateWriterFence>[0]> = {}) {
		return {
			possession: possession(unixEndpoint()),
			leaseCompromised: false,
			controlDb: new FakeControlDb(5),
			adoptedGeneration: 5,
			now: () => NOW,
			...overrides,
		};
	}
	it("passes only when possession, lease, identity, and generation all hold", () => {
		const a = evaluateWriterFence(state(), "pre_durable_append");
		expect(a.outcome).toBe("pass");
		expect(assertWriterFence(state(), "pre_durable_append").outcome).toBe("pass");
	});
	it("fails and throws when possession lost", () => {
		const p = possession(unixEndpoint());
		p.relinquish();
		expect(evaluateWriterFence(state({ possession: p }), "pre_durable_append").outcome).toBe("fail");
		expect(() => assertWriterFence(state({ possession: p }), "pre_durable_append")).toThrow(/possession lost/);
	});
	it("fails when lease compromised", () => {
		expect(() => assertWriterFence(state({ leaseCompromised: true }), "pre_replay_verify")).toThrow(
			/lease compromised/,
		);
	});
	it("fails when the endpoint identity changed", () => {
		const p = possession(unixEndpoint(), () => ({ dev: 9, ino: 9 }));
		expect(() => assertWriterFence(state({ possession: p }), "pre_durable_append")).toThrow(/identity changed/);
	});
	it("fails when the control-DB generation advanced past the adopted one", () => {
		expect(() => assertWriterFence(state({ controlDb: new FakeControlDb(6) }), "pre_durable_append")).toThrow(
			/generation advanced/,
		);
	});
});

// --------------------------------------------------------------------------- fence result surfaces

describe("fence result surfaces (§5.5)", () => {
	it("OK is not zero-effect and observed == adopted", () => {
		const r = fenceResultOk(5, 5, route(), "endpoint_possession", NOW);
		expect(r.zeroEffect).toBe(false);
		expect(r.observedGeneration).toBe(r.adoptedGeneration);
		expect(["endpoint_possession", "control_db_generation_cas"]).toContain(r.authoritySignal);
	});
	it("every reject variant is zero-effect with a sound discriminator", () => {
		expect(fenceGenerationStale(6, 5, NOW).zeroEffect).toBe(true);
		expect(fenceGenerationStale(6, 5, NOW).observedGeneration).toBeGreaterThan(5);
		const rs = fenceRouteStale(route({ routeRevision: 2 }), 3, NOW);
		expect(rs.zeroEffect).toBe(true);
		expect(rs.route.routeRevision).not.toBe(rs.currentRouteRevision);
		const om = fenceOwnerMismatch(route(), "worker-2", 1, NOW);
		expect(om.zeroEffect).toBe(true);
		expect(om.resolvedWorkerId).not.toBe(om.requestedRoute.workerId);
		const ou = fenceOwnerUnavailable(route(), NOW);
		expect(ou.zeroEffect).toBe(true);
		expect(ou.routeState).toBe("recovering");
		const au = fenceAdmissionUnknown("torn_append", DIGEST, NOW);
		expect(au.zeroEffect).toBe(true);
		expect(au.reason).toBe("torn_append");
	});
});

// --------------------------------------------------------------------------- route resolution

describe("resolveRoute exact-tuple resolution (§5.3, R1/R2)", () => {
	it("OK on exact match", () => {
		const db = new FakeControlDb();
		db.setRoute(routeRow());
		const res = resolveRoute(db, route(), NOW);
		expect(res.code).toBe("OK");
	});
	it("OWNER_UNAVAILABLE for a recovering route (W3)", () => {
		const db = new FakeControlDb();
		db.setRoute(routeRow({ state: "recovering" }));
		expect(resolveRoute(db, route(), NOW).code).toBe("OWNER_UNAVAILABLE");
	});
	it("ROUTE_STALE for a revision mismatch (R1)", () => {
		const db = new FakeControlDb();
		db.setRoute(routeRow({ routeRevision: 3 }));
		const res = resolveRoute(db, route({ routeRevision: 2 }), NOW);
		expect(res.code).toBe("ROUTE_STALE");
	});
	it("OWNER_MISMATCH for a foreign worker or a missing row (R2)", () => {
		const db = new FakeControlDb();
		db.setRoute(routeRow({ workerId: "worker-2" }));
		expect(resolveRoute(db, route({ workerId: "worker-1" }), NOW).code).toBe("OWNER_MISMATCH");
		expect(resolveRoute(new FakeControlDb(), route(), NOW).code).toBe("OWNER_MISMATCH");
	});
});

// --------------------------------------------------------------------------- acquisition

describe("acquisition soundness (§4.2, §11)", () => {
	function firstInit(): AcquisitionRecord {
		return {
			protocol: OSFENCE_ACQUISITION_PROTOCOL,
			kind: "first_init",
			priorGeneration: 0,
			generation: 1,
			endpointIdentity: unixEndpoint(),
			incarnationId: "sup-inc-1",
			schemaDigest: DIGEST,
			authoritySignal: "control_db_generation_cas",
			revocationProof: null,
			capability: "available",
			acquiredAt: NOW,
		};
	}
	function takeover(): AcquisitionRecord {
		return {
			...firstInit(),
			kind: "takeover",
			priorGeneration: 4,
			generation: 5,
			revocationProof: "endpoint_released_kernel_confirmed",
		};
	}
	it("accepts a well-formed first_init and takeover", () => {
		assertAcquisitionRecordSound(firstInit());
		assertAcquisitionRecordSound(takeover());
	});
	it("rejects illegal first_init (gen!=1, prior!=0, non-null proof)", () => {
		expect(() => assertAcquisitionRecordSound({ ...firstInit(), generation: 2 })).toThrow();
		expect(() => assertAcquisitionRecordSound({ ...firstInit(), priorGeneration: 1 })).toThrow();
		expect(() => assertAcquisitionRecordSound({ ...firstInit(), revocationProof: "operator_fence" })).toThrow();
	});
	it("rejects illegal takeover (gen!=prior+1, missing proof)", () => {
		expect(() => assertAcquisitionRecordSound({ ...takeover(), generation: 6 })).toThrow();
		expect(() => assertAcquisitionRecordSound({ ...takeover(), revocationProof: null })).toThrow();
	});
	it("rejects a diagnostic authority signal, unavailable capability, and win32", () => {
		expect(() => assertAcquisitionRecordSound({ ...takeover(), authoritySignal: "pid_liveness" as never })).toThrow();
		expect(() => assertAcquisitionRecordSound({ ...takeover(), capability: "unavailable" })).toThrow();
		expect(() =>
			assertAcquisitionRecordSound({
				...takeover(),
				endpointIdentity: { endpoint: "x", endpointDev: null, endpointIno: null, platform: "win32" },
			}),
		).toThrow();
	});
	it("runSupervisorAcquisition validates the injected control-DB record", () => {
		const db = new FakeControlDb();
		db.acquire = () => takeover();
		const record = runSupervisorAcquisition({
			controlDb: db,
			endpointIdentity: unixEndpoint(),
			incarnationId: "sup-inc-1",
			schemaDigest: DIGEST,
			revocationProof: "endpoint_released_kernel_confirmed",
		});
		expect(record.generation).toBe(5);
		const bad = new FakeControlDb();
		bad.acquire = () => ({ ...takeover(), generation: 99 });
		expect(() =>
			runSupervisorAcquisition({
				controlDb: bad,
				endpointIdentity: unixEndpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: DIGEST,
				revocationProof: "endpoint_released_kernel_confirmed",
			}),
		).toThrow();
	});
});

// --------------------------------------------------------------------------- two-way handshake

describe("two-way nonce handshake (§5.2, §5.4)", () => {
	function offer(gen = "5"): HandshakeOffer {
		return buildHandshakeOffer({
			claim: {
				supervisorGeneration: gen,
				supervisorIncarnationId: "sup-inc-2",
				supervisorPid: 4321,
				supervisorProcessStartId: "start-1",
				supervisorSocketPath: "/run/prime/daemon.sock",
				endpointIdentity: unixEndpoint(),
				schemaDigest: DIGEST,
			},
			route: route(),
			workerIncarnationId: "worker-inc-1",
			capabilityDigest: DIGEST_B,
			now: NOW,
			nonce: "0".repeat(64),
		});
	}
	function worker(overrides: Partial<WorkerAdoptionState> = {}): WorkerAdoptionState {
		return {
			workerId: "worker-1",
			workerGeneration: 1,
			workerIncarnationId: "worker-inc-1",
			adoptedSupervisorGeneration: 4,
			adoptedSupervisorIncarnationId: "sup-inc-1",
			schemaDigest: DIGEST,
			capabilityDigest: DIGEST_B,
			route: route(),
			...overrides,
		};
	}
	it("adopts a strictly greater generation with matching digests and route", () => {
		const ack = decideWorkerAdoption(offer("5"), worker(), NOW, "1".repeat(64));
		expect(ack.decision).toBe("adopt");
		expect(verifyHandshakeAckOnSupervisor(offer("5"), { ...ack }).accepted).toBe(true);
	});
	it("reconnects on equal generation with the same incarnation", () => {
		const ack = decideWorkerAdoption(
			offer("4"),
			worker({ adoptedSupervisorIncarnationId: "sup-inc-2" }),
			NOW,
			"1".repeat(64),
		);
		expect(ack.decision).toBe("reconnect");
	});
	it("rejects equal generation with a different incarnation", () => {
		const ack = decideWorkerAdoption(offer("4"), worker(), NOW, "1".repeat(64));
		expect(ack.decision).toBe("reject_incarnation");
	});
	it("rejects a lower generation and a greater one with mismatched route/digest", () => {
		expect(decideWorkerAdoption(offer("3"), worker(), NOW, "1".repeat(64)).decision).toBe("reject_stale");
		expect(
			decideWorkerAdoption(offer("5"), worker({ schemaDigest: `sha256:${"c".repeat(64)}` }), NOW, "1".repeat(64))
				.decision,
		).toBe("reject_stale");
	});
	it("channel binding is deterministic and diverges on any bound field", () => {
		const o = offer("5");
		const wn = "1".repeat(64);
		expect(computeChannelBinding(o, wn, 1)).toBe(computeChannelBinding(o, wn, 1));
		expect(computeChannelBinding(o, wn, 1)).not.toBe(computeChannelBinding(o, wn, 2));
		expect(computeChannelBinding(o, wn, 1)).not.toBe(computeChannelBinding(o, "2".repeat(64), 1));
	});
	it("supervisor rejects a tampered binding, nonce mismatch, or worker rejection", () => {
		const o = offer("5");
		const ack = decideWorkerAdoption(o, worker(), NOW, "1".repeat(64));
		expect(verifyHandshakeAckOnSupervisor(o, { ...ack, channelBindingDigest: DIGEST }).accepted).toBe(false);
		expect(verifyHandshakeAckOnSupervisor(o, { ...ack, supervisorNonce: "2".repeat(64) }).accepted).toBe(false);
		const rejected = decideWorkerAdoption(offer("3"), worker(), NOW, "1".repeat(64));
		expect(verifyHandshakeAckOnSupervisor(offer("3"), rejected).accepted).toBe(false);
	});
	it("nonces and digests are well-formed", () => {
		expect(generateNonce()).toMatch(/^[0-9a-f]{64}$/);
		expect(digestOf("a", "b")).toMatch(/^sha256:[0-9a-f]{64}$/);
	});
});

// --------------------------------------------------------------------------- closed-schema shape conformance

describe("closed-schema shape conformance (docs/api/workflow-v2-slice3-osfence.schema.json)", () => {
	const HERE = dirname(fileURLToPath(import.meta.url));
	const candidates = [
		join(HERE, "../../../../pi-plugin-workflow/docs/api/workflow-v2-slice3-osfence.schema.json"),
		"/home/cole/WebstormProjects/active/pi-plugin-workflow/docs/api/workflow-v2-slice3-osfence.schema.json",
	];
	const schemaPath = candidates.find((p) => existsSync(p));

	it.runIf(schemaPath)("every constructed record validates against its closed $def", () => {
		const schema = JSON.parse(readFileSync(schemaPath as string, "utf8"));
		const ajv = addFormats(new Ajv2020({ strict: false }));
		const defs = schema.$defs;
		const validator = (name: string) => ajv.compile({ $defs: defs, $ref: `#/$defs/${name}` });
		const ok = validator("fenceResultOk")(fenceResultOk(5, 5, route(), "endpoint_possession", NOW));
		expect(ok, JSON.stringify(validator("fenceResultOk").errors)).toBe(true);
		expect(validator("fenceResultGenerationStale")(fenceGenerationStale(6, 5, NOW))).toBe(true);
		expect(validator("fenceResultRouteStale")(fenceRouteStale(route({ routeRevision: 2 }), 3, NOW))).toBe(true);
		expect(validator("fenceResultOwnerMismatch")(fenceOwnerMismatch(route(), "worker-2", 1, NOW))).toBe(true);
		expect(validator("fenceResultOwnerUnavailable")(fenceOwnerUnavailable(route(), NOW))).toBe(true);
		expect(validator("fenceResultAdmissionUnknown")(fenceAdmissionUnknown("torn_append", DIGEST, NOW))).toBe(true);
		expect(validator("routeTuple")(route())).toBe(true);
		const wf = evaluateWriterFence(
			{
				possession: possession(unixEndpoint()),
				leaseCompromised: false,
				controlDb: new FakeControlDb(5),
				adoptedGeneration: 5,
				now: () => NOW,
			},
			"pre_durable_append",
		);
		expect(validator("writerFenceAssertion")(wf), JSON.stringify(validator("writerFenceAssertion").errors)).toBe(
			true,
		);
	});
});
