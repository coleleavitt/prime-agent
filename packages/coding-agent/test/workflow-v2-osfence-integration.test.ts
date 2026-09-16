import { createHash } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type {
	Slice3DispatchBinding,
	Slice3DispatchClaim,
	Slice3DispatchFacts,
	Slice3DispatchJournal,
} from "../src/core/workflow-v2-retained-executor.js";
import {
	type Fence as SettlementFence,
	type SettlementInput,
	validateSettlementCommit,
} from "../src/core/workflow-v2-settlement.js";
import {
	type AgentEndObservation,
	type AssistantTerminalObservation,
	type RawUsage,
	TerminalCaptureSlot,
	type TurnBinding,
} from "../src/core/workflow-v2-terminal-capture.js";
import {
	type EndpointIdentity,
	EndpointPossession,
	type OsfControlDb,
	type RouteTuple,
	SupervisorWriterFenceError,
} from "../src/modes/daemon/daemon-osfence.js";
import {
	type AdmitInput,
	type RlmLedgerAdmitRecord,
	Slice3CodecError,
	type Slice3Fence,
	type Slice3LedgerCodec,
} from "../src/modes/daemon/rlm-ledger.js";
import {
	type OpenControlDbOptions,
	probeSupervisorControlDbCapability,
	resetSupervisorControlDbDriverCacheForTest,
	SupervisorControlDb,
	SupervisorControlDbGenerationStaleError,
	SupervisorControlDbOsfAdapter,
} from "../src/modes/daemon/supervisor-control-db.js";
import {
	resolveWorkflowV2OsfencePipeline,
	WORKFLOW_V2_OSFENCE_BOUND_PROFILE,
	WorkflowV2OsfencePipeline,
	WorkflowV2OsfencePipelineError,
} from "../src/modes/daemon/workflow-v2-osfence-integration.js";

// This suite drives the WHOLE Slice 3 host stack behind the base-off guard through the OS fence:
// the real durable SQLite control.db (Layer B, Form-1 BEGIN IMMEDIATE), real EndpointPossession
// (Layer A), the writer-mode composite admission ledger, the at-most-once retained dispatch
// executor, the tools-none profile binding, and the pure settlement reducer. Every acceptance row is
// proven from durable bytes / OS state, never from sleeps, PID liveness, mtime, or TTL. The pipeline
// is constructed with capabilityAvailable:true only inside the test; production negotiation returns
// CAPABILITY_UNAVAILABLE, so this path never runs live (proven by the base-off gate test below).

function jcs(value: unknown): string {
	if (value === null) return "null";
	const t = typeof value;
	if (t === "boolean") return value ? "true" : "false";
	if (t === "number") {
		if (!Number.isFinite(value as number)) throw new Slice3CodecError("non-finite number");
		if (!Number.isSafeInteger(value as number)) throw new Slice3CodecError("non-integer number");
		return JSON.stringify(value);
	}
	if (t === "string") return JSON.stringify(value);
	if (Array.isArray(value)) return `[${value.map(jcs).join(",")}]`;
	if (t === "object") {
		const obj = value as Record<string, unknown>;
		const keys = Object.keys(obj).sort();
		return `{${keys.map((k) => `${JSON.stringify(k)}:${jcs(obj[k])}`).join(",")}}`;
	}
	throw new Slice3CodecError(`uncanonicalizable ${t}`);
}

const codecCanonicalize = (v: unknown) => Buffer.from(jcs(v), "utf8");
const codecDigest = (b: Buffer) => `sha256:${createHash("sha256").update(b).digest("hex")}`;

function makeReferenceCodec(onValidate?: (record: unknown) => void): Slice3LedgerCodec {
	return {
		canonicalize: (value) => codecCanonicalize(value),
		digest: (bytes) => codecDigest(bytes),
		encodeCanonicalBytes: (bytes) => bytes.toString("base64"),
		validateAdmitRecord: (record) => {
			onValidate?.(record);
			return validateAdmitRecordReference(record);
		},
	};
}

function eq(a: unknown, b: unknown, field: string): void {
	if (a !== b) throw new Slice3CodecError(`admit field mismatch: ${field}`);
}

