//! `web.search` end to end: the Anthropic backend against a local Messages
//! endpoint that streams server-tool blocks, backend selection from settings
//! (Camoufox default with an Anthropic fallback), and the handler.

mod common;

use std::sync::Arc;

use common::{Reply, serve, sse};
use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_web::config::{BackendChoice, WebSettings};
use pa_web::search::{SearchBackend, SearchRequest};
use pa_web::{SEARCH_REQUEST, WebFeature, WebOverrides, resolve_backend};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn search_stream() -> String {
    sse(&[
        json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"tokio 2026\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[
            {"type":"web_search_result","title":"Tokio","url":"https://tokio.rs/","encrypted_content":"e"},
            {"type":"web_search_result","title":"Docs","url":"https://docs.rs/tokio","encrypted_content":"e"}
        ]}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Tokio is an async runtime."}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ])
}

fn write_models(dir: &std::path::Path, base_url: &str) {
    std::fs::write(
        dir.join("models.json"),
        json!({ "providers": { "mock-anthropic": {
            "baseUrl": base_url, "apiKey": "sk-ant-api-test", "api": "anthropic-messages",
            "models": [{ "id": "mock-haiku", "name": "Mock Haiku", "contextWindow": 200_000, "maxTokens": 8192 }]
        }}})
        .to_string(),
    )
    .unwrap();
}

fn context(dir: &std::path::Path) -> SessionFeatureContext {
    SessionFeatureContext {
        agent_dir: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        session_id: "s-web".into(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    }
}

async fn search(handlers: &HostRequestHandlers, data: Value) -> anyhow::Result<Value> {
    let handler = handlers
        .get(SEARCH_REQUEST)
        .expect("web.search is registered")
        .clone();
    handler(HostRequestPayload { data }).await
}

/// The configured `searchModel` drives Anthropic's server tool: the request
/// carries the tool, its domain filter and the forced choice; the reply
/// carries the streamed results and the Sources reminder.
#[tokio::test]
async fn the_anthropic_backend_runs_the_server_tool_through_the_handler() {
    let model_server = serve(|_request| Reply::ok("text/event-stream", search_stream())).await;
    let dir = tempfile::tempdir().unwrap();
    write_models(dir.path(), &model_server.base);
    std::fs::write(
        dir.path().join("settings.json"),
        json!({ "web": { "searchModel": "mock-anthropic/mock-haiku" } }).to_string(),
    )
    .unwrap();
    let mut handlers = HostRequestHandlers::default();
    WebFeature::new().register_host_handlers(&context(dir.path()), &mut handlers);
    let reply = search(
        &handlers,
        json!({ "query": "tokio", "allowed_domains": ["tokio.rs", "docs.rs"] }),
    )
    .await
    .unwrap();
    assert_eq!(reply["backend"], json!("anthropic"));
    assert_eq!(reply["model"], json!("mock-anthropic/mock-haiku"));
    assert_eq!(
        reply["results"],
        json!([
            { "title": "Tokio", "url": "https://tokio.rs/" },
            { "title": "Docs", "url": "https://docs.rs/tokio" },
        ])
    );
    assert_eq!(reply["searches"][0]["query"], json!("tokio 2026"));
    assert_eq!(reply["commentary"], json!(["Tokio is an async runtime."]));
    assert_eq!(reply["search_count"], json!(1));
    let text = reply["text"].as_str().unwrap();
    assert!(
        text.starts_with("Web search results for query: \"tokio\"\n\nLinks: [{\"title\":\"Tokio\"")
    );
    assert!(text.ends_with("REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."));

    let request = &model_server.requests()[0];
    assert_eq!(request.header("x-api-key"), Some("sk-ant-api-test"));
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(
        body["tools"],
        json!([{
            "type": "web_search_20250305", "name": "web_search", "max_uses": 8,
            "allowed_domains": ["tokio.rs", "docs.rs"],
        }])
    );
    assert_eq!(
        body["tool_choice"],
        json!({ "type": "tool", "name": "web_search" })
    );
    assert!(
        body["messages"][0]["content"]
            .to_string()
            .contains("Perform a web search for the query: tokio")
    );
    assert!(
        body["system"]
            .to_string()
            .contains("You run a web search for another Claude model")
    );
}

#[tokio::test]
async fn a_failed_anthropic_call_is_the_handlers_error() {
    let model_server = serve(|_request| {
        Reply {
            status: 400,
            reason: "Bad Request",
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"web search is not enabled"}}"#.to_vec(),
        }
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    write_models(dir.path(), &model_server.base);
    std::fs::write(
        dir.path().join("settings.json"),
        json!({ "web": { "searchModel": "mock-anthropic/mock-haiku", "searchBackend": "anthropic" } }).to_string(),
    )
    .unwrap();
    let mut handlers = HostRequestHandlers::default();
    WebFeature::new().register_host_handlers(&context(dir.path()), &mut handlers);
    let error = search(&handlers, json!({ "query": "tokio" }))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("Anthropic web search failed (mock-anthropic/mock-haiku)"),
        "{error}"
    );
    let invalid = search(&handlers, json!({ "query": "t" }))
        .await
        .unwrap_err();
    assert!(invalid.to_string().contains("Missing query"), "{invalid}");
}

