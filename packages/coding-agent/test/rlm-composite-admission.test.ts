import { createHash } from "node:crypto";
import { appendFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { describe, expect, it } from "vitest";
import {
	type AdmissionReceipt,
	type AdmissionReceiptPayload,
	type AdmitInput,
	RlmCompositeAdmissionLedger,
	type RlmLedgerAdmitRecord,
	RlmSpawnLedger,
	rlmLedgerPath,
	Slice3CodecError,
	type Slice3Fence,
	type Slice3LedgerCodec,
} from "../src/modes/daemon/rlm-ledger.js";

// ---------------------------------------------------------------------------
// Reference Slice3 codec (stands in for core/workflow-v2-slice3-codec.ts).
// RFC 8785 JCS over the record's value space, SHA-256, canonical padded
// base64, and a focused semantic validator that recomputes digests and proves
// the receipt envelope/payload bind their enclosing record.
// ---------------------------------------------------------------------------
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

/** Rebuild a valid record from a mutated payload, recomputing every dependent digest. */
function rebuildWithPayload(record: RlmLedgerAdmitRecord, payload: AdmissionReceiptPayload): RlmLedgerAdmitRecord {
	const receiptDigest = codecDigest(codecCanonicalize(payload));
	const decodedReceipt: AdmissionReceipt = {
		protocol: "prime.workflow.retained-admission-receipt/v2-slice3",
		payload,
		digest: receiptDigest,
	};
	return {
		...record,
		admissionSequence: payload.admissionSequence,
		receiptDigest,
		receipt: codecCanonicalize(decodedReceipt).toString("base64"),
		decodedReceipt,
	};
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------
const FENCE: Slice3Fence = {
	supervisorGeneration: 3,
	supervisorIncarnationId: "sup-inc-1",
	workerId: "worker-1",
	workerGeneration: 5,
	workerIncarnationId: "wrk-inc-1",
	routeRevision: 2,
};

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

function tempDirs(): { agentDir: string; sessionsDir: string; cleanup: () => void } {
	const root = mkdtempSync(join(tmpdir(), "rlm-v2-admit-"));
	return {
		agentDir: join(root, "agent"),
		sessionsDir: join(root, "sessions"),
		cleanup: () => rmSync(root, { recursive: true, force: true }),
	};
}

function writer(
	agentDir: string,
	sessionsDir: string,
	opts: { assertWriterFence?: () => void; codec?: Slice3LedgerCodec } = {},
): RlmCompositeAdmissionLedger {
	return new RlmCompositeAdmissionLedger(agentDir, sessionsDir, {
		mode: "writer",
		codec: opts.codec ?? makeReferenceCodec(),
		assertWriterFence: opts.assertWriterFence,
	});
}

function reader(agentDir: string, sessionsDir: string): RlmCompositeAdmissionLedger {
	return new RlmCompositeAdmissionLedger(agentDir, sessionsDir, { mode: "reader", codec: makeReferenceCodec() });
}

function seedFile(path: string, contents: string): void {
	rmSync(dirname(path), { recursive: true, force: true });
	mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
	writeFileSync(path, contents);
}

function countAdmitLines(path: string): number {
	let raw: string;
	try {
		raw = readFileSync(path, "utf8");
	} catch {
		return 0;
	}
	return raw.split("\n").filter((l) => {
		if (!l.trim()) return false;
		try {
			const o = JSON.parse(l) as { v?: unknown; op?: unknown };
			return o.v === 2 && o.op === "admit";
		} catch {
			return false;
		}
	}).length;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
describe("RlmCompositeAdmissionLedger — durable composite admission", () => {
	it("admits one composite record, fsynced and verified before returning", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			const outcome = await ledger.admit(makeInput());
			expect(outcome.disposition).toBe("admitted");
			if (outcome.disposition !== "admitted") throw new Error("unreachable");
			expect(outcome.record.rlmChildId).toBe("sub-abc123");
			expect(outcome.record.turnId).toBe("turn-1");
			expect(outcome.record.admissionSequence).toBe(0);
			expect(outcome.record.receiptDigest).toMatch(/^sha256:[0-9a-f]{64}$/);
			expect(countAdmitLines(ledger.ledgerPath)).toBe(1);
		} finally {
			cleanup();
		}
	});

	it("A1: 100 concurrent identical admissions produce exactly one record with byte-identical replies", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			const outcomes = await Promise.all(Array.from({ length: 100 }, () => ledger.admit(makeInput())));
			expect(outcomes.filter((o) => o.disposition === "admitted")).toHaveLength(1);
			expect(outcomes.filter((o) => o.disposition === "replayed")).toHaveLength(99);
			expect(countAdmitLines(ledger.ledgerPath)).toBe(1);
			const canon = (o: (typeof outcomes)[number]) =>
				o.disposition === "admitted" || o.disposition === "replayed" ? jcs(o.record) : "X";
			const first = canon(outcomes[0]);
			for (const o of outcomes) expect(canon(o)).toBe(first);
		} finally {
			cleanup();
		}
	});

	it("A2: same requestId with different canonical bytes returns REQUEST_ID_CONFLICT and writes no second record", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			await ledger.admit(makeInput());
			const changed = Buffer.from('{"prompt":"HELLO"}', "utf8");
			const conflict = await ledger.admit(
				makeInput({ canonicalRequest: changed, requestDigest: codecDigest(changed) }),
			);
			expect(conflict.disposition).toBe("conflict");
			if (conflict.disposition !== "conflict") throw new Error("unreachable");
			expect(conflict.code).toBe("REQUEST_ID_CONFLICT");
			expect(countAdmitLines(ledger.ledgerPath)).toBe(1);
		} finally {
			cleanup();
		}
	});

	it("replay idempotency: a fresh ledger instance (restart) replays the byte-identical stored receipt", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const first = await writer(agentDir, sessionsDir).admit(makeInput());
			const second = await writer(agentDir, sessionsDir).admit(makeInput());
			expect(first.disposition).toBe("admitted");
			expect(second.disposition).toBe("replayed");
			if (first.disposition !== "admitted" && first.disposition !== "replayed") throw new Error("x");
			if (second.disposition !== "admitted" && second.disposition !== "replayed") throw new Error("x");
			expect(jcs(second.record)).toBe(jcs(first.record));
			expect(countAdmitLines(rlmLedgerPath(agentDir, sessionsDir))).toBe(1);
		} finally {
			cleanup();
		}
	});

	it("A3 absent: no record → admit proceeds normally", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			expect((await writer(agentDir, sessionsDir).admit(makeInput())).disposition).toBe("admitted");
		} finally {
			cleanup();
		}
	});

	it("A3 torn tail: an uncommitted partial line is skipped/truncated; admit commits cleanly (absent → retry)", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const path = rlmLedgerPath(agentDir, sessionsDir);
			seedFile(path, '{"v":1,"op":"meta","at":"2026-01-01T00:00:00.000Z","sessionsDir":"/x"}\n');
			appendFileSync(
				path,
				'{"v":2,"op":"admit","requestId":"req-1","authorityId":"authority-1","parentSessionId":"parent-1","receiptDigest":"sha256:',
			);
			const ledger = writer(agentDir, sessionsDir);
			const outcome = await ledger.admit(makeInput());
			expect(outcome.disposition).toBe("admitted");
			expect(countAdmitLines(path)).toBe(1);
		} finally {
			cleanup();
		}
	});

	it("verify gate: if the stored record cannot be re-read validly, admit returns ADMISSION_UNKNOWN (no false success)", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			let calls = 0;
			const codec = makeReferenceCodec(() => {
				calls += 1;
				if (calls >= 2) throw new Slice3CodecError("simulated stored corruption on verify");
			});
			const outcome = await writer(agentDir, sessionsDir, { codec }).admit(makeInput());
			expect(outcome.disposition).toBe("rejected");
			if (outcome.disposition !== "rejected") throw new Error("unreachable");
			expect(outcome.code).toBe("ADMISSION_UNKNOWN");
		} finally {
			cleanup();
		}
	});

	it("interior contradictory duplicate (valid but different digest) fences with STORE_CORRUPT", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			const first = await ledger.admit(makeInput());
			if (first.disposition !== "admitted") throw new Error("setup");
			// Forge a SECOND valid record for the same scope with a different admissionSequence
			// (hence a different, correctly-recomputed receiptDigest): a genuine contradiction.
			const contradictory = rebuildWithPayload(first.record, {
				...first.record.decodedReceipt.payload,
				admissionSequence: 1,
			});
			appendFileSync(ledger.ledgerPath, `${JSON.stringify(contradictory)}\n`);
			const retry = await writer(agentDir, sessionsDir).admit(makeInput());
			expect(retry.disposition).toBe("rejected");
			if (retry.disposition !== "rejected") throw new Error("unreachable");
			expect(retry.code).toBe("STORE_CORRUPT");
		} finally {
			cleanup();
		}
	});

	it("A6: a record the codec rejects is never written (ADMISSION_UNKNOWN, no durable row)", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const codec = makeReferenceCodec(() => {
				throw new Slice3CodecError("simulated invalid record");
			});
			const ledger = writer(agentDir, sessionsDir, { codec });
			expect((await ledger.admit(makeInput())).disposition).toBe("rejected");
			expect(countAdmitLines(ledger.ledgerPath)).toBe(0);
		} finally {
			cleanup();
		}
	});

	it("sole writer: a read-only reader ledger cannot admit", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			await expect(reader(agentDir, sessionsDir).admit(makeInput())).rejects.toThrow(/read-only ledger/);
			expect(countAdmitLines(rlmLedgerPath(agentDir, sessionsDir))).toBe(0);
		} finally {
			cleanup();
		}
	});

	it("writer fence: a lost fence rejects admission before any durable write", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir, {
				assertWriterFence: () => {
					throw new Error("supervisor generation stale");
				},
			});
			await expect(ledger.admit(makeInput())).rejects.toThrow(/generation stale/);
			expect(countAdmitLines(ledger.ledgerPath)).toBe(0);
		} finally {
			cleanup();
		}
	});

	it("reader lookup resolves an admitted scope after a writer commits it", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			await writer(agentDir, sessionsDir).admit(makeInput());
			const r = reader(agentDir, sessionsDir);
			expect((await r.lookup("authority-1", "parent-1", "req-1"))?.rlmChildId).toBe("sub-abc123");
			expect(await r.lookup("authority-1", "parent-1", "req-missing")).toBeUndefined();
		} finally {
			cleanup();
		}
	});

	it("admissionSequence is monotonic across distinct admissions", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			const a = await ledger.admit(makeInput({ requestId: "req-a" }));
			const b = await ledger.admit(makeInput({ requestId: "req-b", rlmChildId: "sub-b", turnId: "turn-b" }));
			if (a.disposition !== "admitted" || b.disposition !== "admitted") throw new Error("setup");
			expect(a.record.admissionSequence).toBe(0);
			expect(b.record.admissionSequence).toBe(1);
		} finally {
			cleanup();
		}
	});

	it("buildMaterializeCommand produces a generation-bound command matching the admitted record", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			const outcome = await ledger.admit(makeInput());
			if (outcome.disposition !== "admitted") throw new Error("setup");
			const cmd = ledger.buildMaterializeCommand(outcome.record, "cmd-1");
			expect(cmd.protocol).toBe("prime.workflow.retained-materialize/v2-slice3");
			expect(cmd.commandId).toBe("cmd-1");
			expect(cmd.fence).toEqual(FENCE);
			expect(cmd.admissionReceiptDigest).toBe(outcome.record.receiptDigest);
			expect(cmd.rlmChildId).toBe(outcome.record.rlmChildId);
			expect(cmd.turnId).toBe(outcome.record.turnId);
			expect(cmd.requestDigest).toBe(outcome.record.requestDigest);
			expect(cmd.profile).toBe("workflow-v2-tools-none-v1");
			expect(cmd.tools).toBe("none");
			expect(cmd.maxTurns).toBe(1);
			expect(cmd.admissionSequence).toBe(outcome.record.admissionSequence);
		} finally {
			cleanup();
		}
	});
});