function validateAdmitRecordReference(value: unknown): RlmLedgerAdmitRecord {
	if (!value || typeof value !== "object") throw new Slice3CodecError("admit not an object");
	const r = value as Record<string, unknown>;
	if (r.v !== 2 || r.op !== "admit") throw new Slice3CodecError("admit bad v/op");
	if (r.profile !== "workflow-v2-tools-none-v1" || r.tools !== "none" || r.maxTurns !== 1) {
		throw new Slice3CodecError("admit bad profile/tools/maxTurns");
	}
	if (typeof r.promptUtf8Bytes !== "number" || r.promptUtf8Bytes < 1 || r.promptUtf8Bytes > 65536) {
		throw new Slice3CodecError("admit promptUtf8Bytes out of bounds");
	}
	if (typeof r.depth !== "number" || r.depth < 1 || r.depth > 64) {
		throw new Slice3CodecError("admit depth out of bounds");
	}
	const decoded = r.decodedReceipt as Record<string, unknown> | undefined;
	if (!decoded || typeof decoded !== "object") throw new Slice3CodecError("admit missing decodedReceipt");
	const payload = decoded.payload as Record<string, unknown> | undefined;
	if (!payload || typeof payload !== "object") throw new Slice3CodecError("admit missing receipt payload");
	const recomputed = codecDigest(codecCanonicalize(payload));
	eq(decoded.digest, recomputed, "decodedReceipt.digest");
	eq(r.receiptDigest, recomputed, "receiptDigest");
	const receiptBytes = codecCanonicalize(decoded);
	if (typeof r.receipt !== "string" || r.receipt !== receiptBytes.toString("base64")) {
		throw new Slice3CodecError("receipt bytes mismatch");
	}
	for (const f of [
		"authorityId",
		"rootSessionId",
		"parentSessionId",
		"workflowRunId",
		"nodeId",
		"attemptId",
		"workflowChildId",
		"requestId",
		"requestDigest",
		"rlmChildId",
		"turnId",
		"effectiveModel",
		"profile",
		"effectiveThinkingLevel",
		"admissionSequence",
	]) {
		eq(payload[f], r[f], `payload.${f}`);
	}
	eq(payload.tools, "none", "payload.tools");
	eq(payload.maxTurns, 1, "payload.maxTurns");
	eq(payload.operation, "child.admit", "payload.operation");
	eq(jcs(payload.fence), jcs(r.fence), "payload.fence");
	if (typeof r.canonicalRequest !== "string") throw new Slice3CodecError("canonicalRequest missing");
	if (Buffer.from(r.canonicalRequest, "base64").length > 1_048_576) {
		throw new Slice3CodecError("canonicalRequest too large");
	}
	return value as RlmLedgerAdmitRecord;
}

function makeInput(overrides: Partial<AdmitInput> = {}): AdmitInput {
	const canonicalRequest = overrides.canonicalRequest ?? Buffer.from('{"prompt":"hello"}', "utf8");
	return {
		fence: FENCE,
		authorityId: "authority-1",
		rootSessionId: "root-1",
		parentSessionId: "parent-1",
		parentSessionPath: "/sessions/parent-1.jsonl",
		requestId: "req-1",
		requestDigest: codecDigest(canonicalRequest),
		workflowRunId: "run-1",
		nodeId: "node-1",
		attemptId: "attempt-1",
		workflowChildId: "wfchild-1",
		rlmChildId: "sub-abc123",
		childSessionPath: "/sessions/parent-1-artifacts/sub-abc123/child.jsonl",
		childArtifactDir: "/sessions/parent-1-artifacts/sub-abc123",
		depth: 2,
		name: "child-name",
		turnId: "turn-1",
		promptUtf8Bytes: 5,
		promptDigest: codecDigest(Buffer.from("hello", "utf8")),
		canonicalRequest,
		effectiveModel: "anthropic:claude",
		effectiveThinkingLevel: "medium",
		...overrides,
	};
}

// ---------------------------------------------------------------------------
// Fixtures shared with the reference codec above (a default fence for makeInput).
// ---------------------------------------------------------------------------
const FENCE: Slice3Fence = {
	supervisorGeneration: 1,
	supervisorIncarnationId: "sup-inc-1",
	workerId: "worker-1",
	workerGeneration: 1,
	workerIncarnationId: "wrk-inc-1",
	routeRevision: 1,
};

const CTRL_DIGEST = `sha256:${"a".repeat(64)}`;
const DESCRIPTOR_DIGEST = `sha256:${"d".repeat(64)}`;

let roots: string[] = [];
function makeRoot(): string {
	const r = mkdtempSync(join(tmpdir(), "wf-osf-int-"));
	roots.push(r);
	return r;
}

