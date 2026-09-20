import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getBundledSkillsDir } from "../src/config.js";
import { PRIMING_DIVERSE } from "../src/core/dream/policy.js";
import type {
	DreamExperimentRequest,
	DreamRunRequest,
	DreamRunServiceDeps,
	DreamRunStatus,
} from "../src/core/dream/run-service.js";
import { DREAM_TASK_IDS } from "../src/core/dream/tasks/index.js";
import { isSessionSlashCommandResultMessage } from "../src/core/messages.js";
import { loadSkillsFromDir } from "../src/core/skills.js";
import { DREAM_USAGE, parseDreamCommandOptions, parseSessionSlashCommand } from "../src/core/slash-commands.js";
import { formatDreamRunStatusLine } from "../src/modes/agents-view/agents-view-state.js";
import { createHarness, type Harness } from "./suite/harness.js";

const dreamFake = vi.hoisted(() => {
	type Status = DreamRunStatus;
	type Deps = Pick<DreamRunServiceDeps, "onUpdate" | "dir">;
	class FakeDreamRunService {
		static instances: FakeDreamRunService[] = [];
		readonly deps: Deps;
		readonly startCalls: DreamRunRequest[] = [];
		readonly experimentCalls: DreamExperimentRequest[] = [];
		cancelCalls = 0;
		current: Status | undefined;
		private resolveStart: ((status: Status) => void) | undefined;
		private rejectStart: ((error: unknown) => void) | undefined;

		constructor(deps: Deps) {
			this.deps = deps;
			FakeDreamRunService.instances.push(this);
		}

		start(request: DreamRunRequest): Promise<Status> {
			this.startCalls.push(request);
			return this.#launch(request.task, "run");
		}

		startExperiment(request: DreamExperimentRequest): Promise<Status> {
			this.experimentCalls.push(request);
			return this.#launch(request.task, "experiment");
		}

		#launch(task: DreamRunRequest["task"], kind: "run" | "experiment"): Promise<Status> {
			this.current = {
				runId: `run-${this.startCalls.length + this.experimentCalls.length}`,
				phase: "idle",
				task,
				kind,
				iteration: 0,
				bestNodeScore: 0,
				startedAt: 1,
				updatedAt: 1,
			};
			return new Promise<Status>((resolve, reject) => {
				this.resolveStart = resolve;
				this.rejectStart = reject;
			});
		}

		status(): Status | undefined {
			return this.current;
		}

		cancel(): boolean {
			this.cancelCalls++;
			return this.running;
		}

		get running(): boolean {
			return this.current !== undefined && this.current.stopReason === undefined;
		}

		emit(patch: Partial<Status>): Status {
			if (!this.current) throw new Error("no run");
			this.current = { ...this.current, ...patch, updatedAt: this.current.updatedAt + 1 };
			this.deps.onUpdate(this.current);
			return this.current;
		}

		finish(stopReason: DreamRunStatus["stopReason"], extra: Partial<Status> = {}): void {
			const status = this.emit({ phase: "stopped", stopReason, ...extra });
			this.resolveStart?.(status);
		}

		fail(error: Error): void {
			this.emit({ phase: "stopped" });
			this.rejectStart?.(error);
		}
	}
	return { FakeDreamRunService };
});

vi.mock("../src/core/dream/run-service.js", async (importOriginal) => ({
	...(await importOriginal<Record<string, unknown>>()),
	DreamRunService: dreamFake.FakeDreamRunService,
}));

type FakeService = InstanceType<typeof dreamFake.FakeDreamRunService>;

function lastFake(): FakeService {
	const instance = dreamFake.FakeDreamRunService.instances.at(-1);
	if (!instance) throw new Error("DreamRunService was not constructed");
	return instance;
}

function commandResultTexts(harness: Harness): string[] {
	return harness.session.messages
		.filter((message) => isSessionSlashCommandResultMessage(message))
		.map((message) => (typeof message.content === "string" ? message.content : JSON.stringify(message.content)));
}

