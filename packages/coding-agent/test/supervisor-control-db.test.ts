import { chmodSync, lstatSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	CONTROL_DB_APPLICATION_ID,
	CONTROL_DB_FILE_NAME,
	CONTROL_DB_USER_VERSION,
	type DurabilityEvent,
	type EndpointIdentity,
	fenceResultAdmissionUnknown,
	fenceResultGenerationStale,
	fenceResultOk,
	fenceResultOwnerMismatch,
	fenceResultOwnerUnavailable,
	fenceResultRouteStale,
	MAX_GENERATION,
	type OpenControlDbOptions,
	probeSupervisorControlDbCapability,
	resetSupervisorControlDbDriverCacheForTest,
	SupervisorControlDb,
	SupervisorControlDbError,
	SupervisorControlDbGenerationStaleError,
	SupervisorWriterFenceError,
} from "../src/modes/daemon/supervisor-control-db.js";

const DIGEST = `sha256:${"a".repeat(64)}`;
const DIGEST_B = `sha256:${"b".repeat(64)}`;

function unixEndpoint(overrides: Partial<EndpointIdentity> = {}): EndpointIdentity {
	return { endpoint: "/run/prime/daemon.sock", endpointDev: 66310, endpointIno: 4242, platform: "unix", ...overrides };
}

let roots: string[] = [];

function makeRoot(): string {
	const root = mkdtempSync(join(tmpdir(), "wf-osfence-"));
	roots.push(root);
	return root;
}

function openOptions(root: string, overrides: Partial<OpenControlDbOptions> = {}): OpenControlDbOptions {
	return {
		root: join(root, "endpoint-key"),
		schemaDigest: DIGEST,
		incarnationId: "sup-inc-1",
		endpointIdentity: unixEndpoint(),
		pid: 4321,
		processStartId: "start-1",
		...overrides,
	};
}

beforeEach(() => {
	roots = [];
	resetSupervisorControlDbDriverCacheForTest();
});

afterEach(() => {
	for (const root of roots) rmSync(root, { recursive: true, force: true });
});

describe("probeSupervisorControlDbCapability", () => {
	it("reports available on a runtime with node:sqlite + WAL + FULL + IMMEDIATE", () => {
		if (process.platform === "win32") return;
		const probe = probeSupervisorControlDbCapability();
		expect(probe).toEqual({ capability: "available", reason: null });
	});

	it("reports unavailable on win32 without touching the filesystem", () => {
		if (process.platform !== "win32") return;
		expect(probeSupervisorControlDbCapability().capability).toBe("unavailable");
	});
});

describe("SupervisorControlDb.open + first init", () => {
	it("creates a hardened, tagged, WAL/FULL control DB with an owner-only dir and file", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const db = SupervisorControlDb.open(options);
		try {
			const dirStat = lstatSync(options.root);
			expect(dirStat.isDirectory()).toBe(true);
			expect(dirStat.mode & 0o077).toBe(0);

			const filePath = join(options.root, CONTROL_DB_FILE_NAME);
			const fileStat = lstatSync(filePath);
			expect(fileStat.isFile()).toBe(true);
			expect(fileStat.mode & 0o077).toBe(0);

			const raw = new DatabaseSync(filePath, { readOnly: true });
			try {
				expect(Number((raw.prepare("PRAGMA application_id").get() as Record<string, number>).application_id)).toBe(
					CONTROL_DB_APPLICATION_ID,
				);
				expect(Number((raw.prepare("PRAGMA user_version").get() as Record<string, number>).user_version)).toBe(
					CONTROL_DB_USER_VERSION,
				);
				expect(String((raw.prepare("PRAGMA journal_mode").get() as Record<string, string>).journal_mode)).toBe(
					"wal",
				);
			} finally {
				raw.close();
			}
		} finally {
			db.close();
		}
	});

	it("first_init writes generation 1 with a control_db_generation_cas authority signal and no revocation proof", () => {
		if (process.platform === "win32") return;
		const db = SupervisorControlDb.open(openOptions(makeRoot()));
		try {
			const record = db.acquire();
			expect(record).toMatchObject({
				protocol: "prime.workflow.osfence-acquisition/v2-slice3",
				kind: "first_init",
				priorGeneration: 0,
				generation: 1,
				authoritySignal: "control_db_generation_cas",
				revocationProof: null,
				capability: "available",
			});
			expect(db.adopted).toBe(1);
			expect(db.readSupervisorStateRow()?.generation).toBe(1);
		} finally {
			db.close();
		}
	});

	it("emits durable ordering: COMMIT before fsync_dir on every acquisition/mutation", () => {
		if (process.platform === "win32") return;
		const events: DurabilityEvent[] = [];
		const db = SupervisorControlDb.open(openOptions(makeRoot(), { onDurabilityEvent: (e) => events.push(e) }));
		try {
			db.acquire();
			db.setPhase("owner");
			const commits = events.map((e) => e.op);
			// open() emits a single fsync_dir; each committed mutation emits commit then fsync_dir.
			expect(commits.filter((op) => op === "commit").length).toBe(2);
			for (let i = 0; i < events.length - 1; i++) {
				if (events[i].op === "commit") {
					expect(events[i + 1].op).toBe("fsync_dir");
				}
			}
		} finally {
			db.close();
		}
	});
});

