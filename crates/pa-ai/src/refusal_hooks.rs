//! Refusal system integration with provider request hooks.
//!
//! When enabled via settings, intercepts outbound API requests to predict
//! refusal probability and filter/rewrite content before sending.

use std::sync::Arc;

use pa_refusal::RefusalGuard;

use crate::request_hooks::{OutgoingRequest, ProviderRequestHooks, install_request_hooks};

/// Request hooks that guard outbound requests against predicted refusals.
pub struct RefusalHooks {
    guard: RefusalGuard,
}

impl RefusalHooks {
    /// Create new refusal hooks with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            guard: RefusalGuard::new(),
        }
    }

    /// Install the refusal hooks for the given provider.
    pub fn install(provider_id: &str) {
        install_request_hooks(provider_id, "content-filter", Arc::new(Self::new()));
    }

    /// Install hooks for all supported providers.
    pub fn install_all() {
        Self::install("anthropic");
    }
}

impl Default for RefusalHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRequestHooks for RefusalHooks {
    fn prepare(&self, request: &mut OutgoingRequest<'_>) {
        // Score and filter the request body
        let result = self.guard.guard(request.payload);

        // Log at debug level
        if result.filtered {
            tracing::debug!(
                probability_before = result.probability_before,
                probability_after = result.probability_after,
                escalations = result.escalations,
                changes = ?result.changes,
                "Content filter applied rewrites"
            );
            // Apply the filtered body
            *request.payload = result.body;
        } else if result.danger {
            tracing::debug!(
                probability = result.probability_before,
                threshold = result.threshold,
                "Request flagged as potentially dangerous but not filtered"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_refusal_hooks_new() {
        let hooks = RefusalHooks::new();
        let _ = hooks;
    }
}