describe("dream entry points", () => {
	const harnesses: Harness[] = [];
	const previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		dreamFake.FakeDreamRunService.instances.length = 0;
		if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
		else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	});

	async function dreamHarness(): Promise<Harness> {
		const harness = await createHarness({ persistSession: true, rlmDepth: 0 });
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		return harness;
	}

	it("dream.run starts a background run and forwards service updates as dream_run_update events", async () => {
		const harness = await dreamHarness();
		const result = harness.session.handleDreamHostRequest("dream.run", {
			task: "circle-packing",
			iterations: 2,
			seed: 7,
			workers: 3,
			llm_dreamer: true,
		});
		expect(result).toMatchObject({ started: true, runId: "run-1" });

		const fake = lastFake();
		expect(fake.startCalls).toEqual([
			{ task: "circle-packing", iterations: 2, seed: 7, workers: 3, llmDreamer: true } satisfies DreamRunRequest,
		]);

		expect(harness.eventsOfType("dream_run_update")).toHaveLength(0);
		fake.emit({ phase: "rollout", iteration: 0, bestNodeScore: 0.5 });
		const updates = harness.eventsOfType("dream_run_update");
		expect(updates).toHaveLength(1);
		expect(updates[0].status).toMatchObject({ runId: "run-1", phase: "rollout", bestNodeScore: 0.5 });
		expect(harness.session.handleDreamHostRequest("dream.status")).toMatchObject({
			runId: "run-1",
			phase: "rollout",
		});

		const second = harness.session.handleDreamHostRequest("dream.run", { task: "sum-difference" });
		expect(second).toMatchObject({ started: false, reason: expect.stringContaining("run-1") });
		expect(fake.startCalls).toHaveLength(1);

		fake.finish("completed");
		await Promise.resolve();
		expect(harness.session.handleDreamHostRequest("dream.status")).toMatchObject({ stopReason: "completed" });
	});

	it("dream.experiment starts a background experiment and forwards its updates", async () => {
		const harness = await dreamHarness();
		const result = harness.session.handleDreamHostRequest("dream.experiment", {
			task: "sum-difference",
			rounds: 3,
			arms: ["fixed", "dream"],
			seed: 2,
			k1: 4,
		});
		expect(result).toMatchObject({ started: true, runId: "run-1", note: expect.stringContaining("resultPath") });
		const fake = lastFake();
		expect(fake.startCalls).toHaveLength(0);
		expect(fake.experimentCalls).toEqual([
			{
				task: "sum-difference",
				rounds: 3,
				arms: ["fixed", "dream"],
				seed: 2,
				k1: 4,
			} satisfies DreamExperimentRequest,
		]);
		fake.emit({ phase: "rollout", arm: "fixed", armIndex: 0, armCount: 2, round: 1, cumulativeProbes: 9 });
		const updates = harness.eventsOfType("dream_run_update");
		expect(updates).toHaveLength(1);
		expect(updates[0].status).toMatchObject({ kind: "experiment", arm: "fixed", round: 1, cumulativeProbes: 9 });
		expect(harness.session.handleDreamHostRequest("dream.status")).toMatchObject({
			kind: "experiment",
			arm: "fixed",
		});

		// The slot is shared with runs.
		expect(harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing" })).toMatchObject({
			started: false,
			reason: expect.stringContaining("run-1"),
		});
		fake.finish("completed", { resultPath: "/x/experiments/e/result.json" });
		await Promise.resolve();
		expect(harness.session.handleDreamHostRequest("dream.status")).toMatchObject({
			stopReason: "completed",
			resultPath: "/x/experiments/e/result.json",
		});
		// Guided arms with llm_proposer pass the parser (the service decides whether it can serve them).
		expect(
			harness.session.handleDreamHostRequest("dream.experiment", {
				task: "sum-difference",
				arms: ["dream", "dream-guided"],
				llm_proposer: true,
			}),
		).toMatchObject({ started: true });
		expect(lastFake().experimentCalls.at(-1)).toEqual({
			task: "sum-difference",
			arms: ["dream", "dream-guided"],
			llmProposer: true,
		});
		lastFake().finish("cancelled");
	});

	it("dream.experiment accepts seeds, the child knobs, priming and every registered task, and forwards them", async () => {
		const harness = await dreamHarness();
		const result = harness.session.handleDreamHostRequest("dream.experiment", {
			task: "autocorrelation",
			seeds: [7, 8, 9],
			model: "anthropic/claude-sonnet-5",
			thinking: "off",
			max_output_tokens: 4096,
			priming: "diverse",
			llm_proposer: true,
		});
		expect(result).toMatchObject({ started: true, seeds: 3, note: expect.stringContaining("3 seeds") });
		expect(lastFake().experimentCalls).toEqual([
			{
				task: "autocorrelation",
				seeds: [7, 8, 9],
				llmProposer: true,
				model: "anthropic/claude-sonnet-5",
				thinking: "off",
				maxOutputTokens: 4096,
				primingPolicies: [...PRIMING_DIVERSE],
			} satisfies DreamExperimentRequest,
		]);
		lastFake().finish("completed", {
			resultPaths: ["/d/experiments/a-s7/result.json", "/d/experiments/a-s8/result.json"],
			resultPath: "/d/experiments/a-s8/result.json",
		});
		await Promise.resolve();
		expect(harness.session.handleDreamHostRequest("dream.status")).toMatchObject({
			stopReason: "completed",
			resultPaths: ["/d/experiments/a-s7/result.json", "/d/experiments/a-s8/result.json"],
		});
		// `priming: "none"` is the default and leaves the request untouched; the run request takes the same knobs.
		harness.session.handleDreamHostRequest("dream.run", { task: "sum-difference", priming: "none", thinking: "LOW" });
		expect(lastFake().startCalls.at(-1)).toEqual({ task: "sum-difference", thinking: "low" });
		lastFake().finish("cancelled");
	});

	it("rejects malformed dream.experiment seeds and child knobs", async () => {
		const harness = await dreamHarness();
		const request = (payload: Record<string, unknown>) =>
			harness.session.handleDreamHostRequest("dream.experiment", { task: "sum-difference", ...payload });
		const invalid = /seeds must be a non-empty array of distinct non-negative integers/;
		expect(() => request({ seeds: "1,2" })).toThrow(invalid);
		expect(() => request({ seeds: [] })).toThrow(invalid);
		expect(() => request({ seeds: [1, 1] })).toThrow(invalid);
		expect(() => request({ seeds: [1, -1] })).toThrow(invalid);
		expect(() => request({ seeds: [1.5] })).toThrow(invalid);
		expect(() => request({ seeds: ["1"] })).toThrow(invalid);
		expect(() => request({ seeds: Array.from({ length: 17 }, (_, index) => index) })).toThrow(/at most 16 seeds/);
		expect(() => request({ seed: 1, seeds: [2] })).toThrow(/either seed or seeds, not both/);
		expect(() => request({ thinking: "deep" })).toThrow(/dream.experiment thinking must be one of/);
		expect(() => request({ model: "" })).toThrow(/model must not be empty/);
		expect(() => request({ max_output_tokens: 0 })).toThrow(/max_output_tokens must be a positive integer/);
		expect(() => request({ priming: "lots" })).toThrow(/priming must be "none" or "diverse"/);
		// The error text names every registered task, autocorrelation included.
		expect(() => harness.session.handleDreamHostRequest("dream.experiment", { task: "nope" })).toThrow(
			new RegExp(`task must be one of ${DREAM_TASK_IDS.join(", ")}`),
		);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "nope" })).toThrow(
			new RegExp(`task must be one of ${DREAM_TASK_IDS.join(", ")}`),
		);
		expect(dreamFake.FakeDreamRunService.instances.every((instance) => instance.experimentCalls.length === 0)).toBe(
			true,
		);
	});

	it("rejects malformed dream.experiment payloads", async () => {
		const harness = await dreamHarness();
		const request = (payload: Record<string, unknown>) =>
			harness.session.handleDreamHostRequest("dream.experiment", payload);
		expect(() => request({})).toThrow(/dream.experiment task must be one of/);
		expect(() => request({ task: "sum-difference", rounds: 0 })).toThrow(/rounds must be a positive integer/);
		expect(() => request({ task: "sum-difference", arms: ["bogus"] })).toThrow(/arms must be one of/);
		expect(() => request({ task: "sum-difference", arms: [] })).toThrow(/non-empty array/);
		expect(() => request({ task: "sum-difference", arms: "dream" })).toThrow(/non-empty array/);
		expect(() => request({ task: "sum-difference", arms: ["dream", "dream"] })).toThrow(/distinct/);
		expect(() => request({ task: "sum-difference", arms: ["fixed-guided"] })).toThrow(/require llm_proposer/);
		expect(() => request({ task: "sum-difference", llm_dreamer: 1 })).toThrow(/llm_dreamer must be a boolean/);
		expect(() => request({ task: "sum-difference", iterations: 2 })).not.toThrow();
		const fakes = dreamFake.FakeDreamRunService.instances;
		// Only the one legal request above reached the service.
		expect(fakes.reduce((count, instance) => count + instance.experimentCalls.length, 0)).toBe(1);
		lastFake().finish("cancelled");
	});

	it("dream.status is idle before any run and dream.cancel forwards to the service", async () => {
		const harness = await dreamHarness();
		expect(harness.session.handleDreamHostRequest("dream.status")).toEqual({ phase: "idle" });
		expect(harness.session.handleDreamHostRequest("dream.cancel")).toEqual({ cancelled: false });
		expect(lastFake().cancelCalls).toBe(1);

		harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing" });
		expect(harness.session.handleDreamHostRequest("dream.cancel")).toEqual({ cancelled: true });
		expect(lastFake().cancelCalls).toBe(2);
		lastFake().finish("cancelled");
	});

	it("rejects malformed dream.run payloads and unknown request types", async () => {
		const harness = await dreamHarness();
		expect(() => harness.session.handleDreamHostRequest("dream.run", {})).toThrow(/task must be one of/);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "nope" })).toThrow(
			/task must be one of/,
		);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", n: 0 })).toThrow(
			/n must be a positive integer/,
		);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", seed: -1 })).toThrow(
			/seed must be a non-negative integer/,
		);
		expect(() =>
			harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", iterations: 1.5 }),
		).toThrow(/iterations must be a positive integer/);
		expect(() =>
			harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", llm_proposer: "yes" }),
		).toThrow(/llm_proposer must be a boolean/);
		expect(() => harness.session.handleDreamHostRequest("dream.bogus")).toThrow(/unknown dream request type/);
		expect(dreamFake.FakeDreamRunService.instances.every((instance) => instance.startCalls.length === 0)).toBe(true);
	});

	it("is unavailable to RLM children", async () => {
		const harness = await createHarness({ persistSession: true, rlmDepth: 1 });
		harnesses.push(harness);
		expect(harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing" })).toMatchObject({
			started: false,
			reason: expect.stringContaining("not available"),
		});
		expect(harness.session.handleDreamHostRequest("dream.status")).toEqual({ phase: "idle" });
		expect(dreamFake.FakeDreamRunService.instances).toHaveLength(0);
	});

	it("/dream appends the started row and the terminal row when the run settles", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt("/dream --task circle-packing --iterations 2 --seed 4");
		await harness.session.waitForIdle();

		const fake = lastFake();
		expect(fake.startCalls).toEqual([{ task: "circle-packing", iterations: 2, seed: 4 } satisfies DreamRunRequest]);
		expect(commandResultTexts(harness)).toEqual(["Dream-RSI run run-1 started: circle-packing"]);

		fake.finish("completed");
		await vi.waitFor(() =>
			expect(commandResultTexts(harness)).toEqual([
				"Dream-RSI run run-1 started: circle-packing",
				"Dream-RSI run run-1 completed",
			]),
		);
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows.every((row) => row.details.success)).toBe(true);
		expect(rows.every((row) => row.details.command.name === "dream")).toBe(true);
	});

	it("/dream experiment starts an experiment and reports the result path in the terminal row", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt("/dream experiment --task sum-difference --rounds 3 --arms fixed,dream --seed 4");
		await harness.session.waitForIdle();

		const fake = lastFake();
		expect(fake.startCalls).toHaveLength(0);
		expect(fake.experimentCalls).toEqual([
			{ task: "sum-difference", seed: 4, rounds: 3, arms: ["fixed", "dream"] } satisfies DreamExperimentRequest,
		]);
		expect(commandResultTexts(harness)).toEqual(["Dream-RSI experiment run-1 started: sum-difference (fixed,dream)"]);

		fake.finish("completed", { resultPath: "/tmp/d/experiments/e/result.json" });
		await vi.waitFor(() =>
			expect(commandResultTexts(harness)).toEqual([
				"Dream-RSI experiment run-1 started: sum-difference (fixed,dream)",
				"Dream-RSI experiment run-1 completed (results /tmp/d/experiments/e/result.json)",
			]),
		);
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows.every((row) => row.details.success)).toBe(true);
	});

	it("/dream experiment --seeds forwards the seed list and the terminal row lists every result path", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt(
			"/dream experiment --task autocorrelation --seeds 7,8 --priming diverse --thinking off",
		);
		await harness.session.waitForIdle();
		const fake = lastFake();
		expect(fake.experimentCalls).toEqual([
			{ task: "autocorrelation", seeds: [7, 8], thinking: "off", primingPolicies: [...PRIMING_DIVERSE] },
		]);
		expect(commandResultTexts(harness)).toEqual([
			"Dream-RSI experiment run-1 started: autocorrelation (dream,fixed, 2 seeds)",
		]);
		fake.finish("completed", {
			resultPaths: ["/tmp/d/experiments/a-s7/result.json", "/tmp/d/experiments/a-s8/result.json"],
			resultPath: "/tmp/d/experiments/a-s8/result.json",
		});
		await vi.waitFor(() =>
			expect(commandResultTexts(harness).at(-1)).toBe(
				"Dream-RSI experiment run-1 completed (results /tmp/d/experiments/a-s7/result.json, /tmp/d/experiments/a-s8/result.json)",
			),
		);
	});

	it("/dream reports a run failure as an error row", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt("/dream --task python-speedup");
		await harness.session.waitForIdle();
		lastFake().fail(new Error("loop crashed"));
		await vi.waitFor(() =>
			expect(commandResultTexts(harness)).toEqual([
				"Dream-RSI run run-1 started: python-speedup",
				"Command failed: Dream-RSI run run-1 failed: loop crashed",
			]),
		);
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows.at(-1)?.details.success).toBe(false);
	});

	it("/dream with a bad flag fails the command", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt("/dream --task not-a-task");
		await harness.session.waitForIdle();
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows).toHaveLength(1);
		expect(rows[0].details.success).toBe(false);
		expect(rows[0].content).toContain("Usage: /dream");
		expect(dreamFake.FakeDreamRunService.instances.every((instance) => instance.startCalls.length === 0)).toBe(true);
	});

	it("dispose cancels an in-flight run", async () => {
		const harness = await dreamHarness();
		harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing" });
		const fake = lastFake();
		harness.session.dispose();
		expect(fake.cancelCalls).toBe(1);
		fake.finish("cancelled");
	});
});

