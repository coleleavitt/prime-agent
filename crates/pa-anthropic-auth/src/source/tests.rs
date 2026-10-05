//! The store source against a temporary store and a loopback token
//! endpoint: never the user's store, Claude Code's files, or the network.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier, Mutex};

use anthropic::token::Credential;
use anthropic::Account;
use chrono::{Duration, Utc};
use pa_core::auth::{install_credential_source, AuthStorage, AuthStorageData, NoOAuth};
use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::models::{ModelRegistry, ResolvedRequestAuth};

use super::*;
use crate::test_support::*;

/// `auth.json` holding the provider's own (live) OAuth login.
fn auth_json_login(provider: &str) -> AuthStorage {
    let data = serde_json::json!({
        provider: {
            "type": "oauth", "access": "sk-ant-oat01-auth-json-access",
            "refresh": "auth-json-refresh", "expires": 4_102_444_800_000i64
        }
    });
    AuthStorage::in_memory_without_env(
        &AuthStorageData(data.as_object().cloned().unwrap_or_default()),
        Arc::new(NoOAuth),
    )
}

fn model(provider: &str) -> pa_types::ai::Model {
    serde_json::from_value(serde_json::json!({
        "id": "claude-test", "name": "claude-test", "api": "anthropic-messages",
        "provider": provider, "baseUrl": "http://127.0.0.1:9", "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .expect("a test model")
}

fn served(api_key: &str) -> ResolvedRequestAuth {
    ResolvedRequestAuth {
        ok: true,
        api_key: Some(api_key.to_string()),
        headers: None,
        error: None,
        oauth_refresh_failed: false,
    }
}

#[test]
fn the_provider_gets_the_store_token() {
    let provider = "anthropic-store-live";
    let (url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("a", Duration::hours(2))], &url);
    install_credential_source(provider, source.clone());
    let mut registry = ModelRegistry::in_memory(auth_json_login(provider));

    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        served("sk-ant-oat01-a-store-access-000")
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        source.usage(),
        SourceUsage {
            store: 1,
            first: Some("store"),
            ..SourceUsage::default()
        }
    );
}

#[test]
fn an_expired_token_refreshes_once_under_concurrency() {
    let provider = "anthropic-store-concurrent";
    let (url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("concurrent", Duration::hours(-1))], &url);
    install_credential_source(provider, source.clone());

    // Two sessions' requests, each with its own auth view, at once.
    let start = Arc::new(Barrier::new(2));
    let requests: Vec<_> = (0..2)
        .map(|_| {
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                let mut registry = ModelRegistry::in_memory(auth_json_login(provider));
                start.wait();
                registry.get_api_key_and_headers(&model(provider), None)
            })
        })
        .collect();
    let resolved: Vec<_> = requests
        .into_iter()
        .map(|request| request.join().expect("the request thread"))
        .collect();

    assert_eq!(
        resolved,
        vec![served(ROTATED_ACCESS), served(ROTATED_ACCESS)]
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // The second request read the first one's rotation from the store.
    assert_eq!(
        source.usage(),
        SourceUsage {
            store: 1,
            refreshed: 1,
            first: Some("refreshed"),
            ..SourceUsage::default()
        }
    );
    let stored = AccountStore::load(source.store_path()).expect("the store");
    assert_eq!(
        stored
            .get("concurrent")
            .and_then(Account::oauth)
            .map(|tokens| tokens.refresh.expose().to_string()),
        Some("sk-ant-ort01-rotated-rotated-rotated-00".to_string())
    );
}

#[test]
fn a_refresh_failure_is_the_oauth_authentication_error() {
    let provider = "anthropic-store-revoked";
    let (url, hits) = token_endpoint(400, INVALID_GRANT);
    let (_home, source) = source_over(vec![row("revoked", Duration::hours(-1))], &url);
    install_credential_source(provider, source.clone());
    // auth.json's own login is never served in the store's place.
    let mut registry = ModelRegistry::in_memory(auth_json_login(provider));

    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        ResolvedRequestAuth {
            ok: false,
            api_key: None,
            headers: None,
            error: Some(pa_core::auth::oauth_refresh_failed_message(provider)),
            oauth_refresh_failed: true,
        }
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        source.usage(),
        SourceUsage {
            failed: 1,
            ..SourceUsage::default()
        }
    );
}

