//! Failure-triggered refines (TS `_queueFailureTriggeredRefine`,
//! `mergeRefineRequests`, `withRefineRun` and the trigger half of
//! `_observeFailuresAtTurnBoundary`): a fingerprint entering the recurring
//! set queues a `recurrence` refine, and a provisional champion whose claim
//! recurred inside its window queues a `regression` repair in the
//! champion's own scope. Both are deduped per fingerprint per session and
//! ride the session's pending refine, merged with a pending request of the
//! same scope and parked behind one of the other scope.

use pa_core::session_engine::turn_boundary::{PendingRefine, RefineTrigger};
use serde::{Deserialize, Serialize};

use crate::gate::{RefineKind, RefineReason};

/// What a queued refine is for, as its trigger carries it. Field order is
/// the TS request's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FailureRequest {
    pub reason: RefineReason,
    pub kind: RequestKind,
    pub trigger_fingerprint_ids: Vec<String>,
}

/// The kind a queued request is gated as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RequestKind {
    Failure,
    Directed,
}

impl RequestKind {
    pub(crate) fn refine_kind(self) -> RefineKind {
        match self {
            Self::Failure => RefineKind::Failure,
            Self::Directed => RefineKind::Directed,
        }
    }
}

/// `regression > recurrence > refine_run > manual`.
fn rank(reason: RefineReason) -> u8 {
    match reason {
        RefineReason::Manual => 1,
        RefineReason::RefineRun => 2,
        RefineReason::Recurrence => 3,
        RefineReason::Regression => 4,
        RefineReason::TurnInterval
        | RefineReason::Compact
        | RefineReason::Rollback
        | RefineReason::RavoRun => 0,
    }
}

/// The reason a merged pending refine runs under (TS
/// `dominantRefineReason`).
pub(crate) fn dominant_reason(current: RefineReason, incoming: RefineReason) -> RefineReason {
    if rank(incoming) > rank(current) {
        incoming
    } else {
        current
    }
}

/// How the gate reads a refine's trigger: the request, with an agent's
/// `refine.run` that joined it folded in (gated as directed, under the
/// stronger reason, its failures kept).
pub(crate) fn read_trigger(trigger: &RefineTrigger) -> Option<FailureRequest> {
    let mut request: FailureRequest = serde_json::from_value(trigger.data.clone()).ok()?;
    if trigger.joined_by_agent {
        request.reason = dominant_reason(request.reason, RefineReason::RefineRun);
        request.kind = RequestKind::Directed;
    }
    Some(request)
}

/// The request a pending refine stands for: the agent's own `refine.run`
/// when it carries no trigger.
fn pending_request(pending: &PendingRefine) -> FailureRequest {
    pending
        .trigger
        .as_ref()
        .and_then(read_trigger)
        .unwrap_or(FailureRequest {
            reason: RefineReason::RefineRun,
            kind: RequestKind::Directed,
            trigger_fingerprint_ids: Vec::new(),
        })
}

fn to_pending(
    instructions: Option<String>,
    global: bool,
    request: &FailureRequest,
) -> PendingRefine {
    PendingRefine {
        instructions,
        global,
        trigger: Some(RefineTrigger {
            data: serde_json::to_value(request).unwrap_or_default(),
            joined_by_agent: false,
        }),
        plan_id: None,
    }
}

/// A failure refine to queue.
pub(crate) fn failure_refine(
    instructions: String,
    reason: RefineReason,
    fingerprint_ids: Vec<String>,
    global: bool,
) -> PendingRefine {
    to_pending(
        Some(instructions),
        global,
        &FailureRequest {
            reason,
            kind: RequestKind::Failure,
            trigger_fingerprint_ids: fingerprint_ids,
        },
    )
}

