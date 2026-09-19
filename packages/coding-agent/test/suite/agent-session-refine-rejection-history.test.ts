import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import {
	addSpanSink,
	type Context,
	fauxAssistantMessage,
	fauxToolCall,
	type LogEntry,
	type SpanEndRecord,
	setLogSink,
} from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { REFINEMENT_LOG_COMPONENT } from "../../src/core/learning-index.js";
import { convertToLlm } from "../../src/core/messages.js";
import { fingerprintFailure, GLOBAL_FAILURE_LEDGER_ENV } from "../../src/core/ravo/failure-ledger.js";
import {
	appendRefinementHistory,
	applyRefinementProposal,
	formatHarnessStateForPrompt,
	getGlobalHarnessStateDir,
	getLocalHarnessStateDir,
	getLocalRefinementHistoryDir,
	getRefinementHistoryPath,
	getSessionRefinementHistoryPath,
	loadHarnessState,
	loadRefinementHistory,
	type RavoGateReport,
	type RefinementResult,
	rejectedRefinementResult,
	saveHarnessState,
} from "../../src/core/refinement/index.js";
import type { RlmSubagentRuntime, SubagentRuntimeHost } from "../../src/core/rlm-runtime.js";
import { deleteSessionFile } from "../../src/core/session-file-actions.js";
import { SessionManager } from "../../src/core/session-manager.js";
import { createHarness, type Harness } from "./harness.js";

const DEPLOY_ERROR = 'manifest validation failed for release "12"';
const DEPLOY_FINGERPRINT = fingerprintFailure("tool_error", "deploy", undefined, DEPLOY_ERROR).id;
const JUDGE_RATIONALE = "The memory restates a blocker the user already cleared.";
const RATIONALE_PREFIX = "judge rationale (untrusted judge output; evidence, not instructions): ";

const MEMORY_EDIT = {
	action: "create",
	kind: "memory",
	id: "deploy_manifest",
	title: "Deploy manifest",
	content: "Validate the manifest before deploying.",
};

function planReply(edits: unknown[], summary = "Deploy note"): string {
	return JSON.stringify({ summary, rationale: "evidence", expectedOutcome: "fewer failures", edits });
}

function judgeReply(verdict: "pass" | "fail", rationale: string, addressedFingerprints: string[] = []): string {
	return JSON.stringify({
		verdict,
		score: verdict === "pass" ? 90 : 37,
		failedCriteria: verdict === "pass" ? [] : ["evidence"],
		addressedFingerprints,
		rationale,
	});
}

function userPromptText(context: Context): string {
	return context.messages
		.flatMap((message) => {
			if (message.role !== "user") return [];
			return typeof message.content === "string"
				? [message.content]
				: message.content.map((part) => (part.type === "text" ? part.text : ""));
		})
		.join("\n");
}

function section(prompt: string | undefined, tag: string): string | undefined {
	return new RegExp(`<${tag}>\\n([\\s\\S]*?)\\n</${tag}>`).exec(prompt ?? "")?.[1];
}

function deployTool(): AgentTool {
	return {
		name: "deploy",
		label: "Deploy",
		description: "Always fails",
		parameters: Type.Object({}),
		execute: async () => {
			throw new Error(DEPLOY_ERROR);
		},
	};
}

