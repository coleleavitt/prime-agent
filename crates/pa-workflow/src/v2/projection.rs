//! The one normative projection validator (`WORKFLOW-V2.md` §11, TS
//! `validateProjectionSemantics` in `workflow-v2-reducer.ts`): strict
//! schema validation of the projection the kind selects, then the closed
//! contradiction and required-proof tables, verbatim from the contract.
//! Every decode boundary that carries a projection routes through
//! [`validate_projection_semantics`]; there is no partial copy.

use serde_json::Value;

use super::schema;
use super::wire::WireError;

/// The projection kinds of the orthogonal state model (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionKind {
    Run,
    Node,
    Attempt,
    Turn,
    Operation,
}

impl ProjectionKind {
    fn def(self) -> &'static str {
        match self {
            ProjectionKind::Run => "runProjection",
            ProjectionKind::Node => "nodeProjection",
            ProjectionKind::Attempt => "attemptProjection",
            ProjectionKind::Turn => "turnProjection",
            ProjectionKind::Operation => "operationProjection",
        }
    }

    /// `CONTRADICTIONS[kind]`.
    fn contradictions(self) -> &'static [(&'static str, &'static str)] {
        match self {
            ProjectionKind::Run => &[
                ("admission_open", "admission_fenced"),
                ("integrity_verified", "integrity_failed"),
            ],
            ProjectionKind::Node => &[
                ("dependencies_pending", "dependencies_accepted"),
                ("candidate_absent", "candidate_present"),
                ("acceptance_pending", "acceptance_passed"),
                ("acceptance_pending", "acceptance_failed"),
                ("acceptance_passed", "acceptance_failed"),
            ],
            ProjectionKind::Attempt => &[
                ("admission_unbound", "admission_bound"),
                ("result_absent", "result_present"),
                ("usage_final", "usage_known_prefix"),
                ("quiescence_unproved", "quiescence_proved"),
            ],
            ProjectionKind::Turn => &[
                ("result_absent", "result_present"),
                ("usage_final", "usage_known_prefix"),
                ("cancel_unactuated", "cancel_actuated"),
                ("quiescence_unproved", "quiescence_proved"),
            ],
            ProjectionKind::Operation => &[
                ("claim_unheld", "claim_held"),
                ("receipt_absent", "receipt_present"),
                ("effect_unclassified", "effect_classified"),
            ],
        }
    }

    /// `REQUIRED_PROOFS[kind][outcome]`.
    fn required_proofs(self, outcome: &str) -> &'static [&'static str] {
        match (self, outcome) {
            (ProjectionKind::Run, "succeeded") => &[
                "admission_fenced",
                "owned_work_quiescent",
                "effects_classified",
                "budgets_within_limit",
                "integrity_verified",
            ],
            (ProjectionKind::Node, "accepted") => &[
                "dependencies_accepted",
                "candidate_present",
                "acceptance_passed",
            ],
            (ProjectionKind::Attempt, "completed") => &[
                "admission_bound",
                "result_present",
                "usage_final",
                "quiescence_proved",
            ],
            (ProjectionKind::Turn, "completed") => {
                &["result_present", "usage_final", "quiescence_proved"]
            }
            (ProjectionKind::Operation, "succeeded") => &["receipt_present", "effect_classified"],
            _ => &[],
        }
    }
}

/// A validated projection, conditions sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub phase: String,
    pub intent: String,
    pub outcome: Option<String>,
    pub conditions: Vec<String>,
}

