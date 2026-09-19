import { execFileSync } from "node:child_process";
import { addSpanSink, type SpanEndRecord } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { FailureRecord } from "../src/core/ravo/failure-ledger.js";
import type { ReplayCase } from "../src/core/ravo/referee.js";
import {
	type HarnessTrustWindow,
	type HarnessTrustWindows,
	MAX_TRUST_ADJUDICATION_RUNS,
} from "../src/core/refinement/harness-trust.js";
import type { HarnessEntry, HarnessState, RefinementKind } from "../src/core/refinement/refinement.js";
import {
	adjudicateTrustRecurrences,
	findTrustWindowRecurrences,
	MAX_TRUST_ADJUDICATION_JOBS,
	planTrustAdjudications,
	releaseAwaitingTrustAdjudication,
	type TrustAdjudicationJob,
	type TrustRecurrence,
	trustAdjudicationKey,
} from "../src/core/refinement/trust-adjudication.js";

const FP = "fp_missing_module";
const OTHER_FP = "fp_other";
const VERIFIED_AT = "2026-09-16T08:00:00.000Z";

function probe(source: string, verified = true): ReplayCase {
	return {
		language: "python",
		source,
		exceptionClass: "ModuleNotFoundError",
		...(verified ? { verifiedAt: VERIFIED_AT } : {}),
	};
}

function failureRecord(cases: ReplayCase[], fingerprintId = FP): FailureRecord {
	return {
		fingerprint: {
			id: fingerprintId,
			kind: "python_exception",
			source: "ipython",
			exceptionClass: "ModuleNotFoundError",
			message: "no module named ?",
		},
		count: 3,
		firstSeenTurn: 1,
		lastSeenTurn: 3,
		firstSeenAt: VERIFIED_AT,
		lastSeenAt: VERIFIED_AT,
		excerpt: "ModuleNotFoundError: No module named 'pkg'",
		addressedByProposalIds: [],
		replayCases: cases,
	};
}

function entry(kind: RefinementKind, id: string, reference: Record<string, unknown> = {}): HarnessEntry {
	return {
		id,
		kind,
		title: id,
		content: id,
		path: "general",
		scope: "local",
		reference,
		arguments: {},
		metadata: {},
		source: "refine",
		created_at: VERIFIED_AT,
		updated_at: VERIFIED_AT,
		version: 1,
	};
}

function entries(skills: Record<string, string | undefined> = { s: "pkg.mod" }): HarnessState["entries"] {
	const state: HarnessState["entries"] = { prompt: {}, memory: {}, skill: {}, subagent: {} };
	for (const [id, module] of Object.entries(skills)) {
		if (module === undefined) continue;
		state.skill[id] = entry("skill", id, { type: "python", import: module, callable: "run" });
	}
	state.memory.m = entry("memory", "m");
	state.prompt.p = entry("prompt", "p");
	state.subagent.a = entry("subagent", "a");
	return state;
}

function window(proposalId: string, overrides: Partial<HarnessTrustWindow> = {}): HarnessTrustWindow {
	return {
		proposalId,
		touched: ["skill:s"],
		claimedFingerprints: [FP],
		committedTurn: 10,
		untilTurn: 30,
		outcome: "open",
		skillImports: { "skill:s": ["pkg.mod"] },
		...overrides,
	};
}

function windowsOf(...list: HarnessTrustWindow[]): HarnessTrustWindows {
	return Object.fromEntries(list.map((item) => [item.proposalId, item]));
}

function recurrenceOf(
	proposalId: string,
	ordinal: number,
	observedCases = [probe("import pkg", false)],
): TrustRecurrence {
	return { proposalId, fingerprintId: FP, ordinal, observedCases };
}

function plan(
	windows: HarnessTrustWindows,
	recurrences: TrustRecurrence[],
	options: { record?: FailureRecord; skills?: Record<string, string | undefined> } = {},
) {
	const record = options.record ?? failureRecord([probe("import pkg")]);
	return planTrustAdjudications({
		scope: "local",
		windows,
		recurrences,
		entries: entries(options.skills),
		recordOf: (id) => (id === record.fingerprint.id ? record : undefined),
		triggerTraceId: "trace-1",
	});
}

function job(overrides: Partial<TrustAdjudicationJob> = {}): TrustAdjudicationJob {
	return {
		scope: "local",
		proposalId: "refine_a",
		entry: "skill:s",
		fingerprintId: FP,
		ordinal: 12,
		skillImports: ["prime_agent_trust_absent_mod"],
		record: failureRecord([probe("import prime_agent_trust_absent_mod")]),
		triggerTraceId: "trace-1",
		...overrides,
	};
}

