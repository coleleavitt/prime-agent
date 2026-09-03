import { existsSync } from "node:fs";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createRunAgentChildCall } from "../../../src/core/ravo/runtime-adapter.js";
import type {
	CreateRlmSubagentRuntimeOptions,
	RlmSubagentRuntime,
	SubagentRuntimeHost,
} from "../../../src/core/rlm-runtime.js";
import type { RunAgentProgress } from "../../../src/core/run-agent.js";
import { createHarness, getUserTexts, type Harness } from "../harness.js";

const harnesses: Harness[] = [];
afterEach(() => {
	for (const harness of harnesses.splice(0)) harness.cleanup();
});

async function createParent(options?: { tools?: AgentTool[]; response?: string }) {
	let child: Harness | undefined;
	let creation: CreateRlmSubagentRuntimeOptions | undefined;
	const release = vi.fn(async () => {});
	const host: SubagentRuntimeHost = {
		async createRlmSubagentRuntime(runtimeOptions): Promise<RlmSubagentRuntime> {
			creation = runtimeOptions;
			child = await createHarness({
				tools: options?.tools,
				rlmDepth: runtimeOptions.rlmDepth,
				rlmMaxDepth: runtimeOptions.rlmMaxDepth,
			});
			harnesses.push(child);
			child.session.setActiveToolsByName(runtimeOptions.activeToolNames);
			child.setResponses([fauxAssistantMessage(options?.response ?? "structured child result")]);
			return { session: child.session };
		},
		releaseRlmSubagentRuntime: release,
		async deleteRlmSubagentRuntime() {},
	};
	const parent = await createHarness({
		tools: options?.tools,
		subagentRuntimeHost: host,
		rlmMaxDepth: 3,
	});
	harnesses.push(parent);
	return {
		parent,
		release,
		get child() {
			return child;
		},
		get creation() {
			return creation;
		},
	};
}

describe("extension runAgent regression", () => {
	it("creates an isolated child, defaults the model, intersects tools, and returns a terminal result", async () => {
		const read: AgentTool = {
			name: "read",
			label: "read",
			description: "read",
			parameters: { type: "object" },
			execute: async () => ({ content: [], details: undefined }),
		};
		const write: AgentTool = {
			name: "write",
			label: "write",
			description: "write",
			parameters: { type: "object" },
			execute: async () => ({ content: [], details: undefined }),
		};
		const runtime = await createParent({ tools: [read, write] });
		runtime.parent.session.setActiveToolsByName(["read"]);
		const progress: RunAgentProgress[] = [];

		const result = await runtime.parent.session.extensionRunner.createContext().runAgent(
			{ prompt: "do the focused task" },
			{
				tools: { allow: ["read", "write"] },
				onProgress: (event) => progress.push(event),
			},
		);

		expect(result).toMatchObject({
			status: "completed",
			output: "structured child result",
			turns: 1,
		});
		expect(result.model).toBe(`${runtime.parent.session.model?.provider}/${runtime.parent.session.model?.id}`);
		expect(runtime.creation?.activeToolNames).toEqual(["read"]);
		expect(runtime.child).toBeDefined();
		expect(runtime.child && getUserTexts(runtime.child)).toEqual(["do the focused task"]);
		expect(runtime.parent.session.messages).toEqual([]);
		expect(runtime.release).toHaveBeenCalledWith(expect.anything(), expect.anything(), "done");
		expect(runtime.creation && existsSync(runtime.creation.sessionDir)).toBe(false);
		expect(progress.map((event) => event.type)).toEqual(["started", "turn", "finished"]);
	});

	it("rejects unavailable models before creating a child", async () => {
		const runtime = await createParent();
		await expect(
			runtime.parent.session.runAgent({
				prompt: "task",
				model: "missing/model",
			}),
		).rejects.toThrow("unavailable, unauthenticated, or expired");
		expect(runtime.child).toBeUndefined();
	});

	it("contains progress callback failures", async () => {
		const runtime = await createParent();
		await expect(
			runtime.parent.session.runAgent(
				{ prompt: "task" },
				{
					onProgress: () => {
						throw new Error("observer failed");
					},
				},
			),
		).resolves.toMatchObject({ status: "completed" });
	});
	it("runs the RAVO synchronous adapter with scoped tools and releases its AgentSession runtime", async () => {
		const read: AgentTool = {
			name: "read",
			label: "read",
			description: "read",
			parameters: { type: "object" },
			execute: async () => ({ content: [], details: undefined }),
		};
		const write: AgentTool = {
			name: "write",
			label: "write",
			description: "write",
			parameters: { type: "object" },
			execute: async () => ({ content: [], details: undefined }),
		};
		const runtime = await createParent({
			tools: [read, write],
			response: '{"summary":"done"}',
		});
		runtime.parent.session.setActiveToolsByName(["read"]);
		const call = createRunAgentChildCall(runtime.parent.session.runAgent, {
			prompt: ({ task }: { task: string }) => `inspect: ${task}`,
			validate: (value) => {
				if (
					typeof value !== "object" ||
					value === null ||
					!("summary" in value) ||
					typeof value.summary !== "string"
				) {
					throw new Error("summary is required");
				}
				return { summary: value.summary };
			},
			scope: { tools: { allow: ["read", "write"] }, maxTurns: 2 },
		});

		await expect(
			call({ task: "repository" }, { signal: new AbortController().signal, tokenBudget: 100_000 }),
		).resolves.toMatchObject({
			status: "completed",
			value: { summary: "done" },
		});
		expect(runtime.creation?.activeToolNames).toEqual(["read"]);
		expect(runtime.child && getUserTexts(runtime.child)).toEqual(["inspect: repository"]);
		expect(runtime.release).toHaveBeenCalledWith(expect.anything(), expect.anything(), "done");
		expect(runtime.creation && existsSync(runtime.creation.sessionDir)).toBe(false);
	});
});
