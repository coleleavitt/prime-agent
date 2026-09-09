import type { SessionInfo } from "../../core/session-manager.js";
import type { AgentConnectionSavedSessionInfo } from "../agent-connection/types.js";
import type { DaemonSavedSessionInfo } from "./daemon-protocol.js";

/**
 * `includeSearchText: false` sends "" for the transcript corpus. Clients must
 * treat "" as "not loaded" and fetch it separately; only clients that
 * negotiated `deferred_session_search_text` ever see it.
 */
export function serializeSavedSessionInfo(
	session: SessionInfo,
	options: { includeSearchText?: boolean } = {},
): DaemonSavedSessionInfo {
	return {
		path: session.path,
		id: session.id,
		cwd: session.cwd,
		name: session.name,
		state: session.state,
		parentSessionPath: session.parentSessionPath,
		rlmDepth: session.rlmDepth,
		created: session.created.toISOString(),
		modified: session.modified.toISOString(),
		messageCount: session.messageCount,
		firstMessage: session.firstMessage,
		allMessagesText: options.includeSearchText === false ? "" : session.allMessagesText,
		agentStatus: session.agentStatus,
		usage: session.usage,
	};
}

export function deserializeSavedSessionInfo(session: DaemonSavedSessionInfo): AgentConnectionSavedSessionInfo {
	return {
		path: session.path,
		id: session.id,
		cwd: session.cwd,
		name: session.name,
		state: session.state,
		parentSessionPath: session.parentSessionPath,
		rlmDepth: session.rlmDepth,
		created: new Date(session.created),
		modified: new Date(session.modified),
		messageCount: session.messageCount,
		firstMessage: session.firstMessage,
		allMessagesText: session.allMessagesText,
		agentStatus: session.agentStatus,
		usage: session.usage,
	};
}
