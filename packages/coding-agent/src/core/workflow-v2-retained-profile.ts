import { Agent, type StreamFn, type ThinkingLevel, type ToolCallPolicy } from "@earendil-works/pi-agent-core";
import type { Model, ServiceTier, Transport } from "@earendil-works/pi-ai";
import type { Theme } from "../modes/interactive/theme/theme.js";
import { AgentSession } from "./agent-session.js";
import type { SessionStartEvent } from "./extensions/index.js";
import { createExtensionRuntime } from "./extensions/loader.js";
import type { LoadExtensionsResult } from "./extensions/types.js";
import type { ModelRegistry } from "./model-registry.js";
import type { PromptTemplate } from "./prompt-templates.js";
import type { ResourceDiagnostic, ResourceLoader } from "./resource-loader.js";
import type { RlmSubagentRuntime } from "./rlm-runtime.js";
import { SessionManager } from "./session-manager.js";
import type { SettingsManager } from "./settings-manager.js";
import type { Skill } from "./skills.js";

/**
 * Closed discriminant for the Workflow V2 Slice 3 retained-child construction
 * profile. This is the only profile that {@link createWorkflowV2RetainedChildRuntime}
 * builds and is repeated in the admission receipt, ledger record, and
 * materialization command per docs/WORKFLOW-V2-SLICE3.md §6.
 */
export const WORKFLOW_V2_TOOLS_NONE_PROFILE = "workflow-v2-tools-none-v1" as const;
export type WorkflowV2RetainedProfile = typeof WORKFLOW_V2_TOOLS_NONE_PROFILE;

/** A tools-none retained child admits exactly one turn. */
export const WORKFLOW_V2_TOOLS_NONE_MAX_TURNS = 1 as const;

/**
 * The only provider capability a retained tools-none child may reuse from its
 * parent: the credential-aware provider transport for one already-resolved
 * model. This deliberately excludes parent `convertToLlm`, `transformContext`,
 * `onPayload`, `onResponse`, `toolExecution`, and any extension-modified stream
 * wrapper. Section 6 forbids inheriting those.
 */
export interface WorkflowV2ParentTransportAuthority {
	/** Credential-aware provider transport for the resolved model. */
	streamFn: StreamFn;
	/** Credential lookup used only by the resolved model's provider. */
	getApiKey?: (provider: string) => Promise<string | undefined> | string | undefined;
	/** Wire transport preference; not a parent extension hook. */
	transport?: Transport;
}

/**
 * Durable admission facts required to materialize an already-admitted retained
 * child. Identity fields (child id, turn id, session paths, generations, route)
 * are preallocated by the supervisor; the executor lane binds them. This factory
 * consumes them and never allocates identity.
 */
export interface WorkflowV2BoundRetainedAdmission {
	/** Supervisor-preallocated native child identity. */
	readonly rlmChildId: string;
	/** Stable sibling display name for the retained child. */
	readonly sessionName: string;
	/** Child session directory root chosen by the supervisor. */
	readonly sessionDir: string;
	/** Working directory inherited from the parent session. */
	readonly cwd: string;
	/** Optional agent config directory. */
	readonly agentDir?: string;
	/** The single resolved model bound by the admission receipt. */
	readonly model: Model<any>;
	/** The single resolved thinking level bound by the admission receipt. */
	readonly thinkingLevel: ThinkingLevel;
	/** Resolved service tier for the bound model. */
	readonly serviceTier: ServiceTier;
	/** Child depth (parent depth + 1). Also used as the max depth to forbid descendants. */
	readonly rlmDepth: number;
	/** Host model registry; never a parent scoped-model set. */
	readonly modelRegistry: ModelRegistry;
	/** Host settings manager. */
	readonly settingsManager: SettingsManager;
	/** Parent session file path, for native topology linkage. */
	readonly parentSessionFile?: string;
	/** Parent session id, for semantic-edge lineage. */
	readonly parentSessionId?: string;
	/** Parent display label, for native parent attribution. */
	readonly parentAgentLabel?: string;
	/** Supervisor node id that owns this child. */
	readonly rlmParentNodeId?: string;
	/** Request id of the admitting operation, for semantic-edge lineage. */
	readonly spawnedByRequestId?: string;
	/** Publish the session to the parent before it becomes addressable. */
	readonly onSessionPublished?: (session: AgentSession) => void;
}

