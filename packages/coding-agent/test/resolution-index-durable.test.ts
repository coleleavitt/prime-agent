import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type LogEntry, setLogSink } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { getResolutionDir, getResolutionStorePath } from "../src/config.js";
import { DEFAULT_MAX_RESOLUTIONS, openResolutionStore, ResolutionIndex } from "../src/core/distill/resolution-index.js";

const FAILING_CELL = "agents = client.list_agents()";
const FIX_CELL = 'agents = client.agents()\nprint(f"{len(agents)} agents")';
const RECURRENCE_CELL = "for agent in client.list_agents():\n    print(agent.name)";

function attributeErrorTraceback(line: number, source: string): string {
	return [
		"Traceback (most recent call last):",
		`  File "<ipython-input-${line}>", line 1, in <module>`,
		`    ${source}`,
		"AttributeError: 'AgentClient' object has no attribute 'list_agents'",
	].join("\n");
}

let agentDir = "";
let repoDir = "";
let previousAgentDir: string | undefined;
let logs: LogEntry[] = [];

/** A session that has already learned the fix for the AttributeError. */
function sessionThatLearnedTheFix(): ResolutionIndex {
	const index = new ResolutionIndex({ store: openResolutionStore(repoDir) });
	index.observe({ code: FAILING_CELL, output: attributeErrorTraceback(1, FAILING_CELL), isError: true });
	index.observe({ code: FIX_CELL, output: "3 agents", isError: false });
	return index;
}

beforeEach(() => {
	agentDir = realpathSync(mkdtempSync(join(tmpdir(), "resolution-agent-")));
	repoDir = realpathSync(mkdtempSync(join(tmpdir(), "resolution-repo-")));
	execFileSync("git", ["init", "-q"], { cwd: repoDir });
	previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
	logs = [];
	setLogSink((entry) => logs.push(entry));
});

afterEach(() => {
	setLogSink(undefined);
	if (previousAgentDir === undefined) {
		delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	} else {
		process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	}
	rmSync(agentDir, { recursive: true, force: true });
	rmSync(repoDir, { recursive: true, force: true });
});

