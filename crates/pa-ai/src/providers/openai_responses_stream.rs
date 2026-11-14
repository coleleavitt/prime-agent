//! `OpenAI` Responses stream event processor: hook types and service-tier pricing live in
//! [`super::openai_responses_hooks`].
//!
//! `handle_event` dispatches each event type of the TS event match to the method that owns it.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::event_stream::{AssistantMessageEvent, AssistantMessageEventWriter};
use crate::models::calculate_cost;
pub use crate::providers::openai_responses_hooks::{
    ResponsesStreamHooks,
    apply_service_tier_pricing,
};
use crate::providers::openai_responses_shared::encode_text_signature_v1;
use crate::types::{
    AssistantContent,
    AssistantMessage,
    Model,
    StopReason,
    TextContent,
    TextSignaturePhase,
    ThinkingContent,
    ToolCall,
    Usage,
    UsageCost,
};
use crate::utils_inner::http::HttpResponse;
use crate::utils_inner::json_parse::{
    StreamingJsonAccumulator,
    parse_json_with_repair,
    parse_streaming_json,
};
use crate::utils_inner::sse::SseDecoder;
use crate::utils_inner::stream_failure::{
    ProviderError,
    StreamFailureError,
    StreamFailureInfo,
    StreamFailureKind,
    classify_stream_failure,
    open_stream_block,
    stream_drop_failure,
    stream_failure_from_stop_reason,
};

/// Streaming slot state for one output item.
struct Slot {
    /// The provider's item object (reasoning/message), kept for replay.
    item: Value,
    /// Scratch for tool-call argument accumulation.
    partial_json: StreamingJsonAccumulator,
    content_index: usize,
}

/// Incremental Responses stream event processor (port of `processResponsesStream`).
pub struct ResponsesStreamProcessor<'a> {
    model: &'a Model,
    output: &'a mut AssistantMessage,
    writer: &'a AssistantMessageEventWriter,
    hooks: ResponsesStreamHooks,
    slots: HashMap<u64, Slot>,
    current_output_index: Option<u64>,
    saw_terminal_response: bool,
}

impl<'a> ResponsesStreamProcessor<'a> {
    pub fn new(
        model: &'a Model,
        output: &'a mut AssistantMessage,
        writer: &'a AssistantMessageEventWriter,
        hooks: ResponsesStreamHooks,
    ) -> Self {
        Self {
            model,
            output,
            writer,
            hooks,
            slots: HashMap::new(),
            current_output_index: None,
            saw_terminal_response: false,
        }
    }

