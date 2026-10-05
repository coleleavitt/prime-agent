//! Which login serves a request: the plugins' routing over the store's
//! logins, under the sidecar's `routing.mode`, quota policy and killswitch.
//!
//! The candidates are the store's logins in its routing order (the pinned
//! `current` first; cooling-down and exhausted rows out; under
//! `ANTHROPIC_QUOTA_RESERVE_PCT` when any is). The first plays the plugins'
//! main account, the rest their fallbacks; what is known of each one's
//! quota is this process's readings (headers and polls) over what the store
//! recorded.
//!
//! - `main-first` (default; pi's ordered pass): the first serves unless a
//!   fresh reading has it spent (a window, or the request model's scoped
//!   window, at 0% left; a stale spent reading is confirmed by a poll) or
//!   the killswitch blocks it; then the first other login that passes the
//!   quota policy (`quota.minimumRemaining` per window, unknown quota failing
//!   closed unless `failClosedOnUnknownQuota: false`), the model's scoped
//!   window and the killswitch, each polled first when its reading is due;
//!   none: the first serves anyway (its own answer decides), unless the
//!   killswitch blocks it.
//! - `fallback-first`: those other logins first, then the first.
//! - `sticky-balanced`: routed by the ordered pass (the sticky router is
//!   not wired yet).
//! - The killswitch (`killswitch.enabled`): a login whose remaining 5h/7d
//!   percent is below its threshold (`killswitch.accounts[<store id>]`, else
//!   `killswitch.main`, else 5%/10%), or whose scoped window for the request
//!   model is at or below its scoped threshold, never serves; unknown quota
//!   blocks under `failClosedOnUnknownQuota`. Its readings are polled first
//!   when due (the opencode plugin's eager refresh). When no login can
//!   serve, the request is refused locally with the opencode plugin's 429
//!   (`Killswitch: no routable accounts. Retry in …`, or the scoped model's
//!   weekly-limit message), never sent.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anthropic::access::{access_candidates, AccessErrorKind, AccessRequest};
use anthropic::killswitch::killswitch_retry_after_secs;
use anthropic::quota::{QuotaSnapshot, QuotaWindowName};
use anthropic::sticky_routing::RoutingMode;
use anthropic::{Account, AccountStore};
use chrono::{DateTime, Utc};
use pa_ai::request_hooks::LocalRefusal;

use crate::config::RoutingConfig;
use crate::source::{PollWait, SharedStoreSource};

/// The request a login is chosen for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteRequest<'a> {
    /// The request's model id.
    pub(crate) model: &'a str,
    /// The login this request already failed on (a 429 moving on).
    pub(crate) exclude: Option<&'a str>,
}

/// What the routing did, counts only (the adoption event's facts).
#[derive(Debug, Default)]
pub(crate) struct RoutingCounts {
    /// Requests sent past the first login by the quota policy or the
    /// killswitch.
    pub(crate) quota_routed: AtomicU64,
    /// Requests refused locally (the killswitch).
    pub(crate) blocked: AtomicU64,
}

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// This store row serves it.
    Login(String),
    /// The store's own pick serves it (no candidate to choose among).
    Store,
    /// No other login may take a request that already failed: the
    /// provider's answer is reported.
    Report,
    /// Nothing may serve it: this answer, without sending it.
    Refuse(LocalRefusal),
}

/// The 429 a hold or a block answers with.
fn rate_limited(message: &str, retry_after_secs: u64) -> LocalRefusal {
    LocalRefusal {
        status: 429,
        headers: BTreeMap::from([
            ("content-type".to_string(), "application/json".to_string()),
            ("retry-after".to_string(), retry_after_secs.to_string()),
        ]),
        body: serde_json::json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": message },
        })
        .to_string(),
    }
}

/// A fresh or stale reading says a window is spent (the plugins'
/// `quotaSnapshotIsExhausted`).
fn exhausted(quota: &QuotaSnapshot) -> bool {
    QuotaWindowName::ALL.iter().any(|name| {
        quota
            .window(*name)
            .is_some_and(|window| window.remaining_percent <= 0.0)
    })
}

impl SharedStoreSource {
    /// The login `request` goes to now. Blocking: reads the store and the
    /// sidecar, and may wait for usage polls the decision needs.
    pub(crate) fn route(&self, request: &RouteRequest<'_>) -> Route {
        let settings = self.settings();
        let now = Utc::now();
        let Ok(store) = AccountStore::load(&self.config.store_path) else {
            return Route::Store;
        };
        let candidates = self.candidates(&store, request, now);
        if candidates.is_empty() {
            return match request.exclude {
                Some(_) => Route::Report,
                None => Route::Store,
            };
        }
        for account in &candidates {
            self.quota.seed(account, now);
        }
        self.ordered(&candidates, request, &settings)
    }

