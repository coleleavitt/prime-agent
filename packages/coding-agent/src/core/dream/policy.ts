/**
 * The typed, serializable exploration policy and its soundness-critical parser.
 *
 * A policy is DATA, never code: a flat JSON object of bounded numbers and
 * named-rule string literals. The only thing that ever acts on it is the fixed
 * interpreter in `interpreter.ts`. `parseExplorationPolicy` rejects any unknown
 * key, any out-of-range or wrong-typed number, and any named rule outside the
 * const literal arrays, so a stringified function or a smuggled code field is
 * rejected as an unknown-key/type error. There is no eval, no Function, no
 * dynamic import of policy content anywhere in this subsystem.
 */

import { canonicalJson, sha256 } from "../ravo/canonical-json.js";

/** How eligible cells are ranked before the batch is cut. */
export const SELECTION_RULES = ["best-first", "explore-root", "round-robin", "weighted"] as const;
/** What a failed/stalled branch falls back to (advisory to the online driver). */
export const RECOVERY_POLICIES = ["retry-best", "retry-root", "widen", "abandon"] as const;
/** When exploration stops. */
export const STOP_RULES = ["patience", "threshold", "fixed-rounds", "never"] as const;

export type SelectionRule = (typeof SELECTION_RULES)[number];
export type RecoveryPolicy = (typeof RECOVERY_POLICIES)[number];
export type StopRule = (typeof STOP_RULES)[number];

export interface ExplorationPolicy {
	selectionRule: SelectionRule;
	recoveryPolicy: RecoveryPolicy;
	stopRule: StopRule;
	/** Branch/step width the proposer scale derives from (integer). */
	branchWidth: number;
	/** Refinement candidates per generation attempt (integer). */
	refineDepth: number;
	/** Cells probed per round, capped at runtime by W (integer). */
	batchSize: number;
	/** Patience for "patience"/round cap for "fixed-rounds" (integer). */
	beta: number;
	/** A node is "promising" when its score >= this fraction of the best (0..1). */
	promisingThreshold: number;
	/** Target best score for the "threshold" stop rule. */
	targetScore: number;
	/** Weight the "weighted" rule puts on exploring promising branches (0..1). */
	explorationBias: number;
}

interface NumericBound {
	readonly min: number;
	readonly max: number;
	readonly integer: boolean;
}

export const POLICY_BOUNDS = {
	branchWidth: { min: 1, max: 8, integer: true },
	refineDepth: { min: 0, max: 8, integer: true },
	batchSize: { min: 1, max: 8, integer: true },
	beta: { min: 1, max: 32, integer: true },
	promisingThreshold: { min: 0, max: 1, integer: false },
	targetScore: { min: 0, max: 1_000_000, integer: false },
	explorationBias: { min: 0, max: 1, integer: false },
} as const satisfies Record<string, NumericBound>;

type NumericField = keyof typeof POLICY_BOUNDS;

const NUMERIC_FIELDS = Object.keys(POLICY_BOUNDS) as NumericField[];

const RULE_FIELDS = {
	selectionRule: SELECTION_RULES,
	recoveryPolicy: RECOVERY_POLICIES,
	stopRule: STOP_RULES,
} as const;

type RuleField = keyof typeof RULE_FIELDS;

const RULE_FIELD_NAMES = Object.keys(RULE_FIELDS) as RuleField[];

/** The exact set of keys a policy may carry; anything else is rejected on parse. */
const POLICY_KEYS: ReadonlySet<string> = new Set<string>([...NUMERIC_FIELDS, ...RULE_FIELD_NAMES]);

/** A parallel-refine default: refine the best nodes in parallel, stop on patience. */
export const DEFAULT_POLICY: ExplorationPolicy = {
	selectionRule: "best-first",
	recoveryPolicy: "retry-best",
	stopRule: "patience",
	branchWidth: 2,
	refineDepth: 2,
	batchSize: 4,
	beta: 6,
	promisingThreshold: 0.5,
	targetScore: 1_000_000,
	explorationBias: 0.25,
};

export class PolicyValidationError extends Error {}

function isPlainObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Strict schema parse. Every known key must be present and in range, and no
 * unknown key may appear. This is the trust boundary for an LLM-emitted policy:
 * a code payload can only ever reach here as an extra key or a wrong type, and
 * both are rejected.
 */
