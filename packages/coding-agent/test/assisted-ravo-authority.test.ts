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
import { findProvisionalRegressions } from "../src/core/ravo/failure-ledger.js";
import { type JsonValue, type RavoState, ravoObserveChampion, ravoPressure, ravoW } from "../src/core/ravo/reducer.js";
import { type RefereeVerdict, refereeVerdict } from "../src/core/ravo/referee.js";
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

	it("keeps the whole state when a provisional window carries a clock this build does not know", () => {
		const committed = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: ["novelty"], addressedFingerprints: ["fpA"] },
			failureOpponents: ["failure:fpA"],
			turn: 3,
			turnClock: "ordinal",
		}).nextState;
		const future = JSON.parse(JSON.stringify(committed)) as RavoState<JsonValue>;
		(future.lineage[0].provisional as unknown as { clock: string }).clock = "wallclock";
		expect(ravoW(future)).toBe(false);

		const state = normalizeAssistedRavoState(future);
		expect(state.championId).toBe("p1");
		expect(state.evaluatedProposalIds).toEqual(["p1"]);
		expect(state.opponents).toEqual(committed.opponents);
		expect(state.lineage[0]).toMatchObject({ proposalId: "p1", claimedFingerprints: ["fpA"] });
		// Read as a legacy window: kept, but it never regresses.
		expect(state.lineage[0].provisional).toEqual({
			committedTurn: 3,
			untilTurn: 3 + DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
		});
		expect(findProvisionalRegressions(state, ["fpA"], 5)).toEqual([]);
		expect(normalizeAssistedRavoState(JSON.parse(JSON.stringify(committed)))).toEqual(committed);
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

	it("charges a claim whose evidence is missing and lets a claim no replay can speak to stand", () => {
		const verdicts: RefereeVerdict[] = [
			refereeVerdict("fpA", "no_evidence", "no_evidence: no replay case ever reproduced this failure"),
			refereeVerdict("fpB", "not_applicable", "not_applicable: the proposal changes no skill"),
		];
		const result = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: {
				status: "pass",
				score: 90,
				detail: "judged",
				failedCriteria: [],
				addressedFingerprints: ["fpA", "fpB"],
			},
			failureOpponents: ["failure:fpA", "failure:fpB"],
			refereeVerdicts: verdicts,
			epsilon: 0,
		});
		expect(result.authorized).toBe(false);
		expect(result.certificate.missedCriterionIds).toEqual(["failure:fpA"]);
		const criteria = new Map(result.certificate.criteria.map((item) => [item.criterionId, item]));
		expect(criteria.get("failure:fpA")?.detail).toBe(verdicts[0].detail);
		expect(criteria.get("failure:fpB")).toMatchObject({ status: "pass", detail: "judged" });
		// Neither verdict is replay evidence, so neither adds a referee opponent.
		expect([...criteria.keys()].filter((id) => id.startsWith("referee:"))).toEqual([]);
	});

	it("fails a persisted referee criterion closed when the later claim has no evidence", () => {
		const base = pass().nextState;
		const state: RavoState<JsonValue> = {
			...base,
			opponents: {
				criteria: [
					...base.opponents.criteria,
					{ id: "failure:fpA", seedWeight: 1, currentWeight: 1 },
					{ id: "referee:fpA", seedWeight: 1, currentWeight: 1 },
				],
			},
		};
		const claim = (proposalId: string, verdict: RefereeVerdict) =>
			authorizeAssistedRavo({
				proposalId,
				artifact,
				baseline,
				fastScore: 100,
				observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: ["fpA"] },
				state,
				failureOpponents: ["failure:fpA"],
				refereeVerdicts: [verdict],
				epsilon: 1,
			});
		const missing = claim("p2", refereeVerdict("fpA", "no_evidence", "no_evidence: evidence is gone"));
		expect(missing.authorized).toBe(false);
		expect(missing.certificate.missedCriterionIds).toEqual(["failure:fpA", "referee:fpA"]);
		expect(missing.certificate.missedCurrentWeight).toBe(2);

		const inapplicable = claim("p3", refereeVerdict("fpA", "not_applicable", "not_applicable: memory fix"));
		expect(inapplicable.authorized).toBe(true);
		expect(inapplicable.certificate.missedCriterionIds).toEqual([]);
	});

	it("passes persisted criteria /refine never observes as dormant, keeping their weights", () => {
		const base = pass().nextState;
		const state: RavoState<JsonValue> = {
			...base,
			opponents: {
				criteria: [
					...base.opponents.criteria,
					{ id: "arc:all-levels", seedWeight: 1, currentWeight: 2 },
					{ id: "arc:no-crash", seedWeight: 1, currentWeight: 1 },
				],
			},
		};
		const result = authorizeAssistedRavo({
			proposalId: "p2",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 95, failedCriteria: [] },
			state,
			epsilon: 1,
		});
		expect(result.authorized).toBe(true);
		expect(result.certificate.missedCriterionIds).toEqual([]);
		expect(result.certificate.criteria.find((item) => item.criterionId === "arc:all-levels")).toMatchObject({
			status: "pass",
			countedAsMissed: false,
			currentWeight: 2,
			detail: "dormant: not a /refine criterion",
		});
		expect(result.nextState.opponents.criteria.find((criterion) => criterion.id === "arc:all-levels")).toEqual({
			id: "arc:all-levels",
			seedWeight: 1,
			currentWeight: 2,
		});
	});

	it("applies the unclaimed-commit policy only to a commit that claims nothing", () => {
		const state = pass().nextState;
		const decide = (proposalId: string, unclaimedCommit: "reject" | "unmeasured", addressed: string[]) =>
			authorizeAssistedRavo({
				proposalId,
				artifact,
				baseline,
				fastScore: 100,
				observation: { status: "pass", score: 95, failedCriteria: ["novelty"], addressedFingerprints: addressed },
				state,
				failureOpponents: ["failure:fpA"],
				epsilon: 2,
				turn: 30,
				turnClock: "ordinal",
				unclaimedCommit,
			});

		const rejected = decide("p2", "reject", []);
		expect(rejected.authorized).toBe(false);
		expect(rejected.certificate).toMatchObject({ committed: false, rejection: "unclaimed" });
		expect(rejected.nextState.lineage).toEqual(state.lineage);
		expect(rejected.nextState.evaluatedProposalIds).toEqual([...state.evaluatedProposalIds, "p2"]);
		expect(rejected.nextState.opponents.criteria.find((criterion) => criterion.id === "novelty")?.currentWeight).toBe(
			1,
		);
		expect(ravoW(rejected.nextState)).toBe(true);

		const unmeasured = decide("p2", "unmeasured", []);
		expect(unmeasured.authorized).toBe(true);
		expect(unmeasured.nextState).toBe(state);

		for (const policy of ["reject", "unmeasured"] as const) {
			const claimed = decide("p3", policy, ["fpA"]);
			expect(claimed.authorized).toBe(true);
			expect(claimed.nextState.lineage.at(-1)).toMatchObject({
				proposalId: "p3",
				claimedFingerprints: ["fpA"],
				provisional: { committedTurn: 30, untilTurn: 30 + DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS, clock: "ordinal" },
			});
			expect(ravoW(claimed.nextState)).toBe(true);
			expect(findProvisionalRegressions(claimed.nextState, ["fpA"], 31)).toHaveLength(1);
		}
	});

	it("stamps a window on the local ordinal that survives a reload and regresses only on that clock", () => {
		const committed = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: ["fpA"] },
			failureOpponents: ["failure:fpA"],
			turn: 2,
			turnClock: "local-ordinal",
		}).nextState;
		const window = { committedTurn: 2, untilTurn: 2 + DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS, clock: "local-ordinal" };
		expect(committed.lineage[0].provisional).toEqual(window);
		expect(ravoW(committed)).toBe(true);

		const reloaded = normalizeAssistedRavoState(JSON.parse(JSON.stringify(committed)));
		expect(reloaded).toEqual(committed);
		expect(findProvisionalRegressions(reloaded, ["fpA"], 5, "local-ordinal")).toHaveLength(1);
		expect(findProvisionalRegressions(reloaded, ["fpA"], 5, "ordinal")).toEqual([]);
	});

	it("ignores a provisional window opened on the per-session clock", () => {
		const committed = authorizeAssistedRavo({
			proposalId: "p1",
			artifact,
			baseline,
			fastScore: 100,
			observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: ["fpA"] },
			failureOpponents: ["failure:fpA"],
			turn: 3,
		});
		expect(committed.nextState.lineage[0].provisional).toEqual({
			committedTurn: 3,
			untilTurn: 3 + DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
		});
		expect(findProvisionalRegressions(committed.nextState, ["fpA"], 5)).toEqual([]);
		expect(
			ravoW({
				...committed.nextState,
				lineage: [
					{
						...committed.nextState.lineage[0],
						provisional: { committedTurn: 3, untilTurn: 9, clock: "session" as unknown as "ordinal" },
					},
				],
			}),
		).toBe(false);
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
