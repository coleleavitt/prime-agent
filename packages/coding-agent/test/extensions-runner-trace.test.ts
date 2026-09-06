/**
 * Tests for the `extension.hooks` timing span opened by ExtensionRunner around
 * every emit* call.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { type SpanEndRecord, setSpanSink } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { createEventBus } from "../src/core/event-bus.js";
import { createExtensionRuntime, loadExtensionFromFactory } from "../src/core/extensions/loader.js";
import { ExtensionRunner, extensionSpanLabel } from "../src/core/extensions/runner.js";
import type { ExtensionError, ExtensionFactory, ExtensionRuntime } from "../src/core/extensions/types.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

describe("ExtensionRunner extension.hooks span", () => {
	let tempDir: string;
	let sessionManager: SessionManager;
	let modelRegistry: ModelRegistry;
	let runtime: ExtensionRuntime;
	let spans: SpanEndRecord[];

	beforeEach(() => {
		tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-runner-trace-test-"));
		sessionManager = SessionManager.inMemory();
		const authStorage = AuthStorage.create(path.join(tempDir, "auth.json"));
		modelRegistry = ModelRegistry.create(authStorage);
		runtime = createExtensionRuntime();
		spans = [];
		setSpanSink((record) => spans.push(record));
	});

	afterEach(() => {
		setSpanSink(undefined);
		fs.rmSync(tempDir, { recursive: true, force: true });
	});

	const hookSpans = () => spans.filter((span) => span.name === "extension.hooks");

	async function makeRunner(factories: Array<{ path: string; factory: ExtensionFactory }>): Promise<ExtensionRunner> {
		const eventBus = createEventBus();
		const extensions = [];
		for (const { path: extensionPath, factory } of factories) {
			extensions.push(await loadExtensionFromFactory(factory, tempDir, eventBus, runtime, extensionPath));
		}
		return new ExtensionRunner(extensions, runtime, tempDir, sessionManager, modelRegistry);
	}

	it("opens no span when no extension handles the event", async () => {
		const runner = await makeRunner([
			{
				path: "/ext/other.ts",
				factory: (pi) => {
					pi.on("tool_call", () => undefined);
				},
			},
		]);

		const messages = await runner.emitContext([]);
		await runner.emitBeforeProviderRequest({ payload: 1 });
		await runner.emitInput("hi", undefined, "interactive");

		expect(messages).toEqual([]);
		expect(hookSpans()).toEqual([]);
	});

	it("records one span per emit with the event type and handler count", async () => {
		const runner = await makeRunner([
			{
				path: "/ext/first.ts",
				factory: (pi) => {
					pi.on("context", (event) => ({ messages: [...event.messages] }));
				},
			},
			{
				path: "/ext/second.ts",
				factory: (pi) => {
					pi.on("context", () => undefined);
				},
			},
		]);

		await runner.emitContext([]);

		const recorded = hookSpans();
		expect(recorded).toHaveLength(1);
		const span = recorded[0]!;
		expect(span.status).toBe("ok");
		expect(span.attrs["hook.event"]).toBe("context");
		expect(span.attrs["hook.handlers"]).toBe(2);
		expect(span.attrs["hook.errors"]).toBeUndefined();
		expect(typeof span.attrs["hook.slowest"]).toBe("string");
		expect(typeof span.attrs["hook.slowest_ms"]).toBe("number");
		expect(span.durationMs).toBeGreaterThanOrEqual(0);
	});

	it("attributes a slow handler to its extension", async () => {
		const runner = await makeRunner([
			{
				path: "/ext/fast-one.ts",
				factory: (pi) => {
					pi.on("before_provider_request", () => undefined);
				},
			},
			{
				path: "/ext/slow-refresh.ts",
				factory: (pi) => {
					pi.on("before_provider_request", async (event) => {
						await sleep(35);
						return { ...(event.payload as object), refreshed: true };
					});
				},
			},
		]);

		const payload = await runner.emitBeforeProviderRequest({ original: true });

		expect(payload).toEqual({ original: true, refreshed: true });
		const recorded = hookSpans();
		expect(recorded).toHaveLength(1);
		const span = recorded[0]!;
		expect(span.attrs["hook.event"]).toBe("before_provider_request");
		expect(span.attrs["hook.handlers"]).toBe(2);
		expect(span.attrs["hook.slowest"]).toBe("slow-refresh");
		expect(span.attrs["hook.slowest_ms"]).toBeGreaterThanOrEqual(25);
		expect(span.attrs["hook.slow-refresh_ms"]).toBeGreaterThanOrEqual(25);
		expect(span.attrs["hook.fast-one_ms"]).toBeUndefined();
		expect(span.durationMs).toBeGreaterThanOrEqual(span.attrs["hook.slowest_ms"] as number);
	});

	it("counts throwing handlers in hook.errors and keeps the span ok", async () => {
		const runner = await makeRunner([
			{
				path: "/ext/broken.ts",
				factory: (pi) => {
					pi.on("context", () => {
						throw new Error("boom");
					});
				},
			},
			{
				path: "/ext/healthy.ts",
				factory: (pi) => {
					pi.on("context", (event) => ({ messages: event.messages }));
				},
			},
		]);
		const errors: ExtensionError[] = [];
		runner.onError((error) => errors.push(error));

		const messages = await runner.emitContext([]);

		expect(messages).toEqual([]);
		expect(errors).toHaveLength(1);
		expect(errors[0]).toMatchObject({ extensionPath: "/ext/broken.ts", event: "context", error: "boom" });
		const recorded = hookSpans();
		expect(recorded).toHaveLength(1);
		const span = recorded[0]!;
		expect(span.status).toBe("ok");
		expect(span.error).toBeUndefined();
		expect(span.attrs["hook.errors"]).toBe(1);
		expect(span.attrs["hook.handlers"]).toBe(2);
	});

	it("still short-circuits on a blocking tool_call result and counts only invoked handlers", async () => {
		let secondCalled = false;
		const runner = await makeRunner([
			{
				path: "/ext/blocker.ts",
				factory: (pi) => {
					pi.on("tool_call", () => ({ block: true, reason: "nope" }));
				},
			},
			{
				path: "/ext/never.ts",
				factory: (pi) => {
					pi.on("tool_call", () => {
						secondCalled = true;
						return undefined;
					});
				},
			},
		]);

		const result = await runner.emitToolCall({
			type: "tool_call",
			toolName: "custom_tool",
			toolCallId: "call-1",
			input: {},
		});

		expect(result).toEqual({ block: true, reason: "nope" });
		expect(secondCalled).toBe(false);
		const recorded = hookSpans();
		expect(recorded).toHaveLength(1);
		expect(recorded[0]!.attrs["hook.handlers"]).toBe(1);
		expect(recorded[0]!.attrs["hook.slowest"]).toBe("blocker");
	});

	it("uses the generic emit path for session lifecycle events", async () => {
		const runner = await makeRunner([
			{
				path: "/ext/pkg/index.ts",
				factory: (pi) => {
					pi.on("session_before_compact", () => ({ cancel: true }));
				},
			},
		]);

		const result = await runner.emit({
			type: "session_before_compact",
			preparation: { messagesToSummarize: [], turnPrefixMessages: [], fileOps: [] } as never,
			branchEntries: [],
			customInstructions: undefined,
			signal: undefined,
		} as never);

		expect(result).toEqual({ cancel: true });
		const recorded = hookSpans();
		expect(recorded).toHaveLength(1);
		expect(recorded[0]!.attrs["hook.event"]).toBe("session_before_compact");
		expect(recorded[0]!.attrs["hook.slowest"]).toBe("pkg");
	});

	it("sanitises extension labels", () => {
		expect(extensionSpanLabel("/home/me/.pi/extensions/my ext.v2.ts")).toBe("my_ext_v2");
		expect(extensionSpanLabel("<inline>")).toBe("inline");
		expect(extensionSpanLabel("C:\\ext\\dir\\index.js")).toBe("dir");
		expect(extensionSpanLabel("")).toBe("extension");
	});
});
