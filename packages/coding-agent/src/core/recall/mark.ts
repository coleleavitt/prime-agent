import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { lstat, open, readlink, realpath } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { getLogger } from "@earendil-works/pi-ai";
import { findGitPaths } from "../../utils/git.js";

/**
 * Workspace capture for Workspace Recall: HEAD, a digest of the index, a
 * digest per dirty path, and the skip-worktree paths absent from disk.
 * Nothing here keeps file content; every value that leaves this module is a
 * digest or a path.
 *
 * The rsi plan names blake2b-128. Node's crypto has no 128-bit blake2b, so the
 * digest is sha256 truncated to 128 bits and the mark file names it
 * "sha256-128" rather than claiming an algorithm it does not use.
 */
export const RECALL_DIGEST_ALGORITHM = "sha256-128";
export const RECALL_UNVERIFIABLE = "unverifiable";
export const RECALL_MAX_DIRTY_PATHS = 200;
export const RECALL_MAX_FILE_BYTES = 8 * 1024 * 1024;
export const RECALL_MAX_HASHED_BYTES = 64 * 1024 * 1024;
export const RECALL_GIT_TIMEOUT_MS = 3000;
/** More skip-worktree entries than this are not checked for presence on disk, so none of them is hashed. */
export const RECALL_MAX_SKIP_WORKTREE_CHECKS = 1000;
/** More skip-worktree and assume-unchanged paths than this (core.ignoreStat, sparse checkout) are counted, never hashed. */
export const RECALL_MAX_TAGGED_PATHS = 100;

const GIT_MAX_BUFFER_BYTES = 64 * 1024 * 1024;
const LSTAT_BATCH_SIZE = 64;
/** Environment that would point git somewhere other than the directory findGitPaths resolved, or change pathspec matching. */
const GIT_OVERRIDE_ENV = [
	"GIT_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_COMMON_DIR",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
	"GIT_NAMESPACE",
	"GIT_PREFIX",
	"GIT_LITERAL_PATHSPECS",
	"GIT_GLOB_PATHSPECS",
	"GIT_NOGLOB_PATHSPECS",
	"GIT_ICASE_PATHSPECS",
];

const log = getLogger("coding-agent.workspace-recall");

export function recallDigest(data: string | Uint8Array): string {
	return createHash("sha256").update(data).digest("hex").slice(0, 32);
}

/** Digest of "nothing at this path", so a deletion recorded at mark time compares equal to the same deletion later. */
export const RECALL_ABSENT_DIGEST = recallDigest("\0prime-agent-recall:absent");

/** Skip-worktree entries missing on disk, such as a sparse checkout's excluded paths. Names only. */
export interface AbsentSkipWorktree {
	count: number;
	/** absentSkipWorktreeDigest() of every name, listed or not. */
	digest: string;
	/** Sorted, while there are at most RECALL_MAX_TAGGED_PATHS; past that they are counted in dirtyOverflow instead. */
	paths?: string[];
}

export interface WorkspaceState {
	head: string | null;
	trackedTreeDigest: string;
	/** Repo-relative path to a content digest, RECALL_ABSENT_DIGEST, or RECALL_UNVERIFIABLE. */
	dirty: ReadonlyMap<string, string>;
	/**
	 * Paths that needed a digest but were neither hashed nor recorded: dirty
	 * paths beyond RECALL_MAX_DIRTY_PATHS, plus every tagged path once the
	 * tagged set is over RECALL_MAX_TAGGED_PATHS, plus every absent
	 * skip-worktree entry once those are over it.
	 */
	dirtyOverflow: number;
	/**
	 * Null when presence was not checked (more than
	 * RECALL_MAX_SKIP_WORKTREE_CHECKS entries, all counted in dirtyOverflow).
	 * Undefined only in a mark written before presence was recorded, where it
	 * is unknown.
	 */
	absentSkipWorktree?: AbsentSkipWorktree | null;
}

