import { diagramKind, type MermaidArt, render, type Span } from "lovely-mermaid";
import { Marked, type Token } from "marked";
import type { MermaidRenderingMode } from "../../../core/settings-manager.js";
import type { Theme } from "../theme/theme.js";

const markdownParser = new Marked();

export interface MermaidRenderOptions {
	getMode: () => MermaidRenderingMode;
	theme?: Theme;
}

/** Rewrites assistant Markdown before pi-tui renders it, with the exact width available for content. */
export type MermaidMarkdownTransform = (markdown: string, availableWidth: number, isStreaming: boolean) => string;

/** Plain (non-Markdown) text split into prose and pre-drawn diagram rows that must not be re-wrapped. */
export type MermaidTextSegment = { kind: "text"; text: string } | { kind: "rows"; rows: string[] };

/** Splits plain message text around drawn Mermaid diagrams; `undefined` when nothing was drawn. */
export type MermaidTextRenderer = (text: string, availableWidth: number) => MermaidTextSegment[] | undefined;

export interface MermaidNotice {
	level: "info" | "warning";
	text: string;
}

export type MermaidLayout =
	| { kind: "art"; art: MermaidArt; notices: MermaidNotice[] }
	| { kind: "source"; notices: MermaidNotice[] };

type MermaidCodeToken = Token & { type: "code"; text: string; lang?: string };

function isMermaid(token: Token): token is MermaidCodeToken {
	return token.type === "code" && token.lang?.trim().split(/\s+/, 1)[0]?.toLowerCase() === "mermaid";
}

const FLOWCHART_HEADER = /^(\s*(?:flowchart|graph))(?:([ \t]+)(TB|TD|BT|LR|RL))?(?=[\s;]|%%|$)/i;

/**
 * The same flowchart turned a quarter: vertical layouts (TB/TD/BT) become LR, horizontal ones (LR/RL) become TD.
 * A diagram too wide for the terminal is usually a wide fan-out or side-by-side subgraphs, which the other axis
 * stacks. `undefined` for every other diagram type.
 */
export function rotateFlowchart(source: string): { source: string; direction: "LR" | "TD" } | undefined {
	const lines = source.split("\n");
	let index = 0;
	while (index < lines.length && lines[index]!.trim() === "") index++;
	if (lines[index]?.trim() === "---") {
		index++;
		while (index < lines.length && lines[index]!.trim() !== "---") index++;
		index++;
	}
	while (index < lines.length && (lines[index]!.trim() === "" || lines[index]!.trim().startsWith("%%"))) index++;
	const header = lines[index];
	const match = header === undefined ? null : FLOWCHART_HEADER.exec(header);
	if (!header || !match) return undefined;
	const current = match[3]?.toUpperCase() ?? "TB";
	const direction = current === "LR" || current === "RL" ? "TD" : "LR";
	lines[index] = `${match[1]} ${direction}${header.slice(match[0].length)}`;
	return { source: lines.join("\n"), direction };
}

function describeWarnings(warnings: readonly string[]): string {
	const suffix = warnings.length > 1 ? ` (+${warnings.length - 1} more)` : "";
	return `Mermaid diagram incomplete: ${warnings[0]}${suffix}`;
}

/**
 * Decide how one Mermaid block is shown in `availableWidth` columns.
 *
 * The renderer lays a diagram out at whatever width it needs and leaves fitting to the caller, so a diagram
 * wider than the terminal is retried on the other axis (flowcharts only) before falling back to its source.
 * Warnings are advisory per the renderer's contract: the art is still drawn and they are listed beside it.
 * Notices are omitted while streaming, where nearly every intermediate state warns.
 */
export function layoutMermaid(source: string, availableWidth: number, isStreaming: boolean): MermaidLayout {
	const notices: MermaidNotice[] = [];
	const art = render(source);
	let chosen: MermaidArt | undefined = art && art.width <= availableWidth ? art : undefined;
	let neededWidth = art?.width;
	if (!chosen && art) {
		const rotated = rotateFlowchart(source);
		const rotatedArt = rotated ? render(rotated.source) : null;
		if (rotated && rotatedArt) {
			neededWidth = Math.min(art.width, rotatedArt.width);
			if (rotatedArt.width <= availableWidth) {
				chosen = rotatedArt;
				if (!isStreaming) {
					const axis = rotated.direction === "LR" ? "left to right" : "top to bottom";
					notices.push({ level: "info", text: `Mermaid diagram drawn ${axis} to fit ${availableWidth} columns` });
				}
			}
		}
	}

	if (chosen) {
		if (!isStreaming && chosen.warnings.length > 0) {
			notices.push({ level: "warning", text: describeWarnings(chosen.warnings) });
		}
		return { kind: "art", art: chosen, notices };
	}

	if (!isStreaming) {
		if (neededWidth !== undefined) {
			notices.push({
				level: "warning",
				text: `Mermaid diagram not drawn: needs ${neededWidth} columns, ${availableWidth} available`,
			});
		} else if (diagramKind(source) !== null) {
			notices.push({ level: "warning", text: "Mermaid diagram not drawn: no statement could be parsed" });
		} else {
			const header = source.trim().split(/\s+/, 1)[0];
			if (header) {
				notices.push({
					level: "info",
					text: `Mermaid diagram not drawn: ${header} is not supported in the terminal`,
				});
			}
		}
	}
	return { kind: "source", notices };
}

