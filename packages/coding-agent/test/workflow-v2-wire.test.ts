import { describe, expect, it } from "vitest";
import {
	decodeWorkflowV2Capability,
	decodeWorkflowV2Definition,
	decodeWorkflowV2PublicError,
	decodeWorkflowV2PublicRequest,
	decodeWorkflowV2PublicRequestJson,
	decodeWorkflowV2RetainedRequest,
	decodeWorkflowV2RetainedResult,
	parseWorkflowV2Json,
	WORKFLOW_V2_CAPABILITY_AVAILABLE,
	workflowV2RequestDigest,
} from "../src/core/workflow-v2-wire.js";

const digest = `sha256:${"a".repeat(64)}`;
const definition = () => ({
	protocol: "prime.workflow.definition/v2",
	nodes: [
		{
			nodeId: "n1",
			kind: "agent",
			prompt: "hi",
			dependsOn: [],
			model: "model",
			maxTurns: 1,
			tools: "none",
			maxTokens: 10,
		},
	],
	outputs: ["n1"],
	budget: { maxConcurrentAttempts: 1, maxTotalTokens: 10, semantics: "soft_admission" },
});
const retainedBase = { protocol: "prime.workflow.retained-request/v2", requestId: "r1", nodeId: "n1" };
const turn = { prompt: "hi", model: "model", maxTurns: 1, tools: "none" };
const retainedRequests = [
	{ ...retainedBase, operation: "child.admit", attemptId: "a1", workflowChildId: "w1", turn, requestDigest: digest },
	{ ...retainedBase, operation: "child.send", attemptId: "a1", rlmChildId: "c1", turn, requestDigest: digest },
	{ ...retainedBase, operation: "child.get", rlmChildId: "c1" },
	{ ...retainedBase, operation: "child.list", limit: 200 },
	{ ...retainedBase, operation: "child.events", limit: 500 },
	{ ...retainedBase, operation: "child.wait", rlmChildId: "c1", turnId: "t1", timeoutMs: 30000 },
	{
		...retainedBase,
		operation: "child.cancel",
		rlmChildId: "c1",
		turnId: "t1",
		reason: "stop",
		requestDigest: digest,
	},
	{ ...retainedBase, operation: "child.delete", rlmChildId: "c1", requestDigest: digest },
];
const receipt = {
	protocol: "prime.workflow.retained-result/v2",
	requestId: "r1",
	hostCursor: "h1",
	receiptDigest: digest,
};
const retainedResults = [
	{ ...receipt, operation: "child.admit", disposition: "admitted", rlmChildId: "c1", turnId: "t1" },
	{ ...receipt, operation: "child.send", disposition: "replayed", rlmChildId: "c1", turnId: "t1" },
	{
		...receipt,
		operation: "child.get",
		disposition: "snapshot",
		child: { rlmChildId: "c1", turnId: "t1", lifecycle: "running" },
	},
	{ ...receipt, operation: "child.list", disposition: "page", children: [], nextCursor: "h1", caughtUp: true },
	{ ...receipt, operation: "child.events", disposition: "page", events: [], nextCursor: "h1", caughtUp: true },
	{
		...receipt,
		operation: "child.wait",
		disposition: "pending",
		rlmChildId: "c1",
		turnId: "t1",
		projection: { phase: "running", intent: "execute", outcome: null, conditions: ["result_absent"] },
	},
	{
		...receipt,
		operation: "child.cancel",
		disposition: "requested",
		rlmChildId: "c1",
		turnId: "t1",
		actuation: "requested",
	},
	{ ...receipt, operation: "child.delete", disposition: "tombstoned", rlmChildId: "c1" },
];