describe("Layer B monotonic generation + O2 paused-predecessor fence", () => {
	it("takeover advances exactly one generation and requires a revocation proof", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const first = SupervisorControlDb.open(options);
		first.acquire();
		first.close();

		const second = SupervisorControlDb.open(
			openOptions(makeRoot(), { root: options.root, incarnationId: "sup-inc-2" }),
		);
		try {
			expect(() => second.acquire()).toThrow(SupervisorControlDbError); // takeover without proof
			const record = second.acquire({ takeover: { revocationProof: "endpoint_released_kernel_confirmed" } });
			expect(record.kind).toBe("takeover");
			expect(record.priorGeneration).toBe(1);
			expect(record.generation).toBe(2);
			expect(record.revocationProof).toBe("endpoint_released_kernel_confirmed");
		} finally {
			second.close();
		}
	});

	it("O2: a paused predecessor's fenced mutation matches 0 rows -> GENERATION_STALE, zero effect", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const oldSup = SupervisorControlDb.open(options);
		oldSup.acquire(); // generation 1, oldSup.adopted == 1

		// A successor takes over out of band (predecessor is "paused").
		const newSup = SupervisorControlDb.open(
			openOptions(makeRoot(), { root: options.root, incarnationId: "sup-inc-2" }),
		);
		newSup.acquire({ takeover: { revocationProof: "operator_fence" } }); // generation 2
		newSup.setPhase("owner");
		newSup.close();

		// The resumed predecessor still believes it is generation 1.
		expect(() => oldSup.setPhase("stopping")).toThrow(SupervisorControlDbGenerationStaleError);

		// Zero effect: the stored generation and phase reflect only the successor.
		const verify = SupervisorControlDb.open(
			openOptions(makeRoot(), { root: options.root, incarnationId: "sup-inc-3" }),
		);
		try {
			const row = verify.readSupervisorStateRow();
			expect(row?.generation).toBe(2);
			expect(row?.phase).toBe("owner");
			expect(row?.incarnationId).toBe("sup-inc-2");
		} finally {
			verify.close();
			oldSup.close();
		}
	});
});

function writeRawControlDb(
	root: string,
	opts: { applicationId?: number; userVersion?: number; createTables?: boolean; mode?: number },
): string {
	mkdirSync(root, { recursive: true, mode: 0o700 });
	const filePath = join(root, CONTROL_DB_FILE_NAME);
	const raw = new DatabaseSync(filePath);
	try {
		if (opts.createTables) raw.exec("CREATE TABLE junk (a INTEGER PRIMARY KEY)");
		if (opts.applicationId !== undefined) raw.exec(`PRAGMA application_id = ${opts.applicationId}`);
		if (opts.userVersion !== undefined) raw.exec(`PRAGMA user_version = ${opts.userVersion}`);
	} finally {
		raw.close();
	}
	chmodSync(filePath, opts.mode ?? 0o600);
	return filePath;
}

describe("O3 fail-closed hardening", () => {
	it("rejects a group/other-accessible control root", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		mkdirSync(root, { recursive: true, mode: 0o700 });
		chmodSync(root, 0o755);
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(
			/group\/other-accessible|mode exceeds/,
		);
	});

	it("rejects a symlink where the control DB should be", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		mkdirSync(root, { recursive: true, mode: 0o700 });
		const target = join(makeRoot(), "elsewhere.db");
		writeFileSync(target, "");
		chmodSync(target, 0o600);
		symlinkSync(target, join(root, CONTROL_DB_FILE_NAME));
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(SupervisorControlDbError);
	});

	it("rejects a group/other-accessible control DB file", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		writeRawControlDb(root, {
			applicationId: CONTROL_DB_APPLICATION_ID,
			userVersion: 1,
			createTables: true,
			mode: 0o644,
		});
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(
			/group\/other-accessible|mode exceeds/,
		);
	});

	it("rejects a foreign application_id", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		writeRawControlDb(root, { applicationId: 12345, userVersion: 1, createTables: true, mode: 0o600 });
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(/foreign|application_id/i);
	});

	it("rejects an untagged file that already has tables (torn/foreign)", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		writeRawControlDb(root, { createTables: true, mode: 0o600 });
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(SupervisorControlDbError);
	});

	it("rejects a newer user_version", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		writeRawControlDb(root, { applicationId: CONTROL_DB_APPLICATION_ID, userVersion: 99, mode: 0o600 });
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(/user_version|newer/i);
	});

	it("rejects a tagged DB whose required tables are missing (torn schema)", () => {
		if (process.platform === "win32") return;
		const root = join(makeRoot(), "endpoint-key");
		writeRawControlDb(root, { applicationId: CONTROL_DB_APPLICATION_ID, userVersion: 1, mode: 0o600 });
		expect(() => SupervisorControlDb.open(openOptions(root, { root }))).toThrow(/torn|Missing required table/i);
	});
});

