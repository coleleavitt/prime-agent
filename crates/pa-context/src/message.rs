//! What the context manager reads and writes of one message: its stable
//! identity key, its tag-bearing text, its token estimate, and the two
//! rewrites (the tag prefix, the drop placeholder). Assistant messages are
//! never rewritten: they may carry signed thinking or text signatures.

use std::collections::HashMap;

use pa_agent::types::{
    AgentMessage,
    AssistantContent,
    CustomAgentMessage,
    Message,
    TextContent,
    ToolResultContent,
    UserContent,
    UserPart,
};

/// Characters per estimated token.
const CHARS_PER_TOKEN: usize = 4;
/// The estimate charged for one image.
const IMAGE_TOKENS: usize = 1_000;

/// What kind of message a key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    User,
    Assistant,
    ToolResult,
    /// A displayed custom row: input the session received besides the
    /// user's prompts (an agent message, a task notification, a scheduled
    /// fire). Like a user message it starts a stretch of work, so it is a
    /// historian boundary; it is never tagged or rewritten.
    Input,
    /// A hidden custom row (session context, reminders, the digest).
    Custom,
}

/// The kind of `message`.
pub(crate) fn kind(message: &AgentMessage) -> Kind {
    match message {
        AgentMessage::Standard(Message::User(_)) => Kind::User,
        AgentMessage::Standard(Message::Assistant(_)) => Kind::Assistant,
        AgentMessage::Standard(Message::ToolResult(_)) => Kind::ToolResult,
        AgentMessage::Custom(custom) if custom.payload["display"] == true => Kind::Input,
        AgentMessage::Custom(_) => Kind::Custom,
    }
}

/// Whether a fold range may start at, or end before, `message`.
pub(crate) fn is_boundary(message: &AgentMessage) -> bool {
    matches!(kind(message), Kind::User | Kind::Input)
}

fn custom_timestamp(custom: &CustomAgentMessage) -> Option<i64> {
    custom.payload["timestamp"].as_i64()
}

/// The identity key of every message, in order: a tool result's call id; a
/// user, assistant or displayed custom message's role and timestamp, with
/// its occurrence when several share one; `None` for hidden custom rows
/// (session context, reminders), which are neither keyed nor boundaries.
pub(crate) fn keys(messages: &[AgentMessage]) -> Vec<Option<String>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    messages
        .iter()
        .map(|message| {
            let base = match message {
                AgentMessage::Standard(Message::ToolResult(result)) => {
                    return Some(format!("t:{}", result.tool_call_id));
                }
                AgentMessage::Standard(Message::User(user)) => format!("u:{}", user.timestamp),
                AgentMessage::Standard(Message::Assistant(assistant)) => {
                    format!("a:{}", assistant.timestamp)
                }
                AgentMessage::Custom(custom) => match (kind(message), custom_timestamp(custom)) {
                    (Kind::Input, Some(timestamp)) => format!("c:{timestamp}"),
                    _ => return None,
                },
            };
            let occurrence = seen.entry(base.clone()).or_insert(0);
            *occurrence += 1;
            Some(if *occurrence == 1 {
                base
            } else {
                format!("{base}#{occurrence}")
            })
        })
        .collect()
}

fn text_tokens(text: &str) -> usize {
    text.len().div_ceil(CHARS_PER_TOKEN)
}

