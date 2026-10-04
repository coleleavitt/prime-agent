// Regenerates the TS-written index fixtures from the TS product's writer.
//
//   git show perf/session-catalog-resume:packages/coding-agent/src/core/session-catalog-index.ts \
//     > "$TMP/session-catalog-index.ts"
//   node crates/pa-session-index/tests/fixtures/generate.ts "$TMP/session-catalog-index.ts" \
//     crates/pa-session-index/tests/fixtures
//
// The rows mirror what TS `snapshotSessionInfo` builds (its key order), newest first.
import { join } from "node:path";
import { mkdtemp, copyFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";

const [source, out] = process.argv.slice(2);
const index = await import(source);
const dir = await mkdtemp(join(tmpdir(), "pa-session-index-golden-"));

const full = {
	path: join(dir, "019a0000-0000-7000-8000-000000000001.jsonl"),
	id: "019a0000-0000-7000-8000-000000000001",
	cwd: "/work/repo",
	name: "alpha \"quoted\" é",
	state: { status: "active" },
	model: { provider: "anthropic", modelId: "claude-opus-4-5" },
	parentSessionPath: "/home/user/.prime/agent/sessions/parent.jsonl",
	rlmDepth: 1,
	created: new Date("2026-09-01T10:00:00.000Z"),
	modified: new Date("2026-09-01T10:05:00.250Z"),
	messageCount: 4,
	firstMessage: "fix the login bug\nin auth.rs",
	allMessagesText: "fix the login bug\nin auth.rs fixed in auth.rs",
	agentStatus: undefined,
	usage: { inputTokens: 1200, outputTokens: 80, cost: 0.012345 },
};
const minimal = {
	path: join(dir, "019a0000-0000-7000-8000-000000000002.jsonl"),
	id: "019a0000-0000-7000-8000-000000000002",
	cwd: "/work/other",
	name: undefined,
	state: undefined,
	model: undefined,
	parentSessionPath: undefined,
	rlmDepth: 0,
	created: new Date("2026-08-01T00:00:00.000Z"),
	modified: new Date("2026-08-01T00:00:00.000Z"),
	messageCount: 0,
	firstMessage: "(no messages)",
	allMessagesText: "",
	agentStatus: undefined,
	usage: { inputTokens: 3, outputTokens: 0, cost: 0 },
};
const foreign = join(dir, "foreign.jsonl");

const metadata = new Map([
	[full.path, { size: 20480, mtimeMs: 1789177896715.1235, info: full }],
	[minimal.path, { size: 512, mtimeMs: 1785000000000, info: minimal }],
	[foreign, { size: 26, mtimeMs: 1784000000000.5, info: null }],
]);
await index.writeSessionCatalogIndex(dir, metadata);
const corpus = new Map([
	[full.path, { size: 20480, mtimeMs: 1789177896715.1235, searchText: full.allMessagesText }],
	[minimal.path, { size: 512, mtimeMs: 1785000000000, searchText: minimal.allMessagesText }],
]);
await index.writeSessionSearchTextIndex(dir, corpus);
await copyFile(index.getSessionCatalogIndexPath(dir), join(out, "ts-session-index.ndjson"));
await copyFile(index.getSessionSearchTextIndexPath(dir), join(out, "ts-session-search-index.ndjson"));
await rm(dir, { recursive: true });
