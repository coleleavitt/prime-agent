//! `web.fetch` end to end through the feature's handler: a local HTTP
//! server stands in for the web and a local Anthropic Messages endpoint for
//! the answering model.

mod common;

use std::sync::{Arc, Mutex};

use common::{Reply, anthropic_model, serve, text_answer};
use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_telemetry::Properties;
use pa_web::fetch::FetchOptions;
use pa_web::model::ResolvedModel;
use pa_web::{FETCH_EVENT, FETCH_REQUEST, WebFeature, WebOverrides};
use serde_json::{Value, json};

type Tracked = Arc<Mutex<Vec<(String, Properties)>>>;

fn context(dir: &std::path::Path, tracked: &Tracked) -> SessionFeatureContext {
    let recorder = Arc::clone(tracked);
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
        telemetry: Some(FeatureTelemetry::new(move |name, properties| {
            recorder
                .lock()
                .unwrap()
                .push((name.to_string(), properties));
        })),
        rlm_depth: 0,
        session_artifact_dir: None,
    }
}

fn local_fetch() -> FetchOptions {
    FetchOptions {
        upgrade_http: false,
        allow_local: true,
        ..FetchOptions::default()
    }
}

fn handlers(
    overrides: WebOverrides,
    dir: &std::path::Path,
    tracked: &Tracked,
) -> HostRequestHandlers {
    let mut handlers = HostRequestHandlers::default();
    WebFeature::with_overrides(overrides)
        .register_host_handlers(&context(dir, tracked), &mut handlers);
    handlers
}

async fn fetch(handlers: &HostRequestHandlers, data: Value) -> anyhow::Result<Value> {
    let handler = handlers
        .get(FETCH_REQUEST)
        .expect("web.fetch is registered")
        .clone();
    handler(HostRequestPayload { data }).await
}

const PAGE: &str = "<html><head><title>Doc</title><script>steal()</script></head><body><h1>Guide</h1><p>Use <a href=\"https://x.dev/\">x</a>.</p></body></html>";

fn site() -> impl Fn(&common::Received) -> Reply + Send + Sync + 'static {
    |request| match request.target.as_str() {
        "/page" => Reply::ok("text/html; charset=utf-8", PAGE),
        "/moved" => Reply::redirect("/page"),
        "/away" => Reply::redirect("http://elsewhere.example/landing"),
        "/missing" => Reply::status(404, "Not Found"),
        "/busy" => Reply::status(429, "Too Many Requests").with_header("Retry-After", "30"),
        "/image" => Reply::ok("image/png", b"\x89PNG\r\n".to_vec()),
        "/long" => Reply::ok("text/plain", "x".repeat(100_010)),
        _ => Reply::status(500, "Internal Server Error"),
    }
}

#[tokio::test]
async fn a_page_comes_back_as_fenced_markdown_and_is_cached() {
    let server = serve(site()).await;
    let dir = tempfile::tempdir().unwrap();
    let tracked = Tracked::default();
    let handlers = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let url = format!("{}/page", server.base);
    let first = fetch(&handlers, json!({ "url": url })).await.unwrap();
    assert_eq!(first["kind"], json!("html"));
    assert_eq!(first["code"], json!(200));
    assert_eq!(
        first["content"],
        json!("# Guide\n\nUse [x](https://x.dev/).")
    );
    assert_eq!(first["cached"], json!(false));
    assert_eq!(first["next_offset"], Value::Null);
    let text = first["text"].as_str().unwrap();
    assert!(text.starts_with(&format!(
        "Fetched {url} (HTTP 200 OK, text/html, 33 characters)."
    )));
    assert!(text.contains("UNTRUSTED web content"));
    assert!(!text.contains("steal"));
    let request = &server.requests()[0];
    assert_eq!(
        request.header("accept"),
        Some("text/markdown, text/html, */*")
    );
    assert!(
        request
            .header("user-agent")
            .unwrap()
            .starts_with("prime-agent/")
    );

    let second = fetch(&handlers, json!({ "url": url })).await.unwrap();
    assert_eq!(second["cached"], json!(true));
    assert_eq!(
        server.requests().len(),
        1,
        "the second fetch is served from the cache"
    );

    let events = tracked.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, FETCH_EVENT);
    let first_event = &events[0].1;
    assert_eq!(first_event.get("outcome"), Some(&json!("page")));
    assert_eq!(first_event.get("kind"), Some(&json!("html")));
    assert_eq!(first_event.get("prompt"), Some(&json!("none")));
    assert_eq!(first_event.get("cached"), Some(&json!(false)));
    assert_eq!(events[1].1.get("cached"), Some(&json!(true)));
}

