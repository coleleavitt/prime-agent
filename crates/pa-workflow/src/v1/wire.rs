//! The Workflow V1 wire: the closed `prime.workflow.run-agent/v1` request
//! the kernel's `rlm.workflow.run_agent` sends, and the closed
//! `prime.workflow.run-agent-result/v1` reply the host settles it with.
//!
//! Both shapes are the TS host's (`workflow-v1-wire.ts`) and the runtime's
//! (`rlm/workflow.py`) exactly: the runtime re-validates every reply and
//! rejects unknown keys, mismatched digests, budget contradictions, and
//! finality that does not match the outcome.

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// The request protocol tag.
pub const REQUEST_PROTOCOL: &str = "prime.workflow.run-agent/v1";
/// The reply protocol tag.
pub const REPLY_PROTOCOL: &str = "prime.workflow.run-agent-result/v1";
/// The ceiling on `maxResultUtf8Bytes`.
pub const MAX_RESULT_UTF8_BYTES: u64 = 1_048_576;
/// `Number.MAX_SAFE_INTEGER`: the wire's integer ceiling.
pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Reply error messages are capped at this many characters.
pub(crate) const MAX_ERROR_CHARS: usize = 512;

const MAX_PROMPT_UTF16_UNITS: usize = 262_144;
const MAX_MODEL_UTF16_UNITS: usize = 512;
const MAX_SOFT_TOKEN_BUDGET: u64 = 1_000_000;
const MAX_DRAIN_TIMEOUT_MS: u64 = 30_000;
const MAX_ID_TAIL: usize = 127;
const REQUIRED_KEYS: [&str; 9] = [
    "protocol",
    "requestId",
    "nodeId",
    "prompt",
    "model",
    "maxTurns",
    "maxResultUtf8Bytes",
    "drainTimeoutMs",
    "tools",
];
const OPTIONAL_KEY: &str = "softTokenBudget";

/// A decoded, validated `workflow.run_agent` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentRequest {
    pub request_id: String,
    pub node_id: String,
    pub prompt: String,
    /// A `provider/model-id` selector; `None` runs the session model.
    pub model: Option<String>,
    pub soft_token_budget: Option<u64>,
    pub max_result_utf8_bytes: u64,
    pub drain_timeout_ms: u64,
}

/// Why a request was refused before any work started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("request must be an object")]
    NotAnObject,
    #[error("request has missing or unknown fields")]
    Fields,
    #[error("unsupported protocol, tools, or maxTurns")]
    Unsupported,
    #[error("{0} is invalid")]
    Invalid(&'static str),
}

/// Decode and validate one request (TS `decodeWorkflowRunAgentRequest`).
///
/// # Errors
///
/// Returns the first violated rule: a non-object, a missing or unknown key,
/// another protocol, tools policy, or turn count, or an out-of-range field.
pub fn decode_request(value: &Value) -> Result<RunAgentRequest, RequestError> {
    let object = value.as_object().ok_or(RequestError::NotAnObject)?;
    if !REQUIRED_KEYS.iter().all(|key| object.contains_key(*key))
        || object
            .keys()
            .any(|key| key != OPTIONAL_KEY && !REQUIRED_KEYS.contains(&key.as_str()))
    {
        return Err(RequestError::Fields);
    }
    if object["protocol"] != REQUEST_PROTOCOL
        || object["tools"] != "none"
        || object["maxTurns"].as_u64() != Some(1)
    {
        return Err(RequestError::Unsupported);
    }
    let request_id = id(&object["requestId"], "requestId")?;
    let node_id = id(&object["nodeId"], "nodeId")?;
    let model = match &object["model"] {
        Value::Null => None,
        Value::String(model) if is_model_selector(model) => Some(model.clone()),
        _ => return Err(RequestError::Invalid("model")),
    };
    let prompt = match &object["prompt"] {
        Value::String(prompt)
            if (1..=MAX_PROMPT_UTF16_UNITS).contains(&prompt.encode_utf16().count()) =>
        {
            prompt.clone()
        }
        _ => return Err(RequestError::Invalid("prompt")),
    };
    let soft_token_budget = match object.get(OPTIONAL_KEY) {
        None | Some(Value::Null) => None,
        Some(value) => Some(integer(value, "softTokenBudget", 1, MAX_SOFT_TOKEN_BUDGET)?),
    };
    Ok(RunAgentRequest {
        request_id,
        node_id,
        prompt,
        model,
        soft_token_budget,
        max_result_utf8_bytes: integer(
            &object["maxResultUtf8Bytes"],
            "maxResultUtf8Bytes",
            1,
            MAX_RESULT_UTF8_BYTES,
        )?,
        drain_timeout_ms: integer(
            &object["drainTimeoutMs"],
            "drainTimeoutMs",
            1,
            MAX_DRAIN_TIMEOUT_MS,
        )?,
    })
}

