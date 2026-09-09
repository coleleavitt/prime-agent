import { readFile, rename, unlink, writeFile } from "node:fs/promises";
import { basename, join } from "node:path";
import type { AgentStatus, SessionInfo, SessionState } from "./session-manager.js";
import type { SessionUsageSummary } from "./usage.js";

/**
 * Catalog listing used to reparse every changed JSONL in every process, so a
 * fresh CLI or a restarted daemon paid the full scan again. Session files are
 * append-only, so `(size, mtimeMs)` identifies content exactly; persisting the
 * derived metadata under that key turns a warm catalog into stat-only work.
 *
 * Stored as JSONL rather than one JSON document: a torn or partially written
 * line costs one session's metadata instead of the whole index.
 */
const SESSION_CATALOG_INDEX_VERSION = 2;

/**
 * The transcript corpus is ~14x larger than the metadata it accompanies and is
 * only needed once someone types a query, so it lives in a second file that is
 * read on demand. Retaining it for every session would make the index grow with
 * total transcript volume; retaining the newest slice keeps it bounded.
 */
export const SESSION_SEARCH_TEXT_RETENTION = 200;

export interface SessionCatalogIndexEntry {
	size: number;
	mtimeMs: number;
	/** `null` records a file that scanned to no session, so it is not rescanned. */
	info: SessionInfo | null;
}

/**
 * Lives inside the session directory. The `.ndjson` extension keeps it out of
 * the `.jsonl` catalog filter, so the index is never mistaken for a session.
 */
export function getSessionCatalogIndexPath(sessionDir: string): string {
	return join(sessionDir, "session-index.ndjson");
}

/** Companion corpus file for {@link getSessionCatalogIndexPath}. */
export function getSessionSearchTextIndexPath(sessionDir: string): string {
	return join(sessionDir, "session-search-index.ndjson");
}

interface SerializedSessionInfo {
	id: string;
	cwd: string;
	name?: string;
	state?: SessionState;
	parentSessionPath?: string;
	rlmDepth: number;
	created: string;
	modified: string;
	messageCount: number;
	firstMessage: string;
	agentStatus?: AgentStatus;
	usage?: SessionUsageSummary;
}

function serializeInfo(info: SessionInfo): SerializedSessionInfo {
	// `allMessagesText` is deliberately dropped here; it belongs to the corpus tier.
	const { path: _path, allMessagesText: _searchText, created, modified, ...rest } = info;
	return { ...rest, created: created.toISOString(), modified: modified.toISOString() };
}

function deserializeInfo(value: SerializedSessionInfo, path: string): SessionInfo | undefined {
	const created = new Date(value.created);
	const modified = new Date(value.modified);
	if (Number.isNaN(created.getTime()) || Number.isNaN(modified.getTime())) return undefined;
	if (typeof value.id !== "string" || typeof value.cwd !== "string") return undefined;
	// "" means "corpus not loaded", never "empty transcript".
	return { ...value, path, created, modified, allMessagesText: "" };
}

interface SerializedIndexLine {
	file: string;
	size: number;
	mtimeMs: number;
	info: SerializedSessionInfo | null;
}

/**
 * Read the persisted catalog for `sessionDir`, keyed by absolute session path.
 * A missing, unreadable, or outdated index simply yields no entries; callers
 * then fall back to scanning, so the index is a cache and never a source of
 * truth.
 */
export async function readSessionCatalogIndex(sessionDir: string): Promise<Map<string, SessionCatalogIndexEntry>> {
	const entries = new Map<string, SessionCatalogIndexEntry>();
	let contents: string;
	try {
		contents = await readFile(getSessionCatalogIndexPath(sessionDir), "utf8");
	} catch {
		return entries;
	}
	const lines = contents.split("\n");
	const header = parseLine(lines[0]);
	if (!header || (header as { version?: unknown }).version !== SESSION_CATALOG_INDEX_VERSION) {
		return entries;
	}
	for (const line of lines.slice(1)) {
		const parsed = parseLine(line) as SerializedIndexLine | undefined;
		if (!parsed || typeof parsed.file !== "string") continue;
		if (typeof parsed.size !== "number" || typeof parsed.mtimeMs !== "number") continue;
		const path = join(sessionDir, parsed.file);
		if (parsed.info === null) {
			entries.set(path, { size: parsed.size, mtimeMs: parsed.mtimeMs, info: null });
			continue;
		}
		const info = deserializeInfo(parsed.info, path);
		if (info) entries.set(path, { size: parsed.size, mtimeMs: parsed.mtimeMs, info });
	}
	return entries;
}

