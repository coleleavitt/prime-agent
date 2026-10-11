//! The pure Workflow V2 reducer (`WORKFLOW-V2.md` §5, §6, §11; TS
//! `workflow-v2-reducer.ts`, slice 4).
//!
//! A deterministic fold of the closed controller (`prime.workflow.event/v2`)
//! and Prime host (`prime.workflow.retained-event/v2`) fact vocabularies
//! into the normalized run, node, attempt, and turn projections. It
//! reflects committed facts and derives conditions from accumulated
//! evidence; it performs no I/O, reads no clock, and decides no controller
//! policy (scheduling, acceptance choice, and budget accounting are the
//! controller's, slices 5-7, which choose *which* facts are committed).
//! Every projection it produces goes through the one normative validator
//! ([`validate_projection_semantics`]); an illegal transition is a
//! [`ReducerError`] with zero state change (the fold works on a copy).

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

use super::json;
use super::projection::{
    Projection,
    ProjectionKind,
    assert_run_terminalized_outcome_equality,
    validate_projection_semantics,
};
use super::wire::{self, Def, WireError};

/// The controller-fact protocol.
pub const CONTROLLER_PROTOCOL: &str = "prime.workflow.event/v2";
/// The Prime host-fact protocol.
pub const HOST_PROTOCOL: &str = "prime.workflow.retained-event/v2";

/// Why the fold refused a fact: the TS reducer's closed code (`code()`
/// spells it exactly) and a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {detail}", .code.as_str())]
pub struct ReducerError {
    pub code: ReducerCode,
    pub detail: String,
}

/// The closed reducer rejection codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReducerCode {
    /// A produced projection failed the one normative validator.
    Projection,
    /// A fact failed its strict wire decode.
    FactInvalid,
    FactNotObject,
    FactProtocolUnknown,
    EmptyFactStream,
    RunAlreadyAdmitted,
    RunNotAdmitted,
    RunIdMismatch,
    ControllerSequenceRegressed,
    RunStartPhase,
    RunCancelPhase,
    RunDrainPhase,
    RunQuarantineTerminal,
    RunTerminalizePhase,
    TerminalizedOutcomeMismatch,
    NodeAbsent,
    NodeReadyRunPhase,
    NodeReadyPhase,
    NodeReadyDeps,
    NodeBlockedTerminal,
    NodeAlreadyAccepted,
    AttemptAbsent,
    AttemptPreparedRunPhase,
    AttemptPreparedNodePhase,
    AttemptExists,
    AttemptNodeNonterminal,
    AttemptDispatchPhase,
    AttemptBindPhase,
    AttemptCancelTerminal,
    SettlementBeforeHost,
    AttemptAcceptPhase,
    AttemptRejectPhase,
    AttemptUnknownTerminal,
    ControllerFactUnknown,
    HostBeforeAdmit,
    HostEventDuplicate,
    HostFactUnbound,
    TurnStartedUnbound,
    TurnSettledUnbound,
    TurnSettledTerminal,
    HostFactUnknown,
}

impl ReducerCode {
    /// The TS code string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReducerCode::Projection => "projection_invalid",
            ReducerCode::FactInvalid => "fact_invalid",
            ReducerCode::FactNotObject => "fact_not_object",
            ReducerCode::FactProtocolUnknown => "fact_protocol_unknown",
            ReducerCode::EmptyFactStream => "empty_fact_stream",
            ReducerCode::RunAlreadyAdmitted => "run_already_admitted",
            ReducerCode::RunNotAdmitted => "run_not_admitted",
            ReducerCode::RunIdMismatch => "run_id_mismatch",
            ReducerCode::ControllerSequenceRegressed => "controller_sequence_regressed",
            ReducerCode::RunStartPhase => "run_start_phase",
            ReducerCode::RunCancelPhase => "run_cancel_phase",
            ReducerCode::RunDrainPhase => "run_drain_phase",
            ReducerCode::RunQuarantineTerminal => "run_quarantine_terminal",
            ReducerCode::RunTerminalizePhase => "run_terminalize_phase",
            ReducerCode::TerminalizedOutcomeMismatch => "terminalized_outcome_mismatch",
            ReducerCode::NodeAbsent => "node_absent",
            ReducerCode::NodeReadyRunPhase => "node_ready_run_phase",
            ReducerCode::NodeReadyPhase => "node_ready_phase",
            ReducerCode::NodeReadyDeps => "node_ready_deps",
            ReducerCode::NodeBlockedTerminal => "node_blocked_terminal",
            ReducerCode::NodeAlreadyAccepted => "node_already_accepted",
            ReducerCode::AttemptAbsent => "attempt_absent",
            ReducerCode::AttemptPreparedRunPhase => "attempt_prepared_run_phase",
            ReducerCode::AttemptPreparedNodePhase => "attempt_prepared_node_phase",
            ReducerCode::AttemptExists => "attempt_exists",
            ReducerCode::AttemptNodeNonterminal => "attempt_node_nonterminal",
            ReducerCode::AttemptDispatchPhase => "attempt_dispatch_phase",
            ReducerCode::AttemptBindPhase => "attempt_bind_phase",
            ReducerCode::AttemptCancelTerminal => "attempt_cancel_terminal",
            ReducerCode::SettlementBeforeHost => "settlement_before_host",
            ReducerCode::AttemptAcceptPhase => "attempt_accept_phase",
            ReducerCode::AttemptRejectPhase => "attempt_reject_phase",
            ReducerCode::AttemptUnknownTerminal => "attempt_unknown_terminal",
            ReducerCode::ControllerFactUnknown => "controller_fact_unknown",
            ReducerCode::HostBeforeAdmit => "host_before_admit",
            ReducerCode::HostEventDuplicate => "host_event_duplicate",
            ReducerCode::HostFactUnbound => "host_fact_unbound",
            ReducerCode::TurnStartedUnbound => "turn_started_unbound",
            ReducerCode::TurnSettledUnbound => "turn_settled_unbound",
            ReducerCode::TurnSettledTerminal => "turn_settled_terminal",
            ReducerCode::HostFactUnknown => "host_fact_unknown",
        }
    }
}