describe("/dream argument parsing", () => {
	it("is a session slash command", () => {
		expect(parseSessionSlashCommand("/dream --seed 3")).toEqual({
			name: "dream",
			args: "--seed 3",
			text: "/dream --seed 3",
		});
	});

	it("defaults to circle-packing and the local proposer/dreamer", () => {
		expect(parseDreamCommandOptions("")).toEqual({ task: "circle-packing", llmProposer: false, llmDreamer: false });
	});

	it("parses every knob and both llm flags", () => {
		expect(
			parseDreamCommandOptions(
				"--task python-speedup --n 26 --seed 7 --workers 3 --k1=4 --k2 8 --dreams 2 --iterations 5 --llm-proposer --llm-dreamer",
			),
		).toEqual({
			task: "python-speedup",
			n: 26,
			seed: 7,
			workers: 3,
			k1: 4,
			k2: 8,
			dreams: 2,
			iterations: 5,
			llmProposer: true,
			llmDreamer: true,
		});
	});

	it("names every registered task in its usage and parses each", () => {
		expect(DREAM_USAGE).toContain(`--task <${DREAM_TASK_IDS.join("|")}>`);
		for (const task of DREAM_TASK_IDS) {
			expect(parseDreamCommandOptions(`experiment --task ${task}`).task).toBe(task);
		}
	});

	it("parses --seeds, --priming, --model, --thinking and --max-output-tokens", () => {
		expect(parseDreamCommandOptions("experiment --seeds 1,2,3")).toEqual({
			task: "circle-packing",
			llmProposer: false,
			llmDreamer: false,
			experiment: true,
			seeds: [1, 2, 3],
		});
		expect(parseDreamCommandOptions("experiment --seeds=7")).toMatchObject({ seeds: [7] });
		expect(
			parseDreamCommandOptions(
				"--priming diverse --model anthropic/claude-sonnet-5 --thinking OFF --max-output-tokens 4096 --llm-proposer",
			),
		).toEqual({
			task: "circle-packing",
			llmProposer: true,
			llmDreamer: false,
			model: "anthropic/claude-sonnet-5",
			thinking: "off",
			maxOutputTokens: 4096,
			priming: "diverse",
		});
		// `--priming none` is the default and parses byte-identically to no flag.
		expect(parseDreamCommandOptions("--priming none")).toEqual(parseDreamCommandOptions(""));
		expect(parseDreamCommandOptions("experiment --seeds 1,2,3").seed).toBeUndefined();
		expect(() => parseDreamCommandOptions("--seeds 1,2")).toThrow("belong to /dream experiment");
		expect(() => parseDreamCommandOptions("experiment --seed 1 --seeds 1,2")).toThrow("exclusive");
		expect(() => parseDreamCommandOptions("experiment --seeds 1,1")).toThrow("distinct non-negative integers");
		expect(() => parseDreamCommandOptions("experiment --seeds 1,-2")).toThrow("distinct non-negative integers");
		expect(() => parseDreamCommandOptions("experiment --seeds ,")).toThrow("comma-separated list of seeds");
		expect(() =>
			parseDreamCommandOptions(`experiment --seeds ${Array.from({ length: 17 }, (_, index) => index).join(",")}`),
		).toThrow("at most 16 seeds");
		expect(() => parseDreamCommandOptions("--priming lots")).toThrow("--priming expects none or diverse");
		expect(() => parseDreamCommandOptions("--thinking deep")).toThrow("--thinking expects one of");
		expect(() => parseDreamCommandOptions("--model")).toThrow("--model expects a provider/id selector");
		expect(() => parseDreamCommandOptions("--max-output-tokens 0")).toThrow(
			"--max-output-tokens expects a positive integer",
		);
	});

	it("rejects unknown tasks, stray tokens, and invalid counts", () => {
		expect(() => parseDreamCommandOptions("--task bogus")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("do the thing")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("--workers 0")).toThrow("--workers expects a positive integer");
		expect(() => parseDreamCommandOptions("--seed -1")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("--iterations")).toThrow("--iterations expects a positive integer");
	});

	it("parses the experiment form with --rounds and --arms", () => {
		expect(parseDreamCommandOptions("experiment")).toEqual({
			task: "circle-packing",
			llmProposer: false,
			llmDreamer: false,
			experiment: true,
		});
		expect(
			parseDreamCommandOptions("experiment --task sum-difference --rounds 3 --arms=dream,fixed --seed 2"),
		).toEqual({
			task: "sum-difference",
			seed: 2,
			rounds: 3,
			arms: ["dream", "fixed"],
			llmProposer: false,
			llmDreamer: false,
			experiment: true,
		});
		expect(parseDreamCommandOptions("experiment --arms dream,dream-guided --llm-proposer").arms).toEqual([
			"dream",
			"dream-guided",
		]);
		expect(() => parseDreamCommandOptions("experiment --arms dream,dream-guided")).toThrow("require --llm-proposer");
		expect(() => parseDreamCommandOptions("experiment --iterations 2")).toThrow("takes --rounds");
		expect(() => parseDreamCommandOptions("--rounds 2")).toThrow("belong to /dream experiment");
		expect(() => parseDreamCommandOptions("--arms dream")).toThrow("belong to /dream experiment");
		expect(() => parseDreamCommandOptions("experiment --arms dream,bogus")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("experiment --arms dream,dream")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("experiment --rounds 0")).toThrow("--rounds expects a positive integer");
		expect(() => parseDreamCommandOptions("--task sum-difference experiment")).toThrow("Usage: /dream");
	});
});

