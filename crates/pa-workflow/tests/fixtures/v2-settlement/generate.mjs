// Regenerates the slice-3 settlement goldens by running the TS fork's own
// terminal-capture slot and settlement reducer (commit b46ec9b0e) under node.
//
//   C=b46ec9b0e P=packages/coding-agent/src/core; SRC=$(mktemp -d)
//   for f in workflow-v2-terminal-capture workflow-v2-settlement; do
//     git show $C:$P/$f.ts > "$SRC/$f.ts"; done
//   node crates/pa-workflow/tests/fixtures/v2-settlement/generate.mjs "$SRC" \
//     crates/pa-workflow/tests/fixtures/v2-settlement
//
// Both modules run under node's type stripping as they are; only their
// relative `.js` import is pointed at the copied `.ts` file. Output:
// `cases.json`, each case's slot operations and settlement input (the Rust
// test replays them), the TS capture, closure, and commit, and the exact
// `JSON.stringify` bytes of each settled event.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [source, out] = process.argv.slice(2);
const work = mkdtempSync(join(tmpdir(), "wv2-settlement-src-"));
for (const name of ["workflow-v2-terminal-capture", "workflow-v2-settlement"]) {
	const text = readFileSync(join(source, `${name}.ts`), "utf8").replace(
		/from "\.\/(workflow-v2-[a-z0-9-]+)\.js"/g,
		'from "./$1.ts"',
	);
	writeFileSync(join(work, `${name}.ts`), text);
}
const { TerminalCaptureSlot } = await import(join(work, "workflow-v2-terminal-capture.ts"));
const { reduceSettlement } = await import(join(work, "workflow-v2-settlement.ts"));

const D = (c) => `sha256:${c.repeat(64)}`;
const binding = (over = {}) => ({
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
	...over,
});
const usage = (over = {}) => ({
	input: 10,
	output: 20,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 30,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.25 },
	...over,
});
// `repeat` expands to `text.repeat(n)` (keeps the fixture small).
const text = (t, repeat) => ({ type: "text", text: t, ...(repeat ? { repeat } : {}) });
const obs = (over = {}) => ({
	invocationId: "inv1",
	binding: binding(),
	role: "assistant",
	stopReason: "stop",
	content: [text("hello")],
	provider: "prov",
	model: "prov.model",
	usage: usage(),
	errorMessage: null,
	observedAt: "2026-01-01T00:00:01.000Z",
	...over,
});
const end = (over = {}) => ({ invocationId: "inv1", binding: binding(), closedAt: "2026-01-01T00:00:02.000Z", ...over });
const clean = (o = {}) => [{ messageEnd: obs(o) }, { agentEnd: end() }];
const fence = {
	supervisorGeneration: 1,
	supervisorIncarnationId: "sup1",
	workerId: "w1",
	workerGeneration: 1,
	workerIncarnationId: "wi1",
	routeRevision: 1,
};
const input = (over = {}) => ({
	binding: binding(),
	fence,
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
});
const actuated = { requested: true, actuated: true, actuatedAt: "2026-01-01T00:00:02.500Z", orderedAfterCompletion: false };

