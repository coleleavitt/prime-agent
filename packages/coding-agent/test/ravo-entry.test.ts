import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getBundledSkillsDir } from "../src/config.js";
import { isSessionSlashCommandResultMessage } from "../src/core/messages.js";
import type { RavoStopReason } from "../src/core/ravo/controller.js";
import type { RavoRunRequest, RavoRunServiceDeps, RavoRunStatus } from "../src/core/ravo/run-service.js";
import { loadSkillsFromDir } from "../src/core/skills.js";
import { parseRavoCommandOptions, parseSessionSlashCommand } from "../src/core/slash-commands.js";
import { createHarness, type Harness } from "./suite/harness.js";

const ravoFake = vi.hoisted(() => {
	type Status = RavoRunStatus;
	type Deps = Pick<RavoRunServiceDeps, "onUpdate" | "harnessDir" | "globalHarnessDir">;
	class FakeRavoRunService {
		static instances: FakeRavoRunService[] = [];
		readonly deps: Deps;
		readonly startCalls: RavoRunRequest[] = [];
		cancelCalls = 0;
		current: Status | undefined;
		private resolveStart: ((status: Status) => void) | undefined;
		private rejectStart: ((error: unknown) => void) | undefined;

		constructor(deps: Deps) {
			this.deps = deps;
			FakeRavoRunService.instances.push(this);
		}

		start(request: RavoRunRequest): Promise<Status> {
			this.startCalls.push(request);
			this.current = {
				runId: `run-${this.startCalls.length}`,
				phase: "idle",
				round: 0,
				repairs: 0,
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

		finish(stopReason: RavoStopReason): void {
			const status = this.emit({ phase: "stopped", stopReason });
			this.resolveStart?.(status);
		}

		fail(error: Error): void {
			this.emit({ phase: "stopped" });
			this.rejectStart?.(error);
		}
	}
	return { FakeRavoRunService };
});

vi.mock("../src/core/ravo/run-service.js", () => ({ RavoRunService: ravoFake.FakeRavoRunService }));

type FakeService = InstanceType<typeof ravoFake.FakeRavoRunService>;

function lastFake(): FakeService {
	const instance = ravoFake.FakeRavoRunService.instances.at(-1);
	if (!instance) throw new Error("RavoRunService was not constructed");
	return instance;
}

function commandResultTexts(harness: Harness): string[] {
	return harness.session.messages
		.filter((message) => isSessionSlashCommandResultMessage(message))
		.map((message) => (typeof message.content === "string" ? message.content : JSON.stringify(message.content)));
}

describe("ravo entry points", () => {
	const harnesses: Harness[] = [];
	const previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		ravoFake.FakeRavoRunService.instances.length = 0;
		if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
		else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	});

	async function ravoHarness(): Promise<Harness> {
		const harness = await createHarness({ persistSession: true, rlmDepth: 0 });
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		return harness;
	}

	it("ravo.run starts a background run and forwards service updates as ravo_run_update events", async () => {
		const harness = await ravoHarness();
		const result = harness.session.handleRavoHostRequest("ravo.run", {
			task: "turn the deploy checklist into a skill",
			instructions: "keep it short",
			global: true,
			max_rounds: 3,
			max_repairs: 2,
		});
		expect(result).toMatchObject({ started: true, runId: "run-1" });

		const fake = lastFake();
		expect(fake.startCalls).toEqual([
			{
				task: "turn the deploy checklist into a skill",
				instructions: "keep it short",
				global: true,
				maxRounds: 3,
				maxRepairs: 2,
			} satisfies RavoRunRequest,
		]);
		const expectedLocalDir = join(harness.sessionManager.getSessionArtifactDir() ?? "", "harness");
		expect(fake.deps.harnessDir).toBe(expectedLocalDir);
		expect(fake.deps.globalHarnessDir).toBe(join(harness.tempDir, "agent", "harness"));

		expect(harness.eventsOfType("ravo_run_update")).toHaveLength(0);
		fake.emit({ phase: "plan", round: 1 });
		const updates = harness.eventsOfType("ravo_run_update");
		expect(updates).toHaveLength(1);
		expect(updates[0].status).toMatchObject({ runId: "run-1", phase: "plan", round: 1 });
		expect(harness.session.handleRavoHostRequest("ravo.status")).toMatchObject({
			runId: "run-1",
			phase: "plan",
			round: 1,
		});

		const second = harness.session.handleRavoHostRequest("ravo.run", { task: "another" });
		expect(second).toMatchObject({ started: false, reason: expect.stringContaining("run-1") });
		expect(fake.startCalls).toHaveLength(1);

		fake.finish("accepted");
		await Promise.resolve();
		expect(harness.session.handleRavoHostRequest("ravo.status")).toMatchObject({
			stopReason: "accepted",
		});
	});

	it("ravo.status is idle before any run and ravo.cancel forwards to the service", async () => {
		const harness = await ravoHarness();
		expect(harness.session.handleRavoHostRequest("ravo.status")).toEqual({ phase: "idle" });
		expect(harness.session.handleRavoHostRequest("ravo.cancel")).toEqual({ cancelled: false });
		expect(lastFake().cancelCalls).toBe(1);

		harness.session.handleRavoHostRequest("ravo.run", { task: "x" });
		expect(harness.session.handleRavoHostRequest("ravo.cancel")).toEqual({ cancelled: true });
		expect(lastFake().cancelCalls).toBe(2);
		lastFake().finish("cancelled");
	});

	it("rejects malformed ravo.run payloads and unknown request types", async () => {
		const harness = await ravoHarness();
		expect(() => harness.session.handleRavoHostRequest("ravo.run", {})).toThrow(/task must be a non-empty string/);
		expect(() => harness.session.handleRavoHostRequest("ravo.run", { task: "   " })).toThrow(
			/task must be a non-empty string/,
		);
		expect(() => harness.session.handleRavoHostRequest("ravo.run", { task: "t", instructions: 3 })).toThrow(
			/instructions must be a string/,
		);
		expect(() => harness.session.handleRavoHostRequest("ravo.run", { task: "t", global: "yes" })).toThrow(
			/global must be a boolean/,
		);
		expect(() => harness.session.handleRavoHostRequest("ravo.run", { task: "t", max_rounds: 0 })).toThrow(
			/max_rounds must be a positive integer/,
		);
		expect(() => harness.session.handleRavoHostRequest("ravo.run", { task: "t", deadline_ms: 1.5 })).toThrow(
			/deadline_ms must be a positive integer/,
		);
		expect(() => harness.session.handleRavoHostRequest("ravo.run", { task: "t", token_budget: "1" })).toThrow(
			/token_budget must be a positive integer/,
		);
		expect(() => harness.session.handleRavoHostRequest("ravo.bogus")).toThrow(/unknown ravo request type/);
		expect(ravoFake.FakeRavoRunService.instances.every((instance) => instance.startCalls.length === 0)).toBe(true);
	});

	it("is unavailable to RLM children", async () => {
		const harness = await createHarness({ persistSession: true, rlmDepth: 1 });
		harnesses.push(harness);
		expect(harness.session.handleRavoHostRequest("ravo.run", { task: "t" })).toMatchObject({
			started: false,
			reason: expect.stringContaining("not available"),
		});
		expect(harness.session.handleRavoHostRequest("ravo.status")).toEqual({ phase: "idle" });
		expect(ravoFake.FakeRavoRunService.instances).toHaveLength(0);
	});

	it("/ravo appends the started row and the terminal row when the run settles", async () => {
		const harness = await ravoHarness();
		await harness.session.prompt("/ravo --rounds 2 promote the retry policy --global");
		await harness.session.waitForIdle();

		const fake = lastFake();
		expect(fake.startCalls).toEqual([{ task: "promote the retry policy", global: true, maxRounds: 2 }]);
		expect(commandResultTexts(harness)).toEqual(["RAVO run run-1 started: promote the retry policy"]);

		fake.finish("round_limit");
		await vi.waitFor(() =>
			expect(commandResultTexts(harness)).toEqual([
				"RAVO run run-1 started: promote the retry policy",
				"RAVO run run-1 round_limit",
			]),
		);
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows.every((row) => row.details.success)).toBe(true);
		expect(rows.every((row) => row.details.command.name === "ravo")).toBe(true);
	});

	it("/ravo reports a run failure as an error row", async () => {
		const harness = await ravoHarness();
		await harness.session.prompt("/ravo fix the thing");
		await harness.session.waitForIdle();
		lastFake().fail(new Error("controller crashed"));
		await vi.waitFor(() =>
			expect(commandResultTexts(harness)).toEqual([
				"RAVO run run-1 started: fix the thing",
				"Command failed: RAVO run run-1 failed: controller crashed",
			]),
		);
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows.at(-1)?.details.success).toBe(false);
	});

	it("/ravo without a task fails the command", async () => {
		const harness = await ravoHarness();
		await harness.session.prompt("/ravo --global");
		await harness.session.waitForIdle();
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows).toHaveLength(1);
		expect(rows[0].details.success).toBe(false);
		expect(rows[0].content).toContain(
			"Usage: /ravo [--global] [--rounds N] [--repairs N] [--arc-repo DIR --arc-game ID] <task>",
		);
		expect(ravoFake.FakeRavoRunService.instances.every((instance) => instance.startCalls.length === 0)).toBe(true);
	});

	it("dispose cancels an in-flight run", async () => {
		const harness = await ravoHarness();
		harness.session.handleRavoHostRequest("ravo.run", { task: "t" });
		const fake = lastFake();
		harness.session.dispose();
		expect(fake.cancelCalls).toBe(1);
		fake.finish("cancelled");
	});
});

