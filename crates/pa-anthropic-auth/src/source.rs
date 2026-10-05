//! The shared account store as the `anthropic` provider's credential
//! source (`pa_core::auth::ProviderCredentialSource`).
//!
//! Every credential comes from `anthropic::access::get_access_token`, the
//! entry point the opencode and pi plugins reach through anthropic-napi:
//! account selection, the Claude Code login link, and a refresh claimed
//! through the store (file lock plus a lease every consumer honours, a
//! compare-and-swap that never overwrites a newer rotation). Requests in
//! this process also take one flight lock, so a second request waits for
//! the first one's refresh and reads its result instead of queueing on the
//! store's claim.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use anthropic::access::{
    access_candidates, get_access_token, AccessError, AccessErrorKind, AccessGrant, AccessRequest,
    AccessSource,
};
use anthropic::credentials::NativePublish;
use anthropic::{AccountStore, Endpoints, OAuthClient, SharedRefreshOptions};
use pa_core::auth::{
    CredentialSourceError, CredentialSourceStatus, ProviderCredentialSource, RemovedLogin,
    SourcedCredential, StoredLoginCustody, StoredOAuthLogin,
};
use pa_types::sync::MutexExt;
use sha2::{Digest, Sha256};

use crate::config::{config_path_from_lookup, ConfigFile, RoutingConfig};
use crate::keepalive::{Job, KeepAlive};
use crate::quota::{poll_usage, quota_line, PollRun, QuotaLine, QuotaTracker, StoreWrite};
use crate::routing::{Route, RouteRequest, RoutingCounts};

/// The status rows' label for a login the shared store holds.
pub const STORE_LABEL: &str = "shared account store";

/// Where the source reads and refreshes.
#[derive(Debug, Clone)]
pub struct SharedStoreConfig {
    /// The store file (`~/.anthropic-accounts/accounts.json` or its
    /// `ANTHROPIC_ACCOUNTS_FILE` / `ANTHROPIC_ACCOUNTS_DIR` override).
    pub store_path: PathBuf,
    /// The OAuth endpoints (production, or the `ANTHROPIC_OAUTH_*`
    /// overrides).
    pub endpoints: Endpoints,
    /// Whether a rotation is published to Claude Code's credentials (the
    /// link the plugins share; `ANTHROPIC_NATIVE_PUBLISH=0` turns it off).
    pub native_publish: NativePublish,
    /// The OAuth profile endpoint a new login is identified with
    /// (`ANTHROPIC_OAUTH_PROFILE_URL` overrides it).
    pub profile_url: String,
    /// Refuse every OAuth call to a non-loopback host (tests).
    pub require_loopback: bool,
    /// Run the keep-alive on the crate's own thread once a credential has
    /// been served (off in tests and sandboxes, which call a pass directly).
    pub background: bool,
    /// Where the live Claude Code version is read (the npm registry's
    /// `latest`, on the keep-alive thread, hourly); `None` keeps the
    /// verified floor (`OPENCODE_ANTHROPIC_AUTH_DISABLE_VERSION_CHECK=1`,
    /// the plugins' switch).
    pub version_url: Option<String>,
    /// Prefer logins whose recorded usage is below this percentage in both
    /// windows (`ANTHROPIC_QUOTA_RESERVE_PCT`; the napi `reservePct`).
    pub quota_reserve: Option<f64>,
    /// The plugins' sidecar configuration (`anthropic-auth.json`: routing
    /// mode, quota policy, killswitch), read only; `None` reads none and
    /// keeps the plugins' defaults.
    pub config_path: Option<PathBuf>,
}

impl SharedStoreConfig {
    /// The configuration the plugins resolve from the same environment.
    /// Reads no file.
    #[must_use]
    pub fn from_env() -> Self {
        let config_path = config_path_from_lookup(
            |key| std::env::var(key).ok(),
            &pa_types::platform::dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
        );
        Self {
            store_path: anthropic::default_store_path(),
            endpoints: Endpoints::from_env(),
            native_publish: NativePublish::from_env(),
            profile_url: anthropic::profile::profile_url_from_lookup(|key| std::env::var(key).ok()),
            require_loopback: false,
            background: true,
            version_url: (std::env::var(anthropic::claude_version::DISABLE_VERSION_CHECK_ENV)
                .as_deref()
                != Ok("1"))
            .then(|| anthropic::claude_version::LATEST_VERSION_URL.to_string()),
            quota_reserve: crate::quota::reserve_from_env(),
            config_path: Some(config_path),
        }
    }

