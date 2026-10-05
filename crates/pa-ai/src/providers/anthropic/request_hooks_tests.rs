//! The provider request hooks around the Anthropic stream, against a local
//! mock Messages endpoint: stub hooks, never a real credential store.

use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;
use serde_json::json;

use crate::providers::anthropic::{stream_anthropic, AnthropicOptions};
use crate::request_hooks::{
    install_request_hooks, Admission, LocalRefusal, OutgoingRequest, PendingRequest,
    ProviderRequestHooks, RejectedRequest, Rejection,
};
use crate::types::{AssistantContent, Context, Model, StopReason, StreamOptions, TextContent};

const OK_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);
const UNAUTHORIZED: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid token"}}"#;
const RATE_LIMITED: &str =
    r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#;

/// One scripted response: status, body.
type Reply = (u16, &'static str);

/// A mock Messages endpoint answering requests with `replies` in order;
/// returns its base URL and the bearer tokens it saw, in order.
async fn messages_endpoint(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let bearers = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bearers);
    tokio::spawn(async move {
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length || read == 0 {
                        let bearer = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("authorization: Bearer ")
                                    .or_else(|| line.strip_prefix("Authorization: Bearer "))
                            })
                            .unwrap_or_default()
                            .to_string();
                        seen.lock_or_recover().push(bearer);
                        break;
                    }
                }
            }
            let content_type = if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (base, bearers)
}

fn model(provider: &str, base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": "claude-test", "name": "Claude Test", "api": "anthropic-messages",
        "provider": provider, "baseUrl": base_url, "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128_000, "maxTokens": 8192
    }))
    .unwrap()
}

/// One rejection a stub hook was asked about: the credential, why, the
/// status, the provider's error type.
type SeenRejection = (String, Rejection, u16, Option<String>);

/// Stub hooks issuing `issued` in turn on each rejection, recording what
/// they were asked.
#[derive(Default)]
struct StubHooks {
    issued: Mutex<Vec<String>>,
    rejections: Mutex<Vec<SeenRejection>>,
    prepared: Mutex<Vec<String>>,
    observed: Mutex<Vec<(String, u16)>>,
}

impl StubHooks {
    fn issuing(tokens: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            issued: Mutex::new(tokens.iter().rev().map(|t| (*t).to_string()).collect()),
            ..Self::default()
        })
    }
}

impl ProviderRequestHooks for StubHooks {
    fn prepare(&self, request: &mut OutgoingRequest<'_>) {
        self.prepared
            .lock_or_recover()
            .push(request.api_key.to_string());
        request
            .headers
            .push(("x-stub-hook".to_string(), "1".to_string()));
    }

    fn observe(&self, _model: &Model, api_key: &str, response: &crate::types::ProviderResponse) {
        self.observed
            .lock_or_recover()
            .push((api_key.to_string(), response.status));
    }

    fn rejected(&self, rejected: &RejectedRequest<'_>) -> Option<String> {
        self.rejections.lock_or_recover().push((
            rejected.api_key.to_string(),
            rejected.rejection,
            rejected.status,
            rejected.provider_error_type.map(str::to_string),
        ));
        self.issued.lock_or_recover().pop()
    }
}

async fn run(model: &Model, api_key: &str) -> crate::types::AssistantMessage {
    let options = AnthropicOptions::from_base(StreamOptions {
        api_key: Some(api_key.to_string()),
        ..Default::default()
    });
    let mut reader = stream_anthropic(
        model,
        &Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        },
        Some(&options),
    );
    while reader.next_event().await.is_some() {}
    reader.result().await
}

fn text(message: &crate::types::AssistantMessage) -> Vec<AssistantContent> {
    message.content.clone()
}

fn hello() -> Vec<AssistantContent> {
    vec![AssistantContent::Text(TextContent {
        text: "hello".to_string(),
        text_signature: None,
        rest: serde_json::Map::default(),
    })]
}

#[tokio::test]
async fn a_401_is_re_sent_once_with_the_hooks_new_credential() {
    let provider = "hooks-401-then-ok";
    let (base, bearers) = messages_endpoint(vec![(401, UNAUTHORIZED), (200, OK_STREAM)]).await;
    let hooks = StubHooks::issuing(&["sk-ant-oat01-refreshed"]);
    install_request_hooks(provider, hooks.clone());

    let message = run(&model(provider, &base), "sk-ant-oat01-rejected").await;

    assert_eq!(
        (message.stop_reason, text(&message)),
        (StopReason::Stop, hello())
    );
    assert_eq!(
        *bearers.lock_or_recover(),
        vec!["sk-ant-oat01-rejected", "sk-ant-oat01-refreshed"]
    );
    assert_eq!(
        *hooks.rejections.lock_or_recover(),
        vec![(
            "sk-ant-oat01-rejected".to_string(),
            Rejection::Unauthorized,
            401,
            Some("authentication_error".to_string())
        )]
    );
    assert_eq!(
        *hooks.prepared.lock_or_recover(),
        vec!["sk-ant-oat01-rejected", "sk-ant-oat01-refreshed"]
    );
    assert_eq!(
        *hooks.observed.lock_or_recover(),
        vec![
            ("sk-ant-oat01-rejected".to_string(), 401),
            ("sk-ant-oat01-refreshed".to_string(), 200)
        ]
    );
}

#[tokio::test]
async fn a_second_401_is_reported_not_re_sent() {
    let provider = "hooks-401-twice";
    let (base, bearers) = messages_endpoint(vec![(401, UNAUTHORIZED), (401, UNAUTHORIZED)]).await;
    let hooks = StubHooks::issuing(&["sk-ant-oat01-second", "sk-ant-oat01-third"]);
    install_request_hooks(provider, hooks.clone());

    let message = run(&model(provider, &base), "sk-ant-oat01-first").await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        *bearers.lock_or_recover(),
        vec!["sk-ant-oat01-first", "sk-ant-oat01-second"]
    );
    assert_eq!(hooks.rejections.lock_or_recover().len(), 1);
}