    /// The store's logins in routing order: available, under the quota
    /// reserve when any is, minus the request's failed login.
    fn candidates<'s>(
        &self,
        store: &'s AccountStore,
        request: &RouteRequest<'_>,
        now: DateTime<Utc>,
    ) -> Vec<&'s Account> {
        let reserved = self.config.quota_reserve.map(|reserve| {
            access_candidates(
                store,
                &AccessRequest {
                    reserve_percent: Some(reserve),
                    ..AccessRequest::default()
                },
                now,
            )
        });
        let candidates = match reserved {
            Some(Ok(candidates)) => Ok(candidates),
            Some(Err(error)) if error.kind != AccessErrorKind::QuotaReserve => Err(error),
            Some(Err(_)) | None => access_candidates(store, &AccessRequest::default(), now),
        };
        candidates
            .unwrap_or_default()
            .into_iter()
            .filter(|account| !account.refresh_token_is_dead())
            .filter(|account| request.exclude != Some(account.id.as_str()))
            .collect()
    }

    /// Poll `account_id` and wait, when its reading is due for `model`.
    fn poll_if_due(&self, account_id: &str, model: &str) {
        if self.quota.is_stale(account_id, Some(model), Utc::now()) {
            self.queue_poll(account_id, PollWait::Result);
        }
    }

    /// The ordered pass (`main-first` / `fallback-first`).
    fn ordered(
        &self,
        candidates: &[&Account],
        request: &RouteRequest<'_>,
        settings: &RoutingConfig,
    ) -> Route {
        let model = Some(request.model);
        let killswitch = &settings.killswitch;
        let policy = &settings.quota;
        if killswitch.enabled {
            for account in candidates {
                self.poll_if_due(&account.id, request.model);
            }
        }
        let passes_killswitch = |id: &str, quota: Option<&QuotaSnapshot>| {
            killswitch.passes(quota, Some(id), model, policy.fail_closed_on_unknown)
        };
        // A request moving on from a failed login has no first login: every
        // remaining one is a fallback that must pass the policy.
        let (main, others) = match request.exclude {
            Some(_) => (None, candidates),
            None => (Some(candidates[0]), &candidates[1..]),
        };
        let spent =
            |quota: &QuotaSnapshot| exhausted(quota) || quota.model_scope_is_exhausted(model);
        let mut main_quota = main.and_then(|main| self.quota.snapshot(&main.id));
        if let Some(main) = main {
            if main_quota.as_ref().is_some_and(spent)
                && self.quota.is_stale(&main.id, model, Utc::now())
            {
                // A stale spent reading is confirmed before the first is
                // passed over (the opencode plugin's synchronous re-check).
                self.queue_poll(&main.id, PollWait::Result);
                main_quota = self.quota.snapshot(&main.id);
            }
        }
        let main_spent = main.is_some_and(|main| {
            main_quota.as_ref().is_some_and(spent)
                && !self.quota.is_stale(&main.id, model, Utc::now())
        });
        let main_blocked = main.is_some_and(|main| {
            killswitch.enabled && !passes_killswitch(&main.id, main_quota.as_ref())
        });
        let first_eligible_other = || {
            others.iter().find(|account| {
                self.poll_if_due(&account.id, request.model);
                let quota = self.quota.snapshot(&account.id);
                policy.passes(quota.as_ref())
                    && !quota
                        .as_ref()
                        .is_some_and(|quota| quota.model_scope_is_exhausted(model))
                    && passes_killswitch(&account.id, quota.as_ref())
            })
        };
        let other = match (settings.mode, main) {
            (_, None) | (RoutingMode::FallbackFirst, Some(_)) => first_eligible_other(),
            (RoutingMode::MainFirst | RoutingMode::StickyBalanced, Some(_))
                if main_spent || main_blocked =>
            {
                first_eligible_other()
            }
            (RoutingMode::MainFirst | RoutingMode::StickyBalanced, Some(_)) => None,
        };
        if let Some(other) = other {
            if main.is_some() {
                self.counts.quota_routed.fetch_add(1, Ordering::SeqCst);
            }
            return Route::Login(other.id.clone());
        }
        let Some(main) = main else {
            return Route::Report;
        };
        if main_blocked {
            self.counts.blocked.fetch_add(1, Ordering::SeqCst);
            return Route::Refuse(self.killswitch_block(candidates, request, settings));
        }
        Route::Login(main.id.clone())
    }

    /// The opencode plugin's killswitch block: scoped-driven when the first
    /// login's 5h/7d pass and the request model's scoped window is at or
    /// below its threshold (the scoped reset decides the retry), else
    /// account-level (the earliest 5h/7d reset).
    fn killswitch_block(
        &self,
        candidates: &[&Account],
        request: &RouteRequest<'_>,
        settings: &RoutingConfig,
    ) -> LocalRefusal {
        let killswitch = &settings.killswitch;
        let main = candidates[0];
        let main_quota = self.quota.snapshot(&main.id);
        let scoped = main_quota.as_ref().and_then(|quota| {
            let window = quota.scoped_window_for_model(Some(request.model))?;
            let account_level = !killswitch.passes(
                Some(quota),
                Some(&main.id),
                None,
                settings.quota.fail_closed_on_unknown,
            );
            (!account_level
                && window.remaining_percent.is_finite()
                && window.remaining_percent <= killswitch.thresholds_for(Some(&main.id)).scoped)
                .then(|| window.model_name.clone())
        });
        let quotas: Vec<Option<QuotaSnapshot>> = candidates
            .iter()
            .map(|account| self.quota.snapshot(&account.id))
            .collect();
        let retry_after = killswitch_retry_after_secs(
            quotas.iter().map(Option::as_ref),
            Utc::now().timestamp_millis(),
            scoped.is_some().then_some(request.model),
        );
        let hint = format!("Retry in {}m {}s.", retry_after / 60, retry_after % 60);
        let message = match scoped {
            Some(model_name) => {
                format!("{model_name} weekly limit reached, no routable accounts. {hint}")
            }
            None => format!("Killswitch: no routable accounts. {hint}"),
        };
        rate_limited(&message, retry_after)
    }
}

#[cfg(test)]
mod tests;
