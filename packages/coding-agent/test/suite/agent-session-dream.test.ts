import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { isSessionSlashCommandResultMessage } from "../../src/core/messages.js";
import { createHarness, type Harness } from "./harness.js";

/**
 * End-to-end coverage of the in-session Dream-RSI surfaces against the faux
 * provider. Every run here uses the local zero-token proposer and dreamer, so
 * `harness.faux.state.callCount` must stay 0: a real model call would prove the
 * default path spent tokens, which it must never do.
 */

function commandResultTexts(harness: Harness): string[] {
	return harness.session.messages
		.filter((message) => isSessionSlashCommandResultMessage(message))
		.map((message) => (typeof message.content === "string" ? message.content : JSON.stringify(message.content)));
}

describe("agent-session Dream-RSI entry points (faux provider)", () => {
	const harnesses: Harness[] = [];
	const previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	const previousDreamDir = process.env.PRIME_AGENT_DREAM_DIR;

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
		else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
		if (previousDreamDir === undefined) delete process.env.PRIME_AGENT_DREAM_DIR;
		else process.env.PRIME_AGENT_DREAM_DIR = previousDreamDir;
	});

	async function dreamHarness(): Promise<Harness> {
		const harness = await createHarness({ persistSession: true, rlmDepth: 0 });
		harnesses.push(harness);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = join(harness.tempDir, "agent");
		process.env.PRIME_AGENT_DREAM_DIR = join(harness.tempDir, "dream-store");
		return harness;
	}

	it("a bare /dream (local proposer) completes and reports a result with zero real tokens", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt("/dream --iterations 1 --workers 1 --k1 2 --k2 4 --dreams 1 --seed 3");
		await harness.session.waitForIdle();

		expect(commandResultTexts(harness).some((text) => /^Dream-RSI run .+ started: circle-packing$/.test(text))).toBe(
			true,
		);
		await vi.waitFor(() =>
			expect(commandResultTexts(harness).some((text) => /^Dream-RSI run .+ completed$/.test(text))).toBe(true),
		);

		expect(harness.faux.state.callCount).toBe(0);
		const updates = harness.eventsOfType("dream_run_update");
		expect(updates.length).toBeGreaterThan(0);
		expect(updates[0].status.phase).toBe("rollout");
		const terminal = updates.at(-1)?.status;
		expect(terminal?.stopReason).toBe("completed");
		expect(["accepted", "stopped"]).toContain(terminal?.phase);
		expect(typeof terminal?.finalPolicyScore).toBe("number");
		expect(typeof terminal?.improved).toBe("boolean");

		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows.every((row) => row.details.command.name === "dream")).toBe(true);
		expect(rows.every((row) => row.details.success)).toBe(true);
	});

	it("dream.run starts a background run, dream.status reflects it, and dream.cancel aborts it", async () => {
		const harness = await dreamHarness();
		const started = harness.session.handleDreamHostRequest("dream.run", {
			task: "circle-packing",
			iterations: 3,
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 5,
		});
		expect(started).toMatchObject({ started: true });
		const runId = (started as { runId: string }).runId;
		expect(runId).toMatch(/^dream_/);

		expect(harness.session.handleDreamHostRequest("dream.status")).toMatchObject({
			runId,
			task: "circle-packing",
		});

		// A second run is refused while one is in progress.
		const second = harness.session.handleDreamHostRequest("dream.run", { task: "sum-difference" });
		expect(second).toMatchObject({ started: false, reason: expect.stringContaining(runId) });

		expect(harness.session.handleDreamHostRequest("dream.cancel")).toEqual({ cancelled: true });
		await vi.waitFor(() => {
			const status = harness.session.handleDreamHostRequest("dream.status") as { stopReason?: string };
			expect(status.stopReason).toBe("cancelled");
		});

		expect(harness.faux.state.callCount).toBe(0);
		const updates = harness.eventsOfType("dream_run_update");
		expect(updates.at(-1)?.status).toMatchObject({ phase: "stopped", stopReason: "cancelled" });
	});

	it("dream.experiment with seeds runs every seed under one run id at zero tokens and lists every result path", async () => {
		const harness = await dreamHarness();
		const started = harness.session.handleDreamHostRequest("dream.experiment", {
			task: "sum-difference",
			seeds: [5, 6],
			rounds: 2,
			arms: ["dream", "fixed"],
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
		});
		expect(started).toMatchObject({ started: true, seeds: 2 });
		const runId = (started as { runId: string }).runId;
		await vi.waitFor(() => {
			const status = harness.session.handleDreamHostRequest("dream.status") as { stopReason?: string };
			expect(status.stopReason).toBe("completed");
		});
		const status = harness.session.handleDreamHostRequest("dream.status") as {
			runId: string;
			seedIndex: number;
			seedCount: number;
			resultPath: string;
			resultPaths: string[];
			tokens?: number;
		};
		expect(status.runId).toBe(runId);
		expect(status.seedCount).toBe(2);
		expect(status.seedIndex).toBe(1);
		expect(status.resultPaths).toHaveLength(2);
		expect(status.resultPaths[0]).toMatch(/sum-difference-s5-n2-\d+\/result\.json$/);
		expect(status.resultPaths[1]).toMatch(/sum-difference-s6-n2-\d+\/result\.json$/);
		expect(status.resultPath).toBe(status.resultPaths[1]);
		expect(status.tokens).toBeUndefined();
		expect(harness.faux.state.callCount).toBe(0);
		const updates = harness.eventsOfType("dream_run_update");
		expect(updates.every((update) => update.status.runId === runId)).toBe(true);
		expect(new Set(updates.map((update) => update.status.seedIndex))).toEqual(new Set([0, 1]));
	});

	it("dream.status is idle before any run, dream.cancel is false, and malformed payloads are rejected", async () => {
		const harness = await dreamHarness();
		expect(harness.session.handleDreamHostRequest("dream.status")).toEqual({ phase: "idle" });
		expect(harness.session.handleDreamHostRequest("dream.cancel")).toEqual({ cancelled: false });

		expect(() => harness.session.handleDreamHostRequest("dream.run", {})).toThrow(/task must be one of/);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "nope" })).toThrow(
			/task must be one of/,
		);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", workers: 0 })).toThrow(
			/workers must be a positive integer/,
		);
		expect(() => harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", seed: -1 })).toThrow(
			/seed must be a non-negative integer/,
		);
		expect(() =>
			harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing", llm_proposer: "yes" }),
		).toThrow(/llm_proposer must be a boolean/);
		expect(() => harness.session.handleDreamHostRequest("dream.bogus")).toThrow(/unknown dream request type/);

		expect(harness.faux.state.callCount).toBe(0);
	});

	it("/dream with a bad flag fails the command with the usage string", async () => {
		const harness = await dreamHarness();
		await harness.session.prompt("/dream --task not-a-task");
		await harness.session.waitForIdle();
		const rows = harness.session.messages.filter((message) => isSessionSlashCommandResultMessage(message));
		expect(rows).toHaveLength(1);
		expect(rows[0].details.success).toBe(false);
		expect(rows[0].content).toContain("Usage: /dream");
		expect(harness.faux.state.callCount).toBe(0);
	});

	it("is unavailable to RLM children", async () => {
		const harness = await createHarness({ persistSession: true, rlmDepth: 1 });
		harnesses.push(harness);
		expect(harness.session.handleDreamHostRequest("dream.run", { task: "circle-packing" })).toMatchObject({
			started: false,
			reason: expect.stringContaining("not available"),
		});
		expect(harness.session.handleDreamHostRequest("dream.status")).toEqual({ phase: "idle" });
	});
});
