import { existsSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	Agent,
	type AgentOptions,
	type AgentTool,
	type StreamFn,
	type ThinkingLevel,
} from "@earendil-works/pi-agent-core";
import { type AssistantMessage, createAssistantMessageEventStream, getModel, type Usage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AgentSession, type AgentSessionConfig } from "../src/core/agent-session.js";
import { AuthStorage } from "../src/core/auth-storage.js";
import { createExtensionRuntime } from "../src/core/extensions/loader.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import type { ResourceLoader } from "../src/core/resource-loader.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import {
	attestWorkflowV2RetainedProfile,
	createWorkflowV2RetainedChildRuntime,
	WORKFLOW_V2_TOOLS_NONE_MAX_TURNS,
	WORKFLOW_V2_TOOLS_NONE_PROFILE,
	type WorkflowV2BoundRetainedAdmission,
	type WorkflowV2ParentTransportAuthority,
	WorkflowV2RetainedProfileError,
} from "../src/core/workflow-v2-retained-profile.js";

const model = getModel("anthropic", "claude-sonnet-4-5")!;

function usage(input = 5, output = 2): Usage {
	return {
		input,
		output,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: input + output,
		cost: { input, output, cacheRead: 0, cacheWrite: 0, total: input + output },
	};
}

/** A streamFn that returns exactly one assistant text answer and counts calls. */
function countingTextStream(): { streamFn: StreamFn; calls: () => number } {
	let calls = 0;
	const streamFn: StreamFn = (streamModel) => {
		calls += 1;
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => {
			const message: AssistantMessage = {
				role: "assistant",
				content: [{ type: "text", text: "ok" }],
				api: streamModel.api,
				provider: streamModel.provider,
				model: streamModel.id,
				usage: usage(),
				stopReason: "stop",
				timestamp: Date.now(),
			};
			stream.push({ type: "done", reason: "stop", message });
		});
		return stream;
	};
	return { streamFn, calls: () => calls };
}

interface TestEnv {
	tempDir: string;
	settingsManager: SettingsManager;
	modelRegistry: ModelRegistry;
	cleanups: Array<() => void>;
}

const environments: TestEnv[] = [];

function makeEnv(): TestEnv {
	const tempDir = join(tmpdir(), `wf-v2-profile-${Date.now()}-${Math.random().toString(36).slice(2)}`);
	mkdirSync(tempDir, { recursive: true });
	const settingsManager = SettingsManager.create(tempDir, tempDir);
	const authStorage = AuthStorage.create(join(tempDir, "auth.json"));
	const modelRegistry = ModelRegistry.create(authStorage, tempDir);
	const env: TestEnv = { tempDir, settingsManager, modelRegistry, cleanups: [] };
	environments.push(env);
	return env;
}

function admissionFor(
	env: TestEnv,
	overrides: Partial<WorkflowV2BoundRetainedAdmission> = {},
): WorkflowV2BoundRetainedAdmission {
	const childId = overrides.rlmChildId ?? `child-${Math.random().toString(36).slice(2)}`;
	return {
		rlmChildId: childId,
		sessionName: overrides.sessionName ?? "wf-child",
		sessionDir: overrides.sessionDir ?? join(env.tempDir, childId),
		cwd: env.tempDir,
		model,
		thinkingLevel: "off",
		serviceTier: "default",
		rlmDepth: 1,
		modelRegistry: env.modelRegistry,
		settingsManager: env.settingsManager,
		...overrides,
	};
}

function transportFor(): WorkflowV2ParentTransportAuthority & { calls: () => number } {
	const { streamFn, calls } = countingTextStream();
	return { streamFn, getApiKey: () => "test-key", calls };
}

