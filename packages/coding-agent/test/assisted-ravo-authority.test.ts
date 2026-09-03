import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { assistedRavoCertificateMatches, authorizeAssistedRavo } from "../src/core/ravo/authority.js";
import type { JsonValue } from "../src/core/ravo/reducer.js";
import { applyRefinementProposal, loadHarnessState } from "../src/core/refinement/index.js";

const artifact = {
	summary: "candidate",
	edits: [{ action: "create", kind: "memory", title: "x", content: "y" }],
} as unknown as JsonValue;
const baseline = { entries: { memory: {} }, refinements: [] } as unknown as JsonValue;

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

	it("compensates all successful edits when any proposed edit fails", () => {
		const state = loadHarnessState(mkdtempSync(join(tmpdir(), "assisted-ravo-")), "local");
		const result = applyRefinementProposal(
			state,
			{
				summary: "atomic",
				rationale: "test",
				expectedOutcome: "none",
				edits: [
					{ action: "create", kind: "memory", id: "ok", title: "ok", content: "ok" },
					{ action: "update", kind: "memory", id: "missing", title: "bad", content: "bad" },
				],
			},
			{ id: "r1", scope: "local" },
		);
		expect(result.appliedEdits.every((edit) => !edit.applied)).toBe(true);
		expect(state.entries.memory.ok).toBeUndefined();
		expect(state.refinements.at(-1)?.changes).toEqual([]);
	});
});