#[tokio::test]
async fn same_host_redirects_are_followed_and_others_reported() {
    let server = serve(site()).await;
    let dir = tempfile::tempdir().unwrap();
    let tracked = Tracked::default();
    let handlers = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let moved = fetch(
        &handlers,
        json!({ "url": format!("{}/moved", server.base) }),
    )
    .await
    .unwrap();
    assert_eq!(moved["kind"], json!("html"));
    assert_eq!(moved["final_url"], json!(format!("{}/page", server.base)));

    let away = fetch(
        &handlers,
        json!({ "url": format!("{}/away", server.base), "prompt": "summarize" }),
    )
    .await
    .unwrap();
    assert_eq!(away["kind"], json!("redirect"));
    assert_eq!(away["code"], json!(302));
    assert_eq!(
        away["redirect_url"],
        json!("http://elsewhere.example/landing")
    );
    assert!(
        away["text"]
            .as_str()
            .unwrap()
            .contains("rlm.web_fetch(\"http://elsewhere.example/landing\", prompt=\"summarize\")")
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.target != "/landing"),
        "the cross-host redirect is not followed"
    );
}

#[tokio::test]
async fn http_errors_binaries_and_refusals() {
    let server = serve(site()).await;
    let dir = tempfile::tempdir().unwrap();
    let tracked = Tracked::default();
    let handlers = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let missing = fetch(
        &handlers,
        json!({ "url": format!("{}/missing", server.base) }),
    )
    .await
    .unwrap();
    assert_eq!(missing["kind"], json!("http_error"));
    assert!(missing["text"].as_str().unwrap().starts_with(
        "The server returned HTTP 404 Not Found.\n\nThe response body was not retrieved."
    ));
    let busy = fetch(&handlers, json!({ "url": format!("{}/busy", server.base) }))
        .await
        .unwrap();
    assert!(busy["text"].as_str().unwrap().contains("Retry-After: 30"));

    let image = fetch(
        &handlers,
        json!({ "url": format!("{}/image", server.base), "prompt": "what is it" }),
    )
    .await
    .unwrap();
    assert_eq!(image["kind"], json!("binary"));
    assert_eq!(image["content"], json!(""));
    assert!(
        image["text"]
            .as_str()
            .unwrap()
            .contains("Binary content (image/png, 6 bytes) is not shown")
    );

    // The product's options refuse the loopback server outright.
    let product = handlers_with_defaults(dir.path(), &tracked);
    let refused = fetch(&product, json!({ "url": format!("{}/page", server.base) }))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("use bash() with curl"),
        "{refused}"
    );
    let invalid = fetch(&product, json!({ "url": "ftp://example.com/" }))
        .await
        .unwrap_err();
    assert!(invalid.to_string().starts_with("Invalid URL"), "{invalid}");
    assert!(fetch(&product, json!({})).await.is_err());
}

fn handlers_with_defaults(dir: &std::path::Path, tracked: &Tracked) -> HostRequestHandlers {
    handlers(WebOverrides::default(), dir, tracked)
}

