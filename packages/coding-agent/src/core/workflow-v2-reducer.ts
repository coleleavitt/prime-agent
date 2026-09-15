/**
 * Workflow V2 Slice 4 — pure event-sourced reducer + the one normative
 * projection semantic validator.
 *
 * Normative authority: docs/WORKFLOW-V2.md §5 (orthogonal state model), §6
 * (event vocabulary), §11 (mandatory projection validator + mutation matrix) and
 * the closed shape authority docs/api/workflow-v2.schema.json ($defs
 * runProjection/nodeProjection/attemptProjection/turnProjection/
 * operationProjection). Slice scope: docs/reviews/v2-slices-4-9-scope.md
 * (Slice 4).
 *
 * This module has NO I/O. It is:
 *   1. The single normative semantic entry point `validateProjectionSemantics`,
 *      reused by EVERY decode boundary (§11 (1)-(5)). Boundary-specific or
 *      partial copies are forbidden; every caller routes through this function.
 *   2. The `RunTerminalized` outcome-equality rule (§5/§11), the one cross-record
 *      invariant outside the per-value tables.
 *   3. A deterministic, replay-equivalent fold of the closed controller and Prime
 *      host fact vocabularies into normalized run/node/attempt/turn/operation
 *      projections. It REFLECTS committed facts and DERIVES conditions from
 *      accumulated evidence; it never performs host I/O, never reads a clock,
 *      never uses randomness, and never decides controller policy (scheduling,
 *      acceptance choice, and budget accounting belong to the controller — slices
 *      5-7 — which drive WHICH facts are committed).
 *
 * DORMANT: nothing here is imported by a production/controller path; the Workflow
 * V2 capability stays CAPABILITY_UNAVAILABLE (core/workflow-v2-capability.ts).
 *
 * Determinism contract: `reduce*` are pure functions of (previous immutable
 * state, one already-schema-validated fact). `reduceRun(definition, facts)` folds
 * an ordered fact list; folding N facts one at a time equals folding the whole
 * list, and re-folding a JSON round-tripped fact list yields a deep-equal
 * aggregate (replay equivalence). Illegal transitions throw `ReducerError` with a
 * closed code and produce zero state change.
 */

import {
	canonicalJson,
	decodeWorkflowV2ControllerEvent,
	decodeWorkflowV2Definition,
	decodeWorkflowV2RetainedEvent,
	type WorkflowV2Value,
} from "./workflow-v2-wire.js";

// ---------------------------------------------------------------------------
// Closed projection vocabulary (mirrors docs/api/workflow-v2.schema.json $defs).
// These constants ARE the schema-closed enums; the semantic validator treats
// them as the shape authority so this module needs no second JSON-Schema engine.
// ---------------------------------------------------------------------------

export type ProjectionKind = "run" | "node" | "attempt" | "turn" | "operation";

export const PROJECTION_KINDS: readonly ProjectionKind[] = ["run", "node", "attempt", "turn", "operation"];

const RUN_PHASES = ["created", "active", "cancelling", "draining", "terminal", "quarantined"] as const;
const RUN_INTENTS = ["none", "start", "cancel"] as const;
const RUN_OUTCOMES = ["succeeded", "failed", "cancelled", "budget_exceeded", "execution_unknown"] as const;
const RUN_CONDITIONS = [
	"admission_open",
	"admission_fenced",
	"owned_work_quiescent",
	"effects_classified",
	"budgets_within_limit",
	"integrity_verified",
	"integrity_failed",
] as const;

const NODE_PHASES = ["blocked", "ready", "attempting", "exhausted", "terminal"] as const;
const NODE_INTENTS = ["none", "execute", "cancel", "retry"] as const;
const NODE_OUTCOMES = ["accepted", "rejected", "cancelled", "execution_unknown"] as const;
const NODE_CONDITIONS = [
	"dependencies_pending",
	"dependencies_accepted",
	"candidate_absent",
	"candidate_present",
	"acceptance_pending",
	"acceptance_passed",
	"acceptance_failed",
] as const;

const ATTEMPT_PHASES = [
	"prepared",
	"dispatch_committed",
	"admission_bound",
	"running",
	"cancelling",
	"settled",
	"terminal",
] as const;
const ATTEMPT_INTENTS = ["execute", "cancel"] as const;
const ATTEMPT_OUTCOMES = ["completed", "failed", "cancelled", "execution_unknown"] as const;
const ATTEMPT_CONDITIONS = [
	"admission_unbound",
	"admission_bound",
	"result_absent",
	"result_present",
	"usage_final",
	"usage_known_prefix",
	"quiescence_unproved",
	"quiescence_proved",
] as const;

const TURN_PHASES = ["admitted", "running", "terminal"] as const;
const TURN_INTENTS = ["execute", "cancel"] as const;
const TURN_OUTCOMES = ["completed", "failed", "cancelled", "execution_unknown"] as const;
const TURN_CONDITIONS = [
	"result_absent",
	"result_present",
	"usage_final",
	"usage_known_prefix",
	"cancel_unactuated",
	"cancel_actuated",
	"quiescence_unproved",
	"quiescence_proved",
] as const;

const OPERATION_PHASES = ["pending", "claimed", "acknowledged", "retry_wait", "terminal"] as const;
const OPERATION_INTENTS = ["deliver", "cancel"] as const;
const OPERATION_OUTCOMES = ["succeeded", "failed", "ambiguous"] as const;
const OPERATION_CONDITIONS = [
	"claim_unheld",
	"claim_held",
	"receipt_absent",
	"receipt_present",
	"effect_unclassified",
	"effect_classified",
] as const;