describe("legacy V1 read compatibility (C2)", () => {
	it("a mixed v1+v2 ledger: V1 reader sees only v1 topology; V2 reader sees only the admission", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const ledger = writer(agentDir, sessionsDir);
			await ledger.admit(makeInput());
			appendFileSync(
				ledger.ledgerPath,
				`${JSON.stringify({
					v: 1,
					op: "spawn",
					at: "2026-01-01T00:00:00.000Z",
					childId: "child-legacy",
					parent: "/sessions/parent-1.jsonl",
					child: "/sessions/child-legacy.jsonl",
					depth: 1,
					name: "legacy",
				})}\n`,
			);
			const edges = await new RlmSpawnLedger(agentDir, sessionsDir).edges();
			expect(edges).toHaveLength(1);
			expect(edges[0]?.childId).toBe("child-legacy");
			const admissions = await ledger.admissions();
			expect(admissions).toHaveLength(1);
			expect(admissions[0]?.rlmChildId).toBe("sub-abc123");
		} finally {
			cleanup();
		}
	});

	it("a v1-only ledger is unaffected by the v2 reader path", async () => {
		const { agentDir, sessionsDir, cleanup } = tempDirs();
		try {
			const path = rlmLedgerPath(agentDir, sessionsDir);
			seedFile(
				path,
				`${JSON.stringify({
					v: 1,
					op: "spawn",
					at: "2026-01-01T00:00:00.000Z",
					childId: "c1",
					parent: "/sessions/p.jsonl",
					child: "/sessions/c.jsonl",
					depth: 1,
					name: "n",
				})}\n`,
			);
			const v1 = new RlmSpawnLedger(agentDir, sessionsDir);
			const edges = await v1.edges();
			expect(edges).toHaveLength(1);
			expect(await reader(agentDir, sessionsDir).admissions()).toHaveLength(0);
			expect(await v1.edges()).toEqual(edges);
		} finally {
			cleanup();
		}
	});
});