    /// A configuration over `store_path` that reaches only the given
    /// loopback token and profile endpoints (every other OAuth host is
    /// refused) and never Claude Code's credentials: tests and sandboxes.
    #[must_use]
    pub fn isolated(store_path: PathBuf, token_url: &str, profile_url: &str) -> Self {
        let mut endpoints = Endpoints::prod();
        endpoints.token_url = token_url.to_string();
        Self {
            store_path,
            endpoints,
            native_publish: NativePublish::Off,
            profile_url: profile_url.to_string(),
            require_loopback: true,
            background: false,
            version_url: None,
            quota_reserve: None,
            config_path: None,
        }
    }

    /// The OAuth client over these endpoints and rules.
    pub(crate) fn client(&self) -> OAuthClient {
        OAuthClient::new(self.endpoints.clone())
            .native_publish(self.native_publish.clone())
            .require_loopback(self.require_loopback)
    }
}

/// How the served credentials were obtained in this process, without any
/// account identity (the adoption telemetry's facts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceUsage {
    /// Live tokens served as the store held them.
    pub store: u64,
    /// Tokens this process refreshed.
    pub refreshed: u64,
    /// Tokens another process had already rotated (adopted).
    pub adopted: u64,
    /// Claude Code's live token, borrowed for its linked account.
    pub claude_code: u64,
    /// Requests the store could not serve (refresh failed, revoked,
    /// unreadable, network).
    pub failed: u64,
    /// auth.json logins moved into the store.
    pub migrated: u64,
    /// Requests re-sent after a 401 with a recovered token.
    pub recovered: u64,
    /// Requests moved to another login after a 429.
    pub rotated: u64,
    /// The way the first served credential was obtained.
    pub first: Option<&'static str>,
}

impl SourceUsage {
    /// Whether the source answered any request.
    #[must_use]
    pub fn answered(&self) -> bool {
        self.first.is_some() || self.failed > 0
    }
}

/// A custody or recovery event the adoption report counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageEvent {
    /// An auth.json login moved into the store.
    Migrated,
    /// A 401 recovered with a new token.
    Recovered,
    /// A 429 moved to another login.
    Rotated,
}

/// The store file's identity for the status memo: `(len, mtime)`. The
/// store is replaced by rename on every write, so an unchanged stamp is an
/// unchanged document.
type FileStamp = (u64, SystemTime);

/// The shared account store serving one provider id.
pub struct SharedStoreSource {
    pub(crate) config: SharedStoreConfig,
    /// Built on the first credential request (no startup cost).
    client: OnceLock<OAuthClient>,
    /// One credential resolution (or custody change) at a time in this
    /// process.
    pub(crate) flight: Mutex<()>,
    status_memo: Mutex<Option<(FileStamp, Option<CredentialSourceStatus>)>>,
    usage: Mutex<SourceUsage>,
    /// The access tokens this source served (newest last, bounded) and the
    /// logins they belong to: only these take part in the request hooks.
    served: Mutex<std::collections::VecDeque<ServedToken>>,
    /// The installation's device id, read (or created) with the first
    /// served token.
    device_id: OnceLock<Option<String>>,
    /// This process's session id per store row (the plugin's per-account
    /// Claude Code identity).
    sessions: Mutex<std::collections::HashMap<String, String>>,
    /// The quota readings of this process's responses and usage polls.
    pub(crate) quota: Arc<QuotaTracker>,
    /// The sidecar's routing, quota and killswitch settings.
    settings: ConfigFile,
    /// The row this process served last.
    last_served: Mutex<Option<String>>,
    /// What the routing did (counts only).
    pub(crate) counts: RoutingCounts,
    /// The keep-alive thread's work queue, once it runs.
    jobs: OnceLock<std::sync::mpsc::Sender<Job>>,
    /// The keep-alive's state, shared with its thread.
    keepalive: Arc<KeepAlive>,
    /// The keep-alive thread starts once.
    keepalive_started: std::sync::Once,
}

