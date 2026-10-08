//! The pure settlement reducer (`WORKFLOW-V2.md` §8, §9; TS
//! `workflow-v2-settlement.ts`, slice 3).
//!
//! A closed, total, I/O-free reduction from one discriminated terminal
//! capture, its closure, and the durable dispatch/cancel/quiescence facts
//! into one [`AtomicSettlementCommit`]: the immutable `TurnSettlement`, the
//! exact `TurnSettled` host event (`prime.workflow.retained-event/v2`, what
//! the slice-4 store ingests), the terminal replay receipt, and the
//! successor host cursor. A possible provider effect without complete
//! durable terminal evidence reduces to `execution_unknown`; recovery never
//! relaunches it. Every digest is RFC 8785 and byte-compatible with TS.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::capture::{
    digest_value, seal, sealed_digest, CaptureClosure, CaptureStopReason, ExactResult, ExactUsage,
    Finality, NoneReason, SafeError, SafeErrorCode, TerminalCapture, TurnBinding,
    PLACEHOLDER_DIGEST,
};
use super::wire::WireError;

/// The commit bundle's protocol.
pub const SETTLEMENT_COMMIT_PROTOCOL: &str = "prime.workflow.retained-settlement-commit/v2-slice3";
/// The host-fact protocol of the settled event.
pub const RETAINED_EVENT_PROTOCOL: &str = "prime.workflow.retained-event/v2";
/// The terminal replay receipt's protocol.
pub const TERMINAL_RECEIPT_PROTOCOL: &str = "prime.workflow.retained-terminal-receipt/v2-slice3";
const MAX_SAFE: u64 = 9_007_199_254_740_991;

/// The writer fence the commit was made under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fence {
    pub supervisor_generation: u64,
    pub supervisor_incarnation_id: String,
    pub worker_id: String,
    pub worker_generation: u64,
    pub worker_incarnation_id: String,
    pub route_revision: u64,
}

/// Whether the provider was entered for the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEntry {
    Observed,
    NotObserved,
    Uncertain,
}

/// Owned-descendant quiescence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quiescence {
    Proved,
    Unproved,
}

/// The durable cancellation facts for the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelEvidence {
    pub requested: bool,
    pub actuated: bool,
    pub actuated_at: Option<String>,
    /// Proven that the cancel was ordered strictly after durable completion.
    pub ordered_after_completion: bool,
}

/// Everything one settlement reduces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementInput {
    pub binding: TurnBinding,
    pub fence: Fence,
    pub capture: TerminalCapture,
    pub capture_closure: CaptureClosure,
    pub dispatching_sequence: u64,
    pub provider_entry: ProviderEntry,
    pub cancel: CancelEvidence,
    pub quiescence: Quiescence,
    pub topology_evidence_digest: String,
    pub started_at: Option<String>,
    pub settled_at: String,
    pub host_cursor: String,
    pub next_host_cursor: String,
    pub host_event_id: String,
}

/// A settled turn's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Completed,
    Failed,
    Cancelled,
    ExecutionUnknown,
}

/// The cancellation state the commit records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelState {
    NotRequested,
    Requested,
    Actuated,
    Uncertain,
}

/// The terminal replay receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalReceipt {
    pub protocol: String,
    pub request_id: String,
    pub request_digest: String,
    pub rlm_child_id: String,
    pub turn_id: String,
    pub host_cursor: String,
    pub settlement_digest: String,
}

/// The commit's closed evidence summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementEvidence {
    pub dispatching_sequence: u64,
    pub provider_entry: ProviderEntry,
    pub capture_closed: bool,
    pub agent_end_evidence_digest: Option<String>,
    pub cancel_state: CancelState,
    pub quiescence: Quiescence,
}

/// One atomic settlement commit (TS key order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtomicSettlementCommit {
    pub protocol: String,
    pub fence: Fence,
    pub binding: TurnBinding,
    pub capture: TerminalCapture,
    pub capture_closure: CaptureClosure,
    pub topology_evidence_digest: String,
    /// The sealed `TurnSettlement` (`turnSettlement` on the wire).
    pub settlement: Value,
    pub settlement_digest: String,
    /// The sealed `TurnSettled` host event.
    pub event: Value,
    pub next_host_cursor: String,
    pub receipt: TerminalReceipt,
    pub evidence: SettlementEvidence,
    pub commit_digest: String,
}

