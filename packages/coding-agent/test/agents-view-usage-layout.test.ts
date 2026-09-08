import { describe, expect, it } from "vitest";
import type { AgentsViewRow } from "../src/modes/agents-view/agents-view-state.js";
import {
	AgentsViewRenderPreparationCache,
	prepareAgentsViewRender,
} from "../src/modes/agents-view/agents-view-usage-layout.js";
import type { SessionSummary } from "../src/modes/daemon/daemon-session-list.js";

const NOW = Date.parse("2026-01-01T00:10:00Z");

function summary(overrides: Partial<SessionSummary> = {}): SessionSummary {
	return {
		id: "agent",
		activeSessionId: "agent",
		lifecycle: "live",
		activity: "idle",
		isSessionActive: false,
		sessionId: "agent-session",
		cwd: "/tmp",
		isStreaming: false,
		isCompacting: false,
		attachedClients: 0,
		messageCount: 1,
		sessionActions: { queuedCount: 0, steering: [], followUps: [] },
		created: "2026-01-01T00:09:00Z",
		...overrides,
	};
}

function row(overrides: Partial<AgentsViewRow> = {}): AgentsViewRow {
	return {
		kind: "agent",
		section: "idle",
		summary: summary(),
		title: "agent",
		subtitle: "",
		statusLabel: "",
		depth: 0,
		selectable: true,
		runningSubagentCount: 0,
		recursiveCost: 1.25,
		descendantCount: 1,
		identity: "agent:agent",
		...overrides,
	};
}

describe("agents view render preparation", () => {
	it("indexes sections and usage in one reusable result", () => {
		const parent = row({
			section: "running",
			summary: summary({ activity: "working", usage: { inputTokens: 12_000, outputTokens: 1_200, cost: 0.5 } }),
		});
		const nested = row({
			kind: "subagent",
			depth: 1,
			section: "idle",
			identity: "agent:child",
			parentIdentity: parent.identity,
			descendantCount: 0,
			recursiveCost: 0.25,
			summary: summary({ id: "child", activeSessionId: "child", sessionId: "child-session" }),
		});
		const inactive = row({
			section: "inactive",
			identity: "agent:saved",
			summary: summary({ id: "saved", activeSessionId: undefined, sessionId: "saved-session" }),
		});

		const result = prepareAgentsViewRender([parent, nested, inactive], NOW);
		expect(result.counts).toEqual({ running: 1, idle: 0, inactive: 1 });
		expect(result.displayItems.map((item) => item.type)).toEqual([
			"heading",
			"row",
			"row",
			"spacer",
			"heading",
			"empty",
			"spacer",
			"heading",
			"row",
		]);
		expect(result.usageLayout.details.get(nested.identity)).toContain("$0.25");
		expect(result.usageLayout.legends.get("running")).toContain("$total");
		expect(result.expiresAt).toBe(Date.parse("2026-01-01T00:11:00Z"));
	});

	it("reuses preparation for the same row array until its age label expires", () => {
		const rows = [row({ summary: summary({ created: "2026-01-01T00:09:30Z" }) })];
		const cache = new AgentsViewRenderPreparationCache();
		const first = cache.get(rows, NOW);
		expect(cache.get(rows, NOW + 500)).toBe(first);
		expect(cache.get([...rows], NOW + 500)).not.toBe(first);

		const beforeExpiry = cache.get(rows, NOW + 500);
		expect(cache.get(rows, NOW + 999)).toBe(beforeExpiry);
		const afterExpiry = cache.get(rows, NOW + 1_000);
		expect(afterExpiry).not.toBe(beforeExpiry);
		expect(afterExpiry.usageLayout.details.get(rows[0]!.identity)).toContain("31s");
	});

	it("omits usage cells for empty sessions but retains their age", () => {
		const empty = row({
			section: "inactive",
			summary: summary({ activeSessionId: undefined, messageCount: 0, modified: "2026-01-01T00:08:00Z" }),
		});
		const detail = prepareAgentsViewRender([empty], NOW).usageLayout.details.get(empty.identity);
		expect(detail).toBe(" 2m");
	});
});
