//! The session's [`RunAgent`]: one tool-less child agent on the provider
//! transport (TS `runAgent` with `tools: "none"`).
//!
//! The child is the native agent loop with no tools and no system prompt:
//! the model is resolved the way RLM children resolve theirs (the session
//! model when none is named), its credential cleared before any provider
//! I/O, the thinking level checked against the model, and the visible
//! answer capped (`max_tokens` = cap plus the thinking allowance, never
//! above the model's own limit). A turn that would continue (a tool call)
//! stops at `max_turns` or once usage reaches the token budget, reporting
//! `turn_limit` / `budget_exceeded`; a cancel aborts the stream. It runs on
//! the dream thread, blocking on the session runtime's handle; nothing here
//! touches the parent session's messages, files or ledgers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::abort::AbortController;
use pa_agent::agent_loop::{AgentLoopConfig, run_agent_loop};
use pa_agent::stream::StreamFn;
use pa_agent::types::{
    AgentContext,
    AgentMessage,
    AssistantContent,
    Message,
    StopReason,
    UserContent,
    UserMessage,
};
use pa_core::models::ModelRegistry;
use pa_core::session_engine::provider_adapter::stream_once;
use pa_core::session_engine::rlm_in_process::{assert_thinking_supported, resolve_child_model};

use crate::child::{
    RunAgent,
    RunAgentOptions,
    RunAgentRequest,
    RunAgentResult,
    RunAgentStatus,
    capped_max_tokens,
};

/// The session facts a child resolves against.
#[derive(Clone)]
pub struct AgentRunAgent {
    pub agent_dir: PathBuf,
    pub cwd: PathBuf,
    pub session_model: pa_agent::types::Model,
    pub runtime: tokio::runtime::Handle,
}

/// A model cleared for provider I/O.
struct Cleared {
    model: pa_agent::types::Model,
    /// The registry's model the provider transport streams with.
    registered: pa_types::ai::Model,
    api_key: Option<String>,
    headers: Option<BTreeMap<String, String>>,
}

impl AgentRunAgent {
    fn clear(&self, request: &RunAgentRequest) -> Result<Cleared, String> {
        let mut registry = ModelRegistry::for_session(&self.agent_dir, self.cwd.clone());
        registry.load_private_authorization_from_cache();
        let resolved = resolve_child_model(
            &registry,
            request.model.as_deref(),
            Some(&self.session_model),
            "dream child",
        )
        .map_err(|error| format!("{error:#}"))?;
        assert_thinking_supported(
            &registry,
            request.thinking_level.as_deref(),
            &resolved.selector,
        )
        .map_err(|error| format!("{error:#}"))?;
        let (provider, id) = (&resolved.model.provider, &resolved.model.id);
        let registered = registry
            .get_all()
            .iter()
            .find(|model| &model.provider == provider && &model.id == id)
            .cloned()
            .ok_or_else(|| format!("Model \"{provider}/{id}\" is not registered"))?;
        let auth = registry.get_api_key_and_headers(&registered, None);
        if !auth.ok {
            return Err(auth
                .error
                .unwrap_or_else(|| format!("No credential found for \"{provider}\"")));
        }
        Ok(Cleared {
            model: resolved.model,
            registered,
            api_key: auth.api_key,
            headers: auth.headers,
        })
    }
}

fn thinking_level(level: Option<&str>) -> Option<pa_agent::types::ThinkingLevel> {
    level.and_then(|level| serde_json::from_value(serde_json::Value::from(level)).ok())
}

fn stop_reason_name(reason: StopReason) -> Option<String> {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
}

#[derive(Default)]
struct Limits {
    turns: u32,
    tokens: u64,
    hit: Option<RunAgentStatus>,
}

impl RunAgent for AgentRunAgent {
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult {
        if options.cancel.is_cancelled() {
            return RunAgentResult::empty(RunAgentStatus::Aborted);
        }
        let cleared = match self.clear(request) {
            Ok(cleared) => cleared,
            Err(error) => {
                return RunAgentResult {
                    error: Some(error),
                    ..RunAgentResult::empty(RunAgentStatus::Error)
                };
            }
        };
        self.runtime.block_on(run_child(request, options, cleared))
    }
}