describe("O4 / BLOCK-4 Windows floor", () => {
	it("fails closed for a win32 endpoint identity regardless of host platform", () => {
		const root = makeRoot();
		const winIdentity: EndpointIdentity = {
			endpoint: "\\\\.\\pipe\\prime-daemon",
			endpointDev: null,
			endpointIno: null,
			platform: "win32",
		};
		expect(() => SupervisorControlDb.open(openOptions(root, { endpointIdentity: winIdentity }))).toThrow(
			/Windows|win32/i,
		);
	});

	it("fails closed for a unix endpoint missing dev/ino", () => {
		if (process.platform === "win32") return;
		const root = makeRoot();
		const bad = unixEndpoint({ endpointDev: null });
		expect(() => SupervisorControlDb.open(openOptions(root, { endpointIdentity: bad }))).toThrow(
			/dev and ino|endpoint/i,
		);
	});
});

describe("W1 worker generation + R1 route revision monotonicity", () => {
	it("reserves monotone per-worker generations under the fence", () => {
		if (process.platform === "win32") return;
		const db = SupervisorControlDb.open(openOptions(makeRoot()));
		try {
			db.acquire();
			expect(db.reserveWorkerGeneration("worker-1")).toBe(1);
			expect(db.reserveWorkerGeneration("worker-1")).toBe(2);
			expect(db.reserveWorkerGeneration("worker-2")).toBe(1);
			expect(db.readWorkerGeneration("worker-1")).toBe(2);
		} finally {
			db.close();
		}
	});

	it("bumps route_revision on every write and records updated_by_generation", () => {
		if (process.platform === "win32") return;
		const db = SupervisorControlDb.open(openOptions(makeRoot()));
		try {
			db.acquire();
			const r1 = db.writeRoute({
				rootSessionId: "root-1",
				directParentSessionId: "parent-1",
				workerId: "worker-1",
				workerGeneration: 1,
				state: "starting",
				descriptorDigest: DIGEST,
			});
			expect(r1.routeRevision).toBe(1);
			expect(r1.updatedByGeneration).toBe(1);
			const r2 = db.writeRoute({
				rootSessionId: "root-1",
				directParentSessionId: "parent-1",
				workerId: "worker-1",
				workerGeneration: 1,
				state: "ready",
				descriptorDigest: DIGEST,
			});
			expect(r2.routeRevision).toBe(2);
			expect(db.readWorkerRoute("root-1", "parent-1")?.routeRevision).toBe(2);
			expect(db.readWorkerRoute("root-1", "parent-1")?.state).toBe("ready");
		} finally {
			db.close();
		}
	});

	it("refuses fenced mutations before acquire()", () => {
		if (process.platform === "win32") return;
		const db = SupervisorControlDb.open(openOptions(makeRoot()));
		try {
			expect(() => db.reserveWorkerGeneration("worker-1")).toThrow(/not been acquired|acquire/i);
		} finally {
			db.close();
		}
	});

	it("refuses a second acquire on one handle", () => {
		if (process.platform === "win32") return;
		const db = SupervisorControlDb.open(openOptions(makeRoot()));
		try {
			db.acquire();
			expect(() => db.acquire()).toThrow(SupervisorControlDbError);
		} finally {
			db.close();
		}
	});
});

