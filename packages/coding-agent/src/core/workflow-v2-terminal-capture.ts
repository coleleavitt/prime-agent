import { createHash } from "node:crypto";

/**
 * Workflow V2 Slice 3 — exact live terminal capture.
 *
 * Ownership (design authority: docs/WORKFLOW-V2-SLICE3.md §7, file-map §9):
 * binding, the owned-invocation capture slot, exact Node UTF-8 hashing, usage
 * normalization, and closed ambiguity facts. This module is a pure producer of
 * the canonical `terminalCapture` and `captureClosure` values defined in
 * `workflow-v2-slice3.schema.json`. It performs no I/O, reads no transcript,
 * roster, preview, session message, or `getLastAssistantText`, and never
 * selects a terminal by transcript position. The executor
 * (`workflow-v2-retained-executor.ts`) drives fsync/journaling; the codec
 * (`workflow-v2-slice3-codec.ts`) is the integration canonicalizer authority.
 * The RFC 8785 serializer and digest helpers here are byte-compatible with that
 * codec for the closed Slice 3 JSON subset (strings, safe integers, booleans,
 * null, arrays, objects) so the reducer and codec agree on every digest.
 */

export const RETAINED_JOURNAL_PROTOCOL = "prime.workflow.retained-journal/v2-slice3" as const;
export const TOOLS_NONE_PROFILE = "workflow-v2-tools-none-v1" as const;
export const MAX_INLINE_RESULT_BYTES = 262_144;
const MAX_SAFE = Number.MAX_SAFE_INTEGER;

export type Digest = `sha256:${string}`;
export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";
export type CaptureStopReason = "stop" | "length" | "error" | "aborted" | "tool_use" | "other";

export interface TurnBinding {
	authorityId: string;
	rootSessionId: string;
	parentSessionId: string;
	requestId: string;
	requestDigest: Digest;
	workflowRunId: string;
	nodeId: string;
	attemptId: string;
	workflowChildId: string;
	rlmChildId: string;
	turnId: string;
	admittedAt: string;
	effectiveModel: string;
	profile: typeof TOOLS_NONE_PROFILE;
	effectiveToolsDigest: Digest;
	effectiveThinkingLevel: ThinkingLevel;
}

const TURN_BINDING_KEYS: readonly (keyof TurnBinding)[] = [
	"authorityId",
	"rootSessionId",
	"parentSessionId",
	"requestId",
	"requestDigest",
	"workflowRunId",
	"nodeId",
	"attemptId",
	"workflowChildId",
	"rlmChildId",
	"turnId",
	"admittedAt",
	"effectiveModel",
	"profile",
	"effectiveToolsDigest",
	"effectiveThinkingLevel",
];

export interface ExactUsage {
	inputTokens: number;
	outputTokens: number;
	cacheReadTokens: number;
	cacheWriteTokens: number;
	totalTokens: number;
	costMicrousd: number | null;
	finality: "final" | "known_prefix";
}

export type ExactResult =
	| { kind: "text"; text: string; utf8Bytes: number; sha256: Digest }
	| { kind: "none"; reason: "no_assistant" | "provider_error" | "cancelled" | "unknown" }
	| { kind: "too_large"; utf8Bytes: number; sha256: Digest };

export interface SafeError {
	code:
		| "PROVIDER_FAILED"
		| "AUTH_FAILED"
		| "MODEL_UNAVAILABLE"
		| "CANCELLED"
		| "RESULT_INVALID"
		| "USAGE_INVALID"
		| "EXECUTION_UNKNOWN"
		| "INTERNAL_ERROR";
	message: string;
	retryable: boolean;
}

export interface ObservedCapture {
	kind: "observed";
	binding: TurnBinding;
	invocationId: string;
	stopReason: CaptureStopReason;
	result: ExactResult;
	usage: ExactUsage;
	observationCount: 1;
	captureDigest: Digest;
	observedAt: string;
	provider: string;
	model: string;
	safeError: SafeError | null;
}

export type AmbiguousCaptureReason =
	| "missing_terminal"
	| "multiple_terminals"
	| "wrong_invocation"
	| "wrong_binding"
	| "late_event"
	| "capture_write_failed"
	| "usage_invalid"
	| "process_lost"
	| "correlation_fault";

export interface AmbiguousCapture {
	kind: "ambiguous";
	binding: TurnBinding;
	invocationId: string;
	reason: AmbiguousCaptureReason;
	usagePrefix: ExactUsage;
	observationCount: number;
	evidenceDigest: Digest;
}

