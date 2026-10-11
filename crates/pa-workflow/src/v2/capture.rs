//! Exact live terminal capture (`WORKFLOW-V2.md` §8 `TurnSettlement`; TS
//! `workflow-v2-terminal-capture.ts`, slice 3).
//!
//! A pure producer of the closed `terminalCapture` and `captureClosure`
//! values: the owned-invocation capture slot, exact UTF-8 hashing, usage
//! normalization, and closed ambiguity facts. It reads no transcript,
//! roster, preview, or session message and never selects a terminal by
//! position: one owned assistant `message_end` and one owned `agent_end`
//! are evidence; anything else is an explicit ambiguity, never a guess.
//! Every record is sealed with a digest over its RFC 8785 bytes without the
//! digest field, byte-compatible with the TS producer.

use pa_types::ai::{AssistantContentBlock, StopReason, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::wire;

/// The tools-none execution profile every V2 attempt runs.
pub const TOOLS_NONE_PROFILE: &str = "workflow-v2-tools-none-v1";
/// The largest result kept inline (bytes); larger is `too_large`.
pub const MAX_INLINE_RESULT_BYTES: usize = 262_144;
/// `sha256:` of 64 zeros: the placeholder a sealed field holds while its
/// record is hashed without it.
pub(crate) const PLACEHOLDER_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const MAX_SAFE: u64 = 9_007_199_254_740_991;

/// The exact turn every capture, closure, and settlement names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnBinding {
    pub authority_id: String,
    pub root_session_id: String,
    pub parent_session_id: String,
    pub request_id: String,
    pub request_digest: String,
    pub workflow_run_id: String,
    pub node_id: String,
    pub attempt_id: String,
    pub workflow_child_id: String,
    pub rlm_child_id: String,
    pub turn_id: String,
    pub admitted_at: String,
    pub effective_model: String,
    /// Always [`TOOLS_NONE_PROFILE`].
    pub profile: String,
    pub effective_tools_digest: String,
    pub effective_thinking_level: String,
}

/// Host usage finality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finality {
    Final,
    KnownPrefix,
}

/// Host-authoritative usage (`totalTokens` is the host's, never summed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExactUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub cost_microusd: Option<u64>,
    pub finality: Finality,
}

impl ExactUsage {
    /// Zero usage known only as a prefix (an ambiguous capture's).
    #[must_use]
    pub fn zero_known_prefix() -> ExactUsage {
        ExactUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            cost_microusd: None,
            finality: Finality::KnownPrefix,
        }
    }

    /// The same counts as a lower bound.
    #[must_use]
    pub fn as_known_prefix(self) -> ExactUsage {
        ExactUsage {
            finality: Finality::KnownPrefix,
            ..self
        }
    }
}

/// Why a terminal has no result bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoneReason {
    NoAssistant,
    ProviderError,
    Cancelled,
    Unknown,
}

/// The exact result of one owned terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ExactResult {
    Text {
        text: String,
        utf8_bytes: u64,
        sha256: String,
    },
    None {
        reason: NoneReason,
    },
    TooLarge {
        utf8_bytes: u64,
        sha256: String,
    },
}

/// The closed safe error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SafeErrorCode {
    ProviderFailed,
    AuthFailed,
    ModelUnavailable,
    Cancelled,
    ResultInvalid,
    UsageInvalid,
    ExecutionUnknown,
    InternalError,
}

/// A bounded, redacted terminal error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeError {
    pub code: SafeErrorCode,
    pub message: String,
    pub retryable: bool,
}

/// The terminal's stop reason as the capture records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStopReason {
    Stop,
    Length,
    Error,
    Aborted,
    ToolUse,
    Other,
}

impl From<StopReason> for CaptureStopReason {
    fn from(reason: StopReason) -> Self {
        match reason {
            StopReason::Stop => CaptureStopReason::Stop,
            StopReason::Length => CaptureStopReason::Length,
            StopReason::Error => CaptureStopReason::Error,
            StopReason::Aborted => CaptureStopReason::Aborted,
            StopReason::ToolUse => CaptureStopReason::ToolUse,
        }
    }
}

