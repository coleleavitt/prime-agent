import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it, vi } from "vitest";
import { RavoArchive, RavoStaleCommitError } from "../src/core/ravo/archive.js";
import { type EvaluationAdapter, type RavoControllerOptions, runRavoController } from "../src/core/ravo/controller.js";
import { ErrorBudgetLedger } from "../src/core/ravo/error-budget-ledger.js";
import { Rational } from "../src/core/ravo/rational.js";
import { emptyRavoState } from "../src/core/ravo/reducer.js";

const limits = { maxTokens: 100, maxBytes: 100, maxItems: 5, lineageDepth: 2, maxArtifactBytesPerItem: 100 };
const opponents = { criteria: [{ id: "correctness", seedWeight: 1, currentWeight: 1 }] };
async function base(
	overrides: Partial<RavoControllerOptions<{ version: number }>> = {},
): Promise<RavoControllerOptions<{ version: number }>> {
	const dir = await mkdtemp(path.join(tmpdir(), "ravo-controller-"));
	const pass = (kind: "fast" | "deep" | "opponent", id = kind): EvaluationAdapter<{ version: number }> => ({
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
			tokens: 1,
		})),
		repair: vi.fn(async ({ candidate }) => ({
			status: "completed" as const,
			value: {
				id: `${candidate.id}-repair`,
				parentId: candidate.id,
				repairOf: candidate.id,
				artifact: { version: 2 },
			},
			tokens: 1,
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

describe("RAVO controller", () => {
	it("commits a passing candidate exactly once and archives the sole champion update", async () => {
		const options = await base();
		const events: string[] = [];
		options.onProgress = (event) => events.push(event.type);
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(options.implement).toHaveBeenCalledOnce();
		expect(result.gateCertificateDigest).toMatch(/^[a-f0-9]{64}$/);
		expect((await options.archive.recover()).revision).toBe(1);
		expect(events).toContain("evaluation");
	});
	it.each(["fast", "deep", "opponent"] as const)("repairs after a %s rejection and reevaluates", async (kind) => {
		const options = await base();
		let calls = 0;
		options.evaluators = options.evaluators.map((adapter) =>
			adapter.kind !== kind
				? adapter
				: {
						...adapter,
						evaluate: async () => ({
							status: "completed",
							value: { status: calls++ === 0 ? "fail" : "pass", score: 10, detail: "bad" },
							tokens: 1,
						}),
					},
		);
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(options.implement).toHaveBeenCalledOnce();
		expect(options.repair).toHaveBeenCalledOnce();
		expect(result.checkpoint.certificates).toHaveLength(2);
		expect(result.checkpoint.candidate?.repairOf).toBe("p1");
	});
	it("treats evaluator errors and abstentions conservatively", async () => {
		const options = await base({ maxRepairs: 0 });
		options.evaluators = options.evaluators.map((a) =>
			a.kind === "deep"
				? {
						...a,
						evaluate: async () => {
							throw new Error("down");
						},
					}
				: a.kind === "opponent"
					? {
							...a,
							evaluate: async () => ({ status: "completed" as const, value: { status: "abstain" }, tokens: 1 }),
						}
					: a,
		);
		const result = await runRavoController(options);
		expect(result.reason).toBe("repair_limit");
		expect(result.certificate?.committed).toBe(false);
	});
	it.each(["noop", "advice", "failure"])("contains supervisor %s behavior", async (mode) => {
		const options = await base();
		options.shouldConsultSupervisor = () => true;
		options.supervisor =
			mode === "failure"
				? async () => {
						throw new Error("no supervisor");
					}
				: async () => ({
						status: "completed",
						value: { intervene: mode === "advice", advice: mode === "advice" ? "focus" : undefined },
						tokens: 1,
					});
		await expect(runRavoController(options)).resolves.toMatchObject({ reason: "accepted" });
		expect(options.implement).toHaveBeenCalledWith(
			expect.objectContaining(
				mode === "advice" ? { plan: expect.objectContaining({ supervisorAdvice: "focus" }) } : {},
			),
			expect.anything(),
		);
	});
	it("stops for cancellation, reservation budget, and deadline", async () => {
		const cancelled = new AbortController();
		cancelled.abort();
		expect((await runRavoController(await base({ signal: cancelled.signal }))).reason).toBe("cancelled");
		expect((await runRavoController(await base({ tokenBudget: 1 }))).reason).toBe("budget");
		expect(
			(
				await runRavoController(
					await base({
						deadlineMs: 1,
						now: (() => {
							let n = 0;
							return () => n++ * 2;
						})(),
					}),
				)
			).reason,
		).toBe("deadline");
	});
	it("returns stale CAS without updating lineage", async () => {
		const options = await base();
		options.archive.accept = vi.fn(async () => {
			throw new RavoStaleCommitError();
		});
		const result = await runRavoController(options);
		expect(result.reason).toBe("stale_cas");
		expect(result.checkpoint.state.championId).toBeNull();
	});
	it("restarts from a rejection checkpoint without proposing the original again", async () => {
		const first = await base({ maxRepairs: 0 });
		first.evaluators = first.evaluators.map((a) =>
			a.kind === "fast"
				? { ...a, evaluate: async () => ({ status: "completed" as const, value: { status: "fail" }, tokens: 1 }) }
				: a,
		);
		const stopped = await runRavoController(first);
		const resumed = await base({ runId: "run", archive: first.archive, checkpoint: stopped.checkpoint });
		const result = await runRavoController(resumed);
		expect(result.reason).toBe("accepted");
		expect(resumed.implement).not.toHaveBeenCalled();
		expect(resumed.repair).toHaveBeenCalledOnce();
	});
	it("archives external gate rejects and repairs them", async () => {
		const options = await base();
		let calls = 0;
		options.commitGate = async () => ({ accepted: calls++ > 0, detail: "tests failed" });
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(options.repair).toHaveBeenCalledOnce();
	});
	it("orders inspect, plan, implement and checkpoints every transition", async () => {
		const options = await base();
		const order: string[] = [];
		const checkpoints: string[] = [];
		const inspect = options.inspect;
		options.inspect = async (...args) => {
			order.push("inspect");
			return inspect(...args);
		};
		const plan = options.plan;
		options.plan = async (...args) => {
			order.push("plan");
			return plan(...args);
		};
		const implement = options.implement;
		options.implement = async (...args) => {
			order.push("implement");
			return implement(...args);
		};
		options.onCheckpoint = async (checkpoint) => {
			checkpoints.push(checkpoint.phase);
		};
		await runRavoController(options);
		expect(order).toEqual(["inspect", "plan", "implement"]);
		expect(checkpoints).toEqual(expect.arrayContaining(["inspect", "plan", "implement", "evaluate", "commit_gate"]));
	});
	it("continues a retained async worker handle", async () => {
		const options = await base();
		const completed = options.implement;
		options.implement = async (input, callOptions) => ({
			status: "deferred",
			handle: "child-1",
			wait: async () => completed(input, callOptions),
		});
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(result.checkpoint.workerHandle).toBe("child-1");
	});
	it("binds the champion CAS before evaluation begins", async () => {
		const options = await base();
		const competingDigest = "c".repeat(64);
		const deep = options.evaluators.find((adapter) => adapter.kind === "deep");
		if (!deep) throw new Error("missing deep evaluator");
		deep.evaluate = async () => {
			await options.archive.accept(
				{ proposalId: "competing" },
				{ revision: 0, championDigest: null },
				competingDigest,
			);
			return { status: "completed", value: { status: "pass", score: 10 }, tokens: 1 };
		};
		const result = await runRavoController(options);
		expect(result.reason).toBe("stale_cas");
		expect(result.checkpoint.archiveBaseline).toEqual({ revision: 0, championDigest: null });
		expect((await options.archive.recover()).championDigest).toBe(competingDigest);
	});

	it("restores the error budget from a restart checkpoint", async () => {
		const first = await base({ maxRepairs: 0 });
		first.ledger.registerCalibration({
			id: "calibration",
			evaluatorId: "deep",
			historyKey: "round",
			falsePassUpperBound: Rational.of(1, 20),
			basis: "test calibration",
		});
		const deep = first.evaluators.find((adapter) => adapter.kind === "deep");
		if (!deep) throw new Error("missing deep evaluator");
		deep.probabilistic = true;
		deep.allocation = () => ({
			decisionId: "decision-1",
			evaluatorId: "deep",
			historyKey: "round",
			calibrationId: "calibration",
			delta: Rational.of(1, 20),
		});
		deep.evaluate = async () => ({ status: "completed", value: { status: "fail", score: 0 }, tokens: 1 });
		const stopped = await runRavoController(first);
		expect(stopped.checkpoint.errorBudget.spentDelta).toBe("1/20");

		const resumed = await base({ archive: first.archive, checkpoint: stopped.checkpoint });
		await runRavoController(resumed);
		expect(resumed.ledger.spentDelta.toString()).toBe("1/20");
		expect(resumed.ledger.allocatedDelta.toString()).toBe("1/20");
	});
});
