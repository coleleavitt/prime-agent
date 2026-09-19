/**
 * The per-run rejection log of the LLM proposer: one JSONL line per child result
 * the proposer refused, so a rejected output's cause is recoverable after the
 * fact. A rejected child result used to vanish behind a local fallback with no
 * record but its tokens; this file is the system of record for WHY the agent's
 * candidate did not enter the tree.
 *
 * Layout: `<dreamDir>/rejections/<runKey>.jsonl`, beside `trees/` (never inside
 * it, so `listTrees` does not see it). Every field is a scalar; the child's
 * output is never stored whole, only a bounded head/tail excerpt. Reached only
 * from the flag-gated LLM path (`llm.ts`, `experiment-llm.ts`): the local path
 * never writes a rejection, so it stays byte-identical.
 */

import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import type { RunAgentStatus } from "../run-agent.js";
import { isProposalRejectReason, type ProposalRejectReason } from "./proposer.js";
import type { DreamClock } from "./types.js";

const DIR_MODE = 0o700;
const FILE_MODE = 0o600;

/** Total characters an output excerpt keeps (head and tail, joined by an ellipsis marker). */
export const REJECTION_EXCERPT_CHARS = 240;
const EXCERPT_JOINER = " ... ";

/** One rejected child result. */
export interface ProposalRejection {
	type: "rejection";
	/** Injected clock reading when the rejection was recorded. */
	ts: number;
	/** Loop iteration whose rollout the attempt belongs to (the shared experiment round 1 is 0). */
	iteration: number;
	/** Online round of the rollout. */
	round: number;
	/** 1-based child result index within the propose call: 2 is the retry. */
	attempt: number;
	reason: ProposalRejectReason;
	/** Terminal `RunAgentStatus` of the child. */
	status: RunAgentStatus;
	/** True when this was the attempt's last child result and the local mutator stood in. */
	fellBack: boolean;
	/** Total tokens the child spent. */
	tokens: number;
	/** Output tokens the child spent (0 when the handler reports none). */
	outputTokens: number;
	/** The child's terminal assistant stop reason, when a message carried one. */
	stopReason?: string;
	/** The validation or child error message. */
	error?: string;
	/** Bounded head/tail of the child's output. */
	excerpt: string;
}

export type ProposalRejectionInput = Omit<ProposalRejection, "type" | "ts">;

export function rejectionsDir(dir: string): string {
	return join(dir, "rejections");
}

export function rejectionsPath(dir: string, runKey: string): string {
	return join(rejectionsDir(dir), `${runKey}.jsonl`);
}

/** Head and tail of `output`, at most `REJECTION_EXCERPT_CHARS` characters in total. */
export function excerptOf(output: string, maxChars = REJECTION_EXCERPT_CHARS): string {
	const cap = Math.max(EXCERPT_JOINER.length + 2, Math.trunc(maxChars));
	if (output.length <= cap) return output;
	const keep = cap - EXCERPT_JOINER.length;
	const head = Math.ceil(keep / 2);
	const tail = keep - head;
	return `${output.slice(0, head)}${EXCERPT_JOINER}${output.slice(output.length - tail)}`;
}

/** Append-only writer for one run's rejection log; the file is created on the first rejection. */
export class RejectionLog {
	constructor(
		readonly path: string,
		private readonly clock: DreamClock,
	) {}

	append(input: ProposalRejectionInput): void {
		mkdirSync(dirname(this.path), { recursive: true, mode: DIR_MODE });
		const record: ProposalRejection = { type: "rejection", ts: this.clock(), ...input };
		appendFileSync(this.path, `${JSON.stringify(record)}\n`, { mode: FILE_MODE });
	}
}

/** Parse a rejection log; a missing file is an empty log, a malformed line throws. */
export function readRejections(path: string): ProposalRejection[] {
	let text: string;
	try {
		text = readFileSync(path, "utf8");
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
		throw error;
	}
	const records: ProposalRejection[] = [];
	for (const line of text.split("\n")) {
		if (line.trim().length === 0) continue;
		const parsed = JSON.parse(line) as unknown;
		if (!isProposalRejection(parsed)) throw new Error(`malformed rejection line in ${path}`);
		records.push(parsed);
	}
	return records;
}

export function isProposalRejection(value: unknown): value is ProposalRejection {
	if (typeof value !== "object" || value === null) return false;
	const record = value as Record<string, unknown>;
	return (
		record.type === "rejection" &&
		typeof record.ts === "number" &&
		typeof record.iteration === "number" &&
		typeof record.round === "number" &&
		typeof record.attempt === "number" &&
		isProposalRejectReason(record.reason) &&
		typeof record.status === "string" &&
		typeof record.fellBack === "boolean" &&
		typeof record.tokens === "number" &&
		typeof record.outputTokens === "number" &&
		typeof record.excerpt === "string"
	);
}
