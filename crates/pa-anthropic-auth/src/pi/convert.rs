//! pi's Messages body for a conversation (anthropic-auth `packages/pi`
//! `convert.ts`, `buildAnthropicRequest`): pi's own message converter in
//! place of the generic Anthropic one, the system-prompt split, the billing
//! block, the thinking shape, the cache breakpoints and cache mode, and
//! `metadata.user_id`.
//!
//! The body keeps pi's object key order (`body`); `body_text` is what pi
//! sends when nothing changes the body afterwards: Claude Code's key order,
//! `JSON.stringify` bytes, the billing block's `cch` slot normalized
//! (`signRequestBody`).

use anthropic::cch::{js_json_stringify, reset_billing_header_cch};
use anthropic::claude_code::order_claude_code_body;
use anthropic::models::{ThinkingShape, clamp_effort_for_model, resolve_thinking_shape};
use pa_ai::request_hooks::RequestSource;
use pa_ai::types::{
    AssistantContent,
    AssistantMessage,
    Message,
    Tool,
    ToolResultMessage,
    UserMessageContent,
    UserOrToolContent,
};
use serde_json::{Map, Value, json};

use crate::shape::{ShapeIdentity, billing_text, metadata_user_id};

/// The paragraph of pi's system prompt Anthropic rejects in `system[]`.
const PI_DOCS_ANCHOR: &str = "Pi documentation";
/// Claude Code's own tools, sent with its casing.
const CLAUDE_CODE_TOOLS: [&str; 10] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];
/// The research tool name Anthropic reserves, and its wire alias.
pub(crate) const DEEP_RESEARCH_TOOL: &str = "deep_research";
pub(crate) const DEEP_RESEARCH_WIRE_TOOL: &str = "prime_deep_research";
/// APIs whose thinking signatures Anthropic issued.
const ANTHROPIC_SIGNATURE_APIS: [&str; 2] = ["anthropic-messages", "cortexkit-anthropic-messages"];
/// Claude Code's identity block.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// pi's `max_tokens` when the caller sets none.
const DEFAULT_MAX_TOKENS: u64 = 16_384;

/// The plugin's prompt-cache setting (`/claude-cache`; `claudeCache` in its
/// configuration).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CacheMode {
    /// pi's own breakpoints, each with a one-hour TTL.
    #[default]
    Explicit,
    /// A top-level `cache_control` only (besides pi's breakpoints).
    Automatic,
    /// A top-level `cache_control` and every breakpoint with a one-hour TTL.
    Hybrid,
}

impl CacheMode {
    /// The configured value (`explicit` for anything else, as the plugin
    /// normalizes it).
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            Some("automatic") => Self::Automatic,
            Some("hybrid") => Self::Hybrid,
            _ => Self::Explicit,
        }
    }
}

/// The plugin settings a request reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RequestSettings {
    /// `/claude-cache on`.
    pub(crate) cache_enabled: bool,
    /// `/claude-cache mode`.
    pub(crate) cache_mode: CacheMode,
    /// `/claude-fast on`.
    pub(crate) fast_mode: bool,
}

/// pi's request for one conversation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BuiltRequest {
    /// The body in pi's own key order.
    pub(crate) body: Value,
    /// What pi sends when nothing changes the body afterwards.
    pub(crate) body_text: String,
}

/// JavaScript's whitespace (`String.prototype.trim`).
fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// Whether `text.trim()` is empty in JavaScript.
fn js_blank(text: &str) -> bool {
    text.chars().all(js_whitespace)
}

/// `sanitizeToolId`: Anthropic's `^[a-zA-Z0-9_-]+$`, at most 256 long.
fn sanitize_tool_id(id: &str) -> String {
    if id.is_empty() {
        return "tool_call_unknown".to_string();
    }
    // JavaScript replaces each UTF-16 unit outside the class.
    let cleaned: String = id
        .chars()
        .flat_map(|c| {
            let keep = c.is_ascii_alphanumeric() || c == '_' || c == '-';
            let units = if keep { 1 } else { c.len_utf16() };
            std::iter::repeat_n(if keep { c } else { '_' }, units)
        })
        .collect();
    cleaned.chars().take(256).collect()
}

