import { AsyncLocalStorage } from "node:async_hooks";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	type AssistantMessage,
	addSpanSink,
	installAsyncTraceContextStorage,
	type SpanEndRecord,
	type StopReason,
	type TraceContext,
	type Usage,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { type DreamCandidateLine, type DreamStepLine, dreamsPath, readDreamsLog } from "../src/core/dream/dreams.js";
import { proposePolicies, runDreaming, scorePolicyOnPool } from "../src/core/dream/improve.js";
import { projectProposeParams } from "../src/core/dream/interpreter.js";
import {
	buildDreamerInput,
	buildDreamPrompt,
	buildGuidanceInput,
	createLlmProposer,
	DREAMER_PROMPT_HEADER,
	DreamAbortError,
	type DreamLoopWithAgentOptions,
	type DreamProgressEvent,
	GUIDANCE_PROMPT_HEADER,
	type GuidanceInput,
	historyOf,
	isDreamAbortError,
	mergePrimedRollouts,
	PROPOSER_JSON_ONLY,
	PROPOSER_PROMPT_HEADER,
	parseCandidateArray,
	proposePoliciesWithAgent,
	runDreamLoopWithAgent,
	runOnlineExplorationWithAgent,
} from "../src/core/dream/llm.js";
import {
	type DreamHandlerCalls,
	type DreamRoundRecord,
	dreamRunId,
	primingTreeId,
	runDreamLoop,
} from "../src/core/dream/loop.js";
import {
	DEFAULT_POLICY,
	type ExplorationPolicy,
	PRIMING_DIVERSE,
	policyId,
	REPLAY_DEAD_FIELDS,
} from "../src/core/dream/policy.js";
import {
	asyncOf,
	createLocalProposer,
	type ProposalTally,
	totalRejected,
	zeroProposalTally,
} from "../src/core/dream/proposer.js";
import { excerptOf, RejectionLog, readRejections, rejectionsPath } from "../src/core/dream/rejections.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import { runOnlineExploration } from "../src/core/dream/rollout.js";
import { buildRecordedTree, DreamStoreError, listTrees, type RecordedTree, readTree } from "../src/core/dream/store.js";
import type { ScoredTask } from "../src/core/dream/task.js";
import { AUTOCORRELATION_SHAPE_EXAMPLE, createAutocorrelationTask } from "../src/core/dream/tasks/autocorrelation.js";
import { resolveTask, taskPromptContext } from "../src/core/dream/tasks/index.js";
import type { NodeRecord, TreeRecord } from "../src/core/dream/types.js";
import type { ChildRuntimeScope } from "../src/core/ravo/runtime-adapter.js";
import type { RunAgentHandler, RunAgentResult } from "../src/core/run-agent.js";

// The async driver's span nesting relies on AsyncLocalStorage carrying the trace
// context across awaits; install it exactly as the other async trace tests do.
installAsyncTraceContextStorage(new AsyncLocalStorage<TraceContext>());

const FIXED_CLOCK = 1_700_000_000_000;
const SCOPE: ChildRuntimeScope = { tools: "none", maxTurns: 2, tokenBudget: 500_000 };

let dreamDir: string;
let priorDreamDir: string | undefined;
let priorAgentDir: string | undefined;
const scratchDirs: string[] = [];

function scratch(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	scratchDirs.push(dir);
	return dir;
}

beforeEach(() => {
	dreamDir = scratch("dream-llm-");
	priorDreamDir = process.env.PRIME_AGENT_DREAM_DIR;
	priorAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	process.env.PRIME_AGENT_DREAM_DIR = dreamDir;
	process.env.PRIME_AGENT_CODING_AGENT_DIR = scratch("dream-llm-agent-");
});

afterEach(() => {
	if (priorDreamDir === undefined) delete process.env.PRIME_AGENT_DREAM_DIR;
	else process.env.PRIME_AGENT_DREAM_DIR = priorDreamDir;
	if (priorAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	else process.env.PRIME_AGENT_CODING_AGENT_DIR = priorAgentDir;
	for (const dir of scratchDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function usage(totalTokens: number, output = 0): Usage {
	return {
		input: Math.max(0, totalTokens - output),
		output,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
}

/** The child's terminal assistant message, so the proposer can read its stop reason. */
function assistant(text: string, stopReason: StopReason, tokens: Usage): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		api: "anthropic-messages",
		provider: "anthropic",
		model: "faux/stub",
		usage: tokens,
		stopReason,
		timestamp: FIXED_CLOCK,
	};
}

function result(over: Partial<RunAgentResult> & Pick<RunAgentResult, "status">): RunAgentResult {
	return {
		output: "",
		messages: [],
		model: "faux/stub",
		turns: 1,
		toolCalls: 0,
		usage: usage(0),
		...over,
	};
}

type StubRole = keyof DreamHandlerCalls;
type StubAnswer = {
	output?: string;
	status?: RunAgentResult["status"];
	tokens?: number;
	/** Output tokens within `tokens` (0 unless set). */
	outputTokens?: number;
	/** When set, the result carries a terminal assistant message with this stop reason. */
	stopReason?: StopReason;
};
const DEFAULT_INSIGHTS = "Sets with a wide spread of gaps scored higher; dense arithmetic runs scored lower.";

/** Classify a child prompt by the role header `llm.ts` puts on its first line; anything else is a test failure. */
function roleOf(prompt: string): StubRole {
	if (prompt.startsWith(GUIDANCE_PROMPT_HEADER)) return "guidance";
	if (prompt.startsWith(DREAMER_PROMPT_HEADER)) return "dreamer";
	if (prompt.startsWith(PROPOSER_PROMPT_HEADER)) return "proposer";
	throw new Error(`unclassified child prompt: ${prompt.slice(0, 60)}`);
}

/**
 * A stub RunAgentHandler that answers by role (proposer, dreamer, guidance writer),
 * records every prompt it saw, and tallies calls and the tokens it reports per role.
 * It never spends a real token.
 */
function makeStub(opts: {
	proposerOutput?: (call: number) => StubAnswer;
	dreamerOutput?: (call: number) => StubAnswer;
	guidanceOutput?: (call: number) => StubAnswer;
}): {
	handler: RunAgentHandler;
	totalTokens: () => number;
	calls: () => number;
	roleCalls: DreamHandlerCalls;
	roleTokens: DreamHandlerCalls;
	prompts: Record<StubRole, string[]>;
} {
	let total = 0;
	let calls = 0;
	const roleCalls: DreamHandlerCalls = { proposer: 0, dreamer: 0, guidance: 0 };
	const roleTokens: DreamHandlerCalls = { proposer: 0, dreamer: 0, guidance: 0 };
	const prompts: Record<StubRole, string[]> = { proposer: [], dreamer: [], guidance: [] };
	const handler: RunAgentHandler = async (request) => {
		calls += 1;
		const role = roleOf(request.prompt);
		roleCalls[role] += 1;
		prompts[role].push(request.prompt);
		let spec: StubAnswer | undefined;
		if (role === "dreamer") spec = opts.dreamerOutput?.(roleCalls.dreamer);
		else if (role === "guidance") {
			spec = opts.guidanceOutput?.(roleCalls.guidance) ?? { output: JSON.stringify({ insights: DEFAULT_INSIGHTS }) };
		} else spec = opts.proposerOutput?.(roleCalls.proposer);
		const tokens = spec?.tokens ?? 100;
		total += tokens;
		roleTokens[role] += tokens;
		const output = spec?.output ?? "";
		const used = usage(tokens, spec?.outputTokens ?? 0);
		return result({
			status: spec?.status ?? "completed",
			output,
			usage: used,
			messages: spec?.stopReason ? [assistant(output, spec.stopReason, used)] : [],
		});
	};
	return { handler, totalTokens: () => total, calls: () => calls, roleCalls, roleTokens, prompts };
}

function liveController(): AbortController {
	return new AbortController();
}

function readTreeFiles(dir: string, treeId: string): Record<string, string> {
	const files: Record<string, string> = {};
	files["tree.jsonl"] = readFileSync(join(dir, "trees", `${treeId}.jsonl`), "utf8");
	const blobDir = join(dir, "trees", treeId, "blobs");
	for (const name of readdirSync(blobDir).sort()) {
		files[`blob/${name}`] = readFileSync(join(blobDir, name), "utf8");
	}
	return files;
}

// A shallow synthetic pool (mirrors dream-improve): revealing more of the root's
// children raises the best score, so a policy that keeps going beats one that stops.
function node(over: Partial<NodeRecord> & Pick<NodeRecord, "id" | "parentId" | "seq" | "score">): NodeRecord {
	return { type: "node", branch: 0, round: 0, valid: true, artifactRef: "ref", tokens: 0, ts: 0, ...over };
}
const SYNTH: TreeRecord[] = [
	{
		type: "tree",
		version: 1,
		treeId: "synth",
		taskId: "synthetic",
		w: 2,
		seed: 1,
		policyId: "p",
		iteration: 0,
		createdTs: 0,
	},
	node({ id: "synth-n0", parentId: null, seq: 0, score: 0.3 }),
	node({ id: "synth-n1", parentId: "synth-n0", seq: 1, branch: 0, score: 0.5 }),
	node({ id: "synth-n2", parentId: "synth-n0", seq: 2, branch: 1, score: 0.4 }),
	node({ id: "synth-n3", parentId: "synth-n0", seq: 3, branch: 2, score: 0.9 }),
];
function synthPool(): RecordedTree[] {
	return [buildRecordedTree(SYNTH)];
}
function policy(over: Partial<ExplorationPolicy>): ExplorationPolicy {
	return { ...DEFAULT_POLICY, ...over };
}
const CFG = { k1: 5, k2: 10, objective: { beta1: 0.05, beta2: 0.05, beta3: 0 } };
const CURRENT = policy({ selectionRule: "explore-root", stopRule: "patience", beta: 1, batchSize: 1 });
const WORSE = policy({ selectionRule: "best-first", stopRule: "never", batchSize: 1 });

const PARAMS = projectProposeParams(DEFAULT_POLICY);

describe("createLlmProposer", () => {
	it("returns the deserialized child artifact and the child's tokens", async () => {
		const task = resolveTask({ task: "sum-difference" });
		const artifactJson = JSON.stringify({ set: [0, 1, 2, 4, 9, 15] });
		const stub = makeStub({ proposerOutput: () => ({ output: artifactJson, tokens: 1234 }) });
		const proposer = createLlmProposer(stub.handler, task, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
		});
		const outcome = await proposer.propose(null, PARAMS, createSeededRng(1), 1);
		expect(outcome.tokens).toBe(1234);
		expect(task.serialize(outcome.artifact)).toEqual({ set: [0, 1, 2, 4, 9, 15] });
		expect(task.evaluate(outcome.artifact).valid).toBe(true);
		expect(stub.totalTokens()).toBe(1234);
	});

	it("falls back to the local proposer on invalid child output, summing the spent tokens", async () => {
		const task = resolveTask({ task: "sum-difference" });
		// Valid JSON but a shape the task rejects -> deserialize throws inside the child call.
		const stub = makeStub({ proposerOutput: () => ({ output: JSON.stringify({ notASet: true }), tokens: 40 }) });
		const proposer = createLlmProposer(stub.handler, task, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
		});
		const parent = task.deserialize({ set: [0, 1, 2, 3] });
		const outcome = await proposer.propose(parent, PARAMS, createSeededRng(7), 2);
		// retried once (1 retry) -> two calls -> summed tokens.
		expect(outcome.tokens).toBe(80);
		expect(() => task.evaluate(outcome.artifact)).not.toThrow();
		expect(task.evaluate(outcome.artifact).valid).toBe(true);
		// The fallback is the deterministic local proposer for the same rng/params.
		const local = createLocalProposer(task).propose(parent, PARAMS, createSeededRng(7), 2);
		expect(task.serialize(outcome.artifact)).toEqual(task.serialize(local.artifact));
	});

	it("surfaces an aborted child as a DreamAbortError sentinel (does not fall back)", async () => {
		const task = resolveTask({ task: "sum-difference" });
		const stub = makeStub({ proposerOutput: () => ({ status: "aborted", tokens: 10 }) });
		const tally = zeroProposalTally();
		const proposer = createLlmProposer(stub.handler, task, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
			tally,
		});
		await expect(proposer.propose(null, PARAMS, createSeededRng(1), 1)).rejects.toBeInstanceOf(DreamAbortError);
		expect(tally).toEqual({
			...zeroProposalTally(),
			llmProposals: 1,
			llmRejected: { ...zeroProposalTally().llmRejected, aborted: 1 },
		});
		expect(tally.localFallbacks).toBe(0);
	});
});

