//! The shared account store as the `anthropic` provider's credential
//! source (`pa_core::auth::ProviderCredentialSource`).
//!
//! Every credential comes from `anthropic::access::get_access_token`, the
//! entry point the opencode and pi plugins reach through anthropic-napi:
//! account selection, the Claude Code login link, and a refresh claimed
//! through the store (file lock plus a lease every consumer honours, a
//! compare-and-swap that never overwrites a newer rotation). Requests in
//! this process also take one flight lock, so a second request waits for
//! the first one's refresh and reads its result instead of queueing on the
//! store's claim.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anthropic::access::{
    access_candidates, get_access_token, AccessErrorKind, AccessRequest, AccessSource,
};
use anthropic::credentials::NativePublish;
use anthropic::{AccountStore, Endpoints, OAuthClient, SharedRefreshOptions};
use pa_core::auth::{
    CredentialSourceError, CredentialSourceStatus, ProviderCredentialSource, SourcedCredential,
    StoredLoginCustody, StoredOAuthLogin,
};
use pa_types::sync::MutexExt;
use sha2::{Digest, Sha256};

/// The status rows' label for a login the shared store holds.
pub const STORE_LABEL: &str = "shared account store";

/// Where the source reads and refreshes.
#[derive(Debug, Clone)]
pub struct SharedStoreConfig {
    /// The store file (`~/.anthropic-accounts/accounts.json` or its
    /// `ANTHROPIC_ACCOUNTS_FILE` / `ANTHROPIC_ACCOUNTS_DIR` override).
    pub store_path: PathBuf,
    /// The OAuth endpoints (production, or the `ANTHROPIC_OAUTH_*`
    /// overrides).
    pub endpoints: Endpoints,
    /// Whether a rotation is published to Claude Code's credentials (the
    /// link the plugins share; `ANTHROPIC_NATIVE_PUBLISH=0` turns it off).
    pub native_publish: NativePublish,
    /// The OAuth profile endpoint a new login is identified with
    /// (`ANTHROPIC_OAUTH_PROFILE_URL` overrides it).
    pub profile_url: String,
    /// Refuse every OAuth call to a non-loopback host (tests).
    pub require_loopback: bool,
}

impl SharedStoreConfig {
    /// The configuration the plugins resolve from the same environment.
    /// Reads no file.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            store_path: anthropic::default_store_path(),
            endpoints: Endpoints::from_env(),
            native_publish: NativePublish::from_env(),
            profile_url: anthropic::profile::profile_url_from_lookup(|key| std::env::var(key).ok()),
            require_loopback: false,
        }
    }

    /// A configuration over `store_path` that reaches only the given
    /// loopback token and profile endpoints (every other OAuth host is
    /// refused) and never Claude Code's credentials: tests and sandboxes.
    #[must_use]
    pub fn isolated(store_path: PathBuf, token_url: &str, profile_url: &str) -> Self {
        let mut endpoints = Endpoints::prod();
        endpoints.token_url = token_url.to_string();
        Self {
            store_path,
            endpoints,
            native_publish: NativePublish::Off,
            profile_url: profile_url.to_string(),
            require_loopback: true,
        }
    }
}

/// How the served credentials were obtained in this process, without any
/// account identity (the adoption telemetry's facts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceUsage {
    /// Live tokens served as the store held them.
    pub store: u64,
    /// Tokens this process refreshed.
    pub refreshed: u64,
    /// Tokens another process had already rotated (adopted).
    pub adopted: u64,
    /// Claude Code's live token, borrowed for its linked account.
    pub claude_code: u64,
    /// Requests the store could not serve (refresh failed, revoked,
    /// unreadable, network).
    pub failed: u64,
    /// The way the first served credential was obtained.
    pub first: Option<&'static str>,
}

impl SourceUsage {
    /// Whether the source answered any request.
    #[must_use]
    pub fn answered(&self) -> bool {
        self.first.is_some() || self.failed > 0
    }
}

/// The store file's identity for the status memo: `(len, mtime)`. The
/// store is replaced by rename on every write, so an unchanged stamp is an
/// unchanged document.
type FileStamp = (u64, SystemTime);

/// The shared account store serving one provider id.
pub struct SharedStoreSource {
    pub(crate) config: SharedStoreConfig,
    /// Built on the first credential request (no startup cost).
    client: OnceLock<OAuthClient>,
    /// One credential resolution at a time in this process.
    flight: Mutex<()>,
    status_memo: Mutex<Option<(FileStamp, Option<CredentialSourceStatus>)>>,
    usage: Mutex<SourceUsage>,
}

