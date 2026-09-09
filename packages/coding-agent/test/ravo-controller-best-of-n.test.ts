import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { installDefaultSpanSink, type SpanEndRecord, setSpanSink } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RavoArchive } from "../src/core/ravo/archive.js";
import {
	type ControllerProposal,
	type EvaluationAdapter,
	type RavoChildResult,
	type RavoControllerOptions,
	type RavoProgressEvent,
	runRavoController,
} from "../src/core/ravo/controller.js";
import { ErrorBudgetLedger } from "../src/core/ravo/error-budget-ledger.js";
import { Rational } from "../src/core/ravo/rational.js";
import { emptyRavoState, type GateStatus } from "../src/core/ravo/reducer.js";

type Artifact = { version: number };
type Proposal = ControllerProposal<Artifact>;
type Screen = { status: GateStatus; score?: number };

const limits = { maxTokens: 100, maxBytes: 100, maxItems: 5, lineageDepth: 2, maxArtifactBytesPerItem: 100 };
const opponents = { criteria: [{ id: "correctness", seedWeight: 1, currentWeight: 1 }] };

function proposal(id: string, version = 1): Proposal {
	return { id, parentId: null, repairOf: null, artifact: { version } };
}
/** An implement child that returns the scripted candidates in call order (wrapping around). */
function scriptedImplement(script: readonly { proposal: Proposal; tokens: number }[]) {
	let calls = 0;
	return vi.fn(async (): Promise<RavoChildResult<Proposal>> => {
		const entry = script[calls++ % script.length];
		if (!entry) throw new Error("unreachable");
		return { status: "completed", value: entry.proposal, tokens: entry.tokens };
	});
}
/** A fast evaluator that scores each proposal id from a table (default: pass 10). */
function scriptedFast(scores: Readonly<Record<string, Screen>>, tokens = 1): EvaluationAdapter<Artifact> {
	return {
		id: "fast",
		kind: "fast",
		evaluate: vi.fn(async ({ proposal }) => ({
			status: "completed" as const,
			value: scores[proposal.id] ?? { status: "pass" as const, score: 10 },
			tokens,
		})),
	};
}

