import { randomBytes } from "node:crypto";
import {
	chmodSync,
	closeSync,
	fsyncSync,
	mkdirSync,
	openSync,
	readFileSync,
	renameSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { dirname } from "node:path";
import { getLogger } from "@earendil-works/pi-ai";
import { lockSync } from "proper-lockfile";
import { getResolutionStorePath } from "../../config.js";
import { findGitPaths } from "../../utils/git.js";
import {
	type FailureFingerprint,
	failureOpponentId,
	fingerprintFailure,
	parsePythonTraceback,
} from "../ravo/failure-ledger.js";

/**
 * Resolution index: joins a failure fingerprint observed in a Python cell to
 * the cell that subsequently stopped it happening, so that when the same
 * fingerprint recurs the tool result carries the fix instead of leaving the
 * model to rediscover it.
 *
 * 74 of 96 fingerprint sources in the local corpus are `ipython`, and the two
 * largest classes ("object has no attribute", "module has no attribute") are
 * the model guessing an API that does not exist — a retrieval failure, not a
 * reasoning one. It reuses the failure ledger's fingerprinting unchanged so an
 * id here is the same id there.
 *
 * Recurrence is mostly a cross-session event: across 689 session ledgers on one
 * machine there were 308 second-or-later hits inside a single session against
 * 1 122 first hits of a fingerprint an earlier session had already hit, so a
 * purely in-memory index can see 21.5% of what recurs. Live records are
 * therefore mirrored to a per-repo file under the agent dir, keyed by the same
 * fingerprint id, and consulted when this session has not seen the failure
 * before. A hint is advice, not a commit, so nothing here is gated; every
 * failure path degrades to the session-only behaviour.
 */

const log = getLogger("coding-agent.resolution-index");

/** One executed cell: its source, the text the model sees back, and whether it failed. */
export interface ResolutionCell {
	code: string;
	/** Tool result text (stdout + stderr + result + traceback), before any annotation. */
	output: string;
	/** True when the cell errored or was aborted. */
	isError: boolean;
}

export interface ResolutionRecord {
	fingerprintId: string;
	exceptionClass?: string;
	/** Source of the cell that ran clean after the failure. */
	fix: string;
	/** Source of the cell that produced the failure. */
	failed: string;
	failedAtCell: number;
	fixedAtCell: number;
	/** Epoch ms the resolution was recorded; orders eviction in the shared per-repo store. */
	recordedAt?: number;
}

/** Whether a hint came from this session's own history or from the per-repo store. */
export type ResolutionOrigin = "session" | "store";

export interface ResolutionHint {
	fingerprintId: string;
	record: ResolutionRecord;
	origin: ResolutionOrigin;
	/** Block to append to the tool result. */
	text: string;
}

export interface ResolutionIndexOptions {
	/** Fingerprint source, matching the tool name the failure ledger records. */
	source?: string;
	/** How many cells later a clean cell may still count as the fix. */
	window?: number;
	/** Cap on retained resolutions; the least recently recorded is evicted first. */
	maxRecords?: number;
	/** Cell source is clipped to this many characters before it is retained. */
	maxCellChars?: number;
	/** Durable per-repo store, consulted when this session has not seen the fingerprint. Omit for session-only. */
	store?: ResolutionStore;
}

export const DEFAULT_RESOLUTION_SOURCE = "ipython";
export const DEFAULT_RESOLUTION_WINDOW = 6;
export const DEFAULT_MAX_RESOLUTIONS = 64;
export const DEFAULT_MAX_CELL_CHARS = 1200;

const IDENTIFIER = /[A-Za-z_][A-Za-z0-9_]*/g;

/**
 * Python keywords plus the builtins common enough that sharing one says nothing
 * about two cells being about the same thing.
 */
const COMMON_TOKENS = new Set(
	`abs all and any as assert async await bool break bytes callable class cls continue def del dict dir elif else
	 enumerate except false filter finally float for format from getattr global hasattr id if import in input int is
	 isinstance iter lambda len list map max min next none nonlocal not object open or pass print raise range repr
	 return round self set setattr sorted str sum super true try tuple type vars while with yield zip`.split(/\s+/),
);

interface PendingFailure {
	fingerprint: FailureFingerprint;
	code: string;
	tokens: Set<string>;
	cell: number;
}

export class ResolutionIndex {
	private readonly pending = new Map<string, PendingFailure>();
	private readonly resolutions = new Map<string, ResolutionRecord>();
	private readonly source: string;
	private readonly window: number;
	private readonly maxRecords: number;
	private readonly maxCellChars: number;
	private readonly store?: ResolutionStore;
	private cells = 0;

	constructor(options: ResolutionIndexOptions = {}) {
		this.source = options.source ?? DEFAULT_RESOLUTION_SOURCE;
		this.window = Math.max(1, options.window ?? DEFAULT_RESOLUTION_WINDOW);
		this.maxRecords = Math.max(1, options.maxRecords ?? DEFAULT_MAX_RESOLUTIONS);
		this.maxCellChars = Math.max(1, options.maxCellChars ?? DEFAULT_MAX_CELL_CHARS);
		this.store = options.store;
	}

	/** The resolution recorded for a fingerprint, by this session or by an earlier one. */
	lookup(fingerprintId: string): ResolutionRecord | undefined {
		return this.resolutions.get(fingerprintId) ?? this.stored(fingerprintId);
	}

	/** Every resolution this session recorded, least recently recorded first. */
	records(): ResolutionRecord[] {
		return [...this.resolutions.values()];
	}

	/** Every resolution the per-repo store holds for this repo, oldest first. */
	durableRecords(): ResolutionRecord[] {
		return [...(this.store?.load() ?? [])];
	}

	/** Fingerprints seen failing that no later cell has resolved yet. */
	unresolved(): string[] {
		return [...this.pending.keys()];
	}

	/**
	 * Record one executed cell and return the hint to append to its result when
	 * the cell reproduced a fingerprint this session has already resolved once.
	 */
	observe(cell: ResolutionCell): ResolutionHint | undefined {
		const index = this.cells++;
		const fingerprint = this.fingerprintOf(cell);
		const hint = fingerprint ? this.hintFor(fingerprint, cell.code) : undefined;
		this.settlePending(cell, index, fingerprint?.id);
		if (fingerprint) {
			this.pending.set(fingerprint.id, {
				fingerprint,
				code: clip(cell.code, this.maxCellChars),
				tokens: significantTokens(cell.code),
				cell: index,
			});
		}
		return hint;
	}

	private hintFor(fingerprint: FailureFingerprint, code: string): ResolutionHint | undefined {
		const session = this.resolutions.get(fingerprint.id);
		const record = session ?? this.stored(fingerprint.id);
		if (!record) return undefined;
		// The recorded fix is the cell that just failed, so it never was one. A
		// stale durable fix has to go too, or every later session repeats it.
		if (record.fix === clip(code, this.maxCellChars)) {
			this.resolutions.delete(fingerprint.id);
			this.store?.forget(fingerprint.id);
			return undefined;
		}
		const origin: ResolutionOrigin = session ? "session" : "store";
		return { fingerprintId: fingerprint.id, record, origin, text: formatResolutionHint(record, origin) };
	}

	private stored(fingerprintId: string): ResolutionRecord | undefined {
		if (!this.store) return undefined;
		return this.store.load().find((record) => record.fingerprintId === fingerprintId);
	}

	/**
	 * A pending failure is resolved by the next cell that does not reproduce it,
	 * ran clean, lands inside the window, and shares an identifier with the cell
	 * that failed. The last condition is not in the definition of "the next cell
	 * that does not reproduce it", but without it every unrelated cell resolves
	 * every open failure and the hint is noise.
	 */
	private settlePending(cell: ResolutionCell, index: number, reproducedId: string | undefined): void {
		const tokens = cell.isError ? undefined : significantTokens(cell.code);
		for (const [id, failure] of [...this.pending]) {
			if (id === reproducedId) continue;
			if (index - failure.cell > this.window) {
				this.pending.delete(id);
				continue;
			}
			if (!tokens || !sharesToken(failure.tokens, tokens)) continue;
			this.pending.delete(id);
			// Re-running the same cell clean resolves the failure but teaches nothing.
			if (clip(cell.code, this.maxCellChars) === failure.code) continue;
			this.record({
				fingerprintId: id,
				exceptionClass: failure.fingerprint.exceptionClass,
				fix: clip(cell.code, this.maxCellChars),
				failed: failure.code,
				failedAtCell: failure.cell,
				fixedAtCell: index,
			});
		}
	}

	private record(record: ResolutionRecord): void {
		const stamped: ResolutionRecord = { ...record, recordedAt: record.recordedAt ?? Date.now() };
		this.resolutions.delete(stamped.fingerprintId);
		this.resolutions.set(stamped.fingerprintId, stamped);
		while (this.resolutions.size > this.maxRecords) {
			const oldest = this.resolutions.keys().next();
			if (oldest.done) break;
			this.resolutions.delete(oldest.value);
		}
		this.store?.save(stamped);
	}

	/** Same shape as the failure ledger's tool-result observation, so the ids match. */
	private fingerprintOf(cell: ResolutionCell): FailureFingerprint | undefined {
		const traceback = parsePythonTraceback(cell.output);
		if (traceback) {
			return fingerprintFailure(
				"python_exception",
				traceback.skillName ?? this.source,
				traceback.exceptionClass,
				traceback.message,
			);
		}
		if (!cell.isError) return undefined;
		const raw = cell.output.trim() || "tool returned an error without output";
		return fingerprintFailure("tool_error", this.source, undefined, raw);
	}
}

export function formatResolutionHint(record: ResolutionRecord, origin: ResolutionOrigin = "session"): string {
	const label = record.exceptionClass
		? `${failureOpponentId(record.fingerprintId)}, ${record.exceptionClass}`
		: failureOpponentId(record.fingerprintId);
	const where = origin === "store" ? "an earlier session" : "this session";
	return [
		"<ipython_resolution_hint>",
		"You hit this before; this fixed it:",
		"```python",
		record.fix,
		"```",
		`(${label}; first hit at cell ${record.failedAtCell + 1}, fixed at cell ${record.fixedAtCell + 1} of ${where})`,
		"</ipython_resolution_hint>",
	].join("\n");
}

function significantTokens(code: string): Set<string> {
	const tokens = new Set<string>();
	for (const match of code.matchAll(IDENTIFIER)) {
		const token = match[0];
		if (token.length < 2 || COMMON_TOKENS.has(token.toLowerCase())) continue;
		tokens.add(token);
	}
	return tokens;
}

function sharesToken(a: ReadonlySet<string>, b: ReadonlySet<string>): boolean {
	for (const token of a) {
		if (b.has(token)) return true;
	}
	return false;
}

function clip(text: string, max: number): string {
	const trimmed = text.trim();
	return trimmed.length > max ? `${trimmed.slice(0, max)}\n# ... truncated` : trimmed;
}

/** Durable half of the index: the resolutions one repo has accumulated across sessions. */
export interface ResolutionStore {
	/** Records held for this repo, oldest first. Empty whenever the store cannot be read. */
	load(): ResolutionRecord[];
	/** Merge one record into the store, replacing any earlier record for the same fingerprint. */
	save(record: ResolutionRecord): void;
	/** Drop a record whose fix has stopped working. */
	forget(fingerprintId: string): void;
}

const STORE_VERSION = 1;
/**
 * A record's `fix` is up to DEFAULT_MAX_CELL_CHARS of verbatim cell source, so
 * the file can hold anything the model typed into a cell — an inline token, a
 * connection string, a path into someone's home directory. Those values are not
 * key-shaped, so the RAVO archive's `assertNoSecrets` screen would not catch
 * them. Owner-only, local to this machine, and never exported anywhere.
 */
const STORE_FILE_MODE = 0o600;
const STORE_DIR_MODE = 0o700;
const STORE_LOCK_ATTEMPTS = 40;
const STORE_LOCK_STALE_MS = 10_000;

interface ResolutionStoreFile {
	version: number;
	repo: string;
	records: ResolutionRecord[];
}

/**
 * The store for the repo containing `cwd`, or undefined outside a git
 * worktree — there is no stable key for "this project" then, and a global file
 * would hand one project's fixes to an unrelated one.
 */
export function openResolutionStore(cwd: string, agentDir?: string): ResolutionStore | undefined {
	const paths = findGitPaths(cwd);
	if (!paths) {
		log.debug("resolution store disabled: not inside a git repository", { cwd });
		return undefined;
	}
	try {
		return new FileResolutionStore(getResolutionStorePath(paths.repoDir, agentDir), paths.repoDir);
	} catch (error) {
		log.warn("resolution store unavailable; hints stay session-local", {
			cwd,
			error: error instanceof Error ? error.message : String(error),
		});
		return undefined;
	}
}

/**
 * One JSON file per repo, holding at most DEFAULT_MAX_RESOLUTIONS records of at
 * most DEFAULT_MAX_CELL_CHARS per cell — roughly 160KB at the bound. Writes are
 * temp file + fsync + rename under a cross-process lock, because several
 * sessions share one repo and a read-modify-write without one silently drops
 * the other side's records. Every failure degrades to the session-only index.
 */
class FileResolutionStore implements ResolutionStore {
	private cached?: { mtimeMs: number; size: number; records: ResolutionRecord[] };

	constructor(
		private readonly path: string,
		private readonly repoDir: string,
	) {}

	load(): ResolutionRecord[] {
		const stat = this.statStore();
		if (!stat) return [];
		if (this.cached && this.cached.mtimeMs === stat.mtimeMs && this.cached.size === stat.size) {
			return this.cached.records;
		}
		const records = this.read();
		this.cached = { mtimeMs: stat.mtimeMs, size: stat.size, records };
		return records;
	}

	save(record: ResolutionRecord): void {
		this.mutate((records) => [...records.filter((held) => held.fingerprintId !== record.fingerprintId), record]);
	}

	forget(fingerprintId: string): void {
		this.mutate((records) => {
			const kept = records.filter((held) => held.fingerprintId !== fingerprintId);
			return kept.length === records.length ? undefined : kept;
		});
	}

	private statStore(): { mtimeMs: number; size: number } | undefined {
		try {
			const stat = statSync(this.path, { throwIfNoEntry: false });
			return stat ? { mtimeMs: stat.mtimeMs, size: stat.size } : undefined;
		} catch (error) {
			this.warn("resolution store cannot be stat'd", error);
			return undefined;
		}
	}

	/** Parse the file, treating anything unreadable or malformed as an empty store. */
	private read(): ResolutionRecord[] {
		try {
			const parsed: unknown = JSON.parse(readFileSync(this.path, "utf-8"));
			if (!isStoreFile(parsed)) {
				this.warn("resolution store has an unexpected shape; ignoring it", undefined);
				return [];
			}
			return parsed.records.filter(isResolutionRecord);
		} catch (error) {
			// The first write of a repo's store reads a file that is not there yet.
			if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
				this.warn("resolution store is unreadable; ignoring it", error);
			}
			return [];
		}
	}

	private mutate(update: (records: readonly ResolutionRecord[]) => ResolutionRecord[] | undefined): void {
		let release: (() => void) | undefined;
		try {
			mkdirSync(dirname(this.path), { recursive: true, mode: STORE_DIR_MODE });
			chmodSync(dirname(this.path), STORE_DIR_MODE);
			release = acquireStoreLock(this.path);
			if (!release) {
				log.warn("resolution store is locked by another session; hint not persisted", { path: this.path });
				return;
			}
			// Re-read under the lock: the cache may predate another session's write.
			const next = update(this.read());
			if (!next) return;
			this.write(bound(next));
		} catch (error) {
			this.warn("resolution store write failed; hint stays session-local", error);
		} finally {
			try {
				release?.();
			} catch {
				// The lock goes stale on its own; a failed release must not break the cell.
			}
		}
	}

	private write(records: readonly ResolutionRecord[]): void {
		const file: ResolutionStoreFile = { version: STORE_VERSION, repo: this.repoDir, records: [...records] };
		const temp = `${this.path}.${process.pid}.${randomBytes(4).toString("hex")}.tmp`;
		try {
			const fd = openSync(temp, "wx", STORE_FILE_MODE);
			try {
				writeFileSync(fd, `${JSON.stringify(file, null, 2)}\n`);
				fsyncSync(fd);
			} finally {
				closeSync(fd);
			}
			renameSync(temp, this.path);
		} catch (error) {
			rmSync(temp, { force: true });
			throw error;
		}
		// openSync's mode is masked by the umask, so pin it after the rename.
		chmodSync(this.path, STORE_FILE_MODE);
		this.cached = undefined;
	}

	private warn(message: string, error: unknown): void {
		log.warn(message, {
			path: this.path,
			...(error === undefined ? {} : { error: error instanceof Error ? error.message : String(error) }),
		});
	}
}