    fn block_kind(&self, content_index: usize) -> Option<&'static str> {
        match self.output.content.get(content_index)? {
            AssistantContent::Text(_) => Some("text"),
            AssistantContent::Thinking(_) => Some("thinking"),
            AssistantContent::ToolCall(_) => Some("toolCall"),
        }
    }

    fn current_slot(&mut self, output_index: Option<u64>) -> Option<&mut Slot> {
        match output_index.or(self.current_output_index) {
            Some(index) => self.slots.get_mut(&index),
            None => None,
        }
    }

    /// Process one parsed stream event. Errors mirror the TS thrown `StreamFailureError`s.
    pub fn handle_event(&mut self, event: &Value) -> Result<(), ProviderError> {
        let output_index = event
            .get("output_index")
            .and_then(serde_json::Value::as_u64);
        if event.get("output_index").is_some() {
            self.current_output_index = output_index;
        }

        match str_field(event, "type") {
            "response.created" => {
                if let Some(id) = event
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(|value| value.as_str())
                {
                    self.output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                self.start_output_item(item, output_index);
            }
            "response.reasoning_summary_part.added" => {
                self.add_item_part(output_index, "reasoning", "summary", event.get("part"));
            }
            "response.reasoning_summary_text.delta" => {
                self.append_reasoning_summary_text(output_index, str_field(event, "delta"));
            }
            "response.reasoning_summary_part.done" => self.end_reasoning_summary_part(output_index),
            "response.reasoning_text.delta" => {
                if let Some(content_index) = self.slot_block(output_index, "thinking") {
                    self.push_thinking_delta(content_index, str_field(event, "delta"));
                }
            }
            "response.content_part.added" => {
                let part = event.get("part");
                let part_type = part.map_or("", |part| str_field(part, "type"));
                if part_type == "output_text" || part_type == "refusal" {
                    self.add_item_part(output_index, "message", "content", part);
                }
            }
            "response.output_text.delta" => {
                self.append_message_text(
                    output_index,
                    "output_text",
                    "text",
                    str_field(event, "delta"),
                );
            }
            "response.refusal.delta" => {
                self.append_message_text(
                    output_index,
                    "refusal",
                    "refusal",
                    str_field(event, "delta"),
                );
            }
            "response.function_call_arguments.delta" => {
                self.append_function_call_arguments(output_index, str_field(event, "delta"));
            }
            "response.function_call_arguments.done" => {
                self.finish_function_call_arguments(output_index, str_field(event, "arguments"));
            }
            "response.output_item.done" => {
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                self.finish_output_item(&item, output_index);
            }
            "response.completed" | "response.incomplete" => {
                self.saw_terminal_response = true;
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                self.settle_response(&response);
            }
            "error" => return Err(error_event_failure(event)),
            "response.failed" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                return Err(failed_response_failure(&response));
            }
            _ => {}
        }

        Ok(())
    }

    /// The current slot's content index when its block is of `kind`.
    fn slot_block(&mut self, output_index: Option<u64>, kind: &str) -> Option<usize> {
        let content_index = self.current_slot(output_index)?.content_index;
        (self.block_kind(content_index) == Some(kind)).then_some(content_index)
    }

    fn push_thinking_delta(&mut self, content_index: usize, delta: &str) {
        if let Some(AssistantContent::Thinking(block)) = self.output.content.get_mut(content_index)
        {
            block.thinking.push_str(delta);
        }
        self.writer.push(AssistantMessageEvent::ThinkingDelta {
            content_index: content_index as u64,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    fn push_text_delta(&mut self, content_index: usize, delta: &str) {
        if let Some(AssistantContent::Text(block)) = self.output.content.get_mut(content_index) {
            block.text.push_str(delta);
        }
        self.writer.push(AssistantMessageEvent::TextDelta {
            content_index: content_index as u64,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    /// `response.output_item.added`: open the item's content block and its slot.
    fn start_output_item(&mut self, item: Value, output_index: Option<u64>) {
        let content_index = self.output.content.len();
        let (block, partial_json) = match str_field(&item, "type") {
            "reasoning" => (
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                    rest: Map::default(),
                }),
                StreamingJsonAccumulator::default(),
            ),
            "message" => (
                AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                    rest: Map::default(),
                }),
                StreamingJsonAccumulator::default(),
            ),
            "function_call" => (
                AssistantContent::ToolCall(ToolCall {
                    id: tool_call_id(&item),
                    name: str_field(&item, "name").to_string(),
                    arguments: Map::default(),
                    thought_signature: None,
                    rest: Map::default(),
                }),
                StreamingJsonAccumulator::new(str_field(&item, "arguments").to_string()),
            ),
            _ => return,
        };
        let start: fn(u64, AssistantMessage) -> AssistantMessageEvent = match &block {
            AssistantContent::Thinking(_) => {
                |content_index, partial| AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial,
                }
            }
            AssistantContent::Text(_) => {
                |content_index, partial| AssistantMessageEvent::TextStart {
                    content_index,
                    partial,
                }
            }
            AssistantContent::ToolCall(_) => {
                |content_index, partial| AssistantMessageEvent::ToolcallStart {
                    content_index,
                    partial,
                }
            }
        };
        self.output.content.push(block);
        self.writer
            .push(start(content_index as u64, self.output.clone()));
        if let Some(index) = output_index {
            self.slots.insert(
                index,
                Slot {
                    item,
                    partial_json,
                    content_index,
                },
            );
        }
    }

    /// A part added to the open item of `item_type`: kept on the item's `list`
    /// for replay.
    fn add_item_part(
        &mut self,
        output_index: Option<u64>,
        item_type: &str,
        list: &str,
        part: Option<&Value>,
    ) {
        let Some(slot) = self.current_slot(output_index) else {
            return;
        };
        if str_field(&slot.item, "type") != item_type {
            return;
        }
        if let Some(parts) = item_parts(&mut slot.item, list) {
            parts.push(part.cloned().unwrap_or(Value::Null));
        }
    }

    /// `response.reasoning_summary_part.done`: a blank line closes the summary
    /// part, on the item and in the thinking block.
    fn end_reasoning_summary_part(&mut self, output_index: Option<u64>) {
        let Some(content_index) = self.slot_block(output_index, "thinking") else {
            return;
        };
        if let Some(slot) = self.current_slot(output_index)
            && let Some(Value::String(text)) = item_parts(&mut slot.item, "summary")
                .and_then(|summary| summary.last_mut())
                .and_then(|last_part| last_part.get_mut("text"))
        {
            text.push_str("\n\n");
        }
        self.push_thinking_delta(content_index, "\n\n");
    }

    /// `response.reasoning_summary_text.delta`: grow the open summary part and the thinking block.
    fn append_reasoning_summary_text(&mut self, output_index: Option<u64>, delta: &str) {
        let Some(content_index) = self.slot_block(output_index, "thinking") else {
            return;
        };
        let appended = self.current_slot(output_index).is_some_and(|slot| {
            let Some(last_part) = slot
                .item
                .get_mut("summary")
                .and_then(Value::as_array_mut)
                .and_then(|parts| parts.last_mut())
            else {
                return false;
            };
            match last_part
                .as_object_mut()
                .expect("part is an object")
                .get_mut("text")
            {
                Some(Value::String(text)) => {
                    text.push_str(delta);
                    true
                }
                _ => false,
            }
        });
        if appended {
            self.push_thinking_delta(content_index, delta);
        }
    }

    /// `response.output_text.delta` / `response.refusal.delta`: grow the open message part of
    /// `part_type` (its `field`) and the text block.
    fn append_message_text(
        &mut self,
        output_index: Option<u64>,
        part_type: &str,
        field: &str,
        delta: &str,
    ) {
        let Some(content_index) = self.slot_block(output_index, "text") else {
            return;
        };
        let appended = self.current_slot(output_index).is_some_and(|slot| {
            let last_part = slot
                .item
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .and_then(|parts| parts.last_mut())
                .filter(|last_part| {
                    last_part.get("type").and_then(Value::as_str) == Some(part_type)
                });
            match last_part.and_then(|last_part| last_part.get_mut(field)) {
                Some(Value::String(text)) => {
                    text.push_str(delta);
                    true
                }
                _ => false,
            }
        });
        if appended {
            self.push_text_delta(content_index, delta);
        }
    }

    /// `response.function_call_arguments.delta`: accumulate the arguments and re-parse them.
    fn append_function_call_arguments(&mut self, output_index: Option<u64>, delta: &str) {
        let Some(content_index) = self.slot_block(output_index, "toolCall") else {
            return;
        };
        let parsed = self
            .current_slot(output_index)
            .and_then(|slot| slot.partial_json.append(delta));
        if let Some(parsed) = parsed
            && let Some(AssistantContent::ToolCall(tool_call)) =
                self.output.content.get_mut(content_index)
        {
            tool_call.arguments = parsed.as_object().cloned().unwrap_or_default();
        }
        self.writer.push(AssistantMessageEvent::ToolcallDelta {
            content_index: content_index as u64,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    /// `response.function_call_arguments.done`: take the final arguments, emitting the part of
    /// them the deltas did not deliver.
    fn finish_function_call_arguments(&mut self, output_index: Option<u64>, arguments: &str) {
        let Some(content_index) = self.slot_block(output_index, "toolCall") else {
            return;
        };
        let previous_partial = self
            .current_slot(output_index)
            .map_or_else(String::new, |slot| {
                let previous = slot.partial_json.text().to_string();
                slot.partial_json = StreamingJsonAccumulator::new(arguments.to_string());
                previous
            });
        let parsed = parse_streaming_json(Some(arguments));
        if let Some(AssistantContent::ToolCall(tool_call)) =
            self.output.content.get_mut(content_index)
        {
            tool_call.arguments = parsed.as_object().cloned().unwrap_or_default();
        }
        if let Some(delta) = arguments.strip_prefix(&previous_partial)
            && !delta.is_empty()
        {
            self.writer.push(AssistantMessageEvent::ToolcallDelta {
                content_index: content_index as u64,
                delta: delta.to_string(),
                partial: self.output.clone(),
            });
        }
    }

    /// `response.output_item.done`: close the item's slot and settle its content block.
    fn finish_output_item(&mut self, item: &Value, output_index: Option<u64>) {
        let (content_index, accumulated_arguments) = match self.current_slot(output_index) {
            Some(slot) => (
                Some(slot.content_index),
                slot.partial_json.text().to_string(),
            ),
            None => (None, String::new()),
        };
        if let Some(index) = output_index {
            self.slots.remove(&index);
        }
        let Some(content_index) = content_index else {
            return;
        };
        match str_field(item, "type") {
            "reasoning" => self.finish_reasoning(item, content_index),
            "message" => self.finish_message(item, content_index),
            "function_call" => {
                self.finish_function_call(item, content_index, &accumulated_arguments);
            }
            _ => {}
        }
    }

    fn finish_reasoning(&mut self, item: &Value, content_index: usize) {
        if self.block_kind(content_index) != Some("thinking") {
            return;
        }
        let summary_text = joined_part_texts(item, "summary");
        let content_text = joined_part_texts(item, "content");
        let final_text = if !summary_text.is_empty() {
            summary_text
        } else if !content_text.is_empty() {
            content_text
        } else {
            match self.output.content.get(content_index) {
                Some(AssistantContent::Thinking(block)) => block.thinking.clone(),
                _ => String::new(),
            }
        };
        if let Some(AssistantContent::Thinking(block)) = self.output.content.get_mut(content_index)
        {
            block.thinking.clone_from(&final_text);
            block.thinking_signature = Some(item.to_string());
        }
        self.writer.push(AssistantMessageEvent::ThinkingEnd {
            content_index: content_index as u64,
            content: final_text,
            partial: self.output.clone(),
        });
    }

    fn finish_message(&mut self, item: &Value, content_index: usize) {
        if self.block_kind(content_index) != Some("text") {
            return;
        }
        let text = item
            .get("content")
            .and_then(|content| content.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .map(
                        |part| match part.get("type").and_then(|value| value.as_str()) {
                            Some("output_text") => str_field(part, "text"),
                            _ => str_field(part, "refusal"),
                        },
                    )
                    .collect::<String>()
            })
            .unwrap_or_default();
        let phase =
            item.get("phase")
                .and_then(|value| value.as_str())
                .and_then(|phase| match phase {
                    "commentary" => Some(TextSignaturePhase::Commentary),
                    "final_answer" => Some(TextSignaturePhase::FinalAnswer),
                    _ => None,
                });
        let signature = encode_text_signature_v1(str_field(item, "id"), phase);
        if let Some(AssistantContent::Text(block)) = self.output.content.get_mut(content_index) {
            block.text.clone_from(&text);
            block.text_signature = Some(signature);
        }
        self.writer.push(AssistantMessageEvent::TextEnd {
            content_index: content_index as u64,
            content: text,
            partial: self.output.clone(),
        });
    }

    /// Settle a `function_call` item: replace its live block, or append a synthesized tool call
    /// when the slot's block is not a tool call.
    fn finish_function_call(&mut self, item: &Value, content_index: usize, accumulated: &str) {
        let name = str_field(item, "name").to_string();
        if self.block_kind(content_index) != Some("toolCall") {
            let arguments_text = item
                .get("arguments")
                .and_then(|value| value.as_str())
                .unwrap_or("{}");
            let tool_call = ToolCall {
                id: tool_call_id(item),
                name,
                arguments: parse_streaming_json(Some(arguments_text))
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                thought_signature: None,
                rest: Map::default(),
            };
            self.output
                .content
                .push(AssistantContent::ToolCall(tool_call.clone()));
            self.writer.push(AssistantMessageEvent::ToolcallEnd {
                content_index: (self.output.content.len() - 1) as u64,
                tool_call,
                partial: self.output.clone(),
            });
            return;
        }
        let arguments_text = item
            .get("arguments")
            .and_then(|value| value.as_str())
            .filter(|text| !text.is_empty())
            .unwrap_or(accumulated);
        let arguments = if arguments_text.is_empty() {
            json!({})
        } else {
            parse_streaming_json(Some(arguments_text))
        };
        let tool_call = ToolCall {
            id: tool_call_id(item),
            name,
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            thought_signature: None,
            rest: Map::default(),
        };
        if let Some(AssistantContent::ToolCall(block)) = self.output.content.get_mut(content_index)
        {
            *block = tool_call.clone();
        }
        self.writer.push(AssistantMessageEvent::ToolcallEnd {
            content_index: content_index as u64,
            tool_call,
            partial: self.output.clone(),
        });
    }

    /// `response.completed` / `response.incomplete`: response id, usage, cost and stop reason.
    fn settle_response(&mut self, response: &Value) {
        if self.model.provider == "xai" {
            self.merge_xai_encrypted_reasoning(response);
        }
        if let Some(id) = response.get("id").and_then(|value| value.as_str()) {
            self.output.response_id = Some(id.to_string());
        }
        if let Some(usage) = response.get("usage") {
            self.output.usage = responses_usage(usage);
        }
        calculate_cost(self.model, &mut self.output.usage, None);
        if let Some(apply) = &self.hooks.apply_service_tier_pricing {
            let response_tier = response
                .get("service_tier")
                .and_then(|value| value.as_str())
                .map(std::string::ToString::to_string);
            let request_tier = self.hooks.request_service_tier.map(|tier| {
                serde_json::to_value(tier)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            });
            let service_tier = match &self.hooks.resolve_service_tier {
                Some(resolve) => resolve(response_tier, request_tier),
                None => response_tier.or(request_tier),
            };
            apply(&mut self.output.usage, service_tier);
        }
        let status = response.get("status").and_then(|value| value.as_str());
        self.output.stop_reason = map_responses_stop_reason(status);
        let has_tool_call = self
            .output
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)));
        if has_tool_call && self.output.stop_reason == StopReason::Stop {
            self.output.stop_reason = StopReason::ToolUse;
        }
        if self.output.stop_reason == StopReason::Error
            && let Some(status) = status
        {
            self.output.stop_reason_raw = Some(status.to_string());
        }
    }

    /// xAI delivers reasoning's `encrypted_content` only on the terminal response: copy it into
    /// the stored signature of the thinking block with the same item id.
    fn merge_xai_encrypted_reasoning(&mut self, response: &Value) {
        let Some(output_items) = response.get("output").and_then(|value| value.as_array()) else {
            return;
        };
        for item in output_items {
            if item.get("type").and_then(|value| value.as_str()) != Some("reasoning") {
                continue;
            }
            let encrypted = item.get("encrypted_content");
            if encrypted.is_none() || encrypted == Some(&Value::Null) {
                continue;
            }
            let item_id = str_field(item, "id");
            for block in &mut self.output.content {
                let AssistantContent::Thinking(thinking) = block else {
                    continue;
                };
                let Some(signature) = &thinking.thinking_signature else {
                    continue;
                };
                let Ok(mut stored) = serde_json::from_str::<Value>(signature) else {
                    continue;
                };
                if stored.get("id").and_then(|value| value.as_str()) == Some(item_id)
                    && stored.get("encrypted_content").is_none()
                {
                    if let Some(object) = stored.as_object_mut() {
                        object.insert(
                            "encrypted_content".into(),
                            encrypted.cloned().unwrap_or(Value::Null),
                        );
                    }
                    thinking.thinking_signature = Some(stored.to_string());
                }
            }
        }
    }

    /// One SSE event's data: blank data is skipped, the rest parsed (a
    /// malformed body repaired) and handled.
    fn handle_sse_data(&mut self, data: &str) -> Result<(), ProviderError> {
        if data.trim().is_empty() {
            return Ok(());
        }
        let event = match parse_json_with_repair(data) {
            Ok(event) => event,
            Err(_) => parse_streaming_json(Some(data)),
        };
        self.handle_event(&event)
    }

    /// Terminal stop reason of the message under construction (read through the processor so
    /// error-path checks can run while the borrow of the output message lives here).
    pub fn stop_reason(&self) -> StopReason {
        self.output.stop_reason
    }

    /// Raw stop-reason string of the message under construction.
    pub fn stop_reason_raw(&self) -> Option<&str> {
        self.output.stop_reason_raw.as_deref()
    }

    /// The TS catch settle: finalize tool-call blocks whose parsed preview may lag the accumulated
    /// text under the growth throttle. The outer providers call this on their error paths before
    /// the error event carries the message.
    pub fn settle_partial_tool_calls(&mut self) {
        for slot in self.slots.values_mut() {
            let Some(parsed) = slot.partial_json.flush() else {
                continue;
            };
            if let Some(AssistantContent::ToolCall(tool_call)) =
                self.output.content.get_mut(slot.content_index)
            {
                tool_call.arguments = parsed.as_object().cloned().unwrap_or_default();
            }
        }
    }

    /// Check invariants after the stream ended (port of the trailing checks).
    ///
    /// A stream that ends without `response.completed` / `response.incomplete` was cut off
    /// mid-response: it fails closed instead of settling the partial message as a normal stop
    /// (#1623, #2089). xAI keeps the TS failure text; every other Responses provider (`openai`,
    /// `openai-codex`, `azure-openai-responses`) reports the retryable `stream_drop` class, like the completions path.
    pub fn finish(&self) -> Result<(), ProviderError> {
        if self.saw_terminal_response {
            return Ok(());
        }
        if self.model.provider == "xai" {
            return Err(ProviderError::StreamFailure(StreamFailureError {
                message: "xAI Responses stream ended before a terminal response event".to_string(),
                info: StreamFailureInfo {
                    kind: StreamFailureKind::Unknown,
                    ..StreamFailureInfo::unknown()
                },
            }));
        }
        Err(ProviderError::StreamFailure(stream_drop_failure(
            open_stream_block(self.output),
        )))
    }
}

