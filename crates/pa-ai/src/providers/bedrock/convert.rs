//! Bedrock Converse request conversion: messages, system prompt, tool config, and model-capability
//! classification.

use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::models::clamp_thinking_level;
use crate::providers::transform_messages::transform_messages_with_normalizer;
use crate::types::{
    AssistantContent, CacheRetention, Context, Message, Model, ModelThinkingLevel, Tool,
    UserMessageContent,
};
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;
use crate::ProviderError;

fn get_model_match_candidates(model_id: &str, model_name: Option<&str>) -> Vec<String> {
    let mut values = vec![model_id.to_string()];
    if let Some(name) = model_name {
        values.push(name.to_string());
    }
    values
        .iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let dashed = lower.replace(
                |c: char| c.is_whitespace() || c == '_' || c == '.' || c == ':',
                "-",
            );
            [lower, dashed]
        })
        .collect()
}

/// Adaptive thinking support (Opus 4.6+, Sonnet 4.6).
pub fn supports_adaptive_thinking(model_id: &str, model_name: Option<&str>) -> bool {
    get_model_match_candidates(model_id, model_name)
        .iter()
        .any(|s| {
            s.contains("opus-4-6")
                || s.contains("opus-4-7")
                || s.contains("opus-4-8")
                || s.contains("opus-5")
                || s.contains("sonnet-4-6")
                || s.contains("sonnet-5")
                || s.contains("fable-5")
                || s.contains("mythos-5")
                || s.contains("mythos-preview")
        })
}

/// Fable/Mythos models — and Claude Opus 5.5 — think every turn and reject sampling params with a
/// 400.
pub fn supports_always_on_adaptive_thinking(model_id: &str, model_name: Option<&str>) -> bool {
    get_model_match_candidates(model_id, model_name)
        .iter()
        .any(|s| {
            s.contains("fable-5")
                || s.contains("mythos-5")
                || s.contains("mythos-preview")
                || s.contains("opus-5-5")
                || s.contains("opus-5.5")
        })
}

pub fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

pub fn supports_prompt_caching(model: &Model) -> bool {
    let candidates = get_model_match_candidates(&model.id, Some(&model.name));
    let has_claude_ref = candidates.iter().any(|s| s.contains("claude"));
    if !has_claude_ref {
        // Application inference profiles don't contain the model name in the ARN. Allow users to
        // force cache points via environment variable.
        return std::env::var("AWS_BEDROCK_FORCE_CACHE").as_deref() == Ok("1");
    }
    // Catalog metadata first: Bedrock bills cache writes only for models that take cache points,
    // so a Claude entry the catalog prices cache writes for supports them.
    if model.cost.cache_write.as_f64() > 0.0 {
        return true;
    }
    candidates.iter().any(|s| {
        CACHEABLE_CURRENT_RELEASE.is_match(s)
            || s.contains("-4-")
            || s.contains("claude-3-7-sonnet")
            || s.contains("claude-3-5-haiku")
    })
}

/// The documented Claude 5 / Mythos releases that take Bedrock cache points
/// (<https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html>), matched against
/// the dashed match candidates. The list stays explicit: a future major or minor is not capability
/// evidence. A release ends the candidate, or is followed by a version (`-v1`), a date
/// (`-20250929`), or a parenthesised qualifier (`Claude Fable 5.1 (Global)`).
static CACHEABLE_CURRENT_RELEASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[./-])claude-(?:opus-5(?:-5)?|sonnet-5|(?:fable|mythos)-5(?:-1)?|mythos-preview)(?:$|-(?:v\d+|20\d{6})(?:-|$)|-\()",
    )
    .expect("the cacheable-release pattern compiles")
});

pub fn supports_thinking_signature(model: &Model) -> bool {
    is_anthropic_claude_model(model)
}

/// Bedrock tool-use IDs are `[a-zA-Z0-9_-]{1,64}`.
pub fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() > 64 {
        sanitized[..64].to_string()
    } else {
        sanitized
    }
}

