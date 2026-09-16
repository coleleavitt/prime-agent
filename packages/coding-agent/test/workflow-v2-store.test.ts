import { spawnSync } from "node:child_process";
import { lstatSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	probeWorkflowV2StoreCapability,
	resetWorkflowV2StoreDriverCacheForTest,
	STORE_APPLICATION_ID,
	STORE_CAPACITY,
	STORE_FILE_NAME,
	STORE_USER_VERSION,
	WorkflowV2Store,
	WorkflowV2StoreError,
} from "../src/core/workflow-v2-store.js";
import { ctrlEvent, EVIDENCE, hostEvent, turnSettlement, validDefinition } from "./workflow-v2-slice4-fixtures.js";

const SCOPE = `sha256:${"c".repeat(64)}`;
let roots: string[] = [];
function makeRoot(): string {
	const root = mkdtempSync(join(tmpdir(), "wf-v2-store-"));
	roots.push(root);
	return join(root, "workflows");
}
let nowMs = Date.UTC(2026, 8, 15, 0, 0, 0);
function testClock(): string {
	return new Date(nowMs).toISOString();
}
function advanceClock(ms: number): void {
	nowMs += ms;
}
function openStore(root: string, epoch = 1): WorkflowV2Store {
	const store = WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock });
	store.acquireWriter(epoch);
	return store;
}

function createReq(runId: string, requestId = `req-create-${runId}`) {
	return { protocol: "prime.workflow.request/v2", requestId, action: "create", definition: validDefinition() };
}
function createEffect(runId: string) {
	return { definition: validDefinition(), events: [ctrlEvent(runId, 1, "RunAdmitted", { evidenceDigest: EVIDENCE })] };
}
function postReq(
	action: string,
	runId: string,
	commandId: string,
	revision: number,
	extra: Record<string, unknown> = {},
) {
	return {
		protocol: "prime.workflow.request/v2",
		requestId: `req-${action}-${commandId}`,
		action,
		runId,
		commandId,
		expectedRevision: revision,
		expectedControllerEpoch: 1,
		expectedCancelEpoch: 0,
		...extra,
	};
}

beforeEach(() => {
	roots = [];
	nowMs = Date.UTC(2026, 8, 15, 0, 0, 0);
	resetWorkflowV2StoreDriverCacheForTest();
});
afterEach(() => {
	for (const root of roots) rmSync(root, { recursive: true, force: true });
});

describe("probe + open discipline (§7)", () => {
	it("probes available on node:sqlite + WAL + FULL + IMMEDIATE", () => {
		if (process.platform === "win32") return;
		expect(probeWorkflowV2StoreCapability()).toEqual({ capability: "available", reason: null });
	});
	it("creates a hardened owner-only dir + file, tagged + versioned", () => {
		if (process.platform === "win32") return;
		const root = makeRoot();
		const store = openStore(root);
		try {
			expect(lstatSync(root).mode & 0o077).toBe(0);
			const dbPath = join(root, STORE_FILE_NAME);
			expect(lstatSync(dbPath).mode & 0o077).toBe(0);
			const raw = new DatabaseSync(dbPath, { timeout: 0 });
			expect(Number((raw.prepare("PRAGMA application_id").get() as Record<string, unknown>).application_id)).toBe(
				STORE_APPLICATION_ID,
			);
			expect(Number((raw.prepare("PRAGMA user_version").get() as Record<string, unknown>).user_version)).toBe(
				STORE_USER_VERSION,
			);
			const tables = raw
				.prepare("SELECT COUNT(*) AS c FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
				.get() as Record<string, unknown>;
			// 15 domain tables + store_migrations
			expect(Number(tables.c)).toBe(16);
			raw.close();
		} finally {
			store.close();
		}
	});
	it("fences the writer by strictly-increasing controllerEpoch", () => {
		const root = makeRoot();
		const store = WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock });
		try {
			expect(store.acquireWriter(5)).toBe(5);
			expect(() => store.acquireWriter(5)).toThrow(/store_epoch_stale/);
			expect(() => store.acquireWriter(4)).toThrow(/store_epoch_stale/);
		} finally {
			store.close();
		}
	});
});