describe("formatDreamRunStatusLine", () => {
	const base = { runId: "dream_1", task: "autocorrelation" as const, iteration: 0, startedAt: 1, updatedAt: 1 };

	it("prints phase, iteration and best for a plain run", () => {
		expect(formatDreamRunStatusLine({ ...base, phase: "rollout", bestNodeScore: 0.5 })).toBe(
			"dream rollout it0 best 0.5000",
		);
		expect(
			formatDreamRunStatusLine({
				...base,
				phase: "stopped",
				stopReason: "completed",
				bestNodeScore: 0.5,
				finalPolicyScore: 0.9,
				improved: true,
			}),
		).toBe("dream completed");
		expect(formatDreamRunStatusLine({ ...base, phase: "stopped", error: "boom", bestNodeScore: 0 })).toBe(
			"dream error",
		);
	});

	it("names seed i/n, the arm and r/rounds for an experiment, plus the last completed seed's tokens", () => {
		expect(
			formatDreamRunStatusLine({
				...base,
				kind: "experiment",
				phase: "rollout",
				bestNodeScore: 0.58,
				seedIndex: 1,
				seedCount: 3,
				arm: "dream",
				armIndex: 0,
				armCount: 2,
				round: 2,
				rounds: 4,
				tokens: 163_460,
			}),
		).toBe("dream experiment seed 2/3 dream 1/2 r2/4 rollout best 0.5800 tokens 163460");
		expect(
			formatDreamRunStatusLine({ ...base, kind: "experiment", phase: "idle", bestNodeScore: 0, rounds: 4 }),
		).toBe("dream experiment r0/4 idle best 0.0000");
		expect(
			formatDreamRunStatusLine({
				...base,
				kind: "experiment",
				phase: "stopped",
				stopReason: "completed",
				bestNodeScore: 0.6,
				resultPaths: ["/a/result.json", "/b/result.json", "/c/result.json"],
			}),
		).toBe("dream experiment completed (3 result files)");
		expect(
			formatDreamRunStatusLine({
				...base,
				kind: "experiment",
				phase: "stopped",
				stopReason: "cancelled",
				bestNodeScore: 0.6,
				resultPath: "/a/result.json",
			}),
		).toBe("dream experiment cancelled (1 result file)");
	});
});

