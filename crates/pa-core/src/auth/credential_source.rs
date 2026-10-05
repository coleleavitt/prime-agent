//! The provider credential source seam: a credential store outside
//! `auth.json` that serves one provider id's requests (for example a
//! machine-wide account store shared with other tools).
//!
//! Nothing is installed in the native product: every lookup reads
//! `auth.json`, the environment and the fallback resolver exactly as it
//! always has. The composition root installs a source, once, before any
//! session or worker starts; native crates never name the implementation.
//!
//! An installed source sits after the runtime `--api-key` override and
//! before the stored `auth.json` credential. While it reports a login
//! ([`ProviderCredentialSource::status`]) it owns the provider: its
//! credential serves every request, and a failure to produce one is the
//! provider's OAuth authentication failure rather than a fall-through to
//! a credential the source may have superseded.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock, RwLock};

use pa_types::sync::RwLockExt;

/// A credential store that serves one provider id ahead of `auth.json`.
///
/// Implementations own their own coordination: a refresh runs under the
/// store's own lock (one refresh across every process sharing the store),
/// and nothing here caches the returned credential. Methods are called
/// from synchronous auth lookups, off the async runtime's critical path;
/// [`Self::status`] must stay cheap and offline because model listings
/// and status rows call it.
pub trait ProviderCredentialSource: Send + Sync {
    /// Whether the source holds a login for the provider, without network
    /// or refresh. `None` means nothing is configured and the lookup falls
    /// through to `auth.json`.
    fn status(&self) -> Option<CredentialSourceStatus>;

    /// A usable credential, refreshed on demand under the source's own
    /// lock. May block on disk and network.
    ///
    /// # Errors
    ///
    /// [`CredentialSourceError::NotConfigured`] when the login disappeared
    /// since [`Self::status`] (the lookup falls through), otherwise
    /// [`CredentialSourceError::Unavailable`]: the provider's
    /// authentication failure.
    fn credential(&self) -> Result<SourcedCredential, CredentialSourceError>;

    /// Offered the OAuth login `auth.json` holds for the provider, once per
    /// lookup while one is there: the source may take custody of it (a
    /// one-time migration into the source's store). On
    /// [`StoredLoginCustody::Adopted`] the lookup removes `auth.json`'s
    /// entry (only while it still holds this login), so the two never both
    /// refresh it; on [`StoredLoginCustody::Kept`] `auth.json` keeps it and
    /// serves it as before while the source reports no login.
    ///
    /// The default keeps every stored login. May block on disk and network.
    fn adopt_stored_login(&self, login: &StoredOAuthLogin) -> StoredLoginCustody {
        let _ = login;
        StoredLoginCustody::Kept
    }

    /// `/logout` for the provider: remove the login the source serves it
    /// now. May block on disk.
    ///
    /// # Errors
    ///
    /// [`CredentialSourceError::NotConfigured`] when the source holds no
    /// login (the default: a source that cannot remove logins), otherwise
    /// [`CredentialSourceError::Unavailable`] with a secret-free reason.
    fn remove_login(&self) -> Result<RemovedLogin, CredentialSourceError> {
        Err(CredentialSourceError::NotConfigured)
    }
}

/// A login a source removed on `/logout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedLogin {
    /// What the logout should tell the user beyond "logged out" (where the
    /// login was, what still serves the provider), without secrets.
    pub notice: Option<String>,
}

/// An OAuth login `auth.json` holds, offered to the provider's source.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredOAuthLogin {
    /// The access token.
    pub access: String,
    /// The refresh token.
    pub refresh: String,
    /// When the access token expires, epoch milliseconds.
    pub expires_ms: i64,
}

impl std::fmt::Debug for StoredOAuthLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredOAuthLogin")
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// What a source did with a stored login it was offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredLoginCustody {
    /// The source holds the login (or a login of the same account it keeps
    /// instead): `auth.json`'s copy is removed.
    Adopted,
    /// The source did not take it: `auth.json` keeps it.
    Kept,
}

/// What a source reports about its login, without secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSourceStatus {
    /// The status row's label (where the login lives).
    pub label: String,
    /// An opaque, non-secret identifier of the credential the source would
    /// serve now; it changes when the source rotates or replaces it, which
    /// clears a stale mark set against the previous one.
    pub revision: String,
}