/// `^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`.
fn id(value: &Value, name: &'static str) -> Result<String, RequestError> {
    let valid = value.as_str().filter(|text| {
        let mut chars = text.chars();
        chars
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
            && text.len() <= MAX_ID_TAIL + 1
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    });
    valid.map(str::to_string).ok_or(RequestError::Invalid(name))
}

/// `^\S+/\S+$`, at most 512 UTF-16 units.
fn is_model_selector(text: &str) -> bool {
    text.encode_utf16().count() <= MAX_MODEL_UTF16_UNITS
        && !text.chars().any(char::is_whitespace)
        && text
            .find('/')
            .is_some_and(|slash| slash > 0 && slash + 1 < text.len())
}

fn integer(value: &Value, name: &'static str, low: u64, high: u64) -> Result<u64, RequestError> {
    value
        .as_u64()
        .filter(|number| (low..=high).contains(number))
        .ok_or(RequestError::Invalid(name))
}

/// Whether a usage observation is the settled total or a known prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Finality {
    Final,
    KnownPrefix,
}

/// The host-observed usage of the one turn.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,
    pub cost_total: Option<f64>,
    pub completeness: Completeness,
    pub finality: Finality,
}

/// The only completeness the V1 host reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    CompleteHostObservation,
}

/// What a zero usage reports for its costs: the runner's zero is a
/// measured zero, a run that never reached the runner has no cost at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroCost {
    Measured,
    Unobserved,
}

impl Usage {
    #[must_use]
    pub fn zero(finality: Finality, cost: ZeroCost) -> Self {
        let cost = match cost {
            ZeroCost::Measured => Some(0.0),
            ZeroCost::Unobserved => None,
        };
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            cost_input: cost,
            cost_output: cost,
            cost_cache_read: cost,
            cost_cache_write: cost,
            cost_total: cost,
            completeness: Completeness::CompleteHostObservation,
            finality,
        }
    }

    #[must_use]
    pub fn with_finality(self, finality: Finality) -> Self {
        Self { finality, ..self }
    }
}

/// The completed result: the assistant text parts joined without a
/// separator, its UTF-8 size, and its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultText {
    pub text: String,
    pub utf8_bytes: u64,
    pub sha256: String,
}

impl ResultText {
    #[must_use]
    pub fn new(text: String) -> Self {
        let digest = Sha256::digest(text.as_bytes());
        Self {
            utf8_bytes: text.len() as u64,
            sha256: digest.iter().fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
            text,
        }
    }
}

/// Why a run failed (each has a fixed error code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    ModelResolutionFailed,
    ProviderFailed,
    ResultMissing,
    ResultTooLarge,
    UsageInvalid,
    UnexpectedToolCall,
    HostFailed,
}

