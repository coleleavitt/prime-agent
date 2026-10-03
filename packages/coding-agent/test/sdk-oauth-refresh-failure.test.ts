import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getModel } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { createExtensionRuntime } from "../src/core/extensions/loader.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import type { ResourceLoader } from "../src/core/resource-loader.js";
import { createAgentSession } from "../src/core/sdk.js";
import { SessionManager } from "../src/core/session-manager.js";

function emptyResourceLoader(): ResourceLoader {
	return {
		getExtensions: () => ({ extensions: [], errors: [], runtime: createExtensionRuntime() }),
		getSkills: () => ({ skills: [], diagnostics: [] }),
		getPrompts: () => ({ prompts: [], diagnostics: [] }),
		getThemes: () => ({ themes: [], diagnostics: [] }),
		getAgentsFiles: () => ({ agentsFiles: [] }),
		getSystemPrompt: () => undefined,
		getAppendSystemPrompt: () => [],
		extendResources: () => {},
		reload: async () => {},
	};
}

describe("createAgentSession stream auth", () => {
	let tempDir: string;

	beforeEach(() => {
		tempDir = mkdtempSync(join(tmpdir(), "pi-sdk-oauth-"));
	});

	afterEach(() => {
		vi.restoreAllMocks();
		rmSync(tempDir, { recursive: true, force: true });
	});

	it("reports a failed OAuth refresh instead of reaching the provider without a key", async () => {
		const authStorage = AuthStorage.inMemory({
			anthropic: { type: "oauth", access: "", refresh: "dead", expires: 0 },
		});
		const modelRegistry = ModelRegistry.inMemory(authStorage);
		// The refresh failed: auth resolution succeeds but yields no key.
		vi.spyOn(modelRegistry, "getApiKeyAndHeaders").mockResolvedValue({ ok: true, apiKey: undefined });
		const model = getModel("anthropic", "claude-opus-4-5");

		const { session } = await createAgentSession({
			cwd: tempDir,
			agentDir: tempDir,
			sessionManager: SessionManager.inMemory(),
			resourceLoader: emptyResourceLoader(),
			authStorage,
			modelRegistry,
			model,
		});

		const attempt = Promise.resolve().then(() =>
			session.agent.streamFn(model, { systemPrompt: "", messages: [] }, {}),
		);
		await expect(attempt).rejects.toThrow('Authentication failed for "anthropic"');
		await expect(attempt).rejects.toThrow("--no-extensions");
	});
});