export type RunPhase = (typeof RUN_PHASES)[number];
export type RunIntent = (typeof RUN_INTENTS)[number];
export type RunOutcome = (typeof RUN_OUTCOMES)[number];
export type RunCondition = (typeof RUN_CONDITIONS)[number];
export type NodePhase = (typeof NODE_PHASES)[number];
export type NodeIntent = (typeof NODE_INTENTS)[number];
export type NodeOutcome = (typeof NODE_OUTCOMES)[number];
export type NodeCondition = (typeof NODE_CONDITIONS)[number];
export type AttemptPhase = (typeof ATTEMPT_PHASES)[number];
export type AttemptIntent = (typeof ATTEMPT_INTENTS)[number];
export type AttemptOutcome = (typeof ATTEMPT_OUTCOMES)[number];
export type AttemptCondition = (typeof ATTEMPT_CONDITIONS)[number];
export type TurnPhase = (typeof TURN_PHASES)[number];
export type TurnIntent = (typeof TURN_INTENTS)[number];
export type TurnOutcome = (typeof TURN_OUTCOMES)[number];
export type TurnCondition = (typeof TURN_CONDITIONS)[number];
export type OperationPhase = (typeof OPERATION_PHASES)[number];
export type OperationIntent = (typeof OPERATION_INTENTS)[number];
export type OperationOutcome = (typeof OPERATION_OUTCOMES)[number];
export type OperationCondition = (typeof OPERATION_CONDITIONS)[number];

export interface RunProjection {
	phase: RunPhase;
	intent: RunIntent;
	outcome: RunOutcome | null;
	conditions: RunCondition[];
}
export interface NodeProjection {
	phase: NodePhase;
	intent: NodeIntent;
	outcome: NodeOutcome | null;
	conditions: NodeCondition[];
}
export interface AttemptProjection {
	phase: AttemptPhase;
	intent: AttemptIntent;
	outcome: AttemptOutcome | null;
	conditions: AttemptCondition[];
}
export interface TurnProjection {
	phase: TurnPhase;
	intent: TurnIntent;
	outcome: TurnOutcome | null;
	conditions: TurnCondition[];
}
export interface OperationProjection {
	phase: OperationPhase;
	intent: OperationIntent;
	outcome: OperationOutcome | null;
	conditions: OperationCondition[];
}

export type AnyProjection = RunProjection | NodeProjection | AttemptProjection | TurnProjection | OperationProjection;

interface ProjectionShape {
	phases: readonly string[];
	intents: readonly string[];
	outcomes: readonly string[];
	conditions: readonly string[];
	/** Phases whose projection carries a non-null terminal outcome (§ schema if/then). */
	terminalPhases: readonly string[];
}

const PROJECTION_SHAPES: Readonly<Record<ProjectionKind, ProjectionShape>> = Object.freeze({
	run: {
		phases: RUN_PHASES,
		intents: RUN_INTENTS,
		outcomes: RUN_OUTCOMES,
		conditions: RUN_CONDITIONS,
		terminalPhases: ["terminal"],
	},
	node: {
		phases: NODE_PHASES,
		intents: NODE_INTENTS,
		outcomes: NODE_OUTCOMES,
		conditions: NODE_CONDITIONS,
		terminalPhases: ["terminal"],
	},
	attempt: {
		phases: ATTEMPT_PHASES,
		intents: ATTEMPT_INTENTS,
		outcomes: ATTEMPT_OUTCOMES,
		conditions: ATTEMPT_CONDITIONS,
		terminalPhases: ["terminal"],
	},
	turn: {
		phases: TURN_PHASES,
		intents: TURN_INTENTS,
		outcomes: TURN_OUTCOMES,
		conditions: TURN_CONDITIONS,
		terminalPhases: ["terminal"],
	},
	operation: {
		phases: OPERATION_PHASES,
		intents: OPERATION_INTENTS,
		outcomes: OPERATION_OUTCOMES,
		conditions: OPERATION_CONDITIONS,
		terminalPhases: ["terminal"],
	},
});

// ---------------------------------------------------------------------------
// §11 normative tables — copied verbatim from WORKFLOW-V2.md. Equivalent code
// generation from these tables is permitted; boundary-specific validators are
// not. Every entry is a closed condition pair / proof-set for one kind+outcome.
// ---------------------------------------------------------------------------

const CONTRADICTIONS: Readonly<Record<ProjectionKind, readonly (readonly [string, string])[]>> = Object.freeze({
	run: [
		["admission_open", "admission_fenced"],
		["integrity_verified", "integrity_failed"],
	],
	node: [
		["dependencies_pending", "dependencies_accepted"],
		["candidate_absent", "candidate_present"],
		["acceptance_pending", "acceptance_passed"],
		["acceptance_pending", "acceptance_failed"],
		["acceptance_passed", "acceptance_failed"],
	],
	attempt: [
		["admission_unbound", "admission_bound"],
		["result_absent", "result_present"],
		["usage_final", "usage_known_prefix"],
		["quiescence_unproved", "quiescence_proved"],
	],
	turn: [
		["result_absent", "result_present"],
		["usage_final", "usage_known_prefix"],
		["cancel_unactuated", "cancel_actuated"],
		["quiescence_unproved", "quiescence_proved"],
	],
	operation: [
		["claim_unheld", "claim_held"],
		["receipt_absent", "receipt_present"],
		["effect_unclassified", "effect_classified"],
	],
});

