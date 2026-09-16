import { readFileSync } from "node:fs";
import type { Context, Model, SimpleStreamOptions } from "@earendil-works/pi-ai";
import {
	createAssistantMessageEventStream,
	fauxAssistantMessage,
	fauxText,
	fauxToolCall,
	registerFauxProvider,
	streamSimple,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const agentConstruction = vi.hoisted(() => ({
	instances: [] as Array<{ state: { tools: unknown[] } }>,
	options: [] as unknown[],
}));

vi.mock("@earendil-works/pi-agent-core", async (importOriginal) => {
	const actual = await importOriginal<typeof import("@earendil-works/pi-agent-core")>();
	class ObservedAgent extends actual.Agent {
		constructor(options: ConstructorParameters<typeof actual.Agent>[0]) {
			super(options);
			agentConstruction.options.push(options);
			agentConstruction.instances.push(this);
		}
	}
	return { ...actual, Agent: ObservedAgent };
});

import { runWorkflowAgent } from "../src/core/run-workflow-agent.js";

const registrations: Array<ReturnType<typeof registerFauxProvider>> = [];
beforeEach(() => {
	agentConstruction.instances.length = 0;
	agentConstruction.options.length = 0;
});
afterEach(() => {
	for (const registration of registrations.splice(0)) registration.unregister();
});

function setup(responses: Parameters<ReturnType<typeof registerFauxProvider>["setResponses"]>[0]) {
	const registration = registerFauxProvider({ provider: `workflow-${Math.random()}` });
	registrations.push(registration);
	registration.setResponses(responses);
	return registration;
}

function run(registration: ReturnType<typeof registerFauxProvider>, overrides = {}) {
	return runWorkflowAgent({
		prompt: "one turn",
		model: registration.getModel() as Model<any>,
		streamFn: streamSimple,
		getApiKey: () => "test-key",
		drainTimeoutMs: 1_000,
		...overrides,
	});
}

describe("runWorkflowAgent", () => {
	it("has no alternate completeSimple, RLM, or session execution path", () => {
		const source = readFileSync(new URL("../src/core/run-workflow-agent.ts", import.meta.url), "utf8");
		expect(source).not.toMatch(/\bcompleteSimple\b/);
		expect(source).not.toMatch(/\brunRlmChild\b|\bcreateRlmSession\b|\bAgentSession\b/);
		expect(source).toContain("const source = await input.streamFn(model, context, providerOptions)");
	});

	it("constructs exactly one bare Agent with a frozen empty registry and immutable reject policy", async () => {
		const registration = setup([fauxAssistantMessage("done")]);

		const result = await run(registration);

		expect(result).toMatchObject({ outcome: "completed", turnsStarted: 1 });
		expect(agentConstruction.instances).toHaveLength(1);
		expect(agentConstruction.options).toHaveLength(1);
		const options = agentConstruction.options[0] as Record<string, unknown>;
		expect(Object.keys(options).sort()).toEqual([
			"getApiKey",
			"initialState",
			"shouldStopAfterTurn",
			"streamFn",
			"toolCallPolicy",
		]);
		expect(options).toMatchObject({
			initialState: { model: registration.getModel(), tools: [] },
			toolCallPolicy: "reject",
		});
		const agent = agentConstruction.instances[0]!;
		expect(agent.state.tools).toEqual([]);
		expect(Object.isFrozen(agent.state.tools)).toBe(true);
		expect(() => agent.state.tools.push({})).toThrow();
		expect(() => Object.assign(agent, { toolCallPolicy: "execute" })).toThrow();
	});
	it("keeps the workflow boundary free of session, RLM, ledger, kernel, extension, skill, and controller imports", () => {
		const source = readFileSync(new URL("../src/core/run-workflow-agent.ts", import.meta.url), "utf8");
		const imports = [...source.matchAll(/from\s+["']([^"']+)["']/g)].map((match) => match[1]);
		expect(imports).toEqual(["node:crypto", "@earendil-works/pi-agent-core", "@earendil-works/pi-ai"]);
		expect(source).not.toMatch(/new\s+(?:AgentSession|RLM|Ledger|Kernel|Extension|Hook|Skill|WorkflowController)\b/);
		expect(source).not.toMatch(
			/(?:createAgentSession|createRlm|createLedger|createKernel|loadExtensions|loadSkills|createWorkflowController)\s*\(/,
		);
	});

	it("reuses one frozen empty registry singleton across isolated executions", async () => {
		const first = setup([fauxAssistantMessage("one")]);
		const second = setup([fauxAssistantMessage("two")]);
		await run(first);
		await run(second);

		expect(agentConstruction.options).toHaveLength(2);
		const firstTools = (agentConstruction.options[0] as { initialState: { tools: unknown[] } }).initialState.tools;
		const secondTools = (agentConstruction.options[1] as { initialState: { tools: unknown[] } }).initialState.tools;
		expect(firstTools).toBe(secondTools);
		expect(firstTools).toEqual([]);
		expect(Object.isFrozen(firstTools)).toBe(true);
	});

	it("returns no-separator UTF-8 text, digest, authoritative usage, and forces maxRetries zero", async () => {
		let options: SimpleStreamOptions | undefined;
		const message = fauxAssistantMessage([fauxText("hé"), fauxText("llo")]);
		message.usage = {
			input: 2,
			output: 3,
			cacheRead: 4,
			cacheWrite: 5,
			totalTokens: 99,
			cost: { input: 1, output: 2, cacheRead: 3, cacheWrite: 4, total: 42 },
		};
		const registration = setup([]);
		const result = await run(registration, {
			streamFn: (_model: Model<any>, _context: Context, received: SimpleStreamOptions | undefined) => {
				options = received;
				const stream = createAssistantMessageEventStream();
				stream.push({ type: "done", reason: "stop", message });
				return stream;
			},
		});
		expect(options).toMatchObject({ apiKey: "test-key", maxRetries: 0 });
		expect(Object.keys(options ?? {}).sort()).toEqual(["apiKey", "maxRetries", "signal"]);
		expect(options).not.toHaveProperty("sessionId");
		expect(options).not.toHaveProperty("onPayload");
		expect(options).not.toHaveProperty("onResponse");
		expect(result).toMatchObject({
			outcome: "completed",
			result: {
				text: "héllo",
				utf8Bytes: 6,
				sha256: "3c48591d8d098a4538f5e013dfcf406e948eac4d3277b10bf614e295d6068179",
			},
			usage: { totalTokens: 99, costTotal: 42, finality: "final" },
			turnsStarted: 1,
		});
	});

	it("overrides caller retry policy at the physical provider boundary", async () => {
		const received: SimpleStreamOptions[] = [];
		const registration = setup([]);
		const result = await run(registration, {
			streamFn: (_model: Model<any>, _context: Context, options: SimpleStreamOptions | undefined) => {
				if (!options) throw new Error("missing physical provider options");
				received.push(options);
				const stream = createAssistantMessageEventStream();
				stream.push({ type: "done", reason: "stop", message: fauxAssistantMessage("only") });
				return stream;
			},
		});
		expect(result.outcome).toBe("completed");
		expect(received).toHaveLength(1);
		expect(Object.hasOwn(received[0]!, "maxRetries")).toBe(true);
		expect((received[0]! as SimpleStreamOptions & { maxRetries?: number }).maxRetries).toBe(0);
	});

	it("does not retry or fall back after a provider failure", async () => {
		let physicalRequests = 0;
		const registration = setup([
			fauxAssistantMessage([], { stopReason: "error", errorMessage: "first request failed" }),
			fauxAssistantMessage("fallback must remain queued"),
		]);
		const result = await run(registration, {
			streamFn: (model: Model<any>, context: Context, options: SimpleStreamOptions | undefined) => {
				physicalRequests += 1;
				return streamSimple(model, context, options);
			},
		});
		expect(result).toMatchObject({ outcome: "failed", reason: "provider_failed", turnsStarted: 1 });
		expect(physicalRequests).toBe(1);
		expect(registration.getPendingResponseCount()).toBe(1);
	});

	it("rejects a tool call before a dispatcher can run and starts no second request", async () => {
		const dispatcher = vi.fn();
		const registration = setup([
			fauxAssistantMessage([fauxToolCall("forbidden", {}, { id: "call-1" })], { stopReason: "toolUse" }),
			fauxAssistantMessage("must not run"),
		]);
		const result = await run(registration, { dispatcher });
		expect(result).toMatchObject({ outcome: "failed", reason: "unexpected_tool_call", turnsStarted: 1 });
		expect(registration.getPendingResponseCount()).toBe(1);
		expect(dispatcher).not.toHaveBeenCalled();
	});

	it("returns no partial result or digest when UTF-8 bytes exceed the bound", async () => {
		const registration = setup([fauxAssistantMessage("éé")]);
		const result = await run(registration, { maxResultUtf8Bytes: 3 });
		expect(result).toMatchObject({ outcome: "failed", reason: "result_too_large" });
		expect(result).not.toHaveProperty("result");
	});

	it("links a pre-dispatch abort without provider I/O", async () => {
		const registration = setup([fauxAssistantMessage("must not run")]);
		const controller = new AbortController();
		controller.abort();
		const result = await run(registration, { signal: controller.signal });
		expect(result).toMatchObject({ outcome: "cancelled", turnsStarted: 0, usage: { finality: "final" } });
		expect(registration.getPendingResponseCount()).toBe(1);
	});

	it("maps an unsettled physical stream after abort to execution_unknown with known-prefix usage", async () => {
		let release!: () => void;
		let markStarted!: () => void;
		const barrier = new Promise<void>((resolve) => {
			release = resolve;
		});
		const started = new Promise<void>((resolve) => {
			markStarted = resolve;
		});
		const registration = setup([
			async () => {
				markStarted();
				await barrier;
				return fauxAssistantMessage("late");
			},
		]);
		const controller = new AbortController();
		const pending = run(registration, { drainTimeoutMs: 1, signal: controller.signal });
		await started;
		controller.abort();
		const result = await pending;
		expect(result).toMatchObject({
			outcome: "execution_unknown",
			reason: "drain_timeout",
			error: "EXECUTION_UNKNOWN",
			usage: { finality: "known_prefix" },
		});
		release();
	});
	it("keeps terminal completion when a later abort loses linearization", async () => {
		const registration = setup([fauxAssistantMessage("done")]);
		const controller = new AbortController();
		const result = await run(registration, { signal: controller.signal });
		controller.abort();
		expect(result).toMatchObject({ outcome: "completed", result: { text: "done" } });
	});
});
