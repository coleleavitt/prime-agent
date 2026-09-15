/* biome-ignore-all lint/suspicious/noThenProperty: JSON Schema uses the standard then keyword. */
import { createHash } from "node:crypto";

/** Workflow V2 is a closed wire contract only. No runtime capability is exposed here. */
export const WORKFLOW_V2_CAPABILITY_AVAILABLE = false as const;
export type WorkflowV2Value = Record<string, unknown>;

const MAX_MESSAGE_BYTES = 1_048_576;
const MAX_DEPTH = 32;
const MAX_NODES = 10_000;
const defs = {
	id: { type: "string", pattern: "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$" },
	digest: { type: "string", pattern: "^sha256:[0-9a-f]{64}$" },
	time: { type: "string", format: "date-time", maxLength: 64, pattern: "Z$" },
	uint: { type: "integer", minimum: 0, maximum: 9007199254740991 },
	boundedText: { type: "string", maxLength: 512, "x-utf8MaxBytes": 512 },
	dependency: {
		type: "object",
		additionalProperties: false,
		properties: { nodeId: { $ref: "#/$defs/id" }, require: { const: "accepted" } },
		required: ["nodeId", "require"],
	},
	nodeDefinition: {
		type: "object",
		additionalProperties: false,
		properties: {
			nodeId: { $ref: "#/$defs/id" },
			kind: { const: "agent" },
			prompt: { type: "string", minLength: 1, maxLength: 65536, "x-utf8MaxBytes": 65536 },
			dependsOn: { type: "array", maxItems: 127, uniqueItems: true, items: { $ref: "#/$defs/dependency" } },
			model: { $ref: "#/$defs/id" },
			maxTurns: { const: 1 },
			tools: { const: "none" },
			maxTokens: { type: "integer", minimum: 1, maximum: 1000000 },
		},
		required: ["nodeId", "kind", "prompt", "dependsOn", "model", "maxTurns", "tools", "maxTokens"],
	},
	budget: {
		type: "object",
		additionalProperties: false,
		properties: {
			maxConcurrentAttempts: { type: "integer", minimum: 1, maximum: 64 },
			maxTotalTokens: { type: "integer", minimum: 1, maximum: 1000000000 },
			semantics: { const: "soft_admission" },
		},
		required: ["maxConcurrentAttempts", "maxTotalTokens", "semantics"],
	},
	definition: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.definition/v2" },
			nodes: { type: "array", minItems: 1, maxItems: 128, items: { $ref: "#/$defs/nodeDefinition" } },
			outputs: { type: "array", minItems: 1, maxItems: 128, uniqueItems: true, items: { $ref: "#/$defs/id" } },
			budget: { $ref: "#/$defs/budget" },
		},
		required: ["protocol", "nodes", "outputs", "budget"],
	},
	runProjection: {
		type: "object",
		additionalProperties: false,
		properties: {
			phase: { enum: ["created", "active", "cancelling", "draining", "terminal", "quarantined"] },
			intent: { enum: ["none", "start", "cancel"] },
			outcome: {
				anyOf: [
					{ enum: ["succeeded", "failed", "cancelled", "budget_exceeded", "execution_unknown"] },
					{ type: "null" },
				],
			},
			conditions: {
				type: "array",
				uniqueItems: true,
				items: {
					enum: [
						"admission_open",
						"admission_fenced",
						"owned_work_quiescent",
						"effects_classified",
						"budgets_within_limit",
						"integrity_verified",
						"integrity_failed",
					],
				},
			},
		},
		required: ["phase", "intent", "outcome", "conditions"],
		allOf: [
			{
				if: { properties: { phase: { const: "terminal" } } },
				then: {
					properties: {
						outcome: { enum: ["succeeded", "failed", "cancelled", "budget_exceeded", "execution_unknown"] },
					},
				},
				else: { properties: { outcome: { type: "null" } } },
			},
			{
				if: { properties: { phase: { const: "quarantined" } } },
				then: {
					properties: { outcome: { type: "null" }, conditions: { contains: { const: "integrity_failed" } } },
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "admission_open" } }, { contains: { const: "admission_fenced" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "integrity_verified" } },
								{ contains: { const: "integrity_failed" } },
							],
						},
					},
				},
			},
			{
				if: { properties: { outcome: { const: "succeeded" } }, required: ["outcome"] },
				then: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "admission_fenced" } },
								{ contains: { const: "owned_work_quiescent" } },
								{ contains: { const: "effects_classified" } },
								{ contains: { const: "budgets_within_limit" } },
								{ contains: { const: "integrity_verified" } },
								{ not: { contains: { const: "integrity_failed" } } },
							],
						},
					},
				},
			},
		],
	},
	nodeProjection: {
		type: "object",
		additionalProperties: false,
		properties: {
			phase: { enum: ["blocked", "ready", "attempting", "exhausted", "terminal"] },
			intent: { enum: ["none", "execute", "cancel", "retry"] },
			outcome: { anyOf: [{ enum: ["accepted", "rejected", "cancelled", "execution_unknown"] }, { type: "null" }] },
			conditions: {
				type: "array",
				uniqueItems: true,
				items: {
					enum: [
						"dependencies_pending",
						"dependencies_accepted",
						"candidate_absent",
						"candidate_present",
						"acceptance_pending",
						"acceptance_passed",
						"acceptance_failed",
					],
				},
			},
		},
		required: ["phase", "intent", "outcome", "conditions"],
		allOf: [
			{
				if: { properties: { phase: { const: "terminal" } } },
				then: { properties: { outcome: { enum: ["accepted", "rejected", "cancelled", "execution_unknown"] } } },
				else: { properties: { outcome: { type: "null" } } },
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "dependencies_pending" } },
								{ contains: { const: "dependencies_accepted" } },
							],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "candidate_absent" } }, { contains: { const: "candidate_present" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "acceptance_pending" } },
								{ contains: { const: "acceptance_passed" } },
							],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "acceptance_pending" } },
								{ contains: { const: "acceptance_failed" } },
							],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "acceptance_passed" } },
								{ contains: { const: "acceptance_failed" } },
							],
						},
					},
				},
			},
			{
				if: { properties: { outcome: { const: "accepted" } }, required: ["outcome"] },
				then: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "dependencies_accepted" } },
								{ contains: { const: "candidate_present" } },
								{ contains: { const: "acceptance_passed" } },
								{ not: { contains: { const: "dependencies_pending" } } },
								{ not: { contains: { const: "candidate_absent" } } },
								{ not: { contains: { const: "acceptance_pending" } } },
								{ not: { contains: { const: "acceptance_failed" } } },
							],
						},
					},
				},
			},
		],
	},
	attemptProjection: {
		type: "object",
		additionalProperties: false,
		properties: {
			phase: {
				enum: ["prepared", "dispatch_committed", "admission_bound", "running", "cancelling", "settled", "terminal"],
			},
			intent: { enum: ["execute", "cancel"] },
			outcome: { anyOf: [{ enum: ["completed", "failed", "cancelled", "execution_unknown"] }, { type: "null" }] },
			conditions: {
				type: "array",
				uniqueItems: true,
				items: {
					enum: [
						"admission_unbound",
						"admission_bound",
						"result_absent",
						"result_present",
						"usage_final",
						"usage_known_prefix",
						"quiescence_unproved",
						"quiescence_proved",
					],
				},
			},
		},
		required: ["phase", "intent", "outcome", "conditions"],
		allOf: [
			{
				if: { properties: { phase: { const: "terminal" } } },
				then: { properties: { outcome: { enum: ["completed", "failed", "cancelled", "execution_unknown"] } } },
				else: { properties: { outcome: { type: "null" } } },
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "admission_unbound" } }, { contains: { const: "admission_bound" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "result_absent" } }, { contains: { const: "result_present" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "usage_final" } }, { contains: { const: "usage_known_prefix" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "quiescence_unproved" } },
								{ contains: { const: "quiescence_proved" } },
							],
						},
					},
				},
			},
			{
				if: { properties: { outcome: { const: "completed" } }, required: ["outcome"] },
				then: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "admission_bound" } },
								{ contains: { const: "result_present" } },
								{ contains: { const: "usage_final" } },
								{ contains: { const: "quiescence_proved" } },
								{ not: { contains: { const: "admission_unbound" } } },
								{ not: { contains: { const: "result_absent" } } },
								{ not: { contains: { const: "usage_known_prefix" } } },
								{ not: { contains: { const: "quiescence_unproved" } } },
							],
						},
					},
				},
			},
		],
	},
	turnProjection: {
		type: "object",
		additionalProperties: false,
		properties: {
			phase: { enum: ["admitted", "running", "terminal"] },
			intent: { enum: ["execute", "cancel"] },
			outcome: { anyOf: [{ enum: ["completed", "failed", "cancelled", "execution_unknown"] }, { type: "null" }] },
			conditions: {
				type: "array",
				uniqueItems: true,
				items: {
					enum: [
						"result_absent",
						"result_present",
						"usage_final",
						"usage_known_prefix",
						"cancel_unactuated",
						"cancel_actuated",
						"quiescence_unproved",
						"quiescence_proved",
					],
				},
			},
		},
		required: ["phase", "intent", "outcome", "conditions"],
		allOf: [
			{
				if: { properties: { phase: { const: "terminal" } } },
				then: { properties: { outcome: { enum: ["completed", "failed", "cancelled", "execution_unknown"] } } },
				else: { properties: { outcome: { type: "null" } } },
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "result_absent" } }, { contains: { const: "result_present" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "usage_final" } }, { contains: { const: "usage_known_prefix" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "cancel_unactuated" } }, { contains: { const: "cancel_actuated" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "quiescence_unproved" } },
								{ contains: { const: "quiescence_proved" } },
							],
						},
					},
				},
			},
			{
				if: { properties: { outcome: { const: "completed" } }, required: ["outcome"] },
				then: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "result_present" } },
								{ contains: { const: "usage_final" } },
								{ contains: { const: "quiescence_proved" } },
								{ not: { contains: { const: "result_absent" } } },
								{ not: { contains: { const: "usage_known_prefix" } } },
								{ not: { contains: { const: "quiescence_unproved" } } },
							],
						},
					},
				},
			},
		],
	},
	validateRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "validate" },
			definition: { $ref: "#/$defs/definition" },
		},
		required: ["protocol", "requestId", "action", "definition"],
	},
	createRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "create" },
			definition: { $ref: "#/$defs/definition" },
		},
		required: ["protocol", "requestId", "action", "definition"],
	},
	startRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "start" },
			runId: { $ref: "#/$defs/id" },
			commandId: { $ref: "#/$defs/id" },
			expectedRevision: { $ref: "#/$defs/uint" },
			expectedControllerEpoch: { $ref: "#/$defs/uint" },
			expectedCancelEpoch: { $ref: "#/$defs/uint" },
		},
		required: [
			"protocol",
			"requestId",
			"action",
			"runId",
			"commandId",
			"expectedRevision",
			"expectedControllerEpoch",
			"expectedCancelEpoch",
		],
	},
	cancelRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "cancel" },
			runId: { $ref: "#/$defs/id" },
			commandId: { $ref: "#/$defs/id" },
			expectedRevision: { $ref: "#/$defs/uint" },
			expectedControllerEpoch: { $ref: "#/$defs/uint" },
			expectedCancelEpoch: { $ref: "#/$defs/uint" },
			reason: { $ref: "#/$defs/boundedText" },
		},
		required: [
			"protocol",
			"requestId",
			"action",
			"runId",
			"commandId",
			"expectedRevision",
			"expectedControllerEpoch",
			"expectedCancelEpoch",
			"reason",
		],
	},
	retryRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "retry" },
			runId: { $ref: "#/$defs/id" },
			commandId: { $ref: "#/$defs/id" },
			expectedRevision: { $ref: "#/$defs/uint" },
			expectedControllerEpoch: { $ref: "#/$defs/uint" },
			expectedCancelEpoch: { $ref: "#/$defs/uint" },
			nodeId: { $ref: "#/$defs/id" },
			fromAttemptId: { $ref: "#/$defs/id" },
			reason: { $ref: "#/$defs/boundedText" },
		},
		required: [
			"protocol",
			"requestId",
			"action",
			"runId",
			"commandId",
			"expectedRevision",
			"expectedControllerEpoch",
			"expectedCancelEpoch",
			"nodeId",
			"fromAttemptId",
			"reason",
		],
	},
	statusRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "status" },
			runId: { $ref: "#/$defs/id" },
			include: { type: "array", uniqueItems: true, items: { enum: ["nodes", "attempts", "blockers"] } },
		},
		required: ["protocol", "requestId", "action", "runId"],
	},
	eventsRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.request/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "events" },
			runId: { $ref: "#/$defs/id" },
			after: { $ref: "#/$defs/id" },
			limit: { type: "integer", minimum: 1, maximum: 500 },
		},
		required: ["protocol", "requestId", "action", "runId"],
	},
	usage: {
		type: "object",
		additionalProperties: false,
		properties: {
			inputTokens: { $ref: "#/$defs/uint" },
			outputTokens: { $ref: "#/$defs/uint" },
			cacheReadTokens: { $ref: "#/$defs/uint" },
			cacheWriteTokens: { $ref: "#/$defs/uint" },
			totalTokens: { $ref: "#/$defs/uint" },
			costMicrousd: { anyOf: [{ $ref: "#/$defs/uint" }, { type: "null" }] },
			finality: { enum: ["final", "known_prefix"] },
		},
		required: [
			"inputTokens",
			"outputTokens",
			"cacheReadTokens",
			"cacheWriteTokens",
			"totalTokens",
			"costMicrousd",
			"finality",
		],
	},
	resultText: {
		type: "object",
		additionalProperties: false,
		properties: {
			kind: { const: "text" },
			text: { type: "string", minLength: 1, maxLength: 65536, "x-utf8MaxBytes": 262144 },
			utf8Bytes: { type: "integer", minimum: 1, maximum: 262144 },
			sha256: { $ref: "#/$defs/digest" },
		},
		required: ["kind", "text", "utf8Bytes", "sha256"],
	},
	resultNone: {
		type: "object",
		additionalProperties: false,
		properties: {
			kind: { const: "none" },
			reason: { enum: ["no_assistant", "provider_error", "cancelled", "unknown"] },
		},
		required: ["kind", "reason"],
	},
	resultTooLarge: {
		type: "object",
		additionalProperties: false,
		properties: {
			kind: { const: "too_large" },
			utf8Bytes: { type: "integer", minimum: 262145, maximum: 9007199254740991 },
			sha256: { $ref: "#/$defs/digest" },
		},
		required: ["kind", "utf8Bytes", "sha256"],
	},
	turnResult: {
		oneOf: [{ $ref: "#/$defs/resultText" }, { $ref: "#/$defs/resultNone" }, { $ref: "#/$defs/resultTooLarge" }],
	},
	safeError: {
		type: "object",
		additionalProperties: false,
		properties: {
			code: {
				enum: [
					"PROVIDER_FAILED",
					"AUTH_FAILED",
					"MODEL_UNAVAILABLE",
					"CANCELLED",
					"RESULT_INVALID",
					"USAGE_INVALID",
					"EXECUTION_UNKNOWN",
					"INTERNAL_ERROR",
				],
			},
			message: { $ref: "#/$defs/boundedText" },
			retryable: { type: "boolean" },
		},
		required: ["code", "message", "retryable"],
	},
	turnSettlement: {
		oneOf: [
			{ $ref: "#/$defs/completedSettlement" },
			{ $ref: "#/$defs/failedSettlement" },
			{ $ref: "#/$defs/cancelledSettlement" },
			{ $ref: "#/$defs/unknownSettlement" },
		],
	},
	eventData: {
		type: "object",
		additionalProperties: false,
		properties: {
			nodeId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			attemptId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			operationId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			evidenceDigest: { anyOf: [{ $ref: "#/$defs/digest" }, { type: "null" }] },
			outcome: {
				anyOf: [
					{
						enum: [
							"succeeded",
							"failed",
							"cancelled",
							"budget_exceeded",
							"execution_unknown",
							"accepted",
							"rejected",
							"completed",
						],
					},
					{ type: "null" },
				],
			},
		},
		required: ["nodeId", "attemptId", "operationId", "evidenceDigest", "outcome"],
	},
	controllerEvent: {
		oneOf: [
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "RunAdmitted" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "RunStarted" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "RunCancellationRequested" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "RunDraining" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "RunTerminalized" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							evidenceDigest: { $ref: "#/$defs/digest" },
							outcome: { enum: ["succeeded", "failed", "cancelled", "budget_exceeded", "execution_unknown"] },
						},
						required: ["evidenceDigest", "outcome"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "RunQuarantined" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "NodeBecameReady" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { nodeId: { $ref: "#/$defs/id" }, evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["nodeId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "NodeBlocked" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { nodeId: { $ref: "#/$defs/id" }, evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["nodeId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptPrepared" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["nodeId", "attemptId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptDispatchCommitted" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["nodeId", "attemptId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptAdmissionBound" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							operationId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							turnId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["nodeId", "attemptId", "operationId", "rlmChildId", "turnId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptCancellationRequested" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["nodeId", "attemptId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptSettlementObserved" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							settlementDigest: { $ref: "#/$defs/digest" },
							outcome: { enum: ["completed", "failed", "cancelled", "execution_unknown"] },
						},
						required: ["nodeId", "attemptId", "settlementDigest", "outcome"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptAccepted" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["nodeId", "attemptId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptRejected" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["nodeId", "attemptId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "AttemptOutcomeUnknown" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							nodeId: { $ref: "#/$defs/id" },
							attemptId: { $ref: "#/$defs/id" },
							settlementDigest: { $ref: "#/$defs/digest" },
							outcome: { const: "execution_unknown" },
						},
						required: ["nodeId", "attemptId", "settlementDigest", "outcome"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.event/v2" },
					eventId: { $ref: "#/$defs/id" },
					runId: { $ref: "#/$defs/id" },
					sequence: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					revision: { type: "integer", minimum: 1, maximum: 9007199254740991 },
					type: { const: "HostCursorAdvanced" },
					recordedAt: { $ref: "#/$defs/time" },
					controllerEpoch: { $ref: "#/$defs/uint" },
					cancelEpoch: { $ref: "#/$defs/uint" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { hostCursor: { $ref: "#/$defs/id" }, evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["hostCursor", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"eventId",
					"runId",
					"sequence",
					"revision",
					"type",
					"recordedAt",
					"controllerEpoch",
					"cancelEpoch",
					"data",
					"digest",
				],
			},
		],
	},
	childAdmitRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.admit" },
			attemptId: { $ref: "#/$defs/id" },
			workflowChildId: { $ref: "#/$defs/id" },
			turn: {
				type: "object",
				additionalProperties: false,
				properties: {
					prompt: { type: "string", minLength: 1, maxLength: 65536, "x-utf8MaxBytes": 65536 },
					model: { $ref: "#/$defs/id" },
					maxTurns: { const: 1 },
					tools: { const: "none" },
				},
				required: ["prompt", "model", "maxTurns", "tools"],
			},
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: [
			"protocol",
			"requestId",
			"nodeId",
			"operation",
			"attemptId",
			"workflowChildId",
			"turn",
			"requestDigest",
		],
	},
	childSendRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.send" },
			attemptId: { $ref: "#/$defs/id" },
			rlmChildId: { $ref: "#/$defs/id" },
			turn: {
				type: "object",
				additionalProperties: false,
				properties: {
					prompt: { type: "string", minLength: 1, maxLength: 65536, "x-utf8MaxBytes": 65536 },
					model: { $ref: "#/$defs/id" },
					maxTurns: { const: 1 },
					tools: { const: "none" },
				},
				required: ["prompt", "model", "maxTurns", "tools"],
			},
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "attemptId", "rlmChildId", "turn", "requestDigest"],
	},
	childGetRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.get" },
			rlmChildId: { $ref: "#/$defs/id" },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "rlmChildId"],
	},
	childListRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.list" },
			after: { $ref: "#/$defs/id" },
			limit: { type: "integer", minimum: 1, maximum: 200 },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "limit"],
	},
	childEventsRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.events" },
			after: { $ref: "#/$defs/id" },
			limit: { type: "integer", minimum: 1, maximum: 500 },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "limit"],
	},
	childWaitRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.wait" },
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { $ref: "#/$defs/id" },
			timeoutMs: { type: "integer", minimum: 0, maximum: 30000 },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "rlmChildId", "turnId", "timeoutMs"],
	},
	childCancelRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.cancel" },
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { $ref: "#/$defs/id" },
			reason: { $ref: "#/$defs/boundedText" },
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "rlmChildId", "turnId", "reason", "requestDigest"],
	},
	childDeleteRequest: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-request/v2" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			operation: { const: "child.delete" },
			rlmChildId: { $ref: "#/$defs/id" },
			expectedLastTurnId: { $ref: "#/$defs/id" },
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: ["protocol", "requestId", "nodeId", "operation", "rlmChildId", "requestDigest"],
	},
	hostEventData: {
		type: "object",
		additionalProperties: false,
		properties: {
			requestId: { $ref: "#/$defs/id" },
			rlmChildId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			turnId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			settlement: { anyOf: [{ $ref: "#/$defs/turnSettlement" }, { type: "null" }] },
			evidenceDigest: { $ref: "#/$defs/digest" },
		},
		required: ["requestId", "rlmChildId", "turnId", "settlement", "evidenceDigest"],
	},
	retainedEvent: {
		oneOf: [
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "OperationAdmitted" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: { requestId: { $ref: "#/$defs/id" }, evidenceDigest: { $ref: "#/$defs/digest" } },
						required: ["requestId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "ChildAdmitted" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "TurnAdmitted" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							turnId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "turnId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "TurnStarted" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							turnId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "turnId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "TurnSettled" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							turnId: { $ref: "#/$defs/id" },
							settlement: { $ref: "#/$defs/turnSettlement" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "turnId", "settlement", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "CancelRequested" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							turnId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "turnId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "CancelActuated" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							turnId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "turnId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "ChildQuiescent" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "DeleteRequested" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "ChildTombstoned" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "ChildCleanupCompleted" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-event/v2" },
					hostEventId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					type: { const: "ChildCleanupFailed" },
					recordedAt: { $ref: "#/$defs/time" },
					data: {
						type: "object",
						additionalProperties: false,
						properties: {
							requestId: { $ref: "#/$defs/id" },
							rlmChildId: { $ref: "#/$defs/id" },
							evidenceDigest: { $ref: "#/$defs/digest" },
						},
						required: ["requestId", "rlmChildId", "evidenceDigest"],
					},
					digest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "hostEventId", "hostCursor", "type", "recordedAt", "data", "digest"],
			},
		],
	},
	publicError: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.error/v2" },
			requestId: { $ref: "#/$defs/id" },
			code: {
				enum: [
					"INVALID_REQUEST",
					"INVALID_DEFINITION",
					"POLICY_DENIED",
					"NOT_FOUND",
					"IDEMPOTENCY_CONFLICT",
					"REVISION_CONFLICT",
					"CONTROLLER_EPOCH_CONFLICT",
					"CANCEL_EPOCH_CONFLICT",
					"ILLEGAL_TRANSITION",
					"RETRY_UNSAFE",
					"CURSOR_INVALID",
					"SNAPSHOT_REQUIRED",
					"RESOURCE_LIMIT",
					"CAPABILITY_UNAVAILABLE",
					"STORE_UNAVAILABLE",
					"INTEGRITY_FAILURE",
					"ADMISSION_UNKNOWN",
					"CAPACITY_EXCEEDED",
				],
			},
			message: { $ref: "#/$defs/boundedText" },
			retryable: { type: "boolean" },
			currentRevision: { anyOf: [{ $ref: "#/$defs/uint" }, { type: "null" }] },
		},
		required: ["protocol", "requestId", "code", "message", "retryable", "currentRevision"],
	},
	validateResult: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.result/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "validate" },
			valid: { type: "boolean" },
			definitionDigest: { anyOf: [{ $ref: "#/$defs/digest" }, { type: "null" }] },
			errors: { type: "array", maxItems: 256, items: { $ref: "#/$defs/boundedText" } },
			warnings: { type: "array", maxItems: 256, items: { $ref: "#/$defs/boundedText" } },
		},
		required: ["protocol", "requestId", "action", "valid", "definitionDigest", "errors", "warnings"],
	},
	createResult: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.result/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "create" },
			runId: { $ref: "#/$defs/id" },
			definitionDigest: { $ref: "#/$defs/digest" },
			revision: { $ref: "#/$defs/uint" },
			controllerEpoch: { $ref: "#/$defs/uint" },
			cancelEpoch: { $ref: "#/$defs/uint" },
			disposition: { enum: ["created", "already_created"] },
			createdAt: { $ref: "#/$defs/time" },
			eventCursor: { $ref: "#/$defs/id" },
		},
		required: [
			"protocol",
			"requestId",
			"action",
			"runId",
			"definitionDigest",
			"revision",
			"controllerEpoch",
			"cancelEpoch",
			"disposition",
			"createdAt",
			"eventCursor",
		],
	},
	commandResult: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.result/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { enum: ["start", "cancel", "retry"] },
			runId: { $ref: "#/$defs/id" },
			commandId: { $ref: "#/$defs/id" },
			disposition: { enum: ["accepted", "already_applied"] },
			revision: { $ref: "#/$defs/uint" },
			controllerEpoch: { $ref: "#/$defs/uint" },
			cancelEpoch: { $ref: "#/$defs/uint" },
			projection: { $ref: "#/$defs/runProjection" },
			eventCursor: { $ref: "#/$defs/id" },
		},
		required: [
			"protocol",
			"requestId",
			"action",
			"runId",
			"commandId",
			"disposition",
			"revision",
			"controllerEpoch",
			"cancelEpoch",
			"projection",
			"eventCursor",
		],
	},
	attemptStatus: {
		type: "object",
		additionalProperties: false,
		properties: {
			attemptId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			rlmChildId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			turnId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			projection: { $ref: "#/$defs/attemptProjection" },
			hostTurn: { anyOf: [{ $ref: "#/$defs/turnProjection" }, { type: "null" }] },
		},
		required: ["attemptId", "nodeId", "rlmChildId", "turnId", "projection", "hostTurn"],
	},
	nodeStatus: {
		type: "object",
		additionalProperties: false,
		properties: {
			nodeId: { $ref: "#/$defs/id" },
			projection: { $ref: "#/$defs/nodeProjection" },
			attempts: { type: "array", maxItems: 64, items: { $ref: "#/$defs/attemptStatus" } },
		},
		required: ["nodeId", "projection", "attempts"],
	},
	statusResult: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.result/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "status" },
			runId: { $ref: "#/$defs/id" },
			definitionDigest: { $ref: "#/$defs/digest" },
			revision: { $ref: "#/$defs/uint" },
			controllerEpoch: { $ref: "#/$defs/uint" },
			cancelEpoch: { $ref: "#/$defs/uint" },
			projection: { $ref: "#/$defs/runProjection" },
			nodes: { type: "array", maxItems: 128, items: { $ref: "#/$defs/nodeStatus" } },
			eventCursor: { $ref: "#/$defs/id" },
		},
		required: [
			"protocol",
			"requestId",
			"action",
			"runId",
			"definitionDigest",
			"revision",
			"controllerEpoch",
			"cancelEpoch",
			"projection",
			"nodes",
			"eventCursor",
		],
	},
	eventsResult: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.result/v2" },
			requestId: { $ref: "#/$defs/id" },
			action: { const: "events" },
			runId: { $ref: "#/$defs/id" },
			events: { type: "array", maxItems: 500, items: { $ref: "#/$defs/controllerEvent" } },
			nextCursor: { $ref: "#/$defs/id" },
			caughtUp: { type: "boolean" },
		},
		required: ["protocol", "requestId", "action", "runId", "events", "nextCursor", "caughtUp"],
	},
	retainedResult: {
		oneOf: [
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.admit" },
					disposition: { const: "admitted" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.admit" },
					disposition: { const: "replayed" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.send" },
					disposition: { const: "admitted" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.send" },
					disposition: { const: "replayed" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.get" },
					disposition: { const: "snapshot" },
					child: { $ref: "#/$defs/childProjection" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: ["protocol", "requestId", "operation", "disposition", "child", "hostCursor", "receiptDigest"],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.list" },
					disposition: { const: "page" },
					children: { type: "array", maxItems: 200, items: { $ref: "#/$defs/childProjection" } },
					nextCursor: { $ref: "#/$defs/id" },
					caughtUp: { type: "boolean" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"children",
					"nextCursor",
					"caughtUp",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.events" },
					disposition: { const: "page" },
					events: { type: "array", maxItems: 500, items: { $ref: "#/$defs/retainedEvent" } },
					nextCursor: { $ref: "#/$defs/id" },
					caughtUp: { type: "boolean" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"events",
					"nextCursor",
					"caughtUp",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.wait" },
					disposition: { const: "pending" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					projection: { $ref: "#/$defs/turnProjection" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"projection",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.wait" },
					disposition: { const: "settled" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					settlement: { $ref: "#/$defs/turnSettlement" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"settlement",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.cancel" },
					disposition: { const: "requested" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
					actuation: { enum: ["requested", "actuated", "not_needed"] },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"hostCursor",
					"receiptDigest",
					"actuation",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.cancel" },
					disposition: { const: "already_settled" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					settlement: { $ref: "#/$defs/turnSettlement" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
					actuation: { enum: ["requested", "actuated", "not_needed"] },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"settlement",
					"hostCursor",
					"receiptDigest",
					"actuation",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.cancel" },
					disposition: { const: "replayed" },
					rlmChildId: { $ref: "#/$defs/id" },
					turnId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
					actuation: { enum: ["requested", "actuated", "not_needed"] },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"turnId",
					"hostCursor",
					"receiptDigest",
					"actuation",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.delete" },
					disposition: { const: "tombstoned" },
					rlmChildId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.delete" },
					disposition: { const: "cleanup_pending" },
					rlmChildId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"hostCursor",
					"receiptDigest",
				],
			},
			{
				type: "object",
				additionalProperties: false,
				properties: {
					protocol: { const: "prime.workflow.retained-result/v2" },
					requestId: { $ref: "#/$defs/id" },
					operation: { const: "child.delete" },
					disposition: { const: "replayed" },
					rlmChildId: { $ref: "#/$defs/id" },
					hostCursor: { $ref: "#/$defs/id" },
					receiptDigest: { $ref: "#/$defs/digest" },
				},
				required: [
					"protocol",
					"requestId",
					"operation",
					"disposition",
					"rlmChildId",
					"hostCursor",
					"receiptDigest",
				],
			},
		],
	},
	capability: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.capability/v2" },
			api: { const: "prime.workflow.retained" },
			version: { const: 2 },
			semantics: { const: "2026-09-14" },
			features: {
				type: "array",
				minItems: 7,
				maxItems: 7,
				uniqueItems: true,
				items: {
					enum: [
						"durable_request_id",
						"direct_parent_ownership",
						"per_turn_settlement",
						"cursor_replay",
						"cancel_fence",
						"tombstone_delete",
						"host_result_attribution",
					],
				},
				allOf: [
					{ contains: { const: "durable_request_id" } },
					{ contains: { const: "direct_parent_ownership" } },
					{ contains: { const: "per_turn_settlement" } },
					{ contains: { const: "cursor_replay" } },
					{ contains: { const: "cancel_fence" } },
					{ contains: { const: "tombstone_delete" } },
					{ contains: { const: "host_result_attribution" } },
				],
			},
			limits: {
				type: "object",
				additionalProperties: false,
				properties: {
					maxPromptUtf8Bytes: { const: 65536 },
					maxResultUtf8Bytes: { const: 262144 },
					maxPageSize: { const: 500 },
					maxWaitMs: { const: 30000 },
					maxChildren: { const: 10000 },
				},
				required: ["maxPromptUtf8Bytes", "maxResultUtf8Bytes", "maxPageSize", "maxWaitMs", "maxChildren"],
			},
		},
		required: ["protocol", "api", "version", "semantics", "features", "limits"],
	},
	view: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.view/v2" },
			runId: { $ref: "#/$defs/id" },
			viewRevision: { $ref: "#/$defs/id" },
			observedRevision: { $ref: "#/$defs/uint" },
			stale: { type: "boolean" },
			projection: { $ref: "#/$defs/runProjection" },
			nodes: { type: "array", maxItems: 128, items: { $ref: "#/$defs/nodeStatus" } },
			nativeRosterJoined: { type: "boolean" },
		},
		required: [
			"protocol",
			"runId",
			"viewRevision",
			"observedRevision",
			"stale",
			"projection",
			"nodes",
			"nativeRosterJoined",
		],
	},
	operationProjection: {
		type: "object",
		additionalProperties: false,
		properties: {
			phase: { enum: ["pending", "claimed", "acknowledged", "retry_wait", "terminal"] },
			intent: { enum: ["deliver", "cancel"] },
			outcome: { anyOf: [{ enum: ["succeeded", "failed", "ambiguous"] }, { type: "null" }] },
			conditions: {
				type: "array",
				uniqueItems: true,
				items: {
					enum: [
						"claim_unheld",
						"claim_held",
						"receipt_absent",
						"receipt_present",
						"effect_unclassified",
						"effect_classified",
					],
				},
			},
		},
		required: ["phase", "intent", "outcome", "conditions"],
		allOf: [
			{
				if: { properties: { phase: { const: "terminal" } } },
				then: { properties: { outcome: { enum: ["succeeded", "failed", "ambiguous"] } } },
				else: { properties: { outcome: { type: "null" } } },
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "claim_unheld" } }, { contains: { const: "claim_held" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [{ contains: { const: "receipt_absent" } }, { contains: { const: "receipt_present" } }],
						},
					},
				},
			},
			{
				not: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "effect_unclassified" } },
								{ contains: { const: "effect_classified" } },
							],
						},
					},
				},
			},
			{
				if: { properties: { outcome: { const: "succeeded" } }, required: ["outcome"] },
				then: {
					properties: {
						conditions: {
							allOf: [
								{ contains: { const: "receipt_present" } },
								{ contains: { const: "effect_classified" } },
								{ not: { contains: { const: "receipt_absent" } } },
								{ not: { contains: { const: "effect_unclassified" } } },
							],
						},
					},
				},
			},
		],
	},
	retainedError: {
		type: "object",
		additionalProperties: false,
		properties: {
			protocol: { const: "prime.workflow.retained-error/v2" },
			requestId: { $ref: "#/$defs/id" },
			code: {
				enum: [
					"CAPABILITY_UNAVAILABLE",
					"INVALID_REQUEST",
					"IDEMPOTENCY_CONFLICT",
					"NOT_FOUND",
					"PARENT_NOT_ACTIVE",
					"CHILD_NOT_AVAILABLE",
					"TURN_NOT_AVAILABLE",
					"ADMISSION_UNKNOWN",
					"SETTLEMENT_UNKNOWN",
					"MODEL_UNAVAILABLE",
					"AUTH_FAILED",
					"TOOL_SCOPE_DENIED",
					"DEPTH_LIMIT",
					"CHILD_BUSY",
					"CHILD_CANCELLING",
					"CHILD_NOT_QUIESCENT",
					"TURN_MISMATCH",
					"CURSOR_INVALID",
					"SNAPSHOT_REQUIRED",
					"OPERATION_ABORTED",
					"CLEANUP_FAILED",
					"STORE_CORRUPT",
					"INTERNAL_ERROR",
				],
			},
			message: { $ref: "#/$defs/boundedText" },
			retryable: { type: "boolean" },
		},
		required: ["protocol", "requestId", "code", "message", "retryable"],
	},
	completedSettlement: {
		type: "object",
		additionalProperties: false,
		properties: {
			authorityScope: { $ref: "#/$defs/digest" },
			parentId: { $ref: "#/$defs/id" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			attemptId: { $ref: "#/$defs/id" },
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { $ref: "#/$defs/id" },
			admittedAt: { $ref: "#/$defs/time" },
			startedAt: { anyOf: [{ $ref: "#/$defs/time" }, { type: "null" }] },
			settledAt: { $ref: "#/$defs/time" },
			cancelActuated: { type: "boolean" },
			descendantsQuiescent: { type: "boolean" },
			hostCursor: { $ref: "#/$defs/id" },
			settlementDigest: { $ref: "#/$defs/digest" },
			outcome: { const: "completed" },
			result: { $ref: "#/$defs/resultText" },
			usage: {
				type: "object",
				additionalProperties: false,
				properties: {
					inputTokens: { $ref: "#/$defs/uint" },
					outputTokens: { $ref: "#/$defs/uint" },
					cacheReadTokens: { $ref: "#/$defs/uint" },
					cacheWriteTokens: { $ref: "#/$defs/uint" },
					totalTokens: { $ref: "#/$defs/uint" },
					costMicrousd: { anyOf: [{ $ref: "#/$defs/uint" }, { type: "null" }] },
					finality: { const: "final" },
				},
				required: [
					"inputTokens",
					"outputTokens",
					"cacheReadTokens",
					"cacheWriteTokens",
					"totalTokens",
					"costMicrousd",
					"finality",
				],
			},
			error: { type: "null" },
			workflowChildId: { $ref: "#/$defs/id" },
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: [
			"authorityScope",
			"parentId",
			"requestId",
			"nodeId",
			"attemptId",
			"rlmChildId",
			"turnId",
			"admittedAt",
			"startedAt",
			"settledAt",
			"cancelActuated",
			"descendantsQuiescent",
			"hostCursor",
			"settlementDigest",
			"outcome",
			"result",
			"usage",
			"error",
			"workflowChildId",
			"requestDigest",
		],
	},
	failedSettlement: {
		type: "object",
		additionalProperties: false,
		properties: {
			authorityScope: { $ref: "#/$defs/digest" },
			parentId: { $ref: "#/$defs/id" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			attemptId: { $ref: "#/$defs/id" },
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { $ref: "#/$defs/id" },
			admittedAt: { $ref: "#/$defs/time" },
			startedAt: { anyOf: [{ $ref: "#/$defs/time" }, { type: "null" }] },
			settledAt: { $ref: "#/$defs/time" },
			cancelActuated: { type: "boolean" },
			descendantsQuiescent: { type: "boolean" },
			hostCursor: { $ref: "#/$defs/id" },
			settlementDigest: { $ref: "#/$defs/digest" },
			outcome: { const: "failed" },
			result: { oneOf: [{ $ref: "#/$defs/resultNone" }, { $ref: "#/$defs/resultTooLarge" }] },
			usage: {
				type: "object",
				additionalProperties: false,
				properties: {
					inputTokens: { $ref: "#/$defs/uint" },
					outputTokens: { $ref: "#/$defs/uint" },
					cacheReadTokens: { $ref: "#/$defs/uint" },
					cacheWriteTokens: { $ref: "#/$defs/uint" },
					totalTokens: { $ref: "#/$defs/uint" },
					costMicrousd: { anyOf: [{ $ref: "#/$defs/uint" }, { type: "null" }] },
					finality: { enum: ["final", "known_prefix"] },
				},
				required: [
					"inputTokens",
					"outputTokens",
					"cacheReadTokens",
					"cacheWriteTokens",
					"totalTokens",
					"costMicrousd",
					"finality",
				],
			},
			error: { $ref: "#/$defs/safeError" },
			workflowChildId: { $ref: "#/$defs/id" },
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: [
			"authorityScope",
			"parentId",
			"requestId",
			"nodeId",
			"attemptId",
			"rlmChildId",
			"turnId",
			"admittedAt",
			"startedAt",
			"settledAt",
			"cancelActuated",
			"descendantsQuiescent",
			"hostCursor",
			"settlementDigest",
			"outcome",
			"result",
			"usage",
			"error",
			"workflowChildId",
			"requestDigest",
		],
	},
	cancelledSettlement: {
		type: "object",
		additionalProperties: false,
		properties: {
			authorityScope: { $ref: "#/$defs/digest" },
			parentId: { $ref: "#/$defs/id" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			attemptId: { $ref: "#/$defs/id" },
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { $ref: "#/$defs/id" },
			admittedAt: { $ref: "#/$defs/time" },
			startedAt: { anyOf: [{ $ref: "#/$defs/time" }, { type: "null" }] },
			settledAt: { $ref: "#/$defs/time" },
			cancelActuated: { type: "boolean" },
			descendantsQuiescent: { type: "boolean" },
			hostCursor: { $ref: "#/$defs/id" },
			settlementDigest: { $ref: "#/$defs/digest" },
			outcome: { const: "cancelled" },
			result: { $ref: "#/$defs/resultNone" },
			usage: {
				type: "object",
				additionalProperties: false,
				properties: {
					inputTokens: { $ref: "#/$defs/uint" },
					outputTokens: { $ref: "#/$defs/uint" },
					cacheReadTokens: { $ref: "#/$defs/uint" },
					cacheWriteTokens: { $ref: "#/$defs/uint" },
					totalTokens: { $ref: "#/$defs/uint" },
					costMicrousd: { anyOf: [{ $ref: "#/$defs/uint" }, { type: "null" }] },
					finality: { enum: ["final", "known_prefix"] },
				},
				required: [
					"inputTokens",
					"outputTokens",
					"cacheReadTokens",
					"cacheWriteTokens",
					"totalTokens",
					"costMicrousd",
					"finality",
				],
			},
			error: { $ref: "#/$defs/safeError" },
			workflowChildId: { $ref: "#/$defs/id" },
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: [
			"authorityScope",
			"parentId",
			"requestId",
			"nodeId",
			"attemptId",
			"rlmChildId",
			"turnId",
			"admittedAt",
			"startedAt",
			"settledAt",
			"cancelActuated",
			"descendantsQuiescent",
			"hostCursor",
			"settlementDigest",
			"outcome",
			"result",
			"usage",
			"error",
			"workflowChildId",
			"requestDigest",
		],
	},
	unknownSettlement: {
		type: "object",
		additionalProperties: false,
		properties: {
			authorityScope: { $ref: "#/$defs/digest" },
			parentId: { $ref: "#/$defs/id" },
			requestId: { $ref: "#/$defs/id" },
			nodeId: { $ref: "#/$defs/id" },
			attemptId: { $ref: "#/$defs/id" },
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { $ref: "#/$defs/id" },
			admittedAt: { $ref: "#/$defs/time" },
			startedAt: { anyOf: [{ $ref: "#/$defs/time" }, { type: "null" }] },
			settledAt: { $ref: "#/$defs/time" },
			cancelActuated: { type: "boolean" },
			descendantsQuiescent: { type: "boolean" },
			hostCursor: { $ref: "#/$defs/id" },
			settlementDigest: { $ref: "#/$defs/digest" },
			outcome: { const: "execution_unknown" },
			result: { $ref: "#/$defs/resultNone" },
			usage: {
				type: "object",
				additionalProperties: false,
				properties: {
					inputTokens: { $ref: "#/$defs/uint" },
					outputTokens: { $ref: "#/$defs/uint" },
					cacheReadTokens: { $ref: "#/$defs/uint" },
					cacheWriteTokens: { $ref: "#/$defs/uint" },
					totalTokens: { $ref: "#/$defs/uint" },
					costMicrousd: { anyOf: [{ $ref: "#/$defs/uint" }, { type: "null" }] },
					finality: { enum: ["final", "known_prefix"] },
				},
				required: [
					"inputTokens",
					"outputTokens",
					"cacheReadTokens",
					"cacheWriteTokens",
					"totalTokens",
					"costMicrousd",
					"finality",
				],
			},
			error: { $ref: "#/$defs/safeError" },
			workflowChildId: { $ref: "#/$defs/id" },
			requestDigest: { $ref: "#/$defs/digest" },
		},
		required: [
			"authorityScope",
			"parentId",
			"requestId",
			"nodeId",
			"attemptId",
			"rlmChildId",
			"turnId",
			"admittedAt",
			"startedAt",
			"settledAt",
			"cancelActuated",
			"descendantsQuiescent",
			"hostCursor",
			"settlementDigest",
			"outcome",
			"result",
			"usage",
			"error",
			"workflowChildId",
			"requestDigest",
		],
	},
	childProjection: {
		type: "object",
		additionalProperties: false,
		properties: {
			rlmChildId: { $ref: "#/$defs/id" },
			turnId: { anyOf: [{ $ref: "#/$defs/id" }, { type: "null" }] },
			lifecycle: { enum: ["idle", "running", "cancelling", "quiescent", "tombstoned", "cleanup_failed"] },
		},
		required: ["rlmChildId", "turnId", "lifecycle"],
	},
} as const;
type Schema = Record<string, any>;

function fail(path: string, why: string): never {
	throw new Error(`${path} ${why}`);
}
function isObject(v: unknown): v is Record<string, unknown> {
	return v !== null && typeof v === "object" && !Array.isArray(v);
}
function same(a: unknown, b: unknown): boolean {
	return JSON.stringify(a) === JSON.stringify(b);
}
function hasUnpairedSurrogate(s: string): boolean {
	for (let i = 0; i < s.length; i++) {
		const c = s.charCodeAt(i);
		if (c >= 0xd800 && c <= 0xdbff) {
			const n = s.charCodeAt(++i);
			if (!(n >= 0xdc00 && n <= 0xdfff)) return true;
		} else if (c >= 0xdc00 && c <= 0xdfff) return true;
	}
	return false;
}
function resolve(s: Schema): Schema {
	if (s.$ref) {
		const prefix = "#/$defs/";
		if (typeof s.$ref !== "string" || !s.$ref.startsWith(prefix)) throw new Error("unsupported schema reference");
		const d = (defs as Record<string, Schema>)[s.$ref.slice(prefix.length)];
		if (!d) throw new Error("unknown schema reference");
		return d;
	}
	return s;
}
function testSchema(value: unknown, schema: Schema): boolean {
	try {
		validate(value, schema, "$", false);
		return true;
	} catch {
		return false;
	}
}
function validate(value: unknown, raw: Schema | undefined, path: string, utf8 = true): void {
	if (!raw) return;
	const schema = resolve(raw);
	if (schema.oneOf) {
		const hits = schema.oneOf.filter((x: Schema) => testSchema(value, x)).length;
		if (hits !== 1) fail(path, "does not match exactly one closed variant");
		validate(
			value,
			schema.oneOf.find((x: Schema) => testSchema(value, x)),
			path,
			utf8,
		);
		return;
	}
	if (schema.anyOf) {
		const hit = schema.anyOf.find((x: Schema) => testSchema(value, x));
		if (!hit) fail(path, "does not match any allowed variant");
		validate(value, hit, path, utf8);
	}
	if (schema.const !== undefined && !same(value, schema.const)) fail(path, "has the wrong constant");
	if (schema.enum && !schema.enum.some((x: unknown) => same(x, value))) fail(path, "is outside the closed enum");
	if (schema.type === "object" || schema.properties || schema.required) {
		if (!isObject(value)) fail(path, "must be an object");
		const props = schema.properties ?? {};
		const required: string[] = schema.required ?? [];
		for (const key of required) if (!Object.hasOwn(value, key)) fail(`${path}.${key}`, "is required");
		if (schema.additionalProperties === false)
			for (const key of Object.keys(value)) if (!Object.hasOwn(props, key)) fail(`${path}.${key}`, "is unknown");
		for (const [key, child] of Object.entries(props))
			if (Object.hasOwn(value, key)) validate(value[key], child as Schema, `${path}.${key}`, utf8);
	} else if (schema.type === "array") {
		if (!Array.isArray(value)) fail(path, "must be an array");
		if (schema.minItems !== undefined && value.length < schema.minItems) fail(path, "has too few items");
		if (schema.maxItems !== undefined && value.length > schema.maxItems) fail(path, "has too many items");
		if (schema.uniqueItems) {
			const seen = new Set<string>();
			for (const x of value) {
				const k = canonicalJson(x);
				if (seen.has(k)) fail(path, "has duplicate items");
				seen.add(k);
			}
		}
		if (schema.items) for (const [i, x] of value.entries()) validate(x, schema.items, `${path}[${i}]`, utf8);
	} else if (schema.type === "string") {
		if (typeof value !== "string") fail(path, "must be a string");
		if (hasUnpairedSurrogate(value)) fail(path, "contains invalid Unicode");
		const points = [...value].length;
		if (schema.minLength !== undefined && points < schema.minLength) fail(path, "is too short");
		if (schema.maxLength !== undefined && points > schema.maxLength) fail(path, "is too long");
		if (schema["x-utf8MaxBytes"] !== undefined && Buffer.byteLength(value, "utf8") > schema["x-utf8MaxBytes"])
			fail(path, "exceeds its UTF-8 byte bound");
		if (schema.pattern && !new RegExp(schema.pattern).test(value)) fail(path, "has invalid syntax");
		if (schema.format === "date-time") {
			const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d+)?Z$/.exec(value);
			if (!m || !Number.isFinite(Date.parse(value))) fail(path, "must be RFC 3339 UTC");
			const d = new Date(value);
			if (
				d.getUTCFullYear() !== +m[1] ||
				d.getUTCMonth() + 1 !== +m[2] ||
				d.getUTCDate() !== +m[3] ||
				d.getUTCHours() !== +m[4] ||
				d.getUTCMinutes() !== +m[5] ||
				d.getUTCSeconds() !== +m[6]
			)
				fail(path, "must be a real RFC 3339 UTC time");
		}
	} else if (schema.type === "integer") {
		if (!Number.isSafeInteger(value)) fail(path, "must be a safe integer");
		if (schema.minimum !== undefined && (value as number) < schema.minimum) fail(path, "is below its minimum");
		if (schema.maximum !== undefined && (value as number) > schema.maximum) fail(path, "is above its maximum");
	} else if (schema.type === "boolean") {
		if (typeof value !== "boolean") fail(path, "must be boolean");
	} else if (schema.type === "null") {
		if (value !== null) fail(path, "must be null");
	}
	if (schema.allOf) for (const x of schema.allOf) validate(value, x, path, utf8);
	if (schema.if) validate(value, testSchema(value, schema.if) ? schema.then : schema.else, path, utf8);
	if (schema.not && testSchema(value, schema.not)) fail(path, "matches a forbidden shape");
	if (schema.contains) {
		if (!Array.isArray(value) || !value.some((x) => testSchema(x, schema.contains)))
			fail(path, "lacks a required item");
	}
}

class StrictJsonParser {
	private i = 0;
	private nodes = 0;
	constructor(private readonly source: string) {}
	parse(): unknown {
		this.ws();
		const out = this.value(1);
		this.ws();
		if (this.i !== this.source.length) fail("$", "has trailing content");
		return out;
	}
	private bump(depth: number): void {
		if (depth > MAX_DEPTH) fail("$", "exceeds maximum depth");
		if (++this.nodes > MAX_NODES) fail("$", "exceeds maximum JSON nodes");
	}
	private ws(): void {
		while (/[\x20\t\r\n]/.test(this.source[this.i] ?? "")) this.i++;
	}
	private value(depth: number): unknown {
		this.ws();
		this.bump(depth);
		const c = this.source[this.i];
		if (c === "{") return this.object(depth);
		if (c === "[") return this.array(depth);
		if (c === '"') return this.string();
		if (c === "t" && this.take("true")) return true;
		if (c === "f" && this.take("false")) return false;
		if (c === "n" && this.take("null")) return null;
		return this.number();
	}
	private take(s: string): boolean {
		if (this.source.slice(this.i, this.i + s.length) !== s) return false;
		this.i += s.length;
		return true;
	}
	private string(): string {
		const start = this.i++;
		while (this.i < this.source.length) {
			const c = this.source[this.i++];
			if (c === '"') {
				const token = this.source.slice(start, this.i);
				try {
					const v = JSON.parse(token);
					if (typeof v !== "string" || hasUnpairedSurrogate(v)) fail("$", "contains invalid Unicode");
					return v;
				} catch {
					fail("$", "contains an invalid JSON string");
				}
			}
			if (c === "\\") {
				const e = this.source[this.i++];
				if (e === "u") {
					if (!/^[0-9a-fA-F]{4}$/.test(this.source.slice(this.i, this.i + 4)))
						fail("$", "contains an invalid escape");
					this.i += 4;
				} else if (!'"\\/bfnrt'.includes(e ?? "")) fail("$", "contains an invalid escape");
			} else if (c === undefined || c < " ") fail("$", "contains an invalid string character");
		}
		fail("$", "contains an unterminated string");
	}
	private number(): number {
		const m = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(this.source.slice(this.i));
		if (!m || m.index !== 0) fail("$", "contains invalid JSON");
		this.i += m[0].length;
		const n = Number(m[0]);
		if (!Number.isFinite(n)) fail("$", "contains a non-finite number");
		return n;
	}
	private object(depth: number): Record<string, unknown> {
		this.i++;
		this.ws();
		const out: Record<string, unknown> = {};
		const seen = new Set<string>();
		if (this.source[this.i] === "}") {
			this.i++;
			return out;
		}
		for (;;) {
			this.ws();
			if (this.source[this.i] !== '"') fail("$", "contains an invalid object key");
			const k = this.string();
			if (seen.has(k)) fail("$", "contains a duplicate object key");
			seen.add(k);
			this.ws();
			if (this.source[this.i++] !== ":") fail("$", "contains an invalid object");
			out[k] = this.value(depth + 1);
			this.ws();
			const c = this.source[this.i++];
			if (c === "}") return out;
			if (c !== ",") fail("$", "contains an invalid object");
		}
	}
	private array(depth: number): unknown[] {
		this.i++;
		this.ws();
		const out: unknown[] = [];
		if (this.source[this.i] === "]") {
			this.i++;
			return out;
		}
		for (;;) {
			out.push(this.value(depth + 1));
			this.ws();
			const c = this.source[this.i++];
			if (c === "]") return out;
			if (c !== ",") fail("$", "contains an invalid array");
		}
	}
}

export function parseWorkflowV2Json(input: string | Uint8Array): unknown {
	let text: string;
	if (typeof input === "string") {
		if (Buffer.byteLength(input, "utf8") > MAX_MESSAGE_BYTES) fail("$", "exceeds 1 MiB");
		text = input;
	} else {
		if (input.byteLength > MAX_MESSAGE_BYTES) fail("$", "exceeds 1 MiB");
		text = new TextDecoder("utf-8", { fatal: true }).decode(input);
	}
	return new StrictJsonParser(text).parse();
}
function validateDigestBindings(value: unknown, path = "$", seen = new Set<unknown>()): void {
	if (!value || typeof value !== "object" || seen.has(value)) return;
	seen.add(value);
	if (
		isObject(value) &&
		typeof value.text === "string" &&
		Object.hasOwn(value, "utf8Bytes") &&
		Object.hasOwn(value, "sha256")
	) {
		const bytes = Buffer.byteLength(value.text, "utf8");
		const hash = `sha256:${createHash("sha256").update(value.text, "utf8").digest("hex")}`;
		if (value.utf8Bytes !== bytes || value.sha256 !== hash) fail(path, "has a result byte/digest mismatch");
	}
	if (Array.isArray(value)) for (const [i, v] of value.entries()) validateDigestBindings(v, `${path}[${i}]`, seen);
	else for (const [k, v] of Object.entries(value)) validateDigestBindings(v, `${path}.${k}`, seen);
}
function decode(value: unknown, name: keyof typeof defs): WorkflowV2Value {
	validate(value, (defs as any)[name], "$");
	validateDigestBindings(value);
	return value as WorkflowV2Value;
}
function fromJson(input: string | Uint8Array, decoder: (v: unknown) => WorkflowV2Value): WorkflowV2Value {
	return decoder(parseWorkflowV2Json(input));
}
export function canonicalJson(value: unknown): string {
	if (value === null || typeof value === "boolean" || typeof value === "string") return JSON.stringify(value);
	if (typeof value === "number") {
		if (!Number.isFinite(value)) throw new Error("non-finite canonical number");
		return JSON.stringify(value);
	}
	if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
	if (isObject(value))
		return `{${Object.keys(value)
			.sort()
			.map((k) => `${JSON.stringify(k)}:${canonicalJson(value[k])}`)
			.join(",")}}`;
	throw new Error("unsupported canonical JSON value");
}
export function workflowV2RequestDigest(request: unknown): string {
	return `sha256:${createHash("sha256").update(canonicalJson(request)).digest("hex")}`;
}

export const decodeWorkflowV2Definition = (v: unknown) => decode(v, "definition");
export function decodeWorkflowV2PublicRequest(v: unknown): WorkflowV2Value {
	const x = v as any;
	const map: any = {
		validate: "validateRequest",
		create: "createRequest",
		start: "startRequest",
		cancel: "cancelRequest",
		retry: "retryRequest",
		status: "statusRequest",
		events: "eventsRequest",
	};
	return decode(v, map[x?.action] ?? "validateRequest");
}
export function decodeWorkflowV2PublicResult(v: unknown): WorkflowV2Value {
	const x = v as any;
	const map: any = {
		validate: "validateResult",
		create: "createResult",
		start: "commandResult",
		cancel: "commandResult",
		retry: "commandResult",
		status: "statusResult",
		events: "eventsResult",
	};
	return decode(v, map[x?.action] ?? "validateResult");
}
export const decodeWorkflowV2PublicError = (v: unknown) => decode(v, "publicError");
export const decodeWorkflowV2ControllerEvent = (v: unknown) => decode(v, "controllerEvent");
export function decodeWorkflowV2RetainedRequest(v: unknown): WorkflowV2Value {
	const x = v as any;
	const map: any = {
		"child.admit": "childAdmitRequest",
		"child.send": "childSendRequest",
		"child.get": "childGetRequest",
		"child.list": "childListRequest",
		"child.events": "childEventsRequest",
		"child.wait": "childWaitRequest",
		"child.cancel": "childCancelRequest",
		"child.delete": "childDeleteRequest",
	};
	return decode(v, map[x?.operation] ?? "childGetRequest");
}
export const decodeWorkflowV2RetainedResult = (v: unknown) => decode(v, "retainedResult");
export const decodeWorkflowV2RetainedError = (v: unknown) => decode(v, "retainedError");
export const decodeWorkflowV2RetainedEvent = (v: unknown) => decode(v, "retainedEvent");
export const decodeWorkflowV2Capability = (v: unknown) => decode(v, "capability");
export const decodeWorkflowV2View = (v: unknown) => decode(v, "view");
export const decodeWorkflowV2TurnSettlement = (v: unknown) => decode(v, "turnSettlement");

export const decodeWorkflowV2PublicRequestJson = (v: string | Uint8Array) => fromJson(v, decodeWorkflowV2PublicRequest);
export const decodeWorkflowV2PublicResultJson = (v: string | Uint8Array) => fromJson(v, decodeWorkflowV2PublicResult);
export const decodeWorkflowV2RetainedRequestJson = (v: string | Uint8Array) =>
	fromJson(v, decodeWorkflowV2RetainedRequest);
export const decodeWorkflowV2RetainedResultJson = (v: string | Uint8Array) =>
	fromJson(v, decodeWorkflowV2RetainedResult);