/// How many served tokens the source remembers (the pi plugin's bound).
const SERVED_TOKENS_LIMIT: usize = 64;
/// The longest a request waits for a usage poll it needs the result of.
const POLL_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether a poll's caller waits for its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollWait {
    /// Queue it and go on.
    Background,
    /// Wait for it (bounded by [`POLL_WAIT`]).
    Result,
}

/// A token this source served and the login it belongs to.
#[derive(Clone)]
pub(crate) struct ServedToken {
    token: String,
    /// The store row.
    pub(crate) account_id: String,
    /// The account's uuid, when the store knows it.
    pub(crate) account_uuid: Option<String>,
    /// The quota the store recorded for the row when the token was served.
    pub(crate) quota: Option<anthropic::account::QuotaObservation>,
}

impl SharedStoreSource {
    /// A source over `config`. Does no I/O.
    #[must_use]
    pub fn new(config: SharedStoreConfig) -> Self {
        let quota = Arc::new(QuotaTracker::default());
        let keepalive = Arc::new(KeepAlive::new(config.clone(), Arc::clone(&quota)));
        Self {
            settings: ConfigFile::new(config.config_path.clone()),
            config,
            client: OnceLock::new(),
            flight: Mutex::new(()),
            status_memo: Mutex::new(None),
            usage: Mutex::new(SourceUsage::default()),
            served: Mutex::new(std::collections::VecDeque::new()),
            device_id: OnceLock::new(),
            sessions: Mutex::new(std::collections::HashMap::new()),
            quota,
            last_served: Mutex::new(None),
            counts: RoutingCounts::default(),
            jobs: OnceLock::new(),
            keepalive,
            keepalive_started: std::sync::Once::new(),
        }
    }

    /// The keep-alive's state (its passes run on the crate's thread).
    #[cfg(test)]
    pub(crate) fn keepalive(&self) -> &KeepAlive {
        &self.keepalive
    }

    /// Whether the keep-alive thread was started.
    #[cfg(test)]
    pub(crate) fn keepalive_started(&self) -> bool {
        self.keepalive_started.is_completed()
    }

    /// Start the keep-alive thread, once, when the configuration runs one.
    fn start_keepalive(&self) {
        if !self.config.background {
            return;
        }
        self.keepalive_started.call_once(|| {
            let keepalive = Arc::clone(&self.keepalive);
            let client = self.config.client();
            let (sender, jobs) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("anthropic-keepalive".to_string())
                .spawn(move || keepalive.run(&client, &jobs));
            match spawned {
                Ok(_) => {
                    let _ = self.jobs.set(sender);
                }
                Err(error) => {
                    tracing::warn!(%error, "the shared store's keep-alive thread did not start");
                }
            }
        });
    }

    /// Hand a store write to the keep-alive thread (applied here, blocking,
    /// when no thread runs: tests and sandboxes).
    pub(crate) fn queue_write(&self, write: StoreWrite) {
        match self.jobs.get() {
            Some(sender) => {
                if let Err(unsent) = sender.send(Job::Write(write)) {
                    if let Job::Write(write) = unsent.0 {
                        write.apply(&self.config.store_path);
                    }
                }
            }
            None => write.apply(&self.config.store_path),
        }
    }

    /// Poll `account_id`'s usage on the keep-alive thread (here, blocking,
    /// when no thread runs: tests and sandboxes). With [`PollWait::Result`]
    /// the caller waits for it (bounded) and hears how it went; a poll
    /// already queued for the row is not queued again.
    pub(crate) fn queue_poll(&self, account_id: &str, wait: PollWait) -> Option<PollRun> {
        if !self.quota.claim_poll(account_id) {
            return None;
        }
        self.run_poll(account_id, wait)
    }