function endpoint(overrides: Partial<EndpointIdentity> = {}): EndpointIdentity {
	return { endpoint: "/run/prime/daemon.sock", endpointDev: 66310, endpointIno: 4242, platform: "unix", ...overrides };
}

function openControlDb(
	root: string,
	opts: { incarnationId?: string; endpointIdentity?: EndpointIdentity; schemaDigest?: string } = {},
): SupervisorControlDb {
	const options: OpenControlDbOptions = {
		root: join(root, "endpoint-key"),
		schemaDigest: opts.schemaDigest ?? CTRL_DIGEST,
		incarnationId: opts.incarnationId ?? "sup-inc-1",
		endpointIdentity: opts.endpointIdentity ?? endpoint(),
		pid: 4321,
		processStartId: "start-1",
	};
	return SupervisorControlDb.open(options);
}

function makePossession(ep: EndpointIdentity, observedIno?: number | null): EndpointPossession {
	return new EndpointPossession({
		socketPath: ep.endpoint,
		boundIdentity: { dev: ep.endpointDev as number, ino: ep.endpointIno as number },
		platform: "linux",
		lease: undefined,
		readIdentity: () =>
			observedIno === null
				? undefined
				: { dev: ep.endpointDev as number, ino: observedIno ?? (ep.endpointIno as number) },
	});
}

const ROUTE: RouteTuple = {
	rootSessionId: "root-1",
	directParentSessionId: "parent-1",
	workerId: "worker-1",
	workerGeneration: 1,
	routeRevision: 1,
};

/**
 * File-backed dispatch journal (durable bytes). It persists dispatch facts to a JSON file so a
 * simulated process restart (a fresh instance over the same file) reaches the exact same
 * at-most-once decision — the retained-executor's authority is this durable journal, not process
 * memory.
 */
interface DispatchFactsRecord {
	captureArmed: boolean;
	outboxClaimed: boolean;
	dispatching: boolean;
	providerEntered: boolean;
}
class FileDispatchJournal implements Slice3DispatchJournal {
	constructor(
		private readonly path: string,
		private readonly assertFence: () => void = () => {},
	) {}
	private key(b: Slice3DispatchBinding): string {
		return `${b.rlmChildId}\u0000${b.turnId}\u0000${b.admissionReceiptDigest}`;
	}
	private load(): Record<string, DispatchFactsRecord> {
		try {
			return JSON.parse(require("node:fs").readFileSync(this.path, "utf8")) as Record<string, DispatchFactsRecord>;
		} catch {
			return {};
		}
	}
	private store(map: Record<string, DispatchFactsRecord>): void {
		const fs = require("node:fs");
		fs.writeFileSync(this.path, JSON.stringify(map));
		const fd = fs.openSync(this.path, "r");
		try {
			fs.fsyncSync(fd);
		} finally {
			fs.closeSync(fd);
		}
	}
	arm(b: Slice3DispatchBinding): void {
		const map = this.load();
		map[this.key(b)] = { captureArmed: true, outboxClaimed: false, dispatching: false, providerEntered: false };
		this.store(map);
	}
	readDispatchFacts(b: Slice3DispatchBinding): Slice3DispatchFacts {
		const rec = this.load()[this.key(b)] ?? {
			captureArmed: false,
			outboxClaimed: false,
			dispatching: false,
			providerEntered: false,
		};
		return {
			captureArmed: rec.captureArmed,
			outboxUnclaimed: !rec.outboxClaimed,
			hasDispatching: rec.dispatching,
			hasProviderEntered: rec.providerEntered,
		};
	}
	claimAndCommitDispatching(b: Slice3DispatchBinding): Slice3DispatchClaim {
		// Fence at the durable-write boundary; a stale writer must not commit `dispatching`.
		this.assertFence();
		const map = this.load();
		const rec = map[this.key(b)];
		if (!rec || !rec.captureArmed) return { ok: false, code: "STORE_CORRUPT" };
		if (rec.dispatching || rec.providerEntered) return { ok: false, code: "DISPATCHING_ALREADY_COMMITTED" };
		if (rec.outboxClaimed) return { ok: false, code: "OUTBOX_ALREADY_CLAIMED" };
		rec.outboxClaimed = true;
		rec.dispatching = true;
		this.store(map);
		return { ok: true };
	}
	commitProviderEntered(b: Slice3DispatchBinding): void {
		const map = this.load();
		const rec = map[this.key(b)];
		if (rec) {
			rec.providerEntered = true;
			this.store(map);
		}
	}
}