fn reject<T>(code: ReducerCode, detail: impl Into<String>) -> Result<T, ReducerError> {
    Err(ReducerError {
        code,
        detail: detail.into(),
    })
}

fn projection_error(kind: ProjectionKind, error: &WireError) -> ReducerError {
    ReducerError {
        code: ReducerCode::Projection,
        detail: format!("{kind:?} projection {error}"),
    }
}

/// Build one projection through the normative validator.
///
/// # Errors
///
/// The validator's verdict.
pub fn projection(
    kind: ProjectionKind,
    phase: &str,
    intent: &str,
    outcome: Option<&str>,
    conditions: &[String],
) -> Result<Projection, ReducerError> {
    let value = json!({
        "phase": phase,
        "intent": intent,
        "outcome": outcome,
        "conditions": conditions,
    });
    validate_projection_semantics(kind, &value).map_err(|error| projection_error(kind, &error))
}

/// `withConditions`: the set `base - remove + add`, sorted.
fn with_conditions(base: &[String], add: &[&str], remove: &[&str]) -> Vec<String> {
    let mut set: BTreeSet<String> = base.iter().cloned().collect();
    for condition in remove {
        set.remove(*condition);
    }
    for condition in add {
        set.insert((*condition).to_string());
    }
    set.into_iter().collect()
}

fn owned(conditions: &[&str]) -> Vec<String> {
    conditions
        .iter()
        .map(|condition| (*condition).to_string())
        .collect()
}

/// The immutable node graph of a decoded definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionGraph {
    pub node_ids: Vec<String>,
    pub depends_on: HashMap<String, Vec<String>>,
    pub outputs: Vec<String>,
}

impl DefinitionGraph {
    /// The graph of a decoded definition.
    #[must_use]
    pub fn of(definition: &wire::Definition) -> DefinitionGraph {
        DefinitionGraph {
            node_ids: definition
                .nodes
                .iter()
                .map(|node| node.node_id.clone())
                .collect(),
            depends_on: definition
                .nodes
                .iter()
                .map(|node| {
                    (
                        node.node_id.clone(),
                        node.depends_on
                            .iter()
                            .map(|dependency| dependency.node_id.clone())
                            .collect(),
                    )
                })
                .collect(),
            outputs: definition.outputs.clone(),
        }
    }

    fn dependencies(&self, node_id: &str) -> &[String] {
        self.depends_on.get(node_id).map_or(&[], Vec::as_slice)
    }
}

/// One node's normalized projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    pub node_id: String,
    pub projection: Projection,
}

/// One attempt's normalized projection and its bindings (the embedded
/// Prime turn projection once admission binds it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub attempt_id: String,
    pub node_id: String,
    pub operation_id: Option<String>,
    pub rlm_child_id: Option<String>,
    pub turn_id: Option<String>,
    pub settlement_digest: Option<String>,
    pub projection: Projection,
    pub turn: Option<Projection>,
}

/// One run's aggregate: the run projection, its nodes in definition order,
/// its attempts in creation order, the fences, and the cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAggregate {
    pub run_id: String,
    pub revision: u64,
    pub controller_epoch: u64,
    pub cancel_epoch: u64,
    pub run: Projection,
    pub nodes: Vec<NodeState>,
    pub attempts: Vec<AttemptRecord>,
    pub host_cursor: Option<String>,
    pub last_controller_sequence: u64,
    /// Host events folded by this in-memory aggregate (the store dedups
    /// durably through `host_inbox`; a hydrated aggregate starts empty).
    pub applied_host_event_ids: Vec<String>,
}

