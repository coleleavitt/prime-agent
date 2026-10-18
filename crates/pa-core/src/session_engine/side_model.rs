//! One-off model calls beside a session's own loop: a system prompt and a
//! few messages in, the reply's text out, no tools. The goal evaluator, the
//! observer and the advisor ask through it.
//!
//! Two routes: [`SideModel::Session`] streams through the session's own
//! stream fn (its model and credentials, the way `/btw` does), and
//! [`SideModel::Selector`] resolves a `provider/model-id` through the model
//! registry and calls pa-ai directly (another model than the session's).

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::stream::{LlmContext, StreamFn, StreamRequestOptions};
use pa_agent::types::{AssistantContent, Message, StopReason};

/// The longest a side call may take before it counts as failed.
pub const SIDE_MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Where a side call goes.
#[derive(Clone)]
pub enum SideModel {
    /// The session's own stream fn and model.
    Session {
        stream_fn: StreamFn,
        model: pa_agent::types::Model,
    },
    /// A `provider/model-id` the registry resolves (with its credentials)
    /// for the session's directories.
    Selector {
        selector: String,
        agent_dir: PathBuf,
        cwd: PathBuf,
    },
}

impl std::fmt::Debug for SideModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SideModel::Session { model, .. } => f
                .debug_struct("Session")
                .field("model", &format!("{}/{}", model.provider, model.id))
                .finish_non_exhaustive(),
            SideModel::Selector { selector, .. } => f
                .debug_struct("Selector")
                .field("selector", selector)
                .finish_non_exhaustive(),
        }
    }
}

/// One side request.
#[derive(Debug, Clone)]
pub struct SideRequest {
    pub system_prompt: String,
    pub messages: Vec<Message>,
}

impl SideRequest {
    /// A request of one user message.
    #[must_use]
    pub fn single(system_prompt: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            messages: vec![Message::User(pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Text(user_text.into()),
                timestamp: pa_agent::now_ms(),
            })],
        }
    }
}

fn reply_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

impl SideModel {
    /// Ask once; the reply's text, or why it failed (a provider error, a
    /// missing credential, the timeout).
    ///
    /// # Errors
    ///
    /// Returns the failure as text-bearing `anyhow` error.
    pub async fn complete(&self, request: SideRequest) -> anyhow::Result<String> {
        match tokio::time::timeout(SIDE_MODEL_TIMEOUT, self.complete_inner(request)).await {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "the side model call timed out after {}s",
                SIDE_MODEL_TIMEOUT.as_secs()
            ),
        }
    }

    async fn complete_inner(&self, request: SideRequest) -> anyhow::Result<String> {
        match self {
            SideModel::Session { stream_fn, model } => {
                let context = LlmContext {
                    system_prompt: Some(request.system_prompt),
                    messages: request.messages,
                    tools: Vec::new(),
                };
                let mut stream =
                    stream_fn(model.clone(), context, StreamRequestOptions::default()).await?;
                while stream.next_event().await.is_some() {}
                let message = stream.result().await?;
                if message.stop_reason == StopReason::Error {
                    anyhow::bail!(
                        "{}",
                        message
                            .error_message
                            .unwrap_or_else(|| "the model call failed".to_string())
                    );
                }
                Ok(reply_text(&message.content))
            }
            SideModel::Selector {
                selector,
                agent_dir,
                cwd,
            } => {
                let (model, api_key, headers) = {
                    let selector = selector.clone();
                    let agent_dir = agent_dir.clone();
                    let cwd = cwd.clone();
                    tokio::task::spawn_blocking(move || resolve(&selector, &agent_dir, cwd))
                        .await??
                };
                let messages = request
                    .messages
                    .iter()
                    .map(|message| {
                        super::provider_adapter::json_round_trip::<_, pa_types::ai::Message>(
                            message,
                        )
                        .ok_or_else(|| anyhow::anyhow!("a side message failed the wire conversion"))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let context = pa_types::ai::Context {
                    system_prompt: Some(request.system_prompt),
                    messages,
                    tools: None,
                };
                let options =
                    pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
                        api_key,
                        headers: headers.map(|headers| headers.into_iter().collect()),
                        ..Default::default()
                    });
                let reply = pa_ai::complete_simple(&model, &context, Some(options))
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                if reply.stop_reason == pa_types::ai::StopReason::Error {
                    anyhow::bail!(
                        "{}",
                        reply
                            .error_message
                            .unwrap_or_else(|| "the model call failed".to_string())
                    );
                }
                Ok(reply
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>())
            }
        }
    }
}