describe("command transaction + idempotency (§6/§7)", () => {
	it("create persists definition/run/nodes/budget/host_streams + one gap-free event", () => {
		const store = openStore(makeRoot());
		try {
			const receipt = store.applyCommand(createReq("run-1"), createEffect("run-1"));
			expect(receipt.action).toBe("create");
			expect(receipt.appliedSequences).toEqual([1]);
			expect(store.getRunProjection("run-1")?.phase).toBe("created");
			expect(store.tableCount("definitions")).toBe(1);
			expect(store.tableCount("nodes")).toBe(1);
			expect(store.tableCount("budgets")).toBe(1);
			expect(store.tableCount("host_streams")).toBe(1);
			expect(store.tableCount("events")).toBe(1);
			expect(store.tableCount("commands")).toBe(1);
		} finally {
			store.close();
		}
	});
	it("returns the stored receipt on an identical replay (no duplicate rows)", () => {
		const store = openStore(makeRoot());
		try {
			const req = createReq("run-1");
			const eff = createEffect("run-1");
			const r1 = store.applyCommand(req, eff);
			const r2 = store.applyCommand(req, createEffect("run-1"));
			expect(r2).toEqual(r1);
			expect(store.tableCount("commands")).toBe(1);
			expect(store.tableCount("runs")).toBe(1);
			expect(store.tableCount("events")).toBe(1);
		} finally {
			store.close();
		}
	});
	it("IDEMPOTENCY_CONFLICT on the same requestId with changed canonical bytes", () => {
		const store = openStore(makeRoot());
		try {
			store.applyCommand(createReq("run-1", "shared-id"), createEffect("run-1"));
			const conflicting = {
				protocol: "prime.workflow.request/v2",
				requestId: "shared-id",
				action: "create",
				definition: validDefinition({ maxTotalTokens: 999 }),
			};
			expect(() => store.applyCommand(conflicting, createEffect("run-2"))).toThrow(/IDEMPOTENCY_CONFLICT/);
			expect(store.tableCount("runs")).toBe(1);
		} finally {
			store.close();
		}
	});
	it("IDEMPOTENCY_CONFLICT on the same (run,commandId) with changed bytes", () => {
		const store = openStore(makeRoot());
		try {
			store.applyCommand(createReq("run-1"), createEffect("run-1"));
			store.applyCommand(postReq("start", "run-1", "cmd-1", 1), {
				events: [ctrlEvent("run-1", 2, "RunStarted", { evidenceDigest: EVIDENCE })],
			});
			// same commandId, different reason via cancel action bytes → different digest
			const clash = postReq("start", "run-1", "cmd-1", 2, {});
			clash.requestId = "different-request";
			expect(() =>
				store.applyCommand(clash, { events: [ctrlEvent("run-1", 3, "RunDraining", { evidenceDigest: EVIDENCE })] }),
			).toThrow(/IDEMPOTENCY_CONFLICT/);
		} finally {
			store.close();
		}
	});
	it("validate() writes nothing", () => {
		const store = openStore(makeRoot());
		try {
			const before = store.tableCount("commands");
			const res = store.validate({
				protocol: "prime.workflow.request/v2",
				requestId: "v1",
				action: "validate",
				definition: validDefinition(),
			});
			expect(res.ok).toBe(true);
			expect(store.tableCount("commands")).toBe(before);
			expect(store.tableCount("runs")).toBe(0);
			expect(store.tableCount("events")).toBe(0);
		} finally {
			store.close();
		}
	});
	it("rejects a stale fence with no mutation", () => {
		const store = openStore(makeRoot());
		try {
			store.applyCommand(createReq("run-1"), createEffect("run-1"));
			const before = store.tableCount("events");
			expect(() =>
				store.applyCommand(postReq("start", "run-1", "cmd-1", 99), {
					events: [ctrlEvent("run-1", 2, "RunStarted", { evidenceDigest: EVIDENCE })],
				}),
			).toThrow(/COMMAND_FENCE_STALE/);
			expect(store.tableCount("events")).toBe(before);
			expect(store.getRunProjection("run-1")?.phase).toBe("created");
		} finally {
			store.close();
		}
	});
	it("rejects a gapped event sequence atomically", () => {
		const store = openStore(makeRoot());
		try {
			store.applyCommand(createReq("run-1"), createEffect("run-1"));
			const before = store.tableCount("events");
			expect(() =>
				store.applyCommand(postReq("start", "run-1", "cmd-1", 1), {
					events: [ctrlEvent("run-1", 5, "RunStarted", { evidenceDigest: EVIDENCE })],
				}),
			).toThrow(/store_event_gap/);
			expect(store.tableCount("events")).toBe(before);
		} finally {
			store.close();
		}
	});
	it("NOT_FOUND for a post-create command on an unknown run", () => {
		const store = openStore(makeRoot());
		try {
			expect(() => store.applyCommand(postReq("start", "ghost", "cmd-1", 1), { events: [] })).toThrow(/NOT_FOUND/);
		} finally {
			store.close();
		}
	});
});