#[test]
fn an_empty_store_leaves_auth_json_in_charge() {
    let provider = "anthropic-store-empty";
    let (url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(Vec::new(), &url);
    install_credential_source(provider, source.clone());
    let mut registry = ModelRegistry::in_memory(auth_json_login(provider));

    assert_eq!(source.status(), None);
    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        served("sk-ant-oat01-auth-json-access")
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(source.usage(), SourceUsage::default());
}

#[test]
fn the_status_names_the_store_and_follows_rotation() {
    let (url, _hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("rotating", Duration::hours(-1))], &url);
    let before = source.status().expect("a login");
    assert_eq!(before.label, STORE_LABEL);

    source.credential().expect("a refreshed credential");
    let after = source.status().expect("a login");

    assert_eq!(after.label, STORE_LABEL);
    assert_ne!(after.revision, before.revision);
}

#[test]
fn the_debug_form_of_a_credential_never_shows_the_token() {
    let (url, _hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("a", Duration::hours(2))], &url);
    let credential = source.credential().expect("a credential");

    assert_eq!(
        credential,
        SourcedCredential {
            api_key: "sk-ant-oat01-a-store-access-000".to_string(),
            headers: BTreeMap::new(),
        }
    );
    assert!(!format!("{credential:?}").contains("sk-ant"));
}

fn context(telemetry: pa_core::features::FeatureTelemetry) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: std::path::PathBuf::from("/nonexistent/agent"),
        cwd: std::path::PathBuf::from("/nonexistent/cwd"),
        session_id: "session".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .expect("stub model"),
        telemetry: Some(telemetry),
        rlm_depth: 0,
        session_artifact_dir: None,
    })
}

#[test]
fn the_adoption_event_is_reported_once_per_process() {
    let (url, _hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("adoption", Duration::hours(-1))], &url);
    let events: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let recorder = Arc::clone(&events);
    let context = context(pa_core::features::FeatureTelemetry::new(
        move |name, properties| {
            recorder.lock_or_recover().push((
                name.to_string(),
                serde_json::to_value(&properties).expect("properties serialize"),
            ));
        },
    ));
    let feature = crate::AnthropicAuthFeature::new(Arc::clone(&source));

    // Nothing to report until the store answers a request.
    feature.on_agent_end(&context);
    assert!(events.lock_or_recover().is_empty());

    source.credential().expect("a refreshed credential");
    source.credential().expect("the stored credential");
    feature.on_agent_end(&context);
    feature.on_agent_end(&context);

    assert_eq!(
        *events.lock_or_recover(),
        vec![(
            crate::TELEMETRY_EVENT.to_string(),
            serde_json::json!({
                "source": "refreshed", "refreshed": 1, "failed": 0,
                "migrated": 0, "recovered": 0, "rotated": 0,
                "polled": 0, "poll_failed": 0
            })
        )]
    );
}

