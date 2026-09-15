/** Closed Workflow V2 capability profile from docs/api/workflow-v2.schema.json. */
export const WORKFLOW_V2_FEATURES = [
	"durable_request_id",
	"direct_parent_ownership",
	"per_turn_settlement",
	"cursor_replay",
	"cancel_fence",
	"tombstone_delete",
	"host_result_attribution",
] as const;

export type WorkflowV2Feature = (typeof WORKFLOW_V2_FEATURES)[number];

export interface WorkflowV2Capability {
	protocol: "prime.workflow.capability/v2";
	api: "prime.workflow.retained";
	version: 2;
	semantics: "2026-09-14";
	features: readonly WorkflowV2Feature[];
	limits: Readonly<{
		maxPromptUtf8Bytes: 65_536;
		maxResultUtf8Bytes: 262_144;
		maxPageSize: 500;
		maxWaitMs: 30_000;
		maxChildren: 10_000;
	}>;
}

export const WORKFLOW_V2_REQUIRED_CAPABILITY: Readonly<WorkflowV2Capability> = Object.freeze({
	protocol: "prime.workflow.capability/v2",
	api: "prime.workflow.retained",
	version: 2,
	semantics: "2026-09-14",
	features: Object.freeze([...WORKFLOW_V2_FEATURES]),
	limits: Object.freeze({
		maxPromptUtf8Bytes: 65_536,
		maxResultUtf8Bytes: 262_144,
		maxPageSize: 500,
		maxWaitMs: 30_000,
		maxChildren: 10_000,
	}),
});

export interface WorkflowV2CapabilityUnavailable {
	available: false;
	code: "CAPABILITY_UNAVAILABLE";
}

/**
 * Compares the complete version/profile as one decision. No individual match
 * can enable a partial Workflow V2 profile.
 */
export function matchesWorkflowV2CapabilityProfile(value: unknown): value is WorkflowV2Capability {
	if (!isRecord(value) || !hasExactKeys(value, ["protocol", "api", "version", "semantics", "features", "limits"])) {
		return false;
	}
	if (
		value.protocol !== WORKFLOW_V2_REQUIRED_CAPABILITY.protocol ||
		value.api !== WORKFLOW_V2_REQUIRED_CAPABILITY.api ||
		value.version !== WORKFLOW_V2_REQUIRED_CAPABILITY.version ||
		value.semantics !== WORKFLOW_V2_REQUIRED_CAPABILITY.semantics ||
		!hasExactFeatureSet(value.features) ||
		!hasExactLimits(value.limits)
	) {
		return false;
	}
	return true;
}

/** Slice 2 scaffold: negotiation is deliberately disabled until slices 3-8 exist. */
export function negotiateWorkflowV2Capability(_candidate: unknown): WorkflowV2CapabilityUnavailable {
	return { available: false, code: "CAPABILITY_UNAVAILABLE" };
}

function hasExactFeatureSet(value: unknown): boolean {
	if (!Array.isArray(value) || value.length !== WORKFLOW_V2_FEATURES.length) return false;
	const features = new Set(value);
	return (
		features.size === WORKFLOW_V2_FEATURES.length && WORKFLOW_V2_FEATURES.every((feature) => features.has(feature))
	);
}

function hasExactLimits(value: unknown): boolean {
	if (
		!isRecord(value) ||
		!hasExactKeys(value, ["maxPromptUtf8Bytes", "maxResultUtf8Bytes", "maxPageSize", "maxWaitMs", "maxChildren"])
	) {
		return false;
	}
	const required = WORKFLOW_V2_REQUIRED_CAPABILITY.limits;
	return (
		value.maxPromptUtf8Bytes === required.maxPromptUtf8Bytes &&
		value.maxResultUtf8Bytes === required.maxResultUtf8Bytes &&
		value.maxPageSize === required.maxPageSize &&
		value.maxWaitMs === required.maxWaitMs &&
		value.maxChildren === required.maxChildren
	);
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasExactKeys(value: Record<string, unknown>, keys: readonly string[]): boolean {
	const actual = Object.keys(value);
	return actual.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}
