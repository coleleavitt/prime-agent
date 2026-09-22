import type { AssistantMessage } from "@earendil-works/pi-ai";
import { Markdown } from "@earendil-works/pi-tui";
import { beforeAll, describe, expect, it } from "vitest";
import type { AgentSessionMessage } from "../src/core/agent-messages.js";
import type { MermaidRenderingMode } from "../src/core/settings-manager.js";
import { AgentMessageComponent, agentMessageBodyLines } from "../src/modes/interactive/components/agent-message.js";
import { AssistantMessageComponent } from "../src/modes/interactive/components/assistant-message.js";
import { CustomMessageComponent } from "../src/modes/interactive/components/custom-message.js";
import {
	createMermaidMarkdownTransform,
	createMermaidTextRenderer,
	layoutMermaid,
	type MermaidMarkdownTransform,
	rotateFlowchart,
} from "../src/modes/interactive/components/mermaid.js";
import { SideQuestionComponent } from "../src/modes/interactive/components/side-question.js";
import { getMarkdownTheme, initTheme } from "../src/modes/interactive/theme/theme.js";

const strip = (text: string) => text.replace(/\x1b\[[0-9;]*m/g, "").replace(/\x1b\][^\x07]*\x07/g, "");

function renderMarkdown(markdown: string, width: number, transform: MermaidMarkdownTransform, isStreaming = false) {
	const component = new Markdown(markdown, 1, 0, getMarkdownTheme(), undefined, {
		transform: (md, availableWidth) => transform(md, availableWidth, isStreaming),
	});
	return component.render(width).map((line) => strip(line).replace(/\s+$/, ""));
}

function fence(source: string): string {
	return `\`\`\`mermaid\n${source}\n\`\`\``;
}

/** Index of the first output line where every row of `rows` appears in order, one column in (the Markdown padding). */
function findRows(lines: string[], rows: string[]): number {
	const wanted = rows.map((row) => row.replace(/\s+$/, ""));
	for (let start = 0; start + wanted.length <= lines.length; start++) {
		if (wanted.every((row, offset) => lines[start + offset]!.slice(1) === row)) return start;
	}
	return -1;
}

// Three unconnected subgraphs sit side by side top-to-bottom: wide as TD, narrow once rotated to LR.
const SIDE_BY_SIDE = [
	"flowchart TD",
	...["one", "two", "three"].flatMap((name) => [
		`    subgraph ${name.toUpperCase()}["tier ${name}"]`,
		`        ${name}A["a fairly long label for ${name} a"]`,
		`        ${name}B["a fairly long label for ${name} b"]`,
		`        ${name}C["a fairly long label for ${name} c"]`,
		"    end",
	]),
].join("\n");

const SMALL = "flowchart LR\n  A[Start] --> B[Done]";

const mode = (value: MermaidRenderingMode) => () => value;

beforeAll(() => {
	initTheme("dark");
});

describe("rotateFlowchart", () => {
	it("turns vertical flowcharts left to right and horizontal ones top to bottom", () => {
		expect(rotateFlowchart("flowchart TD\n  A --> B")).toEqual({
			source: "flowchart LR\n  A --> B",
			direction: "LR",
		});
		expect(rotateFlowchart("graph BT;A-->B")).toEqual({ source: "graph LR;A-->B", direction: "LR" });
		expect(rotateFlowchart("flowchart RL\nA-->B")?.source).toBe("flowchart TD\nA-->B");
		expect(rotateFlowchart("flowchart\nA-->B")?.source).toBe("flowchart LR\nA-->B");
	});

	it("skips frontmatter, blank lines and comments before the header", () => {
		const source = "\n---\ntitle: T\n---\n%% note\nflowchart TB %% trailing\nA-->B";
		expect(rotateFlowchart(source)?.source).toBe("\n---\ntitle: T\n---\n%% note\nflowchart LR %% trailing\nA-->B");
	});

	it("leaves other diagram types alone", () => {
		expect(rotateFlowchart("sequenceDiagram\nA->>B: hi")).toBeUndefined();
		expect(rotateFlowchart("flowcharts TD\nA-->B")).toBeUndefined();
	});
});