impl RunAggregate {
    /// The node's projection.
    #[must_use]
    pub fn node(&self, node_id: &str) -> Option<&Projection> {
        self.nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .map(|node| &node.projection)
    }

    /// The attempt's record.
    #[must_use]
    pub fn attempt(&self, attempt_id: &str) -> Option<&AttemptRecord> {
        self.attempts
            .iter()
            .find(|attempt| attempt.attempt_id == attempt_id)
    }

    fn node_index(&self, node_id: &str) -> Result<usize, ReducerError> {
        match self.nodes.iter().position(|node| node.node_id == node_id) {
            Some(index) => Ok(index),
            None => reject(
                ReducerCode::NodeAbsent,
                format!("node {node_id} is not in the definition"),
            ),
        }
    }

    fn attempt_index(&self, attempt_id: &str) -> Result<usize, ReducerError> {
        match self
            .attempts
            .iter()
            .position(|attempt| attempt.attempt_id == attempt_id)
        {
            Some(index) => Ok(index),
            None => reject(
                ReducerCode::AttemptAbsent,
                format!("attempt {attempt_id} does not exist"),
            ),
        }
    }

    fn attempt_by_child_turn(
        &self,
        rlm_child_id: &str,
        turn_id: &str,
    ) -> Result<usize, ReducerError> {
        match self.attempts.iter().position(|attempt| {
            attempt.rlm_child_id.as_deref() == Some(rlm_child_id)
                && attempt.turn_id.as_deref() == Some(turn_id)
        }) {
            Some(index) => Ok(index),
            None => reject(
                ReducerCode::HostFactUnbound,
                format!("no attempt bound to child {rlm_child_id} turn {turn_id}"),
            ),
        }
    }
}

