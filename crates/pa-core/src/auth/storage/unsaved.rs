//! Refreshed OAuth credentials the auth document could not take (a full
//! disk, EACCES, a lock or rename failure), kept until a retried save
//! lands them. The provider rotated the refresh token when it issued them,
//! so the stored login is already dead: dropping one logs the user out.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::platform::HeartbeatLock;

/// The first retry runs at the next lookup; each failed one doubles the
/// wait from this, up to [`MAX_RETRY_DELAY`].
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(1);

const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// One provider's kept refresh.
pub(super) struct Kept {
    /// An auth document holding only this provider's refreshed credential.
    content: String,
    /// The provider's refresh claim, held while no other process can find
    /// the credential (its recovery file could not be written either), so
    /// none of them spends the dead refresh token.
    pub(super) claim: Option<HeartbeatLock>,
    next_attempt: Instant,
    failed_attempts: u32,
}

/// One store's kept refreshes in this process, by provider.
#[derive(Default)]
pub(super) struct KeptRefreshes(HashMap<String, Kept>);

impl KeptRefreshes {
    /// Keep `content` for `provider`, due a save at the next lookup. A claim
    /// already held for the provider stays held unless `claim` replaces it;
    /// the replaced one is returned, to drop outside the registry lock.
    pub(super) fn keep(
        &mut self,
        provider: &str,
        content: String,
        claim: Option<HeartbeatLock>,
    ) -> Option<HeartbeatLock> {
        let previous = self.0.remove(provider).and_then(|kept| kept.claim);
        let (claim, replaced) = match claim {
            Some(claim) => (Some(claim), previous),
            None => (previous, None),
        };
        self.0.insert(
            provider.to_string(),
            Kept {
                content,
                claim,
                next_attempt: Instant::now(),
                failed_attempts: 0,
            },
        );
        replaced
    }

    /// A copy another process left (its recovery file): kept here too when
    /// this process holds none, so this process also retries the save.
    pub(super) fn adopt(&mut self, provider: &str, content: &str) {
        if !self.0.contains_key(provider) {
            self.keep(provider, content.to_string(), None);
        }
    }

    pub(super) fn content(&self, provider: &str) -> Option<String> {
        self.0.get(provider).map(|kept| kept.content.clone())
    }

    pub(super) fn holds_claim(&self, provider: &str) -> bool {
        self.0
            .get(provider)
            .is_some_and(|kept| kept.claim.is_some())
    }

    /// Take the provider's held claim (other processes can now find the
    /// credential without it).
    pub(super) fn release_claim(&mut self, provider: &str) -> Option<HeartbeatLock> {
        self.0.get_mut(provider)?.claim.take()
    }

    /// The providers whose save is due now. Each is rescheduled as if this
    /// attempt fails; a save that lands forgets the entry instead.
    pub(super) fn due(&mut self) -> Vec<String> {
        let now = Instant::now();
        self.0
            .iter_mut()
            .filter(|(_, kept)| kept.next_attempt <= now)
            .map(|(provider, kept)| {
                let doublings = kept.failed_attempts.min(6);
                kept.next_attempt =
                    now + (FIRST_RETRY_DELAY * (1 << doublings)).min(MAX_RETRY_DELAY);
                kept.failed_attempts = kept.failed_attempts.saturating_add(1);
                provider.clone()
            })
            .collect()
    }

    /// Remove the provider's entry when it still holds `content`.
    pub(super) fn forget(&mut self, provider: &str, content: &str) -> Option<Kept> {
        if self.0.get(provider)?.content != content {
            return None;
        }
        self.0.remove(provider)
    }
}