/// Ask once over the parent session's own conversation (Claude Code's
/// forked side call: `cacheSafeParams`, one turn, tools denied): the
/// parent's system prompt, its messages, its tool definitions (so the
/// request shares the conversation's prompt-cache prefix), then `prompt`
/// as one more user message. The reply's text; a tool call the model
/// makes anyway is ignored, never run. The parent agent is not touched.
///
/// # Errors
///
/// No model is selected, the provider call fails, or the timeout passes.
pub async fn complete_forked(
    parent: &pa_agent::agent::Agent,
    stream_fn: &StreamFn,
    prompt: &str,
) -> anyhow::Result<String> {
    let state = parent.state().await;
    if state.model == pa_agent::types::Model::unknown() {
        anyhow::bail!("Select a model first");
    }
    let mut messages = state.messages;
    messages.push(pa_agent::types::AgentMessage::Standard(Message::User(
        pa_agent::types::UserMessage {
            content: pa_agent::types::UserContent::Text(prompt.to_string()),
            timestamp: pa_agent::now_ms(),
        },
    )));
    let context = LlmContext {
        system_prompt: Some(state.system_prompt),
        messages: super::messages::loop_convert_to_llm(messages),
        tools: state
            .tools
            .iter()
            .map(|tool| pa_agent::stream::ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters().clone(),
            })
            .collect(),
    };
    let call = async {
        let mut stream = stream_fn(
            state.model.clone(),
            context,
            StreamRequestOptions::default(),
        )
        .await?;
        while stream.next_event().await.is_some() {}
        let message = stream.result().await?;
        if message.stop_reason == StopReason::Error {
            anyhow::bail!(
                "{}",
                message
                    .error_message
                    .unwrap_or_else(|| "the model call failed".to_string())
            );
        }
        Ok(reply_text(&message.content))
    };
    match tokio::time::timeout(SIDE_MODEL_TIMEOUT, call).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "the side model call timed out after {}s",
            SIDE_MODEL_TIMEOUT.as_secs()
        ),
    }
}

type Resolved = (
    pa_types::ai::Model,
    Option<String>,
    Option<std::collections::BTreeMap<String, String>>,
);

/// Resolve `provider/model-id` and its credentials (blocking: the registry
/// reads disk).
fn resolve(selector: &str, agent_dir: &std::path::Path, cwd: PathBuf) -> anyhow::Result<Resolved> {
    let (provider, id) = selector
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("model \"{selector}\" is not provider/model-id"))?;
    let mut registry = crate::models::ModelRegistry::for_session(agent_dir, cwd);
    registry.load_private_authorization_from_cache();
    let model = registry
        .get_all()
        .iter()
        .find(|model| {
            model.provider.eq_ignore_ascii_case(provider) && model.id.eq_ignore_ascii_case(id)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("model \"{selector}\" is not registered"))?;
    let auth = registry.get_api_key_and_headers(&model, None);
    if !auth.ok {
        anyhow::bail!(
            "{}",
            auth.error
                .unwrap_or_else(|| format!("no credential found for \"{}\"", model.provider))
        );
    }
    Ok((model, auth.api_key, auth.headers))
}

