import type { DeleteSessionFileResult } from "../../core/session-file-actions.js";
import type {
	AgentConnectionSavedSessionInfo,
	AgentConnectionSavedSessionScope,
	AgentConnectionSessionListCallbacks,
} from "../agent-connection/types.js";
import type { DaemonTransportClient } from "./daemon-client.js";
import { deserializeDaemonError } from "./daemon-errors.js";
import type {
	DaemonCommand,
	DaemonDeleteSavedSessionResult,
	DaemonSavedSessionInfo,
	DaemonSavedSessionListCommand,
	DaemonSavedSessionSearchTextEntry,
} from "./daemon-protocol.js";
import { deserializeSavedSessionInfo } from "./saved-session-info.js";

export { deserializeSavedSessionInfo } from "./saved-session-info.js";

export type DaemonSavedSessionCatalogContext = { activeSessionId: string } | { cwd: string; sessionDir?: string };

export async function listDaemonSavedSessions(
	client: DaemonTransportClient,
	context: DaemonSavedSessionCatalogContext,
	scope: AgentConnectionSavedSessionScope,
	callbacks?: AgentConnectionSessionListCallbacks,
	options: { includeSearchText?: boolean } = {},
): Promise<AgentConnectionSavedSessionInfo[]> {
	// Only opt out against a daemon that can serve the corpus separately; an
	// older daemon would ignore the flag and the corpus would be lost, not
	// deferred.
	const includeSearchText =
		options.includeSearchText === false && client.supportsServerCapability("deferred_session_search_text")
			? { includeSearchText: false }
			: {};
	const command: DaemonSavedSessionListCommand =
		"activeSessionId" in context
			? { type: "list_saved_sessions", activeSessionId: context.activeSessionId, scope, ...includeSearchText }
			: {
					type: "list_saved_sessions",
					cwd: context.cwd,
					sessionDir: context.sessionDir,
					scope,
					...includeSearchText,
				};
	const response = await client.request(command, 30000, {
		onProgress: (update) => {
			if (update.type === "session_list_progress") {
				callbacks?.onProgress?.(update.loaded, update.total);
			} else {
				callbacks?.onSession?.(deserializeSavedSessionInfo(update.session));
			}
		},
	});
	if (!response.success) {
		throw deserializeDaemonError(response);
	}
	const data = response.data as { sessions: DaemonSavedSessionInfo[] };
	return data.sessions.map(deserializeSavedSessionInfo);
}

/**
 * Fetch transcript corpora for specific sessions. Returns an empty map when the
 * daemon predates the capability, so callers keep whatever the catalog sent.
 */
export async function fetchDaemonSavedSessionSearchText(
	client: DaemonTransportClient,
	paths: readonly string[],
	sessionDir?: string,
): Promise<Map<string, string>> {
	const corpora = new Map<string, string>();
	if (paths.length === 0 || !client.supportsServerCapability("deferred_session_search_text")) {
		return corpora;
	}
	const response = await client.request({ type: "get_saved_session_search_text", paths, sessionDir }, 30000);
	if (!response.success) {
		throw deserializeDaemonError(response);
	}
	const data = response.data as { entries: DaemonSavedSessionSearchTextEntry[] };
	for (const entry of data.entries) {
		corpora.set(entry.path, entry.allMessagesText);
	}
	return corpora;
}

export async function renameDaemonSavedSession(
	client: DaemonTransportClient,
	context: DaemonSavedSessionCatalogContext,
	sessionPath: string,
	name: string,
): Promise<void> {
	const command: Extract<DaemonCommand, { type: "rename_saved_session" }> =
		"activeSessionId" in context
			? { type: "rename_saved_session", activeSessionId: context.activeSessionId, sessionPath, name }
			: { type: "rename_saved_session", sessionPath, name };
	const response = await client.request(command);
	if (!response.success) {
		throw deserializeDaemonError(response);
	}
}

export async function deleteDaemonSavedSession(
	client: DaemonTransportClient,
	context: DaemonSavedSessionCatalogContext,
	sessionPath: string,
): Promise<DeleteSessionFileResult> {
	const command: Extract<DaemonCommand, { type: "delete_saved_session" }> =
		"activeSessionId" in context
			? { type: "delete_saved_session", activeSessionId: context.activeSessionId, sessionPath }
			: { type: "delete_saved_session", sessionPath };
	const response = await client.request(command);
	if (!response.success) {
		throw deserializeDaemonError(response);
	}
	return response.data as DaemonDeleteSavedSessionResult;
}