/** A 4-bin autocorrelation task (below the registry's paper sizes, so built directly): small enough to read a prompt. */
function n4Task(): ScoredTask<unknown> {
	return createAutocorrelationTask(4) as unknown as ScoredTask<unknown>;
}
const N4_PARENT = { n: 4, weights: [2, 2, 2, 2] };

describe("createLlmProposer: extraction, rejection provenance and origin", () => {
	function n4Proposer(stub: ReturnType<typeof makeStub>, tally: ProposalTally) {
		const task = n4Task();
		return {
			task,
			proposer: createLlmProposer(stub.handler, task, {
				scope: SCOPE,
				signal: liveController().signal,
				tokenBudget: 200_000,
				promptContext: taskPromptContext("autocorrelation", 4),
				tally,
			}),
		};
	}

	it("accepts a JSON object wrapped in prose and code fences and marks it origin llm", async () => {
		const wrapped = [
			"Here is my improved candidate. I moved mass from the middle bins [1, 2] toward the edges:",
			"```json",
			'{"n": 4, "weights": [3, 1, 1, 3]}',
			"```",
			"This lowers the central peak {see the hint about a flat top}.",
		].join("\n");
		const stub = makeStub({ proposerOutput: () => ({ output: wrapped, tokens: 300, outputTokens: 120 }) });
		const tally = zeroProposalTally();
		const { task, proposer } = n4Proposer(stub, tally);
		const { value: outcome, spans } = await captureSpans(() =>
			proposer.propose(task.deserialize(N4_PARENT), PARAMS, createSeededRng(1), 1),
		);
		expect(outcome.origin).toBe("llm");
		expect(outcome.tokens).toBe(300);
		expect(task.serialize(outcome.artifact)).toMatchObject({ n: 4, weights: [3, 1, 1, 3] });
		expect(stub.calls()).toBe(1);
		expect(tally).toEqual({ ...zeroProposalTally(), llmProposals: 1, llmAccepted: 1 });
		const span = spans.find((record) => record.name === "dream.llm_propose")!;
		expect(span.attrs["dream.llm_fallback"]).toBe(false);
		expect(span.attrs["dream.origin"]).toBe("llm");
		expect(span.attrs["dream.llm_attempts"]).toBe(1);
		expect(span.attrs["dream.llm_output_tokens"]).toBe(120);
		expect(span.attrs["dream.llm_reject_reason"]).toBeUndefined();
	});

	it("rejects wrong-length weights as shape, retries once, then falls back with origin local and logs both rejections", async () => {
		const short = '{"n": 4, "weights": [1, 2, 3]}';
		const stub = makeStub({ proposerOutput: () => ({ output: short, tokens: 40, outputTokens: 15 }) });
		const tally = zeroProposalTally();
		const task = n4Task();
		const logPath = rejectionsPath(dreamDir, "unit");
		const proposer = createLlmProposer(stub.handler, task, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
			promptContext: taskPromptContext("autocorrelation", 4),
			tally,
			iteration: 3,
			rejections: new RejectionLog(logPath, () => FIXED_CLOCK),
		});
		const parent = task.deserialize(N4_PARENT);
		const { value: outcome, spans } = await captureSpans(() =>
			proposer.propose(parent, PARAMS, createSeededRng(7), 2),
		);
		expect(stub.calls()).toBe(2);
		expect(outcome.origin).toBe("local");
		expect(outcome.tokens).toBe(80);
		const local = createLocalProposer(task).propose(parent, PARAMS, createSeededRng(7), 2);
		expect(task.serialize(outcome.artifact)).toEqual(task.serialize(local.artifact));
		expect(tally.llmProposals).toBe(2);
		expect(tally.llmAccepted).toBe(0);
		expect(tally.llmRejected.shape).toBe(2);
		expect(totalRejected(tally)).toBe(2);
		expect(tally.localFallbacks).toBe(1);
		const span = spans.find((record) => record.name === "dream.llm_propose")!;
		expect(span.attrs["dream.llm_fallback"]).toBe(true);
		expect(span.attrs["dream.origin"]).toBe("local");
		expect(span.attrs["dream.llm_reject_reason"]).toBe("shape");
		expect(span.attrs["dream.llm_status"]).toBe("completed");
		expect(span.attrs["dream.llm_attempts"]).toBe(2);
		expect(span.attrs["dream.llm_output_tokens"]).toBe(30);
		expect(span.attrs["dream.llm_reject_excerpt"]).toBe(short);
		const logged = readRejections(logPath);
		expect(logged).toHaveLength(2);
		expect(logged.map((record) => [record.attempt, record.fellBack, record.reason])).toEqual([
			[1, false, "shape"],
			[2, true, "shape"],
		]);
		expect(logged.every((record) => record.iteration === 3 && record.round === 2 && record.ts === FIXED_CLOCK)).toBe(
			true,
		);
		expect(
			logged.every((record) => record.status === "completed" && record.tokens === 40 && record.outputTokens === 15),
		).toBe(true);
		expect(logged[0]!.excerpt).toBe(short);
		expect(logged[0]!.error).toMatch(/length 4/);
	});

	it("classifies an output cut at the cap as length, does not retry it, and bounds the excerpt", async () => {
		const runaway = `Let me reason about this carefully. ${"The peak is the central knot. ".repeat(40)}{"n": 4, "weights": [1, 2,`;
		const stub = makeStub({
			proposerOutput: () => ({ output: runaway, tokens: 32_500, outputTokens: 32_000, stopReason: "length" }),
		});
		const tally = zeroProposalTally();
		const { task, proposer } = n4Proposer(stub, tally);
		const { value: outcome, spans } = await captureSpans(() =>
			proposer.propose(task.deserialize(N4_PARENT), PARAMS, createSeededRng(3), 1),
		);
		expect(stub.calls()).toBe(1);
		expect(outcome.origin).toBe("local");
		expect(outcome.tokens).toBe(32_500);
		expect(tally).toEqual({
			...zeroProposalTally(),
			llmProposals: 1,
			llmRejected: { ...zeroProposalTally().llmRejected, length: 1 },
			localFallbacks: 1,
		});
		const span = spans.find((record) => record.name === "dream.llm_propose")!;
		expect(span.attrs["dream.llm_reject_reason"]).toBe("length");
		expect(span.attrs["dream.llm_output_tokens"]).toBe(32_000);
		expect(span.attrs["dream.llm_attempts"]).toBe(1);
		const excerpt = span.attrs["dream.llm_reject_excerpt"] as string;
		expect(excerpt.length).toBeLessThanOrEqual(240);
		expect(excerpt.startsWith("Let me reason")).toBe(true);
		expect(excerpt.endsWith("[1, 2,")).toBe(true);
		expect(excerpt).toContain(" ... ");
	});

	it("maps a finished-but-truncated parse to length too, and a mid-prose fragment never shadows the answer", async () => {
		// A complete small object inside the prose plus the real, larger object: the largest wins.
		const output =
			'Compared with {"n": 4} the shape below is flatter:\n{"n": 4, "weights": [2.5, 1.5, 1.5, 2.5]}\nDone.';
		const stub = makeStub({ proposerOutput: () => ({ output, tokens: 50 }) });
		const tally = zeroProposalTally();
		const { task, proposer } = n4Proposer(stub, tally);
		const outcome = await proposer.propose(task.deserialize(N4_PARENT), PARAMS, createSeededRng(3), 1);
		expect(outcome.origin).toBe("llm");
		expect(task.serialize(outcome.artifact)).toMatchObject({ weights: [2.5, 1.5, 1.5, 2.5] });

		// No JSON object at all -> parse; retried once, then fallback.
		const prose = makeStub({
			proposerOutput: () => ({ output: "I cannot improve on the uniform density.", tokens: 5 }),
		});
		const proseTally = zeroProposalTally();
		const p = n4Proposer(prose, proseTally);
		const fell = await p.proposer.propose(p.task.deserialize(N4_PARENT), PARAMS, createSeededRng(3), 1);
		expect(fell.origin).toBe("local");
		expect(prose.calls()).toBe(2);
		expect(proseTally.llmRejected.parse).toBe(2);
		expect(proseTally.localFallbacks).toBe(1);
	});

	it("maps a child error, turn limit and budget to their reasons; only the error is retried", async () => {
		for (const [status, reason, calls] of [
			["error", "error", 2],
			["turn_limit", "turn-limit", 1],
			["budget_exceeded", "budget", 1],
		] as const) {
			const stub = makeStub({ proposerOutput: () => ({ status, tokens: 3 }) });
			const tally = zeroProposalTally();
			const { task, proposer } = n4Proposer(stub, tally);
			const outcome = await proposer.propose(task.deserialize(N4_PARENT), PARAMS, createSeededRng(1), 1);
			expect(outcome.origin, status).toBe("local");
			expect(stub.calls(), status).toBe(calls);
			expect(tally.llmRejected[reason], status).toBe(calls);
			expect(tally.localFallbacks, status).toBe(1);
			expect(tally.llmProposals, status).toBe(calls);
		}
	});

	it("puts the exact n, a parseable shape example and the JSON-only instruction LAST in the prompt", async () => {
		const stub = makeStub({ proposerOutput: () => ({ output: '{"n": 4, "weights": [1, 1, 1, 1]}', tokens: 1 }) });
		const { task, proposer } = n4Proposer(stub, zeroProposalTally());
		await proposer.propose(task.deserialize(N4_PARENT), PARAMS, createSeededRng(1), 1);
		const prompt = stub.prompts.proposer[0]!;
		expect(prompt.startsWith(PROPOSER_PROMPT_HEADER)).toBe(true);
		expect(prompt).toContain('"n": 4');
		expect(prompt).toContain("exactly 4 weights");
		expect(prompt).toContain("exactly 4 entries");
		expect(prompt).not.toContain("exactly n entries");
		expect(prompt).toContain(AUTOCORRELATION_SHAPE_EXAMPLE);
		expect(JSON.parse(AUTOCORRELATION_SHAPE_EXAMPLE)).toEqual({ n: 4, weights: [1.5, 2.5, 2.5, 1.5] });
		const lines = prompt.split("\n");
		expect(lines.at(-1)).toBe(PROPOSER_JSON_ONLY);
		expect(PROPOSER_JSON_ONLY).toMatch(/ONLY the JSON object/);
		expect(PROPOSER_JSON_ONLY).toMatch(/no prose, no code fences/);
		// Order: candidate, then the task contract, then the output contract, then the JSON-only line.
		const candidate = prompt.indexOf("Current candidate (JSON)");
		const contract = prompt.indexOf("Contract:");
		const output = prompt.indexOf("Output contract:");
		const last = prompt.lastIndexOf(PROPOSER_JSON_ONLY);
		expect(candidate).toBeGreaterThan(0);
		expect(contract).toBeGreaterThan(candidate);
		expect(output).toBeGreaterThan(contract);
		expect(last).toBeGreaterThan(output);
		expect(prompt.indexOf(PROPOSER_JSON_ONLY)).toBe(last);
	});
});

