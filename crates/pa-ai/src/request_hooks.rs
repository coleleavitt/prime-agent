//! Provider request hooks: a process-wide registry, keyed by provider id,
//! of participants in the requests a provider sends with the credentials
//! they issued (a credential store outside this process that rotates,
//! shapes and recovers its own credentials).
//!
//! Nothing is registered in the native product, and a provider request
//! whose credential no hook claims is sent exactly as before. A provider
//! that supports the hooks (`anthropic-messages`) consults the hooks
//! registered for the request's provider id around each send:
//!
//! 1. [`ProviderRequestHooks::current_credential`] before the request is
//!    built: a fresher credential to send in place of the one the request
//!    was resolved with (the store rotated it since);
//! 2. [`ProviderRequestHooks::prepare`] with the built headers and payload,
//!    and the request as the caller asked for it ([`RequestSource`]: the
//!    conversation and options before the provider converted them), so a
//!    hook may rebuild the body itself and send exact bytes
//!    ([`OutgoingRequest::body`]);
//! 3. [`ProviderRequestHooks::observe`] with every response's status and
//!    headers;
//! 4. [`ProviderRequestHooks::rejected`] when the provider rejected the
//!    credential (HTTP 401) or rate-limited it (HTTP 429, or a stream that
//!    opens with a rate-limit or overload error; with the error body): a
//!    credential to re-send the request with. A 401 is re-sent at most once per request; a rate
//!    limit as long as the hook names a credential the request has not
//!    used yet, up to [`MAX_CREDENTIAL_ATTEMPTS`] sends in all.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock, RwLock};

use pa_types::sync::RwLockExt;

use crate::types::{Context, Model, ModelThinkingLevel, ThinkingBudgets};

/// The most sends one request makes across credential re-sends.
pub const MAX_CREDENTIAL_ATTEMPTS: usize = 8;

/// The request options a caller gave the provider, kept as given (before
/// the provider applied its own defaults) for [`RequestSource`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallerOptions {
    /// The caller's reasoning level, as given (`None`: not set).
    pub reasoning: Option<ModelThinkingLevel>,
    /// The caller's thinking budgets.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// The caller's `max_tokens` (`None`: not set; the provider's default
    /// is not applied here).
    pub max_tokens: Option<u64>,
}

/// The request as the caller asked for it, before the provider converted
/// it into its wire body: what a hook that builds its own body reads.
#[derive(Debug, Clone, Copy)]
pub struct RequestSource<'a> {
    /// The conversation: system prompt, messages and tools.
    pub context: &'a Context,
    /// The caller's options, as given.
    pub options: &'a CallerOptions,
    /// The caller's session id, if any.
    pub session_id: Option<&'a str>,
}

/// A request about to be sent: the hook may rewrite its headers and its
/// JSON payload, or set the exact body bytes to send.
pub struct OutgoingRequest<'a> {
    /// The request's model.
    pub model: &'a Model,
    /// The credential the request authenticates with.
    pub api_key: &'a str,
    /// The request headers, in send order (names as the provider wrote
    /// them).
    pub headers: &'a mut Vec<(String, String)>,
    /// The request body.
    pub payload: &'a mut serde_json::Value,
    /// The serialized body to send in place of `payload`'s own
    /// serialization (`None`: the provider serializes `payload`). A hook
    /// that sets it keeps `payload` the same request.
    pub body: &'a mut Option<String>,
    /// The request as the caller asked for it.
    pub source: RequestSource<'a>,
}

/// Why the provider turned a request down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// HTTP 401: the credential was not accepted.
    Unauthorized,
    /// HTTP 429, or a stream that opened with a rate-limit or overload
    /// error event: the credential's account cannot serve now.
    RateLimited,
}

/// A rejected request.
pub struct RejectedRequest<'a> {
    /// The request's model.
    pub model: &'a Model,
    /// The credential the provider turned down.
    pub api_key: &'a str,
    /// Why.
    pub rejection: Rejection,
    /// The response status (200 for a stream that opened with an error
    /// event).
    pub status: u16,
    /// The provider's error type, when the response named one
    /// (`rate_limit_error`, `overloaded_error`, ...).
    pub provider_error_type: Option<&'a str>,
    /// The response headers (`retry-after`, rate-limit resets).
    pub headers: &'a BTreeMap<String, String>,
    /// The error body the provider answered with (the error event's data
    /// for a stream that opened with one).
    pub body: &'a str,
}

