import type { AgentContext, AgentTool } from "@earendil-works/pi-agent-core";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { computeProviderRequestContext } from "../src/core/agent-session.js";
import { type BuildSystemPromptOptions, buildSystemPrompt } from "../src/core/system-prompt.js";

const tool: AgentTool = {
	name: "ipython",
	label: "Python",
	description: "Run Python",
	parameters: Type.Object({}),
	execute: async () => ({ content: [{ type: "text", text: "ok" }], details: {} }),
};

function fixture() {
	const options: BuildSystemPromptOptions = {
		cwd: "/tmp/project",
		selectedTools: ["ipython"],
		toolSnippets: { ipython: "Use ipython." },
		promptGuidelines: ["Call the ipython tool."],
		customPrompt: "Custom base prompt",
	};
	const base = buildSystemPrompt(options);
	const context: AgentContext = {
		systemPrompt: `${base}\n\nextension suffix`,
		messages: [],
		tools: [tool],
	};
	return { options, base, context };
}

describe("provider tool capability request context", () => {
	it("preserves normal provider prompt and tools", () => {
		const { options, base, context } = fixture();
		const effective = computeProviderRequestContext(context, base, options, true);
		expect(effective).toEqual({ systemPrompt: context.systemPrompt, tools: context.tools });
	});

	it("builds text-only prompt without tool references and does not mutate desired state", () => {
		const { options, base, context } = fixture();
		const originalTools = context.tools;
		const originalPrompt = context.systemPrompt;
		const effective = computeProviderRequestContext(context, base, options, false);
		expect(effective.tools).toEqual([]);
		expect(effective.systemPrompt).toContain("Custom base prompt");
		expect(effective.systemPrompt).toContain("extension suffix");
		expect(effective.systemPrompt).not.toContain("Use ipython");
		expect(effective.systemPrompt).not.toContain("Call the ipython tool");
		expect(context.tools).toBe(originalTools);
		expect(context.systemPrompt).toBe(originalPrompt);
	});
});