/// A credential a source produced for one request.
#[derive(Clone, PartialEq, Eq)]
pub struct SourcedCredential {
    /// The bearer or key the provider request authenticates with.
    pub api_key: String,
    /// Request headers the source's credential needs, merged into the
    /// request's headers (after the provider's own).
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for SourcedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourcedCredential")
            .field("api_key", &"<redacted>")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Why a source produced no credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialSourceError {
    /// The source no longer holds a login: the lookup falls through to the
    /// next candidate.
    #[error("no login is configured")]
    NotConfigured,
    /// The login exists but no credential could be produced (refresh
    /// failed, refresh token revoked, store unreadable, network down). The
    /// message is secret-free.
    #[error("{0}")]
    Unavailable(String),
}

type Registry = RwLock<HashMap<String, Arc<dyn ProviderCredentialSource>>>;

fn registry() -> &'static Registry {
    static SOURCES: OnceLock<Registry> = OnceLock::new();
    SOURCES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Install `source` for `provider_id`, process-wide, replacing any earlier
/// one. Called by the composition root before any session or worker
/// starts.
pub fn install_credential_source(provider_id: &str, source: Arc<dyn ProviderCredentialSource>) {
    registry()
        .write_or_recover()
        .insert(provider_id.to_string(), source);
}

/// The source installed for `provider_id`, if any.
#[must_use]
pub fn credential_source(provider_id: &str) -> Option<Arc<dyn ProviderCredentialSource>> {
    registry().read_or_recover().get(provider_id).cloned()
}

/// The provider ids with an installed source, sorted.
#[must_use]
pub fn credential_source_providers() -> Vec<String> {
    let mut providers: Vec<String> = registry().read_or_recover().keys().cloned().collect();
    providers.sort();
    providers
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use pa_types::sync::MutexExt;

    use super::*;
    use crate::auth::{
        AuthApiKeyResult, AuthSource, AuthSourceToken, AuthStatus, AuthStorage, AuthStorageData,
        NoOAuth,
    };

    /// A scripted source: a status and a credential result, counting the
    /// credential calls.
    struct StubSource {
        status: Mutex<Option<CredentialSourceStatus>>,
        result: Mutex<Result<SourcedCredential, CredentialSourceError>>,
        calls: AtomicUsize,
        /// The answer to a stored login; `Adopted` also starts serving.
        custody: Mutex<StoredLoginCustody>,
        offered: Mutex<Vec<StoredOAuthLogin>>,
    }

    impl StubSource {
        fn serving(revision: &str, api_key: &str) -> Arc<Self> {
            Arc::new(Self {
                status: Mutex::new(Some(status(revision))),
                result: Mutex::new(Ok(SourcedCredential {
                    api_key: api_key.to_string(),
                    headers: BTreeMap::from([("x-stub".to_string(), "1".to_string())]),
                })),
                calls: AtomicUsize::new(0),
                custody: Mutex::new(StoredLoginCustody::Kept),
                offered: Mutex::new(Vec::new()),
            })
        }

        fn set(
            &self,
            status: Option<CredentialSourceStatus>,
            result: Result<SourcedCredential, CredentialSourceError>,
        ) {
            *self.status.lock_or_recover() = status;
            *self.result.lock_or_recover() = result;
        }
    }

    impl ProviderCredentialSource for StubSource {
        fn status(&self) -> Option<CredentialSourceStatus> {
            self.status.lock_or_recover().clone()
        }

        fn credential(&self) -> Result<SourcedCredential, CredentialSourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.lock_or_recover().clone()
        }

        fn adopt_stored_login(&self, login: &StoredOAuthLogin) -> StoredLoginCustody {
            self.offered.lock_or_recover().push(login.clone());
            let custody = *self.custody.lock_or_recover();
            if custody == StoredLoginCustody::Adopted {
                *self.status.lock_or_recover() = Some(status("adopted"));
            }
            custody
        }
    }

    /// A source that adopts every stored login after another process
    /// wrote a newer one into the same `auth.json`.
    struct RacedSource {
        auth_path: std::path::PathBuf,
        provider: &'static str,
    }

    impl ProviderCredentialSource for RacedSource {
        fn status(&self) -> Option<CredentialSourceStatus> {
            None
        }

        fn credential(&self) -> Result<SourcedCredential, CredentialSourceError> {
            Err(CredentialSourceError::NotConfigured)
        }

        fn adopt_stored_login(&self, _login: &StoredOAuthLogin) -> StoredLoginCustody {
            let newer = serde_json::json!({
                self.provider: {
                    "type": "oauth", "access": "newer-access", "refresh": "newer-refresh",
                    "expires": 4_102_444_800_000i64
                }
            });
            std::fs::write(&self.auth_path, newer.to_string()).expect("write the newer login");
            StoredLoginCustody::Adopted
        }
    }