function hasPython3(): boolean {
	try {
		execFileSync("python3", ["-c", "pass"], { stdio: "ignore" });
		return true;
	} catch {
		return false;
	}
}

const PYTHON3 = hasPython3();

describe("trust window recurrences", () => {
	it("finds recurrences only for open windows that claimed the fingerprint and whose range holds the ordinal", () => {
		const windows = windowsOf(
			window("refine_b", { claimedFingerprints: [OTHER_FP, FP] }),
			window("refine_a"),
			window("refine_clean", { outcome: "clean" }),
			window("refine_contested", { outcome: "contested" }),
			window("refine_faulted", { outcome: "faulted" }),
			window("refine_unmeasured", { outcome: "unmeasured" }),
			window("refine_unclaimed", { claimedFingerprints: ["fp_never"] }),
			window("refine_late", { committedTurn: 13, untilTurn: 33 }),
			window("refine_early", { committedTurn: 0, untilTurn: 11 }),
		);
		const recurred = new Map<string, ReplayCase[]>([
			[FP, [probe("import pkg", false), probe("import pkg", false), probe("from pkg import x", false)]],
			[OTHER_FP, []],
		]);

		const found = findTrustWindowRecurrences(windows, recurred, 12);

		expect(found.map((item) => [item.proposalId, item.fingerprintId])).toEqual([
			["refine_a", FP],
			["refine_b", FP],
			["refine_b", OTHER_FP],
		]);
		expect(found[0].observedCases).toEqual([probe("import pkg", false)]);
		expect(found[0].ordinal).toBe(12);
		expect(found[2].observedCases).toEqual([]);
		expect(findTrustWindowRecurrences(undefined, recurred, 12)).toEqual([]);
	});
});