describe("layoutMermaid", () => {
	it("draws a diagram that fits", () => {
		const layout = layoutMermaid(SMALL, 80, false);
		expect(layout.kind).toBe("art");
		expect(layout.notices).toEqual([]);
	});

	it("rotates a flowchart that is too wide and says so once settled", () => {
		const natural = layoutMermaid(SIDE_BY_SIDE, 1000, false);
		const rotated = layoutMermaid(rotateFlowchart(SIDE_BY_SIDE)!.source, 1000, false);
		expect(natural.kind === "art" && rotated.kind === "art").toBe(true);
		if (natural.kind !== "art" || rotated.kind !== "art") return;
		expect(rotated.art.width).toBeLessThan(natural.art.width);

		const width = rotated.art.width;
		const layout = layoutMermaid(SIDE_BY_SIDE, width, false);
		expect(layout.kind).toBe("art");
		if (layout.kind !== "art") return;
		expect(layout.art.plain).toEqual(rotated.art.plain);
		expect(layout.notices).toEqual([
			{ level: "info", text: `Mermaid diagram drawn left to right to fit ${width} columns` },
		]);
		expect(layoutMermaid(SIDE_BY_SIDE, width, true).notices).toEqual([]);
	});

	it("reports the narrowest width it could manage when nothing fits", () => {
		const rotated = layoutMermaid(rotateFlowchart(SIDE_BY_SIDE)!.source, 1000, false);
		if (rotated.kind !== "art") throw new Error("expected art");
		const layout = layoutMermaid(SIDE_BY_SIDE, rotated.art.width - 1, false);
		expect(layout).toEqual({
			kind: "source",
			notices: [
				{
					level: "warning",
					text: `Mermaid diagram not drawn: needs ${rotated.art.width} columns, ${rotated.art.width - 1} available`,
				},
			],
		});
		expect(layoutMermaid(SIDE_BY_SIDE, rotated.art.width - 1, true)).toEqual({ kind: "source", notices: [] });
	});

	it("draws art with warnings and lists them beside it", () => {
		const source = "flowchart LR\n  A --> B\n  this is not ~~~ valid ((( ";
		const layout = layoutMermaid(source, 80, false);
		expect(layout.kind).toBe("art");
		expect(layout.notices).toHaveLength(1);
		expect(layout.notices[0]!.level).toBe("warning");
		expect(layout.notices[0]!.text).toMatch(/^Mermaid diagram incomplete: /);
		expect(layoutMermaid(source, 80, true).notices).toEqual([]);
	});

	it("explains unsupported and unparseable diagrams", () => {
		expect(layoutMermaid("gantt\n  title x", 80, false)).toEqual({
			kind: "source",
			notices: [{ level: "info", text: "Mermaid diagram not drawn: gantt is not supported in the terminal" }],
		});
		expect(layoutMermaid("flowchart TD\n  ((( ", 80, false)).toEqual({
			kind: "source",
			notices: [{ level: "warning", text: "Mermaid diagram not drawn: no statement could be parsed" }],
		});
	});
});

describe("createMermaidMarkdownTransform", () => {
	it("renders the art row for row", () => {
		const layout = layoutMermaid(SMALL, 80, false);
		if (layout.kind !== "art") throw new Error("expected art");
		const lines = renderMarkdown(fence(SMALL), 82, createMermaidMarkdownTransform({ getMode: mode("final") }));
		expect(findRows(lines, layout.art.plain)).toBe(0);
		// No trailing blank line when the diagram ends the message.
		expect(lines).toHaveLength(layout.art.plain.length);
		expect(lines.join("\n")).not.toContain("```");
	});

	it("draws a too-wide flowchart rotated, with the notice under it", () => {
		const rotated = layoutMermaid(rotateFlowchart(SIDE_BY_SIDE)!.source, 1000, false);
		if (rotated.kind !== "art") throw new Error("expected art");
		const width = rotated.art.width + 2;
		const lines = renderMarkdown(
			fence(SIDE_BY_SIDE),
			width,
			createMermaidMarkdownTransform({ getMode: mode("final") }),
		);
		const start = findRows(lines, rotated.art.plain);
		expect(start).toBeGreaterThanOrEqual(0);
		const after = lines
			.slice(start + rotated.art.plain.length)
			.map((line) => line.trim())
			.join(" ");
		expect(after).toBe(`Mermaid diagram drawn left to right to fit ${rotated.art.width} columns`);
	});

	it("keeps the source and explains why when the diagram cannot fit", () => {
		const lines = renderMarkdown(fence(SIDE_BY_SIDE), 24, createMermaidMarkdownTransform({ getMode: mode("final") }));
		expect(lines.join("\n")).toContain("flowchart TD");
		expect(lines.map((line) => line.trim()).join(" ")).toMatch(
			/Mermaid diagram not drawn: needs \d+ columns, 22 available/,
		);
	});

	it("does not join following text onto the last diagram row", () => {
		const transform = createMermaidMarkdownTransform({ getMode: mode("final") });
		const layout = layoutMermaid(SMALL, 80, false);
		if (layout.kind !== "art") throw new Error("expected art");
		for (const after of ["\nAfter text", "\n\nAfter text"]) {
			const lines = renderMarkdown(`Before\n\n${fence(SMALL)}${after}`, 82, transform);
			const start = findRows(lines, layout.art.plain);
			expect(start).toBeGreaterThan(0);
			const tail = lines.slice(start + layout.art.plain.length);
			expect(tail.filter((line) => line !== "")).toEqual([" After text"]);
			expect(tail[0]).toBe("");
		}
	});

	it("respects the rendering mode", () => {
		const markdown = fence(SMALL);
		expect(createMermaidMarkdownTransform({ getMode: mode("off") })(markdown, 80, false)).toBe(markdown);
		expect(createMermaidMarkdownTransform({ getMode: mode("final") })(markdown, 80, true)).toBe(markdown);
		expect(createMermaidMarkdownTransform({ getMode: mode("streaming") })(markdown, 80, true)).not.toBe(markdown);
	});
});

