import { afterEach, describe, expect, it, vi } from "vitest";
import { createHarness, type Harness } from "./harness.js";

type SessionInternals = {
	_consumePendingRequestedRefine: () => boolean;
	_emitRefineFailed: (error: unknown) => void;
	_pendingRequestedRefine: { instructions?: string; global?: boolean; reason?: string } | undefined;
	_serializedPlanInFlight?: Promise<unknown>;
	_serializedExplicitRefineOptions?: { instructions?: string; global?: boolean; reason?: string };
	_refineAbortController?: AbortController;
	_createKernelHostHandlers: () => Record<string, unknown>;
	refine: (options: { instructions?: string; global?: boolean }) => Promise<unknown>;
};

type FailureQueueInternals = {
	_queueFailureTriggeredRefine(
		instructions: string,
		reason: "recurrence" | "regression",
		fingerprintIds: readonly string[],
		global?: boolean,
	): void;
	_deferredRefineRequests: unknown[];
};

function setStreaming(harness: Harness, streaming: boolean) {
	(harness.session.agent.state as { isStreaming: boolean }).isStreaming = streaming;
}

describe("AgentSession refine skill host requests", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		vi.restoreAllMocks();
		while (harnesses.length > 0) {
			harnesses.pop()?.cleanup();
		}
	});

	it("schedules via refine.run and reports pending via refine.status", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		const runResult = harness.session.handleRefineHostRequest("refine.run", { instructions: "test instructions" });
		setStreaming(harness, false);
		expect(runResult.scheduled).toBe(true);
		expect(runResult.note).toBeDefined();

		const status = harness.session.handleRefineHostRequest("refine.status");
		expect(status.pending).toBe(true);
	});

	it("stores global flag from refine.run", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { global: true });
		setStreaming(harness, false);

		const internals = harness.session as unknown as SessionInternals;
		expect(internals._pendingRequestedRefine?.global).toBe(true);
	});

	it("defaults to local scope when global is not provided", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run");
		setStreaming(harness, false);

		const internals = harness.session as unknown as SessionInternals;
		expect(internals._pendingRequestedRefine?.global).toBeUndefined();
		expect(internals._pendingRequestedRefine?.instructions).toBeUndefined();
	});

	it("updates pending request when called again", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "first", global: true });
		harness.session.handleRefineHostRequest("refine.run", { instructions: "second", global: true });
		setStreaming(harness, false);

		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;
		expect(internals._pendingRequestedRefine?.instructions).toBe("second");
		expect(internals._pendingRequestedRefine?.global).toBe(true);
		expect(internals._deferredRefineRequests).toEqual([]);
	});

	it("never turns a refine.run without the global flag into a global one", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "first", global: true });
		harness.session.handleRefineHostRequest("refine.run", { instructions: "second" });
		setStreaming(harness, false);

		expect(internals._pendingRequestedRefine).toEqual({ instructions: "first", global: true, reason: "refine_run" });
		expect(internals._deferredRefineRequests).toEqual([{ instructions: "second", reason: "refine_run" }]);
	});

	it("keeps the agent's local refine.run out of a queued global failure repair", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;
		const globalRepair = {
			instructions: "global regression",
			reason: "regression",
			kind: "failure",
			triggerFingerprintIds: ["fp_global"],
			global: true,
		};

		setStreaming(harness, true);
		internals._queueFailureTriggeredRefine("global regression", "regression", ["fp_global"], true);
		// The refine skill leaves the key out for global_=False.
		harness.session.handleRefineHostRequest("refine.run", {
			instructions: "remember this user prefers tabs in this repo",
		});
		harness.session.handleRefineHostRequest("refine.run", { instructions: "and spaces in markdown" });
		setStreaming(harness, false);

		expect(internals._pendingRequestedRefine).toEqual(globalRepair);
		expect(internals._deferredRefineRequests).toEqual([
			{ instructions: "and spaces in markdown", reason: "refine_run" },
		]);

		const refineSpy = vi.spyOn(internals, "refine").mockResolvedValue({});
		expect(internals._consumePendingRequestedRefine()).toBe(true);
		expect(refineSpy.mock.calls).toEqual([
			[
				{ instructions: "global regression", global: true },
				{
					source: "self",
					reason: "regression",
					queueVersion: expect.any(Number),
					kind: "failure",
					triggerFingerprintIds: ["fp_global"],
				},
			],
			[
				{ instructions: "and spaces in markdown", global: undefined },
				{ source: "self", reason: "refine_run", queueVersion: expect.any(Number) },
			],
		]);
	});

	it("runs merged pending requests under the strongest reason without losing a failure list", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "remember the deploy flag" });
		expect(internals._pendingRequestedRefine?.reason).toBe("refine_run");
		internals._queueFailureTriggeredRefine("recurrence list", "recurrence", ["fp_a"]);
		// The agent's own request is in it, so it is gated as directed, not as a failure refine.
		expect(internals._pendingRequestedRefine).toEqual({
			instructions: "remember the deploy flag\n\nrecurrence list",
			reason: "recurrence",
			kind: "directed",
			triggerFingerprintIds: ["fp_a"],
		});
		internals._queueFailureTriggeredRefine("regression list", "regression", ["fp_b"]);
		internals._queueFailureTriggeredRefine("second recurrence list", "recurrence", ["fp_a", "fp_c"]);
		expect(internals._pendingRequestedRefine?.reason).toBe("regression");
		// A later refine.run neither downgrades the reason nor drops the failure lists it was merged into.
		harness.session.handleRefineHostRequest("refine.run", { instructions: "and this" });
		setStreaming(harness, false);
		expect(internals._pendingRequestedRefine).toEqual({
			instructions:
				"remember the deploy flag\n\nrecurrence list\n\nregression list\n\nsecond recurrence list\n\nand this",
			reason: "regression",
			kind: "directed",
			triggerFingerprintIds: ["fp_a", "fp_b", "fp_c"],
		});

		const refineSpy = vi.spyOn(internals, "refine").mockResolvedValue({});
		internals._consumePendingRequestedRefine();
		expect(refineSpy).toHaveBeenCalledWith(
			{ instructions: expect.stringContaining("regression list"), global: undefined },
			{
				source: "self",
				reason: "regression",
				queueVersion: expect.any(Number),
				kind: "directed",
				triggerFingerprintIds: ["fp_a", "fp_b", "fp_c"],
			},
		);
	});

	it("keeps a merged request a failure refine only while every part of it is one", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;

		internals._queueFailureTriggeredRefine("recurrence list", "recurrence", ["fp_a"]);
		internals._queueFailureTriggeredRefine("regression list", "regression", ["fp_b"]);
		expect(internals._pendingRequestedRefine).toEqual({
			instructions: "recurrence list\n\nregression list",
			reason: "regression",
			kind: "failure",
			triggerFingerprintIds: ["fp_a", "fp_b"],
		});

		const refineSpy = vi.spyOn(internals, "refine").mockResolvedValue({});
		expect(internals._consumePendingRequestedRefine()).toBe(true);
		expect(refineSpy).toHaveBeenCalledWith(
			{ instructions: "recurrence list\n\nregression list", global: undefined },
			{
				source: "self",
				reason: "regression",
				queueVersion: expect.any(Number),
				kind: "failure",
				triggerFingerprintIds: ["fp_a", "fp_b"],
			},
		);
	});

	it("queues a global repair apart from a local one and runs both, each in its own scope", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;

		internals._queueFailureTriggeredRefine("local regression", "regression", ["fp_local"]);
		internals._queueFailureTriggeredRefine("global regression", "regression", ["fp_global"], true);
		internals._queueFailureTriggeredRefine("local recurrence", "recurrence", ["fp_other"]);
		expect(internals._pendingRequestedRefine).toEqual({
			instructions: "local regression\n\nlocal recurrence",
			reason: "regression",
			kind: "failure",
			triggerFingerprintIds: ["fp_local", "fp_other"],
		});
		expect(internals._deferredRefineRequests).toEqual([
			{
				instructions: "global regression",
				reason: "regression",
				kind: "failure",
				triggerFingerprintIds: ["fp_global"],
				global: true,
			},
		]);
		expect(harness.session.handleRefineHostRequest("refine.status").pending).toBe(true);

		const refineSpy = vi.spyOn(internals, "refine").mockResolvedValue({});
		expect(internals._consumePendingRequestedRefine()).toBe(true);
		expect(refineSpy.mock.calls).toEqual([
			[
				{ instructions: "local regression\n\nlocal recurrence", global: undefined },
				{
					source: "self",
					reason: "regression",
					queueVersion: expect.any(Number),
					kind: "failure",
					triggerFingerprintIds: ["fp_local", "fp_other"],
				},
			],
			[
				{ instructions: "global regression", global: true },
				{
					source: "self",
					reason: "regression",
					queueVersion: expect.any(Number),
					kind: "failure",
					triggerFingerprintIds: ["fp_global"],
				},
			],
		]);
		expect(harness.session.handleRefineHostRequest("refine.status").pending).toBe(false);
	});

	it("does not let a refine.run of the other scope absorb a queued failure repair", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;

		setStreaming(harness, true);
		internals._queueFailureTriggeredRefine("local recurrence", "recurrence", ["fp_a"]);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "share this", global: true });
		setStreaming(harness, false);

		expect(internals._pendingRequestedRefine).toEqual({
			instructions: "local recurrence",
			reason: "recurrence",
			kind: "failure",
			triggerFingerprintIds: ["fp_a"],
		});
		expect(internals._deferredRefineRequests).toEqual([
			{ instructions: "share this", global: true, reason: "refine_run" },
		]);
	});

	it("drops parked repairs along with the pending request on dispose", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;

		internals._queueFailureTriggeredRefine("local regression", "regression", ["fp_local"]);
		internals._queueFailureTriggeredRefine("global regression", "regression", ["fp_global"], true);
		harness.session.dispose();

		expect(internals._pendingRequestedRefine).toBeUndefined();
		expect(internals._deferredRefineRequests).toEqual([]);
	});

	it("replaces an in-flight serialized plan instead of applying both requests", async () => {
		const harness = await createHarness({ persistSession: true, serializedRefine: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals;
		const abort = new AbortController();
		internals._serializedPlanInFlight = new Promise(() => {});
		internals._serializedExplicitRefineOptions = { instructions: "first", global: true };
		internals._refineAbortController = abort;

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "replacement", global: true });
		setStreaming(harness, false);

		expect(abort.signal.aborted).toBe(true);
		expect(internals._pendingRequestedRefine).toEqual({
			instructions: "replacement",
			global: true,
			reason: "refine_run",
		});
	});

	it("discards a settled serialized plan when a replacement request arrives", async () => {
		const harness = await createHarness({ persistSession: true, serializedRefine: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals;
		internals._serializedPlanInFlight = Promise.resolve({ status: "plan" });
		internals._serializedExplicitRefineOptions = { instructions: "first", global: true };

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "replacement", global: true });
		setStreaming(harness, false);

		await expect(internals._serializedPlanInFlight).resolves.toEqual({
			status: "invalidated",
			branchVersion: expect.any(Number),
		});
		expect(internals._pendingRequestedRefine).toEqual({
			instructions: "replacement",
			global: true,
			reason: "refine_run",
		});
	});

	it("leaves a global repair planning in flight alone and parks a refine.run without the global flag", async () => {
		const harness = await createHarness({ persistSession: true, serializedRefine: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals & FailureQueueInternals;
		const abort = new AbortController();
		const planning = new Promise<unknown>(() => {});
		internals._serializedPlanInFlight = planning;
		internals._serializedExplicitRefineOptions = {
			instructions: "global regression",
			global: true,
			reason: "regression",
		};
		internals._refineAbortController = abort;

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", {
			instructions: "remember this user prefers tabs in this repo",
		});
		setStreaming(harness, false);

		expect(abort.signal.aborted).toBe(false);
		expect(internals._serializedPlanInFlight).toBe(planning);
		expect(internals._pendingRequestedRefine).toBeUndefined();
		expect(internals._deferredRefineRequests).toEqual([
			{ instructions: "remember this user prefers tabs in this repo", reason: "refine_run" },
		]);
		expect(harness.session.handleRefineHostRequest("refine.status").pending).toBe(true);
	});

	it("rejects refine.run while no turn is active", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		const result = harness.session.handleRefineHostRequest("refine.run");
		expect(result.scheduled).toBe(false);
		expect(result.reason).toContain("no active turn");
		expect(harness.session.handleRefineHostRequest("refine.status").pending).toBe(false);
	});

	it("validates instructions type in refine.run", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");

		setStreaming(harness, true);
		expect(() =>
			harness.session.handleRefineHostRequest("refine.run", { instructions: 123 as unknown as string }),
		).toThrow("instructions must be a string");
		setStreaming(harness, false);
	});

	it("validates global flag type in refine.run", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");

		setStreaming(harness, true);
		expect(() =>
			harness.session.handleRefineHostRequest("refine.run", { global: "yes" as unknown as boolean }),
		).toThrow("global must be a boolean");
		setStreaming(harness, false);
	});

	it("reports in_flight as false when no refine is active", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");

		const status = harness.session.handleRefineHostRequest("refine.status");
		expect(status.in_flight).toBe(false);
		expect(status.pending).toBe(false);
	});

	it("consumes pending refine request at turn boundary", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "test" });
		setStreaming(harness, false);

		const internals = harness.session as unknown as SessionInternals;
		const refineSpy = vi.spyOn(internals, "refine").mockResolvedValue({});
		internals._consumePendingRequestedRefine();
		expect(refineSpy).toHaveBeenCalledWith(
			{ instructions: "test", global: undefined },
			{ source: "self", reason: "refine_run", queueVersion: expect.any(Number) },
		);
		expect(internals._pendingRequestedRefine).toBeUndefined();
	});

	it("does nothing when no pending refine at turn boundary", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");

		const internals = harness.session as unknown as SessionInternals;
		const refineSpy = vi.spyOn(internals, "refine").mockResolvedValue({});
		expect(internals._consumePendingRequestedRefine()).toBe(false);
		expect(refineSpy).not.toHaveBeenCalled();
	});

	it("catches errors from refine at turn boundary without throwing", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "test" });
		setStreaming(harness, false);

		const internals = harness.session as unknown as SessionInternals;
		vi.spyOn(internals, "refine").mockRejectedValue(new Error("refine failed"));
		const failed = new Promise<string>((resolve) => {
			const unsubscribe = harness.session.subscribe((event) => {
				if (event.type === "refine_failed") {
					unsubscribe();
					resolve(event.error);
				}
			});
		});

		expect(internals._consumePendingRequestedRefine()).toBe(true);
		expect(await failed).toBe("refine failed");
		expect(internals._pendingRequestedRefine).toBeUndefined();
	});

	it("continues notifying refine listeners after one throws", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		const internals = harness.session as unknown as SessionInternals;
		const observed: string[] = [];
		harness.session.subscribe(() => {
			throw new Error("broken listener");
		});
		harness.session.subscribe((event) => {
			if (event.type === "refine_failed") {
				observed.push(event.error);
			}
		});

		expect(() => internals._emitRefineFailed(new Error("planning failed"))).not.toThrow();
		expect(observed).toEqual(["planning failed"]);
	});

	it("registers refine.run and refine.status handlers when auto-refine is allowed", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");

		const internals = harness.session as unknown as SessionInternals;
		const handlerKeys = Object.keys(internals._createKernelHostHandlers());
		expect(handlerKeys).toEqual(expect.arrayContaining(["refine.run", "refine.status"]));
	});

	it("does not register refine handlers when auto-refine is not allowed (rlmDepth > 0)", async () => {
		const harness = await createHarness({ persistSession: true, rlmDepth: 1 });
		harnesses.push(harness);
		await harness.session.prompt("one");

		const internals = harness.session as unknown as SessionInternals;
		const handlerKeys = Object.keys(internals._createKernelHostHandlers());
		expect(handlerKeys).not.toContain("refine.run");
		expect(handlerKeys).not.toContain("refine.status");
	});

	it("does not register refine handlers without a persisted session", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		await harness.session.prompt("one");

		const internals = harness.session as unknown as SessionInternals;
		const handlerKeys = Object.keys(internals._createKernelHostHandlers());
		expect(handlerKeys).not.toContain("refine.run");
		expect(handlerKeys).not.toContain("refine.status");
	});

	it("clears pending refine on dispose", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");
		await harness.session.prompt("two");

		setStreaming(harness, true);
		harness.session.handleRefineHostRequest("refine.run", { instructions: "test" });
		setStreaming(harness, false);

		expect(harness.session.handleRefineHostRequest("refine.status").pending).toBe(true);
		harness.session.dispose();
		expect(harness.session.handleRefineHostRequest("refine.status").pending).toBe(false);
	});

	it("rejects unknown refine request type", async () => {
		const harness = await createHarness({ persistSession: true });
		harnesses.push(harness);
		await harness.session.prompt("one");

		expect(() => harness.session.handleRefineHostRequest("refine.unknown")).toThrow(
			'unknown refine request type "refine.unknown"',
		);
	});
});