#[allow(clippy::too_many_lines)] // one child run: config, limits, stream, result
async fn run_child(
    request: &RunAgentRequest,
    options: &RunAgentOptions,
    cleared: Cleared,
) -> RunAgentResult {
    let reasoning = thinking_level(request.thinking_level.as_deref());
    let mut config = AgentLoopConfig::new(
        cleared.model.clone(),
        AgentLoopConfig::default_convert_to_llm(),
    );
    config.api_key.clone_from(&cleared.api_key);
    if let Some(reasoning) = reasoning {
        config.reasoning = reasoning;
    }
    if let Some(cap) = options.max_output_tokens {
        let allowed = capped_max_tokens(cap, request.thinking_level.as_deref());
        let model_limit = cleared.model.max_tokens;
        config.max_tokens = Some(if model_limit > 0 {
            allowed.min(model_limit)
        } else {
            allowed
        });
    }
    let limits = Arc::new(Mutex::new(Limits::default()));
    let (max_turns, budget) = (options.max_turns, options.token_budget);
    let tracked = Arc::clone(&limits);
    config.should_stop_after_turn = Some(Arc::new(move |turn| {
        let mut limits = tracked.lock().unwrap_or_else(PoisonError::into_inner);
        limits.turns += 1;
        limits.tokens += turn.message.usage.total_tokens;
        // Limits stop FURTHER turns: a turn without a tool call is the answer.
        let continues = turn.message.stop_reason == StopReason::ToolUse;
        let stop = continues
            && if max_turns.is_some_and(|max| limits.turns >= max) {
                limits.hit = Some(RunAgentStatus::TurnLimit);
                true
            } else if limits.tokens >= budget {
                limits.hit = Some(RunAgentStatus::BudgetExceeded);
                true
            } else {
                false
            };
        Box::pin(async move { Ok(stop) })
    }));
    let model = cleared.registered.clone();
    let headers = cleared.headers.clone();
    let stream_fn: StreamFn = Arc::new(move |_requested, context, stream_options| {
        let model = model.clone();
        let headers = headers.clone();
        Box::pin(async move {
            let api_key = stream_options.api_key.clone();
            stream_once(&model, api_key, None, headers, context, stream_options)
        })
    });
    let controller = AbortController::new();
    let signal = controller.signal();
    let cancel = options.cancel.clone();
    let watcher = tokio::spawn(async move {
        cancel.cancelled().await;
        controller.abort();
    });
    let prompt = AgentMessage::Standard(Message::User(UserMessage {
        content: UserContent::Text(request.prompt.clone()),
        timestamp: i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis()),
        )
        .unwrap_or(0),
    }));
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let emit: pa_agent::agent_loop::AgentEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
    let outcome = run_agent_loop(
        vec![prompt],
        context,
        &config,
        emit,
        Some(&signal),
        Some(&stream_fn),
    )
    .await;
    watcher.abort();
    let limits = limits.lock().unwrap_or_else(PoisonError::into_inner);
    let messages = match outcome {
        Ok(messages) => messages,
        Err(error) => {
            let status = if options.cancel.is_cancelled() {
                RunAgentStatus::Aborted
            } else {
                limits.hit.unwrap_or(RunAgentStatus::Error)
            };
            return RunAgentResult {
                error: (status == RunAgentStatus::Error).then(|| format!("{error:#}")),
                total_tokens: limits.tokens,
                ..RunAgentResult::empty(status)
            };
        }
    };
    let assistants: Vec<&pa_agent::types::AssistantMessage> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::Assistant(assistant)) => Some(assistant),
            _ => None,
        })
        .collect();
    let total_tokens = assistants
        .iter()
        .map(|message| message.usage.total_tokens)
        .sum();
    let output_tokens = assistants.iter().map(|message| message.usage.output).sum();
    let last = assistants.last();
    let output = last.map_or_else(String::new, |message| {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    let mut status = if options.cancel.is_cancelled() {
        RunAgentStatus::Aborted
    } else {
        limits.hit.unwrap_or(RunAgentStatus::Completed)
    };
    let mut error = None;
    if status == RunAgentStatus::Completed {
        match last.map(|message| &message.stop_reason) {
            Some(StopReason::Error) => {
                status = RunAgentStatus::Error;
                error = Some(
                    last.and_then(|message| message.error_message.clone())
                        .unwrap_or_else(|| "Agent request failed".to_string()),
                );
            }
            Some(StopReason::Aborted) => status = RunAgentStatus::Aborted,
            _ => {}
        }
    }
    RunAgentResult {
        status,
        output,
        stop_reason: last.and_then(|message| stop_reason_name(message.stop_reason)),
        total_tokens,
        output_tokens,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_levels_and_stop_reasons_use_the_wire_names() {
        assert_eq!(
            thinking_level(Some("off")),
            Some(pa_agent::types::ThinkingLevel::Off)
        );
        assert_eq!(
            thinking_level(Some("xhigh")),
            Some(pa_agent::types::ThinkingLevel::Xhigh)
        );
        assert_eq!(thinking_level(Some("huge")), None);
        assert_eq!(
            stop_reason_name(StopReason::Length).as_deref(),
            Some("length")
        );
        assert_eq!(
            stop_reason_name(StopReason::ToolUse).as_deref(),
            Some("toolUse")
        );
    }
}
