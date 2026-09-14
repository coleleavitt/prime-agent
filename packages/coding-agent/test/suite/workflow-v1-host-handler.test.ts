import { fauxAssistantMessage, fauxText } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import type { HostRequestHandlers } from "../../src/core/kernel/index.js";
import { createHarness } from "./harness.js";

function registerHarnessProvider(
	harness: Awaited<ReturnType<typeof createHarness>>,
	config: { apiKey?: string; headers?: Record<string, string>; authHeader?: boolean },
): void {
	const model = harness.getModel();
	harness.session.modelRegistry.registerProvider(model.provider, {
		...config,
		baseUrl: model.baseUrl,
		api: harness.faux.api,
		models: harness.faux.models.map((registeredModel) => ({
			id: registeredModel.id,
			name: registeredModel.name,
			api: registeredModel.api,
			reasoning: registeredModel.reasoning,
			input: registeredModel.input,
			cost: registeredModel.cost,
			contextWindow: registeredModel.contextWindow,
			maxTokens: registeredModel.maxTokens,
			baseUrl: registeredModel.baseUrl,
		})),
	});
}

const request = {
	protocol: "prime.workflow.run-agent/v1",
	requestId: "req-1",
	nodeId: "node-1",
	prompt: "one turn",
	model: null,
	maxTurns: 1,
	maxResultUtf8Bytes: 1024,
	drainTimeoutMs: 1000,
	tools: "none",
};

