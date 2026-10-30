//! The keep-alive, on the crate's own thread (started by the first served
//! credential, never at install, never on a paint or startup path):
//!
//! - the plugins' machine-wide pass (`keepAliveOnce`, the opencode
//!   plugin's ten-minute tick): idle logins whose refresh token nears its
//!   expiry are refreshed, one process per machine at a time behind the
//!   store's keep-alive lease;
//! - the live Claude Code version the requests claim (the npm registry's
//!   `latest`, read at the thread's start and hourly; never below the
//!   verified floor);
//! - the request path's store writes (quota readings, use) and usage polls
//!   (`quota.rs`), between passes, at most one poll a second (the plugins'
//!   quota API gate);
//! - ahead of expiry: a login this process served within the last hour
//!   whose access token expires before the next tick is refreshed now
//!   (claimed through the store), so a request never waits for it.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};

use anthropic::claude_version::{ClaudeCodeVersionTracker, fetch_latest_claude_code_version_from};
use anthropic::keepalive::KeepAliveSkip;
use anthropic::{AccountStore, KeepAliveOptions, OAuthClient, SharedRefreshOptions};
use chrono::{DateTime, Duration, Utc};
use pa_types::sync::MutexExt;

use crate::SharedStoreConfig;
use crate::quota::{PollRun, QuotaTracker, StoreWrite, poll_usage};
use crate::source::token_fingerprint;

/// The least time between two usage polls (the plugins' `API_CALL_GAP_MS`).
const POLL_GAP: StdDuration = StdDuration::from_secs(1);

/// Work the request path hands to the keep-alive thread.
pub(crate) enum Job {
    /// A store bookkeeping write.
    Write(StoreWrite),
    /// A usage poll of one row; `done` hears how it went.
    Poll {
        account_id: String,
        done: Option<Sender<PollRun>>,
    },
}

/// The pause before the first pass (the process is starting).
const FIRST_TICK: StdDuration = StdDuration::from_mins(1);
/// The pass cadence (the plugins' `refresh.intervalMinutes` default).
const TICK: StdDuration = StdDuration::from_mins(10);
/// The most a pass is delayed at random (the plugins' tick jitter).
const TICK_JITTER_MS: u64 = 60_000;
/// A login whose access token expires within this window is refreshed
/// ahead: longer than a tick and its jitter plus the request path's own
/// five-minute refresh leeway, so a request never meets one to refresh.
pub(crate) const AHEAD_WINDOW_SECS: i64 = 20 * 60;
/// A login served within this window counts as in use by this process.
const IN_USE: StdDuration = StdDuration::from_hours(1);

/// What one pass did, counts only (no account ids).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KeepAliveTick {
    /// Idle logins the machine-wide pass refreshed.
    pub(crate) idle_refreshed: usize,
    /// Logins in use here refreshed ahead of their expiry.
    pub(crate) ahead_refreshed: usize,
    /// Refreshes that failed.
    pub(crate) failed: usize,
    /// Another process holds the machine-wide pass's lease.
    pub(crate) lease_held: bool,
}

/// The keep-alive's state: what it reaches, and the logins in use here.
pub(crate) struct KeepAlive {
    config: SharedStoreConfig,
    in_use: Mutex<HashMap<String, Instant>>,
    /// The logins whose expired refresh token this process already named in the log.
    expired_noted: Mutex<HashSet<String>>,
    /// The live Claude Code version (floored), refreshed on this thread.
    version: Mutex<ClaudeCodeVersionTracker>,
    /// The quota readings the polls land in (shared with the source).
    quota: Arc<QuotaTracker>,
}

impl KeepAlive {
    pub(crate) fn new(config: SharedStoreConfig, quota: Arc<QuotaTracker>) -> Self {
        Self {
            config,
            in_use: Mutex::new(HashMap::new()),
            expired_noted: Mutex::new(HashSet::new()),
            version: Mutex::new(ClaudeCodeVersionTracker::new()),
            quota,
        }
    }

