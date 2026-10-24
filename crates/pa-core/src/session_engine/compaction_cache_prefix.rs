//! Cache-prefix compaction (Claude Code's `tengu_compact_cache_prefix`, on
//! by default: `kasbfbhj:gir` -> `Oon`): the summary request continues the
//! live conversation instead of re-serializing it. It carries the session's
//! own system prompt, tools, thinking level and converted messages, exactly
//! as the loop's last request did, with the summary instruction appended as
//! one more user turn; the provider reads everything but that turn from the
//! prompt cache.
//!
//! A reply without text (a tool call, an empty answer), a provider error, or
//! a context that would not fit falls back to the serialized summarizer
//! request (`compact_session`), as Claude Code falls back
//! (`tengu_compact_cache_sharing_fallback`). An abort is never a fallback.

use std::sync::Arc;

use pa_agent::agent_loop::ConvertToLlmFn;
use pa_agent::stream::{LlmContext, StreamFn, StreamRequestOptions, ToolDefinition};
use pa_agent::types::{AssistantContent, Message, StopReason, TextContent, UserContent, UserPart};

use super::compaction_exec::{SummaryDeltaSink, SummarySlice};

/// Claude Code's text-only preamble (`Tnn`), in the kernel harness's terms.
const TEXT_ONLY_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n- Do NOT call python_repl or ANY other tool.\n- You already have all the context you need in the conversation above.\n- Tool calls will be REJECTED and will waste your only turn - you will fail the task.\n- Your entire response must be plain text: the structured summary below.\n\n";

/// Claude Code's closing reminder (`Cnn`).
const TEXT_ONLY_REMINDER: &str = "\n\nREMINDER: Do NOT call any tools. Respond with plain text only - the structured summary. Tool calls will be rejected and you will fail the task.";

/// What an earlier summary at the top of the conversation means here (the
/// update mode the serialized request expresses with `<previous-summary>`).
const EARLIER_SUMMARY_NOTE: &str = "If the conversation above begins with an earlier summary, keep everything it records that still holds and add the newer work; the summary you write replaces it.";

/// The seam the engine installs: the session's pre-semantic stream fn and
/// its loop-boundary message conversion.
#[derive(Clone)]
pub struct CachePrefixSeam {
    pub stream_fn: StreamFn,
    pub convert_to_llm: ConvertToLlmFn,
}

/// One cache-prefix summary request: the live loop's request plus the
/// session's stream fn.
pub struct CachePrefixRequest {
    stream_fn: StreamFn,
    model: pa_agent::types::Model,
    context: LlmContext,
    reasoning: pa_agent::types::ThinkingLevel,
}

/// Why a cache-prefix summary fell back to the serialized request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cache-prefix summary fell back: {0}")]
pub struct CachePrefixFallback(pub &'static str);

impl CachePrefixRequest {
    /// The request continuing `messages` under the live loop's system
    /// prompt, tools and thinking level.
    ///
    /// # Errors
    ///
    /// Returns the conversion's error.
    pub async fn from_live(
        seam: &CachePrefixSeam,
        state: &pa_agent::agent::AgentStateSnapshot,
    ) -> anyhow::Result<Self> {
        let messages = (seam.convert_to_llm)(state.messages.clone()).await?;
        Ok(Self {
            stream_fn: Arc::clone(&seam.stream_fn),
            model: state.model.clone(),
            context: LlmContext {
                system_prompt: Some(state.system_prompt.clone()),
                messages,
                tools: state
                    .tools
                    .iter()
                    .map(|tool| ToolDefinition {
                        name: tool.name().to_string(),
                        description: tool.description().to_string(),
                        parameters: tool.parameters().clone(),
                    })
                    .collect(),
            },
            reasoning: state.thinking_level,
        })
    }

    /// The converted conversation the summary turn continues.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.context.messages
    }
}

/// The appended summary turn: Claude Code's text-only frame around the
/// session's summarization prompt.
#[must_use]
pub fn summary_turn_text(custom_instructions: Option<&str>) -> String {
    format!(
        "{TEXT_ONLY_PREAMBLE}{}\n\n{EARLIER_SUMMARY_NOTE}{TEXT_ONLY_REMINDER}",
        super::compaction::build_summarization_prompt(custom_instructions, None)
    )
}

/// Run the summary as a continuation of the cached conversation.
/// `headers` carry the compaction's request id; `abort` cancels the
/// stream; the completed summary goes to `sink` once.
///
/// # Errors
///
/// [`CachePrefixFallback`] when the caller should run the serialized
/// request instead; the abort error when `abort` fired.
pub async fn summarize(
    request: &CachePrefixRequest,
    custom_instructions: Option<&str>,
    max_tokens: u64,
    headers: Option<std::collections::BTreeMap<String, String>>,
    abort: Option<&pa_agent::abort::AbortSignal>,
    sink: Option<&SummaryDeltaSink>,
) -> anyhow::Result<SummarySlice> {
    let mut context = request.context.clone();
    context
        .messages
        .push(Message::User(pa_agent::types::UserMessage {
            content: UserContent::Parts(vec![UserPart::Text(TextContent {
                text: summary_turn_text(custom_instructions),
                text_signature: None,
            })]),
            timestamp: pa_agent::now_ms(),
        }));
    let signal = abort.cloned().unwrap_or_default();
    let options = StreamRequestOptions {
        temperature: None,
        max_tokens: Some(max_tokens),
        reasoning: request.reasoning,
        session_id: None,
        service_tier: None,
        api_key: None,
        signal: signal.clone(),
        on_payload: None,
        on_response: None,
        headers,
        tool_choice: None,
    };
    let opened = pa_agent::abort::race_with_abort(
        (request.stream_fn)(request.model.clone(), context, options),
        &signal,
    )
    .await;
    let mut stream = match opened {
        Err(_) => return Err(pa_agent::abort::aborted_error()),
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => return Err(CachePrefixFallback("stream_open_failed").into()),
    };
    while let Some(_event) =
        match pa_agent::abort::race_with_abort(stream.next_event(), &signal).await {
            Ok(event) => event,
            Err(_) => {
                stream.close();
                return Err(pa_agent::abort::aborted_error());
            }
        }
    {}
    let assistant = match stream.result().await {
        Ok(assistant) => assistant,
        Err(_) if signal.is_aborted() => return Err(pa_agent::abort::aborted_error()),
        Err(_) => return Err(CachePrefixFallback("no_response").into()),
    };
    if signal.is_aborted() || assistant.stop_reason == StopReason::Aborted {
        return Err(pa_agent::abort::aborted_error());
    }
    if assistant.stop_reason == StopReason::Error {
        return Err(CachePrefixFallback("error").into());
    }
    let summary = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if summary.trim().is_empty() {
        return Err(CachePrefixFallback("no_text_response").into());
    }
    if let Some(sink) = sink {
        sink(&summary);
    }
    Ok(SummarySlice {
        summary,
        usage: super::provider_adapter::json_round_trip(&assistant.usage),
    })
}

#[cfg(test)]
mod tests;