function buildPipeline(
	root: string,
	db: SupervisorControlDb,
	possession: EndpointPossession,
	journal: Slice3DispatchJournal,
	adopted: number,
	extra: { leaseCompromised?: () => boolean } = {},
) {
	const resolution = resolveWorkflowV2OsfencePipeline(
		{
			capabilityAvailable: true, // test-only; production negotiation returns CAPABILITY_UNAVAILABLE.
			platform: "linux",
			controlDb: new SupervisorControlDbOsfAdapter(db),
			sqliteProbeOk: true,
			controlRootIsLocal: true,
		},
		{
			possession,
			adoptedGeneration: adopted,
			supervisorIncarnationId: "sup-inc-1",
			worker: { workerId: "worker-1", workerGeneration: 1, workerIncarnationId: "wrk-inc-1" },
			agentDir: join(root, "agent"),
			sessionsDir: join(root, "sessions"),
			codec: makeReferenceCodec(),
			dispatchJournal: journal,
			...(extra.leaseCompromised ? { leaseCompromised: extra.leaseCompromised } : {}),
		},
	);
	if (!resolution.enabled) throw new Error(`pipeline unexpectedly disabled: ${resolution.reason}`);
	return resolution.pipeline;
}

function cleanup(): void {
	for (const r of roots) rmSync(r, { recursive: true, force: true });
	roots = [];
}

// TerminalCaptureSlot + settlement fixtures (mirrors the settlement suite) for the capture/settle lane.
const SD = (c: string): `sha256:${string}` => `sha256:${c.repeat(64)}`;
function capBinding(overrides: Partial<TurnBinding> = {}): TurnBinding {
	return {
		authorityId: "authority-1",
		rootSessionId: "root-1",
		parentSessionId: "parent-1",
		requestId: "req-1",
		requestDigest: SD("a"),
		workflowRunId: "run-1",
		nodeId: "node-1",
		attemptId: "attempt-1",
		workflowChildId: "wfchild-1",
		rlmChildId: "sub-abc123",
		turnId: "turn-1",
		admittedAt: "2026-09-15T00:00:00.000Z",
		effectiveModel: "anthropic:claude",
		profile: "workflow-v2-tools-none-v1",
		effectiveToolsDigest: SD("b"),
		effectiveThinkingLevel: "medium",
		...overrides,
	};
}
function rawUsage(): RawUsage {
	return {
		input: 10,
		output: 20,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 30,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.25 },
	};
}
function cleanCapture() {
	const slot = new TerminalCaptureSlot(capBinding(), "inv-1");
	slot.observeMessageEnd({
		invocationId: "inv-1",
		binding: capBinding(),
		role: "assistant",
		stopReason: "stop",
		content: [{ type: "text", text: "hello" }],
		provider: "anthropic",
		model: "anthropic:claude",
		usage: rawUsage(),
		errorMessage: null,
		observedAt: "2026-09-15T00:00:01.000Z",
	} satisfies AssistantTerminalObservation);
	slot.observeAgentEnd({
		invocationId: "inv-1",
		binding: capBinding(),
		closedAt: "2026-09-15T00:00:02.000Z",
	} satisfies AgentEndObservation);
	return { capture: slot.capture(), closure: slot.closure() };
}
function settleFence(): SettlementFence {
	return {
		supervisorGeneration: 1,
		supervisorIncarnationId: "sup-inc-1",
		workerId: "worker-1",
		workerGeneration: 1,
		workerIncarnationId: "wrk-inc-1",
		routeRevision: 1,
	};
}

const skipNoSqlite = process.platform === "win32" || probeSupervisorControlDbCapability().capability !== "available";

beforeEach(() => {
	roots = [];
	resetSupervisorControlDbDriverCacheForTest();
});
afterEach(cleanup);