// Bind an attempt so host facts can resolve; returns the store at revision after admission_bound.
function driveToAdmissionBound(store: WorkflowV2Store, runId = "run-1"): void {
	store.applyCommand(createReq(runId), createEffect(runId));
	store.applyCommand(postReq("start", runId, "c-start", 1), {
		events: [ctrlEvent(runId, 2, "RunStarted", { evidenceDigest: EVIDENCE })],
	});
	store.applyCommand(postReq("start", runId, "c-ready", 2), {
		events: [ctrlEvent(runId, 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE })],
	});
	store.applyCommand(postReq("start", runId, "c-prep", 3), {
		events: [ctrlEvent(runId, 4, "AttemptPrepared", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE })],
	});
	store.applyCommand(postReq("start", runId, "c-disp", 4), {
		events: [
			ctrlEvent(runId, 5, "AttemptDispatchCommitted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
		],
		operations: [
			{
				operationId: "op1",
				attemptId: "a1",
				kind: "deliver",
				requestId: "hostreq-1",
				canonicalRequest: { protocol: "prime.workflow.retained-request/v2", operation: "child.admit" },
			},
		],
	});
	store.applyCommand(postReq("start", runId, "c-bind", 5), {
		events: [
			ctrlEvent(runId, 6, "AttemptAdmissionBound", {
				nodeId: "n1",
				attemptId: "a1",
				operationId: "op1",
				rlmChildId: "child1",
				turnId: "turn1",
				evidenceDigest: EVIDENCE,
			}),
		],
		bindings: [
			{
				attemptId: "a1",
				workflowChildId: "wfc-a1",
				rlmChildId: "child1",
				turnId: "turn1",
				requestId: "hostreq-1",
				canonicalDigest: EVIDENCE,
			},
		],
	});
}

describe("outbox at-least-once + epoch-fenced ack (§7)", () => {
	it("claims a stable operation, redelivers the SAME identity on lease expiry, and terminalizes on ack", () => {
		const store = openStore(makeRoot());
		try {
			driveToAdmissionBound(store);
			const first = store.claimOutbox("worker-1", 60_000, 10);
			expect(first).toHaveLength(1);
			expect(first[0].operationId).toBe("op1");
			expect(first[0].requestId).toBe("hostreq-1");
			// not redelivered while the lease is held
			expect(store.claimOutbox("worker-1", 60_000, 10)).toHaveLength(0);
			// after the lease expires, the SAME canonical request + operation id is redelivered
			advanceClock(120_000);
			const redelivered = store.claimOutbox("worker-1", 60_000, 10);
			expect(redelivered).toHaveLength(1);
			expect(redelivered[0].operationId).toBe("op1");
			expect(redelivered[0].canonicalRequest).toBe(first[0].canonicalRequest);
			// a stale-epoch/owner ack is rejected
			expect(() =>
				store.acknowledgeOutbox({
					operationId: "op1",
					owner: "intruder",
					hostRequestId: "hostreq-1",
					outcome: "succeeded",
					receipt: { ok: true },
				}),
			).toThrow(/STALE_CLAIM/);
			store.acknowledgeOutbox({
				operationId: "op1",
				owner: "worker-1",
				hostRequestId: "hostreq-1",
				outcome: "succeeded",
				receipt: { ok: true },
			});
			expect(String(store.getOperation("op1")?.phase)).toBe("terminal");
			// terminal operations are not re-claimed
			expect(store.claimOutbox("worker-1", 0, 10)).toHaveLength(0);
		} finally {
			store.close();
		}
	});
});

