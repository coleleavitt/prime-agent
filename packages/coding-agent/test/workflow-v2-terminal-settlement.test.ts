import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import {
	type AtomicSettlementCommit,
	type Fence,
	reduceSettlement,
	type SettlementInput,
	validateSettlementCommit,
} from "../src/core/workflow-v2-settlement.js";
import {
	type AgentEndObservation,
	type AssistantTerminalObservation,
	type CaptureClosure,
	canonicalize,
	classifyResult,
	decimalToMicrousd,
	digestValue,
	extractTerminalText,
	MAX_INLINE_RESULT_BYTES,
	normalizeUsage,
	type RawUsage,
	type TerminalCapture,
	TerminalCaptureSlot,
	type TurnBinding,
} from "../src/core/workflow-v2-terminal-capture.js";

const D = (c: string): `sha256:${string}` => `sha256:${c.repeat(64)}`;

function binding(overrides: Partial<TurnBinding> = {}): TurnBinding {
	return {
		authorityId: "auth1",
		rootSessionId: "root1",
		parentSessionId: "parent1",
		requestId: "req1",
		requestDigest: D("a"),
		workflowRunId: "run1",
		nodeId: "node1",
		attemptId: "att1",
		workflowChildId: "wchild1",
		rlmChildId: "child1",
		turnId: "turn1",
		admittedAt: "2026-01-01T00:00:00.000Z",
		effectiveModel: "prov.model",
		profile: "workflow-v2-tools-none-v1",
		effectiveToolsDigest: D("b"),
		effectiveThinkingLevel: "off",
		...overrides,
	};
}

function rawUsage(overrides: Partial<RawUsage> = {}): RawUsage {
	return {
		input: 10,
		output: 20,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 30,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.25 },
		...overrides,
	};
}

function obs(overrides: Partial<AssistantTerminalObservation> = {}): AssistantTerminalObservation {
	return {
		invocationId: "inv1",
		binding: binding(),
		role: "assistant",
		stopReason: "stop",
		content: [{ type: "text", text: "hello" }],
		provider: "prov",
		model: "prov.model",
		usage: rawUsage(),
		errorMessage: null,
		observedAt: "2026-01-01T00:00:01.000Z",
		...overrides,
	};
}

function agentEnd(overrides: Partial<AgentEndObservation> = {}): AgentEndObservation {
	return { invocationId: "inv1", binding: binding(), closedAt: "2026-01-01T00:00:02.000Z", ...overrides };
}

function cleanSlot(o: Partial<AssistantTerminalObservation> = {}): {
	capture: TerminalCapture;
	closure: CaptureClosure;
} {
	const slot = new TerminalCaptureSlot(binding(), "inv1");
	slot.observeMessageEnd(obs(o));
	slot.observeAgentEnd(agentEnd());
	return { capture: slot.capture(), closure: slot.closure() };
}

function fence(): Fence {
	return {
		supervisorGeneration: 1,
		supervisorIncarnationId: "sup1",
		workerId: "w1",
		workerGeneration: 1,
		workerIncarnationId: "wi1",
		routeRevision: 1,
	};
}

function settlementInput(over: Partial<SettlementInput> = {}): SettlementInput {
	const { capture, closure } = cleanSlot();
	return {
		binding: binding(),
		fence: fence(),
		capture,
		captureClosure: closure,
		dispatchingSequence: 1,
		providerEntry: "observed",
		cancel: { requested: false, actuated: false, actuatedAt: null, orderedAfterCompletion: false },
		quiescence: "proved",
		topologyEvidenceDigest: D("c"),
		startedAt: "2026-01-01T00:00:00.500Z",
		settledAt: "2026-01-01T00:00:03.000Z",
		hostCursor: "cur-100",
		nextHostCursor: "cur-101",
		hostEventId: "evt-1",
		...over,
	};
}

function commitOf(over: Partial<SettlementInput> = {}): AtomicSettlementCommit {
	const r = reduceSettlement(settlementInput(over));
	if (r.kind !== "commit") throw new Error(`expected commit, got ${r.reason}`);
	return r.commit;
}

