import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type * as PiAi from "@earendil-works/pi-ai";
import type { AssistantMessage, Model } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import { type FailureRecord, failureOpponentId, findProvisionalRegressions } from "../src/core/ravo/failure-ledger.js";
import {
	type JsonValue,
	type RavoWindowClock,
	ravoObserveChampion,
	ravoPressure as ravoPoolPressure,
} from "../src/core/ravo/reducer.js";
import type { RefinementProposal } from "../src/core/refinement/index.js";
import {
	countValidRefinementEdits,
	emptyRavoState,
	RAVO_DEFAULT_CONFIG,
	type RavoGateReport,
	type RavoHarnessState,
	type RefineKind,
	ravoBestScore,
	ravoClears,
	ravoCommit,
	ravoDecide,
	ravoEnabled,
	ravoEvaluateProposal,
	ravoFastScreen,
	ravoMissedWeight,
	ravoPressure,
	rejectedRefinementResult,
} from "../src/core/refinement/index.js";
import { saveToolforgeLedger, toolforgeLedgerPath } from "../src/core/toolforge/ledger.js";

const { completeSimpleMock } = vi.hoisted(() => ({
	completeSimpleMock: vi.fn(),
}));

vi.mock("@earendil-works/pi-ai", async (importOriginal) => {
	const actual = await importOriginal<typeof PiAi>();
	return {
		...actual,
		completeSimple: completeSimpleMock,
	};
});

beforeEach(() => {
	completeSimpleMock.mockReset();
});

function report(overrides: Partial<RavoGateReport> = {}): RavoGateReport {
	return {
		decision: "commit",
		fastScore: 100,
		deepScore: 50,
		bestScore: 0,
		missedCriteria: [],
		missedWeight: 0,
		epsilon: RAVO_DEFAULT_CONFIG.epsilon,
		screenThreshold: RAVO_DEFAULT_CONFIG.screenThreshold,
		deepTolerance: RAVO_DEFAULT_CONFIG.deepTolerance,
		rationale: "test",
		addressedFingerprints: [],
		failureOpponents: [],
		measurable: false,
		refereeCounts: { cleared: 0, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 0 },
		...overrides,
	};
}

const judgeModel: Model<"openai-completions"> = {
	id: "openai/gpt-5.5",
	name: "GPT 5.5",
	api: "openai-completions",
	provider: "prime-inference",
	baseUrl: "https://inference.primeintellect.ai/v1",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200000,
	maxTokens: 8192,
};

function assistantText(text: string): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		api: "openai-completions",
		provider: "prime-inference",
		model: "openai/gpt-5.5",
		usage: {
			input: 1,
			output: 1,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 2,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop",
		timestamp: Date.now(),
	};
}

const recurringRecord: FailureRecord = {
	fingerprint: {
		id: "fp1234567890abcd",
		kind: "python_exception",
		source: "websearch",
		exceptionClass: "KeyError",
		message: "keyerror: ?",
	},
	count: 3,
	firstSeenTurn: 2,
	lastSeenTurn: 6,
	firstSeenAt: "2026-01-01T00:00:00.000Z",
	lastSeenAt: "2026-01-01T00:01:00.000Z",
	excerpt: "KeyError: 'results'",
	addressedByProposalIds: [],
};

const judgedProposal: RefinementProposal = {
	summary: "Guard websearch results",
	rationale: "The websearch skill raised KeyError three times.",
	expectedOutcome: "No more KeyError from websearch.",
	edits: [{ action: "create", kind: "memory", title: "websearch guard", content: "Check results key." }],
};

function skillProposal(importName: string, callable = "run"): RefinementProposal {
	return {
		...judgedProposal,
		edits: [
			{
				action: "create",
				kind: "skill",
				title: `${importName} skill`,
				content: `Call ${importName}.${callable}.`,
				reference: { type: "python", import: importName, callable },
				arguments: {},
			},
		],
	};
}

function resolvePython(): string | undefined {
	const probe = spawnSync("python3", ["-c", "import sys; sys.stdout.write(sys.executable)"], { encoding: "utf8" });
	return probe.status === 0 && probe.stdout.trim() ? probe.stdout.trim() : undefined;
}

const PYTHON = resolvePython();

