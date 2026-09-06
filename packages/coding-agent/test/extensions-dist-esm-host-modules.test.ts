import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as hostAi from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { discoverAndLoadExtensions } from "../src/core/extensions/loader.js";
import { ExtensionRunner } from "../src/core/extensions/runner.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";

/**
 * Regression: a prebuilt extension (`"type": "module"` package with a
 * `dist/index.js` entry) must share the host's pi-ai instance exactly like a
 * `.ts` extension does.
 *
 * jiti 2.x hands an ESM `.js` file imported asynchronously to Node's native
 * loader instead of transpiling it, and a native import never consults the
 * loader's virtualModules. The extension's `@earendil-works/pi-ai` then
 * resolved to its own node_modules copy: host-registered API providers were
 * invisible to it, providers it registered were invisible to the host, and
 * spans it opened reported to a sink nobody had installed. The decoy
 * `node_modules/@earendil-works/pi-ai` below reproduces that layout; before
 * the loader fix every assertion here failed against the decoy.
 *
 * `hostAi` is the instance the loader's virtualModules serve for
 * "@earendil-works/pi-ai" (vitest aliases both to ai/src).
 */
describe("prebuilt ESM extension host module sharing", () => {
	let tempDir: string;
	let extensionsDir: string;
	let sessionManager: SessionManager;
	let modelRegistry: ModelRegistry;
	let cleanups: Array<() => void>;

	beforeEach(() => {
		tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-dist-esm-ext-test-"));
		extensionsDir = path.join(tempDir, "extensions");
		fs.mkdirSync(extensionsDir);
		sessionManager = SessionManager.inMemory();
		const authStorage = AuthStorage.create(path.join(tempDir, "auth.json"));
		modelRegistry = ModelRegistry.create(authStorage);
		cleanups = [];
	});

	afterEach(() => {
		for (const cleanup of cleanups.splice(0)) cleanup();
		fs.rmSync(tempDir, { recursive: true, force: true });
	});

	function writeDistExtension(name: string, files: Record<string, string>): string {
		const packageDir = path.join(extensionsDir, name);
		fs.mkdirSync(path.join(packageDir, "dist"), { recursive: true });
		fs.writeFileSync(
			path.join(packageDir, "package.json"),
			JSON.stringify({ name, type: "module", pi: { extensions: ["./dist/index.js"] } }),
		);
		// Decoy copy of pi-ai: what a native import of the bare specifier resolves to.
		const decoyDir = path.join(packageDir, "node_modules", "@earendil-works", "pi-ai");
		fs.mkdirSync(decoyDir, { recursive: true });
		fs.writeFileSync(
			path.join(decoyDir, "package.json"),
			JSON.stringify({ name: "@earendil-works/pi-ai", version: "0.0.0-decoy", type: "module", main: "index.js" }),
		);
		fs.writeFileSync(
			path.join(decoyDir, "index.js"),
			[
				"export const DECOY = true;",
				"export function getApiProvider() { return undefined; }",
				"export function registerApiProvider() {}",
				"export function withSpan(name, attrs, fn) { return typeof attrs === 'function' ? attrs({ setAttributes() {}, recordError() {}, end() {} }) : fn({ setAttributes() {}, recordError() {}, end() {} }); }",
			].join("\n"),
		);
		for (const [file, content] of Object.entries(files)) {
			fs.writeFileSync(path.join(packageDir, file), content);
		}
		return packageDir;
	}

	async function loadAndRun(toolName: string) {
		const result = await discoverAndLoadExtensions([], tempDir, tempDir);
		expect(result.errors).toEqual([]);
		const runner = new ExtensionRunner(result.extensions, result.runtime, tempDir, sessionManager, modelRegistry);
		const tool = runner.getAllRegisteredTools().find((t) => t.definition.name === toolName);
		expect(tool).toBeDefined();
		const outcome = await tool!.definition.execute(
			"call-1",
			{},
			undefined as never,
			undefined as never,
			runner.createContext(),
		);
		return outcome.details as Record<string, unknown>;
	}

	it("shares the host API registry in both directions, including nested .js modules", async () => {
		const faux = hostAi.registerFauxProvider();
		cleanups.push(faux.unregister);
		cleanups.push(() => hostAi.unregisterApiProviders("dist-esm-probe"));

		writeDistExtension("dist-probe", {
			"dist/index.js": `
				import { DECOY, getApiProvider } from "@earendil-works/pi-ai";
				import { registerNestedProvider } from "./nested.js";

				export default function (pi) {
					registerNestedProvider();
					pi.registerTool({
						name: "dist_registry_probe",
						label: "Dist registry probe",
						description: "Reports which pi-ai instance a prebuilt extension sees",
						parameters: { type: "object", properties: {} },
						async execute() {
							const details = {
								decoy: DECOY === true,
								hostProviderVisible: getApiProvider(${JSON.stringify(faux.api)}) !== undefined,
							};
							return { content: [{ type: "text", text: JSON.stringify(details) }], details };
						},
					});
				}
			`,
			"dist/nested.js": `
				import { registerApiProvider } from "@earendil-works/pi-ai";
				export function registerNestedProvider() {
					registerApiProvider(
						{ api: "dist-esm-probe-api", stream: async function* () {}, streamSimple: async function* () {} },
						"dist-esm-probe",
					);
				}
			`,
		});

		const details = await loadAndRun("dist_registry_probe");
		expect(details).toEqual({ decoy: false, hostProviderVisible: true });
		expect(hostAi.getApiProvider("dist-esm-probe-api" as hostAi.Api)).toBeDefined();
	});

	it("reports spans opened by a prebuilt extension to the host span sink", async () => {
		const spans: Array<{ name: string; attrs: Record<string, unknown> }> = [];
		hostAi.setSpanSink((record) => spans.push({ name: record.name, attrs: record.attrs }));
		cleanups.push(() => hostAi.setSpanSink(undefined));

		writeDistExtension("dist-span-probe", {
			"dist/index.js": `
				import { withSpan } from "@earendil-works/pi-ai";

				export default function (pi) {
					pi.registerTool({
						name: "dist_span_probe",
						label: "Dist span probe",
						description: "Opens a span through the extension's pi-ai import",
						parameters: { type: "object", properties: {} },
						async execute() {
							const value = withSpan("dist-ext.probe", { "probe.kind": "dist" }, () => 42);
							return { content: [{ type: "text", text: String(value) }], details: { value } };
						},
					});
				}
			`,
		});

		const details = await loadAndRun("dist_span_probe");
		expect(details).toEqual({ value: 42 });
		expect(spans.filter((span) => span.name === "dist-ext.probe")).toEqual([
			{ name: "dist-ext.probe", attrs: { "probe.kind": "dist" } },
		]);
	});

	it("still loads a symlinked prebuilt extension whose dependencies live next to its real location", async () => {
		// Real package lives outside the extensions dir with its dependency
		// installed there; the extensions dir only holds a symlink to it.
		const realDir = path.join(tempDir, "real-packages", "linked-ext");
		fs.mkdirSync(path.join(realDir, "dist"), { recursive: true });
		fs.mkdirSync(path.join(realDir, "node_modules", "dep"), { recursive: true });
		fs.writeFileSync(
			path.join(realDir, "package.json"),
			JSON.stringify({ name: "linked-ext", type: "module", pi: { extensions: ["./dist/index.js"] } }),
		);
		fs.writeFileSync(
			path.join(realDir, "node_modules", "dep", "package.json"),
			JSON.stringify({ name: "dep", type: "module", main: "index.js" }),
		);
		fs.writeFileSync(path.join(realDir, "node_modules", "dep", "index.js"), "export const depValue = 'from-dep';");
		fs.writeFileSync(
			path.join(realDir, "dist", "index.js"),
			`
				import { depValue } from "dep";
				export default function (pi) {
					pi.registerTool({
						name: "linked_probe",
						label: "Linked probe",
						description: "Reports a value from a dependency of a symlinked extension",
						parameters: { type: "object", properties: {} },
						async execute() {
							return { content: [{ type: "text", text: depValue }], details: { depValue } };
						},
					});
				}
			`,
		);
		fs.symlinkSync(realDir, path.join(extensionsDir, "linked-ext"), "dir");

		const details = await loadAndRun("linked_probe");
		expect(details).toEqual({ depValue: "from-dep" });
	});
});