#[test]
fn a_login_joins_the_store_as_its_current_account() {
    const PROFILE: &str = r#"{"account":{"uuid":"acct-0001","email":"person@example.com"},"organization":{"uuid":"org-0001","name":"Org"}}"#;
    let (profile_url, profile_hits) = token_endpoint(200, PROFILE);
    let (_home, seeded) = source_over(vec![row("other", Duration::hours(2))], "http://127.0.0.1:9");
    let source = SharedStoreSource::new(SharedStoreConfig::isolated(
        seeded.store_path().to_path_buf(),
        "http://127.0.0.1:9/v1/oauth/token",
        &profile_url,
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let stored = runtime
        .block_on(source.store_login(crate::NewLogin {
            access: "sk-ant-oat01-new-login-access-000".to_string(),
            refresh: "sk-ant-ort01-new-login-refresh-000".to_string(),
            expires_ms: (Utc::now() + Duration::hours(8)).timestamp_millis(),
        }))
        .expect("the login is stored");

    assert_eq!(
        stored,
        crate::StoredLogin {
            store_path: source.store_path().to_path_buf(),
            claude_code: None,
        }
    );
    assert_eq!(profile_hits.load(Ordering::SeqCst), 1);
    let store = AccountStore::load(source.store_path()).expect("the store");
    assert_eq!(
        (
            store.current.as_deref(),
            store
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            Some("person@example.com"),
            vec!["other", "person@example.com"]
        )
    );
    // The current account serves first.
    assert_eq!(
        source.credential().map(|credential| credential.api_key),
        Ok("sk-ant-oat01-new-login-access-000".to_string())
    );
}

const PROFILE: &str = r#"{"account":{"uuid":"acct-0001","email":"person@example.com"},"organization":{"uuid":"org-0001","name":"Org"}}"#;

/// `auth.json` holding a well-formed Anthropic login (the native
/// `/login anthropic` shape), its access token live for `access_in`.
fn auth_json_native_login(provider: &str, access_in: Duration) -> AuthStorage {
    let data = serde_json::json!({
        provider: {
            "type": "oauth", "access": "sk-ant-oat01-auth-json-native-access-000",
            "refresh": "sk-ant-ort01-auth-json-native-refresh-000",
            "expires": (Utc::now() + access_in).timestamp_millis()
        }
    });
    AuthStorage::in_memory_without_env(
        &AuthStorageData(data.as_object().cloned().unwrap_or_default()),
        Arc::new(NoOAuth),
    )
}

/// A source over a store in a fresh home (seeded with `accounts`), whose
/// profile endpoint is `profile_url`.
fn source_with_profile(
    accounts: Vec<Account>,
    token_url: &str,
    profile_url: &str,
) -> (tempfile::TempDir, Arc<SharedStoreSource>) {
    let (home, seeded) = source_over(accounts, token_url);
    let source = SharedStoreSource::new(SharedStoreConfig::isolated(
        seeded.store_path().to_path_buf(),
        token_url,
        profile_url,
    ));
    (home, Arc::new(source))
}

/// The store's rows as `(id, refresh token)`, and its `current`.
fn rows(source: &SharedStoreSource) -> (Vec<(String, String)>, Option<String>) {
    let store = AccountStore::load(source.store_path()).expect("the store");
    (
        store
            .accounts
            .iter()
            .map(|a| {
                (
                    a.id.clone(),
                    a.oauth()
                        .map(|t| t.refresh.expose().to_string())
                        .unwrap_or_default(),
                )
            })
            .collect(),
        store.current.clone(),
    )
}

#[test]
fn an_auth_json_login_moves_into_an_empty_store() {
    let provider = "anthropic-migrate-empty";
    let (profile_url, profile_hits) = token_endpoint(200, PROFILE);
    let (_home, source) = source_with_profile(Vec::new(), "http://127.0.0.1:9", &profile_url);
    install_credential_source(provider, source.clone());
    let mut registry =
        ModelRegistry::in_memory(auth_json_native_login(provider, Duration::hours(2)));

    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        served("sk-ant-oat01-auth-json-native-access-000")
    );
    assert_eq!(profile_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        rows(&source),
        (
            vec![(
                "person@example.com".to_string(),
                "sk-ant-ort01-auth-json-native-refresh-000".to_string()
            )],
            Some("person@example.com".to_string())
        )
    );
    // auth.json no longer holds it: the store is the only custodian.
    assert_eq!(registry.auth.get_all().get(provider), None);
    assert_eq!(source.usage().migrated, 1);
}

#[test]
fn the_store_s_own_login_of_the_same_account_wins_over_auth_json() {
    let provider = "anthropic-migrate-kept";
    let (profile_url, _profile_hits) = token_endpoint(200, PROFILE);
    let mut own = row("own", Duration::hours(2));
    if let Credential::Oauth(tokens) = &mut own.credential {
        tokens.account = Some(anthropic::token::TokenAccount {
            uuid: "acct-0001".to_string(),
            email_address: Some("person@example.com".to_string()),
        });
        tokens.organization = Some(anthropic::token::TokenOrganization {
            uuid: "org-0001".to_string(),
        });
    }
    let (_home, source) = source_with_profile(vec![own], "http://127.0.0.1:9", &profile_url);
    install_credential_source(provider, source.clone());
    let mut registry =
        ModelRegistry::in_memory(auth_json_native_login(provider, Duration::hours(2)));

    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        served("sk-ant-oat01-own-store-access-000")
    );
    assert_eq!(
        rows(&source),
        (
            vec![(
                "own".to_string(),
                "sk-ant-ort01-own-store-refresh-000".to_string()
            )],
            None
        )
    );
    assert_eq!(registry.auth.get_all().get(provider), None);
}