const cases = [
	{ name: "completed", ops: clean(), input: input() },
	{
		name: "multibyte_text_excludes_thinking_and_tool_calls_never_mix",
		ops: clean({ content: [text("a\u0000é"), { type: "thinking", thinking: "SECRET" }, text("😀é  \"q\"")] }),
		input: input(),
	},
	{ name: "error_terminal", ops: clean({ stopReason: "error", content: [], errorMessage: "boom" }), input: input() },
	{ name: "error_terminal_without_message", ops: clean({ stopReason: "error", content: [], errorMessage: "" }), input: input() },
	{ name: "too_large", ops: clean({ content: [text("x", 262_154)] }), input: input() },
	{ name: "length_terminal", ops: clean({ stopReason: "length", content: [text("hi")] }), input: input() },
	{ name: "tool_call_terminal", ops: clean({ stopReason: "toolUse", content: [text("x"), { type: "toolCall", id: "t", name: "x", arguments: {} }] }), input: input() },
	{ name: "empty_stop", ops: clean({ content: [text("")] }), input: input() },
	{ name: "cancelled", ops: clean({ stopReason: "aborted", content: [] }), input: input({ cancel: actuated }) },
	{ name: "aborted_without_actuation", ops: clean({ stopReason: "aborted", content: [] }), input: input() },
	{ name: "actuated_cancel_after_completion", ops: clean(), input: input({ cancel: { ...actuated, orderedAfterCompletion: true } }) },
	{ name: "actuated_cancel_unordered", ops: clean(), input: input({ cancel: actuated }) },
	{ name: "cancel_requested_only", ops: clean(), input: input({ cancel: { requested: true, actuated: false, actuatedAt: null, orderedAfterCompletion: false } }) },
	{ name: "unproved_quiescence", ops: clean(), input: input({ quiescence: "unproved" }) },
	{ name: "inexact_cost", ops: clean({ usage: usage({ cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.1234565 } }) }), input: input() },
	{ name: "tiny_cost", ops: clean({ usage: usage({ cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 1e-7 } }) }), input: input() },
	{ name: "missing_terminal", ops: [{ agentEnd: end() }], input: input() },
	{ name: "multiple_terminals", ops: [{ messageEnd: obs() }, { messageEnd: obs({ content: [text("second")] }) }, { agentEnd: end() }], input: input() },
	{ name: "wrong_invocation", ops: [{ messageEnd: obs({ invocationId: "other" }) }, { agentEnd: end() }], input: input() },
	{ name: "wrong_binding", ops: [{ messageEnd: obs({ binding: binding({ turnId: "turnX" }) }) }, { agentEnd: end() }], input: input() },
	{ name: "late_event", ops: [{ messageEnd: obs() }, { agentEnd: end() }, { messageEnd: obs() }], input: input() },
	{ name: "missing_agent_end_uncertain_entry", ops: [{ messageEnd: obs() }], input: input({ providerEntry: "uncertain" }) },
	{ name: "duplicate_agent_end", ops: [{ messageEnd: obs() }, { agentEnd: end() }, { agentEnd: end() }], input: input() },
	{ name: "wrong_invocation_agent_end", ops: [{ messageEnd: obs() }, { agentEnd: end({ invocationId: "other" }) }], input: input() },
	{ name: "process_lost", ops: [{ messageEnd: obs() }, { mark: "processLost" }, { agentEnd: end() }], input: input() },
	{ name: "capture_write_failed", ops: [...clean(), { mark: "captureWriteFailed" }], input: input() },
	{ name: "closure_write_failed", ops: [...clean(), { mark: "closureWriteFailed" }], input: input() },
	{ name: "rejected_cursor_not_successor", ops: clean(), input: input({ nextHostCursor: "cur-100" }) },
	{ name: "rejected_correlation", ops: clean(), input: input({ binding: binding({ turnId: "turnZ" }) }) },
	{ name: "rejected_dispatch_sequence", ops: clean(), input: input({ dispatchingSequence: 0 }) },
];

const expand = (o) => ({
	...o,
	content: o.content.map((b) => (b.repeat ? { type: "text", text: b.text.repeat(b.repeat) } : b)),
});
const results = cases.map((c) => {
	const slot = new TerminalCaptureSlot(binding(), "inv1");
	for (const op of c.ops) {
		if (op.messageEnd) slot.observeMessageEnd(expand(op.messageEnd));
		else if (op.agentEnd) slot.observeAgentEnd(op.agentEnd);
		else if (op.mark === "processLost") slot.markProcessLost();
		else if (op.mark === "captureWriteFailed") slot.markCaptureWriteFailed();
		else if (op.mark === "closureWriteFailed") slot.markClosureWriteFailed();
		else throw new Error(`unknown op ${JSON.stringify(op)}`);
	}
	const capture = slot.capture();
	const closure = slot.closure();
	const reduced = reduceSettlement({ ...c.input, capture, captureClosure: closure });
	return {
		...c,
		capture,
		closure,
		...(reduced.kind === "commit"
			? { commit: reduced.commit, eventJson: JSON.stringify(reduced.commit.event) }
			: { rejected: reduced.reason }),
	};
});
writeFileSync(join(out, "cases.json"), `${JSON.stringify(results, null, "\t")}\n`);
process.stdout.write(`cases=${results.length}\n`);
rmSync(work, { recursive: true, force: true });