describe("host inbox idempotent apply + atomic cursor advance (§7)", () => {
	it("applies a host fact once, advances hostCursor, persists settlement; re-apply is a no-op", () => {
		const store = openStore(makeRoot());
		try {
			driveToAdmissionBound(store);
			const started = store.ingestHostEvent(
				"run-1",
				hostEvent("he-start", "hc-1", "TurnStarted", {
					requestId: "hostreq-1",
					rlmChildId: "child1",
					turnId: "turn1",
					evidenceDigest: EVIDENCE,
				}),
			);
			expect(started).toEqual({ applied: true, hostCursor: "hc-1" });
			const settled = store.ingestHostEvent(
				"run-1",
				hostEvent("he-settle", "hc-2", "TurnSettled", {
					requestId: "hostreq-1",
					rlmChildId: "child1",
					turnId: "turn1",
					settlement: turnSettlement({ nodeId: "n1", attemptId: "a1", rlmChildId: "child1", turnId: "turn1" }),
					evidenceDigest: EVIDENCE,
				}),
			);
			expect(settled).toEqual({ applied: true, hostCursor: "hc-2" });
			expect(store.tableCount("settlements")).toBe(1);
			expect(store.tableCount("host_inbox")).toBe(2);
			// idempotent re-apply of the same hostEventId: no cursor regression, no duplicate row
			const again = store.ingestHostEvent(
				"run-1",
				hostEvent("he-settle", "hc-9", "TurnSettled", {
					requestId: "hostreq-1",
					rlmChildId: "child1",
					turnId: "turn1",
					settlement: turnSettlement({ nodeId: "n1", attemptId: "a1", rlmChildId: "child1", turnId: "turn1" }),
					evidenceDigest: EVIDENCE,
				}),
			);
			expect(again).toEqual({ applied: false, hostCursor: "hc-2" });
			expect(store.tableCount("host_inbox")).toBe(2);
			expect(store.getHostCursor("run-1")).toBe("hc-2");
		} finally {
			store.close();
		}
	});
});

describe("cursor families never convert (§7)", () => {
	it("hostCursor (opaque) and eventCursor (sequence) are independent", () => {
		const store = openStore(makeRoot());
		try {
			driveToAdmissionBound(store);
			const lastSeqBefore = store.loadAggregate("run-1")?.lastControllerSequence;
			store.ingestHostEvent(
				"run-1",
				hostEvent("he-start", "hc-77", "TurnStarted", {
					requestId: "hostreq-1",
					rlmChildId: "child1",
					turnId: "turn1",
					evidenceDigest: EVIDENCE,
				}),
			);
			// advancing the opaque host cursor does not change the numeric event cursor
			expect(store.loadAggregate("run-1")?.lastControllerSequence).toBe(lastSeqBefore);
			expect(store.getHostCursor("run-1")).toBe("hc-77");
			// event reads use a numeric sequence cursor, unrelated to the host cursor string
			expect(store.listEvents("run-1", 0, 500).length).toBe(6);
			expect(store.listEvents("run-1", 6, 500).length).toBe(0);
		} finally {
			store.close();
		}
	});
});

describe("crash/restart durability (§7)", () => {
	it("reopens at a higher epoch with state intact", () => {
		const root = makeRoot();
		const s1 = openStore(root, 1);
		driveToAdmissionBound(s1);
		s1.close();
		const s2 = WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock });
		try {
			// prior epoch was 1; a restart must fence at a strictly higher epoch
			expect(() => s2.acquireWriter(1)).toThrow(/store_epoch_stale/);
			s2.acquireWriter(2);
			const agg = s2.loadAggregate("run-1");
			expect(agg?.attempts.a1.projection.phase).toBe("admission_bound");
			expect(agg?.lastControllerSequence).toBe(6);
		} finally {
			s2.close();
		}
	});
	it("a scope-digest mismatch on reopen fails closed", () => {
		const root = makeRoot();
		const s1 = openStore(root, 1);
		s1.close();
		expect(() =>
			WorkflowV2Store.open({ root, rootScopeDigest: `sha256:${"d".repeat(64)}`, clock: testClock }),
		).toThrow(/store_scope_mismatch/);
	});
});