/// The side model for an optional `provider/model-id` setting: the selector
/// when set, else the session's own stream fn.
#[must_use]
pub fn side_model_for(
    selector: Option<&str>,
    session_stream_fn: &StreamFn,
    session_model: &pa_agent::types::Model,
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> SideModel {
    match selector {
        Some(selector) => SideModel::Selector {
            selector: selector.to_string(),
            agent_dir: agent_dir.to_path_buf(),
            cwd: cwd.to_path_buf(),
        },
        None => SideModel::Session {
            stream_fn: Arc::clone(session_stream_fn),
            model: session_model.clone(),
        },
    }
}

/// Render a conversation as a bounded plain-text transcript (newest kept
/// when over `max_chars`): what a side model reads of the session.
#[must_use]
pub fn render_transcript(messages: &[pa_agent::types::AgentMessage], max_chars: usize) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for message in messages {
        let pa_agent::types::AgentMessage::Standard(message) = message else {
            continue;
        };
        let block = match message {
            Message::User(user) => {
                let text = match &user.content {
                    pa_agent::types::UserContent::Text(text) => text.clone(),
                    pa_agent::types::UserContent::Parts(parts) => parts
                        .iter()
                        .map(|part| match part {
                            pa_agent::types::UserPart::Text(text) => text.text.as_str(),
                            pa_agent::types::UserPart::Image(_) => "[image]",
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                format!("User: {}", clip(&text, 4_000))
            }
            Message::Assistant(assistant) => {
                let mut parts = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                            parts.push(clip(&text.text, 4_000));
                        }
                        AssistantContent::ToolCall(call) => parts.push(format!(
                            "[tool call {}] {}",
                            call.name,
                            clip(
                                &serde_json::to_string(&call.arguments).unwrap_or_default(),
                                2_000
                            )
                        )),
                        AssistantContent::Text(_) | AssistantContent::Thinking(_) => {}
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                format!("Assistant: {}", parts.join("\n"))
            }
            Message::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .map(|block| match block {
                        pa_agent::types::ToolResultContent::Text(text) => text.text.as_str(),
                        pa_agent::types::ToolResultContent::Image(_) => "[image]",
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Tool result ({}{}): {}",
                    result.tool_name,
                    if result.is_error { ", error" } else { "" },
                    clip(&text, 2_000)
                )
            }
        };
        blocks.push(block);
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for block in blocks.iter().rev() {
        if used + block.len() + 2 > max_chars && !kept.is_empty() {
            break;
        }
        used += block.len() + 2;
        kept.push(block);
    }
    let omitted = blocks.len() - kept.len();
    kept.reverse();
    let mut transcript = kept.join("\n\n");
    if omitted > 0 {
        transcript = format!("[{omitted} earlier entries omitted]\n\n{transcript}");
    }
    transcript
}

/// `text` cut to at most `max` characters (on a char boundary), marked.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}... [+{} chars truncated]", text.chars().count() - max)
}

#[cfg(test)]
mod tests {
    use pa_agent::scripted::ScriptedProvider;

    use super::*;

    fn test_model() -> pa_agent::types::Model {
        pa_agent::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            cost: pa_agent::types::UsageCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            max_tokens_explicit: false,
        }
    }

    #[tokio::test]
    async fn the_session_route_returns_the_reply_text_and_sends_the_system_prompt() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("the verdict");
        let side = SideModel::Session {
            stream_fn: provider.stream_fn(),
            model: test_model(),
        };
        let reply = side
            .complete(SideRequest::single("judge it", "is it done?"))
            .await
            .unwrap();
        assert_eq!(reply, "the verdict");
        let calls = provider.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].system_prompt.as_deref(), Some("judge it"));
        assert!(calls[0].tools.is_empty());
    }

    #[tokio::test]
    async fn a_provider_failure_is_an_error() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_fail_start_turn("boom");
        let side = SideModel::Session {
            stream_fn: provider.stream_fn(),
            model: test_model(),
        };
        assert!(side.complete(SideRequest::single("s", "u")).await.is_err());
    }

    #[test]
    fn an_unregistered_selector_fails_to_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let error = resolve("nope/missing", tmp.path(), tmp.path().to_path_buf()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "model \"nope/missing\" is not registered"
        );
        let error = resolve("no-slash", tmp.path(), tmp.path().to_path_buf()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "model \"no-slash\" is not provider/model-id"
        );
    }

    #[test]
    fn the_transcript_keeps_the_newest_entries_within_the_bound() {
        let user = |text: &str| {
            pa_agent::types::AgentMessage::Standard(Message::User(pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Text(text.to_string()),
                timestamp: 0,
            }))
        };
        let messages = vec![user("first"), user("second"), user("third")];
        assert_eq!(
            render_transcript(&messages, 10_000),
            "User: first\n\nUser: second\n\nUser: third"
        );
        assert_eq!(
            render_transcript(&messages, 30),
            "[1 earlier entries omitted]\n\nUser: second\n\nUser: third"
        );
        assert_eq!(clip("abcdef", 3), "abc... [+3 chars truncated]");
    }
}
