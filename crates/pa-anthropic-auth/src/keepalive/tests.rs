//! Keep-alive passes against a temporary store and a loopback token
//! endpoint (called directly: no thread, no clock waits).

use std::sync::atomic::Ordering;

use anthropic::token::Credential;
use anthropic::AccountStore;
use chrono::{Duration, Utc};

use super::KeepAliveTick;
use crate::test_support::*;
use crate::{SharedStoreConfig, SharedStoreSource};

fn refresh_token_of(source: &SharedStoreSource, id: &str) -> Option<String> {
    AccountStore::load(source.store_path())
        .expect("the store")
        .get(id)
        .and_then(anthropic::Account::oauth)
        .map(|tokens| tokens.refresh.expose().to_string())
}

fn pass(source: &SharedStoreSource) -> KeepAliveTick {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(source.keepalive().tick(source.client(), Utc::now()))
}

#[test]
fn a_login_in_use_is_refreshed_ahead_of_its_expiry() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("ahead", Duration::minutes(10))], &token_url);
    pa_core::auth::ProviderCredentialSource::credential(source.as_ref()).expect("served");

    assert_eq!(
        pass(&source),
        KeepAliveTick {
            ahead_refreshed: 1,
            ..KeepAliveTick::default()
        }
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        refresh_token_of(&source, "ahead"),
        Some("sk-ant-ort01-rotated-rotated-rotated-00".to_string())
    );
}

#[test]
fn a_login_far_from_expiry_or_not_in_use_is_left_alone() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(
        vec![
            row("far", Duration::hours(2)),
            row("unused", Duration::minutes(10)),
        ],
        &token_url,
    );
    pa_core::auth::ProviderCredentialSource::credential(source.as_ref()).expect("served");

    assert_eq!(pass(&source), KeepAliveTick::default());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn an_idle_login_whose_refresh_token_nears_expiry_is_kept_alive() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let mut idle = row("idle", Duration::hours(-1));
    if let Credential::Oauth(tokens) = &mut idle.credential {
        tokens.refresh_expires_at = Some(Utc::now() + Duration::days(3));
    }
    let (_home, source) = source_over(vec![idle], &token_url);

    assert_eq!(
        pass(&source),
        KeepAliveTick {
            idle_refreshed: 1,
            ..KeepAliveTick::default()
        }
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn the_keepalive_thread_starts_with_the_first_served_credential() {
    let (_home, seeded) = source_over(
        vec![row("thread", Duration::hours(2))],
        "http://127.0.0.1:9",
    );
    let mut config = SharedStoreConfig::isolated(
        seeded.store_path().to_path_buf(),
        "http://127.0.0.1:9/v1/oauth/token",
        "http://127.0.0.1:9/api/oauth/profile",
    );
    config.background = true;
    let source = SharedStoreSource::new(config);

    // Construction and status reads start nothing.
    assert!(pa_core::auth::ProviderCredentialSource::status(&source).is_some());
    assert!(!source.keepalive_started());

    pa_core::auth::ProviderCredentialSource::credential(&source).expect("served");
    assert!(source.keepalive_started());
}