export interface WorkspaceSnapshot extends WorkspaceState {
	repoRoot: string;
	absentSkipWorktree: AbsentSkipWorktree | null;
	/** Paths in the index now. Counted, never persisted. */
	trackedPaths: ReadonlySet<string>;
	/** Names of the dirty paths beyond RECALL_MAX_DIRTY_PATHS, unverifiable by construction. Never persisted. */
	overflowPaths: readonly string[];
	/** Skip-worktree and assume-unchanged paths that were hashed. Never persisted. */
	taggedPaths: ReadonlySet<string>;
	/**
	 * Tagged paths left unhashed, and absent skip-worktree entries left
	 * unlisted, because there were too many; counted in dirtyOverflow. Never
	 * persisted.
	 */
	unhashedTaggedPaths: readonly string[];
	/** Repo-relative agent dir left out of the capture, so recall's own writes are never workspace changes. */
	excludedPath?: string;
}

export interface StatusEntry {
	xy: string;
	path: string;
	origPath?: string;
}

export type RecallGitFailure = "git_unavailable" | "git_timeout" | "aborted";
export type RecallGitResult = { ok: true; stdout: Buffer } | { ok: false; failure: RecallGitFailure };

export interface RecallGitOptions {
	timeoutMs?: number;
	/** Aborting kills the git process. */
	signal?: AbortSignal;
}

export type RecallCaptureFailure = RecallGitFailure | "not_repo";
export type RecallCaptureResult =
	| { ok: true; snapshot: WorkspaceSnapshot }
	| { ok: false; failure: RecallCaptureFailure };

export interface CaptureWorkspaceOptions {
	/** Excluded from the capture when it lives inside the worktree. */
	agentDir?: string;
	signal?: AbortSignal;
}

function compareStrings(a: string, b: string): number {
	return a < b ? -1 : a > b ? 1 : 0;
}

export function absentSkipWorktreeDigest(paths: readonly string[]): string {
	return recallDigest(JSON.stringify(paths));
}

/**
 * One digest over everything a claim depends on; equal digests of fully
 * verifiable states mean an identical tracked and non-ignored tree,
 * skip-worktree and assume-unchanged entries included, with the same
 * skip-worktree entries absent from disk.
 */
export function workspaceDigest(state: WorkspaceState): string {
	const dirty = [...state.dirty.entries()].sort(([a], [b]) => compareStrings(a, b));
	const absent = state.absentSkipWorktree;
	return recallDigest(
		JSON.stringify({
			head: state.head,
			trackedTreeDigest: state.trackedTreeDigest,
			dirty,
			dirtyOverflow: state.dirtyOverflow,
			// Left out for a mark from before presence was recorded, so it keeps the digest its claims were made with.
			absentSkipWorktree: absent && { count: absent.count, digest: absent.digest },
		}),
	);
}

/**
 * True when the snapshot's digests cover every path they claim to: nothing
 * unverifiable, nothing past the limit, and every absent skip-worktree entry
 * named.
 */
export function isFullyVerifiable(state: WorkspaceState): boolean {
	if (state.dirtyOverflow > 0 || state.absentSkipWorktree?.paths === undefined) return false;
	for (const digest of state.dirty.values()) {
		if (digest === RECALL_UNVERIFIABLE) return false;
	}
	return true;
}

/** The repo toplevel for `cwd`, or undefined outside a git worktree. Filesystem walk only; no git process. */
export function findRecallRepo(cwd: string): string | undefined {
	try {
		return findGitPaths(cwd)?.repoDir;
	} catch {
		return undefined;
	}
}

function gitEnv(): NodeJS.ProcessEnv {
	const env: NodeJS.ProcessEnv = { ...process.env, GIT_OPTIONAL_LOCKS: "0", GIT_TERMINAL_PROMPT: "0" };
	for (const name of GIT_OVERRIDE_ENV) delete env[name];
	return env;
}

