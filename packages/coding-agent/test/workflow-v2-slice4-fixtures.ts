/**
 * Shared Workflow V2 Slice 4 test fixtures (controller/host wire builders +
 * definitions). Test-only; not part of any production path.
 */

import { createHash } from "node:crypto";
import { canonicalJson, type WorkflowV2Value } from "../src/core/workflow-v2-wire.js";

export const RECORDED_AT = "2026-09-15T00:00:00.000Z";
export function digestOf(value: unknown): string {
	return `sha256:${createHash("sha256").update(canonicalJson(value)).digest("hex")}`;
}
export const EVIDENCE = `sha256:${"a".repeat(64)}`;

export function validDefinition(options?: {
	nodes?: Array<{ nodeId: string; dependsOn?: string[]; maxTokens?: number }>;
	outputs?: string[];
	maxTotalTokens?: number;
	maxConcurrentAttempts?: number;
}): WorkflowV2Value {
	const nodeSpecs = options?.nodes ?? [{ nodeId: "n1" }];
	const nodes = nodeSpecs.map((n) => ({
		nodeId: n.nodeId,
		kind: "agent",
		prompt: `prompt for ${n.nodeId}`,
		dependsOn: (n.dependsOn ?? []).map((d) => ({ nodeId: d, require: "accepted" })),
		model: "model.test",
		maxTurns: 1,
		tools: "none",
		maxTokens: n.maxTokens ?? 100,
	}));
	return {
		protocol: "prime.workflow.definition/v2",
		nodes,
		outputs: options?.outputs ?? [nodeSpecs[nodeSpecs.length - 1].nodeId],
		budget: {
			maxConcurrentAttempts: options?.maxConcurrentAttempts ?? 4,
			maxTotalTokens: options?.maxTotalTokens ?? 10_000,
			semantics: "soft_admission",
		},
	};
}

let eventCounter = 0;
export function resetEventCounter(): void {
	eventCounter = 0;
}

export function ctrlEvent(
	runId: string,
	sequence: number,
	type: string,
	data: WorkflowV2Value,
	opts?: { revision?: number; controllerEpoch?: number; cancelEpoch?: number; eventId?: string },
): WorkflowV2Value {
	eventCounter += 1;
	return {
		protocol: "prime.workflow.event/v2",
		eventId: opts?.eventId ?? `ev-${runId}-${sequence}-${eventCounter}`,
		runId,
		sequence,
		revision: opts?.revision ?? sequence,
		type,
		recordedAt: RECORDED_AT,
		controllerEpoch: opts?.controllerEpoch ?? 1,
		cancelEpoch: opts?.cancelEpoch ?? 0,
		data,
		digest: EVIDENCE,
	};
}

export function hostEvent(
	hostEventId: string,
	hostCursor: string,
	type: string,
	data: WorkflowV2Value,
): WorkflowV2Value {
	return {
		protocol: "prime.workflow.retained-event/v2",
		hostEventId,
		hostCursor,
		type,
		recordedAt: RECORDED_AT,
		data,
		digest: EVIDENCE,
	};
}

export function turnSettlement(options: {
	runId?: string;
	nodeId: string;
	attemptId: string;
	rlmChildId: string;
	turnId: string;
	result?: "text" | "none";
	usageFinal?: boolean;
	quiescent?: boolean;
	cancelActuated?: boolean;
	outcome?: "completed" | "failed" | "cancelled" | "execution_unknown";
}): WorkflowV2Value {
	const text = "hello world";
	const utf8Bytes = Buffer.byteLength(text, "utf8");
	const result =
		(options.result ?? "text") === "text"
			? {
					kind: "text",
					text,
					utf8Bytes,
					sha256: `sha256:${createHash("sha256").update(text, "utf8").digest("hex")}`,
				}
			: { kind: "none", reason: "no_output" };
	const usage = {
		inputTokens: 10,
		outputTokens: 20,
		cacheReadTokens: 0,
		cacheWriteTokens: 0,
		totalTokens: 30,
		costMicrousd: 0,
		finality: options.usageFinal === false ? "known_prefix" : "final",
	};
	const base: WorkflowV2Value = {
		authorityScope: EVIDENCE,
		parentId: "parent.session",
		requestId: `req-${options.attemptId}`,
		nodeId: options.nodeId,
		attemptId: options.attemptId,
		rlmChildId: options.rlmChildId,
		turnId: options.turnId,
		admittedAt: RECORDED_AT,
		startedAt: RECORDED_AT,
		settledAt: RECORDED_AT,
		cancelActuated: options.cancelActuated ?? false,
		descendantsQuiescent: options.quiescent ?? true,
		hostCursor: "hc-1",
		settlementDigest: EVIDENCE,
		outcome: options.outcome ?? "completed",
		result,
		usage,
		error: null,
		workflowChildId: `wfc-${options.attemptId}`,
		requestDigest: EVIDENCE,
	};
	return base;
}
