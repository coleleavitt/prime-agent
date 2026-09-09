import { execSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import type { Usage } from "@earendil-works/pi-ai";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import { buildBoundedContextView } from "../src/core/ravo/context-view.js";
import type { ChildCall, ControllerProposal } from "../src/core/ravo/controller.js";
import type { JsonValue, RavoState } from "../src/core/ravo/reducer.js";
import {
	adjudicateReferee,
	createRefereeOpponent,
	evaluateReferee,
	REFEREE_CRITERION_ID,
	REFEREE_OPPONENT_ID,
	type RefereeChallenge,
	type RefereeInput,
	refereePrompt,
	refereeSkillEdits,
	refereeTaskText,
	validateRefereeChallenge,
	withoutRefereeCriterion,
} from "../src/core/ravo/refereed-opponent.js";
import {
	proposalOf,
	type RavoRunRequest,
	RavoRunService,
	type RavoRunServiceDeps,
	type RavoRunStatus,
} from "../src/core/ravo/run-service.js";
import type { HarnessState, RefinementEdit } from "../src/core/refinement/refinement.js";
import {
	COUNTEREXAMPLE_INVALID_EXIT_CODE,
	readSkillModuleSource,
	runSkillCounterexample,
	type SkillCounterexampleRun,
} from "../src/core/refinement/skill-dry-run.js";
import type { RunAgentHandler, RunAgentOptions, RunAgentResult } from "../src/core/run-agent.js";

const python3 = execSync("which python3", { encoding: "utf8" }).trim();
const SKILL_MODULE = "ravo_referee_skill";
const SKILL_SOURCE = 'def add(a, b):\n    """Return a + b."""\n    return a + b\n';
const FAILING_TEST = `import ${SKILL_MODULE}\nassert ${SKILL_MODULE}.add(2, 2) == 5, "documented contract: add(2, 2) must be 5"\n`;
const PASSING_TEST = `from ${SKILL_MODULE} import add\nassert add(2, 2) == 4\n`;

let tmp: string;
const savedEnv = { ...process.env };
beforeAll(async () => {
	tmp = await mkdtemp(path.join(tmpdir(), "ravo-referee-"));
	await writeFile(path.join(tmp, `${SKILL_MODULE}.py`), SKILL_SOURCE, "utf8");
	// The kernel the fast screen, the referee runner, and the source reader resolve.
	process.env.PRIME_AGENT_KERNEL_PYTHON = python3;
	process.env.PRIME_AGENT_KERNEL_VENV = path.join(tmp, "venv-scratch");
	process.env.PYTHONPATH = tmp;
});
afterAll(() => {
	for (const key of ["PRIME_AGENT_KERNEL_PYTHON", "PRIME_AGENT_KERNEL_VENV", "PYTHONPATH"]) {
		if (savedEnv[key] === undefined) delete process.env[key];
		else process.env[key] = savedEnv[key];
	}
});

const skillEdit = (overrides: Partial<RefinementEdit> = {}): RefinementEdit => ({
	action: "create",
	kind: "skill",
	title: "Adder",
	content: `Call \`${SKILL_MODULE}.add(a, b)\`; returns the integer sum a + b.`,
	path: "math/add",
	reference: { type: "python", import: SKILL_MODULE, callable: "add" },
	arguments: { a: "int", b: "int" },
	...overrides,
});
const memoryEdit: RefinementEdit = {
	action: "create",
	kind: "memory",
	title: "Deploy barrier",
	content: "Wait on the database-state barrier before deploying.",
	path: "deploy/gates",
};

function artifact(edits: RefinementEdit[]): JsonValue {
	return JSON.parse(
		JSON.stringify({
			summary: "Add the adder skill",
			rationale: "The user asked for it twice.",
			expectedOutcome: "Sums are available.",
			addressedFingerprints: [],
			edits,
		}),
	) as JsonValue;
}
function controllerProposal(edits: RefinementEdit[]): ControllerProposal<JsonValue> {
	return { id: "p1", parentId: null, repairOf: null, artifact: artifact(edits) };
}
const context = buildBoundedContextView(
	{
		currentTask: { id: "task", kind: "current_task", text: "install the adder skill" },
		champion: { id: "champion:seed", kind: "champion", text: "Current champion seed (score 60): CHAMPION_SENTINEL" },
		constraints: [{ id: "harness-overview", kind: "constraint", text: "HARNESS_OVERVIEW_SENTINEL" }],
	},
	{ maxTokens: 24_000, maxBytes: 96_000, maxItems: 16, lineageDepth: 3, maxArtifactBytesPerItem: 32_000 },
);

function fakeRun(outcome: SkillCounterexampleRun["outcome"], stderr = ""): SkillCounterexampleRun {
	return {
		outcome,
		passed: outcome === "passed",
		exitCode: outcome === "passed" ? 0 : outcome === "failed" ? 1 : null,
		stdout: "",
		stderr,
		detail: `${outcome} detail`,
		durationMs: 1,
	};
}
function fakeChallenge(value: RefereeChallenge | "error" | "aborted"): {
	call: ChildCall<RefereeInput, RefereeChallenge>;
	inputs: RefereeInput[];
} {
	const inputs: RefereeInput[] = [];
	const call: ChildCall<RefereeInput, RefereeChallenge> = async (input) => {
		inputs.push(input);
		if (value === "error") return { status: "error", tokens: 7, error: "child output is not valid JSON" };
		if (value === "aborted") return { status: "aborted", tokens: 3 };
		return { status: "completed", value, tokens: 11 };
	};
	return { call, inputs };
}

describe("referee child output", () => {
	it("accepts a null flaw and a flaw with a test, clamping confidence", () => {
		expect(validateRefereeChallenge({ flaw: null })).toEqual({ flaw: null, confidence: 0 });
		expect(validateRefereeChallenge({ flaw: "  ", confidence: 0.9, test: "x" })).toEqual({
			flaw: null,
			confidence: 0.9,
		});
		expect(
			validateRefereeChallenge({ flaw: "add is wrong", confidence: 7, test: FAILING_TEST, editIndex: 1 }),
		).toEqual({ flaw: "add is wrong", confidence: 1, test: FAILING_TEST, editIndex: 1 });
		expect(validateRefereeChallenge({ flaw: "add is wrong", confidence: "-3", test: FAILING_TEST })).toMatchObject({
			confidence: 0,
		});
	});

	it("rejects a claimed flaw that carries no executable test (prose is not evidence)", () => {
		expect(() => validateRefereeChallenge({ flaw: "add is wrong", confidence: 0.9 })).toThrow(/executable test/);
		expect(() => validateRefereeChallenge({ flaw: "add is wrong", test: "   " })).toThrow(/executable test/);
		expect(validateRefereeChallenge("garbage")).toEqual({ flaw: null, confidence: 0 });
	});

	it("selects only create/update skill edits with a python reference", () => {
		const proposal = proposalOf(
			artifact([
				memoryEdit,
				skillEdit(),
				{ action: "delete", kind: "skill", id: "old" },
				skillEdit({ reference: { type: "shell", command: "x" } }),
				skillEdit({ action: "update", id: "adder" }),
			]),
		);
		expect(refereeSkillEdits(proposal).map((item) => item.editIndex)).toEqual([1, 4]);
	});
});

describe("adjudicateReferee", () => {
	const challenge: RefereeChallenge = { flaw: "add(2, 2) is not 5", confidence: 0.8, test: FAILING_TEST };

	it("upholds the flaw only when the test fails, carrying flaw, stderr, and the test source", () => {
		const stderr = `Traceback\nAssertionError: documented contract: add(2, 2) must be 5\n${"x".repeat(1000)}`;
		const upheld = adjudicateReferee(challenge, fakeRun("failed", stderr), 11);
		expect(upheld).toMatchObject({ verdict: "flaw_upheld", status: "fail", tokens: 11 });
		expect(upheld.detail).toContain("flaw upheld (confidence 0.80): add(2, 2) is not 5");
		expect(upheld.detail).toContain("AssertionError: documented contract");
		expect(upheld.detail).toContain(`<counterexample>\n${FAILING_TEST}\n</counterexample>`);
		expect(upheld.detail).not.toContain("x".repeat(401));
	});

	it("finds no flaw when the test passes, is invalid, times out, or was never produced", () => {
		expect(adjudicateReferee(challenge, fakeRun("passed"), 1)).toMatchObject({
			verdict: "no_flaw_found",
			status: "pass",
			detail: expect.stringContaining("counter-example passed"),
		});
		for (const outcome of ["invalid", "timeout", "aborted", "skipped", "spawn_failed"] as const) {
			const result = adjudicateReferee(challenge, fakeRun(outcome), 1);
			expect(result.verdict).toBe("no_flaw_found");
			expect(result.status).toBe("pass");
			expect(result.detail).toContain(`no executable counter-example (${outcome}`);
		}
		expect(adjudicateReferee({ flaw: null, confidence: 0 }, undefined, 2)).toMatchObject({
			status: "pass",
			detail: "referee found no flaw",
		});
		expect(adjudicateReferee(undefined, undefined, 0)).toMatchObject({
			status: "pass",
			detail: "no executable counter-example",
		});
	});
});

describe("referee sealing", () => {
	it("passes only the task text across the seal", () => {
		expect(refereeTaskText(context)).toBe("install the adder skill");
	});

	it("renders the proposal, the contract, and the resolved module source and nothing else", () => {
		const prompt = refereePrompt({
			proposal: controllerProposal([memoryEdit, skillEdit()]),
			task: "install the adder skill",
			skills: [
				{
					editIndex: 1,
					edit: skillEdit(),
					source: { origin: "/tmp/x.py", source: SKILL_SOURCE, truncated: false, detail: "read" },
				},
			],
		});
		expect(prompt.startsWith("# RAVO referee")).toBe(true);
		expect(prompt).toContain('<skill editIndex="1" action="create">');
		expect(prompt).toContain(`"import":"${SKILL_MODULE}"`);
		expect(prompt).toContain('<module_source path="/tmp/x.py">\ndef add(a, b):');
		expect(prompt).toContain("<task>\ninstall the adder skill\n</task>");
		expect(prompt).not.toContain("CHAMPION_SENTINEL");
		expect(prompt).not.toContain("HARNESS_OVERVIEW_SENTINEL");
		expect(prompt).not.toContain("failedCriteria");
	});
});

describe("evaluateReferee", () => {
	const run = vi.fn(async (_edit: RefinementEdit, test: string) =>
		test === FAILING_TEST ? fakeRun("failed", "AssertionError: nope") : fakeRun("passed"),
	);
	const deps = (call: ChildCall<RefereeInput, RefereeChallenge>) => ({
		challenge: call,
		run,
		readSource: async () => ({ origin: "/x.py", source: SKILL_SOURCE, truncated: false, detail: "read" }),
		proposalOf,
	});
	const options = { signal: new AbortController().signal, tokenBudget: 1000 };

	it("abstains (as a pass) when the proposal touches no skill, without calling the child", async () => {
		const { call, inputs } = fakeChallenge({ flaw: "x", confidence: 1, test: FAILING_TEST });
		const result = await evaluateReferee(
			deps(call),
			{ proposal: controllerProposal([memoryEdit]), context },
			options,
		);
		expect(result).toEqual({
			status: "completed",
			tokens: 0,
			value: {
				verdict: "not_applicable",
				status: "pass",
				detail: "abstain: proposal touches no skill edit",
				tokens: 0,
			},
		});
		expect(inputs).toHaveLength(0);
	});

	it("runs the referee's test against the targeted skill edit and upholds a failing one", async () => {
		const { call, inputs } = fakeChallenge({
			flaw: "add(2, 2) is not 5",
			confidence: 0.6,
			test: FAILING_TEST,
			editIndex: 1,
		});
		const result = await evaluateReferee(
			deps(call),
			{ proposal: controllerProposal([memoryEdit, skillEdit()]), context },
			options,
		);
		expect(result.status).toBe("completed");
		if (result.status !== "completed") return;
		expect(result.value).toMatchObject({ verdict: "flaw_upheld", status: "fail" });
		expect(result.tokens).toBe(11);
		expect(inputs[0]?.task).toBe("install the adder skill");
		expect(inputs[0]?.skills.map((skill) => skill.editIndex)).toEqual([1]);
		expect(inputs[0]?.skills[0]?.source?.source).toBe(SKILL_SOURCE);
		expect(run).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "skill" }), FAILING_TEST, {
			signal: options.signal,
		});
	});

	it("treats a malformed child result as no counter-example, not as a miss", async () => {
		const { call } = fakeChallenge("error");
		const result = await evaluateReferee(
			deps(call),
			{ proposal: controllerProposal([skillEdit()]), context },
			options,
		);
		expect(result).toMatchObject({
			status: "completed",
			tokens: 7,
			value: {
				verdict: "no_flaw_found",
				status: "pass",
				detail: "no executable counter-example (child output is not valid JSON)",
			},
		});
	});

	it("propagates an aborted child to the controller", async () => {
		const { call } = fakeChallenge("aborted");
		const result = await evaluateReferee(
			deps(call),
			{ proposal: controllerProposal([skillEdit()]), context },
			options,
		);
		expect(result).toEqual({ status: "aborted", tokens: 3 });
	});

	it("exposes the opponent adapter under the referee criterion id", async () => {
		const { call } = fakeChallenge({ flaw: null, confidence: 0.2 });
		const adapter = createRefereeOpponent(deps(call));
		expect(adapter).toMatchObject({ id: REFEREE_OPPONENT_ID, kind: "opponent", criterionId: REFEREE_CRITERION_ID });
		await expect(
			adapter.evaluate({ proposal: controllerProposal([skillEdit()]), context }, options),
		).resolves.toEqual({
			status: "completed",
			value: { status: "pass", detail: "referee found no flaw" },
			tokens: 11,
		});
	});

	it("strips the referee criterion from a persisted pool and leaves other pools untouched", () => {
		const base = emptyAssistedRavoState();
		expect(withoutRefereeCriterion(base)).toBe(base);
		const extended: RavoState<JsonValue> = {
			...base,
			opponents: {
				criteria: [...base.opponents.criteria, { id: REFEREE_CRITERION_ID, seedWeight: 1, currentWeight: 2 }],
			},
		};
		expect(withoutRefereeCriterion(extended).opponents.criteria.map((criterion) => criterion.id)).toEqual(
			base.opponents.criteria.map((criterion) => criterion.id),
		);
	});
});

