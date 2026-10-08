//! The pure Workflow V2 reducer (`WORKFLOW-V2.md` §5, §6, §11): the TS
//! slice-4 suite (`workflow-v2-reducer.test.ts`) ported — the validator's
//! closed shape through the reducer's projection builder, the fold's
//! determinism and replay equivalence, and the illegal-transition matrix.

mod v2_support;

use pa_workflow::v2::projection::ProjectionKind;
use pa_workflow::v2::reducer::{
    check_terminalized_outcome, projection, reduce_run, revalidate_aggregate, ReducerCode,
    ReducerError,
};
use serde_json::{json, Value};
use v2_support::{ctrl_event, definition, evidence, host_event, turn_data, turn_settlement};

fn code(result: Result<impl std::fmt::Debug, ReducerError>) -> ReducerCode {
    result.unwrap_err().code
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

/// The admitted..admission-bound prefix of one single-node run.
fn bound_prefix(run_id: &str) -> Vec<Value> {
    let e = evidence();
    vec![
        ctrl_event(run_id, 1, "RunAdmitted", json!({ "evidenceDigest": e })),
        ctrl_event(run_id, 2, "RunStarted", json!({ "evidenceDigest": e })),
        ctrl_event(
            run_id,
            3,
            "NodeBecameReady",
            json!({ "nodeId": "n1", "evidenceDigest": e }),
        ),
        ctrl_event(
            run_id,
            4,
            "AttemptPrepared",
            json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
        ),
        ctrl_event(
            run_id,
            5,
            "AttemptDispatchCommitted",
            json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
        ),
        ctrl_event(
            run_id,
            6,
            "AttemptAdmissionBound",
            json!({
                "nodeId": "n1", "attemptId": "a1", "operationId": "op1",
                "rlmChildId": "child1", "turnId": "turn1", "evidenceDigest": e,
            }),
        ),
    ]
}

/// A fully accepted, drained, succeeded single-node run.
fn happy_path() -> Vec<Value> {
    let e = evidence();
    let mut facts = bound_prefix("run-1");
    let turn = turn_data("req-a1", "child1", "turn1");
    facts.push(host_event("he1", "hc-1", "TurnStarted", turn.clone()));
    let mut settled = turn;
    settled["settlement"] = turn_settlement("a1", "child1", "turn1", true, true);
    facts.push(host_event("he2", "hc-2", "TurnSettled", settled));
    facts.extend([
        ctrl_event(
            "run-1",
            7,
            "AttemptSettlementObserved",
            json!({ "nodeId": "n1", "attemptId": "a1", "settlementDigest": e, "outcome": "completed" }),
        ),
        ctrl_event("run-1", 8, "AttemptAccepted", json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e })),
        ctrl_event("run-1", 9, "RunDraining", json!({ "evidenceDigest": e })),
        ctrl_event("run-1", 10, "RunTerminalized", json!({ "evidenceDigest": e, "outcome": "succeeded" })),
    ]);
    facts
}

// ---- the validator's closed shape -------------------------------------------

#[test]
fn a_legal_projection_comes_back_with_sorted_conditions() {
    let run = projection(
        ProjectionKind::Run,
        "terminal",
        "start",
        Some("succeeded"),
        &strings(&[
            "owned_work_quiescent",
            "admission_fenced",
            "integrity_verified",
            "budgets_within_limit",
            "effects_classified",
        ]),
    )
    .unwrap();
    assert_eq!(
        run.conditions,
        strings(&[
            "admission_fenced",
            "budgets_within_limit",
            "effects_classified",
            "integrity_verified",
            "owned_work_quiescent",
        ])
    );
}

#[test]
fn out_of_vocabulary_duplicate_and_uncoupled_projections_are_refused() {
    let refused = |kind, phase, intent, outcome, conditions: &[&str]| {
        projection(kind, phase, intent, outcome, &strings(conditions))
            .unwrap_err()
            .code
    };
    for (kind, phase, intent, outcome, conditions) in [
        (ProjectionKind::Run, "nope", "none", None, &[][..]),
        (ProjectionKind::Run, "created", "nope", None, &[]),
        (ProjectionKind::Run, "terminal", "none", Some("nope"), &[]),
        (ProjectionKind::Run, "created", "none", None, &["nope"]),
        (
            ProjectionKind::Run,
            "created",
            "none",
            None,
            &["admission_open", "admission_open"],
        ),
        (ProjectionKind::Node, "terminal", "none", None, &[]),
        (ProjectionKind::Node, "ready", "none", Some("accepted"), &[]),
        (ProjectionKind::Run, "quarantined", "none", None, &[]),
    ] {
        assert_eq!(
            refused(kind, phase, intent, outcome, conditions),
            ReducerCode::Projection,
            "{kind:?} {phase} {intent} {outcome:?} {conditions:?}"
        );
    }
    assert!(projection(
        ProjectionKind::Run,
        "quarantined",
        "none",
        None,
        &strings(&["integrity_failed"])
    )
    .is_ok());
}

