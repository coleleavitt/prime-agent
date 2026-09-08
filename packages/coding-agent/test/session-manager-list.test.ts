import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

const scan = vi.hoisted(() => ({
	active: 0,
	maxActive: 0,
	starts: [] as string[],
	delays: new Map<string, number>(),
}));

vi.mock("../src/utils/file-lines.js", async (importOriginal) => {
	const actual = await importOriginal<typeof import("../src/utils/file-lines.js")>();
	return {
		...actual,
		async *readLinesAsBuffers(filePath: string): AsyncGenerator<Buffer> {
			scan.active++;
			scan.starts.push(filePath);
			scan.maxActive = Math.max(scan.maxActive, scan.active);
			try {
				await new Promise((resolve) => setTimeout(resolve, scan.delays.get(filePath) ?? 0));
				for (const line of readFileSync(filePath, "utf8").trimEnd().split("\n")) {
					yield Buffer.from(line);
				}
			} finally {
				scan.active--;
			}
		},
	};
});

import {
	getSessionCatalogIndexPath,
	getSessionSearchTextIndexPath,
	SESSION_SEARCH_TEXT_RETENTION,
} from "../src/core/session-catalog-index.js";
import { SessionManager } from "../src/core/session-manager.js";

const tempDirs: string[] = [];

afterEach(() => {
	scan.active = 0;
	scan.maxActive = 0;
	scan.starts.length = 0;
	scan.delays.clear();
	while (tempDirs.length > 0) {
		rmSync(tempDirs.pop()!, { recursive: true, force: true });
	}
});

function createSession(dir: string, index: number, ...messages: string[]): string {
	const id = `session-${index.toString().padStart(2, "0")}`;
	const file = join(dir, `${id}.jsonl`);
	const lines = [
		JSON.stringify({
			type: "session",
			version: 3,
			id,
			timestamp: new Date(index * 1000).toISOString(),
			cwd: "/tmp/project",
		}),
	];
	for (const [messageIndex, message] of messages.entries()) {
		lines.push(
			JSON.stringify({
				type: "message",
				id: `${id}-m${messageIndex}`,
				timestamp: new Date(index * 1000).toISOString(),
				message: { role: "user", content: message },
			}),
		);
	}
	writeFileSync(file, `${lines.join("\n")}\n`);
	return file;
}

// Writes an index exactly as another process would, so listing can be exercised
// with a warm catalog that this process never scanned.
function writeIndex(
	sessionDir: string,
	files: string[],
	overrides: { size?: number; searchText?: boolean } = {},
): void {
	const lines = [JSON.stringify({ version: 2 })];
	const corpusLines = [JSON.stringify({ version: 2 })];
	for (const [index, file] of files.entries()) {
		const stats = statSync(file);
		lines.push(
			JSON.stringify({
				file: basename(file),
				size: overrides.size ?? stats.size,
				mtimeMs: stats.mtimeMs,
				info: {
					id: `indexed-${index}`,
					cwd: "/tmp/project",
					rlmDepth: 0,
					created: new Date(index * 1000).toISOString(),
					modified: new Date(index * 1000).toISOString(),
					messageCount: 1,
					firstMessage: `indexed ${index}`,
				},
			}),
		);
		corpusLines.push(
			JSON.stringify({
				file: basename(file),
				size: overrides.size ?? stats.size,
				mtimeMs: stats.mtimeMs,
				searchText: `corpus ${index}`,
			}),
		);
	}
	writeFileSync(getSessionCatalogIndexPath(sessionDir), `${lines.join("\n")}\n`);
	if (overrides.searchText !== false) {
		writeFileSync(getSessionSearchTextIndexPath(sessionDir), `${corpusLines.join("\n")}\n`);
	}
}