    /// The version requests claim now: the registry's latest once read,
    /// never below the verified floor.
    pub(crate) fn claude_code_version(&self) -> String {
        self.version.lock_or_recover().current().to_string()
    }

    /// Read the live Claude Code version when the cached one is older than
    /// an hour (the plugins' `getClaudeCodeVersion`). A failure keeps the
    /// current one.
    pub(crate) async fn refresh_version(&self, now: DateTime<Utc>) {
        let Some(url) = &self.config.version_url else {
            return;
        };
        if self.version.lock_or_recover().is_fresh(now) {
            return;
        }
        let latest = fetch_latest_claude_code_version_from(
            &anthropic::oauth::default_oauth_http_client(),
            url,
        )
        .await;
        if let Some(latest) = latest {
            self.version.lock_or_recover().adopt(&latest, now);
        }
    }

    /// A login was served to a request of this process.
    pub(crate) fn note_served(&self, account_id: &str) {
        self.in_use
            .lock_or_recover()
            .insert(account_id.to_string(), Instant::now());
    }

    /// One pass at `now`: the machine-wide pass, then the logins in use
    /// here that expire within [`AHEAD_WINDOW_SECS`].
    pub(crate) async fn tick(&self, client: &OAuthClient, now: DateTime<Utc>) -> KeepAliveTick {
        let path = self.config.store_path.as_path();
        let mut report = KeepAliveTick::default();
        let before = token_states(path);
        match client
            .keep_alive_once(path, now, &KeepAliveOptions::default())
            .await
        {
            Ok(pass) => {
                if !pass.refreshed.is_empty() || !pass.adopted.is_empty() {
                    let after = token_states(path);
                    for (account_id, reason) in &pass.refreshed {
                        log_rotation(&before, &after, account_id, &format!("idle {reason:?}"));
                    }
                    for account_id in &pass.adopted {
                        log_rotation(&before, &after, account_id, "adopted a peer's rotation");
                    }
                }
                for failure in &pass.failed {
                    tracing::warn!(
                        account_id = failure.account_id.as_str(),
                        failure = ?failure.failure,
                        message = failure.message.as_str(),
                        "the keep-alive could not refresh an idle login"
                    );
                }
                // A login whose refresh token is past its own expiry is skipped for good
                // ("only a re-login helps"), so without this it would look usable forever.
                let expired: Vec<&str> = pass
                    .skipped
                    .iter()
                    .filter(|(_, skip)| *skip == KeepAliveSkip::RefreshExpired)
                    .map(|(account_id, _)| account_id.as_str())
                    .collect();
                let mut noted = self.expired_noted.lock_or_recover();
                noted.retain(|account_id| expired.contains(&account_id.as_str()));
                for account_id in expired {
                    if noted.insert(account_id.to_string()) {
                        tracing::warn!(
                            account_id,
                            "this login's refresh token has expired and cannot be renewed; log in again (/login anthropic)"
                        );
                    }
                }
                drop(noted);
                report.idle_refreshed = pass.refreshed.len();
                report.failed += pass.failed.len();
                report.lease_held = matches!(
                    pass.lease,
                    anthropic::keepalive::KeepAliveLeaseOutcome::Held { .. }
                );
            }
            Err(error) => {
                tracing::warn!(%error, "the shared store's keep-alive pass failed");
            }
        }
        let in_use: Vec<String> = {
            let mut in_use = self.in_use.lock_or_recover();
            in_use.retain(|_, served| served.elapsed() < IN_USE);
            in_use.keys().cloned().collect()
        };
        for account_id in in_use {
            let due = AccountStore::load(path).ok().and_then(|store| {
                let account = store.get(&account_id)?;
                let tokens = account.oauth()?;
                (account.enabled
                    && !account.refresh_token_is_dead()
                    && tokens.expires_at <= now + Duration::seconds(AHEAD_WINDOW_SECS))
                .then(|| tokens.clone())
            });
            let Some(tokens) = due else {
                continue;
            };
            let before = token_states(path);
            match client
                .refresh_shared(path, &tokens, &SharedRefreshOptions::default())
                .await
            {
                Ok(_) => {
                    report.ahead_refreshed += 1;
                    log_rotation(&before, &token_states(path), &account_id, "ahead of expiry");
                }
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(
                        account_id = account_id.as_str(),
                        token_fp = token_fingerprint(tokens.access.expose()),
                        expires_at = %tokens.expires_at,
                        error = %anthropic::token::redact_secrets(&error.to_string()),
                        "refreshing a login ahead of its expiry failed"
                    );
                }
            }
        }
        report
    }

    /// The keep-alive loop, run on the crate's own thread for the life of
    /// the process.
    pub(crate) fn run(&self, client: &OAuthClient, jobs: &Receiver<Job>) {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::warn!("the shared store's keep-alive could not start a runtime");
            return;
        };
        runtime.block_on(self.refresh_version(Utc::now()));
        let mut next = Instant::now() + FIRST_TICK;
        let mut last_poll: Option<Instant> = None;
        loop {
            // The request path's writes and polls run here between passes.
            match jobs.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Job::Write(write)) => {
                    write.apply(&self.config.store_path);
                    continue;
                }
                Ok(Job::Poll { account_id, done }) => {
                    if let Some(wait) = last_poll
                        .map(|at| POLL_GAP.saturating_sub(at.elapsed()))
                        .filter(|wait| !wait.is_zero())
                    {
                        std::thread::sleep(wait);
                    }
                    let run = runtime.block_on(poll_usage(
                        &self.config.store_path,
                        client,
                        &self.quota,
                        &account_id,
                    ));
                    if run != PollRun::Skipped {
                        last_poll = Some(Instant::now());
                    }
                    if let Some(done) = done {
                        let _ = done.send(run);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                }
            }
            runtime.block_on(self.refresh_version(Utc::now()));
            let report = runtime.block_on(self.tick(client, Utc::now()));
            if report != KeepAliveTick::default() {
                tracing::info!(
                    idle_refreshed = report.idle_refreshed,
                    ahead_refreshed = report.ahead_refreshed,
                    failed = report.failed,
                    lease_held = report.lease_held,
                    "shared store keep-alive pass"
                );
            }
            next = Instant::now() + TICK + StdDuration::from_millis(jitter_ms());
        }
    }
}