    /// Run a poll [`QuotaTracker::claim_poll`] (or `claim_due_poll`)
    /// queued for `account_id`.
    pub(crate) fn run_poll(&self, account_id: &str, wait: PollWait) -> Option<PollRun> {
        let inline = || {
            block_on_own_runtime(poll_usage(
                &self.config.store_path,
                self.client(),
                &self.quota,
                account_id,
            ))
            .ok()
        };
        let Some(sender) = self.jobs.get() else {
            return inline();
        };
        let (done, outcome) = match wait {
            PollWait::Result => {
                let (done, outcome) = std::sync::mpsc::channel();
                (Some(done), Some(outcome))
            }
            PollWait::Background => (None, None),
        };
        let job = Job::Poll {
            account_id: account_id.to_string(),
            done,
        };
        if sender.send(job).is_err() {
            return inline();
        }
        outcome.and_then(|outcome| outcome.recv_timeout(POLL_WAIT).ok())
    }

    /// The sidecar's settings now (re-read when the file changed), with the
    /// quota policy handed to the quota readings.
    pub(crate) fn settings(&self) -> Arc<RoutingConfig> {
        let settings = self.settings.current();
        self.quota.set_policy(&settings.quota);
        settings
    }

    /// The quota line of the login this process served last: its latest
    /// response reading, else what the store recorded for it.
    pub(crate) fn quota_line(&self) -> Option<QuotaLine> {
        let account_id = self.last_served.lock_or_recover().clone()?;
        let recorded = self
            .served
            .lock_or_recover()
            .iter()
            .rev()
            .find(|known| known.account_id == account_id)
            .and_then(|known| known.quota.clone());
        quota_line(self.quota.snapshot(&account_id).as_ref(), recorded.as_ref())
    }

    /// The store's token for a request now: the routing order's pick
    /// (refreshed when expired), preferring logins under the quota reserve
    /// when one is set (every login at it: the plain pick). While every
    /// login is cooling down or spent, the pinned (or first) login's live
    /// token serves and the provider's answer decides.
    pub(crate) fn resolve(&self) -> Result<Result<AccessGrant, AccessError>, String> {
        let client = self.client();
        let path = self.config.store_path.as_path();
        let options = SharedRefreshOptions::default();
        let reserved = self.config.quota_reserve.map(|reserve| {
            let request = AccessRequest {
                reserve_percent: Some(reserve),
                ..AccessRequest::default()
            };
            block_on_own_runtime(get_access_token(client, path, &request, &options))
        });
        let resolved = match reserved {
            Some(Ok(Err(error))) if error.kind == AccessErrorKind::QuotaReserve => {
                tracing::info!("every login is at the quota reserve; the store's pick serves");
                None
            }
            other => other,
        }
        .unwrap_or_else(|| {
            block_on_own_runtime(get_access_token(
                client,
                path,
                &AccessRequest::default(),
                &options,
            ))
        });
        if let Ok(Err(error)) = &resolved {
            if matches!(
                error.kind,
                AccessErrorKind::Transient | AccessErrorKind::QuotaReserve
            ) {
                if let Some(grant) = self.cooling_down_token() {
                    return Ok(Ok(grant));
                }
            }
        }
        resolved
    }

    /// The served login's stored token while it is live (every login cooling
    /// down or spent).
    fn cooling_down_token(&self) -> Option<AccessGrant> {
        let store = AccountStore::load(&self.config.store_path).ok()?;
        let now = chrono::Utc::now();
        let account = served_login(&store, now)?;
        let tokens = account.oauth().filter(|tokens| !tokens.is_expired(now))?;
        Some(AccessGrant {
            access_token: tokens.access.expose().to_string(),
            account_id: account.id.clone(),
            email: account.email.clone(),
            expires_at: tokens.expires_at,
            source: AccessSource::Store,
        })
    }

    /// The token for `request` as the routing picks its login (refreshed
    /// when expired): `Ok(None)` when the store cannot produce one (the
    /// request goes out with what it has), `Err` when nothing may serve it
    /// (a local refusal).
    pub(crate) fn routed_token(
        &self,
        request: &RouteRequest<'_>,
    ) -> Result<Option<String>, pa_ai::request_hooks::LocalRefusal> {
        let route = self.route(request);
        let _flight = self.flight.lock_or_recover();
        let resolved = match route {
            Route::Refuse(refusal) => return Err(refusal),
            Route::Report => return Ok(None),
            Route::Store => self.resolve(),
            Route::Login(account_id) => {
                let login = block_on_own_runtime(get_access_token(
                    self.client(),
                    &self.config.store_path,
                    &AccessRequest {
                        account: Some(account_id.clone()),
                        ..AccessRequest::default()
                    },
                    &SharedRefreshOptions::default(),
                ));
                match login {
                    Ok(Ok(grant)) if grant.account_id == account_id => Ok(Ok(grant)),
                    // That login cannot produce a token now: the store's
                    // own pick (which rotates past a dead login).
                    _ => self.resolve(),
                }
            }
        };
        let Ok(Ok(grant)) = resolved else {
            return Ok(None);
        };
        self.record(Some(grant.source));
        self.remember(&grant.access_token, &grant.account_id);
        Ok(Some(grant.access_token))
    }

