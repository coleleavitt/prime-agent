//! Quota and rate limits, as the plugins track them in the store:
//!
//! - every response to a request the store's token authenticated is read
//!   for the `anthropic-ratelimit-unified-*` windows
//!   (`normalize_quota_headers`); a changed reading is recorded on the row
//!   holding the token (anthropic-napi `recordQuotaHeaders`), and a served
//!   request marks its row used (`markUsed`, at most every five minutes);
//!   the writes run on the keep-alive thread, never on the request;
//! - the usage poll (`GET /api/oauth/usage`, the plugins' `QuotaManager`):
//!   the windows, their resets, the model-scoped weekly windows and the
//!   extra-usage credits of one login, polled on the keep-alive thread when
//!   the login's reading is due (the sidecar's `quota.checkIntervalMinutes`,
//!   five by default; a minute after the reset of a window below its
//!   minimum; never-polled header readings once per interval; every
//!   `quota.refreshEveryNRequests` requests), with the plugins' backoff
//!   (one to fifteen minutes, five for a non-transient failure, none for a
//!   401/403) and at most one poll a second; the result merges with the
//!   header readings (a newer header window wins) and its percentages are
//!   recorded on the row holding the token it was read with, as the pi
//!   plugin's `recordQuota` does;
//! - the latest reading is the quota line the agents view shows for an
//!   Anthropic session (`publish_feature_status`; prime-agent has no other
//!   usage surface);
//! - a 429 (or a stream opening with a rate-limit or overload error) puts
//!   the row in a cooldown (`retry-after`, else the binding window's reset,
//!   else a minute) and unpins it (napi `markRateLimited`), and the request
//!   moves to the next login in the store's order, as the plugins' fallback
//!   pass does;
//! - an optional quota reserve (`ANTHROPIC_QUOTA_RESERVE_PCT`, the napi
//!   `reservePct`) prefers logins whose recorded usage is below it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anthropic::access::{get_access_token, AccessRequest};
use anthropic::account::QuotaObservation;
use anthropic::backoff::FailureFacts;
use anthropic::quota::{
    is_quota_bearing_header_frame, normalize_quota_headers, QuotaFieldSource, QuotaPolicy,
    QuotaSnapshot,
};
use anthropic::quota_manager::{PollDecision, PollOutcome, QuotaManager};
use anthropic::retry::retry_after_ms;
use anthropic::token::AccessToken;
use anthropic::{Account, AccountStore, OAuthClient, SharedRefreshOptions};
use chrono::{DateTime, Duration, Utc};
use pa_types::sync::MutexExt;

/// The environment variable naming the quota reserve, percent (0-100).
pub const QUOTA_RESERVE_ENV: &str = "ANTHROPIC_QUOTA_RESERVE_PCT";
/// A cooldown with no server-directed end (the SDK's minimum quota delay).
const DEFAULT_COOLDOWN_SECS: i64 = 60;
/// A row's use is recorded at most this often.
const MARK_USED_EVERY: std::time::Duration = std::time::Duration::from_mins(5);
/// An access token this close to expiry is not polled with (the plugins'
/// `ACCESS_TOKEN_EXPIRY_MARGIN_MS`).
const ACCESS_EXPIRY_MARGIN_MS: i64 = 60_000;

/// A store write the request path hands to the keep-alive thread.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StoreWrite {
    /// A quota reading for the row holding `access_token`.
    Quota {
        access_token: String,
        snapshot: Box<QuotaSnapshot>,
    },
    /// The row served a request.
    Used { account_id: String },
}