export function runGit(
	repoRoot: string,
	args: readonly string[],
	options: RecallGitOptions = {},
): Promise<RecallGitResult> {
	const { timeoutMs = RECALL_GIT_TIMEOUT_MS, signal } = options;
	if (signal?.aborted) return Promise.resolve({ ok: false, failure: "aborted" });
	return new Promise((resolvePromise) => {
		try {
			execFile(
				"git",
				["--no-optional-locks", ...args],
				{
					cwd: repoRoot,
					encoding: "buffer",
					timeout: timeoutMs,
					maxBuffer: GIT_MAX_BUFFER_BYTES,
					windowsHide: true,
					env: gitEnv(),
					signal,
				},
				(error, stdout) => {
					if (!error) {
						resolvePromise({ ok: true, stdout });
						return;
					}
					// `killed` means execFile itself killed git: the timeout, since maxBuffer and abort are told apart first.
					const failure: RecallGitFailure = signal?.aborted
						? "aborted"
						: error.killed && error.code !== "ERR_CHILD_PROCESS_STDIO_MAXBUFFER"
							? "git_timeout"
							: "git_unavailable";
					log.debug("git call failed; recall skipped", { args: args.join(" "), failure, error: error.message });
					resolvePromise({ ok: false, failure });
				},
			);
		} catch (error) {
			log.debug("git could not be started; recall skipped", {
				error: error instanceof Error ? error.message : String(error),
			});
			resolvePromise({ ok: false, failure: "git_unavailable" });
		}
	});
}

/** `git status --porcelain=v1 -z`: `XY path\0`, with the source path as an extra field for renames and copies. */
export function parsePorcelainStatus(output: Buffer | string): StatusEntry[] {
	const fields = output.toString("utf8").split("\0");
	const entries: StatusEntry[] = [];
	for (let i = 0; i < fields.length; i++) {
		const field = fields[i]!;
		if (field.length < 4 || field[2] !== " ") continue;
		const xy = field.slice(0, 2);
		const entry: StatusEntry = { xy, path: field.slice(3) };
		if (xy.includes("R") || xy.includes("C")) {
			const origPath = fields[i + 1];
			if (origPath) entry.origPath = origPath;
			i++;
		}
		entries.push(entry);
	}
	return entries;
}

export interface IndexListing {
	/** Paths in the index; unmerged stages collapse to one path. */
	paths: Set<string>;
	/** Digest of the listing with its tags removed: byte for byte the digest of `git ls-files -s -z`. */
	digest: string;
	/** Tag "S": git status never looks at these paths. */
	skipWorktree: string[];
	/** Lowercase tag: git status trusts the index for these paths and never reads the file. */
	assumeUnchanged: string[];
}

const SPACE = 0x20;
const TAG_SKIP_WORKTREE = 0x53;
const NUL = Buffer.from([0]);

/** `git ls-files -s -v -z`: `T mode object stage\tpath\0`, where T is the one-letter tag. */
export function parseIndexListing(output: Buffer): IndexListing {
	const hash = createHash("sha256");
	const paths = new Set<string>();
	const skipWorktree: string[] = [];
	const assumeUnchanged: string[] = [];
	let start = 0;
	while (start < output.length) {
		const nul = output.indexOf(0, start);
		const end = nul < 0 ? output.length : nul;
		const record = output.subarray(start, end);
		const tagged = record.length >= 2 && record[1] === SPACE;
		hash.update(tagged ? record.subarray(2) : record);
		if (nul >= 0) hash.update(NUL);
		const text = record.toString("utf8");
		const tab = text.indexOf("\t");
		if (tab >= 0 && tab < text.length - 1) {
			const path = text.slice(tab + 1);
			if (!paths.has(path)) {
				paths.add(path);
				const tag = tagged ? record[0]! : 0;
				if (tag >= 0x61 && tag <= 0x7a) assumeUnchanged.push(path);
				else if (tag === TAG_SKIP_WORKTREE) skipWorktree.push(path);
			}
		}
		start = end + 1;
	}
	return { paths, digest: hash.digest("hex").slice(0, 32), skipWorktree, assumeUnchanged };
}

interface HashBudget {
	remainingBytes: number;
}

/**
 * Digest one repo-relative path. `absentIsState` says a missing file is the
 * expected state (a deletion) rather than a path that vanished between the
 * listing and the read. Directories, special files, oversized or unreadable
 * files, and anything past the byte budget are unverifiable: the digest is
 * either of real bytes or it is not recorded at all.
 */
