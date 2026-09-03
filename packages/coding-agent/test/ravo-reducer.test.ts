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
