//! Anthropic's server-side `web_search` tool (`web_search_20250305`), run
//! the way Claude Code's `WebSearch` runs it: a nested call to an Anthropic
//! Messages model with the server tool attached and forced (unless the
//! model always thinks), a short system prompt asking for one search and a
//! brief summary, and the result blocks read off the stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    SearchBackend,
    SearchError,
    SearchFuture,
    SearchHit,
    SearchItem,
    SearchOutcome,
    SearchRequest,
};
use crate::model::{CompletionHooks, ResolvedModel, complete};

/// The server tool's type.
pub const WEB_SEARCH_TOOL_TYPE: &str = "web_search_20250305";
/// Searches one call may run (Claude Code's `max_uses`).
pub const MAX_USES: u64 = 8;
/// Claude Code's system prompt for the nested search call.
pub const SEARCH_SYSTEM_PROMPT: &str = "You run a web search for another Claude model, which is waiting on the result and can search again. Search once, then reply with a brief summary of the results.";

/// The Anthropic backend: searches with `model`.
#[derive(Debug, Clone)]
pub struct AnthropicSearch {
    pub model: ResolvedModel,
}

/// The server tool entry for `request`.
#[must_use]
pub fn tool_schema(request: &SearchRequest) -> Value {
    let mut tool = json!({
        "type": WEB_SEARCH_TOOL_TYPE,
        "name": "web_search",
        "max_uses": MAX_USES,
    });
    if !request.allowed_domains.is_empty() {
        tool["allowed_domains"] = json!(request.allowed_domains);
    }
    if !request.blocked_domains.is_empty() {
        tool["blocked_domains"] = json!(request.blocked_domains);
    }
    tool
}

/// Attach the server tool to a built Messages payload. The tool is forced
/// unless the payload enables thinking (the API refuses a forced tool with
/// thinking on); a model that always thinks still searches on its own.
#[must_use]
pub fn attach_tool(mut payload: Value, tool: &Value) -> Value {
    let thinking_on = payload
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "disabled");
    payload["tools"] = json!([tool]);
    if !thinking_on {
        payload["tool_choice"] = json!({ "type": "tool", "name": "web_search" });
    }
    payload
}

/// Folds the streamed events into search items, in order: commentary text,
/// each search (its query from the `server_tool_use` input) and its hits.
#[derive(Debug, Default)]
pub struct EventCollector {
    items: Vec<SearchItem>,
    commentary: String,
    /// Content index -> the server tool use id and its input JSON so far.
    tool_inputs: HashMap<u64, (String, String)>,
    /// Server tool use id -> its query.
    queries: HashMap<String, String>,
    /// Content indexes of text blocks.
    text_blocks: std::collections::HashSet<u64>,
}

impl EventCollector {
    fn flush_commentary(&mut self) {
        let text = self.commentary.trim();
        if !text.is_empty() {
            self.items.push(SearchItem::Commentary(text.to_string()));
        }
        self.commentary.clear();
    }

