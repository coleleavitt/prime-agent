//! pi's request end to end through pa-ai: a store-served token's request
//! leaves byte for byte as the plugin sends the same conversation.

use std::collections::BTreeMap;

use pa_types::sync::MutexExt;

use crate::pi::convert::tests::{cases, Case};
use crate::test_support::*;

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
    let golden = crate::pi::convert::tests::golden();
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