describe("runOnlineExplorationWithAgent", () => {
	it("with the async local proposer grows a tree byte-identical to the sync driver", async () => {
		const task: ScoredTask<unknown> = resolveTask({ task: "sum-difference" });
		const seed = 5;
		const base = {
			task,
			taskId: "sum-difference" as const,
			seed,
			clock: () => FIXED_CLOCK,
			workers: 4,
			k1: 12,
			policy: DEFAULT_POLICY,
			iteration: 0,
		};
		const dirSync = scratch("dream-sync-");
		const dirAsync = scratch("dream-async-");
		const sync = runOnlineExploration({ ...base, rng: createSeededRng(seed), dir: dirSync });
		const agentRun = await runOnlineExplorationWithAgent(
			{ ...base, rng: createSeededRng(seed), dir: dirAsync },
			asyncOf(createLocalProposer(task)),
		);
		expect(agentRun.treeId).toBe(sync.treeId);
		expect(agentRun.tokens).toBe(0);
		expect(agentRun.revealedCount).toBe(sync.revealedCount);
		expect(agentRun.bestNodeId).toBe(sync.bestNodeId);
		expect(readTreeFiles(dirAsync, agentRun.treeId)).toEqual(readTreeFiles(dirSync, sync.treeId));
	});

	it("with the LLM proposer grows a tree at token counts greater than zero from the stub", async () => {
		const task = resolveTask({ task: "sum-difference" });
		const stub = makeStub({
			proposerOutput: () => ({ output: JSON.stringify({ set: [0, 1, 3, 7, 12] }), tokens: 250 }),
		});
		const proposer = createLlmProposer(stub.handler, task, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
		});
		const explore = await runOnlineExplorationWithAgent(
			{
				task,
				taskId: "sum-difference",
				seed: 3,
				rng: createSeededRng(3),
				clock: () => FIXED_CLOCK,
				workers: 3,
				k1: 6,
				dir: dreamDir,
				policy: DEFAULT_POLICY,
				iteration: 0,
			},
			proposer,
		);
		expect(explore.revealedCount).toBeGreaterThan(0);
		expect(explore.tokens).toBeGreaterThan(0);
		expect(explore.tokens).toBe(stub.totalTokens());
		expect(explore.tokens).toBe(explore.revealedCount * 250);
	});
});

/** The policy ids of a dreamed candidate list. */
function idsOf(candidates: readonly { policy: ExplorationPolicy }[]): string[] {
	return candidates.map((candidate) => policyId(candidate.policy));
}

describe("proposePoliciesWithAgent (LLM dreamer)", () => {
	it("keeps the in-bounds candidates with their drop reasons, tops the budget up locally and reports a mixed dreamer", async () => {
		const candidatesJson = JSON.stringify([
			policy({ stopRule: "never" }), // valid
			policy({ branchWidth: 999 }), // out of range
			{ ...DEFAULT_POLICY, sneaky: "code" }, // unknown key
			policy({ selectionRule: "nope" as ExplorationPolicy["selectionRule"] }), // non-literal rule
		]);
		const stub = makeStub({ dreamerOutput: () => ({ output: candidatesJson, tokens: 500 }) });
		const { value: dreamed, spans } = await captureSpans(() =>
			proposePoliciesWithAgent(stub.handler, DEFAULT_POLICY, 4, {
				scope: SCOPE,
				signal: liveController().signal,
				tokenBudget: 200_000,
				localFallbackRng: createSeededRng(1),
				iteration: 2,
			}),
		);
		expect(stub.calls()).toBe(1);
		expect(dreamed.tokens).toBe(500);
		expect(dreamed.returned).toBe(4);
		expect(dreamed.dropped.map((entry) => entry.index)).toEqual([1, 2, 3]);
		expect(dreamed.dropped[0]!.reason).toMatch(/branchWidth must be within/);
		expect(dreamed.dropped[1]!.reason).toMatch(/unknown policy field: sneaky/);
		expect(dreamed.dropped[2]!.reason).toMatch(/selectionRule must be one of/);
		expect(dreamed.kept).toBe(1);
		expect(dreamed.truncated).toBe(0);
		expect(dreamed.local).toBe(3);
		expect(dreamed.llmFallback).toBe(false);
		expect(dreamed.dreamer).toBe("mixed");
		// The child's survivor first, then the local top-up on the fallback fork (`cand:0..2`).
		expect(dreamed.candidates).toHaveLength(4);
		expect(dreamed.candidates[0]).toEqual({ policy: policy({ stopRule: "never" }), origin: "llm" });
		expect(dreamed.candidates.slice(1).every((candidate) => candidate.origin === "local")).toBe(true);
		expect(idsOf(dreamed.candidates.slice(1))).toEqual(
			proposePolicies(DEFAULT_POLICY, 3, createSeededRng(1)).map(policyId),
		);
		const span = spans.find((record) => record.name === "dream.llm_dream")!;
		expect(span.attrs).toMatchObject({
			"dream.iteration": 2,
			"dream.candidates_requested": 4,
			"dream.candidates_returned": 4,
			"dream.candidates_dropped": 3,
			"dream.candidates_kept": 1,
			"dream.candidates_truncated": 0,
			"dream.candidates_local": 3,
			"dream.candidates": 4,
			"dream.dreamer": "mixed",
			"dream.llm_fallback": false,
			"dream.llm_status": "completed",
			"dream.llm_attempts": 1,
			"dream.tokens": 500,
		});
		expect(span.attrs["dream.llm_reject_reason"]).toBeUndefined();
	});

	it("falls back to the local mutator when every candidate is dropped (after one retry) or the call fails, logging role dreamer", async () => {
		const allJunk = JSON.stringify([{ ...DEFAULT_POLICY, sneaky: 1 }, policy({ beta: -5 })]);
		const junkStub = makeStub({ dreamerOutput: () => ({ output: allJunk, tokens: 300 }) });
		const logPath = rejectionsPath(dreamDir, "dreamer-unit");
		const { value: dropped, spans } = await captureSpans(() =>
			proposePoliciesWithAgent(junkStub.handler, DEFAULT_POLICY, 4, {
				scope: SCOPE,
				signal: liveController().signal,
				tokenBudget: 200_000,
				localFallbackRng: createSeededRng(1),
				iteration: 3,
				rejections: new RejectionLog(logPath, () => FIXED_CLOCK),
			}),
		);
		// An all-dropped array is a `shape` rejection: retried once like the proposer's, then the whole set is local.
		expect(junkStub.calls()).toBe(2);
		expect(dropped.tokens).toBe(600);
		expect(dropped.returned).toBe(2);
		expect(dropped.dropped.map((entry) => entry.reason)).toEqual([
			expect.stringMatching(/unknown policy field: sneaky/),
			expect.stringMatching(/beta must be within/),
		]);
		expect(dropped.kept).toBe(0);
		expect(dropped.local).toBe(4);
		expect(dropped.llmFallback).toBe(true);
		expect(dropped.dreamer).toBe("local");
		expect(dropped.candidates).toHaveLength(4);
		expect(dropped.candidates.every((candidate) => candidate.origin === "local")).toBe(true);
		expect(idsOf(dropped.candidates)).toEqual(proposePolicies(DEFAULT_POLICY, 4, createSeededRng(1)).map(policyId));
		const span = spans.find((record) => record.name === "dream.llm_dream")!;
		expect(span.attrs).toMatchObject({
			"dream.iteration": 3,
			"dream.llm_fallback": true,
			"dream.llm_status": "completed",
			"dream.llm_reject_reason": "shape",
			"dream.llm_reject_excerpt": excerptOf(allJunk),
			"dream.llm_attempts": 2,
			"dream.candidates_returned": 2,
			"dream.candidates_dropped": 2,
			"dream.candidates_kept": 0,
			"dream.candidates_local": 4,
			"dream.dreamer": "local",
		});
		const logged = readRejections(logPath);
		expect(logged).toHaveLength(2);
		expect(
			logged.map((record) => [record.role, record.iteration, record.round, record.attempt, record.fellBack]),
		).toEqual([
			["dreamer", 3, 0, 1, false],
			["dreamer", 3, 0, 2, true],
		]);
		expect(logged.every((record) => record.reason === "shape" && record.status === "completed")).toBe(true);
		expect(logged[0]!.error).toMatch(
			/every entry was dropped: \[0\] unknown policy field: sneaky; \[1\] beta must be within/,
		);

		const failed = await proposePoliciesWithAgent(
			makeStub({ dreamerOutput: () => ({ status: "error", tokens: 20 }) }).handler,
			DEFAULT_POLICY,
			3,
			{ scope: SCOPE, signal: liveController().signal, tokenBudget: 200_000, localFallbackRng: createSeededRng(2) },
		);
		// One retry on error -> two calls -> summed tokens; still a full local fallback set.
		expect(failed.tokens).toBe(40);
		expect(failed.returned).toBe(0);
		expect(failed.llmFallback).toBe(true);
		expect(idsOf(failed.candidates)).toEqual(proposePolicies(DEFAULT_POLICY, 3, createSeededRng(2)).map(policyId));

		// A turn limit is terminal: one call, local set.
		const limited = makeStub({ dreamerOutput: () => ({ status: "turn_limit", tokens: 5 }) });
		const capped = await proposePoliciesWithAgent(limited.handler, DEFAULT_POLICY, 2, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
			localFallbackRng: createSeededRng(2),
		});
		expect(limited.calls()).toBe(1);
		expect(capped.local).toBe(2);
	});

	it("counts only distinct new policies toward m, passes identical and duplicate entries through and truncates the rest", async () => {
		const a = policy({ stopRule: "never" });
		const b = policy({ selectionRule: "round-robin" });
		const c = policy({ batchSize: 1 });
		const stub = makeStub({
			dreamerOutput: () => ({ output: JSON.stringify([a, DEFAULT_POLICY, a, b, c]), tokens: 9 }),
		});
		const dreamed = await proposePoliciesWithAgent(stub.handler, DEFAULT_POLICY, 2, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
			localFallbackRng: createSeededRng(1),
		});
		expect(dreamed.returned).toBe(5);
		expect(dreamed.dropped).toEqual([]);
		expect(dreamed.kept).toBe(2);
		expect(dreamed.truncated).toBe(1);
		expect(dreamed.local).toBe(0);
		expect(dreamed.dreamer).toBe("llm");
		expect(idsOf(dreamed.candidates)).toEqual([policyId(a), policyId(DEFAULT_POLICY), policyId(a), policyId(b)]);
		expect(dreamed.candidates.every((candidate) => candidate.origin === "llm")).toBe(true);
		// The selection labels the pass-throughs without simulating them again.
		const selection = runDreaming({
			current: DEFAULT_POLICY,
			pool: synthPool(),
			dreams: 2,
			k1: CFG.k1,
			k2: CFG.k2,
			rng: createSeededRng(1),
			objective: CFG.objective,
			proposeCandidates: () => dreamed.candidates,
		});
		expect(selection.candidates.map((verdict) => verdict.reason)).toEqual([
			expect.not.stringMatching(/identical|duplicate/),
			"identical",
			"duplicate",
			expect.not.stringMatching(/identical|duplicate/),
		]);
		expect(selection.candidates[2]!.duplicateOf).toBe(0);
		expect(selection.scoredCount).toBe(3);
		expect(selection.dreamer).toBe("llm");
	});

	it("makes no child call for m = 0 and surfaces an aborted child as DreamAbortError", async () => {
		const idle = makeStub({ dreamerOutput: () => ({ output: REVISED, tokens: 1 }) });
		const none = await proposePoliciesWithAgent(idle.handler, DEFAULT_POLICY, 0, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
			localFallbackRng: createSeededRng(1),
		});
		expect(idle.calls()).toBe(0);
		expect(none.candidates).toEqual([]);
		expect(none.tokens).toBe(0);
		expect(none.llmFallback).toBe(false);

		const aborting = makeStub({ dreamerOutput: () => ({ status: "aborted", tokens: 2 }) });
		const logPath = rejectionsPath(dreamDir, "dreamer-abort");
		await expect(
			proposePoliciesWithAgent(aborting.handler, DEFAULT_POLICY, 2, {
				scope: SCOPE,
				signal: liveController().signal,
				tokenBudget: 200_000,
				localFallbackRng: createSeededRng(1),
				rejections: new RejectionLog(logPath, () => FIXED_CLOCK),
			}),
		).rejects.toBeInstanceOf(DreamAbortError);
		expect(aborting.calls()).toBe(1);
		const logged = readRejections(logPath);
		expect(logged).toHaveLength(1);
		expect(logged[0]).toMatchObject({ role: "dreamer", reason: "aborted", status: "aborted", fellBack: false });
	});

	it("parseCandidateArray keeps every accepted entry and names the reason for every dropped one", () => {
		expect(parseCandidateArray("nope")).toEqual({ kept: [], dropped: [], returned: 0 });
		const parsed = parseCandidateArray([DEFAULT_POLICY, { ...DEFAULT_POLICY, beta: 1.5 }, 7]);
		expect(parsed.returned).toBe(3);
		expect(parsed.kept).toEqual([DEFAULT_POLICY]);
		expect(parsed.dropped).toEqual([
			{ index: 1, reason: expect.stringMatching(/beta must be an integer/) },
			{ index: 2, reason: expect.stringMatching(/policy must be a JSON object/) },
		]);
	});
});