/// Why a settlement input was refused before any commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettlementRejection {
    #[error("correlation_mismatch")]
    CorrelationMismatch,
    #[error("cursor_not_successor")]
    CursorNotSuccessor,
    #[error("invalid_input")]
    InvalidInput,
}

impl From<WireError> for SettlementRejection {
    fn from(_: WireError) -> Self {
        SettlementRejection::InvalidInput
    }
}

/// `sha256:` of the closed `{authorityId, parentSessionId, rootSessionId}`
/// scope.
///
/// # Errors
///
/// Never in practice (the scope holds only strings).
pub fn authority_scope_digest(binding: &TurnBinding) -> Result<String, WireError> {
    digest_value(&json!({
        "authorityId": binding.authority_id,
        "parentSessionId": binding.parent_session_id,
        "rootSessionId": binding.root_session_id,
    }))
}

struct OutcomePlan {
    outcome: TerminalOutcome,
    result: ExactResult,
    usage: ExactUsage,
    error: Option<SafeError>,
    cancel_actuated: bool,
    descendants_quiescent: bool,
    cancel_state: CancelState,
}

fn safe_error(code: SafeErrorCode, message: &str) -> SafeError {
    SafeError {
        code,
        message: message.to_string(),
        retryable: false,
    }
}

fn unknown_plan(usage_prefix: ExactUsage, input: &SettlementInput) -> OutcomePlan {
    let cancel_state = if input.cancel.actuated {
        CancelState::Actuated
    } else if input.cancel.requested {
        CancelState::Uncertain
    } else {
        CancelState::NotRequested
    };
    OutcomePlan {
        outcome: TerminalOutcome::ExecutionUnknown,
        result: ExactResult::None {
            reason: NoneReason::Unknown,
        },
        usage: usage_prefix.as_known_prefix(),
        error: Some(safe_error(
            SafeErrorCode::ExecutionUnknown,
            "execution outcome unknown",
        )),
        cancel_actuated: input.cancel.actuated,
        descendants_quiescent: input.quiescence == Quiescence::Proved,
        cancel_state,
    }
}