describe("base-off guard keeps the whole integration dormant (§7.1)", () => {
	it("resolves disabled when the V2 capability is unavailable (production default)", () => {
		const res = resolveWorkflowV2OsfencePipeline(
			{
				capabilityAvailable: false,
				platform: "linux",
				controlDb: undefined,
				sqliteProbeOk: true,
				controlRootIsLocal: true,
			},
			{
				possession: makePossession(endpoint()),
				adoptedGeneration: 1,
				supervisorIncarnationId: "sup-inc-1",
				worker: { workerId: "worker-1", workerGeneration: 1, workerIncarnationId: "wrk-inc-1" },
				agentDir: "/tmp/x/agent",
				sessionsDir: "/tmp/x/sessions",
				codec: makeReferenceCodec(),
				dispatchJournal: new FileDispatchJournal("/tmp/x/j.json"),
			},
		);
		expect(res.enabled).toBe(false);
		if (!res.enabled) expect(res.reason).toBe("workflow_v2_capability_unavailable");
	});

	it("resolves disabled on win32 / failed sqlite probe / non-local root — no fallback", () => {
		const deps = {
			possession: makePossession(endpoint()),
			adoptedGeneration: 1,
			supervisorIncarnationId: "sup-inc-1",
			worker: { workerId: "worker-1", workerGeneration: 1, workerIncarnationId: "wrk-inc-1" },
			agentDir: "/tmp/x/agent",
			sessionsDir: "/tmp/x/sessions",
			codec: makeReferenceCodec(),
			dispatchJournal: new FileDispatchJournal("/tmp/x/j.json"),
		};
		expect(
			resolveWorkflowV2OsfencePipeline(
				{
					capabilityAvailable: true,
					platform: "win32",
					controlDb: undefined,
					sqliteProbeOk: true,
					controlRootIsLocal: true,
				},
				deps,
			).enabled,
		).toBe(false);
		expect(
			resolveWorkflowV2OsfencePipeline(
				{
					capabilityAvailable: true,
					platform: "linux",
					controlDb: undefined,
					sqliteProbeOk: false,
					controlRootIsLocal: true,
				},
				deps,
			).enabled,
		).toBe(false);
		expect(
			resolveWorkflowV2OsfencePipeline(
				{
					capabilityAvailable: true,
					platform: "linux",
					controlDb: undefined,
					sqliteProbeOk: true,
					controlRootIsLocal: false,
				},
				deps,
			).enabled,
		).toBe(false);
	});
});

