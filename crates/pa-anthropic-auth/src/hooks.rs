//! The store in the `anthropic` provider's requests (pa-ai's provider
//! request hooks): each request carries the store's current token for the
//! login and the pi plugin's request shape (`shape.rs`), and a 401 gets the
//! plugins' one claimed recovery
//! (anthropic-napi's `handleUnauthorized`, the pi plugin's
//! `recoverSharedAccessTokenAfter401`). Only tokens this source served are
//! touched: a runtime key or another store's login is sent as it is.

use std::sync::atomic::Ordering;

use anthropic::quota::{is_quota_bearing_header_frame, normalize_quota_headers};
use anthropic::refresh::{RefreshFailure, classify_refresh_failure};
use anthropic::{AccountStore, SharedRefreshOptions};
use pa_ai::request_hooks::{
    Admission,
    OutgoingRequest,
    PendingRequest,
    ProviderRequestHooks,
    RejectedRequest,
    Rejection,
};
use pa_ai::types::{Model, ProviderResponse};
use pa_types::sync::MutexExt;
use serde_json::Value;

use crate::SharedStoreSource;
use crate::quota::cooldown_until;
use crate::routing::RouteRequest;
use crate::shape::ShapeIdentity;
use crate::source::{PollWait, ServedToken, UsageEvent, block_on_own_runtime, token_fingerprint};

impl SharedStoreSource {
    /// The store's token for the request now: the routing order's pick
    /// (refreshed under the store's claim when it has expired). `None` when
    /// it cannot produce one (the request goes out with what it has, and
    /// its rejection is recovered or reported).
    fn store_token(&self) -> Option<String> {
        let _flight = self.flight.lock_or_recover();
        let grant = match self.resolve() {
            Ok(Ok(grant)) => grant,
            Ok(Err(error)) => {
                tracing::info!(
                    kind = ?error.kind,
                    "the shared store could not resolve an access token"
                );
                return None;
            }
            Err(error) => {
                tracing::info!(error = %error, "resolving a shared store token failed to run");
                return None;
            }
        };
        self.record(Some(grant.source));
        self.remember(&grant.access_token, &grant.account_id, grant.expires_at);
        Some(grant.access_token)
    }

    /// After a 429 (or a rate-limited stream opening), as the plugins'
    /// routing does:
    ///
    /// - a sticky session (pi's sticky pass): a 429 or a `rate_limit_error`
    ///   opening is checked with a usage poll of the login; only a confirmed
    ///   long-lived exhaustion moves the session to another login, anything
    ///   else (an overload, a short 5h hold, an unconfirmed limit) keeps it
    ///   there and the 429 is reported;
    /// - otherwise the row cools down until the server lets it serve again
    ///   and is unpinned (napi `markRateLimited`), its quota reading is
    ///   recorded and confirmed by a usage poll, and the request moves to
    ///   the first other login that passes the quota policy (polled first
    ///   when its reading is due), when there is one.
    fn rotate_after_rate_limit(&self, rejected: &RejectedRequest<'_>) -> Option<String> {
        let served = self.served_token(rejected.api_key)?;
        let model = rejected.model.id.as_str();
        if self.routes_sticky() {
            let confirmable =
                rejected.status == 429 || rejected.provider_error_type == Some("rate_limit_error");
            if !confirmable || !self.sticky_migrates_after(&served.account_id, model) {
                tracing::info!(
                    status = rejected.status,
                    "a sticky shared store login was rate-limited; keeping the session on it"
                );
                return None;
            }
        } else {
            let headers: Vec<(String, String)> = rejected
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            let now = chrono::Utc::now();
            let until = cooldown_until(&headers, now);
            let marked = AccountStore::mutate(&self.config.store_path, |store| {
                if is_quota_bearing_header_frame(&headers) {
                    store.record_quota_snapshot_for_access_token(
                        rejected.api_key,
                        &normalize_quota_headers(&headers, now.timestamp_millis()),
                        now,
                    );
                }
                let Ok(account) = store.get_mut(&served.account_id) else {
                    return Ok(false);
                };
                account.mark_rate_limited(until);
                if store.current.as_deref() == Some(served.account_id.as_str()) {
                    store.current = None;
                }
                Ok(true)
            });
            if !matches!(marked, Ok(true)) {
                return None;
            }
            // The plugins confirm a 429 with a usage poll before moving on:
            // its reading lands on the row, so an exhausted login stays
            // skipped after its cooldown.
            self.queue_poll(&served.account_id, PollWait::Result);
        }
        let next = self
            .routed_token(&RouteRequest {
                model,
                context_bytes: self.counts.last_context_bytes.load(Ordering::SeqCst),
                exclude: Some(&served.account_id),
            })
            .ok()
            .flatten()?;
        let moved = next != rejected.api_key
            && self
                .served_token(&next)
                .is_some_and(|login| login.account_id != served.account_id);
        tracing::info!(
            moved,
            status = rejected.status,
            "a shared store login was rate-limited"
        );
        if moved {
            self.count(UsageEvent::Rotated);
        }
        moved.then_some(next)
    }

