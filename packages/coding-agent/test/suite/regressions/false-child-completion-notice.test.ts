import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import type { CustomMessage } from "../../../src/core/messages.js";
import { createHarness, type Harness } from "../harness.js";

function terminalNotices(messages: readonly unknown[]): CustomMessage[] {
	return messages.filter(
		(message): message is CustomMessage =>
			typeof message === "object" &&
			message !== null &&
			(message as { role?: unknown }).role === "custom" &&
			(message as { customType?: unknown }).customType === "rlm_child_terminal_notice",
	);
}

describe("false child completion notice", () => {
	let parent: Harness | undefined;
	let child: Harness | undefined;

	afterEach(() => {
		child?.cleanup();
		parent?.cleanup();
		child = undefined;
		parent = undefined;
	});

	it("does not deliver a stale completion notice for a child that kept working", async () => {
		child = await createHarness({ rlmDepth: 1 });
		parent = await createHarness({
			rlmDepth: 0,
			rlmMaxDepth: 1,
			subagentRuntimeHost: {
				createRlmSubagentRuntime: async () => ({ session: child!.session }),
				deleteRlmSubagentRuntime: async () => {},
			},
		});
		child.setResponses([fauxAssistantMessage("turn one: build started, report follows")]);
		parent.setResponses([fauxAssistantMessage("parent consumed the child result")]);

		// The parent is paused exactly like an ACP close, so the notice composed at
		// the end of the child's first turn stays deferred while the child works on.
		const inputPause = parent.session.acquireSessionInputPause();
		const spawned = await parent.session.runRlmChild("start a long build", { name: "wakeup-worker" });
		const deferredNotices = () =>
			parent!.session
				.getPendingNextTurnMessageSnapshots()
				.filter((message) => message.customType === "rlm_child_terminal_notice");
		await expect.poll(() => deferredNotices()).toHaveLength(1);

		child.setResponses([fauxAssistantMessage("final report: build passed")]);
		await child.session.prompt("wakeup: check the build");

		inputPause.release();
		expect(terminalNotices(parent.session.messages)).toHaveLength(0);
		await expect.poll(() => terminalNotices(parent!.session.messages), { timeout: 5000 }).toHaveLength(1);
		expect(terminalNotices(parent.session.messages)[0]).toMatchObject({
			details: {
				kind: "completed_without_reply",
				childId: spawned.rlm_child_id,
				lastAssistantTextPreview: "final report: build passed",
			},
		});
	});
});
