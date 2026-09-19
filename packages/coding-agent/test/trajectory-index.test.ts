import { describe, expect, it } from "vitest";
import {
	DEFAULT_MIN_TRAJECTORY_WINDOWS,
	isoWeek,
	isTrajectoryIndexEnabled,
	readTrajectoryIndex,
	sealTrajectoryWindows,
	type TrajectoryCorpus,
	trajectoryClassForEntries,
	trajectoryInternalizedFingerprints,
} from "../src/core/distill/trajectory-index.js";
import type { FingerprintDayStats, LearningDay, RefinementCommit } from "../src/core/learning-index.js";
import type { HarnessState } from "../src/core/refinement/refinement.js";

// A fixed "now" well past every fixture week, so the current ISO week (2025-W11) is
// never one of the sealed windows below.
const NOW_MS = Date.UTC(2025, 2, 10); // 2025-03-10, Monday, ISO 2025-W11

/** A calendar day (Monday of each ISO week used below) and the ISO week it seals into. */
const D = {
	w02: "2025-01-06", // 2025-W02
	w03: "2025-01-13", // 2025-W03
	w04: "2025-01-20", // 2025-W04
	w05: "2025-01-27", // 2025-W05
	w06: "2025-02-03", // 2025-W06
	w07: "2025-02-10", // 2025-W07
} as const;

interface FpSpec {
	fingerprint: string;
	count: number;
	failure?: boolean;
	name?: string;
	message?: string;
	status?: string;
}

function fp(spec: FpSpec): FingerprintDayStats {
	return {
		fingerprint: spec.fingerprint,
		name: spec.name ?? spec.fingerprint,
		status: spec.status ?? (spec.failure === false ? "ok" : "error"),
		failure: spec.failure ?? true,
		count: spec.count,
		p50Ms: 0,
		p95Ms: 0,
		message: spec.message ?? "",
	};
}

function day(dayStr: string, turns: number, fingerprints: FpSpec[], commits: RefinementCommit[] = []): LearningDay {
	return {
		schema: 1,
		day: dayStr,
		sealedAt: "",
		turns,
		fingerprints: fingerprints.map(fp),
		commits,
		parseErrors: 0,
		sourceFiles: [],
	};
}

function labelFor(
	file: ReturnType<typeof sealTrajectoryWindows>,
	fingerprint: string,
	corpus: TrajectoryCorpus = "prime",
) {
	return file.labels.find((label) => label.fingerprint === fingerprint && label.corpus === corpus);
}

describe("isoWeek (ISO-8601 week-year, UTC, Monday start)", () => {
	it("uses the ISO week-year across a year boundary", () => {
		expect(isoWeek("2025-01-06")).toBe("2025-W02");
		expect(isoWeek("2021-01-01")).toBe("2020-W53"); // Friday belongs to the prior week-year
		expect(isoWeek("2020-12-28")).toBe("2020-W53");
		expect(isoWeek("2019-12-30")).toBe("2020-W01"); // Monday belongs to the next week-year
	});
});

describe("sealTrajectoryWindows windowing", () => {
	it("buckets days into ISO weeks, leaves the current week open, and runs the ordinal", () => {
		const days = [
			day(D.w02, 5, [fp({ fingerprint: "fpA", count: 1 })]),
			day(D.w03, 5, [fp({ fingerprint: "fpA", count: 2 })]),
			day(D.w04, 5, [fp({ fingerprint: "fpA", count: 1 })]),
			day(D.w05, 5, [fp({ fingerprint: "fpA", count: 3 })]),
			day(D.w06, 5, [fp({ fingerprint: "fpA", count: 1 })]),
			// A day inside the current ISO week must be left open (never sealed).
			day("2025-03-10", 9, [fp({ fingerprint: "fpA", count: 99 })]),
		].map((d) => d as unknown as LearningDay);
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(file.windows.map((w) => w.window)).toEqual(["2025-W02", "2025-W03", "2025-W04", "2025-W05", "2025-W06"]);
		expect(file.windows.some((w) => w.window === "2025-W11")).toBe(false);
		expect(file.windowsObserved).toBe(5);
		// cumulativeOrdinal == running total of all failure counts through the window.
		expect(file.windows[0]!.fingerprints[0]!.cumulativeOrdinal).toBe(1);
		expect(file.windows[4]!.fingerprints[0]!.cumulativeOrdinal).toBe(8);
		expect(file.windows[3]!.fingerprints[0]!.count).toBe(3);
	});
});

