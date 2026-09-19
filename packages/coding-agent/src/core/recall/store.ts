import { randomBytes } from "node:crypto";
import { chmod, mkdir, open, readFile, rename, rm } from "node:fs/promises";
import { basename, dirname } from "node:path";
import { getLogger } from "@earendil-works/pi-ai";
import lockfile from "proper-lockfile";
import { getRecallMarkPath } from "../../config.js";
import {
	isBuildClaimCommand,
	isRecallClaim,
	mergeRecallClaims,
	RECALL_MAX_CLAIM_COMMAND_CHARS,
	type RecallClaim,
} from "./claims.js";
import {
	type AbsentSkipWorktree,
	absentSkipWorktreeDigest,
	captureWorkspace,
	RECALL_DIGEST_ALGORITHM,
	RECALL_MAX_DIRTY_PATHS,
	RECALL_MAX_TAGGED_PATHS,
	type WorkspaceSnapshot,
	type WorkspaceState,
	workspaceDigest,
} from "./mark.js";

/**
 * The durable half of Workspace Recall: one small JSON mark per repo under
 * `<agentDir>/recall/`, holding HEAD, digests, and at most RECALL_MAX_CLAIMS
 * build claims. It stores the invalidation, never the answer: no file
 * content, no command output. Writes are read-merge-write under an async
 * proper-lockfile lock, then temp file + rename.
 */

export const RECALL_MARK_SCHEMA = 1;
/** How long every process leaves a repo alone after one of its git calls timed out. */
export const RECALL_SKIP_TTL_MS = 10 * 60 * 1000;

const MARK_FILE_MODE = 0o600;
const MARK_DIR_MODE = 0o700;
const MARK_LOCK_STALE_MS = 10_000;
const MARK_LOCK_RETRIES = { retries: 12, factor: 1.5, minTimeout: 25, maxTimeout: 500 };
const SKIP_SCHEMA = 1;

const log = getLogger("coding-agent.workspace-recall");

export interface RecallMarkFile {
	schema: typeof RECALL_MARK_SCHEMA;
	digestAlgorithm: typeof RECALL_DIGEST_ALGORITHM;
	repoRoot: string;
	head: string | null;
	trackedTreeDigest: string;
	/** Repo-relative path to a digest or "unverifiable". */
	dirty: Record<string, string>;
	dirtyOverflow: number;
	/** Skip-worktree entries absent from disk; null when presence was not checked, missing in a mark from before it was recorded. */
	absentSkipWorktree?: AbsentSkipWorktree | null;
	claims: RecallClaim[];
	writtenAt: string;
}

export interface RecallClaimInput {
	command: string;
	exitCode: number;
	/** workspaceDigest() of the workspace the command ran against; defaults to the workspace captured by this write. */
	digestAtClaim?: string;
	/** ISO timestamp; defaults to the time of this write. */
	at?: string;
}

export interface WriteRecallMarkOptions {
	agentDir?: string;
	/** Only build commands that exited 0 are kept. */
	claims?: readonly RecallClaimInput[];
	now?: () => Date;
}

export interface WriteRecallMarkResult {
	markPath: string;
	repoKey: string;
	mark: RecallMarkFile;
	/** The mark this write replaced, if there was a readable one. */
	previous: RecallMarkFile | undefined;
	snapshot: WorkspaceSnapshot;
}

/** Why no mark was written. The last two are decided by the extension before a write is attempted. */
export type RecallSkipReason =
	| "git_unavailable"
	| "git_timeout"
	| "lock_busy"
	| "write_failed"
	| "not_repo"
	| "disabled"
	| "child_session";

export type WriteRecallMarkOutcome =
	| ({ ok: true } & WriteRecallMarkResult)
	| { ok: false; reason: RecallSkipReason; error?: string };

/** A repo whose git timed out, shared through the agent dir. A missed tool-path deadline is never stored here. */
export interface RecallSkipEntry {
	reason: "git_timeout";
	/** Epoch milliseconds. */
	until: number;
}

export function recallMarkPath(repoRoot: string, agentDir?: string): string {
	return getRecallMarkPath(repoRoot, agentDir);
}

/** `<basename>.<16 hex>`: the mark file name without its extension. */
export function recallRepoKey(repoRoot: string, agentDir?: string): string {
	return basename(recallMarkPath(repoRoot, agentDir), ".json");
}

export function recallSkipPath(repoRoot: string, agentDir?: string): string {
	return recallMarkPath(repoRoot, agentDir).replace(/\.json$/, ".skip.json");
}