export async function digestRecallPath(
	repoRoot: string,
	relativePath: string,
	absentIsState: boolean,
	budget: HashBudget = { remainingBytes: RECALL_MAX_HASHED_BYTES },
): Promise<string> {
	const fullPath = join(repoRoot, relativePath);
	let stats: Awaited<ReturnType<typeof lstat>>;
	try {
		stats = await lstat(fullPath);
	} catch (error) {
		const code = (error as NodeJS.ErrnoException).code;
		return (code === "ENOENT" || code === "ENOTDIR") && absentIsState ? RECALL_ABSENT_DIGEST : RECALL_UNVERIFIABLE;
	}
	if (stats.isSymbolicLink()) {
		try {
			return recallDigest(`\0symlink:${await readlink(fullPath)}`);
		} catch {
			return RECALL_UNVERIFIABLE;
		}
	}
	if (!stats.isFile() || stats.size > RECALL_MAX_FILE_BYTES || stats.size > budget.remainingBytes) {
		return RECALL_UNVERIFIABLE;
	}
	let handle: Awaited<ReturnType<typeof open>> | undefined;
	try {
		// O_NONBLOCK keeps a path swapped for a FIFO after the lstat from blocking the open.
		const flags = constants.O_RDONLY | (constants.O_NONBLOCK ?? 0) | (constants.O_NOFOLLOW ?? 0);
		handle = await open(fullPath, flags);
		const opened = await handle.stat();
		if (!opened.isFile() || opened.size > RECALL_MAX_FILE_BYTES || opened.size > budget.remainingBytes) {
			return RECALL_UNVERIFIABLE;
		}
		const bytes = await handle.readFile();
		if (bytes.length > RECALL_MAX_FILE_BYTES || bytes.length > budget.remainingBytes) return RECALL_UNVERIFIABLE;
		budget.remainingBytes -= bytes.length;
		return recallDigest(bytes);
	} catch {
		return RECALL_UNVERIFIABLE;
	} finally {
		await handle?.close().catch(() => undefined);
	}
}

function isDeletion(xy: string): boolean {
	return xy[1] === "D" || (xy[0] === "D" && xy[1] === " ");
}

/** The deepest existing ancestor resolved through symlinks, with the missing tail appended. */
async function realpathOrResolve(path: string): Promise<string> {
	const missing: string[] = [];
	let current = resolve(path);
	while (true) {
		try {
			return join(await realpath(current), ...missing);
		} catch {
			const parent = dirname(current);
			if (parent === current) return resolve(path);
			missing.unshift(basename(current));
			current = parent;
		}
	}
}

/**
 * The agent dir relative to `repoRoot`, with `/` separators, when it lives
 * inside the worktree: "" when it is the worktree itself, undefined when it
 * is outside.
 */
export async function recallExcludedPath(repoRoot: string, agentDir: string | undefined): Promise<string | undefined> {
	if (!agentDir) return undefined;
	const [root, dir] = await Promise.all([realpathOrResolve(repoRoot), realpathOrResolve(agentDir)]);
	const rel = relative(root, dir);
	if (rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel)) return undefined;
	return rel.split(sep).join("/");
}

export function isRecallExcluded(path: string, excludedPath: string | undefined): boolean {
	return !!excludedPath && (path === excludedPath || path.startsWith(`${excludedPath}/`));
}

function excludePathspec(excludedPath: string | undefined): string[] {
	return excludedPath ? ["--", ".", `:(exclude,literal)${excludedPath}`] : [];
}

/** Skip-worktree paths split by presence on disk: present ones are hashed, absent ones are recorded by name. */
async function splitSkipWorktreeByPresence(
	repoRoot: string,
	paths: readonly string[],
	signal: AbortSignal | undefined,
): Promise<{ present: string[]; absent: string[] }> {
	const present: string[] = [];
	const absent: string[] = [];
	for (let index = 0; index < paths.length && !signal?.aborted; index += LSTAT_BATCH_SIZE) {
		const batch = paths.slice(index, index + LSTAT_BATCH_SIZE);
		const exists = await Promise.all(
			batch.map((path) =>
				lstat(join(repoRoot, path)).then(
					() => true,
					(error: NodeJS.ErrnoException) => error.code !== "ENOENT" && error.code !== "ENOTDIR",
				),
			),
		);
		batch.forEach((path, position) => {
			(exists[position] ? present : absent).push(path);
		});
	}
	return { present, absent };
}

/**
 * Snapshot the workspace at `repoRoot`. Fails whenever git cannot answer or
 * disagrees with findGitPaths about the toplevel: no mark and no block is
 * always the safe outcome, never a partial snapshot.
 */