    /// Observe one parsed stream event.
    pub fn observe(&mut self, event: &Value) {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "content_block_start" => {
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                match block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "server_tool_use" => {
                        self.flush_commentary();
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        // A non-streamed input arrives whole on the block.
                        let input = block
                            .get("input")
                            .filter(|input| input.as_object().is_some_and(|map| !map.is_empty()))
                            .map(Value::to_string)
                            .unwrap_or_default();
                        self.tool_inputs.insert(index, (id, input));
                    }
                    "web_search_tool_result" => self.observe_result(&block),
                    "text" => {
                        self.text_blocks.insert(index);
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            self.commentary.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "input_json_delta" => {
                        if let Some((_, input)) = self.tool_inputs.get_mut(&index) {
                            input.push_str(
                                delta
                                    .get("partial_json")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            );
                        }
                    }
                    "text_delta" if self.text_blocks.contains(&index) => {
                        self.commentary.push_str(
                            delta
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some((id, input)) = self.tool_inputs.remove(&index) {
                    let query = serde_json::from_str::<Value>(&input)
                        .ok()
                        .and_then(|input| {
                            input
                                .get("query")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });
                    if let Some(query) = query {
                        self.queries.insert(id, query);
                    }
                }
            }
            _ => {}
        }
    }

    fn observe_result(&mut self, block: &Value) {
        let tool_use_id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let query = self.queries.get(tool_use_id).cloned();
        match block.get("content") {
            Some(Value::Array(results)) => {
                let hits = results
                    .iter()
                    .filter_map(|result| {
                        let url = result.get("url").and_then(Value::as_str)?;
                        Some(SearchHit {
                            title: result
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            url: url.to_string(),
                            snippet: None,
                        })
                    })
                    .collect();
                self.items.push(SearchItem::Search {
                    query: query.unwrap_or_default(),
                    hits,
                });
            }
            Some(error) => {
                let code = error
                    .get("error_code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                self.items
                    .push(SearchItem::Error(format!("Web search error: {code}")));
            }
            None => {}
        }
    }

    /// The items in order, the trailing commentary included; searches that
    /// did not report their query carry `fallback_query`.
    #[must_use]
    pub fn finish(mut self, fallback_query: &str) -> Vec<SearchItem> {
        self.flush_commentary();
        for item in &mut self.items {
            if let SearchItem::Search { query, .. } = item {
                if query.is_empty() {
                    fallback_query.clone_into(query);
                }
            }
        }
        self.items
    }
}

impl SearchBackend for AnthropicSearch {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn search(&self, request: SearchRequest, cancel: CancellationToken) -> SearchFuture {
        let model = self.model.clone();
        Box::pin(async move {
            let tool = tool_schema(&request);
            let collector = Arc::new(Mutex::new(EventCollector::default()));
            let observer = Arc::clone(&collector);
            let hooks = CompletionHooks {
                on_payload: Some(Arc::new(move |payload, _model| {
                    Some(attach_tool(payload, &tool))
                })),
                on_event: Some(Arc::new(move |event, _model| {
                    observer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .observe(event);
                })),
            };
            let result = complete(
                &model,
                Some(SEARCH_SYSTEM_PROMPT.to_string()),
                format!("Perform a web search for the query: {}", request.query),
                None,
                hooks,
                cancel,
            )
            .await;
            let collected = std::mem::take(
                &mut *collector
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let items = collected.finish(&request.query);
            let searched = items
                .iter()
                .any(|item| matches!(item, SearchItem::Search { .. } | SearchItem::Error(_)));
            if let Err(error) = result {
                if !searched {
                    return Err(SearchError(format!(
                        "Anthropic web search failed ({}/{}): {error}",
                        model.model.provider, model.model.id
                    )));
                }
            }
            Ok(SearchOutcome {
                backend: "anthropic",
                model: Some(format!("{}/{}", model.model.provider, model.model.id)),
                items,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_is_forced_unless_thinking_is_on() {
        let request = SearchRequest {
            query: "q".into(),
            allowed_domains: vec!["docs.rs".into()],
            blocked_domains: vec![],
        };
        let tool = tool_schema(&request);
        assert_eq!(
            tool,
            json!({
                "type": "web_search_20250305", "name": "web_search", "max_uses": 8,
                "allowed_domains": ["docs.rs"],
            })
        );
        let forced = attach_tool(
            json!({ "model": "m", "thinking": { "type": "disabled" } }),
            &tool,
        );
        assert_eq!(
            forced["tool_choice"],
            json!({ "type": "tool", "name": "web_search" })
        );
        assert_eq!(forced["tools"], json!([tool]));
        let thinking = attach_tool(
            json!({ "model": "m", "thinking": { "type": "adaptive" } }),
            &tool,
        );
        assert!(thinking.get("tool_choice").is_none());
    }

    #[test]
    fn the_collector_orders_commentary_searches_and_errors() {
        let mut collector = EventCollector::default();
        let events = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me search."}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\": \"rust "}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"2026\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[
                {"type":"web_search_result","title":"Rust","url":"https://www.rust-lang.org/","encrypted_content":"x"},
                {"type":"web_search_result","title":"No url"}
            ]}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"server_tool_use","id":"srv_2","name":"web_search","input":{"query":"rust blog"}}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"content_block_start","index":4,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_2","content":{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}}}),
            json!({"type":"content_block_stop","index":4}),
            json!({"type":"content_block_start","index":5,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":5,"delta":{"type":"text_delta","text":"Rust is "}}),
            json!({"type":"content_block_delta","index":5,"delta":{"type":"citations_delta","citation":{}}}),
            json!({"type":"content_block_delta","index":5,"delta":{"type":"text_delta","text":"fast."}}),
            json!({"type":"content_block_stop","index":5}),
        ];
        for event in &events {
            collector.observe(event);
        }
        assert_eq!(
            collector.finish("rust"),
            vec![
                SearchItem::Commentary("Let me search.".into()),
                SearchItem::Search {
                    query: "rust 2026".into(),
                    hits: vec![SearchHit {
                        title: "Rust".into(),
                        url: "https://www.rust-lang.org/".into(),
                        snippet: None,
                    }],
                },
                SearchItem::Error("Web search error: max_uses_exceeded".into()),
                SearchItem::Commentary("Rust is fast.".into()),
            ]
        );
    }
}
