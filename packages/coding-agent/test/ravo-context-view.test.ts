import { describe, expect, it } from "vitest";
import {
	buildBoundedContextView,
	type ContextArchive,
	encodeLineage,
	type LineageSummary,
	truncateUtf8,
} from "../src/core/ravo/context-view.js";

const limits = {
	maxTokens: 64,
	maxBytes: 64,
	maxItems: 4,
	lineageDepth: 2,
	maxArtifactBytesPerItem: 32,
};

function archive(items: ContextArchive["items"] = []): ContextArchive {
	return {
		currentTask: { id: "task", kind: "current_task", text: "ship it" },
		champion: { id: "champ", kind: "champion", text: "best" },
		constraints: [{ id: "constraint", kind: "constraint", text: "safe" }],
		items,
	};
}

describe("bounded recursive context view", () => {
	it("satisfies window_le bounds and preserves required context first", () => {
		const view = buildBoundedContextView(
			archive(
				Array.from({ length: 100 }, (_, index) => ({ id: `a${index}`, kind: "archive", text: "x".repeat(20) })),
			),
			limits,
		);
		expect(view.usage.bytes).toBeLessThanOrEqual(limits.maxBytes);
		expect(view.usage.tokens).toBeLessThanOrEqual(limits.maxTokens);
		expect(view.usage.items).toBeLessThanOrEqual(limits.maxItems);
		expect(view.items.slice(0, 3).map((item) => item.kind)).toEqual(["current_task", "champion", "constraint"]);
		expect(view.omissions.some((item) => item.reason === "max_items" && item.count > 0)).toBe(true);
	});

	it("encodeLineage visits at most lineageDepth ancestors", () => {
		const lineage: LineageSummary = {
			id: "3",
			summary: "third",
			parent: { id: "2", summary: "second", parent: { id: "1", summary: "first" } },
		};
		const result = encodeLineage(lineage, 2);
		expect(result).toEqual({
			lineage: { id: "3", summary: "third", parent: { id: "2", summary: "second" } },
			omitted: true,
		});
	});

	it("stays bounded as the archive grows and selects deterministically", () => {
		const huge = Array.from({ length: 10_000 }, (_, index) => ({
			id: `item-${index.toString().padStart(5, "0")}`,
			kind: "archive" as const,
			text: "z".repeat(10_000),
			priority: index % 7,
		}));
		const first = buildBoundedContextView(archive(huge), limits);
		const second = buildBoundedContextView(archive([...huge].reverse()), limits);
		expect(first).toEqual(second);
		expect(first.usage).toMatchObject({ items: expect.any(Number) });
		expect(first.usage.bytes).toBeLessThanOrEqual(64);
		expect(JSON.stringify(first).length).toBeLessThan(1_000_000);
	});

	it("counts UTF-8 bytes without splitting a code point", () => {
		expect(truncateUtf8("a😀é", 5)).toEqual({ value: "a😀", truncated: true });
		const view = buildBoundedContextView(
			{ currentTask: { id: "task", kind: "current_task", text: "😀".repeat(100) } },
			{ ...limits, maxBytes: 7, maxTokens: 100 },
		);
		expect(view.items[0]?.content).toBe("😀");
		expect(view.usage.bytes).toBe(4);
	});

	it("keeps oversized artifacts opaque with hashes, redactions, and an explicit reason", () => {
		const view = buildBoundedContextView(
			{
				currentTask: { id: "task", kind: "current_task", text: "task" },
				items: [
					{
						id: "blob",
						kind: "artifact",
						artifact: {
							id: "artifact-1",
							uri: "artifact://durable/1",
							byteLength: Number.MAX_SAFE_INTEGER,
							sha256: "a".repeat(64),
							redactions: ["secret"],
						},
					},
				],
			},
			{ ...limits, maxBytes: 100, maxTokens: 100 },
		);
		const blob = view.items.find((item) => item.id === "blob");
		expect(blob?.artifact?.uri).toBe("artifact://durable/1");
		expect(blob?.content).toBeUndefined();
		expect(blob?.reasons).toContain("artifact_too_large");
		expect(blob?.redactions).toContain("secret");
		expect(blob?.sha256).toMatch(/^[0-9a-f]{64}$/);
	});

	it("handles zero and adversarial limits with explicit omissions", () => {
		const view = buildBoundedContextView(archive([{ id: "bad", kind: "archive", text: "x" }]), {
			maxTokens: 0,
			maxBytes: 0,
			maxItems: 0,
			lineageDepth: 0,
			maxArtifactBytesPerItem: 0,
		});
		expect(view.items).toEqual([]);
		expect(view.usage).toEqual({ bytes: 0, tokens: 0, items: 0 });
		expect(view.omissions).toEqual([{ reason: "max_items", count: 4 }]);
		expect(() => buildBoundedContextView(archive(), { ...limits, maxBytes: Number.MAX_VALUE })).toThrow(RangeError);
	});
});