describe("Slice 3 acceptance through the integrated pipeline (durable bytes)", () => {
	it("O1: a second first_init cannot re-init a live control DB; generation unchanged", () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const dbA = openControlDb(root);
		try {
			const adapterA = new SupervisorControlDbOsfAdapter(dbA);
			const rec = adapterA.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			expect(rec.kind).toBe("first_init");
			expect(rec.generation).toBe(1);
			// A second handle over the same durable file cannot first_init again (prior != 0).
			const dbB = openControlDb(root, { incarnationId: "sup-inc-2" });
			try {
				const adapterB = new SupervisorControlDbOsfAdapter(dbB);
				expect(() =>
					adapterB.acquire({
						endpointIdentity: endpoint(),
						incarnationId: "sup-inc-2",
						schemaDigest: CTRL_DIGEST,
						revocationProof: null,
					}),
				).toThrow();
				expect(dbB.readGenerationUnchecked()).toBe(1); // durable generation unchanged
			} finally {
				dbB.close();
			}
		} finally {
			dbA.close();
		}
	});

	it("O2: a paused-then-resumed predecessor cannot admit or mutate after a takeover (GENERATION_STALE, zero effect)", async () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const dbPre = openControlDb(root);
		const dbSucc = openControlDb(root, { incarnationId: "sup-inc-2" });
		try {
			const pre = new SupervisorControlDbOsfAdapter(dbPre);
			pre.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			}); // gen 1
			// Predecessor builds its writer pipeline while it still holds the endpoint (adopted gen 1).
			const prePossession = makePossession(endpoint());
			const preJournal = new FileDispatchJournal(join(root, "pre-journal.json"));
			const prePipeline = buildPipeline(root, dbPre, prePossession, preJournal, 1);
			// The successor takes over: kernel-confirmed endpoint release -> generation advances to 2.
			const succ = new SupervisorControlDbOsfAdapter(dbSucc);
			const takeover = succ.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-2",
				schemaDigest: CTRL_DIGEST,
				revocationProof: "endpoint_released_kernel_confirmed",
			});
			expect(takeover.kind).toBe("takeover");
			expect(takeover.generation).toBe(2);
			// Layer B: the predecessor's admit now fails the Form-2 fence (observed gen 2 != adopted 1); nothing is appended.
			await expect(prePipeline.admit(makeInput())).rejects.toBeInstanceOf(SupervisorWriterFenceError);
			// Layer A: even if we also relinquish the predecessor's possession, the fence still fails closed.
			prePossession.relinquish();
			await expect(prePipeline.admit(makeInput())).rejects.toBeInstanceOf(SupervisorWriterFenceError);
			// Predecessor's Form-1 control-DB mutation self-fences to zero rows -> GENERATION_STALE, zero durable effect.
			expect(() =>
				dbPre.writeRoute({
					rootSessionId: "root-1",
					directParentSessionId: "parent-1",
					workerId: "worker-1",
					workerGeneration: 1,
					state: "ready",
					descriptorDigest: DESCRIPTOR_DIGEST,
				}),
			).toThrow(SupervisorControlDbGenerationStaleError);
			expect(dbPre.readWorkerRoute("root-1", "parent-1")).toBeNull(); // no route row written
			expect(dbSucc.readGenerationUnchecked()).toBe(2); // durable generation is the successor's
		} finally {
			dbPre.close();
			dbSucc.close();
		}
	});

	it("O3: a corrupt control DB fails closed at open; the pipeline never binds", () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const dbA = openControlDb(root);
		dbA.close();
		// Truncate/corrupt the durable file, then a fresh open must fail closed (no repair guess).
		const fs = require("node:fs");
		fs.writeFileSync(join(root, "endpoint-key", "control.db"), Buffer.from("not a sqlite file"));
		expect(() => openControlDb(root)).toThrow();
	});

	it("W1: a worker restart bumps the durable worker generation by exactly one", () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			expect(adapter.reserveWorkerGeneration(1, "worker-1")).toBe(1);
			expect(adapter.reserveWorkerGeneration(1, "worker-1")).toBe(2); // restart -> prior + 1
			expect(adapter.reserveWorkerGeneration(1, "worker-9")).toBe(1); // independent per worker
		} finally {
			db.close();
		}
	});

	it("W2: a delayed stale-generation admit after adoption is rejected with zero effect", async () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			}); // gen 1
			adapter.writeRoute(1, ROUTE, "ready", DESCRIPTOR_DIGEST);
			// A pipeline that adopted a STALE generation (0) can never pass the Form-2 fence.
			const stalePipeline = buildPipeline(
				root,
				db,
				makePossession(endpoint()),
				new FileDispatchJournal(join(root, "j.json")),
				0,
			);
			await expect(stalePipeline.admit(makeInput())).rejects.toBeInstanceOf(SupervisorWriterFenceError);
			expect(countLedgerAdmits(join(root, "sessions"))).toBe(0);
		} finally {
			db.close();
		}
	});

	it("W3: an unreachable worker route stays recovering and mutating calls resolve OWNER_UNAVAILABLE", () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			adapter.writeRoute(1, ROUTE, "recovering", DESCRIPTOR_DIGEST);
			const pipeline = buildPipeline(
				root,
				db,
				makePossession(endpoint()),
				new FileDispatchJournal(join(root, "j.json")),
				1,
			);
			const resolution = pipeline.resolveRoute(ROUTE);
			expect(resolution.code).toBe("OWNER_UNAVAILABLE");
			expect(() => pipeline.buildFenceForRoute(ROUTE)).toThrow(WorkflowV2OsfencePipelineError);
		} finally {
			db.close();
		}
	});

	it("R1: an admission whose route revision is stale resolves ROUTE_STALE; the exact revision binds", () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			adapter.writeRoute(1, ROUTE, "ready", DESCRIPTOR_DIGEST); // revision 1
			const pipeline = buildPipeline(
				root,
				db,
				makePossession(endpoint()),
				new FileDispatchJournal(join(root, "j.json")),
				1,
			);
			// Exact current revision binds.
			expect(pipeline.buildFenceForRoute({ ...ROUTE, routeRevision: 1 }).routeRevision).toBe(1);
			// A concurrent route change bumps the revision to 2; the stale revision-1 admission is ROUTE_STALE.
			adapter.writeRoute(1, ROUTE, "ready", DESCRIPTOR_DIGEST);
			expect(pipeline.resolveRoute({ ...ROUTE, routeRevision: 1 }).code).toBe("ROUTE_STALE");
			expect(pipeline.buildFenceForRoute({ ...ROUTE, routeRevision: 2 }).routeRevision).toBe(2);
		} finally {
			db.close();
		}
	});

	it("R2: a foreign-worker route tuple resolves OWNER_MISMATCH with no scan", () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			adapter.writeRoute(1, ROUTE, "ready", DESCRIPTOR_DIGEST);
			const pipeline = buildPipeline(
				root,
				db,
				makePossession(endpoint()),
				new FileDispatchJournal(join(root, "j.json")),
				1,
			);
			expect(pipeline.resolveRoute({ ...ROUTE, workerId: "worker-OTHER" }).code).toBe("OWNER_MISMATCH");
			expect(pipeline.resolveRoute({ ...ROUTE, rootSessionId: "root-OTHER" }).code).toBe("OWNER_MISMATCH");
		} finally {
			db.close();
		}
	});

	it("tools-none + fenced admit: the durable admit record carries the tools-none profile", async () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			adapter.writeRoute(1, ROUTE, "ready", DESCRIPTOR_DIGEST);
			const pipeline = buildPipeline(
				root,
				db,
				makePossession(endpoint()),
				new FileDispatchJournal(join(root, "j.json")),
				1,
			);
			expect(pipeline.boundProfile).toEqual(WORKFLOW_V2_OSFENCE_BOUND_PROFILE);
			expect(WORKFLOW_V2_OSFENCE_BOUND_PROFILE.maxTurns).toBe(1);
			const fence = pipeline.buildFenceForRoute(ROUTE);
			const outcome = await pipeline.admit(makeInput({ fence }));
			if (outcome.disposition !== "admitted") throw new Error(`expected admitted, got ${outcome.disposition}`);
			expect(outcome.record.profile).toBe("workflow-v2-tools-none-v1");
			expect(outcome.record.tools).toBe("none");
			expect(outcome.record.maxTurns).toBe(1);
			expect(outcome.record.fence.supervisorGeneration).toBe(1);
			// Idempotent replay of the same admit returns the byte-identical record.
			const replay = await pipeline.admit(makeInput({ fence }));
			if (replay.disposition !== "replayed") throw new Error(`expected replayed, got ${replay.disposition}`);
			expect(replay.record.receiptDigest).toBe(outcome.record.receiptDigest);
		} finally {
			db.close();
		}
	});

	it("process-kill/replay: the single physical provider call happens at most once across a restart", async () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			adapter.writeRoute(1, ROUTE, "ready", DESCRIPTOR_DIGEST);
			const journalPath = join(root, "dispatch.json");
			const binding: Slice3DispatchBinding = {
				rlmChildId: "sub-abc123",
				turnId: "turn-1",
				admissionReceiptDigest: SD("f"),
			};
			let calls = 0;
			const physical = async () => {
				calls += 1;
				return `answer-${calls}`;
			};
			// First run: arm + dispatch once.
			const j1 = new FileDispatchJournal(journalPath);
			j1.arm(binding);
			const p1 = buildPipeline(root, db, makePossession(endpoint()), j1, 1);
			const first = await p1.dispatchOnce(binding, physical);
			expect(first.outcome).toBe("dispatched");
			expect(calls).toBe(1);
			// Simulated process kill + restart: fresh journal over the SAME durable file, fresh executor.
			const j2 = new FileDispatchJournal(journalPath);
			const p2 = buildPipeline(root, db, makePossession(endpoint()), j2, 1);
			const second = await p2.dispatchOnce(binding, physical);
			expect(second.outcome).toBe("already_dispatched");
			expect(calls).toBe(1); // no second physical provider call, ever
		} finally {
			db.close();
		}
	});

	it("dispatch fence loss after a durable dispatching fact yields execution_unknown with no provider call", async () => {
		if (skipNoSqlite) return;
		const root = makeRoot();
		const db = openControlDb(root);
		try {
			const adapter = new SupervisorControlDbOsfAdapter(db);
			adapter.acquire({
				endpointIdentity: endpoint(),
				incarnationId: "sup-inc-1",
				schemaDigest: CTRL_DIGEST,
				revocationProof: null,
			});
			const journalPath = join(root, "dispatch.json");
			const binding: Slice3DispatchBinding = {
				rlmChildId: "sub-abc123",
				turnId: "turn-1",
				admissionReceiptDigest: SD("f"),
			};
			let calls = 0;
			const possession = makePossession(endpoint());
			const j = new FileDispatchJournal(journalPath);
			j.arm(binding);
			const pipeline = buildPipeline(root, db, possession, j, 1);
			possession.relinquish(); // endpoint possession lost before the provider-effect boundary
			const outcome = await pipeline.dispatchOnce(binding, async () => {
				calls += 1;
				return "should-not-run";
			});
			expect(outcome.outcome).toBe("fence_lost_after_dispatch");
			expect(calls).toBe(0); // no physical provider call
			expect(j.readDispatchFacts(binding).hasDispatching).toBe(true); // dispatching is durable -> never relaunch
		} finally {
			db.close();
		}
	});

	it("settlement: a complete terminal reduces to a valid atomic settlement commit", () => {
		const { capture, closure } = cleanCapture();
		const pipeline = pipelineForPureLanes();
		const input: SettlementInput = {
			binding: capBinding(),
			fence: settleFence(),
			capture,
			captureClosure: closure,
			dispatchingSequence: 1,
			providerEntry: "observed",
			cancel: { requested: false, actuated: false, actuatedAt: null, orderedAfterCompletion: false },
			quiescence: "proved",
			topologyEvidenceDigest: SD("c"),
			startedAt: "2026-09-15T00:00:00.500Z",
			settledAt: "2026-09-15T00:00:03.000Z",
			hostCursor: "cur-100",
			nextHostCursor: "cur-101",
			hostEventId: "evt-1",
		};
		const reduction = pipeline.settle(input);
		expect(reduction.kind).toBe("commit");
		if (reduction.kind === "commit") {
			expect(reduction.commit.settlement.outcome).toBe("completed");
			expect(validateSettlementCommit(reduction.commit).ok).toBe(true);
		}
	});

	it("settlement: a possible provider effect without complete terminal evidence reduces to execution_unknown", () => {
		const { capture, closure } = cleanCapture();
		const pipeline = pipelineForPureLanes();
		const input: SettlementInput = {
			binding: capBinding(),
			fence: settleFence(),
			capture,
			captureClosure: closure,
			dispatchingSequence: 1,
			providerEntry: "uncertain",
			cancel: { requested: false, actuated: false, actuatedAt: null, orderedAfterCompletion: false },
			quiescence: "unproved",
			topologyEvidenceDigest: SD("c"),
			startedAt: "2026-09-15T00:00:00.500Z",
			settledAt: "2026-09-15T00:00:03.000Z",
			hostCursor: "cur-100",
			nextHostCursor: "cur-101",
			hostEventId: "evt-1",
		};
		const reduction = pipeline.settle(input);
		expect(reduction.kind).toBe("commit");
		if (reduction.kind === "commit") {
			expect(reduction.commit.settlement.outcome).toBe("execution_unknown");
		}
	});
});

