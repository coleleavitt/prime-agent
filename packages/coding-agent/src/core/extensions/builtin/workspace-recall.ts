/**
 * Built-in Workspace Recall extension (docs/rsi-plan.md section 8, row 8).
 *
 * Stores the invalidation, not the answer. On agent_end it writes a per-repo
 * mark of digests (HEAD, index, dirty paths, build claims). On the first
 * ipython result of a top-level session it recomputes every digest against
 * the live filesystem and appends a bounded `<workspace_recall>` block saying
 * what changed, what is provably unchanged, and what could not be checked.
 *
 * A build claim is a bash() build or test command that exited 0 inside an
 * ipython cell during which the workspace digest did not move.
 *
 * Every failure degrades to today's behaviour: no mark and no block.
 */

import { realpath } from "node:fs/promises";
import { basename } from "node:path";
import {
	currentTraceContext,
	getLogger,
	runWithTraceContext,
	type Span,
	type SpanAttributes,
	withSpan,
} from "@earendil-works/pi-ai";
import { getAgentDir } from "../../../config.js";
import { isBuildClaimCommand, mentionsBuildCommand, RECALL_MAX_CLAIMS } from "../../recall/claims.js";
import {
	captureWorkspace,
	findRecallRepo,
	isFullyVerifiable,
	RECALL_UNVERIFIABLE,
	workspaceDigest,
} from "../../recall/mark.js";
import { renderRecallBlock } from "../../recall/render.js";
import {
	type RecallClaimInput,
	type RecallMarkFile,
	type RecallSkipEntry,
	type RecallSkipReason,
	readRecallMark,
	readRecallSkip,
	recallRepoKey,
	type WriteRecallMarkOptions,
	type WriteRecallMarkOutcome,
	writeRecallMark,
	writeRecallSkip,
} from "../../recall/store.js";
import { witnessWorkspace } from "../../recall/witness.js";
import type { ReadonlySessionManager } from "../../session-manager.js";
import {
	type ExtensionAPI,
	type ExtensionContext,
	type ExtensionFactory,
	type IpythonToolResultEvent,
	isIpythonToolResult,
	isToolCallEventType,
	type ToolResultEvent,
} from "../types.js";

export const WORKSPACE_RECALL_ENV = "PRIME_AGENT_WORKSPACE_RECALL";

const WITNESS_TOOL_NAME = "ipython";
/** How long a witness waits for its session's first mark to record the mark it is about to replace. */
const PRIOR_MARK_WAIT_MS = 300;
/** Recall work on the tool path (witness, cell digests) gives up after this. */
const TOOL_PATH_DEADLINE_MS = 1000;
/** How long a missed deadline keeps this runtime, and no other, off the repo's tool path: the stall may have been its own. */
const DEADLINE_SKIP_TTL_MS = 60_000;
const MARK_WAIT_AT_SHUTDOWN_MS = 2000;
const MAX_TRACKED_CELLS = 32;
const CHILD_SESSION_DIR = /^sub-[0-9a-f]{8}$/;

const log = getLogger("coding-agent.workspace-recall");

/** Enabled unless the kill switch is `0`, `off`, `false`, or `no`. */
export function isWorkspaceRecallEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
	const value = env[WORKSPACE_RECALL_ENV]?.trim().toLowerCase();
	return !(value === "0" || value === "off" || value === "false" || value === "no");
}

export type WorkspaceRecallMarkOutcome =
	| { status: "written"; sessionId: string; repoRoot: string; repoKey: string; mark: RecallMarkFile }
	| { status: "skipped"; sessionId: string; reason: RecallSkipReason };

export interface WorkspaceRecallExtensionOptions {
	/** Agent dir holding `recall/`. Defaults to getAgentDir() when the factory runs. */
	agentDir?: string;
	/** Called after each mark attempt settles, including skipped ones; for tests and embedders. */
	onMarkSettled?: (outcome: WorkspaceRecallMarkOutcome) => void;
	/** Replaces the mark writer; for tests that need a mark that fails or never settles. */
	writeMark?: (repoRoot: string, options: WriteRecallMarkOptions) => Promise<WriteRecallMarkOutcome>;
}