#[tokio::test]
async fn a_429_moves_to_each_new_credential_the_hooks_name() {
    let provider = "hooks-429-rotate";
    let (base, bearers) = messages_endpoint(vec![
        (429, RATE_LIMITED),
        (200, "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n"),
        (200, OK_STREAM),
    ])
    .await;
    let hooks = StubHooks::issuing(&["sk-ant-oat01-b", "sk-ant-oat01-c"]);
    install_request_hooks(provider, hooks.clone());

    let message = run(&model(provider, &base), "sk-ant-oat01-a").await;

    assert_eq!(
        (message.stop_reason, text(&message)),
        (StopReason::Stop, hello())
    );
    assert_eq!(
        *bearers.lock_or_recover(),
        vec!["sk-ant-oat01-a", "sk-ant-oat01-b", "sk-ant-oat01-c"]
    );
    assert_eq!(
        *hooks.rejections.lock_or_recover(),
        vec![
            (
                "sk-ant-oat01-a".to_string(),
                Rejection::RateLimited,
                429,
                Some("rate_limit_error".to_string())
            ),
            (
                "sk-ant-oat01-b".to_string(),
                Rejection::RateLimited,
                200,
                Some("overloaded_error".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn a_429_with_no_new_credential_is_reported() {
    let provider = "hooks-429-exhausted";
    let (base, bearers) = messages_endpoint(vec![(429, RATE_LIMITED)]).await;
    let hooks = StubHooks::issuing(&[]);
    install_request_hooks(provider, hooks.clone());

    let message = run(&model(provider, &base), "sk-ant-oat01-a").await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(*bearers.lock_or_recover(), vec!["sk-ant-oat01-a"]);
}

#[tokio::test]
async fn without_hooks_a_401_is_reported_as_before() {
    let (base, bearers) = messages_endpoint(vec![(401, UNAUTHORIZED)]).await;

    let message = run(&model("hooks-none", &base), "sk-ant-oat01-a").await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(*bearers.lock_or_recover(), vec!["sk-ant-oat01-a"]);
}

/// Stub hooks admitting every request one way, recording what they saw.
struct AdmittingHooks {
    admission: Admission,
    seen: Mutex<Vec<(String, u64)>>,
}

impl ProviderRequestHooks for AdmittingHooks {
    fn admit(&self, request: &PendingRequest<'_>) -> Admission {
        self.seen
            .lock_or_recover()
            .push((request.api_key.to_string(), request.context_bytes));
        self.admission.clone()
    }
}

/// Stub hooks that only know a fresher credential.
struct FresherHooks;

impl ProviderRequestHooks for FresherHooks {
    fn current_credential(&self, _model: &Model, api_key: &str) -> Option<String> {
        (api_key == "sk-ant-oat01-stale").then(|| "sk-ant-oat01-fresh".to_string())
    }
}

#[tokio::test]
async fn an_admission_can_name_another_credential_and_sees_the_context_size() {
    let provider = "hooks-admit-send-with";
    let (base, bearers) = messages_endpoint(vec![(200, OK_STREAM)]).await;
    let hooks = Arc::new(AdmittingHooks {
        admission: Admission::SendWith("sk-ant-oat01-chosen".to_string()),
        seen: Mutex::new(Vec::new()),
    });
    install_request_hooks(provider, hooks.clone());
    let empty = Context {
        system_prompt: None,
        messages: vec![],
        tools: None,
    };

    let message = run(&model(provider, &base), "sk-ant-oat01-resolved").await;

    assert_eq!(
        (message.stop_reason, text(&message)),
        (StopReason::Stop, hello())
    );
    assert_eq!(*bearers.lock_or_recover(), vec!["sk-ant-oat01-chosen"]);
    assert_eq!(
        *hooks.seen.lock_or_recover(),
        vec![(
            "sk-ant-oat01-resolved".to_string(),
            serde_json::to_vec(&empty).unwrap().len() as u64
        )]
    );
}

#[tokio::test]
async fn a_refused_request_is_never_sent_and_fails_with_the_refusal() {
    let provider = "hooks-admit-refuse";
    let (base, bearers) = messages_endpoint(vec![(200, OK_STREAM)]).await;
    let hooks = Arc::new(AdmittingHooks {
        admission: Admission::Refuse(LocalRefusal {
            status: 429,
            headers: [("retry-after".to_string(), "60".to_string())].into(),
            body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"blocked by policy"}}"#
                .to_string(),
        }),
        seen: Mutex::new(Vec::new()),
    });
    install_request_hooks(provider, hooks.clone());

    let message = run(&model(provider, &base), "sk-ant-oat01-a").await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message
        .error_message
        .as_deref()
        .is_some_and(|error| error.contains("blocked by policy")));
    assert_eq!(*bearers.lock_or_recover(), Vec::<String>::new());
}

#[tokio::test]
async fn the_default_admission_sends_the_hooks_fresher_credential() {
    let provider = "hooks-admit-default";
    let (base, bearers) = messages_endpoint(vec![(200, OK_STREAM), (200, OK_STREAM)]).await;
    install_request_hooks(provider, Arc::new(FresherHooks));

    run(&model(provider, &base), "sk-ant-oat01-stale").await;
    run(&model(provider, &base), "sk-ant-oat01-other").await;

    assert_eq!(
        *bearers.lock_or_recover(),
        vec!["sk-ant-oat01-fresh", "sk-ant-oat01-other"]
    );
}
