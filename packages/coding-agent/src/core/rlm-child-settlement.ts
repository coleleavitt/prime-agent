import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage } from "@earendil-works/pi-ai";

/**
 * Verdict on a child run whose prompt resolved. A resolved `promptAndWait()`
 * only proves the child's agent loop ended; it says nothing about whether the
 * terminal assistant message was a usable result. Providers can end a turn
 * with a provider error, an abort, or a nominal "stop" that carries no text
 * and no tool call (an upstream proxy that swallowed an error frame). None of
 * those may be settled as `done`, or the parent receives a
 * "completed without sending a reply" notice for work that never happened.
 */
export type RlmChildSettlement = { ok: true } | { ok: false; reason: string };

export interface RlmChildSettlementInput {
	/** Child transcript after the prompt resolved. */
	messages: readonly AgentMessage[];
	/** `details.id` of the spawn message; only messages after it are judged. */
	spawnMessageId: string;
	/** True when the child sent at least one reply to the parent during the run. */
	repliedToParent: boolean;
}

function hasDeliverableContent(message: AssistantMessage): boolean {
	return message.content.some(
		(block) => (block.type === "text" && block.text.trim().length > 0) || block.type === "toolCall",
	);
}

function describeUsage(message: AssistantMessage): string {
	return `stopReason=${message.stopReason} usage=${message.usage?.totalTokens ?? 0}`;
}

function findSpawnIndex(messages: readonly AgentMessage[], spawnMessageId: string): number {
	for (let i = messages.length - 1; i >= 0; i--) {
		const message = messages[i];
		if (
			message?.role === "custom" &&
			typeof (message as { details?: { id?: unknown } }).details?.id === "string" &&
			(message as { details: { id: string } }).details.id === spawnMessageId
		) {
			return i;
		}
	}
	return -1;
}

/**
 * Judge the terminal assistant message of a child run. Fail-closed: a run is
 * `ok` only when its last assistant turn after the spawn prompt ended
 * normally and carried text or a tool call, or when the child already
 * delivered its result as a reply to the parent.
 */
export function assessRlmChildSettlement(input: RlmChildSettlementInput): RlmChildSettlement {
	const start = findSpawnIndex(input.messages, input.spawnMessageId) + 1;
	let terminal: AssistantMessage | undefined;
	for (let i = input.messages.length - 1; i >= start; i--) {
		const message = input.messages[i];
		if (message?.role === "assistant") {
			terminal = message as AssistantMessage;
			break;
		}
	}
	if (!terminal) {
		return input.repliedToParent
			? { ok: true }
			: { ok: false, reason: "child produced no assistant response for the task" };
	}
	if (terminal.stopReason === "error") {
		return {
			ok: false,
			reason: `child ended with a provider error: ${terminal.errorMessage ?? "unknown error"}`,
		};
	}
	if (terminal.stopReason === "aborted") {
		return {
			ok: false,
			reason: `child turn was aborted: ${terminal.errorMessage ?? "no reason recorded"}`,
		};
	}
	if (!hasDeliverableContent(terminal) && !input.repliedToParent) {
		return {
			ok: false,
			reason: `child ended with an empty assistant response (${describeUsage(terminal)}, model=${terminal.model}); the provider returned no text and no tool call, so the task did not complete`,
		};
	}
	return { ok: true };
}