describe("the dreamer prompt (buildDreamerInput / buildDreamPrompt)", () => {
	function growPool(seed: number): RecordedTree[] {
		const task = resolveTask({ task: "sum-difference" });
		for (const iteration of [0, 1]) {
			runOnlineExploration({
				task,
				taskId: "sum-difference",
				seed,
				rng: createSeededRng(seed).fork(`iter:${iteration}`),
				clock: () => FIXED_CLOCK,
				workers: 3,
				k1: 5,
				dir: dreamDir,
				policy: DEFAULT_POLICY,
				iteration,
			});
		}
		return listTrees(dreamDir).map((summary) => readTree(summary.treeId, dreamDir));
	}

	it("digests the pool as the current policy's per-tree replay under the selection's own scoring", () => {
		const pool = growPool(7);
		const cfg = { k1: 5, k2: 10, objective: CFG.objective };
		const input = buildDreamerInput(DEFAULT_POLICY, 4, {
			pool,
			iteration: 2,
			objective: cfg.objective,
			workers: 3,
			k1: 5,
			k2: 10,
		});
		expect(input.m).toBe(4);
		expect(input.iteration).toBe(2);
		expect(input.budget).toEqual({ workers: 3, k1: 5, k2: 10 });
		expect(input.pool.map((tree) => tree.treeId)).toEqual(pool.map((tree) => tree.header.treeId).sort());
		const score = scorePolicyOnPool(DEFAULT_POLICY, pool, cfg);
		const meanV = input.pool.reduce((sum, tree) => sum + tree.value, 0) / input.pool.length;
		expect(meanV).toBeCloseTo(score.value, 12);
		expect(input.pool.reduce((sum, tree) => sum + tree.N, 0) / input.pool.length).toBeCloseTo(score.N, 12);
		expect(input.history).toEqual([]);
		// Without a pool the digest is empty and the budget falls back to W 1.
		const bare = buildDreamerInput(DEFAULT_POLICY, 2, {});
		expect(bare.pool).toEqual([]);
		expect(bare.budget.workers).toBe(1);
		expect(bare.scale).toEqual({ scoreMin: 0, scoreMax: 0 });
	});

	it("states the objective, the selection rule, the replay mechanics, the field semantics, the pool and the history", () => {
		const pool = growPool(7);
		const earlier = policy({ selectionRule: "weighted" });
		const history = historyOf(1, [
			{
				index: 0,
				policyId: policyId(earlier),
				policy: earlier,
				origin: "llm",
				changed: ["selectionRule"],
				duplicateOf: null,
				value: 0.5,
				quality: 1,
				anytime: 0.9,
				cost: 0.5,
				roundsSaved: 0,
				N: 10,
				rounds: 5,
				outOfSupportCells: 0,
				inSupportMean: 1,
				inSupportMin: 1,
				eligible: true,
				reason: "tie",
			},
		]);
		const input = buildDreamerInput(DEFAULT_POLICY, 4, {
			pool,
			iteration: 2,
			objective: { beta1: 0.05, beta2: 0.1, beta3: 0.25 },
			workers: 3,
			k1: 5,
			k2: 10,
			history,
		});
		const prompt = buildDreamPrompt(input);
		const lines = prompt.split("\n");
		expect(lines[0]).toBe(DREAMER_PROMPT_HEADER);
		expect(prompt).toContain("Propose up to 4 revised exploration policies");
		expect(prompt).toContain("dreaming step 2");
		// The V formula, its constants and the selection rule in one paragraph.
		expect(prompt).toContain(
			"V = (1 - beta3) * q + beta3 * anytime - beta1 * S / (W * k1) + beta2 * (1 - rounds / k1)",
		);
		expect(prompt).toContain("beta1 = 0.05, beta2 = 0.1, beta3 = 0.25");
		expect(prompt).toContain("W = 3 cells per round, online round cap k1 = 5, replay round cap k2 = 10");
		expect(prompt).toContain("the current policy wins every tie, so only a STRICTLY higher mean V");
		expect(prompt).toContain("mean q is below the current policy's is excluded");
		// Replay mechanics in two sentences.
		expect(prompt).toContain("Replay mechanics: a candidate re-walks each recorded tree");
		expect(prompt).toContain("out of support: it reveals nothing but is charged as a probe");
		// Field semantics, the replay-dead fields and the batchSize cap.
		expect(prompt).toContain(`${REPLAY_DEAD_FIELDS.join(", ")} are never read by replay`);
		expect(prompt).toContain("capped at W = 3 at runtime");
		expect(prompt).toContain("beta is read only under patience and fixed-rounds");
		expect(prompt).toContain("patience stops after beta consecutive rounds without improving");
		// The pool digest names every tree and the value to beat.
		for (const tree of pool) expect(prompt).toContain(`- ${tree.header.treeId}: N `);
		expect(prompt).toContain("mean V ");
		expect(prompt).toContain("is the value to beat");
		// The history and the count contract; the JSON-only instruction is last.
		expect(prompt).toContain("Earlier candidates and their verdicts");
		expect(prompt).toContain(
			`- iteration 1: ${policyId(earlier)} (llm; changed selectionRule) -> V 0.500000, q 1: tie`,
		);
		expect(prompt).toContain("Return at most 4 policy objects that are pairwise distinct");
		expect(lines.at(-1)).toMatch(/Return exactly one JSON array and nothing else/);
		const schema = prompt.indexOf("Named-rule fields");
		const objective = prompt.indexOf("How a candidate is judged");
		const semantics = prompt.indexOf("Field semantics");
		const current = prompt.indexOf("Current policy:");
		const poolBlock = prompt.indexOf("Current policy on the pool");
		const historyBlock = prompt.indexOf("Earlier candidates");
		expect(schema).toBeGreaterThan(0);
		expect(objective).toBeGreaterThan(schema);
		expect(semantics).toBeGreaterThan(objective);
		expect(current).toBeGreaterThan(semantics);
		expect(poolBlock).toBeGreaterThan(current);
		expect(historyBlock).toBeGreaterThan(poolBlock);
		// No history block and an empty pool line when there is nothing to show.
		const empty = buildDreamPrompt(buildDreamerInput(DEFAULT_POLICY, 1, {}));
		expect(empty).not.toContain("Earlier candidates");
		expect(empty).toContain("no recorded trees yet");
		expect(empty.split("\n")[0]).toBe(DREAMER_PROMPT_HEADER);
	});
});

describe("soundness: a bad LLM policy can never be deployed", () => {
	it("keeps the current policy when the dreamer returns a strictly worse one", async () => {
		const dreamed = await proposePoliciesWithAgent(
			makeStub({ dreamerOutput: () => ({ output: JSON.stringify([WORSE]), tokens: 111 }) }).handler,
			CURRENT,
			1,
			{ scope: SCOPE, signal: liveController().signal, tokenBudget: 200_000, localFallbackRng: createSeededRng(1) },
		);
		expect(idsOf(dreamed.candidates)).toEqual([policyId(WORSE)]);
		expect(dreamed.dreamer).toBe("llm");
		const selection = runDreaming({
			current: CURRENT,
			pool: synthPool(),
			dreams: 1,
			k1: CFG.k1,
			k2: CFG.k2,
			rng: createSeededRng(1),
			objective: CFG.objective,
			proposeCandidates: () => dreamed.candidates,
		});
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicyId).toBe(policyId(CURRENT));
		expect(selection.chosenScore).toBeGreaterThanOrEqual(selection.currentScore);
		expect(selection.chosenQuality).toBeGreaterThanOrEqual(selection.currentQuality);
	});

	it("keeps the current policy when the dreamer returns one that collapses exploration to a lower best", async () => {
		// On the synthetic tree a one-round policy reveals only the first child (0.5) while the current
		// policy reaches 0.9; the quality guard rejects it before V is even compared.
		const collapsing = policy({ selectionRule: "explore-root", stopRule: "fixed-rounds", beta: 1, batchSize: 1 });
		const exploring = policy({ selectionRule: "explore-root", stopRule: "never", batchSize: 1 });
		const dreamed = await proposePoliciesWithAgent(
			makeStub({ dreamerOutput: () => ({ output: JSON.stringify([collapsing]), tokens: 5 }) }).handler,
			exploring,
			1,
			{ scope: SCOPE, signal: liveController().signal, tokenBudget: 200_000, localFallbackRng: createSeededRng(1) },
		);
		expect(idsOf(dreamed.candidates)).toEqual([policyId(collapsing)]);
		const selection = runDreaming({
			current: exploring,
			pool: synthPool(),
			dreams: 1,
			k1: CFG.k1,
			k2: CFG.k2,
			rng: createSeededRng(1),
			// A pathological beta1 that would make the cheaper policy win on V alone.
			objective: { beta1: 5, beta2: 0, beta3: 0 },
			proposeCandidates: () => dreamed.candidates,
		});
		expect(selection.chosenPolicyId).toBe(policyId(exploring));
		expect(selection.improved).toBe(false);
		expect(selection.qualityRejected).toBe(1);
		expect(selection.currentQuality).toBe(1);
	});

	it("drops a policy carrying an extra key on parse so it never reaches candidates", async () => {
		const dreamed = await proposePoliciesWithAgent(
			makeStub({
				dreamerOutput: () => ({ output: JSON.stringify([{ ...CURRENT, exfiltrate: "rm -rf" }]), tokens: 9 }),
			}).handler,
			CURRENT,
			1,
			{ scope: SCOPE, signal: liveController().signal, tokenBudget: 200_000, localFallbackRng: createSeededRng(3) },
		);
		// The malformed candidate is rejected on parse; the fallback set is local-only.
		expect(idsOf(dreamed.candidates)).toEqual(proposePolicies(CURRENT, 1, createSeededRng(3)).map(policyId));
		expect(dreamed.candidates.every((candidate) => candidate.origin === "local")).toBe(true);
		expect(dreamed.dreamer).toBe("local");
		expect(dreamed.dropped[0]!.reason).toMatch(/unknown policy field: exfiltrate/);
	});
});