function styleSpan(span: Span, theme: Theme): string {
	switch (span.role) {
		case "border":
			return theme.fg("borderMuted", span.text);
		case "text":
			return theme.fg("text", span.text);
		case "edge":
			return theme.fg("accent", span.text);
		case "edgeLabel":
			return theme.fg("muted", span.text);
		case "title":
			return theme.fg("accent", theme.bold(span.text));
		case "none":
			return span.text;
	}
}

function artRows(art: MermaidArt, theme: Theme | undefined): string[] {
	return theme ? art.styled.map((row) => row.map((span) => styleSpan(span, theme)).join("")) : art.plain;
}

function noticeText(notice: MermaidNotice, theme: Theme | undefined): string {
	if (!theme) return notice.text;
	return theme.fg(notice.level === "warning" ? "warning" : "muted", notice.text);
}

function codeSpan(line: string): string {
	// Inline code spans preserve the diagram row's spacing; a blank row becomes NBSP to keep visible height.
	const content = line || "\u00a0";
	// CommonMark: the delimiter must beat the longest backtick run, and padding keeps edge backticks as content.
	const longestBacktickRun = Math.max(0, ...Array.from(content.matchAll(/`+/g), (match) => match[0].length));
	const fence = "`".repeat(longestBacktickRun + 1);
	const padding = content.startsWith("`") || content.endsWith("`") ? " " : "";
	return `${fence}${padding}${content}${padding}${fence}`;
}

function isActive(mode: MermaidRenderingMode, isStreaming: boolean): boolean {
	return mode !== "off" && (!isStreaming || mode === "streaming");
}

/** Create a transform that replaces top-level Mermaid code blocks with Unicode terminal diagrams. */
export function createMermaidMarkdownTransform(options: MermaidRenderOptions): MermaidMarkdownTransform {
	return (markdown, availableWidth, isStreaming) => {
		if (!isActive(options.getMode(), isStreaming)) {
			return markdown;
		}

		const tokens = markdownParser.lexer(markdown);
		return tokens
			.map((token, index) => {
				if (!isMermaid(token)) return token.raw;
				const layout = layoutMermaid(token.text, availableWidth, isStreaming);
				const notices = layout.notices.map((notice) => codeSpan(noticeText(notice, options.theme)));
				// A paragraph (the rows or the notices) needs a blank line before directly following content, or that
				// content would join its last line.
				const next = tokens[index + 1];
				const end = next && next.type !== "space" ? "\n\n" : "\n";
				if (layout.kind === "source") {
					if (notices.length === 0) return token.raw;
					return `${token.raw.replace(/\n*$/, "\n")}${notices.join("  \n")}${end}`;
				}
				// Markdown hard breaks keep every row on its own line.
				return `${[...artRows(layout.art, options.theme).map(codeSpan), ...notices].join("  \n")}${end}`;
			})
			.join("");
	};
}

/** Create a renderer that draws Mermaid code blocks found in plain (non-Markdown) message text. */
export function createMermaidTextRenderer(options: MermaidRenderOptions): MermaidTextRenderer {
	return (text, availableWidth) => {
		if (!isActive(options.getMode(), false) || !text.includes("mermaid")) {
			return undefined;
		}
		const segments: MermaidTextSegment[] = [];
		let pending = "";
		let changed = false;
		for (const token of markdownParser.lexer(text)) {
			if (!isMermaid(token)) {
				pending += token.raw;
				continue;
			}
			const layout = layoutMermaid(token.text, availableWidth, false);
			const notices = layout.notices.map((notice) => `${noticeText(notice, options.theme)}\n`).join("");
			changed ||= layout.kind === "art" || notices !== "";
			if (layout.kind === "source") {
				pending += `${token.raw.replace(/\n*$/, "\n")}${notices}`;
				continue;
			}
			if (pending) segments.push({ kind: "text", text: pending.replace(/\n$/, "") });
			segments.push({ kind: "rows", rows: artRows(layout.art, options.theme) });
			pending = notices;
		}
		if (pending) segments.push({ kind: "text", text: pending.replace(/\n+$/, "") });
		return changed ? segments : undefined;
	};
}
