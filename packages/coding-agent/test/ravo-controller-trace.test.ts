import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { currentSpan, installDefaultSpanSink, type SpanEndRecord, setSpanSink, withSpan } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RavoArchive } from "../src/core/ravo/archive.js";
import { type EvaluationAdapter, type RavoControllerOptions, runRavoController } from "../src/core/ravo/controller.js";
import { ErrorBudgetLedger } from "../src/core/ravo/error-budget-ledger.js";
import { Rational } from "../src/core/ravo/rational.js";
import { emptyRavoState } from "../src/core/ravo/reducer.js";

type Artifact = { version: number };

const limits = { maxTokens: 100, maxBytes: 100, maxItems: 5, lineageDepth: 2, maxArtifactBytesPerItem: 100 };
const opponents = { criteria: [{ id: "correctness", seedWeight: 1, currentWeight: 1 }] };

async function base(
	overrides: Partial<RavoControllerOptions<Artifact>> = {},
): Promise<RavoControllerOptions<Artifact>> {
	const dir = await mkdtemp(path.join(tmpdir(), "ravo-controller-trace-"));
	const pass = (kind: "fast" | "deep" | "opponent", id = kind): EvaluationAdapter<Artifact> => ({
		id,
		kind,
		...(kind === "opponent" ? { criterionId: "correctness" } : {}),
		evaluate: vi.fn(async () => ({
			status: "completed" as const,
			value: { status: "pass" as const, score: 10 },
			tokens: 1,
		})),
	});
	return {
		runId: "run",
		context: { currentTask: { id: "task", kind: "current_task", text: "do it" } },
		contextLimits: limits,
		initialState: emptyRavoState(opponents),
		reducerConfig: { screenThreshold: 5, epsilon: 0 },
		archive: new RavoArchive({ artifactRoot: dir }),
		ledger: new ErrorBudgetLedger(Rational.of(1, 10)),
		inspect: vi.fn(async () => ({
			status: "completed" as const,
			value: { summary: "inspected", facts: ["fact"] },
			tokens: 1,
		})),
		plan: vi.fn(async ({ feedback }) => ({
			status: "completed" as const,
			value: { id: feedback ? "repair-plan" : "plan", steps: ["implement"] },
			tokens: 1,
		})),
		implement: vi.fn(async () => ({
			status: "completed" as const,
			value: { id: "p1", parentId: null, repairOf: null, artifact: { version: 1 } },
			tokens: 3,
		})),
		repair: vi.fn(async ({ candidate }) => ({
			status: "completed" as const,
			value: {
				id: `${candidate.id}-repair`,
				parentId: candidate.id,
				repairOf: candidate.id,
				artifact: { version: 2 },
			},
			tokens: 2,
		})),
		evaluators: [pass("fast"), pass("deep"), pass("opponent")],
		commitGate: vi.fn(async () => ({ accepted: true })),
		maxRounds: 3,
		maxRepairs: 2,
		deadlineMs: 10_000,
		tokenBudget: 100,
		reservationPerCall: 1,
		concurrency: 3,
		...overrides,
	};
}

/** Direct children of `parent` in span-end (completion) order. */
function childrenOf(records: readonly SpanEndRecord[], parent: SpanEndRecord): SpanEndRecord[] {
	return records.filter((record) => record.parentSpanId === parent.spanId);
}
function only(records: readonly SpanEndRecord[], name: string): SpanEndRecord {
	const matches = records.filter((record) => record.name === name);
	expect(matches, `expected exactly one ${name} span`).toHaveLength(1);
	return matches[0] as SpanEndRecord;
}
function named(records: readonly SpanEndRecord[], name: string): SpanEndRecord[] {
	return records.filter((record) => record.name === name);
}

