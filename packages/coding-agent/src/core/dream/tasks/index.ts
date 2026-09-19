/**
 * Registry and resolver for the self-contained scored tasks, so the rollout
 * driver and CLI pick a task by id without importing each implementation.
 */

import type { DreamTaskId, ScoredTask } from "../task.js";
import { createCirclePackingTask } from "./circle-packing.js";
import { createPythonSpeedupTask, PYTHON_SPEEDUP_PROMPT_CONTEXT } from "./python-speedup.js";
import { createSumDifferenceTask } from "./sum-difference.js";

export const DREAM_TASK_IDS = ["circle-packing", "sum-difference", "python-speedup"] as const;

/** The circle counts the paper studies; the default is the smaller one. */
export const CIRCLE_PACKING_PAPER_N = [26, 32] as const;
export const DEFAULT_CIRCLE_PACKING_N = 26;

export interface DreamTaskSpec {
	task: DreamTaskId;
	n?: number;
}

export function isDreamTaskId(value: unknown): value is DreamTaskId {
	return typeof value === "string" && (DREAM_TASK_IDS as readonly string[]).includes(value);
}

/**
 * Build a task from a spec. Artifacts are opaque to the caller, so the concrete
 * `ScoredTask<CirclePackingArtifact>` is widened to `ScoredTask<unknown>`: the
 * rollout only ever round-trips artifacts back through the same task, so the
 * widening is sound.
 */
export function resolveTask(spec: DreamTaskSpec): ScoredTask<unknown> {
	switch (spec.task) {
		case "circle-packing": {
			const n = spec.n ?? DEFAULT_CIRCLE_PACKING_N;
			if (!Number.isInteger(n) || n < 2) {
				throw new RangeError(`circle-packing requires an integer n >= 2 (got ${String(spec.n)})`);
			}
			return createCirclePackingTask(n) as unknown as ScoredTask<unknown>;
		}
		case "sum-difference":
			return createSumDifferenceTask() as unknown as ScoredTask<unknown>;
		case "python-speedup":
			return createPythonSpeedupTask() as unknown as ScoredTask<unknown>;
	}
}

/**
 * The task-specific context an LLM proposer prompt carries (the public contract
 * and examples, never hidden tests). Only python-speedup has one; the other
 * tasks are fully described by their serialized candidate.
 */
export function taskPromptContext(taskId: DreamTaskId): string | undefined {
	return taskId === "python-speedup" ? PYTHON_SPEEDUP_PROMPT_CONTEXT : undefined;
}