#[test]
fn the_terminalized_outcome_must_equal_the_normalized_run() {
    let run = projection(
        ProjectionKind::Run,
        "terminal",
        "none",
        Some("succeeded"),
        &strings(&[
            "admission_fenced",
            "owned_work_quiescent",
            "effects_classified",
            "budgets_within_limit",
            "integrity_verified",
        ]),
    )
    .unwrap();
    assert_eq!(check_terminalized_outcome(Some("succeeded"), &run), Ok(()));
    assert_eq!(
        code(check_terminalized_outcome(Some("failed"), &run)),
        ReducerCode::TerminalizedOutcomeMismatch
    );
    let active = projection(
        ProjectionKind::Run,
        "active",
        "start",
        None,
        &strings(&["admission_open"]),
    )
    .unwrap();
    assert_eq!(
        code(check_terminalized_outcome(None, &active)),
        ReducerCode::TerminalizedOutcomeMismatch
    );
}

// ---- determinism and replay equivalence -------------------------------------

#[test]
fn the_happy_path_folds_to_a_succeeded_run() {
    let aggregate = reduce_run(&definition(), &happy_path()).unwrap();
    assert_eq!(
        (
            aggregate.run.phase.as_str(),
            aggregate.run.outcome.as_deref(),
            aggregate.nodes[0].projection.outcome.as_deref(),
            aggregate.attempts[0].projection.outcome.as_deref(),
            aggregate.host_cursor.as_deref(),
            aggregate.last_controller_sequence,
            aggregate.applied_host_event_ids.clone(),
        ),
        (
            "terminal",
            Some("succeeded"),
            Some("accepted"),
            Some("completed"),
            Some("hc-2"),
            10,
            strings(&["he1", "he2"]),
        )
    );
    revalidate_aggregate(&aggregate).unwrap();
}

#[test]
fn the_fold_is_deterministic_and_replay_equivalent() {
    let facts = happy_path();
    let direct = reduce_run(&definition(), &facts).unwrap();
    assert_eq!(reduce_run(&definition(), &facts).unwrap(), direct);
    let round_tripped: Vec<Value> =
        serde_json::from_str(&serde_json::to_string(&facts).unwrap()).unwrap();
    assert_eq!(reduce_run(&definition(), &round_tripped).unwrap(), direct);
}

#[test]
fn folding_prefix_by_prefix_ends_at_the_batch_fold() {
    let facts = happy_path();
    let full = reduce_run(&definition(), &facts).unwrap();
    let mut last = None;
    for end in 1..=facts.len() {
        last = Some(reduce_run(&definition(), &facts[..end]).unwrap());
    }
    assert_eq!(last, Some(full));
}

// ---- illegal transitions ------------------------------------------------------

fn fold(facts: &[Value]) -> ReducerCode {
    code(reduce_run(&definition(), facts))
}

#[test]
fn a_stream_must_start_with_run_admitted() {
    assert_eq!(
        fold(&[ctrl_event(
            "run-1",
            1,
            "RunStarted",
            json!({ "evidenceDigest": evidence() })
        )]),
        ReducerCode::RunNotAdmitted
    );
    assert_eq!(fold(&[]), ReducerCode::EmptyFactStream);
    let mut twice = bound_prefix("run-1");
    twice.truncate(1);
    twice.push(ctrl_event(
        "run-1",
        2,
        "RunAdmitted",
        json!({ "evidenceDigest": evidence() }),
    ));
    assert_eq!(fold(&twice), ReducerCode::RunAlreadyAdmitted);
}

#[test]
fn run_started_twice_is_refused() {
    let mut facts = bound_prefix("run-1");
    facts.truncate(2);
    facts.push(ctrl_event(
        "run-1",
        3,
        "RunStarted",
        json!({ "evidenceDigest": evidence() }),
    ));
    assert_eq!(fold(&facts), ReducerCode::RunStartPhase);
}

