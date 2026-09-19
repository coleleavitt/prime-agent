import { afterEach, describe, expect, it, vi } from "vitest";
import type { AutoRefineReview } from "../../src/core/refinement/index.js";
import { createHarness, type Harness } from "./harness.js";

/**
 * Coverage for the auto-refine scope policy. Scope is MOST PERMISSIVE by default:
 * an auto-refine review with no scope (or an explicit "global") threads
 * `global: true` through every auto path (serialized review, interactive approved
 * refine, serialized background plan) and gets the global planner instructions.
 * Only an explicit "local" scope keeps a refine session-scoped with the
 * do-not-promote-global wording.
 */

type ScopeInternals = {
	_runSerializedRefineCheckpoint(): Promise<void>;
	_runApprovedRefine(reason: string, review: AutoRefineReview): Promise<void>;
	_maybeStartSerializedBackgroundPlan(): void;
	_planRefine(
		options: { global?: boolean; instructions?: string },
		signal: AbortSignal,
		trigger?: unknown,
		source?: unknown,
	): Promise<unknown>;
	_applyRefine(
		plan: unknown,
		options: { global?: boolean; instructions?: string },
		abort: AbortController,
		source: unknown,
	): Promise<unknown>;
	_assistantTurnsSinceAutoRefine: number;
	_serializedPlanInFlight?: Promise<unknown>;
	_serializedRefine: boolean;
};

function emptyRefinementResult() {
	return {
		id: "refine_test",
		summary: "test refinement",
		rationale: "test rationale",
		expectedOutcome: "test outcome",
		appliedEdits: [],
		harnessStatePath: "/tmp/harness_state.json",
	};
}

const EXPLICIT_GLOBAL: AutoRefineReview = {
	shouldRefine: true,
	rationale: "standing preference",
	scope: "global",
	instructions: "always run npm run check before finishing",
};

const DEFAULT_SCOPE: AutoRefineReview = {
	shouldRefine: true,
	rationale: "durable lesson",
	instructions: "prefer ripgrep over grep",
};

const EXPLICIT_LOCAL: AutoRefineReview = {
	shouldRefine: true,
	rationale: "session-only progress",
	scope: "local",
	instructions: "record the current task state",
};

describe("Auto-refine durable scope (global by default)", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		vi.restoreAllMocks();
		while (harnesses.length > 0) {
			harnesses.pop()?.cleanup();
		}
	});

	async function serializedHarness(review: AutoRefineReview): Promise<Harness> {
		const harness = await createHarness({
			persistSession: true,
			serializedRefine: true,
			settings: { autoRefine: { enabled: true, turnInterval: 1, cooldownMs: 0 } },
			autoRefineReviewer: vi.fn(async () => review),
		});
		harnesses.push(harness);
		return harness;
	}

	async function serializedGlobalFlag(review: AutoRefineReview): Promise<boolean | undefined> {
		const harness = await serializedHarness(review);
		const internals = harness.session as unknown as ScopeInternals;
		const planSpy = vi.spyOn(internals, "_planRefine").mockResolvedValue({ id: "p", proposal: { edits: [] } });
		vi.spyOn(internals, "_applyRefine").mockResolvedValue(emptyRefinementResult());
		internals._assistantTurnsSinceAutoRefine = 1;
		await internals._runSerializedRefineCheckpoint();
		expect(planSpy).toHaveBeenCalledTimes(1);
		return planSpy.mock.calls[0]?.[0].global;
	}

	it("T1: an explicit global review threads global scope into the serialized review path", async () => {
		expect(await serializedGlobalFlag(EXPLICIT_GLOBAL)).toBe(true);
	});

	it("T2: a review with no scope defaults to global in the serialized review path", async () => {
		expect(await serializedGlobalFlag(DEFAULT_SCOPE)).toBe(true);
	});

	it("T2b: only an explicit local review stays local in the serialized review path", async () => {
		expect(await serializedGlobalFlag(EXPLICIT_LOCAL)).toBeFalsy();
	});

	it("T3: the interactive approved refine defaults to global; explicit local stays local", async () => {
		const harness = await createHarness({
			persistSession: true,
			serializedRefine: false,
			settings: { autoRefine: { enabled: true, turnInterval: 1, cooldownMs: 0 } },
		});
		harnesses.push(harness);
		const internals = harness.session as unknown as ScopeInternals;
		const refineSpy = vi
			.spyOn(harness.session, "refine")
			.mockResolvedValue(emptyRefinementResult() as unknown as Awaited<ReturnType<typeof harness.session.refine>>);

		await internals._runApprovedRefine("turn_interval", DEFAULT_SCOPE);

		expect(refineSpy).toHaveBeenCalledTimes(1);
		const globalOptions = refineSpy.mock.calls[0]?.[0] as { global?: boolean; instructions?: string };
		expect(globalOptions.global).toBe(true);
		expect(globalOptions.instructions).toContain("global harness entries");
		expect(globalOptions.instructions).not.toContain("Do not promote anything global");

		refineSpy.mockClear();
		await internals._runApprovedRefine("turn_interval", EXPLICIT_LOCAL);

		expect(refineSpy).toHaveBeenCalledTimes(1);
		const localOptions = refineSpy.mock.calls[0]?.[0] as { global?: boolean; instructions?: string };
		expect(localOptions.global).toBeFalsy();
		expect(localOptions.instructions).toContain("Do not promote anything global");
	});

	it("T4: default scope reaches both plan and apply as global through the serialized background plan", async () => {
		const harness = await serializedHarness(DEFAULT_SCOPE);
		const internals = harness.session as unknown as ScopeInternals;
		const planSpy = vi.spyOn(internals, "_planRefine").mockResolvedValue({ id: "p", proposal: { edits: [] } });
		const applySpy = vi.spyOn(internals, "_applyRefine").mockResolvedValue(emptyRefinementResult());

		internals._assistantTurnsSinceAutoRefine = 1;
		internals._maybeStartSerializedBackgroundPlan();
		await internals._serializedPlanInFlight;
		await internals._runSerializedRefineCheckpoint();

		expect(planSpy).toHaveBeenCalledTimes(1);
		expect(applySpy).toHaveBeenCalledTimes(1);
		expect(planSpy.mock.calls[0]?.[0].global).toBe(true);
		expect(applySpy.mock.calls[0]?.[1].global).toBe(true);
	});

	it("T4 control: an explicit local background plan stays local at plan and apply", async () => {
		const harness = await serializedHarness(EXPLICIT_LOCAL);
		const internals = harness.session as unknown as ScopeInternals;
		const planSpy = vi.spyOn(internals, "_planRefine").mockResolvedValue({ id: "p", proposal: { edits: [] } });
		const applySpy = vi.spyOn(internals, "_applyRefine").mockResolvedValue(emptyRefinementResult());

		internals._assistantTurnsSinceAutoRefine = 1;
		internals._maybeStartSerializedBackgroundPlan();
		await internals._serializedPlanInFlight;
		await internals._runSerializedRefineCheckpoint();

		expect(planSpy).toHaveBeenCalledTimes(1);
		expect(applySpy).toHaveBeenCalledTimes(1);
		expect(planSpy.mock.calls[0]?.[0].global).toBeFalsy();
		expect(applySpy.mock.calls[0]?.[1].global).toBeFalsy();
	});
});