#[test]
fn an_expired_auth_json_login_moves_in_without_an_identity_lookup() {
    let provider = "anthropic-migrate-expired";
    let (profile_url, profile_hits) = token_endpoint(200, PROFILE);
    let (url, token_hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_with_profile(Vec::new(), &url, &profile_url);
    install_credential_source(provider, source.clone());
    let mut registry =
        ModelRegistry::in_memory(auth_json_native_login(provider, Duration::hours(-1)));

    // The store refreshes it (once, claimed) like any of its own logins.
    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        served(ROTATED_ACCESS)
    );
    assert_eq!(profile_hits.load(Ordering::SeqCst), 0);
    assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    let (stored, current) = rows(&source);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].0.starts_with("account-"), "{stored:?}");
    assert_eq!(current.as_deref(), Some(stored[0].0.as_str()));
    assert_eq!(registry.auth.get_all().get(provider), None);
}

#[test]
fn a_malformed_auth_json_login_stays_in_auth_json() {
    let provider = "anthropic-migrate-malformed";
    let (_home, source) = source_over(Vec::new(), "http://127.0.0.1:9");
    install_credential_source(provider, source.clone());
    let mut registry = ModelRegistry::in_memory(auth_json_login(provider));

    assert_eq!(
        registry.get_api_key_and_headers(&model(provider), None),
        served("sk-ant-oat01-auth-json-access")
    );
    assert!(!source.store_path().exists());
    assert!(registry.auth.get_all().get(provider).is_some());
}

#[test]
fn logout_removes_the_login_the_provider_is_served_from() {
    let (_home, source) = source_over(
        vec![
            row("first", Duration::hours(2)),
            row("pinned", Duration::hours(2)),
        ],
        "http://127.0.0.1:9",
    );
    AccountStore::mutate(source.store_path(), |store| store.set_current("pinned"))
        .expect("pin a login");
    let removed = |remaining: &str| {
        Ok(pa_core::auth::RemovedLogin {
            notice: Some(format!(
                "Removed the login from the shared account store ({}); other tools that share the store no longer see it.{remaining}",
                source.store_path().display()
            )),
        })
    };

    assert_eq!(
        source.remove_login(),
        removed(" 1 more login there still serves this provider.")
    );
    assert_eq!(
        rows(&source),
        (
            vec![(
                "first".to_string(),
                "sk-ant-ort01-first-store-refresh-000".to_string()
            )],
            None
        )
    );
    assert_eq!(
        source.credential().map(|credential| credential.api_key),
        Ok("sk-ant-oat01-first-store-access-000".to_string())
    );

    assert_eq!(source.remove_login(), removed(""));
    assert_eq!(source.status(), None);
    assert_eq!(
        source.remove_login(),
        Err(CredentialSourceError::NotConfigured)
    );
}

#[test]
fn logout_without_a_store_removes_nothing() {
    let (_home, source) = source_over(Vec::new(), "http://127.0.0.1:9");

    assert_eq!(
        source.remove_login(),
        Err(CredentialSourceError::NotConfigured)
    );
    assert!(!source.store_path().exists());
}