describe("RAVO controller tracing", () => {
	let ended: SpanEndRecord[];
	beforeEach(() => {
		ended = [];
		setSpanSink((record) => ended.push(record));
	});
	afterEach(() => {
		installDefaultSpanSink();
	});

	it("nests ravo.run > ravo.round > ravo.proposal/ravo.evaluation for an accepted run and ends child-first", async () => {
		const options = await base();
		// Stand-in for the rlm.run_agent span the runtime adapter opens per child call:
		// it must land under the ravo.evaluation span through the ambient context.
		const deep = options.evaluators.find((adapter) => adapter.kind === "deep");
		if (!deep) throw new Error("missing deep evaluator");
		let ambientDuringDeep: string | undefined;
		deep.evaluate = async () => {
			ambientDuringDeep = currentSpan()?.name;
			return withSpan("rlm.run_agent", { "rlm.status": "completed" }, async () => ({
				status: "completed" as const,
				value: { status: "pass" as const, score: 10 },
				tokens: 1,
			}));
		};
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");

		const run = only(ended, "ravo.run");
		expect(run.status).toBe("ok");
		expect(run.parentSpanId).toBeUndefined();
		expect(run.attrs).toMatchObject({
			"ravo.run_id": "run",
			"ravo.resumed": false,
			"ravo.reason": "accepted",
			"ravo.rounds": 1,
			"ravo.repairs": 0,
			"ravo.spent_tokens": result.checkpoint.spentTokens,
			"ravo.certificate_digest": result.gateCertificateDigest,
		});
		expect(result.checkpoint.spentTokens).toBe(8);

		const round = only(ended, "ravo.round");
		expect(round.status).toBe("ok");
		expect(round.parentSpanId).toBe(run.spanId);
		expect(round.traceId).toBe(run.traceId);
		expect(round.attrs).toMatchObject({
			"ravo.round": 1,
			"ravo.phase": "commit_gate",
			"ravo.outcome": "accepted",
			"ravo.certificate_digest": result.gateCertificateDigest,
		});

		const proposal = only(ended, "ravo.proposal");
		expect(proposal.parentSpanId).toBe(round.spanId);
		expect(proposal.status).toBe("ok");
		expect(proposal.attrs).toMatchObject({
			"ravo.round": 1,
			"ravo.kind": "implement",
			"ravo.proposal_id": "p1",
			"ravo.candidate_tokens": 3,
		});

		const evaluations = named(ended, "ravo.evaluation");
		expect(evaluations).toHaveLength(4);
		for (const evaluation of evaluations) {
			expect(evaluation.parentSpanId).toBe(round.spanId);
			expect(evaluation.status).toBe("ok");
			expect(evaluation.attrs["ravo.proposal_id"]).toBe("p1");
		}
		const byEvaluator = new Map(evaluations.map((e) => [e.attrs["ravo.evaluator"], e]));
		expect([...byEvaluator.keys()].sort()).toEqual(["commit_gate", "deep", "fast", "opponent"]);
		expect(byEvaluator.get("fast")?.attrs).toMatchObject({ "ravo.evaluator_kind": "fast", "ravo.verdict": "pass" });
		expect(byEvaluator.get("deep")?.attrs).toMatchObject({ "ravo.evaluator_kind": "deep", "ravo.verdict": "pass" });
		expect(byEvaluator.get("opponent")?.attrs).toMatchObject({ "ravo.verdict": "pass" });
		expect(byEvaluator.get("commit_gate")?.attrs).toMatchObject({
			"ravo.evaluator_kind": "commit_gate",
			"ravo.verdict": "accepted",
			"ravo.certificate_digest": result.gateCertificateDigest,
		});
		expect(result.gateCertificateDigest).toMatch(/^[a-f0-9]{64}$/);

		// The child span opened from inside evaluate() nests under ravo.evaluation via the ambient context.
		expect(ambientDuringDeep).toBe("ravo.evaluation");
		const child = only(ended, "rlm.run_agent");
		expect(child.parentSpanId).toBe(byEvaluator.get("deep")?.spanId);

		// Spans end child-first: run is last, round just before it, all leaves before the round.
		expect(ended.at(-1)?.name).toBe("ravo.run");
		expect(ended.at(-2)?.name).toBe("ravo.round");
		const roundIndex = ended.indexOf(round);
		for (const leaf of [proposal, child, ...evaluations]) expect(ended.indexOf(leaf)).toBeLessThan(roundIndex);
		expect(ended.indexOf(child)).toBeLessThan(ended.indexOf(byEvaluator.get("deep") as SpanEndRecord));
		expect(ended.indexOf(proposal)).toBeLessThan(ended.indexOf(byEvaluator.get("fast") as SpanEndRecord));
		expect(childrenOf(ended, run)).toEqual([round]);
		expect(childrenOf(ended, round)).toHaveLength(5);
	});

	it("records a rejected round followed by an accepted repair round", async () => {
		const options = await base();
		let calls = 0;
		options.evaluators = options.evaluators.map((adapter) =>
			adapter.kind !== "fast"
				? adapter
				: {
						...adapter,
						evaluate: async () => ({
							status: "completed" as const,
							value: { status: calls++ === 0 ? ("fail" as const) : ("pass" as const), score: 10, detail: "bad" },
							tokens: 1,
						}),
					},
		);
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");

		const run = only(ended, "ravo.run");
		expect(run.status).toBe("ok");
		expect(run.attrs).toMatchObject({ "ravo.reason": "accepted", "ravo.rounds": 2, "ravo.repairs": 1 });

		const rounds = named(ended, "ravo.round");
		expect(rounds).toHaveLength(2);
		const [first, second] = rounds as [SpanEndRecord, SpanEndRecord];
		expect(first.parentSpanId).toBe(run.spanId);
		expect(second.parentSpanId).toBe(run.spanId);
		expect(first.attrs).toMatchObject({ "ravo.round": 1, "ravo.outcome": "rejected", "ravo.phase": "diagnose" });
		expect(first.status).toBe("ok");
		expect(second.attrs).toMatchObject({ "ravo.round": 2, "ravo.outcome": "accepted", "ravo.phase": "commit_gate" });

		const proposals = named(ended, "ravo.proposal");
		expect(proposals).toHaveLength(2);
		expect(proposals[0]?.parentSpanId).toBe(first.spanId);
		expect(proposals[0]?.attrs).toMatchObject({ "ravo.kind": "implement", "ravo.proposal_id": "p1" });
		expect(proposals[1]?.parentSpanId).toBe(second.spanId);
		expect(proposals[1]?.attrs).toMatchObject({
			"ravo.kind": "repair",
			"ravo.proposal_id": "p1-repair",
			"ravo.candidate_tokens": 2,
		});

		const firstEvaluations = childrenOf(ended, first).filter((s) => s.name === "ravo.evaluation");
		// A screen failure never reaches the commit gate: fast/deep/opponent only.
		expect(firstEvaluations).toHaveLength(3);
		expect(firstEvaluations.find((s) => s.attrs["ravo.evaluator"] === "fast")?.attrs["ravo.verdict"]).toBe("fail");
		const secondEvaluations = childrenOf(ended, second).filter((s) => s.name === "ravo.evaluation");
		expect(secondEvaluations).toHaveLength(4);
		for (const evaluation of secondEvaluations) expect(evaluation.attrs["ravo.proposal_id"]).toBe("p1-repair");
	});

	it("marks a resumed run and a rejected commit gate", async () => {
		const first = await base({ maxRepairs: 0 });
		first.commitGate = vi.fn(async () => ({ accepted: false, detail: "tests failed" }));
		const stopped = await runRavoController(first);
		expect(stopped.reason).toBe("repair_limit");
		const stoppedRun = only(ended, "ravo.run");
		expect(stoppedRun.status).toBe("ok");
		expect(stoppedRun.attrs).toMatchObject({
			"ravo.resumed": false,
			"ravo.reason": "repair_limit",
			"ravo.repairs": 1,
		});
		const gate = named(ended, "ravo.evaluation").find((s) => s.attrs["ravo.evaluator"] === "commit_gate");
		expect(gate?.attrs).toMatchObject({ "ravo.verdict": "rejected" });
		expect(gate?.attrs["ravo.certificate_digest"]).toBeUndefined();
		expect(only(ended, "ravo.round").attrs).toMatchObject({
			"ravo.outcome": "rejected",
			"ravo.reason": "repair_limit",
		});

		ended.length = 0;
		const resumed = await base({ runId: "run", archive: first.archive, checkpoint: stopped.checkpoint });
		const result = await runRavoController(resumed);
		expect(result.reason).toBe("accepted");
		const resumedRun = only(ended, "ravo.run");
		expect(resumedRun.attrs).toMatchObject({ "ravo.resumed": true, "ravo.reason": "accepted" });
		expect(only(ended, "ravo.proposal").attrs).toMatchObject({
			"ravo.kind": "repair",
			"ravo.proposal_id": "p1-repair",
		});
	});

	it("ends deadline and budget stops ok with ravo.reason", async () => {
		const deadline = await runRavoController(
			await base({
				deadlineMs: 1,
				now: (() => {
					let n = 0;
					return () => n++ * 2;
				})(),
			}),
		);
		expect(deadline.reason).toBe("deadline");
		let run = only(ended, "ravo.run");
		expect(run.status).toBe("ok");
		expect(run.error).toBeUndefined();
		expect(run.attrs).toMatchObject({ "ravo.reason": "deadline", "ravo.rounds": 0, "ravo.spent_tokens": 0 });
		expect(named(ended, "ravo.round")).toHaveLength(0);

		ended.length = 0;
		const budget = await runRavoController(await base({ tokenBudget: 1 }));
		expect(budget.reason).toBe("budget");
		run = only(ended, "ravo.run");
		expect(run.status).toBe("ok");
		expect(run.error).toBeUndefined();
		expect(run.attrs).toMatchObject({ "ravo.reason": "budget", "ravo.rounds": 1, "ravo.spent_tokens": 1 });
		const round = only(ended, "ravo.round");
		expect(round.status).toBe("ok");
		expect(round.attrs).toMatchObject({
			"ravo.round": 1,
			"ravo.phase": "plan",
			"ravo.outcome": "stopped",
			"ravo.reason": "budget",
		});
		expect(named(ended, "ravo.proposal")).toHaveLength(0);
		expect(ended.map((s) => s.name)).toEqual(["ravo.round", "ravo.run"]);

		ended.length = 0;
		const cancelled = new AbortController();
		cancelled.abort();
		expect((await runRavoController(await base({ signal: cancelled.signal }))).reason).toBe("cancelled");
		run = only(ended, "ravo.run");
		expect(run.status).toBe("ok");
		expect(run.attrs).toMatchObject({ "ravo.reason": "cancelled" });
	});

	it("ends spans with status error only for unexpected failures and still rethrows", async () => {
		const options = await base();
		options.implement = vi.fn(async () => ({ status: "error" as const, tokens: 1, error: "child crashed" }));
		await expect(runRavoController(options)).rejects.toThrow("child crashed");
		const run = only(ended, "ravo.run");
		expect(run.status).toBe("error");
		expect(run.error).toBe("child crashed");
		expect(run.attrs["ravo.reason"]).toBeUndefined();
		const round = only(ended, "ravo.round");
		expect(round.status).toBe("error");
		expect(round.attrs).toMatchObject({ "ravo.round": 1, "ravo.phase": "implement" });
		expect(round.attrs["ravo.outcome"]).toBeUndefined();
		const proposal = only(ended, "ravo.proposal");
		expect(proposal.status).toBe("error");
		expect(proposal.error).toBe("child crashed");
		expect(proposal.attrs["ravo.proposal_id"]).toBeUndefined();
		expect(ended.map((s) => s.name)).toEqual(["ravo.proposal", "ravo.round", "ravo.run"]);
	});

	it("never lets a failing span sink change the controller result", async () => {
		setSpanSink(() => {
			throw new Error("sink down");
		});
		const result = await runRavoController(await base());
		expect(result.reason).toBe("accepted");
	});
});
