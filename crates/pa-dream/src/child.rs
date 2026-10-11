//! The child-agent seam of the in-session LLM path (TS `run-agent.ts`'s
//! `RunAgentHandler` and `ravo/runtime-adapter.ts`'s structured child call).
//!
//! [`RunAgent`] runs one tool-less child agent to a terminal result. It is
//! synchronous: the whole Dream-RSI run executes on a blocking thread, and the
//! session's implementation (`crate::session::AgentRunAgent`) bridges into the
//! async provider stream itself. Tests supply scripted runners; nothing here
//! spends a token.
//!
//! [`run_structured_child`] is one child call classified for the proposer and
//! the dreamer: a non-completed child maps by status, a completed one must
//! yield a JSON value of the requested container ([`extract_json_value`]:
//! fences and prose tolerated) that the caller's validator accepts. A result
//! whose final message stopped at its output cap is a `length` rejection
//! whatever the parser said, since the cap is the cause.

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::json;
use crate::proposer::ProposalRejectReason;
use crate::rejections::{REJECTION_EXCERPT_CHARS, excerpt_of};

/// Per-attempt child token budget when a caller does not set one.
pub const DEFAULT_CHILD_TOKEN_BUDGET: u64 = 200_000;
/// Child results one call examines at most: the first plus this many retries.
pub const PROPOSER_RETRIES: u32 = 1;

/// Output tokens reserved for thinking at each level when a visible-answer cap
/// is in force (TS `THINKING_ALLOWANCE`); `off` (or unknown) adds nothing.
#[must_use]
pub fn thinking_allowance(level: &str) -> u64 {
    match level {
        "minimal" => 1024,
        "low" => 2048,
        "medium" => 8192,
        "high" | "xhigh" | "max" => 16_384,
        _ => 0,
    }
}

/// The stream `max_tokens` a visible-answer cap allows at a thinking level
/// (TS `cappedMaxTokens`).
#[must_use]
pub fn capped_max_tokens(cap: u64, reasoning: Option<&str>) -> u64 {
    cap + reasoning.map_or(0, thinking_allowance)
}

/// A child's terminal status (TS `RunAgentStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunAgentStatus {
    Completed,
    Aborted,
    TurnLimit,
    BudgetExceeded,
    Error,
}

impl RunAgentStatus {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::TurnLimit => "turn_limit",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Error => "error",
        }
    }

    /// Parse a wire literal.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "completed" => Self::Completed,
            "aborted" => Self::Aborted,
            "turn_limit" => Self::TurnLimit,
            "budget_exceeded" => Self::BudgetExceeded,
            "error" => Self::Error,
            _ => return None,
        })
    }

    /// The reject reason of a non-completed child (TS `statusRejectReason`).
    #[must_use]
    pub fn reject_reason(self) -> ProposalRejectReason {
        match self {
            Self::Aborted => ProposalRejectReason::Aborted,
            Self::TurnLimit => ProposalRejectReason::TurnLimit,
            Self::BudgetExceeded => ProposalRejectReason::Budget,
            Self::Completed | Self::Error => ProposalRejectReason::Error,
        }
    }
}

/// What a child is asked (TS `RunAgentRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentRequest {
    pub prompt: String,
    /// Model selector (`provider/id`); the session model when `None`.
    pub model: Option<String>,
    /// Thinking level (`off`, `minimal`, ...); the session's when `None`.
    pub thinking_level: Option<String>,
}

/// How a child runs (TS `RunAgentOptions`; tools are always `none`).
#[derive(Debug, Clone)]
pub struct RunAgentOptions {
    /// Stop after this many completed model turns that would continue.
    pub max_turns: Option<u32>,
    /// Stop continuing once assistant usage reaches this many total tokens.
    pub token_budget: u64,
    /// Visible-answer cap per model call, in output tokens.
    pub max_output_tokens: Option<u64>,
    /// Cancels the child without touching the parent session.
    pub cancel: CancellationToken,
}

/// A child's terminal result (TS `RunAgentResult`, the fields Dream reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentResult {
    pub status: RunAgentStatus,
    /// The final assistant message's text.
    pub output: String,
    /// The final assistant message's stop reason (`stop`, `length`, ...), when any.
    pub stop_reason: Option<String>,
    pub total_tokens: u64,
    pub output_tokens: u64,
    pub error: Option<String>,
}

impl RunAgentResult {
    /// A result with no output and no usage.
    #[must_use]
    pub fn empty(status: RunAgentStatus) -> Self {
        Self {
            status,
            output: String::new(),
            stop_reason: None,
            total_tokens: 0,
            output_tokens: 0,
            error: None,
        }
    }
}