async function evaluate(options: {
	judge: Record<string, unknown>;
	state?: ReturnType<typeof emptyAssistedRavoState>;
	turn?: number;
	observationWindowTurns?: number;
	recurringFailures?: FailureRecord[];
	proposalId?: string;
	refineKind?: RefineKind;
	turnClock?: RavoWindowClock;
	proposal?: RefinementProposal;
}): Promise<RavoGateReport> {
	completeSimpleMock.mockResolvedValueOnce(assistantText(JSON.stringify(options.judge)));
	const proposal = options.proposal ?? judgedProposal;
	return ravoEvaluateProposal(proposal, {
		state: options.state ?? emptyAssistedRavoState(),
		config: RAVO_DEFAULT_CONFIG,
		validEdits: countValidRefinementEdits(proposal),
		conversationText: "conversation",
		harnessOverview: "overview",
		baseline: { entries: {} } as unknown as JsonValue,
		proposalId: options.proposalId ?? "refine_1",
		model: judgeModel,
		apiKey: "key",
		recurringFailures: options.recurringFailures ?? [recurringRecord],
		turn: options.turn,
		observationWindowTurns: options.observationWindowTurns,
		...(options.refineKind ? { refineKind: options.refineKind } : {}),
		...(options.turnClock ? { turnClock: options.turnClock } : {}),
	});
}

