import { AsyncLocalStorage } from "node:async_hooks";
import {
	childContext,
	currentTraceContext,
	type FauxProviderRegistration,
	fauxAssistantMessage,
	fauxToolCall,
	installAsyncTraceContextStorage,
	installDefaultSpanSink,
	type Message,
	registerFauxProvider,
	runWithTraceContext,
	type SpanEndRecord,
	setSpanSink,
	type TraceContext,
} from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { runAgentLoop } from "../src/agent-loop.js";
import type { AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool } from "../src/types.js";

// The loop awaits between span start and the nested provider/tool calls, so the
// synchronous fallback storage would drop the context. Idempotent when the
// module's own best-effort install already ran.
installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const echoSchema = Type.Object({ value: Type.String() });

function identityConverter(messages: AgentMessage[]): Message[] {
	return messages.filter((m) => m.role === "user" || m.role === "assistant" || m.role === "toolResult") as Message[];
}

function userMessage(text: string): AgentMessage {
	return { role: "user", content: text, timestamp: Date.now() };
}

function echoTool(execute: AgentTool<typeof echoSchema>["execute"]): AgentTool<typeof echoSchema> {
	return { name: "echo", label: "Echo", description: "Echo", parameters: echoSchema, execute };
}

const okEcho = echoTool(async (_id, params) => ({
	content: [{ type: "text", text: `echo:${params.value}` }],
	details: {},
}));

type RunResult = { events: AgentEvent[]; messages: AgentMessage[] };

async function runPrompt(
	registration: FauxProviderRegistration,
	tools: AgentTool<any>[],
	overrides: Partial<AgentLoopConfig> = {},
	signal?: AbortSignal,
): Promise<RunResult> {
	const context: AgentContext = { systemPrompt: "test", messages: [], tools };
	const config: AgentLoopConfig = { model: registration.getModel(), convertToLlm: identityConverter, ...overrides };
	const events: AgentEvent[] = [];
	const messages = await runAgentLoop(
		[userMessage("hi")],
		context,
		config,
		(event) => void events.push(event),
		signal,
	);
	return { events, messages };
}

