import { describe, expect, it } from "vitest";
import { FAILURE_FINGERPRINT_ATTR, spanFingerprintKey } from "../src/core/learning-index.js";
import { fingerprintFailure, fingerprintToolResultText } from "../src/core/ravo/failure-ledger.js";

const TRACEBACK = [
	"Traceback (most recent call last):",
	'  File "<cell>", line 1, in <module>',
	"    websearch(query)",
	"KeyError: 'results'",
].join("\n");

describe("fingerprintToolResultText", () => {
	// The span attribute and the ledger must be the same string, or the join
	// the learning index performs silently matches nothing.
	it("agrees with the ledger's own fingerprint for a python traceback", () => {
		const viaHook = fingerprintToolResultText("ipython", TRACEBACK, false);
		const viaLedger = fingerprintFailure("python_exception", "ipython", "KeyError", "'results'");
		expect(viaHook?.id).toBe(viaLedger.id);
		expect(viaHook?.kind).toBe("python_exception");
	});

	it("fingerprints a flagged tool error that carries no traceback", () => {
		const fingerprint = fingerprintToolResultText("edit", "file not found", true);
		expect(fingerprint?.kind).toBe("tool_error");
		expect(fingerprint?.id).toBe(fingerprintFailure("tool_error", "edit", undefined, "file not found").id);
	});

	it("is undefined for an ordinary successful result", () => {
		expect(fingerprintToolResultText("ipython", "42\n", false)).toBeUndefined();
	});

	it("reads a traceback even when the result was not flagged as an error", () => {
		// A cell that raises returns its traceback as ordinary output; nothing
		// sets isError. This is the path 260 of 387 recorded observations take.
		expect(fingerprintToolResultText("ipython", TRACEBACK, false)?.kind).toBe("python_exception");
	});
});

describe("spanFingerprintKey", () => {
	const span = (name: string, extra: Record<string, unknown> = {}) => ({
		name,
		status: "error",
		msg: "span_end",
		...extra,
	});

	it("keys on the explicit ledger fingerprint when the span carries one", () => {
		const key = spanFingerprintKey(
			span("tool.execute", { attrs: { [FAILURE_FINGERPRINT_ATTR]: "abc123" } }) as never,
		);
		expect(key.fingerprint).toBe("abc123");
		expect(key.failure).toBe(true);
	});

	// Documented double count: kernel.cell errors on every in-cell traceback and
	// the same traceback is fingerprinted onto the enclosing tool.execute. It is
	// left counted on purpose -- suppressing it by name would also drop the
	// SyntaxErrors and snapshot errors that nothing upstream fingerprints, and
	// losing real failures from the control biases toward a false positive.
	it("still counts a kernel failure, accepting the known shadow", () => {
		expect(spanFingerprintKey(span("kernel.cell", { error: "KeyError: ?" }) as never).failure).toBe(true);
	});

	it("still counts failures nothing upstream covers", () => {
		expect(spanFingerprintKey(span("bash.command", { error: "exit code 1" }) as never).failure).toBe(true);
		expect(spanFingerprintKey(span("llm.request", { error: "overloaded" }) as never).failure).toBe(true);
	});

	it("separates failures by their error text once it is present", () => {
		// The hoist in repl-manager puts the Python span's error text where this
		// reads it. Without it every kernel and bash error shares one key.
		const a = spanFingerprintKey(span("bash.command", { error: "exit code 1" }) as never);
		const b = spanFingerprintKey(span("bash.command", { error: "killed by SIGTERM" }) as never);
		const none = spanFingerprintKey(span("bash.command") as never);
		expect(a.fingerprint).not.toBe(b.fingerprint);
		expect(a.fingerprint).not.toBe(none.fingerprint);
	});

	it("deliberately collapses numeric variants of one failure", () => {
		// normalizeFailureMessage maps every digit run to "#", so "exit code 1"
		// and "exit code 128" are one fingerprint on purpose.
		const a = spanFingerprintKey(span("bash.command", { error: "exit code 1" }) as never);
		const b = spanFingerprintKey(span("bash.command", { error: "exit code 128" }) as never);
		expect(a.fingerprint).toBe(b.fingerprint);
	});
});