/// `toClaudeCodeToolName`.
pub(crate) fn to_claude_code_tool_name(name: &str) -> String {
    if name == DEEP_RESEARCH_TOOL {
        return DEEP_RESEARCH_WIRE_TOOL.to_string();
    }
    let lower = name.to_lowercase();
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.to_lowercase() == lower)
        .map_or_else(|| name.to_string(), |tool| (*tool).to_string())
}

/// `isOpenAIReasoningSignature`.
pub(crate) fn is_openai_reasoning_signature(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if value.starts_with("gAAAA") {
        return true;
    }
    if !value.starts_with('{') {
        return false;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(value) else {
        return false;
    };
    parsed.get("type").and_then(Value::as_str) == Some("reasoning")
        && parsed
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("rs_"))
        && parsed
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|content| content.starts_with("gAAAA"))
}

fn tool_call_ids(message: Option<&Message>) -> Vec<String> {
    let Some(Message::Assistant(message)) = message else {
        return Vec::new();
    };
    let mut ids: Vec<String> = Vec::new();
    for block in &message.content {
        if let AssistantContent::ToolCall(call) = block {
            let id = sanitize_tool_id(&call.id);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

fn immediate_result_ids(messages: &[Message], assistant: usize) -> Vec<String> {
    messages[assistant + 1..]
        .iter()
        .map_while(|message| match message {
            Message::ToolResult(result) => Some(sanitize_tool_id(&result.tool_call_id)),
            _ => None,
        })
        .collect()
}

/// Every tool call of the assistant message at `index` has its result
/// right after it.
fn calls_have_immediate_results(messages: &[Message], index: usize) -> bool {
    let calls = tool_call_ids(messages.get(index));
    if calls.is_empty() {
        return true;
    }
    let results = immediate_result_ids(messages, index);
    calls.iter().all(|id| results.contains(id))
}

/// A block's `type` tag, modeled or not.
fn block_type(block: &UserOrToolContent) -> Option<&str> {
    match block {
        UserOrToolContent::Text(_) => Some("text"),
        UserOrToolContent::Image(_) => Some("image"),
        UserOrToolContent::Raw(raw) => raw.get("type").and_then(Value::as_str),
    }
}

/// `convertTextAndImages`: joined text without an image, else blocks (an
/// image-only message gets a placeholder text block first).
fn convert_text_and_images(content: &[UserOrToolContent]) -> Value {
    if !content
        .iter()
        .any(|block| block_type(block) == Some("image"))
    {
        return Value::String(
            content
                .iter()
                .filter(|block| block_type(block) == Some("text"))
                .map(|block| block.text().unwrap_or("undefined").to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .filter_map(|block| {
            if block_type(block) == Some("text") {
                return Some(
                    json!({ "type": "text", "text": block.text().unwrap_or("undefined") }),
                );
            }
            let (data, mime) = match block {
                UserOrToolContent::Image(image) => {
                    (Some(image.data.as_str()), Some(image.mime_type.as_str()))
                }
                UserOrToolContent::Raw(raw) => (
                    raw.get("data").and_then(Value::as_str),
                    raw.get("mimeType").and_then(Value::as_str),
                ),
                UserOrToolContent::Text(_) => (None, None),
            };
            let data = data.filter(|data| !data.is_empty())?;
            let mut source = Map::new();
            source.insert("type".to_string(), json!("base64"));
            if let Some(mime) = mime {
                source.insert("media_type".to_string(), json!(mime));
            }
            source.insert("data".to_string(), json!(data));
            Some(json!({ "type": "image", "source": Value::Object(source) }))
        })
        .collect();
    if !blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("text"))
    {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

/// `anthropicThinkingSignature`: a signature Anthropic issued, else none.
fn anthropic_signature<'a>(
    message: &AssistantMessage,
    signature: Option<&'a str>,
) -> Option<&'a str> {
    let signature = signature.filter(|signature| !signature.is_empty())?;
    if is_openai_reasoning_signature(Some(signature)) {
        return None;
    }
    if !ANTHROPIC_SIGNATURE_APIS.contains(&message.api.as_str()) {
        return None;
    }
    Some(signature)
}

fn convert_assistant(message: &AssistantMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            AssistantContent::Text(text) if !js_blank(&text.text) => {
                blocks.push(json!({ "type": "text", "text": text.text }));
            }
            AssistantContent::Thinking(thinking) if !js_blank(&thinking.thinking) => {
                let signature = thinking.thinking_signature.as_deref();
                if is_openai_reasoning_signature(signature) {
                    continue;
                }
                match anthropic_signature(message, signature) {
                    Some(signature) => blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking.thinking,
                        "signature": signature,
                    })),
                    None => blocks.push(json!({ "type": "text", "text": thinking.thinking })),
                }
            }
            AssistantContent::ToolCall(call) => blocks.push(json!({
                "type": "tool_use",
                "id": sanitize_tool_id(&call.id),
                "name": to_claude_code_tool_name(&call.name),
                "input": Value::Object(call.arguments.clone()),
            })),
            _ => {}
        }
    }
    blocks
}