/// Runs one tool-less child agent to a terminal result.
pub trait RunAgent: Send + Sync {
    /// Run `request` under `options`. Never panics on a provider failure: a
    /// failed child is a result with status `error`.
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult;
}

impl<F> RunAgent for F
where
    F: Fn(&RunAgentRequest, &RunAgentOptions) -> RunAgentResult + Send + Sync,
{
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult {
        self(request, options)
    }
}

/// The runtime scope every child of a run shares (TS `ChildRuntimeScope`;
/// tools are always `none`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildRuntimeScope {
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub token_budget: Option<u64>,
    pub thinking_level: Option<String>,
    pub max_output_tokens: Option<u64>,
}

impl ChildRuntimeScope {
    /// The request a structured child is prompted with (TS `structuredChildRequest`).
    #[must_use]
    pub fn request(&self, prompt: String) -> RunAgentRequest {
        RunAgentRequest {
            prompt,
            model: self.model.clone().filter(|model| !model.is_empty()),
            thinking_level: self.thinking_level.clone(),
        }
    }

    /// The options a structured child runs under: the scope's caps and the
    /// smaller of the scope's and the allocated budget (TS `runOptions`).
    #[must_use]
    pub fn options(&self, cancel: &CancellationToken, allocated: u64) -> RunAgentOptions {
        RunAgentOptions {
            max_turns: self.max_turns,
            token_budget: self
                .token_budget
                .map_or(allocated, |configured| configured.min(allocated)),
            max_output_tokens: self.max_output_tokens,
            cancel: cancel.clone(),
        }
    }
}

/// The top-level JSON container a lenient extraction looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonContainer {
    Object,
    Array,
}

impl JsonContainer {
    fn wants(self, value: &Value) -> bool {
        match self {
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
        }
    }

    fn opener(self) -> u8 {
        match self {
            Self::Object => b'{',
            Self::Array => b'[',
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
        }
    }
}

/// Byte index just past the bracket balancing the one at `start`, skipping
/// brackets inside strings; `None` when unbalanced.
fn balanced_json_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0_i64;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `String.prototype.trim`: Unicode white space plus the BOM.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Lenient JSON extraction (TS `extractJsonValue`): the whole output is tried
/// first; otherwise every opener is a candidate start, its balanced close is
/// found by a string-aware scan, and the LARGEST candidate (in UTF-16 units)
/// that parses to the requested container wins, so a fragment quoted in an
/// explanation never shadows the answer and a nested value never shadows its
/// parent.
///
/// # Errors
///
/// `child output contains no JSON <container>` when none parses.
pub fn extract_json_value(output: &str, container: JsonContainer) -> Result<Value, String> {
    let text = js_trim(output);
    if let Ok(whole) = json::parse(text) {
        if container.wants(&whole) {
            return Ok(whole);
        }
    }
    let bytes = text.as_bytes();
    let opener = container.opener();
    let find = |from: usize| {
        bytes
            .get(from..)
            .and_then(|rest| rest.iter().position(|&byte| byte == opener))
            .map(|offset| from + offset)
    };
    let mut best: Option<(Value, usize)> = None;
    let mut start = find(0);
    while let Some(at) = start {
        let mut next = at + 1;
        if let Some(end) = balanced_json_end(bytes, at) {
            // Brackets are ASCII, so both ends sit on char boundaries.
            let slice = &text[at..end];
            if let Ok(value) = json::parse(slice) {
                let length = slice.encode_utf16().count();
                if container.wants(&value) && best.as_ref().is_none_or(|(_, best)| length > *best) {
                    best = Some((value, length));
                }
                next = end;
            }
        }
        start = find(next);
    }
    best.map(|(value, _)| value)
        .ok_or_else(|| format!("child output contains no JSON {}", container.name()))
}

/// Why a validator refused a parsed value (TS `TypeError` vs any other error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// A structural refusal: wrong keys, wrong length, non-numeric entries.
    Shape(String),
    /// Any other refusal.
    Invalid(String),
}

/// What became of one child result before it could be used.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildOutcome<T> {
    Accepted {
        value: T,
        tokens: u64,
        output_tokens: u64,
    },
    Rejected(ChildRejection),
}

/// A refused child result, with everything the rejection log records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRejection {
    pub reason: ProposalRejectReason,
    pub child_status: RunAgentStatus,
    pub tokens: u64,
    pub output_tokens: u64,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    pub excerpt: String,
}