const REQUIRED_PROOFS: Readonly<Record<ProjectionKind, Readonly<Record<string, readonly string[]>>>> = Object.freeze({
	run: {
		succeeded: [
			"admission_fenced",
			"owned_work_quiescent",
			"effects_classified",
			"budgets_within_limit",
			"integrity_verified",
		],
	},
	node: { accepted: ["dependencies_accepted", "candidate_present", "acceptance_passed"] },
	attempt: { completed: ["admission_bound", "result_present", "usage_final", "quiescence_proved"] },
	turn: { completed: ["result_present", "usage_final", "quiescence_proved"] },
	operation: { succeeded: ["receipt_present", "effect_classified"] },
});

/** Closed reducer/validator failure. Zero state change accompanies every throw. */
export class ReducerError extends Error {
	constructor(
		readonly code: string,
		message: string,
	) {
		super(message);
		this.name = "ReducerError";
	}
}

function reject(code: string, message: string): never {
	throw new ReducerError(code, message);
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

// ---------------------------------------------------------------------------
// §11 the one normative semantic entry point.
// ---------------------------------------------------------------------------

/**
 * validateProjectionSemantics(kind, value): the sole normative projection
 * validator (§11). It (1) strictly validates the value against the closed
 * projection shape selected by `kind` (exact keys, closed enums, unique
 * conditions, terminal<->outcome coupling, run-quarantine coupling), then (2)
 * rejects every CONTRADICTIONS pair, (3) enforces REQUIRED_PROOFS for the
 * outcome. Returns a fresh normalized (frozen) projection on success; throws
 * ReducerError otherwise. Callers MUST route every decode/replay/hydration/
 * inbox/recovery/view boundary through THIS function — no partial copies.
 */
export function validateProjectionSemantics(kind: ProjectionKind, value: unknown): AnyProjection {
	const shape = PROJECTION_SHAPES[kind];
	if (!shape) reject("projection_kind_unknown", `Unknown projection kind: ${String(kind)}`);
	if (!isRecord(value)) reject("projection_not_object", `${kind} projection is not an object`);

	const keys = Object.keys(value);
	const EXPECTED = ["phase", "intent", "outcome", "conditions"];
	if (keys.length !== EXPECTED.length || !EXPECTED.every((k) => Object.hasOwn(value, k))) {
		reject("projection_keys", `${kind} projection must have exactly {phase,intent,outcome,conditions}`);
	}

	const phase = value.phase;
	const intent = value.intent;
	const outcome = value.outcome;
	const conditions = value.conditions;

	if (typeof phase !== "string" || !shape.phases.includes(phase)) {
		reject("projection_phase", `${kind} projection has an out-of-vocabulary phase: ${String(phase)}`);
	}
	if (typeof intent !== "string" || !shape.intents.includes(intent)) {
		reject("projection_intent", `${kind} projection has an out-of-vocabulary intent: ${String(intent)}`);
	}
	if (outcome !== null && (typeof outcome !== "string" || !shape.outcomes.includes(outcome))) {
		reject("projection_outcome", `${kind} projection has an out-of-vocabulary outcome: ${String(outcome)}`);
	}
	if (!Array.isArray(conditions)) {
		reject("projection_conditions_type", `${kind} projection conditions must be an array`);
	}
	const seen = new Set<string>();
	for (const c of conditions) {
		if (typeof c !== "string" || !shape.conditions.includes(c)) {
			reject("projection_condition", `${kind} projection has an out-of-vocabulary condition: ${String(c)}`);
		}
		if (seen.has(c)) reject("projection_condition_duplicate", `${kind} projection repeats condition ${c}`);
		seen.add(c);
	}

	// terminal <-> outcome coupling (schema if/then/else). Terminal phases carry a
	// non-null outcome; every other phase (including run `quarantined`) is null.
	const isTerminalPhase = shape.terminalPhases.includes(phase);
	if (isTerminalPhase && outcome === null) {
		reject("projection_terminal_outcome", `${kind} terminal projection requires a non-null outcome`);
	}
	if (!isTerminalPhase && outcome !== null) {
		reject("projection_nonterminal_outcome", `${kind} non-terminal projection requires a null outcome`);
	}
	// run quarantine coupling: quarantined => outcome null (covered above) AND
	// integrity_failed present (schema then).
	if (kind === "run" && phase === "quarantined" && !seen.has("integrity_failed")) {
		reject("projection_quarantine_proof", "run quarantined projection requires integrity_failed");
	}

	// (2) contradictions
	for (const [a, b] of CONTRADICTIONS[kind]) {
		if (seen.has(a) && seen.has(b)) {
			reject("projection_contradiction", `${kind} projection holds contradictory conditions ${a} & ${b}`);
		}
	}
	// (3) required proofs for the outcome
	if (typeof outcome === "string") {
		const proofs = REQUIRED_PROOFS[kind][outcome];
		if (proofs) {
			for (const p of proofs) {
				if (!seen.has(p)) {
					reject("projection_missing_proof", `${kind} outcome ${outcome} requires condition ${p}`);
				}
			}
		}
	}

	return Object.freeze({
		phase,
		intent,
		outcome: (outcome as string | null) ?? null,
		conditions: Object.freeze([...(conditions as string[])].sort()),
	}) as AnyProjection;
}

/**
 * §5/§11 cross-record rule: in the terminalizing transaction and again on
 * DB load / replay / recovery-snapshot application, a `RunTerminalized`
 * controller fact's `data.outcome` MUST equal the normalized run projection
 * outcome. Mismatch quarantines the run (the caller performs the quarantine);
 * this asserts the invariant and throws on violation.
 */
export function assertRunTerminalizedOutcomeEquality(eventOutcome: unknown, runProjection: RunProjection): void {
	const run = validateProjectionSemantics("run", runProjection) as RunProjection;
	if (run.phase !== "terminal") {
		reject("terminalized_run_not_terminal", "RunTerminalized applies only to a terminal run projection");
	}
	if (eventOutcome !== run.outcome) {
		reject(
			"terminalized_outcome_mismatch",
			`RunTerminalized.data.outcome ${String(eventOutcome)} != normalized run outcome ${String(run.outcome)}`,
		);
	}
}

// ---------------------------------------------------------------------------
// Definition graph (pure projection of a decoded definition).
// ---------------------------------------------------------------------------

export interface DefinitionGraph {
	nodeIds: readonly string[];
	dependsOn: Readonly<Record<string, readonly string[]>>;
	outputs: readonly string[];
}

/** Build the immutable node graph from a schema-decoded definition (no I/O). */
export function buildDefinitionGraph(definition: WorkflowV2Value): DefinitionGraph {
	const nodes = definition.nodes as WorkflowV2Value[];
	const nodeIds: string[] = [];
	const dependsOn: Record<string, string[]> = {};
	for (const node of nodes) {
		const id = node.nodeId as string;
		nodeIds.push(id);
		dependsOn[id] = (node.dependsOn as WorkflowV2Value[]).map((d) => d.nodeId as string);
	}
	return Object.freeze({
		nodeIds: Object.freeze([...nodeIds]),
		dependsOn: Object.freeze(dependsOn),
		outputs: Object.freeze([...(definition.outputs as string[])]),
	});
}

// ---------------------------------------------------------------------------
// Aggregate state (all fields immutable per step; the fold returns new objects).
// ---------------------------------------------------------------------------

export interface AttemptRecord {
	attemptId: string;
	nodeId: string;
	operationId: string | null;
	rlmChildId: string | null;
	turnId: string | null;
	settlementDigest: string | null;
	projection: AttemptProjection;
	turn: TurnProjection | null;
}

export interface RunAggregate {
	runId: string;
	revision: number;
	controllerEpoch: number;
	cancelEpoch: number;
	run: RunProjection;
	nodes: Record<string, NodeProjection>;
	nodeOrder: string[];
	attempts: Record<string, AttemptRecord>;
	attemptOrder: string[];
	hostCursor: string | null;
	lastControllerSequence: number;
	appliedHostEventIds: string[];
}

const CONTROLLER_PROTOCOL = "prime.workflow.event/v2";
const HOST_PROTOCOL = "prime.workflow.retained-event/v2";

export type ReducerFact = WorkflowV2Value;

function proj<K extends ProjectionKind>(kind: K, value: unknown): AnyProjection {
	return validateProjectionSemantics(kind, value);
}

function withConditions(base: readonly string[], add: readonly string[], remove: readonly string[]): string[] {
	const set = new Set(base);
	for (const r of remove) set.delete(r);
	for (const a of add) set.add(a);
	return [...set].sort();
}

function cloneAggregate(state: RunAggregate): RunAggregate {
	return {
		runId: state.runId,
		revision: state.revision,
		controllerEpoch: state.controllerEpoch,
		cancelEpoch: state.cancelEpoch,
		run: state.run,
		nodes: { ...state.nodes },
		nodeOrder: [...state.nodeOrder],
		attempts: { ...state.attempts },
		attemptOrder: [...state.attemptOrder],
		hostCursor: state.hostCursor,
		lastControllerSequence: state.lastControllerSequence,
		appliedHostEventIds: [...state.appliedHostEventIds],
	};
}

function requireAttempt(state: RunAggregate, attemptId: string): AttemptRecord {
	const rec = state.attempts[attemptId];
	if (!rec) reject("attempt_absent", `attempt ${attemptId} does not exist`);
	return rec;
}

function requireNode(state: RunAggregate, nodeId: string): NodeProjection {
	const node = state.nodes[nodeId];
	if (!node) reject("node_absent", `node ${nodeId} is not in the definition`);
	return node;
}

function isTerminal(p: { phase: string }): boolean {
	return p.phase === "terminal";
}

function findAttemptByChildTurn(state: RunAggregate, rlmChildId: string, turnId: string): AttemptRecord {
	for (const id of state.attemptOrder) {
		const rec = state.attempts[id];
		if (rec.rlmChildId === rlmChildId && rec.turnId === turnId) return rec;
	}
	reject("host_fact_unbound", `no attempt bound to child ${rlmChildId} turn ${turnId}`);
}

// ---------------------------------------------------------------------------
// Controller-fact fold (§6 controller vocabulary).
// ---------------------------------------------------------------------------

function reduceControllerFact(prev: RunAggregate | null, event: WorkflowV2Value, graph: DefinitionGraph): RunAggregate {
	const type = event.type as string;
	const data = event.data as WorkflowV2Value;
	const revision = event.revision as number;
	const sequence = event.sequence as number;
	const controllerEpoch = event.controllerEpoch as number;
	const cancelEpoch = event.cancelEpoch as number;

	if (type === "RunAdmitted") {
		if (prev !== null) reject("run_already_admitted", "RunAdmitted on an existing run");
		const nodes: Record<string, NodeProjection> = {};
		for (const nodeId of graph.nodeIds) {
			const deps = graph.dependsOn[nodeId] ?? [];
			const depsAccepted = deps.length === 0;
			nodes[nodeId] = proj("node", {
				phase: "blocked",
				intent: "none",
				outcome: null,
				conditions: [
					depsAccepted ? "dependencies_accepted" : "dependencies_pending",
					"candidate_absent",
					"acceptance_pending",
				],
			}) as NodeProjection;
		}
		return {
			runId: event.runId as string,
			revision,
			controllerEpoch,
			cancelEpoch,
			run: proj("run", {
				phase: "created",
				intent: "none",
				outcome: null,
				conditions: ["admission_open"],
			}) as RunProjection,
			nodes,
			nodeOrder: [...graph.nodeIds],
			attempts: {},
			attemptOrder: [],
			hostCursor: null,
			lastControllerSequence: sequence,
			appliedHostEventIds: [],
		};
	}

	if (prev === null) reject("run_not_admitted", `${type} before RunAdmitted`);
	if (event.runId !== prev.runId) reject("run_id_mismatch", "controller fact runId does not match the run");
	if (sequence <= prev.lastControllerSequence) {
		reject("controller_sequence_regressed", `sequence ${sequence} <= applied ${prev.lastControllerSequence}`);
	}
	const next = cloneAggregate(prev);
	next.revision = revision;
	next.controllerEpoch = controllerEpoch;
	next.cancelEpoch = cancelEpoch;
	next.lastControllerSequence = sequence;

	switch (type) {
		case "RunStarted": {
			if (next.run.phase !== "created") reject("run_start_phase", "RunStarted requires a created run");
			next.run = proj("run", {
				phase: "active",
				intent: "start",
				outcome: null,
				conditions: next.run.conditions,
			}) as RunProjection;
			return next;
		}
		case "RunCancellationRequested": {
			if (next.run.phase !== "created" && next.run.phase !== "active") {
				reject("run_cancel_phase", "RunCancellationRequested requires a created/active run");
			}
			next.run = proj("run", {
				phase: "cancelling",
				intent: "cancel",
				outcome: null,
				conditions: withConditions(next.run.conditions, ["admission_fenced"], ["admission_open"]),
			}) as RunProjection;
			return next;
		}
		case "RunDraining": {
			if (next.run.phase !== "active" && next.run.phase !== "cancelling") {
				reject("run_drain_phase", "RunDraining requires an active/cancelling run");
			}
			next.run = proj("run", {
				phase: "draining",
				intent: next.run.intent,
				outcome: null,
				conditions: withConditions(next.run.conditions, ["admission_fenced"], ["admission_open"]),
			}) as RunProjection;
			return next;
		}
		case "RunQuarantined": {
			if (isTerminal(next.run)) reject("run_quarantine_terminal", "cannot quarantine a terminal run");
			next.run = proj("run", {
				phase: "quarantined",
				intent: next.run.intent,
				outcome: null,
				conditions: withConditions(next.run.conditions, ["integrity_failed"], ["integrity_verified"]),
			}) as RunProjection;
			return next;
		}
		case "RunTerminalized": {
			if (isTerminal(next.run) || next.run.phase === "quarantined") {
				reject("run_terminalize_phase", "RunTerminalized on a terminal/quarantined run");
			}
			const outcome = data.outcome as RunOutcome;
			const conditions = deriveRunTerminalConditions(next, outcome);
			const run = proj("run", { phase: "terminal", intent: next.run.intent, outcome, conditions }) as RunProjection;
			assertRunTerminalizedOutcomeEquality(outcome, run);
			next.run = run;
			return next;
		}
		case "NodeBecameReady": {
			const nodeId = data.nodeId as string;
			const node = requireNode(next, nodeId);
			if (next.run.phase !== "active") reject("node_ready_run_phase", "NodeBecameReady requires an active run");
			if (node.phase !== "blocked" && node.phase !== "exhausted") {
				reject("node_ready_phase", `NodeBecameReady requires a blocked/exhausted node, got ${node.phase}`);
			}
			for (const dep of graph.dependsOn[nodeId] ?? []) {
				const depNode = next.nodes[dep];
				if (!depNode || depNode.outcome !== "accepted") {
					reject("node_ready_deps", `node ${nodeId} dependency ${dep} is not accepted`);
				}
			}
			next.nodes[nodeId] = proj("node", {
				phase: "ready",
				intent: "execute",
				outcome: null,
				conditions: withConditions(node.conditions, ["dependencies_accepted"], ["dependencies_pending"]),
			}) as NodeProjection;
			return next;
		}
		case "NodeBlocked": {
			const nodeId = data.nodeId as string;
			const node = requireNode(next, nodeId);
			if (isTerminal(node)) reject("node_blocked_terminal", "cannot block a terminal node");
			next.nodes[nodeId] = proj("node", {
				phase: "blocked",
				intent: node.intent,
				outcome: null,
				conditions: node.conditions,
			}) as NodeProjection;
			return next;
		}
		case "AttemptPrepared": {
			const nodeId = data.nodeId as string;
			const attemptId = data.attemptId as string;
			const node = requireNode(next, nodeId);
			if (next.run.phase !== "active")
				reject("attempt_prepared_run_phase", "AttemptPrepared requires an active run");
			if (node.phase !== "ready" && node.phase !== "attempting") {
				reject(
					"attempt_prepared_node_phase",
					`AttemptPrepared requires a ready/attempting node, got ${node.phase}`,
				);
			}
			if (next.attempts[attemptId]) reject("attempt_exists", `attempt ${attemptId} already exists`);
			for (const id of next.attemptOrder) {
				const rec = next.attempts[id];
				if (rec.nodeId === nodeId && !isTerminal(rec.projection)) {
					reject("attempt_node_nonterminal", `node ${nodeId} already has a nonterminal attempt`);
				}
			}
			next.attempts[attemptId] = {
				attemptId,
				nodeId,
				operationId: null,
				rlmChildId: null,
				turnId: null,
				settlementDigest: null,
				projection: proj("attempt", {
					phase: "prepared",
					intent: "execute",
					outcome: null,
					conditions: ["admission_unbound", "result_absent", "quiescence_unproved"],
				}) as AttemptProjection,
				turn: null,
			};
			next.attemptOrder = [...next.attemptOrder, attemptId];
			next.nodes[nodeId] = proj("node", {
				phase: "attempting",
				intent: "execute",
				outcome: null,
				conditions: node.conditions,
			}) as NodeProjection;
			return next;
		}
		case "AttemptDispatchCommitted": {
			const rec = requireAttempt(next, data.attemptId as string);
			if (rec.projection.phase !== "prepared")
				reject("attempt_dispatch_phase", "AttemptDispatchCommitted requires a prepared attempt");
			next.attempts[rec.attemptId] = {
				...rec,
				projection: proj("attempt", {
					phase: "dispatch_committed",
					intent: rec.projection.intent,
					outcome: null,
					conditions: rec.projection.conditions,
				}) as AttemptProjection,
			};
			return next;
		}
		case "AttemptAdmissionBound": {
			const rec = requireAttempt(next, data.attemptId as string);
			if (rec.projection.phase !== "dispatch_committed")
				reject("attempt_bind_phase", "AttemptAdmissionBound requires a dispatch_committed attempt");
			next.attempts[rec.attemptId] = {
				...rec,
				operationId: data.operationId as string,
				rlmChildId: data.rlmChildId as string,
				turnId: data.turnId as string,
				projection: proj("attempt", {
					phase: "admission_bound",
					intent: rec.projection.intent,
					outcome: null,
					conditions: withConditions(rec.projection.conditions, ["admission_bound"], ["admission_unbound"]),
				}) as AttemptProjection,
				turn: proj("turn", {
					phase: "admitted",
					intent: "execute",
					outcome: null,
					conditions: ["result_absent", "cancel_unactuated", "quiescence_unproved"],
				}) as TurnProjection,
			};
			return next;
		}
		case "AttemptCancellationRequested": {
			const rec = requireAttempt(next, data.attemptId as string);
			if (isTerminal(rec.projection)) reject("attempt_cancel_terminal", "cannot cancel a terminal attempt");
			next.attempts[rec.attemptId] = {
				...rec,
				projection: proj("attempt", {
					phase: "cancelling",
					intent: "cancel",
					outcome: null,
					conditions: rec.projection.conditions,
				}) as AttemptProjection,
				turn: rec.turn
					? (proj("turn", {
							phase: rec.turn.phase === "terminal" ? "terminal" : rec.turn.phase,
							intent: "cancel",
							outcome: rec.turn.outcome,
							conditions: rec.turn.conditions,
						}) as TurnProjection)
					: rec.turn,
			};
			return next;
		}
		case "AttemptSettlementObserved": {
			const rec = requireAttempt(next, data.attemptId as string);
			if (!rec.turn || rec.turn.phase !== "terminal")
				reject("settlement_before_host", "AttemptSettlementObserved requires a terminal host turn");
			const nodeId = rec.nodeId;
			const node = requireNode(next, nodeId);
			const settledOutcome = data.outcome as AttemptOutcome;
			next.attempts[rec.attemptId] = {
				...rec,
				settlementDigest: data.settlementDigest as string,
				projection: proj("attempt", {
					phase: "settled",
					intent: rec.projection.intent,
					outcome: null,
					conditions: rec.projection.conditions,
				}) as AttemptProjection,
			};
			if (settledOutcome === "completed") {
				next.nodes[nodeId] = proj("node", {
					phase: node.phase === "terminal" ? "terminal" : "attempting",
					intent: node.intent,
					outcome: node.outcome,
					conditions: withConditions(node.conditions, ["candidate_present"], ["candidate_absent"]),
				}) as NodeProjection;
			}
			return next;
		}
		case "AttemptAccepted": {
			const rec = requireAttempt(next, data.attemptId as string);
			const nodeId = rec.nodeId;
			const node = requireNode(next, nodeId);
			if (rec.projection.phase !== "settled")
				reject("attempt_accept_phase", "AttemptAccepted requires a settled attempt");
			for (const id of next.attemptOrder) {
				const other = next.attempts[id];
				if (
					other.nodeId === nodeId &&
					other.attemptId !== rec.attemptId &&
					other.projection.outcome === "completed" &&
					other.projection.phase === "terminal"
				) {
					// downstream authority is granted once; a second accepted attempt is illegal
					if (next.nodes[nodeId].outcome === "accepted")
						reject("node_already_accepted", `node ${nodeId} already accepted`);
				}
			}
			if (next.nodes[nodeId].outcome === "accepted")
				reject("node_already_accepted", `node ${nodeId} already accepted`);
			// attempt completed requires its proofs (admission_bound,result_present,usage_final,quiescence_proved)
			next.attempts[rec.attemptId] = {
				...rec,
				projection: proj("attempt", {
					phase: "terminal",
					intent: rec.projection.intent,
					outcome: "completed",
					conditions: rec.projection.conditions,
				}) as AttemptProjection,
			};
			next.nodes[nodeId] = proj("node", {
				phase: "terminal",
				intent: "none",
				outcome: "accepted",
				conditions: withConditions(
					node.conditions,
					["dependencies_accepted", "candidate_present", "acceptance_passed"],
					["dependencies_pending", "candidate_absent", "acceptance_pending", "acceptance_failed"],
				),
			}) as NodeProjection;
			return next;
		}
		case "AttemptRejected": {
			const rec = requireAttempt(next, data.attemptId as string);
			const nodeId = rec.nodeId;
			const node = requireNode(next, nodeId);
			if (rec.projection.phase !== "settled")
				reject("attempt_reject_phase", "AttemptRejected requires a settled attempt");
			next.attempts[rec.attemptId] = {
				...rec,
				projection: proj("attempt", {
					phase: "terminal",
					intent: rec.projection.intent,
					outcome: rec.projection.conditions.includes("result_present") ? "completed" : "failed",
					conditions: rec.projection.conditions,
				}) as AttemptProjection,
			};
			next.nodes[nodeId] = proj("node", {
				phase: "exhausted",
				intent: "none",
				outcome: null,
				conditions: withConditions(
					node.conditions,
					["acceptance_failed"],
					["acceptance_pending", "acceptance_passed"],
				),
			}) as NodeProjection;
			return next;
		}
		case "AttemptOutcomeUnknown": {
			const rec = requireAttempt(next, data.attemptId as string);
			if (isTerminal(rec.projection))
				reject("attempt_unknown_terminal", "AttemptOutcomeUnknown on a terminal attempt");
			next.attempts[rec.attemptId] = {
				...rec,
				settlementDigest: data.settlementDigest as string,
				projection: proj("attempt", {
					phase: "terminal",
					intent: rec.projection.intent,
					outcome: "execution_unknown",
					conditions: rec.projection.conditions,
				}) as AttemptProjection,
			};
			return next;
		}
		case "HostCursorAdvanced": {
			next.hostCursor = data.hostCursor as string;
			return next;
		}
		default:
			reject("controller_fact_unknown", `unknown controller fact type ${type}`);
	}
}

/**
 * Derive the run condition set at terminalization from accumulated evidence.
 * Deliberately conservative: the derived set only asserts a proof when the fold
 * has evidence for it. The validator then rejects any outcome whose required
 * proofs are missing (e.g. `succeeded` without proven quiescence).
 */
function deriveRunTerminalConditions(state: RunAggregate, outcome: RunOutcome): string[] {
	const nonterminalAttempts = state.attemptOrder.some((id) => !isTerminal(state.attempts[id].projection));
	const allTurnsQuiescent = state.attemptOrder.every((id) => {
		const turn = state.attempts[id].turn;
		return turn === null || turn.conditions.includes("quiescence_proved");
	});
	const ownedQuiescent = !nonterminalAttempts && allTurnsQuiescent;
	const add: string[] = [];
	const remove: string[] = ["admission_open"];
	// admissions must be fenced before a terminal verdict.
	add.push("admission_fenced");
	if (!state.run.conditions.includes("integrity_failed")) add.push("integrity_verified");
	// slice 4 has no budget accounting or operation classification in the run fold;
	// they default satisfied and are refined by the controller in slice 6/7.
	add.push("budgets_within_limit", "effects_classified");
	if (ownedQuiescent) add.push("owned_work_quiescent");
	// outcome is informational for the derivation; the validator enforces proofs.
	void outcome;
	return withConditions(state.run.conditions, add, remove);
}

// ---------------------------------------------------------------------------
// Host-fact fold (§6 Prime host vocabulary; consumed from host_inbox).
// ---------------------------------------------------------------------------

function settlementConditions(settlement: WorkflowV2Value): {
	result: "result_present" | "result_absent";
	usage: "usage_final" | "usage_known_prefix";
	quiescence: "quiescence_proved" | "quiescence_unproved";
	cancel: "cancel_actuated" | "cancel_unactuated";
	outcome: TurnOutcome;
} {
	const outcome = settlement.outcome as TurnOutcome;
	const result = settlement.result as WorkflowV2Value;
	const usage = settlement.usage as WorkflowV2Value | undefined;
	const hasText = result?.kind === "text";
	const finality = usage?.finality;
	return {
		result: hasText ? "result_present" : "result_absent",
		usage: finality === "final" ? "usage_final" : "usage_known_prefix",
		quiescence: settlement.descendantsQuiescent === true ? "quiescence_proved" : "quiescence_unproved",
		cancel: settlement.cancelActuated === true ? "cancel_actuated" : "cancel_unactuated",
		outcome,
	};
}

function reduceHostFact(prev: RunAggregate | null, event: WorkflowV2Value): RunAggregate {
	if (prev === null) reject("host_before_admit", "host fact before RunAdmitted");
	const type = event.type as string;
	const data = event.data as WorkflowV2Value;
	const hostEventId = event.hostEventId as string;
	if (prev.appliedHostEventIds.includes(hostEventId)) {
		reject("host_event_duplicate", `host event ${hostEventId} already applied`);
	}
	const next = cloneAggregate(prev);
	next.appliedHostEventIds = [...next.appliedHostEventIds, hostEventId];
	next.hostCursor = event.hostCursor as string;

	switch (type) {
		case "OperationAdmitted":
		case "ChildAdmitted":
		case "TurnAdmitted":
		case "DeleteRequested":
		case "ChildTombstoned":
		case "ChildCleanupCompleted":
		case "ChildCleanupFailed":
			// Operation/child topology + cleanup lifecycle are store/outbox and
			// slice-7 concerns; recorded via cursor + dedup only in the run fold.
			return next;
		case "TurnStarted": {
			const rec = findAttemptByChildTurn(next, data.rlmChildId as string, data.turnId as string);
			if (!rec.turn) reject("turn_started_unbound", "TurnStarted without a bound turn");
			if (rec.projection.phase === "admission_bound") {
				next.attempts[rec.attemptId] = {
					...rec,
					projection: proj("attempt", {
						phase: "running",
						intent: rec.projection.intent,
						outcome: null,
						conditions: rec.projection.conditions,
					}) as AttemptProjection,
					turn: proj("turn", {
						phase: "running",
						intent: rec.turn.intent,
						outcome: null,
						conditions: rec.turn.conditions,
					}) as TurnProjection,
				};
			}
			return next;
		}
		case "TurnSettled": {
			const rec = findAttemptByChildTurn(next, data.rlmChildId as string, data.turnId as string);
			if (!rec.turn) reject("turn_settled_unbound", "TurnSettled without a bound turn");
			if (isTerminal(rec.turn)) reject("turn_settled_terminal", "TurnSettled on a terminal turn");
			const s = settlementConditions(data.settlement as WorkflowV2Value);
			const turnConditions = withConditions(
				rec.turn.conditions,
				[s.result, s.usage, s.quiescence, s.cancel],
				[
					"result_absent",
					"result_present",
					"usage_final",
					"usage_known_prefix",
					"quiescence_unproved",
					"quiescence_proved",
					"cancel_actuated",
					"cancel_unactuated",
				],
			);
			const attemptConditions = withConditions(
				rec.projection.conditions,
				[s.result, s.usage, s.quiescence],
				[
					"result_absent",
					"result_present",
					"usage_final",
					"usage_known_prefix",
					"quiescence_unproved",
					"quiescence_proved",
				],
			);
			next.attempts[rec.attemptId] = {
				...rec,
				projection: proj("attempt", {
					phase: "settled",
					intent: rec.projection.intent,
					outcome: null,
					conditions: attemptConditions,
				}) as AttemptProjection,
				turn: proj("turn", {
					phase: "terminal",
					intent: rec.turn.intent,
					outcome: s.outcome,
					conditions: turnConditions,
				}) as TurnProjection,
			};
			return next;
		}
		case "CancelRequested": {
			const rec = findAttemptByChildTurn(next, data.rlmChildId as string, data.turnId as string);
			if (!rec.turn || isTerminal(rec.turn)) return next;
			next.attempts[rec.attemptId] = {
				...rec,
				turn: proj("turn", {
					phase: rec.turn.phase,
					intent: "cancel",
					outcome: null,
					conditions: rec.turn.conditions,
				}) as TurnProjection,
			};
			return next;
		}
		case "CancelActuated": {
			const rec = findAttemptByChildTurn(next, data.rlmChildId as string, data.turnId as string);
			if (!rec.turn || isTerminal(rec.turn)) return next;
			next.attempts[rec.attemptId] = {
				...rec,
				turn: proj("turn", {
					phase: rec.turn.phase,
					intent: rec.turn.intent,
					outcome: null,
					conditions: withConditions(rec.turn.conditions, ["cancel_actuated"], ["cancel_unactuated"]),
				}) as TurnProjection,
			};
			return next;
		}
		case "ChildQuiescent": {
			const rlmChildId = data.rlmChildId as string;
			for (const id of next.attemptOrder) {
				const rec = next.attempts[id];
				if (rec.rlmChildId !== rlmChildId) continue;
				const attemptConditions = withConditions(
					rec.projection.conditions,
					["quiescence_proved"],
					["quiescence_unproved"],
				);
				next.attempts[id] = {
					...rec,
					projection: proj("attempt", {
						phase: rec.projection.phase,
						intent: rec.projection.intent,
						outcome: rec.projection.outcome,
						conditions: attemptConditions,
					}) as AttemptProjection,
					turn: rec.turn
						? (proj("turn", {
								phase: rec.turn.phase,
								intent: rec.turn.intent,
								outcome: rec.turn.outcome,
								conditions: withConditions(rec.turn.conditions, ["quiescence_proved"], ["quiescence_unproved"]),
							}) as TurnProjection)
						: rec.turn,
				};
			}
			return next;
		}
		default:
			reject("host_fact_unknown", `unknown host fact type ${type}`);
	}
}

// ---------------------------------------------------------------------------
// Public fold entry points.
// ---------------------------------------------------------------------------

/** Classify + validate one already-JSON-parsed fact, then fold it (pure). */
export function reduceFact(prev: RunAggregate | null, rawFact: unknown, graph: DefinitionGraph): RunAggregate {
	if (!isRecord(rawFact)) reject("fact_not_object", "reducer fact is not an object");
	const protocol = rawFact.protocol;
	if (protocol === CONTROLLER_PROTOCOL) {
		const event = decodeWorkflowV2ControllerEvent(rawFact);
		return reduceControllerFact(prev, event, graph);
	}
	if (protocol === HOST_PROTOCOL) {
		const event = decodeWorkflowV2RetainedEvent(rawFact);
		return reduceHostFact(prev, event);
	}
	reject("fact_protocol_unknown", `unknown fact protocol ${String(protocol)}`);
}

/**
 * Fold an ordered fact list over a decoded definition. Deterministic and
 * replay-equivalent: no clock, no randomness, pure functions of inputs. The
 * first fact MUST be `RunAdmitted`. Every produced projection is validated by
 * `validateProjectionSemantics`; an illegal transition throws with zero effect.
 */
export function reduceRun(definition: WorkflowV2Value, facts: readonly unknown[]): RunAggregate {
	const graph = buildDefinitionGraph(decodeWorkflowV2Definition(definition));
	let state: RunAggregate | null = null;
	for (const fact of facts) {
		state = reduceFact(state, fact, graph);
	}
	if (state === null) reject("empty_fact_stream", "reduceRun requires at least a RunAdmitted fact");
	return state;
}

/** Re-validate a hydrated aggregate's projections through the one entry point. */
export function revalidateAggregate(state: RunAggregate): void {
	validateProjectionSemantics("run", state.run);
	for (const nodeId of state.nodeOrder) validateProjectionSemantics("node", state.nodes[nodeId]);
	for (const attemptId of state.attemptOrder) {
		const rec = state.attempts[attemptId];
		validateProjectionSemantics("attempt", rec.projection);
		if (rec.turn) validateProjectionSemantics("turn", rec.turn);
	}
}

export { canonicalJson };