/** Empty isolated loader matching the factory's, for building baseline mutant sessions directly. */
function emptyLoader(): ResourceLoader {
	const extensionsResult = { extensions: [], errors: [], runtime: createExtensionRuntime() };
	return {
		getExtensions: () => extensionsResult,
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

/**
 * Build a direct baseline tools-none child config identical to the factory's, so
 * mutation tests can flip exactly one field and prove the attestation guard
 * rejects it. Not the production path; the production path is the factory.
 */
function baselineConfig(env: TestEnv, agentOverrides: Partial<AgentOptions> = {}): AgentSessionConfig {
	const sessionDir = join(env.tempDir, `direct-${Math.random().toString(36).slice(2)}`);
	const sessionManager = SessionManager.create(env.tempDir, sessionDir);
	const agent = new Agent({
		initialState: {
			systemPrompt: "",
			model,
			thinkingLevel: "off" as ThinkingLevel,
			serviceTier: "default",
			tools: [],
		},
		streamFn: countingTextStream().streamFn,
		getApiKey: () => "test-key",
		shouldStopAfterTurn: () => true,
		toolCallPolicy: "reject",
		...agentOverrides,
	});
	return {
		agent,
		sessionManager,
		settingsManager: env.settingsManager,
		cwd: env.tempDir,
		modelRegistry: env.modelRegistry,
		resourceLoader: emptyLoader(),
		baseToolsOverride: Object.freeze({}),
		scopedModels: [],
		customTools: [],
		initialActiveToolNames: [],
		allowedToolNames: [],
		includeGoals: false,
		includeCompactSkill: false,
		prewarmIpythonKernel: false,
		serializedRefine: false,
		rlmDepth: 1,
		rlmMaxDepth: 1,
		rlmSessionDir: sessionDir,
		sessionStartEvent: { type: "session_start", reason: "startup" },
	};
}

function buildBaseline(env: TestEnv, config: AgentSessionConfig): AgentSession {
	const session = new AgentSession(config);
	// Match the factory: re-freeze tools after construction rebuilt the registry.
	Object.freeze(session.agent.state.tools);
	env.cleanups.push(() => session.dispose());
	return session;
}

afterEach(() => {
	for (const env of environments.splice(0)) {
		for (const cleanup of env.cleanups.splice(0)) {
			try {
				cleanup();
			} catch {
				// best-effort teardown
			}
		}
		if (existsSync(env.tempDir)) rmSync(env.tempDir, { recursive: true, force: true });
	}
	vi.restoreAllMocks();
});

describe("workflow-v2 retained tools-none profile: construction", () => {
	it("constructs a retained child whose attestation proves the full closed profile", () => {
		const env = makeEnv();
		const transport = transportFor();
		const { runtime, attestation } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transport);
		env.cleanups.push(() => runtime.session.dispose());

		expect(attestation.profile).toBe(WORKFLOW_V2_TOOLS_NONE_PROFILE);
		expect(attestation.tools).toBe("none");
		expect(attestation.maxTurns).toBe(WORKFLOW_V2_TOOLS_NONE_MAX_TURNS);
		expect(attestation.toolsEmpty).toBe(true);
		expect(attestation.toolsFrozen).toBe(true);
		expect(attestation.toolCallPolicy).toBe("reject");
		expect(attestation.continuationDisabled).toBe(true);
		expect(attestation.activeToolNames).toEqual([]);
		expect(attestation.kernelProvisioned).toBe(false);
		expect(attestation.kernelSnapshotAssigned).toBe(false);
		expect(attestation.extensionCount).toBe(0);
		expect(attestation.resourcesEmpty).toBe(true);
		expect(attestation.controllersInstalled).toBe(false);
		expect(attestation.mcpInstalled).toBe(false);
		expect(attestation.goalsIncluded).toBe(false);
		expect(attestation.compactSkillIncluded).toBe(false);
		expect(attestation.autonomousEnabled).toBe(false);
		expect(attestation.serializedRefine).toBe(false);
		expect(attestation.descendantsAllowed).toBe(false);
		expect(attestation.rlmDepth).toBe(1);
		expect(attestation.rlmMaxDepth).toBe(1);
		expect(attestation.parentHooksReused).toBe(false);
		expect(attestation.effectiveModel).toEqual({ provider: model.provider, id: model.id });
		expect(attestation.effectiveThinkingLevel).toBe("off");
	});

	it("freezes the low-level tools array and makes toolCallPolicy immutable", () => {
		const env = makeEnv();
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		env.cleanups.push(() => runtime.session.dispose());
		const agent = runtime.session.agent;
		expect(Object.isFrozen(agent.state.tools)).toBe(true);
		expect(() => {
			(agent.state.tools as AgentTool[]).push({} as AgentTool);
		}).toThrow();
		// toolCallPolicy is defined non-writable, non-configurable.
		expect(() => {
			(agent as unknown as { toolCallPolicy: string }).toolCallPolicy = "execute";
		}).toThrow();
		expect((agent as unknown as { toolCallPolicy: string }).toolCallPolicy).toBe("reject");
	});

	it("does not construct an IpythonKernelProvisioner", async () => {
		const ipython = await import("../src/core/tools/ipython.js");
		const spy = vi.spyOn(ipython, "IpythonKernelProvisioner");
		const env = makeEnv();
		const { runtime, attestation } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		env.cleanups.push(() => runtime.session.dispose());
		expect(spy).not.toHaveBeenCalled();
		expect(attestation.kernelProvisioned).toBe(false);
	});

	it("cannot activate any tool: setActiveToolsByName leaves the registry empty", () => {
		const env = makeEnv();
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		env.cleanups.push(() => runtime.session.dispose());
		runtime.session.setActiveToolsByName(["ipython", "bash", "read"]);
		expect(runtime.session.getActiveToolNames()).toEqual([]);
		expect(runtime.session.getAllTools()).toEqual([]);
	});

	it("forbids descendants: rlmDepth equals rlmMaxDepth", () => {
		const env = makeEnv();
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env, { rlmDepth: 3 }), transportFor());
		env.cleanups.push(() => runtime.session.dispose());
		expect(runtime.session.rlmDepth).toBe(3);
		expect(runtime.session.rlmMaxDepth).toBe(3);
	});

	it("reuses no parent context/payload/response hook", () => {
		const env = makeEnv();
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		env.cleanups.push(() => runtime.session.dispose());
		expect(runtime.session.agent.transformContext).toBeUndefined();
		expect(runtime.session.agent.onPayload).toBeUndefined();
		expect(runtime.session.agent.onResponse).toBeUndefined();
	});

	it("keeps isolated resources empty before and after reload; rejects extendResources", async () => {
		const env = makeEnv();
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		env.cleanups.push(() => runtime.session.dispose());
		const loader = (runtime.session as unknown as { _resourceLoader: ResourceLoader })._resourceLoader;
		expect(loader.getExtensions().extensions).toEqual([]);
		expect(loader.getSkills().skills).toEqual([]);
		await loader.reload();
		expect(loader.getExtensions().extensions).toEqual([]);
		expect(loader.getSkills().skills).toEqual([]);
		expect(() => loader.extendResources({})).toThrow(WorkflowV2RetainedProfileError);
	});

	it("inline and daemon paths use one factory and produce equal attestation", () => {
		const env = makeEnv();
		const a = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		const b = createWorkflowV2RetainedChildRuntime(admissionFor(env), transportFor());
		env.cleanups.push(() => a.runtime.session.dispose());
		env.cleanups.push(() => b.runtime.session.dispose());
		const stripActive = (x: typeof a.attestation) => ({ ...x, activeToolNames: [...x.activeToolNames] });
		expect(stripActive(a.attestation)).toEqual(stripActive(b.attestation));
	});

	it("rejects an invalid admission before constructing anything", () => {
		const env = makeEnv();
		expect(() => createWorkflowV2RetainedChildRuntime(admissionFor(env, { rlmChildId: "" }), transportFor())).toThrow(
			WorkflowV2RetainedProfileError,
		);
		expect(() => createWorkflowV2RetainedChildRuntime(admissionFor(env, { rlmDepth: -1 }), transportFor())).toThrow(
			WorkflowV2RetainedProfileError,
		);
	});
});

