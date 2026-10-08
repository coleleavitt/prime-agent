//! A refresh whose new credential `auth.json` could not take (a full disk,
//! EACCES, a lock or rename failure). The provider has already rotated the
//! refresh token, so the stored one is dead: the new credential must serve
//! this request, every later lookup in this process, and other processes
//! sharing the store, until a retried write lands it in `auth.json`.

use super::*;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};

const STORED_ACCESS: &str = "sk-stored-access-0";
const STORED_REFRESH: &str = "sk-stored-refresh-0";
const CHILD_DIR_ENV: &str = "PA_AUTH_UNSAVED_CHILD_DIR";
const CHILD_PROVIDER_ENV: &str = "PA_AUTH_UNSAVED_CHILD_PROVIDER";
const MARK: &str = "PA_AUTH_UNSAVED_CHILD ";

/// A file store whose document writes fail while `failing` is set: the
/// lock is taken and the document read, then the write errors as a full
/// disk does. Reads, the refresh claim and everything else go to the real
/// file backend.
struct FailingWrites {
    inner: crate::auth::storage::FileAuthStorageBackend,
    failing: AtomicBool,
}

impl FailingWrites {
    fn new(dir: &std::path::Path, failing: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: crate::auth::storage::FileAuthStorageBackend::new(dir.join("auth.json")),
            failing: AtomicBool::new(failing),
        })
    }
}

impl AuthStorageBackend for FailingWrites {
    fn read(&self) -> anyhow::Result<Option<String>> {
        self.inner.read()
    }

    fn with_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> anyhow::Result<((), Option<String>)>,
    ) -> anyhow::Result<()> {
        if !self.failing.load(Ordering::SeqCst) {
            return self.inner.with_lock(update);
        }
        self.inner.with_lock(&mut |current| {
            let ((), next) = update(current)?;
            if next.is_some() {
                anyhow::bail!("No space left on device (os error 28)");
            }
            Ok(((), None))
        })
    }

    fn changed_externally(&self) -> bool {
        self.inner.changed_externally()
    }

    fn claim_refresh(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<crate::platform::HeartbeatLock>> {
        self.inner.claim_refresh(provider_id)
    }

    fn keep_unsaved_refresh(
        &self,
        provider_id: &str,
        content: String,
        claim: Option<crate::platform::HeartbeatLock>,
    ) -> crate::auth::storage::UnsavedRefreshKept {
        self.inner.keep_unsaved_refresh(provider_id, content, claim)
    }

    fn unsaved_refreshes(&self, provider_id: &str) -> Vec<String> {
        self.inner.unsaved_refreshes(provider_id)
    }

    fn unsaved_refreshes_due(&self) -> Vec<String> {
        self.inner.unsaved_refreshes_due()
    }

    fn forget_unsaved_refresh(&self, provider_id: &str, content: &str) {
        self.inner.forget_unsaved_refresh(provider_id, content);
    }
}

/// A token endpoint that rotates: every refresh spends the presented
/// refresh token and issues a new pair. Each spend is one line in
/// `{dir}/spent`, so peers in other processes count against the same file.
struct RotatingOAuth {
    dir: std::path::PathBuf,
    /// The issued credential's lifetime (negative: already expired).
    lifetime_ms: i64,
}