describe("runSkillCounterexample (kernel python)", () => {
	const opts = () => ({ pythonPath: python3, env: { PYTHONPATH: tmp }, timeoutMs: 5000 });

	it("passes a test that honors the documented contract", async () => {
		const run = await runSkillCounterexample(skillEdit(), PASSING_TEST, opts());
		expect(run).toMatchObject({ outcome: "passed", passed: true, exitCode: 0 });
	});

	it("fails a test whose assertion is violated and reports the assertion text", async () => {
		const run = await runSkillCounterexample(skillEdit(), FAILING_TEST, opts());
		expect(run).toMatchObject({ outcome: "failed", passed: false, exitCode: 1 });
		expect(run.stderr).toContain("AssertionError: documented contract: add(2, 2) must be 5");
		expect(run.detail).toContain("AssertionError");
	});

	it("classifies a test that does not compile as invalid, not as a demonstrated flaw", async () => {
		const run = await runSkillCounterexample(skillEdit(), "def broken(:\n", opts());
		expect(run).toMatchObject({ outcome: "invalid", passed: false, exitCode: COUNTEREXAMPLE_INVALID_EXIT_CODE });
		expect(run.detail).toContain("does not compile: SyntaxError");
	});

	it("reports a timeout and a sys.exit as distinct outcomes", async () => {
		const slow = await runSkillCounterexample(skillEdit(), "import time\ntime.sleep(30)\n", {
			...opts(),
			timeoutMs: 500,
		});
		expect(slow.outcome).toBe("timeout");
		const exited = await runSkillCounterexample(skillEdit(), "import sys\nsys.exit(3)\n", opts());
		expect(exited).toMatchObject({ outcome: "failed", exitCode: 3 });
	});

	it("rejects malformed targets and empty tests without spawning, and skips without a kernel", async () => {
		expect((await runSkillCounterexample(memoryEdit, PASSING_TEST, opts())).outcome).toBe("invalid");
		expect((await runSkillCounterexample(skillEdit({ reference: undefined }), PASSING_TEST, opts())).outcome).toBe(
			"invalid",
		);
		expect((await runSkillCounterexample(skillEdit(), "   ", opts())).outcome).toBe("invalid");
		const saved = process.env.PRIME_AGENT_KERNEL_PYTHON;
		process.env.PRIME_AGENT_KERNEL_PYTHON = "/nonexistent/python";
		try {
			expect(await runSkillCounterexample(skillEdit(), PASSING_TEST)).toMatchObject({
				outcome: "skipped",
				detail: "counter-example skipped: no kernel python",
			});
		} finally {
			process.env.PRIME_AGENT_KERNEL_PYTHON = saved;
		}
	});

	it("reads the module source the reference resolves to", async () => {
		const source = await readSkillModuleSource(skillEdit(), opts());
		expect(source.origin).toBe(path.join(tmp, `${SKILL_MODULE}.py`));
		expect(source.source).toBe(SKILL_SOURCE);
		const clipped = await readSkillModuleSource(skillEdit(), { ...opts(), maxChars: 10 });
		expect(clipped).toMatchObject({ source: SKILL_SOURCE.slice(0, 10), truncated: true });
		const missing = await readSkillModuleSource(
			skillEdit({ reference: { type: "python", import: "no_such_module_xyz", callable: "run" } }),
			opts(),
		);
		expect(missing.source).toBeUndefined();
	});
});

