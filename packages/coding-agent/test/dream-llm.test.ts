import { AsyncLocalStorage } from "node:async_hooks";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	addSpanSink,
	installAsyncTraceContextStorage,
	type SpanEndRecord,
	type TraceContext,
	type Usage,
	withSpan,
} from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { proposePolicies, runDreaming } from "../src/core/dream/improve.js";
import { projectProposeParams } from "../src/core/dream/interpreter.js";
import {
	createLlmProposer,
	DreamAbortError,
	type DreamLoopWithAgentOptions,
	isDreamAbortError,
	proposePoliciesWithAgent,
	runDreamLoopWithAgent,
	runOnlineExplorationWithAgent,
} from "../src/core/dream/llm.js";
import { DEFAULT_POLICY, type ExplorationPolicy, policyId } from "../src/core/dream/policy.js";
import { asyncOf, createLocalProposer } from "../src/core/dream/proposer.js";
import { createSeededRng } from "../src/core/dream/rng.js";
import { runOnlineExploration } from "../src/core/dream/rollout.js";
import { buildRecordedTree, type RecordedTree } from "../src/core/dream/store.js";
import type { ScoredTask } from "../src/core/dream/task.js";
import { resolveTask } from "../src/core/dream/tasks/index.js";
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

function usage(totalTokens: number): Usage {
	return {
		input: totalTokens,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
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

/** A stub RunAgentHandler that answers by role (proposer vs. dreamer) and tallies the tokens it reports. */
function makeStub(opts: {
	proposerOutput?: (call: number) => { output?: string; status?: RunAgentResult["status"]; tokens?: number };
	dreamerOutput?: (call: number) => { output?: string; status?: RunAgentResult["status"]; tokens?: number };
}): { handler: RunAgentHandler; totalTokens: () => number; calls: () => number } {
	let total = 0;
	let calls = 0;
	let proposerCalls = 0;
	let dreamerCalls = 0;
	const handler: RunAgentHandler = async (request) => {
		calls += 1;
		const isDreamer = request.prompt.includes("policy dreamer");
		const spec = isDreamer ? opts.dreamerOutput?.(++dreamerCalls) : opts.proposerOutput?.(++proposerCalls);
		const tokens = spec?.tokens ?? 100;
		total += tokens;
		return result({ status: spec?.status ?? "completed", output: spec?.output ?? "", usage: usage(tokens) });
	};
	return { handler, totalTokens: () => total, calls: () => calls };
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
const CFG = { k2: 10, objective: { beta1: 0.01, beta2: 0.02 } };
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
		const proposer = createLlmProposer(stub.handler, task, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
		});
		await expect(proposer.propose(null, PARAMS, createSeededRng(1), 1)).rejects.toBeInstanceOf(DreamAbortError);
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

describe("proposePoliciesWithAgent (LLM dreamer)", () => {
	it("keeps only the strictly in-bounds candidates and drops the rest", async () => {
		const candidatesJson = JSON.stringify([
			policy({ stopRule: "never" }), // valid
			policy({ branchWidth: 999 }), // out of range
			{ ...DEFAULT_POLICY, sneaky: "code" }, // unknown key
			policy({ selectionRule: "nope" as ExplorationPolicy["selectionRule"] }), // non-literal rule
		]);
		const stub = makeStub({ dreamerOutput: () => ({ output: candidatesJson, tokens: 500 }) });
		const dreamed = await proposePoliciesWithAgent(stub.handler, DEFAULT_POLICY, 4, {
			scope: SCOPE,
			signal: liveController().signal,
			tokenBudget: 200_000,
			localFallbackRng: createSeededRng(1),
		});
		expect(dreamed.candidates).toHaveLength(1);
		expect(dreamed.candidates[0]!.stopRule).toBe("never");
		expect(dreamed.tokens).toBe(500);
	});

	it("falls back to the local proposer when every candidate is dropped or the call fails", async () => {
		const allJunk = JSON.stringify([{ ...DEFAULT_POLICY, sneaky: 1 }, policy({ beta: -5 })]);
		const dropped = await proposePoliciesWithAgent(
			makeStub({ dreamerOutput: () => ({ output: allJunk, tokens: 300 }) }).handler,
			DEFAULT_POLICY,
			4,
			{ scope: SCOPE, signal: liveController().signal, tokenBudget: 200_000, localFallbackRng: createSeededRng(1) },
		);
		expect(dropped.candidates).toHaveLength(4);
		expect(dropped.tokens).toBe(300);
		const expected = proposePolicies(DEFAULT_POLICY, 4, createSeededRng(1)).map(policyId);
		expect(dropped.candidates.map(policyId)).toEqual(expected);

		const failed = await proposePoliciesWithAgent(
			makeStub({ dreamerOutput: () => ({ status: "error", tokens: 20 }) }).handler,
			DEFAULT_POLICY,
			3,
			{ scope: SCOPE, signal: liveController().signal, tokenBudget: 200_000, localFallbackRng: createSeededRng(2) },
		);
		// One retry on error -> two calls -> summed tokens; still a full local fallback set.
		expect(failed.tokens).toBe(40);
		expect(failed.candidates.map(policyId)).toEqual(
			proposePolicies(DEFAULT_POLICY, 3, createSeededRng(2)).map(policyId),
		);
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
		expect(dreamed.candidates.map(policyId)).toEqual([policyId(WORSE)]);
		const selection = runDreaming({
			current: CURRENT,
			pool: synthPool(),
			dreams: 1,
			k2: CFG.k2,
			rng: createSeededRng(1),
			objective: CFG.objective,
			proposeCandidates: () => dreamed.candidates,
		});
		expect(selection.improved).toBe(false);
		expect(selection.chosenPolicyId).toBe(policyId(CURRENT));
		expect(selection.chosenScore).toBeGreaterThanOrEqual(selection.currentScore);
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
		expect(dreamed.candidates.map(policyId)).toEqual(proposePolicies(CURRENT, 1, createSeededRng(3)).map(policyId));
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
