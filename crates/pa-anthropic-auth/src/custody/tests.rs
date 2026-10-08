//! Custody of `auth.json`'s login across processes: a temporary agent
//! directory, a temporary store and a loopback token endpoint answering by
//! refresh token.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anthropic::AccountStore;
use chrono::{Duration, Utc};
use pa_core::auth::{
    install_credential_source, AuthStorage, AuthStorageBackend, FileAuthStorageBackend, NoOAuth,
};
use pa_types::sync::MutexExt;

use crate::test_support::*;

const LOGIN_ACCESS: &str = "sk-ant-oat01-auth-json-moved-access-000";
const LOGIN_REFRESH: &str = "sk-ant-ort01-auth-json-moved-refresh-00";

/// Write `auth.json` in `agent_dir` holding `provider`'s expired login.
fn write_auth_json(agent_dir: &Path, provider: &str) {
    let data = serde_json::json!({
        provider: {
            "type": "oauth", "access": LOGIN_ACCESS, "refresh": LOGIN_REFRESH,
            "expires": (Utc::now() - Duration::hours(1)).timestamp_millis()
        }
    });
    std::fs::write(
        agent_dir.join("auth.json"),
        serde_json::to_string_pretty(&data).expect("auth.json"),
    )
    .expect("write auth.json");
}

/// The refresh token of every row the store holds.
fn stored_refresh_tokens(store_path: &Path) -> Vec<String> {
    AccountStore::load(store_path)
        .expect("the store")
        .accounts
        .iter()
        .filter_map(|account| {
            account
                .oauth()
                .map(|tokens| tokens.refresh.expose().to_string())
        })
        .collect()
}

/// `auth.json` for a process that read it earlier: its first read returns
/// that copy; later reads and its locked writes see the file as it is now.
struct ReadEarlier {
    file: FileAuthStorageBackend,
    /// The earlier copy, until the first read takes it.
    earlier: Mutex<Option<String>>,
}

impl AuthStorageBackend for ReadEarlier {
    fn read(&self) -> anyhow::Result<Option<String>> {
        match self.earlier.lock_or_recover().take() {
            Some(earlier) => Ok(Some(earlier)),
            None => self.file.read(),
        }
    }

    fn with_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> anyhow::Result<((), Option<String>)>,
    ) -> anyhow::Result<()> {
        self.file.with_lock(update)
    }
}

#[test]
fn a_login_another_process_moved_and_spent_is_not_imported_again() {
    let provider = "anthropic-custody-moved";
    let (url, presented) =
        token_endpoint_by_refresh(vec![(LOGIN_REFRESH.to_string(), 200, ROTATED)]);
    let (home, source) = source_over(Vec::new(), &url);
    install_credential_source(provider, source.clone());
    let agent_dir = home.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("the agent dir");
    write_auth_json(&agent_dir, provider);
    let earlier = std::fs::read_to_string(agent_dir.join("auth.json")).expect("auth.json");

    // One process moves the login into the store, which spends its refresh
    // token (single use) on the first request.
    let mut mover = AuthStorage::create_with_oauth(&agent_dir, Arc::new(NoOAuth));
    assert_eq!(
        mover.get_api_key(provider),
        Some(ROTATED_ACCESS.to_string())
    );
    // Another process read auth.json before the move.
    let mut late = AuthStorage::from_storage(
        Arc::new(ReadEarlier {
            file: FileAuthStorageBackend::new(agent_dir.join("auth.json")),
            earlier: Mutex::new(Some(earlier)),
        }),
        Arc::new(NoOAuth),
    );
    assert_eq!(late.get_api_key(provider), Some(ROTATED_ACCESS.to_string()));

    // The spent token was presented once and has no custodian left: the
    // store holds only its rotation, and auth.json nothing.
    assert_eq!(
        *presented.lock_or_recover(),
        vec![LOGIN_REFRESH.to_string()]
    );
    assert_eq!(
        stored_refresh_tokens(source.store_path()),
        vec!["sk-ant-ort01-rotated-rotated-rotated-00".to_string()]
    );
    assert_eq!(late.get_all().get(provider), None);
}