/// The estimated tokens of `message`.
pub(crate) fn tokens(message: &AgentMessage) -> usize {
    match message {
        AgentMessage::Standard(Message::User(user)) => match &user.content {
            UserContent::Text(text) => text_tokens(text),
            UserContent::Parts(parts) => parts
                .iter()
                .map(|part| match part {
                    UserPart::Text(text) => text_tokens(&text.text),
                    UserPart::Image(_) => IMAGE_TOKENS,
                })
                .sum(),
        },
        AgentMessage::Standard(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .map(|part| match part {
                AssistantContent::Text(text) => text_tokens(&text.text),
                AssistantContent::Thinking(thinking) => text_tokens(&thinking.thinking),
                AssistantContent::ToolCall(call) => {
                    text_tokens(&call.name) + text_tokens(&call.arguments.to_string())
                }
            })
            .sum(),
        AgentMessage::Standard(Message::ToolResult(result)) => result
            .content
            .iter()
            .map(|part| match part {
                ToolResultContent::Text(text) => text_tokens(&text.text),
                ToolResultContent::Image(_) => IMAGE_TOKENS,
            })
            .sum(),
        AgentMessage::Custom(custom) => text_tokens(&custom.payload.to_string()),
    }
}

/// The text of `message` (images as `[image]`), for the historian, expand
/// and the nudge's sizes.
pub(crate) fn text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Standard(Message::User(user)) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Parts(parts) => parts
                .iter()
                .map(|part| match part {
                    UserPart::Text(text) => text.text.as_str(),
                    UserPart::Image(_) => "[image]",
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        AgentMessage::Standard(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text(text) => Some(text.text.clone()),
                AssistantContent::Thinking(_) => None,
                AssistantContent::ToolCall(call) => Some(format!(
                    "[call {}] {}",
                    call.name,
                    call.arguments
                        .get("code")
                        .and_then(serde_json::Value::as_str)
                        .map_or_else(|| call.arguments.to_string(), str::to_string)
                )),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::Standard(Message::ToolResult(result)) => result
            .content
            .iter()
            .map(|part| match part {
                ToolResultContent::Text(text) => text.text.as_str(),
                ToolResultContent::Image(_) => "[image]",
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentMessage::Custom(custom) => match &custom.payload["content"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .map(|part| part["text"].as_str().unwrap_or("[image]"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => custom.payload.to_string(),
        },
    }
}

/// The one placeholder of a dropped item: a pure function of its tag, so
/// every replay renders the same bytes.
pub(crate) fn placeholder(tag: u32) -> String {
    format!("[dropped §{tag}§]")
}

fn text_content(text: String) -> TextContent {
    TextContent {
        text,
        text_signature: None,
    }
}

/// `message` with `§tag§ ` in front of its first text (a text part inserted
/// when it has none). Only user messages and tool results are tagged.
pub(crate) fn tagged(mut message: AgentMessage, tag: u32) -> AgentMessage {
    let prefix = format!("§{tag}§ ");
    match &mut message {
        AgentMessage::Standard(Message::User(user)) => match &mut user.content {
            UserContent::Text(text) => text.insert_str(0, &prefix),
            UserContent::Parts(parts) => match parts.iter_mut().find_map(|part| match part {
                UserPart::Text(text) => Some(text),
                UserPart::Image(_) => None,
            }) {
                Some(text) => text.text.insert_str(0, &prefix),
                None => parts.insert(
                    0,
                    UserPart::Text(text_content(prefix.trim_end().to_string())),
                ),
            },
        },
        AgentMessage::Standard(Message::ToolResult(result)) => {
            match result.content.iter_mut().find_map(|part| match part {
                ToolResultContent::Text(text) => Some(text),
                ToolResultContent::Image(_) => None,
            }) {
                Some(text) => text.text.insert_str(0, &prefix),
                None => result
                    .content
                    .insert(0, ToolResultContent::text(prefix.trim_end())),
            }
        }
        AgentMessage::Standard(Message::Assistant(_)) | AgentMessage::Custom(_) => {}
    }
    message
}

/// `message` reduced to its placeholder. A tool result keeps its message
/// (the provider pairs it with its call); only user messages and tool
/// results are droppable.
pub(crate) fn dropped(mut message: AgentMessage, tag: u32) -> AgentMessage {
    match &mut message {
        AgentMessage::Standard(Message::User(user)) => {
            user.content = UserContent::Text(placeholder(tag));
        }
        AgentMessage::Standard(Message::ToolResult(result)) => {
            result.content = vec![ToolResultContent::text(placeholder(tag))];
        }
        AgentMessage::Standard(Message::Assistant(_)) | AgentMessage::Custom(_) => {}
    }
    message
}

/// A synthetic user message carrying `text`, stamped `timestamp`.
pub(crate) fn synthetic_user(text: String, timestamp: i64) -> AgentMessage {
    AgentMessage::Standard(Message::User(pa_agent::types::UserMessage {
        content: UserContent::Text(text),
        timestamp,
    }))
}

/// The timestamp of `message` (0 for a custom row without one).
pub(crate) fn timestamp(message: &AgentMessage) -> i64 {
    match message {
        AgentMessage::Standard(Message::User(user)) => user.timestamp,
        AgentMessage::Standard(Message::Assistant(assistant)) => assistant.timestamp,
        AgentMessage::Standard(Message::ToolResult(result)) => result.timestamp,
        AgentMessage::Custom(custom) => custom_timestamp(custom).unwrap_or(0),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use pa_agent::types::{ToolResultMessage, UserMessage};

    use super::*;

    pub(crate) fn user(text: &str, timestamp: i64) -> AgentMessage {
        AgentMessage::Standard(Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        }))
    }

    pub(crate) fn tool_result(call: &str, text: &str, timestamp: i64) -> AgentMessage {
        AgentMessage::Standard(Message::ToolResult(ToolResultMessage {
            tool_call_id: call.to_string(),
            tool_name: "python_repl".to_string(),
            content: vec![ToolResultContent::text(text)],
            details: None,
            is_error: false,
            timestamp,
        }))
    }

    /// A custom row; `display` makes it session input (an agent message).
    pub(crate) fn custom(text: &str, display: bool, timestamp: i64) -> AgentMessage {
        AgentMessage::Custom(CustomAgentMessage {
            role: "custom".to_string(),
            payload: serde_json::json!({
                "customType": "agent_message", "content": text, "display": display,
                "timestamp": timestamp,
            }),
        })
    }

    pub(crate) fn assistant(text: &str, call: Option<&str>, timestamp: i64) -> AgentMessage {
        let mut content = vec![AssistantContent::Text(text_content(text.to_string()))];
        if let Some(call) = call {
            content.push(AssistantContent::ToolCall(pa_agent::types::ToolCall {
                id: call.to_string(),
                name: "python_repl".to_string(),
                arguments: serde_json::json!({"code": "print(1)"}),
                thought_signature: None,
            }));
        }
        serde_json::from_value(serde_json::json!({
            "role": "assistant", "content": content, "api": "test", "provider": "test",
            "model": "m", "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "stopReason": "stop", "timestamp": timestamp
        }))
        .unwrap()
    }

    #[test]
    fn keys_are_stable_and_disambiguated() {
        let messages = [
            user("a", 5),
            user("b", 5),
            tool_result("c1", "out", 6),
            assistant("x", Some("c1"), 7),
            custom("from a sibling", true, 8),
            custom("reminder", false, 9),
        ];
        assert_eq!(
            keys(&messages),
            [
                Some("u:5".to_string()),
                Some("u:5#2".to_string()),
                Some("t:c1".to_string()),
                Some("a:7".to_string()),
                Some("c:8".to_string()),
                None,
            ]
        );
    }

    #[test]
    fn tags_and_drops_rewrite_only_the_text() {
        let tagged_result = tagged(tool_result("c1", "hello", 1), 7);
        assert_eq!(text(&tagged_result), "§7§ hello");
        assert_eq!(text(&tagged(user("hi", 1), 3)), "§3§ hi");
        let gone = dropped(tool_result("c1", &"x".repeat(4_000), 1), 7);
        assert_eq!(text(&gone), "[dropped §7§]");
        assert!(matches!(
            &gone,
            AgentMessage::Standard(Message::ToolResult(result)) if result.tool_call_id == "c1"
        ));
        assert_eq!(tokens(&tool_result("c", &"y".repeat(400), 1)), 100);
        let untouched = assistant("reply", None, 1);
        assert_eq!(tagged(untouched.clone(), 1), untouched);
    }
}