describe("Workflow V2 strict closed wire codecs", () => {
	it("keeps the runtime capability unavailable", () => expect(WORKFLOW_V2_CAPABILITY_AVAILABLE).toBe(false));
	it("accepts the public request family", () => {
		const base = { protocol: "prime.workflow.request/v2", requestId: "r1" };
		const requests = [
			{ ...base, action: "validate", definition: definition() },
			{ ...base, action: "create", definition: definition() },
			{
				...base,
				action: "start",
				runId: "run",
				commandId: "cmd",
				expectedRevision: 0,
				expectedControllerEpoch: 0,
				expectedCancelEpoch: 0,
			},
			{
				...base,
				action: "cancel",
				runId: "run",
				commandId: "cmd",
				expectedRevision: 0,
				expectedControllerEpoch: 0,
				expectedCancelEpoch: 0,
				reason: "stop",
			},
			{
				...base,
				action: "retry",
				runId: "run",
				commandId: "cmd",
				expectedRevision: 0,
				expectedControllerEpoch: 0,
				expectedCancelEpoch: 0,
				nodeId: "n1",
				fromAttemptId: "a1",
				reason: "again",
			},
			{ ...base, action: "status", runId: "run", include: ["nodes", "attempts"] },
			{ ...base, action: "events", runId: "run", after: "e1", limit: 500 },
		];
		for (const request of requests) expect(decodeWorkflowV2PublicRequest(request)).toBe(request);
	});
	it("accepts all eight retained request and result operations", () => {
		for (const v of retainedRequests) expect(decodeWorkflowV2RetainedRequest(v)).toBe(v);
		retainedResults.forEach((v, i) => {
			try {
				expect(decodeWorkflowV2RetainedResult(v)).toBe(v);
			} catch (e) {
				throw new Error(`result ${i}: ${String(e)}`);
			}
		});
	});
	it("rejects discriminator, exact-key, integer, identifier, digest, enum and UTF-8 mutations", () => {
		const start = {
			protocol: "prime.workflow.request/v2",
			requestId: "r1",
			action: "start",
			runId: "run",
			commandId: "cmd",
			expectedRevision: 0,
			expectedControllerEpoch: 0,
			expectedCancelEpoch: 0,
		};
		for (const bad of [
			{ ...start, protocol: "prime.workflow.request/v1" },
			{ ...start, extra: true },
			{ ...start, expectedRevision: true },
			{ ...start, expectedRevision: 2 ** 53 },
			{ ...start, runId: " bad" },
		])
			expect(() => decodeWorkflowV2PublicRequest(bad)).toThrow();
		expect(() =>
			decodeWorkflowV2RetainedRequest({ ...retainedRequests[0], requestDigest: `sha256:${"A".repeat(64)}` }),
		).toThrow();
		expect(() => decodeWorkflowV2RetainedRequest({ ...retainedRequests[5], timeoutMs: 30001 })).toThrow();
		expect(() =>
			decodeWorkflowV2PublicRequest({
				protocol: "prime.workflow.request/v2",
				requestId: "r",
				action: "cancel",
				runId: "run",
				commandId: "cmd",
				expectedRevision: 0,
				expectedControllerEpoch: 0,
				expectedCancelEpoch: 0,
				reason: "💣".repeat(129),
			}),
		).toThrow("UTF-8");
	});
	it("rejects duplicate keys, trailing bytes, invalid UTF-8, over-depth and over-size inputs", () => {
		expect(() =>
			decodeWorkflowV2PublicRequestJson(
				'{"protocol":"prime.workflow.request/v2","requestId":"a","requestId":"b","action":"status","runId":"r"}',
			),
		).toThrow("duplicate");
		expect(() => parseWorkflowV2Json("{} x")).toThrow("trailing");
		expect(() => parseWorkflowV2Json(Uint8Array.from([0x7b, 0xff, 0x7d]))).toThrow();
		expect(() => parseWorkflowV2Json("[".repeat(33) + "]".repeat(33))).toThrow("depth");
		expect(() => parseWorkflowV2Json(JSON.stringify("x".repeat(1_048_576)))).toThrow("1 MiB");
	});
	it("checks definition bounds and closed nested keys", () => {
		expect(decodeWorkflowV2Definition(definition()).protocol).toBe("prime.workflow.definition/v2");
		expect(() =>
			decodeWorkflowV2Definition({ ...definition(), nodes: [{ ...definition().nodes[0], oops: true }] }),
		).toThrow("unknown");
		expect(() =>
			decodeWorkflowV2Definition({
				...definition(),
				nodes: [{ ...definition().nodes[0], prompt: "💣".repeat(16385) }],
			}),
		).toThrow("UTF-8");
	});
	it("rejects every invalid definition graph and inconsistent node budget through all definition entry points", () => {
		const node = definition().nodes[0];
		const second = { ...node, nodeId: "n2" };
		const invalid = [
			{ ...definition(), nodes: [node, { ...second, nodeId: "n1" }] },
			{ ...definition(), nodes: [{ ...node, dependsOn: [{ nodeId: "n1", require: "accepted" }] }] },
			{ ...definition(), nodes: [{ ...node, dependsOn: [{ nodeId: "missing", require: "accepted" }] }] },
			{
				...definition(),
				nodes: [
					{ ...node, dependsOn: [{ nodeId: "n2", require: "accepted" }] },
					{ ...second, dependsOn: [{ nodeId: "n1", require: "accepted" }] },
				],
			},
			{ ...definition(), outputs: ["missing"] },
			{ ...definition(), budget: { ...definition().budget, maxTotalTokens: 9 } },
		];
		for (const value of invalid) {
			expect(() => decodeWorkflowV2Definition(value)).toThrow();
			for (const action of ["validate", "create"])
				expect(() =>
					decodeWorkflowV2PublicRequest({
						protocol: "prime.workflow.request/v2",
						requestId: "r",
						action,
						definition: value,
					}),
				).toThrow();
		}
	});
	it("checks public errors, fixed capability features, and canonical request digests", () => {
		const error = {
			protocol: "prime.workflow.error/v2",
			requestId: "r",
			code: "CAPABILITY_UNAVAILABLE",
			message: "off",
			retryable: false,
			currentRevision: null,
		};
		expect(decodeWorkflowV2PublicError(error)).toBe(error);
		const features = [
			"durable_request_id",
			"direct_parent_ownership",
			"per_turn_settlement",
			"cursor_replay",
			"cancel_fence",
			"tombstone_delete",
			"host_result_attribution",
		];
		const capability = {
			protocol: "prime.workflow.capability/v2",
			api: "prime.workflow.retained",
			version: 2,
			semantics: "2026-09-14",
			features,
			limits: {
				maxPromptUtf8Bytes: 65536,
				maxResultUtf8Bytes: 262144,
				maxPageSize: 500,
				maxWaitMs: 30000,
				maxChildren: 10000,
			},
		};
		expect(decodeWorkflowV2Capability(capability)).toBe(capability);
		expect(() => decodeWorkflowV2Capability({ ...capability, features: features.slice(1) })).toThrow();
		expect(workflowV2RequestDigest({ b: 1, a: 2 })).toBe(workflowV2RequestDigest({ a: 2, b: 1 }));
	});
});