describe("workflow-v2 retained tools-none profile: one-turn and reject behavior", () => {
	it("answers with exactly one provider request and stops after the turn", async () => {
		const env = makeEnv();
		const transport = transportFor();
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env), transport);
		env.cleanups.push(() => runtime.session.dispose());
		runtime.session.subscribe(() => {});
		await runtime.session.agent.prompt("hello");
		await runtime.session.agent.waitForIdle();
		expect(transport.calls()).toBe(1);
	});

	it("rejects a model-emitted tool call without executing any handler", async () => {
		const env = makeEnv();
		let calls = 0;
		const executed = vi.fn();
		const streamFn: StreamFn = (streamModel) => {
			calls += 1;
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => {
				const message: AssistantMessage = {
					role: "assistant",
					content: [{ type: "toolCall", id: "t1", name: "bash", arguments: { command: "echo hi" } }],
					api: streamModel.api,
					provider: streamModel.provider,
					model: streamModel.id,
					usage: usage(),
					stopReason: "toolUse",
					timestamp: Date.now(),
				};
				stream.push({ type: "done", reason: "stop", message });
			});
			return stream;
		};
		const { runtime } = createWorkflowV2RetainedChildRuntime(admissionFor(env), {
			streamFn,
			getApiKey: () => "k",
		});
		env.cleanups.push(() => runtime.session.dispose());
		runtime.session.subscribe(() => {});
		await runtime.session.agent.prompt("do a tool");
		await runtime.session.agent.waitForIdle();
		// reject policy converts the tool call into a terminal error; no second
		// provider request and no tool execution.
		expect(calls).toBe(1);
		expect(executed).not.toHaveBeenCalled();
	});
});