    /// One claimed refresh of the row that owns `rejected` after a 401:
    /// a new token only when the refresh produced a new version of the same
    /// login (`decide_retry_after_401`). A login whose refresh token is
    /// revoked is gone for good: the request moves to the login the routing
    /// takes past it (the pi plugin's fallback past a dead login), when there
    /// is one. A token the store no longer holds (another process rotated
    /// it) is answered with the store's current token, re-read under the
    /// store lock, when that is a different one.
    fn recover_unauthorized(&self, request: &RejectedRequest<'_>) -> Option<String> {
        let rejected = request.api_key;
        // Re-read the store first, as Codex's session manager does ("recovery
        // always rereads under the lock before choosing a token"). A tool that
        // shares the store (Claude Code) may have already rotated a fresh token
        // in, and `store_token` also refreshes a merely-expired token under the
        // store claim - either way, when the store now yields a token other
        // than the rejected one, use it without the explicit 401-refresh dance
        // (no needless lock fight with the peer mid-refresh). This is the
        // overnight "token revoked" case: the presented token was invalidated
        // by someone else's rotation, and a daemon restart used to be the only
        // way to pick the new one up.
        if let Some(current) = self.reread_store_token(rejected) {
            return Some(current);
        }
        // The store still holds the rejected token: claim a refresh, and if the
        // login's refresh token is revoked, rotate to another login.
        tracing::debug!(
            "a 401 arrived and the store still yields the rejected token; forcing a claimed refresh"
        );
        let recovery = {
            let _flight = self.flight.lock_or_recover();
            block_on_own_runtime(self.client().recover_unauthorized(
                &self.config.store_path,
                rejected,
                &SharedRefreshOptions::default(),
            ))
        };
        // The refresh could not run (a held store claim, an I/O/runtime error):
        // re-read once more in case a peer rotated a token in while it ran. Each
        // way of not running is logged so a stuck 401 is never a silent no-op.
        let recovery = match recovery {
            Ok(Ok(recovery)) => recovery,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "a 401 claimed refresh returned an error from the store SDK; re-reading the store"
                );
                return self.reread_store_token(rejected);
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "a 401 claimed refresh could not run (held claim or runtime error); re-reading the store"
                );
                return self.reread_store_token(rejected);
            }
        };
        if let Some(outcome) = recovery.outcome.filter(|_| recovery.decision.retry) {
            let token = outcome.tokens.access.expose().to_string();
            if let Some(account_id) = &outcome.account_id {
                self.remember(&token, account_id, outcome.tokens.expires_at);
            }
            self.count_refreshed();
            self.count(UsageEvent::Recovered);
            tracing::info!("the shared store's token was rejected with 401; refreshed");
            return Some(token);
        }
        if let Some(failure) = &recovery.failure {
            if classify_refresh_failure(failure) == RefreshFailure::Revoked {
                // Routed afresh: the store now records the login dead, so
                // the routing passes over it as if it were not there.
                let revoked = self.served_token(rejected).map(|served| served.account_id);
                let next = self
                    .routed_token(&RouteRequest {
                        model: &request.model.id,
                        context_bytes: self.counts.last_context_bytes.load(Ordering::SeqCst),
                        exclude: None,
                    })
                    .ok()
                    .flatten()
                    .filter(|next| {
                        self.served_token(next)
                            .is_some_and(|login| Some(login.account_id) != revoked)
                    });
                if let Some(next) = next {
                    self.count(UsageEvent::Recovered);
                    tracing::info!(
                        "the shared store's token was rejected with 401 and its login is revoked; moved to another login"
                    );
                    return Some(next);
                }
                tracing::warn!(
                    reason = recovery.decision.reason.as_str(),
                    "a 401 login is revoked and no other login can serve the request; re-login is required (run /login anthropic)"
                );
                return self.reread_store_token(rejected);
            }
            tracing::warn!(
                reason = recovery.decision.reason.as_str(),
                failure = %failure,
                "the shared store's token was rejected with 401 and could not be refreshed"
            );
            // The refresh failed (a held store claim, a transient error), but a
            // tool sharing the store may still have rotated a fresh token in -
            // re-read before giving up.
            return self.reread_store_token(rejected);
        }
        // The SDK neither refreshed nor reported a failure: it declined to retry
        // (decision.reason names why - e.g. the refresh returned the same token
        // version). Surface it so a stuck 401 is never an invisible dead end.
        tracing::warn!(
            reason = recovery.decision.reason.as_str(),
            "a 401 recovery produced no new token and no failure; the request will surface its error"
        );
        self.reread_store_token(rejected)
    }

    /// After a 401, re-read the store's current token: a tool that shares the
    /// store (Claude Code) may have rotated a fresh one in. `Some` only when it
    /// differs from the rejected token - otherwise there is nothing new to try.
    fn reread_store_token(&self, rejected: &str) -> Option<String> {
        let Some(current) = self.store_token() else {
            tracing::info!(
                "re-reading the store after a 401 produced no token (the store could not resolve one)"
            );
            return None;
        };
        if current == rejected {
            tracing::info!(
                token_fp = token_fingerprint(rejected),
                "re-read the store after a 401; it still holds the rejected token"
            );
            return None;
        }
        self.count(UsageEvent::Recovered);
        tracing::info!(
            rejected_fp = token_fingerprint(rejected),
            token_fp = token_fingerprint(&current),
            "the shared store's token was rejected with 401; re-read a newer one"
        );
        Some(current)
    }
}

