import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { discoverAndLoadExtensions } from "../src/core/extensions/loader.js";
import { ExtensionRunner } from "../src/core/extensions/runner.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";

/**
 * Regression: an extension importing "@earendil-works/pi-ai/compat" must share
 * the pi-ai module instance the loader serves to extensions (ai/dist), and
 * therefore its API-provider registry. Before the /compat alias + subpath
 * existed, the specifier escaped the loader's module map: in dev mode the
 * import failed to resolve, and in bundled mode it loaded a second pi-ai copy
 * whose registry lacked every host-registered custom provider, so
 * workflow-style extensions failed with
 * "No API provider registered for api: <api>".
 *
 * The faux provider is registered on the dist entry directly because that is
 * the instance the loader aliases extensions to; vitest itself aliases
 * "@earendil-works/pi-ai" to ai/src, which is a different module instance and
 * would make this test pass or fail for the wrong reason.
 */
describe("extension pi-ai/compat registry sharing", () => {
	let tempDir: string;
	let extensionsDir: string;
	let sessionManager: SessionManager;
	let modelRegistry: ModelRegistry;
	let unregister: (() => void) | undefined;

	beforeEach(() => {
		tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-compat-registry-test-"));
		extensionsDir = path.join(tempDir, "extensions");
		fs.mkdirSync(extensionsDir);
		sessionManager = SessionManager.inMemory();
		const authStorage = AuthStorage.create(path.join(tempDir, "auth.json"));
		modelRegistry = ModelRegistry.create(authStorage);
	});

	afterEach(() => {
		unregister?.();
		unregister = undefined;
		fs.rmSync(tempDir, { recursive: true, force: true });
	});

	it("resolves /compat imports to the loader-served registry with custom providers visible", async () => {
		const distAi = (await import("../../ai/dist/index.js")) as unknown as typeof import("@earendil-works/pi-ai");
		const faux = distAi.registerFauxProvider();
		unregister = faux.unregister;
		expect(distAi.getApiProvider(faux.api)).toBeDefined();

		const extCode = `
			import { getApiProvider } from "@earendil-works/pi-ai/compat";

			export default function (pi) {
				pi.registerTool({
					name: "compat_registry_probe",
					label: "Compat registry probe",
					description: "Reports whether a host-registered API provider is visible through /compat",
					parameters: { type: "object", properties: {} },
					async execute() {
						const visible = getApiProvider(${JSON.stringify(faux.api)}) !== undefined;
						return { content: [{ type: "text", text: String(visible) }], details: { visible } };
					},
				});
			}
		`;
		fs.writeFileSync(path.join(extensionsDir, "compat-probe.ts"), extCode);

		const result = await discoverAndLoadExtensions([], tempDir, tempDir);
		expect(result.errors).toEqual([]);

		const runner = new ExtensionRunner(result.extensions, result.runtime, tempDir, sessionManager, modelRegistry);
		const tool = runner.getAllRegisteredTools().find((t) => t.definition.name === "compat_registry_probe");
		expect(tool).toBeDefined();

		const outcome = await tool!.definition.execute(
			"call-1",
			{},
			undefined as never,
			undefined as never,
			runner.createContext(),
		);
		expect(outcome.details).toEqual({ visible: true });
	});
});