// -------------------------------------------------------------------------
// T1: exact text bytes, thinking exclusion, oversize boundary
// -------------------------------------------------------------------------
describe("terminal capture — text and bytes (T1)", () => {
	it("concatenates text blocks without separators and excludes thinking/tool blocks", () => {
		const text = extractTerminalText([
			{ type: "text", text: "foo" },
			{ type: "thinking", thinking: "SECRET" },
			{ type: "text", text: "bar" },
			{ type: "toolCall", id: "t", name: "x" },
		]);
		expect(text).toBe("foobar");
	});

	it("counts exact Node UTF-8 bytes for astral, combining, NUL, and emoji", () => {
		const s = "a\u0000é😀e\u0301";
		const r = classifyResult("stop", [{ type: "text", text: s }]);
		expect(r.kind).toBe("text");
		if (r.kind === "text") {
			expect(r.utf8Bytes).toBe(Buffer.byteLength(s, "utf8"));
			expect(r.sha256).toBe(`sha256:${createHash("sha256").update(Buffer.from(s, "utf8")).digest("hex")}`);
		}
	});

	it("keeps a 262144-byte result inline and marks 262145 too_large with complete digest", () => {
		const atCap = "x".repeat(MAX_INLINE_RESULT_BYTES);
		const over = "x".repeat(MAX_INLINE_RESULT_BYTES + 1);
		const rIn = classifyResult("stop", [{ type: "text", text: atCap }]);
		const rOut = classifyResult("stop", [{ type: "text", text: over }]);
		expect(rIn.kind).toBe("text");
		expect(rOut.kind).toBe("too_large");
		if (rOut.kind === "too_large") {
			expect(rOut.utf8Bytes).toBe(MAX_INLINE_RESULT_BYTES + 1);
			expect(rOut.sha256).toBe(`sha256:${createHash("sha256").update(Buffer.from(over, "utf8")).digest("hex")}`);
		}
	});

	it("encodes a lone surrogate as Node replacement bytes", () => {
		const lone = "\ud800";
		const r = classifyResult("stop", [{ type: "text", text: lone }]);
		expect(r.kind).toBe("text");
		if (r.kind === "text") expect(r.utf8Bytes).toBe(Buffer.byteLength(lone, "utf8"));
	});
});

// -------------------------------------------------------------------------
// T2: classification of empty/tool/length/error/abort/duplicate/missing/late/wrong
// -------------------------------------------------------------------------
describe("terminal capture — ambiguity and failure classification (T2)", () => {
	it("empty stop terminal is no_assistant", () => {
		const r = classifyResult("stop", [{ type: "text", text: "" }]);
		expect(r).toEqual({ kind: "none", reason: "no_assistant" });
	});
	it("tool_use terminal yields no_assistant", () => {
		const r = classifyResult("stop", [{ type: "toolCall", id: "t", name: "x" }]);
		expect(r).toEqual({ kind: "none", reason: "no_assistant" });
	});
	it("length terminal yields no_assistant", () => {
		expect(classifyResult("length", [{ type: "text", text: "hi" }])).toEqual({
			kind: "none",
			reason: "no_assistant",
		});
	});
	it("error terminal yields provider_error", () => {
		expect(classifyResult("error", [])).toEqual({ kind: "none", reason: "provider_error" });
	});
	it("missing terminal is ambiguous missing_terminal", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeAgentEnd(agentEnd());
		const c = slot.capture();
		expect(c.kind).toBe("ambiguous");
		if (c.kind === "ambiguous") expect(c.reason).toBe("missing_terminal");
	});
	it("two owned terminals is ambiguous multiple_terminals", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs());
		slot.observeMessageEnd(obs({ content: [{ type: "text", text: "second" }] }));
		slot.observeAgentEnd(agentEnd());
		const c = slot.capture();
		expect(c.kind === "ambiguous" && c.reason).toBe("multiple_terminals");
	});
	it("wrong invocation event does not select and is ambiguous", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs({ invocationId: "other" }));
		slot.observeAgentEnd(agentEnd());
		const c = slot.capture();
		expect(c.kind === "ambiguous" && c.reason).toBe("wrong_invocation");
	});
	it("wrong binding event is ambiguous wrong_binding", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs({ binding: binding({ turnId: "turnX" }) }));
		slot.observeAgentEnd(agentEnd());
		const c = slot.capture();
		expect(c.kind === "ambiguous" && c.reason).toBe("wrong_binding");
	});
	it("post-closure event is a late_event ambiguity", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs());
		slot.observeAgentEnd(agentEnd());
		slot.observeMessageEnd(obs());
		const c = slot.capture();
		expect(c.kind === "ambiguous" && c.reason).toBe("late_event");
	});
	it("missing agent_end is an ambiguous closure", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs());
		const cl = slot.closure();
		expect(cl.kind === "ambiguous" && cl.reason).toBe("agent_end_missing");
	});
	it("duplicate agent_end is an ambiguous closure", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs());
		slot.observeAgentEnd(agentEnd());
		slot.observeAgentEnd(agentEnd());
		const cl = slot.closure();
		expect(cl.kind === "ambiguous" && cl.reason).toBe("agent_end_duplicate");
	});
	it("observed clean capture and closure have matching bindings and count", () => {
		const { capture, closure } = cleanSlot();
		expect(capture.kind).toBe("observed");
		expect(closure.kind).toBe("observed");
		if (capture.kind === "observed" && closure.kind === "observed") {
			expect(capture.observationCount).toBe(1);
			expect(closure.observationCount).toBe(1);
			expect(capture.invocationId).toBe(closure.invocationId);
		}
	});
});