#[test]
fn explicit_backends_need_their_keys_and_a_searchable_model() {
    let dir = tempfile::tempdir().unwrap();
    write_models(dir.path(), "http://127.0.0.1:9");
    let resolve = |settings: &WebSettings| {
        resolve_backend(settings, dir.path(), dir.path(), ("p1", "m1")).map(|backend| backend.name())
    };
    // Camoufox is keyless: it always resolves, and Auto defaults to it (the
    // FallbackSearch reports its primary's name).
    let camoufox = WebSettings {
        search_backend: BackendChoice::Camoufox,
        ..WebSettings::default()
    };
    assert_eq!(resolve(&camoufox), Ok("camoufox"));
    assert_eq!(resolve(&WebSettings::default()), Ok("camoufox"));
    // Explicit Anthropic needs a searchable model.
    let anthropic = WebSettings {
        search_backend: BackendChoice::Anthropic,
        ..WebSettings::default()
    };
    assert!(resolve(&anthropic).unwrap_err().contains("login or key"));
    let configured = WebSettings {
        search_backend: BackendChoice::Anthropic,
        search_model: Some("mock-anthropic/mock-haiku".into()),
        ..WebSettings::default()
    };
    assert_eq!(resolve(&configured), Ok("anthropic"));
    let unknown = WebSettings {
        search_backend: BackendChoice::Anthropic,
        search_model: Some("nobody/none".into()),
        ..WebSettings::default()
    };
    assert!(resolve(&unknown).unwrap_err().contains("not registered"));
    let pinned = WebSettings {
        search_backend: BackendChoice::Anthropic,
        search_model: Some("mock-anthropic/mock-haiku".into()),
        allowed_models: Some(vec!["other/*".into()]),
        ..WebSettings::default()
    };
    assert!(resolve(&pinned).unwrap_err().contains("allowedModels"));
}

#[tokio::test]
async fn an_override_backend_serves_every_search() {
    struct Fixed;
    impl SearchBackend for Fixed {
        fn name(&self) -> &'static str {
            "brave"
        }
        fn search(
            &self,
            request: SearchRequest,
            _cancel: CancellationToken,
        ) -> pa_web::search::SearchFuture {
            Box::pin(async move {
                Ok(pa_web::search::SearchOutcome {
                    backend: "brave",
                    model: None,
                    items: vec![pa_web::search::SearchItem::Search {
                        query: request.query,
                        hits: vec![],
                    }],
                })
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut handlers = HostRequestHandlers::default();
    WebFeature::with_overrides(WebOverrides {
        search_backend: Some(Arc::new(Fixed)),
        ..WebOverrides::default()
    })
    .register_host_handlers(&context(dir.path()), &mut handlers);
    let reply = search(&handlers, json!({ "query": "anything" }))
        .await
        .unwrap();
    assert!(reply["text"].as_str().unwrap().contains("No links found."));
}