/// One clean owned terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedCapture {
    pub binding: TurnBinding,
    pub invocation_id: String,
    pub stop_reason: CaptureStopReason,
    pub result: ExactResult,
    pub usage: ExactUsage,
    pub observation_count: u64,
    pub capture_digest: String,
    pub observed_at: String,
    pub provider: String,
    pub model: String,
    pub safe_error: Option<SafeError>,
}

/// Why no single clean owned terminal exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbiguousCaptureReason {
    MissingTerminal,
    MultipleTerminals,
    WrongInvocation,
    WrongBinding,
    LateEvent,
    CaptureWriteFailed,
    UsageInvalid,
    ProcessLost,
    CorrelationFault,
}

/// An explicit capture ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbiguousCapture {
    pub binding: TurnBinding,
    pub invocation_id: String,
    pub reason: AmbiguousCaptureReason,
    pub usage_prefix: ExactUsage,
    pub observation_count: u64,
    pub evidence_digest: String,
}

/// The closed `terminalCapture`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalCapture {
    Observed(ObservedCapture),
    Ambiguous(AmbiguousCapture),
}

impl TerminalCapture {
    /// The binding the capture names.
    #[must_use]
    pub fn binding(&self) -> &TurnBinding {
        match self {
            TerminalCapture::Observed(capture) => &capture.binding,
            TerminalCapture::Ambiguous(capture) => &capture.binding,
        }
    }

    /// The owned invocation.
    #[must_use]
    pub fn invocation_id(&self) -> &str {
        match self {
            TerminalCapture::Observed(capture) => &capture.invocation_id,
            TerminalCapture::Ambiguous(capture) => &capture.invocation_id,
        }
    }
}

/// One clean owned `agent_end`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedClosure {
    pub binding: TurnBinding,
    pub invocation_id: String,
    /// Always true.
    pub agent_end_observed: bool,
    pub observation_count: u64,
    pub closed_at: String,
    pub closure_digest: String,
}

/// Why the invocation's closure is unproven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbiguousClosureReason {
    AgentEndMissing,
    AgentEndDuplicate,
    AgentEndWrongInvocation,
    ClosureWriteFailed,
    ProcessLost,
}

/// An explicit closure ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbiguousClosure {
    pub binding: TurnBinding,
    pub invocation_id: String,
    /// Always false.
    pub agent_end_observed: bool,
    pub reason: AmbiguousClosureReason,
    pub observation_count: u64,
    pub evidence_digest: String,
}

/// The closed `captureClosure`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureClosure {
    Observed(ObservedClosure),
    Ambiguous(AmbiguousClosure),
}

impl CaptureClosure {
    /// The binding the closure names.
    #[must_use]
    pub fn binding(&self) -> &TurnBinding {
        match self {
            CaptureClosure::Observed(closure) => &closure.binding,
            CaptureClosure::Ambiguous(closure) => &closure.binding,
        }
    }

    /// The owned invocation.
    #[must_use]
    pub fn invocation_id(&self) -> &str {
        match self {
            CaptureClosure::Observed(closure) => &closure.invocation_id,
            CaptureClosure::Ambiguous(closure) => &closure.invocation_id,
        }
    }
}

/// One owned assistant `message_end`, as the executor translates it.
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantTerminalObservation {
    pub invocation_id: String,
    pub binding: TurnBinding,
    pub stop_reason: StopReason,
    pub content: Vec<AssistantContentBlock>,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub error_message: Option<String>,
    pub observed_at: String,
}

/// One owned `agent_end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEndObservation {
    pub invocation_id: String,
    pub binding: TurnBinding,
    pub closed_at: String,
}

// ---------------------------------------------------------------------------
// Digests, text, usage.
// ---------------------------------------------------------------------------