export function parseExplorationPolicy(value: unknown): ExplorationPolicy {
	if (!isPlainObject(value)) {
		throw new PolicyValidationError("policy must be a JSON object");
	}
	for (const key of Object.keys(value)) {
		if (!POLICY_KEYS.has(key)) {
			throw new PolicyValidationError(`unknown policy field: ${key}`);
		}
	}
	const numeric = {} as Record<NumericField, number>;
	for (const field of NUMERIC_FIELDS) {
		const raw = value[field];
		if (typeof raw !== "number" || !Number.isFinite(raw)) {
			throw new PolicyValidationError(`${field} must be a finite number`);
		}
		const bound = POLICY_BOUNDS[field];
		if (bound.integer && !Number.isInteger(raw)) {
			throw new PolicyValidationError(`${field} must be an integer`);
		}
		if (raw < bound.min || raw > bound.max) {
			throw new PolicyValidationError(`${field} must be within [${bound.min}, ${bound.max}]`);
		}
		numeric[field] = raw;
	}
	const rules = {} as Record<RuleField, string>;
	for (const field of RULE_FIELD_NAMES) {
		const raw = value[field];
		const allowed = RULE_FIELDS[field] as readonly string[];
		if (typeof raw !== "string" || !allowed.includes(raw)) {
			throw new PolicyValidationError(`${field} must be one of ${allowed.join(", ")}`);
		}
		rules[field] = raw;
	}
	return {
		selectionRule: rules.selectionRule as SelectionRule,
		recoveryPolicy: rules.recoveryPolicy as RecoveryPolicy,
		stopRule: rules.stopRule as StopRule,
		branchWidth: numeric.branchWidth,
		refineDepth: numeric.refineDepth,
		batchSize: numeric.batchSize,
		beta: numeric.beta,
		promisingThreshold: numeric.promisingThreshold,
		targetScore: numeric.targetScore,
		explorationBias: numeric.explorationBias,
	};
}

function clampNumber(raw: unknown, bound: NumericBound, fallback: number): number {
	if (typeof raw !== "number" || !Number.isFinite(raw)) return fallback;
	const clamped = Math.min(bound.max, Math.max(bound.min, raw));
	return bound.integer ? Math.round(clamped) : clamped;
}

function snapRule<T extends string>(raw: unknown, allowed: readonly T[], fallback: T): T {
	return typeof raw === "string" && (allowed as readonly string[]).includes(raw) ? (raw as T) : fallback;
}

/**
 * Project an arbitrary object into a valid policy: out-of-range numbers are
 * clamped (and rounded where an integer is required), invalid named rules snap
 * to the default, and unknown keys are dropped. Used after a mutation perturbs
 * fields; never throws.
 */
export function clampPolicy(raw: unknown): ExplorationPolicy {
	const source = isPlainObject(raw) ? raw : {};
	return {
		selectionRule: snapRule(source.selectionRule, SELECTION_RULES, DEFAULT_POLICY.selectionRule),
		recoveryPolicy: snapRule(source.recoveryPolicy, RECOVERY_POLICIES, DEFAULT_POLICY.recoveryPolicy),
		stopRule: snapRule(source.stopRule, STOP_RULES, DEFAULT_POLICY.stopRule),
		branchWidth: clampNumber(source.branchWidth, POLICY_BOUNDS.branchWidth, DEFAULT_POLICY.branchWidth),
		refineDepth: clampNumber(source.refineDepth, POLICY_BOUNDS.refineDepth, DEFAULT_POLICY.refineDepth),
		batchSize: clampNumber(source.batchSize, POLICY_BOUNDS.batchSize, DEFAULT_POLICY.batchSize),
		beta: clampNumber(source.beta, POLICY_BOUNDS.beta, DEFAULT_POLICY.beta),
		promisingThreshold: clampNumber(
			source.promisingThreshold,
			POLICY_BOUNDS.promisingThreshold,
			DEFAULT_POLICY.promisingThreshold,
		),
		targetScore: clampNumber(source.targetScore, POLICY_BOUNDS.targetScore, DEFAULT_POLICY.targetScore),
		explorationBias: clampNumber(
			source.explorationBias,
			POLICY_BOUNDS.explorationBias,
			DEFAULT_POLICY.explorationBias,
		),
	};
}

/**
 * A stable identity for a policy: the sha256 of the canonical JSON of exactly
 * its known fields. Stable under key reordering (canonical JSON sorts keys) and
 * across runtime objects that carry stray properties; changes when any field
 * changes.
 */
export function policyId(policy: ExplorationPolicy): string {
	const canonical: ExplorationPolicy = {
		selectionRule: policy.selectionRule,
		recoveryPolicy: policy.recoveryPolicy,
		stopRule: policy.stopRule,
		branchWidth: policy.branchWidth,
		refineDepth: policy.refineDepth,
		batchSize: policy.batchSize,
		beta: policy.beta,
		promisingThreshold: policy.promisingThreshold,
		targetScore: policy.targetScore,
		explorationBias: policy.explorationBias,
	};
	return sha256(canonicalJson(canonical)).slice(0, 16);
}
