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
    fn the_debug_form_never_shows_the_credential() {
        let credential = SourcedCredential {
            api_key: "secret-access".to_string(),
            headers: BTreeMap::from([("x-stub".to_string(), "secret-header".to_string())]),
        };

        assert_eq!(
            format!("{credential:?}"),
            r#"SourcedCredential { api_key: "<redacted>", headers: ["x-stub"] }"#
        );
    }
}