export type TerminalCapture = ObservedCapture | AmbiguousCapture;

export interface ObservedClosure {
	kind: "observed";
	binding: TurnBinding;
	invocationId: string;
	agentEndObserved: true;
	observationCount: number;
	closedAt: string;
	closureDigest: Digest;
}

export type AmbiguousClosureReason =
	| "agent_end_missing"
	| "agent_end_duplicate"
	| "agent_end_wrong_invocation"
	| "closure_write_failed"
	| "process_lost";

export interface AmbiguousClosure {
	kind: "ambiguous";
	binding: TurnBinding;
	invocationId: string;
	agentEndObserved: false;
	reason: AmbiguousClosureReason;
	observationCount: number;
	evidenceDigest: Digest;
}

export type CaptureClosure = ObservedClosure | AmbiguousClosure;

/** Minimal pi-ai assistant content blocks the slot reads. */
export type TerminalContentBlock =
	| { type: "text"; text: string }
	| { type: "thinking"; thinking: string }
	| { type: "toolCall"; id?: string; name?: string }
	| { type: string; [key: string]: unknown };

/** Raw pi-ai `Usage` shape. */
export interface RawUsage {
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	totalTokens: number;
	cost?: { input: number; output: number; cacheRead: number; cacheWrite: number; total: number } | null;
}

/**
 * One owned assistant `message_end` observation, translated by the executor
 * from the low-level `Agent` event bound to the owned invocation. The slot,
 * not this payload, decides ownership and ambiguity.
 */
export interface AssistantTerminalObservation {
	invocationId: string;
	binding: TurnBinding;
	role: string;
	stopReason: string;
	content: readonly TerminalContentBlock[];
	provider: string;
	model: string;
	usage: RawUsage;
	errorMessage?: string | null;
	observedAt: string;
}

/** One owned `agent_end` observation. */
export interface AgentEndObservation {
	invocationId: string;
	binding: TurnBinding;
	closedAt: string;
}

// ---------------------------------------------------------------------------
// Canonical serialization (RFC 8785 subset) and digests
// ---------------------------------------------------------------------------

/**
 * RFC 8785 (JCS) canonical serialization for the closed Slice 3 JSON subset.
 * Object keys are sorted by UTF-16 code unit (JS default string order, which
 * matches JCS). Only null, boolean, safe-integer numbers, strings, arrays, and
 * plain objects are permitted; floats, non-finite, and unsafe integers throw so
 * a digest can never be computed over an unrepresentable value.
 */
export function canonicalize(value: unknown): string {
	if (value === null) return "null";
	const t = typeof value;
	if (t === "boolean") return value ? "true" : "false";
	if (t === "number") {
		if (!Number.isFinite(value as number)) throw new Error("canonicalize: non-finite number");
		if (!Number.isInteger(value as number)) throw new Error("canonicalize: non-integer number");
		if (!Number.isSafeInteger(value as number)) throw new Error("canonicalize: unsafe integer");
		return String(value);
	}
	if (t === "string") return JSON.stringify(value);
	if (Array.isArray(value)) return `[${value.map((item) => canonicalize(item)).join(",")}]`;
	if (t === "object") {
		const obj = value as Record<string, unknown>;
		const keys = Object.keys(obj).sort();
		const parts: string[] = [];
		for (const key of keys) {
			const v = obj[key];
			if (v === undefined) continue;
			parts.push(`${JSON.stringify(key)}:${canonicalize(v)}`);
		}
		return `{${parts.join(",")}}`;
	}
	throw new Error(`canonicalize: unsupported value of type ${t}`);
}

export function sha256HexOfString(text: string): string {
	return createHash("sha256").update(Buffer.from(text, "utf8")).digest("hex");
}

export function sha256HexOfBytes(bytes: Buffer): string {
	return createHash("sha256").update(bytes).digest("hex");
}

/** Digest of a value's canonical bytes, in `sha256:<hex>` form. */
export function digestValue(value: unknown): Digest {
	return `sha256:${sha256HexOfString(canonicalize(value))}`;
}

/**
 * Seal a record with a self digest computed over its canonical bytes without
 * that digest field. Non-self-referential by construction.
 */
export function sealWithDigest<T extends object>(obj: T, field: keyof T & string): T {
	const { [field]: _drop, ...rest } = obj as Record<string, unknown>;
	const digest = digestValue(rest);
	return { ...obj, [field]: digest } as T;
}

// ---------------------------------------------------------------------------
// Text, usage, stop-reason normalization
// ---------------------------------------------------------------------------