fn tool_result_block(result: &ToolResultMessage, tool_use_id: &str) -> Value {
    let mut content = convert_text_and_images(&result.content);
    let empty = match &content {
        Value::String(text) => text.is_empty(),
        Value::Array(blocks) => blocks.is_empty(),
        _ => false,
    };
    if result.is_error && empty {
        content = json!([{ "type": "text", "text": "Error" }]);
    }
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
        "is_error": result.is_error,
    })
}

/// pi's `convertMessages`.
pub(crate) fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => {
                    if !js_blank(text) {
                        result.push(json!({ "role": "user", "content": text }));
                    }
                }
                UserMessageContent::Blocks(blocks) => {
                    result.push(
                        json!({ "role": "user", "content": convert_text_and_images(blocks) }),
                    );
                }
            },
            Message::Assistant(assistant) => {
                let has_calls = !tool_call_ids(messages.get(index)).is_empty();
                if !has_calls || calls_have_immediate_results(messages, index) {
                    let blocks = convert_assistant(assistant);
                    if !blocks.is_empty() {
                        result.push(json!({ "role": "assistant", "content": blocks }));
                    }
                }
            }
            Message::ToolResult(_) => {
                let previous = index
                    .checked_sub(1)
                    .and_then(|previous| messages.get(previous));
                let paired = matches!(previous, Some(Message::Assistant(_)))
                    && calls_have_immediate_results(messages, index - 1);
                if !paired {
                    while matches!(messages.get(index + 1), Some(Message::ToolResult(_))) {
                        index += 1;
                    }
                    index += 1;
                    continue;
                }
                let mut expected = tool_call_ids(previous);
                let mut results = Vec::new();
                while let Some(Message::ToolResult(tool_result)) = messages.get(index) {
                    let id = sanitize_tool_id(&tool_result.tool_call_id);
                    if let Some(position) = expected.iter().position(|expected| *expected == id) {
                        expected.remove(position);
                        results.push(tool_result_block(tool_result, &id));
                    }
                    index += 1;
                }
                index -= 1;
                if !results.is_empty() {
                    result.push(json!({ "role": "user", "content": results }));
                }
            }
        }
        index += 1;
    }
    result
}

/// pi's `convertTools`.
fn convert_tools(tools: Option<&[Tool]>) -> Option<Vec<Value>> {
    let tools = tools.filter(|tools| !tools.is_empty())?;
    Some(
        tools
            .iter()
            .map(|tool| {
                let field = |name: &str, default: Value| {
                    tool.parameters
                        .get(name)
                        .filter(|value| !value.is_null())
                        .cloned()
                        .unwrap_or(default)
                };
                json!({
                    "name": to_claude_code_tool_name(&tool.name),
                    "description": tool.description,
                    "input_schema": {
                        "type": "object",
                        "properties": field("properties", json!({})),
                        "required": field("required", json!([])),
                    },
                })
            })
            .collect(),
    )
}

/// pi's `splitPiSystemPrompt`: the documentation paragraphs go to the first
/// user message; the rest stays in `system[]` (an unknown prompt shape goes
/// to the message whole).
pub(crate) fn split_system_prompt(prompt: &str) -> (Option<String>, String) {
    let paragraphs = split_paragraphs(prompt);
    let (docs, keep): (Vec<&str>, Vec<&str>) = paragraphs
        .into_iter()
        .partition(|paragraph| paragraph.contains(PI_DOCS_ANCHOR));
    if docs.is_empty() {
        return (None, prompt.to_string());
    }
    let system = (!keep.is_empty()).then(|| keep.join("\n\n"));
    (system, docs.join("\n\n"))
}