fn plan_outcome(input: &SettlementInput) -> OutcomePlan {
    // Any ambiguity, unproven quiescence, or unresolved closure is terminal
    // unknown.
    let observed = match &input.capture {
        TerminalCapture::Ambiguous(capture) => return unknown_plan(capture.usage_prefix, input),
        TerminalCapture::Observed(capture) => capture,
    };
    if matches!(input.capture_closure, CaptureClosure::Ambiguous(_))
        || input.quiescence != Quiescence::Proved
        || observed.usage.finality != Finality::Final
    {
        return unknown_plan(observed.usage, input);
    }
    let usage = observed.usage;
    let settled = |outcome, result, error, cancel_state| OutcomePlan {
        outcome,
        result,
        usage,
        error,
        cancel_actuated: false,
        descendants_quiescent: true,
        cancel_state,
    };
    // Cancellation: an aborted terminal is cancelled only with correlated
    // actuation; a non-abort terminal after an actuated cancel of unproven
    // ordering is uncertain.
    if observed.stop_reason == CaptureStopReason::Aborted {
        if !input.cancel.actuated {
            return unknown_plan(usage, input);
        }
        return OutcomePlan {
            cancel_actuated: true,
            ..settled(
                TerminalOutcome::Cancelled,
                ExactResult::None {
                    reason: NoneReason::Cancelled,
                },
                Some(safe_error(SafeErrorCode::Cancelled, "turn cancelled")),
                CancelState::Actuated,
            )
        };
    }
    if input.cancel.actuated && !input.cancel.ordered_after_completion {
        return unknown_plan(usage, input);
    }
    let cancel_state = if input.cancel.requested {
        CancelState::Requested
    } else {
        CancelState::NotRequested
    };
    if matches!(observed.result, ExactResult::TooLarge { .. }) {
        return settled(
            TerminalOutcome::Failed,
            observed.result.clone(),
            Some(safe_error(
                SafeErrorCode::ResultInvalid,
                "result exceeds inline capacity",
            )),
            cancel_state,
        );
    }
    if observed.stop_reason == CaptureStopReason::Error {
        let code = match observed.safe_error.as_ref().map(|error| error.code) {
            Some(code @ (SafeErrorCode::AuthFailed | SafeErrorCode::ModelUnavailable)) => code,
            _ => SafeErrorCode::ProviderFailed,
        };
        let message = observed
            .safe_error
            .as_ref()
            .map_or("provider error", |error| error.message.as_str());
        return settled(
            TerminalOutcome::Failed,
            ExactResult::None {
                reason: NoneReason::ProviderError,
            },
            Some(safe_error(code, message)),
            cancel_state,
        );
    }
    if observed.stop_reason == CaptureStopReason::Stop
        && matches!(observed.result, ExactResult::Text { .. })
    {
        return settled(
            TerminalOutcome::Completed,
            observed.result.clone(),
            None,
            cancel_state,
        );
    }
    // Empty stop, length, tool use, other, or a malformed profile terminal.
    let stop = serde_json::to_value(observed.stop_reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    settled(
        TerminalOutcome::Failed,
        ExactResult::None {
            reason: NoneReason::NoAssistant,
        },
        Some(safe_error(
            SafeErrorCode::ResultInvalid,
            &format!("unusable terminal ({stop})"),
        )),
        cancel_state,
    )
}

fn to_value(value: &impl Serialize) -> Result<Value, SettlementRejection> {
    serde_json::to_value(value).map_err(|_| SettlementRejection::InvalidInput)
}

/// The sealed `TurnSettlement` (TS key order).
fn settlement_record(
    input: &SettlementInput,
    plan: &OutcomePlan,
) -> Result<Value, SettlementRejection> {
    let binding = &input.binding;
    seal(
        json!({
            "authorityScope": authority_scope_digest(binding)?,
            "parentId": binding.parent_session_id,
            "requestId": binding.request_id,
            "nodeId": binding.node_id,
            "attemptId": binding.attempt_id,
            "rlmChildId": binding.rlm_child_id,
            "turnId": binding.turn_id,
            "admittedAt": binding.admitted_at,
            "startedAt": input.started_at,
            "settledAt": input.settled_at,
            "cancelActuated": plan.cancel_actuated,
            "descendantsQuiescent": plan.descendants_quiescent,
            "hostCursor": input.host_cursor,
            "settlementDigest": PLACEHOLDER_DIGEST,
            "outcome": plan.outcome,
            "result": to_value(&plan.result)?,
            "usage": to_value(&plan.usage)?,
            "error": plan.error,
            "workflowChildId": binding.workflow_child_id,
            "requestDigest": binding.request_digest,
        }),
        "settlementDigest",
    )
    .map_err(SettlementRejection::from)
}

/// Reduce the durable capture/closure/evidence facts into one atomic
/// settlement commit. Pure and deterministic: identical input yields
/// byte-identical settlement, event, receipt, cursor, and digests.
///
/// # Errors
///
/// [`SettlementRejection`] when the capture, closure, and binding do not
/// name one turn, the next cursor is not a successor, or the dispatch
/// sequence is out of range.
pub fn reduce_settlement(
    input: &SettlementInput,
) -> Result<AtomicSettlementCommit, SettlementRejection> {
    let binding = &input.binding;
    let (capture, closure) = (input.capture.binding(), input.capture_closure.binding());
    if capture.rlm_child_id != binding.rlm_child_id
        || capture.turn_id != binding.turn_id
        || capture.request_id != binding.request_id
        || closure.rlm_child_id != binding.rlm_child_id
        || closure.turn_id != binding.turn_id
        || input.capture.invocation_id() != input.capture_closure.invocation_id()
    {
        return Err(SettlementRejection::CorrelationMismatch);
    }
    if input.host_cursor == input.next_host_cursor {
        return Err(SettlementRejection::CursorNotSuccessor);
    }
    if !(1..=MAX_SAFE).contains(&input.dispatching_sequence) {
        return Err(SettlementRejection::InvalidInput);
    }

    let plan = plan_outcome(input);
    let settlement = settlement_record(input, &plan)?;
    let settlement_digest = settlement["settlementDigest"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let evidence_digest = digest_value(&json!({
        "capture": to_value(&input.capture)?,
        "captureClosure": to_value(&input.capture_closure)?,
        "cancel": to_value(&input.cancel)?,
        "quiescence": input.quiescence,
        "dispatch": {
            "dispatchingSequence": input.dispatching_sequence,
            "providerEntry": input.provider_entry,
        },
        "topologyEvidenceDigest": input.topology_evidence_digest,
    }))?;
    let event = seal(
        json!({
            "protocol": RETAINED_EVENT_PROTOCOL,
            "hostEventId": input.host_event_id,
            "hostCursor": input.host_cursor,
            "type": "TurnSettled",
            "recordedAt": input.settled_at,
            "data": {
                "requestId": binding.request_id,
                "rlmChildId": binding.rlm_child_id,
                "turnId": binding.turn_id,
                "settlement": settlement,
                "evidenceDigest": evidence_digest,
            },
            "digest": PLACEHOLDER_DIGEST,
        }),
        "digest",
    )?;
    let receipt = TerminalReceipt {
        protocol: TERMINAL_RECEIPT_PROTOCOL.to_string(),
        request_id: binding.request_id.clone(),
        request_digest: binding.request_digest.clone(),
        rlm_child_id: binding.rlm_child_id.clone(),
        turn_id: binding.turn_id.clone(),
        host_cursor: input.host_cursor.clone(),
        settlement_digest: settlement_digest.clone(),
    };
    let agent_end_evidence_digest = match &input.capture_closure {
        CaptureClosure::Observed(closure) => Some(closure.closure_digest.clone()),
        CaptureClosure::Ambiguous(_) => None,
    };
    let mut commit = AtomicSettlementCommit {
        protocol: SETTLEMENT_COMMIT_PROTOCOL.to_string(),
        fence: input.fence.clone(),
        binding: binding.clone(),
        capture: input.capture.clone(),
        capture_closure: input.capture_closure.clone(),
        topology_evidence_digest: input.topology_evidence_digest.clone(),
        settlement,
        settlement_digest,
        event,
        next_host_cursor: input.next_host_cursor.clone(),
        receipt,
        evidence: SettlementEvidence {
            dispatching_sequence: input.dispatching_sequence,
            provider_entry: input.provider_entry,
            capture_closed: matches!(input.capture_closure, CaptureClosure::Observed(_)),
            agent_end_evidence_digest,
            cancel_state: plan.cancel_state,
            quiescence: input.quiescence,
        },
        commit_digest: PLACEHOLDER_DIGEST.to_string(),
    };
    commit.commit_digest = sealed_digest(&to_value(&commit)?, "commitDigest")?;
    Ok(commit)
}

/// Why a commit failed re-validation (the TS reason string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("settlement commit invalid: {0}")]
pub struct CommitInvalid(pub &'static str);

fn sealed_matches(value: &Value, field: &str) -> bool {
    sealed_digest(value, field).is_ok_and(|digest| value[field].as_str() == Some(digest.as_str()))
}

/// Re-derive every digest and cross-check every repeated binding, id,
/// cursor, and outcome/usage rule of one commit; any contradiction refuses
/// it before commit, hydration, replay, or projection.
///
/// # Errors
///
/// The first contradiction.
pub fn validate_settlement_commit(commit: &AtomicSettlementCommit) -> Result<(), CommitInvalid> {
    let fail = |reason| Err(CommitInvalid(reason));
    let whole = serde_json::to_value(commit).map_err(|_| CommitInvalid("commit_shape"))?;
    if !sealed_matches(&whole, "commitDigest") {
        return fail("commit_digest");
    }
    if !sealed_matches(&commit.event, "digest") {
        return fail("event_digest");
    }
    let settlement = &commit.settlement;
    if !sealed_matches(settlement, "settlementDigest") {
        return fail("settlement_digest");
    }
    let capture =
        serde_json::to_value(&commit.capture).map_err(|_| CommitInvalid("capture_shape"))?;
    match &commit.capture {
        TerminalCapture::Observed(observed) => {
            if !sealed_matches(&capture, "captureDigest") {
                return fail("capture_digest");
            }
            if observed.observation_count != 1 {
                return fail("capture_count");
            }
        }
        TerminalCapture::Ambiguous(_) if !sealed_matches(&capture, "evidenceDigest") => {
            return fail("capture_evidence_digest")
        }
        TerminalCapture::Ambiguous(_) => {}
    }
    let closure = serde_json::to_value(&commit.capture_closure)
        .map_err(|_| CommitInvalid("closure_shape"))?;
    match &commit.capture_closure {
        CaptureClosure::Observed(_) if !sealed_matches(&closure, "closureDigest") => {
            return fail("closure_digest")
        }
        CaptureClosure::Ambiguous(_) if !sealed_matches(&closure, "evidenceDigest") => {
            return fail("closure_evidence_digest")
        }
        _ => {}
    }

    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_string();
    let digest = text(settlement, "settlementDigest");
    if commit.settlement_digest != digest {
        return fail("settlement_digest_copy");
    }
    if commit.receipt.settlement_digest != digest {
        return fail("receipt_settlement_digest");
    }
    let cursor = text(settlement, "hostCursor");
    if text(&commit.event, "hostCursor") != cursor {
        return fail("event_cursor");
    }
    if commit.receipt.host_cursor != cursor {
        return fail("receipt_cursor");
    }
    if commit.next_host_cursor == cursor {
        return fail("next_cursor_equal");
    }
    let data = &commit.event["data"];
    if data["settlement"] != *settlement {
        return fail("event_settlement_ref");
    }
    if ["requestId", "rlmChildId", "turnId"]
        .iter()
        .any(|key| data[key] != settlement[key])
    {
        return fail("event_correlation");
    }
    let binding = &commit.binding;
    if text(settlement, "requestId") != binding.request_id
        || text(settlement, "rlmChildId") != binding.rlm_child_id
        || text(settlement, "turnId") != binding.turn_id
        || text(settlement, "workflowChildId") != binding.workflow_child_id
        || text(settlement, "requestDigest") != binding.request_digest
        || text(settlement, "parentId") != binding.parent_session_id
    {
        return fail("binding_copy");
    }
    if commit.receipt.request_id != binding.request_id
        || commit.receipt.request_digest != binding.request_digest
    {
        return fail("receipt_binding");
    }
    validate_outcome(settlement)?;
    let closed = matches!(commit.capture_closure, CaptureClosure::Observed(_));
    if commit.evidence.capture_closed != closed {
        return fail("evidence_capture_closed");
    }
    let quiescent = settlement["descendantsQuiescent"] == true;
    let recorded = if quiescent {
        Quiescence::Proved
    } else {
        Quiescence::Unproved
    };
    if commit.evidence.quiescence != recorded && settlement["outcome"] != "execution_unknown" {
        // Only unknown may carry unproven quiescence.
        return fail("evidence_quiescence");
    }
    Ok(())
}

fn validate_outcome(settlement: &Value) -> Result<(), CommitInvalid> {
    let fail = |reason| Err(CommitInvalid(reason));
    let outcome = settlement["outcome"].as_str().unwrap_or_default();
    let finality = &settlement["usage"]["finality"];
    let error_code = &settlement["error"]["code"];
    if outcome == "execution_unknown" {
        if finality != "known_prefix" {
            return fail("unknown_usage_finality");
        }
        if error_code != "EXECUTION_UNKNOWN" {
            return fail("unknown_error");
        }
    } else {
        if finality != "final" {
            return fail("final_usage_required");
        }
        if settlement["descendantsQuiescent"] != true {
            return fail("quiescence_required");
        }
    }
    if outcome == "completed" {
        if settlement["cancelActuated"] != false {
            return fail("completed_cancel");
        }
        if !settlement["error"].is_null() {
            return fail("completed_error");
        }
        if settlement["result"]["kind"] != "text" {
            return fail("completed_result");
        }
    }
    if outcome == "cancelled" {
        if settlement["cancelActuated"] != true {
            return fail("cancelled_flag");
        }
        if error_code != "CANCELLED" {
            return fail("cancelled_error");
        }
    }
    Ok(())
}