#[tokio::test]
async fn long_pages_are_windowed_and_read_on_with_an_offset() {
    let server = serve(site()).await;
    let dir = tempfile::tempdir().unwrap();
    let tracked = Tracked::default();
    let handlers = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let url = format!("{}/long", server.base);
    let first = fetch(&handlers, json!({ "url": url })).await.unwrap();
    assert_eq!(first["kind"], json!("text"));
    assert_eq!(first["total_chars"], json!(100_010));
    assert_eq!(first["next_offset"], json!(100_000));
    assert_eq!(first["content"].as_str().unwrap().len(), 100_000);
    let rest = fetch(&handlers, json!({ "url": url, "offset": 100_000 }))
        .await
        .unwrap();
    assert_eq!(rest["content"], json!("x".repeat(10)));
    assert_eq!(rest["next_offset"], Value::Null);
    assert_eq!(rest["cached"], json!(true));
}

#[tokio::test]
async fn a_prompt_is_answered_by_the_answering_model_or_the_page_comes_back() {
    let server = serve(site()).await;
    let model_server =
        serve(|_request| Reply::ok("text/event-stream", text_answer("The page is a guide."))).await;
    let dir = tempfile::tempdir().unwrap();
    let tracked = Tracked::default();
    let answering = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            answer_model: Some(ResolvedModel {
                model: anthropic_model(&model_server.base),
                api_key: Some("sk-ant-api-test".into()),
                headers: None,
            }),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let url = format!("{}/page", server.base);
    let answered = fetch(
        &answering,
        json!({ "url": url, "prompt": "What is this page?" }),
    )
    .await
    .unwrap();
    assert_eq!(answered["answer"], json!("The page is a guide."));
    assert_eq!(answered["answer_model"], json!("mock-anthropic/mock-haiku"));
    assert_eq!(answered["text"], json!("The page is a guide."));
    let request = &model_server.requests()[0];
    assert_eq!(request.target, "/v1/messages");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let prompt = body["messages"][0]["content"].to_string();
    assert!(prompt.contains("Web page content:"), "{prompt}");
    assert!(prompt.contains("# Guide"), "{prompt}");
    assert!(prompt.contains("What is this page?"), "{prompt}");
    assert_eq!(body["max_tokens"], json!(4096));

    // No answering model configured (an empty agent dir): the page text.
    let plain = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let unanswered = fetch(
        &plain,
        json!({ "url": url, "prompt": "What is this page?" }),
    )
    .await
    .unwrap();
    assert_eq!(unanswered["answer"], Value::Null);
    assert!(
        unanswered["text"]
            .as_str()
            .unwrap()
            .contains("No answering model is configured")
    );
    let prompts: Vec<Value> = tracked
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, properties)| properties.get("prompt").cloned())
        .collect();
    assert_eq!(prompts, vec![json!("answered"), json!("no_model")]);
}

#[tokio::test]
async fn the_answering_model_resolves_from_the_auxiliary_model_setting() {
    let server = serve(site()).await;
    let model_server =
        serve(|_request| Reply::ok("text/event-stream", text_answer("From settings."))).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        json!({ "providers": { "mock-anthropic": {
            "baseUrl": model_server.base, "apiKey": "sk-ant-api-test", "api": "anthropic-messages",
            "models": [{ "id": "mock-haiku", "name": "Mock Haiku", "contextWindow": 200_000, "maxTokens": 8192 }]
        }}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        json!({ "auxiliaryModel": "mock-anthropic/mock-haiku" }).to_string(),
    )
    .unwrap();
    let tracked = Tracked::default();
    let handlers = handlers(
        WebOverrides {
            fetch: Some(local_fetch()),
            ..WebOverrides::default()
        },
        dir.path(),
        &tracked,
    );
    let answered = fetch(
        &handlers,
        json!({ "url": format!("{}/page", server.base), "prompt": "?" }),
    )
    .await
    .unwrap();
    assert_eq!(answered["answer"], json!("From settings."));
    assert_eq!(answered["answer_model"], json!("mock-anthropic/mock-haiku"));
    assert_eq!(
        model_server.requests()[0].header("x-api-key"),
        Some("sk-ant-api-test")
    );
}
