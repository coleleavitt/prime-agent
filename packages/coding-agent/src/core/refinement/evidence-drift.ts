import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { serializeConversation } from "../compaction/utils.js";
import { convertToLlm } from "../messages.js";
import type { RavoDecision, RavoGateReport } from "./ravo.js";

/**
 * How far the conversation moved between the moment a refine's proposer read
 * it and the moment its RAVO judge read it. Planning takes a model call, and
 * the session keeps working meanwhile, so the judge can weigh evidence the
 * proposer never saw. Drift is measured by message identity: the live array
 * keeps the objects the proposer read, a compaction or a retry replaces them.
 */

/** The conversation as the proposer read it. */
export interface RefineEvidenceSnapshot {
	readonly messages: AgentMessage[];
	readonly leafId: string | null;
}

export type RefineEvidenceDriftKind = "none" | "appended" | "rewritten";

export interface RefineEvidenceDrift {
	kind: RefineEvidenceDriftKind;
	snapshotMessages: number;
	judgeMessages: number;
	/** LLM-visible messages the judge reads that the proposer's snapshot did not hold. */
	appendedMessages: number;
	/** LLM-visible messages the proposer read that the judge's conversation no longer holds. */
	removedMessages: number;
	/** Serialized characters of the appended messages. */
	appendedChars: number;
	snapshotLeafId: string | null;
	judgeLeafId: string | null;
}

const STALE_EVIDENCE_DECISIONS: readonly RavoDecision[] = ["reject_deep", "reject_criteria", "reject_unclaimed"];

function llmText(messages: readonly AgentMessage[]): string {
	return messages.length === 0 ? "" : serializeConversation(convertToLlm([...messages]));
}

function visibleMessages(messages: readonly AgentMessage[]): number {
	let count = 0;
	for (const message of messages) {
		if (llmText([message]).length > 0) count++;
	}
	return count;
}

/** Copy the array the proposer is about to read; the messages themselves are kept by identity. */
export function captureRefineEvidence(
	messages: readonly AgentMessage[],
	leafId: string | null,
): RefineEvidenceSnapshot {
	return { messages: [...messages], leafId };
}

/**
 * Compare the proposer's snapshot with the conversation the judge reads now.
 * Only messages the judge can read count: a slash-command row or an excluded
 * bash run moves nothing. Losing a message the proposer read (a compaction, a
 * rewind, a retry dropping a partial reply) is `rewritten`; otherwise any new
 * readable message is `appended`. Past the common prefix, messages still match
 * by identity, so a message inserted before one the proposer read, or the tail
 * a compaction keeps, counts only what actually came or went.
 */
export function measureRefineEvidenceDrift(
	snapshot: RefineEvidenceSnapshot,
	live: readonly AgentMessage[],
	leafId: string | null,
): RefineEvidenceDrift {
	const before = snapshot.messages;
	const limit = Math.min(before.length, live.length);
	let common = 0;
	while (common < limit && before[common] === live[common]) common++;
	const beforeTail = before.slice(common);
	const liveTail = live.slice(common);
	const kept = new Set(liveTail);
	const read = new Set(beforeTail);
	const removedMessages = visibleMessages(beforeTail.filter((message) => !kept.has(message)));
	const appended = liveTail.filter((message) => !read.has(message));
	const appendedMessages = visibleMessages(appended);
	return {
		kind: removedMessages > 0 ? "rewritten" : appendedMessages > 0 ? "appended" : "none",
		snapshotMessages: before.length,
		judgeMessages: live.length,
		appendedMessages,
		removedMessages,
		appendedChars: llmText(appended).length,
		snapshotLeafId: snapshot.leafId,
		judgeLeafId: leafId,
	};
}

/**
 * Whether a gate rejection was made on evidence that moved while the proposal
 * was planned: the judge itself refused it (not the structural screen, not a
 * judge error) and no referee verdict speaks against the claim. A mechanical
 * verdict holds whatever the conversation did, so it is never stale.
 */
export function isStaleEvidenceRejection(report: RavoGateReport, drift: RefineEvidenceDrift | undefined): boolean {
	if (drift === undefined || drift.kind === "none") return false;
	if (report.judgeError !== undefined || !STALE_EVIDENCE_DECISIONS.includes(report.decision)) return false;
	const counts = report.refereeCounts;
	return counts.upheld + counts.unverifiable + counts.no_evidence === 0;
}