// -------------------------------------------------------------------------
// U1: usage normalization and cost precision
// -------------------------------------------------------------------------
describe("usage normalization (U1)", () => {
	it("maps token fields and preserves totalTokens", () => {
		const u = normalizeUsage(rawUsage({ totalTokens: 999 }), "final");
		expect(u).toEqual({
			inputTokens: 10,
			outputTokens: 20,
			cacheReadTokens: 0,
			cacheWriteTokens: 0,
			totalTokens: 999,
			costMicrousd: 250000,
			finality: "final",
		});
	});
	it("rejects boolean, negative, fractional, unsafe, NaN, and missing tokens", () => {
		expect(normalizeUsage(rawUsage({ input: true as unknown as number }), "final")).toBeNull();
		expect(normalizeUsage(rawUsage({ output: -1 }), "final")).toBeNull();
		expect(normalizeUsage(rawUsage({ cacheRead: 1.5 }), "final")).toBeNull();
		expect(normalizeUsage(rawUsage({ cacheWrite: Number.MAX_SAFE_INTEGER + 1 }), "final")).toBeNull();
		expect(normalizeUsage(rawUsage({ totalTokens: Number.NaN }), "final")).toBeNull();
		expect(normalizeUsage(rawUsage({ input: undefined as unknown as number }), "final")).toBeNull();
	});
	it("emits micro-USD only when integral and safe, else null", () => {
		expect(decimalToMicrousd(0.25)).toBe(250000);
		expect(decimalToMicrousd(0.123456)).toBe(123456);
		expect(decimalToMicrousd(0.1234565)).toBeNull();
		expect(decimalToMicrousd(1e-7)).toBeNull();
		expect(decimalToMicrousd(0)).toBe(0);
		expect(decimalToMicrousd(-0.5)).toBeNull();
		expect(decimalToMicrousd(Number.POSITIVE_INFINITY)).toBeNull();
		expect(decimalToMicrousd(2)).toBe(2000000);
	});
	it("null cost yields null micro-USD but valid usage", () => {
		const u = normalizeUsage(rawUsage({ cost: null }), "final");
		expect(u?.costMicrousd).toBeNull();
	});
	it("invalid usage forces an ambiguous usage_invalid capture", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs({ usage: rawUsage({ input: -5 }) }));
		slot.observeAgentEnd(agentEnd());
		const c = slot.capture();
		expect(c.kind === "ambiguous" && c.reason).toBe("usage_invalid");
	});
});

// -------------------------------------------------------------------------
// Settlement outcome table
// -------------------------------------------------------------------------
describe("settlement reduction — outcome table", () => {
	it("completed for clean stop text, quiescent, closed, final usage", () => {
		const c = commitOf();
		expect(c.settlement.outcome).toBe("completed");
		expect((c.settlement.usage as { finality: string }).finality).toBe("final");
		expect(c.settlement.cancelActuated).toBe(false);
		expect(c.settlement.descendantsQuiescent).toBe(true);
	});
	it("failed/provider_error for an error terminal", () => {
		const c = commitOf({ ...errorTurn() });
		expect(c.settlement.outcome).toBe("failed");
		expect((c.settlement.result as { reason: string }).reason).toBe("provider_error");
	});
	it("failed/too_large for oversize result", () => {
		const big = "x".repeat(MAX_INLINE_RESULT_BYTES + 10);
		const { capture, closure } = cleanSlot({ content: [{ type: "text", text: big }] });
		const c = commitOf({ capture, captureClosure: closure });
		expect(c.settlement.outcome).toBe("failed");
		expect((c.settlement.result as { kind: string }).kind).toBe("too_large");
		expect((c.settlement.error as { code: string }).code).toBe("RESULT_INVALID");
	});
	it("cancelled requires aborted terminal plus actuation", () => {
		const { capture, closure } = cleanSlot({ stopReason: "aborted", content: [] });
		const c = commitOf({
			capture,
			captureClosure: closure,
			cancel: {
				requested: true,
				actuated: true,
				actuatedAt: "2026-01-01T00:00:02.500Z",
				orderedAfterCompletion: false,
			},
		});
		expect(c.settlement.outcome).toBe("cancelled");
		expect(c.settlement.cancelActuated).toBe(true);
		expect((c.settlement.error as { code: string }).code).toBe("CANCELLED");
	});
	it("aborted terminal without actuation is execution_unknown", () => {
		const { capture, closure } = cleanSlot({ stopReason: "aborted", content: [] });
		const c = commitOf({ capture, captureClosure: closure });
		expect(c.settlement.outcome).toBe("execution_unknown");
	});
	it("unproven quiescence forces execution_unknown with known_prefix", () => {
		const c = commitOf({ quiescence: "unproved" });
		expect(c.settlement.outcome).toBe("execution_unknown");
		expect((c.settlement.usage as { finality: string }).finality).toBe("known_prefix");
		expect(c.settlement.descendantsQuiescent).toBe(false);
	});
	it("ambiguous capture forces execution_unknown", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeAgentEnd(agentEnd());
		const capture = slot.capture();
		const closure = slot.closure();
		const c = commitOf({ capture, captureClosure: closure });
		expect(c.settlement.outcome).toBe("execution_unknown");
	});
	it("possible dispatch with incomplete capture never completes (execution_unknown)", () => {
		const slot = new TerminalCaptureSlot(binding(), "inv1");
		slot.observeMessageEnd(obs());
		// no agent_end -> ambiguous closure
		const c = commitOf({ capture: slot.capture(), captureClosure: slot.closure(), providerEntry: "uncertain" });
		expect(c.settlement.outcome).toBe("execution_unknown");
	});
});