/**
 * RLM children write no mark and get no block: they share the parent's
 * workspace and would only repeat the parent's orientation. The extension
 * context carries no depth, so this reads the session's own metadata: the
 * header depth, a parent without a recorded depth, or the `sub-xxxxxxxx`
 * session directory every child runtime is created under.
 */
export function isRecallChildSession(
	sessionManager: Pick<ReadonlySessionManager, "getHeader" | "getSessionDir">,
): boolean {
	const header = sessionManager.getHeader();
	if (typeof header?.rlmDepth === "number") {
		if (header.rlmDepth > 0) return true;
	} else if (header?.parentSession) {
		return true;
	}
	return CHILD_SESSION_DIR.test(basename(sessionManager.getSessionDir()));
}

function hasEarlierIpythonResult(sessionManager: ReadonlySessionManager, toolCallId: string): boolean {
	for (const entry of sessionManager.getBranch()) {
		if (entry.type !== "message") continue;
		const message = entry.message;
		if (
			message.role === "toolResult" &&
			message.toolName === WITNESS_TOOL_NAME &&
			message.toolCallId !== toolCallId
		) {
			return true;
		}
	}
	return false;
}

function waitAtMost(promise: Promise<void> | undefined, ms: number): Promise<void> {
	if (!promise) return Promise.resolve();
	return new Promise((resolve) => {
		const timer = setTimeout(resolve, ms);
		timer.unref?.();
		void promise.finally(() => {
			clearTimeout(timer);
			resolve();
		});
	});
}

type Deadlined<T> = { ok: true; value: T } | { ok: false };

