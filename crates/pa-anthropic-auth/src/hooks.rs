//! The store in the `anthropic` provider's requests (pa-ai's provider
//! request hooks): each request carries the store's current token for the
//! login and the pi plugin's request shape (`shape.rs`), and a 401 gets the
//! plugins' one claimed recovery
//! (anthropic-napi's `handleUnauthorized`, the pi plugin's
//! `recoverSharedAccessTokenAfter401`). Only tokens this source served are
//! touched: a runtime key or another store's login is sent as it is.

use std::sync::atomic::Ordering;

use anthropic::quota::{is_quota_bearing_header_frame, normalize_quota_headers};
use anthropic::{AccountStore, SharedRefreshOptions};
use pa_ai::request_hooks::{
    Admission, OutgoingRequest, PendingRequest, ProviderRequestHooks, RejectedRequest, Rejection,
};
use pa_ai::types::{Model, ProviderResponse};
use pa_types::sync::MutexExt;
use serde_json::Value;

use crate::quota::cooldown_until;
use crate::routing::RouteRequest;
use crate::shape::ShapeIdentity;
use crate::source::{block_on_own_runtime, PollWait, UsageEvent};
use crate::SharedStoreSource;

impl SharedStoreSource {
    /// The store's token for the request now: the routing order's pick
    /// (refreshed under the store's claim when it has expired). `None` when
    /// it cannot produce one (the request goes out with what it has, and
    /// its rejection is recovered or reported).
    fn store_token(&self) -> Option<String> {
        let _flight = self.flight.lock_or_recover();
        let grant = self.resolve().ok()?.ok()?;
        self.record(Some(grant.source));
        self.remember(&grant.access_token, &grant.account_id);
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
    /// login (`decide_retry_after_401`). A token the store no longer holds
    /// (another process rotated it) is answered with the store's current
    /// token, re-read under the store lock, when that is a different one.
    fn recover_unauthorized(&self, rejected: &str) -> Option<String> {
        let recovery = {
            let _flight = self.flight.lock_or_recover();
            block_on_own_runtime(self.client().recover_unauthorized(
                &self.config.store_path,
                rejected,
                &SharedRefreshOptions::default(),
            ))
            .ok()?
            .ok()?
        };
        if let Some(outcome) = recovery.outcome.filter(|_| recovery.decision.retry) {
            let token = outcome.tokens.access.expose().to_string();
            if let Some(account_id) = &outcome.account_id {
                self.remember(&token, account_id);
            }
            self.count_refreshed();
            self.count(UsageEvent::Recovered);
            tracing::info!("the shared store's token was rejected with 401; refreshed");
            return Some(token);
        }
        if recovery.failure.is_some() {
            tracing::warn!(
                reason = recovery.decision.reason.as_str(),
                "the shared store's token was rejected with 401 and could not be refreshed"
            );
            return None;
        }
        let current = self.store_token()?;
        (current != rejected).then(|| {
            self.count(UsageEvent::Recovered);
            tracing::info!("the shared store's token was rejected with 401; re-read a newer one");
            current
        })
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
        if !self.served(request.api_key) {
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
            Ok(None) => return Admission::Send,
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
        if current == request.api_key {
            Admission::Send
        } else {
            Admission::SendWith(current)
        }
    }

    fn rejected(&self, rejected: &RejectedRequest<'_>) -> Option<String> {
        if !self.served(rejected.api_key) {
            return None;
        }
        self.pi.context1m.observe(
            &rejected.model.id,
            rejected.api_key,
            rejected.status,
            rejected.body,
        );
        match rejected.rejection {
            Rejection::Unauthorized => self.recover_unauthorized(rejected.api_key),
            Rejection::RateLimited => self.rotate_after_rate_limit(rejected),
        }
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