impl RotatingOAuth {
    fn new(dir: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.to_path_buf(),
            lifetime_ms: 3_600_000,
        })
    }

    fn spent(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("spent"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl OAuthIntegration for RotatingOAuth {
    fn api_key_for(&self, _provider: &str, credential: &AuthCredential) -> Option<String> {
        match credential {
            AuthCredential::Oauth { access, .. } => Some(access.clone()),
            _ => None,
        }
    }

    fn refresh(
        &self,
        provider: &str,
        data: &AuthStorageData,
    ) -> Result<AuthCredential, OAuthRefreshError> {
        let Some(AuthCredential::Oauth {
            refresh: Some(presented),
            ..
        }) = data.credential(provider)
        else {
            return Err(OAuthRefreshError::Failed);
        };
        let issued = Self::spent(&self.dir).len() + 1;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("spent"))
            .map_err(|_| OAuthRefreshError::Failed)?;
        file.write_all(format!("{presented}\n").as_bytes())
            .map_err(|_| OAuthRefreshError::Failed)?;
        Ok(AuthCredential::Oauth {
            access: format!("sk-new-access-{issued}"),
            refresh: Some(format!("sk-new-refresh-{issued}")),
            expires: now_epoch_ms() + self.lifetime_ms,
            account_id: None,
            enterprise_url: None,
            endpoint: None,
            token_endpoint: None,
            client_id: None,
            resource: None,
            issuer: None,
        })
    }
}

fn stored_login() -> AuthCredential {
    AuthCredential::Oauth {
        access: STORED_ACCESS.into(),
        refresh: Some(STORED_REFRESH.into()),
        expires: 1000,
        account_id: None,
        enterprise_url: None,
        endpoint: None,
        token_endpoint: None,
        client_id: None,
        resource: None,
        issuer: None,
    }
}

/// A store directory whose `auth.json` holds `provider`'s expired login.
fn seeded(provider: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut seed = AuthStorageData::default();
    seed.insert(provider, &stored_login());
    std::fs::write(
        dir.path().join("auth.json"),
        serde_json::to_string_pretty(&seed.0).unwrap(),
    )
    .unwrap();
    dir
}

fn store_over(backend: Arc<FailingWrites>, oauth: Arc<RotatingOAuth>) -> AuthStorage {
    let mut auth = AuthStorage::from_storage(backend, oauth);
    auth.env_credentials = Arc::new(ScriptedEnv(HashMap::new()));
    auth
}

/// `provider`'s credential as `auth.json` holds it on disk.
fn on_disk(dir: &std::path::Path, provider: &str) -> Option<AuthCredential> {
    let content = std::fs::read_to_string(dir.join("auth.json")).unwrap();
    parse_storage_data(Some(&content))
        .unwrap()
        .credential(provider)
}

/// `{auth}.{kind}-{hash}`: the per-provider sidecar names other processes
/// (and the next start) look for, pinned here as the on-disk contract.
fn sidecar(dir: &std::path::Path, kind: &str, provider: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(provider.as_bytes());
    dir.join(format!("auth.json.{kind}-{}", hex(&digest[..8])))
}

fn recovery_files(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("auth.json.unsaved-"))
        .collect()
}

fn issued(n: usize) -> String {
    format!("sk-new-access-{n}")
}

#[test]
fn a_refresh_whose_write_fails_serves_the_new_credential() {
    const PROVIDER: &str = "x-unsaved-serves";
    let dir = seeded(PROVIDER);
    let mut auth = store_over(
        FailingWrites::new(dir.path(), true),
        RotatingOAuth::new(dir.path()),
    );
    let result = auth.get_api_key_with_source_token(PROVIDER, true);
    assert_eq!(
        (result.api_key, result.oauth_refresh_failed),
        (Some(issued(1)), false),
        "the fetched credential serves, never the expired one it replaced"
    );
    assert_eq!(RotatingOAuth::spent(dir.path()), vec![STORED_REFRESH]);
    assert_eq!(
        on_disk(dir.path(), PROVIDER),
        Some(stored_login()),
        "the failed write left auth.json as it was"
    );
}

#[test]
fn later_lookups_in_this_process_use_the_unsaved_credential_without_a_fetch() {
    const PROVIDER: &str = "x-unsaved-in-process";
    let dir = seeded(PROVIDER);
    let oauth = RotatingOAuth::new(dir.path());
    let mut auth = store_over(FailingWrites::new(dir.path(), true), Arc::clone(&oauth));
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    // The same store again, while the disk is still full.
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    // Another store instance on the same file (another session, the MCP
    // manager's): it reads the expired login from disk.
    let mut other = store_over(FailingWrites::new(dir.path(), true), Arc::clone(&oauth));
    assert_eq!(other.get_api_key(PROVIDER), Some(issued(1)));
    // The MCP session path reads the raw entry after a reload.
    other.reload();
    let Some(AuthCredential::Oauth { access, .. }) = other.get_all().credential(PROVIDER) else {
        panic!("the entry stays an OAuth login");
    };
    assert_eq!(access, issued(1));
    assert_eq!(
        RotatingOAuth::spent(dir.path()),
        vec![STORED_REFRESH],
        "one fetch: no lookup spent the dead refresh token again"
    );
}

