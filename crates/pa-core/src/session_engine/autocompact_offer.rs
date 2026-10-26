//! Claude Code 2.1.310's autocompact offer: a *threshold* compaction asks the
//! user first, on the session's confirmation surface, instead of running
//! silently. Overflow (ceiling) compaction never asks. The offer reuses the
//! shared confirmation surface ([`crate::features::confirm`]), so the TUI
//! renders it as an ordinary yes/no panel with no new wire surface.
//!
//! The daemon's turn-boundary compaction ([`crate::session_engine::SessionEngine
//! ::offer_autocompaction`]) calls [`offer_autocompaction`] when a compaction is
//! already due; a declined offer is suppressed there until the context drops
//! below the threshold again, so the user is not re-asked every turn.

use std::path::Path;

use crate::features::{self, ConfirmationAnswer};
use crate::settings::SettingsManager;

/// The action the confirmation panel names.
const OFFER_ACTION: &str = "Compact the conversation";
/// Why the panel asks.
const OFFER_REASON: &str = "The context is approaching the limit. Compact it now to free space by summarizing older turns?";

/// Whether `experimental.autocompactOffer` is on for the session at `cwd`
/// (resolved on demand, the same way idle compaction reads its window).
#[must_use]
pub fn offer_enabled(cwd: &Path, agent_dir: &Path) -> bool {
    SettingsManager::create(cwd, agent_dir)
        .experimental()
        .autocompact_offer
}

/// Ask the user, on `session_id`'s confirmation surface, whether a threshold
/// compaction may run. Returns whether to compact: `false` only on an explicit
/// decline; no surface, a timeout, or an answer a reviewer made all return
/// `true`, so an unwatched, headless or reviewer-gated session still stays
/// healthy.
pub async fn ask_autocompaction(session_id: &str) -> bool {
    !matches!(
        features::confirm(
            session_id,
            OFFER_ACTION,
            OFFER_REASON,
            features::CONFIRMATION_TIMEOUT,
        )
        .await,
        ConfirmationAnswer::Denied
    )
}

/// The offer when it is enabled, else `true` (compact silently) without asking.
pub async fn offer_autocompaction(session_id: &str, cwd: &Path, agent_dir: &Path) -> bool {
    if !offer_enabled(cwd, agent_dir) {
        return true;
    }
    ask_autocompaction(session_id).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::features::{
        answer_confirmation,
        pending_confirmations,
        register_confirmation_surface,
    };

    /// Answer the one pending confirmation once it appears.
    async fn answer_when_pending(session_id: &str, allow: bool) {
        for _ in 0..200 {
            if let Some(pending) = pending_confirmations(session_id).first() {
                assert!(answer_confirmation(session_id, &pending.id, allow));
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no confirmation was asked");
    }

    #[tokio::test]
    async fn an_accepted_offer_compacts_and_a_declined_offer_does_not() {
        let session = "autocompact-offer-answer";
        let _surface = register_confirmation_surface(session);

        let accepted = {
            let id = session.to_string();
            let ask = tokio::spawn(async move { ask_autocompaction(&id).await });
            answer_when_pending(session, true).await;
            ask.await.unwrap()
        };
        assert!(accepted, "accepting the offer compacts");

        let declined = {
            let id = session.to_string();
            let ask = tokio::spawn(async move { ask_autocompaction(&id).await });
            answer_when_pending(session, false).await;
            ask.await.unwrap()
        };
        assert!(!declined, "declining the offer skips the compaction");
    }

    #[tokio::test]
    async fn without_a_confirmation_surface_the_offer_compacts() {
        // No surface registered: the session is headless or unwatched, so the
        // compaction runs rather than hanging on an answer that never comes.
        assert!(ask_autocompaction("autocompact-offer-no-surface").await);
    }
}