describe("planning post-commit replays", () => {
	it("plans a replay only for a skill whose current imports are the ones its commit recorded and that the recurrence's own case probes", () => {
		const planned = plan(windowsOf(window("refine_a")), [recurrenceOf("refine_a", 12)]);
		expect(planned.awaiting).toEqual([]);
		expect(planned.jobs).toHaveLength(1);
		expect(planned.jobs[0]).toMatchObject({
			scope: "local",
			proposalId: "refine_a",
			entry: "skill:s",
			fingerprintId: FP,
			skillImports: ["pkg.mod"],
			ordinal: 12,
			triggerTraceId: "trace-1",
		});
		expect(trustAdjudicationKey(planned.jobs[0])).toBe(`local refine_a skill:s ${FP}`);

		const nothing = (windows: HarnessTrustWindows, skills?: Record<string, string | undefined>) => {
			const result = plan(windows, [recurrenceOf("refine_a", 12)], { skills });
			expect(result).toEqual({ jobs: [], awaiting: [] });
		};
		nothing(
			windowsOf(
				window("refine_a", {
					touched: ["memory:m", "prompt:p", "subagent:a"],
					skillImports: { "memory:m": ["pkg.mod"] } as Record<string, string[]>,
				}),
			),
		);
		nothing(windowsOf(window("refine_a", { skillImports: undefined })));
		nothing(windowsOf(window("refine_a")), { s: "pkg.other" });
		nothing(windowsOf(window("refine_a")), {});
		const runs = (count: number) => Array.from({ length: count }, (_, index) => `2026-09-16T10:00:0${index}.000Z`);
		nothing(
			windowsOf(
				window("refine_a", {
					adjudications: [{ entry: "skill:s", fingerprintId: FP, status: "upheld", ordinal: 11, runs: runs(1) }],
				}),
			),
		);
		nothing(
			windowsOf(
				window("refine_a", {
					adjudications: [
						{
							entry: "skill:s",
							fingerprintId: FP,
							status: "cleared",
							ordinal: 11,
							runs: runs(MAX_TRUST_ADJUDICATION_RUNS),
						},
					],
				}),
			),
		);
		nothing(windowsOf(window("refine_a", { outcome: "faulted" })));

		// Fewer runs than the cap on another fingerprint or entry do not block this one.
		const partial = plan(
			windowsOf(
				window("refine_a", {
					adjudications: [{ entry: "skill:s", fingerprintId: FP, status: "cleared", ordinal: 11, runs: runs(2) }],
				}),
			),
			[recurrenceOf("refine_a", 12)],
		);
		expect(partial.jobs).toHaveLength(1);
	});

	it("plans nothing for another module that fails under the same fingerprint", () => {
		const result = plan(
			windowsOf(window("refine_a")),
			[recurrenceOf("refine_a", 12, [probe("import other", false)])],
			{
				record: failureRecord([probe("import pkg"), probe("import other", false)]),
			},
		);
		expect(result).toEqual({ jobs: [], awaiting: [] });
		expect(
			plan(windowsOf(window("refine_a")), [recurrenceOf("refine_a", 12, [probe("from pkg import mod", false)])], {
				record: failureRecord([probe("from pkg import mod")]),
			}),
		).toEqual({ jobs: [], awaiting: [] });
		expect(plan(windowsOf(window("refine_a")), [recurrenceOf("refine_a", 12, [])])).toEqual({
			jobs: [],
			awaiting: [],
		});
	});

	it("gives an overlapping replay fact to the newest window that wrote the entry's imports", () => {
		const older = window("refine_w1", { committedTurn: 10, untilTurn: 30 });
		const newer = window("refine_w2", { committedTurn: 13, untilTurn: 33 });
		const windows = windowsOf(older, newer);

		const at15 = plan(windows, [recurrenceOf("refine_w1", 15), recurrenceOf("refine_w2", 15)]);
		expect(at15.jobs.map((item) => item.proposalId)).toEqual(["refine_w2"]);
		const at12 = plan(windows, [recurrenceOf("refine_w1", 12)]);
		expect(at12.jobs.map((item) => item.proposalId)).toEqual(["refine_w1"]);

		// A newer window that rewrote the import does not supersede the older one's claim on its own import.
		const rewrote = windowsOf(older, { ...newer, skillImports: { "skill:s": ["pkg.other"] } });
		expect(plan(rewrote, [recurrenceOf("refine_w1", 15)]).jobs.map((item) => item.proposalId)).toEqual(["refine_w1"]);

		const faulted = windowsOf(older, { ...newer, outcome: "faulted" });
		expect(plan(faulted, [recurrenceOf("refine_w1", 15)])).toEqual({ jobs: [], awaiting: [] });
	});

	it("defers a job until the self-check verifies the observed case", () => {
		const record = failureRecord([probe("import pkg", false)]);
		const result = plan(windowsOf(window("refine_a")), [recurrenceOf("refine_a", 12)], { record });
		expect(result.jobs).toEqual([]);
		expect(result.awaiting).toHaveLength(1);
		const [awaiting] = result.awaiting;
		expect(awaiting.sources).toEqual(["import pkg"]);

		expect(
			releaseAwaitingTrustAdjudication(awaiting, [
				{ fingerprintId: FP, source: "import other", verifiedAt: VERIFIED_AT },
				{ fingerprintId: OTHER_FP, source: "import pkg", verifiedAt: VERIFIED_AT },
			]),
		).toEqual({ matched: false });

		const released = releaseAwaitingTrustAdjudication(awaiting, [
			{ fingerprintId: FP, source: "import pkg", verifiedAt: VERIFIED_AT },
		]);
		expect(released.matched).toBe(true);
		expect(released.job).toMatchObject({ proposalId: "refine_a", entry: "skill:s", ordinal: 12 });
		expect(released.job?.record.replayCases).toEqual([probe("import pkg")]);
		// The awaiting record itself is untouched.
		expect(awaiting.job.record.replayCases).toEqual([probe("import pkg", false)]);
	});

	it("caps jobs and awaiting at MAX_TRUST_ADJUDICATION_JOBS", () => {
		const count = MAX_TRUST_ADJUDICATION_JOBS + 3;
		const ids = Array.from({ length: count }, (_, index) => `s${String(index).padStart(2, "0")}`);
		const touched = ids.map((id) => `skill:${id}`);
		const skillImports = Object.fromEntries(touched.map((ref) => [ref, ["pkg.mod"]]));
		const skills = Object.fromEntries(ids.map((id) => [id, "pkg.mod"]));
		const windows = windowsOf(window("refine_a", { touched, skillImports }));

		const verified = plan(windows, [recurrenceOf("refine_a", 12)], { skills });
		expect(verified.jobs).toHaveLength(MAX_TRUST_ADJUDICATION_JOBS);
		expect(verified.awaiting).toEqual([]);

		const unverified = plan(windows, [recurrenceOf("refine_a", 12)], {
			skills,
			record: failureRecord([probe("import pkg", false)]),
		});
		expect(unverified.jobs).toEqual([]);
		expect(unverified.awaiting).toHaveLength(MAX_TRUST_ADJUDICATION_JOBS);
	});
});