function errorTurn(): Partial<SettlementInput> {
	const { capture, closure } = cleanSlot({ stopReason: "error", content: [], errorMessage: "boom" });
	return { capture, captureClosure: closure };
}

// -------------------------------------------------------------------------
// T3 / S3 / S5a: digest self-consistency, idempotence, mutation rejection
// -------------------------------------------------------------------------
describe("settlement commit integrity (T3/S3/S5a/S5)", () => {
	it("validates a well-formed commit", () => {
		expect(validateSettlementCommit(commitOf())).toEqual({ ok: true });
	});
	it("is byte-identical for identical input (idempotent replay)", () => {
		expect(canonicalize(commitOf())).toBe(canonicalize(commitOf()));
	});
	it("differs when any owned observation byte differs", () => {
		const a = canonicalize(commitOf());
		const { capture, closure } = cleanSlot({ content: [{ type: "text", text: "different" }] });
		const b = canonicalize(commitOf({ capture, captureClosure: closure }));
		expect(a).not.toBe(b);
	});
	it("rejects a mutated settlement outcome", () => {
		const c = commitOf();
		(c.settlement as Record<string, unknown>).outcome = "failed";
		expect(validateSettlementCommit(c).ok).toBe(false);
	});
	it("rejects a mutated event cursor", () => {
		const c = commitOf();
		(c.event as { hostCursor: string }).hostCursor = "cur-999";
		expect(validateSettlementCommit(c).ok).toBe(false);
	});
	it("rejects a mutated result text after capture (frozen owned message_end)", () => {
		const c = commitOf();
		(c.capture as { result?: unknown }).result = { kind: "text", text: "tampered", utf8Bytes: 8, sha256: D("f") };
		expect(validateSettlementCommit(c).ok).toBe(false);
	});
	it("rejects a mutated settlement usage token", () => {
		const c = commitOf();
		(c.settlement.usage as { totalTokens: number }).totalTokens = 12345;
		expect(validateSettlementCommit(c).ok).toBe(false);
	});
	it("rejects next cursor equal to host cursor", () => {
		const r = reduceSettlement(settlementInput({ nextHostCursor: "cur-100" }));
		expect(r.kind === "rejected" && r.reason).toBe("cursor_not_successor");
	});
	it("rejects correlation mismatch between capture and binding", () => {
		const { capture, closure } = cleanSlot();
		const r = reduceSettlement(
			settlementInput({ capture, captureClosure: closure, binding: binding({ turnId: "turnZ" }) }),
		);
		expect(r.kind === "rejected" && r.reason).toBe("correlation_mismatch");
	});
	it("event carries the exact settlement object and matching digests", () => {
		const c = commitOf();
		expect(c.event.data.settlement).toBe(c.settlement);
		expect(c.receipt.settlementDigest).toBe(c.settlement.settlementDigest);
		expect(c.event.hostCursor).toBe(c.settlement.hostCursor);
		expect(c.receipt.hostCursor).toBe(c.settlement.hostCursor);
	});
	it("recomputes commit and event digests from canonical bytes", () => {
		const c = commitOf();
		const { commitDigest, ...restCommit } = c as unknown as Record<string, unknown>;
		expect(commitDigest).toBe(digestValue(restCommit));
		const { digest, ...restEvent } = c.event as unknown as Record<string, unknown>;
		expect(digest).toBe(digestValue(restEvent));
	});
});