/// Set in the processes [`custody_child`] runs in: their shared setup, as
/// JSON (`agent_dir`, `store`, `token_url`, `provider`).
const CHILD_SETUP_ENV: &str = "PA_ANTHROPIC_AUTH_CUSTODY_CHILD";

/// The built-in refresh of `auth.json`'s login (pa-core's step after the
/// source), presenting the refresh token to the test's token endpoint, so
/// a spend from `auth.json` is counted with the store's.
struct PresentingOAuth {
    token_url: String,
}

impl pa_core::auth::OAuthIntegration for PresentingOAuth {
    fn api_key_for(
        &self,
        _provider: &str,
        credential: &pa_core::auth::AuthCredential,
    ) -> Option<String> {
        match credential {
            pa_core::auth::AuthCredential::Oauth { access, .. } => Some(access.clone()),
            _ => None,
        }
    }

    fn refresh(
        &self,
        provider_id: &str,
        credentials: &pa_core::auth::AuthStorageData,
    ) -> Result<pa_core::auth::AuthCredential, pa_core::auth::OAuthRefreshError> {
        use std::io::Write as _;
        let Some(pa_core::auth::AuthCredential::Oauth {
            refresh: Some(refresh),
            ..
        }) = credentials.credential(provider_id)
        else {
            return Err(pa_core::auth::OAuthRefreshError::Failed);
        };
        let address = self
            .token_url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .expect("the endpoint's address");
        let body = serde_json::json!({ "grant_type": "refresh_token", "refresh_token": refresh })
            .to_string();
        let mut stream = std::net::TcpStream::connect(address).expect("the token endpoint");
        write!(
            stream,
            "POST /v1/oauth/token HTTP/1.1\r\nhost: {address}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("present the refresh token");
        let mut answer = String::new();
        let _ = std::io::Read::read_to_string(&mut stream, &mut answer);
        // The test reads what was presented; the answer is not used.
        Err(pa_core::auth::OAuthRefreshError::Failed)
    }
}

/// One process of
/// [`auth_json_and_the_store_never_both_refresh_one_token_across_processes`]:
/// a lookup over the shared `auth.json` and store. Without its setup (a
/// plain `--ignored` run) it does nothing.
#[test]
#[ignore = "a child process of auth_json_and_the_store_never_both_refresh_one_token_across_processes"]
fn custody_child() {
    let Ok(setup) = std::env::var(CHILD_SETUP_ENV) else {
        return;
    };
    let setup: serde_json::Value = serde_json::from_str(&setup).expect("the setup");
    let field = |name: &str| setup[name].as_str().expect("a setup field").to_string();
    let provider: &'static str = field("provider").leak();
    let token_url = field("token_url");
    let source = Arc::new(crate::SharedStoreSource::new(
        crate::SharedStoreConfig::isolated(
            field("store").into(),
            &token_url,
            "http://127.0.0.1:9/api/oauth/profile",
        ),
    ));
    install_credential_source(provider, source);
    let mut auth =
        AuthStorage::create_with_oauth(field("agent_dir"), Arc::new(PresentingOAuth { token_url }));

    // The rotation, or (a peer's refresh outlasting this process's wait
    // for its claim, on a loaded machine) no key this time; never
    // auth.json's spent login.
    let key = auth.get_api_key(provider);
    assert!(
        key.is_none() || key.as_deref() == Some(ROTATED_ACCESS),
        "served {key:?}"
    );
}

#[test]
fn auth_json_and_the_store_never_both_refresh_one_token_across_processes() {
    let provider = "anthropic-custody-processes";
    let (url, presented) =
        token_endpoint_by_refresh(vec![(LOGIN_REFRESH.to_string(), 200, ROTATED)]);
    let (home, source) = source_over(Vec::new(), &url);
    let agent_dir = home.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("the agent dir");
    write_auth_json(&agent_dir, provider);
    let setup = serde_json::json!({
        "agent_dir": agent_dir,
        "store": source.store_path(),
        "token_url": url,
        "provider": provider,
    })
    .to_string();
    let test_binary = std::env::current_exe().expect("the test binary");

    // Every process starts at once with the expired login in auth.json, as
    // a daemon's workers do after a restart.
    let children: Vec<std::process::Child> = (0..4)
        .map(|_| {
            std::process::Command::new(&test_binary)
                .args([
                    "custody::tests::custody_child",
                    "--exact",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env(CHILD_SETUP_ENV, &setup)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("start a process")
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().expect("the process ends");
        assert!(
            output.status.success(),
            "a process failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // One custodian spent the token, once; nothing holds it any more.
    assert_eq!(
        *presented.lock_or_recover(),
        vec![LOGIN_REFRESH.to_string()]
    );
    assert_eq!(
        stored_refresh_tokens(source.store_path()),
        vec!["sk-ant-ort01-rotated-rotated-rotated-00".to_string()]
    );
    let on_disk: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(agent_dir.join("auth.json")).expect("auth.json"),
    )
    .expect("auth.json parses");
    assert_eq!(on_disk.get(provider), None);
}

const CLAUDE_CODE_ACCESS: &str = "sk-ant-oat01-claude-code-own-access-000";
const CLAUDE_CODE_REFRESH: &str = "sk-ant-ort01-claude-code-own-refresh-00";

/// Write Claude Code's login into `claude_dir` (`.credentials.json`, owner
/// only) and who it is logged in as (`.claude.json`), as Claude Code keeps
/// them.
fn write_claude_code_login(claude_dir: &Path, access: &str, refresh: &str, expires_in: Duration) {
    let credentials = claude_dir.join(".credentials.json");
    std::fs::write(
        &credentials,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": (Utc::now() + expires_in).timestamp_millis(),
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max"
            }
        })
        .to_string(),
    )
    .expect("write Claude Code's credentials");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600))
            .expect("owner-only credentials");
    }
    std::fs::write(
        claude_dir.join(".claude.json"),
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": "acct-claude-code",
                "organizationUuid": "org-claude-code",
                "emailAddress": "person@example.com"
            }
        })
        .to_string(),
    )
    .expect("write Claude Code's config");
}