/// `prompt.split(/\n\n+/)`.
fn split_paragraphs(prompt: &str) -> Vec<&str> {
    let mut paragraphs = Vec::new();
    let mut rest = prompt;
    while let Some(start) = rest.find("\n\n") {
        paragraphs.push(&rest[..start]);
        rest = rest[start..].trim_start_matches('\n');
    }
    paragraphs.push(rest);
    paragraphs
}

/// `prependCachedPromptBlock`.
fn prepend_prompt_block(messages: &mut [Value], text: &str) {
    let Some(first) = messages
        .iter_mut()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    let block = json!({ "type": "text", "text": text, "cache_control": { "type": "ephemeral" } });
    let Some(content) = first.get_mut("content") else {
        return;
    };
    match content {
        Value::String(user) => {
            let user = std::mem::take(user);
            *content = json!([block, { "type": "text", "text": user }]);
        }
        Value::Array(blocks) => blocks.insert(0, block),
        _ => {}
    }
}

/// `addEphemeralCacheControl`: the last tool, the last system block, the
/// last user message's last block (a string turn becomes one text block).
fn add_ephemeral_cache_control(body: &mut Map<String, Value>) {
    let ephemeral = || json!({ "type": "ephemeral" });
    if let Some(tool) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
        .and_then(Value::as_object_mut)
    {
        tool.insert("cache_control".to_string(), ephemeral());
    }
    if let Some(system) = body
        .get_mut("system")
        .and_then(Value::as_array_mut)
        .and_then(|system| system.last_mut())
        .and_then(Value::as_object_mut)
    {
        system.insert("cache_control".to_string(), ephemeral());
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    let Some(content) = message.get_mut("content") else {
        return;
    };
    match content {
        Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) {
                last.insert("cache_control".to_string(), ephemeral());
            }
        }
        Value::String(text) => {
            let text = std::mem::take(text);
            *content = json!([{ "type": "text", "text": text, "cache_control": ephemeral() }]);
        }
        _ => {}
    }
}

/// Every `cache_control` object anywhere in `value` gets a one-hour TTL.
fn add_ttl(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(add_ttl),
        Value::Object(map) => {
            if let Some(Value::Object(cache_control)) = map.get_mut("cache_control") {
                cache_control.insert("ttl".to_string(), json!("1h"));
            }
            map.values_mut().for_each(add_ttl);
        }
        _ => {}
    }
}

/// `applyCacheMode`.
fn apply_cache_mode(body: &mut Map<String, Value>, settings: RequestSettings) {
    if !settings.cache_enabled {
        return;
    }
    if settings.cache_mode == CacheMode::Automatic {
        body.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
        return;
    }
    if settings.cache_mode == CacheMode::Hybrid {
        body.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    }
    let mut value = Value::Object(std::mem::take(body));
    add_ttl(&mut value);
    if let Value::Object(map) = value {
        *body = map;
    }
}

/// `applyClaudeCodeMetadata`.
fn apply_metadata(body: &mut Map<String, Value>, identity: &ShapeIdentity) {
    match metadata_user_id(identity) {
        Some(user_id) => {
            let metadata = body
                .entry("metadata")
                .or_insert_with(|| Value::Object(Map::new()));
            if !metadata.is_object() {
                *metadata = Value::Object(Map::new());
            }
            if let Some(metadata) = metadata.as_object_mut() {
                metadata.insert("user_id".to_string(), Value::String(user_id));
            }
        }
        None => {
            if let Some(metadata) = body.get_mut("metadata").and_then(Value::as_object_mut) {
                metadata.remove("user_id");
            }
        }
    }
}

/// The model families pi names (anthropic-auth core `models.ts`).
pub(crate) mod family {
    fn is(model: &str, id: &str) -> bool {
        model == id
            || model
                .strip_prefix(id)
                .is_some_and(|rest| rest.starts_with('-'))
    }

    /// `isClaudeFableOrMythos5Model`.
    pub(crate) fn fable_or_mythos_5(model: &str) -> bool {
        is(model, "claude-fable-5") || is(model, "claude-mythos-5")
    }

    /// `isClaudeSonnet5Model`.
    pub(crate) fn sonnet_5(model: &str) -> bool {
        is(model, "claude-sonnet-5")
    }

    /// `isClaudeOpus5Model`: Opus 5 and its point releases (Opus 5.5).
    pub(crate) fn opus_5(model: &str) -> bool {
        is(model, "claude-opus-5")
    }

    /// `isFastModeSupportedModel`.
    pub(crate) fn fast_mode_supported(model: &str) -> bool {
        [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
        ]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    }
}