impl ProviderRequestHooks for SharedStoreSource {
    fn prepare(&self, request: &mut OutgoingRequest<'_>) {
        let Some(served) = self.served_token(request.api_key) else {
            return;
        };
        let identity = ShapeIdentity {
            device_id: self.device_id(),
            account_uuid: served.account_uuid,
            session_id: self.session_id(&served.account_id),
        };
        crate::pi::prepare(self, request, &identity);
        // The plugin tracks each send for its cache keep-alive.
        self.track_cachekeep(request, &served.account_id);
    }

    fn response_event(&self, _model: &Model, api_key: &str, event: Value) -> Vec<Value> {
        if !self.served(api_key) {
            return vec![event];
        }
        crate::pi::response_event(event)
    }

    fn admit(&self, request: &PendingRequest<'_>) -> Admission {
        let model = request.model.id.as_str();
        if !self.served(request.api_key) {
            log_send(model, request.api_key, None, "unserved");
            return Admission::Send;
        }
        self.counts
            .last_context_bytes
            .store(request.context_bytes, Ordering::SeqCst);
        let routed = self.routed_token(&RouteRequest {
            model: &request.model.id,
            context_bytes: request.context_bytes,
            exclude: None,
        });
        let current = match routed {
            Ok(Some(current)) => current,
            Ok(None) => {
                let kept = self.served_token(request.api_key);
                log_send(model, request.api_key, kept.as_ref(), "kept");
                return Admission::Send;
            }
            Err(refusal) => return Admission::Refuse(refusal),
        };
        // The request's login is polled for its usage when its reading is
        // due (or every N requests), on the keep-alive thread.
        if let Some(served) = self.served_token(&current) {
            let count = self.quota.count_request();
            if self.quota.claim_due_poll(
                &served.account_id,
                count,
                Some(&request.model.id),
                chrono::Utc::now(),
            ) {
                self.run_poll(&served.account_id, PollWait::Background);
            }
        }
        let route = if current == request.api_key {
            "routed"
        } else {
            "swapped"
        };
        log_send(model, &current, self.served_token(&current).as_ref(), route);
        if current == request.api_key {
            Admission::Send
        } else {
            Admission::SendWith(current)
        }
    }