/// Two queued requests of the same scope as one (TS
/// `mergeRefineRequests`): instructions appended, the stronger reason, a
/// failure refine only if both were, the union of the triggers.
pub(crate) fn merge_requests(previous: &PendingRefine, incoming: &PendingRefine) -> PendingRefine {
    let before = pending_request(previous);
    let after = pending_request(incoming);
    let mut triggers = before.trigger_fingerprint_ids.clone();
    for id in after.trigger_fingerprint_ids {
        if !triggers.contains(&id) {
            triggers.push(id);
        }
    }
    let instructions = match (&previous.instructions, &incoming.instructions) {
        (Some(queued), Some(added)) if !queued.is_empty() && !added.is_empty() => {
            Some(format!("{queued}\n\n{added}"))
        }
        (queued, added) => added.clone().or_else(|| queued.clone()),
    };
    let mut merged = to_pending(
        instructions,
        previous.global,
        &FailureRequest {
            reason: dominant_reason(before.reason, after.reason),
            kind: if before.kind == RequestKind::Failure && after.kind == RequestKind::Failure {
                RequestKind::Failure
            } else {
                RequestKind::Directed
            },
            trigger_fingerprint_ids: triggers,
        },
    );
    // An approved (previewed) plan stays pinned through the merge.
    merged.plan_id = previous.plan_id.clone().or_else(|| incoming.plan_id.clone());
    merged
}

/// Queue `request` on the session's pending refine: alone, merged into a
/// pending one of the same scope, or parked (merged into a parked one of
/// its scope) behind one of the other scope.
pub(crate) fn queue(
    pending: Option<PendingRefine>,
    request: PendingRefine,
    parked: &mut Vec<PendingRefine>,
) -> PendingRefine {
    let Some(pending) = pending else {
        return request;
    };
    if pending.global == request.global {
        return merge_requests(&pending, &request);
    }
    match parked
        .iter_mut()
        .find(|queued| queued.global == request.global)
    {
        Some(queued) => *queued = merge_requests(queued, &request),
        None => parked.push(request),
    }
    pending
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn agent_request(instructions: &str, global: bool) -> PendingRefine {
        PendingRefine {
            instructions: Some(instructions.to_string()),
            global,
            trigger: None,
            plan_id: None,
        }
    }

    #[test]
    fn requests_merge_parks_and_join_like_the_ts_session() {
        let recurrence = failure_refine(
            "fix a".to_string(),
            RefineReason::Recurrence,
            vec!["a".to_string()],
            false,
        );
        let regression = failure_refine(
            "repair b".to_string(),
            RefineReason::Regression,
            vec!["b".to_string(), "a".to_string()],
            false,
        );
        let mut parked = Vec::new();
        let merged = queue(Some(recurrence.clone()), regression, &mut parked);
        assert_eq!(
            merged,
            PendingRefine {
                instructions: Some("fix a\n\nrepair b".to_string()),
                global: false,
                trigger: Some(RefineTrigger {
                    data: json!({
                        "reason": "regression",
                        "kind": "failure",
                        "triggerFingerprintIds": ["a", "b"]
                    }),
                    joined_by_agent: false,
                }),
                plan_id: None,
            }
        );
        // The agent's own pending request makes the merge directed.
        let directed = queue(
            Some(agent_request("note it", false)),
            recurrence.clone(),
            &mut parked,
        );
        assert_eq!(
            directed.trigger.unwrap().data,
            json!({ "reason": "recurrence", "kind": "directed", "triggerFingerprintIds": ["a"] })
        );
        assert!(parked.is_empty());
        // The other scope parks, merged with what is parked for it.
        let global = failure_refine(
            "g1".to_string(),
            RefineReason::Regression,
            vec!["g".to_string()],
            true,
        );
        let kept = queue(Some(recurrence.clone()), global.clone(), &mut parked);
        assert_eq!(kept, recurrence);
        let again = queue(Some(recurrence.clone()), global, &mut parked);
        assert_eq!(again, recurrence);
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].instructions.as_deref(), Some("g1\n\ng1"));
        // A joined trigger reads as directed under the stronger reason.
        let joined = RefineTrigger {
            joined_by_agent: true,
            ..recurrence.trigger.unwrap()
        };
        assert_eq!(
            read_trigger(&joined),
            Some(FailureRequest {
                reason: RefineReason::Recurrence,
                kind: RequestKind::Directed,
                trigger_fingerprint_ids: vec!["a".to_string()],
            })
        );
    }
}