describe("workflow.run_agent host handler", () => {
	it("exposes only the exact workflow.run_agent route and protocol", async () => {
		const harness = await createHarness({ provider: "workflow-host-route", withConfiguredAuth: true });
		try {
			const handlers = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers();
			expect(handlers["workflow.run_agent"]).toBeTypeOf("function");
			expect(handlers).not.toHaveProperty("workflow.runAgent");
			expect(handlers).not.toHaveProperty("workflow.run-agent");
			expect(handlers).not.toHaveProperty("prime.workflow.run-agent/v1");
		} finally {
			harness.cleanup();
		}
	});

	it.each([
		["old protocol", { ...request, protocol: "prime.workflow.run-agent/v0" }],
		[
			"mixed protocol",
			{ ...request, protocol: "prime.workflow.run-agent/v1", legacyProtocol: "prime.workflow.run-agent/v0" },
		],
	])("fails closed before provider I/O for %s", async (_name, invalidRequest) => {
		const harness = await createHarness({ provider: "workflow-host-wire", withConfiguredAuth: true });
		try {
			harness.setResponses([fauxAssistantMessage("must remain")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			await expect(
				handler(
					{ request: invalidRequest },
					{ signal: new AbortController().signal, requestId: "transport-invalid" },
				),
			).rejects.toThrow();
			expect(harness.getPendingResponseCount()).toBe(1);
		} finally {
			harness.cleanup();
		}
	});

	it("fails closed on absent auth before provider I/O even for the active faux model", async () => {
		const harness = await createHarness({ provider: "workflow-host-absent-auth", withConfiguredAuth: false });
		try {
			expect(harness.session.modelRegistry.getProviderAuthStatus(harness.getModel().provider)).toMatchObject({
				configured: false,
			});
			harness.setResponses([fauxAssistantMessage("must remain")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request },
				{ signal: new AbortController().signal, requestId: "transport-absent-auth" },
			);
			expect(result).toMatchObject({
				outcome: "failed",
				stopReason: "model_resolution_failed",
				resolvedModel: null,
				turnsStarted: 0,
				result: null,
			});
			expect(harness.getPendingResponseCount()).toBe(1);
		} finally {
			harness.cleanup();
		}
	});

	it("allows a registered provider with an explicit no-auth policy", async () => {
		const harness = await createHarness({ provider: "workflow-host-approved-no-auth", withConfiguredAuth: false });
		try {
			registerHarnessProvider(harness, { authHeader: false });
			harness.setResponses([fauxAssistantMessage("local result")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request },
				{ signal: new AbortController().signal, requestId: "transport-approved-no-auth" },
			);
			expect(result).toMatchObject({ outcome: "completed", turnsStarted: 1, result: { text: "local result" } });
		} finally {
			harness.cleanup();
		}
	});

	it("requires a resolvable credential when authHeader is enabled", async () => {
		const provider = "workflow-host-auth-header-missing";
		const envName = "WORKFLOW_HOST_MISSING_API_KEY";
		const previous = process.env[envName];
		process.env[envName] = "";
		const harness = await createHarness({ provider, withConfiguredAuth: false });
		try {
			registerHarnessProvider(harness, { apiKey: envName, authHeader: true });
			harness.setResponses([fauxAssistantMessage("must remain")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request },
				{ signal: new AbortController().signal, requestId: "transport-auth-header-missing" },
			);
			expect(result).toMatchObject({
				outcome: "failed",
				stopReason: "model_resolution_failed",
				resolvedModel: null,
				turnsStarted: 0,
			});
			expect(harness.getPendingResponseCount()).toBe(1);
		} finally {
			if (previous === undefined) delete process.env[envName];
			else process.env[envName] = previous;
			harness.cleanup();
		}
	});

	it.each([
		["literal authHeader key", { apiKey: "workflow-key", authHeader: true }],
		["command-backed authHeader key", { apiKey: "!printf workflow-command-key", authHeader: true }],
		[
			"command-backed header on approved no-auth provider",
			{ headers: { "X-Local-Auth": "!printf local-header" }, authHeader: false },
		],
	])("accepts %s during preflight", async (_name, config) => {
		const harness = await createHarness({ provider: `workflow-host-${_name}`, withConfiguredAuth: false });
		try {
			registerHarnessProvider(harness, config);
			harness.setResponses([fauxAssistantMessage("configured")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request },
				{ signal: new AbortController().signal, requestId: "transport-configured-auth" },
			);
			expect(result).toMatchObject({ outcome: "completed", turnsStarted: 1, result: { text: "configured" } });
		} finally {
			harness.cleanup();
		}
	});

	it("authenticates and resolves the model before any physical provider request", async () => {
		const harness = await createHarness({ provider: "workflow-host-no-auth", withConfiguredAuth: true });
		try {
			expect(harness.session.modelRegistry.markProviderAuthStale(harness.getModel().provider)).toBe(true);
			expect(harness.session.modelRegistry.markProviderAuthStale(harness.getModel().provider)).toBe(true);
			harness.setResponses([fauxAssistantMessage("must remain")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request },
				{ signal: new AbortController().signal, requestId: "transport-no-auth" },
			);
			expect(result).toMatchObject({
				outcome: "failed",
				stopReason: "model_resolution_failed",
				resolvedModel: null,
				turnsStarted: 0,
				result: null,
			});
			expect(harness.getPendingResponseCount()).toBe(1);
		} finally {
			harness.cleanup();
		}
	});

	it("does not enter session, RLM, or child-session routes", async () => {
		const harness = await createHarness({ provider: "workflow-host-isolated", withConfiguredAuth: true });
		try {
			harness.setResponses([fauxAssistantMessage("isolated")]);
			const session = harness.session as unknown as Record<string, unknown>;
			const forbidden = vi.fn(() => {
				throw new Error("alternate route entered");
			});
			for (const name of ["prompt", "runRlmChild", "createRlmSession"]) session[name] = forbidden;
			const beforeMessages = harness.session.messages.length;
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request },
				{ signal: new AbortController().signal, requestId: "transport-isolated" },
			);
			expect(result).toMatchObject({ outcome: "completed", result: { text: "isolated" } });
			expect(forbidden).not.toHaveBeenCalled();
			expect(harness.session.messages).toHaveLength(beforeMessages);
			expect(harness.getPendingResponseCount()).toBe(0);
		} finally {
			harness.cleanup();
		}
	});

	it("uses the current authenticated model and returns the closed authoritative result", async () => {
		const harness = await createHarness({ provider: "workflow-host", withConfiguredAuth: true });
		try {
			harness.setResponses([fauxAssistantMessage([fauxText("hé"), fauxText("llo")])]);
			const handlers = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers();
			const handler = handlers["workflow.run_agent"];
			if (!handler) throw new Error("missing workflow.run_agent handler");
			const result = await handler({ request }, { signal: new AbortController().signal, requestId: "transport-1" });
			expect(result).toMatchObject({
				protocol: "prime.workflow.run-agent-result/v1",
				requestId: "req-1",
				nodeId: "node-1",
				resolvedModel: `${harness.getModel().provider}/${harness.getModel().id}`,
				turnsStarted: 1,
				outcome: "completed",
				stopReason: "completed",
				result: { text: "héllo", utf8Bytes: 6 },
			});
			expect(harness.getPendingResponseCount()).toBe(0);
		} finally {
			harness.cleanup();
		}
	});

	it("fails closed before provider I/O for an unavailable exact model", async () => {
		const harness = await createHarness({ provider: "workflow-host-fail", withConfiguredAuth: true });
		try {
			harness.setResponses([fauxAssistantMessage("must remain")]);
			const handler = (
				harness.session as unknown as { _createKernelHostHandlers(): HostRequestHandlers }
			)._createKernelHostHandlers()["workflow.run_agent"]!;
			const result = await handler(
				{ request: { ...request, model: "missing/model" } },
				{ signal: new AbortController().signal, requestId: "transport-2" },
			);
			expect(result).toMatchObject({
				outcome: "failed",
				stopReason: "model_resolution_failed",
				turnsStarted: 0,
				result: null,
			});
			expect(harness.getPendingResponseCount()).toBe(1);
		} finally {
			harness.cleanup();
		}
	});
});