/// Validates the mime type and passes the base64 bytes straight through (the AWS JSON protocol
/// transmits blobs as base64). An image Bedrock cannot take fails the request (TS
/// `createImageBlock` throws, and the stream reports it as its error event).
fn create_image_block(mime_type: &str, data: &str) -> Result<Value, ProviderError> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => {
            return Err(ProviderError::Message(format!(
                "Unknown image type: {other}"
            )))
        }
    };
    // Fail fast on invalid base64 so the request never leaves with bad bytes.
    if base64::engine::general_purpose::STANDARD
        .decode(data)
        .is_err()
    {
        return Err(ProviderError::Message(
            "Invalid base64 image data for Bedrock image block".to_string(),
        ));
    }
    Ok(json!({
        "format": format,
        "source": { "bytes": data },
    }))
}

pub fn build_system_prompt(
    system_prompt: Option<&str>,
    model: &Model,
    cache_retention: CacheRetention,
) -> Option<Vec<Value>> {
    let system_prompt = system_prompt?;
    let mut blocks = vec![json!({ "text": sanitize_surrogates(system_prompt) })];

    if cache_retention != CacheRetention::None && supports_prompt_caching(model) {
        let mut cache_point = Map::new();
        cache_point.insert("type".into(), json!("default"));
        if cache_retention == CacheRetention::Long {
            cache_point.insert("ttl".into(), json!("1h"));
        }
        blocks.push(json!({ "cachePoint": Value::Object(cache_point) }));
    }

    Some(blocks)
}

/// # Errors
///
/// An image block Bedrock cannot take (see [`create_image_block`]).
// Long by design: mirrors the provider's stream shape.
#[allow(clippy::too_many_lines)]
pub fn convert_messages(
    context: &Context,
    model: &Model,
    cache_retention: CacheRetention,
) -> Result<Vec<Value>, ProviderError> {
    let mut result: Vec<Value> = Vec::new();
    let transformed = transform_messages_with_normalizer(&context.messages, model, &|id, _, _| {
        Some(normalize_tool_call_id(id))
    });

    let mut i = 0usize;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(user) => {
                let content_blocks: Vec<Value> = match &user.content {
                    UserMessageContent::Text(text) => {
                        vec![json!({ "text": sanitize_surrogates(text) })]
                    }
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|c| match crate::types::user_block_payload(c) {
                            crate::types::UserBlockPayload::Text(text) => {
                                Ok(json!({ "text": sanitize_surrogates(text) }))
                            }
                            crate::types::UserBlockPayload::Image { data, mime_type } => {
                                Ok(json!({ "image": create_image_block(mime_type, data)? }))
                            }
                            crate::types::UserBlockPayload::Opaque(json) => {
                                Ok(json!({ "text": sanitize_surrogates(&json) }))
                            }
                        })
                        .collect::<Result<_, ProviderError>>()?,
                };
                result.push(json!({ "role": "user", "content": content_blocks }));
                i += 1;
            }
            Message::Assistant(assistant) => {
                // Bedrock rejects messages with empty content arrays.
                if assistant.content.is_empty() {
                    i += 1;
                    continue;
                }
                let mut content_blocks: Vec<Value> = Vec::new();
                for c in &assistant.content {
                    match c {
                        AssistantContent::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content_blocks
                                    .push(json!({ "text": sanitize_surrogates(&text.text) }));
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            content_blocks.push(json!({
                                "toolUse": {
                                    "toolUseId": call.id,
                                    "name": call.name,
                                    "input": call.arguments,
                                }
                            }));
                        }
                        AssistantContent::Thinking(thinking) => {
                            if thinking.thinking.trim().is_empty() {
                                continue;
                            }
                            // Only Anthropic models support the signature field in reasoningText.
                            // For other models we omit it to avoid: "This model doesn't support the
                            // reasoningContent.reasoningText.signature field".
                            if supports_thinking_signature(model) {
                                // Signatures arrive after thinking deltas. If a partial or
                                // externally persisted message lacks a signature, Bedrock rejects
                                // the replayed reasoning block. Fall back to plain text, matching
                                // Anthropic.
                                let signature = thinking
                                    .thinking_signature
                                    .as_deref()
                                    .filter(|signature| !signature.trim().is_empty());
                                match signature {
                                    None => {
                                        content_blocks.push(json!({
                                            "text": sanitize_surrogates(&thinking.thinking)
                                        }));
                                    }
                                    Some(signature) => {
                                        content_blocks.push(json!({
                                            "reasoningContent": {
                                                "reasoningText": {
                                                    "text": sanitize_surrogates(&thinking.thinking),
                                                    "signature": signature,
                                                }
                                            }
                                        }));
                                    }
                                }
                            } else {
                                content_blocks.push(json!({
                                    "reasoningContent": {
                                        "reasoningText": { "text": sanitize_surrogates(&thinking.thinking) }
                                    }
                                }));
                            }
                        }
                    }
                }
                if !content_blocks.is_empty() {
                    result.push(json!({ "role": "assistant", "content": content_blocks }));
                }
                i += 1;
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages into a single user message: Bedrock
                // requires all tool results in one message.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut j = i;
                while j < transformed.len() {
                    let Message::ToolResult(current) = &transformed[j] else {
                        break;
                    };
                    tool_results.push(json!({
                        "toolResult": {
                            "toolUseId": current.tool_call_id,
                            "content": current.content.iter().map(|c| match crate::types::user_block_payload(c) {
                                crate::types::UserBlockPayload::Text(text) => {
                                    Ok(json!({ "text": sanitize_surrogates(text) }))
                                }
                                crate::types::UserBlockPayload::Image { data, mime_type } => {
                                    Ok(json!({ "image": create_image_block(mime_type, data)? }))
                                }
                                crate::types::UserBlockPayload::Opaque(json) => {
                                    Ok(json!({ "text": sanitize_surrogates(&json) }))
                                }
                            }).collect::<Result<Vec<Value>, ProviderError>>()?,
                            "status": if current.is_error { "error" } else { "success" },
                        }
                    }));
                    j += 1;
                }
                i = j;
                result.push(json!({ "role": "user", "content": tool_results }));
            }
        }
    }

    // Add a cache point to the last user message for supported Claude models when caching is
    // enabled.
    if cache_retention != CacheRetention::None
        && supports_prompt_caching(model)
        && !result.is_empty()
    {
        let last = result.last_mut().expect("checked non-empty");
        if last.get("role").and_then(Value::as_str) == Some("user") {
            let mut cache_point = Map::new();
            cache_point.insert("type".into(), json!("default"));
            if cache_retention == CacheRetention::Long {
                cache_point.insert("ttl".into(), json!("1h"));
            }
            last.get_mut("content")
                .and_then(Value::as_array_mut)
                .expect("user messages always carry content")
                .push(json!({ "cachePoint": Value::Object(cache_point) }));
        }
    }

    Ok(result)
}