    fn status(revision: &str) -> CredentialSourceStatus {
        CredentialSourceStatus {
            label: "stub store".to_string(),
            revision: revision.to_string(),
        }
    }

    /// `auth.json` holding a live OAuth login for `provider`; no ambient
    /// environment.
    fn storage_with_login(provider: &str) -> AuthStorage {
        let data = serde_json::json!({
            provider: {
                "type": "oauth", "access": "auth-json-access", "refresh": "r",
                "expires": 4_102_444_800_000i64
            }
        });
        AuthStorage::in_memory_without_env(
            &AuthStorageData(data.as_object().cloned().unwrap_or_default()),
            Arc::new(NoOAuth),
        )
    }

    fn served(provider: &str, revision: &str, api_key: &str) -> AuthApiKeyResult {
        AuthApiKeyResult {
            api_key: Some(api_key.to_string()),
            source_token: Some(AuthSourceToken {
                provider: provider.to_string(),
                source: AuthSource::CredentialSource,
                identity_fingerprint: crate::auth::manager::fingerprint(
                    AuthSource::CredentialSource,
                    &format!("identity:credential-source {provider}"),
                ),
                value_fingerprint: crate::auth::manager::fingerprint(
                    AuthSource::CredentialSource,
                    &format!("value:credential-source {revision}"),
                ),
            }),
            credential_type: Some("oauth"),
            oauth_refresh_failed: false,
            headers: Some(BTreeMap::from([("x-stub".to_string(), "1".to_string())])),
        }
    }