describe("createMermaidTextRenderer", () => {
	it("returns undefined when there is nothing to draw", () => {
		const render = createMermaidTextRenderer({ getMode: mode("final") });
		expect(render("plain text", 80)).toBeUndefined();
		expect(createMermaidTextRenderer({ getMode: mode("off") })(fence(SMALL), 80)).toBeUndefined();
	});

	it("draws agent message diagrams without re-wrapping their rows", () => {
		const layout = layoutMermaid(SMALL, 76, false);
		if (layout.kind !== "art") throw new Error("expected art");
		const body = `Here is the plan:\n\n${fence(SMALL)}\n\nDone.`;
		const lines = agentMessageBodyLines(body, 80, createMermaidTextRenderer({ getMode: mode("final") })).map((line) =>
			strip(line).replace(/\s+$/, ""),
		);
		expect(lines[0]).toBe(" ╰─ Here is the plan:");
		const rows = layout.art.plain.map((row) => `    ${row}`.replace(/\s+$/, ""));
		const start = lines.indexOf(rows[0]!);
		expect(start).toBeGreaterThan(0);
		expect(lines.slice(start, start + rows.length)).toEqual(rows);
		expect(lines.at(-1)).toBe("    Done.");
		expect(lines.join("\n")).not.toContain("```");
	});

	it("keeps the source with a notice when an agent message diagram is too wide", () => {
		const lines = agentMessageBodyLines(
			fence(SIDE_BY_SIDE),
			24,
			createMermaidTextRenderer({ getMode: mode("final") }),
		);
		const text = lines.map((line) => strip(line).trim());
		expect(text.join("\n")).toContain("flowchart TD");
		expect(text.join(" ")).toMatch(/Mermaid diagram not drawn: needs \d+ columns, 20 available/);
	});

	it("is wired through AgentMessageComponent", () => {
		const message = {
			role: "custom",
			customType: "agent_message",
			content: "",
			display: true,
			timestamp: 0,
			details: { message: fence(SMALL), from: "child", fromRelationship: "child" },
		} as unknown as AgentSessionMessage;
		const component = new AgentMessageComponent(message, getMarkdownTheme(), {
			renderMermaid: createMermaidTextRenderer({ getMode: mode("final") }),
		});
		component.setExpanded(true);
		const text = component.render(80).map(strip).join("\n");
		expect(text).toContain("───▶");
		expect(text).not.toContain("```");
	});
});

describe("other markdown surfaces", () => {
	it("draws diagrams in custom messages", () => {
		const component = new CustomMessageComponent(
			{ role: "custom", customType: "note", content: fence(SMALL), display: true, timestamp: 0 },
			undefined,
			getMarkdownTheme(),
			createMermaidMarkdownTransform({ getMode: mode("final") }),
		);
		expect(component.render(80).map(strip).join("\n")).toContain("───▶");
	});

	it("draws /btw answers once they settle in final mode", () => {
		const event = { id: "q1", question: "draw it", answer: fence(SMALL), status: "running" as const };
		const component = new SideQuestionComponent(
			event as never,
			2,
			createMermaidMarkdownTransform({ getMode: mode("final") }),
		);
		expect(component.render(80).map(strip).join("\n")).not.toContain("───▶");
		component.update({ ...event, status: "complete" } as never);
		expect(component.render(80).map(strip).join("\n")).toContain("───▶");
	});

	it("draws assistant diagrams rotated to fit the terminal", () => {
		const rotated = layoutMermaid(rotateFlowchart(SIDE_BY_SIDE)!.source, 1000, false);
		if (rotated.kind !== "art") throw new Error("expected art");
		const message: AssistantMessage = {
			role: "assistant",
			content: [{ type: "text", text: fence(SIDE_BY_SIDE) }],
			api: "anthropic-messages",
			provider: "anthropic",
			model: "test",
			usage: {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop",
			timestamp: 0,
		};
		const component = new AssistantMessageComponent(message, false, getMarkdownTheme(), {
			mermaidTransform: createMermaidMarkdownTransform({ getMode: mode("streaming") }),
		});
		const lines = component.render(rotated.art.width + 2).map((line) => strip(line).replace(/\s+$/, ""));
		expect(findRows(lines, rotated.art.plain)).toBeGreaterThanOrEqual(0);
	});
});
