//! Quota and rate limits, as the plugins track them in the store:
//!
//! - every response to a request the store's token authenticated is read
//!   for the `anthropic-ratelimit-unified-*` windows
//!   (`normalize_quota_headers`); a changed reading is recorded on the row
//!   holding the token (anthropic-napi `recordQuotaHeaders`), and a served
//!   request marks its row used (`markUsed`, at most every five minutes);
//!   the writes run on the keep-alive thread, never on the request;
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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use anthropic::account::QuotaObservation;
use anthropic::quota::{is_quota_bearing_header_frame, normalize_quota_headers, QuotaSnapshot};
use anthropic::retry::retry_after_ms;
use anthropic::AccountStore;
use chrono::{DateTime, Duration, Utc};
use pa_types::sync::MutexExt;

/// The environment variable naming the quota reserve, percent (0-100).
pub const QUOTA_RESERVE_ENV: &str = "ANTHROPIC_QUOTA_RESERVE_PCT";
/// A cooldown with no server-directed end (the SDK's minimum quota delay).
const DEFAULT_COOLDOWN_SECS: i64 = 60;
/// A row's use is recorded at most this often.
const MARK_USED_EVERY: std::time::Duration = std::time::Duration::from_mins(5);

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

/// What the request path read about quota, per login.
#[derive(Default)]
pub(crate) struct QuotaTracker {
    /// The latest header reading per store row.
    readings: Mutex<HashMap<String, QuotaSnapshot>>,
    /// When each row's use was last recorded.
    used: Mutex<HashMap<String, Instant>>,
}

impl QuotaTracker {
    /// Read a response for the row `account_id` (its token `access_token`):
    /// the store writes it calls for.
    pub(crate) fn observe(
        &self,
        account_id: &str,
        access_token: &str,
        status: u16,
        headers: &[(String, String)],
        now: DateTime<Utc>,
    ) -> Vec<StoreWrite> {
        let mut writes = Vec::new();
        if is_quota_bearing_header_frame(headers) {
            let snapshot = normalize_quota_headers(headers, now.timestamp_millis());
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

    /// The latest header reading for `account_id`.
    pub(crate) fn reading(&self, account_id: &str) -> Option<QuotaSnapshot> {
        self.readings.lock_or_recover().get(account_id).cloned()
    }
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

/// The line for a header reading, else the row's recorded observation.
pub(crate) fn quota_line(
    reading: Option<&QuotaSnapshot>,
    recorded: Option<&QuotaObservation>,
) -> Option<QuotaLine> {
    let (five, seven, checked_at, source) = match (reading, recorded) {
        (Some(reading), _) if reading.has_standard_windows() => {
            let (five, seven) = percents(reading);
            (five, seven, Some(reading.checked_at_max()), "headers")
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