describe("labels", () => {
	function fiveWindowBackbone(): FpSpec[][] {
		// fpStay is present in every window so K stays 5 and the windows exist.
		return [[], [], [], [], []];
	}

	it("labels a fingerprint that first appears only in the newest window NEW", () => {
		const stay = { fingerprint: "fpStay", count: 2 };
		const days = [
			day(D.w02, 5, [stay]),
			day(D.w03, 5, [stay]),
			day(D.w04, 5, [stay]),
			day(D.w05, 5, [stay]),
			day(D.w06, 5, [stay, { fingerprint: "fpNew", count: 1 }]),
		];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "fpNew")?.label).toBe("new");
	});

	it("labels a recurring-then-absent fingerprint DROPPED, but refuses it when the domain went inactive", () => {
		const stay = { fingerprint: "fpStay", count: 2 };
		const drop = (count: number) => ({ fingerprint: "fpDrop", count });
		const active = [
			day(D.w02, 5, [stay, drop(2)]),
			day(D.w03, 5, [stay, drop(2)]),
			day(D.w04, 5, [stay]),
			day(D.w05, 5, [stay]),
			day(D.w06, 5, [stay]),
		];
		const activeFile = sealTrajectoryWindows({ days: active, nowMs: NOW_MS });
		expect(labelFor(activeFile, "fpDrop")?.label).toBe("dropped");
		expect(labelFor(activeFile, "fpDrop")?.confounds).toContain("task-mix");

		// Same shape but the last M windows have zero turns: absence is task-mix, not learning.
		const inactive = [
			day(D.w02, 5, [stay, drop(2)]),
			day(D.w03, 5, [stay, drop(2)]),
			day(D.w04, 5, [stay]),
			day(D.w05, 0, [stay]),
			day(D.w06, 0, [stay]),
		];
		const inactiveFile = sealTrajectoryWindows({ days: inactive, nowMs: NOW_MS });
		expect(labelFor(inactiveFile, "fpDrop")?.label).toBeNull();
		expect(labelFor(inactiveFile, "fpDrop")?.withheld).toContain("domain inactive");
	});

	it("does not label DROPPED when a committed refinement claimed the fingerprint", () => {
		const stay = { fingerprint: "fpStay", count: 2 };
		const drop = (count: number) => ({ fingerprint: "fpDrop", count });
		const days = [
			day(D.w02, 5, [stay, drop(2)], [{ at: "2025-01-08T00:00:00Z", proposalId: "p1", addressed: ["fpDrop"] }]),
			day(D.w03, 5, [stay, drop(2)]),
			day(D.w04, 5, [stay]),
			day(D.w05, 5, [stay]),
			day(D.w06, 5, [stay]),
		];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "fpDrop")?.label).not.toBe("dropped");
		expect(labelFor(file, "fpDrop")?.claimedByRefinement).toBe(true);
	});

	it("labels a fingerprint recurring across a strict majority of windows PERSISTS and flags a security class", () => {
		const stay = { fingerprint: "fpStay", count: 2 };
		const sec = { fingerprint: "fpSec", count: 2, name: "authentication error", message: "invalid token" };
		const days = [
			day(D.w02, 5, [stay, sec]),
			day(D.w03, 5, [stay, sec]),
			day(D.w04, 5, [stay, sec]),
			day(D.w05, 5, [stay, sec]),
			day(D.w06, 5, [stay, sec]),
		];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "fpStay")?.label).toBe("persists");
		expect(labelFor(file, "fpSec")?.label).toBe("persists");
		expect(labelFor(file, "fpSec")?.securityClass).toBe(true);
		expect(labelFor(file, "fpStay")?.securityClass).toBe(false);
		expect(labelFor(file, "fpStay")?.confounds).toEqual(["task-mix"]);
		void fiveWindowBackbone;
	});

	it("withholds every label below the minimum window count, and emits once the floor is cleared", () => {
		const a = { fingerprint: "fpA", count: 2 };
		const threeWeeks = [day(D.w02, 5, [a]), day(D.w03, 5, [a]), day(D.w04, 5, [a])];
		const withheldFile = sealTrajectoryWindows({ days: threeWeeks, nowMs: NOW_MS });
		expect(withheldFile.windowsObserved).toBe(3);
		expect(withheldFile.labels.every((label) => label.label === null)).toBe(true);
		expect(labelFor(withheldFile, "fpA")?.withheld).toBe(
			`fewer than ${DEFAULT_MIN_TRAJECTORY_WINDOWS} observed windows (3)`,
		);

		const fourWeeks = [...threeWeeks, day(D.w05, 5, [a])];
		const emitted = sealTrajectoryWindows({ days: fourWeeks, nowMs: NOW_MS });
		expect(emitted.windowsObserved).toBe(4);
		expect(labelFor(emitted, "fpA")?.label).toBe("persists");
	});

	it("never produces a label for a non-failure fingerprint", () => {
		const days = [
			day(D.w02, 5, [
				{ fingerprint: "ok", count: 5, failure: false },
				{ fingerprint: "bad", count: 2 },
			]),
			day(D.w03, 5, [
				{ fingerprint: "ok", count: 5, failure: false },
				{ fingerprint: "bad", count: 2 },
			]),
			day(D.w04, 5, [{ fingerprint: "bad", count: 2 }]),
			day(D.w05, 5, [{ fingerprint: "bad", count: 2 }]),
		];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "ok")).toBeUndefined();
		expect(labelFor(file, "bad")).toBeDefined();
	});
});

