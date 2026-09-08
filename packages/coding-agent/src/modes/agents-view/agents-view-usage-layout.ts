import { visibleWidth } from "@earendil-works/pi-tui";
import type { SessionSummary } from "../daemon/daemon-session-list.js";
import { formatTokenCount } from "../interactive/agent-activity.js";
import { type AgentsViewRow, type AgentsViewSection, isEmptyAgentsViewSession } from "./agents-view-state.js";

const SECTIONS: readonly AgentsViewSection[] = ["running", "idle", "inactive"];

interface AgentsViewUsageParts {
	inTokens: string;
	outTokens: string;
	agentCost: string;
	count: string;
	totalCost: string;
	age: string;
}

const USAGE_LABELS: AgentsViewUsageParts = {
	inTokens: "↑in",
	outTokens: "↓out",
	agentCost: "$agent",
	count: "#sub",
	totalCost: "$total",
	age: "age",
};

const USAGE_COLUMNS = Object.keys(USAGE_LABELS) as (keyof AgentsViewUsageParts)[];

export type AgentsViewDisplayItem =
	| { type: "spacer" }
	| { type: "heading"; section: AgentsViewSection }
	| { type: "empty"; section: AgentsViewSection }
	| { type: "row"; row: AgentsViewRow };

export type AgentsViewSectionCounts = Record<AgentsViewSection, number>;

export interface AgentsViewUsageLayout {
	/** Legend line per section, padded to that section's column widths. */
	legends: ReadonlyMap<AgentsViewSection, string>;
	/** Details string per row identity, padded to its section's column widths. */
	details: ReadonlyMap<string, string>;
}

export interface AgentsViewRenderPreparation {
	counts: AgentsViewSectionCounts;
	displayItems: readonly AgentsViewDisplayItem[];
	usageLayout: AgentsViewUsageLayout;
	/** First instant at which an age label can change. */
	expiresAt: number;
	hasRunningRow: boolean;
	hasStaleAge: boolean;
}

interface UsageEntry {
	identity: string;
	empty: boolean;
	parts: AgentsViewUsageParts;
}

/**
 * Precompute the section index, top-level counts, and aligned usage cells in
 * one traversal of the rows. The result is immutable and safe to reuse until
 * the rows change or `expiresAt` is reached.
 */
