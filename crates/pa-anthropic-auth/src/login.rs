//! A fresh login into the shared store (the custody half of anthropic-napi's
//! `completeLogin`): the host's login flow obtains the tokens; the store
//! takes custody of them, names the row after the signed-in account, makes
//! it current, and (one login per account) hands the new login to Claude
//! Code when Claude Code is logged into the same account.

use std::path::PathBuf;

use anthropic::credentials::{ClaudeCodeIdentity, publish_native_login, read_claude_code_identity};
use anthropic::token::{AccessToken, Credential, OAuthTokens, RefreshToken, TokenAccount};
use anthropic::{Account, AccountStore};
use chrono::{TimeZone, Utc};

use crate::SharedStoreSource;

/// The tokens a host's login flow obtained.
#[derive(Clone, PartialEq, Eq)]
pub struct NewLogin {
    /// The access token.
    pub access: String,
    /// The refresh token (the store becomes its only custodian).
    pub refresh: String,
    /// When the access token expires, wall-clock epoch milliseconds.
    pub expires_ms: i64,
}

impl std::fmt::Debug for NewLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewLogin")
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// Where a stored login landed, without any account identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLogin {
    /// The store file now holding the login.
    pub store_path: PathBuf,
    /// Claude Code was logged into the same account: this login replaced
    /// its login (Anthropic keeps one per account) and was published to
    /// Claude Code (`written` / `unchanged`) or could not be (any other
    /// `NativePublishOutcome` code).
    pub claude_code: Option<&'static str>,
}

impl StoredLogin {
    /// The notice for a login that replaced Claude Code's, if any.
    #[must_use]
    pub fn claude_code_notice(&self) -> Option<String> {
        let outcome = self.claude_code?;
        let effect = match outcome {
            "written" | "unchanged" => {
                "Claude Code's credentials now hold this login, so Claude Code keeps working"
                    .to_string()
            }
            other => format!(
                "Claude Code's credentials could not be updated ({other}); log Claude Code in again"
            ),
        };
        Some(format!(
            "Claude Code is logged into this account. An account has one login at a time: this login replaces Claude Code's, which Anthropic revokes. {effect}."
        ))
    }
}

impl SharedStoreSource {
    /// The account behind an access token, from the profile endpoint (best
    /// effort: empty when the endpoint is refused, unreachable or silent).
    pub(crate) async fn fetch_identity(
        &self,
        access: &str,
    ) -> anthropic::profile::OAuthAccountIdentity {
        if anthropic::endpoints::check_oauth_url(
            &self.config.profile_url,
            self.config.require_loopback,
        )
        .is_err()
        {
            return anthropic::profile::OAuthAccountIdentity::default();
        }
        anthropic::profile::fetch_oauth_account_identity_from(
            &anthropic::oauth::default_oauth_http_client(),
            &self.config.profile_url,
            access,
        )
        .await
    }

    /// Take custody of a fresh login: identify the account (the profile
    /// endpoint, best effort), merge it into the row holding the same login
    /// (or a new row), make it current, and publish it to Claude Code when
    /// Claude Code is logged into the same account.
    ///
    /// # Errors
    ///
    /// The store could not be written (secret-free message).
    pub async fn store_login(&self, login: NewLogin) -> Result<StoredLogin, String> {
        let expires_at = Utc
            .timestamp_millis_opt(login.expires_ms)
            .single()
            .ok_or_else(|| "the login's expiry is out of range".to_string())?;
        let mut tokens = OAuthTokens {
            access: AccessToken::new(login.access),
            refresh: RefreshToken::new(login.refresh),
            expires_at,
            refresh_expires_at: None,
            scopes: Vec::new(),
            account: None,
            organization: None,
        };
        let identity = self.fetch_identity(tokens.access.expose()).await;
        if let Some(uuid) = identity.account_uuid.clone() {
            tokens.account = Some(TokenAccount {
                uuid,
                email_address: identity.email.clone(),
            });
            if let Some(organization) = identity.organization_uuid.clone() {
                tokens.organization =
                    Some(anthropic::token::TokenOrganization { uuid: organization });
            }
        }

        let path = self.config.store_path.clone();
        let files = self.client().claude_code_files();
        let stored_tokens = tokens.clone();
        AccountStore::mutate(&path, |store| {
            let id = store
                .login_account_id(
                    identity.email.as_deref(),
                    false,
                    identity.organization_uuid.as_deref(),
                    identity.organization_name.as_deref(),
                )
                .or_else(|| identity.account_uuid.clone())
                .unwrap_or_else(|| {
                    format!(
                        "account-{}",
                        anthropic::token_fingerprint(stored_tokens.refresh.expose())
                    )
                });
            let mut account = Account::new(id, Credential::Oauth(stored_tokens));
            account.email.clone_from(&identity.email);
            let id = store.merge_login(account)?;
            store.current = Some(id);
            Ok(())
        })
        .map_err(|error| format!("could not save the login: {error}"))?;
        // One login per account: a login of the account Claude Code is
        // logged into revokes Claude Code's, so it is handed the new one.
        let claude_code = files.and_then(|files| {
            let native = read_claude_code_identity(&files.config)?;
            ClaudeCodeIdentity::of_tokens(&tokens)
                .is_some_and(|login| login.same_account(&native))
                .then(|| publish_native_login(&files, None, &tokens).code())
        });
        Ok(StoredLogin {
            store_path: path,
            claude_code,
        })
    }
}