/// `sha256:` of a value's RFC 8785 bytes.
///
/// # Errors
///
/// A non-integer number (outside the closed subset).
pub fn digest_value(value: &Value) -> Result<String, wire::WireError> {
    wire::request_digest(value)
}

/// Seal `value` (a JSON object): `field` becomes the digest of the object's
/// canonical bytes without `field`, in `field`'s own key position.
///
/// # Errors
///
/// A non-integer number.
pub fn seal(mut value: Value, field: &str) -> Result<Value, wire::WireError> {
    let digest = sealed_digest(&value, field)?;
    value[field] = Value::String(digest);
    Ok(value)
}

/// The digest `seal` would write into `field`.
///
/// # Errors
///
/// A non-integer number.
pub fn sealed_digest(value: &Value, field: &str) -> Result<String, wire::WireError> {
    let mut rest = value.clone();
    if let Some(object) = rest.as_object_mut() {
        object.shift_remove(field);
    }
    digest_value(&rest)
}

/// Serialize a typed record and seal it.
fn seal_record(record: &impl Serialize, field: &str) -> String {
    serde_json::to_value(record)
        .ok()
        .and_then(|value| sealed_digest(&value, field).ok())
        .unwrap_or_else(|| PLACEHOLDER_DIGEST.to_string())
}