/** A pipeline whose only exercised lane is the pure settlement reducer (no sqlite needed). */
function pipelineForPureLanes(): WorkflowV2OsfencePipeline {
	const root = makeRoot();
	return new WorkflowV2OsfencePipeline({
		possession: makePossession(endpoint()),
		controlDb: pureFakeControlDb(),
		adoptedGeneration: 1,
		supervisorIncarnationId: "sup-inc-1",
		worker: { workerId: "worker-1", workerGeneration: 1, workerIncarnationId: "wrk-inc-1" },
		agentDir: join(root, "agent"),
		sessionsDir: join(root, "sessions"),
		codec: makeReferenceCodec(),
		dispatchJournal: new FileDispatchJournal(join(root, "j.json")),
	});
}

/** Minimal in-memory OsfControlDb for the settlement-only lane (never asserts ownership). */
function pureFakeControlDb(): OsfControlDb {
	return {
		acquire() {
			throw new Error("unused");
		},
		readGenerationUnchecked() {
			return 1;
		},
		reserveWorkerGeneration() {
			return 1;
		},
		writeRoute() {},
		readRoute() {
			return undefined;
		},
		release() {},
	};
}

function countLedgerAdmits(sessionsDir: string): number {
	const fs = require("node:fs");
	const path = require("node:path");
	// The composite ledger path is <agentDir? no> under sessionsDir; find any rlm-ledger file with admit lines.
	function walk(dir: string): string[] {
		let out: string[] = [];
		let entries: string[] = [];
		try {
			entries = fs.readdirSync(dir);
		} catch {
			return out;
		}
		for (const e of entries) {
			const p = path.join(dir, e);
			let st: { isDirectory(): boolean } | undefined;
			try {
				st = fs.statSync(p);
			} catch {
				continue;
			}
			if (st?.isDirectory()) out = out.concat(walk(p));
			else out.push(p);
		}
		return out;
	}
	let count = 0;
	for (const f of walk(sessionsDir)) {
		let raw = "";
		try {
			raw = fs.readFileSync(f, "utf8");
		} catch {
			continue;
		}
		for (const line of raw.split("\n")) {
			if (line.includes('"op":"admit"')) count += 1;
		}
	}
	return count;
}
