//! The 1M-context credits latch (anthropic-auth `packages/pi` `stream.ts`,
//! after Claude Code 2.1.260's account-local `longContext1mCreditsBlocked`):
//! a 1M-capable model's request carries the `context-1m` beta until
//! Anthropic answers one with HTTP 429 saying usage credits are required for
//! long context; from then on that access token's requests take the
//! standard 200k window (no `context-1m` beta).
//!
//! Like the plugin's, the latch is keyed by the token's fingerprint, holds
//! for the life of the process, and is never written anywhere: a rotated
//! token (or a new process) starts unlatched. The 429 that set it is
//! reported as it is; the next request or retry uses the latch.

use std::collections::HashSet;
use std::sync::Mutex;

use anthropic::models::model_supports_context_1m;
use anthropic::retry::is_long_context_credits_required_error;
use pa_types::sync::MutexExt;
use sha2::{Digest, Sha256};

/// `tokenFingerprint`: the first 16 hex digits of the token's SHA-256.
pub(crate) fn token_fingerprint(token: &str) -> String {
    let mut hex = format!("{:x}", Sha256::digest(token.as_bytes()));
    hex.truncate(16);
    hex
}

/// The fingerprints of the tokens latched to the standard window.
#[derive(Debug, Default)]
pub(crate) struct Context1mLatch {
    clamped: Mutex<HashSet<String>>,
}

impl Context1mLatch {
    /// Whether `token`'s requests take the standard window.
    pub(crate) fn is_clamped(&self, token: &str) -> bool {
        self.clamped
            .lock_or_recover()
            .contains(&token_fingerprint(token))
    }

    /// A response to `model`'s request sent with `token`: latch the token
    /// when it is the credits 429 (only for a 1M-capable model). Whether it
    /// latched now.
    pub(crate) fn observe(&self, model: &str, token: &str, status: u16, body: &str) -> bool {
        if status != 429
            || !model_supports_context_1m(model)
            || !is_long_context_credits_required_error(status, body)
        {
            return false;
        }
        let latched = self
            .clamped
            .lock_or_recover()
            .insert(token_fingerprint(token));
        if latched {
            tracing::warn!(
                model,
                "Anthropic requires usage credits for this 1M-context route; later requests will use 200k"
            );
        }
        latched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDITS: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Extra usage is required for long context requests."}}"#;

    #[test]
    fn a_credits_429_latches_the_token_for_1m_models_only() {
        let latch = Context1mLatch::default();
        assert!(!latch.observe("claude-haiku-4-5", "a", 429, CREDITS));
        assert!(!latch.observe(
            "claude-opus-4-8",
            "a",
            429,
            r#"{"error":{"message":"Rate limited"}}"#
        ));
        assert!(!latch.observe("claude-opus-4-8", "a", 400, CREDITS));
        assert!(!latch.is_clamped("a"));
        assert!(latch.observe("claude-opus-4-8", "a", 429, CREDITS));
        assert!(!latch.observe("claude-opus-4-8", "a", 429, CREDITS));
        assert_eq!(
            (latch.is_clamped("a"), latch.is_clamped("b")),
            (true, false)
        );
    }

    #[test]
    fn the_fingerprint_is_the_plugin_s() {
        // node: createHash('sha256').update('sk-ant-oat01-x').digest('hex').slice(0, 16)
        assert_eq!(token_fingerprint("sk-ant-oat01-x"), "3a8178ac731908db");
    }
}
