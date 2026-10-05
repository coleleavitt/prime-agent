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
//! - `sticky-balanced` (the plugins' `StickySessionRouter`, through the
//!   SDK's port): each session is assigned a login by spendable quota per
//!   hour until reset (the reserve per window: the larger of the quota
//!   minimum and, when armed, the killswitch threshold) over the prompt
//!   bytes already assigned to it, persisted by hashed session id in the
//!   sidecar's `anthropic-auth-routing-state.json` (shared with pi), and
//!   kept across processes and restarts until a fresh reading shows its 7d
//!   or scoped window spent, its 5h window spent with more than 15 minutes to
//!   the reset (at 15 minutes or less the request is held with a jittered
//!   `retry-after` instead), the killswitch blocks it, the login leaves the
//!   store, or the model changes. A complete pool with no eligible login
//!   answers the plugins' 429 (or 401 when logins need a re-login); an
//!   incomplete one (a reading missing or stale after its poll) falls back
//!   to the ordered pass.
//! - The killswitch (`killswitch.enabled`): a login whose remaining 5h/7d
//!   percent is below its threshold (`killswitch.accounts[<store id>]`, else
//!   `killswitch.main`, else 5%/10%), or whose scoped window for the request
//!   model is at or below its scoped threshold, never serves; unknown quota
//!   blocks under `failClosedOnUnknownQuota`. Its readings are polled first
//!   when due (the opencode plugin's eager refresh). When no login can
//!   serve, the request is refused locally with the opencode plugin's 429
//!   (`Killswitch: no routable accounts. Retry in …`, or the scoped model's
//!   weekly-limit message), never sent.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anthropic::access::{access_candidates, AccessErrorKind, AccessRequest};
use anthropic::killswitch::killswitch_retry_after_secs;
use anthropic::quota::{QuotaSnapshot, QuotaWindowName};
use anthropic::sticky_routing::{
    decide_sticky_quota_failure, sticky_no_route, sticky_quota_snapshot_is_fresh,
    sticky_retry_after_with_jitter, sticky_route_family_for_model, RoutingMode, StickyPolicy,
    StickyQuotaFailureDecision, StickyResolveRequest, StickyRouteCandidate, StickySessionRouter,
};
use anthropic::{Account, AccountStore};
use chrono::{DateTime, Utc};
use pa_ai::request_hooks::LocalRefusal;
use pa_types::sync::MutexExt;

use crate::config::RoutingConfig;
use crate::source::{PollWait, SharedStoreSource};

/// The request a login is chosen for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteRequest<'a> {
    /// The request's model id.
    pub(crate) model: &'a str,
    /// The size of its conversation context (the sticky load estimate).
    pub(crate) context_bytes: u64,
    /// The login this request already failed on (a 429 moving on).
    pub(crate) exclude: Option<&'a str>,
}

/// What the routing did (counts only, the adoption event's facts) and the
/// load it last saw.
#[derive(Debug, Default)]
pub(crate) struct RoutingCounts {
    /// Requests sent past the first login by the quota policy or the
    /// killswitch.
    pub(crate) quota_routed: AtomicU64,
    /// Requests refused locally (killswitch, sticky pool with no login).
    pub(crate) blocked: AtomicU64,
    /// Sessions assigned a login (new or moved).
    pub(crate) sticky_assigned: AtomicU64,
    /// Sessions moved to another login.
    pub(crate) sticky_migrated: AtomicU64,
    /// The context size of the request admitted last (the load a session
    /// moved after a 429 carries to its new login).
    pub(crate) last_context_bytes: AtomicU64,
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
        let session = self.session();
        match (settings.mode, session, &self.config.routing_state_path) {
            (RoutingMode::StickyBalanced, Some(session), Some(state)) => self
                .sticky(&store, &candidates, request, &settings, &session, state)
                .unwrap_or_else(|| self.ordered(&candidates, request, &settings)),
            (
                RoutingMode::MainFirst | RoutingMode::FallbackFirst | RoutingMode::StickyBalanced,
                _,
                _,
            ) => self.ordered(&candidates, request, &settings),
        }
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