/** Race `work` against a deadline; either way its signal is aborted afterwards so git children die with it. */
async function withinDeadline<T>(ms: number, work: (signal: AbortSignal) => Promise<T>): Promise<Deadlined<T>> {
	const controller = new AbortController();
	let timer: ReturnType<typeof setTimeout> | undefined;
	const expired = new Promise<Deadlined<T>>((resolve) => {
		timer = setTimeout(() => resolve({ ok: false }), ms);
		timer.unref?.();
	});
	try {
		return await Promise.race([
			work(controller.signal).then((value): Deadlined<T> => ({ ok: true, value })),
			expired,
		]);
	} finally {
		clearTimeout(timer);
		controller.abort();
	}
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export function createWorkspaceRecallExtension(options: WorkspaceRecallExtensionOptions = {}): ExtensionFactory {
	return (pi: ExtensionAPI) => {
		workspaceRecallExtensionImpl(pi, options);
	};
}

interface PendingMark {
	promise: Promise<void>;
	/** Settles once the first run has recorded the mark it replaces, or has decided not to write. */
	priorReady: Promise<void>;
	rerun?: { cwd: string; triggerTraceId?: string };
}

interface TrackedCell {
	sessionId: string;
	repoRoot: string;
	digest: string;
}

interface PendingClaim extends RecallClaimInput {
	repoRoot: string;
}

interface CellDigest {
	digest: string;
	verifiable: boolean;
}

type RecallSlowReason = "git_timeout" | "deadline";

function workspaceRecallExtensionImpl(pi: ExtensionAPI, options: WorkspaceRecallExtensionOptions): void {
	if (!isWorkspaceRecallEnabled()) return;
	const agentDir = options.agentDir ?? getAgentDir();
	const writeMark = options.writeMark ?? writeRecallMark;

	/** Sessions whose witness is decided: "pending" while it runs, "done" once it has run or was ruled out. */
	const witnessState = new Map<string, "pending" | "done">();
	/**
	 * The mark a session's own first agent_end replaced, kept until its first
	 * witness. Without it a session that answers once without ipython would
	 * witness against the mark it just wrote and report nothing changed.
	 */
	const priorMarks = new Map<string, RecallMarkFile | null>();
	const pendingMarks = new Map<string, PendingMark>();
	/** Workspace digest taken before a build cell ran, by tool call id. */
	const trackedCells = new Map<string, TrackedCell>();
	/** Claims waiting for the session's next mark. */
	const pendingClaims = new Map<string, PendingClaim[]>();
	/** Repos whose git timed out, by repo key; mirrored on disk for every process sharing the agent dir. */
	const timedOutRepos = new Map<string, RecallSkipEntry>();
	/** Repos whose tool-path recall missed its deadline, by repo key, to an epoch ms. In memory only, never shared. */
	const deadlineMisses = new Map<string, number>();

	async function gitTimeoutEntry(repoRoot: string, repoKey: string): Promise<RecallSkipEntry | undefined> {
		const now = Date.now();
		const held = timedOutRepos.get(repoKey);
		if (held && held.until > now) return held;
		const stored = await readRecallSkip(repoRoot, agentDir, now);
		if (stored) timedOutRepos.set(repoKey, stored);
		else timedOutRepos.delete(repoKey);
		return stored;
	}

	/** Why the tool path should leave this repo alone right now, if it should. */
	async function toolPathSkip(repoRoot: string, repoKey: string): Promise<RecallSlowReason | undefined> {
		const until = deadlineMisses.get(repoKey);
		if (until !== undefined) {
			if (until > Date.now()) return "deadline";
			deadlineMisses.delete(repoKey);
		}
		return (await gitTimeoutEntry(repoRoot, repoKey))?.reason;
	}

	async function rememberGitTimeout(repoRoot: string, repoKey: string): Promise<void> {
		timedOutRepos.set(repoKey, await writeRecallSkip(repoRoot, agentDir));
	}

	function rememberDeadlineMiss(repoKey: string): void {
		deadlineMisses.set(repoKey, Date.now() + DEADLINE_SKIP_TTL_MS);
	}

	function settle(outcome: WorkspaceRecallMarkOutcome): void {
		try {
			options.onMarkSettled?.(outcome);
		} catch (error) {
			log.debug("recall onMarkSettled threw", { error: errorMessage(error) });
		}
	}

	function addClaims(sessionId: string, claims: readonly PendingClaim[]): void {
		if (claims.length === 0) return;
		pendingClaims.set(sessionId, [...(pendingClaims.get(sessionId) ?? []), ...claims].slice(-RECALL_MAX_CLAIMS));
	}

	function takeClaims(sessionId: string, repoRoot: string): PendingClaim[] {
		const held = pendingClaims.get(sessionId) ?? [];
		pendingClaims.delete(sessionId);
		return held.filter((claim) => claim.repoRoot === repoRoot);
	}

	async function markRepo(
		span: Span,
		attrs: SpanAttributes,
		sessionId: string,
		repoRoot: string,
		repoKey: string,
		priorReady: () => void,
	): Promise<WorkspaceRecallMarkOutcome> {
		// Only a git timeout stops marks: they run off the tool path, so a missed deadline there is no reason to skip.
		if (await gitTimeoutEntry(repoRoot, repoKey)) {
			Object.assign(attrs, {
				"recall.skipped": true,
				"recall.skip_reason": "git_timeout",
				"recall.negative_cache": true,
			});
			return { status: "skipped", sessionId, reason: "git_timeout" };
		}
		if (witnessState.get(sessionId) !== "done" && !priorMarks.has(sessionId)) {
			// Recorded before this write starts, so a witness that reads the file afterwards can tell which mark was prior.
			priorMarks.set(sessionId, (await readRecallMark(repoRoot, agentDir)) ?? null);
			if (witnessState.get(sessionId) === "done") priorMarks.delete(sessionId);
		}
		priorReady();
		const claims = takeClaims(sessionId, repoRoot);
		const result = await writeMark(repoRoot, {
			agentDir,
			claims: claims.map(({ repoRoot: _repoRoot, ...claim }) => claim),
		});
		if (!result.ok) {
			addClaims(sessionId, claims);
			Object.assign(attrs, { "recall.skipped": true, "recall.skip_reason": result.reason });
			if (result.reason === "git_timeout") await rememberGitTimeout(repoRoot, repoKey);
			if (result.reason === "write_failed") span.recordError(result.error ?? "recall mark write failed");
			return { status: "skipped", sessionId, reason: result.reason };
		}
		const dirty = Object.values(result.mark.dirty);
		Object.assign(attrs, {
			"recall.dirty_count": dirty.length + result.mark.dirtyOverflow,
			"recall.claims": result.mark.claims.length,
			"recall.unverifiable":
				dirty.filter((digest) => digest === RECALL_UNVERIFIABLE).length + result.mark.dirtyOverflow,
		});
		return { status: "written", sessionId, repoRoot, repoKey: result.repoKey, mark: result.mark };
	}

	async function runMark(
		sessionId: string,
		cwd: string,
		triggerTraceId: string | undefined,
		priorReady: () => void,
	): Promise<void> {
		let outcome: WorkspaceRecallMarkOutcome = { status: "skipped", sessionId, reason: "git_unavailable" };
		try {
			if (!isWorkspaceRecallEnabled()) {
				outcome = { status: "skipped", sessionId, reason: "disabled" };
				return;
			}
			const found = findRecallRepo(cwd);
			if (!found) {
				outcome = { status: "skipped", sessionId, reason: "not_repo" };
				return;
			}
			// agent_end does not wait for this work, so it must not become a child that outlives the turn.
			await runWithTraceContext(undefined, () =>
				withSpan("recall.mark", { "trigger.trace_id": triggerTraceId }, async (span) => {
					const started = performance.now();
					const repoRoot = await resolveRepoRoot(found);
					const repoKey = recallRepoKey(repoRoot, agentDir);
					const attrs: SpanAttributes = { "recall.repo_key": repoKey };
					try {
						outcome = await markRepo(span, attrs, sessionId, repoRoot, repoKey, priorReady);
					} finally {
						attrs["recall.ms"] = Math.round(performance.now() - started);
						span.setAttributes(attrs);
					}
				}),
			);
		} catch (error) {
			log.debug("recall mark failed", { sessionId, error: errorMessage(error) });
		} finally {
			priorReady();
			settle(outcome);
		}
	}

	function scheduleMark(sessionId: string, cwd: string, triggerTraceId: string | undefined): void {
		const pending = pendingMarks.get(sessionId);
		if (pending) {
			pending.rerun = { cwd, triggerTraceId };
			return;
		}
		let resolvePriorReady: () => void = () => {};
		const entry: PendingMark = {
			promise: Promise.resolve(),
			priorReady: new Promise<void>((resolve) => {
				resolvePriorReady = resolve;
			}),
		};
		pendingMarks.set(sessionId, entry);
		entry.promise = (async () => {
			let next: PendingMark["rerun"] = { cwd, triggerTraceId };
			while (next) {
				entry.rerun = undefined;
				await runMark(sessionId, next.cwd, next.triggerTraceId, resolvePriorReady);
				// Read and release with no await in between, so a rerun requested now is never dropped.
				next = entry.rerun;
				if (!next && pendingMarks.get(sessionId) === entry) pendingMarks.delete(sessionId);
			}
		})();
	}

	async function witnessAgainstMark(
		span: Span,
		sessionId: string,
		repoRoot: string,
		signal: AbortSignal,
	): Promise<string | undefined> {
		await waitAtMost(pendingMarks.get(sessionId)?.priorReady, PRIOR_MARK_WAIT_MS);
		const read = priorMarks.has(sessionId) ? undefined : await readRecallMark(repoRoot, agentDir);
		// The session's first mark may have recorded its prior copy while the file was being read; that copy wins.
		const mark = priorMarks.has(sessionId) ? (priorMarks.get(sessionId) ?? undefined) : read;
		priorMarks.delete(sessionId);
		// Past the deadline the caller owns the span; this path only winds down.
		if (signal.aborted) return undefined;
		if (!mark) {
			span.setAttributes({ "recall.has_mark": false, "recall.block_bytes": 0 });
			return undefined;
		}
		const result = await witnessWorkspace(repoRoot, mark, { agentDir, signal });
		if (signal.aborted) return undefined;
		if (!result.ok) {
			span.setAttributes({
				"recall.has_mark": true,
				"recall.skipped": true,
				"recall.skip_reason": result.failure,
				"recall.block_bytes": 0,
			});
			if (result.failure === "git_timeout") await rememberGitTimeout(repoRoot, recallRepoKey(repoRoot, agentDir));
			return undefined;
		}
		const { report } = result;
		const block = renderRecallBlock(report);
		span.setAttributes({
			"recall.has_mark": true,
			"recall.changed": report.changed.length,
			"recall.changed_unknown": report.changedUnknownReason !== undefined,
			"recall.unchanged": report.unchangedCount,
			"recall.unverifiable": report.unverifiable.length,
			"recall.uncompared": report.uncomparedCount,
			"recall.claims_current": report.claims.filter((verdict) => verdict.status === "CURRENT").length,
			"recall.claims_expired": report.claims.filter((verdict) => verdict.status === "EXPIRED").length,
			"recall.head_moved": report.headMoved,
			"recall.block_bytes": Buffer.byteLength(block, "utf8"),
		});
		return block;
	}

	async function witness(
		event: ToolResultEvent,
		sessionId: string,
		cwd: string,
		sessionManager: ReadonlySessionManager,
	): Promise<string | undefined> {
		if (isRecallChildSession(sessionManager) || hasEarlierIpythonResult(sessionManager, event.toolCallId)) {
			return undefined;
		}
		const found = findRecallRepo(cwd);
		if (!found) return undefined;
		return withSpan("recall.witness", undefined, async (span) => {
			const repoRoot = await resolveRepoRoot(found);
			const repoKey = recallRepoKey(repoRoot, agentDir);
			span.setAttributes({ "recall.repo_key": repoKey });
			const skip = await toolPathSkip(repoRoot, repoKey);
			if (skip) {
				span.setAttributes({
					"recall.skipped": true,
					"recall.skip_reason": skip,
					"recall.negative_cache": true,
					"recall.block_bytes": 0,
				});
				return undefined;
			}
			const outcome = await withinDeadline(TOOL_PATH_DEADLINE_MS, (signal) =>
				witnessAgainstMark(span, sessionId, repoRoot, signal),
			);
			if (outcome.ok) return outcome.value;
			span.setAttributes({ "recall.skipped": true, "recall.skip_reason": "deadline", "recall.block_bytes": 0 });
			rememberDeadlineMiss(repoKey);
			return undefined;
		});
	}

	/** Workspace digest for build claims, bounded like the witness; undefined when it could not be taken in time. */
	function cellDigest(
		phase: "tool_call" | "tool_result",
		repoRoot: string,
		compareTo?: string,
	): Promise<CellDigest | undefined> {
		return withSpan("recall.digest", { "recall.phase": phase }, async (span) => {
			const started = performance.now();
			const repoKey = recallRepoKey(repoRoot, agentDir);
			const attrs: SpanAttributes = { "recall.repo_key": repoKey };
			try {
				const skip = await toolPathSkip(repoRoot, repoKey);
				if (skip) {
					Object.assign(attrs, {
						"recall.skipped": true,
						"recall.skip_reason": skip,
						"recall.negative_cache": true,
					});
					return undefined;
				}
				const captured = await withinDeadline(TOOL_PATH_DEADLINE_MS, (signal) =>
					captureWorkspace(repoRoot, { agentDir, signal }),
				);
				if (!captured.ok) {
					Object.assign(attrs, { "recall.skipped": true, "recall.skip_reason": "deadline" });
					rememberDeadlineMiss(repoKey);
					return undefined;
				}
				if (!captured.value.ok) {
					Object.assign(attrs, { "recall.skipped": true, "recall.skip_reason": captured.value.failure });
					if (captured.value.failure === "git_timeout") await rememberGitTimeout(repoRoot, repoKey);
					return undefined;
				}
				const { snapshot } = captured.value;
				const digest = { digest: workspaceDigest(snapshot), verifiable: isFullyVerifiable(snapshot) };
				attrs["recall.verifiable"] = digest.verifiable;
				if (compareTo !== undefined) attrs["recall.digest_matched"] = digest.digest === compareTo;
				return digest;
			} finally {
				attrs["recall.ms"] = Math.round(performance.now() - started);
				span.setAttributes(attrs);
			}
		});
	}

	function trackCell(toolCallId: string, cell: TrackedCell): void {
		trackedCells.set(toolCallId, cell);
		// A blocked or abandoned call never reaches tool_result; drop the oldest instead of growing.
		for (const staleId of trackedCells.keys()) {
			if (trackedCells.size <= MAX_TRACKED_CELLS) break;
			trackedCells.delete(staleId);
		}
	}

	async function recordCellClaims(event: IpythonToolResultEvent, cell: TrackedCell): Promise<void> {
		const commands = (event.details?.bashCommands ?? []).filter(
			(command) => command.exitCode === 0 && !command.commandTruncated && isBuildClaimCommand(command.command),
		);
		if (commands.length === 0) return;
		const after = await cellDigest("tool_result", cell.repoRoot, cell.digest);
		if (!after?.verifiable || after.digest !== cell.digest) return;
		const at = new Date().toISOString();
		addClaims(
			cell.sessionId,
			commands.map((command) => ({
				command: command.command,
				exitCode: 0,
				at,
				digestAtClaim: cell.digest,
				repoRoot: cell.repoRoot,
			})),
		);
	}

	pi.on("tool_call", async (event, ctx) => {
		// Registered for every tool call, so everything before the source check stays allocation- and I/O-free.
		if (!isToolCallEventType("ipython", event) || !isWorkspaceRecallEnabled()) return undefined;
		const code: unknown = event.input.code;
		if (typeof code !== "string" || !mentionsBuildCommand(code)) return undefined;
		try {
			if (isRecallChildSession(ctx.sessionManager)) return undefined;
			const sessionId = ctx.sessionManager.getSessionId();
			const found = findRecallRepo(ctx.cwd);
			if (!found) return undefined;
			const repoRoot = await resolveRepoRoot(found);
			const before = await cellDigest("tool_call", repoRoot);
			if (before?.verifiable) trackCell(event.toolCallId, { sessionId, repoRoot, digest: before.digest });
		} catch (error) {
			log.debug("recall cell digest failed", { error: errorMessage(error) });
		}
		return undefined;
	});

	pi.on("tool_result", async (event, ctx) => {
		// Registered for every tool call, so everything before the session check stays allocation- and I/O-free.
		if (!isIpythonToolResult(event) || !isWorkspaceRecallEnabled()) return undefined;
		const cell = trackedCells.get(event.toolCallId);
		if (cell) trackedCells.delete(event.toolCallId);
		const claimsRecorded = cell
			? recordCellClaims(event, cell).catch((error: unknown) => {
					log.debug("recall claim recording failed", { error: errorMessage(error) });
				})
			: undefined;
		let sessionId: string;
		let cwd: string;
		let sessionManager: ReadonlySessionManager;
		try {
			sessionManager = ctx.sessionManager;
			sessionId = sessionManager.getSessionId();
			cwd = ctx.cwd;
		} catch {
			await claimsRecorded;
			return undefined;
		}
		if (witnessState.has(sessionId)) {
			await claimsRecorded;
			return undefined;
		}
		witnessState.set(sessionId, "pending");
		try {
			const [block] = await Promise.all([witness(event, sessionId, cwd, sessionManager), claimsRecorded]);
			if (!block) return undefined;
			return { content: [...event.content, { type: "text", text: block }] };
		} catch (error) {
			log.debug("recall witness failed", { sessionId, error: errorMessage(error) });
			return undefined;
		} finally {
			witnessState.set(sessionId, "done");
			priorMarks.delete(sessionId);
		}
	});

	pi.on("agent_end", (_event, ctx: ExtensionContext) => {
		let sessionId: string;
		let cwd: string;
		let child: boolean;
		try {
			sessionId = ctx.sessionManager.getSessionId();
			cwd = ctx.cwd;
			child = isRecallChildSession(ctx.sessionManager);
		} catch {
			return;
		}
		if (!isWorkspaceRecallEnabled()) {
			settle({ status: "skipped", sessionId, reason: "disabled" });
			return;
		}
		if (child) {
			settle({ status: "skipped", sessionId, reason: "child_session" });
			return;
		}
		scheduleMark(sessionId, cwd, currentTraceContext()?.traceId);
	});

	pi.on("session_shutdown", async () => {
		if (pendingMarks.size === 0) return;
		await waitAtMost(
			Promise.all([...pendingMarks.values()].map((pending) => pending.promise)).then(() => undefined),
			MARK_WAIT_AT_SHUTDOWN_MS,
		);
	});
}

/** The mark is keyed by the resolved toplevel so a symlinked cwd and its target share one mark. */
async function resolveRepoRoot(repoDir: string): Promise<string> {
	try {
		return await realpath(repoDir);
	} catch {
		return repoDir;
	}
}