    fn rejected(&self, rejected: &RejectedRequest<'_>) -> Option<String> {
        let kind = match rejected.rejection {
            Rejection::Unauthorized => "unauthorized",
            Rejection::RateLimited => "rate-limited",
        };
        let token_fp = token_fingerprint(rejected.api_key);
        let Some(served) = self.served_token(rejected.api_key) else {
            tracing::info!(
                status = rejected.status,
                rejection = kind,
                token_fp,
                "a rejection arrived for a token this source did not serve; not recovering"
            );
            return None;
        };
        tracing::info!(
            status = rejected.status,
            rejection = kind,
            account_id = served.account_id.as_str(),
            token_fp,
            expires_at = %served.expires_at,
            expires_in_s = (served.expires_at - chrono::Utc::now()).num_seconds(),
            "a request with a shared store token was rejected"
        );
        self.pi.context1m.observe(
            &rejected.model.id,
            rejected.api_key,
            rejected.status,
            rejected.body,
        );
        let recovered = match rejected.rejection {
            Rejection::Unauthorized => self.recover_unauthorized(rejected),
            Rejection::RateLimited => self.rotate_after_rate_limit(rejected),
        };
        if recovered.is_none() {
            tracing::warn!(
                status = rejected.status,
                rejection = kind,
                account_id = served.account_id.as_str(),
                token_fp,
                "a shared store rejection was not recovered; the request will surface its error"
            );
        }
        recovered
    }

    fn observe(&self, _model: &Model, api_key: &str, response: &ProviderResponse) {
        let Some(served) = self.served_token(api_key) else {
            return;
        };
        let headers: Vec<(String, String)> = response
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        for write in self.quota.observe(
            &served.account_id,
            served.account_uuid.as_deref(),
            api_key,
            response.status,
            &headers,
            chrono::Utc::now(),
        ) {
            self.queue_write(write);
        }
    }
}

#[cfg(test)]
mod tests;

/// Log the token a request goes out with (`route`: how admission chose it):
/// its login, fingerprint and expiry. The store refreshes a token before
/// serving it, so an expired one is a warning: a stale credential reached
/// the request.
fn log_send(model: &str, token: &str, served: Option<&ServedToken>, route: &str) {
    let token_fp = token_fingerprint(token);
    let Some(served) = served else {
        tracing::info!(
            model,
            token_fp,
            route,
            "an anthropic request carries a token the shared store did not serve; it is sent as it is"
        );
        return;
    };
    let expires_in_s = (served.expires_at - chrono::Utc::now()).num_seconds();
    if expires_in_s <= 0 {
        tracing::warn!(
            model,
            account_id = served.account_id.as_str(),
            token_fp,
            expires_at = %served.expires_at,
            expires_in_s,
            route,
            "an anthropic request is sending an expired shared store token"
        );
    } else {
        tracing::info!(
            model,
            account_id = served.account_id.as_str(),
            token_fp,
            expires_at = %served.expires_at,
            expires_in_s,
            route,
            "an anthropic request's shared store token"
        );
    }
}
