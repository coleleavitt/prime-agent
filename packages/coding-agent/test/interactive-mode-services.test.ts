import { describe, expect, it, vi } from "vitest";
import type { AgentSession } from "../src/core/agent-session.js";
import type { AgentSessionServices } from "../src/core/agent-session-services.js";
import type { SessionManager } from "../src/core/session-manager.js";
import {
	createInteractiveModeUiServices,
	createInteractiveModeUiServicesFromServices,
} from "../src/modes/interactive/interactive-mode-services.js";

describe("InteractiveModeUiServices MCP refresh", () => {
	it("wires local sessions through the narrow session refresh method", () => {
		const refreshMcpProviders = vi.fn();
		const session = {
			settingsManager: {},
			modelRegistry: {},
			sessionManager: { getCwd: () => "/local", getSessionName: () => "local-session" },
			resourceLoader: { getThemes: () => ({ themes: [] }) },
			refreshMcpProviders,
		} as unknown as AgentSession;

		const services = createInteractiveModeUiServices(session);
		services.refreshMcpProviders?.();

		expect(refreshMcpProviders).toHaveBeenCalledOnce();
		expect(services.getInitialCwd()).toBe("/local");
	});

	it("wires daemon-backed services directly to the MCP manager", () => {
		const refresh = vi.fn();
		const services = createInteractiveModeUiServicesFromServices({
			services: {
				settingsManager: {},
				modelRegistry: {},
				resourceLoader: { getThemes: () => ({ themes: [] }) },
				mcpManager: { refresh },
			} as unknown as AgentSessionServices,
			sessionManager: {
				getCwd: () => "/daemon",
				getSessionName: () => "daemon-session",
			} as unknown as SessionManager,
		});

		services.refreshMcpProviders?.();

		expect(refresh).toHaveBeenCalledOnce();
		expect(services.getInitialSessionName()).toBe("daemon-session");
	});

	it("exposes locally loaded extension shortcuts to a daemon-backed UI client", () => {
		const shortcut = {
			shortcut: "ctrl+space",
			extensionPath: "/extensions/voice.ts",
			description: "Start or stop voice dictation",
			handler: vi.fn(),
		};
		const extension = {
			path: "/extensions/voice.ts",
			shortcuts: new Map([["ctrl+space", shortcut]]),
		};
		const extensionResult = {
			extensions: [extension],
			errors: [],
			runtime: { flagValues: new Map(), pendingProviderRegistrations: [] },
		};
		const services = createInteractiveModeUiServicesFromServices({
			services: {
				cwd: "/daemon",
				settingsManager: {},
				modelRegistry: {},
				resourceLoader: {
					getThemes: () => ({ themes: [] }),
					getExtensions: () => extensionResult,
				},
				mcpManager: { refresh: vi.fn() },
			} as unknown as AgentSessionServices,
			sessionManager: {
				getCwd: () => "/daemon",
				getSessionName: () => "daemon-session",
			} as unknown as SessionManager,
		});

		const runner = services.getClientExtensionRunner?.();
		expect(runner).toBeDefined();
		expect(runner).toBe(services.getClientExtensionRunner?.());
		expect(runner?.getShortcuts({})).toEqual(new Map([["ctrl+space", shortcut]]));
	});
});