/// A participant in the requests one provider id sends with the
/// credentials it issued.
///
/// Every method has a no-op default, and every method must ignore a
/// credential it did not issue (the request may carry a runtime key or
/// another store's login). [`Self::prepare`] and [`Self::observe`] run
/// inline on the request path and must return promptly (defer disk and
/// network work); [`Self::current_credential`] and [`Self::rejected`] may
/// block on disk and network (the provider calls them on the blocking
/// pool).
pub trait ProviderRequestHooks: Send + Sync {
    /// The credential to send in place of `api_key`, when the hook issued
    /// `api_key` and holds a fresher one for the request; `None` keeps it.
    fn current_credential(&self, model: &Model, api_key: &str) -> Option<String> {
        let _ = (model, api_key);
        None
    }

    /// Adjust a request about to be sent with a credential the hook
    /// issued.
    fn prepare(&self, request: &mut OutgoingRequest<'_>) {
        let _ = request;
    }

    /// Observe the response to a request sent with `api_key`.
    fn observe(&self, model: &Model, api_key: &str, response: &crate::types::ProviderResponse) {
        let _ = (model, api_key, response);
    }

    /// The provider rejected a request sent with a credential the hook
    /// issued: the credential to re-send it with, or `None` to report the
    /// rejection.
    fn rejected(&self, rejected: &RejectedRequest<'_>) -> Option<String> {
        let _ = rejected;
        None
    }
}

type Registry = RwLock<HashMap<String, Arc<dyn ProviderRequestHooks>>>;

fn registry() -> &'static Registry {
    static HOOKS: OnceLock<Registry> = OnceLock::new();
    HOOKS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Install `hooks` for `provider_id`, process-wide, replacing any earlier
/// ones. Called by the composition root before any request is sent.
pub fn install_request_hooks(provider_id: &str, hooks: Arc<dyn ProviderRequestHooks>) {
    registry()
        .write_or_recover()
        .insert(provider_id.to_string(), hooks);
}

/// The hooks installed for `provider_id`, if any.
#[must_use]
pub fn request_hooks(provider_id: &str) -> Option<Arc<dyn ProviderRequestHooks>> {
    registry().read_or_recover().get(provider_id).cloned()
}

/// Rate-limit and overload error types a stream may open with.
pub(crate) const RATE_LIMIT_STREAM_ERRORS: [&str; 2] = ["rate_limit_error", "overloaded_error"];

/// The credentials one request has sent, and what it may still re-send.
pub(crate) struct CredentialAttempts {
    used: Vec<String>,
    unauthorized_retried: bool,
}

impl CredentialAttempts {
    pub(crate) fn new(first: &str) -> Self {
        Self {
            used: vec![first.to_string()],
            unauthorized_retried: false,
        }
    }

    /// Whether `rejection` may still be re-sent at all.
    pub(crate) fn may_retry(&self, rejection: Rejection) -> bool {
        self.used.len() < MAX_CREDENTIAL_ATTEMPTS
            && match rejection {
                Rejection::Unauthorized => !self.unauthorized_retried,
                Rejection::RateLimited => true,
            }
    }

    /// Record a re-send with `next` after `rejection`; `false` when `next`
    /// is a credential a rate-limited request already used (re-sending it
    /// would only be turned down again).
    pub(crate) fn admit(&mut self, rejection: Rejection, next: &str) -> bool {
        match rejection {
            Rejection::Unauthorized => self.unauthorized_retried = true,
            Rejection::RateLimited => {
                if self.used.iter().any(|used| used == next) {
                    return false;
                }
            }
        }
        self.used.push(next.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unauthorized_request_is_re_sent_once() {
        let mut attempts = CredentialAttempts::new("a");
        assert!(attempts.may_retry(Rejection::Unauthorized));
        assert!(attempts.admit(Rejection::Unauthorized, "a2"));
        assert!(!attempts.may_retry(Rejection::Unauthorized));
    }

    #[test]
    fn a_rate_limited_request_moves_to_unused_credentials_only() {
        let mut attempts = CredentialAttempts::new("a");
        assert!(attempts.admit(Rejection::RateLimited, "b"));
        assert!(!attempts.admit(Rejection::RateLimited, "a"));
        for index in 0..MAX_CREDENTIAL_ATTEMPTS {
            attempts.admit(Rejection::RateLimited, &format!("c{index}"));
        }
        assert!(!attempts.may_retry(Rejection::RateLimited));
    }
}
