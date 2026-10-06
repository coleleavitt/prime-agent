//! The low-level agent loop.
//!
//! The loop works with [`types::AgentMessage`] throughout and converts to LLM-bound
//! [`types::Message`] values only at the model call boundary.

use std::sync::Arc;

use crate::abort::AbortSignal;
use crate::types::{
    AfterToolCallContext, AfterToolCallResult, AgentEvent, AgentMessage, BeforeToolCallContext,
    BeforeToolCallResult, GetContinuationMessagesContext, Message, Model,
    ShouldStopAfterTurnContext, ThinkingLevel, ToolExecutionMode,
};

/// Sink receiving the loop's events. The loop awaits every emission, so
/// listeners see events strictly in order.
pub type AgentEventSink =
    Arc<dyn Fn(AgentEvent) -> crate::BoxFut<'static, anyhow::Result<()>> + Send + Sync>;

/// `convertToLlm`: converts `AgentMessage`s to LLM-compatible `Message`s
/// before each call. Must not fail; a returned `Err` interrupts the loop like
/// a `throw` in the TS reference.
pub type ConvertToLlmFn = Arc<
    dyn Fn(Vec<AgentMessage>) -> crate::BoxFut<'static, anyhow::Result<Vec<Message>>> + Send + Sync,
>;

/// AgentMessage-level transform applied before `convert_to_llm` (context
/// pruning, external injection).
pub type TransformContextFn = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>>
        + Send
        + Sync,
>;

/// Resolves the system prompt immediately before each LLM call.
pub type GetSystemPromptFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Resolves an API key dynamically for each LLM call.
pub type GetApiKeyFn =
    Arc<dyn Fn(String) -> crate::BoxFut<'static, anyhow::Result<Option<String>>> + Send + Sync>;

/// Called after each turn fully completes and `turn_end` was emitted; return
/// true to stop the run before polling steering/follow-up queues.
pub type ShouldStopAfterTurnFn = Arc<
    dyn Fn(ShouldStopAfterTurnContext) -> crate::BoxFut<'static, anyhow::Result<bool>>
        + Send
        + Sync,
>;

/// Called synchronously after a completed turn and before polling for another
/// turn; never checked before the initial assistant turn.
pub type ShouldStopBeforeTurnFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Returns steering messages to inject mid-run.
pub type PollMessagesFn =
    Arc<dyn Fn() -> crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>> + Send + Sync>;

/// Returns continuation messages when the agent would otherwise stop.
pub type GetContinuationMessagesFn = Arc<
    dyn Fn(
            GetContinuationMessagesContext,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>>
        + Send
        + Sync,
>;

/// Return `{ block: true }` to prevent execution.
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Option<BeforeToolCallResult>>>
        + Send
        + Sync,
>;

/// Partial override of the executed tool result.
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Option<AfterToolCallResult>>>
        + Send
        + Sync,
>;

/// Mints the continuation row for auto-continuation `attempt` of `max`.
pub type LengthContinuationMessageFn = Arc<dyn Fn(u32, u32) -> AgentMessage + Send + Sync>;

/// Bounded auto-continuation of a response cut off at the output-token
/// limit (`stopReason: length`; upstream #969). When a turn ends on the
/// limit with text and no tool calls, and no steering is waiting, the loop
/// delivers [`LengthContinuation::message`] as the next turn instead of
/// ending the run, at most `max_continuations` times in a row; a turn that
/// ends any other way resets the count. Off unless a host installs it (TS
/// v0.9.8 had no auto-continuation).
#[derive(Clone)]
pub struct LengthContinuation {
    pub max_continuations: u32,
    pub message: LengthContinuationMessageFn,
}

impl LengthContinuation {
    /// Whether `message` is a truncated reply this policy continues: cut
    /// at the limit with visible text, no tool calls (those are actionable
    /// as they stand), and the bound not yet reached.
    #[must_use]
    pub fn continues(&self, message: &crate::types::AssistantMessage, used: u32) -> bool {
        message.stop_reason == crate::types::StopReason::Length
            && used < self.max_continuations
            && message.tool_calls().is_empty()
            && message.content.iter().any(|block| {
                matches!(block, crate::types::AssistantContent::Text(text) if !text.text.trim().is_empty())
            })
    }
}

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub api_key: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub reasoning: ThinkingLevel,
    pub session_id: Option<String>,
    /// The requested service tier rides the loop config into every stream
    /// request; `None` until a host supplies one.
    pub service_tier: Option<crate::types::ServiceTier>,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_system_prompt: Option<GetSystemPromptFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub should_stop_before_turn: Option<ShouldStopBeforeTurnFn>,
    pub get_steering_messages: Option<PollMessagesFn>,
    pub get_follow_up_messages: Option<PollMessagesFn>,
    pub get_continuation_messages: Option<GetContinuationMessagesFn>,
    pub tool_execution: ToolExecutionMode,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Auto-continuation of output-limit truncations; `None` ends the run
    /// on a truncated reply (the TS behavior).
    pub length_continuation: Option<LengthContinuation>,
    /// Stop a degenerate looping generation mid-stream (upstream #1798);
    /// `None` streams every response to its natural end.
    pub repetition_guard: Option<crate::repetition_guard::RepetitionGuardConfig>,
}

impl AgentLoopConfig {
    /// Config with a pass-through `convert_to_llm` (keeps user/assistant/
    /// toolResult messages, filters everything else) and no hooks.
    pub fn new(model: Model, convert_to_llm: ConvertToLlmFn) -> Self {
        AgentLoopConfig {
            model,
            api_key: None,
            temperature: None,
            max_tokens: None,
            reasoning: ThinkingLevel::Off,
            session_id: None,
            service_tier: None,
            convert_to_llm,
            transform_context: None,
            get_system_prompt: None,
            get_api_key: None,
            should_stop_after_turn: None,
            should_stop_before_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            get_continuation_messages: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            length_continuation: None,
            repetition_guard: None,
        }
    }

    #[must_use]
    pub fn default_convert_to_llm() -> ConvertToLlmFn {
        Arc::new(|messages: Vec<AgentMessage>| {
            Box::pin(async move {
                Ok(messages
                    .into_iter()
                    .filter(|m| {
                        matches!(
                            m,
                            AgentMessage::Standard(
                                Message::User(_) | Message::Assistant(_) | Message::ToolResult(_),
                            )
                        )
                    })
                    .map(|m| match m {
                        AgentMessage::Standard(message) => message,
                        AgentMessage::Custom(_) => unreachable!("filtered above"),
                    })
                    .collect())
            })
        })
    }
}

mod abort;
mod entry;
pub(crate) mod response;
mod run;
mod tool_call;
mod tools;

pub use entry::{run_agent_loop, run_agent_loop_continue};
