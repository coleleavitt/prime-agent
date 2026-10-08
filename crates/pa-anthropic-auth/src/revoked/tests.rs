//! A revoked login against a temporary store, a loopback token endpoint
//! answering by refresh token, and a captured log.

use std::sync::{Arc, Mutex};

use anthropic::token::Credential;
use anthropic::{Account, AccountStore};
use chrono::{Duration, Utc};
use pa_core::auth::{
    install_credential_source, AuthStorage, AuthStorageData, NoOAuth, ProviderCredentialSource,
};
use pa_core::features::{FeatureStatus, SessionFeature};
use pa_core::models::{ModelRegistry, ResolvedRequestAuth};
use pa_types::sync::MutexExt;

use crate::test_support::*;
use crate::{AnthropicAuthFeature, NewLogin, SharedStoreConfig, SharedStoreSource};

/// The log line a revocation is reported with, for a login logged as
/// `login` while `serving` serves.
fn revoked_line(login: &str, serving: &str) -> String {
    format!(
        "an Anthropic login in the shared account store was revoked (its refresh token was refused with invalid_grant); requests skip it until it is logged in again login={login} serving={serving}"
    )
}

/// `auth.json` still holding the store row `id`'s login, expired (what an
/// older host left there).
fn auth_json_holding(provider: &str, id: &str) -> AuthStorage {
    let data = serde_json::json!({
        provider: {
            "type": "oauth", "access": access_of(id), "refresh": refresh_of(id),
            "expires": (Utc::now() - Duration::hours(1)).timestamp_millis()
        }
    });
    AuthStorage::in_memory_without_env(
        &AuthStorageData(data.as_object().cloned().unwrap_or_default()),
        Arc::new(NoOAuth),
    )
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

/// The statuses the feature publishes for `session`, and the sink that
/// keeps the route open.
fn status_sink(
    session: &str,
) -> (
    Arc<Mutex<Vec<FeatureStatus>>>,
    pa_core::features::FeatureStatusSink,
) {
    let statuses: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
    let sink_statuses = Arc::clone(&statuses);
    let sink: pa_core::features::FeatureStatusSink =
        Arc::new(move |status| sink_statuses.lock_or_recover().push(status));
    pa_core::features::register_feature_status_sink(session, &sink);
    (statuses, sink)
}

/// [`feature_context`] for `session`.
fn session_context(session: &str) -> Arc<pa_core::features::SessionFeatureContext> {
    let mut context = (*feature_context()).clone();
    context.session_id = session.to_string();
    Arc::new(context)
}

#[test]
fn a_revoked_login_is_refreshed_once_and_reported_once() {
    let provider = "anthropic-revoked-once";
    let (url, presented) = token_endpoint_by_refresh(vec![
        (refresh_of("once-main"), 400, INVALID_GRANT),
        (refresh_of("once-pool"), 200, ROTATED),
    ]);
    let (_home, source) = source_over(
        vec![
            row("once-main", Duration::hours(-1)),
            row("once-pool", Duration::hours(-1)),
        ],
        &url,
    );
    AccountStore::mutate(source.store_path(), |store| store.set_current("once-main"))
        .expect("pin the login");
    install_credential_source(provider, source.clone());
    let mut registry = ModelRegistry::in_memory(auth_json_holding(provider, "once-main"));
    let model = messages_model(provider, "http://127.0.0.1:9");
    let log = WarningLog::default();

    let answers: Vec<ResolvedRequestAuth> = log.capture(|| {
        (0..6)
            .map(|_| registry.get_api_key_and_headers(&model, None))
            .collect()
    });

    // Every request is served by the healthy login; the revoked one is
    // presented once, and reported once.
    assert_eq!(answers, vec![served(ROTATED_ACCESS); 6]);
    assert_eq!(
        *presented.lock_or_recover(),
        vec![refresh_of("once-main"), refresh_of("once-pool")]
    );
    assert_eq!(log.messages(), vec![revoked_line("once-main", "once-pool")]);
    // auth.json no longer offers it.
    assert_eq!(registry.auth.get_all().get(provider), None);

    // One notice for the session, however many runs end.
    let session = "revoked-once";
    let (statuses, sink) = status_sink(session);
    let feature = AnthropicAuthFeature::new(Arc::clone(&source));
    for _ in 0..3 {
        feature.on_agent_end(&session_context(session));
    }
    assert_eq!(
        statuses
            .lock_or_recover()
            .iter()
            .map(|status| status.line.clone())
            .collect::<Vec<_>>(),
        vec![Some(
            "Your Anthropic login once-main was revoked; using once-pool. Run /login anthropic to restore it."
                .to_string()
        )]
    );
    drop(sink);
}

const PROFILE: &str = r#"{"account":{"uuid":"acct-relogin","email":"person@example.com"},"organization":{"uuid":"org-relogin","name":"Org"}}"#;

/// [`row`] for `id`, identified as the [`PROFILE`] account.
fn identified_row(id: &str, access_in: Duration) -> Account {
    let mut account = row(id, access_in);
    account.email = Some("person@example.com".to_string());
    if let Credential::Oauth(tokens) = &mut account.credential {
        tokens.account = Some(anthropic::token::TokenAccount {
            uuid: "acct-relogin".to_string(),
            email_address: Some("person@example.com".to_string()),
        });
        tokens.organization = Some(anthropic::token::TokenOrganization {
            uuid: "org-relogin".to_string(),
        });
    }
    account
}

#[test]
fn a_re_login_clears_the_revoked_state() {
    let main = "person@example.com";
    let (url, presented) = token_endpoint_by_refresh(vec![
        (refresh_of(main), 400, INVALID_GRANT),
        (refresh_of("relogin-pool"), 200, ROTATED),
    ]);
    let (profile_url, _profile_hits) = token_endpoint(200, PROFILE);
    let (_home, seeded) = source_over(
        vec![
            identified_row(main, Duration::hours(-1)),
            row("relogin-pool", Duration::hours(-1)),
        ],
        &url,
    );
    AccountStore::mutate(seeded.store_path(), |store| store.set_current(main))
        .expect("pin the login");
    let source = Arc::new(SharedStoreSource::new(SharedStoreConfig::isolated(
        seeded.store_path().to_path_buf(),
        &url,
        &profile_url,
    )));
    let session = "revoked-relogin";
    let (statuses, sink) = status_sink(session);
    let feature = AnthropicAuthFeature::new(Arc::clone(&source));
    let log = WarningLog::default();

    let first = log.capture(|| source.credential().map(|credential| credential.api_key));
    feature.on_agent_end(&session_context(session));
    // `/login anthropic` of the same account: the store merges the new
    // login into the revoked row and pins it.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(source.store_login(NewLogin {
            access: "sk-ant-oat01-relogin-fresh-access-000000".to_string(),
            refresh: "sk-ant-ort01-relogin-fresh-refresh-00000".to_string(),
            expires_ms: (Utc::now() + Duration::hours(8)).timestamp_millis(),
        }))
        .expect("the login is stored");
    let after = log.capture(|| source.credential().map(|credential| credential.api_key));
    feature.on_agent_end(&session_context(session));

    assert_eq!(first, Ok(ROTATED_ACCESS.to_string()));
    // The re-logged-in row serves again, with nothing refreshed.
    assert_eq!(
        after,
        Ok("sk-ant-oat01-relogin-fresh-access-000000".to_string())
    );
    assert_eq!(
        *presented.lock_or_recover(),
        vec![refresh_of(main), refresh_of("relogin-pool")]
    );
    // The log never names the account by its email.
    let handle = &anthropic::token_fingerprint(main)[..8];
    assert_eq!(log.messages(), vec![revoked_line(handle, "relogin-pool")]);
    // The notice is withdrawn.
    assert_eq!(
        statuses
            .lock_or_recover()
            .iter()
            .map(|status| status.line.clone())
            .collect::<Vec<_>>(),
        vec![
            Some(format!(
                "Your Anthropic login {main} was revoked; using relogin-pool. Run /login anthropic to restore it."
            )),
            None
        ]
    );
    drop(sink);
}
