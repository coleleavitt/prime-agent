//! What the user hears about the store's logins beyond a request's own
//! answer, through pa-core's auth notices (one per condition per session):
//!
//! - a login whose refresh token Anthropic revoked (`invalid_grant`). The
//!   store records the verdict on the row (bound to that refresh token) and
//!   the routing passes over the row from then on, so the revoked token is
//!   presented once. This process reports the revocations it observed
//!   itself (the SDK remembers the tokens it presented and saw refused): the
//!   one process that met the revocation logs it, once, and raises a notice
//!   naming the login that serves instead. A new login on the row (a
//!   re-login from any tool sharing the store), or the row's removal, ends
//!   it.
//! - a login whose refresh the store file could not take (the SDK keeps the
//!   rotation beside the store until a write saves it). It ends once saved.

use anthropic::{Account, AccountStore, DeadRefreshTokens};
use pa_core::auth::{AuthNotice, clear_auth_notice, raise_auth_notice};
use pa_types::sync::MutexExt;

use crate::PROVIDER_ID;
use crate::source::SharedStoreSource;

/// The conditions this process raised and that still stand.
#[derive(Debug, Default)]
pub(crate) struct Revocations {
    /// Revoked logins, oldest first.
    revoked: Vec<Reported>,
    /// Rows whose rotation is not in the store file yet.
    unsaved: Vec<String>,
}

/// One reported revocation.
#[derive(Debug)]
struct Reported {
    /// The store row.
    account_id: String,
    /// The fingerprint of the row's revoked refresh token.
    fingerprint: String,
}

/// How a row is named in the log and in a notice's condition: its id,
/// unless the id is an email (then a fingerprint of it), as the pi
/// plugin's spans name accounts.
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

/// The notice condition of `account_id`'s revocation.
fn revoked_condition(account_id: &str) -> String {
    format!("revoked:{}", log_name(account_id))
}

/// The notice condition of `account_id`'s unsaved rotation.
fn unsaved_condition(account_id: &str) -> String {
    format!("unsaved:{}", log_name(account_id))
}

impl SharedStoreSource {
    /// Report the revocations this process observed in `store` that it has
    /// not reported yet, end the ones a new login replaced, and raise or end
    /// the store's unsaved rotations. `serving` is the row that serves now,
    /// if any.
    pub(crate) fn note_revocations(&self, store: &AccountStore, serving: Option<&str>) {
        let mut conditions = self.revocations.lock_or_recover();
        conditions.revoked.retain(|reported| {
            let stands = store.get(&reported.account_id).is_some_and(|account| {
                account.refresh_token_is_dead()
                    && account.credential_fingerprint().as_ref() == Some(&reported.fingerprint)
            });
            if !stands {
                clear_auth_notice(PROVIDER_ID, &revoked_condition(&reported.account_id));
            }
            stands
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
            if conditions.revoked.iter().any(|reported| {
                reported.account_id == account.id && reported.fingerprint == fingerprint
            }) {
                continue;
            }
            tracing::warn!(
                login = %log_name(&account.id),
                serving = %serving.map_or_else(|| "none".to_string(), |serving| log_name(&serving.id)),
                "an Anthropic login in the shared account store was revoked (its refresh token was refused with invalid_grant); requests skip it until it is logged in again"
            );
            let message = match serving {
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
            // A new revocation of the row replaces an earlier one.
            clear_auth_notice(PROVIDER_ID, &revoked_condition(&account.id));
            raise_auth_notice(AuthNotice {
                provider: PROVIDER_ID.to_string(),
                condition: revoked_condition(&account.id),
                message,
            });
            conditions
                .revoked
                .retain(|reported| reported.account_id != account.id);
            conditions.revoked.push(Reported {
                account_id: account.id.clone(),
                fingerprint,
            });
        }
        let unsaved = anthropic::unsaved::unsaved_accounts(&self.config.store_path);
        for ended in conditions.unsaved.iter().filter(|id| !unsaved.contains(id)) {
            clear_auth_notice(PROVIDER_ID, &unsaved_condition(ended));
        }
        for id in &unsaved {
            let name = store.get(id).map_or(id.as_str(), user_name);
            raise_auth_notice(AuthNotice {
                provider: PROVIDER_ID.to_string(),
                condition: unsaved_condition(id),
                message: format!(
                    "Your Anthropic login {name} was refreshed but could not be saved to the shared account store; it is kept beside the store and saved by its next write."
                ),
            });
        }
        conditions.unsaved = unsaved;
    }
}

#[cfg(test)]
mod tests;
