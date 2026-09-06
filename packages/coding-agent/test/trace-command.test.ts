import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
	buildTraceTree,
	normalizeTraceId,
	parseTraceCommandArgs,
	readTraceLogLines,
	runTraceCommand,
	TraceCommandUsageError,
	traceLogFiles,
} from "../src/cli/trace-command.js";

const TRACE = "0af7651916cd43dd8448eb211c80319c";
const OTHER_TRACE = "1bf7651916cd43dd8448eb211c80319d";
const TURN = "b7ad6b7169203331";
const LLM = "c8be7c8270314442";
const TOOL = "d9cf8d9381425553";

function line(fields: Record<string, unknown>): string {
	return JSON.stringify(fields);
}

/**
 * agent.turn (ends last) with two children: llm.request, then tool.execute.
 * A stray info line sits under llm.request, one line is bound to the trace but
 * to no span, and an unrelated trace must be filtered out entirely.
 */
const FIXTURE = [
	line({
		ts: "2026-09-07T10:00:00.100Z",
		level: "info",
		component: "session",
		msg: "turn started",
		traceId: TRACE,
		spanId: TURN,
		sessionId: "abc",
	}),
	line({
		ts: "2026-09-07T10:00:00.200Z",
		level: "info",
		component: "ai.provider",
		msg: "request",
		traceId: TRACE,
		spanId: LLM,
		parentSpanId: TURN,
		baseUrl: "https://api.example.test/v1",
	}),
	line({
		ts: "2026-09-07T10:00:00.250Z",
		level: "warn",
		component: "other",
		msg: "unrelated",
		traceId: OTHER_TRACE,
		spanId: "eeeeeeeeeeeeeeee",
	}),
	"this line is not json",
	line({
		ts: "2026-09-07T10:00:00.900Z",
		level: "warn",
		component: "trace",
		msg: "span_end",
		name: "llm.request",
		traceId: TRACE,
		spanId: LLM,
		parentSpanId: TURN,
		durationMs: 750,
		status: "error",
		attrs: { "llm.provider": "openai", "llm.base_url": "https://api.example.test/v1" },
		error: "401 archived",
	}),
	line({
		ts: "2026-09-07T10:00:01.000Z",
		level: "info",
		component: "trace",
		msg: "span_end",
		name: "tool.execute",
		traceId: TRACE,
		spanId: TOOL,
		parentSpanId: TURN,
		durationMs: 50,
		status: "ok",
		attrs: { "tool.name": "bash", "tool.call_id": "call_1" },
	}),
	line({
		ts: "2026-09-07T10:00:01.100Z",
		level: "info",
		component: "trace",
		msg: "span_end",
		name: "agent.turn",
		traceId: TRACE,
		spanId: TURN,
		durationMs: 1050,
		status: "ok",
		attrs: { "session.id": "abc", "turn.index": 1 },
	}),
	line({ ts: "2026-09-07T10:00:01.200Z", level: "debug", component: "daemon", msg: "context only", traceId: TRACE }),
	line({
		ts: "2026-09-07T10:00:01.300Z",
		level: "info",
		component: "trace",
		msg: "span_end",
		name: "other.span",
		traceId: OTHER_TRACE,
		spanId: "ffffffffffffffff",
		durationMs: 1,
		status: "ok",
		attrs: {},
	}),
].join("\n");

const tempDirs: string[] = [];

function writeFixture(content: string = FIXTURE, name = "agent.jsonl"): string {
	const dir = mkdtempSync(join(tmpdir(), "trace-command-"));
	tempDirs.push(dir);
	const path = join(dir, name);
	writeFileSync(path, `${content}\n`);
	return path;
}

function run(args: string[]): { code: number; stdout: string[]; stderr: string[] } {
	const stdout: string[] = [];
	const stderr: string[] = [];
	const code = runTraceCommand(args, { stdout: (text) => stdout.push(text), stderr: (text) => stderr.push(text) });
	return { code, stdout, stderr };
}

afterEach(() => {
	while (tempDirs.length > 0) {
		rmSync(tempDirs.pop()!, { recursive: true, force: true });
	}
});

describe("trace id input", () => {
	it("accepts a bare trace id, upper-case hex, and a full traceparent", () => {
		expect(normalizeTraceId(TRACE)).toBe(TRACE);
		expect(normalizeTraceId(TRACE.toUpperCase())).toBe(TRACE);
		expect(normalizeTraceId(`00-${TRACE}-${TURN}-01`)).toBe(TRACE);
	});

	it("rejects malformed ids with a usage error", () => {
		expect(() => normalizeTraceId("nope")).toThrow(TraceCommandUsageError);
		expect(() => normalizeTraceId("0".repeat(32))).toThrow(TraceCommandUsageError);
		expect(() => normalizeTraceId(`01-${TRACE}-${TURN}-01`)).toThrow(TraceCommandUsageError);
	});

	it("parses --log and --json in any position", () => {
		expect(parseTraceCommandArgs(["--json", TRACE, "--log", "/tmp/x.jsonl"])).toEqual({
			traceId: TRACE,
			logPath: "/tmp/x.jsonl",
			json: true,
		});
		expect(parseTraceCommandArgs([TRACE, "--log=/tmp/y.jsonl"])).toMatchObject({
			logPath: "/tmp/y.jsonl",
			json: false,
		});
		expect(() => parseTraceCommandArgs([])).toThrow(/Missing trace id/);
		expect(() => parseTraceCommandArgs([TRACE, "--log"])).toThrow(/--log requires a path/);
		expect(() => parseTraceCommandArgs([TRACE, "--bogus"])).toThrow(/Unknown option/);
		expect(() => parseTraceCommandArgs([TRACE, OTHER_TRACE])).toThrow(/exactly one/);
	});
});

