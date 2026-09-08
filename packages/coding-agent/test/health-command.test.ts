import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gzipSync } from "node:zlib";
import { afterEach, describe, expect, it } from "vitest";
import { parseHealthCommandArgs, runHealthCommand } from "../src/cli/health-command.js";

const NOW = Date.parse("2026-09-08T12:00:00.000Z");
const TRACE_PROVIDER = "0af7651916cd43dd8448eb211c80319c";
const TRACE_STUCK = "1bf7651916cd43dd8448eb211c80319d";
const dirs: string[] = [];

function row(fields: Record<string, unknown>): string {
	return JSON.stringify(fields);
}
function fixture(): string {
	return [
		row({
			ts: "2026-09-07T10:00:00.000Z",
			component: "ai.provider",
			msg: "provider stream failure",
			provider: "old",
			message: "outside window",
		}),
		row({
			ts: "2026-09-08T10:00:00.000Z",
			component: "trace",
			msg: "span_end",
			name: "historian.validate",
			status: "error",
			error: "invalid summary",
			traceId: "2cf7651916cd43dd8448eb211c80319e",
			attrs: { "historian.session_id": "hist" },
		}),
		row({
			ts: "2026-09-08T10:10:00.000Z",
			component: "ai.provider",
			msg: "provider stream failure",
			provider: "openai",
			message: "rate limited",
			traceId: TRACE_PROVIDER,
			spanId: "provider-span",
			sessionId: "session-1",
		}),
		row({
			ts: "2026-09-08T10:10:00.001Z",
			component: "trace",
			msg: "span_end",
			name: "llm.request",
			status: "error",
			error: "rate limited",
			traceId: TRACE_PROVIDER,
			spanId: "provider-span",
			attrs: { "llm.provider": "openai" },
		}),
		row({
			ts: "2026-09-08T10:59:00.000Z",
			component: "trace",
			msg: "span_start",
			name: "agent.prompt",
			spanId: "open-turn",
			traceId: TRACE_STUCK,
			attrs: { "session.id": "session-stuck" },
		}),
		row({
			ts: "2026-09-08T11:00:00.000Z",
			component: "trace",
			msg: "span_end",
			name: "llm.request",
			status: "ok",
			spanId: "child",
			parentSpanId: "open-turn",
			traceId: TRACE_STUCK,
			attrs: { "llm.provider": "anthropic" },
		}),
		row({
			ts: "2026-09-08T11:50:00.000Z",
			component: "coding-agent.daemon-supervisor",
			msg: "Worker worker-1 recovery failed: socket closed",
		}),
		row({
			ts: "2026-09-08T11:55:00.000Z",
			component: "coding-agent.daemon-supervisor",
			msg: "Worker worker-2 recovered successfully",
		}),
		"not json",
	].join("\n");
}
function writeLog(): string {
	const dir = mkdtempSync(join(tmpdir(), "health-command-"));
	dirs.push(dir);
	const path = join(dir, "agent.jsonl");
	writeFileSync(path, `${fixture()}\n`);
	return path;
}
function run(args: string[]) {
	const stdout: string[] = [];
	const stderr: string[] = [];
	const code = runHealthCommand(args, {
		stdout: (value) => stdout.push(value),
		stderr: (value) => stderr.push(value),
		now: () => NOW,
	});
	return { code, stdout, stderr };
}
afterEach(() => {
	while (dirs.length) rmSync(dirs.pop()!, { recursive: true, force: true });
});

