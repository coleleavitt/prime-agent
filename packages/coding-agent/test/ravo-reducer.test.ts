import { describe, expect, it } from "vitest";
import {
	emptyRavoState,
	F_gatekeeper,
	type RavoDeepObservation,
	type RavoEvaluation,
	type RavoOpponentPool,
	type RavoScreenObservation,
	type RavoState,
	ravoBestScore,
	ravoExtendOpponents,
	ravoMarkProvisional,
	ravoObserveChampion,
	ravoPressure,
	ravoStep,
	ravoW,
	shareStrictlyIncreased,
} from "../src/core/ravo/reducer.js";

const opponents: RavoOpponentPool = {
	criteria: [
		{ id: "correctness", seedWeight: 2, currentWeight: 2 },
		{ id: "scope", seedWeight: 1, currentWeight: 1 },
		{ id: "evidence", seedWeight: 3, currentWeight: 3 },
	],
};

const config = { screenThreshold: 5, epsilon: 2 };

function evaluation(
	proposalId: string,
	options: {
		screen?: RavoScreenObservation;
		deep?: RavoDeepObservation;
		failed?: readonly string[];
		criteria?: RavoEvaluation["criteria"];
	} = {},
): RavoEvaluation {
	const failed = new Set(options.failed ?? []);
	return {
		proposalId,
		screen: options.screen ?? { status: "pass", score: 10 },
		deep: options.deep ?? { status: "pass", score: 10 },
		criteria:
			options.criteria ??
			opponents.criteria.map(({ id }) => ({ criterionId: id, status: failed.has(id) ? "fail" : "pass" })),
	};
}

function proposal(id: string, generation = 0) {
	return { id, artifact: { generation } } as const;
}