pub fn convert_tool_config(
    tools: Option<&[Tool]>,
    tool_choice: Option<&BedrockToolChoice>,
) -> Option<Value> {
    let tools = tools?;
    if tools.is_empty() {
        return None;
    }
    if matches!(tool_choice, Some(BedrockToolChoice::None)) {
        return None;
    }

    let bedrock_tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": { "json": tool.parameters },
                }
            })
        })
        .collect();

    let bedrock_tool_choice = match tool_choice {
        Some(BedrockToolChoice::Auto) => Some(json!({ "auto": {} })),
        Some(BedrockToolChoice::Any) => Some(json!({ "any": {} })),
        Some(BedrockToolChoice::Tool { name }) => Some(json!({ "tool": { "name": name } })),
        _ => None,
    };

    let mut config = Map::new();
    config.insert("tools".into(), Value::Array(bedrock_tools));
    if let Some(choice) = bedrock_tool_choice {
        config.insert("toolChoice".into(), choice);
    }
    Some(Value::Object(config))
}

/// Tool selection.
#[allow(dead_code)] // full TS option surface; variants set by callers
#[derive(Clone, Debug, PartialEq)]
pub enum BedrockToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

pub fn map_thinking_level_to_effort(model: &Model, level: ModelThinkingLevel) -> &'static str {
    // Clamp to what the model actually supports so callers that bypass clampThinkingLevel can't
    // send an effort the model lacks.
    let effective = clamp_thinking_level(model, level);
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&effective))
        .and_then(std::clone::Clone::clone);
    match mapped.as_deref() {
        Some("low") => return "low",
        Some("medium") => return "medium",
        Some("high") => return "high",
        Some("xhigh") => return "xhigh",
        Some("max") => return "max",
        _ => {}
    }
    match effective {
        ModelThinkingLevel::Minimal | ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High | ModelThinkingLevel::Off => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

pub fn map_stop_reason(reason: Option<&str>) -> crate::types::StopReason {
    use crate::types::StopReason;
    match reason {
        Some("end_turn" | "stop_sequence") => StopReason::Stop,
        Some("max_tokens" | "model_context_window_exceeded") => StopReason::Length,
        Some("tool_use") => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

#[cfg(test)]
mod supports_always_on_adaptive_thinking_tests {
    use super::supports_always_on_adaptive_thinking;

    #[test]
    fn bedrock_always_on_models_reject_sampling_params() {
        assert!(supports_always_on_adaptive_thinking(
            "us.anthropic.claude-opus-5-5-v1",
            Some("Claude Opus 5.5")
        ));
        assert!(supports_always_on_adaptive_thinking(
            "anthropic.claude-fable-5",
            None
        ));
    }

    #[test]
    fn bedrock_optional_thinking_models_keep_sampling_params() {
        assert!(!supports_always_on_adaptive_thinking(
            "us.anthropic.claude-opus-5-v1",
            Some("Claude Opus 5")
        ));
    }
}

#[cfg(test)]
mod supports_prompt_caching_tests {
    use super::supports_prompt_caching;
    use crate::types::{zero_model_cost, Model, ModelCost, ModelInput};
    use pa_types::JsNumber;

    fn bedrock_model(id: &str, name: &str, cost: ModelCost) -> Model {
        Model {
            id: id.into(),
            name: name.into(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost,
            context_window: 200_000,
            max_tokens: 8192,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    /// #2548 (upstream #2978, #1082): cache points for the Claude 5 / Opus 5.x / Mythos releases,
    /// whether the id or (for an application inference profile) only the name identifies them.
    /// Every candidate here is a Claude reference, so `AWS_BEDROCK_FORCE_CACHE` is never consulted.
    #[test]
    fn claude_5_and_mythos_releases_get_cache_points_by_id_or_name() {
        let cases: &[(&str, &str, bool)] = &[
            ("us.anthropic.claude-opus-5", "Claude Opus 5", true),
            ("us.anthropic.claude-opus-5-5", "Claude Opus 5.5", true),
            ("eu.anthropic.claude-sonnet-5", "Claude Sonnet 5", true),
            ("global.anthropic.claude-fable-5", "Claude Fable 5", true),
            ("us.anthropic.claude-fable-5-1", "Claude Fable 5.1", true),
            ("us.anthropic.claude-mythos-5", "Claude Mythos 5", true),
            ("us.anthropic.claude-mythos-5-1", "Claude Mythos 5.1", true),
            (
                "us.anthropic.claude-mythos-preview",
                "Claude Mythos Preview",
                true,
            ),
            ("us.anthropic.claude-opus-5-v1:0", "profile", true),
            ("custom-profile", "Claude Fable 5.1 (Global)", true),
            (
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/test",
                "Claude Fable 5.1",
                true,
            ),
            (
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/test",
                "Claude Sonnet 4.6",
                true,
            ),
            (
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                "Claude Sonnet 4.5",
                true,
            ),
            (
                "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
                "Claude 3.7 Sonnet",
                true,
            ),
            (
                "us.anthropic.claude-3-5-haiku-20241022-v1:0",
                "Claude 3.5 Haiku",
                true,
            ),
            (
                "us.anthropic.claude-3-sonnet-20240229-v1:0",
                "Claude 3 Sonnet",
                false,
            ),
            // A future major/minor is not capability evidence without catalog pricing.
            ("us.anthropic.claude-opus-50", "Claude Opus 50", false),
            ("us.anthropic.claude-opus-5-99", "Claude Opus 5.99", false),
        ];
        let actual: Vec<(&str, &str, bool)> = cases
            .iter()
            .map(|&(id, name, _)| {
                let model = bedrock_model(id, name, zero_model_cost());
                (id, name, supports_prompt_caching(&model))
            })
            .collect();
        assert_eq!(actual, cases.to_vec());
    }

    /// Catalog metadata wins over string matching: a Claude entry the catalog prices cache
    /// writes for supports cache points even when its id matches no known family.
    #[test]
    fn catalog_cache_write_pricing_enables_cache_points_for_claude() {
        let priced = ModelCost {
            input: JsNumber(5.0),
            output: JsNumber(25.0),
            cache_read: JsNumber(0.5),
            cache_write: JsNumber(6.25),
        };
        let model = bedrock_model("us.anthropic.claude-opus-6", "Claude Opus 6", priced);
        assert!(supports_prompt_caching(&model));
    }
}