describe("confounds", () => {
	it("always seeds task-mix and flags all three when a backfill corpus is mixed in", () => {
		const stay = { fingerprint: "fpStay", count: 2 };
		const days = [day(D.w02, 5, [stay]), day(D.w03, 5, [stay]), day(D.w04, 5, [stay]), day(D.w05, 5, [stay])];
		const primeOnly = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(primeOnly, "fpStay")?.confounds).toEqual(["task-mix"]);

		const backfillDays = [
			{ corpus: "backfill:opencode" as const, day: day(D.w02, 3, [{ fingerprint: "bf", count: 2 }]) },
			{ corpus: "backfill:opencode" as const, day: day(D.w03, 3, [{ fingerprint: "bf", count: 2 }]) },
		];
		const mixed = sealTrajectoryWindows({ days, backfillDays, nowMs: NOW_MS });
		const backfillLabel = labelFor(mixed, "bf", "backfill:opencode");
		expect(backfillLabel?.confounds).toEqual(["task-mix", "tool-surface", "measurement-instrument"]);
		// The prime label now sits in a mixed index, so it too carries all three.
		expect(labelFor(mixed, "fpStay", "prime")?.confounds).toEqual([
			"task-mix",
			"tool-surface",
			"measurement-instrument",
		]);
		expect(mixed.windows.some((w) => w.corpus.startsWith("backfill:"))).toBe(true);
	});

	it("flags measurement-instrument when a fingerprint's span crosses an observed-week gap", () => {
		const g = { fingerprint: "fpG", count: 2 };
		// W02, W03, then a gap, then W06, W07 — fpG spans the gap.
		const days = [day(D.w02, 5, [g]), day(D.w03, 5, [g]), day(D.w06, 5, [g]), day(D.w07, 5, [g])];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "fpG")?.confounds).toContain("measurement-instrument");
		expect(labelFor(file, "fpG")?.confounds).not.toContain("tool-surface");
		// The gap-adjacent rate step is null (uncountable across a gap).
		const w06 = file.rate.find((step) => step.window === "2025-W06");
		expect(w06?.retired).toBeNull();
		expect(w06?.newMinusRetired).toBeNull();
	});
});

