//! The store in the provider's requests: a temporary store, a loopback
//! token endpoint and a mock Messages endpoint.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anthropic::AccountStore;
use chrono::Duration;
use pa_types::sync::MutexExt;

use crate::test_support::*;
use crate::SharedStoreSource;

/// The store's source installed (credential source and request hooks) for
/// a provider id of the test's own.
fn install(provider: &str, source: &Arc<SharedStoreSource>) {
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
}

fn refresh_token_of(source: &SharedStoreSource, id: &str) -> Option<String> {
    AccountStore::load(source.store_path())
        .expect("the store")
        .get(id)
        .and_then(anthropic::Account::oauth)
        .map(|tokens| tokens.refresh.expose().to_string())
}

#[test]
fn a_401_is_retried_once_with_a_claimed_refresh() {
    let provider = "anthropic-hooks-401";
    let (token_url, token_hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("recover", Duration::hours(2))], &token_url);
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![
        (401, Vec::new(), UNAUTHORIZED),
        (200, Vec::new(), OK_STREAM),
    ]);
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;

    let message = complete(&messages_model(provider, &base), &served);

    assert_eq!(text_of(&message), "hello");
    assert_eq!(
        requests
            .lock_or_recover()
            .iter()
            .map(CapturedRequest::bearer)
            .collect::<Vec<_>>(),
        vec![served, ROTATED_ACCESS.to_string()]
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        refresh_token_of(&source, "recover"),
        Some("sk-ant-ort01-rotated-rotated-rotated-00".to_string())
    );
    assert_eq!((source.usage().refreshed, source.usage().recovered), (1, 1));
}

#[test]
fn a_401_the_refresh_cannot_recover_is_reported() {
    let provider = "anthropic-hooks-401-dead";
    let (token_url, token_hits) = token_endpoint(400, INVALID_GRANT);
    let (_home, source) = source_over(vec![row("dead401", Duration::hours(2))], &token_url);
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![(401, Vec::new(), UNAUTHORIZED)]);
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;

    let message = complete(&messages_model(provider, &base), &served);

    assert_eq!(message.stop_reason, pa_ai::types::StopReason::Error);
    assert_eq!(requests.lock_or_recover().len(), 1);
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn a_401_on_a_revoked_login_moves_to_a_healthy_login() {
    let provider = "anthropic-hooks-401-revoked";
    let (token_url, presented) =
        token_endpoint_by_refresh(vec![(refresh_of("revoked401"), 400, INVALID_GRANT)]);
    let (_home, source) = source_over(
        vec![
            row("revoked401", Duration::hours(2)),
            row("healthy401", Duration::hours(2)),
        ],
        &token_url,
    );
    AccountStore::mutate(source.store_path(), |store| store.set_current("revoked401"))
        .expect("pin the login");
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![
        (401, Vec::new(), UNAUTHORIZED),
        (200, Vec::new(), OK_STREAM),
    ]);
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;

    let message = complete(&messages_model(provider, &base), &served);

    // The revoked login's 401 is not the user's failure while another
    // login can serve: the request moves there, unrefreshed.
    assert_eq!(text_of(&message), "hello");
    assert_eq!(
        requests
            .lock_or_recover()
            .iter()
            .map(CapturedRequest::bearer)
            .collect::<Vec<_>>(),
        vec![access_of("revoked401"), access_of("healthy401")]
    );
    assert_eq!(*presented.lock_or_recover(), vec![refresh_of("revoked401")]);
    assert_eq!(source.usage().recovered, 1);
}

#[test]
fn a_token_another_process_rotated_is_replaced_before_the_send() {
    let provider = "anthropic-hooks-rotated";
    let (_home, source) = source_over(vec![row("peer", Duration::hours(2))], "http://127.0.0.1:9");
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM)]);
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;
    // Another process rotates the row.
    let mut rotated = row("peer", Duration::hours(8));
    if let anthropic::token::Credential::Oauth(tokens) = &mut rotated.credential {
        tokens.access = anthropic::token::AccessToken::new(ROTATED_ACCESS.to_string());
    }
    AccountStore::mutate(source.store_path(), |store| {
        store.upsert(rotated);
        Ok(())
    })
    .expect("rotate the row");

    let message = complete(&messages_model(provider, &base), &served);

    assert_eq!(text_of(&message), "hello");
    assert_eq!(
        requests
            .lock_or_recover()
            .iter()
            .map(CapturedRequest::bearer)
            .collect::<Vec<_>>(),
        vec![ROTATED_ACCESS.to_string()]
    );
}

#[test]
fn a_token_the_store_did_not_serve_is_left_alone() {
    let provider = "anthropic-hooks-foreign";
    let (token_url, token_hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("foreign", Duration::hours(2))], &token_url);
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![(401, Vec::new(), UNAUTHORIZED)]);

    let message = complete(
        &messages_model(provider, &base),
        "sk-ant-oat01-runtime-key-not-the-store-0",
    );

    assert_eq!(message.stop_reason, pa_ai::types::StopReason::Error);
    assert_eq!(
        requests
            .lock_or_recover()
            .iter()
            .map(CapturedRequest::bearer)
            .collect::<Vec<_>>(),
        vec!["sk-ant-oat01-runtime-key-not-the-store-0".to_string()]
    );
    assert_eq!(token_hits.load(Ordering::SeqCst), 0);
}
