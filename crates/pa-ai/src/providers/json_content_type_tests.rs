//! #3351: JSON request bodies carry `Content-Type: application/json`; strict OpenAI-compatible
//! servers (and gateways in front of Responses, Azure and Google) reject a body without it with
//! 415.

use serde_json::json;

use crate::event_stream::AssistantMessageEventStream;
use crate::test_mock_http::{serve, MockHttp, MockResponse};
use crate::types::{Context, Model, StreamOptions};

fn model(api: &str, provider: &str, base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": "m-1", "name": "m-1", "api": api, "provider": provider,
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100,
    }))
    .unwrap()
}

fn base_options() -> StreamOptions {
    StreamOptions {
        api_key: Some("test".into()),
        ..Default::default()
    }
}

/// A server that answers every request with a 400, so only the captured request matters.
async fn rejecting_server() -> MockHttp {
    serve(vec![MockResponse {
        status: 400,
        content_type: "application/json",
        frames: vec![br#"{"error":{"message":"rejected"}}"#.to_vec()],
        hold_open: false,
    }])
    .await
}

/// Drive `start` against a fresh server and return the request's content type.
async fn sent_content_type(
    start: impl FnOnce(String) -> AssistantMessageEventStream,
) -> Option<String> {
    let server = rejecting_server().await;
    let _settled = start(server.base_url()).result().await;
    server.requests()[0].header("content-type")
}

#[tokio::test]
async fn json_providers_send_an_application_json_content_type() {
    use crate::providers::azure_openai_responses::{
        stream_azure_openai_responses, AzureOpenAIResponsesOptions,
    };
    use crate::providers::google::{stream_google, GoogleOptions};
    use crate::providers::google_vertex::{stream_google_vertex, GoogleVertexOptions};
    use crate::providers::openai_completions::{
        stream_openai_completions, OpenAICompletionsOptions,
    };
    use crate::providers::openai_responses::{stream_openai_responses, OpenAIResponsesOptions};

    let context = Context::default();
    let mut sent = Vec::new();
    sent.push((
        "openai-completions",
        sent_content_type(|base_url| {
            let options = OpenAICompletionsOptions::from_base(base_options());
            stream_openai_completions(
                &model("openai-completions", "deepseek", &base_url),
                &context,
                Some(&options),
            )
        })
        .await,
    ));
    sent.push((
        "openai-responses",
        sent_content_type(|base_url| {
            let options = OpenAIResponsesOptions::from_base(base_options());
            stream_openai_responses(
                &model("openai-responses", "openai", &base_url),
                &context,
                Some(&options),
            )
        })
        .await,
    ));
    sent.push((
        "azure-openai-responses",
        sent_content_type(|base_url| {
            let mut options = AzureOpenAIResponsesOptions::from_base(base_options());
            options.azure_base_url = Some(format!("{base_url}/openai/v1"));
            stream_azure_openai_responses(
                &model("azure-openai-responses", "azure-openai-responses", ""),
                &context,
                Some(&options),
            )
        })
        .await,
    ));
    sent.push((
        "google-generative-ai",
        sent_content_type(|base_url| {
            let options = GoogleOptions {
                base: base_options(),
                ..Default::default()
            };
            stream_google(
                &model("google-generative-ai", "google", &base_url),
                &context,
                Some(&options),
            )
        })
        .await,
    ));
    sent.push((
        "google-vertex",
        sent_content_type(|base_url| {
            let options = GoogleVertexOptions {
                base: base_options(),
                ..Default::default()
            };
            stream_google_vertex(
                &model("google-vertex", "google-vertex", &format!("{base_url}/v1")),
                &context,
                Some(&options),
            )
        })
        .await,
    ));
    let json = Some("application/json".to_string());
    assert_eq!(
        sent,
        vec![
            ("openai-completions", json.clone()),
            ("openai-responses", json.clone()),
            ("azure-openai-responses", json.clone()),
            ("google-generative-ai", json.clone()),
            ("google-vertex", json),
        ]
    );
}

/// A caller-supplied content type (model or option headers) wins over the default; no duplicate
/// header is sent.
#[tokio::test]
async fn a_configured_content_type_is_kept_without_a_duplicate() {
    use crate::providers::openai_completions::{
        stream_openai_completions, OpenAICompletionsOptions,
    };
    let server = rejecting_server().await;
    let mut base = base_options();
    base.headers = Some(
        [(
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )]
        .into_iter()
        .collect(),
    );
    let options = OpenAICompletionsOptions::from_base(base);
    let _settled = stream_openai_completions(
        &model("openai-completions", "deepseek", &server.base_url()),
        &Context::default(),
        Some(&options),
    )
    .result()
    .await;
    let head = server.requests()[0].head.to_ascii_lowercase();
    assert_eq!(
        head.matches("\r\ncontent-type:").count(),
        1,
        "exactly one content-type header: {head}"
    );
    assert_eq!(
        server.requests()[0].header("content-type"),
        Some("application/json; charset=utf-8".to_string())
    );
}