/// Run a Responses SSE body through a processor. Like the TS try/catch, this
/// covers the streaming section and the abort and stop-reason checks; on a
/// failure the partial tool calls settle before the error event carries the
/// message.
pub(crate) async fn process_responses_sse(
    model: &Model,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
    hooks: ResponsesStreamHooks,
    response: &mut HttpResponse,
    signal: Option<&CancellationToken>,
    request_id: Option<&str>,
) -> Result<(), ProviderError> {
    let mut processor = ResponsesStreamProcessor::new(model, output, writer, hooks);
    let result = async {
        let mut decoder = SseDecoder::new();
        while let Some(chunk) = response.next_text().await? {
            for sse in decoder.push_text(&chunk) {
                processor.handle_sse_data(&sse.data)?;
            }
        }
        for sse in decoder.finish() {
            processor.handle_sse_data(&sse.data)?;
        }
        processor.finish()?;
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(ProviderError::Aborted);
        }
        if matches!(
            processor.stop_reason(),
            StopReason::Aborted | StopReason::Error
        ) {
            return Err(ProviderError::StreamFailure(
                stream_failure_from_stop_reason(processor.stop_reason_raw(), request_id),
            ));
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        processor.settle_partial_tool_calls();
    }
    result
}

fn map_responses_stop_reason(status: Option<&str>) -> StopReason {
    match status {
        None | Some("completed" | "in_progress" | "queued") => StopReason::Stop,
        Some("incomplete") => StopReason::Length,
        Some(_) => StopReason::Error,
    }
}

