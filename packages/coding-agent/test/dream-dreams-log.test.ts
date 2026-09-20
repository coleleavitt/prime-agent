import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
	type DreamCandidateLine,
	type DreamProbationLine,
	type DreamStepLine,
	DreamsLog,
	dreamsDir,
	dreamsPath,
	isDreamsLogLine,
	readDreamsLog,
} from "../src/core/dream/dreams.js";
import { DEFAULT_POLICY, policyId } from "../src/core/dream/policy.js";
import type { CandidateVerdict, DreamProbationRecord, LeverScanRecord } from "../src/core/dream/types.js";

/**
 * The dreams log: `<dir>/dreams/<runKey>.jsonl`, one candidate line per verdict
 * per step plus a step line, appended in order, parsed back by `readDreamsLog`.
 * Pure file I/O against a scratch directory; no rng, no tree, no clock but the
 * injected one.
 */

const scratchDirs: string[] = [];

function scratch(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-dreams-log-"));
	scratchDirs.push(dir);
	return dir;
}

afterEach(() => {
	for (const dir of scratchDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

const OTHER = { ...DEFAULT_POLICY, batchSize: 2 };

function verdict(over: Partial<CandidateVerdict> = {}): CandidateVerdict {
	return {
		index: 0,
		policyId: policyId(OTHER),
		policy: OTHER,
		origin: "local",
		changed: ["batchSize"],
		duplicateOf: null,
		value: 0.5,
		quality: 1,
		anytime: 0.9,
		cost: 0.25,
		roundsSaved: 0.5,
		N: 3,
		rounds: 2,
		outOfSupportCells: 0,
		inSupportMean: 1,
		inSupportMin: 1,
		chargedProbes: 3,
		chargedRounds: 2,
		evidenceTrees: 0,
		eligible: true,
		reason: "winner",
		...over,
	};
}

const LEVER: LeverScanRecord = {
	policies: 10,
	eligible: 4,
	bestValue: 0.6,
	bestPolicyId: policyId(OTHER),
	gap: 0.1,
	simulations: 10,
};

describe("dreams log paths", () => {
	it("lives beside trees/, keyed by the run key", () => {
		expect(dreamsDir("/store")).toBe(join("/store", "dreams"));
		expect(dreamsPath("/store", "task-s7-r1")).toBe(join("/store", "dreams", "task-s7-r1.jsonl"));
	});
});

describe("DreamsLog.recordStep", () => {
	it("creates the file on the first step and appends candidate lines then the step line, with the injected clock", () => {
		const dir = scratch();
		const path = dreamsPath(dir, "run-a");
		let now = 100;
		const log = new DreamsLog(path, () => now++, { experimentId: "exp", arm: "dream" });
		expect(existsSync(path)).toBe(false);
		log.recordStep({
			iteration: 1,
			poolSize: 1,
			selection: {
				candidates: [
					verdict(),
					verdict({
						index: 1,
						policyId: policyId(DEFAULT_POLICY),
						policy: DEFAULT_POLICY,
						changed: [],
						eligible: false,
						reason: "identical",
						value: 0.4,
					}),
				],
				currentScore: 0.4,
				chosenPolicy: OTHER,
				improved: true,
				dreamer: "local",
				measuredTrees: 1,
			},
			leverScan: LEVER,
		});
		expect(existsSync(path)).toBe(true);
		const lines = readDreamsLog(path);
		expect(lines).toHaveLength(3);
		expect(lines.map((line) => line.type)).toEqual(["candidate", "candidate", "step"]);
		expect(lines.map((line) => line.ts)).toEqual([100, 101, 102]);
		const [first, second, step] = lines as [DreamCandidateLine, DreamCandidateLine, DreamStepLine];
		expect(first).toMatchObject({ type: "candidate", iteration: 1, experimentId: "exp", arm: "dream", ...verdict() });
		expect(second.reason).toBe("identical");
		expect(step).toEqual({
			type: "step",
			ts: 102,
			experimentId: "exp",
			arm: "dream",
			iteration: 1,
			poolSize: 1,
			measuredTrees: 1,
			evidenceTrees: 0,
			currentValue: 0.4,
			chosenPolicyId: policyId(OTHER),
			improved: true,
			dreamer: "local",
			leverScan: LEVER,
		});
		// One JSON object per line, every value a scalar, the policy object or the lever record.
		const raw = readFileSync(path, "utf8");
		expect(raw.endsWith("\n")).toBe(true);
		expect(raw.trim().split("\n")).toHaveLength(3);
		expect((statSync(path).mode & 0o777).toString(8)).toBe("600");
	});

	it("appends later steps and the post-hoc final selection (iteration -1) without context when none is given", () => {
		const dir = scratch();
		const path = dreamsPath(dir, "run-b");
		const log = new DreamsLog(path, () => 7);
		log.recordStep({
			iteration: 1,
			poolSize: 1,
			selection: {
				candidates: [verdict()],
				currentScore: 0.4,
				chosenPolicy: OTHER,
				improved: true,
				dreamer: "llm",
				measuredTrees: 1,
			},
			leverScan: LEVER,
		});
		log.recordStep({
			iteration: -1,
			poolSize: 2,
			selection: {
				candidates: [verdict({ reason: "tie", eligible: true })],
				currentScore: 0.5,
				chosenPolicy: DEFAULT_POLICY,
				improved: false,
				dreamer: "local",
				measuredTrees: 2,
			},
			leverScan: null,
		});
		const lines = readDreamsLog(path);
		expect(lines).toHaveLength(4);
		expect(lines.map((line) => line.iteration)).toEqual([1, 1, -1, -1]);
		const final = lines[3] as DreamStepLine;
		expect(final.leverScan).toBeNull();
		expect(final.chosenPolicyId).toBe(policyId(DEFAULT_POLICY));
		expect(final.dreamer).toBe("local");
		expect("experimentId" in final).toBe(false);
		expect("arm" in final).toBe(false);
		expect((lines[0] as DreamCandidateLine).policy).toEqual(OTHER);
	});

	it("writes nothing but the step line for a step with no candidates", () => {
		const dir = scratch();
		const path = dreamsPath(dir, "run-c");
		new DreamsLog(path, () => 1).recordStep({
			iteration: -1,
			poolSize: 3,
			selection: {
				candidates: [],
				currentScore: 1,
				chosenPolicy: DEFAULT_POLICY,
				improved: false,
				dreamer: "local",
				measuredTrees: 3,
			},
			leverScan: null,
		});
		expect(readDreamsLog(path)).toEqual([
			{
				type: "step",
				ts: 1,
				iteration: -1,
				poolSize: 3,
				measuredTrees: 3,
				evidenceTrees: 2,
				currentValue: 1,
				chosenPolicyId: policyId(DEFAULT_POLICY),
				improved: false,
				dreamer: "local",
				leverScan: null,
			},
		]);
	});
});

describe("readDreamsLog", () => {
	it("returns an empty log for a missing file and throws on a malformed line", () => {
		const dir = scratch();
		expect(readDreamsLog(join(dir, "dreams", "absent.jsonl"))).toEqual([]);
		const path = join(dir, "bad.jsonl");
		writeFileSync(path, `${JSON.stringify({ type: "step", ts: 1 })}\n`);
		expect(() => readDreamsLog(path)).toThrow(/malformed dreams log line/);
		writeFileSync(path, "not json\n");
		expect(() => readDreamsLog(path)).toThrow();
	});

	it("skips blank lines", () => {
		const dir = scratch();
		const path = join(dir, "blank.jsonl");
		const step: DreamStepLine = {
			type: "step",
			ts: 1,
			iteration: 2,
			poolSize: 2,
			measuredTrees: 1,
			evidenceTrees: 0,
			currentValue: 0.3,
			chosenPolicyId: "abc",
			improved: false,
			dreamer: "mixed",
			leverScan: null,
		};
		writeFileSync(path, `\n${JSON.stringify(step)}\n\n`);
		expect(readDreamsLog(path)).toEqual([step]);
	});
});

const PROBATION: DreamProbationRecord = {
	policyId: policyId(OTHER),
	incumbentPolicyId: policyId(DEFAULT_POLICY),
	treeId: "tree-2",
	roundBest: 0.2,
	floor: 1,
	chargedProbes: 1,
	chargedRounds: 1,
	incumbentChargedProbes: 6,
	incumbentChargedRounds: 4,
	evidenceTrees: 1,
	reverted: true,
};

describe("DreamsLog.recordProbation", () => {
	it("appends a probation line after the step, with the context and the injected clock", () => {
		const dir = scratch();
		let now = 10;
		const log = new DreamsLog(dreamsPath(dir, "run"), () => now++, { experimentId: "exp", arm: "dream" });
		log.recordStep({
			iteration: 2,
			poolSize: 2,
			selection: {
				candidates: [verdict()],
				currentScore: 0.4,
				chosenPolicy: OTHER,
				improved: true,
				dreamer: "local",
				measuredTrees: 2,
			},
			leverScan: null,
		});
		log.recordProbation(2, PROBATION);
		const lines = readDreamsLog(dreamsPath(dir, "run"));
		expect(lines.map((line) => line.type)).toEqual(["candidate", "step", "probation"]);
		const probation = lines[2] as DreamProbationLine;
		expect(probation).toEqual({
			type: "probation",
			ts: 12,
			experimentId: "exp",
			arm: "dream",
			iteration: 2,
			...PROBATION,
		});
		expect(isDreamsLogLine(probation)).toBe(true);
		expect(isDreamsLogLine({ ...probation, reverted: "yes" })).toBe(false);
		expect(isDreamsLogLine({ type: "probation", ts: 1, iteration: 2 })).toBe(false);
	});
});

describe("isDreamsLogLine", () => {
	it("accepts well-formed candidate and step lines and rejects the rest", () => {
		expect(isDreamsLogLine({ type: "candidate", ts: 1, iteration: 0, ...verdict() })).toBe(true);
		expect(isDreamsLogLine({ type: "candidate", ts: 1, iteration: 0, ...verdict({ reason: "nope" as never }) })).toBe(
			false,
		);
		expect(isDreamsLogLine({ type: "candidate", ts: 1, iteration: 0, ...verdict(), policy: null })).toBe(false);
		expect(
			isDreamsLogLine({
				type: "step",
				ts: 1,
				iteration: -1,
				poolSize: 1,
				currentValue: 0,
				chosenPolicyId: "x",
				improved: false,
				dreamer: "local",
				leverScan: null,
			}),
		).toBe(true);
		expect(isDreamsLogLine({ type: "step", ts: 1, iteration: -1 })).toBe(false);
		expect(isDreamsLogLine({ type: "other", ts: 1, iteration: 0 })).toBe(false);
		expect(isDreamsLogLine({ type: "step", iteration: 0 })).toBe(false);
		expect(isDreamsLogLine(null)).toBe(false);
		expect(isDreamsLogLine("step")).toBe(false);
	});
});