describe("Form 2 synchronous writer fence", () => {
	it("passes when possession, identity, and generation all hold", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const db = SupervisorControlDb.open(options);
		try {
			db.acquire();
			const assertion = db.assertWriterFence({
				endpointPossessed: true,
				endpointLeaseCompromised: false,
				observedEndpointIdentity: options.endpointIdentity,
				callSite: "pre_durable_append",
			});
			expect(assertion.outcome).toBe("pass");
			expect(assertion.adoptedGeneration).toBe(assertion.observedGeneration);
		} finally {
			db.close();
		}
	});

	it("throws (zero native effect) when possession is lost or the lease is compromised", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const db = SupervisorControlDb.open(options);
		try {
			db.acquire();
			expect(() =>
				db.assertWriterFence({
					endpointPossessed: false,
					endpointLeaseCompromised: false,
					observedEndpointIdentity: options.endpointIdentity,
					callSite: "pre_durable_append",
				}),
			).toThrow(SupervisorWriterFenceError);
			expect(() =>
				db.assertWriterFence({
					endpointPossessed: true,
					endpointLeaseCompromised: true,
					observedEndpointIdentity: options.endpointIdentity,
					callSite: "pre_replay_verify",
				}),
			).toThrow(/lease compromised/);
		} finally {
			db.close();
		}
	});

	it("throws when the observed endpoint identity changed", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const db = SupervisorControlDb.open(options);
		try {
			db.acquire();
			expect(() =>
				db.assertWriterFence({
					endpointPossessed: true,
					endpointLeaseCompromised: false,
					observedEndpointIdentity: unixEndpoint({ endpointIno: 9999 }),
					callSite: "pre_durable_append",
				}),
			).toThrow(/identity changed/);
		} finally {
			db.close();
		}
	});

	it("throws when a successor advanced the generation (defense-in-depth check b)", () => {
		if (process.platform === "win32") return;
		const options = openOptions(makeRoot());
		const oldSup = SupervisorControlDb.open(options);
		oldSup.acquire();
		const newSup = SupervisorControlDb.open(
			openOptions(makeRoot(), { root: options.root, incarnationId: "sup-inc-2" }),
		);
		newSup.acquire({ takeover: { revocationProof: "operator_fence" } });
		newSup.close();
		try {
			expect(() =>
				oldSup.assertWriterFence({
					endpointPossessed: true,
					endpointLeaseCompromised: false,
					observedEndpointIdentity: options.endpointIdentity,
					callSite: "pre_durable_append",
				}),
			).toThrow(/generation advanced/);
		} finally {
			oldSup.close();
		}
	});
});

describe("fence-result vocabulary builders (schema-conformant, fail-closed)", () => {
	const route = {
		rootSessionId: "root-1",
		directParentSessionId: "parent-1",
		workerId: "worker-1",
		workerGeneration: 1,
		routeRevision: 2,
	};

	it("builds OK only when observed == adopted", () => {
		const ok = fenceResultOk({
			observedGeneration: 5,
			adoptedGeneration: 5,
			route,
			authoritySignal: "endpoint_possession",
		});
		expect(ok).toMatchObject({ code: "OK", zeroEffect: false, capability: "available" });
		expect(() =>
			fenceResultOk({ observedGeneration: 6, adoptedGeneration: 5, route, authoritySignal: "endpoint_possession" }),
		).toThrow();
	});

	it("builds GENERATION_STALE only when observed > adopted", () => {
		expect(fenceResultGenerationStale({ observedGeneration: 6, adoptedGeneration: 5 }).zeroEffect).toBe(true);
		expect(() => fenceResultGenerationStale({ observedGeneration: 5, adoptedGeneration: 5 })).toThrow();
	});

	it("builds ROUTE_STALE only on a revision mismatch", () => {
		expect(fenceResultRouteStale({ route, currentRouteRevision: 3 }).code).toBe("ROUTE_STALE");
		expect(() => fenceResultRouteStale({ route, currentRouteRevision: 2 })).toThrow();
	});

	it("builds OWNER_MISMATCH only on a worker mismatch", () => {
		expect(
			fenceResultOwnerMismatch({ requestedRoute: route, resolvedWorkerId: "worker-2", resolvedWorkerGeneration: 1 })
				.code,
		).toBe("OWNER_MISMATCH");
		expect(() =>
			fenceResultOwnerMismatch({ requestedRoute: route, resolvedWorkerId: "worker-1", resolvedWorkerGeneration: 1 }),
		).toThrow();
	});

	it("builds OWNER_UNAVAILABLE (recovering) and ADMISSION_UNKNOWN", () => {
		expect(fenceResultOwnerUnavailable({ route }).routeState).toBe("recovering");
		expect(fenceResultAdmissionUnknown({ reason: "torn_append", evidenceDigest: DIGEST_B }).code).toBe(
			"ADMISSION_UNKNOWN",
		);
	});
});

describe("generation range constant", () => {
	it("matches Number.MAX_SAFE_INTEGER", () => {
		expect(MAX_GENERATION).toBe(Number.MAX_SAFE_INTEGER);
	});
});