    /// Remember a token this source handed out, for the store row
    /// `account_id` (its account uuid read from the store). Blocking: reads
    /// the store, and the device id the first time.
    pub(crate) fn remember(&self, token: &str, account_id: &str) {
        let row = AccountStore::load(&self.config.store_path)
            .ok()
            .and_then(|store| store.get(account_id).cloned());
        let account_uuid = row
            .as_ref()
            .and_then(|row| {
                row.oauth()?
                    .account
                    .as_ref()
                    .map(|account| account.uuid.clone())
            })
            .filter(|uuid| !uuid.trim().is_empty());
        let quota = row.and_then(|row| row.quota);
        self.device_id
            .get_or_init(|| crate::device::load_or_create(&self.config.store_path));
        let mut served = self.served.lock_or_recover();
        served.retain(|known| known.token != token);
        served.push_back(ServedToken {
            token: token.to_string(),
            account_id: account_id.to_string(),
            account_uuid,
            quota,
        });
        while served.len() > SERVED_TOKENS_LIMIT {
            served.pop_front();
        }
        drop(served);
        *self.last_served.lock_or_recover() = Some(account_id.to_string());
        self.keepalive.note_served(account_id);
        self.start_keepalive();
    }

    /// Whether this source handed out `token`.
    pub(crate) fn served(&self, token: &str) -> bool {
        self.served_token(token).is_some()
    }

    /// The login a token this source handed out belongs to.
    pub(crate) fn served_token(&self, token: &str) -> Option<ServedToken> {
        self.served
            .lock_or_recover()
            .iter()
            .find(|known| known.token == token)
            .cloned()
    }

    /// The installation's device id, once a token was served (no I/O).
    pub(crate) fn device_id(&self) -> Option<String> {
        self.device_id.get().cloned().flatten()
    }

    /// This process's session id for the store row `account_id`.
    pub(crate) fn session_id(&self, account_id: &str) -> String {
        self.sessions
            .lock_or_recover()
            .entry(account_id.to_string())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    /// The Claude Code version the requests claim now.
    pub(crate) fn claude_code_version(&self) -> String {
        self.keepalive.claude_code_version()
    }

    /// Count a refresh this process made outside a credential lookup (the
    /// 401 recovery).
    pub(crate) fn count_refreshed(&self) {
        let mut usage = self.usage.lock_or_recover();
        usage.refreshed += 1;
        usage.first.get_or_insert(AccessSource::Refreshed.code());
    }

    /// Count one custody or recovery event.
    pub(crate) fn count(&self, event: UsageEvent) {
        let mut usage = self.usage.lock_or_recover();
        match event {
            UsageEvent::Migrated => usage.migrated += 1,
            UsageEvent::Recovered => usage.recovered += 1,
            UsageEvent::Rotated => usage.rotated += 1,
        }
    }

    /// The store file this source reads.
    #[must_use]
    pub fn store_path(&self) -> &Path {
        &self.config.store_path
    }

    /// How this process's requests were served so far.
    #[must_use]
    pub fn usage(&self) -> SourceUsage {
        *self.usage.lock_or_recover()
    }

    pub(crate) fn client(&self) -> &OAuthClient {
        self.client.get_or_init(|| self.config.client())
    }

    fn read_status(&self) -> Option<CredentialSourceStatus> {
        let store = match AccountStore::load(&self.config.store_path) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(%error, "the shared Anthropic account store is unreadable");
                return None;
            }
        };
        // A login cooling down after a 429 is still a login.
        let candidates: Vec<&anthropic::Account> = logins(&store).collect();
        if candidates.is_empty() {
            return None;
        }
        // The revision changes whenever a login's access token rotates.
        let mut hasher = Sha256::new();
        for account in &candidates {
            hasher.update(account.id.as_bytes());
            hasher.update([0]);
            if let Some(tokens) = account.oauth() {
                hasher.update(tokens.access.expose().as_bytes());
            }
            hasher.update([0]);
        }
        let digest = hasher.finalize();
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&digest[..8]);
        Some(CredentialSourceStatus {
            label: STORE_LABEL.to_string(),
            revision: format!("{:016x}", u64::from_be_bytes(prefix)),
        })
    }

    /// Count one answered request: how its credential was obtained, or
    /// `None` for a failure.
    pub(crate) fn record(&self, served: Option<AccessSource>) {
        let mut usage = self.usage.lock_or_recover();
        let Some(source) = served else {
            usage.failed += 1;
            return;
        };
        match source {
            AccessSource::Store => usage.store += 1,
            AccessSource::Refreshed => usage.refreshed += 1,
            AccessSource::Adopted => usage.adopted += 1,
            AccessSource::ClaudeCode => usage.claude_code += 1,
        }
        usage.first.get_or_insert(source.code());
    }
}