impl<T> ChildOutcome<T> {
    /// Total tokens the child spent.
    pub fn tokens(&self) -> u64 {
        match self {
            Self::Accepted { tokens, .. } => *tokens,
            Self::Rejected(rejection) => rejection.tokens,
        }
    }

    /// Output tokens the child spent.
    pub fn output_tokens(&self) -> u64 {
        match self {
            Self::Accepted { output_tokens, .. } => *output_tokens,
            Self::Rejected(rejection) => rejection.output_tokens,
        }
    }
}

/// Rejections worth one more child call (TS `RETRYABLE_REJECTIONS`): a cap
/// overrun would most likely run away again, and a turn limit, budget or
/// abort is terminal for the call by definition.
#[must_use]
pub fn retryable(reason: ProposalRejectReason) -> bool {
    matches!(
        reason,
        ProposalRejectReason::Error
            | ProposalRejectReason::Parse
            | ProposalRejectReason::Shape
            | ProposalRejectReason::InvalidCandidate
    )
}

/// One structured child call, classified (TS `runStructuredChild`).
pub fn run_structured_child<T>(
    runner: &dyn RunAgent,
    request: &RunAgentRequest,
    options: &RunAgentOptions,
    container: JsonContainer,
    mut validate: impl FnMut(&Value) -> Result<T, ValidationError>,
) -> ChildOutcome<T> {
    let result = runner.run(request, options);
    let rejected = |reason: ProposalRejectReason, error: Option<String>| {
        let capped = result.stop_reason.as_deref() == Some("length")
            && result.status == RunAgentStatus::Completed;
        ChildOutcome::Rejected(ChildRejection {
            reason: if capped {
                ProposalRejectReason::Length
            } else {
                reason
            },
            child_status: result.status,
            tokens: result.total_tokens,
            output_tokens: result.output_tokens,
            stop_reason: result.stop_reason.clone(),
            error,
            excerpt: excerpt_of(&result.output, REJECTION_EXCERPT_CHARS),
        })
    };
    if result.status != RunAgentStatus::Completed {
        return rejected(result.status.reject_reason(), result.error.clone());
    }
    let value = match extract_json_value(&result.output, container) {
        Ok(value) => value,
        Err(error) => return rejected(ProposalRejectReason::Parse, Some(error)),
    };
    match validate(&value) {
        Ok(value) => ChildOutcome::Accepted {
            value,
            tokens: result.total_tokens,
            output_tokens: result.output_tokens,
        },
        Err(ValidationError::Shape(error)) => rejected(ProposalRejectReason::Shape, Some(error)),
        Err(ValidationError::Invalid(error)) => {
            rejected(ProposalRejectReason::InvalidCandidate, Some(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn extraction_tolerates_fences_and_prose_and_prefers_the_largest_value() {
        let wrapped = "Here is my candidate, moving mass from [1, 2]:\n```json\n{\"n\": 4, \"weights\": [3, 1, 1, 3]}\n```\nThis lowers the peak {see the hint}.";
        assert_eq!(
            extract_json_value(wrapped, JsonContainer::Object).unwrap(),
            json!({"n": 4, "weights": [3, 1, 1, 3]})
        );
        let shadow = "Compared with {\"n\": 4} the shape below is flatter:\n{\"n\": 4, \"weights\": [2.5, 1.5, 1.5, 2.5]}\nDone.";
        assert_eq!(
            extract_json_value(shadow, JsonContainer::Object).unwrap(),
            json!({"n": 4, "weights": [2.5, 1.5, 1.5, 2.5]})
        );
        // The larger value wins wherever it sits, never just the last one.
        assert_eq!(
            extract_json_value(
                "{\"n\": 4, \"weights\": [1, 2, 2, 1]} beats {\"n\": 4}",
                JsonContainer::Object
            )
            .unwrap(),
            json!({"n": 4, "weights": [1, 2, 2, 1]})
        );
        assert_eq!(
            extract_json_value("  [1, {\"a\": \"]\"}]  ", JsonContainer::Array).unwrap(),
            json!([1, {"a": "]"}])
        );
        // A bare object is not an array, and a truncated object is nothing.
        assert_eq!(
            extract_json_value("{\"a\": 1}", JsonContainer::Array),
            Err("child output contains no JSON array".to_string())
        );
        assert_eq!(
            extract_json_value("{\"n\": 4, \"weights\": [1, 2,", JsonContainer::Object),
            Err("child output contains no JSON object".to_string())
        );
    }

    #[test]
    fn the_cap_adds_the_thinking_allowance() {
        assert_eq!(capped_max_tokens(4096, None), 4096);
        assert_eq!(capped_max_tokens(4096, Some("off")), 4096);
        assert_eq!(capped_max_tokens(4096, Some("medium")), 4096 + 8192);
        assert_eq!(capped_max_tokens(100, Some("max")), 100 + 16_384);
    }

    #[test]
    fn the_scope_bounds_the_budget_and_carries_the_knobs() {
        let scope = ChildRuntimeScope {
            model: Some("faux/stub".into()),
            max_turns: Some(8),
            token_budget: Some(500),
            thinking_level: Some("off".into()),
            max_output_tokens: Some(4096),
        };
        let cancel = CancellationToken::new();
        let options = scope.options(&cancel, 200_000);
        assert_eq!(
            (
                options.max_turns,
                options.token_budget,
                options.max_output_tokens
            ),
            (Some(8), 500, Some(4096))
        );
        assert_eq!(
            scope.request("p".into()),
            RunAgentRequest {
                prompt: "p".into(),
                model: Some("faux/stub".into()),
                thinking_level: Some("off".into()),
            }
        );
        assert_eq!(
            ChildRuntimeScope::default()
                .options(&cancel, 7)
                .token_budget,
            7
        );
    }

    fn answer<'a>(
        status: RunAgentStatus,
        output: &'a str,
        stop: Option<&'a str>,
    ) -> impl RunAgent + use<'a> {
        move |_: &RunAgentRequest, _: &RunAgentOptions| RunAgentResult {
            status,
            output: output.to_string(),
            stop_reason: stop.map(str::to_string),
            total_tokens: 30,
            output_tokens: 10,
            error: (status == RunAgentStatus::Error).then(|| "boom".to_string()),
        }
    }

    fn classify(runner: &dyn RunAgent) -> ChildOutcome<Value> {
        run_structured_child(
            runner,
            &ChildRuntimeScope::default().request("p".into()),
            &ChildRuntimeScope::default().options(&CancellationToken::new(), 100),
            JsonContainer::Object,
            |value| {
                if value.get("ok").is_some() {
                    Ok(value.clone())
                } else {
                    Err(ValidationError::Shape("missing ok".into()))
                }
            },
        )
    }

    fn reason_of(outcome: &ChildOutcome<Value>) -> Option<ProposalRejectReason> {
        match outcome {
            ChildOutcome::Accepted { .. } => None,
            ChildOutcome::Rejected(rejection) => Some(rejection.reason),
        }
    }

    #[test]
    fn a_child_result_is_classified_by_status_parse_shape_and_cap() {
        let ok = classify(&answer(RunAgentStatus::Completed, "{\"ok\":1}", None));
        assert_eq!(
            ok,
            ChildOutcome::Accepted {
                value: json!({"ok": 1}),
                tokens: 30,
                output_tokens: 10
            }
        );
        let cases = [
            (
                RunAgentStatus::Completed,
                "no json",
                None,
                ProposalRejectReason::Parse,
            ),
            (
                RunAgentStatus::Completed,
                "{\"x\":1}",
                None,
                ProposalRejectReason::Shape,
            ),
            // The cap is the cause whatever the parser said.
            (
                RunAgentStatus::Completed,
                "{\"x\":",
                Some("length"),
                ProposalRejectReason::Length,
            ),
            (RunAgentStatus::Error, "", None, ProposalRejectReason::Error),
            (
                RunAgentStatus::TurnLimit,
                "",
                Some("length"),
                ProposalRejectReason::TurnLimit,
            ),
            (
                RunAgentStatus::BudgetExceeded,
                "",
                None,
                ProposalRejectReason::Budget,
            ),
            (
                RunAgentStatus::Aborted,
                "",
                None,
                ProposalRejectReason::Aborted,
            ),
        ];
        for (status, output, stop, reason) in cases {
            assert_eq!(
                reason_of(&classify(&answer(status, output, stop))),
                Some(reason),
                "{status:?} {output}"
            );
        }
        let ChildOutcome::Rejected(rejection) =
            classify(&answer(RunAgentStatus::Error, "partial", None))
        else {
            panic!("an errored child is rejected");
        };
        assert_eq!(
            rejection,
            ChildRejection {
                reason: ProposalRejectReason::Error,
                child_status: RunAgentStatus::Error,
                tokens: 30,
                output_tokens: 10,
                stop_reason: None,
                error: Some("boom".into()),
                excerpt: "partial".into(),
            }
        );
    }
}