/**
 * `proper-lockfile` implements the lock as a DIRECTORY (mkdir is the atomic
 * primitive), so nothing here creates the store file to take it; `realpath:
 * false` lets the lock precede the file's first write.
 */
function acquireStoreLock(path: string): (() => void) | undefined {
	const wait = new Int32Array(new SharedArrayBuffer(4));
	for (let attempt = 0; attempt < STORE_LOCK_ATTEMPTS; attempt++) {
		try {
			return lockSync(path, { realpath: false, lockfilePath: `${path}.lock`, stale: STORE_LOCK_STALE_MS });
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ELOCKED") throw error;
			Atomics.wait(wait, 0, 0, 5);
		}
	}
	return undefined;
}

/** Keep the file bounded whatever the writer's own limits were: newest DEFAULT_MAX_RESOLUTIONS records, clipped cells. */
function bound(records: readonly ResolutionRecord[]): ResolutionRecord[] {
	return [...records]
		.sort((a, b) => (a.recordedAt ?? 0) - (b.recordedAt ?? 0))
		.slice(-DEFAULT_MAX_RESOLUTIONS)
		.map((record) => ({
			...record,
			fix: clip(record.fix, DEFAULT_MAX_CELL_CHARS),
			failed: clip(record.failed, DEFAULT_MAX_CELL_CHARS),
		}));
}

function isStoreFile(value: unknown): value is ResolutionStoreFile {
	if (typeof value !== "object" || value === null) return false;
	const file = value as Partial<ResolutionStoreFile>;
	return file.version === STORE_VERSION && Array.isArray(file.records);
}

function isResolutionRecord(value: unknown): value is ResolutionRecord {
	if (typeof value !== "object" || value === null) return false;
	const record = value as Partial<ResolutionRecord>;
	return (
		typeof record.fingerprintId === "string" &&
		typeof record.fix === "string" &&
		typeof record.failed === "string" &&
		typeof record.failedAtCell === "number" &&
		typeof record.fixedAtCell === "number"
	);
}
