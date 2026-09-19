import { join } from "node:path";
import { getAgentDir } from "../../config.js";

/**
 * Where refinement history lives on disk. Kept free of the refinement module's
 * imports so session deletion can find a session's log without loading the
 * planner, the gate, or the referee.
 */

export const HARNESS_STATE_DIR_NAME = "harness";
export const LOCAL_REFINEMENT_HISTORY_DIR_NAME = "local-refinements";

const SESSION_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$/;

/** Directory of per-session refinement logs, beside the global harness state. */
export function getLocalRefinementHistoryDir(agentDir: string = getAgentDir()): string {
	return join(agentDir, HARNESS_STATE_DIR_NAME, LOCAL_REFINEMENT_HISTORY_DIR_NAME);
}

/**
 * A session's durable local refinement log, or undefined for an id that could
 * name anything other than one plain file in that directory.
 */
export function getSessionRefinementHistoryPath(
	sessionId: string,
	agentDir: string = getAgentDir(),
): string | undefined {
	return SESSION_ID_PATTERN.test(sessionId)
		? join(getLocalRefinementHistoryDir(agentDir), `${sessionId}.jsonl`)
		: undefined;
}