/**
 * Construction-time attestation derived by inspecting the actually-constructed
 * runtime, not by echoing inputs. Every boolean is read back from the built
 * `Agent`/`AgentSession`. The executor and journal lanes digest this record into
 * the durable `profile_attested` fact; this module never canonicalizes or
 * digests it (the codec lane is the sole canonicalizer).
 */
export interface WorkflowV2RetainedProfileAttestation {
	readonly profile: WorkflowV2RetainedProfile;
	readonly tools: "none";
	readonly maxTurns: typeof WORKFLOW_V2_TOOLS_NONE_MAX_TURNS;
	/** Low-level `agent.state.tools` is empty. */
	readonly toolsEmpty: boolean;
	/** Low-level `agent.state.tools` is a frozen array. */
	readonly toolsFrozen: boolean;
	/** Low-level construction-time `toolCallPolicy` equals `reject`. */
	readonly toolCallPolicy: ToolCallPolicy;
	/**
	 * All session-level turn-continuation sources are disabled (goals,
	 * autonomous continuation, serialized refinement), so the retained child
	 * has no in-session path to a second task turn. The low-level Agent is also
	 * built with `shouldStopAfterTurn: () => true`, but `AgentSession` installs
	 * its own turn hook, so that option is not independently observable here;
	 * the executor lane enforces at-most-one physical provider dispatch.
	 */
	readonly continuationDisabled: boolean;
	/** Session active tool registry is empty. */
	readonly activeToolNames: readonly string[];
	/** No `IpythonKernelProvisioner` was constructed. */
	readonly kernelProvisioned: boolean;
	/** No kernel snapshot directory was assigned. */
	readonly kernelSnapshotAssigned: boolean;
	/** Number of extensions the isolated loader exposed (must be zero). */
	readonly extensionCount: number;
	/** The isolated resource loader exposes no skills/prompts/themes/context/system prompt. */
	readonly resourcesEmpty: boolean;
	/** No message/observe/heartbeat controller is installed. */
	readonly controllersInstalled: boolean;
	/** No MCP manager is installed. */
	readonly mcpInstalled: boolean;
	/** Long-running goals feature is disabled. */
	readonly goalsIncluded: boolean;
	/** Bundled compact skill is disabled. */
	readonly compactSkillIncluded: boolean;
	/** Autonomous continuation is disabled. */
	readonly autonomousEnabled: boolean;
	/** Serialized refinement is disabled. */
	readonly serializedRefine: boolean;
	/** Whether the child may create descendants (must be false). */
	readonly descendantsAllowed: boolean;
	readonly rlmDepth: number;
	readonly rlmMaxDepth: number;
	/** No parent `transformContext`/`onPayload`/`onResponse` hook was reused. */
	readonly parentHooksReused: boolean;
	readonly effectiveModel: Readonly<{ provider: string; id: string }>;
	readonly effectiveThinkingLevel: ThinkingLevel;
}

/** Stable typed failure when the constructed runtime violates the closed profile. */
export class WorkflowV2RetainedProfileError extends Error {
	constructor(
		readonly code: WorkflowV2RetainedProfileErrorCode,
		message: string,
	) {
		super(message);
		this.name = "WorkflowV2RetainedProfileError";
	}
}

export type WorkflowV2RetainedProfileErrorCode = "INVALID_ADMISSION" | "PROFILE_VIOLATION";

export interface WorkflowV2RetainedChildRuntime {
	readonly runtime: RlmSubagentRuntime;
	readonly attestation: WorkflowV2RetainedProfileAttestation;
}

/** Frozen empty resource collection reused by the isolated loader. */
const EMPTY_DIAGNOSTICS: readonly ResourceDiagnostic[] = Object.freeze([]);

/**
 * A permanently empty {@link ResourceLoader}. Every accessor returns immutable
 * empty collections, and `reload` is a no-op, so a rebuild/reload/resume can
 * never introduce an extension, skill, prompt, theme, context file, or system
 * prompt addition. `extendResources` is rejected: nothing may widen the profile.
 */
class IsolatedEmptyResourceLoader implements ResourceLoader {
	private readonly extensionsResult: LoadExtensionsResult;

	constructor() {
		// Empty, runtime-frozen collections. The static types stay mutable to
		// satisfy LoadExtensionsResult, but nothing ever mutates them and the
		// loader always returns the same frozen object.
		const extensions: LoadExtensionsResult["extensions"] = [];
		const errors: LoadExtensionsResult["errors"] = [];
		Object.freeze(extensions);
		Object.freeze(errors);
		this.extensionsResult = Object.freeze({
			extensions,
			errors,
			runtime: createExtensionRuntime(),
		}) as LoadExtensionsResult;
	}