describe("runDreamLoopWithAgent (end to end, stub handler)", () => {
	function loopOptions(over: Partial<DreamLoopWithAgentOptions>): DreamLoopWithAgentOptions {
		return {
			runAgent: makeStub({}).handler,
			task: resolveTask({ task: "sum-difference" }),
			taskId: "sum-difference",
			seed: 7,
			clock: () => FIXED_CLOCK,
			workers: 3,
			k1: 5,
			k2: 10,
			dreams: 4,
			iterations: 2,
			dir: dreamDir,
			useLlmProposer: true,
			useLlmDreamer: true,
			scope: SCOPE,
			signal: liveController().signal,
			childTokenBudget: 500_000,
			...over,
		};
	}

	it("runs the whole loop, never regresses the policy, and sums the stub's tokens", async () => {
		const stub = makeStub({
			proposerOutput: () => ({ output: JSON.stringify({ set: [0, 1, 2, 4, 9] }), tokens: 100 }),
			dreamerOutput: () => ({ output: JSON.stringify([policy({ stopRule: "never" })]), tokens: 200 }),
		});
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let loopResult: Awaited<ReturnType<typeof runDreamLoopWithAgent>>;
		try {
			loopResult = await withSpan("test.turn", {}, () =>
				runDreamLoopWithAgent(loopOptions({ runAgent: stub.handler })),
			);
		} finally {
			unsubscribe();
		}
		expect(loopResult.mode).toBe("llm");
		expect(loopResult.treeIds).toHaveLength(3); // iteration 0 + 2 redeploys
		expect(loopResult.finalPolicyScore).toBeGreaterThanOrEqual(loopResult.initialPolicyScore);
		expect(loopResult.tokens).toBe(stub.totalTokens());
		expect(loopResult.tokens).toBeGreaterThan(0);

		// dream.run is a DETACHED ROOT of a fresh trace carrying the trigger.
		const turn = spans.find((span) => span.name === "test.turn");
		const run = spans.find((span) => span.name === "dream.run");
		expect(turn && run).toBeTruthy();
		expect(run!.parentSpanId).toBeUndefined();
		expect(run!.traceId).not.toBe(turn!.traceId);
		expect(run!.attrs["trigger.trace_id"]).toBe(turn!.traceId);
		expect(run!.attrs["dream.mode"]).toBe("llm");

		// Every span in the dream.run trace ended and links to a parent inside the trace.
		const runTrace = spans.filter((span) => span.traceId === run!.traceId);
		const ids = new Set(runTrace.map((span) => span.spanId));
		for (const span of runTrace) {
			if (span.name === "dream.run") continue;
			expect(span.parentSpanId, `${span.name} must have a parent`).toBeDefined();
			expect(ids.has(span.parentSpanId!), `${span.name} parent must be in the trace`).toBe(true);
		}
		// The LLM path opened both new span families.
		expect(runTrace.some((span) => span.name === "dream.llm_propose")).toBe(true);
		expect(runTrace.some((span) => span.name === "dream.llm_dream")).toBe(true);
	});

	it("runs each toggle independently and deterministically with the stub", async () => {
		const proposerOnly = (dir: string) =>
			runDreamLoopWithAgent(
				loopOptions({
					dir,
					useLlmProposer: true,
					useLlmDreamer: false,
					runAgent: makeStub({
						proposerOutput: () => ({ output: JSON.stringify({ set: [0, 1, 2, 4, 9] }), tokens: 100 }),
					}).handler,
				}),
			);
		const a = await proposerOnly(scratch("dream-prop-a-"));
		const b = await proposerOnly(scratch("dream-prop-b-"));
		expect(b.finalPolicyId).toBe(a.finalPolicyId);
		expect(b.treeIds).toEqual(a.treeIds);
		expect(b.tokens).toBe(a.tokens);
		expect(a.tokens).toBeGreaterThan(0);

		const dreamerOnly = (dir: string) =>
			runDreamLoopWithAgent(
				loopOptions({
					dir,
					useLlmProposer: false,
					useLlmDreamer: true,
					runAgent: makeStub({
						dreamerOutput: () => ({ output: JSON.stringify([policy({ stopRule: "never" })]), tokens: 200 }),
					}).handler,
				}),
			);
		const c = await dreamerOnly(scratch("dream-dream-a-"));
		const d = await dreamerOnly(scratch("dream-dream-b-"));
		expect(d.finalPolicyId).toBe(c.finalPolicyId);
		expect(d.treeIds).toEqual(c.treeIds);
		expect(d.tokens).toBe(c.tokens);
		// Local proposer: the rollouts spend nothing; only the dreamer's child tokens count.
		expect(c.tokens).toBe(200 * c.iterations);
	});

	it("grows the same trees and round table for one seed under two clocks on the local-proposer path", async () => {
		const run = (dir: string, clockMs: number) =>
			runDreamLoopWithAgent(
				loopOptions({
					dir,
					clock: () => clockMs,
					useLlmProposer: false,
					useLlmDreamer: false,
					runAgent: makeStub({}).handler,
				}),
			);
		const a = await run(scratch("dream-clock-a-"), 1_789_842_143_996);
		const b = await run(scratch("dream-clock-b-"), 1);
		expect(b.treeIds).not.toEqual(a.treeIds);
		const clockFree = (result: typeof a) => result.rounds.map(({ treeId: _treeId, ...rest }) => rest);
		expect(clockFree(b)).toEqual(clockFree(a));
		expect(b.finalPolicyId).toBe(a.finalPolicyId);
		expect(b.finalPolicyScore).toBe(a.finalPolicyScore);
		expect(b.bestNodeScore).toBe(a.bestNodeScore);
	});

	it("stops promptly and records the abort when a child returns aborted", async () => {
		const stub = makeStub({ proposerOutput: () => ({ status: "aborted", tokens: 10 }) });
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let caught: unknown;
		try {
			await withSpan("test.turn", {}, () => runDreamLoopWithAgent(loopOptions({ runAgent: stub.handler })));
		} catch (error) {
			caught = error;
		} finally {
			unsubscribe();
		}
		expect(isDreamAbortError(caught)).toBe(true);
		const run = spans.find((span) => span.name === "dream.run");
		expect(run).toBeDefined();
		expect(run!.attrs["dream.stopped"]).toBe("aborted");
		// No tree was fully written; the run did not silently finish.
		expect(spans.some((span) => span.name === "dream.llm_propose")).toBe(true);
	});

	it("stops promptly on a pre-aborted signal even with the local proposer", async () => {
		const controller = liveController();
		controller.abort();
		const spans: SpanEndRecord[] = [];
		const unsubscribe = addSpanSink((record) => spans.push(record));
		let caught: unknown;
		try {
			await withSpan("test.turn", {}, () =>
				runDreamLoopWithAgent(
					loopOptions({ useLlmProposer: false, useLlmDreamer: false, signal: controller.signal }),
				),
			);
		} catch (error) {
			caught = error;
		} finally {
			unsubscribe();
		}
		expect(isDreamAbortError(caught)).toBe(true);
		const run = spans.find((span) => span.name === "dream.run");
		expect(run!.attrs["dream.stopped"]).toBe("aborted");
		// The local proposer never ran: no rollout tree was written.
		expect(existsSync(join(dreamDir, "trees"))).toBe(false);
	});
});

const ARTIFACT = JSON.stringify({ set: [0, 1, 2, 4, 9] });
const REVISED = JSON.stringify([policy({ stopRule: "never" })]);
const GUIDANCE_PREFIX = "Directional insights from prior trajectories";

function sumRounds(
	rounds: readonly {
		tokens: { rollout: number; dreamer: number; guidance: number };
		handlerCalls: DreamHandlerCalls;
	}[],
): { tokens: number; calls: DreamHandlerCalls } {
	const calls: DreamHandlerCalls = { proposer: 0, dreamer: 0, guidance: 0 };
	let tokens = 0;
	for (const round of rounds) {
		tokens += round.tokens.rollout + round.tokens.dreamer + round.tokens.guidance;
		calls.proposer += round.handlerCalls.proposer;
		calls.dreamer += round.handlerCalls.dreamer;
		calls.guidance += round.handlerCalls.guidance;
	}
	return { tokens, calls };
}

async function captureSpans<T>(run: () => Promise<T>): Promise<{ value: T; spans: SpanEndRecord[] }> {
	const spans: SpanEndRecord[] = [];
	const unsubscribe = addSpanSink((record) => spans.push(record));
	try {
		return { value: await run(), spans };
	} finally {
		unsubscribe();
	}
}

function agentOptions(over: Partial<DreamLoopWithAgentOptions>): DreamLoopWithAgentOptions {
	return {
		runAgent: makeStub({}).handler,
		task: resolveTask({ task: "sum-difference" }),
		taskId: "sum-difference",
		seed: 7,
		clock: () => FIXED_CLOCK,
		workers: 3,
		k1: 5,
		k2: 10,
		dreams: 4,
		iterations: 2,
		dir: dreamDir,
		useLlmProposer: true,
		useLlmDreamer: true,
		scope: SCOPE,
		signal: liveController().signal,
		childTokenBudget: 500_000,
		...over,
	};
}