#[test]
fn once_writes_succeed_the_credential_reaches_auth_json_and_the_recovery_state_clears() {
    const PROVIDER: &str = "x-unsaved-retried";
    let dir = seeded(PROVIDER);
    let oauth = RotatingOAuth::new(dir.path());
    let backend = FailingWrites::new(dir.path(), true);
    let mut auth = store_over(Arc::clone(&backend), Arc::clone(&oauth));
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    let unsaved = auth.get_all().credential(PROVIDER);
    // Disk space is back: the next lookup saves the kept credential.
    backend.failing.store(false, Ordering::SeqCst);
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    assert_eq!(on_disk(dir.path(), PROVIDER), unsaved);
    assert_eq!(recovery_files(dir.path()), Vec::<String>::new());
    // A store with no memory of the failure (the next start) reads it from
    // auth.json alone.
    let mut next_start = AuthStorage::from_storage(
        Arc::new(crate::auth::storage::FileAuthStorageBackend::new(
            dir.path().join("auth.json"),
        )),
        oauth,
    );
    next_start.env_credentials = Arc::new(ScriptedEnv(HashMap::new()));
    assert_eq!(next_start.get_api_key(PROVIDER), Some(issued(1)));
    assert_eq!(RotatingOAuth::spent(dir.path()), vec![STORED_REFRESH]);
}

#[cfg(unix)]
#[test]
fn the_recovery_file_is_owner_only_and_holds_only_the_refreshed_login() {
    const PROVIDER: &str = "x-unsaved-mode";
    let dir = seeded(PROVIDER);
    let mut auth = store_over(
        FailingWrites::new(dir.path(), true),
        RotatingOAuth::new(dir.path()),
    );
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    let path = sidecar(dir.path(), "unsaved", PROVIDER);
    assert_eq!(crate::platform::perms::file_mode(&path), Some(0o600));
    let kept = parse_storage_data(Some(&std::fs::read_to_string(&path).unwrap())).unwrap();
    assert_eq!(kept.keys(), vec![PROVIDER.to_string()]);
    assert_eq!(
        kept.credential(PROVIDER),
        auth.get_all().credential(PROVIDER)
    );
}

#[test]
fn a_store_that_cannot_keep_a_recovery_file_holds_the_refresh_claim_until_the_save() {
    const PROVIDER: &str = "x-unsaved-claim-held";
    let dir = seeded(PROVIDER);
    // The recovery file cannot be written either (a non-empty directory sits
    // at its path, so the rename fails).
    let blocked = sidecar(dir.path(), "unsaved", PROVIDER);
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("occupied"), "").unwrap();
    let oauth = RotatingOAuth::new(dir.path());
    let backend = FailingWrites::new(dir.path(), true);
    let mut auth = store_over(Arc::clone(&backend), Arc::clone(&oauth));
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    // No other process may spend the dead refresh token: the claim stays
    // held, so their refresh waits and fails closed instead.
    let claim = sidecar(dir.path(), "refresh", PROVIDER);
    let peer =
        crate::platform::lock_dir::LockDir::acquire(&claim, std::time::Duration::from_secs(10));
    assert_eq!(
        peer.map(drop).map_err(|error| error.kind()),
        Err(std::io::ErrorKind::WouldBlock),
        "the refresh claim is still held for the unsaved credential"
    );
    backend.failing.store(false, Ordering::SeqCst);
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    assert!(matches!(
        on_disk(dir.path(), PROVIDER),
        Some(AuthCredential::Oauth { access, .. }) if access == issued(1)
    ));
    assert!(
        crate::platform::lock_dir::LockDir::acquire(&claim, std::time::Duration::from_secs(10))
            .is_ok(),
        "the claim is released once auth.json holds the credential"
    );
    assert_eq!(RotatingOAuth::spent(dir.path()), vec![STORED_REFRESH]);
}