describe("durable resolution index", () => {
	it("hands a fix recorded by one session to a later session in the same repo", () => {
		const sessionA = sessionThatLearnedTheFix();
		expect(sessionA.records()).toHaveLength(1);

		// A fresh instance owns no in-memory state; everything it knows came off disk.
		const sessionB = new ResolutionIndex({ store: openResolutionStore(repoDir) });
		expect(sessionB.records()).toEqual([]);

		const hint = sessionB.observe({
			code: RECURRENCE_CELL,
			output: attributeErrorTraceback(3, "for agent in client.list_agents():"),
			isError: true,
		});

		expect(hint?.origin).toBe("store");
		expect(hint?.record.fix).toBe(FIX_CELL);
		expect(hint?.text).toContain("You hit this before; this fixed it:");
		expect(hint?.text).toContain('print(f"{len(agents)} agents")');
		expect(hint?.text).toContain("of an earlier session");
	});

	it("keeps each repo's resolutions to itself", () => {
		sessionThatLearnedTheFix();
		const otherRepo = realpathSync(mkdtempSync(join(tmpdir(), "resolution-other-")));
		execFileSync("git", ["init", "-q"], { cwd: otherRepo });
		try {
			const elsewhere = new ResolutionIndex({ store: openResolutionStore(otherRepo) });
			const hint = elsewhere.observe({
				code: RECURRENCE_CELL,
				output: attributeErrorTraceback(3, "for agent in client.list_agents():"),
				isError: true,
			});
			expect(hint).toBeUndefined();
		} finally {
			rmSync(otherRepo, { recursive: true, force: true });
		}
	});

	it("merges a second session's record instead of clobbering the first", () => {
		sessionThatLearnedTheFix();

		const sessionB = new ResolutionIndex({ store: openResolutionStore(repoDir) });
		// A different exception class, so the ledger's normalizer cannot fold this
		// into the fingerprint session A recorded.
		const otherFailure = [
			"Traceback (most recent call last):",
			'  File "<ipython-input-1>", line 1, in <module>',
			"    total = frame.agg(how)",
			"ValueError: cannot reindex on an axis with duplicate labels",
		].join("\n");
		sessionB.observe({ code: "total = frame.agg(how)", output: otherFailure, isError: true });
		sessionB.observe({ code: "total = frame.agg('sum')", output: "ok", isError: false });

		expect(
			sessionB
				.durableRecords()
				.map((record) => record.fix)
				.sort(),
		).toEqual([FIX_CELL, "total = frame.agg('sum')"].sort());
	});

	it("degrades to an empty store and logs when the file is corrupt", () => {
		sessionThatLearnedTheFix();
		const storePath = getResolutionStorePath(repoDir, agentDir);
		writeFileSync(storePath, "{ not json at all");
		logs = [];

		const session = new ResolutionIndex({ store: openResolutionStore(repoDir) });
		let hint: unknown;
		expect(() => {
			hint = session.observe({
				code: RECURRENCE_CELL,
				output: attributeErrorTraceback(3, "for agent in client.list_agents():"),
				isError: true,
			});
		}).not.toThrow();
		expect(hint).toBeUndefined();
		expect(logs.filter((entry) => entry.level === "warn" && entry.msg.includes("unreadable"))).not.toHaveLength(0);
	});

	it("writes the store owner-only, because a record holds verbatim cell source", () => {
		sessionThatLearnedTheFix();
		const storePath = getResolutionStorePath(repoDir, agentDir);
		expect(existsSync(storePath)).toBe(true);
		expect(statSync(storePath).mode & 0o777).toBe(0o600);
		expect(readFileSync(storePath, "utf-8")).toContain("client.agents()");
	});

	it("bounds the store at the retained-record cap", () => {
		const index = new ResolutionIndex({ store: openResolutionStore(repoDir), maxRecords: 1 });
		for (let i = 0; i < DEFAULT_MAX_RESOLUTIONS + 6; i++) {
			// Digits, quoted values and long hex runs are normalized out of a
			// fingerprint, so the attribute name is what has to differ here — and it
			// has to be built from a letter that cannot read as hex.
			const name = "z".repeat(i + 1);
			const failure = [
				"Traceback (most recent call last):",
				'  File "<ipython-input-1>", line 1, in <module>',
				`    handle.${name}()`,
				`AttributeError: module tool has no attribute ${name}`,
			].join("\n");
			index.observe({ code: `handle.${name}()`, output: failure, isError: true });
			index.observe({ code: `handle.run_${name}()`, output: "ok", isError: false });
		}
		expect(index.durableRecords()).toHaveLength(DEFAULT_MAX_RESOLUTIONS);
		expect(index.durableRecords().at(-1)?.fix).toBe(`handle.run_${"z".repeat(DEFAULT_MAX_RESOLUTIONS + 6)}()`);
	});

	it("is a no-op outside a git repository", () => {
		const looseDir = realpathSync(mkdtempSync(join(tmpdir(), "resolution-loose-")));
		try {
			expect(openResolutionStore(looseDir)).toBeUndefined();
			const index = new ResolutionIndex({ store: openResolutionStore(looseDir) });
			expect(() => {
				index.observe({ code: FAILING_CELL, output: attributeErrorTraceback(1, FAILING_CELL), isError: true });
				index.observe({ code: FIX_CELL, output: "3 agents", isError: false });
			}).not.toThrow();
			expect(index.records()).toHaveLength(1);
			expect(index.durableRecords()).toEqual([]);
			expect(existsSync(getResolutionDir(agentDir))).toBe(false);
		} finally {
			rmSync(looseDir, { recursive: true, force: true });
		}
	});
});