#[test]
fn claude_code_s_login_left_in_auth_json_follows_claude_code_s_rotation() {
    let provider = "anthropic-custody-claude-code";
    let (url, presented) = token_endpoint_by_refresh(vec![
        // Claude Code spends its own refresh token when it next starts.
        (CLAUDE_CODE_REFRESH.to_string(), 400, INVALID_GRANT),
    ]);
    let home = tempfile::tempdir().expect("a temporary home");
    let claude_dir = home.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("Claude Code's dir");
    // Claude Code's login, expired; another host copied it into auth.json.
    write_claude_code_login(
        &claude_dir,
        CLAUDE_CODE_ACCESS,
        CLAUDE_CODE_REFRESH,
        Duration::hours(-1),
    );
    let (_store_home, source) =
        source_configured(vec![row("cc-pool", Duration::hours(2))], |config| {
            config.endpoints.token_url = url.clone();
            config.native_publish =
                anthropic::credentials::NativePublish::At(claude_dir.join(".credentials.json"));
        });
    install_credential_source(provider, source.clone());
    let data = serde_json::json!({
        provider: {
            "type": "oauth", "access": CLAUDE_CODE_ACCESS, "refresh": CLAUDE_CODE_REFRESH,
            "expires": (Utc::now() - Duration::hours(1)).timestamp_millis()
        }
    });
    let mut auth = AuthStorage::in_memory_without_env(
        &pa_core::auth::AuthStorageData(data.as_object().cloned().unwrap_or_default()),
        Arc::new(NoOAuth),
    );

    // The store takes the login over; the pool's live token serves.
    assert_eq!(auth.get_api_key(provider), Some(access_of("cc-pool")));
    // Claude Code starts, refreshes its own login, and goes on with it.
    write_claude_code_login(
        &claude_dir,
        "sk-ant-oat01-claude-code-next-access-00",
        "sk-ant-ort01-claude-code-next-refresh-0",
        Duration::hours(8),
    );
    AccountStore::mutate(source.store_path(), |store| {
        store.get_mut("cc-pool")?.enabled = false;
        Ok(())
    })
    .expect("take the pool out");

    // The imported row is Claude Code's login: it follows Claude Code's
    // rotation instead of presenting the token Claude Code spent.
    assert_eq!(
        auth.get_api_key(provider),
        Some("sk-ant-oat01-claude-code-next-access-00".to_string())
    );
    assert_eq!(*presented.lock_or_recover(), Vec::<String>::new());
}