#[test]
fn a_node_holds_at_most_one_nonterminal_attempt() {
    let mut facts = bound_prefix("run-1");
    facts.truncate(4);
    facts.push(ctrl_event(
        "run-1",
        5,
        "AttemptPrepared",
        json!({ "nodeId": "n1", "attemptId": "a2", "evidenceDigest": evidence() }),
    ));
    assert_eq!(fold(&facts), ReducerCode::AttemptNodeNonterminal);
}

#[test]
fn a_controller_sequence_never_regresses() {
    let e = evidence();
    let facts = [
        ctrl_event("run-1", 1, "RunAdmitted", json!({ "evidenceDigest": e })),
        ctrl_event("run-1", 1, "RunStarted", json!({ "evidenceDigest": e })),
    ];
    assert_eq!(fold(&facts), ReducerCode::ControllerSequenceRegressed);
}

#[test]
fn success_without_proven_quiescence_is_refused() {
    let e = evidence();
    let mut facts = bound_prefix("run-1");
    facts.truncate(4);
    facts.extend([
        ctrl_event("run-1", 5, "RunDraining", json!({ "evidenceDigest": e })),
        ctrl_event(
            "run-1",
            6,
            "RunTerminalized",
            json!({ "evidenceDigest": e, "outcome": "succeeded" }),
        ),
    ]);
    assert_eq!(fold(&facts), ReducerCode::Projection);
}

#[test]
fn a_host_event_id_applies_once() {
    let mut facts = bound_prefix("run-1");
    let turn = turn_data("req-a1", "child1", "turn1");
    facts.push(host_event("dup", "hc-1", "TurnStarted", turn.clone()));
    facts.push(host_event("dup", "hc-2", "TurnStarted", turn));
    assert_eq!(fold(&facts), ReducerCode::HostEventDuplicate);
}

#[test]
fn a_node_is_accepted_once() {
    let e = evidence();
    let mut facts = happy_path();
    facts.truncate(facts.len() - 2); // through AttemptAccepted
    facts.push(ctrl_event(
        "run-1",
        9,
        "AttemptAccepted",
        json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
    ));
    // The accepted attempt is terminal now, so the settled-phase rule fires
    // first; either way no second authority grant.
    assert_eq!(fold(&facts), ReducerCode::AttemptAcceptPhase);
}

#[test]
fn settlement_observation_waits_for_the_host_turn() {
    let mut facts = bound_prefix("run-1");
    facts.push(ctrl_event(
        "run-1",
        7,
        "AttemptSettlementObserved",
        json!({ "nodeId": "n1", "attemptId": "a1", "settlementDigest": evidence(), "outcome": "completed" }),
    ));
    assert_eq!(fold(&facts), ReducerCode::SettlementBeforeHost);
}

#[test]
fn a_completed_settlement_with_known_prefix_usage_never_reaches_the_fold() {
    let e = evidence();
    let mut facts = bound_prefix("run-1");
    let mut settled = turn_data("req-a1", "child1", "turn1");
    settled["settlement"] = turn_settlement("a1", "child1", "turn1", false, true);
    facts.push(host_event("he2", "hc-2", "TurnSettled", settled));
    facts.extend([
        ctrl_event(
            "run-1",
            7,
            "AttemptSettlementObserved",
            json!({ "nodeId": "n1", "attemptId": "a1", "settlementDigest": e, "outcome": "completed" }),
        ),
        ctrl_event("run-1", 8, "AttemptAccepted", json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e })),
    ]);
    // `completed` requires final usage (§8): the closed settlement wire
    // refuses it before any projection could carry it.
    assert_eq!(fold(&facts), ReducerCode::FactInvalid);
}

#[test]
fn an_unknown_outcome_is_terminal_for_the_attempt() {
    let e = evidence();
    let mut facts = bound_prefix("run-1");
    facts.push(ctrl_event(
        "run-1",
        7,
        "AttemptOutcomeUnknown",
        json!({ "nodeId": "n1", "attemptId": "a1", "settlementDigest": e, "outcome": "execution_unknown" }),
    ));
    let aggregate = reduce_run(&definition(), &facts).unwrap();
    assert_eq!(
        aggregate.attempts[0].projection.outcome.as_deref(),
        Some("execution_unknown")
    );
    facts.push(ctrl_event(
        "run-1",
        8,
        "AttemptOutcomeUnknown",
        json!({ "nodeId": "n1", "attemptId": "a1", "settlementDigest": e, "outcome": "execution_unknown" }),
    ));
    assert_eq!(fold(&facts), ReducerCode::AttemptUnknownTerminal);
}