/// pi's thinking budget for a reasoning level on a manual-budget model.
fn default_budget(reasoning: &str) -> Option<u64> {
    match reasoning {
        "minimal" => Some(1024),
        "low" => Some(4096),
        "medium" => Some(10_240),
        "high" => Some(20_480),
        "xhigh" => Some(32_000),
        _ => None,
    }
}

/// pi's effort for a reasoning level on an adaptive model.
fn effort(reasoning: &str) -> &'static str {
    match reasoning {
        "minimal" | "low" => "low",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => "medium",
    }
}

/// The thinking fields (`thinking`, `output_config`).
fn apply_thinking(
    body: &mut Map<String, Value>,
    model: &str,
    source: &RequestSource<'_>,
    max_tokens: u64,
    disable_adaptive_flag: Option<&str>,
) {
    let five_series =
        family::fable_or_mythos_5(model) || family::sonnet_5(model) || family::opus_5(model);
    if five_series {
        body.insert(
            "thinking".to_string(),
            json!({ "type": "adaptive", "display": "summarized" }),
        );
    }
    // pi tests the level's string: "off" is set, and maps like any level
    // without an entry.
    let Some(reasoning) = source
        .options
        .reasoning
        .map(pa_ai::types::ModelThinkingLevel::wire_name)
    else {
        return;
    };
    if resolve_thinking_shape(model, disable_adaptive_flag) == ThinkingShape::Adaptive {
        if !five_series {
            body.insert(
                "thinking".to_string(),
                json!({ "type": "adaptive", "display": "summarized" }),
            );
        }
        body.insert(
            "output_config".to_string(),
            json!({ "effort": clamp_effort_for_model(effort(reasoning), model) }),
        );
    } else {
        let budgets = source.options.thinking_budgets.as_ref();
        let custom = budgets.and_then(|budgets| match reasoning {
            "minimal" => budgets.minimal,
            "low" => budgets.low,
            "medium" => budgets.medium,
            "high" => budgets.high,
            _ => None,
        });
        let requested = custom
            .or_else(|| default_budget(reasoning))
            .unwrap_or(10_240);
        // `Math.min(budget, max_tokens - 1)`, negative for a zero cap.
        let budget = i128::from(requested).min(i128::from(max_tokens) - 1);
        body.insert(
            "thinking".to_string(),
            json!({ "type": "enabled", "budget_tokens": i64::try_from(budget).unwrap_or(i64::MAX) }),
        );
    }
}

/// pi's `buildAnthropicRequest` for `model` over the caller's request.
pub(crate) fn build_request(
    model: &str,
    source: &RequestSource<'_>,
    settings: RequestSettings,
    identity: &ShapeIdentity,
    version: &str,
    disable_adaptive_flag: Option<&str>,
) -> BuiltRequest {
    let context = source.context;
    let mut messages = convert_messages(&context.messages);
    while messages
        .last()
        .is_some_and(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
    {
        messages.pop();
    }
    let mut system = vec![
        json!({ "type": "text", "text": billing_text(&messages, version) }),
        json!({ "type": "text", "text": CLAUDE_CODE_IDENTITY }),
    ];
    if let Some(prompt) = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !js_blank(prompt))
    {
        let (system_text, message_text) = split_system_prompt(prompt);
        if let Some(system_text) = system_text {
            system.push(json!({ "type": "text", "text": system_text }));
        }
        prepend_prompt_block(&mut messages, &message_text);
    }
    let max_tokens = source.options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = Map::new();
    body.insert("model".to_string(), json!(model));
    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), json!(true));
    body.insert("system".to_string(), Value::Array(system));
    body.insert("messages".to_string(), Value::Array(messages));
    if let Some(tools) = convert_tools(context.tools.as_deref()) {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if settings.fast_mode && family::fast_mode_supported(model) {
        body.insert("speed".to_string(), json!("fast"));
    }
    apply_thinking(&mut body, model, source, max_tokens, disable_adaptive_flag);
    add_ephemeral_cache_control(&mut body);
    apply_cache_mode(&mut body, settings);
    apply_metadata(&mut body, identity);
    let body = Value::Object(body);
    let body_text =
        reset_billing_header_cch(&js_json_stringify(&order_claude_code_body(body.clone())));
    BuiltRequest { body, body_text }
}

#[cfg(test)]
pub(crate) mod tests;