#[test]
fn an_unsaved_credential_that_expires_refreshes_with_its_own_refresh_token() {
    const PROVIDER: &str = "x-unsaved-expires";
    let dir = seeded(PROVIDER);
    let blocked = sidecar(dir.path(), "unsaved", PROVIDER);
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("occupied"), "").unwrap();
    // Issued credentials are already expired: the next lookup refreshes the
    // kept credential, under the claim this process still holds for it.
    let oauth = Arc::new(RotatingOAuth {
        dir: dir.path().to_path_buf(),
        lifetime_ms: -1,
    });
    let backend = FailingWrites::new(dir.path(), true);
    let mut auth = store_over(Arc::clone(&backend), oauth);
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    let result = auth.get_api_key_with_source_token(PROVIDER, true);
    assert_eq!(
        (result.api_key, result.oauth_refresh_failed),
        (Some(issued(2)), false)
    );
    assert_eq!(
        RotatingOAuth::spent(dir.path()),
        vec![STORED_REFRESH.to_string(), "sk-new-refresh-1".to_string()],
        "the second refresh spent the kept refresh token, not the dead stored one"
    );
    // Writes recover: the save releases the claim this process held.
    backend.failing.store(false, Ordering::SeqCst);
    auth.get_api_key(PROVIDER);
    assert!(crate::platform::lock_dir::LockDir::acquire(
        &sidecar(dir.path(), "refresh", PROVIDER),
        std::time::Duration::from_secs(10)
    )
    .is_ok());
}

/// Collects every event and span field as text, with its level.
#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

struct FieldText<'a>(&'a mut String);

impl tracing::field::Visit for FieldText<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for CapturedLog {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut text = String::new();
        span.record(&mut FieldText(&mut text));
        self.0
            .lock()
            .unwrap()
            .push((*span.metadata().level(), text));
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut text = String::new();
        values.record(&mut FieldText(&mut text));
        self.0.lock().unwrap().push((tracing::Level::TRACE, text));
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut text = String::new();
        event.record(&mut FieldText(&mut text));
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), text));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Set in the process that runs the logging test alone (see below).
const LOG_CHILD_ENV: &str = "PA_AUTH_UNSAVED_LOG_CHILD";

/// Runs in a process of its own, as the only test there: tracing caches a
/// callsite's interest process-wide, so a parallel test reaching the same
/// `warn!` with no subscriber installed can make this thread's capture
/// miss it.
#[test]
fn the_unsaved_login_is_reported_once_and_names_no_token() {
    const PROVIDER: &str = "x-unsaved-reported";
    const NAME: &str =
        "auth::manager::tests::unsaved_refresh::the_unsaved_login_is_reported_once_and_names_no_token";
    if std::env::var_os(LOG_CHILD_ENV).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(LOG_CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = seeded(PROVIDER);
    let log = CapturedLog::default();
    let mut auth = store_over(
        FailingWrites::new(dir.path(), true),
        RotatingOAuth::new(dir.path()),
    );
    tracing::subscriber::with_default(log.clone(), || {
        for _ in 0..3 {
            assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
        }
    });
    let lines = log.0.lock().unwrap().clone();
    let warnings: Vec<_> = lines
        .iter()
        .filter(|(level, _)| *level == tracing::Level::WARN)
        .map(|(_, text)| text.clone())
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "one warning for one unsaved login: {lines:?}"
    );
    assert!(
        warnings[0].contains("could not be saved")
            && warnings[0].contains("No space left on device"),
        "the warning says the login was not saved, and why: {warnings:?}"
    );
    let errors = auth.drain_errors();
    for secret in [
        STORED_ACCESS,
        STORED_REFRESH,
        &issued(1),
        "sk-new-refresh-1",
    ] {
        for text in lines.iter().map(|(_, text)| text).chain(&errors) {
            assert!(
                !text.contains(secret),
                "a token value reached the log: {text}"
            );
        }
    }
}