export async function captureWorkspace(
	repoRoot: string,
	options: CaptureWorkspaceOptions = {},
): Promise<RecallCaptureResult> {
	const { signal } = options;
	const toplevelOutput = await runGit(repoRoot, ["rev-parse", "--show-toplevel"], { signal });
	if (!toplevelOutput.ok) return { ok: false, failure: toplevelOutput.failure };
	const toplevel = toplevelOutput.stdout.toString("utf8").trim();
	try {
		if (!toplevel || (await realpath(toplevel)) !== (await realpath(repoRoot))) {
			log.debug("git toplevel differs from the recall repo; recall skipped", { repoRoot, toplevel });
			return { ok: false, failure: "not_repo" };
		}
	} catch {
		return { ok: false, failure: "not_repo" };
	}
	const excludedPath = await recallExcludedPath(repoRoot, options.agentDir);
	if (excludedPath === "") {
		log.debug("the agent dir is the repo root; recall skipped", { repoRoot });
		return { ok: false, failure: "not_repo" };
	}
	const pathspec = excludePathspec(excludedPath);

	const [headOutput, index, status] = await Promise.all([
		runGit(repoRoot, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], { signal }),
		runGit(repoRoot, ["ls-files", "-s", "-v", "-z", ...pathspec], { signal }),
		runGit(repoRoot, ["status", "--porcelain=v1", "-z", "--untracked-files=all", ...pathspec], { signal }),
	]);
	if (signal?.aborted) return { ok: false, failure: "aborted" };
	if (!index.ok) return { ok: false, failure: index.failure };
	if (!status.ok) return { ok: false, failure: status.failure };
	// An unborn HEAD makes rev-parse exit non-zero; only a timeout is a failure of the capture.
	if (!headOutput.ok && headOutput.failure !== "git_unavailable") return { ok: false, failure: headOutput.failure };
	const head = (headOutput.ok && headOutput.stdout.toString("utf8").trim()) || null;
	const listing = parseIndexListing(index.stdout);

	const targets: Array<{ path: string; absentIsState: boolean }> = [];
	const seen = new Set<string>();
	const addTarget = (path: string, absentIsState: boolean) => {
		if (seen.has(path)) return;
		seen.add(path);
		targets.push({ path, absentIsState });
	};
	for (const entry of parsePorcelainStatus(status.stdout)) {
		addTarget(entry.path, isDeletion(entry.xy));
		if (entry.origPath !== undefined) addTarget(entry.origPath, true);
	}
	// git status reports no edit to either kind of entry, so hash them like dirty paths while there are few of them.
	const skipWorktreeUnchecked = listing.skipWorktree.length > RECALL_MAX_SKIP_WORKTREE_CHECKS;
	const skipWorktree = skipWorktreeUnchecked
		? { present: listing.skipWorktree, absent: undefined }
		: await splitSkipWorktreeByPresence(repoRoot, listing.skipWorktree, signal);
	if (signal?.aborted) return { ok: false, failure: "aborted" };
	const tagged = [...listing.assumeUnchanged, ...skipWorktree.present];
	const taggedPaths = new Set<string>();
	let unhashedTaggedPaths = tagged.filter((path) => !seen.has(path));
	if (skipWorktreeUnchecked || unhashedTaggedPaths.length > RECALL_MAX_TAGGED_PATHS) {
		log.debug("too many skip-worktree or assume-unchanged paths to hash; counting them unverifiable", {
			count: unhashedTaggedPaths.length,
		});
	} else {
		unhashedTaggedPaths = [];
		for (const path of tagged) {
			taggedPaths.add(path);
			addTarget(path, true);
		}
	}
	// Which entries a sparse checkout left off disk is part of the workspace: set, add or disable moves it with no status change.
	let absentSkipWorktree: AbsentSkipWorktree | null = null;
	if (skipWorktree.absent) {
		const absent = [...skipWorktree.absent].sort(compareStrings);
		absentSkipWorktree = { count: absent.length, digest: absentSkipWorktreeDigest(absent) };
		if (absent.length <= RECALL_MAX_TAGGED_PATHS) {
			absentSkipWorktree.paths = absent;
		} else {
			log.debug("too many absent skip-worktree paths to list; counting them unverifiable", { count: absent.length });
			unhashedTaggedPaths = [...unhashedTaggedPaths, ...absent.filter((path) => !seen.has(path))];
		}
	}

	const recorded = targets.slice(0, RECALL_MAX_DIRTY_PATHS);
	const overflowPaths = targets.slice(RECALL_MAX_DIRTY_PATHS).map((target) => target.path);
	const budget: HashBudget = { remainingBytes: RECALL_MAX_HASHED_BYTES };
	const dirty = new Map<string, string>();
	for (const target of recorded) {
		if (signal?.aborted) return { ok: false, failure: "aborted" };
		dirty.set(target.path, await digestRecallPath(repoRoot, target.path, target.absentIsState, budget));
	}

	return {
		ok: true,
		snapshot: {
			repoRoot,
			head,
			trackedTreeDigest: listing.digest,
			trackedPaths: listing.paths,
			dirty,
			dirtyOverflow: overflowPaths.length + unhashedTaggedPaths.length,
			absentSkipWorktree,
			overflowPaths,
			taggedPaths,
			unhashedTaggedPaths,
			...(excludedPath ? { excludedPath } : {}),
		},
	};
}

