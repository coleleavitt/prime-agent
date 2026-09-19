import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getBundledSkillsDir } from "../src/config.js";
import type { DreamRunRequest, DreamRunServiceDeps, DreamRunStatus } from "../src/core/dream/run-service.js";
import { isSessionSlashCommandResultMessage } from "../src/core/messages.js";
import { loadSkillsFromDir } from "../src/core/skills.js";
import { parseDreamCommandOptions, parseSessionSlashCommand } from "../src/core/slash-commands.js";
import { createHarness, type Harness } from "./suite/harness.js";

const dreamFake = vi.hoisted(() => {
	type Status = DreamRunStatus;
	type Deps = Pick<DreamRunServiceDeps, "onUpdate" | "dir">;
	class FakeDreamRunService {
		static instances: FakeDreamRunService[] = [];
		readonly deps: Deps;
		readonly startCalls: DreamRunRequest[] = [];
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
			this.current = {
				runId: `run-${this.startCalls.length}`,
				phase: "idle",
				task: request.task,
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

		finish(stopReason: DreamRunStatus["stopReason"]): void {
			const status = this.emit({ phase: "stopped", stopReason });
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

	it("rejects unknown tasks, stray tokens, and invalid counts", () => {
		expect(() => parseDreamCommandOptions("--task bogus")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("do the thing")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("--workers 0")).toThrow("--workers expects a positive integer");
		expect(() => parseDreamCommandOptions("--seed -1")).toThrow("Usage: /dream");
		expect(() => parseDreamCommandOptions("--iterations")).toThrow("--iterations expects a positive integer");
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
});