// ---------------------------------------------------------------------------
// RavoRunService integration: the judge cannot substitute for the referee.
// ---------------------------------------------------------------------------

type Role = "inspect" | "plan" | "implement" | "repair" | "judge" | "supervisor" | "referee";
type Script = Partial<Record<Role, (call: number, prompt: string, options?: RunAgentOptions) => unknown>>;

const usage = (totalTokens: number): Usage => ({
	input: totalTokens,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});
const JUDGE_SENTINEL = "JUDGE_RATIONALE_SENTINEL";
const judgeVerdict = (score: number, failedCriteria: string[] = []) => ({
	score,
	failedCriteria,
	addressedFingerprints: [],
	rationale: JUDGE_SENTINEL,
});
const skillProposal = () => artifact([skillEdit()]);
const defaultScript: Script = {
	inspect: () => ({ summary: "no adder skill yet", facts: ["no skill mentions add"] }),
	plan: () => ({ steps: ["create skill adder"] }),
	implement: () => skillProposal(),
	repair: () => skillProposal(),
	judge: () => judgeVerdict(100),
	supervisor: () => ({ intervene: false }),
	referee: () => ({ flaw: null }),
};

function fakeRunAgent(script: Script) {
	const calls: Record<Role, number> = {
		inspect: 0,
		plan: 0,
		implement: 0,
		repair: 0,
		judge: 0,
		supervisor: 0,
		referee: 0,
	};
	const prompts: Partial<Record<Role, string[]>> = {};
	const runAgent: RunAgentHandler = async (request, options) => {
		const match = request.prompt.match(/^# RAVO (\w+)/);
		const role = match?.[1] as Role | undefined;
		if (!role || !(role in calls)) throw new Error(`unknown role prompt: ${request.prompt.slice(0, 40)}`);
		calls[role] += 1;
		prompts[role] = [...(prompts[role] ?? []), request.prompt];
		const handler = script[role] ?? defaultScript[role];
		if (!handler) throw new Error(`no script for ${role}`);
		const value = await handler(calls[role], request.prompt, options);
		const result: RunAgentResult = {
			status: "completed",
			output: typeof value === "string" ? value : JSON.stringify(value),
			messages: [],
			model: "faux/child",
			turns: 1,
			toolCalls: 0,
			usage: usage(10),
		};
		return result;
	};
	return { runAgent, calls, prompts };
}

function harnessState(overrides: Partial<HarnessState> = {}): HarnessState {
	return { schema: 1, entries: { prompt: {}, memory: {}, skill: {}, subagent: {} }, refinements: [], ...overrides };
}
function seededRavo(score: number, refereeWeight?: number): RavoState<JsonValue> {
	const state = emptyAssistedRavoState();
	return {
		...state,
		opponents: {
			criteria: [
				...state.opponents.criteria,
				...(refereeWeight === undefined
					? []
					: [{ id: REFEREE_CRITERION_ID, seedWeight: 1, currentWeight: refereeWeight }]),
			],
		},
		lineage: [{ proposalId: "seed", parentId: null, score, artifact: null, missedCriterionIds: [] }],
		championId: "seed",
		evaluatedProposalIds: ["seed"],
	};
}

async function harness(script: Script, initial: HarnessState = harnessState()) {
	const harnessDir = await mkdtemp(path.join(tmpdir(), "ravo-referee-service-"));
	let state = initial;
	const updates: RavoRunStatus[] = [];
	const saveState = vi.fn(async (next: HarnessState) => {
		state = structuredClone(next);
	});
	const fake = fakeRunAgent(script);
	const deps: RavoRunServiceDeps = {
		runAgent: fake.runAgent,
		harnessDir,
		loadState: async () => structuredClone(state),
		saveState,
		onUpdate: (status) => updates.push(status),
	};
	return {
		service: new RavoRunService(deps),
		updates,
		saveState,
		calls: fake.calls,
		prompts: fake.prompts,
		harnessDir,
		state: () => state,
	};
}

const request: RavoRunRequest = { task: "install the adder skill", maxRounds: 2, maxRepairs: 1 };
const certificates = (updates: RavoRunStatus[]) =>
	updates.filter((status) => status.lastEvent?.type === "evaluation").map((status) => status.lastCertificate);

describe("RavoRunService with the referee opponent", () => {
	it("rejects a judge-perfect proposal whose counter-example fails once the referee's weight exceeds epsilon, and hands the failing test to the repair", async () => {
		const { service, saveState, calls, prompts, updates, harnessDir } = await harness(
			{
				referee: () => ({
					flaw: "add(2, 2) is documented as 5",
					confidence: 0.9,
					test: FAILING_TEST,
					editIndex: 0,
				}),
			},
			harnessState({ ravo: seededRavo(60, 2) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("repair_limit");
		expect(terminal.phase).toBe("stopped");
		expect(certificates(updates).map((certificate) => certificate?.status)).toEqual([
			"reject_criteria",
			"reject_criteria",
		]);
		expect(terminal.lastCertificate).toMatchObject({
			status: "reject_criteria",
			deepScore: 100,
			missed: [REFEREE_CRITERION_ID],
		});
		expect(saveState).not.toHaveBeenCalled();
		expect(calls).toMatchObject({ judge: 2, referee: 2, repair: 1 });
		const repairPrompt = prompts.repair?.[0] ?? "";
		expect(repairPrompt).toContain("rejection: opponents");
		expect(repairPrompt).toContain(
			`opponent:${REFEREE_CRITERION_ID} [fail]: flaw upheld (confidence 0.90): add(2, 2) is documented as 5`,
		);
		expect(repairPrompt).toContain("AssertionError: documented contract: add(2, 2) must be 5");
		expect(repairPrompt).toContain(`<counterexample>\n${FAILING_TEST}\n</counterexample>`);
		expect(existsSync(path.join(harnessDir, "ravo", "runs", `${terminal.runId}.json`))).toBe(true);
	});

	it("charges a single upheld flaw at seed weight, commits under epsilon, and persists the doubled referee weight", async () => {
		const { service, saveState, state } = await harness(
			{ referee: () => ({ flaw: "add(2, 2) is documented as 5", confidence: 0.9, test: FAILING_TEST }) },
			harnessState({ ravo: seededRavo(60) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", missed: [REFEREE_CRITERION_ID] });
		expect(saveState).toHaveBeenCalledOnce();
		const persisted = state();
		expect(persisted.ravo?.lineage.at(-1)).toMatchObject({ score: 100, missedCriterionIds: [REFEREE_CRITERION_ID] });
		// Weakness pressure survives across runs: the missed referee is kept in
		// the persisted pool at double weight, so the next upheld flaw blocks.
		expect(
			persisted.ravo?.opponents.criteria.find((criterion) => criterion.id === REFEREE_CRITERION_ID),
		).toMatchObject({ currentWeight: 2 });
		expect(Object.values(persisted.entries.skill)).toHaveLength(1);
	});

	it("does not count a referee that returns prose without a test as a miss", async () => {
		const { service, calls, state } = await harness(
			{ referee: () => "I believe add() mishandles negative numbers but cannot prove it." },
			harnessState({ ravo: seededRavo(60, 2) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", missed: [] });
		expect(calls.referee).toBe(2);
		// Seeded at 2 and not missed, so no pressure is added.
		expect(state().ravo?.opponents.criteria.find((criterion) => criterion.id === REFEREE_CRITERION_ID)).toMatchObject(
			{
				currentWeight: 2,
			},
		);
	});

	it("passes a counter-example that the skill satisfies and seals the referee from the judge and the harness", async () => {
		const { service, prompts } = await harness(
			{ referee: () => ({ flaw: "add(2, 2) might not be 4", confidence: 0.3, test: PASSING_TEST }) },
			harnessState({ ravo: seededRavo(60, 2) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", missed: [] });
		const refereePrompts = prompts.referee ?? [];
		expect(refereePrompts).toHaveLength(1);
		expect(refereePrompts[0]).toContain("<task>\ninstall the adder skill\n</task>");
		expect(refereePrompts[0]).toContain(
			`<module_source path="${path.join(tmp, `${SKILL_MODULE}.py`)}">\n${SKILL_SOURCE}`,
		);
		expect(refereePrompts[0]).not.toContain(JUDGE_SENTINEL);
		expect(refereePrompts[0]).not.toContain("harness-overview");
		expect(refereePrompts[0]).not.toContain("Recurring failures");
		expect(prompts.judge?.[0]).toContain("harness-overview");
	});

	it("abstains on a memory-only proposal without spawning a referee child", async () => {
		const { service, calls } = await harness(
			{ implement: () => artifact([memoryEdit]), referee: () => ({ flaw: "x", confidence: 1, test: FAILING_TEST }) },
			harnessState({ ravo: seededRavo(60) }),
		);
		const terminal = await service.start(request);
		expect(terminal.stopReason).toBe("accepted");
		expect(terminal.lastCertificate).toMatchObject({ status: "commit", missed: [] });
		expect(calls.referee).toBe(0);
	});
});
