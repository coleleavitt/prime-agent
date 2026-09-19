import { type Dirent, existsSync } from "node:fs";
import { readdir, rm, unlink } from "node:fs/promises";
import { basename, join } from "node:path";
import { spawnSyncHidden } from "../utils/child-process.js";
import { readFirstLineSync } from "../utils/file-lines.js";
import { getSessionRefinementHistoryPath } from "./refinement/history-paths.js";
import { getSessionArtifactPathForFile } from "./session-manager.js";

export type DeleteSessionFileResult = { ok: true; method: "trash" | "unlink" } | { ok: false; error: string };

export interface DeleteSessionFileOptions {
	afterFileRemoved?: () => void;
}

/**
 * Permanently remove a session's artifact directory (durable schedule state,
 * kernel snapshot, RLM scratch files, …), which lives at
 * `<dirname(sessionDir)>/session-artifacts/<id>`. The RLM child transcripts
 * inside it go too, and so do those children's durable refinement logs.
 * Only invoked on delete, never on deactivation.
 */
export async function deleteSessionArtifacts(sessionPath: string): Promise<void> {
	// A degenerate name (".jsonl") would resolve to the artifacts root itself.
	if (!basename(sessionPath).replace(/\.jsonl$/, "")) return;
	const artifactDir = getSessionArtifactPathForFile(sessionPath);
	const childIds = await childSessionIds(artifactDir);
	await rm(artifactDir, { recursive: true, force: true });
	await deleteRefinementHistories(childIds);
}

/** The header id of a session transcript, or undefined for a file with no readable session header. */
function sessionFileId(sessionPath: string): string | undefined {
	try {
		const header = JSON.parse(readFirstLineSync(sessionPath) ?? "") as { type?: unknown; id?: unknown } | null;
		return header?.type === "session" && typeof header.id === "string" ? header.id : undefined;
	} catch {
		return undefined;
	}
}

/**
 * Ids of the RLM child sessions, at any depth, whose transcripts live under an
 * artifact directory: each child's transcript sits directly in its `sub-*` directory.
 */
async function childSessionIds(artifactDir: string): Promise<string[]> {
	let entries: Dirent[];
	try {
		entries = await readdir(artifactDir, { withFileTypes: true, recursive: true });
	} catch {
		return [];
	}
	return entries.flatMap((entry) => {
		if (!entry.isFile() || !entry.name.endsWith(".jsonl") || !basename(entry.parentPath).startsWith("sub-"))
			return [];
		const id = sessionFileId(join(entry.parentPath, entry.name));
		return id === undefined ? [] : [id];
	});
}

/**
 * Remove deleted sessions' durable refinement logs
 * (`<agentDir>/harness/local-refinements/<id>.jsonl`). They hold proposals and
 * judge rationales derived from those conversations, so they go with the sessions.
 */
async function deleteRefinementHistories(sessionIds: readonly string[]): Promise<void> {
	for (const sessionId of sessionIds) {
		const historyPath = getSessionRefinementHistoryPath(sessionId);
		if (historyPath) await rm(historyPath, { force: true });
	}
}

/** Remove the session `.jsonl`, trying the `trash` CLI first, then falling back to unlink. */
async function removeSessionFile(sessionPath: string): Promise<DeleteSessionFileResult> {
	const trashArgs = sessionPath.startsWith("-") ? ["--", sessionPath] : [sessionPath];
	const trashResult = spawnSyncHidden("trash", trashArgs, { encoding: "utf-8" });

	const getTrashErrorHint = (): string | null => {
		const parts: string[] = [];
		if (trashResult.error) {
			parts.push(trashResult.error.message);
		}
		const stderr = trashResult.stderr?.trim();
		if (stderr) {
			parts.push(stderr.split("\n")[0] ?? stderr);
		}
		if (parts.length === 0) return null;
		return `trash: ${parts.join(" - ").slice(0, 200)}`;
	};

	if (trashResult.status === 0 || !existsSync(sessionPath)) {
		return { ok: true, method: "trash" };
	}

	try {
		await unlink(sessionPath);
		return { ok: true, method: "unlink" };
	} catch (err) {
		const unlinkError = err instanceof Error ? err.message : String(err);
		const trashErrorHint = getTrashErrorHint();
		const error = trashErrorHint ? `${unlinkError} (${trashErrorHint})` : unlinkError;
		return { ok: false, error };
	}
}

/**
 * Delete a session file, trying the `trash` CLI first, then falling back to unlink.
 * Also permanently removes the session's artifact directory and its durable
 * refinement log, but only once the session file itself is gone — otherwise a
 * failed delete would orphan a session whose kernel snapshot has already been
 * destroyed.
 */
export async function deleteSessionFile(
	sessionPath: string,
	options: DeleteSessionFileOptions = {},
): Promise<DeleteSessionFileResult> {
	// A session's log is keyed by its header id, which a file opened by explicit path does not
	// share with its name; a file never written can only be named by the id it would have held.
	const sessionId = existsSync(sessionPath)
		? sessionFileId(sessionPath)
		: basename(sessionPath).replace(/\.jsonl$/, "");
	const result = await removeSessionFile(sessionPath);
	if (result.ok) {
		options.afterFileRemoved?.();
		await deleteSessionArtifacts(sessionPath);
		if (sessionId !== undefined) await deleteRefinementHistories([sessionId]);
	}
	return result;
}
