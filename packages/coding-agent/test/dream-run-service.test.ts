import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { createSeededRng } from "../src/core/dream/rng.js";
import { type DreamRunRequest, DreamRunService, type DreamRunStatus } from "../src/core/dream/run-service.js";
import type { RunAgentHandler } from "../src/core/run-agent.js";

/**
 * Unit coverage of DreamRunService against a STUB RunAgentHandler that throws if
 * ever invoked: the default (local) path must never call it, so any call fails
 * the test and proves the run spent tokens. Determinism flows through the
 * injected SeededRng and clock.
 */

const failingRunAgent: RunAgentHandler = async () => {
	throw new Error("runAgent must not be called on the local Dream-RSI path");
};

const dirs: string[] = [];

function tempDir(): string {
	const dir = mkdtempSync(join(tmpdir(), "dream-run-service-"));
	dirs.push(dir);
	return dir;
}

afterEach(() => {
	while (dirs.length > 0) {
		const dir = dirs.pop();
		if (dir) rmSync(dir, { recursive: true, force: true });
	}
});

describe("DreamRunService", () => {
	it("runs the local zero-token path without ever calling runAgent and emits ordered phases", async () => {
		const updates: DreamRunStatus[] = [];
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			rng: createSeededRng(5),
			onUpdate: (status) => updates.push(status),
		});
		const request: DreamRunRequest = {
			task: "circle-packing",
			iterations: 1,
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 5,
		};
		const status = await service.start(request);

		expect(status.stopReason).toBe("completed");
		expect(["accepted", "stopped"]).toContain(status.phase);
		expect(status.task).toBe("circle-packing");
		expect(typeof status.finalPolicyScore).toBe("number");
		expect(typeof status.improved).toBe("boolean");
		expect(service.running).toBe(false);

		const phases = updates.map((update) => update.phase);
		expect(phases[0]).toBe("rollout");
		expect(phases).toContain("dreaming");
		expect(phases).toContain("redeploying");
		expect(updates.at(-1)?.stopReason).toBe("completed");
	});

	it("status() returns a defensive clone", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			rng: createSeededRng(1),
			onUpdate: () => {},
		});
		await service.start({ task: "circle-packing", iterations: 0, workers: 1, k1: 2, k2: 4, dreams: 1, seed: 1 });
		const first = service.status();
		const second = service.status();
		expect(first).toEqual(second);
		expect(first).not.toBe(second);
	});

	it("a cancel mid-run resolves with stopReason cancelled and never rethrows", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			rng: createSeededRng(5),
			onUpdate: () => {},
		});
		const completion = service.start({
			task: "circle-packing",
			iterations: 5,
			workers: 1,
			k1: 2,
			k2: 4,
			dreams: 1,
			seed: 5,
		});
		expect(service.cancel()).toBe(true);
		const status = await completion;
		expect(status.stopReason).toBe("cancelled");
		expect(status.phase).toBe("stopped");
		expect(status.error).toBeUndefined();
		expect(service.running).toBe(false);
	});

	it("a non-abort failure sets phase stopped with an error message and rejects", async () => {
		const service = new DreamRunService({
			runAgent: failingRunAgent,
			dir: tempDir(),
			now: () => 1000,
			onUpdate: () => {},
		});
		// circle-packing requires n >= 2; resolveTask throws before the loop starts.
		await expect(service.start({ task: "circle-packing", n: 1 })).rejects.toThrow(/n >= 2/);
		expect(service.status()?.phase).toBe("stopped");
		expect(service.status()?.error).toMatch(/n >= 2/);
		expect(service.running).toBe(false);
	});

	it("is deterministic: the same seed and clock yield the same scores", async () => {
		const run = async (): Promise<DreamRunStatus> => {
			const service = new DreamRunService({
				runAgent: failingRunAgent,
				dir: tempDir(),
				now: () => 1000,
				rng: createSeededRng(9),
				onUpdate: () => {},
			});
			return service.start({ task: "circle-packing", iterations: 2, workers: 2, k1: 3, k2: 6, dreams: 2, seed: 9 });
		};
		const first = await run();
		const second = await run();
		expect(second.finalPolicyScore).toBe(first.finalPolicyScore);
		expect(second.bestNodeScore).toBe(first.bestNodeScore);
		expect(second.improved).toBe(first.improved);
	});
});