describe("health command", () => {
	it("parses bounded duration and output options", () => {
		expect(parseHealthCommandArgs(["--since=6h", "--stuck-after", "30m", "--limit", "50", "--json"])).toMatchObject({
			sinceMs: 21_600_000,
			stuckAfterMs: 1_800_000,
			limit: 50,
			json: true,
		});
		expect(() => parseHealthCommandArgs(["--since", "soon"])).toThrow(/requires a duration/);
		expect(() => parseHealthCommandArgs(["--limit", "201"])).toThrow(/1 to 200/);
		expect(() => parseHealthCommandArgs(["extra"])).toThrow(/Unknown option/);
	});

	it("summarizes all four incident classes and deduplicates provider span/log pairs", () => {
		const path = writeLog();
		const result = run(["--log", path]);
		expect(result.code).toBe(2);
		expect(result.stderr).toEqual([]);
		expect(result.stdout[0]).toContain("health since 2026-09-07T12:00:00.000Z  (5 incidents;");
		expect(result.stdout[0]).toContain("Historian failures: 1");
		expect(result.stdout[0]).toContain("Provider errors: 1");
		expect(result.stdout[0]).toContain("Stuck turns: 1");
		expect(result.stdout[0]).toContain("Daemon recovery failures: 1");
		expect(result.stdout[0]).not.toContain("outside window");
		expect(result.stdout[0]).not.toContain("recovered successfully");
		expect(result.stdout[0]).toContain(`trace=${TRACE_PROVIDER}`);
	});

	it("prints stable JSON and bounds incident details without losing counts", () => {
		const path = writeLog();
		const result = run(["--log", path, "--json", "--limit", "2"]);
		const summary = JSON.parse(result.stdout[0]!) as {
			status: string;
			counts: Record<string, number>;
			incidents: unknown[];
			truncated: boolean;
		};
		expect(summary.counts).toMatchObject({ historian: 1, provider: 1, stuck_turn: 1, daemon_recovery: 1 });
		expect(summary.status).toBe("unhealthy");
		expect(summary.incidents).toHaveLength(2);
		expect(summary.truncated).toBe(true);
	});

	it("flags an active-operation span_start with no matching end even without child spans", () => {
		const path = writeLog();
		writeFileSync(
			path,
			`${fixture()}\n${row({ ts: "2026-09-08T11:00:00.000Z", component: "trace", msg: "span_start", name: "agent.prompt", spanId: "silent-open", traceId: TRACE_STUCK, attrs: { "session.id": "session-open" } })}\n`,
		);
		const result = run(["--log", path, "--json"]);
		const summary = JSON.parse(result.stdout[0]!) as {
			counts: Record<string, number>;
			incidents: Array<{ summary: string }>;
		};
		expect(summary.counts.stuck_turn).toBe(2);
		expect(summary.incidents.some((item) => item.summary.includes("agent.prompt span silent-open"))).toBe(true);
	});

	it("detects a still-open turn that started before the incident window", () => {
		const path = writeLog();
		writeFileSync(
			path,
			`${fixture()}\n${row({ ts: "2026-09-06T11:00:00.000Z", component: "trace", msg: "span_start", name: "client.turn", spanId: "old-open", traceId: TRACE_STUCK })}\n`,
		);
		const result = run(["--log", path, "--json"]);
		const summary = JSON.parse(result.stdout[0]!) as { counts: Record<string, number> };
		expect(summary.counts.stuck_turn).toBe(2);
	});

	it("does not flag a child whose parent span ended", () => {
		const path = writeLog();
		writeFileSync(
			path,
			`${fixture()}\n${row({ ts: "2026-09-08T11:01:00.000Z", component: "trace", msg: "span_end", name: "agent.turn", status: "ok", spanId: "open-turn", traceId: TRACE_STUCK })}\n`,
		);
		const result = run(["--log", path, "--json"]);
		const summary = JSON.parse(result.stdout[0]!) as { counts: Record<string, number> };
		expect(summary.counts.stuck_turn).toBe(0);
	});

	it("reads gzip-compressed retained generations", () => {
		const path = writeLog();
		writeFileSync(
			`${path}.old.1.gz`,
			gzipSync(
				`${row({ ts: "2026-09-08T09:00:00.000Z", component: "ai.provider", msg: "provider stream failure", provider: "retained", message: "failed" })}\n`,
			),
		);
		const result = run(["--log", path, "--json"]);
		const summary = JSON.parse(result.stdout[0]!) as { counts: Record<string, number>; files: string[] };
		expect(summary.counts.provider).toBe(2);
		expect(summary.files[0]).toBe(`${path}.old.1.gz`);
	});

	it("expands process, kernel, child, lock, and orphan evidence and fails closed", () => {
		const path = writeLog();
		writeFileSync(
			path,
			`${[
				row({
					ts: "2026-09-08T10:00:00.000Z",
					component: "trace",
					msg: "span_start",
					name: "bash.command",
					traceId: "p",
					spanId: "p1",
					attrs: { "bash.command": "sleep 99" },
				}),
				row({
					ts: "2026-09-08T10:01:00.000Z",
					component: "trace",
					msg: "span_start",
					name: "kernel.cell",
					traceId: "k",
					spanId: "k1",
					attrs: { "kernel.cell": "print(1)" },
				}),
				row({
					ts: "2026-09-08T10:02:00.000Z",
					component: "trace",
					msg: "span_end",
					name: "rlm.child",
					status: "error",
					error: "child timed out",
					traceId: "c",
					spanId: "c1",
				}),
				row({
					ts: "2026-09-08T10:03:00.000Z",
					component: "trace",
					msg: "span_end",
					name: "cargo_lock_wait",
					status: "error",
					error: "lock timed out",
					traceId: "l",
					spanId: "l1",
				}),
				row({
					ts: "2026-09-08T10:04:00.000Z",
					component: "orphan-process",
					msg: "Could not reap orphaned worker resources",
				}),
				row({
					ts: "2026-09-08T10:05:00.000Z",
					component: "kernel",
					msg: "kernel_exit",
					message: "unexpected exit",
				}),
			].join("\n")}\n`,
		);
		const result = run(["--log", path, "--json"]);
		const summary = JSON.parse(result.stdout[0]!) as { status: string; counts: Record<string, number> };
		expect(result.code).toBe(2);
		expect(summary.status).toBe("unhealthy");
		expect(summary.counts).toMatchObject({ process: 1, kernel: 2, child: 1, lock: 1, orphan: 1 });
	});

	it("returns UNKNOWN for malformed, empty, or stale evidence", () => {
		const path = writeLog();
		writeFileSync(path, "not json\n");
		let result = run(["--log", path, "--json"]);
		let summary = JSON.parse(result.stdout[0]!) as { status: string; parseErrors: number; stale: boolean };
		expect(result.code).toBe(2);
		expect(summary).toMatchObject({ status: "unknown", parseErrors: 1, stale: true });

		writeFileSync(path, `${row({ ts: "2026-09-01T00:00:00.000Z", component: "session", msg: "ok" })}\n`);
		result = run(["--log", path, "--json"]);
		summary = JSON.parse(result.stdout[0]!) as typeof summary;
		expect(summary).toMatchObject({ status: "unknown", parseErrors: 0, stale: true });
	});

	it("returns healthy only with valid recent evidence and no incidents", () => {
		const path = writeLog();
		writeFileSync(path, `${row({ ts: "2026-09-08T11:59:00.000Z", component: "session", msg: "heartbeat" })}\n`);
		const result = run(["--log", path, "--json"]);
		expect(result.code).toBe(0);
		expect(JSON.parse(result.stdout[0]!)).toMatchObject({ status: "healthy", parseErrors: 0, stale: false });
	});

	it("keeps unmatched starts visible after the 100k bounded entry buffer evicts their position", () => {
		const path = writeLog();
		const lines = [
			row({
				ts: "2026-09-08T10:00:00.000Z",
				component: "trace",
				msg: "span_start",
				name: "bash.command",
				traceId: "large",
				spanId: "open",
			}),
		];
		for (let index = 0; index < 100_001; index++) {
			lines.push(row({ ts: "2026-09-08T10:01:00.000Z", component: "noise", msg: `line ${index}` }));
		}
		writeFileSync(path, `${lines.join("\n")}\n`);
		const result = run(["--log", path, "--json"]);
		const summary = JSON.parse(result.stdout[0]!) as { counts: Record<string, number> };
		expect(summary.counts.process).toBe(1);
	});

	it("reports missing logs and usage errors", () => {
		expect(run(["--log", "/missing/agent.jsonl"]).stderr[0]).toBe("Error: no log file at /missing/agent.jsonl");
		const bad = run(["--limit", "0"]);
		expect(bad.code).toBe(1);
		expect(bad.stderr[1]).toContain("Usage: prime-agent health");
	});
});