	getExtensions(): LoadExtensionsResult {
		return this.extensionsResult;
	}

	getSkills(): { skills: Skill[]; diagnostics: ResourceDiagnostic[] } {
		return { skills: [], diagnostics: [...EMPTY_DIAGNOSTICS] };
	}

	getPrompts(): { prompts: PromptTemplate[]; diagnostics: ResourceDiagnostic[] } {
		return { prompts: [], diagnostics: [...EMPTY_DIAGNOSTICS] };
	}

	getThemes(): { themes: Theme[]; diagnostics: ResourceDiagnostic[] } {
		return { themes: [], diagnostics: [...EMPTY_DIAGNOSTICS] };
	}

	getAgentsFiles(): { agentsFiles: Array<{ path: string; content: string }> } {
		return { agentsFiles: [] };
	}

	getSystemPrompt(): string | undefined {
		return undefined;
	}

	getAppendSystemPrompt(): string[] {
		return [];
	}

	extendResources(): void {
		throw new WorkflowV2RetainedProfileError(
			"PROFILE_VIOLATION",
			"workflow-v2-tools-none-v1 forbids extending retained-child resources",
		);
	}

	async reload(): Promise<void> {
		// No-op: the isolated loader has nothing to reload and must stay empty.
	}
}

/** Same-package structural view over private construction state, for attestation only. */
interface RetainedSessionInternals {
	_ipythonKernelProvisioner?: unknown;
	_ipythonKernelSnapshotDir?: string;
	_agentMessageController?: unknown;
	_agentObserveController?: unknown;
	_rlmHeartbeatController?: unknown;
	_mcpManager?: unknown;
	_includeGoals: boolean;
	_includeCompactSkill: boolean;
	_serializedRefine: boolean;
	_autonomousState?: { enabled?: boolean };
	_resourceLoader: ResourceLoader;
}

interface RetainedAgentInternals {
	toolCallPolicy: ToolCallPolicy;
}

function assertValidAdmission(admission: WorkflowV2BoundRetainedAdmission): void {
	const nonEmpty = (value: string | undefined): value is string => typeof value === "string" && value.length > 0;
	if (!nonEmpty(admission.rlmChildId)) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "rlmChildId is required");
	}
	if (!nonEmpty(admission.sessionName)) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "sessionName is required");
	}
	if (!nonEmpty(admission.sessionDir)) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "sessionDir is required");
	}
	if (!nonEmpty(admission.cwd)) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "cwd is required");
	}
	if (!admission.model || !admission.model.provider || !admission.model.id) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "a resolved model is required");
	}
	if (!Number.isSafeInteger(admission.rlmDepth) || admission.rlmDepth < 0) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "rlmDepth must be a non-negative integer");
	}
	if (!admission.modelRegistry) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "modelRegistry is required");
	}
	if (!admission.settingsManager) {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "settingsManager is required");
	}
}

/**
 * Build the immutable tools-none attestation by reading back the constructed
 * runtime. Throws {@link WorkflowV2RetainedProfileError} if any invariant is
 * violated, so a widened runtime can never yield a passing attestation.
 */
