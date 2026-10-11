//! Custody hand-offs between prime-agent and the store: a login `auth.json`
//! still holds, moved into the store (the custody half of anthropic-napi's
//! `importOAuthAccount`, as the pi plugin moves its host's refresh token),
//! so the two never both spend one rotating refresh token; and `/logout`,
//! which removes the login the store serves the provider (the plugins'
//! account removal, `removeAccount`: the row goes, nothing is revoked).

use std::sync::atomic::Ordering;

use anthropic::credentials::read_claude_code_login;
use anthropic::token::{
    AccessToken,
    Credential,
    OAuthTokens,
    RefreshToken,
    TokenAccount,
    TokenOrganization,
    is_valid_access_token,
    is_valid_refresh_token,
};
use anthropic::{Account, AccountStore, account_identities};
use chrono::{TimeZone, Utc};
use pa_core::auth::{CredentialSourceError, RemovedLogin, StoredLoginCustody, StoredOAuthLogin};
use pa_types::sync::MutexExt;

use crate::SharedStoreSource;
use crate::source::{UsageEvent, block_on_own_runtime, logins, served_login};

/// The refresh token the plugins write into a host's own auth file in place
/// of the real one once the store holds the login (anthropic-auth's
/// `STORE_MANAGED_REFRESH_PLACEHOLDER`: the pi plugin seeds a host's
/// `auth.json` with it, replaces a real refresh token it moved into the
/// store with it, and hands it to the host after every refresh; the
/// opencode plugin does the same in opencode's `auth.json`). Such a login
/// is already the store's.
const STORE_MANAGED_REFRESH: &str = "managed-by-anthropic-accounts";

/// What the store did with an imported login (napi `ImportResult.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportStatus {
    /// A row already holds this refresh token.
    AlreadyPresent,
    /// A row of the same account holds its own login, which wins.
    Kept,
    /// A new row holds it.
    Added,
}

impl ImportStatus {
    fn code(self) -> &'static str {
        match self {
            Self::AlreadyPresent => "already_present",
            Self::Kept => "kept",
            Self::Added => "added",
        }
    }
}

impl SharedStoreSource {
    /// Move `login` into the store. The store is the custodian: a row that
    /// already holds this token, or a login of the same account, wins and
    /// the import is discarded. A live login is first identified at the
    /// profile endpoint (an expired one is never refreshed to find out); a
    /// login that is Claude Code's own (Claude Code holds its refresh token)
    /// takes Claude Code's identity, so its row is linked to Claude Code.
    ///
    /// `Ok(None)`: the tokens are malformed (the store refuses them;
    /// `auth.json` keeps them).
    ///
    /// # Errors
    ///
    /// The store could not be read or written (secret-free message).
    pub(crate) async fn import_login(
        &self,
        login: &StoredOAuthLogin,
    ) -> Result<Option<ImportStatus>, String> {
        if !is_valid_access_token(&login.access) || !is_valid_refresh_token(&login.refresh) {
            return Ok(None);
        }
        let Some(expires_at) = Utc.timestamp_millis_opt(login.expires_ms).single() else {
            return Ok(None);
        };
        let mut tokens = OAuthTokens {
            access: AccessToken::new(login.access.clone()),
            refresh: RefreshToken::new(login.refresh.clone()),
            expires_at,
            refresh_expires_at: None,
            scopes: Vec::new(),
            account: None,
            organization: None,
        };
        if tokens.expires_at > Utc::now() {
            let identity = self.fetch_identity(tokens.access.expose()).await;
            if let Some(uuid) = identity.account_uuid {
                tokens.account = Some(TokenAccount {
                    uuid,
                    email_address: identity.email,
                });
                tokens.organization = identity
                    .organization_uuid
                    .map(|uuid| TokenOrganization { uuid });
            }
        }
        // Claude Code's own login, copied here by another host: it is
        // identified as Claude Code's account, so the row is linked to Claude
        // Code and follows its rotations, rather than an unlinked copy that
        // spends the refresh token Claude Code also holds (and spends).
        if tokens.account.is_none() {
            let claude_code = self
                .client()
                .claude_code_files()
                .and_then(|files| read_claude_code_login(&files, None))
                .filter(|login| login.tokens.refresh == tokens.refresh);
            if let Some(login) = claude_code {
                tokens.account = login.tokens.account;
                tokens.organization = login.tokens.organization;
            }
        }
        let email = tokens
            .account
            .as_ref()
            .and_then(|a| a.email_address.clone());
        AccountStore::mutate(&self.config.store_path, |store| {
            if store.find_by_refresh_token(&tokens.refresh).is_some() {
                return Ok(ImportStatus::AlreadyPresent);
            }
            let mut candidate = Account::new(
                preferred_id(store, &tokens),
                Credential::Oauth(tokens.clone()),
            );
            candidate.email.clone_from(&email);
            let incoming = account_identities(&candidate);
            if store
                .accounts
                .iter()
                .any(|a| account_identities(a).iter().any(|k| incoming.contains(k)))
            {
                return Ok(ImportStatus::Kept);
            }
            let id = candidate.id.clone();
            store.accounts.push(candidate);
            if store.current.is_none() {
                store.current = Some(id);
            }
            Ok(ImportStatus::Added)
        })
        .map(Some)
        .map_err(|error| format!("could not import the login into the store: {error}"))
    }