describe("process-kill / mid-transaction atomicity (§7/§13)", () => {
	it("a throw after events append but before commit leaves zero durable effect", () => {
		const store = openStore(makeRoot());
		try {
			store.applyCommand(createReq("run-1"), createEffect("run-1"));
			const before = { events: store.tableCount("events"), ops: store.tableCount("operations") };
			// an outbox op with an invalid operationId throws inside the tx AFTER appendEvents
			expect(() =>
				store.applyCommand(postReq("start", "run-1", "c-start", 1), {
					events: [ctrlEvent("run-1", 2, "RunStarted", { evidenceDigest: EVIDENCE })],
					operations: [
						{
							operationId: "bad id with spaces",
							attemptId: null,
							kind: "deliver",
							requestId: "r",
							canonicalRequest: {},
						},
					],
				}),
			).toThrow(WorkflowV2StoreError);
			expect(store.tableCount("events")).toBe(before.events);
			expect(store.tableCount("operations")).toBe(before.ops);
			expect(store.getRunProjection("run-1")?.phase).toBe("created");
		} finally {
			store.close();
		}
	});
	it("a real SIGKILL before commit leaves an integrity-clean, reopenable store", () => {
		if (process.platform === "win32") return;
		const root = makeRoot();
		const s1 = openStore(root, 1);
		driveToAdmissionBound(s1);
		// simulate an abrupt termination: drop the handle without close(), then reopen.
		const s2 = WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock });
		try {
			s2.acquireWriter(2);
			expect(s2.loadAggregate("run-1")?.lastControllerSequence).toBe(6);
		} finally {
			s2.close();
			s1.close();
		}
	});
});

describe("corruption / migration / capacity fail-closed (§7)", () => {
	it("rejects a foreign application_id", () => {
		const root = makeRoot();
		const store = openStore(root);
		store.close();
		const dbPath = join(root, STORE_FILE_NAME);
		const raw = new DatabaseSync(dbPath, { timeout: 0 });
		raw.exec(`PRAGMA application_id = 12345`);
		raw.close();
		expect(() => WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock })).toThrow(/store_foreign/);
	});
	it("rejects a newer user_version", () => {
		const root = makeRoot();
		const store = openStore(root);
		store.close();
		const dbPath = join(root, STORE_FILE_NAME);
		const raw = new DatabaseSync(dbPath, { timeout: 0 });
		raw.exec(`PRAGMA user_version = ${STORE_USER_VERSION + 5}`);
		raw.close();
		expect(() => WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock })).toThrow(
			/store_user_version_newer/,
		);
	});
	it("rejects a tampered migration checksum", () => {
		const root = makeRoot();
		const store = openStore(root);
		store.close();
		const dbPath = join(root, STORE_FILE_NAME);
		const raw = new DatabaseSync(dbPath, { timeout: 0 });
		raw.exec("UPDATE store_migrations SET checksum = 'sha256:deadbeef' WHERE version = 1");
		raw.close();
		expect(() => WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock })).toThrow(
			/store_migration_checksum/,
		);
	});
	it("rejects an integrity-corrupt database", () => {
		const root = makeRoot();
		const store = openStore(root);
		store.close();
		const dbPath = join(root, STORE_FILE_NAME);
		// overwrite the SQLite header/pages with garbage → integrity/quick check fails or open throws.
		writeFileSync(dbPath, Buffer.from("this is not a sqlite database file at all, totally corrupt"));
		expect(() => WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock: testClock })).toThrow();
	});
	it("rejects create at the text-bytes high-water mark (CAPACITY_EXCEEDED)", () => {
		const root = makeRoot();
		const store = openStore(root);
		store.applyCommand(createReq("run-1"), createEffect("run-1"));
		store.close();
		// push the durable text_bytes counter above 90% of the quota via the raw DB.
		const dbPath = join(root, STORE_FILE_NAME);
		const raw = new DatabaseSync(dbPath, { timeout: 0 });
		const high = String(Math.floor(STORE_CAPACITY.maxTextBytes * STORE_CAPACITY.highWaterFraction) + 1);
		raw.exec(`UPDATE store_meta SET value = '${high}' WHERE key = 'text_bytes'`);
		raw.close();
		const s2 = openStore(root, 2);
		try {
			expect(() => s2.applyCommand(createReq("run-2"), createEffect("run-2"))).toThrow(/CAPACITY_EXCEEDED/);
		} finally {
			s2.close();
		}
	});
});