export function prepareAgentsViewRender(
	rows: readonly AgentsViewRow[],
	now: number = Date.now(),
): AgentsViewRenderPreparation {
	const counts: AgentsViewSectionCounts = { running: 0, idle: 0, inactive: 0 };
	const displayRows = new Map<AgentsViewSection, AgentsViewRow[]>(SECTIONS.map((section) => [section, []]));
	const usageEntries = new Map<AgentsViewSection, UsageEntry[]>(SECTIONS.map((section) => [section, []]));
	let blockSection: AgentsViewSection = "running";
	let expiresAt = Number.POSITIVE_INFINITY;
	let hasRunningRow = false;
	let hasStaleAge = false;

	for (const row of rows) {
		hasRunningRow ||= row.section === "running";
		hasStaleAge ||= row.summary.lastHeardFromAt !== undefined;
		if (row.depth === 0) {
			blockSection = row.section;
			if (row.kind === "agent") counts[row.section] += 1;
		}
		displayRows.get(blockSection)!.push(row);
		if (row.kind !== "agent" && row.kind !== "subagent") continue;

		const age = formatSessionDuration(row.summary, now);
		expiresAt = Math.min(expiresAt, age.expiresAt);
		const usage = row.summary.usage;
		usageEntries.get(blockSection)!.push({
			identity: row.identity,
			empty: isEmptyAgentsViewSession(row.summary),
			parts: {
				inTokens: `↑${formatTokenCount(usage?.inputTokens ?? 0)}`,
				outTokens: `↓${formatTokenCount(usage?.outputTokens ?? 0)}`,
				agentCost: `$${(usage?.cost ?? 0).toFixed(2)}`,
				count: String(row.descendantCount),
				totalCost: `$${row.recursiveCost.toFixed(2)}`,
				age: age.text,
			},
		});
	}

	const displayItems: AgentsViewDisplayItem[] = [];
	const legends = new Map<AgentsViewSection, string>();
	const details = new Map<string, string>();
	for (const [sectionIndex, section] of SECTIONS.entries()) {
		if (sectionIndex > 0) displayItems.push({ type: "spacer" });
		displayItems.push({ type: "heading", section });
		const sectionRows = displayRows.get(section)!;
		if (sectionRows.length === 0) displayItems.push({ type: "empty", section });
		else displayItems.push(...sectionRows.map((row): AgentsViewDisplayItem => ({ type: "row", row })));

		const entries = usageEntries.get(section)!;
		const widths = {} as Record<keyof AgentsViewUsageParts, number>;
		for (const column of USAGE_COLUMNS) {
			let width = visibleWidth(USAGE_LABELS[column]);
			for (const entry of entries) {
				if (entry.empty && column !== "age") continue;
				width = Math.max(width, visibleWidth(entry.parts[column]));
			}
			widths[column] = width;
		}
		const pad = (parts: AgentsViewUsageParts, column: keyof AgentsViewUsageParts): string =>
			padCellStart(parts[column], widths[column]);
		const formatLine = (parts: AgentsViewUsageParts): string =>
			[
				`${pad(parts, "inTokens")} ${pad(parts, "outTokens")}`,
				pad(parts, "agentCost"),
				pad(parts, "count"),
				pad(parts, "totalCost"),
				pad(parts, "age"),
			].join(" · ");
		legends.set(section, formatLine(USAGE_LABELS));
		for (const entry of entries) {
			details.set(entry.identity, entry.empty ? pad(entry.parts, "age") : formatLine(entry.parts));
		}
	}

	return { counts, displayItems, usageLayout: { legends, details }, expiresAt, hasRunningRow, hasStaleAge };
}

/** Backwards-compatible usage-only projection. */
export function buildAgentsViewUsageLayout(
	rows: readonly AgentsViewRow[],
	now: number = Date.now(),
): AgentsViewUsageLayout {
	return prepareAgentsViewRender(rows, now).usageLayout;
}

/** Reference-and-expiry cache for the pure render preparation. */
export class AgentsViewRenderPreparationCache {
	private rows: readonly AgentsViewRow[] | undefined;
	private preparation: AgentsViewRenderPreparation | undefined;
	private observedAt = Number.NEGATIVE_INFINITY;

	get(rows: readonly AgentsViewRow[], now: number = Date.now()): AgentsViewRenderPreparation {
		if (this.rows !== rows || !this.preparation || now < this.observedAt || now >= this.preparation.expiresAt) {
			this.rows = rows;
			this.preparation = prepareAgentsViewRender(rows, now);
		}
		this.observedAt = now;
		return this.preparation;
	}
}

function formatSessionDuration(summary: SessionSummary, now: number): { text: string; expiresAt: number } {
	const value = summary.activeSessionId
		? (summary.created ?? summary.modified)
		: (summary.modified ?? summary.created);
	if (!value) return { text: "", expiresAt: Number.POSITIVE_INFINITY };
	const timestamp = Date.parse(value);
	if (!timestamp || Number.isNaN(timestamp)) return { text: "", expiresAt: Number.POSITIVE_INFINITY };

	const seconds = Math.max(0, Math.floor((now - timestamp) / 1000));
	if (seconds < 60) {
		return { text: `${seconds}s`, expiresAt: timestamp + (seconds + 1) * 1000 };
	}
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) {
		return { text: `${minutes}m`, expiresAt: timestamp + (minutes + 1) * 60_000 };
	}
	const hours = Math.floor(minutes / 60);
	if (hours < 24) {
		return { text: `${hours}h`, expiresAt: timestamp + (hours + 1) * 3_600_000 };
	}
	const days = Math.floor(hours / 24);
	return { text: `${days}d`, expiresAt: timestamp + (days + 1) * 86_400_000 };
}

function padCellStart(value: string, width: number): string {
	return " ".repeat(Math.max(0, width - visibleWidth(value))) + value;
}