describe("agent loop trace spans", () => {
	const ended: SpanEndRecord[] = [];
	const byName = (name: string) => ended.filter((record) => record.name === name);
	let registration: FauxProviderRegistration;

	beforeEach(() => {
		ended.length = 0;
		setSpanSink((record) => ended.push(record));
		registration = registerFauxProvider();
	});
	afterEach(() => {
		registration.unregister();
		installDefaultSpanSink();
	});

	it("opens one agent.turn span per turn with llm.request and tool.execute as children", async () => {
		registration.setResponses([
			fauxAssistantMessage([fauxToolCall("echo", { value: "one" }, { id: "call-1" })], { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);

		const { events } = await runPrompt(registration, [okEcho]);

		expect(events.filter((event) => event.type === "turn_end")).toHaveLength(2);
		const turns = byName("agent.turn");
		expect(turns.map((turn) => turn.attrs["turn.index"])).toEqual([0, 1]);
		expect(turns.map((turn) => turn.status)).toEqual(["ok", "ok"]);
		expect(turns[0]?.attrs).toMatchObject({
			"llm.provider": registration.getModel().provider,
			"llm.model": registration.getModel().id,
			"turn.stop_reason": "toolUse",
			"turn.tool_calls": 1,
		});
		expect(turns[1]?.attrs).toMatchObject({ "turn.stop_reason": "stop", "turn.tool_calls": 0 });
		// No ambient context: every turn is its own root of one shared trace.
		expect(turns.every((turn) => turn.parentSpanId === undefined)).toBe(true);

		const requests = byName("llm.request");
		expect(requests.map((request) => request.parentSpanId)).toEqual(turns.map((turn) => turn.spanId));

		const tool = byName("tool.execute");
		expect(tool).toHaveLength(1);
		expect(tool[0]).toMatchObject({
			parentSpanId: turns[0]?.spanId,
			status: "ok",
			attrs: { "tool.name": "echo", "tool.call_id": "call-1" },
		});

		// Spans are ended child-first, before their parent turn.
		expect(ended.map((record) => record.name)).toEqual([
			"llm.request",
			"tool.execute",
			"agent.turn",
			"llm.request",
			"agent.turn",
		]);
	});

	it("shares one traceId across all spans of a prompt", async () => {
		registration.setResponses([
			fauxAssistantMessage([fauxToolCall("echo", { value: "one" })], { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);

		await runPrompt(registration, [okEcho]);

		const traceIds = new Set(ended.map((record) => record.traceId));
		expect(ended.length).toBeGreaterThan(3);
		// Without an ambient context each root turn mints its own trace; the
		// coding-agent supplies the session-level parent that ties them together.
		expect(traceIds.size).toBe(2);
		for (const turn of byName("agent.turn")) {
			const children = ended.filter((record) => record.parentSpanId === turn.spanId);
			expect(children.length).toBeGreaterThan(0);
			expect(children.every((child) => child.traceId === turn.traceId)).toBe(true);
		}
	});

	it("nests every turn under an ambient trace context and inherits its traceId", async () => {
		registration.setResponses([
			fauxAssistantMessage([fauxToolCall("echo", { value: "one" })], { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
		]);
		const parent = childContext(undefined);

		await runWithTraceContext(parent, () => runPrompt(registration, [okEcho]));

		const turns = byName("agent.turn");
		expect(turns).toHaveLength(2);
		expect(turns.every((turn) => turn.parentSpanId === parent.spanId)).toBe(true);
		expect(ended.every((record) => record.traceId === parent.traceId)).toBe(true);
		expect(new Set(ended.map((record) => record.spanId)).size).toBe(ended.length);
	});

	it.each(["parallel", "sequential"] as const)(
		"marks failing and rejected tools as errors on the %s path",
		async (toolExecution) => {
			const failing = echoTool(async (_id, params) => {
				if (params.value === "throw") throw new Error("tool exploded");
				return { content: [{ type: "text", text: "soft failure" }], details: {} };
			});
			registration.setResponses([
				fauxAssistantMessage(
					[
						fauxToolCall("echo", { value: "throw" }, { id: "call-throw" }),
						fauxToolCall("missing", {}, { id: "call-missing" }),
						fauxToolCall("echo", { value: "hook" }, { id: "call-hook" }),
					],
					{ stopReason: "toolUse" },
				),
				fauxAssistantMessage("done"),
			]);

			await runPrompt(registration, [failing], {
				toolExecution,
				afterToolCall: async ({ toolCall, result }) =>
					toolCall.id === "call-hook" ? { content: result.content, isError: true } : undefined,
			});

			// The parallel path ends immediate rejections before deferred executions, so compare unordered.
			const tool = byName("tool.execute").sort((a, b) =>
				String(a.attrs["tool.call_id"]).localeCompare(String(b.attrs["tool.call_id"])),
			);
			expect(tool.map((record) => [record.attrs["tool.call_id"], record.status, record.error])).toEqual([
				["call-hook", "error", "soft failure"],
				["call-missing", "error", "Tool missing not found"],
				["call-throw", "error", "tool exploded"],
			]);
			expect(tool.map((record) => record.attrs["tool.name"])).toEqual(["echo", "missing", "echo"]);
			const turn = byName("agent.turn")[0];
			expect(tool.every((record) => record.parentSpanId === turn?.spanId)).toBe(true);
			expect(turn?.status).toBe("ok");
		},
	);

	it("gives concurrently running tools distinct spans under the same turn", async () => {
		const seen = new Map<string, TraceContext | undefined>();
		let release: (() => void) | undefined;
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		let started = 0;
		const slow = echoTool(async (id, params) => {
			started++;
			if (started === 2) release?.();
			await gate;
			// Read the context after the barrier, once both executions are in flight.
			seen.set(id, currentTraceContext());
			return { content: [{ type: "text", text: params.value }], details: {} };
		});
		registration.setResponses([
			fauxAssistantMessage(
				[
					fauxToolCall("echo", { value: "a" }, { id: "call-a" }),
					fauxToolCall("echo", { value: "b" }, { id: "call-b" }),
				],
				{ stopReason: "toolUse" },
			),
			fauxAssistantMessage("done"),
		]);

		await runPrompt(registration, [slow]);

		const tool = byName("tool.execute");
		expect(tool.map((record) => record.attrs["tool.call_id"])).toEqual(["call-a", "call-b"]);
		expect(tool[0]?.spanId).not.toBe(tool[1]?.spanId);
		expect(tool[0]?.parentSpanId).toBe(tool[1]?.parentSpanId);
		expect(tool[0]?.parentSpanId).toBe(byName("agent.turn")[0]?.spanId);
		expect(seen.get("call-a")?.spanId).toBe(tool[0]?.spanId);
		expect(seen.get("call-b")?.spanId).toBe(tool[1]?.spanId);
	});

	it("reports an aborted tool as ok with tool.aborted instead of an error", async () => {
		const controller = new AbortController();
		const hanging = echoTool(
			(_id, _params, signal) =>
				new Promise((_resolve, reject) => {
					signal?.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
					controller.abort();
				}),
		);
		registration.setResponses([
			fauxAssistantMessage([fauxToolCall("echo", { value: "x" }, { id: "call-x" })], { stopReason: "toolUse" }),
		]);

		await runPrompt(registration, [hanging], {}, controller.signal);

		const tool = byName("tool.execute");
		expect(tool).toHaveLength(1);
		expect(tool[0]).toMatchObject({ status: "ok", attrs: { "tool.call_id": "call-x", "tool.aborted": true } });
		expect(tool[0]?.error).toBeUndefined();
		expect(byName("agent.turn")).toHaveLength(1);
	});

	it("marks the turn failed when the assistant response is an error", async () => {
		registration.setResponses([fauxAssistantMessage("", { stopReason: "error", errorMessage: "rate limited" })]);

		await runPrompt(registration, []);

		const turns = byName("agent.turn");
		expect(turns).toHaveLength(1);
		expect(turns[0]).toMatchObject({
			status: "error",
			error: "rate limited",
			attrs: { "turn.stop_reason": "error" },
		});
		expect(byName("llm.request")[0]?.parentSpanId).toBe(turns[0]?.spanId);
	});

	it("does not let a throwing span sink break the loop", async () => {
		setSpanSink(() => {
			throw new Error("sink down");
		});
		registration.setResponses([fauxAssistantMessage("fine")]);

		const { messages } = await runPrompt(registration, []);

		const assistant = messages.find((message) => message.role === "assistant");
		expect(assistant?.role).toBe("assistant");
		if (assistant?.role === "assistant") expect(assistant.stopReason).toBe("stop");
	});
});
