//! pi's request end to end through pa-ai: a store-served token's request
//! leaves byte for byte as the plugin sends the same conversation.

use std::collections::BTreeMap;

use pa_types::sync::MutexExt;
use serde_json::Value;

use super::*;
use crate::pi::convert::tests::{cases, golden, version, Case};
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
        // metadata.user_id (the one its session header carries).
        assert_eq!(
            request.body,
            case.body_text.replace(&case.identity.session_id, &session),
            "{}",
            case.name
        );
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