impl ProviderCredentialSource for SharedStoreSource {
    fn status(&self) -> Option<CredentialSourceStatus> {
        let metadata = std::fs::symlink_metadata(&self.config.store_path).ok()?;
        let stamp = (metadata.len(), metadata.modified().ok()?);
        let mut memo = self.status_memo.lock_or_recover();
        if let Some((memo_stamp, status)) = memo.as_ref() {
            if *memo_stamp == stamp {
                return status.clone();
            }
        }
        let status = self.read_status();
        *memo = Some((stamp, status.clone()));
        status
    }

    fn adopt_stored_login(&self, login: &StoredOAuthLogin) -> StoredLoginCustody {
        self.adopt(login)
    }

    fn remove_login(&self) -> Result<RemovedLogin, CredentialSourceError> {
        self.remove_served_login()
    }

    fn credential(&self) -> Result<SourcedCredential, CredentialSourceError> {
        let _flight = self.flight.lock_or_recover();
        match self.resolve() {
            Ok(Ok(grant)) => {
                self.record(Some(grant.source));
                self.remember(&grant.access_token, &grant.account_id);
                Ok(SourcedCredential {
                    api_key: grant.access_token,
                    headers: std::collections::BTreeMap::new(),
                })
            }
            Ok(Err(error)) if error.kind == AccessErrorKind::AuthRequired => {
                Err(CredentialSourceError::NotConfigured)
            }
            Ok(Err(error)) => {
                self.record(None);
                Err(CredentialSourceError::Unavailable(error.to_string()))
            }
            Err(message) => {
                self.record(None);
                Err(CredentialSourceError::Unavailable(message))
            }
        }
    }
}

/// The store's logins the provider can be served from: enabled OAuth rows
/// with an inference scope (a row without scopes predates them), in store
/// order.
pub(crate) fn logins(store: &AccountStore) -> impl Iterator<Item = &anthropic::Account> {
    store.accounts.iter().filter(|account| {
        account.enabled
            && account
                .oauth()
                .is_some_and(|tokens| tokens.scopes.is_empty() || tokens.grants_inference())
    })
}

/// The login the provider is served from now: the routing order's first
/// candidate (`current` first, then the first available), else, while
/// every login is cooling down or spent, the pinned one or the first.
pub(crate) fn served_login(
    store: &AccountStore,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<&anthropic::Account> {
    if let Ok(candidates) = access_candidates(store, &AccessRequest::default(), now) {
        return candidates.first().copied();
    }
    let mut logins = logins(store).peekable();
    let first = logins.peek().copied();
    logins
        .find(|account| store.current.as_deref() == Some(account.id.as_str()))
        .or(first)
}

/// Run `future` to completion on a runtime of its own, on a thread of its
/// own: the source's synchronous entry points may run on an async worker,
/// where blocking on the caller's runtime would deadlock it.
pub(crate) fn block_on_own_runtime<F>(future: F) -> Result<F::Output, String>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                Ok(runtime.block_on(future))
            })
            .join()
            .unwrap_or_else(|_| Err("the store call panicked".to_string()))
    })
}

#[cfg(test)]
mod tests;