fn is_terminal(projection: &Projection) -> bool {
    projection.phase == "terminal"
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn decode_fact(value: &Value, def: Def) -> Result<(), ReducerError> {
    json::check_bounds(value)
        .and_then(|()| wire::decode_as(value, def))
        .map_err(|error| ReducerError {
            code: ReducerCode::FactInvalid,
            detail: error.to_string(),
        })
}

/// Classify, strictly decode, and fold one fact.
///
/// # Errors
///
/// A fact outside the closed vocabulary or an illegal transition; the
/// previous state is untouched.
pub fn reduce_fact(
    prev: Option<&RunAggregate>,
    fact: &Value,
    graph: &DefinitionGraph,
) -> Result<RunAggregate, ReducerError> {
    if !fact.is_object() {
        return reject(ReducerCode::FactNotObject, "reducer fact is not an object");
    }
    match fact.get("protocol").and_then(Value::as_str) {
        Some(CONTROLLER_PROTOCOL) => {
            decode_fact(fact, Def::ControllerEvent)?;
            reduce_controller_fact(prev, fact, graph)
        }
        Some(HOST_PROTOCOL) => {
            decode_fact(fact, Def::RetainedEvent)?;
            reduce_host_fact(prev, fact)
        }
        other => reject(
            ReducerCode::FactProtocolUnknown,
            format!("unknown fact protocol {other:?}"),
        ),
    }
}

/// Fold an ordered fact list over a definition; the first fact must be
/// `RunAdmitted`. Deterministic and replay-equivalent.
///
/// # Errors
///
/// An invalid definition (as [`ReducerCode::FactInvalid`]), an empty
/// stream, or the first refused fact.
pub fn reduce_run(definition: &Value, facts: &[Value]) -> Result<RunAggregate, ReducerError> {
    let definition = wire::decode_definition(definition).map_err(|error| ReducerError {
        code: ReducerCode::FactInvalid,
        detail: error.to_string(),
    })?;
    let graph = DefinitionGraph::of(&definition);
    let mut state: Option<RunAggregate> = None;
    for fact in facts {
        state = Some(reduce_fact(state.as_ref(), fact, &graph)?);
    }
    match state {
        Some(state) => Ok(state),
        None => reject(
            ReducerCode::EmptyFactStream,
            "reduceRun requires at least a RunAdmitted fact",
        ),
    }
}

/// Re-validate every projection of a hydrated aggregate through the one
/// entry point.
///
/// # Errors
///
/// The first projection the validator refuses.
pub fn revalidate_aggregate(state: &RunAggregate) -> Result<(), ReducerError> {
    let check = |kind: ProjectionKind, projection: &Projection| {
        let value = serde_json::to_value(projection).unwrap_or(Value::Null);
        validate_projection_semantics(kind, &value)
            .map(drop)
            .map_err(|error| projection_error(kind, &error))
    };
    check(ProjectionKind::Run, &state.run)?;
    for node in &state.nodes {
        check(ProjectionKind::Node, &node.projection)?;
    }
    for attempt in &state.attempts {
        check(ProjectionKind::Attempt, &attempt.projection)?;
        if let Some(turn) = &attempt.turn {
            check(ProjectionKind::Turn, turn)?;
        }
    }
    Ok(())
}

/// The `RunTerminalized` cross-record rule over a typed run projection.
///
/// # Errors
///
/// [`ReducerCode::TerminalizedOutcomeMismatch`] on disagreement.
pub fn check_terminalized_outcome(
    event_outcome: Option<&str>,
    run: &Projection,
) -> Result<(), ReducerError> {
    let run = serde_json::to_value(run).unwrap_or(Value::Null);
    let event_outcome =
        event_outcome.map_or(Value::Null, |outcome| Value::String(outcome.to_string()));
    assert_run_terminalized_outcome_equality(&event_outcome, &run).map_err(|error| ReducerError {
        code: ReducerCode::TerminalizedOutcomeMismatch,
        detail: error.to_string(),
    })
}

fn admit_run(event: &Value, graph: &DefinitionGraph) -> Result<RunAggregate, ReducerError> {
    let mut nodes = Vec::with_capacity(graph.node_ids.len());
    for node_id in &graph.node_ids {
        let dependencies = if graph.dependencies(node_id).is_empty() {
            "dependencies_accepted"
        } else {
            "dependencies_pending"
        };
        nodes.push(NodeState {
            node_id: node_id.clone(),
            projection: projection(
                ProjectionKind::Node,
                "blocked",
                "none",
                None,
                &owned(&[dependencies, "candidate_absent", "acceptance_pending"]),
            )?,
        });
    }
    Ok(RunAggregate {
        run_id: text(event, "runId").to_string(),
        revision: number(event, "revision"),
        controller_epoch: number(event, "controllerEpoch"),
        cancel_epoch: number(event, "cancelEpoch"),
        run: projection(
            ProjectionKind::Run,
            "created",
            "none",
            None,
            &owned(&["admission_open"]),
        )?,
        nodes,
        attempts: Vec::new(),
        host_cursor: None,
        last_controller_sequence: number(event, "sequence"),
        applied_host_event_ids: Vec::new(),
    })
}

#[allow(clippy::too_many_lines)] // one arm per closed controller fact type
fn reduce_controller_fact(
    prev: Option<&RunAggregate>,
    event: &Value,
    graph: &DefinitionGraph,
) -> Result<RunAggregate, ReducerError> {
    let kind = text(event, "type");
    let data = &event["data"];
    if kind == "RunAdmitted" {
        if prev.is_some() {
            return reject(
                ReducerCode::RunAlreadyAdmitted,
                "RunAdmitted on an existing run",
            );
        }
        return admit_run(event, graph);
    }
    let Some(prev) = prev else {
        return reject(
            ReducerCode::RunNotAdmitted,
            format!("{kind} before RunAdmitted"),
        );
    };
    if text(event, "runId") != prev.run_id {
        return reject(
            ReducerCode::RunIdMismatch,
            "controller fact runId does not match the run",
        );
    }
    let sequence = number(event, "sequence");
    if sequence <= prev.last_controller_sequence {
        return reject(
            ReducerCode::ControllerSequenceRegressed,
            format!(
                "sequence {sequence} <= applied {}",
                prev.last_controller_sequence
            ),
        );
    }
    let mut next = prev.clone();
    next.revision = number(event, "revision");
    next.controller_epoch = number(event, "controllerEpoch");
    next.cancel_epoch = number(event, "cancelEpoch");
    next.last_controller_sequence = sequence;

    match kind {
        "RunStarted" => {
            if next.run.phase != "created" {
                return reject(
                    ReducerCode::RunStartPhase,
                    "RunStarted requires a created run",
                );
            }
            next.run = projection(
                ProjectionKind::Run,
                "active",
                "start",
                None,
                &next.run.conditions,
            )?;
        }
        "RunCancellationRequested" => {
            if next.run.phase != "created" && next.run.phase != "active" {
                return reject(
                    ReducerCode::RunCancelPhase,
                    "RunCancellationRequested requires a created/active run",
                );
            }
            let conditions = with_conditions(
                &next.run.conditions,
                &["admission_fenced"],
                &["admission_open"],
            );
            next.run = projection(
                ProjectionKind::Run,
                "cancelling",
                "cancel",
                None,
                &conditions,
            )?;
        }
        "RunDraining" => {
            if next.run.phase != "active" && next.run.phase != "cancelling" {
                return reject(
                    ReducerCode::RunDrainPhase,
                    "RunDraining requires an active/cancelling run",
                );
            }
            let conditions = with_conditions(
                &next.run.conditions,
                &["admission_fenced"],
                &["admission_open"],
            );
            next.run = projection(
                ProjectionKind::Run,
                "draining",
                &next.run.intent,
                None,
                &conditions,
            )?;
        }
        "RunQuarantined" => {
            if is_terminal(&next.run) {
                return reject(
                    ReducerCode::RunQuarantineTerminal,
                    "cannot quarantine a terminal run",
                );
            }
            let conditions = with_conditions(
                &next.run.conditions,
                &["integrity_failed"],
                &["integrity_verified"],
            );
            next.run = projection(
                ProjectionKind::Run,
                "quarantined",
                &next.run.intent,
                None,
                &conditions,
            )?;
        }
        "RunTerminalized" => {
            if is_terminal(&next.run) || next.run.phase == "quarantined" {
                return reject(
                    ReducerCode::RunTerminalizePhase,
                    "RunTerminalized on a terminal/quarantined run",
                );
            }
            let outcome = text(data, "outcome");
            let conditions = derive_run_terminal_conditions(&next);
            let run = projection(
                ProjectionKind::Run,
                "terminal",
                &next.run.intent,
                Some(outcome),
                &conditions,
            )?;
            check_terminalized_outcome(Some(outcome), &run)?;
            next.run = run;
        }
        "NodeBecameReady" => {
            let node_id = text(data, "nodeId");
            let index = next.node_index(node_id)?;
            if next.run.phase != "active" {
                return reject(
                    ReducerCode::NodeReadyRunPhase,
                    "NodeBecameReady requires an active run",
                );
            }
            let node = &next.nodes[index].projection;
            if node.phase != "blocked" && node.phase != "exhausted" {
                return reject(
                    ReducerCode::NodeReadyPhase,
                    format!(
                        "NodeBecameReady requires a blocked/exhausted node, got {}",
                        node.phase
                    ),
                );
            }
            for dependency in graph.dependencies(node_id) {
                if next
                    .node(dependency)
                    .and_then(|node| node.outcome.as_deref())
                    != Some("accepted")
                {
                    return reject(
                        ReducerCode::NodeReadyDeps,
                        format!("node {node_id} dependency {dependency} is not accepted"),
                    );
                }
            }
            let conditions = with_conditions(
                &node.conditions,
                &["dependencies_accepted"],
                &["dependencies_pending"],
            );
            next.nodes[index].projection =
                projection(ProjectionKind::Node, "ready", "execute", None, &conditions)?;
        }
        "NodeBlocked" => {
            let index = next.node_index(text(data, "nodeId"))?;
            let node = &next.nodes[index].projection;
            if is_terminal(node) {
                return reject(
                    ReducerCode::NodeBlockedTerminal,
                    "cannot block a terminal node",
                );
            }
            next.nodes[index].projection = projection(
                ProjectionKind::Node,
                "blocked",
                &node.intent,
                None,
                &node.conditions,
            )?;
        }
        "AttemptPrepared" => {
            let node_id = text(data, "nodeId");
            let attempt_id = text(data, "attemptId");
            let index = next.node_index(node_id)?;
            if next.run.phase != "active" {
                return reject(
                    ReducerCode::AttemptPreparedRunPhase,
                    "AttemptPrepared requires an active run",
                );
            }
            let node = &next.nodes[index].projection;
            if node.phase != "ready" && node.phase != "attempting" {
                return reject(
                    ReducerCode::AttemptPreparedNodePhase,
                    format!(
                        "AttemptPrepared requires a ready/attempting node, got {}",
                        node.phase
                    ),
                );
            }
            if next.attempt(attempt_id).is_some() {
                return reject(
                    ReducerCode::AttemptExists,
                    format!("attempt {attempt_id} already exists"),
                );
            }
            if next
                .attempts
                .iter()
                .any(|attempt| attempt.node_id == node_id && !is_terminal(&attempt.projection))
            {
                return reject(
                    ReducerCode::AttemptNodeNonterminal,
                    format!("node {node_id} already has a nonterminal attempt"),
                );
            }
            let node_conditions = node.conditions.clone();
            next.attempts.push(AttemptRecord {
                attempt_id: attempt_id.to_string(),
                node_id: node_id.to_string(),
                operation_id: None,
                rlm_child_id: None,
                turn_id: None,
                settlement_digest: None,
                projection: projection(
                    ProjectionKind::Attempt,
                    "prepared",
                    "execute",
                    None,
                    &owned(&["admission_unbound", "result_absent", "quiescence_unproved"]),
                )?,
                turn: None,
            });
            next.nodes[index].projection = projection(
                ProjectionKind::Node,
                "attempting",
                "execute",
                None,
                &node_conditions,
            )?;
        }
        "AttemptDispatchCommitted" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let attempt = &next.attempts[index].projection;
            if attempt.phase != "prepared" {
                return reject(
                    ReducerCode::AttemptDispatchPhase,
                    "AttemptDispatchCommitted requires a prepared attempt",
                );
            }
            next.attempts[index].projection = projection(
                ProjectionKind::Attempt,
                "dispatch_committed",
                &attempt.intent,
                None,
                &attempt.conditions,
            )?;
        }
        "AttemptAdmissionBound" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let attempt = &next.attempts[index].projection;
            if attempt.phase != "dispatch_committed" {
                return reject(
                    ReducerCode::AttemptBindPhase,
                    "AttemptAdmissionBound requires a dispatch_committed attempt",
                );
            }
            let conditions = with_conditions(
                &attempt.conditions,
                &["admission_bound"],
                &["admission_unbound"],
            );
            let bound = projection(
                ProjectionKind::Attempt,
                "admission_bound",
                &attempt.intent,
                None,
                &conditions,
            )?;
            let turn = projection(
                ProjectionKind::Turn,
                "admitted",
                "execute",
                None,
                &owned(&["result_absent", "cancel_unactuated", "quiescence_unproved"]),
            )?;
            let record = &mut next.attempts[index];
            record.operation_id = Some(text(data, "operationId").to_string());
            record.rlm_child_id = Some(text(data, "rlmChildId").to_string());
            record.turn_id = Some(text(data, "turnId").to_string());
            record.projection = bound;
            record.turn = Some(turn);
        }
        "AttemptCancellationRequested" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let record = &next.attempts[index];
            if is_terminal(&record.projection) {
                return reject(
                    ReducerCode::AttemptCancelTerminal,
                    "cannot cancel a terminal attempt",
                );
            }
            let cancelling = projection(
                ProjectionKind::Attempt,
                "cancelling",
                "cancel",
                None,
                &record.projection.conditions,
            )?;
            let turn = match &record.turn {
                Some(turn) => Some(projection(
                    ProjectionKind::Turn,
                    &turn.phase,
                    "cancel",
                    turn.outcome.as_deref(),
                    &turn.conditions,
                )?),
                None => None,
            };
            let record = &mut next.attempts[index];
            record.projection = cancelling;
            record.turn = turn;
        }
        "AttemptSettlementObserved" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let record = &next.attempts[index];
            if !record.turn.as_ref().is_some_and(is_terminal) {
                return reject(
                    ReducerCode::SettlementBeforeHost,
                    "AttemptSettlementObserved requires a terminal host turn",
                );
            }
            let node_index = next.node_index(&record.node_id)?;
            let settled = projection(
                ProjectionKind::Attempt,
                "settled",
                &record.projection.intent,
                None,
                &record.projection.conditions,
            )?;
            let record = &mut next.attempts[index];
            record.settlement_digest = Some(text(data, "settlementDigest").to_string());
            record.projection = settled;
            if text(data, "outcome") == "completed" {
                let node = &next.nodes[node_index].projection;
                let phase = if is_terminal(node) {
                    "terminal"
                } else {
                    "attempting"
                };
                let conditions = with_conditions(
                    &node.conditions,
                    &["candidate_present"],
                    &["candidate_absent"],
                );
                next.nodes[node_index].projection = projection(
                    ProjectionKind::Node,
                    phase,
                    &node.intent,
                    node.outcome.as_deref(),
                    &conditions,
                )?;
            }
        }
        "AttemptAccepted" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let record = &next.attempts[index];
            let node_index = next.node_index(&record.node_id)?;
            if record.projection.phase != "settled" {
                return reject(
                    ReducerCode::AttemptAcceptPhase,
                    "AttemptAccepted requires a settled attempt",
                );
            }
            let node = &next.nodes[node_index].projection;
            if node.outcome.as_deref() == Some("accepted") {
                return reject(
                    ReducerCode::NodeAlreadyAccepted,
                    format!("node {} already accepted", record.node_id),
                );
            }
            let completed = projection(
                ProjectionKind::Attempt,
                "terminal",
                &record.projection.intent,
                Some("completed"),
                &record.projection.conditions,
            )?;
            let conditions = with_conditions(
                &node.conditions,
                &[
                    "dependencies_accepted",
                    "candidate_present",
                    "acceptance_passed",
                ],
                &[
                    "dependencies_pending",
                    "candidate_absent",
                    "acceptance_pending",
                    "acceptance_failed",
                ],
            );
            let accepted = projection(
                ProjectionKind::Node,
                "terminal",
                "none",
                Some("accepted"),
                &conditions,
            )?;
            next.attempts[index].projection = completed;
            next.nodes[node_index].projection = accepted;
        }
        "AttemptRejected" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let record = &next.attempts[index];
            let node_index = next.node_index(&record.node_id)?;
            if record.projection.phase != "settled" {
                return reject(
                    ReducerCode::AttemptRejectPhase,
                    "AttemptRejected requires a settled attempt",
                );
            }
            let outcome = if record
                .projection
                .conditions
                .iter()
                .any(|condition| condition == "result_present")
            {
                "completed"
            } else {
                "failed"
            };
            let rejected = projection(
                ProjectionKind::Attempt,
                "terminal",
                &record.projection.intent,
                Some(outcome),
                &record.projection.conditions,
            )?;
            let node = &next.nodes[node_index].projection;
            let conditions = with_conditions(
                &node.conditions,
                &["acceptance_failed"],
                &["acceptance_pending", "acceptance_passed"],
            );
            let exhausted =
                projection(ProjectionKind::Node, "exhausted", "none", None, &conditions)?;
            next.attempts[index].projection = rejected;
            next.nodes[node_index].projection = exhausted;
        }
        "AttemptOutcomeUnknown" => {
            let index = next.attempt_index(text(data, "attemptId"))?;
            let record = &next.attempts[index];
            if is_terminal(&record.projection) {
                return reject(
                    ReducerCode::AttemptUnknownTerminal,
                    "AttemptOutcomeUnknown on a terminal attempt",
                );
            }
            let unknown = projection(
                ProjectionKind::Attempt,
                "terminal",
                &record.projection.intent,
                Some("execution_unknown"),
                &record.projection.conditions,
            )?;
            let record = &mut next.attempts[index];
            record.settlement_digest = Some(text(data, "settlementDigest").to_string());
            record.projection = unknown;
        }
        "HostCursorAdvanced" => {
            next.host_cursor = Some(text(data, "hostCursor").to_string());
        }
        other => {
            return reject(
                ReducerCode::ControllerFactUnknown,
                format!("unknown controller fact type {other}"),
            );
        }
    }
    Ok(next)
}

