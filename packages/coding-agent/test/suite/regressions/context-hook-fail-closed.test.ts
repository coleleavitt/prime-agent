import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import type { ExtensionAPI } from "../../../src/core/extensions/types.js";
import { createHarness } from "../harness.js";

describe("context hook fail-closed contract", () => {
	it("does not call the provider when a context hook throws a branded blocking error", async () => {
		const harness = await createHarness({
			extensionFactories: [
				(pi: ExtensionAPI) => {
					pi.on("context", () => {
						throw Object.assign(new Error("context is unsafe to serve"), {
							code: "EXTENSION_CONTEXT_BLOCKED" as const,
						});
					});
				},
			],
		});
		harness.setResponses([fauxAssistantMessage("must not be consumed")]);
		try {
			await harness.session.prompt("unsafe context");
			expect(harness.getPendingResponseCount()).toBe(1);
			expect(harness.session.messages.at(-1)).toMatchObject({
				role: "assistant",
				stopReason: "error",
				errorMessage: expect.stringContaining("context is unsafe to serve"),
			});
		} finally {
			harness.cleanup();
		}
	});
});