impl FailureReason {
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::ModelResolutionFailed => "model_resolution_failed",
            Self::ProviderFailed => "provider_failed",
            Self::ResultMissing => "result_missing",
            Self::ResultTooLarge => "result_too_large",
            Self::UsageInvalid => "usage_invalid",
            Self::UnexpectedToolCall => "unexpected_tool_call",
            Self::HostFailed => "host_failed",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::ModelResolutionFailed => "MODEL_RESOLUTION_FAILED",
            Self::ProviderFailed => "PROVIDER_FAILED",
            Self::ResultMissing => "RESULT_MISSING",
            Self::ResultTooLarge => "RESULT_TOO_LARGE",
            Self::UsageInvalid => "USAGE_INVALID",
            Self::UnexpectedToolCall => "UNEXPECTED_TOOL_CALL",
            Self::HostFailed => "HOST_FAILED",
        }
    }
}

/// Why the host cannot say whether the provider turn ran to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    /// The turn did not settle within `drainTimeoutMs` after cancellation.
    DrainTimeout,
    /// More than one terminal assistant message was observed.
    TerminalCaptureAmbiguous,
}

impl UnknownReason {
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::DrainTimeout => "drain_timeout",
            Self::TerminalCaptureAmbiguous => "terminal_capture_ambiguous",
        }
    }
}

/// How one run settled.
#[derive(Debug, Clone, PartialEq)]
pub enum Terminal {
    Completed(ResultText),
    Failed {
        reason: FailureReason,
        message: String,
    },
    Cancelled,
    Unknown(UnknownReason),
}

impl Terminal {
    /// A failure with its message capped at [`MAX_ERROR_CHARS`].
    #[must_use]
    pub fn failed(reason: FailureReason, message: &str) -> Self {
        Self::Failed {
            reason,
            message: message.chars().take(MAX_ERROR_CHARS).collect(),
        }
    }

    #[must_use]
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Completed(_) => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown(_) => "execution_unknown",
        }
    }

    #[must_use]
    pub fn stop_reason(&self) -> &'static str {
        match self {
            Self::Completed(_) => "completed",
            Self::Failed { reason, .. } => reason.wire_name(),
            Self::Cancelled => "caller_aborted",
            Self::Unknown(reason) => reason.wire_name(),
        }
    }
}

/// Everything a reply reports beyond the request's correlation.
#[derive(Debug, Clone, PartialEq)]
pub struct Settlement {
    pub resolved_model: Option<String>,
    pub turns_started: u8,
    pub duration_ms: u64,
    pub usage: Usage,
    pub terminal: Terminal,
}

/// The soft-budget verdict: exhausted at or past the budget, and by how much.
#[must_use]
pub fn budget(request: &RunAgentRequest, total_tokens: u64) -> (bool, u64) {
    request.soft_token_budget.map_or((false, 0), |budget| {
        (total_tokens >= budget, total_tokens.saturating_sub(budget))
    })
}

