//! pi's request end to end through pa-ai: a store-served token's request
//! leaves byte for byte as the plugin sends the same conversation.

use std::collections::BTreeMap;

use pa_types::sync::MutexExt;
use serde_json::Value;

use super::*;
use crate::pi::convert::tests::{Case, cases, golden, version};
use crate::shape::claude_code_headers;
use crate::test_support::*;

fn outgoing(case: &Case, version: &str) -> Outgoing {
    build_outgoing(
        &case.model,
        &case.source(),
        &BuildInputs {
            settings: case.settings,
            identity: &case.identity,
            version,
            disable_adaptive_flag: None,
        },
    )
}

#[test]
fn every_conversation_leaves_byte_for_byte_as_pi_sends_it() {
    let version = version();
    for case in cases() {
        assert_eq!(
            outgoing(&case, &version).text,
            case.body_text,
            "{}",
            case.name
        );
    }
}

#[test]
fn every_request_carries_pi_s_headers() {
    let version = version();
    for case in cases() {
        let outgoing = outgoing(&case, &version);
        let headers: BTreeMap<String, String> = claude_code_headers(
            &case.token,
            &outgoing.payload,
            &case.identity,
            &version,
            &ShapeEnv::default(),
            "",
            "request-id",
        )
        .into_iter()
        .filter(|(name, _)| name != "x-client-request-id")
        .map(|(name, value)| {
            let value = if name == "anthropic-beta" {
                anthropic::claude_code::merge_anthropic_betas(&value, &outgoing.extra_betas)
            } else {
                value
            };
            (name, value)
        })
        .collect();
        assert_eq!(headers, case.headers, "{}", case.name);
    }
}

/// Send `case` through pa-ai with the store's token for it; the captured
/// request and the session id this process used.
fn send(case: &Case, index: usize, settings: &serde_json::Value) -> (CapturedRequest, String) {
    let provider = format!("anthropic-pi-{index}");
    let (_home, source) = source_over(
        vec![row_with_account(
            &format!("pi-case-{index}"),
            case.identity.account_uuid.as_deref(),
        )],
        "http://127.0.0.1:9",
    );
    write_device_id(
        &source,
        case.identity.device_id.as_deref().expect("a device id"),
    );
    write_pi_settings(&source, settings);
    pa_core::auth::install_credential_source(&provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(&provider, source.clone());
    let (base, requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM)]);
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;
    let options = pa_ai::types::SimpleStreamOptions {
        base: pa_ai::types::StreamOptions {
            api_key: Some(served),
            max_tokens: case.options.max_tokens,
            ..Default::default()
        },
        reasoning: case.options.reasoning,
        thinking_budgets: case.options.thinking_budgets.clone(),
        tool_choice: None,
    };
    let message = complete_with(
        &model_with_id(&provider, &base, &case.model),
        &case.context,
        options,
    );
    assert_eq!(text_of(&message), "hello", "{}", case.name);
    let request = requests.lock_or_recover()[0].clone();
    let session = request
        .header("x-claude-code-session-id")
        .expect("the session header")
        .to_string();
    (request, session)
}

#[test]
fn a_store_request_leaves_as_pi_sends_the_same_conversation() {
    let golden = golden();
    for (index, case) in cases().iter().enumerate() {
        let settings = golden["cases"][index]["settings"].clone();
        let (request, session) = send(case, index, &settings);
        // The plugin's body, with this process's session id in
        // metadata.user_id (the one its session header carries), and,
        // where the case sets no cap, the model's request budget (32000
        // for the test model's 128000) in place of pi's 16384.
        let mut expected_body = case.body_text.replace(&case.identity.session_id, &session);
        if case.options.max_tokens.is_none() {
            expected_body = expected_body.replace("\"max_tokens\":16384,", "\"max_tokens\":32000,");
        }
        assert_eq!(request.body, expected_body, "{}", case.name);
        let mut expected = case.headers.clone();
        expected.insert(
            "authorization".to_string(),
            format!("Bearer {}", request.bearer()),
        );
        expected.insert("x-claude-code-session-id".to_string(), session);
        let sent: BTreeMap<String, String> = request
            .headers
            .iter()
            .filter(|(name, _)| {
                ![
                    "host",
                    "content-length",
                    "accept-encoding",
                    "x-client-request-id",
                ]
                .contains(&name.as_str())
            })
            .cloned()
            .collect();
        assert_eq!(sent, expected, "{}", case.name);
    }
}

/// A store-served response, streamed through pa-ai: the content kept.
fn stream_response(
    index: usize,
    model: &str,
    context: &pa_ai::types::Context,
    sse: &'static str,
) -> Vec<Value> {
    let provider = format!("anthropic-pi-response-{index}");
    let (_home, source) = source_over(
        vec![row_with_account(&format!("pi-response-{index}"), None)],
        "http://127.0.0.1:9",
    );
    pa_ai::request_hooks::install_request_hooks(&provider, source.clone());
    let (base, _requests) = messages_endpoint(vec![(200, Vec::new(), sse)]);
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;
    let options = pa_ai::types::SimpleStreamOptions {
        base: pa_ai::types::StreamOptions {
            api_key: Some(served),
            ..Default::default()
        },
        ..Default::default()
    };
    let message = complete_with(&model_with_id(&provider, &base, model), context, options);
    assert_eq!(message.error_message, None);
    message
        .content
        .iter()
        .map(|block| serde_json::to_value(block).expect("a block"))
        .collect()
}