describe("rate of change", () => {
	it("computes appeared - retired per window on a hand-checked fixture", () => {
		const days = [
			day(D.w02, 5, [
				{ fingerprint: "fpX", count: 1 },
				{ fingerprint: "fpZ", count: 1 },
			]),
			day(D.w03, 5, [
				{ fingerprint: "fpX", count: 1 },
				{ fingerprint: "fpY", count: 1 },
			]),
			day(D.w04, 5, [{ fingerprint: "fpY", count: 1 }]),
			day(D.w05, 5, [{ fingerprint: "fpY", count: 1 }]),
		];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(file.rate).toEqual([
			{ window: "2025-W02", appeared: 2, retired: 0, newMinusRetired: 2 },
			{ window: "2025-W03", appeared: 1, retired: 1, newMinusRetired: 0 },
			{ window: "2025-W04", appeared: 0, retired: 1, newMinusRetired: -1 },
			{ window: "2025-W05", appeared: 0, retired: 0, newMinusRetired: 0 },
		]);
	});
});

describe("security exemption", () => {
	it("includes a plain DROPPED fingerprint but excludes a security-class one", () => {
		const stay = { fingerprint: "fpStay", count: 2 };
		const plain = (count: number) => ({ fingerprint: "fpPlain", count });
		const sec = (count: number) => ({
			fingerprint: "fpSecDrop",
			count,
			name: "kernel.cell",
			message: "token expired for the api",
		});
		const days = [
			day(D.w02, 5, [stay, plain(2), sec(2)]),
			day(D.w03, 5, [stay, plain(2), sec(2)]),
			day(D.w04, 5, [stay]),
			day(D.w05, 5, [stay]),
			day(D.w06, 5, [stay]),
		];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "fpPlain")?.label).toBe("dropped");
		expect(labelFor(file, "fpSecDrop")?.label).toBe("dropped");
		expect(labelFor(file, "fpSecDrop")?.securityClass).toBe(true);
		const internalized = trajectoryInternalizedFingerprints(file);
		expect(internalized.has("fpPlain")).toBe(true);
		expect(internalized.has("fpSecDrop")).toBe(false);
	});
});

describe("entry join", () => {
	it("maps a PERSISTS fingerprint to a harness entry via a trust window", () => {
		const stay = { fingerprint: "fpP", count: 2 };
		const days = [day(D.w02, 5, [stay]), day(D.w03, 5, [stay]), day(D.w04, 5, [stay]), day(D.w05, 5, [stay])];
		const file = sealTrajectoryWindows({ days, nowMs: NOW_MS });
		expect(labelFor(file, "fpP")?.label).toBe("persists");

		const state = {
			schema: 1,
			entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
			refinements: [],
			trustWindows: {
				prop1: {
					proposalId: "prop1",
					touched: ["skill:my-skill"],
					claimedFingerprints: ["fpP"],
					committedTurn: 0,
					untilTurn: 10,
					outcome: "open",
				},
			},
		} as unknown as HarnessState;
		const classOf = trajectoryClassForEntries(file, state);
		expect(classOf.get("my-skill")).toBe("stable-gap");
	});
});

describe("kill switch and degraded reads", () => {
	it("is disabled only for 0/off/false/no", () => {
		for (const value of ["0", "off", "false", "no", "OFF", "  No "]) {
			expect(isTrajectoryIndexEnabled({ PRIME_AGENT_TRAJECTORY_INDEX: value })).toBe(false);
		}
		for (const value of ["1", "on", "true", "yes", "", undefined]) {
			expect(isTrajectoryIndexEnabled({ PRIME_AGENT_TRAJECTORY_INDEX: value })).toBe(true);
		}
	});

	it("returns undefined when the store is absent", () => {
		expect(readTrajectoryIndex("/nonexistent-agent-dir-for-eti-test")).toBeUndefined();
	});
});