export function attestWorkflowV2RetainedProfile(session: AgentSession): WorkflowV2RetainedProfileAttestation {
	const internals = session as unknown as RetainedSessionInternals;
	const agent = session.agent;
	const agentInternals = agent as unknown as RetainedAgentInternals;

	const tools = agent.state.tools;
	const toolsEmpty = Array.isArray(tools) && tools.length === 0;
	const toolsFrozen = Object.isFrozen(tools);
	const toolCallPolicy = agentInternals.toolCallPolicy;
	const activeToolNames = session.getActiveToolNames();
	const kernelProvisioned = internals._ipythonKernelProvisioner !== undefined;
	const kernelSnapshotAssigned = internals._ipythonKernelSnapshotDir !== undefined;
	const extensions = internals._resourceLoader.getExtensions();
	const extensionCount = extensions.extensions.length;
	const resourcesEmpty =
		internals._resourceLoader.getSkills().skills.length === 0 &&
		internals._resourceLoader.getPrompts().prompts.length === 0 &&
		internals._resourceLoader.getThemes().themes.length === 0 &&
		internals._resourceLoader.getAgentsFiles().agentsFiles.length === 0 &&
		internals._resourceLoader.getSystemPrompt() === undefined &&
		internals._resourceLoader.getAppendSystemPrompt().length === 0;
	const controllersInstalled =
		internals._agentMessageController !== undefined ||
		internals._agentObserveController !== undefined ||
		internals._rlmHeartbeatController !== undefined;
	const mcpInstalled = internals._mcpManager !== undefined;
	const goalsIncluded = internals._includeGoals === true;
	const compactSkillIncluded = internals._includeCompactSkill === true;
	const autonomousEnabled = internals._autonomousState?.enabled === true;
	const serializedRefine = internals._serializedRefine === true;
	const continuationDisabled = !goalsIncluded && !autonomousEnabled && !serializedRefine;
	const descendantsAllowed = session.rlmDepth < session.rlmMaxDepth;
	const parentHooksReused =
		agent.transformContext !== undefined || agent.onPayload !== undefined || agent.onResponse !== undefined;

	const attestation: WorkflowV2RetainedProfileAttestation = Object.freeze({
		profile: WORKFLOW_V2_TOOLS_NONE_PROFILE,
		tools: "none",
		maxTurns: WORKFLOW_V2_TOOLS_NONE_MAX_TURNS,
		toolsEmpty,
		toolsFrozen,
		toolCallPolicy,
		continuationDisabled,
		activeToolNames: Object.freeze([...activeToolNames]),
		kernelProvisioned,
		kernelSnapshotAssigned,
		extensionCount,
		resourcesEmpty,
		controllersInstalled,
		mcpInstalled,
		goalsIncluded,
		compactSkillIncluded,
		autonomousEnabled,
		serializedRefine,
		descendantsAllowed,
		rlmDepth: session.rlmDepth,
		rlmMaxDepth: session.rlmMaxDepth,
		parentHooksReused,
		effectiveModel: Object.freeze({ provider: agent.state.model.provider, id: agent.state.model.id }),
		effectiveThinkingLevel: agent.state.thinkingLevel,
	});

	assertAttestationSatisfiesProfile(attestation);
	return attestation;
}

/** Fail closed unless the attestation proves the full closed tools-none profile. */
function assertAttestationSatisfiesProfile(attestation: WorkflowV2RetainedProfileAttestation): void {
	const violations: string[] = [];
	if (!attestation.toolsEmpty) violations.push("low-level tools are not empty");
	if (!attestation.toolsFrozen) violations.push("low-level tools array is not frozen");
	if (attestation.toolCallPolicy !== "reject") violations.push("toolCallPolicy is not reject");
	if (!attestation.continuationDisabled)
		violations.push("a turn-continuation source (goals/autonomous/refine) is enabled");
	if (attestation.activeToolNames.length !== 0) violations.push("active tool registry is not empty");
	if (attestation.kernelProvisioned) violations.push("an ipython kernel provisioner was constructed");
	if (attestation.kernelSnapshotAssigned) violations.push("a kernel snapshot directory was assigned");
	if (attestation.extensionCount !== 0) violations.push("extensions are present");
	if (!attestation.resourcesEmpty) violations.push("resource loader is not empty");
	if (attestation.controllersInstalled) violations.push("a message/observe/heartbeat controller is installed");
	if (attestation.mcpInstalled) violations.push("an MCP manager is installed");
	if (attestation.goalsIncluded) violations.push("goals are enabled");
	if (attestation.compactSkillIncluded) violations.push("the compact skill is enabled");
	if (attestation.autonomousEnabled) violations.push("autonomous continuation is enabled");
	if (attestation.serializedRefine) violations.push("serialized refinement is enabled");
	if (attestation.descendantsAllowed) violations.push("descendants are allowed (rlmDepth < rlmMaxDepth)");
	if (attestation.parentHooksReused) violations.push("a parent context/payload/response hook was reused");
	if (violations.length > 0) {
		throw new WorkflowV2RetainedProfileError(
			"PROFILE_VIOLATION",
			`workflow-v2-tools-none-v1 attestation failed: ${violations.join("; ")}`,
		);
	}
}

/**
 * Package-private factory for the Workflow V2 Slice 3 retained tools-none child.
 * Both the inline and daemon retained-child hosts call this when the durable
 * admission profile is `workflow-v2-tools-none-v1`. It constructs restrictions;
 * it never creates a normal inherited child and narrows it afterward. See
 * docs/WORKFLOW-V2-SLICE3.md §6 and docs/reviews/v2-slice3-tools-none-redesign.md.
 */