describe("ravo pure core", () => {
	it("bestScore is 0 on the empty lineage and a running max (Lean Def 1.1)", () => {
		const state = emptyRavoState();
		expect(ravoBestScore(state.lineage)).toBe(0);
		const s1 = ravoCommit(state, { id: "a", summary: "a" }, report({ deepScore: 40 }));
		const s2 = ravoCommit(s1, { id: "b", summary: "b" }, report({ deepScore: 20 }));
		expect(ravoBestScore(s2.lineage)).toBe(40);
	});

	it("lineage best score is monotone under any commit sequence (ravo_run_invariants (1))", () => {
		let state = emptyRavoState();
		let best = 0;
		const scores = [10, 5, 80, 3, 80, 99, 0];
		for (const [index, score] of scores.entries()) {
			state = ravoCommit(state, { id: `c${index}`, summary: "s" }, report({ deepScore: score }));
			const next = ravoBestScore(state.lineage);
			expect(next).toBeGreaterThanOrEqual(best);
			best = next;
		}
	});

	it("screen rejects below tau; deep gate enforces the bar minus tolerance; criteria gate enforces epsilon", () => {
		const state: RavoHarnessState = {
			...emptyRavoState(),
			lineage: [{ id: "x", score: 90, summary: "x", missedCriteria: [], created_at: "" }],
		};
		const config = { screenThreshold: 50, epsilon: 1, deepTolerance: 10 };
		expect(ravoDecide(state, config, { fastScore: 10, deepScore: 100, missedCriteria: [] })).toBe("reject_screen");
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 79, missedCriteria: [] })).toBe("reject_deep");
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 80, missedCriteria: [] })).toBe("commit");
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 95, missedCriteria: ["evidence", "scope"] })).toBe(
			"reject_criteria",
		);
		expect(ravoDecide(state, config, { fastScore: 100, deepScore: 95, missedCriteria: ["evidence"] })).toBe("commit");
	});

	it("missedWeight sums the weights of failed criteria (Rocq Def 3.2)", () => {
		const evaluator = {
			criteria: [
				{ id: "a", weight: 2, description: "" },
				{ id: "b", weight: 3, description: "" },
			],
		};
		expect(ravoMissedWeight(evaluator, [])).toBe(0);
		expect(ravoMissedWeight(evaluator, ["a"])).toBe(2);
		expect(ravoMissedWeight(evaluator, ["a", "b"])).toBe(5);
		expect(ravoClears(evaluator, ["a"], 2)).toBe(true);
		expect(ravoClears(evaluator, ["a", "b"], 2)).toBe(false);
	});

	it("pressure doubles the target weight, preserves support, and increases share (Rocq Props 4.2-4.3)", () => {
		const evaluator = {
			criteria: [
				{ id: "weak", weight: 1, description: "" },
				{ id: "other", weight: 4, description: "" },
			],
		};
		const pressured = ravoPressure(evaluator, "weak");
		const weak = pressured.criteria.find((c) => c.id === "weak")!;
		const other = pressured.criteria.find((c) => c.id === "other")!;
		expect(weak.weight).toBe(2);
		expect(other.weight).toBe(4);
		for (const c of pressured.criteria) {
			expect(c.weight).toBeGreaterThan(0);
		}
		// share strictly increases: w/S < 2w/(S+w) in cross-multiplied form
		const w = 1;
		const S = 5;
		expect(w * (S + w)).toBeLessThan(2 * w * S);
	});

	it("commit applies pressure to the first missed criterion only when one exists (Rocq Thm 7.5)", () => {
		const state = emptyRavoState();
		const clean = ravoCommit(state, { id: "a", summary: "a" }, report());
		expect(clean.evaluator).toEqual(state.evaluator);
		const pressured = ravoCommit(state, { id: "b", summary: "b" }, report({ missedCriteria: ["novelty"] }));
		const novelty = pressured.evaluator.criteria.find((c) => c.id === "novelty")!;
		expect(novelty.weight).toBe(2);
	});

	it("fast screen scores the well-formed fraction and zeroes empty proposals", () => {
		const proposal: RefinementProposal = {
			summary: "s",
			rationale: "r",
			expectedOutcome: "o",
			edits: [
				{ action: "create", kind: "memory", title: "t", content: "c" },
				{ action: "update", kind: "memory" },
			],
		};
		expect(ravoFastScreen(proposal, countValidRefinementEdits(proposal))).toBe(50);
		expect(ravoFastScreen({ ...proposal, edits: [] }, 0)).toBe(0);
	});

	it("rejected results apply no edits and carry the gate report", () => {
		const proposal: RefinementProposal = {
			summary: "s",
			rationale: "r",
			expectedOutcome: "o",
			edits: [{ action: "create", kind: "memory", title: "t", content: "c" }],
		};
		const rejected = rejectedRefinementResult(proposal, report({ decision: "reject_deep" }), {
			id: "refine_x",
			scope: "local",
		});
		expect(rejected.appliedEdits).toHaveLength(1);
		expect(rejected.appliedEdits.every((edit) => !edit.applied)).toBe(true);
		expect(rejected.ravo?.decision).toBe("reject_deep");
	});

	it("judge prompt lists recurring failures as opponents and demands addressedFingerprints", async () => {
		const result = await evaluate({
			judge: { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [] },
		});
		expect(completeSimpleMock).toHaveBeenCalledTimes(1);
		const request = completeSimpleMock.mock.calls[0][1] as { systemPrompt: string; messages: PiAi.Message[] };
		expect(request.systemPrompt).toContain('"addressedFingerprints"');
		expect(request.systemPrompt).toMatch(/outside the\s+harness's control/);
		const userMessage = request.messages[0];
		const text = userMessage.role === "user" && Array.isArray(userMessage.content) ? userMessage.content : [];
		const prompt = text.map((part) => (part.type === "text" ? part.text : "")).join("\n");
		expect(prompt).toContain("<recurring_failures>");
		expect(prompt).toContain(recurringRecord.fingerprint.id);
		expect(prompt).toContain(`- ${failureOpponentId(recurringRecord.fingerprint)} (weight 1)`);
		expect(prompt).toContain('"addressedFingerprints"');
		expect(result.failureOpponents).toEqual([failureOpponentId(recurringRecord.fingerprint)]);
		expect(result.addressedFingerprints).toEqual([]);
	});

	it("rejects a proposal that ignores a weight-2 recurring failure opponent under epsilon 1, commits one that addresses it", async () => {
		const opponentId = failureOpponentId(recurringRecord.fingerprint);
		const base = emptyAssistedRavoState();
		const state = {
			...base,
			opponents: ravoPoolPressure(
				{ criteria: [...base.opponents.criteria, { id: opponentId, seedWeight: 1, currentWeight: 1 }] },
				[opponentId],
			),
		};
		const ignored = await evaluate({
			judge: { verdict: "pass", score: 90, failedCriteria: [], addressedFingerprints: [] },
			state,
			turn: 12,
		});
		expect(ignored.decision).toBe("reject_criteria");
		expect(ignored.missedCriteria).toEqual([opponentId]);
		expect(ignored.missedWeight).toBe(2);
		expect(ignored.epsilon).toBe(1);
		expect(ignored.authorization?.authorized).toBe(false);
		expect(ignored.authorization?.nextState.championId).toBeNull();

		const addressed = await evaluate({
			judge: {
				verdict: "pass",
				score: 90,
				failedCriteria: [],
				addressedFingerprints: [recurringRecord.fingerprint.id, "unknown"],
			},
			state,
			turn: 12,
			proposalId: "refine_2",
		});
		expect(addressed.decision).toBe("commit");
		expect(addressed.addressedFingerprints).toEqual([recurringRecord.fingerprint.id]);
		expect(addressed.missedCriteria).toEqual([]);
		expect(addressed.missedWeight).toBe(0);
		const champion = addressed.authorization?.nextState.lineage.at(-1);
		expect(champion).toMatchObject({
			proposalId: "refine_2",
			score: 90,
			claimedFingerprints: [recurringRecord.fingerprint.id],
			provisional: { committedTurn: 12, untilTurn: 32 },
		});
	});

	it("commit threads the existing lineage and honors a custom observation window", async () => {
		const first = await evaluate({
			judge: {
				verdict: "pass",
				score: 70,
				failedCriteria: [],
				addressedFingerprints: [recurringRecord.fingerprint.id],
			},
			turn: 4,
			observationWindowTurns: 3,
		});
		expect(first.decision).toBe("commit");
		const firstState = first.authorization?.nextState;
		expect(firstState?.lineage.at(-1)?.provisional).toEqual({ committedTurn: 4, untilTurn: 7 });
		const second = await evaluate({
			judge: {
				verdict: "pass",
				score: 75,
				failedCriteria: [],
				addressedFingerprints: [recurringRecord.fingerprint.id],
			},
			state: firstState,
			turn: 9,
			proposalId: "refine_2",
		});
		expect(second.decision).toBe("commit");
		expect(second.bestScore).toBe(70);
		// The deep gate honors RAVO_DEFAULT_CONFIG.deepTolerance (10) under the best score.
		const slack = await evaluate({
			judge: {
				verdict: "pass",
				score: 60,
				failedCriteria: [],
				addressedFingerprints: [recurringRecord.fingerprint.id],
			},
			state: firstState,
			turn: 9,
			proposalId: "refine_slack",
		});
		expect(slack.decision).toBe("commit");
		const starved = await evaluate({
			judge: {
				verdict: "pass",
				score: 59,
				failedCriteria: [],
				addressedFingerprints: [recurringRecord.fingerprint.id],
			},
			state: firstState,
			turn: 9,
			proposalId: "refine_starved",
		});
		expect(starved.decision).toBe("reject_deep");
		expect(second.authorization?.nextState.lineage.map((entry) => entry.proposalId)).toEqual([
			"refine_1",
			"refine_2",
		]);
		expect(second.authorization?.nextState.lineage[1].parentId).toBe("refine_1");
		// Regression is a measured fault only inside the window of a claiming champion.
		const nextState = second.authorization?.nextState;
		if (!nextState) throw new Error("expected next state");
		expect(ravoObserveChampion(nextState, "refine_1", [recurringRecord.fingerprint.id], 8).regression).toBe(false);
		expect(ravoObserveChampion(nextState, "refine_2", [recurringRecord.fingerprint.id], 20).regression).toBe(true);
	});

	it("rejects a failure-triggered refine that claims nothing, and only that kind", async () => {
		const passing = { verdict: "pass", score: 90, failedCriteria: [], addressedFingerprints: [] };
		const failure = await evaluate({ judge: passing, recurringFailures: [], refineKind: "failure", turn: 5 });
		expect(failure.decision).toBe("reject_unclaimed");
		expect(failure.measurable).toBe(false);
		expect(failure.authorization?.authorized).toBe(false);
		expect(failure.authorization?.certificate).toMatchObject({ committed: false, rejection: "unclaimed" });
		// Consumed like any rejection: the id is spent, nothing is appended.
		expect(failure.authorization?.nextState.lineage).toEqual([]);
		expect(failure.authorization?.nextState.evaluatedProposalIds).toEqual(["refine_1"]);

		// A failure refine that ignores the listed failure is unclaimed, whatever
		// the opponents gate would have said on its own.
		const ignored = await evaluate({ judge: passing, refineKind: "failure", turn: 5 });
		expect(ignored.decision).toBe("reject_unclaimed");
		expect(ignored.authorization?.authorized).toBe(false);

		for (const refineKind of ["directed", "checkpoint"] as const) {
			const other = await evaluate({ judge: passing, recurringFailures: [], refineKind, turn: 5 });
			expect(other.decision).toBe("commit");
			expect(other.measurable).toBe(false);
			expect(other.authorization?.authorized).toBe(true);
		}

		const claimed = await evaluate({
			judge: { ...passing, addressedFingerprints: [recurringRecord.fingerprint.id] },
			refineKind: "failure",
			turn: 5,
		});
		expect(claimed.decision).toBe("commit");
		expect(claimed.measurable).toBe(true);

		completeSimpleMock.mockRejectedValueOnce(new Error("judge down"));
		const errored = await ravoEvaluateProposal(judgedProposal, {
			state: emptyAssistedRavoState(),
			config: RAVO_DEFAULT_CONFIG,
			validEdits: 1,
			conversationText: "c",
			harnessOverview: "o",
			baseline: {} as unknown as JsonValue,
			proposalId: "refine_err",
			model: judgeModel,
			apiKey: "key",
			recurringFailures: [recurringRecord],
			refineKind: "failure",
		});
		expect(errored.decision).toBe("reject_deep");
	});

	it("applies a claimless commit unmeasured and leaves the RAVO state exactly as it was", async () => {
		const seeded = await evaluate({
			judge: {
				verdict: "pass",
				score: 70,
				failedCriteria: [],
				addressedFingerprints: [recurringRecord.fingerprint.id],
			},
			turn: 4,
			turnClock: "ordinal",
		});
		const state = seeded.authorization?.nextState;
		if (!state) throw new Error("expected a seeded state");
		const weights = state.opponents.criteria.map((criterion) => [criterion.id, criterion.currentWeight]);

		// Misses one hygiene criterion within epsilon and outscores the champion:
		// a measured commit would append, raise the bar, and pressure "novelty".
		const unmeasured = await evaluate({
			judge: { verdict: "pass", score: 95, failedCriteria: ["novelty"], addressedFingerprints: [] },
			state,
			recurringFailures: [],
			refineKind: "checkpoint",
			turn: 9,
			turnClock: "ordinal",
			proposalId: "refine_2",
		});
		expect(unmeasured.decision).toBe("commit");
		expect(unmeasured.measurable).toBe(false);
		expect(unmeasured.authorization?.authorized).toBe(true);
		const next = unmeasured.authorization?.nextState;
		expect(next).toBe(state);
		expect(next?.lineage).toHaveLength(1);
		expect(next?.championId).toBe("refine_1");
		expect(Math.max(...(next?.lineage.map((entry) => entry.score) ?? []))).toBe(70);
		expect(next?.opponents.criteria.map((criterion) => [criterion.id, criterion.currentWeight])).toEqual(weights);

		const measured = await evaluate({
			judge: {
				verdict: "pass",
				score: 95,
				failedCriteria: ["novelty"],
				addressedFingerprints: [recurringRecord.fingerprint.id],
			},
			state,
			turn: 9,
			turnClock: "ordinal",
			proposalId: "refine_3",
		});
		expect(measured.measurable).toBe(true);
		expect(measured.authorization?.nextState.lineage).toHaveLength(2);
		expect(
			measured.authorization?.nextState.opponents.criteria.find((criterion) => criterion.id === "novelty")
				?.currentWeight,
		).toBe(2);
	});

	it("stamps a measurable commit's window with the ordinal clock, and only such windows regress", async () => {
		const fingerprint = recurringRecord.fingerprint.id;
		const judge = { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [fingerprint] };
		const ordinal = await evaluate({ judge, turn: 40, turnClock: "ordinal" });
		const ordinalState = ordinal.authorization?.nextState;
		expect(ordinalState?.lineage.at(-1)?.provisional).toEqual({ committedTurn: 40, untilTurn: 60, clock: "ordinal" });
		expect(findProvisionalRegressions(ordinalState, [fingerprint], 45)).toEqual([
			{ championId: "refine_1", fingerprints: [fingerprint], committedTurn: 40, untilTurn: 60 },
		]);

		const local = await evaluate({ judge, turn: 40, turnClock: "local-ordinal" });
		const localState = local.authorization?.nextState;
		expect(localState?.lineage.at(-1)?.provisional).toEqual({
			committedTurn: 40,
			untilTurn: 60,
			clock: "local-ordinal",
		});
		expect(findProvisionalRegressions(localState, [fingerprint], 45, "ordinal")).toEqual([]);
		expect(findProvisionalRegressions(localState, [fingerprint], 45, "local-ordinal")).toHaveLength(1);

		const legacy = await evaluate({ judge, turn: 40 });
		const legacyState = legacy.authorization?.nextState;
		expect(legacyState?.lineage.at(-1)?.provisional).toEqual({ committedTurn: 40, untilTurn: 60 });
		for (const clock of ["ordinal", "local-ordinal"] as const) {
			expect(findProvisionalRegressions(legacyState, [fingerprint], 45, clock)).toEqual([]);
		}
	});

	it("counts referee verdicts and keeps a replay-inapplicable claim out of the referee pool", async () => {
		const verified: FailureRecord = {
			...recurringRecord,
			replayCases: [
				{
					language: "python",
					source: "import prime_agent_absent",
					exceptionClass: "ModuleNotFoundError",
					verifiedAt: "2026-09-01T00:00:00.000Z",
				},
			],
		};
		// A memory-only proposal: no replay can observe its fix, so nothing runs.
		const result = await evaluate({
			judge: { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [verified.fingerprint.id] },
			recurringFailures: [verified],
		});
		expect(result.refereeVerdicts?.map((verdict) => verdict.status)).toEqual(["not_applicable"]);
		expect(result.refereeCounts).toEqual({
			cleared: 0,
			upheld: 0,
			unverifiable: 0,
			no_evidence: 0,
			not_applicable: 1,
		});
		expect(result.decision).toBe("commit");
		expect(result.authorization?.nextState.opponents.criteria.map((criterion) => criterion.id)).not.toContain(
			`referee:${verified.fingerprint.id}`,
		);

		// A skill importing the probed module, on a derivable failure whose case
		// never reproduced, fails closed on the failure opponent.
		const unverified: FailureRecord = {
			...recurringRecord,
			replayCases: [{ language: "python", source: "import prime_agent_absent" }],
		};
		const skill = skillProposal("prime_agent_absent");
		const noEvidence = await evaluate({
			judge: { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [unverified.fingerprint.id] },
			recurringFailures: [unverified],
			proposal: skill,
			proposalId: "refine_skill",
		});
		expect(noEvidence.refereeCounts.no_evidence).toBe(1);
		expect(noEvidence.missedCriteria).toEqual([failureOpponentId(unverified.fingerprint)]);
		expect(noEvidence.missedWeight).toBe(1);
	});

	it("does not replay a verified attribute probe for a memory note plus an unrelated skill, and commits", async () => {
		const attribute: FailureRecord = {
			...recurringRecord,
			fingerprint: {
				...recurringRecord.fingerprint,
				exceptionClass: "AttributeError",
				message: "module ? has no attribute ?",
			},
			excerpt: "AttributeError: module 'json' has no attribute 'load_string'",
			replayCases: [
				{
					language: "python",
					source: 'import json\ngetattr(json, "load_string")',
					exceptionClass: "AttributeError",
					verifiedAt: "2026-09-01T00:00:00.000Z",
				},
			],
		};
		const proposal: RefinementProposal = {
			...judgedProposal,
			edits: [...judgedProposal.edits, ...skillProposal("json", "loads").edits],
		};
		const result = await evaluate({
			judge: { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [attribute.fingerprint.id] },
			recurringFailures: [attribute],
			proposal,
		});
		expect(result.refereeVerdicts?.map((verdict) => verdict.status)).toEqual(["not_applicable"]);
		expect(result.decision).toBe("commit");
		expect(result.missedCriteria).toEqual([]);
		expect(result.measurable).toBe(true);
	});

	describe.skipIf(!PYTHON)("a skill claiming the module it imports", () => {
		const importName = "prime_agent_ravo_gate_pkg";
		const missing: FailureRecord = {
			...recurringRecord,
			fingerprint: {
				...recurringRecord.fingerprint,
				exceptionClass: "ModuleNotFoundError",
				message: "no module named ?",
			},
			excerpt: `ModuleNotFoundError: No module named '${importName}'`,
			replayCases: [
				{
					language: "python",
					source: `import ${importName}`,
					exceptionClass: "ModuleNotFoundError",
					verifiedAt: "2026-09-01T00:00:00.000Z",
				},
			],
		};
		const previousPython = process.env.PRIME_AGENT_KERNEL_PYTHON;

		beforeEach(() => {
			process.env.PRIME_AGENT_KERNEL_PYTHON = PYTHON;
		});

		afterEach(() => {
			if (previousPython === undefined) delete process.env.PRIME_AGENT_KERNEL_PYTHON;
			else process.env.PRIME_AGENT_KERNEL_PYTHON = previousPython;
			rmSync(toolforgeLedgerPath(), { force: true });
		});

		const claim = () =>
			evaluate({
				judge: { verdict: "pass", score: 80, failedCriteria: [], addressedFingerprints: [missing.fingerprint.id] },
				recurringFailures: [missing],
				proposal: skillProposal(importName),
			});

		it("upholds the failure while the module does not import, and rejects on both opponents", async () => {
			const result = await claim();
			expect(result.refereeVerdicts?.map((verdict) => verdict.status)).toEqual(["upheld"]);
			expect(result.decision).toBe("reject_criteria");
			expect(result.missedCriteria).toEqual([
				failureOpponentId(missing.fingerprint),
				`referee:${missing.fingerprint.id}`,
			]);
		});

		it("clears the failure once the module imports from a toolforge source root, and commits", async () => {
			const packagePath = mkdtempSync(join(tmpdir(), "ravo-toolforge-"));
			mkdirSync(join(packagePath, "src", importName), { recursive: true });
			writeFileSync(join(packagePath, "src", importName, "__init__.py"), "def run():\n    return 1\n");
			saveToolforgeLedger({
				schema: 1,
				records: [
					{
						name: importName,
						importName,
						packagePath,
						sourceSha: "",
						exitTestSha: "",
						status: "published",
						gate: [],
						installed: false,
						at: "2026-09-01T00:00:00.000Z",
						version: 1,
					},
				],
			});
			try {
				const result = await claim();
				expect(result.refereeVerdicts?.map((verdict) => verdict.status)).toEqual(["cleared"]);
				expect(result.decision).toBe("commit");
				expect(result.missedCriteria).toEqual([]);
			} finally {
				rmSync(packagePath, { recursive: true, force: true });
			}
		});
	});

	it("fails closed on judge errors and still reports the failure opponents", async () => {
		completeSimpleMock.mockRejectedValueOnce(new Error("judge down"));
		const result = await ravoEvaluateProposal(judgedProposal, {
			state: emptyAssistedRavoState(),
			config: RAVO_DEFAULT_CONFIG,
			validEdits: 1,
			conversationText: "c",
			harnessOverview: "o",
			baseline: {} as unknown as JsonValue,
			proposalId: "refine_err",
			model: judgeModel,
			apiKey: "key",
			recurringFailures: [recurringRecord],
			turn: 1,
		});
		expect(result.decision).toBe("reject_deep");
		expect(result.judgeError).toBe("judge down");
		expect(result.failureOpponents).toEqual([failureOpponentId(recurringRecord.fingerprint)]);
		expect(result.addressedFingerprints).toEqual([]);
		expect(result.authorization?.authorized).toBe(false);
	});

	it("ravoEnabled defaults on and honors the kill switch", () => {
		expect(ravoEnabled({})).toBe(true);
		expect(ravoEnabled({ PRIME_AGENT_RAVO: "1" })).toBe(true);
		expect(ravoEnabled({ PRIME_AGENT_RAVO: "0" })).toBe(false);
		expect(ravoEnabled({ PRIME_AGENT_RAVO: "off" })).toBe(false);
	});
});