export interface CommittedChangesOptions {
	signal?: AbortSignal;
	excludedPath?: string;
}

/** Paths changed between two commits, or undefined when git cannot list them (unknown or pruned commit, shallow clone). */
export async function committedChangesBetween(
	repoRoot: string,
	fromHead: string,
	toHead: string,
	options: CommittedChangesOptions = {},
): Promise<Set<string> | undefined> {
	const pathspec = options.excludedPath ? [".", `:(exclude,literal)${options.excludedPath}`] : [];
	const output = await runGit(
		repoRoot,
		["diff", "--name-only", "--no-renames", "-z", fromHead, toHead, "--", ...pathspec],
		{ signal: options.signal },
	);
	if (!output.ok) return undefined;
	return new Set(
		output.stdout
			.toString("utf8")
			.split("\0")
			.filter((path) => path.length > 0),
	);
}

const REGULAR_FILE_MODES = new Set(["100644", "100755"]);
const BLOB_COMPARE_BATCH_SIZE = 64;

/**
 * The paths among `paths` that are regular files whose working content, as
 * `git hash-object` cleans it, is exactly the blob `commit` holds for them.
 * Anything git cannot answer for is left out.
 */
export async function pathsMatchingCommit(
	repoRoot: string,
	commit: string,
	paths: readonly string[],
	signal?: AbortSignal,
): Promise<Set<string>> {
	const matching = new Set<string>();
	const regular: string[] = [];
	for (let index = 0; index < paths.length && !signal?.aborted; index += LSTAT_BATCH_SIZE) {
		const batch = paths.slice(index, index + LSTAT_BATCH_SIZE);
		const isFile = await Promise.all(
			batch.map((path) =>
				lstat(join(repoRoot, path)).then(
					(stats) => stats.isFile(),
					() => false,
				),
			),
		);
		batch.forEach((path, position) => {
			if (isFile[position]) regular.push(path);
		});
	}
	for (let index = 0; index < regular.length && !signal?.aborted; index += BLOB_COMPARE_BATCH_SIZE) {
		const batch = regular.slice(index, index + BLOB_COMPARE_BATCH_SIZE);
		const [tree, hashed] = await Promise.all([
			runGit(repoRoot, ["--literal-pathspecs", "ls-tree", "-z", commit, "--", ...batch], { signal }),
			runGit(repoRoot, ["hash-object", "--", ...batch], { signal }),
		]);
		if (!tree.ok || !hashed.ok) continue;
		const hashes = hashed.stdout.toString("utf8").split("\n").slice(0, batch.length);
		if (hashes.length !== batch.length) continue;
		const blobs = new Map<string, string>();
		for (const record of tree.stdout.toString("utf8").split("\0")) {
			const tab = record.indexOf("\t");
			if (tab < 0) continue;
			const [mode, type, object] = record.slice(0, tab).split(" ");
			if (type === "blob" && object && REGULAR_FILE_MODES.has(mode ?? "")) {
				blobs.set(record.slice(tab + 1), object);
			}
		}
		batch.forEach((path, position) => {
			const object = blobs.get(path);
			if (object !== undefined && object === hashes[position]!.trim()) matching.add(path);
		});
	}
	return matching;
}