export function markState(mark: RecallMarkFile): WorkspaceState {
	return {
		head: mark.head,
		trackedTreeDigest: mark.trackedTreeDigest,
		dirty: new Map(Object.entries(mark.dirty)),
		dirtyOverflow: mark.dirtyOverflow,
		absentSkipWorktree: mark.absentSkipWorktree,
	};
}

function isAbsentSkipWorktree(value: unknown): value is AbsentSkipWorktree | null | undefined {
	if (value === undefined || value === null) return true;
	if (typeof value !== "object" || Array.isArray(value)) return false;
	const { count, digest, paths } = value as Partial<AbsentSkipWorktree>;
	if (typeof count !== "number" || !Number.isInteger(count) || count < 0 || typeof digest !== "string") return false;
	if (paths === undefined) return count > RECALL_MAX_TAGGED_PATHS;
	return (
		Array.isArray(paths) &&
		paths.length === count &&
		paths.every((path) => typeof path === "string") &&
		absentSkipWorktreeDigest(paths) === digest
	);
}

function isRecallMarkFile(value: unknown): value is RecallMarkFile {
	if (typeof value !== "object" || value === null) return false;
	const mark = value as Partial<RecallMarkFile>;
	return (
		mark.schema === RECALL_MARK_SCHEMA &&
		mark.digestAlgorithm === RECALL_DIGEST_ALGORITHM &&
		typeof mark.repoRoot === "string" &&
		(mark.head === null || typeof mark.head === "string") &&
		typeof mark.trackedTreeDigest === "string" &&
		typeof mark.dirty === "object" &&
		mark.dirty !== null &&
		!Array.isArray(mark.dirty) &&
		Object.values(mark.dirty).every((digest) => typeof digest === "string") &&
		typeof mark.dirtyOverflow === "number" &&
		Number.isInteger(mark.dirtyOverflow) &&
		mark.dirtyOverflow >= 0 &&
		isAbsentSkipWorktree(mark.absentSkipWorktree) &&
		Array.isArray(mark.claims) &&
		typeof mark.writtenAt === "string"
	);
}

/** The mark for `repoRoot`, or undefined when there is none or it cannot be trusted as one. */
export async function readRecallMark(repoRoot: string, agentDir?: string): Promise<RecallMarkFile | undefined> {
	const markPath = recallMarkPath(repoRoot, agentDir);
	let parsed: unknown;
	try {
		parsed = JSON.parse(await readFile(markPath, "utf8"));
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
			log.debug("recall mark is unreadable; ignoring it", { markPath, error: errorMessage(error) });
		}
		return undefined;
	}
	if (!isRecallMarkFile(parsed) || parsed.repoRoot !== repoRoot) {
		log.debug("recall mark has an unexpected shape; ignoring it", { markPath });
		return undefined;
	}
	return { ...parsed, claims: parsed.claims.filter(isRecallClaim) };
}

function claimTimestamp(at: string | undefined, fallback: Date): string {
	return at !== undefined && Number.isFinite(Date.parse(at)) ? at : fallback.toISOString();
}

/**
 * Capture the workspace and write it as the repo's mark, carrying the
 * previous mark's claims forward and adding `options.claims`. A previous mark
 * from before absent skip-worktree paths were recorded carries none: its
 * claims were digested without that field and can never be CURRENT again.
 * Never rejects: every reason no mark was written comes back as
 * `{ ok: false, reason }`.
 */
