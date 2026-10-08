//! Refreshed OAuth credentials `auth.json` could not take (the backend
//! keeps them, see [`AuthStorageBackend::keep_unsaved_refresh`]): served
//! over the stored login they replace, saved again under a bounded backoff,
//! and reported once, without a token value.

use super::{parse_storage_data, refresh_flight, AuthCredential, AuthStorage, AuthStorageData};
use crate::auth::storage::UnsavedRefreshKept;
use crate::platform::HeartbeatLock;

/// A kept copy's credential with its expiry, when it is `provider`'s OAuth login.
fn kept_login(content: &str, provider: &str) -> Option<(i64, AuthCredential)> {
    let credential = parse_storage_data(Some(content))
        .ok()?
        .credential(provider)?;
    match credential {
        AuthCredential::Oauth { expires, .. } => Some((expires, credential)),
        AuthCredential::ApiKey { .. } | AuthCredential::McpStaticToken { .. } => None,
    }
}

impl AuthStorage {
    /// Serve the newest kept refresh of each stored OAuth login in its
    /// place, and forget kept copies the document already supersedes (a
    /// peer saved a newer login, or this one was saved).
    pub(super) fn overlay_unsaved_refreshes(&self, data: &mut AuthStorageData) {
        for provider in data.keys() {
            let Some(AuthCredential::Oauth {
                expires: stored, ..
            }) = data.credential(&provider)
            else {
                continue;
            };
            let mut newest: Option<(i64, AuthCredential)> = None;
            for content in self.storage.unsaved_refreshes(&provider) {
                match kept_login(&content, &provider) {
                    Some((expires, credential)) if expires > stored => {
                        if newest.as_ref().is_none_or(|(newest, _)| expires > *newest) {
                            newest = Some((expires, credential));
                        }
                    }
                    Some(_) | None => self.storage.forget_unsaved_refresh(&provider, &content),
                }
            }
            if let Some((_, credential)) = newest {
                data.insert(&provider, &credential);
            }
        }
    }

    /// Writing `credential`, just fetched for `provider`, failed with
    /// `error`: keep it through the backend (handing over the refresh claim)
    /// and report it once.
    pub(super) fn keep_unsaved_refresh(
        &self,
        provider: &str,
        credential: &AuthCredential,
        error: &anyhow::Error,
        claim: Option<HeartbeatLock>,
    ) {
        let mut document = AuthStorageData::default();
        document.insert(provider, credential);
        let kept = match serde_json::to_string_pretty(&document.0) {
            Ok(content) => self.storage.keep_unsaved_refresh(provider, content, claim),
            Err(_) => UnsavedRefreshKept::NotKept,
        };
        match kept {
            UnsavedRefreshKept::Recoverable(path) => tracing::warn!(
                provider,
                error = %error,
                recovery_file = %path.display(),
                "the refreshed OAuth login could not be saved to auth.json; it is kept in memory \
                 and in an owner-only recovery file, and saving is retried"
            ),
            UnsavedRefreshKept::InProcess { reason } => tracing::warn!(
                provider,
                error = %error,
                recovery_error = %reason,
                "the refreshed OAuth login could not be saved to auth.json; it is kept in this \
                 process only, other processes wait for it instead of refreshing, and saving is \
                 retried"
            ),
            UnsavedRefreshKept::NotKept => tracing::warn!(
                provider,
                error = %error,
                "the refreshed OAuth login could not be saved to auth.json; it serves this \
                 request only, and the next refresh may need a new login"
            ),
        }
    }

    /// Retry the save of each kept refresh whose backoff is due: the newest
    /// copy lands in `auth.json` unless the document holds a newer login (or
    /// none any more), and every copy is then forgotten.
    pub(super) fn save_unsaved_refreshes(&mut self) {
        let mut saved = false;
        for provider in self.storage.unsaved_refreshes_due() {
            // A refresh of this provider in flight here spends the kept
            // refresh token and saves its own result; wait for it.
            let _flight = refresh_flight(&provider);
            let copies = self.storage.unsaved_refreshes(&provider);
            let Some((expires, credential)) = copies
                .iter()
                .filter_map(|content| kept_login(content, &provider))
                .max_by_key(|(expires, _)| *expires)
            else {
                for content in &copies {
                    self.storage.forget_unsaved_refresh(&provider, content);
                }
                continue;
            };
            let mut wrote = false;
            let result = self.storage.with_lock(&mut |current| {
                let mut data = parse_storage_data(current.as_deref())?;
                match data.credential(&provider) {
                    Some(AuthCredential::Oauth {
                        expires: stored, ..
                    }) if stored < expires => {
                        data.insert(&provider, &credential);
                        wrote = true;
                        Ok(((), Some(serde_json::to_string_pretty(&data.0)?)))
                    }
                    // A newer login, a logout, or a key in its place: the
                    // kept copy is superseded.
                    Some(_) | None => Ok(((), None)),
                }
            });
            match result {
                Ok(()) => {
                    if wrote {
                        tracing::info!(
                            provider,
                            "the refreshed OAuth login was saved to auth.json"
                        );
                    }
                    for content in &copies {
                        self.storage.forget_unsaved_refresh(&provider, content);
                    }
                    saved = true;
                }
                Err(error) => tracing::debug!(
                    provider,
                    error = %error,
                    "the refreshed OAuth login still could not be saved to auth.json"
                ),
            }
        }
        if saved {
            self.reload();
        }
    }

    /// A login or logout replaced `provider`'s entry: its kept refreshes
    /// must never resurrect the old login.
    pub(super) fn forget_unsaved_refreshes(&self, provider: &str) {
        for content in self.storage.unsaved_refreshes(provider) {
            self.storage.forget_unsaved_refresh(provider, &content);
        }
    }
}
