/**
 * The per-run dreams log: one JSONL line per candidate per dreaming step plus
 * one step summary, so WHY every proposed policy did or did not win is
 * recoverable after the fact. Before it existed a run kept only a candidate
 * count, and the twelve LLM candidates of the first real-token run were lost
 * with the reason each one lost.
 *
 * Layout mirrors `rejections.ts`: `<dreamDir>/dreams/<runKey>.jsonl`, beside
 * `trees/` (never inside it, so `listTrees` does not see it). Every field is a
 * scalar or the typed policy object. Written on BOTH the local and the LLM
 * path; the arm-level post-hoc final selection is logged with `iteration` -1.
 * Writing touches no rng and no tree, so the trees a loop grows are byte-
 * identical with and without the log.
 */

import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import type { PolicySelection } from "./improve.js";
import { policyId } from "./policy.js";
import {
	type CandidateVerdict,
	type DreamClock,
	type DreamerKind,
	isCandidateReason,
	type LeverScanRecord,
} from "./types.js";

const DIR_MODE = 0o700;
const FILE_MODE = 0o600;

/** Where a log line came from when the run is an experiment arm. */
export interface DreamsLogContext {
	experimentId?: string;
	arm?: string;
}

/** One candidate of one dreaming step. */
export interface DreamCandidateLine extends CandidateVerdict, DreamsLogContext {
	type: "candidate";
	ts: number;
	/** Loop iteration of the step; -1 for the post-hoc final selection. */
	iteration: number;
}

/** One dreaming step's summary, written after its candidate lines. */
export interface DreamStepLine extends DreamsLogContext {
	type: "step";
	ts: number;
	/** Loop iteration of the step; -1 for the post-hoc final selection. */
	iteration: number;
	poolSize: number;
	/** Trees of the pool the current policy replayed in full support (the measured pool the scores are means over). */
	measuredTrees: number;
	currentValue: number;
	chosenPolicyId: string;
	improved: boolean;
	dreamer: DreamerKind;
	leverScan: LeverScanRecord | null;
}

export type DreamsLogLine = DreamCandidateLine | DreamStepLine;

/** What one step contributes to the log: the selection's verdicts plus the step facts. */
export interface DreamStepInput {
	iteration: number;
	poolSize: number;
	selection: Pick<
		PolicySelection,
		"candidates" | "currentScore" | "chosenPolicy" | "improved" | "dreamer" | "measuredTrees"
	>;
	leverScan: LeverScanRecord | null;
}

export function dreamsDir(dir: string): string {
	return join(dir, "dreams");
}

export function dreamsPath(dir: string, runKey: string): string {
	return join(dreamsDir(dir), `${runKey}.jsonl`);
}

/** Append-only writer for one run's dreams log; the file is created on the first step. */
export class DreamsLog {
	constructor(
		readonly path: string,
		private readonly clock: DreamClock,
		private readonly context: DreamsLogContext = {},
	) {}

	/** Write every candidate line of a step, then its step line. */
	recordStep(input: DreamStepInput): void {
		const lines: DreamsLogLine[] = input.selection.candidates.map((verdict) => ({
			type: "candidate",
			ts: this.clock(),
			...this.context,
			iteration: input.iteration,
			...verdict,
		}));
		lines.push({
			type: "step",
			ts: this.clock(),
			...this.context,
			iteration: input.iteration,
			poolSize: input.poolSize,
			measuredTrees: input.selection.measuredTrees,
			currentValue: input.selection.currentScore,
			chosenPolicyId: policyId(input.selection.chosenPolicy),
			improved: input.selection.improved,
			dreamer: input.selection.dreamer,
			leverScan: input.leverScan,
		});
		mkdirSync(dirname(this.path), { recursive: true, mode: DIR_MODE });
		appendFileSync(this.path, lines.map((line) => `${JSON.stringify(line)}\n`).join(""), { mode: FILE_MODE });
	}
}

/** Parse a dreams log; a missing file is an empty log, a malformed line throws. */
export function readDreamsLog(path: string): DreamsLogLine[] {
	let text: string;
	try {
		text = readFileSync(path, "utf8");
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
		throw error;
	}
	const records: DreamsLogLine[] = [];
	for (const line of text.split("\n")) {
		if (line.trim().length === 0) continue;
		const parsed = JSON.parse(line) as unknown;
		if (!isDreamsLogLine(parsed)) throw new Error(`malformed dreams log line in ${path}`);
		records.push(parsed);
	}
	return records;
}

export function isDreamsLogLine(value: unknown): value is DreamsLogLine {
	if (typeof value !== "object" || value === null) return false;
	const record = value as Record<string, unknown>;
	if (typeof record.ts !== "number" || typeof record.iteration !== "number") return false;
	if (record.type === "candidate") {
		return (
			typeof record.index === "number" &&
			typeof record.policyId === "string" &&
			typeof record.policy === "object" &&
			record.policy !== null &&
			typeof record.value === "number" &&
			typeof record.quality === "number" &&
			typeof record.eligible === "boolean" &&
			isCandidateReason(record.reason)
		);
	}
	if (record.type === "step") {
		return (
			typeof record.poolSize === "number" &&
			typeof record.currentValue === "number" &&
			typeof record.chosenPolicyId === "string" &&
			typeof record.improved === "boolean" &&
			typeof record.dreamer === "string"
		);
	}
	return false;
}