export async function writeRecallMark(
	repoRoot: string,
	options: WriteRecallMarkOptions = {},
): Promise<WriteRecallMarkOutcome> {
	const markPath = recallMarkPath(repoRoot, options.agentDir);
	try {
		await mkdir(dirname(markPath), { recursive: true, mode: MARK_DIR_MODE });
		await chmod(dirname(markPath), MARK_DIR_MODE);
	} catch (error) {
		log.debug("recall mark directory is not writable; mark not written", { markPath, error: errorMessage(error) });
		return { ok: false, reason: "write_failed", error: errorMessage(error) };
	}
	let release: (() => Promise<void>) | undefined;
	let compromised: Error | undefined;
	try {
		release = await lockfile.lock(markPath, {
			realpath: false,
			lockfilePath: `${markPath}.lock`,
			stale: MARK_LOCK_STALE_MS,
			retries: MARK_LOCK_RETRIES,
			onCompromised: (error) => {
				compromised = error;
			},
		});
	} catch (error) {
		log.debug("recall mark lock unavailable; mark not written", { markPath, error: errorMessage(error) });
		return (error as NodeJS.ErrnoException).code === "ELOCKED"
			? { ok: false, reason: "lock_busy" }
			: { ok: false, reason: "write_failed", error: errorMessage(error) };
	}
	try {
		// Captured under the lock so two sessions can never land an older snapshot over a newer one.
		const captured = await captureWorkspace(repoRoot, { agentDir: options.agentDir });
		if (!captured.ok) {
			return { ok: false, reason: captured.failure === "aborted" ? "git_unavailable" : captured.failure };
		}
		const { snapshot } = captured;
		const previous = await readRecallMark(repoRoot, options.agentDir);
		const now = (options.now ?? (() => new Date()))();
		const digest = workspaceDigest(snapshot);
		const added: RecallClaim[] = (options.claims ?? [])
			.filter((claim) => claim.exitCode === 0 && isBuildClaimCommand(claim.command))
			.map((claim) => ({
				command: claim.command.trim().slice(0, RECALL_MAX_CLAIM_COMMAND_CHARS),
				exitCode: claim.exitCode,
				at: claimTimestamp(claim.at, now),
				digestAtClaim: claim.digestAtClaim ?? digest,
			}));
		const mark: RecallMarkFile = {
			schema: RECALL_MARK_SCHEMA,
			digestAlgorithm: RECALL_DIGEST_ALGORITHM,
			repoRoot,
			head: snapshot.head,
			trackedTreeDigest: snapshot.trackedTreeDigest,
			dirty: Object.fromEntries([...snapshot.dirty.entries()].slice(0, RECALL_MAX_DIRTY_PATHS)),
			dirtyOverflow: snapshot.dirtyOverflow,
			absentSkipWorktree: snapshot.absentSkipWorktree,
			claims: mergeRecallClaims(previous?.absentSkipWorktree === undefined ? [] : previous.claims, added),
			writtenAt: now.toISOString(),
		};
		if (compromised) {
			log.debug("recall mark lock was compromised; mark not written", { markPath, error: compromised.message });
			return { ok: false, reason: "lock_busy" };
		}
		await writeJsonAtomically(markPath, mark);
		return { ok: true, markPath, repoKey: basename(markPath, ".json"), mark, previous, snapshot };
	} catch (error) {
		log.debug("recall mark write failed", { markPath, error: errorMessage(error) });
		return { ok: false, reason: "write_failed", error: errorMessage(error) };
	} finally {
		await release?.().catch(() => undefined);
	}
}

/** The unexpired git-timeout entry for `repoRoot`, shared by every process using this agent dir. */
export async function readRecallSkip(
	repoRoot: string,
	agentDir?: string,
	now: number = Date.now(),
): Promise<RecallSkipEntry | undefined> {
	let parsed: unknown;
	try {
		parsed = JSON.parse(await readFile(recallSkipPath(repoRoot, agentDir), "utf8"));
	} catch {
		return undefined;
	}
	if (typeof parsed !== "object" || parsed === null) return undefined;
	const { schema, reason, until } = parsed as { schema?: unknown; reason?: unknown; until?: unknown };
	if (schema !== SKIP_SCHEMA || reason !== "git_timeout" || typeof until !== "string") return undefined;
	const untilMs = Date.parse(until);
	// An entry reaching further out than one TTL did not come from this clock; ignore it.
	if (!Number.isFinite(untilMs) || untilMs <= now || untilMs > now + RECALL_SKIP_TTL_MS) return undefined;
	return { reason, until: untilMs };
}

/** Leave `repoRoot` alone for RECALL_SKIP_TTL_MS after a git timeout. Best effort: the entry is returned even if it could not be stored. */
export async function writeRecallSkip(
	repoRoot: string,
	agentDir: string | undefined,
	now: number = Date.now(),
): Promise<RecallSkipEntry> {
	const entry: RecallSkipEntry = { reason: "git_timeout", until: now + RECALL_SKIP_TTL_MS };
	const skipPath = recallSkipPath(repoRoot, agentDir);
	try {
		await mkdir(dirname(skipPath), { recursive: true, mode: MARK_DIR_MODE });
		await writeJsonAtomically(skipPath, {
			schema: SKIP_SCHEMA,
			reason: entry.reason,
			until: new Date(entry.until).toISOString(),
		});
	} catch (error) {
		log.debug("recall skip entry write failed", { skipPath, error: errorMessage(error) });
	}
	return entry;
}

async function writeJsonAtomically(path: string, value: unknown): Promise<void> {
	const temp = `${path}.${process.pid}.${randomBytes(4).toString("hex")}.tmp`;
	try {
		const handle = await open(temp, "wx", MARK_FILE_MODE);
		try {
			await handle.writeFile(`${JSON.stringify(value, null, 2)}\n`);
			await handle.sync();
		} finally {
			await handle.close();
		}
		await rename(temp, path);
	} catch (error) {
		await rm(temp, { force: true }).catch(() => undefined);
		throw error;
	}
	// open()'s mode is masked by the umask, so pin it after the rename.
	await chmod(path, MARK_FILE_MODE);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
