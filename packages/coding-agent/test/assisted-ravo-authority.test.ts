import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
	assistedRavoCertificateMatches,
	authorizeAssistedRavo,
	normalizeAssistedRavoState,
} from "../src/core/ravo/authority.js";
import type { JsonValue } from "../src/core/ravo/reducer.js";
import { applyRefinementProposal, loadHarnessState } from "../src/core/refinement/index.js";

const artifact = {
	summary: "candidate",
	edits: [{ action: "create", kind: "memory", title: "x", content: "y" }],
} as unknown as JsonValue;
const baseline = {
	entries: { memory: {} },
	refinements: [],
} as unknown as JsonValue;

function pass() {
	return authorizeAssistedRavo({
		proposalId: "p1",
		artifact,
		baseline,
		fastScore: 100,
		observation: { status: "pass", score: 90, failedCriteria: [] },
	});
}

describe("assisted RAVO authority", () => {
	it("fails closed when evaluation is unavailable", () => {
		const result = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "error", detail: "judge unavailable" },
		});
		expect(result.authorized).toBe(false);
		expect(result.certificate.rejection).toBe("deep");
	});

	it("binds authorization to the complete proposal and baseline", () => {
		const result = pass();
		expect(assistedRavoCertificateMatches(result, artifact, baseline)).toBe(true);
		expect(assistedRavoCertificateMatches(result, { changed: true }, baseline)).toBe(false);
		expect(assistedRavoCertificateMatches(result, artifact, { changed: true })).toBe(false);
	});

	it("migrates the legacy refinement lineage and evaluator weights", () => {
		const state = normalizeAssistedRavoState({
			lineage: [
				{
					id: "old-1",
					score: 70,
					missedCriteria: ["scope"],
					summary: "old",
					created_at: "now",
				},
			],
			evaluator: {
				criteria: [
					{ id: "evidence", weight: 1 },
					{ id: "scope", weight: 4 },
				],
			},
		});

		expect(state.championId).toBe("old-1");
		expect(state.evaluatedProposalIds).toEqual(["old-1"]);
		expect(state.lineage[0]).toMatchObject({
			proposalId: "old-1",
			parentId: null,
			score: 70,
			artifact: null,
			missedCriterionIds: ["scope"],
		});
		expect(state.opponents.criteria.find((criterion) => criterion.id === "scope")?.currentWeight).toBe(4);
	});

	it("compensates all successful edits when any proposed edit fails", () => {
		const state = loadHarnessState(mkdtempSync(join(tmpdir(), "assisted-ravo-")), "local");
		const result = applyRefinementProposal(
			state,
			{
				summary: "atomic",
				rationale: "test",
				expectedOutcome: "none",
				edits: [
					{
						action: "create",
						kind: "memory",
						id: "ok",
						title: "ok",
						content: "ok",
					},
					{
						action: "update",
						kind: "memory",
						id: "missing",
						title: "bad",
						content: "bad",
					},
				],
			},
			{ id: "r1", scope: "local" },
		);
		expect(result.appliedEdits.every((edit) => !edit.applied)).toBe(true);
		expect(state.entries.memory.ok).toBeUndefined();
		expect(state.refinements.at(-1)?.changes).toEqual([]);
	});
});