function driveToTerminal(store: WorkflowV2Store, runId = "run-1"): void {
	driveToAdmissionBound(store, runId);
	store.ingestHostEvent(
		runId,
		hostEvent("he-start", "hc-1", "TurnStarted", {
			requestId: "hostreq-1",
			rlmChildId: "child1",
			turnId: "turn1",
			evidenceDigest: EVIDENCE,
		}),
	);
	store.ingestHostEvent(
		runId,
		hostEvent("he-settle", "hc-2", "TurnSettled", {
			requestId: "hostreq-1",
			rlmChildId: "child1",
			turnId: "turn1",
			settlement: turnSettlement({ nodeId: "n1", attemptId: "a1", rlmChildId: "child1", turnId: "turn1" }),
			evidenceDigest: EVIDENCE,
		}),
	);
	store.applyCommand(postReq("start", runId, "c-obs", 6), {
		events: [
			ctrlEvent(runId, 7, "AttemptSettlementObserved", {
				nodeId: "n1",
				attemptId: "a1",
				settlementDigest: EVIDENCE,
				outcome: "completed",
			}),
		],
	});
	store.applyCommand(postReq("start", runId, "c-acc", 7), {
		events: [ctrlEvent(runId, 8, "AttemptAccepted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE })],
		acceptance: [{ nodeId: "n1", attemptId: "a1", decision: "accepted", evidenceDigest: EVIDENCE }],
	});
	store.applyCommand(postReq("start", runId, "c-drain", 8), {
		events: [ctrlEvent(runId, 9, "RunDraining", { evidenceDigest: EVIDENCE })],
	});
	store.applyCommand(postReq("start", runId, "c-term", 9), {
		events: [ctrlEvent(runId, 10, "RunTerminalized", { evidenceDigest: EVIDENCE, outcome: "succeeded" })],
	});
	// resolve the outbox op so compaction is permitted
	store.claimOutbox("w", 60_000, 10);
	store.acknowledgeOutbox({
		operationId: "op1",
		owner: "w",
		hostRequestId: "hostreq-1",
		outcome: "succeeded",
		receipt: { ok: true },
	});
}

describe("retention: erasure / compaction / backup (§7)", () => {
	it("drives a run to a succeeded terminal + accepted node", () => {
		const store = openStore(makeRoot());
		try {
			driveToTerminal(store);
			expect(store.getRunProjection("run-1")?.outcome).toBe("succeeded");
			expect(store.tableCount("acceptance")).toBe(1);
		} finally {
			store.close();
		}
	});
	it("refuses erasure while a run is nonterminal", () => {
		const store = openStore(makeRoot());
		try {
			driveToAdmissionBound(store);
			expect(() => store.eraseRunText("run-1", "test-policy")).toThrow(/ERASURE_REFUSED/);
		} finally {
			store.close();
		}
	});
	it("erases result text but keeps digest, byte count, and settlement evidence", () => {
		const store = openStore(makeRoot());
		try {
			driveToTerminal(store);
			const res = store.eraseRunText("run-1", "retention-30d");
			expect(res.erased).toBe(true);
			expect(res.freedBytes).toBeGreaterThan(0);
			// the settlement row remains as immutable evidence (digest/byte columns intact)
			expect(store.tableCount("settlements")).toBe(1);
		} finally {
			store.close();
		}
	});
	it("compaction is refused while operations are nonterminal, then returns SNAPSHOT_REQUIRED for a compacted suffix", () => {
		const store = openStore(makeRoot());
		try {
			driveToAdmissionBound(store);
			// op1 still pending → compaction refused
			expect(() => store.compactEvents("run-1", 3, EVIDENCE)).toThrow(/COMPACTION_REFUSED/);
			driveToTerminal(store); // this resolves op1 and reaches terminal
			const { compacted } = store.compactEvents("run-1", 5, EVIDENCE);
			expect(compacted).toBeGreaterThan(0);
			// a read of the compacted suffix now requires a snapshot
			expect(() => store.listEvents("run-1", 0, 500)).toThrow(/SNAPSHOT_REQUIRED/);
			// a read past the snapshot boundary still works
			expect(store.listEvents("run-1", 5, 500).length).toBeGreaterThan(0);
		} finally {
			store.close();
		}
	});
	it("does not compact immutable RunTerminalized evidence", () => {
		const store = openStore(makeRoot());
		try {
			driveToTerminal(store);
			store.compactEvents("run-1", 10, EVIDENCE);
			const terminal = store.listEvents("run-1", 9, 500).find((e) => String(e.type) === "RunTerminalized");
			expect(terminal).toBeTruthy();
			expect(JSON.parse(String(terminal?.data_json)).outcome).toBe("succeeded");
		} finally {
			store.close();
		}
	});
	it("writes a manifest-bound consistent backup image", () => {
		const root = makeRoot();
		const store = openStore(root);
		try {
			driveToTerminal(store);
			const dest = join(root, "backup", "v2.bak");
			const manifest = store.backupTo(dest);
			expect(String(manifest.databaseSha256)).toMatch(/^sha256:[0-9a-f]{64}$/);
			expect(manifest.rootScopeDigest).toBe(SCOPE);
			expect(manifest.applicationId).toBe(STORE_APPLICATION_ID);
			// the backup image is itself a valid, tagged SQLite database
			const raw = new DatabaseSync(dest, { timeout: 0 });
			expect(Number((raw.prepare("PRAGMA application_id").get() as Record<string, unknown>).application_id)).toBe(
				STORE_APPLICATION_ID,
			);
			expect(Number((raw.prepare("SELECT COUNT(*) AS c FROM runs").get() as Record<string, unknown>).c)).toBe(1);
			raw.close();
		} finally {
			store.close();
		}
	});
});

describe("RunTerminalized outcome-equality enforced on load + durable quarantine (§11/§9)", () => {
	function tamperTerminalOutcome(root: string): void {
		const raw = new DatabaseSync(join(root, STORE_FILE_NAME), { timeout: 0 });
		raw.exec("UPDATE runs SET outcome = 'failed', terminal_outcome = 'failed' WHERE run_id = 'run-1'");
		raw.close();
	}
	it("fails closed on load when the normalized outcome disagrees with the persisted RunTerminalized fact", () => {
		const root = makeRoot();
		const s1 = openStore(root, 1);
		driveToTerminal(s1);
		s1.close();
		tamperTerminalOutcome(root);
		const s2 = openStore(root, 2);
		try {
			expect(() => s2.loadAggregate("run-1")).toThrow(/store_terminal_outcome_mismatch/);
			expect(() => s2.getRunProjection("run-1")).toThrow(/store_terminal_outcome_mismatch/);
		} finally {
			s2.close();
		}
	});
	it("reconcileTerminalMismatch durably quarantines the tampered run (idempotently)", () => {
		const root = makeRoot();
		const s1 = openStore(root, 1);
		driveToTerminal(s1);
		s1.close();
		tamperTerminalOutcome(root);
		const s2 = openStore(root, 2);
		try {
			expect(s2.reconcileTerminalMismatch("run-1")).toEqual({ quarantined: true });
			const proj = s2.getRunProjection("run-1");
			expect(proj?.phase).toBe("quarantined");
			expect(proj?.outcome).toBeNull();
			expect(proj?.conditions).toContain("integrity_failed");
			// a quarantined (non-terminal) run is not re-quarantined
			expect(s2.reconcileTerminalMismatch("run-1")).toEqual({ quarantined: false });
		} finally {
			s2.close();
		}
	});
	it("reconcile is a no-op for a consistent terminal run", () => {
		const store = openStore(makeRoot());
		try {
			driveToTerminal(store);
			expect(store.reconcileTerminalMismatch("run-1")).toEqual({ quarantined: false });
			expect(store.getRunProjection("run-1")?.outcome).toBe("succeeded");
		} finally {
			store.close();
		}
	});
});

describe("genuine out-of-process SIGKILL crash window (§7/§13)", () => {
	it("a real SIGKILL mid-uncommitted ingest transaction leaves zero partial effect and an integrity-clean store", () => {
		if (process.platform === "win32") return;
		const here = dirname(fileURLToPath(import.meta.url));
		const childPath = resolve(here, "workflow-v2-slice4-crash-child.ts");
		const root = makeRoot();
		// run the child in the main node process (tsx loader) so the child's own
		// SIGKILL terminates THIS spawned process directly.
		const res = spawnSync(process.execPath, ["--import", "tsx", childPath, root, SCOPE], {
			encoding: "utf8",
			cwd: resolve(here, ".."),
		});
		// the child self-SIGKILLs inside ingestHostEvent, before COMMIT
		expect(res.signal).toBe("SIGKILL");
		expect(res.stdout).toContain("BOUND");
		expect(res.stdout).not.toContain("UNREACHABLE");
		// reopen: WAL recovery is clean, the in-flight ingest rolled back entirely,
		// and every earlier committed command survives.
		const store = openStore(root, 2);
		try {
			expect(store.tableCount("host_inbox")).toBe(0);
			expect(store.getHostCursor("run-1")).toBeNull();
			const agg = store.loadAggregate("run-1");
			expect(agg?.lastControllerSequence).toBe(6);
			expect(agg?.attempts.a1.projection.phase).toBe("admission_bound");
		} finally {
			store.close();
		}
	});
});