describe("bundled dream skill", () => {
	it("loads as a python skill named dream", () => {
		const { skills, diagnostics } = loadSkillsFromDir({ dir: getBundledSkillsDir(), source: "builtin" });
		expect(diagnostics).toEqual([]);
		const dream = skills.find((skill) => skill.name === "dream");
		expect(dream).toBeDefined();
		expect(dream?.kind).toBe("python");
		expect(dream?.kind === "python" && dream.python.importName).toBe("dream");
	});

	it("exposes experiment alongside run, status and cancel", () => {
		const script = [
			"import ast, sys",
			"tree = ast.parse(open(sys.argv[1]).read())",
			"print(','.join(sorted(n.name for n in tree.body if isinstance(n, ast.AsyncFunctionDef))))",
		].join("\n");
		const module = join(getBundledSkillsDir(), "dream", "src", "dream", "__init__.py");
		const parsed = spawnSync("python3", ["-c", script, module], { encoding: "utf8" });
		expect(parsed.status).toBe(0);
		expect(parsed.stdout.trim()).toBe("cancel,experiment,run,status");
	});

	it("accepts every registered task and the seeds and child knobs of the host payloads", () => {
		const script = [
			"import ast, sys",
			"tree = ast.parse(open(sys.argv[1]).read())",
			"tasks = next(ast.literal_eval(n.value) for n in tree.body if isinstance(n, ast.Assign) and n.targets[0].id == '_TASKS')",
			"fns = {n.name: n for n in tree.body if isinstance(n, ast.AsyncFunctionDef)}",
			"args = lambda name: [a.arg for a in fns[name].args.args]",
			"print(','.join(tasks))",
			"print(','.join(args('experiment')))",
			"print(','.join(args('run')))",
		].join("\n");
		const module = join(getBundledSkillsDir(), "dream", "src", "dream", "__init__.py");
		const parsed = spawnSync("python3", ["-c", script, module], { encoding: "utf8" });
		expect(parsed.status, parsed.stderr).toBe(0);
		const [tasks, experimentArgs, runArgs] = parsed.stdout.trim().split("\n");
		expect(tasks!.split(",").sort()).toEqual([...DREAM_TASK_IDS].sort());
		for (const arg of ["seeds", "seed", "model", "thinking", "max_output_tokens", "priming"]) {
			expect(experimentArgs!.split(",")).toContain(arg);
		}
		for (const arg of ["model", "thinking", "max_output_tokens", "priming"]) {
			expect(runArgs!.split(",")).toContain(arg);
		}
		expect(runArgs!.split(",")).not.toContain("seeds");
	});
});