    /// The sticky pass for `session`; `None` hands the request to the
    /// ordered pass (an incomplete pool, an unusable state file).
    // Long by design: one decision, pi's `buildStickyRoutes` then its
    // resolution, kept in the plugin's order.
    #[allow(clippy::too_many_lines)]
    fn sticky(
        &self,
        store: &AccountStore,
        candidates: &[&Account],
        request: &RouteRequest<'_>,
        settings: &RoutingConfig,
        session: &str,
        state: &Path,
    ) -> Option<Route> {
        let model = Some(request.model);
        let policy = StickyPolicy {
            quota: settings.quota.clone(),
            killswitch: Some(settings.killswitch.clone()),
        };
        let fresh = |quota: Option<&QuotaSnapshot>| {
            sticky_quota_snapshot_is_fresh(
                quota,
                &policy.quota,
                Utc::now().timestamp_millis(),
                model,
            )
        };
        let passes_killswitch = |id: &str, quota: Option<&QuotaSnapshot>| {
            settings.killswitch.passes(
                quota,
                Some(id),
                model,
                settings.quota.fail_closed_on_unknown,
            )
        };
        let routes: Vec<(&Account, Option<QuotaSnapshot>)> = candidates
            .iter()
            .map(|account| {
                if !fresh(self.quota.snapshot(&account.id).as_ref()) {
                    self.queue_poll(&account.id, PollWait::Result);
                }
                (*account, self.quota.snapshot(&account.id))
            })
            .collect();
        let now = Utc::now().timestamp_millis();
        let retain: HashSet<String> = routes
            .iter()
            .filter(|(account, quota)| {
                let migrate = fresh(quota.as_ref())
                    && matches!(
                        decide_sticky_quota_failure(quota.as_ref(), model, now),
                        StickyQuotaFailureDecision::Migrate(_)
                    );
                !migrate && passes_killswitch(&account.id, quota.as_ref())
            })
            .map(|(account, _)| account.id.clone())
            .collect();
        let eligible: Vec<StickyRouteCandidate> = routes
            .iter()
            .zip(0_i64..)
            .filter_map(|((account, quota), order)| {
                let quota = quota.as_ref()?;
                (policy.quota.passes(Some(quota))
                    && !quota.model_scope_is_exhausted(model)
                    && passes_killswitch(&account.id, Some(quota)))
                .then(|| StickyRouteCandidate {
                    account_id: account.id.clone(),
                    quota: Some(quota.clone()),
                    order,
                })
            })
            .collect();
        let incomplete = routes.iter().any(|(_, quota)| !fresh(quota.as_ref()));
        let exclude: HashSet<String> = request.exclude.map(str::to_string).into_iter().collect();
        let resolved = StickySessionRouter::new(state).resolve(
            &StickyResolveRequest {
                session_id: session,
                family: sticky_route_family_for_model(request.model),
                model_id: model,
                affinity_model_id: None,
                candidates: &eligible,
                retain_account_ids: &retain,
                policy: &policy,
                input_bytes: request.context_bytes,
                preferred_account_id: None,
                exclude_account_ids: Some(&exclude),
            },
            now,
        );
        let resolution = match resolved {
            Ok(Some(resolution)) => resolution,
            Ok(None) if incomplete => return None,
            Ok(None) => {
                self.counts.blocked.fetch_add(1, Ordering::SeqCst);
                let reauth: Vec<String> = store
                    .accounts
                    .iter()
                    .filter(|account| account.enabled && account.refresh_token_is_dead())
                    .map(|account| account.label.clone().unwrap_or_else(|| account.id.clone()))
                    .collect();
                let quotas: Vec<QuotaSnapshot> =
                    routes.into_iter().filter_map(|(_, quota)| quota).collect();
                let no_route = sticky_no_route(false, &reauth, &quotas, model, now);
                let mut headers =
                    BTreeMap::from([("content-type".to_string(), "application/json".to_string())]);
                if let Some(seconds) = no_route.retry_after_secs {
                    headers.insert("retry-after".to_string(), seconds.to_string());
                }
                let refusal = LocalRefusal {
                    status: no_route.status,
                    headers,
                    body: no_route.body().to_string(),
                };
                return Some(Route::Refuse(refusal));
            }
            Err(error) => {
                tracing::warn!(%error, "the sticky routing state is unusable; routing in order");
                return None;
            }
        };
        if resolution.created || resolution.migrated {
            self.counts.sticky_assigned.fetch_add(1, Ordering::SeqCst);
        }
        if resolution.migrated {
            self.counts.sticky_migrated.fetch_add(1, Ordering::SeqCst);
        }
        let quota = self.quota.snapshot(&resolution.account_id);
        if fresh(quota.as_ref()) {
            if let StickyQuotaFailureDecision::Hold { retry_after_secs } =
                decide_sticky_quota_failure(quota.as_ref(), model, now)
            {
                #[allow(clippy::cast_precision_loss)]
                // seconds until a reset within 15 minutes
                let retry_after = sticky_retry_after_with_jitter(session, retry_after_secs as f64);
                return Some(Route::Refuse(rate_limited(
                    "Sticky OAuth account five-hour quota resets shortly; retaining session affinity.",
                    retry_after,
                )));
            }
        }
        Some(Route::Login(resolution.account_id))
    }

    /// What a 429 on the sticky login means for the session (pi's sticky
    /// pass): the login is polled; a confirmed long-lived exhaustion moves
    /// the session (`true`), anything else keeps it on the login and the
    /// 429 is reported.
    pub(crate) fn sticky_migrates_after(&self, account_id: &str, model: &str) -> bool {
        self.queue_poll(account_id, PollWait::Result);
        matches!(
            decide_sticky_quota_failure(
                self.quota.snapshot(account_id).as_ref(),
                Some(model),
                Utc::now().timestamp_millis()
            ),
            StickyQuotaFailureDecision::Migrate(_)
        )
    }

    /// Whether the sidecar routes sessions sticky now (and can).
    pub(crate) fn routes_sticky(&self) -> bool {
        self.settings().mode == RoutingMode::StickyBalanced
            && self.session().is_some()
            && self.config.routing_state_path.is_some()
    }

    /// The session this process serves (the stickiness key).
    pub(crate) fn session(&self) -> Option<String> {
        self.session.lock_or_recover().clone()
    }
}

#[cfg(test)]
mod tests;