describe("RAVO v2 pure reducer properties", () => {
	it("bestScore_commitGate: committed best score is monotone for generated score sequences", () => {
		const sequences = [
			[0, 1, 2, 3],
			[9, 8, 10, 7, 11],
			[Number.MAX_SAFE_INTEGER - 2, Number.MAX_SAFE_INTEGER - 1, Number.MAX_SAFE_INTEGER],
		];
		for (const scores of sequences) {
			let state = emptyRavoState<{ generation: number }>(opponents);
			let priorBest = 0;
			for (const [index, score] of scores.entries()) {
				const id = `p-${score}-${index}`;
				const result = ravoStep(
					state,
					proposal(id, index),
					evaluation(id, { deep: { status: "pass", score } }),
					config,
				);
				const nextBest = ravoBestScore(result.state.lineage);
				expect(nextBest).toBeGreaterThanOrEqual(priorBest);
				if (result.certificate.committed) expect(score).toBeGreaterThanOrEqual(priorBest);
				state = result.state;
				priorBest = nextBest;
			}
		}
	});

	it("screen noise safety: any non-pass/noisy low screen can reject but cannot commit", () => {
		const noisyScreens: RavoScreenObservation[] = [
			{ status: "pass", score: 4 },
			{ status: "fail", score: 100 },
			{ status: "abstain" },
			{ status: "error", detail: "unavailable" },
			{ status: "pass", score: Number.NaN },
		];
		for (const [index, screen] of noisyScreens.entries()) {
			const state = emptyRavoState<{ generation: number }>(opponents);
			const id = `noise-${index}`;
			const result = ravoStep(state, proposal(id), evaluation(id, { screen }), config);
			expect(result.certificate).toMatchObject({ committed: false, rejection: "screen" });
			expect(result.state.lineage).toEqual(state.lineage);
			expect(result.state.championId).toBe(state.championId);
		}
	});

	it("F_gatekeeper is exactly the inclusive integer epsilon gate", () => {
		for (let epsilon = 0; epsilon < 20; epsilon++) {
			for (let missed = 0; missed < 20; missed++) {
				expect(F_gatekeeper(missed, epsilon)).toBe(missed <= epsilon);
			}
		}
		expect(F_gatekeeper(-1, 2)).toBe(false);
		expect(F_gatekeeper(1.5, 2)).toBe(false);
	});

	it("inv_coStep and epsilon succession hold over generated transitions", () => {
		let state = emptyRavoState<{ generation: number }>(opponents);
		for (let index = 0; index < 40; index++) {
			const id = `step-${index}`;
			const failed = index % 4 === 0 ? ["scope"] : index % 7 === 0 ? ["correctness"] : [];
			const result = ravoStep(
				state,
				proposal(id, index),
				evaluation(id, { deep: { status: "pass", score: 100 + index }, failed }),
				config,
			);
			expect(ravoW(result.state)).toBe(true);
			if (result.certificate.committed) {
				expect(result.certificate.missedCurrentWeight).toBeLessThanOrEqual(config.epsilon);
				expect(result.certificate.missedSeedWeight).toBeLessThanOrEqual(config.epsilon);
				expect(result.state.lineage.at(-1)?.parentId).toBe(state.championId);
			}
			state = result.state;
		}
	});

	it("pressure preserves support, increases target share, and monotonically tightens F", () => {
		const before: RavoOpponentPool = {
			criteria: [
				{ id: "weak", seedWeight: 1, currentWeight: 3 },
				{ id: "other", seedWeight: 4, currentWeight: 7 },
			],
		};
		const after = ravoPressure(before, ["weak"]);
		expect(after.criteria.map(({ id }) => id)).toEqual(before.criteria.map(({ id }) => id));
		expect(after.criteria.every(({ currentWeight }) => currentWeight > 0)).toBe(true);
		expect(shareStrictlyIncreased(before, after, "weak")).toBe(true);
		const oldWeight = before.criteria[0].currentWeight;
		const newWeight = after.criteria[0].currentWeight;
		for (let epsilon = 0; epsilon < 15; epsilon++) {
			if (F_gatekeeper(newWeight, epsilon)) expect(F_gatekeeper(oldWeight, epsilon)).toBe(true);
		}
	});

	it("ravoW rejects broken champion chains, duplicate evaluations, and seed-weight regression", () => {
		const valid = emptyRavoState<{ generation: number }>(opponents);
		expect(ravoW(valid)).toBe(true);
		const brokenStates: RavoState[] = [
			{ ...valid, championId: "ghost" },
			{ ...valid, evaluatedProposalIds: ["p", "p"] },
			{
				...valid,
				opponents: { criteria: [{ id: "x", seedWeight: 2, currentWeight: 1 }] },
			},
		];
		for (const state of brokenStates) expect(ravoW(state)).toBe(false);
	});

	it("evaluates each proposal id exactly once, including conservative rejections", () => {
		const state = emptyRavoState<{ generation: number }>(opponents);
		const first = ravoStep(
			state,
			proposal("once"),
			evaluation("once", { deep: { status: "error", detail: "judge failed" } }),
			config,
		);
		expect(first.certificate).toMatchObject({ committed: false, rejection: "deep" });
		expect(first.state.evaluatedProposalIds).toEqual(["once"]);
		const second = ravoStep(first.state, proposal("once"), evaluation("once"), config);
		expect(second.certificate).toMatchObject({ committed: false, rejection: "already_evaluated" });
		expect(second.state).toBe(first.state);
	});

	it("treats missing, abstaining, and errored criterion judgments as conservative misses", () => {
		for (const criteria of [
			[],
			[{ criterionId: "correctness", status: "abstain" as const }],
			[{ criterionId: "correctness", status: "error" as const, detail: "timeout" }],
		]) {
			const state = emptyRavoState<{ generation: number }>(opponents);
			const result = ravoStep(state, proposal("p"), evaluation("p", { criteria }), config);
			expect(result.certificate.committed).toBe(false);
			expect(result.certificate.missedCriterionIds).toContain("evidence");
			expect(result.certificate.criteria.every((item) => item.countedAsMissed)).toBe(true);
		}
	});

	it("emits the same deterministic certificate for the same value inputs", () => {
		const state = emptyRavoState<{ generation: number }>(opponents);
		const input = evaluation("cert", {
			criteria: [
				{ criterionId: "scope", status: "fail", detail: "out of scope" },
				{ criterionId: "evidence", status: "pass" },
				{ criterionId: "correctness", status: "pass" },
			],
		});
		const left = ravoStep(state, proposal("cert"), input, config).certificate;
		const right = ravoStep(structuredClone(state), proposal("cert"), structuredClone(input), config).certificate;
		expect(JSON.stringify(left)).toBe(JSON.stringify(right));
		expect(left.criteria.map((item) => item.criterionId)).toEqual(["correctness", "evidence", "scope"]);
	});

	it("ravoExtendOpponents is idempotent, keeps existing weights, and preserves ravoW", () => {
		const pressured = ravoPressure(opponents, ["scope"]);
		const extended = ravoExtendOpponents(pressured, ["failure:abc", "scope", "failure:abc", ""]);
		expect(extended.criteria.map(({ id }) => id)).toEqual(["correctness", "scope", "evidence", "failure:abc"]);
		expect(extended.criteria.find(({ id }) => id === "scope")?.currentWeight).toBe(2);
		expect(extended.criteria.find(({ id }) => id === "failure:abc")).toEqual({
			id: "failure:abc",
			seedWeight: 1,
			currentWeight: 1,
		});
		expect(ravoExtendOpponents(extended, ["failure:abc"])).toBe(extended);
		expect(ravoExtendOpponents(extended, [])).toBe(extended);
		const state = emptyRavoState<{ generation: number }>(extended);
		expect(ravoW(state)).toBe(true);
		expect(ravoW({ ...state, opponents: ravoExtendOpponents(state.opponents, ["failure:def"]) })).toBe(true);
	});

	it("missedWeight_app: extending the pool only tightens the gate", () => {
		const extended = ravoExtendOpponents(opponents, ["failure:abc"]);
		for (const failed of [[], ["scope"], ["correctness"], ["scope", "correctness"]]) {
			const before = ravoStep(
				emptyRavoState<{ generation: number }>(opponents),
				proposal("p"),
				evaluation("p", { failed }),
				config,
			).certificate;
			// The failure opponent is unaddressed (no observation), a conservative miss.
			const after = ravoStep(
				emptyRavoState<{ generation: number }>(extended),
				proposal("p"),
				evaluation("p", { failed }),
				config,
			).certificate;
			expect(after.missedCurrentWeight).toBeGreaterThanOrEqual(before.missedCurrentWeight);
			if (after.committed) expect(before.committed).toBe(true);
		}
	});

	it("failure opponents are pressured like any other missed criterion", () => {
		const extended = ravoExtendOpponents(opponents, ["failure:abc"]);
		const state = emptyRavoState<{ generation: number }>(extended);
		const failureCriteria: RavoEvaluation["criteria"] = extended.criteria.map(({ id }) => ({
			criterionId: id,
			status: id === "failure:abc" ? "fail" : "pass",
		}));
		const result = ravoStep(state, proposal("p"), evaluation("p", { criteria: failureCriteria }), config);
		expect(result.certificate.committed).toBe(true);
		expect(result.certificate.missedCriterionIds).toEqual(["failure:abc"]);
		expect(result.state.opponents.criteria.find(({ id }) => id === "failure:abc")?.currentWeight).toBe(2);
		expect(ravoW(result.state)).toBe(true);
	});

	it("ravoObserveChampion flags regression only inside the window and only for claimed fingerprints", () => {
		const committed = ravoStep(
			emptyRavoState<{ generation: number }>(opponents),
			proposal("champ"),
			evaluation("champ"),
			config,
		).state;
		const state = ravoMarkProvisional(committed, "champ", {
			claimedFingerprints: ["fp-a", "fp-b"],
			window: { committedTurn: 10, untilTurn: 30 },
		});
		expect(ravoW(state)).toBe(true);
		expect(state.lineage[0]).toMatchObject({
			claimedFingerprints: ["fp-a", "fp-b"],
			provisional: { committedTurn: 10, untilTurn: 30 },
		});

		// Inside the window, claimed fingerprint: regression.
		const inside = ravoObserveChampion(state, "champ", ["fp-z", "fp-b"], 20);
		expect(inside.regression).toBe(true);
		expect(inside.state.lineage[0].provisional?.observedRecurrence).toEqual({ turn: 20, fingerprints: ["fp-b"] });
		expect(inside.state.lineage.map(({ proposalId, score }) => ({ proposalId, score }))).toEqual(
			state.lineage.map(({ proposalId, score }) => ({ proposalId, score })),
		);
		expect(inside.state.opponents).toBe(state.opponents);
		expect(ravoW(inside.state)).toBe(true);

		// Window boundaries are inclusive.
		expect(ravoObserveChampion(state, "champ", ["fp-a"], 10).regression).toBe(true);
		expect(ravoObserveChampion(state, "champ", ["fp-a"], 30).regression).toBe(true);
		// Outside the window: no regression, state untouched.
		for (const turn of [9, 31, -1, 1.5]) {
			const outside = ravoObserveChampion(state, "champ", ["fp-a"], turn);
			expect(outside.regression).toBe(false);
			expect(outside.state).toBe(state);
		}
		// Unclaimed fingerprints inside the window: no regression.
		const unclaimed = ravoObserveChampion(state, "champ", ["fp-z"], 20);
		expect(unclaimed.regression).toBe(false);
		expect(unclaimed.state).toBe(state);
		// Non-provisional champion and unknown champion: no regression.
		expect(ravoObserveChampion(committed, "champ", ["fp-a"], 20).regression).toBe(false);
		expect(ravoObserveChampion(state, "ghost", ["fp-a"], 20).regression).toBe(false);
	});

	it("deepTolerance slackens only the deep gate; champion scores and bestScore stay unslacked", () => {
		const tolerant = { ...config, deepTolerance: 3 };
		const first = ravoStep(
			emptyRavoState<{ generation: number }>(opponents),
			proposal("a"),
			evaluation("a", { deep: { status: "pass", score: 10 } }),
			tolerant,
		);
		expect(first.certificate).toMatchObject({ committed: true, deepTolerance: 3 });
		const within = ravoStep(
			first.state,
			proposal("b"),
			evaluation("b", { deep: { status: "pass", score: 7 } }),
			tolerant,
		);
		expect(within.certificate.committed).toBe(true);
		expect(within.state.lineage.at(-1)?.score).toBe(7);
		expect(ravoBestScore(within.state.lineage)).toBe(10);
		const below = ravoStep(
			within.state,
			proposal("c"),
			evaluation("c", { deep: { status: "pass", score: 6 } }),
			tolerant,
		);
		expect(below.certificate).toMatchObject({ committed: false, rejection: "deep" });
		const strict = ravoStep(
			first.state,
			proposal("d"),
			evaluation("d", { deep: { status: "pass", score: 9 } }),
			config,
		);
		expect(strict.certificate).toMatchObject({ committed: false, rejection: "deep", deepTolerance: 0 });
		const invalid = ravoStep(first.state, proposal("e"), evaluation("e"), { ...config, deepTolerance: -1 });
		expect(invalid.certificate).toMatchObject({ committed: false, rejection: "invalid_input" });
	});

	it("ravoMarkProvisional ignores unknown champions and invalid windows", () => {
		const committed = ravoStep(
			emptyRavoState<{ generation: number }>(opponents),
			proposal("champ"),
			evaluation("champ"),
			config,
		).state;
		expect(ravoMarkProvisional(committed, "ghost", { claimedFingerprints: ["x"] })).toBe(committed);
		expect(
			ravoMarkProvisional(committed, "champ", {
				claimedFingerprints: ["x"],
				window: { committedTurn: 5, untilTurn: 4 },
			}),
		).toBe(committed);
		const claimedOnly = ravoMarkProvisional(committed, "champ", { claimedFingerprints: ["b", "a", "b"] });
		expect(claimedOnly.lineage[0].claimedFingerprints).toEqual(["a", "b"]);
		expect(claimedOnly.lineage[0].provisional).toBeUndefined();
		expect(ravoW(claimedOnly)).toBe(true);
		expect(
			ravoW({
				...committed,
				lineage: [{ ...committed.lineage[0], provisional: { committedTurn: 3, untilTurn: 1 } }],
			}),
		).toBe(false);
	});

	it("counterexample: invariants do not imply termination or artifact convergence", () => {
		let state = emptyRavoState<{ bit: number }>(opponents);
		const observedBits: number[] = [];
		for (let index = 0; index < 100; index++) {
			const id = `forever-${index}`;
			const bit = index % 2;
			const result = ravoStep(
				state,
				{ id, artifact: { bit } },
				evaluation(id, { deep: { status: "pass", score: 1 } }),
				config,
			);
			expect(result.certificate.committed).toBe(true);
			expect(ravoW(result.state)).toBe(true);
			observedBits.push(result.state.lineage.at(-1)?.artifact.bit ?? -1);
			state = result.state;
		}
		expect(state.lineage).toHaveLength(100);
		expect(new Set(observedBits)).toEqual(new Set([0, 1]));
	});
});