describe("trace tree", () => {
	it("renders nested spans, attributed log lines, and orphans; drops other traces", () => {
		const path = writeFixture();
		const { code, stdout, stderr } = run([TRACE, "--log", path]);
		expect(stderr).toEqual([]);
		expect(code).toBe(0);
		const text = stdout.join("\n");
		expect(text).toBe(
			[
				`trace ${TRACE}  (3 spans, 3 log lines, ${path})`,
				`├─ agent.turn  1050ms  ok  session.id=abc turn.index=1  [${TURN}]`,
				`│  ├─ 10:00:00.100  info   session  turn started  sessionId=abc`,
				`│  ├─ llm.request  750ms  error  llm.provider=openai llm.base_url=https://api.example.test/v1  error=401 archived  [${LLM}]`,
				`│  │  └─ 10:00:00.200  info   ai.provider  request  baseUrl=https://api.example.test/v1`,
				`│  └─ tool.execute  50ms  ok  tool.name=bash tool.call_id=call_1  [${TOOL}]`,
				`└─ (no span)`,
				`   └─ 10:00:01.200  debug  daemon  context only`,
			].join("\n"),
		);
		expect(text).not.toContain("unrelated");
		expect(text).not.toContain("other.span");
	});

	it("accepts a traceparent on the command line", () => {
		const path = writeFixture();
		const { code, stdout } = run([`00-${TRACE}-${TURN}-01`, "--log", path]);
		expect(code).toBe(0);
		expect(stdout[0]).toContain(`trace ${TRACE}`);
		expect(stdout[0]).toContain("agent.turn");
	});

	it("keeps a running parent visible as an open span placeholder", () => {
		// The turn has not ended yet: only its children and its log line are on disk.
		const path = writeFixture(
			FIXTURE.split("\n")
				.filter((raw) => !raw.includes('"name":"agent.turn"'))
				.join("\n"),
		);
		const tree = buildTraceTree(TRACE, readTraceLogLines([path], TRACE));
		expect(tree.spanCount).toBe(2);
		expect(tree.roots.map((root) => [root.name, root.spanId])).toEqual([["(open span)", TURN]]);
		expect(tree.roots[0]!.children.map((child) => child.name)).toEqual(["llm.request", "tool.execute"]);
		expect(tree.roots[0]!.logs.map((line) => line.entry.msg)).toEqual(["turn started"]);
	});

	it("reads the rotated .old sibling before the live log", () => {
		const path = writeFixture(FIXTURE.split("\n").slice(4).join("\n"));
		writeFileSync(`${path}.old`, `${FIXTURE.split("\n").slice(0, 4).join("\n")}\n`);
		expect(traceLogFiles(path)).toEqual([`${path}.old`, path]);
		const { code, stdout } = run([TRACE, "--log", path]);
		expect(code).toBe(0);
		expect(stdout[0]).toContain(`3 spans, 3 log lines, ${path}.old, ${path}`);
		expect(stdout[0]).toContain("turn started");
	});

	it("--json prints the raw matching lines in file order", () => {
		const path = writeFixture();
		const { code, stdout } = run([TRACE, "--json", "--log", path]);
		expect(code).toBe(0);
		expect(stdout).toHaveLength(6);
		expect(stdout.every((raw) => (JSON.parse(raw) as { traceId: string }).traceId === TRACE)).toBe(true);
		expect(stdout).toEqual(FIXTURE.split("\n").filter((raw) => raw.includes(TRACE)));
	});
});

describe("trace command failure paths", () => {
	it("exits 1 with a clear message when the trace is absent", () => {
		const path = writeFixture();
		const missing = "abcdefabcdefabcdefabcdefabcdefab";
		const { code, stdout, stderr } = run([missing, "--log", path]);
		expect(code).toBe(1);
		expect(stdout).toEqual([]);
		expect(stderr).toEqual([`Error: no entries for trace ${missing} in ${path}`]);
	});

	it("exits 1 when the log file does not exist", () => {
		const { code, stderr } = run([TRACE, "--log", "/nonexistent/agent.jsonl"]);
		expect(code).toBe(1);
		expect(stderr[0]).toBe("Error: no log file at /nonexistent/agent.jsonl");
	});

	it("exits 1 with usage on bad arguments", () => {
		const { code, stderr } = run(["not-a-trace"]);
		expect(code).toBe(1);
		expect(stderr[0]).toContain("Not a trace id or traceparent");
		expect(stderr[1]).toContain("Usage: prime-agent trace <traceId|traceparent> [--log <path>] [--json]");
	});
});
