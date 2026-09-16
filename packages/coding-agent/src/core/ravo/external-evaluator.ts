import type { EvaluationAdapter } from "./controller.js";
import type { JsonValue } from "./reducer.js";

/**
 * An outcome evaluator supplied from outside the harness.
 *
 * `RavoRunService` runs the judge gates by default. A suite replaces both gates
 * with its own, contributes its own opponent criteria to the reducer, and may
 * add reference material to the implement/repair prompt. The service knows this
 * interface and nothing about any particular benchmark; every benchmark-specific
 * line lives in the module that implements it.
 */
export interface ExternalEvaluatorSuite {
	/** Opponent criterion ids this suite adds to the reducer's opponent set. */
	readonly criterionIds: readonly string[];
	/** Replaces the structural fast screen. */
	readonly fast: EvaluationAdapter<JsonValue>;
	/** Replaces the judge deep gate. */
	readonly deep: EvaluationAdapter<JsonValue>;
	/** Opponent adapters for `criterionIds`, sharing one evaluation per proposal with `deep`. */
	readonly opponents: readonly EvaluationAdapter<JsonValue>[];
	/** Reference material appended to the implement/repair prompt, or `undefined` for none. */
	promptSection(): Promise<string | undefined>;
	/** Extra JSON fields to keep from the implement/repair child's output. */
	artifactFields(record: Record<string, unknown>): Record<string, JsonValue> | undefined;
	/** Record the committed candidate under `baseDir`, after the commit gate accepts it. */
	persistCommitted(baseDir: string, runId: string, artifact: JsonValue): Promise<void>;
}