/**
 * Replace the persisted catalog atomically. Failures are swallowed: an index
 * that cannot be written only costs the next process a full scan.
 */
export async function writeSessionCatalogIndex(
	sessionDir: string,
	entries: ReadonlyMap<string, SessionCatalogIndexEntry>,
): Promise<void> {
	const path = getSessionCatalogIndexPath(sessionDir);
	const lines = [JSON.stringify({ version: SESSION_CATALOG_INDEX_VERSION })];
	for (const [sessionPath, entry] of entries) {
		const line: SerializedIndexLine = {
			file: basename(sessionPath),
			size: entry.size,
			mtimeMs: entry.mtimeMs,
			info: entry.info ? serializeInfo(entry.info) : null,
		};
		lines.push(JSON.stringify(line));
	}
	await writeIndexFile(path, lines);
}

/** Atomic and best-effort: a failed index write only costs the next scan. */
async function writeIndexFile(path: string, lines: readonly string[]): Promise<void> {
	const temporaryPath = `${path}.${process.pid}.tmp`;
	try {
		await writeFile(temporaryPath, `${lines.join("\n")}\n`);
		await rename(temporaryPath, path);
	} catch {
		await unlink(temporaryPath).catch(() => undefined);
	}
}

function parseLine(line: string | undefined): object | undefined {
	if (!line) return undefined;
	const trimmed = line.trim();
	if (!trimmed) return undefined;
	try {
		const parsed = JSON.parse(trimmed) as unknown;
		return typeof parsed === "object" && parsed !== null ? parsed : undefined;
	} catch {
		return undefined;
	}
}

export interface SessionSearchTextIndexEntry {
	size: number;
	mtimeMs: number;
	searchText: string;
}

/** Read the corpus tier, keyed by absolute session path. */
export async function readSessionSearchTextIndex(
	sessionDir: string,
): Promise<Map<string, SessionSearchTextIndexEntry>> {
	const entries = new Map<string, SessionSearchTextIndexEntry>();
	let contents: string;
	try {
		contents = await readFile(getSessionSearchTextIndexPath(sessionDir), "utf8");
	} catch {
		return entries;
	}
	const lines = contents.split("\n");
	const header = parseLine(lines[0]);
	if (!header || (header as { version?: unknown }).version !== SESSION_CATALOG_INDEX_VERSION) {
		return entries;
	}
	for (const line of lines.slice(1)) {
		const parsed = parseLine(line) as
			| { file?: unknown; size?: unknown; mtimeMs?: unknown; searchText?: unknown }
			| undefined;
		if (!parsed || typeof parsed.file !== "string") continue;
		if (typeof parsed.size !== "number" || typeof parsed.mtimeMs !== "number") continue;
		if (typeof parsed.searchText !== "string") continue;
		entries.set(join(sessionDir, parsed.file), {
			size: parsed.size,
			mtimeMs: parsed.mtimeMs,
			searchText: parsed.searchText,
		});
	}
	return entries;
}

/**
 * Persist the corpus tier for the {@link SESSION_SEARCH_TEXT_RETENTION} most
 * recently modified sessions. Older sessions stay searchable; their corpus is
 * simply rebuilt from the transcript when a query needs it.
 */
export async function writeSessionSearchTextIndex(
	sessionDir: string,
	entries: ReadonlyMap<string, SessionSearchTextIndexEntry>,
): Promise<void> {
	const retained = [...entries.entries()]
		.sort(([, a], [, b]) => b.mtimeMs - a.mtimeMs)
		.slice(0, SESSION_SEARCH_TEXT_RETENTION);
	const lines = [JSON.stringify({ version: SESSION_CATALOG_INDEX_VERSION })];
	for (const [sessionPath, entry] of retained) {
		lines.push(JSON.stringify({ file: basename(sessionPath), ...entry }));
	}
	await writeIndexFile(getSessionSearchTextIndexPath(sessionDir), lines);
}