/// The run conditions at terminalization, from accumulated evidence only;
/// the validator then refuses an outcome whose proofs are missing (e.g.
/// `succeeded` without proven quiescence). Slice 4 folds no budget
/// accounting or operation classification, so those two default satisfied
/// (the controller refines them in slices 6-7), exactly as TS.
fn derive_run_terminal_conditions(state: &RunAggregate) -> Vec<String> {
    let nonterminal_attempts = state
        .attempts
        .iter()
        .any(|attempt| !is_terminal(&attempt.projection));
    let all_turns_quiescent = state.attempts.iter().all(|attempt| {
        attempt.turn.as_ref().is_none_or(|turn| {
            turn.conditions
                .iter()
                .any(|condition| condition == "quiescence_proved")
        })
    });
    let mut add = vec!["admission_fenced"];
    if !state
        .run
        .conditions
        .iter()
        .any(|condition| condition == "integrity_failed")
    {
        add.push("integrity_verified");
    }
    add.extend(["budgets_within_limit", "effects_classified"]);
    if !nonterminal_attempts && all_turns_quiescent {
        add.push("owned_work_quiescent");
    }
    with_conditions(&state.run.conditions, &add, &["admission_open"])
}

/// The turn/attempt conditions a host settlement proves.
struct SettlementConditions {
    result: &'static str,
    usage: &'static str,
    quiescence: &'static str,
    cancel: &'static str,
}