describe("workflow-v2 retained tools-none profile: mutation guard", () => {
	it("baseline direct build passes the attestation guard", () => {
		const env = makeEnv();
		const session = buildBaseline(env, baselineConfig(env));
		expect(() => attestWorkflowV2RetainedProfile(session)).not.toThrow();
	});

	it("rejects omitting baseToolsOverride (a kernel is constructed)", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		delete (config as { baseToolsOverride?: unknown }).baseToolsOverride;
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/kernel/i);
	});

	it("rejects a non-empty resource loader (reused parent resources)", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		const loader = emptyLoader();
		// A reused parent loader would expose skills/prompts/extensions; a single
		// non-empty resource must fail the closed profile.
		loader.getSkills = () => ({ skills: [{ name: "leak" } as never], diagnostics: [] });
		config.resourceLoader = loader;
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/resource/i);
	});

	it("rejects an unfrozen tools array", () => {
		const env = makeEnv();
		const session = new AgentSession(baselineConfig(env));
		env.cleanups.push(() => session.dispose());
		// Deliberately do NOT re-freeze the tools array.
		expect(Object.isFrozen(session.agent.state.tools)).toBe(false);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/frozen/i);
	});

	it("rejects toolCallPolicy execute", () => {
		const env = makeEnv();
		const session = buildBaseline(env, baselineConfig(env, { toolCallPolicy: "execute" }));
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/reject/i);
	});

	it("rejects an installed controller", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		config.agentMessageController = {
			enqueueAgentMessage: async () => ({ ok: true }),
		} as unknown as AgentSessionConfig["agentMessageController"];
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/controller/i);
	});

	it("rejects a raised rlmMaxDepth (descendants allowed)", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		config.rlmMaxDepth = 5;
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/descendant/i);
	});

	it("rejects reused parent transformContext hook", () => {
		const env = makeEnv();
		const config = baselineConfig(env, { transformContext: async (messages) => messages });
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/hook/i);
	});

	it("rejects enabled goals", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		config.includeGoals = true;
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/goal/i);
	});

	it("rejects enabled compact skill", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		config.includeCompactSkill = true;
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/compact/i);
	});

	it("rejects enabled serialized refinement", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		config.serializedRefine = true;
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/refin/i);
	});

	it("rejects enabled autonomous continuation", () => {
		const env = makeEnv();
		const config = baselineConfig(env);
		config.autonomous = { enabled: true } as AgentSessionConfig["autonomous"];
		const session = buildBaseline(env, config);
		expect(() => attestWorkflowV2RetainedProfile(session)).toThrow(/autonomous/i);
	});
});