/// A string field of a stream object, `""` when absent or not a string.
fn str_field<'v>(value: &'v Value, key: &str) -> &'v str {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

/// The item's `key` list (created empty when absent) for appending stream parts.
fn item_parts<'v>(item: &'v mut Value, key: &str) -> Option<&'v mut Vec<Value>> {
    item.as_object_mut()
        .expect("item is an object")
        .entry(key.to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
}

/// The `text` of each part in the item's `key` list, joined by blank lines.
fn joined_part_texts(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(|parts| parts.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// A function-call item's tool call id: `<call_id>|<item id>`.
fn tool_call_id(item: &Value) -> String {
    format!("{}|{}", str_field(item, "call_id"), str_field(item, "id"))
}

/// The usage of a terminal response. `OpenAI` includes cached tokens in `input_tokens`;
/// they are subtracted.
fn responses_usage(usage: &Value) -> Usage {
    let tokens = |value: Option<&Value>| value.and_then(serde_json::Value::as_u64).unwrap_or(0);
    let cached_tokens = tokens(
        usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens")),
    );
    Usage {
        input: tokens(usage.get("input_tokens")).saturating_sub(cached_tokens),
        output: tokens(usage.get("output_tokens")),
        cache_read: cached_tokens,
        cache_write: 0,
        total_tokens: tokens(usage.get("total_tokens")),
        cost: UsageCost::default(),
    }
}

/// The failure an `error` stream event reports.
fn error_event_failure(event: &Value) -> ProviderError {
    let code = event.get("code").and_then(|value| value.as_str());
    ProviderError::StreamFailure(StreamFailureError {
        message: format!(
            "Error Code {}: {}",
            code.unwrap_or_default(),
            str_field(event, "message")
        ),
        info: StreamFailureInfo {
            kind: classify_stream_failure(code, None),
            provider_error_type: code.map(std::string::ToString::to_string),
            ..StreamFailureInfo::unknown()
        },
    })
}

/// The failure a `response.failed` event's response reports.
fn failed_response_failure(response: &Value) -> ProviderError {
    let error = response.get("error");
    let reason = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(|value| value.as_str());
    let provider_error_type = error
        .and_then(|error| error.get("code"))
        .and_then(|value| value.as_str())
        .or(reason);
    let message = if let Some(error) = error {
        format!(
            "{}: {}",
            error
                .get("code")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            error
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("no message")
        )
    } else if let Some(reason) = reason {
        format!("incomplete: {reason}")
    } else {
        "Unknown error (no error details in response)".to_string()
    };
    ProviderError::StreamFailure(StreamFailureError {
        message,
        info: StreamFailureInfo {
            kind: classify_stream_failure(provider_error_type, None),
            provider_error_type: provider_error_type.map(std::string::ToString::to_string),
            ..StreamFailureInfo::unknown()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::{create_assistant_message_event_stream, initial_assistant_message};
    use crate::providers::openai_responses_shared::{
        ConvertResponsesMessagesOptions,
        OPENAI_TOOL_CALL_PROVIDERS,
        convert_responses_messages,
    };
    use crate::types::{Context, Message};

    #[test]
    fn completed_reasoning_is_preserved_for_replay() {
        let model: Model = serde_json::from_value(json!({
            "id": "gpt-5", "name": "GPT-5", "provider": "openai", "api": "openai-responses",
            "baseUrl": "https://unused.invalid", "reasoning": true, "input": ["text"],
            "contextWindow": 10000, "maxTokens": 100,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
        }))
        .unwrap();
        let mut output = initial_assistant_message(&model.api, &model.provider, &model.id);
        let (writer, _stream) = create_assistant_message_event_stream();
        let reasoning = json!({
            "id": "rs_test", "type": "reasoning", "status": "completed",
            "summary": [{"type": "summary_text", "text": "A reasoning summary."}],
            "encrypted_content": "opaque-reasoning"
        });
        {
            let mut processor = ResponsesStreamProcessor::new(
                &model,
                &mut output,
                &writer,
                ResponsesStreamHooks::default(),
            );
            for event in [
                json!({"type": "response.output_item.added", "output_index": 0,
                    "item": {"id": "rs_test", "type": "reasoning", "summary": []}}),
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "summary_index": 0, "delta": "A reasoning summary."}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
            ] {
                processor.handle_event(&event).unwrap();
            }
        }
        let replay = convert_responses_messages(
            &model,
            &Context {
                system_prompt: None,
                messages: vec![Message::Assistant(output)],
                tools: None,
            },
            &OPENAI_TOOL_CALL_PROVIDERS,
            ConvertResponsesMessagesOptions::default(),
        );
        assert_eq!(replay, vec![reasoning]);
    }

    #[test]
    fn function_call_done_keeps_accumulated_arguments() {
        let model: Model = serde_json::from_value(json!({
            "id": "gpt-5", "name": "GPT-5", "provider": "openai", "api": "openai-responses",
            "baseUrl": "https://unused.invalid", "reasoning": true, "input": ["text"],
            "contextWindow": 10000, "maxTokens": 100,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
        }))
        .unwrap();
        let mut output = initial_assistant_message(&model.api, &model.provider, &model.id);
        let (writer, _stream) = create_assistant_message_event_stream();
        {
            let mut processor = ResponsesStreamProcessor::new(
                &model,
                &mut output,
                &writer,
                ResponsesStreamHooks::default(),
            );
            for event in [
                json!({"type": "response.output_item.added", "output_index": 0,
                    "item": {"id": "fc_test", "type": "function_call", "call_id": "call_test",
                        "name": "get_weather", "arguments": ""}}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                    "delta": "{\"city\":\"Par"}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                    "delta": "is\"}"}),
                json!({"type": "response.output_item.done", "output_index": 0,
                    "item": {"id": "fc_test", "type": "function_call", "call_id": "call_test",
                        "name": "get_weather", "arguments": ""}}),
            ] {
                processor.handle_event(&event).unwrap();
            }
        }
        match &output.content[0] {
            AssistantContent::ToolCall(tool_call) => {
                assert_eq!(tool_call.name, "get_weather");
                assert_eq!(
                    tool_call.arguments,
                    json!({"city": "Paris"}).as_object().cloned().unwrap()
                );
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }
}