describe("runDreamLoopWithAgent: fixed policy, per-round records, shared round 1", () => {
	it("fixedPolicy never dreams: no dreamer call, no dreaming spans or events, the initial policy every round", async () => {
		const stub = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 10 }),
			dreamerOutput: () => ({ output: REVISED }),
		});
		const events: DreamProgressEvent[] = [];
		const { value: run, spans } = await captureSpans(() =>
			withSpan("test.turn", {}, () =>
				runDreamLoopWithAgent(
					agentOptions({ runAgent: stub.handler, fixedPolicy: true, onProgress: (event) => events.push(event) }),
				),
			),
		);
		expect(run.fixedPolicy).toBe(true);
		expect(run.iterations).toBe(2);
		expect(run.rounds).toHaveLength(3);
		expect(run.treeIds).toHaveLength(3);
		expect(run.finalPolicyId).toBe(run.initialPolicyId);
		expect(run.finalPolicyId).toBe(policyId(DEFAULT_POLICY));
		expect(run.improved).toBe(false);
		expect(run.finalPolicyScore).toBe(run.initialPolicyScore);
		expect(run.rounds.every((round) => round.dreaming === null)).toBe(true);
		expect(run.rounds.every((round) => round.policyId === run.initialPolicyId)).toBe(true);
		expect(run.rounds.map((round) => round.iteration)).toEqual([0, 1, 2]);
		expect(run.rounds.map((round) => round.poolSize)).toEqual([0, 1, 2]);
		expect(stub.roleCalls.dreamer).toBe(0);
		expect(stub.roleCalls.guidance).toBe(0);
		expect(run.rounds.every((round) => round.handlerCalls.dreamer === 0 && round.tokens.dreamer === 0)).toBe(true);
		expect(events.some((event) => event.type === "phase" && event.phase === "dreaming")).toBe(false);
		expect(events.filter((event) => event.type === "phase" && event.phase === "redeploying")).toHaveLength(2);

		const dreamRun = spans.find((span) => span.name === "dream.run")!;
		expect(dreamRun.attrs["dream.fixed_policy"]).toBe(true);
		const trace = spans.filter((span) => span.traceId === dreamRun.traceId);
		expect(trace.some((span) => span.name === "dream.llm_dream")).toBe(false);
		expect(trace.some((span) => span.name === "dream.dream")).toBe(false);
		expect(trace.some((span) => span.name === "dream.replay")).toBe(false);
		const redeploys = trace.filter((span) => span.name === "dream.redeploy");
		expect(redeploys).toHaveLength(2);
		expect(redeploys.every((span) => span.attrs["dream.fixed_policy"] === true)).toBe(true);
		expect(redeploys.every((span) => span.attrs["dream.policy_id"] === run.initialPolicyId)).toBe(true);
	});

	it("shares iteration 0 byte for byte between a fixed and a dreaming run and records dreaming only where it ran", async () => {
		const proposerOutput = () => ({ output: ARTIFACT, tokens: 10 });
		const fixedDir = scratch("dream-fixed-");
		const dreamingDir = scratch("dream-dreaming-");
		const fixed = await runDreamLoopWithAgent(
			agentOptions({ dir: fixedDir, fixedPolicy: true, runAgent: makeStub({ proposerOutput }).handler }),
		);
		const dreaming = await runDreamLoopWithAgent(
			agentOptions({
				dir: dreamingDir,
				runAgent: makeStub({ proposerOutput, dreamerOutput: () => ({ output: REVISED }) }).handler,
			}),
		);
		expect(dreaming.fixedPolicy).toBe(false);
		expect(dreaming.treeIds).toEqual(fixed.treeIds);
		expect(readTreeFiles(dreamingDir, dreaming.treeIds[0]!)).toEqual(readTreeFiles(fixedDir, fixed.treeIds[0]!));
		expect(dreaming.rounds[0]).toEqual(fixed.rounds[0]);
		expect(dreaming.rounds[0]!.dreaming).toBeNull();
		expect(dreaming.rounds[1]!.dreaming).not.toBeNull();
		expect(dreaming.rounds[1]!.dreaming!.candidates).toBeGreaterThan(0);
		expect(dreaming.rounds[1]!.handlerCalls.dreamer).toBe(1);
		expect(dreaming.rounds[1]!.tokens.dreamer).toBe(100);
		expect(fixed.rounds[1]!.handlerCalls.dreamer).toBe(0);
	});

	it("honours initialPolicy on every tree header and sums per-role tokens and calls to the run totals", async () => {
		const custom = policy({ batchSize: 2, beta: 3, stopRule: "fixed-rounds" });
		const stub = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 7 }),
			dreamerOutput: () => ({ output: REVISED, tokens: 300 }),
		});
		const run = await runDreamLoopWithAgent(agentOptions({ runAgent: stub.handler, initialPolicy: custom }));
		expect(run.initialPolicyId).toBe(policyId(custom));
		expect(run.rounds[0]!.policyId).toBe(policyId(custom));
		expect(listTrees(dreamDir).map((tree) => tree.treeId)).toEqual([...run.treeIds].sort());
		expect(listTrees(dreamDir).find((tree) => tree.iteration === 0)!.policyId).toBe(policyId(custom));
		for (const [index, tree] of listTrees(dreamDir).entries()) {
			const round = run.rounds.find((candidate) => candidate.treeId === tree.treeId)!;
			expect(round, `round for tree ${index}`).toBeDefined();
			expect(round.probes).toBe(tree.nodeCount - 1);
			expect(round.roundBest).toBe(tree.bestScore);
			expect(round.policyId).toBe(tree.policyId);
		}
		const totals = sumRounds(run.rounds);
		expect(totals.tokens).toBe(run.tokens);
		expect(run.tokens).toBe(stub.totalTokens());
		expect(totals.calls).toEqual(stub.roleCalls);
		expect(run.rounds.map((round) => round.tokens.dreamer)).toEqual([0, 300, 300]);
		expect(run.rounds.every((round) => round.tokens.rollout === round.probes * 7)).toBe(true);
		expect(run.rounds.every((round) => round.tokens.guidance === 0 && round.handlerCalls.guidance === 0)).toBe(true);
	});

	it("records the local path with zero calls and tokens, matching the persisted trees", async () => {
		const stub = makeStub({});
		const run = await runDreamLoopWithAgent(
			agentOptions({ runAgent: stub.handler, useLlmProposer: false, useLlmDreamer: false }),
		);
		expect(stub.calls()).toBe(0);
		expect(run.fixedPolicy).toBe(false);
		expect(run.rounds.map((round) => round.treeId)).toEqual(run.treeIds);
		expect(
			run.rounds.every((round) => round.tokens.rollout + round.tokens.dreamer + round.tokens.guidance === 0),
		).toBe(true);
		expect(
			run.rounds.every(
				(round) => round.handlerCalls.proposer + round.handlerCalls.dreamer + round.handlerCalls.guidance === 0,
			),
		).toBe(true);
		expect(run.rounds[0]!.dreaming).toBeNull();
		expect(run.rounds[1]!.dreaming!.candidates).toBe(4);
		expect(run.rounds[2]!.dreaming!.candidates).toBe(4);
		expect(run.tokens).toBe(0);
		// Provenance on the local path: nothing agent-generated, an all-zero tally, no rejection log.
		expect(run.rounds.every((round) => round.agentGeneratedCalls === 0)).toBe(true);
		expect(run.rounds.every((round) => JSON.stringify(round.proposals) === JSON.stringify(zeroProposalTally()))).toBe(
			true,
		);
		expect(existsSync(join(dreamDir, "rejections"))).toBe(false);
	});

	it("counts every child result per round, keeps the tally identities, and logs rejections under the run key", async () => {
		// Alternate: accepted, rejected(shape) then rejected(shape) -> fallback, accepted, ...
		// so every rollout mixes agent-generated nodes with local fallbacks.
		const good = '{"n": 4, "weights": [3, 1, 1, 3]}';
		const bad = '{"n": 4, "weights": [1, 2]}';
		const stub = makeStub({
			proposerOutput: (call) => ({ output: call % 3 === 1 ? good : bad, tokens: 10 }),
		});
		const run = await runDreamLoopWithAgent(
			agentOptions({
				runAgent: stub.handler,
				task: n4Task(),
				taskId: "autocorrelation",
				n: 4,
				useLlmDreamer: false,
				proposerPromptContext: taskPromptContext("autocorrelation", 4),
			}),
		);
		expect(run.rounds).toHaveLength(3);
		let rejectedTotal = 0;
		for (const [index, round] of run.rounds.entries()) {
			const tally = round.proposals!;
			expect(round.agentGeneratedCalls, `round ${index} agent-generated`).toBe(tally.llmAccepted);
			expect(tally.llmProposals, `round ${index} proposals`).toBe(tally.llmAccepted + totalRejected(tally));
			expect(round.probes, `round ${index} probes`).toBe(tally.llmAccepted + tally.localFallbacks);
			expect(tally.llmProposals, `round ${index} handler calls`).toBe(round.handlerCalls.proposer);
			expect(tally.llmRejected.shape, `round ${index} shape`).toBe(totalRejected(tally));
			expect(tally.localFallbacks, `round ${index} fallbacks`).toBeGreaterThan(0);
			expect(tally.llmAccepted, `round ${index} accepted`).toBeGreaterThan(0);
			rejectedTotal += totalRejected(tally);
			// The persisted tree agrees: origin llm nodes are exactly the accepted results.
			const tree = readTree(round.treeId, dreamDir);
			const origins = tree.nodes.map((node) => node.origin);
			expect(origins.filter((origin) => origin === "llm")).toHaveLength(tally.llmAccepted);
			expect(origins.filter((origin) => origin === "local")).toHaveLength(tally.localFallbacks);
			expect(origins.filter((origin) => origin === "root")).toHaveLength(1);
		}
		const logged = readRejections(rejectionsPath(dreamDir, `autocorrelation-s7-r${FIXED_CLOCK}`));
		expect(logged).toHaveLength(rejectedTotal);
		expect(logged.filter((record) => record.fellBack)).toHaveLength(
			run.rounds.reduce((sum, round) => sum + round.proposals!.localFallbacks, 0),
		);
		expect(logged.every((record) => record.reason === "shape" && record.excerpt === bad)).toBe(true);
		expect([...new Set(logged.map((record) => record.iteration))].sort()).toEqual([0, 1, 2]);
		// The round table never counts a fallback as the agent's work.
		const agentGenerated = run.rounds.reduce((sum, round) => sum + (round.agentGeneratedCalls ?? 0), 0);
		const probes = run.rounds.reduce((sum, round) => sum + round.probes, 0);
		expect(agentGenerated).toBeLessThan(probes);
		expect(agentGenerated).toBeGreaterThan(0);
	});

	it("counts retried handler invocations: one failing then one completing child is two proposer calls for one probe", async () => {
		const stub = makeStub({
			proposerOutput: (call) =>
				call % 2 === 1 ? { status: "error", tokens: 3 } : { status: "completed", output: ARTIFACT, tokens: 5 },
		});
		const run = await runDreamLoopWithAgent(
			agentOptions({ runAgent: stub.handler, iterations: 0, k1: 1, workers: 1, useLlmDreamer: false }),
		);
		expect(run.rounds).toHaveLength(1);
		expect(run.rounds[0]!.probes).toBe(1);
		expect(run.rounds[0]!.handlerCalls).toEqual({ proposer: 2, dreamer: 0, guidance: 0 });
		expect(run.rounds[0]!.tokens.rollout).toBe(8);
		expect(run.tokens).toBe(8);
		expect(stub.roleCalls.proposer).toBe(2);
	});

	it("adopts a shared initialRollout: no round-1 rollout, its record copied, later rounds still counted", async () => {
		const proposerOutput = () => ({ output: ARTIFACT, tokens: 10 });
		const task = resolveTask({ task: "sum-difference" });
		const seedStub = makeStub({ proposerOutput });
		const seedTally = zeroProposalTally();
		const shared = await runOnlineExplorationWithAgent(
			{
				task,
				taskId: "sum-difference",
				seed: 7,
				rng: createSeededRng(7).fork("iter:0"),
				clock: () => FIXED_CLOCK,
				workers: 3,
				k1: 5,
				dir: dreamDir,
				policy: DEFAULT_POLICY,
				iteration: 0,
			},
			createLlmProposer(seedStub.handler, task, {
				scope: SCOPE,
				signal: liveController().signal,
				tokenBudget: 1,
				tally: seedTally,
			}),
		);
		const initialRollout = {
			treeId: shared.treeId,
			bestScore: shared.bestScore,
			revealedCount: shared.revealedCount,
			agentGeneratedCount: shared.agentGeneratedCount,
			proposals: seedTally,
			rounds: shared.rounds,
			tokens: shared.tokens,
			handlerCalls: { proposer: seedStub.roleCalls.proposer, dreamer: 0, guidance: 0 },
			probesToBest: shared.probesToBest,
			improvements: shared.improvements,
		};
		expect(shared.agentGeneratedCount).toBe(shared.revealedCount);
		expect(seedTally.llmAccepted).toBe(shared.revealedCount);
		const stub = makeStub({ proposerOutput, dreamerOutput: () => ({ output: REVISED }) });
		const events: DreamProgressEvent[] = [];
		const run = await runDreamLoopWithAgent(
			agentOptions({ runAgent: stub.handler, initialRollout, onProgress: (event) => events.push(event) }),
		);
		expect(run.treeIds[0]).toBe(shared.treeId);
		expect(run.treeIds).toHaveLength(3);
		expect(run.rounds[0]).toEqual({
			iteration: 0,
			treeId: shared.treeId,
			policyId: policyId(DEFAULT_POLICY),
			roundBest: shared.bestScore,
			probes: shared.revealedCount,
			agentGeneratedCalls: shared.revealedCount,
			proposals: seedTally,
			decisionRounds: shared.rounds,
			poolSize: 0,
			tokens: { rollout: shared.tokens, dreamer: 0, guidance: 0 },
			handlerCalls: initialRollout.handlerCalls,
			dreaming: null,
			probesToRoundBest: shared.probesToBest,
			improvements: shared.improvements,
		} satisfies DreamRoundRecord);
		expect(run.rounds[0]!.improvements!.at(-1)!.score).toBe(shared.bestScore);
		// A shared round 1 that ran to the cap is not a stopped-early rollout; the redeploys are counted.
		expect(run.stoppedEarly).toBe(run.rounds.filter((round) => round.decisionRounds < 5).length);
		// The copied tally is a snapshot: later rounds start from zero.
		expect(run.rounds[1]!.proposals!.llmAccepted).toBe(run.rounds[1]!.handlerCalls.proposer);
		expect(run.rounds[1]!.agentGeneratedCalls).toBe(run.rounds[1]!.probes);
		// The loop's own stub saw only iterations 1 and 2.
		expect(stub.roleCalls.proposer).toBe(run.rounds[1]!.handlerCalls.proposer + run.rounds[2]!.handlerCalls.proposer);
		expect(stub.roleCalls.proposer).toBeGreaterThan(0);
		expect(run.tokens).toBe(shared.tokens + stub.totalTokens());
		const first = events[0];
		expect(first?.type === "phase" && first.phase === "rollout" && first.treeId).toBe(shared.treeId);
		// The pool the dreaming step froze at iteration 1 held exactly the shared tree.
		expect(run.rounds[1]!.poolSize).toBe(1);
		expect(listTrees(dreamDir)).toHaveLength(3);

		// A shared rollout whose tree is not in the store is refused before any call.
		const missing = makeStub({ proposerOutput });
		await expect(
			runDreamLoopWithAgent(
				agentOptions({
					runAgent: missing.handler,
					dir: scratch("dream-missing-"),
					initialRollout: { ...initialRollout, treeId: "nope" },
				}),
			),
		).rejects.toBeInstanceOf(DreamStoreError);
		expect(missing.calls()).toBe(0);
	});
});