export function createWorkflowV2RetainedChildRuntime(
	admission: WorkflowV2BoundRetainedAdmission,
	transport: WorkflowV2ParentTransportAuthority,
): WorkflowV2RetainedChildRuntime {
	assertValidAdmission(admission);
	if (typeof transport.streamFn !== "function") {
		throw new WorkflowV2RetainedProfileError("INVALID_ADMISSION", "a provider streamFn is required");
	}

	const childSessionManager = SessionManager.create(admission.cwd, admission.sessionDir);
	if (admission.parentSessionFile) {
		childSessionManager.newSession({
			parentSession: admission.parentSessionFile,
			rlmDepth: admission.rlmDepth,
		});
	}
	childSessionManager.appendModelChange(admission.model.provider, admission.model.id);
	childSessionManager.appendThinkingLevelChange(admission.thinkingLevel);
	childSessionManager.appendServiceTierChange(admission.serviceTier);

	// One frozen empty low-level tools array. `reject` cannot be widened after
	// construction, and `shouldStopAfterTurn` prevents any second provider turn.
	const frozenEmptyTools = Object.freeze([]) as unknown as [];
	const childAgent = new Agent({
		initialState: {
			systemPrompt: "",
			model: admission.model,
			thinkingLevel: admission.thinkingLevel,
			serviceTier: admission.serviceTier,
			tools: frozenEmptyTools,
		},
		// Only the credential-aware provider transport. No parent convertToLlm,
		// transformContext, onPayload, onResponse, toolExecution, or extension
		// stream wrapper.
		streamFn: transport.streamFn,
		getApiKey: transport.getApiKey,
		...(transport.transport ? { transport: transport.transport } : {}),
		// AgentSession installs its own turn hook that overrides this option, so
		// the one-turn boundary is enforced by `toolCallPolicy: "reject"`, the
		// disabled continuation sources below, and the executor's single-dispatch
		// guard. This default is kept as defense in depth if the child agent is
		// ever driven without a session turn hook.
		shouldStopAfterTurn: () => true,
		toolCallPolicy: "reject",
	});

	const child = new AgentSession({
		agent: childAgent,
		sessionManager: childSessionManager,
		settingsManager: admission.settingsManager,
		cwd: admission.cwd,
		agentDir: admission.agentDir,
		modelRegistry: admission.modelRegistry,
		// Isolated empty resources; never the parent's ResourceLoader.
		resourceLoader: new IsolatedEmptyResourceLoader(),
		// Empty base tools => _buildRuntime never constructs the kernel or built-ins.
		baseToolsOverride: Object.freeze({}),
		// No scoped models: exactly one resolved model, no cycling.
		scopedModels: [],
		customTools: [],
		initialActiveToolNames: [],
		allowedToolNames: [],
		includeGoals: false,
		includeCompactSkill: false,
		prewarmIpythonKernel: false,
		serializedRefine: false,
		// Depth equals max depth: descendants can never be created.
		rlmDepth: admission.rlmDepth,
		rlmMaxDepth: admission.rlmDepth,
		rlmSessionDir: admission.sessionDir,
		rlmParentNodeId: admission.rlmParentNodeId,
		rlmParentAgent: admission.parentAgentLabel,
		semanticParentSessionId: admission.parentSessionId,
		semanticSpawnedByRequestId: admission.spawnedByRequestId,
		sessionStartEvent: { type: "session_start", reason: "startup" } satisfies SessionStartEvent,
		// Controllers, MCP manager, autonomous config, initial goal, and
		// auto-refine reviewer are intentionally omitted.
	});

	if (child.sessionName !== admission.sessionName) {
		try {
			child.setSessionName(admission.sessionName);
		} catch (error) {
			child.dispose();
			throw error;
		}
	}

	// Session construction rebuilds the tool registry, which reassigns
	// agent.state.tools to a fresh empty array. Re-freeze it so the retained
	// child's low-level tools stay frozen and empty. The executor lane asserts
	// this again immediately before provider dispatch.
	Object.freeze(childAgent.state.tools);

	let attestation: WorkflowV2RetainedProfileAttestation;
	try {
		attestation = attestWorkflowV2RetainedProfile(child);
	} catch (error) {
		child.dispose();
		throw error;
	}

	admission.onSessionPublished?.(child);

	return { runtime: { session: child }, attestation };
}
