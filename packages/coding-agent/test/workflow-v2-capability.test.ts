import { describe, expect, it } from "vitest";
import {
	matchesWorkflowV2CapabilityProfile,
	negotiateWorkflowV2Capability,
	WORKFLOW_V2_REQUIRED_CAPABILITY,
} from "../src/core/workflow-v2-capability.js";

function capability(): Record<string, unknown> {
	return structuredClone(WORKFLOW_V2_REQUIRED_CAPABILITY) as unknown as Record<string, unknown>;
}

describe("Workflow V2 capability negotiation", () => {
	it("matches only the complete normative capability vector", () => {
		expect(matchesWorkflowV2CapabilityProfile(capability())).toBe(true);
	});

	it("rejects mixed version/profile vectors atomically", () => {
		const candidate = capability();
		candidate.version = 1;
		expect(matchesWorkflowV2CapabilityProfile(candidate)).toBe(false);

		const mixed = capability();
		mixed.protocol = "prime.workflow.capability/v1";
		expect(matchesWorkflowV2CapabilityProfile(mixed)).toBe(false);
	});

	it("rejects a matching version with any mandatory feature missing", () => {
		const candidate = capability();
		candidate.features = (candidate.features as string[]).filter((feature) => feature !== "cancel_fence");
		expect(matchesWorkflowV2CapabilityProfile(candidate)).toBe(false);
	});

	it("rejects feature substitutions, duplicates, and limit mutations", () => {
		const substituted = capability();
		(substituted.features as string[])[0] = "future_feature";
		expect(matchesWorkflowV2CapabilityProfile(substituted)).toBe(false);

		const duplicated = capability();
		(duplicated.features as string[])[0] = "cancel_fence";
		expect(matchesWorkflowV2CapabilityProfile(duplicated)).toBe(false);

		const wrongLimit = capability();
		(wrongLimit.limits as Record<string, number>).maxWaitMs = 30_001;
		expect(matchesWorkflowV2CapabilityProfile(wrongLimit)).toBe(false);
	});

	it("keeps the public capability unavailable even for the exact profile", () => {
		expect(negotiateWorkflowV2Capability(capability())).toEqual({
			available: false,
			code: "CAPABILITY_UNAVAILABLE",
		});
	});
});
