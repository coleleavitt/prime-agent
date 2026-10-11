//! The `workflow.v2.request` kernel host-request handler: the one closed
//! envelope the runtime's `rlm.workflow_v2.request` sends.
//!
//! `validate` is pure (`WORKFLOW-V2.md` §6): strict structural and
//! semantic validation, the canonical definition digest, no store, model,
//! or host effect. Every other action needs the durable controller, its
//! SQLite store, and the retained-child host (§7-§9), which this host does
//! not have, so each answers the closed `CAPABILITY_UNAVAILABLE` public
//! error — never an empty run, page, or receipt. An envelope outside the
//! closed family answers `INVALID_REQUEST` (or `INVALID_DEFINITION` for a
//! `create` whose definition fails), and one without a usable `requestId`
//! cannot be correlated, so it fails the host request itself.

use std::sync::Arc;

use pa_core::features::FeatureTelemetry;
use pa_telemetry::Properties;
use serde_json::{Value, json};

use super::schema;
use super::wire::{
    Action,
    ErrorCode,
    RequestError,
    decode_public_request,
    public_error,
    validate_result,
};

/// The host-request type the runtime sends (`rlm/workflow_v2.py`).
pub const REQUEST_TYPE: &str = "workflow.v2.request";
/// The adoption event (`pa-telemetry` catalog v4).
const TELEMETRY_EVENT: &str = "workflow_durable_request";

/// The session facts one handler reports through.
pub(crate) struct HostConfig {
    pub telemetry: Option<FeatureTelemetry>,
}

/// How one request was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `validate` of a valid definition.
    Valid,
    /// `validate` (`valid: false`) or `create` (`INVALID_DEFINITION`) of an
    /// invalid definition.
    InvalidDefinition,
    /// `INVALID_REQUEST`, or an uncorrelatable envelope.
    InvalidRequest,
    /// `CAPABILITY_UNAVAILABLE`: the action needs the durable controller.
    CapabilityUnavailable,
}

impl Outcome {
    fn wire_name(self) -> &'static str {
        match self {
            Outcome::Valid => "valid",
            Outcome::InvalidDefinition => "invalid_definition",
            Outcome::InvalidRequest => "invalid_request",
            Outcome::CapabilityUnavailable => "capability_unavailable",
        }
    }
}

/// One answered request: the closed reply (or the reason none can be
/// correlated), its action when the envelope named one, and the outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub reply: Result<Value, String>,
    pub action: Option<Action>,
    pub outcome: Outcome,
}

/// Answer one public request (no I/O).
#[must_use]
pub fn answer(request: &Value) -> Answer {
    let named = request
        .get("action")
        .and_then(Value::as_str)
        .and_then(Action::from_wire);
    match decode_public_request(request) {
        Ok(decoded) => match (decoded.action, decoded.definition) {
            (Action::Validate, Some(definition)) => Answer {
                reply: Ok(validate_result(&decoded.request_id, Ok(&definition))),
                action: Some(Action::Validate),
                outcome: Outcome::Valid,
            },
            (action, _) => Answer {
                reply: Ok(public_error(
                    &decoded.request_id,
                    ErrorCode::CapabilityUnavailable,
                    &format!(
                        "Workflow V2 {} is unavailable: this host has no durable workflow controller",
                        action.wire_name()
                    ),
                )),
                action: Some(action),
                outcome: Outcome::CapabilityUnavailable,
            },
        },
        Err(RequestError::Definition(error)) => {
            // Only a closed validate/create envelope reaches the
            // definition, so its requestId is a valid id.
            let request_id = request["requestId"].as_str().unwrap_or_default();
            let reply = if named == Some(Action::Validate) {
                validate_result(request_id, Err(&error))
            } else {
                public_error(request_id, ErrorCode::InvalidDefinition, &error.to_string())
            };
            Answer {
                reply: Ok(reply),
                action: named,
                outcome: Outcome::InvalidDefinition,
            }
        }
        Err(RequestError::Request(error)) => {
            let reply = match request
                .get("requestId")
                .and_then(Value::as_str)
                .filter(|id| schema::is_id(id))
            {
                Some(request_id) => Ok(public_error(
                    request_id,
                    ErrorCode::InvalidRequest,
                    &error.to_string(),
                )),
                None => Err(format!(
                    "workflow.v2.request is outside the closed protocol: {error}"
                )),
            };
            Answer {
                reply,
                action: named,
                outcome: Outcome::InvalidRequest,
            }
        }
    }
}

/// Handle one `workflow.v2.request`.
///
/// # Errors
///
/// Only when the envelope carries no valid `requestId` to correlate a
/// closed reply with (the runtime then reports the capability
/// unavailable).
#[tracing::instrument(name = "workflow.v2.request", skip_all)]
pub(crate) async fn handle_request(
    config: Arc<HostConfig>,
    payload: Value,
) -> anyhow::Result<Value> {
    let answer = answer(payload.get("request").unwrap_or(&Value::Null));
    record(config.telemetry.as_ref(), &answer);
    answer.reply.map_err(anyhow::Error::msg)
}

/// The answer's `tracing` record and adoption event: the action and
/// classification only, never the definition, ids, or messages.
fn record(telemetry: Option<&FeatureTelemetry>, answer: &Answer) {
    let action = answer.action.map_or("unknown", Action::wire_name);
    let outcome = answer.outcome.wire_name();
    tracing::info!(
        target: "pa_workflow",
        action,
        outcome,
        "workflow.v2.request answered"
    );
    let Some(telemetry) = telemetry else {
        return;
    };
    let mut properties = Properties::new();
    properties.set("action", json!(action));
    properties.set("outcome", json!(outcome));
    telemetry.track(TELEMETRY_EVENT, &properties);
}