/// The terminal's text blocks joined without separators (thinking and tool
/// calls excluded).
#[must_use]
pub fn terminal_text(content: &[AssistantContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            AssistantContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn has_tool_call(content: &[AssistantContentBlock]) -> bool {
    content
        .iter()
        .any(|block| matches!(block, AssistantContentBlock::ToolCall(_)))
}

/// Exact micro-USD from a cost's shortest round-trip decimal: an integer
/// only when `total * 1_000_000` is integral, nonnegative, and safe.
#[must_use]
pub fn decimal_to_microusd(total: f64) -> Option<u64> {
    if !total.is_finite() || total < 0.0 {
        return None;
    }
    if total == 0.0 {
        return Some(0); // -0 too, as `(-0).toString()` is "0"
    }
    // Rust's `Display` is the shortest round-trip decimal, never exponent
    // notation: the expansion TS performs on `Number#toString`.
    let text = format!("{total}");
    let (integer, fraction) = text.split_once('.').unwrap_or((&text, ""));
    if fraction.len() > 6 && fraction[6..].bytes().any(|digit| digit != b'0') {
        return None;
    }
    let mut micro_fraction: String = fraction.chars().take(6).collect();
    while micro_fraction.len() < 6 {
        micro_fraction.push('0');
    }
    let micro = integer
        .parse::<u128>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(micro_fraction.parse::<u128>().ok()?)?;
    u64::try_from(micro).ok().filter(|micro| *micro <= MAX_SAFE)
}

/// Exact host usage, or `None` when a counter is outside the safe integer
/// range.
#[must_use]
pub fn normalize_usage(raw: &Usage, finality: Finality) -> Option<ExactUsage> {
    let counts = [
        raw.input,
        raw.output,
        raw.cache_read,
        raw.cache_write,
        raw.total_tokens,
    ];
    if counts.iter().any(|count| *count > MAX_SAFE) {
        return None;
    }
    Some(ExactUsage {
        input_tokens: raw.input,
        output_tokens: raw.output,
        cache_read_tokens: raw.cache_read,
        cache_write_tokens: raw.cache_write,
        total_tokens: raw.total_tokens,
        cost_microusd: decimal_to_microusd(raw.cost.total.as_f64()),
        finality,
    })
}

/// Bound a message to 512 UTF-16 units and 512 UTF-8 bytes on a character
/// boundary (TS `boundText`; a split surrogate pair, which TS would keep
/// as a lone half, is dropped whole).
#[must_use]
pub fn bound_text(message: &str) -> String {
    let mut units = 0;
    let mut end = 0;
    for (index, ch) in message.char_indices() {
        units += ch.len_utf16();
        if units > 512 {
            break;
        }
        end = index + ch.len_utf8();
    }
    let mut out = &message[..end];
    while out.len() > 512 {
        let last = out.char_indices().last().map_or(0, |(index, _)| index);
        out = &out[..last];
    }
    out.to_string()
}

/// The exact result of one owned terminal.
#[must_use]
pub fn classify_result(
    stop_reason: CaptureStopReason,
    content: &[AssistantContentBlock],
    max_inline_bytes: usize,
) -> ExactResult {
    let none = |reason| ExactResult::None { reason };
    match stop_reason {
        CaptureStopReason::Error => return none(NoneReason::ProviderError),
        CaptureStopReason::Aborted => return none(NoneReason::Cancelled),
        _ => {}
    }
    if has_tool_call(content) {
        return none(NoneReason::NoAssistant);
    }
    let text = terminal_text(content);
    if text.is_empty() || stop_reason != CaptureStopReason::Stop {
        return none(NoneReason::NoAssistant);
    }
    let utf8_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let sha256 = wire::sha256_digest(text.as_bytes());
    if text.len() > max_inline_bytes {
        return ExactResult::TooLarge { utf8_bytes, sha256 };
    }
    ExactResult::Text {
        text,
        utf8_bytes,
        sha256,
    }
}

// ---------------------------------------------------------------------------
// The owned-invocation capture slot.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureFault {
    WriteFailed,
    ProcessLost,
    Correlation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosureFault {
    WriteFailed,
    ProcessLost,
}

/// At most one owned assistant terminal per turn: bound to one exact
/// `(binding, invocation)` before prompt admission, it records every
/// observed `message_end` / `agent_end`, decides ownership itself, and
/// produces the closed capture and closure.
#[derive(Debug, Clone)]
pub struct TerminalCaptureSlot {
    binding: TurnBinding,
    invocation_id: String,
    max_inline_bytes: usize,
    owned_count: u64,
    first_owned: Option<AssistantTerminalObservation>,
    wrong_invocation_count: u64,
    wrong_binding_count: u64,
    late_event_count: u64,
    agent_end_count: u64,
    agent_end_wrong_invocation: u64,
    first_close: Option<AgentEndObservation>,
    capture_fault: Option<CaptureFault>,
    closure_fault: Option<ClosureFault>,
    closed: bool,
}

impl TerminalCaptureSlot {
    /// A slot for one owned invocation of `binding`.
    #[must_use]
    pub fn new(binding: TurnBinding, invocation_id: impl Into<String>) -> TerminalCaptureSlot {
        TerminalCaptureSlot::with_inline_limit(binding, invocation_id, MAX_INLINE_RESULT_BYTES)
    }

    /// [`TerminalCaptureSlot::new`] with another inline result limit.
    #[must_use]
    pub fn with_inline_limit(
        binding: TurnBinding,
        invocation_id: impl Into<String>,
        max_inline_bytes: usize,
    ) -> TerminalCaptureSlot {
        TerminalCaptureSlot {
            binding,
            invocation_id: invocation_id.into(),
            max_inline_bytes,
            owned_count: 0,
            first_owned: None,
            wrong_invocation_count: 0,
            wrong_binding_count: 0,
            late_event_count: 0,
            agent_end_count: 0,
            agent_end_wrong_invocation: 0,
            first_close: None,
            capture_fault: None,
            closure_fault: None,
            closed: false,
        }
    }

    /// Observe one assistant `message_end`.
    pub fn observe_message_end(&mut self, observation: AssistantTerminalObservation) {
        if self.closed {
            self.late_event_count += 1;
        } else if observation.invocation_id != self.invocation_id {
            self.wrong_invocation_count += 1;
        } else if observation.binding != self.binding {
            self.wrong_binding_count += 1;
        } else {
            self.owned_count += 1;
            if self.first_owned.is_none() {
                self.first_owned = Some(observation);
            }
        }
    }

    /// Observe one `agent_end`.
    pub fn observe_agent_end(&mut self, observation: AgentEndObservation) {
        if observation.invocation_id != self.invocation_id {
            self.agent_end_wrong_invocation += 1;
            return;
        }
        self.agent_end_count += 1;
        if self.first_close.is_none() {
            self.first_close = Some(observation);
        }
        self.closed = true;
    }

    /// The capture could not be made durable.
    pub fn mark_capture_write_failed(&mut self) {
        self.capture_fault.get_or_insert(CaptureFault::WriteFailed);
    }

    /// The closure could not be made durable.
    pub fn mark_closure_write_failed(&mut self) {
        self.closure_fault.get_or_insert(ClosureFault::WriteFailed);
    }

    /// The process owning the invocation was lost.
    pub fn mark_process_lost(&mut self) {
        self.capture_fault.get_or_insert(CaptureFault::ProcessLost);
        self.closure_fault.get_or_insert(ClosureFault::ProcessLost);
    }

    /// A non-assistant or otherwise uncorrelated terminal on the owned
    /// invocation.
    pub fn mark_correlation_fault(&mut self) {
        self.capture_fault.get_or_insert(CaptureFault::Correlation);
    }

    fn ambiguous_reason(&self) -> Option<AmbiguousCaptureReason> {
        if let Some(fault) = self.capture_fault {
            return Some(match fault {
                CaptureFault::WriteFailed => AmbiguousCaptureReason::CaptureWriteFailed,
                CaptureFault::ProcessLost => AmbiguousCaptureReason::ProcessLost,
                CaptureFault::Correlation => AmbiguousCaptureReason::CorrelationFault,
            });
        }
        if self.owned_count > 1 {
            Some(AmbiguousCaptureReason::MultipleTerminals)
        } else if self.wrong_binding_count > 0 {
            Some(AmbiguousCaptureReason::WrongBinding)
        } else if self.wrong_invocation_count > 0 {
            Some(AmbiguousCaptureReason::WrongInvocation)
        } else if self.late_event_count > 0 {
            Some(AmbiguousCaptureReason::LateEvent)
        } else if self.owned_count == 0 {
            Some(AmbiguousCaptureReason::MissingTerminal)
        } else {
            None
        }
    }

    /// The closed terminal capture. Deterministic and side-effect free.
    #[must_use]
    pub fn capture(&self) -> TerminalCapture {
        let owned = match (self.ambiguous_reason(), &self.first_owned) {
            (None, Some(owned)) => owned,
            (reason, _) => {
                return self.ambiguous(reason.unwrap_or(AmbiguousCaptureReason::MissingTerminal));
            }
        };
        let Some(usage) = normalize_usage(&owned.usage, Finality::Final) else {
            return self.ambiguous(AmbiguousCaptureReason::UsageInvalid);
        };
        let stop_reason = CaptureStopReason::from(owned.stop_reason);
        let safe_error = (stop_reason == CaptureStopReason::Error).then(|| SafeError {
            code: SafeErrorCode::ProviderFailed,
            message: bound_text(
                owned
                    .error_message
                    .as_deref()
                    .filter(|message| !message.is_empty())
                    .unwrap_or("provider error"),
            ),
            retryable: false,
        });
        let mut observed = ObservedCapture {
            binding: self.binding.clone(),
            invocation_id: self.invocation_id.clone(),
            stop_reason,
            result: classify_result(stop_reason, &owned.content, self.max_inline_bytes),
            usage,
            observation_count: 1,
            capture_digest: PLACEHOLDER_DIGEST.to_string(),
            observed_at: owned.observed_at.clone(),
            provider: owned.provider.clone(),
            model: owned.model.clone(),
            safe_error,
        };
        let capture = TerminalCapture::Observed(observed.clone());
        observed.capture_digest = seal_record(&capture, "captureDigest");
        TerminalCapture::Observed(observed)
    }

    fn ambiguous(&self, reason: AmbiguousCaptureReason) -> TerminalCapture {
        let mut ambiguous = AmbiguousCapture {
            binding: self.binding.clone(),
            invocation_id: self.invocation_id.clone(),
            reason,
            usage_prefix: ExactUsage::zero_known_prefix(),
            observation_count: self.owned_count,
            evidence_digest: PLACEHOLDER_DIGEST.to_string(),
        };
        ambiguous.evidence_digest = seal_record(
            &TerminalCapture::Ambiguous(ambiguous.clone()),
            "evidenceDigest",
        );
        TerminalCapture::Ambiguous(ambiguous)
    }

    /// The closed capture closure.
    #[must_use]
    pub fn closure(&self) -> CaptureClosure {
        let reason = match self.closure_fault {
            Some(ClosureFault::WriteFailed) => Some(AmbiguousClosureReason::ClosureWriteFailed),
            Some(ClosureFault::ProcessLost) => Some(AmbiguousClosureReason::ProcessLost),
            None if self.agent_end_wrong_invocation > 0 && self.agent_end_count == 0 => {
                Some(AmbiguousClosureReason::AgentEndWrongInvocation)
            }
            None if self.agent_end_count == 0 => Some(AmbiguousClosureReason::AgentEndMissing),
            None if self.agent_end_count > 1 => Some(AmbiguousClosureReason::AgentEndDuplicate),
            None if self.agent_end_wrong_invocation > 0 => {
                Some(AmbiguousClosureReason::AgentEndWrongInvocation)
            }
            None => None,
        };
        match (reason, &self.first_close) {
            (None, Some(close)) => {
                let mut observed = ObservedClosure {
                    binding: self.binding.clone(),
                    invocation_id: self.invocation_id.clone(),
                    agent_end_observed: true,
                    observation_count: self.owned_count,
                    closed_at: close.closed_at.clone(),
                    closure_digest: PLACEHOLDER_DIGEST.to_string(),
                };
                observed.closure_digest =
                    seal_record(&CaptureClosure::Observed(observed.clone()), "closureDigest");
                CaptureClosure::Observed(observed)
            }
            (reason, _) => {
                let mut ambiguous = AmbiguousClosure {
                    binding: self.binding.clone(),
                    invocation_id: self.invocation_id.clone(),
                    agent_end_observed: false,
                    reason: reason.unwrap_or(AmbiguousClosureReason::AgentEndMissing),
                    observation_count: self.owned_count,
                    evidence_digest: PLACEHOLDER_DIGEST.to_string(),
                };
                ambiguous.evidence_digest = seal_record(
                    &CaptureClosure::Ambiguous(ambiguous.clone()),
                    "evidenceDigest",
                );
                CaptureClosure::Ambiguous(ambiguous)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micro_usd_is_exact_or_absent() {
        for (total, expected) in [
            (0.25, Some(250_000)),
            (0.123_456, Some(123_456)),
            (0.123_456_5, None),
            (1e-7, None),
            (0.0, Some(0)),
            (-0.0, Some(0)),
            (-0.5, None),
            (f64::INFINITY, None),
            (2.0, Some(2_000_000)),
            (1e21, None),
        ] {
            assert_eq!(decimal_to_microusd(total), expected, "{total}");
        }
    }

    #[test]
    fn bounded_text_counts_utf16_units_then_bytes() {
        assert_eq!(bound_text(&"x".repeat(600)).len(), 512);
        // 300 two-byte characters: 300 units, 600 bytes -> 256 characters.
        assert_eq!(bound_text(&"é".repeat(300)).chars().count(), 256);
        // 300 astral characters: two units each -> 256 characters by units,
        // then 4 bytes each -> 128 characters by bytes.
        assert_eq!(bound_text(&"😀".repeat(300)).chars().count(), 128);
    }
}