describe("AgentSession refinement rejection history", () => {
	const harnesses: Harness[] = [];
	const tempDirs: string[] = [];
	const previousEnv = {
		agentDir: process.env.PRIME_AGENT_CODING_AGENT_DIR,
		globalLedger: process.env[GLOBAL_FAILURE_LEDGER_ENV],
	};
	let spans: SpanEndRecord[];
	let logs: LogEntry[];
	let removeSpanSink: () => void;

	beforeEach(() => {
		const agentDir = mkdtempSync(join(tmpdir(), "prime-agent-rejection-history-"));
		tempDirs.push(agentDir);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
		process.env[GLOBAL_FAILURE_LEDGER_ENV] = "0";
		spans = [];
		logs = [];
		removeSpanSink = addSpanSink((record) => spans.push(record));
		setLogSink((entry) => logs.push(entry));
	});

	afterEach(() => {
		removeSpanSink();
		setLogSink(undefined);
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		while (tempDirs.length > 0) rmSync(tempDirs.pop()!, { recursive: true, force: true });
		for (const [key, value] of [
			["PRIME_AGENT_CODING_AGENT_DIR", previousEnv.agentDir],
			[GLOBAL_FAILURE_LEDGER_ENV, previousEnv.globalLedger],
		] as const) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	});

	const spansNamed = (name: string) => spans.filter((span) => span.name === name);
	const outcomeLines = () => logs.filter((entry) => entry.component === REFINEMENT_LOG_COMPONENT);

	async function persistedSession(
		tools: AgentTool[] = [],
		options: { sessionManager?: SessionManager; autoRefine?: boolean } = {},
	): Promise<Harness> {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools,
			...(options.sessionManager ? { sessionManager: options.sessionManager } : {}),
			settings: {
				autoRefine: { enabled: options.autoRefine ?? true, turnInterval: 25, cooldownMs: 20 * 60_000 },
			},
		});
		harnesses.push(harness);
		harness.setResponses([fauxAssistantMessage("hi")]);
		await harness.session.prompt("hello");
		await harness.session.waitForIdle();
		return harness;
	}

	function ownLog(harness: Harness): string {
		return getSessionRefinementHistoryPath(harness.session.sessionId)!;
	}

	/** A manual local refine whose plan the judge refuses. */
	async function rejectLocally(harness: Harness, rationale = JUDGE_RATIONALE): Promise<RefinementResult> {
		harness.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply("fail", rationale)),
		]);
		return harness.session.refine({ instructions: "capture the deploy lesson" });
	}

	/** A refine that proposes nothing, returning the planner's user prompt. */
	async function nextPlannerPrompt(harness: Harness): Promise<string> {
		const prompts: string[] = [];
		harness.setResponses([
			(context) => {
				prompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await harness.session.refine({ instructions: "anything else?" });
		expect(prompts).toHaveLength(1);
		return prompts[0]!;
	}

	async function failDeployTwice(harness: Harness, afterRecurrence: Parameters<Harness["appendResponses"]>[0]) {
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("first done"),
		]);
		await harness.session.prompt("one");
		await harness.session.waitForIdle();
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("deploy", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("second done"),
			...afterRecurrence,
		]);
		await harness.session.prompt("two");
	}

	it("records a local rejection in the session's durable log and shows the next proposer why it was refused", async () => {
		const harness = await persistedSession();

		const rejected = await rejectLocally(harness);

		expect(rejected.ravo?.decision).toBe("reject_deep");
		expect(rejected.rejectionCause).toBe("gate");
		expect(loadRefinementHistory(ownLog(harness), "local")).toEqual([
			expect.objectContaining({ id: rejected.id, scope: "local", rejectionCause: "gate" }),
		]);
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "reject_deep",
			"refine.rejection_cause": "gate",
			"refine.history_record": "appended",
		});
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: "refinement.rejected", proposalId: rejected.id, cause: "gate" }),
		]);

		const history = section(await nextPlannerPrompt(harness), "refinement_history")!;
		expect(history).toContain("gate: reject_deep (");
		expect(history).toContain(`${RATIONALE_PREFIX}${JSON.stringify(JUDGE_RATIONALE)}`);
		expect(history).toContain("missed criteria: evidence");
		expect(history.replaceAll(rejected.id, "")).not.toMatch(/\b37\b/);
	});

	it("records a local applied refinement in the durable log", async () => {
		const harness = await persistedSession();
		harness.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply("pass", "judged")),
		]);

		const result = await harness.session.refine({ instructions: "capture the deploy lesson" });

		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "commit_unmeasured",
			"refine.history_record": "appended",
		});
		expect(spansNamed("refine.apply")[0]?.attrs).not.toHaveProperty("refine.rejection_cause");
		const [record] = loadRefinementHistory(ownLog(harness), "local");
		expect(record).toMatchObject({ id: result.id, scope: "local" });
		expect(record).not.toHaveProperty("rejectionCause");
		expect(outcomeLines()[0]).not.toHaveProperty("cause");
	});

	it("removes a session's refinement log when the session is deleted, and only that session's", async () => {
		const deleted = await persistedSession();
		const kept = await persistedSession();
		await rejectLocally(deleted);
		await rejectLocally(kept);
		expect(existsSync(ownLog(deleted))).toBe(true);
		expect(existsSync(ownLog(kept))).toBe(true);

		const result = await deleteSessionFile(deleted.sessionManager.getSessionFile()!);

		expect(result.ok).toBe(true);
		expect(existsSync(ownLog(deleted))).toBe(false);
		expect(existsSync(deleted.sessionManager.getSessionArtifactDir()!)).toBe(false);
		expect(loadRefinementHistory(ownLog(kept), "local")).toHaveLength(1);
	});

	it("removes the log of a session opened by an explicit path, keyed by its header id rather than the file name", async () => {
		const sessionsDir = join(tempDirs.at(-1)!, "explicit-sessions");
		mkdirSync(sessionsDir, { recursive: true });
		const explicitPath = join(sessionsDir, "notes.jsonl");
		const harness = await persistedSession([], {
			sessionManager: SessionManager.open(explicitPath, sessionsDir, sessionsDir),
		});
		expect(harness.sessionManager.getSessionFile()).toBe(explicitPath);
		expect(harness.session.sessionId).not.toBe("notes");
		await rejectLocally(harness);
		const namedLikeTheFile = getSessionRefinementHistoryPath("notes")!;
		appendRefinementHistory(namedLikeTheFile, rejectedLocalRecord("refine_20260916120000003"));
		expect(existsSync(ownLog(harness))).toBe(true);

		const result = await deleteSessionFile(explicitPath);

		expect(result.ok).toBe(true);
		expect(existsSync(ownLog(harness))).toBe(false);
		expect(existsSync(namedLikeTheFile)).toBe(true);
	});

	it("removes the refinement logs of RLM children at any depth with their deleted parent", async () => {
		const parent = await persistedSession();
		await rejectLocally(parent);
		const artifactDir = parent.sessionManager.getSessionArtifactDir()!;
		const childDir = join(artifactDir, "sub-abcd1234");
		const child = SessionManager.create(parent.tempDir, childDir);
		child.flushNow();
		const grandchild = SessionManager.create(parent.tempDir, join(childDir, "sub-efgh5678"));
		grandchild.flushNow();
		// A session transcript the agent copied into its scratch space is not a child, and neither is a non-session log.
		const copied = SessionManager.create(parent.tempDir, join(tempDirs.at(-1)!, "elsewhere"));
		copied.flushNow();
		mkdirSync(join(artifactDir, "scratch"), { recursive: true });
		copyFileSync(copied.getSessionFile()!, join(artifactDir, "scratch", basename(copied.getSessionFile()!)));
		writeFileSync(join(childDir, "events.jsonl"), `${JSON.stringify({ type: "event", id: "event-log" })}\n`);
		const logs = [child, grandchild, copied].map((manager, index) => {
			const path = getSessionRefinementHistoryPath(manager.getSessionId())!;
			appendRefinementHistory(path, rejectedLocalRecord(`refine_2026091612000000${index}`));
			return path;
		});
		const eventLog = getSessionRefinementHistoryPath("event-log")!;
		appendRefinementHistory(eventLog, rejectedLocalRecord("refine_20260916120000009"));

		const result = await deleteSessionFile(parent.sessionManager.getSessionFile()!);

		expect(result.ok).toBe(true);
		expect(existsSync(artifactDir)).toBe(false);
		expect([ownLog(parent), ...logs, eventLog].map((path) => existsSync(path))).toEqual([
			false,
			false,
			false,
			true,
			true,
		]);
	});

	it("removes an ephemeral runAgent child's refinement log with the child", async () => {
		let childLog: string | undefined;
		const host: SubagentRuntimeHost = {
			async createRlmSubagentRuntime(runtimeOptions): Promise<RlmSubagentRuntime> {
				const child = await createHarness({
					persistSession: true,
					rlmDepth: runtimeOptions.rlmDepth,
					rlmMaxDepth: runtimeOptions.rlmMaxDepth,
				});
				harnesses.push(child);
				child.setResponses([fauxAssistantMessage("child result")]);
				childLog = getSessionRefinementHistoryPath(child.session.sessionId)!;
				appendRefinementHistory(childLog, { ...rejectedLocalRecord("refine_20260916120000001") });
				return { session: child.session };
			},
			releaseRlmSubagentRuntime: async () => {},
			async deleteRlmSubagentRuntime() {},
		};
		const parent = await createHarness({ subagentRuntimeHost: host, rlmMaxDepth: 3 });
		harnesses.push(parent);
		const unrelated = getSessionRefinementHistoryPath("unrelated-session")!;
		appendRefinementHistory(unrelated, rejectedLocalRecord("refine_20260916120000002"));

		const result = await parent.session.runAgent({ prompt: "do the focused task" });

		expect(result.status).toBe("completed");
		expect(childLog).toBeDefined();
		expect(existsSync(childLog!)).toBe(false);
		expect(existsSync(unrelated)).toBe(true);
	});

	it("does not show one session's local rejection to another session's manual refine", async () => {
		const first = await persistedSession();
		const rejected = await rejectLocally(first, "FIRST-SESSION-RATIONALE");
		const second = await persistedSession();

		const prompt = await nextPlannerPrompt(second);

		expect(prompt).not.toContain("FIRST-SESSION-RATIONALE");
		expect(prompt).not.toContain(rejected.id);
		expect(prompt).not.toContain("<other_session_rejections>");
		expect(spansNamed("refine.plan").at(-1)?.attrs).not.toHaveProperty("refine.related_rejections");
	});

	it("shows a failure refine the rejections other sessions recorded for the same failure", async () => {
		const first = await persistedSession([deployTool()]);
		await failDeployTwice(first, [
			fauxAssistantMessage(planReply([MEMORY_EDIT], "FIRST-SESSION-SUMMARY")),
			fauxAssistantMessage(
				judgeReply("fail", "The manifest note repeats a fix that already failed.", [DEPLOY_FINGERPRINT]),
			),
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		await first.session.waitForIdle();
		const [firstRecord] = loadRefinementHistory(ownLog(first), "local");
		expect(firstRecord).toMatchObject({ rejectionCause: "gate", triggerFingerprintIds: [DEPLOY_FINGERPRINT] });

		const second = await persistedSession([deployTool()]);
		const prompts: string[] = [];
		await failDeployTwice(second, [
			(context) => {
				prompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2), { timeout: 10_000 });

		const related = section(prompts[0], "other_session_rejections");
		expect(related).toBeDefined();
		expect(related).toContain(`[${firstRecord!.id}] rejected in another session for failure:${DEPLOY_FINGERPRINT}`);
		expect(related).toContain("not applied: create memory:deploy_manifest");
		expect(related).toContain(`${RATIONALE_PREFIX}"The manifest note repeats a fix that already failed."`);
		expect(related).not.toContain("FIRST-SESSION-SUMMARY");
		expect(related).not.toContain(MEMORY_EDIT.content);
		expect(section(prompts[0], "refinement_history")).toBe("No prior refinement history.");
		expect(spansNamed("refine.plan").at(-1)?.attrs).toMatchObject({
			"refine.reason": "recurrence",
			"refine.related_rejections": 1,
		});
	});

	it("labels another session's rejection planned while the failure recurred as not targeting it", async () => {
		const first = await persistedSession([deployTool()], { autoRefine: false });
		await failDeployTwice(first, []);
		await first.session.waitForIdle();
		first.setResponses([
			fauxAssistantMessage(
				planReply([{ action: "create", kind: "memory", id: "style_pref", title: "Style", content: "Use tabs." }]),
			),
			fauxAssistantMessage(judgeReply("fail", "STYLE-RATIONALE unrelated to deploys")),
		]);
		const rejected = await first.session.refine({ instructions: "record the user's indentation preference" });
		expect(rejected.ravo?.failureOpponents).toEqual([`failure:${DEPLOY_FINGERPRINT}`]);
		expect(rejected.triggerFingerprintIds).toBeUndefined();

		const second = await persistedSession([deployTool()]);
		const prompts: string[] = [];
		await failDeployTwice(second, [
			(context) => {
				prompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2), { timeout: 10_000 });

		const related = section(prompts[0], "other_session_rejections");
		expect(related?.split("\n").slice(0, 2)).toEqual([
			`[${rejected.id}] rejected in another session while failure:${DEPLOY_FINGERPRINT} was recurring (not targeted)`,
			"not applied: create memory:style_pref",
		]);
		expect(related).toContain(`${RATIONALE_PREFIX}"STYLE-RATIONALE unrelated to deploys"`);
	});

	it("does not show a fork the rejection its transcript copied from the parent a second time", async () => {
		const parent = await persistedSession([deployTool()]);
		await failDeployTwice(parent, [
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply("fail", "PARENT-RATIONALE", [DEPLOY_FINGERPRINT])),
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(1), { timeout: 10_000 });
		await parent.session.waitForIdle();
		const [parentRecord] = loadRefinementHistory(ownLog(parent), "local");
		expect(parentRecord).toMatchObject({ rejectionCause: "gate", triggerFingerprintIds: [DEPLOY_FINGERPRINT] });

		const fork = await persistedSession([deployTool()], {
			sessionManager: SessionManager.forkFrom(
				parent.sessionManager.getSessionFile()!,
				parent.tempDir,
				join(parent.tempDir, "sessions"),
			),
		});
		expect(fork.session.sessionId).not.toBe(parent.session.sessionId);
		const prompts: string[] = [];
		await failDeployTwice(fork, [
			(context) => {
				prompts.push(userPromptText(context));
				return fauxAssistantMessage(planReply([]));
			},
		]);
		await vi.waitFor(() => expect(spansNamed("refine.apply")).toHaveLength(2), { timeout: 10_000 });

		expect(section(prompts[0], "refinement_history")).toContain(`[${parentRecord!.id}]`);
		expect(prompts[0]).not.toContain("<other_session_rejections>");
		expect(spansNamed("refine.plan").at(-1)?.attrs).toMatchObject({ "refine.related_rejections": 0 });
	});

	it("routes a global rejection to the global log only", async () => {
		const harness = await persistedSession();
		harness.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply("fail", JUDGE_RATIONALE)),
		]);

		const rejected = await harness.session.refine({ instructions: "for everyone", global: true });

		expect(loadRefinementHistory(getRefinementHistoryPath(getGlobalHarnessStateDir()), "global")).toEqual([
			expect.objectContaining({ id: rejected.id, scope: "global", rejectionCause: "gate" }),
		]);
		expect(existsSync(ownLog(harness))).toBe(false);
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.scope": "global",
			"refine.history_record": "appended",
		});
	});

	it("skips the durable record for an unpersisted session", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const recordedDir = join(harness.tempDir, "recorded-local", "harness");
		const recordedState = loadHarnessState(recordedDir, "local");
		const seeded = applyRefinementProposal(
			recordedState,
			{
				summary: "Seed local memory",
				rationale: "seed",
				expectedOutcome: "seeded",
				edits: [{ action: "create", kind: "memory", id: "remember_me", title: "Remember", content: "x" }],
			},
			{ id: "refine_recorded", scope: "local" },
		);
		seeded.harnessStatePath = saveHarnessState(recordedDir, recordedState);
		harness.sessionManager.appendCustomEntry("prime-agent.refinement", seeded);

		const result = await harness.session.refine({ rollbackId: "refine_recorded" });

		expect(result.rollbackOf).toBe("refine_recorded");
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({
			"refine.decision": "rollback",
			"refine.history_record": "skipped",
		});
		expect(existsSync(getLocalRefinementHistoryDir())).toBe(false);
	});

	it("keeps a rejection out of the working model's context and the harness digest", async () => {
		const harness = await persistedSession();

		await rejectLocally(harness);

		const modelText = JSON.stringify(convertToLlm(harness.session.messages));
		expect(modelText).not.toContain(JUDGE_RATIONALE);
		expect(modelText).not.toContain("RAVO gate rejected");
		expect(
			harness.session.messages.some(
				(message) => message.role === "custom" && message.customType === "refinement_notice",
			),
		).toBe(false);
		const localState = loadHarnessState(getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!);
		expect(localState.refinements).toEqual([]);
		expect(formatHarnessStateForPrompt(localState)).toContain("recent refinements: 0");
	});

	it("shows the proposer a newer durable record ahead of older transcript-only records", async () => {
		const harness = await persistedSession();
		for (let index = 0; index < 20; index++) {
			harness.sessionManager.appendCustomEntry("prime-agent.refinement", {
				id: `refine_202601010000000${index.toString().padStart(2, "0")}`,
				summary: `transcript refinement ${index}`,
				rationale: "old",
				expectedOutcome: "old outcome",
				appliedEdits: [],
				harnessStatePath: "",
				scope: "local",
			} satisfies RefinementResult);
		}
		appendRefinementHistory(ownLog(harness), {
			...rejectedLocalRecord("refine_20260916000000000"),
			ravo: gateReport({ rationale: "newest local rejection" }),
		});

		const history = section(await nextPlannerPrompt(harness), "refinement_history")!;

		expect(history.startsWith("[1 earlier refinements omitted]")).toBe(true);
		expect(history).not.toContain("[refine_20260101000000000]");
		const newest = history.split("\n\n").at(-1)!;
		expect(newest.startsWith("[refine_20260916000000000] RAVO gate rejected: seeded")).toBe(true);
		expect(newest).toContain(`${RATIONALE_PREFIX}"newest local rejection"`);
	});

	/** Seed a memory, gate a plan against it, then change it while the judge is out. */
	async function planAgainstMovingBaseline(harness: Harness, judge: string): Promise<RefinementResult> {
		const localDir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		const seeded = loadHarnessState(localDir, "local");
		applyRefinementProposal(
			seeded,
			{
				summary: "Seed memory",
				rationale: "seed",
				expectedOutcome: "seeded",
				edits: [{ action: "create", kind: "memory", id: "shared", title: "Shared", content: "planning baseline" }],
			},
			{ id: "seed_shared", scope: "local" },
		);
		saveHarnessState(localDir, seeded);
		let releasePlan: () => void = () => {};
		const planGate = new Promise<void>((resolve) => {
			releasePlan = resolve;
		});
		let planStarted: () => void = () => {};
		const started = new Promise<void>((resolve) => {
			planStarted = resolve;
		});
		harness.setResponses([
			async () => {
				planStarted();
				await planGate;
				return fauxAssistantMessage(
					planReply([{ action: "update", kind: "memory", id: "shared", title: "Shared", content: "planned" }]),
				);
			},
			fauxAssistantMessage(judge),
		]);
		const running = harness.session.refine({ instructions: "update shared memory" });
		await started;
		const concurrent = loadHarnessState(localDir, "local");
		concurrent.entries.memory.shared!.content = "concurrent kernel content";
		concurrent.entries.memory.shared!.version++;
		saveHarnessState(localDir, concurrent);
		releasePlan();
		const result = await running;
		expect(loadHarnessState(localDir, "local").entries.memory.shared?.content).toBe("concurrent kernel content");
		return result;
	}

	it("records an apply-time approval loss as baseline_changed without the judge's approval text", async () => {
		const harness = await persistedSession();

		const result = await planAgainstMovingBaseline(harness, judgeReply("pass", "judged"));

		expect(spansNamed("refine.plan")[0]?.attrs["ravo.decision"]).toBe("commit");
		expect(result.ravo?.decision).toBe("reject_deep");
		expect(result.rejectionCause).toBe("baseline_changed");
		expect(loadRefinementHistory(ownLog(harness), "local")[0]).toMatchObject({ rejectionCause: "baseline_changed" });
		expect(spansNamed("refine.apply")[0]?.attrs).toMatchObject({ "refine.rejection_cause": "baseline_changed" });
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: "refinement.rejected", decision: "reject_deep", cause: "baseline_changed" }),
		]);

		const history = section(await nextPlannerPrompt(harness), "refinement_history")!;
		expect(history).toContain("the approval no longer held");
		expect(history).not.toContain("judged");
	});

	it("keeps a gate rejection's own rationale when the harness changed during planning", async () => {
		const harness = await persistedSession();

		const result = await planAgainstMovingBaseline(harness, judgeReply("fail", "JUDGE-SAYS stale"));

		expect(result.ravo).toMatchObject({ decision: "reject_deep", rationale: "JUDGE-SAYS stale" });
		expect(result.rejectionCause).toBe("gate");
		expect(outcomeLines()).toEqual([
			expect.objectContaining({ msg: "refinement.rejected", decision: "reject_deep", cause: "gate" }),
		]);
		const history = section(await nextPlannerPrompt(harness), "refinement_history")!;
		expect(history).toContain(`${RATIONALE_PREFIX}"JUDGE-SAYS stale"`);
		expect(history).not.toContain("no longer held");
	});

	it("keeps an applied global refine successful when its history cannot be written", async () => {
		const harness = await persistedSession();
		const globalLog = getRefinementHistoryPath(getGlobalHarnessStateDir());
		mkdirSync(globalLog, { recursive: true });
		harness.setResponses([
			fauxAssistantMessage(planReply([MEMORY_EDIT])),
			fauxAssistantMessage(judgeReply("pass", "judged")),
		]);

		const result = await harness.session.refine({ instructions: "for everyone", global: true });

		expect(loadHarnessState(getGlobalHarnessStateDir(), "global").entries.memory.deploy_manifest).toBeDefined();
		expect(spansNamed("refine.apply")[0]).toMatchObject({
			status: "ok",
			attrs: expect.objectContaining({ "refine.history_record": "failed" }),
		});
		expect(logs).toEqual(
			expect.arrayContaining([
				expect.objectContaining({ level: "warn", msg: "refinement.history_unreadable", code: "EISDIR" }),
				expect.objectContaining({
					level: "warn",
					msg: "refinement.history_append_failed",
					proposalId: result.id,
					scope: "global",
					code: "EISDIR",
				}),
			]),
		);
		expect(
			harness.sessionManager
				.getEntries()
				.some(
					(entry) =>
						entry.type === "custom" &&
						entry.customType === "prime-agent.refinement" &&
						(entry.data as RefinementResult).id === result.id,
				),
		).toBe(true);
		expect(dirname(globalLog)).toBe(getGlobalHarnessStateDir());
	});
});

function gateReport(overrides: Partial<RavoGateReport> = {}): RavoGateReport {
	return {
		decision: "reject_deep",
		fastScore: 100,
		deepScore: 37,
		bestScore: 0,
		missedCriteria: ["evidence"],
		missedWeight: 1,
		epsilon: 1,
		screenThreshold: 50,
		deepTolerance: 10,
		rationale: "rejected",
		addressedFingerprints: [],
		failureOpponents: [],
		measurable: false,
		refereeCounts: { cleared: 0, upheld: 0, unverifiable: 0, no_evidence: 0, not_applicable: 0 },
		...overrides,
	};
}

function rejectedLocalRecord(id: string): RefinementResult {
	return rejectedRefinementResult(
		{ summary: "seeded", rationale: "seeded", expectedOutcome: "seeded", edits: [MEMORY_EDIT as never] },
		gateReport(),
		{ id, scope: "local" },
	);
}