impl StoreWrite {
    /// Apply under the store lock. A failure is logged and dropped: a
    /// routing decision never depends on a bookkeeping write.
    pub(crate) fn apply(&self, store_path: &std::path::Path) {
        let now = Utc::now();
        let written = AccountStore::mutate(store_path, |store| {
            match self {
                Self::Quota {
                    access_token,
                    snapshot,
                } => {
                    store.record_quota_snapshot_for_access_token(access_token, snapshot, now);
                }
                Self::Used { account_id } => {
                    if let Ok(account) = store.get_mut(account_id) {
                        account.mark_used(now);
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = written {
            tracing::debug!(%error, "a shared store bookkeeping write failed");
        }
    }
}

/// What this process knows about each login's quota.
#[derive(Default)]
pub(crate) struct QuotaTracker {
    /// The latest header reading per store row (what was last recorded).
    readings: Mutex<HashMap<String, QuotaSnapshot>>,
    /// When each row's use was last recorded.
    used: Mutex<HashMap<String, Instant>>,
    /// Header readings and usage polls per row, merged: the SDK's port of
    /// the plugins' `QuotaManager` (cadence, backoff, identity fencing).
    manager: Mutex<QuotaManager>,
    /// Requests this process sent with the store's tokens (the cadence of
    /// `quota.refreshEveryNRequests`).
    requests: AtomicU64,
    /// Rows with a poll queued that has not run yet.
    queued: Mutex<HashSet<String>>,
    /// Usage polls sent.
    polled: AtomicU64,
    /// Usage polls that failed.
    poll_failed: AtomicU64,
}

impl QuotaTracker {
    /// Read a response for the row `account_id` (its token `access_token`,
    /// its account `lineage`): the store writes it calls for.
    pub(crate) fn observe(
        &self,
        account_id: &str,
        lineage: Option<&str>,
        access_token: &str,
        status: u16,
        headers: &[(String, String)],
        now: DateTime<Utc>,
    ) -> Vec<StoreWrite> {
        let mut writes = Vec::new();
        if is_quota_bearing_header_frame(headers) {
            let snapshot = normalize_quota_headers(headers, now.timestamp_millis());
            self.manager.lock_or_recover().push_headers(
                account_id,
                lineage,
                &snapshot,
                now.timestamp_millis(),
            );
            let mut readings = self.readings.lock_or_recover();
            let changed = readings
                .get(account_id)
                .is_none_or(|known| percents(known) != percents(&snapshot));
            readings.insert(account_id.to_string(), snapshot.clone());
            if changed {
                writes.push(StoreWrite::Quota {
                    access_token: access_token.to_string(),
                    snapshot: Box::new(snapshot),
                });
            }
        }
        if (200..300).contains(&status) {
            let mut used = self.used.lock_or_recover();
            if used
                .get(account_id)
                .is_none_or(|at| at.elapsed() >= MARK_USED_EVERY)
            {
                used.insert(account_id.to_string(), Instant::now());
                writes.push(StoreWrite::Used {
                    account_id: account_id.to_string(),
                });
            }
        }
        writes
    }

    /// What is known about `account_id`'s quota: header readings and usage
    /// polls merged, else what the store recorded for it.
    pub(crate) fn snapshot(&self, account_id: &str) -> Option<QuotaSnapshot> {
        self.manager
            .lock_or_recover()
            .get(account_id)
            .map(|entry| entry.quota.clone())
    }

    /// Adopt the sidecar's quota policy (cadence, minimums).
    pub(crate) fn set_policy(&self, policy: &QuotaPolicy) {
        let mut manager = self.manager.lock_or_recover();
        if manager.policy() != policy {
            manager.set_policy(policy.clone());
        }
    }

    /// Count one request sent with a store token; its number.
    pub(crate) fn count_request(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Whether `account_id` is due for a poll for request number
    /// `request_count` of `model`, and not queued yet (it is now).
    pub(crate) fn claim_due_poll(
        &self,
        account_id: &str,
        request_count: u64,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        let due = self.manager.lock_or_recover().needs_refresh(
            account_id,
            request_count,
            model,
            now.timestamp_millis(),
        );
        due && self.claim_poll(account_id)
    }

    /// Usage polls sent, and how many failed.
    pub(crate) fn poll_counts(&self) -> (u64, u64) {
        (
            self.polled.load(Ordering::SeqCst),
            self.poll_failed.load(Ordering::SeqCst),
        )
    }

    /// Mark a poll of `account_id` queued; `false` when one already is.
    pub(crate) fn claim_poll(&self, account_id: &str) -> bool {
        self.queued.lock_or_recover().insert(account_id.to_string())
    }
}

/// The account identity a row's quota belongs to (its account uuid).
pub(crate) fn lineage(account: &Account) -> Option<String> {
    account
        .oauth()?
        .account
        .as_ref()
        .map(|account| account.uuid.clone())
        .filter(|uuid| !uuid.trim().is_empty())
}

/// What one usage poll did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollRun {
    /// The reading landed (in this process and on the row).
    Applied,
    /// Not sent: one is in flight, the poll is backed off, the row is gone
    /// or has no usable token.
    Skipped,
    /// Sent, and it failed.
    Failed,
}

/// One usage poll for the row `account_id` (the plugins' `refreshMain` /
/// `refreshFallback`): its live access token (refreshed under the store's
/// claim when it has expired), the poll, the merge, the row's percentages.
pub(crate) async fn poll_usage(
    store_path: &Path,
    client: &OAuthClient,
    book: &QuotaTracker,
    account_id: &str,
) -> PollRun {
    let run = async {
        let now = Utc::now();
        let Some(row) = AccountStore::load(store_path)
            .ok()
            .and_then(|store| store.get(account_id).cloned())
        else {
            return PollRun::Skipped;
        };
        let ticket = {
            let mut manager = book.manager.lock_or_recover();
            manager.bind_lineage(account_id, lineage(&row).as_deref());
            match manager.begin_poll(account_id, now.timestamp_millis()) {
                PollDecision::Poll(ticket) => ticket,
                PollDecision::InFlight | PollDecision::Cached(_) | PollDecision::BackedOff => {
                    return PollRun::Skipped;
                }
            }
        };
        let live = row
            .oauth()
            .filter(|tokens| {
                tokens.expires_at > now + Duration::milliseconds(ACCESS_EXPIRY_MARGIN_MS)
            })
            .map(|tokens| tokens.access.expose().to_string());
        let token = match live {
            Some(token) => Some(token),
            None => get_access_token(
                client,
                store_path,
                &AccessRequest {
                    account: Some(account_id.to_string()),
                    ..AccessRequest::default()
                },
                &SharedRefreshOptions::default(),
            )
            .await
            .ok()
            .filter(|grant| grant.account_id == account_id)
            .map(|grant| grant.access_token),
        };
        let Some(token) = token else {
            book.manager.lock_or_recover().abandon_poll(ticket);
            return PollRun::Skipped;
        };
        book.manager
            .lock_or_recover()
            .mark_poll_dispatched(&ticket, Utc::now().timestamp_millis());
        book.polled.fetch_add(1, Ordering::SeqCst);
        let result = client
            .usage(&AccessToken::new(token.clone()))
            .await
            .map(|usage| QuotaSnapshot::from_usage_response(&usage, Utc::now().timestamp_millis()))
            .map_err(|error| FailureFacts::from_error(&error));
        let outcome = book.manager.lock_or_recover().complete_poll(
            ticket,
            Utc::now().timestamp_millis(),
            result,
        );
        match outcome {
            PollOutcome::Applied(entry) => {
                StoreWrite::Quota {
                    access_token: token,
                    snapshot: Box::new(entry.quota),
                }
                .apply(store_path);
                PollRun::Applied
            }
            PollOutcome::Superseded(_) => PollRun::Skipped,
            PollOutcome::Failed { backoff } => {
                book.poll_failed.fetch_add(1, Ordering::SeqCst);
                tracing::debug!(
                    backed_off = backoff.is_some(),
                    "a shared store login's usage poll failed"
                );
                PollRun::Failed
            }
        }
    }
    .await;
    book.queued.lock_or_recover().remove(account_id);
    run
}

/// The used percentages of a reading.
fn percents(snapshot: &QuotaSnapshot) -> (Option<f64>, Option<f64>) {
    (
        snapshot
            .five_hour
            .as_ref()
            .map(|window| window.used_percent),
        snapshot
            .seven_day
            .as_ref()
            .map(|window| window.used_percent),
    )
}

/// A login's quota as the agents view shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuotaLine {
    /// `Claude quota: 5h 48% / 7d 55% used`.
    pub(crate) line: String,
    /// `{fiveHourPercent, sevenDayPercent, checkedAt, source}`.
    pub(crate) status: serde_json::Value,
}

/// The line for this process's reading (header readings and usage polls
/// merged), else the row's recorded observation.
pub(crate) fn quota_line(
    reading: Option<&QuotaSnapshot>,
    recorded: Option<&QuotaObservation>,
) -> Option<QuotaLine> {
    let (five, seven, checked_at, source) = match (reading, recorded) {
        (Some(reading), _) if reading.has_standard_windows() => {
            let (five, seven) = percents(reading);
            let source = match reading.source {
                Some(QuotaFieldSource::Poll) => "poll",
                Some(QuotaFieldSource::Headers) => "headers",
                // What the store recorded, learned from the row.
                None => "store",
            };
            (five, seven, Some(reading.checked_at_max()), source)
        }
        (_, Some(recorded))
            if recorded.five_hour_percent.is_some() || recorded.seven_day_percent.is_some() =>
        {
            (
                recorded.five_hour_percent,
                recorded.seven_day_percent,
                recorded.checked_at.map(|at| at.timestamp_millis()),
                "store",
            )
        }
        _ => return None,
    };
    let percent = |value: Option<f64>| {
        value.map_or_else(|| "?".to_string(), |value| format!("{}%", value.round()))
    };
    Some(QuotaLine {
        line: format!(
            "Claude quota: 5h {} / 7d {} used",
            percent(five),
            percent(seven)
        ),
        status: serde_json::json!({
            "fiveHourPercent": five,
            "sevenDayPercent": seven,
            "checkedAt": checked_at,
            "source": source,
        }),
    })
}

/// When a rate-limited login may serve again: the server's `retry-after`,
/// else the reset of the window the server named binding (or the later
/// standard reset it reported), else a minute.
pub(crate) fn cooldown_until(headers: &[(String, String)], now: DateTime<Utc>) -> DateTime<Utc> {
    if let Some(ms) = retry_after_ms(headers, now.timestamp_millis()) {
        #[allow(clippy::cast_possible_truncation)]
        // a delay in milliseconds, bounded by the header
        return now + Duration::milliseconds(ms as i64);
    }
    let reset = |suffix: &str| {
        anthropic::retry::HeaderLookup::header(
            headers,
            &format!("anthropic-ratelimit-unified-{suffix}-reset"),
        )
        .and_then(|value| value.trim().parse::<i64>().ok())
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .filter(|at| *at > now)
    };
    let claim = anthropic::retry::HeaderLookup::header(
        headers,
        anthropic::retry::REPRESENTATIVE_CLAIM_HEADER,
    );
    let binding = match claim {
        Some("five_hour") => reset("5h"),
        Some("seven_day") => reset("7d"),
        _ => None,
    };
    binding
        .or_else(|| reset("5h").into_iter().chain(reset("7d")).max())
        .unwrap_or_else(|| now + Duration::seconds(DEFAULT_COOLDOWN_SECS))
}

/// The quota reserve the environment names, when valid (0-100).
pub(crate) fn reserve_from_env() -> Option<f64> {
    std::env::var(QUOTA_RESERVE_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
}

#[cfg(test)]
mod tests;
