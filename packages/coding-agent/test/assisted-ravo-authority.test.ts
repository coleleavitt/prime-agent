import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
	assistedRavoCertificateMatches,
	authorizeAssistedRavo,
	DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
	normalizeAssistedRavoState,
} from "../src/core/ravo/authority.js";
import { type JsonValue, ravoObserveChampion, ravoPressure, ravoW } from "../src/core/ravo/reducer.js";
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

	it("rejects a proposal that ignores a recurring failure opponent of weight 2 under epsilon 1", () => {
		// Seed the pool with the failure opponent already pressured to weight 2,
		// as after one earlier miss.
		const seeded = pass().nextState;
		const state = {
			...seeded,
			opponents: ravoPressure(
				{ criteria: [...seeded.opponents.criteria, { id: "failure:fp1", seedWeight: 1, currentWeight: 1 }] },
				["failure:fp1"],
			),
		};
		expect(state.opponents.criteria.find((criterion) => criterion.id === "failure:fp1")?.currentWeight).toBe(2);
		const ignored = authorizeAssistedRavo({
			proposalId: "p2",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 95, failedCriteria: [], addressedFingerprints: [] },
			state,
			failureOpponents: ["failure:fp1"],
			epsilon: 1,
			turn: 7,
		});
		expect(ignored.authorized).toBe(false);
		expect(ignored.certificate.rejection).toBe("opponents");
		expect(ignored.certificate.missedCriterionIds).toEqual(["failure:fp1"]);
		expect(ignored.certificate.missedCurrentWeight).toBe(2);
		expect(ignored.nextState.championId).toBe("p1");
		expect(ravoW(ignored.nextState)).toBe(true);

		const addressed = authorizeAssistedRavo({
			proposalId: "p3",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 95, failedCriteria: [], addressedFingerprints: ["fp1"] },
			state,
			failureOpponents: ["failure:fp1"],
			epsilon: 1,
			turn: 7,
		});
		expect(addressed.authorized).toBe(true);
		expect(addressed.certificate.missedCriterionIds).toEqual([]);
		expect(addressed.nextState.championId).toBe("p3");
		expect(addressed.nextState.lineage.at(-1)).toMatchObject({
			proposalId: "p3",
			parentId: "p1",
			claimedFingerprints: ["fp1"],
			provisional: { committedTurn: 7, untilTurn: 7 + DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS },
		});
		expect(ravoW(addressed.nextState)).toBe(true);
	});

	it("extends the pool with new failure opponents at seed weight 1 and pressures them when missed", () => {
		const first = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: [] },
			failureOpponents: ["failure:fpA", "failure:fpA", "not-a-failure"],
			epsilon: 1,
		});
		// Weight 1 miss is within epsilon 1: commits, then pressure doubles it.
		expect(first.authorized).toBe(true);
		expect(first.certificate.missedCriterionIds).toEqual(["failure:fpA"]);
		expect(first.nextState.opponents.criteria.map((criterion) => criterion.id)).toEqual([
			"evidence",
			"scope",
			"minimality",
			"contracts",
			"novelty",
			"failure:fpA",
		]);
		expect(first.nextState.opponents.criteria.at(-1)).toEqual({
			id: "failure:fpA",
			seedWeight: 1,
			currentWeight: 2,
		});
		expect(first.nextState.lineage[0].claimedFingerprints).toEqual([]);
		expect(first.nextState.lineage[0].provisional).toBeUndefined();

		// Ignoring it again now costs weight 2: rejected.
		const second = authorizeAssistedRavo({
			proposalId: "p2",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 95, failedCriteria: [], addressedFingerprints: [] },
			state: first.nextState,
			failureOpponents: ["failure:fpA"],
			epsilon: 1,
		});
		expect(second.authorized).toBe(false);
		expect(second.certificate.missedCurrentWeight).toBe(2);

		// Once the failure stops recurring the dormant opponent passes but keeps its weight.
		const dormant = authorizeAssistedRavo({
			proposalId: "p3",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 95, failedCriteria: [] },
			state: second.nextState,
			epsilon: 1,
		});
		expect(dormant.authorized).toBe(true);
		expect(dormant.certificate.criteria.find((item) => item.criterionId === "failure:fpA")).toMatchObject({
			status: "pass",
			currentWeight: 2,
			countedAsMissed: false,
		});
	});

	it("a judge-failed failure opponent is missed even when claimed as addressed", () => {
		const result = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: {
				status: "pass",
				score: 90,
				failedCriteria: ["failure:fpA"],
				addressedFingerprints: ["fpA"],
			},
			failureOpponents: ["failure:fpA"],
			epsilon: 0,
		});
		expect(result.authorized).toBe(false);
		expect(result.certificate.missedCriterionIds).toEqual(["failure:fpA"]);
	});

	it("provisional champions report regression only for claimed fingerprints inside the window", () => {
		const committed = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: ["fpA"] },
			failureOpponents: ["failure:fpA"],
			turn: 3,
			observationWindowTurns: 5,
		});
		expect(committed.nextState.lineage[0].provisional).toEqual({ committedTurn: 3, untilTurn: 8 });
		const inside = ravoObserveChampion(committed.nextState, "p1", ["fpA"], 8);
		expect(inside.regression).toBe(true);
		const outside = ravoObserveChampion(committed.nextState, "p1", ["fpA"], 9);
		expect(outside.regression).toBe(false);
		const other = ravoObserveChampion(committed.nextState, "p1", ["fpB"], 5);
		expect(other.regression).toBe(false);
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