/// A pseudo-random delay below [`TICK_JITTER_MS`] (spreads processes'
/// passes; not a secret).
fn jitter_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::from(elapsed.subsec_nanos()) % TICK_JITTER_MS
        })
}

/// Each login's access-token fingerprint and expiry in the store at `path`
/// (empty when it cannot be read).
fn token_states(path: &std::path::Path) -> HashMap<String, (String, DateTime<Utc>)> {
    AccountStore::load(path).map_or_default(|store| {
        store
            .accounts
            .iter()
            .filter_map(|account| {
                let tokens = account.oauth()?;
                Some((
                    account.id.clone(),
                    (token_fingerprint(tokens.access.expose()), tokens.expires_at),
                ))
            })
            .collect()
    })
}

/// Log a keep-alive refresh of `account_id`: its token fingerprint and
/// expiry before and after.
fn log_rotation(
    before: &HashMap<String, (String, DateTime<Utc>)>,
    after: &HashMap<String, (String, DateTime<Utc>)>,
    account_id: &str,
    why: &str,
) {
    let fp = |states: &HashMap<String, (String, DateTime<Utc>)>| {
        states
            .get(account_id)
            .map_or("none".to_string(), |(fp, _)| fp.clone())
    };
    let expires_at = after
        .get(account_id)
        .map_or("unknown".to_string(), |(_, at)| at.to_rfc3339());
    tracing::info!(
        account_id,
        previous_fp = fp(before),
        token_fp = fp(after),
        expires_at,
        why,
        "the keep-alive refreshed a shared store login"
    );
}

#[cfg(test)]
mod tests;