/** Concatenate terminal assistant text blocks without separators; exclude thinking and tool-call blocks. */
export function extractTerminalText(content: readonly TerminalContentBlock[]): string {
	let text = "";
	for (const block of content) {
		if (block && block.type === "text" && typeof (block as { text?: unknown }).text === "string") {
			text += (block as { text: string }).text;
		}
	}
	return text;
}

export function hasToolCall(content: readonly TerminalContentBlock[]): boolean {
	return content.some((block) => block && block.type === "toolCall");
}

export function mapStopReason(raw: string): CaptureStopReason {
	switch (raw) {
		case "stop":
			return "stop";
		case "length":
			return "length";
		case "error":
			return "error";
		case "aborted":
			return "aborted";
		case "toolUse":
		case "tool_use":
			return "tool_use";
		default:
			return "other";
	}
}

function isCountToken(value: unknown): value is number {
	return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

/**
 * Exact micro-USD from a shortest round-trip decimal cost. Emits an integer
 * only when `cost.total * 1_000_000` is integral, nonnegative, and safe; else
 * null. Uses BigInt decimal scaling to avoid floating-point drift.
 */
export function decimalToMicrousd(total: unknown): number | null {
	if (typeof total !== "number" || !Number.isFinite(total) || total < 0) return null;
	// Shortest round-trip decimal per ECMAScript Number#toString.
	let s = total.toString();
	// Expand exponent notation, e.g. "1e-7", "1.5e-7", "2e+21".
	if (s.includes("e") || s.includes("E")) {
		const expanded = expandExponent(s);
		if (expanded === null) return null;
		s = expanded;
	}
	const neg = s.startsWith("-");
	if (neg) return null;
	const [intPart, fracPartRaw = ""] = s.split(".");
	const fracPart = fracPartRaw;
	if (fracPart.length > 6) {
		// More than 6 fractional digits cannot scale to an integer micro-USD.
		// Any nonzero digit beyond position 6 makes it non-integral.
		if (/[^0]/.test(fracPart.slice(6))) return null;
	}
	const fracTrunc = fracPart.slice(0, 6).padEnd(6, "0");
	// Reject if beyond-6 digits are all zero but there ARE >6 digits: handled above.
	try {
		const micro = BigInt(intPart) * 1_000_000n + BigInt(fracTrunc);
		if (micro < 0n || micro > BigInt(MAX_SAFE)) return null;
		return Number(micro);
	} catch {
		return null;
	}
}

function expandExponent(s: string): string | null {
	const match = /^(-?)(\d+)(?:\.(\d+))?[eE]([+-]?\d+)$/.exec(s);
	if (!match) return null;
	const [, sign, ip, fp = "", expRaw] = match;
	const exp = Number(expRaw);
	if (!Number.isSafeInteger(exp)) return null;
	const digits = ip + fp;
	const pointPos = ip.length + exp;
	let out: string;
	if (pointPos <= 0) {
		out = `0.${"0".repeat(-pointPos)}${digits}`;
	} else if (pointPos >= digits.length) {
		out = digits + "0".repeat(pointPos - digits.length);
	} else {
		out = `${digits.slice(0, pointPos)}.${digits.slice(pointPos)}`;
	}
	return sign + out;
}

/**
 * Normalize raw pi-ai usage into exact host usage. Returns null when any token
 * counter is a boolean, negative, fractional, non-finite, unsafe, missing, or
 * aliased. `totalTokens` is preserved from the host, never recomputed.
 */
export function normalizeUsage(raw: RawUsage, finality: "final" | "known_prefix"): ExactUsage | null {
	if (!raw || typeof raw !== "object") return null;
	const { input, output, cacheRead, cacheWrite, totalTokens } = raw;
	if (
		!isCountToken(input) ||
		!isCountToken(output) ||
		!isCountToken(cacheRead) ||
		!isCountToken(cacheWrite) ||
		!isCountToken(totalTokens)
	) {
		return null;
	}
	// Reject boolean masquerading (typeof check already excludes, kept explicit).
	if (
		typeof input === "boolean" ||
		typeof output === "boolean" ||
		typeof cacheRead === "boolean" ||
		typeof cacheWrite === "boolean" ||
		typeof totalTokens === "boolean"
	) {
		return null;
	}
	const costMicrousd = raw.cost == null ? null : decimalToMicrousd(raw.cost.total);
	return {
		inputTokens: input,
		outputTokens: output,
		cacheReadTokens: cacheRead,
		cacheWriteTokens: cacheWrite,
		totalTokens,
		costMicrousd,
		finality,
	};
}

export function zeroKnownPrefixUsage(): ExactUsage {
	return {
		inputTokens: 0,
		outputTokens: 0,
		cacheReadTokens: 0,
		cacheWriteTokens: 0,
		totalTokens: 0,
		costMicrousd: null,
		finality: "known_prefix",
	};
}

/** Bound a message to 512 characters and 512 UTF-8 bytes on a codepoint boundary. */
export function boundText(message: string): string {
	let out = message.length > 512 ? message.slice(0, 512) : message;
	while (Buffer.byteLength(out, "utf8") > 512) {
		out = out.slice(0, -1);
	}
	return out;
}

export function bindingEquals(a: TurnBinding, b: TurnBinding): boolean {
	for (const key of TURN_BINDING_KEYS) {
		if (a[key] !== b[key]) return false;
	}
	return true;
}

// ---------------------------------------------------------------------------
// Result classification from one owned terminal
// ---------------------------------------------------------------------------

/** Exact result from a single owned terminal message, using Node UTF-8 bytes. */
export function classifyResult(
	stopReason: CaptureStopReason,
	content: readonly TerminalContentBlock[],
	maxInlineBytes = MAX_INLINE_RESULT_BYTES,
): ExactResult {
	if (stopReason === "error") return { kind: "none", reason: "provider_error" };
	if (stopReason === "aborted") return { kind: "none", reason: "cancelled" };
	if (hasToolCall(content)) return { kind: "none", reason: "no_assistant" };
	const text = extractTerminalText(content);
	const bytes = Buffer.from(text, "utf8");
	const utf8Bytes = bytes.length;
	if (utf8Bytes === 0) return { kind: "none", reason: "no_assistant" };
	if (stopReason === "length") return { kind: "none", reason: "no_assistant" };
	if (stopReason !== "stop") return { kind: "none", reason: "no_assistant" };
	const sha256: Digest = `sha256:${sha256HexOfBytes(bytes)}`;
	if (utf8Bytes > maxInlineBytes) return { kind: "too_large", utf8Bytes, sha256 };
	return { kind: "text", text, utf8Bytes, sha256 };
}

// ---------------------------------------------------------------------------
// Owned-invocation capture slot
// ---------------------------------------------------------------------------

/**
 * At-most-one owned assistant terminal per turn. The slot is bound to one exact
 * `(binding, invocationId)` before prompt admission. It records every observed
 * `message_end` / `agent_end`, classifies ownership, and produces the closed
 * `terminalCapture` and `captureClosure` values. It never scans a transcript
 * and never chooses a terminal by position; anomalies become explicit ambiguity
 * facts, not selection rules.
 */
export class TerminalCaptureSlot {
	private readonly binding: TurnBinding;
	private readonly invocationId: string;
	private readonly maxInlineBytes: number;

	private ownedCount = 0;
	private firstOwned: AssistantTerminalObservation | null = null;
	private wrongInvocationCount = 0;
	private wrongBindingCount = 0;
	private lateEventCount = 0;

	private agentEndCount = 0;
	private agentEndWrongInvocation = 0;
	private firstClose: AgentEndObservation | null = null;

	private captureFault: "capture_write_failed" | "process_lost" | "correlation_fault" | null = null;
	private closureFault: "closure_write_failed" | "process_lost" | null = null;
	private closed = false;

	constructor(binding: TurnBinding, invocationId: string, maxInlineBytes = MAX_INLINE_RESULT_BYTES) {
		this.binding = binding;
		this.invocationId = invocationId;
		this.maxInlineBytes = maxInlineBytes;
	}

	/** Observe one assistant `message_end`. Ownership and ambiguity are decided here, never by the caller. */
	observeMessageEnd(observation: AssistantTerminalObservation): void {
		if (this.closed) {
			this.lateEventCount += 1;
			return;
		}
		if (observation.invocationId !== this.invocationId) {
			this.wrongInvocationCount += 1;
			return;
		}
		if (!bindingEquals(observation.binding, this.binding)) {
			this.wrongBindingCount += 1;
			return;
		}
		if (observation.role !== "assistant") {
			// A non-assistant terminal on the owned invocation is a correlation fault.
			this.captureFault = this.captureFault ?? "correlation_fault";
			return;
		}
		this.ownedCount += 1;
		if (this.firstOwned === null) this.firstOwned = observation;
	}

	/** Observe one `agent_end`. */
	observeAgentEnd(observation: AgentEndObservation): void {
		if (observation.invocationId !== this.invocationId) {
			this.agentEndWrongInvocation += 1;
			return;
		}
		this.agentEndCount += 1;
		if (this.firstClose === null) this.firstClose = observation;
		this.closed = true;
	}

	markCaptureWriteFailed(): void {
		this.captureFault = this.captureFault ?? "capture_write_failed";
	}

	markClosureWriteFailed(): void {
		this.closureFault = this.closureFault ?? "closure_write_failed";
	}

	markProcessLost(): void {
		this.captureFault = this.captureFault ?? "process_lost";
		this.closureFault = this.closureFault ?? "process_lost";
	}

	markCorrelationFault(): void {
		this.captureFault = this.captureFault ?? "correlation_fault";
	}

	private ambiguousReason(): AmbiguousCaptureReason | null {
		if (this.captureFault) return this.captureFault;
		if (this.ownedCount > 1) return "multiple_terminals";
		if (this.wrongBindingCount > 0) return "wrong_binding";
		if (this.wrongInvocationCount > 0) return "wrong_invocation";
		if (this.lateEventCount > 0) return "late_event";
		if (this.ownedCount === 0) return "missing_terminal";
		return null;
	}

	/** Produce the closed terminal capture (observed or ambiguous). Deterministic and side-effect free. */
	capture(): TerminalCapture {
		const reason = this.ambiguousReason();
		if (reason !== null) return this.buildAmbiguous(reason, zeroKnownPrefixUsage());

		// Exactly one clean owned terminal.
		const owned = this.firstOwned as AssistantTerminalObservation;
		const usage = normalizeUsage(owned.usage, "final");
		if (usage === null) return this.buildAmbiguous("usage_invalid", zeroKnownPrefixUsage());

		const stopReason = mapStopReason(owned.stopReason);
		const result = classifyResult(stopReason, owned.content, this.maxInlineBytes);
		const safeError: SafeError | null =
			stopReason === "error"
				? { code: "PROVIDER_FAILED", message: boundText(owned.errorMessage || "provider error"), retryable: false }
				: null;

		const observed: ObservedCapture = {
			kind: "observed",
			binding: this.binding,
			invocationId: this.invocationId,
			stopReason,
			result,
			usage,
			observationCount: 1,
			captureDigest: `sha256:${"0".repeat(64)}` as Digest,
			observedAt: owned.observedAt,
			provider: owned.provider,
			model: owned.model,
			safeError,
		};
		return sealWithDigest(observed, "captureDigest");
	}

	private buildAmbiguous(reason: AmbiguousCaptureReason, usagePrefix: ExactUsage): AmbiguousCapture {
		const ambiguous: AmbiguousCapture = {
			kind: "ambiguous",
			binding: this.binding,
			invocationId: this.invocationId,
			reason,
			usagePrefix,
			observationCount: this.ownedCount,
			evidenceDigest: `sha256:${"0".repeat(64)}` as Digest,
		};
		return sealWithDigest(ambiguous, "evidenceDigest");
	}

	/** Produce the closed capture closure (observed or ambiguous). */
	closure(): CaptureClosure {
		if (this.closureFault) return this.buildAmbiguousClosure(this.closureFault);
		if (this.agentEndWrongInvocation > 0 && this.agentEndCount === 0)
			return this.buildAmbiguousClosure("agent_end_wrong_invocation");
		if (this.agentEndCount === 0) return this.buildAmbiguousClosure("agent_end_missing");
		if (this.agentEndCount > 1) return this.buildAmbiguousClosure("agent_end_duplicate");
		if (this.agentEndWrongInvocation > 0) return this.buildAmbiguousClosure("agent_end_wrong_invocation");

		const closed: ObservedClosure = {
			kind: "observed",
			binding: this.binding,
			invocationId: this.invocationId,
			agentEndObserved: true,
			observationCount: this.ownedCount,
			closedAt: (this.firstClose as AgentEndObservation).closedAt,
			closureDigest: `sha256:${"0".repeat(64)}` as Digest,
		};
		return sealWithDigest(closed, "closureDigest");
	}

	private buildAmbiguousClosure(reason: AmbiguousClosureReason): AmbiguousClosure {
		const ambiguous: AmbiguousClosure = {
			kind: "ambiguous",
			binding: this.binding,
			invocationId: this.invocationId,
			agentEndObserved: false,
			reason,
			observationCount: this.ownedCount,
			evidenceDigest: `sha256:${"0".repeat(64)}` as Digest,
		};
		return sealWithDigest(ambiguous, "evidenceDigest");
	}
}
