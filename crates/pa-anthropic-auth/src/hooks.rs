//! The store in the `anthropic` provider's requests (pa-ai's provider
//! request hooks): each request carries the store's current token for the
//! login and the pi plugin's request shape (`shape.rs`), and a 401 gets the
//! plugins' one claimed recovery
//! (anthropic-napi's `handleUnauthorized`, the pi plugin's
//! `recoverSharedAccessTokenAfter401`). Only tokens this source served are
//! touched: a runtime key or another store's login is sent as it is.

use anthropic::access::{get_access_token, AccessRequest};
use anthropic::SharedRefreshOptions;
use pa_ai::request_hooks::{OutgoingRequest, ProviderRequestHooks, RejectedRequest, Rejection};
use pa_ai::types::Model;
use pa_types::sync::MutexExt;

use crate::shape::{shape_request, ShapeEnv, ShapeIdentity};
use crate::source::block_on_own_runtime;
use crate::SharedStoreSource;

impl SharedStoreSource {
    /// The store's token for the request now: the routing order's pick
    /// (refreshed under the store's claim when it has expired). `None` when
    /// it cannot produce one (the request goes out with what it has, and
    /// its rejection is recovered or reported).
    fn store_token(&self, request: &AccessRequest) -> Option<String> {
        let _flight = self.flight.lock_or_recover();
        let grant = block_on_own_runtime(get_access_token(
            self.client(),
            &self.config.store_path,
            request,
            &SharedRefreshOptions::default(),
        ))
        .ok()?
        .ok()?;
        self.record(Some(grant.source));
        self.remember(&grant.access_token, &grant.account_id);
        Some(grant.access_token)
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
        let current = self.store_token(&AccessRequest::default())?;
        (current != rejected).then(|| {
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
        shape_request(
            request,
            &identity,
            &self.claude_code_version(),
            &ShapeEnv::from_env(),
            &uuid::Uuid::new_v4().to_string(),
        );
    }

    fn current_credential(&self, _model: &Model, api_key: &str) -> Option<String> {
        if !self.served(api_key) {
            return None;
        }
        self.store_token(&AccessRequest::default())
            .filter(|current| current != api_key)
    }

    fn rejected(&self, rejected: &RejectedRequest<'_>) -> Option<String> {
        if !self.served(rejected.api_key) {
            return None;
        }
        match rejected.rejection {
            Rejection::Unauthorized => self.recover_unauthorized(rejected.api_key),
            Rejection::RateLimited => None,
        }
    }
}

#[cfg(test)]
mod tests;