describe("/ravo argument parsing", () => {
	it("is a session slash command", () => {
		expect(parseSessionSlashCommand("/ravo do it")).toEqual({ name: "ravo", args: "do it", text: "/ravo do it" });
	});

	it("accepts flags anywhere and keeps the rest as the task", () => {
		expect(parseRavoCommandOptions("do the thing")).toEqual({ task: "do the thing", global: false });
		expect(parseRavoCommandOptions("--global --rounds 4 --repairs=1 do the\tthing")).toEqual({
			task: "do the thing",
			global: true,
			maxRounds: 4,
			maxRepairs: 1,
		});
		expect(parseRavoCommandOptions("do the thing --rounds=2")).toEqual({
			task: "do the thing",
			global: false,
			maxRounds: 2,
		});
	});

	it("rejects missing tasks and invalid counts", () => {
		expect(() => parseRavoCommandOptions("")).toThrow("Usage: /ravo");
		expect(() => parseRavoCommandOptions("--global")).toThrow("Usage: /ravo");
		expect(parseRavoCommandOptions("--arc-repo /tmp/arc --arc-game ls20 play ls20")).toEqual({
			task: "play ls20",
			global: false,
			evaluator: { kind: "arc-agi", repoDir: "/tmp/arc", game: "ls20" },
		});
		expect(() => parseRavoCommandOptions("--arc-game ls20 play")).toThrow("Usage: /ravo");
		expect(() => parseRavoCommandOptions("--rounds x task")).toThrow("--rounds expects a positive integer");
		expect(() => parseRavoCommandOptions("task --repairs 0")).toThrow("--repairs expects a positive integer");
		expect(() => parseRavoCommandOptions("task --rounds")).toThrow("--rounds expects a positive integer");
	});
});

describe("bundled ravo skill", () => {
	it("loads as a python skill named ravo", () => {
		const { skills, diagnostics } = loadSkillsFromDir({ dir: getBundledSkillsDir(), source: "builtin" });
		expect(diagnostics).toEqual([]);
		const ravo = skills.find((skill) => skill.name === "ravo");
		expect(ravo).toBeDefined();
		expect(ravo?.kind).toBe("python");
		expect(ravo?.kind === "python" && ravo.python.importName).toBe("ravo");
	});
});