describe.skipIf(!PYTHON3)("adjudicating post-commit replays", () => {
	let spans: SpanEndRecord[];
	let removeSink: () => void;

	beforeEach(() => {
		spans = [];
		removeSink = addSpanSink((span) => spans.push(span));
	});

	afterEach(() => {
		removeSink();
	});

	const named = (name: string) => spans.filter((span) => span.name === name);

	it("adjudicates a job in a subprocess and returns upheld evidence under a harness.trust.adjudicate root", async () => {
		const evidence = await adjudicateTrustRecurrences([job()], {
			pythonPath: "python3",
			sessionId: "session-1",
			now: () => "2026-09-16T12:00:00.000Z",
		});

		expect(evidence).toEqual([
			{
				type: "adjudication",
				scope: "local",
				proposalId: "refine_a",
				entry: "skill:s",
				fingerprintId: FP,
				status: "upheld",
				ordinal: 12,
				at: "2026-09-16T12:00:00.000Z",
			},
		]);
		const [adjudicate] = named("harness.trust.adjudicate");
		expect(named("harness.trust.adjudicate")).toHaveLength(1);
		expect(adjudicate).toMatchObject({
			status: "ok",
			parentSpanId: undefined,
			attrs: {
				"session.id": "session-1",
				"trigger.trace_id": "trace-1",
				"trust.jobs": 1,
				"trust.windows": 1,
				"trust.ran": 1,
				"trust.upheld": 1,
				"trust.cleared": 0,
				"trust.unverifiable": 0,
				"trust.skipped": 0,
				"trust.aborted": false,
			},
		});
		const [referee] = named("ravo.referee");
		expect(referee.parentSpanId).toBe(adjudicate.spanId);
		expect(referee.attrs).toMatchObject({
			"referee.adjudicated": 1,
			"referee.upheld": 1,
			"referee.unverifiable": 0,
			"referee.aborted": false,
		});
		expect(named("ravo.replay_case")[0]).toMatchObject({
			parentSpanId: referee.spanId,
			attrs: { "referee.environment": "skill-import" },
		});
	});

	it("returns cleared for a probe that imports, and nothing for a batch aborted before or during its run", async () => {
		const clean = job({ skillImports: ["json"], record: failureRecord([probe("import json")]) });
		const cleared = await adjudicateTrustRecurrences([clean, { ...clean, triggerTraceId: "trace-2" }], {
			pythonPath: "python3",
		});
		expect(cleared.map((item) => item.status)).toEqual(["cleared", "cleared"]);
		expect(named("harness.trust.adjudicate")[0]?.attrs).toMatchObject({ "trust.cleared": 2, "trust.windows": 1 });
		expect(named("harness.trust.adjudicate")[0]?.attrs).not.toHaveProperty(["trigger.trace_id"]);

		spans.length = 0;
		const before = new AbortController();
		before.abort();
		expect(await adjudicateTrustRecurrences([job()], { pythonPath: "python3", signal: before.signal })).toEqual([]);
		expect(named("harness.trust.adjudicate")[0]?.attrs).toMatchObject({ "trust.ran": 0, "trust.aborted": true });
		expect(named("ravo.replay_case")).toEqual([]);

		spans.length = 0;
		const during = new AbortController();
		const running = adjudicateTrustRecurrences([job(), job({ proposalId: "refine_b" })], {
			pythonPath: "python3",
			signal: during.signal,
		});
		during.abort();
		expect(await running).toEqual([]);
		expect(named("harness.trust.adjudicate")[0]?.attrs).toMatchObject({
			"trust.jobs": 2,
			"trust.windows": 2,
			"trust.ran": 0,
			"trust.upheld": 0,
			"trust.aborted": true,
		});
		// The replay the abort cut short is not counted as a measurement on its referee span either.
		expect(named("ravo.referee")).toEqual([
			expect.objectContaining({
				attrs: expect.objectContaining({
					"referee.claimed": 1,
					"referee.adjudicated": 0,
					"referee.unverifiable": 0,
					"referee.upheld": 0,
					"referee.aborted": true,
				}),
			}),
		]);

		expect(await adjudicateTrustRecurrences([], { pythonPath: "python3" })).toEqual([]);
	});
});
