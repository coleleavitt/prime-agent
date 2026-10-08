//! A login the store holds whose refresh token Anthropic revoked
//! (`invalid_grant`): reported once, not on every request.
//!
//! The store records the verdict on the row (bound to that refresh token),
//! and the routing passes over the row from then on, so the revoked token
//! is presented once. This process reports the revocations it observed
//! itself (the SDK remembers the tokens it presented and saw refused), so
//! the one process that met the revocation logs it, once, and its sessions
//! show one notice naming the login that serves instead. A new login on the
//! row (a re-login from any tool sharing the store), or the row's removal,
//! withdraws it.

use anthropic::{Account, AccountStore, DeadRefreshTokens};
use pa_types::sync::MutexExt;

use crate::source::SharedStoreSource;

/// The revocations this process reported and that still stand, oldest
/// first.
#[derive(Debug, Default)]
pub(crate) struct Revocations(Vec<Reported>);

/// One reported revocation.
#[derive(Debug)]
struct Reported {
    /// The store row.
    account_id: String,
    /// The fingerprint of the row's revoked refresh token.
    fingerprint: String,
    /// What the sessions are told.
    notice: String,
}

/// How a row is named in the log: its id, unless the id is an email (then
/// a fingerprint of it), as the pi plugin's spans name accounts.
fn log_name(id: &str) -> String {
    if id.contains('@') {
        anthropic::token_fingerprint(id)[..8].to_string()
    } else {
        id.to_string()
    }
}

/// How a row is named to the user: its label, else its id (as
/// `/claude-quota` names it).
fn user_name(account: &Account) -> &str {
    account.label.as_deref().unwrap_or(&account.id)
}

impl SharedStoreSource {
    /// Report the revocations this process observed in `store` that it has
    /// not reported yet, and withdraw the ones a new login replaced.
    /// `serving` is the row that serves now, if any.
    pub(crate) fn note_revocations(&self, store: &AccountStore, serving: Option<&str>) {
        let mut revocations = self.revocations.lock_or_recover();
        revocations.0.retain(|reported| {
            store.get(&reported.account_id).is_some_and(|account| {
                account.refresh_token_is_dead()
                    && account.credential_fingerprint().as_ref() == Some(&reported.fingerprint)
            })
        });
        let serving = serving.and_then(|id| store.get(id));
        for account in &store.accounts {
            let Some(tokens) = account.oauth() else {
                continue;
            };
            let refresh = tokens.refresh.expose();
            if !account.refresh_token_is_dead() || !DeadRefreshTokens::is_dead(refresh) {
                continue;
            }
            let fingerprint = anthropic::token_fingerprint(refresh);
            if revocations.0.iter().any(|reported| {
                reported.account_id == account.id && reported.fingerprint == fingerprint
            }) {
                continue;
            }
            tracing::warn!(
                login = %log_name(&account.id),
                serving = %serving.map_or_else(|| "none".to_string(), |serving| log_name(&serving.id)),
                "an Anthropic login in the shared account store was revoked (its refresh token was refused with invalid_grant); requests skip it until it is logged in again"
            );
            let notice = match serving {
                Some(serving) => format!(
                    "Your Anthropic login {} was revoked; using {}. Run /login anthropic to restore it.",
                    user_name(account),
                    user_name(serving)
                ),
                None => format!(
                    "Your Anthropic login {} was revoked. Run /login anthropic to restore it.",
                    user_name(account)
                ),
            };
            revocations
                .0
                .retain(|reported| reported.account_id != account.id);
            revocations.0.push(Reported {
                account_id: account.id.clone(),
                fingerprint,
                notice,
            });
        }
    }

    /// The notice for this process's sessions while a revocation it
    /// reported stands.
    pub(crate) fn revoked_notice(&self) -> Option<String> {
        self.revocations
            .lock_or_recover()
            .0
            .last()
            .map(|reported| reported.notice.clone())
    }
}

#[cfg(test)]
mod tests;