describe("runDreamLoopWithAgent: dreams log, verdicts, run id and priming", () => {
	it("fills the additive dreaming fields, writes the dreams log, stamps dream.iteration and emits candidate spans", async () => {
		const stub = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 10 }),
			dreamerOutput: () => ({ output: REVISED, tokens: 20 }),
		});
		const { value: run, spans } = await captureSpans(() =>
			runDreamLoopWithAgent(agentOptions({ runAgent: stub.handler })),
		);
		expect(run.runId).toBe(`sum-difference-s7-r${FIXED_CLOCK}`);
		expect(run.stoppedEarly).toBeDefined();
		expect(run.finalSelection).toHaveLength(run.iterations);
		for (const round of run.rounds.slice(1)) {
			const dreaming = round.dreaming!;
			// REVISED holds one policy and M is 4: the child's one plus three local top-ups.
			expect(dreaming.candidates).toBe(4);
			expect(dreaming.candidateVerdicts).toHaveLength(4);
			expect(dreaming.candidateVerdicts!.map((verdict) => verdict.origin)).toEqual([
				"llm",
				"local",
				"local",
				"local",
			]);
			expect(dreaming.candidateVerdicts![0]!.policyId).toBe(policyId(policy({ stopRule: "never" })));
			expect(dreaming.dreamer).toBe("mixed");
			expect(dreaming.leverScan).not.toBeNull();
			expect(dreaming.leverScan!.policies).toBeGreaterThan(1);
			expect(round.probesToRoundBest).toBeLessThanOrEqual(round.probes);
			expect(round.improvements!.at(-1)!.score).toBe(round.roundBest);
		}
		// The dreams log under the run key: candidate lines then a step line per step, the final selection as -1.
		const lines = readDreamsLog(dreamsPath(dreamDir, run.runId));
		const steps = lines.filter((line): line is DreamStepLine => line.type === "step");
		expect(steps.map((line) => line.iteration)).toEqual([1, 2, -1]);
		expect(steps.map((line) => line.dreamer)).toEqual(["mixed", "mixed", "local"]);
		expect(steps[0]!.poolSize).toBe(1);
		expect(steps[2]!.poolSize).toBe(3);
		expect(steps[2]!.leverScan).toBeNull();
		const candidates = lines.filter((line): line is DreamCandidateLine => line.type === "candidate");
		expect(candidates.filter((line) => line.iteration === 1)).toHaveLength(4);
		expect(candidates.filter((line) => line.iteration === -1)).toHaveLength(run.iterations);
		expect(candidates.every((line) => line.ts === FIXED_CLOCK && !("experimentId" in line))).toBe(true);
		expect(candidates.filter((line) => line.iteration === 1).map((line) => line.policyId)).toEqual(
			run.rounds[1]!.dreaming!.candidateVerdicts!.map((verdict) => verdict.policyId),
		);
		// Spans: dream.iteration on dream.llm_dream, dream.dream and dream.replay; one dream.candidate per verdict.
		const dreamRun = spans.find((span) => span.name === "dream.run")!;
		expect(dreamRun.attrs["dream.run_id"]).toBe(run.runId);
		expect(dreamRun.attrs["dream.priming_policies"]).toBe(0);
		const trace = spans.filter((span) => span.traceId === dreamRun.traceId);
		const byName = (name: string) => trace.filter((span) => span.name === name);
		expect(byName("dream.llm_dream").map((span) => span.attrs["dream.iteration"])).toEqual([1, 2]);
		expect(byName("dream.dream").map((span) => span.attrs["dream.iteration"])).toEqual([1, 2]);
		expect(byName("dream.replay").map((span) => span.attrs["dream.iteration"])).toEqual([1, 2]);
		expect(byName("dream.dream").every((span) => typeof span.attrs["dream.lever_gap"] === "number")).toBe(true);
		const candidateSpans = byName("dream.candidate");
		expect(candidateSpans).toHaveLength(8);
		const dreamSpanIds = new Set(byName("dream.dream").map((span) => span.spanId));
		expect(candidateSpans.every((span) => dreamSpanIds.has(span.parentSpanId!))).toBe(true);
		expect(candidateSpans.filter((span) => span.attrs["dream.iteration"] === 1)).toHaveLength(4);
		expect(candidateSpans.map((span) => span.attrs["dream.origin"])).toEqual([
			"llm",
			"local",
			"local",
			"local",
			"llm",
			"local",
			"local",
			"local",
		]);
		// The dreamer prompt carries the pool and, from the second step, the earlier verdicts: never byte-identical twice.
		expect(stub.prompts.dreamer).toHaveLength(2);
		expect(stub.prompts.dreamer[0]).not.toBe(stub.prompts.dreamer[1]);
		expect(stub.prompts.dreamer[0]).not.toContain("Earlier candidates");
		expect(stub.prompts.dreamer[1]).toContain("Earlier candidates");
		expect(stub.prompts.dreamer[1]).toContain(`- iteration 1: ${policyId(policy({ stopRule: "never" }))} (llm;`);
		expect(stub.prompts.dreamer[0]).toContain(`- ${run.treeIds[0]}: N `);
		expect(stub.prompts.dreamer[1]).toContain(`- ${run.treeIds[1]}: N `);
	});

	it("folds runLabel into the run id and the log keys, and puts the child scope on dream.run", async () => {
		const bad = '{"n": 4, "weights": [1, 2]}';
		const stub = makeStub({
			proposerOutput: (call) => ({ output: call % 2 === 1 ? '{"n": 4, "weights": [3, 1, 1, 3]}' : bad, tokens: 1 }),
			dreamerOutput: () => ({ output: "no array here", tokens: 2 }),
		});
		const { value: run, spans } = await captureSpans(() =>
			runDreamLoopWithAgent(
				agentOptions({
					runAgent: stub.handler,
					task: n4Task(),
					taskId: "autocorrelation",
					n: 4,
					iterations: 1,
					runLabel: "exp 1/dream",
					dreamsLogContext: { experimentId: "exp 1", arm: "dream" },
					scope: { ...SCOPE, model: "faux/child", thinkingLevel: "off", maxOutputTokens: 4096 },
				}),
			),
		);
		const runId = dreamRunId("autocorrelation", 7, FIXED_CLOCK, "exp 1/dream");
		expect(run.runId).toBe(runId);
		expect(runId).toBe(`autocorrelation-s7-r${FIXED_CLOCK}-exp_1_dream`);
		// Proposer and dreamer rejections share the run's rejection log; the dreamer's carry its role.
		const rejections = readRejections(rejectionsPath(dreamDir, runId));
		expect(rejections.some((record) => record.role === undefined && record.reason === "shape")).toBe(true);
		const dreamerLines = rejections.filter((record) => record.role === "dreamer");
		expect(dreamerLines.map((record) => [record.iteration, record.attempt, record.reason, record.fellBack])).toEqual([
			[1, 1, "parse", false],
			[1, 2, "parse", true],
		]);
		expect(run.rounds[1]!.dreaming!.dreamer).toBe("local");
		expect(run.rounds[1]!.dreaming!.candidateVerdicts!.every((verdict) => verdict.origin === "local")).toBe(true);
		// The dreams log lives under the same key and every line names the experiment and arm.
		const lines = readDreamsLog(dreamsPath(dreamDir, runId));
		expect(lines.length).toBeGreaterThan(0);
		expect(lines.every((line) => line.experimentId === "exp 1" && line.arm === "dream")).toBe(true);
		const dreamRun = spans.find((span) => span.name === "dream.run")!;
		expect(dreamRun.attrs["dream.run_id"]).toBe(runId);
		expect(dreamRun.attrs["dream.child_model"]).toBe("faux/child");
		expect(dreamRun.attrs["dream.child_thinking"]).toBe("off");
		expect(dreamRun.attrs["dream.child_max_output_tokens"]).toBe(4096);
		const llmDream = spans.find((span) => span.name === "dream.llm_dream")!;
		expect(llmDream.attrs["dream.llm_fallback"]).toBe(true);
		expect(llmDream.attrs["dream.llm_reject_reason"]).toBe("parse");
		expect(llmDream.attrs["dream.llm_status"]).toBe("completed");
		expect(llmDream.attrs["dream.llm_reject_excerpt"]).toBe("no array here");
	});

	it("rolls priming policies out at iteration 0 exactly like the sync loop and charges them to round 1", async () => {
		const syncDir = scratch("dream-prime-sync-");
		const task = resolveTask({ task: "sum-difference" });
		const base = {
			task,
			taskId: "sum-difference" as const,
			seed: 7,
			clock: () => FIXED_CLOCK,
			workers: 3,
			k1: 5,
			k2: 10,
			dreams: 4,
			iterations: 2,
			primingPolicies: PRIMING_DIVERSE,
		};
		const expected = runDreamLoop({ ...base, dir: syncDir });
		const { value: run, spans } = await captureSpans(() =>
			runDreamLoopWithAgent(
				agentOptions({
					...base,
					dir: dreamDir,
					runAgent: makeStub({}).handler,
					useLlmProposer: false,
					useLlmDreamer: false,
				}),
			),
		);
		expect(run.runId).toBe(expected.runId);
		expect(run.treeIds).toEqual(expected.treeIds);
		expect(JSON.stringify(run.rounds)).toBe(JSON.stringify(expected.rounds));
		expect(run.stoppedEarly).toBe(expected.stoppedEarly);
		expect(JSON.stringify(run.finalSelection)).toBe(JSON.stringify(expected.finalSelection));
		const first = run.rounds[0]!;
		const primingIds = PRIMING_DIVERSE.map((_, index) => primingTreeId("sum-difference", 7, index, FIXED_CLOCK));
		expect(first.primingTreeIds).toEqual(primingIds);
		expect(first.primingProbes).toBeGreaterThan(0);
		expect(listTrees(dreamDir)).toHaveLength(3 + PRIMING_DIVERSE.length);
		for (const treeId of primingIds) expect(readTreeFiles(dreamDir, treeId)).toEqual(readTreeFiles(syncDir, treeId));
		expect(run.rounds[1]!.poolSize).toBe(1 + PRIMING_DIVERSE.length);
		expect(spans.find((span) => span.name === "dream.run")!.attrs["dream.priming_policies"]).toBe(2);
		// The explore spans of iteration 0: the initial rollout plus one per priming policy.
		const explores = spans.filter((span) => span.name === "dream.explore" && span.attrs["dream.iteration"] === 0);
		expect(explores.map((span) => span.attrs["dream.tree_id"])).toEqual([run.treeIds[0], ...primingIds]);
		// The fixed control charges the same priming and reports the same pool size without freezing a pool.
		const fixed = await runDreamLoopWithAgent(
			agentOptions({
				...base,
				dir: scratch("dream-prime-fixed-"),
				runAgent: makeStub({}).handler,
				useLlmProposer: false,
				useLlmDreamer: false,
				fixedPolicy: true,
			}),
		);
		expect(fixed.rounds[0]).toEqual(first);
		expect(fixed.rounds[1]!.poolSize).toBe(1 + PRIMING_DIVERSE.length);
	});

	it("mergePrimedRollouts sums probes, tokens and agent calls, takes the max best and merges the curve", async () => {
		const task = resolveTask({ task: "sum-difference" });
		const explore = (policy: ExplorationPolicy, fork: string, treeId?: string) =>
			runOnlineExplorationWithAgent(
				{
					task,
					taskId: "sum-difference",
					seed: 3,
					rng: createSeededRng(3).fork(fork),
					clock: () => FIXED_CLOCK,
					workers: 3,
					k1: 4,
					dir: dreamDir,
					policy,
					iteration: 0,
					...(treeId ? { treeId } : {}),
				},
				createLlmProposer(makeStub({ proposerOutput: () => ({ output: ARTIFACT, tokens: 5 }) }).handler, task, {
					scope: SCOPE,
					signal: liveController().signal,
					tokenBudget: 1,
				}),
			);
		const initial = await explore(DEFAULT_POLICY, "iter:0");
		const primed = [
			await explore(PRIMING_DIVERSE[0]!, "prime:0", "p0"),
			await explore(PRIMING_DIVERSE[1]!, "prime:1", "p1"),
		];
		const merged = mergePrimedRollouts(initial, primed);
		expect(merged.treeId).toBe(initial.treeId);
		expect(merged.revealedCount).toBe(initial.revealedCount);
		expect(merged.primingTreeIds).toEqual(["p0", "p1"]);
		expect(merged.primingProbes).toBe(primed[0]!.revealedCount + primed[1]!.revealedCount);
		expect(merged.tokens).toBe(initial.tokens + primed[0]!.tokens + primed[1]!.tokens);
		expect(merged.agentGeneratedCount).toBe(initial.revealedCount + merged.primingProbes!);
		expect(merged.bestScore).toBe(Math.max(initial.bestScore, primed[0]!.bestScore, primed[1]!.bestScore));
		expect(merged.rounds).toBe(initial.rounds);
		expect(merged.improvements!.at(-1)!.score).toBe(merged.bestScore);
		expect(merged.probesToBest).toBe(merged.improvements!.at(-1)!.probe);
		expect(merged.probesToBest).toBeLessThanOrEqual(initial.revealedCount + merged.primingProbes!);
		// Without priming the merge is the rollout's own curve and no priming fields.
		const alone = mergePrimedRollouts(initial, []);
		expect(alone.improvements).toEqual(initial.improvements);
		expect(alone.probesToBest).toBe(initial.probesToBest);
		expect(alone.primingTreeIds).toBeUndefined();
		expect(alone.primingProbes).toBeUndefined();
	});
});