#[test]
fn cancellation_fences_admission_and_host_cancel_facts_reach_the_turn() {
    let e = evidence();
    let mut facts = bound_prefix("run-1");
    let turn = turn_data("req-a1", "child1", "turn1");
    facts.extend([
        ctrl_event(
            "run-1",
            7,
            "RunCancellationRequested",
            json!({ "evidenceDigest": e }),
        ),
        ctrl_event(
            "run-1",
            8,
            "AttemptCancellationRequested",
            json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
        ),
        host_event("he-cr", "hc-1", "CancelRequested", turn.clone()),
        host_event("he-ca", "hc-2", "CancelActuated", turn),
        host_event(
            "he-q",
            "hc-3",
            "ChildQuiescent",
            json!({ "requestId": "req-a1", "rlmChildId": "child1", "evidenceDigest": e }),
        ),
    ]);
    let aggregate = reduce_run(&definition(), &facts).unwrap();
    let turn = aggregate.attempts[0].turn.clone().unwrap();
    assert_eq!(
        (
            aggregate.run.phase.as_str(),
            aggregate.run.conditions.clone(),
            aggregate.attempts[0].projection.phase.as_str(),
            aggregate.attempts[0].projection.conditions.clone(),
            turn.intent.as_str(),
            turn.conditions,
        ),
        (
            "cancelling",
            strings(&["admission_fenced"]),
            "cancelling",
            strings(&["admission_bound", "quiescence_proved", "result_absent"]),
            "cancel",
            strings(&["cancel_actuated", "quiescence_proved", "result_absent"]),
        )
    );
    // A new attempt cannot be prepared on a cancelling run.
    facts.push(ctrl_event(
        "run-1",
        9,
        "NodeBecameReady",
        json!({ "nodeId": "n1", "evidenceDigest": e }),
    ));
    assert_eq!(fold(&facts), ReducerCode::NodeReadyRunPhase);
}

#[test]
fn a_dependent_node_waits_for_its_accepted_dependency() {
    let e = evidence();
    let graph = v2_support::valid_definition(&[("n1", &[]), ("n2", &["n1"])], 10_000);
    let facts = [
        ctrl_event("run-1", 1, "RunAdmitted", json!({ "evidenceDigest": e })),
        ctrl_event("run-1", 2, "RunStarted", json!({ "evidenceDigest": e })),
        ctrl_event(
            "run-1",
            3,
            "NodeBecameReady",
            json!({ "nodeId": "n2", "evidenceDigest": e }),
        ),
    ];
    assert_eq!(code(reduce_run(&graph, &facts)), ReducerCode::NodeReadyDeps);
    let admitted = reduce_run(&graph, &facts[..1]).unwrap();
    assert_eq!(
        admitted
            .nodes
            .iter()
            .map(|node| node.projection.conditions.clone())
            .collect::<Vec<_>>(),
        vec![
            strings(&[
                "acceptance_pending",
                "candidate_absent",
                "dependencies_accepted"
            ]),
            strings(&[
                "acceptance_pending",
                "candidate_absent",
                "dependencies_pending"
            ]),
        ]
    );
}

#[test]
fn a_fact_outside_the_closed_wire_is_refused() {
    let mut facts = bound_prefix("run-1");
    facts.truncate(1);
    let mut unknown = ctrl_event(
        "run-1",
        2,
        "RunPaused",
        json!({ "evidenceDigest": evidence() }),
    );
    facts.push(unknown.clone());
    assert_eq!(fold(&facts), ReducerCode::FactInvalid);
    unknown["protocol"] = json!("prime.workflow/v1");
    facts[1] = unknown;
    assert_eq!(fold(&facts), ReducerCode::FactProtocolUnknown);
    facts[1] = json!(["not", "an", "object"]);
    assert_eq!(fold(&facts), ReducerCode::FactNotObject);
}

#[test]
fn a_host_fact_for_an_unbound_turn_is_refused() {
    let mut facts = bound_prefix("run-1");
    facts.push(host_event(
        "he1",
        "hc-1",
        "TurnStarted",
        turn_data("req-a1", "child9", "turn9"),
    ));
    assert_eq!(fold(&facts), ReducerCode::HostFactUnbound);
}