async function base(
	overrides: Partial<RavoControllerOptions<Artifact>> = {},
): Promise<RavoControllerOptions<Artifact>> {
	const dir = await mkdtemp(path.join(tmpdir(), "ravo-best-of-n-"));
	const pass = (kind: "fast" | "deep" | "opponent", tokens = 1): EvaluationAdapter<Artifact> => ({
		id: kind,
		kind,
		...(kind === "opponent" ? { criterionId: "correctness" } : {}),
		evaluate: vi.fn(async () => ({
			status: "completed" as const,
			value: { status: "pass" as const, score: 10 },
			tokens,
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
		implement: scriptedImplement([{ proposal: proposal("p1"), tokens: 1 }]),
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
function record(options: RavoControllerOptions<Artifact>): RavoProgressEvent[] {
	const events: RavoProgressEvent[] = [];
	options.onProgress = (event) => events.push(event);
	return events;
}
function candidatesEvents(events: readonly RavoProgressEvent[]) {
	return events.flatMap((event) => (event.type === "candidates" ? [event] : []));
}
function evaluatedIds(adapter: EvaluationAdapter<Artifact>): string[] {
	return vi.mocked(adapter.evaluate).mock.calls.map(([input]) => input.proposal.id);
}

describe("RAVO controller best-of-n implement", () => {
	let ended: SpanEndRecord[];
	beforeEach(() => {
		ended = [];
		setSpanSink((span) => ended.push(span));
	});
	afterEach(() => {
		installDefaultSpanSink();
	});

	it("fans out n candidates, screens each once and keeps the best fast score", async () => {
		const options = await base({ implementCandidates: 3 });
		options.implement = scriptedImplement([
			{ proposal: proposal("p-a"), tokens: 2 },
			{ proposal: proposal("p-b"), tokens: 3 },
			{ proposal: proposal("p-c"), tokens: 1 },
		]);
		const fast = scriptedFast({
			"p-a": { status: "pass", score: 6 },
			"p-b": { status: "pass", score: 9 },
			"p-c": { status: "fail", score: 10 },
		});
		options.evaluators = [fast, ...options.evaluators.filter((a) => a.kind !== "fast")];
		const events = record(options);
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(result.checkpoint.candidate?.id).toBe("p-b");
		expect(options.implement).toHaveBeenCalledTimes(3);
		// Each candidate is screened exactly once; the winner's screen is reused downstream.
		expect(evaluatedIds(fast).sort()).toEqual(["p-a", "p-b", "p-c"]);
		const deep = options.evaluators.find((a) => a.kind === "deep");
		const opponent = options.evaluators.find((a) => a.kind === "opponent");
		if (!deep || !opponent) throw new Error("missing evaluators");
		expect(evaluatedIds(deep)).toEqual(["p-b"]);
		expect(evaluatedIds(opponent)).toEqual(["p-b"]);
		expect(result.certificate?.screen).toMatchObject({ status: "pass", score: 9 });
		// inspect 1 + plan 1 + implements 6 + screens 3 + deep 1 + opponent 1.
		expect(result.checkpoint.spentTokens).toBe(13);

		expect(candidatesEvents(events)).toEqual([
			{
				type: "candidates",
				round: 1,
				requested: 3,
				considered: 3,
				selected: "p-b",
				candidates: [
					{ proposalId: "p-a", status: "pass", score: 6, tokens: 2 },
					{ proposalId: "p-b", status: "pass", score: 9, tokens: 3 },
					{ proposalId: "p-c", status: "fail", score: 10, tokens: 1 },
				],
			},
		]);
		const types = events.map((e) => e.type);
		expect(types.indexOf("candidates")).toBeLessThan(types.indexOf("proposal"));
		expect(events.find((e) => e.type === "proposal")).toMatchObject({ proposalId: "p-b" });

		const proposalSpan = ended.filter((s) => s.name === "ravo.proposal");
		expect(proposalSpan).toHaveLength(1);
		expect(proposalSpan[0]?.attrs).toMatchObject({
			"ravo.kind": "implement",
			"ravo.proposal_id": "p-b",
			"ravo.candidate_tokens": 3,
			"ravo.candidates_requested": 3,
			"ravo.candidates_considered": 3,
			"ravo.fan_out_tokens": 9,
		});
		const candidateSpans = ended.filter((s) => s.name === "ravo.candidate");
		expect(candidateSpans).toHaveLength(3);
		for (const span of candidateSpans) expect(span.parentSpanId).toBe(proposalSpan[0]?.spanId);
		expect(candidateSpans.map((s) => s.attrs["ravo.verdict"]).sort()).toEqual(["fail", "pass", "pass"]);
		// The fast screens nest under their candidate span, not under the round.
		const screens = ended.filter((s) => s.name === "ravo.evaluation" && s.attrs["ravo.evaluator"] === "fast");
		expect(screens).toHaveLength(3);
		for (const screen of screens) expect(candidateSpans.some((c) => c.spanId === screen.parentSpanId)).toBe(true);
	});

	it("orders pass before fail, then score, then fewer tokens, then index", async () => {
		const cases: { fast: Record<string, Screen>; tokens: [number, number, number]; expected: string }[] = [
			{
				fast: { a: { status: "fail", score: 99 }, b: { status: "pass", score: 1 }, c: { status: "error" } },
				tokens: [1, 1, 1],
				expected: "b",
			},
			{
				fast: { a: { status: "pass", score: 5 }, b: { status: "pass", score: 5 }, c: { status: "pass", score: 5 } },
				tokens: [3, 1, 2],
				expected: "b",
			},
			{
				fast: { a: { status: "pass", score: 5 }, b: { status: "pass", score: 5 }, c: { status: "pass", score: 5 } },
				tokens: [2, 2, 2],
				expected: "a",
			},
			{
				fast: { a: { status: "pass" }, b: { status: "pass", score: 0 }, c: { status: "pass" } },
				tokens: [1, 1, 1],
				expected: "b",
			},
			{
				fast: { a: { status: "fail" }, b: { status: "abstain" }, c: { status: "fail", score: 2 } },
				tokens: [1, 1, 1],
				expected: "c",
			},
		];
		for (const { fast, tokens, expected } of cases) {
			const options = await base({ implementCandidates: 3, maxRepairs: 0, maxRounds: 1 });
			options.implement = scriptedImplement([
				{ proposal: proposal("a"), tokens: tokens[0] },
				{ proposal: proposal("b"), tokens: tokens[1] },
				{ proposal: proposal("c"), tokens: tokens[2] },
			]);
			options.evaluators = [scriptedFast(fast), ...options.evaluators.filter((a) => a.kind !== "fast")];
			const events = record(options);
			await runRavoController(options);
			expect(candidatesEvents(events)[0]?.selected, JSON.stringify(fast)).toBe(expected);
		}
	});

	it("runs the candidates concurrently with a decreasing token ceiling per admission", async () => {
		const options = await base({ implementCandidates: 3, reservationPerCall: 5, tokenBudget: 100 });
		let inFlight = 0;
		let peak = 0;
		const ceilings: number[] = [];
		let index = 0;
		options.implement = vi.fn(async (_input, callOptions): Promise<RavoChildResult<Proposal>> => {
			ceilings.push(callOptions.tokenBudget);
			inFlight += 1;
			peak = Math.max(peak, inFlight);
			await new Promise((resolve) => setTimeout(resolve, 5));
			inFlight -= 1;
			return { status: "completed", value: proposal(`p${index++}`), tokens: 1 };
		});
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(peak).toBe(3);
		// inspect and plan spent 2; each admission sees the previous reservations.
		expect(ceilings).toEqual([98, 93, 88]);
	});

	it("degrades to the admitted candidates when the budget runs out mid-fan-out", async () => {
		// reservationPerCall 3, budget 10: inspect + plan spend 2, then admissions at
		// remaining 8 and 5 pass while the third (remaining 2) is refused.
		const options = await base({ implementCandidates: 3, reservationPerCall: 3, tokenBudget: 10 });
		options.implement = scriptedImplement([
			{ proposal: proposal("p-a"), tokens: 1 },
			{ proposal: proposal("p-b"), tokens: 1 },
			{ proposal: proposal("p-c"), tokens: 1 },
		]);
		const fast = scriptedFast({ "p-a": { status: "pass", score: 4 }, "p-b": { status: "pass", score: 8 } }, 0);
		options.evaluators = [
			fast,
			...options.evaluators
				.filter((a) => a.kind !== "fast")
				.map((a) => ({
					...a,
					evaluate: async () => ({
						status: "completed" as const,
						value: { status: "pass" as const, score: 10 },
						tokens: 0,
					}),
				})),
		];
		const events = record(options);
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(options.implement).toHaveBeenCalledTimes(2);
		expect(evaluatedIds(fast).sort()).toEqual(["p-a", "p-b"]);
		expect(candidatesEvents(events)).toEqual([
			expect.objectContaining({ requested: 3, considered: 2, selected: "p-b" }),
		]);
		expect(result.checkpoint.candidate?.id).toBe("p-b");
		expect(result.checkpoint.spentTokens).toBe(4);
	});

	it("stops on budget when not even the first candidate is admitted", async () => {
		const options = await base({ implementCandidates: 3, reservationPerCall: 3, tokenBudget: 4 });
		const events = record(options);
		const result = await runRavoController(options);
		expect(result.reason).toBe("budget");
		expect(options.implement).not.toHaveBeenCalled();
		expect(candidatesEvents(events)).toEqual([]);
		expect(result.checkpoint.spentTokens).toBe(2);
	});

	it("stops on budget when a candidate child exhausts the budget, after every candidate settled", async () => {
		const options = await base({ implementCandidates: 2 });
		let calls = 0;
		options.implement = vi.fn(async (): Promise<RavoChildResult<Proposal>> => {
			const call = ++calls;
			await new Promise((resolve) => setTimeout(resolve, call === 1 ? 1 : 10));
			return call === 1
				? { status: "budget_exceeded", tokens: 50 }
				: { status: "completed", value: proposal("p-late"), tokens: 3 };
		});
		const result = await runRavoController(options);
		expect(result.reason).toBe("budget");
		expect(options.implement).toHaveBeenCalledTimes(2);
		// Both children were settled before the stop: inspect 1 + plan 1 + 50 + 3,
		// plus the surviving candidate's fast screen (1), which ran within its budget.
		expect(result.checkpoint.spentTokens).toBe(56);
	});

	it("propagates cancellation and child failures from any candidate", async () => {
		const cancel = new AbortController();
		const cancelling = await base({ implementCandidates: 2, signal: cancel.signal });
		cancelling.implement = vi.fn(async (_input, callOptions): Promise<RavoChildResult<Proposal>> => {
			cancel.abort();
			return { status: "aborted", tokens: 0, error: callOptions.signal.aborted ? "aborted" : "not aborted" };
		});
		expect((await runRavoController(cancelling)).reason).toBe("cancelled");

		const failing = await base({ implementCandidates: 2 });
		let calls = 0;
		failing.implement = vi.fn(
			async (): Promise<RavoChildResult<Proposal>> =>
				calls++ === 0
					? { status: "error", tokens: 1, error: "child crashed" }
					: { status: "completed", value: proposal("p-ok"), tokens: 1 },
		);
		await expect(runRavoController(failing)).rejects.toThrow("child crashed");
		expect(failing.implement).toHaveBeenCalledTimes(2);

		const duplicated = await base({ implementCandidates: 2 });
		duplicated.implement = scriptedImplement([{ proposal: proposal("same"), tokens: 1 }]);
		await expect(runRavoController(duplicated)).rejects.toThrow(/distinct proposal ids/);
	});

	it("uses a single candidate for repair rounds", async () => {
		const options = await base({ implementCandidates: 3 });
		options.implement = scriptedImplement([
			{ proposal: proposal("p-a"), tokens: 1 },
			{ proposal: proposal("p-b"), tokens: 1 },
			{ proposal: proposal("p-c"), tokens: 1 },
		]);
		let deepCalls = 0;
		options.evaluators = options.evaluators.map((a) =>
			a.kind === "deep"
				? {
						...a,
						evaluate: async () => ({
							status: "completed" as const,
							value: { status: deepCalls++ === 0 ? ("fail" as const) : ("pass" as const), score: 10 },
							tokens: 1,
						}),
					}
				: a,
		);
		const events = record(options);
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(options.implement).toHaveBeenCalledTimes(3);
		expect(options.repair).toHaveBeenCalledOnce();
		expect(candidatesEvents(events)).toHaveLength(1);
		expect(result.checkpoint.candidate?.repairOf).toBe("p-a");
		// The repair round screens the repair once, in the evaluate phase.
		const fast = options.evaluators.find((a) => a.kind === "fast");
		if (!fast) throw new Error("missing fast");
		expect(evaluatedIds(fast).sort()).toEqual(["p-a", "p-a-repair", "p-b", "p-c"]);
	});

	it("continues a retained worker only on the first candidate and keeps the winner's handle", async () => {
		const options = await base({ implementCandidates: 3 });
		options.checkpoint = {
			runId: "run",
			phase: "inspect",
			round: 0,
			repairs: 0,
			state: emptyRavoState(opponents),
			certificates: [],
			spentTokens: 0,
			errorBudget: options.ledger.toJSON(),
			workerHandle: "retained",
		};
		const handles: (string | undefined)[] = [];
		let index = 0;
		options.implement = vi.fn(async (input): Promise<RavoChildResult<Proposal>> => {
			handles.push(input.workerHandle);
			const id = `p${index++}`;
			return {
				status: "deferred",
				handle: `worker-${id}`,
				wait: async () => ({ status: "completed", value: proposal(id), tokens: 1 }),
			};
		});
		options.evaluators = [
			scriptedFast({
				p0: { status: "pass", score: 1 },
				p1: { status: "pass", score: 9 },
				p2: { status: "pass", score: 5 },
			}),
			...options.evaluators.filter((a) => a.kind !== "fast"),
		];
		const result = await runRavoController(options);
		expect(result.reason).toBe("accepted");
		expect(handles).toHaveLength(3);
		expect(handles.filter((h) => h === "retained")).toHaveLength(1);
		expect(handles.filter((h) => h === undefined)).toHaveLength(2);
		expect(result.checkpoint.candidate?.id).toBe("p1");
		expect(result.checkpoint.workerHandle).toBe("worker-p1");
	});

	it("keeps the n = 1 path identical to the default", async () => {
		const run = async (implementCandidates?: number) => {
			const options = await base(implementCandidates === undefined ? {} : { implementCandidates });
			const events = record(options);
			const spans: string[] = [];
			ended = [];
			const result = await runRavoController(options);
			for (const span of ended) spans.push(`${span.name}:${JSON.stringify(span.attrs)}`);
			return { result: JSON.parse(JSON.stringify(result)) as unknown, events, spans };
		};
		const baseline = await run();
		const explicit = await run(1);
		expect(explicit.result).toEqual(baseline.result);
		expect(explicit.events).toEqual(baseline.events);
		expect(explicit.spans).toEqual(baseline.spans);
		expect(candidatesEvents(baseline.events)).toEqual([]);
		expect(baseline.spans.some((s) => s.startsWith("ravo.candidate:"))).toBe(false);
		expect(baseline.spans.some((s) => s.includes("candidates_requested"))).toBe(false);
	});

	it("rejects an invalid implementCandidates option", async () => {
		for (const bad of [0, 9, 1.5, Number.NaN]) {
			await expect(runRavoController(await base({ implementCandidates: bad }))).rejects.toThrow(
				"implementCandidates is invalid",
			);
		}
	});
});
