// `@earendil-works/pi-ai` for the golden generator: a scripted
// `completeSimple` that records every judge request, a no-op logger, and a
// `withSpan` that runs its body.
export interface JudgeRequest {
	systemPrompt: string;
	prompt: string;
	maxTokens: number;
}
export const judgeRequests: JudgeRequest[] = [];
let nextReply: { text?: string; error?: string } = {};
export function setJudgeReply(reply: { text?: string; error?: string }): void {
	nextReply = reply;
}
export async function completeSimple(
	_model: unknown,
	context: { systemPrompt: string; messages: { content: { text: string }[] }[] },
	options: { maxTokens: number },
): Promise<unknown> {
	judgeRequests.push({
		systemPrompt: context.systemPrompt,
		prompt: context.messages[0].content[0].text,
		maxTokens: options.maxTokens,
	});
	if (nextReply.error !== undefined) {
		return { stopReason: "error", errorMessage: nextReply.error, content: [] };
	}
	return { stopReason: "stop", content: [{ type: "text", text: nextReply.text ?? "" }] };
}
export function getLogger(): { info: () => void } {
	return { info: () => undefined };
}
export async function withSpan<T>(_name: string, _attributes: unknown, body: (span: unknown) => T): Promise<Awaited<T>> {
	return await body({ setAttributes: () => undefined });
}