describe("SessionManager saved-session listing", () => {
	it("bounds concurrent JSONL scans and preserves callback order", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-list-"));
		tempDirs.push(dir);
		const files = Array.from({ length: 12 }, (_, index) => createSession(dir, index));
		for (const [index, file] of files.entries()) {
			// Finish scans in reverse order inside each batch. Callback order must
			// still follow the stable directory order rather than completion order.
			scan.delays.set(file, (files.length - index) * 2);
		}
		const discovered: string[] = [];
		const progress: Array<[number, number]> = [];

		await SessionManager.listAll(
			{
				onSession: (session) => discovered.push(session.id),
				onProgress: (loaded, total) => progress.push([loaded, total]),
			},
			dir,
		);

		expect(scan.maxActive).toBeGreaterThan(1);
		expect(scan.maxActive).toBeLessThanOrEqual(8);
		expect(discovered).toEqual(files.map((file) => file.slice(file.lastIndexOf("/") + 1, -".jsonl".length)));
		expect(progress).toEqual(files.map((_, index) => [index + 1, files.length]));
	});

	it("shares one process-wide scan budget across overlapping catalog requests", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-list-parallel-"));
		tempDirs.push(dir);
		const files = Array.from({ length: 12 }, (_, index) => createSession(dir, index));
		for (const file of files) {
			scan.delays.set(file, 5);
		}

		const [first, second, third] = await Promise.all([
			SessionManager.listAll(undefined, dir),
			SessionManager.listAll(undefined, dir),
			SessionManager.listAll(undefined, dir),
		]);

		// Three concurrent catalog requests must not triple the open scans, and the
		// same file must not be read once per request.
		expect(scan.maxActive).toBeLessThanOrEqual(8);
		expect(scan.starts).toHaveLength(files.length);
		expect(new Set(scan.starts).size).toBe(files.length);
		const ids = files.map((file) => file.slice(file.lastIndexOf("/") + 1, -".jsonl".length));
		for (const sessions of [first, second, third]) {
			expect(sessions.map((session) => session.id).sort()).toEqual([...ids].sort());
		}
	});

	it("publishes sessions in sorted file order regardless of creation order", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-list-order-"));
		tempDirs.push(dir);
		// Create newest-first so creation order is the reverse of lexical order.
		const created = [9, 3, 7, 1, 5].map((index) => createSession(dir, index));
		const discovered: string[] = [];

		await SessionManager.listAll({ onSession: (session) => discovered.push(session.id) }, dir);

		const expected = created.map((file) => file.slice(file.lastIndexOf("/") + 1, -".jsonl".length)).sort();
		expect(discovered).toEqual(expected);
	});

	it("persists catalog metadata so a cold process can skip reparsing", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-"));
		tempDirs.push(dir);
		const files = Array.from({ length: 3 }, (_, index) => createSession(dir, index));

		await SessionManager.listAll(undefined, dir);

		const lines = readFileSync(getSessionCatalogIndexPath(dir), "utf8").trimEnd().split("\n");
		expect(JSON.parse(lines[0]!)).toEqual({ version: 2 });
		expect(lines.slice(1).map((line) => JSON.parse(line).file)).toEqual(files.map((file) => basename(file)));
	});

	it("reuses a valid on-disk index without reading any session file", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-warm-"));
		tempDirs.push(dir);
		// A different process wrote this index; nothing here has been scanned yet.
		writeIndex(dir, [createSession(dir, 0), createSession(dir, 1)]);

		const sessions = await SessionManager.listAll(undefined, dir);

		expect(scan.starts).toEqual([]);
		// listAll sorts newest-modified first.
		expect(sessions.map((session) => session.firstMessage)).toEqual(["indexed 1", "indexed 0"]);
	});

	it("rescans a session whose file changed after the index was written", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-stale-"));
		tempDirs.push(dir);
		const file = createSession(dir, 0);
		writeIndex(dir, [file], { size: 1 });

		const sessions = await SessionManager.listAll(undefined, dir);

		expect(scan.starts).toEqual([file]);
		expect(sessions[0]?.firstMessage).not.toBe("indexed 0");
	});

	it("falls back to scanning when the index is corrupt", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-corrupt-"));
		tempDirs.push(dir);
		const file = createSession(dir, 0);
		writeFileSync(getSessionCatalogIndexPath(dir), "not json\n{oops\n");

		const sessions = await SessionManager.listAll(undefined, dir);

		expect(scan.starts).toEqual([file]);
		expect(sessions).toHaveLength(1);
	});

	it("refreshes the index after a session file changes", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-refresh-"));
		tempDirs.push(dir);
		const file = createSession(dir, 0);
		writeIndex(dir, [file], { size: 1 });

		await SessionManager.listAll(undefined, dir);

		const lines = readFileSync(getSessionCatalogIndexPath(dir), "utf8").trimEnd().split("\n");
		const entry = JSON.parse(lines[1]!);
		expect(entry.size).toBe(statSync(file).size);
		expect(entry.info.id).toBe("session-00");
	});

	it("drops deleted sessions from the index", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-prune-"));
		tempDirs.push(dir);
		const kept = createSession(dir, 0);
		const removed = createSession(dir, 1);
		await SessionManager.listAll(undefined, dir);
		rmSync(removed);

		await SessionManager.listAll(undefined, dir);

		const lines = readFileSync(getSessionCatalogIndexPath(dir), "utf8").trimEnd().split("\n");
		expect(lines.slice(1).map((line) => JSON.parse(line).file)).toEqual([basename(kept)]);
	});

	it("keeps the transcript corpus out of the metadata tier", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-tier1-"));
		tempDirs.push(dir);
		createSession(dir, 0, "hello from session 0", "deep transcript marker");

		await SessionManager.listAll(undefined, dir);

		const metadata = readFileSync(getSessionCatalogIndexPath(dir), "utf8");
		const corpus = readFileSync(getSessionSearchTextIndexPath(dir), "utf8");
		expect(metadata).not.toContain("allMessagesText");
		// The first message is metadata; everything after it belongs to the corpus tier.
		expect(metadata).not.toContain("deep transcript marker");
		expect(corpus).toContain("deep transcript marker");
	});

	it("skips the corpus tier when the caller does not want search text", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-metaonly-"));
		tempDirs.push(dir);
		// Metadata tier only: a corpus request here would have to rescan.
		writeIndex(dir, [createSession(dir, 0)], { searchText: false });

		const sessions = await SessionManager.listAll(undefined, dir, { searchText: false });

		expect(scan.starts).toEqual([]);
		expect(sessions[0]?.firstMessage).toBe("indexed 0");
		expect(sessions[0]?.allMessagesText).toBe("");
	});

	it("rescans for search text that the corpus tier no longer retains", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-index-corpusmiss-"));
		tempDirs.push(dir);
		const file = createSession(dir, 0, "hello from session 0");
		writeIndex(dir, [file], { searchText: false });

		const sessions = await SessionManager.listAll(undefined, dir);

		expect(scan.starts).toEqual([file]);
		expect(sessions[0]?.allMessagesText).toContain("hello from session 0");
	});

	it("caps the corpus tier at the retention limit", () => {
		expect(SESSION_SEARCH_TEXT_RETENTION).toBe(200);
	});

	it("serves readSearchText from the corpus tier and rescans stale entries", async () => {
		const dir = mkdtempSync(join(tmpdir(), "session-manager-readsearch-"));
		tempDirs.push(dir);
		const fresh = createSession(dir, 0, "hello from session 0");
		const stale = createSession(dir, 1, "hello from session 1");
		writeIndex(dir, [fresh]);
		// Same file list, but the stale entry claims a size the file no longer has.
		const staleLines = readFileSync(getSessionSearchTextIndexPath(dir), "utf8").trimEnd().split("\n");
		staleLines.push(JSON.stringify({ file: basename(stale), size: 1, mtimeMs: 1, searchText: "stale corpus" }));
		writeFileSync(getSessionSearchTextIndexPath(dir), `${staleLines.join("\n")}\n`);

		const corpora = await SessionManager.readSearchText([fresh, stale, join(dir, "missing.jsonl")]);

		expect(corpora.get(fresh)).toBe("corpus 0");
		expect(corpora.get(stale)).toContain("hello from session 1");
		expect(corpora.has(join(dir, "missing.jsonl"))).toBe(false);
	});
});