describe("runDreamLoopWithAgent: semantic guidance (the ablation)", () => {
	it("rejects semanticGuidance without useLlmProposer before any span, call or tree file", async () => {
		const stub = makeStub({});
		const { spans } = await captureSpans(async () => {
			await expect(
				runDreamLoopWithAgent(
					agentOptions({ runAgent: stub.handler, useLlmProposer: false, semanticGuidance: true }),
				),
			).rejects.toThrow(/semanticGuidance requires useLlmProposer/);
		});
		expect(stub.calls()).toBe(0);
		expect(spans.some((span) => span.name === "dream.run")).toBe(false);
		expect(existsSync(join(dreamDir, "trees"))).toBe(false);
	});

	it("leaves iteration 0's prompt byte-identical and inserts injected insights after the header from iteration 1", async () => {
		const proposerOutput = () => ({ output: ARTIFACT, tokens: 10 });
		const dreamerOutput = () => ({ output: REVISED });
		const plain = makeStub({ proposerOutput, dreamerOutput });
		const plainRun = await runDreamLoopWithAgent(
			agentOptions({ runAgent: plain.handler, dir: scratch("dream-plain-") }),
		);
		const inputs: GuidanceInput[] = [];
		const guided = makeStub({ proposerOutput, dreamerOutput });
		const guidedRun = await runDreamLoopWithAgent(
			agentOptions({
				runAgent: guided.handler,
				dir: scratch("dream-guided-"),
				semanticGuidance: {
					insights: async (input) => {
						inputs.push(input);
						return { text: `Iteration ${input.iteration}: ${DEFAULT_INSIGHTS}`, tokens: 0 };
					},
				},
			}),
		);
		const firstRound = plainRun.rounds[0]!.handlerCalls.proposer;
		expect(firstRound).toBeGreaterThan(0);
		expect(guidedRun.rounds[0]!.handlerCalls.proposer).toBe(firstRound);
		expect(guided.prompts.proposer.slice(0, firstRound)).toEqual(plain.prompts.proposer.slice(0, firstRound));
		expect(plain.prompts.proposer.some((prompt) => prompt.includes(GUIDANCE_PREFIX))).toBe(false);
		const later = guided.prompts.proposer.slice(firstRound);
		expect(later.length).toBeGreaterThan(0);
		for (const prompt of later) {
			const header = prompt.indexOf(PROPOSER_PROMPT_HEADER);
			const guidance = prompt.indexOf(GUIDANCE_PREFIX);
			const body = prompt.indexOf("Improve the candidate below");
			expect(header).toBe(0);
			expect(guidance).toBeGreaterThan(header);
			expect(body).toBeGreaterThan(guidance);
			expect(prompt).toContain(DEFAULT_INSIGHTS);
		}
		expect(later.some((prompt) => prompt.includes("Iteration 1:"))).toBe(true);
		expect(later.some((prompt) => prompt.includes("Iteration 2:"))).toBe(true);
		// The injected writer replaced the child call: no guidance handler call, no guidance tokens.
		expect(guided.roleCalls.guidance).toBe(0);
		expect(guidedRun.rounds.every((round) => round.tokens.guidance === 0 && round.handlerCalls.guidance === 0)).toBe(
			true,
		);
		expect(inputs.map((input) => [input.iteration, input.poolSize, input.taskId])).toEqual([
			[1, 1, "sum-difference"],
			[2, 2, "sum-difference"],
		]);
		expect(guidedRun.treeIds[0]).toBe(plainRun.treeIds[0]);
	});

	it("asks one guidance-writer child per iteration >= 1, spans it, and accounts its tokens and calls", async () => {
		const stub = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 10 }),
			dreamerOutput: () => ({ output: REVISED, tokens: 20 }),
			guidanceOutput: () => ({ output: JSON.stringify({ insights: DEFAULT_INSIGHTS }), tokens: 77 }),
		});
		const { value: run, spans } = await captureSpans(() =>
			runDreamLoopWithAgent(agentOptions({ runAgent: stub.handler, semanticGuidance: true })),
		);
		expect(stub.roleCalls.guidance).toBe(2);
		expect(stub.prompts.guidance.every((prompt) => prompt.startsWith(GUIDANCE_PROMPT_HEADER))).toBe(true);
		expect(stub.prompts.guidance.every((prompt) => prompt.includes('"trees"'))).toBe(true);
		expect(run.rounds.map((round) => round.handlerCalls.guidance)).toEqual([0, 1, 1]);
		expect(run.rounds.map((round) => round.tokens.guidance)).toEqual([0, 77, 77]);
		expect(run.tokens).toBe(stub.totalTokens());
		const guidanceSpans = spans.filter((span) => span.name === "dream.llm_guidance");
		expect(guidanceSpans).toHaveLength(2);
		expect(guidanceSpans.map((span) => span.attrs["dream.iteration"])).toEqual([1, 2]);
		expect(guidanceSpans.map((span) => span.attrs["dream.pool_size"])).toEqual([1, 2]);
		expect(guidanceSpans.every((span) => span.attrs["dream.llm_fallback"] === false)).toBe(true);
		expect(guidanceSpans.every((span) => span.attrs["dream.tokens"] === 77)).toBe(true);
		const dreamRun = spans.find((span) => span.name === "dream.run")!;
		expect(
			guidanceSpans.every((span) => span.traceId === dreamRun.traceId && span.parentSpanId === dreamRun.spanId),
		).toBe(true);
		const firstRound = run.rounds[0]!.handlerCalls.proposer;
		expect(stub.prompts.proposer.slice(firstRound).every((prompt) => prompt.includes(DEFAULT_INSIGHTS))).toBe(true);
		expect(stub.prompts.proposer.slice(0, firstRound).some((prompt) => prompt.includes(GUIDANCE_PREFIX))).toBe(false);
	});

	it("falls back to empty guidance when the writer fails twice, and aborts the run when it is aborted", async () => {
		const failing = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 10 }),
			dreamerOutput: () => ({ output: REVISED }),
			guidanceOutput: () => ({ status: "error", tokens: 4 }),
		});
		const { value: run, spans } = await captureSpans(() =>
			runDreamLoopWithAgent(agentOptions({ runAgent: failing.handler, semanticGuidance: true })),
		);
		expect(run.rounds).toHaveLength(3);
		// One retry per iteration: two guidance calls, summed tokens, no guidance in any proposer prompt.
		expect(run.rounds.map((round) => round.handlerCalls.guidance)).toEqual([0, 2, 2]);
		expect(run.rounds.map((round) => round.tokens.guidance)).toEqual([0, 8, 8]);
		expect(failing.prompts.proposer.some((prompt) => prompt.includes(GUIDANCE_PREFIX))).toBe(false);
		const guidanceSpans = spans.filter((span) => span.name === "dream.llm_guidance");
		expect(guidanceSpans).toHaveLength(2);
		expect(guidanceSpans.every((span) => span.attrs["dream.llm_fallback"] === true)).toBe(true);
		expect(guidanceSpans.every((span) => span.attrs["dream.tokens"] === 8)).toBe(true);

		const aborting = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 10 }),
			guidanceOutput: () => ({ status: "aborted", tokens: 1 }),
		});
		const { value: caught, spans: abortSpans } = await captureSpans(async () => {
			try {
				await runDreamLoopWithAgent(agentOptions({ runAgent: aborting.handler, semanticGuidance: true }));
				return undefined;
			} catch (error) {
				return error;
			}
		});
		expect(isDreamAbortError(caught)).toBe(true);
		expect(aborting.roleCalls.guidance).toBe(1);
		expect(aborting.roleCalls.dreamer).toBe(0);
		expect(abortSpans.find((span) => span.name === "dream.run")!.attrs["dream.stopped"]).toBe("aborted");
	});

	it("puts the role header on the first line of every child prompt", async () => {
		const stub = makeStub({
			proposerOutput: () => ({ output: ARTIFACT, tokens: 1 }),
			dreamerOutput: () => ({ output: REVISED, tokens: 1 }),
			guidanceOutput: () => ({ output: JSON.stringify({ insights: DEFAULT_INSIGHTS }), tokens: 1 }),
		});
		await runDreamLoopWithAgent(agentOptions({ runAgent: stub.handler, iterations: 1, semanticGuidance: true }));
		expect(stub.prompts.proposer.length).toBeGreaterThan(0);
		expect(stub.prompts.dreamer).toHaveLength(1);
		expect(stub.prompts.guidance).toHaveLength(1);
		expect(stub.prompts.proposer.every((prompt) => prompt.split("\n")[0]!.startsWith(PROPOSER_PROMPT_HEADER))).toBe(
			true,
		);
		expect(stub.prompts.dreamer[0]!.split("\n")[0]).toBe(DREAMER_PROMPT_HEADER);
		expect(stub.prompts.guidance[0]!.split("\n")[0]!.startsWith(GUIDANCE_PROMPT_HEADER)).toBe(true);
		// The three headers are pairwise non-prefixes, so startsWith classification is unambiguous.
		const headers = [PROPOSER_PROMPT_HEADER, DREAMER_PROMPT_HEADER, GUIDANCE_PROMPT_HEADER];
		for (const a of headers) for (const b of headers) if (a !== b) expect(a.startsWith(b)).toBe(false);
	});
});

describe("buildGuidanceInput", () => {
	const blobs: Record<number, unknown> = {
		0: { set: [0, 1] },
		1: { set: [0, 1, 3] },
		2: { set: [0, 2, 3] },
		3: { set: [0, 1, 3, 7, 12, 20] },
	};
	const withFailure: TreeRecord[] = [
		...SYNTH,
		node({ id: "synth-n4", parentId: "synth-n1", seq: 4, round: 2, score: 0, valid: false, failClass: "degenerate" }),
	];
	const treeWithBlobs = (records: TreeRecord[]) => buildRecordedTree(records, (record) => blobs[record.seq] ?? null);

	it("digests recorded artifacts and scalar scores deterministically, in tree-id order, bounded by topK and the char cap", () => {
		const other: TreeRecord[] = withFailure.map((record) =>
			record.type === "tree"
				? { ...record, treeId: "alpha", policyId: "q" }
				: record.type === "node"
					? {
							...record,
							id: record.id.replace("synth", "alpha"),
							parentId: record.parentId?.replace("synth", "alpha") ?? null,
						}
					: record,
		);
		const pool = [treeWithBlobs(withFailure), treeWithBlobs(other)];
		const a = buildGuidanceInput(pool, "sum-difference", 3, 2, 16);
		const b = buildGuidanceInput([...pool].reverse(), "sum-difference", 3, 2, 16);
		expect(JSON.stringify(b)).toBe(JSON.stringify(a));
		expect(a.taskId).toBe("sum-difference");
		expect(a.iteration).toBe(3);
		expect(a.poolSize).toBe(2);
		expect(a.trees.map((tree) => tree.treeId)).toEqual(["alpha", "synth"]);
		const synth = a.trees[1]!;
		expect(synth.policyId).toBe("p");
		expect(synth.bestScore).toBe(0.9);
		expect(synth.attempts).toBe(4);
		expect(synth.rounds).toBe(2);
		expect(synth.failClasses).toEqual(["degenerate"]);
		expect(synth.topNodes).toHaveLength(2);
		expect(synth.topNodes.map((entry) => entry.score)).toEqual([0.9, 0.5]);
		expect(synth.topNodes[0]!.artifactJson).toBe(`${JSON.stringify(blobs[3]).slice(0, 16)}...`);
		expect(synth.topNodes[1]!.artifactJson).toBe(JSON.stringify(blobs[1]));
		// Only serialized artifacts and scalars: no hidden test data can appear because a tree never holds any.
		const serialized = JSON.stringify(a);
		expect(serialized).not.toMatch(/hidden|expected|stdin/);
		expect(buildGuidanceInput(pool, "sum-difference", 3, 0).trees.every((tree) => tree.topNodes.length === 0)).toBe(
			true,
		);
	});

	it("tolerates a tree without a blob loader by recording an empty artifact string", () => {
		const digest = buildGuidanceInput([buildRecordedTree(SYNTH)], "sum-difference", 1);
		expect(digest.trees[0]!.topNodes.map((entry) => entry.artifactJson)).toEqual(["", "", ""]);
		expect(digest.trees[0]!.topNodes.map((entry) => entry.score)).toEqual([0.9, 0.5, 0.4]);
	});
});