    /// [`pa_core::auth::ProviderCredentialSource::adopt_stored_login`]: the
    /// login is the store's once it holds it (or a login of the same
    /// account), or when a plugin already moved it there (its refresh token
    /// is [`STORE_MANAGED_REFRESH`]: nothing to import, and `auth.json`'s
    /// access token is a copy the store may since have rotated); a
    /// malformed login (reported once per source) or an unusable store
    /// leaves it in `auth.json`.
    pub(crate) fn adopt(&self, login: &StoredOAuthLogin) -> StoredLoginCustody {
        if login.refresh.trim() == STORE_MANAGED_REFRESH {
            tracing::debug!("auth.json's Anthropic login is already the shared account store's");
            return StoredLoginCustody::Adopted;
        }
        match block_on_own_runtime(self.import_login(login)) {
            Ok(Ok(Some(status))) => {
                self.count(UsageEvent::Migrated);
                tracing::info!(
                    status = status.code(),
                    "moved auth.json's Anthropic login into the shared account store"
                );
                StoredLoginCustody::Adopted
            }
            Ok(Ok(None)) => {
                // Offered again on every lookup while auth.json holds it.
                if !self.malformed_login_reported.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "auth.json's Anthropic login is malformed; the shared account store did not take it"
                    );
                }
                StoredLoginCustody::Kept
            }
            Ok(Err(message)) | Err(message) => {
                tracing::warn!(error = %message, "auth.json's Anthropic login stays in auth.json");
                StoredLoginCustody::Kept
            }
        }
    }
}

impl SharedStoreSource {
    /// [`pa_core::auth::ProviderCredentialSource::remove_login`]: remove the
    /// row the provider is served from now (the routing order's first
    /// candidate), under the store lock. Nothing is revoked at Anthropic,
    /// and Claude Code's own login is left alone.
    pub(crate) fn remove_served_login(&self) -> Result<RemovedLogin, CredentialSourceError> {
        let _flight = self.flight.lock_or_recover();
        let path = self.config.store_path.as_path();
        if std::fs::symlink_metadata(path).is_err() {
            return Err(CredentialSourceError::NotConfigured);
        }
        let now = Utc::now();
        let remaining = AccountStore::mutate_allow_empty(path, |store| {
            let Some(id) = served_login(store, now).map(|account| account.id.clone()) else {
                return Ok(None);
            };
            store.remove(&id);
            Ok(Some(logins(store).count()))
        })
        .map_err(|error| {
            CredentialSourceError::Unavailable(format!(
                "could not remove the login from the shared account store: {error}"
            ))
        })?;
        let Some(remaining) = remaining else {
            return Err(CredentialSourceError::NotConfigured);
        };
        let removed = format!(
            "Removed the login from the shared account store ({}); other tools that share the store no longer see it.",
            path.display()
        );
        Ok(RemovedLogin {
            notice: Some(match remaining {
                0 => removed,
                1 => format!("{removed} 1 more login there still serves this provider."),
                more => format!("{removed} {more} more logins there still serve this provider."),
            }),
        })
    }
}

#[cfg(test)]
mod tests;

/// The row id an imported login takes (napi `preferred_id`): the account's
/// email (qualified by organization when another organization holds it),
/// else its uuid, else `account-<8 hex>`.
fn preferred_id(store: &AccountStore, tokens: &OAuthTokens) -> String {
    let email = tokens
        .account
        .as_ref()
        .and_then(|a| a.email_address.as_deref());
    store
        .login_account_id(
            email,
            false,
            tokens.organization.as_ref().map(|o| o.uuid.as_str()),
            None,
        )
        .or_else(|| tokens.account.as_ref().map(|a| a.uuid.clone()))
        .unwrap_or_else(|| {
            let id = uuid::Uuid::new_v4().simple().to_string();
            format!("account-{}", &id[..8])
        })
}