/// `validateProjectionSemantics(kind, value)`.
///
/// # Errors
///
/// A schema violation, a contradictory condition pair, or an outcome
/// missing one of its proof conditions.
pub fn validate_projection_semantics(
    kind: ProjectionKind,
    value: &Value,
) -> Result<Projection, WireError> {
    schema::validate_def(value, kind.def())?;
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
    let mut conditions: Vec<String> = value["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let holds = |condition: &str| conditions.iter().any(|held| held == condition);
    for (first, second) in kind.contradictions() {
        if holds(first) && holds(second) {
            return Err(WireError::new(
                "$.conditions",
                format!("holds contradictory conditions {first} & {second}"),
            ));
        }
    }
    let outcome = value["outcome"].as_str().map(str::to_string);
    if let Some(outcome) = &outcome {
        if let Some(missing) = kind
            .required_proofs(outcome)
            .iter()
            .find(|proof| !holds(proof))
        {
            return Err(WireError::new(
                "$.conditions",
                format!("outcome {outcome} requires condition {missing}"),
            ));
        }
    }
    conditions.sort();
    Ok(Projection {
        phase: text("phase"),
        intent: text("intent"),
        outcome,
        conditions,
    })
}

/// The `RunTerminalized` cross-record rule (§5/§11): the event's
/// `data.outcome` equals the normalized terminal run projection's outcome.
///
/// # Errors
///
/// An invalid or non-terminal run projection, or an outcome mismatch.
pub fn assert_run_terminalized_outcome_equality(
    event_outcome: &Value,
    run: &Value,
) -> Result<(), WireError> {
    let run = validate_projection_semantics(ProjectionKind::Run, run)?;
    if run.phase != "terminal" {
        return Err(WireError::new(
            "$.phase",
            "RunTerminalized applies only to a terminal run projection",
        ));
    }
    if event_outcome.as_str() != run.outcome.as_deref() {
        return Err(WireError::new(
            "$.outcome",
            "RunTerminalized.data.outcome differs from the normalized run outcome",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn projection(phase: &str, intent: &str, outcome: Option<&str>, conditions: &[&str]) -> Value {
        json!({ "phase": phase, "intent": intent, "outcome": outcome, "conditions": conditions })
    }

    const RUN_SUCCESS: [&str; 5] = [
        "admission_fenced",
        "owned_work_quiescent",
        "effects_classified",
        "budgets_within_limit",
        "integrity_verified",
    ];

    #[test]
    fn a_proven_success_validates_and_sorts_its_conditions() {
        let run = projection("terminal", "start", Some("succeeded"), &RUN_SUCCESS);
        assert_eq!(
            validate_projection_semantics(ProjectionKind::Run, &run),
            Ok(Projection {
                phase: "terminal".to_string(),
                intent: "start".to_string(),
                outcome: Some("succeeded".to_string()),
                conditions: vec![
                    "admission_fenced".to_string(),
                    "budgets_within_limit".to_string(),
                    "effects_classified".to_string(),
                    "integrity_verified".to_string(),
                    "owned_work_quiescent".to_string(),
                ],
            })
        );
    }

    /// The §11 mutation matrix, row by row, through the one helper.
    #[test]
    fn the_mutation_matrix_is_rejected() {
        // Run `succeeded` missing any success barrier.
        for missing in RUN_SUCCESS {
            let held: Vec<&str> = RUN_SUCCESS.into_iter().filter(|c| *c != missing).collect();
            let run = projection("terminal", "start", Some("succeeded"), &held);
            assert!(
                validate_projection_semantics(ProjectionKind::Run, &run).is_err(),
                "{missing}"
            );
        }
        // Every closed contradictory pair, on every kind.
        let phases = [
            (ProjectionKind::Run, "active", "start"),
            (ProjectionKind::Node, "ready", "execute"),
            (ProjectionKind::Attempt, "running", "execute"),
            (ProjectionKind::Turn, "running", "execute"),
            (ProjectionKind::Operation, "pending", "deliver"),
        ];
        for (kind, phase, intent) in phases {
            for (first, second) in kind.contradictions() {
                let value = projection(phase, intent, None, &[first, second]);
                assert!(
                    validate_projection_semantics(kind, &value).is_err(),
                    "{kind:?} {first} {second}"
                );
            }
        }
        // Attempt/turn `completed` and operation `succeeded` and node
        // `accepted` missing any proof.
        for (kind, outcome) in [
            (ProjectionKind::Attempt, "completed"),
            (ProjectionKind::Turn, "completed"),
            (ProjectionKind::Operation, "succeeded"),
            (ProjectionKind::Node, "accepted"),
        ] {
            let proofs = kind.required_proofs(outcome);
            let phase = "terminal";
            let intent = if kind == ProjectionKind::Operation {
                "deliver"
            } else {
                "execute"
            };
            let complete = projection(phase, intent, Some(outcome), proofs);
            assert!(
                validate_projection_semantics(kind, &complete).is_ok(),
                "{kind:?} {complete}"
            );
            for missing in proofs {
                let held: Vec<&str> = proofs.iter().copied().filter(|c| c != missing).collect();
                let value = projection(phase, intent, Some(outcome), &held);
                assert!(
                    validate_projection_semantics(kind, &value).is_err(),
                    "{kind:?} {missing}"
                );
            }
        }
        // Outcome outside the closed vocabulary; terminal/outcome coupling.
        for value in [
            projection("terminal", "start", Some("partial"), &[]),
            projection("active", "start", Some("failed"), &[]),
            projection("terminal", "start", None, &[]),
            projection("quarantined", "none", None, &[]),
        ] {
            assert!(
                validate_projection_semantics(ProjectionKind::Run, &value).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn terminalized_outcome_equals_the_normalized_run_outcome() {
        let run = projection("terminal", "start", Some("failed"), &["admission_fenced"]);
        assert_eq!(
            assert_run_terminalized_outcome_equality(&json!("failed"), &run),
            Ok(())
        );
        assert_eq!(
            assert_run_terminalized_outcome_equality(&json!("succeeded"), &run)
                .unwrap_err()
                .to_string(),
            "$.outcome RunTerminalized.data.outcome differs from the normalized run outcome"
        );
        let active = projection("active", "start", None, &[]);
        assert!(assert_run_terminalized_outcome_equality(&Value::Null, &active).is_err());
    }
}
