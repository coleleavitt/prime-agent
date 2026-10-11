//! #1232 / #1362: a provider stream that goes silent mid-response settles as a retryable stream
//! failure once the per-read stall budget passes, instead of hanging the session forever.

use std::time::Duration;

use serde_json::json;

use crate::test_mock_http::{MockResponse, serve};
use crate::types::{Context, Model, StopReason, StreamOptions};
use crate::utils_inner::http::stream_stall_failure;

/// The test's outer guard: a regression hangs (the production symptom) and fails here instead.
const HANG_GUARD: Duration = Duration::from_secs(20);

#[tokio::test]
async fn a_silent_stream_fails_as_a_retryable_stall() {
    use crate::providers::openai_completions::{
        OpenAICompletionsOptions,
        stream_openai_completions,
    };
    let content = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n";
    let server = serve(vec![MockResponse {
        status: 200,
        content_type: "text/event-stream",
        frames: vec![content.as_bytes().to_vec()],
        hold_open: true,
    }])
    .await;
    let model: Model = serde_json::from_value(json!({
        "id": "m-1", "name": "m-1", "api": "openai-completions", "provider": "deepseek",
        "baseUrl": server.base_url(), "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100,
    }))
    .unwrap();
    let options = OpenAICompletionsOptions::from_base(StreamOptions {
        api_key: Some("test".into()),
        stream_stall_timeout_ms: Some(150),
        ..Default::default()
    });
    let message = tokio::time::timeout(
        HANG_GUARD,
        stream_openai_completions(&model, &Context::default(), Some(&options)).result(),
    )
    .await
    .expect("the silent stream settles instead of hanging");
    let expected = match stream_stall_failure(Duration::from_millis(150)) {
        crate::ProviderError::StreamFailure(failure) => failure,
        other => panic!("a stall is a stream failure, got {other:?}"),
    };
    assert_eq!(
        (message.stop_reason, message.error_message),
        (StopReason::Error, Some(expected.message)),
    );
    assert_eq!(
        expected.info.kind,
        crate::StreamFailureKind::ServerError,
        "a stall retries like a transient server error"
    );
}