fn settlement_conditions(settlement: &Value) -> SettlementConditions {
    let flag = |key: &str| settlement.get(key).and_then(Value::as_bool) == Some(true);
    SettlementConditions {
        result: if settlement["result"]["kind"] == "text" {
            "result_present"
        } else {
            "result_absent"
        },
        usage: if settlement["usage"]["finality"] == "final" {
            "usage_final"
        } else {
            "usage_known_prefix"
        },
        quiescence: if flag("descendantsQuiescent") {
            "quiescence_proved"
        } else {
            "quiescence_unproved"
        },
        cancel: if flag("cancelActuated") {
            "cancel_actuated"
        } else {
            "cancel_unactuated"
        },
    }
}

const RESULT_USAGE_QUIESCENCE: [&str; 6] = [
    "result_absent",
    "result_present",
    "usage_final",
    "usage_known_prefix",
    "quiescence_unproved",
    "quiescence_proved",
];

#[allow(clippy::too_many_lines)] // one arm per closed host fact type
fn reduce_host_fact(
    prev: Option<&RunAggregate>,
    event: &Value,
) -> Result<RunAggregate, ReducerError> {
    let Some(prev) = prev else {
        return reject(ReducerCode::HostBeforeAdmit, "host fact before RunAdmitted");
    };
    let kind = text(event, "type");
    let data = &event["data"];
    let host_event_id = text(event, "hostEventId");
    if prev
        .applied_host_event_ids
        .iter()
        .any(|id| id == host_event_id)
    {
        return reject(
            ReducerCode::HostEventDuplicate,
            format!("host event {host_event_id} already applied"),
        );
    }
    let mut next = prev.clone();
    next.applied_host_event_ids.push(host_event_id.to_string());
    next.host_cursor = Some(text(event, "hostCursor").to_string());
    let child = text(data, "rlmChildId");
    let turn_id = text(data, "turnId");

    match kind {
        // Operation/child topology and cleanup lifecycle are store/outbox
        // and slice-7 concerns: recorded through the cursor and dedup only.
        "OperationAdmitted"
        | "ChildAdmitted"
        | "TurnAdmitted"
        | "DeleteRequested"
        | "ChildTombstoned"
        | "ChildCleanupCompleted"
        | "ChildCleanupFailed" => {}
        "TurnStarted" => {
            let index = next.attempt_by_child_turn(child, turn_id)?;
            let record = &next.attempts[index];
            let Some(turn) = &record.turn else {
                return reject(
                    ReducerCode::TurnStartedUnbound,
                    "TurnStarted without a bound turn",
                );
            };
            if record.projection.phase == "admission_bound" {
                let running = projection(
                    ProjectionKind::Attempt,
                    "running",
                    &record.projection.intent,
                    None,
                    &record.projection.conditions,
                )?;
                let turn = projection(
                    ProjectionKind::Turn,
                    "running",
                    &turn.intent,
                    None,
                    &turn.conditions,
                )?;
                let record = &mut next.attempts[index];
                record.projection = running;
                record.turn = Some(turn);
            }
        }
        "TurnSettled" => {
            let index = next.attempt_by_child_turn(child, turn_id)?;
            let record = &next.attempts[index];
            let Some(turn) = &record.turn else {
                return reject(
                    ReducerCode::TurnSettledUnbound,
                    "TurnSettled without a bound turn",
                );
            };
            if is_terminal(turn) {
                return reject(
                    ReducerCode::TurnSettledTerminal,
                    "TurnSettled on a terminal turn",
                );
            }
            let settlement = &data["settlement"];
            let proved = settlement_conditions(settlement);
            let mut turn_remove = RESULT_USAGE_QUIESCENCE.to_vec();
            turn_remove.extend(["cancel_actuated", "cancel_unactuated"]);
            let turn_conditions = with_conditions(
                &turn.conditions,
                &[
                    proved.result,
                    proved.usage,
                    proved.quiescence,
                    proved.cancel,
                ],
                &turn_remove,
            );
            let attempt_conditions = with_conditions(
                &record.projection.conditions,
                &[proved.result, proved.usage, proved.quiescence],
                &RESULT_USAGE_QUIESCENCE,
            );
            let settled = projection(
                ProjectionKind::Attempt,
                "settled",
                &record.projection.intent,
                None,
                &attempt_conditions,
            )?;
            let turn = projection(
                ProjectionKind::Turn,
                "terminal",
                &turn.intent,
                Some(text(settlement, "outcome")),
                &turn_conditions,
            )?;
            let record = &mut next.attempts[index];
            record.projection = settled;
            record.turn = Some(turn);
        }
        "CancelRequested" | "CancelActuated" => {
            let index = next.attempt_by_child_turn(child, turn_id)?;
            let record = &next.attempts[index];
            if let Some(turn) = record.turn.as_ref().filter(|turn| !is_terminal(turn)) {
                let turn = if kind == "CancelRequested" {
                    projection(
                        ProjectionKind::Turn,
                        &turn.phase,
                        "cancel",
                        None,
                        &turn.conditions,
                    )?
                } else {
                    let conditions = with_conditions(
                        &turn.conditions,
                        &["cancel_actuated"],
                        &["cancel_unactuated"],
                    );
                    projection(
                        ProjectionKind::Turn,
                        &turn.phase,
                        &turn.intent,
                        None,
                        &conditions,
                    )?
                };
                next.attempts[index].turn = Some(turn);
            }
        }
        "ChildQuiescent" => {
            for record in &mut next.attempts {
                if record.rlm_child_id.as_deref() != Some(child) {
                    continue;
                }
                let conditions = with_conditions(
                    &record.projection.conditions,
                    &["quiescence_proved"],
                    &["quiescence_unproved"],
                );
                record.projection = projection(
                    ProjectionKind::Attempt,
                    &record.projection.phase,
                    &record.projection.intent,
                    record.projection.outcome.as_deref(),
                    &conditions,
                )?;
                if let Some(turn) = &record.turn {
                    let conditions = with_conditions(
                        &turn.conditions,
                        &["quiescence_proved"],
                        &["quiescence_unproved"],
                    );
                    record.turn = Some(projection(
                        ProjectionKind::Turn,
                        &turn.phase,
                        &turn.intent,
                        turn.outcome.as_deref(),
                        &conditions,
                    )?);
                }
            }
        }
        other => {
            return reject(
                ReducerCode::HostFactUnknown,
                format!("unknown host fact type {other}"),
            );
        }
    }
    Ok(next)
}