    #[test]
    fn an_installed_source_serves_ahead_of_auth_json() {
        let provider = "stub-source-ahead";
        let source = StubSource::serving("rev-1", "source-access");
        install_credential_source(provider, source.clone());
        let mut auth = storage_with_login(provider);

        assert_eq!(
            auth.get_api_key_with_source_token(provider, false),
            served(provider, "rev-1", "source-access")
        );
        assert_eq!(
            auth.get_auth_status(provider),
            AuthStatus {
                configured: true,
                source: Some(AuthSource::CredentialSource),
                label: Some("stub store".to_string()),
            }
        );
        // Status rows never ask the source for a credential.
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_source_without_a_login_leaves_auth_json_in_charge() {
        let provider = "stub-source-empty";
        let source = StubSource::serving("rev-1", "source-access");
        source.set(None, Err(CredentialSourceError::NotConfigured));
        install_credential_source(provider, source.clone());
        let mut auth = storage_with_login(provider);

        assert_eq!(
            auth.get_api_key(provider),
            Some("auth-json-access".to_string())
        );
        assert_eq!(
            auth.get_auth_status(provider).source,
            Some(AuthSource::Stored)
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 0);

        // A login that vanished between status and credential falls through.
        source.set(
            Some(status("rev-1")),
            Err(CredentialSourceError::NotConfigured),
        );
        assert_eq!(
            auth.get_api_key(provider),
            Some("auth-json-access".to_string())
        );
    }

    #[test]
    fn a_failing_source_is_the_oauth_authentication_failure() {
        let provider = "stub-source-failing";
        let source = StubSource::serving("rev-1", "source-access");
        source.set(
            Some(status("rev-1")),
            Err(CredentialSourceError::Unavailable(
                "invalid_grant: the refresh token was revoked".to_string(),
            )),
        );
        install_credential_source(provider, source);
        // The auth.json login is never served in the source's place.
        let mut auth = storage_with_login(provider);

        assert_eq!(
            auth.get_api_key_with_source_token(provider, false),
            AuthApiKeyResult {
                credential_type: Some("oauth"),
                oauth_refresh_failed: true,
                ..AuthApiKeyResult::default()
            }
        );
    }

    #[test]
    fn a_stale_mark_holds_until_the_source_rotates() {
        let provider = "stub-source-stale";
        let source = StubSource::serving("rev-1", "source-access");
        install_credential_source(provider, source.clone());
        let mut auth = storage_with_login(provider);

        // The server rejected the served credential: the lookup moves on.
        assert!(auth.mark_auth_stale(provider));
        assert_eq!(
            auth.get_api_key(provider),
            Some("auth-json-access".to_string())
        );

        // The source rotated: the new revision serves again.
        source.set(
            Some(status("rev-2")),
            Ok(SourcedCredential {
                api_key: "rotated-access".to_string(),
                headers: BTreeMap::from([("x-stub".to_string(), "1".to_string())]),
            }),
        );
        assert_eq!(
            auth.get_api_key_with_source_token(provider, false),
            served(provider, "rev-2", "rotated-access")
        );
    }

    #[test]
    fn the_runtime_override_still_wins() {
        let provider = "stub-source-runtime";
        install_credential_source(provider, StubSource::serving("rev-1", "source-access"));
        let mut auth = storage_with_login(provider);
        auth.set_runtime_api_key(provider, "runtime-key".to_string());

        assert_eq!(auth.get_api_key(provider), Some("runtime-key".to_string()));
    }

    #[test]
    fn a_source_lists_its_provider_and_by_default_removes_no_login() {
        let provider = "stub-source-listed";
        let source = StubSource::serving("rev-1", "source-access");
        install_credential_source(provider, source.clone());

        assert!(credential_source_providers().contains(&provider.to_string()));
        assert_eq!(
            source.remove_login(),
            Err(CredentialSourceError::NotConfigured)
        );
    }

    fn stored_login() -> StoredOAuthLogin {
        StoredOAuthLogin {
            access: "auth-json-access".to_string(),
            refresh: "r".to_string(),
            expires_ms: 4_102_444_800_000,
        }
    }

    #[test]
    fn a_source_that_adopts_the_stored_login_takes_it_out_of_auth_json() {
        let provider = "stub-source-adopts";
        let source = StubSource::serving("rev-1", "source-access");
        let serving = source.result.lock_or_recover().clone();
        source.set(None, serving);
        *source.custody.lock_or_recover() = StoredLoginCustody::Adopted;
        install_credential_source(provider, source.clone());
        let mut auth = storage_with_login(provider);

        assert_eq!(
            auth.get_api_key_with_source_token(provider, false),
            served(provider, "adopted", "source-access")
        );
        assert_eq!(*source.offered.lock_or_recover(), vec![stored_login()]);
        assert_eq!(auth.get_all().get(provider), None);

        // Nothing is left to offer.
        auth.get_api_key(provider);
        assert_eq!(source.offered.lock_or_recover().len(), 1);
    }

    #[test]
    fn a_source_that_keeps_the_stored_login_leaves_auth_json_serving() {
        let provider = "stub-source-keeps";
        let source = StubSource::serving("rev-1", "source-access");
        source.set(None, Err(CredentialSourceError::NotConfigured));
        install_credential_source(provider, source.clone());
        let mut auth = storage_with_login(provider);

        assert_eq!(
            auth.get_api_key(provider),
            Some("auth-json-access".to_string())
        );
        assert_eq!(*source.offered.lock_or_recover(), vec![stored_login()]);
        assert!(auth.get_all().get(provider).is_some());
    }

    #[test]
    fn a_login_written_during_the_adoption_stays_in_auth_json() {
        let provider = "stub-source-raced";
        let dir = tempfile::tempdir().expect("a temp agent dir");
        let auth_path = dir.path().join("auth.json");
        let stored = serde_json::json!({
            provider: {
                "type": "oauth", "access": "auth-json-access", "refresh": "r",
                "expires": 4_102_444_800_000i64
            }
        });
        std::fs::write(&auth_path, stored.to_string()).expect("seed auth.json");
        install_credential_source(
            provider,
            Arc::new(RacedSource {
                auth_path: auth_path.clone(),
                provider,
            }),
        );
        let mut auth = AuthStorage::from_storage(
            Arc::new(crate::auth::FileAuthStorageBackend::new(&auth_path)),
            Arc::new(NoOAuth),
        );

        auth.get_api_key(provider);

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&auth_path).expect("auth.json"))
                .expect("auth.json parses");
        assert_eq!(on_disk[provider]["refresh"], "newer-refresh");
    }

    #[test]
    fn the_debug_form_never_shows_the_credential() {
        let credential = SourcedCredential {
            api_key: "secret-access".to_string(),
            headers: BTreeMap::from([("x-stub".to_string(), "secret-header".to_string())]),
        };

        assert_eq!(
            format!("{credential:?}"),
            r#"SourcedCredential { api_key: "<redacted>", headers: ["x-stub"] }"#
        );
        assert_eq!(
            format!("{:?}", stored_login()),
            "StoredOAuthLogin { expires_ms: 4102444800000, .. }"
        );
    }
}