impl SharedStoreSource {
    /// A source over `config`. Does no I/O.
    #[must_use]
    pub fn new(config: SharedStoreConfig) -> Self {
        Self {
            config,
            client: OnceLock::new(),
            flight: Mutex::new(()),
            status_memo: Mutex::new(None),
            usage: Mutex::new(SourceUsage::default()),
        }
    }

    /// The store file this source reads.
    #[must_use]
    pub fn store_path(&self) -> &Path {
        &self.config.store_path
    }

    /// How this process's requests were served so far.
    #[must_use]
    pub fn usage(&self) -> SourceUsage {
        *self.usage.lock_or_recover()
    }

    pub(crate) fn client(&self) -> &OAuthClient {
        self.client.get_or_init(|| {
            OAuthClient::new(self.config.endpoints.clone())
                .native_publish(self.config.native_publish.clone())
                .require_loopback(self.config.require_loopback)
        })
    }

    fn read_status(&self) -> Option<CredentialSourceStatus> {
        let store = match AccountStore::load(&self.config.store_path) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(%error, "the shared Anthropic account store is unreadable");
                return None;
            }
        };
        let request = AccessRequest::default();
        let candidates = access_candidates(&store, &request, chrono::Utc::now()).ok()?;
        // The revision changes whenever a candidate's access token rotates.
        let mut hasher = Sha256::new();
        for account in &candidates {
            hasher.update(account.id.as_bytes());
            hasher.update([0]);
            if let Some(tokens) = account.oauth() {
                hasher.update(tokens.access.expose().as_bytes());
            }
            hasher.update([0]);
        }
        let digest = hasher.finalize();
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&digest[..8]);
        Some(CredentialSourceStatus {
            label: STORE_LABEL.to_string(),
            revision: format!("{:016x}", u64::from_be_bytes(prefix)),
        })
    }

    /// Count one answered request: how its credential was obtained, or
    /// `None` for a failure.
    fn record(&self, served: Option<AccessSource>) {
        let mut usage = self.usage.lock_or_recover();
        let Some(source) = served else {
            usage.failed += 1;
            return;
        };
        match source {
            AccessSource::Store => usage.store += 1,
            AccessSource::Refreshed => usage.refreshed += 1,
            AccessSource::Adopted => usage.adopted += 1,
            AccessSource::ClaudeCode => usage.claude_code += 1,
        }
        usage.first.get_or_insert(source.code());
    }
}

impl ProviderCredentialSource for SharedStoreSource {
    fn status(&self) -> Option<CredentialSourceStatus> {
        let metadata = std::fs::symlink_metadata(&self.config.store_path).ok()?;
        let stamp = (metadata.len(), metadata.modified().ok()?);
        let mut memo = self.status_memo.lock_or_recover();
        if let Some((memo_stamp, status)) = memo.as_ref() {
            if *memo_stamp == stamp {
                return status.clone();
            }
        }
        let status = self.read_status();
        *memo = Some((stamp, status.clone()));
        status
    }

    fn adopt_stored_login(&self, login: &StoredOAuthLogin) -> StoredLoginCustody {
        self.adopt(login)
    }

    fn credential(&self) -> Result<SourcedCredential, CredentialSourceError> {
        let _flight = self.flight.lock_or_recover();
        let client = self.client();
        let path = self.config.store_path.as_path();
        let resolved = block_on_own_runtime(get_access_token(
            client,
            path,
            &AccessRequest::default(),
            &SharedRefreshOptions::default(),
        ));
        match resolved {
            Ok(Ok(grant)) => {
                self.record(Some(grant.source));
                Ok(SourcedCredential {
                    api_key: grant.access_token,
                    headers: std::collections::BTreeMap::new(),
                })
            }
            Ok(Err(error)) if error.kind == AccessErrorKind::AuthRequired => {
                Err(CredentialSourceError::NotConfigured)
            }
            Ok(Err(error)) => {
                self.record(None);
                Err(CredentialSourceError::Unavailable(error.to_string()))
            }
            Err(message) => {
                self.record(None);
                Err(CredentialSourceError::Unavailable(message))
            }
        }
    }
}

/// Run `future` to completion on a runtime of its own, on a thread of its
/// own: the source's synchronous entry points may run on an async worker,
/// where blocking on the caller's runtime would deadlock it.
pub(crate) fn block_on_own_runtime<F>(future: F) -> Result<F::Output, String>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                Ok(runtime.block_on(future))
            })
            .join()
            .unwrap_or_else(|_| Err("the store call panicked".to_string()))
    })
}

#[cfg(test)]
mod tests;