/// A peer process: wait for the go line on stdin, resolve with writes still
/// failing, report the key.
#[test]
fn child() {
    let (Some(dir), Some(provider)) = (
        std::env::var_os(CHILD_DIR_ENV).map(std::path::PathBuf::from),
        std::env::var(CHILD_PROVIDER_ENV).ok(),
    ) else {
        return;
    };
    let mut auth = store_over(FailingWrites::new(&dir, true), RotatingOAuth::new(&dir));
    println!("{MARK}ready");
    std::io::stdout().flush().unwrap();
    let mut go = String::new();
    std::io::stdin().read_line(&mut go).unwrap();
    let key = auth.get_api_key(&provider).unwrap_or_default();
    println!("{MARK}key={key}");
    std::io::stdout().flush().unwrap();
}

#[test]
fn another_process_uses_the_recovered_credential_without_a_fetch() {
    const PROVIDER: &str = "x-unsaved-cross-process";
    let dir = seeded(PROVIDER);
    let mut auth = store_over(
        FailingWrites::new(dir.path(), true),
        RotatingOAuth::new(dir.path()),
    );
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "auth::manager::tests::unsaved_refresh::child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR_ENV, dir.path())
        .env(CHILD_PROVIDER_ENV, PROVIDER)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut next_mark = || {
        lines
            .by_ref()
            .map(Result::unwrap)
            .find_map(|line| line.split_once(MARK).map(|(_, mark)| mark.to_string()))
            .expect("the peer process reported")
    };
    assert_eq!(next_mark(), "ready");
    child.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
    let key = next_mark();
    assert!(child.wait().unwrap().success());
    assert_eq!(key, format!("key={}", issued(1)));
    assert_eq!(
        RotatingOAuth::spent(dir.path()),
        vec![STORED_REFRESH],
        "the peer spent no refresh token: it read the recovery file"
    );
}

#[test]
fn an_unsaved_refresh_is_one_auth_notice_until_it_is_saved() {
    const PROVIDER: &str = "x-unsaved-notice";
    let dir = seeded(PROVIDER);
    let backend = FailingWrites::new(dir.path(), true);
    let mut auth = store_over(Arc::clone(&backend), RotatingOAuth::new(dir.path()));
    let heard: Arc<std::sync::Mutex<Vec<crate::auth::AuthNotice>>> = Arc::default();
    let into = Arc::clone(&heard);
    // The registry is the process's: parallel tests raise their own.
    let sink: crate::auth::AuthNoticeSink = Arc::new(move |notice: &crate::auth::AuthNotice| {
        if notice.provider == PROVIDER {
            into.lock().unwrap().push(notice.clone());
        }
    });
    crate::auth::register_auth_notice_sink("x-unsaved-notice-session", &sink);

    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    // Saved at last: the condition ends, so a session starting now hears
    // nothing.
    backend.failing.store(false, Ordering::SeqCst);
    assert_eq!(auth.get_api_key(PROVIDER), Some(issued(1)));
    let late_heard: Arc<std::sync::Mutex<Vec<crate::auth::AuthNotice>>> = Arc::default();
    let late_into = Arc::clone(&late_heard);
    let late: crate::auth::AuthNoticeSink = Arc::new(move |notice: &crate::auth::AuthNotice| {
        if notice.provider == PROVIDER {
            late_into.lock().unwrap().push(notice.clone());
        }
    });
    crate::auth::register_auth_notice_sink("x-unsaved-notice-late", &late);

    assert_eq!(
        *heard.lock().unwrap(),
        vec![crate::auth::AuthNotice {
            provider: PROVIDER.to_string(),
            condition: "unsaved-refresh".to_string(),
            message: format!(
                "Your {PROVIDER} login was refreshed but could not be saved to auth.json; it is kept and saving is retried."
            ),
        }]
    );
    assert!(late_heard.lock().unwrap().is_empty());
}