/// Build the closed reply object, keys in the TS host's order.
#[must_use]
pub fn reply(request: &RunAgentRequest, settlement: &Settlement) -> Value {
    let (budget_exhausted, budget_overshoot_tokens) =
        budget(request, settlement.usage.total_tokens);
    let (result, error) = match &settlement.terminal {
        Terminal::Completed(result) => (json!(result), Value::Null),
        Terminal::Failed { reason, message } => (
            Value::Null,
            json!({ "code": reason.code(), "message": message }),
        ),
        Terminal::Cancelled => (Value::Null, Value::Null),
        Terminal::Unknown(_) => (
            Value::Null,
            json!({ "code": "EXECUTION_UNKNOWN", "message": "EXECUTION_UNKNOWN" }),
        ),
    };
    let mut object = Map::new();
    object.insert("protocol".into(), json!(REPLY_PROTOCOL));
    object.insert("requestId".into(), json!(request.request_id));
    object.insert("nodeId".into(), json!(request.node_id));
    object.insert("resolvedModel".into(), json!(settlement.resolved_model));
    object.insert("turnsStarted".into(), json!(settlement.turns_started));
    object.insert("durationMs".into(), json!(settlement.duration_ms));
    object.insert("budgetExhausted".into(), json!(budget_exhausted));
    object.insert(
        "budgetOvershootTokens".into(),
        json!(budget_overshoot_tokens),
    );
    object.insert("usage".into(), json!(settlement.usage));
    object.insert("outcome".into(), json!(settlement.terminal.outcome()));
    object.insert(
        "stopReason".into(),
        json!(settlement.terminal.stop_reason()),
    );
    object.insert("result".into(), result);
    object.insert("error".into(), error);
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_value() -> Value {
        json!({
            "protocol": REQUEST_PROTOCOL,
            "requestId": "req-1",
            "nodeId": "node-1",
            "prompt": "hi",
            "model": null,
            "maxTurns": 1,
            "maxResultUtf8Bytes": 100,
            "drainTimeoutMs": 100,
            "tools": "none",
        })
    }

    fn decoded() -> RunAgentRequest {
        RunAgentRequest {
            request_id: "req-1".to_string(),
            node_id: "node-1".to_string(),
            prompt: "hi".to_string(),
            model: None,
            soft_token_budget: None,
            max_result_utf8_bytes: 100,
            drain_timeout_ms: 100,
        }
    }

    fn with(key: &str, value: Value) -> Value {
        let mut request = request_value();
        request[key] = value;
        request
    }

    #[test]
    fn a_closed_request_decodes() {
        assert_eq!(decode_request(&request_value()), Ok(decoded()));
        let mut budgeted = with("softTokenBudget", json!(5));
        budgeted["model"] = json!("prime-inference/z-ai/glm-5.3");
        assert_eq!(
            decode_request(&budgeted),
            Ok(RunAgentRequest {
                model: Some("prime-inference/z-ai/glm-5.3".to_string()),
                soft_token_budget: Some(5),
                ..decoded()
            })
        );
        assert_eq!(
            decode_request(&with("softTokenBudget", Value::Null)),
            Ok(decoded())
        );
    }

    /// The TS host's fail-closed cases: old or mixed protocols, unknown
    /// keys, and every out-of-range field refuse the request.
    #[test]
    fn every_contract_violation_is_refused() {
        let mut missing = request_value();
        missing.as_object_mut().unwrap().remove("tools");
        let cases = [
            (json!([]), RequestError::NotAnObject),
            (missing, RequestError::Fields),
            (
                with("legacyProtocol", json!("prime.workflow.run-agent/v0")),
                RequestError::Fields,
            ),
            (
                with("protocol", json!("prime.workflow.run-agent/v0")),
                RequestError::Unsupported,
            ),
            (with("tools", json!("all")), RequestError::Unsupported),
            (with("maxTurns", json!(2)), RequestError::Unsupported),
            (with("maxTurns", json!(true)), RequestError::Unsupported),
            (
                with("requestId", json!("-x")),
                RequestError::Invalid("requestId"),
            ),
            (
                with("nodeId", json!("n".repeat(129))),
                RequestError::Invalid("nodeId"),
            ),
            (
                with("model", json!("no-slash")),
                RequestError::Invalid("model"),
            ),
            (with("model", json!("a /b")), RequestError::Invalid("model")),
            (with("model", json!(7)), RequestError::Invalid("model")),
            (with("prompt", json!("")), RequestError::Invalid("prompt")),
            (
                with("prompt", json!("x".repeat(262_145))),
                RequestError::Invalid("prompt"),
            ),
            (
                with("softTokenBudget", json!(0)),
                RequestError::Invalid("softTokenBudget"),
            ),
            (
                with("maxResultUtf8Bytes", json!(1_048_577)),
                RequestError::Invalid("maxResultUtf8Bytes"),
            ),
            (
                with("maxResultUtf8Bytes", json!(1.5)),
                RequestError::Invalid("maxResultUtf8Bytes"),
            ),
            (
                with("drainTimeoutMs", json!(30_001)),
                RequestError::Invalid("drainTimeoutMs"),
            ),
            (
                with("drainTimeoutMs", json!(false)),
                RequestError::Invalid("drainTimeoutMs"),
            ),
        ];
        for (request, expected) in cases {
            assert_eq!(decode_request(&request), Err(expected), "{request}");
        }
    }

    #[test]
    fn the_result_digest_is_over_the_utf8_bytes() {
        assert_eq!(
            ResultText::new("héllo".to_string()),
            ResultText {
                text: "héllo".to_string(),
                utf8_bytes: 6,
                sha256: "3c48591d8d098a4538f5e013dfcf406e948eac4d3277b10bf614e295d6068179"
                    .to_string(),
            }
        );
    }

    #[test]
    fn a_completed_reply_is_the_closed_object() {
        let settlement = Settlement {
            resolved_model: Some("p/m".to_string()),
            turns_started: 1,
            duration_ms: 3,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                ..Usage::zero(Finality::Final, ZeroCost::Unobserved)
            },
            terminal: Terminal::Completed(ResultText::new("ok".to_string())),
        };
        assert_eq!(
            reply(&decoded(), &settlement),
            json!({
                "protocol": REPLY_PROTOCOL,
                "requestId": "req-1",
                "nodeId": "node-1",
                "resolvedModel": "p/m",
                "turnsStarted": 1,
                "durationMs": 3,
                "budgetExhausted": false,
                "budgetOvershootTokens": 0,
                "usage": {
                    "inputTokens": 1, "outputTokens": 1, "cacheReadTokens": 0,
                    "cacheWriteTokens": 0, "totalTokens": 2, "costInput": null,
                    "costOutput": null, "costCacheRead": null, "costCacheWrite": null,
                    "costTotal": null, "completeness": "complete_host_observation",
                    "finality": "final"
                },
                "outcome": "completed",
                "stopReason": "completed",
                "result": {
                    "text": "ok",
                    "utf8Bytes": 2,
                    "sha256": "2689367b205c16ce32ed4200942b8b8b1e262dfc70d9bc9fbc77c49699a4f1df"
                },
                "error": null
            })
        );
    }

    #[test]
    fn failure_and_unknown_replies_carry_their_codes_and_the_budget() {
        let request = RunAgentRequest {
            soft_token_budget: Some(1),
            ..decoded()
        };
        let usage = Usage {
            total_tokens: 3,
            ..Usage::zero(Finality::Final, ZeroCost::Measured)
        };
        let failed = reply(
            &request,
            &Settlement {
                resolved_model: None,
                turns_started: 0,
                duration_ms: 0,
                usage,
                terminal: Terminal::failed(FailureReason::ModelResolutionFailed, &"x".repeat(600)),
            },
        );
        assert_eq!(
            (
                &failed["budgetExhausted"],
                &failed["budgetOvershootTokens"],
                &failed["outcome"],
                &failed["stopReason"],
                &failed["error"],
            ),
            (
                &json!(true),
                &json!(2),
                &json!("failed"),
                &json!("model_resolution_failed"),
                &json!({ "code": "MODEL_RESOLUTION_FAILED", "message": "x".repeat(512) }),
            )
        );
        let unknown = reply(
            &request,
            &Settlement {
                resolved_model: Some("p/m".to_string()),
                turns_started: 1,
                duration_ms: 0,
                usage: usage.with_finality(Finality::KnownPrefix),
                terminal: Terminal::Unknown(UnknownReason::DrainTimeout),
            },
        );
        assert_eq!(
            (
                &unknown["stopReason"],
                &unknown["error"],
                &unknown["usage"]["finality"]
            ),
            (
                &json!("drain_timeout"),
                &json!({ "code": "EXECUTION_UNKNOWN", "message": "EXECUTION_UNKNOWN" }),
                &json!("known_prefix"),
            )
        );
    }
}