#[test]
fn a_store_response_keeps_what_pi_keeps() {
    let golden = golden();
    for (index, case) in golden["responses"]
        .as_array()
        .expect("responses")
        .iter()
        .enumerate()
    {
        let context: pa_ai::types::Context =
            serde_json::from_value(case["context"].clone()).expect("a context");
        let sse: &'static str = Box::leak(
            case["sse"]
                .as_str()
                .expect("the stream")
                .to_string()
                .into_boxed_str(),
        );
        let kept = stream_response(
            index,
            case["model"].as_str().expect("a model"),
            &context,
            sse,
        );
        assert_eq!(Value::Array(kept), case["content"], "{}", case["name"]);
    }
}

const CREDITS_429: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Extra usage is required for long context requests."}}"#;

fn beta_of(request: &CapturedRequest) -> Vec<String> {
    request
        .header("anthropic-beta")
        .unwrap_or_default()
        .split(',')
        .map(str::to_string)
        .collect()
}

#[test]
fn a_credits_429_moves_the_token_s_later_requests_to_the_standard_window() {
    let provider = "anthropic-pi-context1m";
    let (_home, source) = source_over(
        vec![row_with_account("pi-context1m", None)],
        "http://127.0.0.1:9",
    );
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    let (base, requests) = messages_endpoint(vec![
        (429, Vec::new(), CREDITS_429),
        (200, Vec::new(), OK_STREAM),
    ]);
    let model = model_with_id(provider, &base, "claude-opus-4-8");
    let served = || {
        pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
            .expect("the store's token")
            .api_key
    };

    let refused = complete(&model, &served());
    assert!(refused.error_message.is_some());
    let message = complete(&model, &served());
    assert_eq!(text_of(&message), "hello");

    let requests = requests.lock_or_recover().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].bearer(), requests[1].bearer());
    let context_1m = anthropic::claude_code::CONTEXT_1M_BETA.to_string();
    assert_eq!(
        (
            beta_of(&requests[0]).contains(&context_1m),
            beta_of(&requests[1]).contains(&context_1m)
        ),
        (true, false)
    );
    // The latch removes only the 1M beta.
    let mut without = beta_of(&requests[0]);
    without.retain(|beta| *beta != context_1m);
    assert_eq!(beta_of(&requests[1]), without);
}

#[test]
fn claude_fast_turns_fast_mode_on_and_off_for_the_store_s_requests() {
    use pa_core::features::SessionFeature;
    let provider = "anthropic-pi-fast";
    let (_home, source) = source_over(
        vec![row_with_account("pi-fast", None)],
        "http://127.0.0.1:9",
    );
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    let feature = crate::AnthropicAuthFeature::new(source.clone());
    let names: Vec<&str> = feature
        .slash_commands()
        .iter()
        .map(|command| command.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "claude-fast",
            "claude-cache",
            "claude-cachekeep",
            "claude-routing",
            "claude-killswitch",
            "claude-quota"
        ]
    );
    let (base, requests) = messages_endpoint(vec![
        (200, Vec::new(), OK_STREAM),
        (200, Vec::new(), OK_STREAM),
    ]);
    let model = model_with_id(provider, &base, "claude-opus-4-8");
    let served = || {
        pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
            .expect("the store's token")
            .api_key
    };

    assert!(run_command(&feature, "claude-fast", "on").starts_with("## Claude Fast Mode Enabled"));
    complete(&model, &served());
    assert!(
        run_command(&feature, "claude-fast", "off").starts_with("## Claude Fast Mode Disabled")
    );
    complete(&model, &served());

    let requests = requests.lock_or_recover().clone();
    let speed = |request: &CapturedRequest| {
        serde_json::from_str::<Value>(&request.body).expect("a body")["speed"].clone()
    };
    let fast_beta = anthropic::claude_code::FAST_MODE_BETA.to_string();
    assert_eq!(
        (
            speed(&requests[0]),
            beta_of(&requests[0]).contains(&fast_beta)
        ),
        (Value::String("fast".to_string()), true)
    );
    assert_eq!(
        (
            speed(&requests[1]),
            beta_of(&requests[1]).contains(&fast_beta)
        ),
        (Value::Null, false)
    );
}

/// With no cap from the caller (the session loop sets none), a store
/// request sends what the API-key route sends for the model: its request
/// budget, the catalog's max output capped at pa-ai's 32000 (claude-opus-5-5
/// declares 128000), or a `maxTokens` the user configured as configured.
/// pi's own fallback, 16384, cut Opus 5.5 off mid tool call: its thinking
/// counts toward the cap.
#[test]
fn a_store_request_without_a_caller_cap_sends_the_model_s_request_budget() {
    let provider = "anthropic-pi-max-tokens";
    let (_home, source) = source_over(
        vec![row_with_account("pi-max-tokens", None)],
        "http://127.0.0.1:9",
    );
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    let (base, requests) = messages_endpoint(vec![
        (200, Vec::new(), OK_STREAM),
        (200, Vec::new(), OK_STREAM),
    ]);
    let served = || {
        pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
            .expect("the store's token")
            .api_key
    };
    let catalog = model_with_id(provider, &base, "claude-opus-5-5");
    let configured = pa_types::ai::Model {
        max_tokens: 64_000,
        max_tokens_explicit: true,
        ..catalog.clone()
    };

    assert_eq!(text_of(&complete(&catalog, &served())), "hello");
    assert_eq!(text_of(&complete(&configured, &served())), "hello");

    let sent: Vec<Value> = requests
        .lock_or_recover()
        .iter()
        .map(|request| {
            serde_json::from_str::<Value>(&request.body).expect("a body")["max_tokens"].clone()
        })
        .collect();
    assert_eq!(sent, vec![Value::from(32_000_u64), Value::from(64_000_u64)]);
}
