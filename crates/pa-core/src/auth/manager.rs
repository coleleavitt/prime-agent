//! `AuthStorage`: credential resolution with runtime overrides, environment
//! keys, stored credentials, fallback resolvers, and stale-marking.

use pa_types::sync::MutexExt;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use super::resolve_config_value::{resolve_config_value, resolve_config_value_uncached};
use super::storage::{parse_storage_data, AuthStorageBackend};
use super::types::{
    AuthCredential, AuthSource, AuthSourceToken, AuthStatus, AuthStorageData, PrimeTeamAssignment,
    PrimeTeamCredential, StoredPrimeTeam, PRIME_INFERENCE_PROVIDER_ID,
};

#[cfg(test)]
mod tests;

mod lookup;

mod prime_inference;

mod unsaved;

pub(crate) fn fingerprint(source: AuthSource, material: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("{source:?}"));
    hasher.update([0]);
    hasher.update(material.as_bytes());
    format!("{source:?}:{}", hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{b:02x}");
        output
    })
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |d| d.as_millis() as i64)
}

/// One OAuth refresh in flight per provider: the token fetch runs outside every
/// lock, so TS's single-threaded single-flight needs its own gate.
fn refresh_flight(provider: &str) -> RefreshFlight {
    static FLIGHTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, &'static std::sync::Mutex<()>>>,
    > = std::sync::OnceLock::new();
    let registry = FLIGHTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let lock = {
        let mut registry = registry.lock_or_recover();
        *registry
            .entry(provider.to_string())
            .or_insert_with(|| Box::leak(Box::new(std::sync::Mutex::new(()))))
    };
    let guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(test)]
    FLIGHT_HELD.with(|held| held.set(true));
    RefreshFlight { _guard: guard }
}

/// A held [`refresh_flight`] gate, released on drop.
struct RefreshFlight {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
thread_local! {
    /// Whether this thread holds a refresh flight (the write-phase verifier).
    static FLIGHT_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
impl Drop for RefreshFlight {
    fn drop(&mut self) {
        FLIGHT_HELD.with(|held| held.set(false));
    }
}

#[derive(Clone)]
struct AuthSourceCandidate {
    source: AuthSource,
    configured: bool,
    label: Option<String>,
    identity_fingerprint: String,
    value_fingerprint: Option<String>,
    /// Deferred value material (commands that must run at read time).
    resolve_value_fingerprint: Option<ValueFingerprintResolver>,
}

impl AuthSourceCandidate {
    fn resolved_value_fingerprint(&self) -> Option<String> {
        self.value_fingerprint
            .clone()
            .or_else(|| self.resolve_value_fingerprint.as_ref().and_then(|f| f()))
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct AuthApiKeyResult {
    pub api_key: Option<String>,
    pub source_token: Option<AuthSourceToken>,
    pub credential_type: Option<&'static str>,
    /// The stored OAuth login was expired and its refresh failed: there
    /// is no key, but the provider is signed in (the credential stays for
    /// a later retry). Callers report an authentication failure
    /// ([`oauth_refresh_failed_message`]) instead of calling the provider
    /// keyless.
    pub oauth_refresh_failed: bool,
    /// Request headers the credential needs (an installed credential
    /// source's), merged into the request after the provider's own.
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

/// The authentication failure a turn reports when a stored OAuth login
/// could not be refreshed (TS `formatAuthenticationFailedMessage`, naming
/// the refresh): the provider would otherwise fail keyless as "No API key
/// for provider".
#[must_use]
pub fn oauth_refresh_failed_message(provider: &str) -> String {
    format!(
        "Authentication failed for \"{provider}\": the OAuth token refresh failed. Credentials may have expired or network is unavailable.\n\nRun /login to update credentials."
    )
}

/// OAuth integration seam, implemented by the pa-ai oauth registry; a
/// trait so auth storage stays testable without network flows.
pub trait OAuthIntegration: Send + Sync {
    /// The resolved API key for stored OAuth credentials (bearer/token form).
    fn api_key_for(&self, provider_id: &str, credential: &AuthCredential) -> Option<String>;
    /// Refresh an expired credential; `None` = refresh failed.
    fn refresh(&self, provider_id: &str, credentials: &AuthStorageData) -> Option<AuthCredential>;
}

/// No OAuth provider registry available (embedded hosts); stored OAuth
/// credentials still serve their access token until expiry.
#[derive(Default)]
pub struct NoOAuth;

impl OAuthIntegration for NoOAuth {
    fn api_key_for(&self, _provider: &str, credential: &AuthCredential) -> Option<String> {
        match credential {
            AuthCredential::Oauth { access, .. } => Some(access.clone()),
            _ => None,
        }
    }

    fn refresh(&self, _provider: &str, _credentials: &AuthStorageData) -> Option<AuthCredential> {
        None
    }
}

/// The seam through which auth resolution reads ambient credentials.
/// Production reads the real environment; tests inject a fixed mapping.
pub(crate) trait EnvCredentialSource: Send + Sync {
    /// Env var names (priority order) currently set to non-empty values that
    /// would supply the provider's API key, if any.
    fn key_names(&self, provider: &str) -> Option<Vec<String>>;
    fn api_key(&self, provider: &str) -> Option<String>;
    /// Raw `PRIME_TEAM_ID` value if set; the caller trims and rejects empty.
    fn prime_team_id(&self) -> Option<String>;
    /// Raw `PRIME_CONTEXT` value if set: the prime CLI context that replaces
    /// a directory's `.prime/context.json`.
    fn prime_context(&self) -> Option<String>;
    /// The home whose `.prime` is the prime CLI's global config (the
    /// directory walk's stop and the saved contexts' location).
    fn home_dir(&self) -> Option<std::path::PathBuf> {
        pa_types::platform::home_dir()
    }
    /// Identity material for ambient multi-variable credential sources
    /// (AWS profiles, container credentials, Google ADC projects).
    fn ambient_identity_material(&self, provider: &str) -> String;
}

struct ProcessEnvCredentials;

/// No-op environment source: no ambient variable supplies a key or team
/// id; the hermetic seam behind [`AuthStorage::in_memory_without_env`].
struct NoEnvCredentials;

impl EnvCredentialSource for NoEnvCredentials {
    fn key_names(&self, _provider: &str) -> Option<Vec<String>> {
        None
    }

    fn api_key(&self, _provider: &str) -> Option<String> {
        None
    }

    fn prime_team_id(&self) -> Option<String> {
        None
    }

    fn prime_context(&self) -> Option<String> {
        None
    }

    fn ambient_identity_material(&self, provider: &str) -> String {
        provider.to_string()
    }
}

impl EnvCredentialSource for ProcessEnvCredentials {
    fn key_names(&self, provider: &str) -> Option<Vec<String>> {
        pa_ai::env_api_keys::find_env_keys(provider)
    }

    fn api_key(&self, provider: &str) -> Option<String> {
        pa_ai::env_api_keys::get_env_api_key(provider)
    }

    fn prime_team_id(&self) -> Option<String> {
        std::env::var("PRIME_TEAM_ID").ok()
    }

    fn prime_context(&self) -> Option<String> {
        std::env::var("PRIME_CONTEXT").ok()
    }

    fn ambient_identity_material(&self, provider: &str) -> String {
        let env = |name: &str| std::env::var(name).unwrap_or_default();
        match provider {
            "amazon-bedrock" => {
                if !env("AWS_PROFILE").is_empty() {
                    return format!("amazon-bedrock:profile:{}", env("AWS_PROFILE"));
                }
                if !env("AWS_ACCESS_KEY_ID").is_empty() {
                    return format!(
                        "amazon-bedrock:access-key:{}:{}:{}",
                        env("AWS_ACCESS_KEY_ID"),
                        env("AWS_SECRET_ACCESS_KEY"),
                        env("AWS_SESSION_TOKEN")
                    );
                }
                if !env("AWS_BEARER_TOKEN_BEDROCK").is_empty() {
                    return format!("amazon-bedrock:bearer:{}", env("AWS_BEARER_TOKEN_BEDROCK"));
                }
                for (name, prefix) in [
                    ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "ecs-relative"),
                    ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "ecs-full"),
                    ("AWS_WEB_IDENTITY_TOKEN_FILE", "web-identity"),
                ] {
                    if !env(name).is_empty() {
                        return format!("amazon-bedrock:{prefix}:{}", env(name));
                    }
                }
                provider.to_string()
            }
            "google-vertex" => format!(
                "google-vertex:{}:{}:{}",
                if env("GOOGLE_CLOUD_PROJECT").is_empty() {
                    env("GCLOUD_PROJECT")
                } else {
                    env("GOOGLE_CLOUD_PROJECT")
                },
                env("GOOGLE_CLOUD_LOCATION"),
                if env("GOOGLE_APPLICATION_CREDENTIALS").is_empty() {
                    "application-default".to_string()
                } else {
                    env("GOOGLE_APPLICATION_CREDENTIALS")
                }
            ),
            other => other.to_string(),
        }
    }
}

/// Fallback key resolver (custom provider configs).
pub type FallbackResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Deferred value-fingerprint resolver (command keys).
type ValueFingerprintResolver = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// A resolved prime CLI directory context, or why it is broken.
type DirectorySelectionMemo =
    std::sync::OnceLock<Result<Option<super::PrimeDirectorySelection>, String>>;

pub struct AuthStorage {
    storage: Arc<dyn AuthStorageBackend>,
    oauth: Arc<dyn OAuthIntegration>,
    env_credentials: Arc<dyn EnvCredentialSource>,
    data: AuthStorageData,
    runtime_overrides: HashMap<String, String>,
    stale_auth_sources: HashMap<String, Vec<AuthSourceToken>>,
    fallback_resolver: Option<FallbackResolver>,
    load_error: Option<String>,
    errors: Vec<String>,
    /// Memoized candidates (TS `authCandidateMemos`), keyed by `source:provider`,
    /// superseded when the hashed material changes.
    candidate_memos: std::sync::Mutex<HashMap<String, (String, AuthSourceCandidate)>>,
    /// The session directory whose prime CLI directory context selects the
    /// Prime Inference team and key (see `with_project_dir`); `None` never
    /// reads one.
    project_dir: Option<std::path::PathBuf>,
    /// The memoized directory context resolution: one walk and parse (and
    /// one warning for a broken pin) per instance, reset by `reload`.
    directory_selection: DirectorySelectionMemo,
}

impl AuthStorage {
    pub fn from_storage(
        storage: Arc<dyn AuthStorageBackend>,
        oauth: Arc<dyn OAuthIntegration>,
    ) -> Self {
        let mut auth = Self {
            storage,
            oauth,
            env_credentials: Arc::new(ProcessEnvCredentials),
            data: AuthStorageData::default(),
            runtime_overrides: HashMap::new(),
            stale_auth_sources: HashMap::new(),
            fallback_resolver: None,
            load_error: None,
            errors: Vec::new(),
            candidate_memos: std::sync::Mutex::new(HashMap::new()),
            project_dir: None,
            directory_selection: DirectorySelectionMemo::default(),
        };
        auth.reload();
        auth
    }

    /// File-backed storage at `agentDir/auth.json`.
    pub fn create(agent_dir: impl AsRef<std::path::Path>) -> Self {
        // Matches TS delegating to the oauth registry on every instance;
        // only token refresh gains (api_key_for is the passthrough).
        Self::create_with_oauth(
            agent_dir,
            Arc::new(super::provider_oauth::ProviderOAuth::new()),
        )
    }

    /// File-backed storage with an explicit OAuth integration (the MCP
    /// manager uses this so stored `mcp:*` tokens refresh on expiry).
    pub fn create_with_oauth(
        agent_dir: impl AsRef<std::path::Path>,
        oauth: Arc<dyn OAuthIntegration>,
    ) -> Self {
        let backend: Arc<dyn AuthStorageBackend> = Arc::new(
            super::storage::FileAuthStorageBackend::new(agent_dir.as_ref().join("auth.json")),
        );
        Self::from_storage(backend, oauth)
    }

    pub fn in_memory(data: &AuthStorageData, oauth: Arc<dyn OAuthIntegration>) -> Self {
        Self::in_memory_with_env_source(data, oauth, Arc::new(ProcessEnvCredentials))
    }

    /// In-memory storage with no ambient environment source: hermetic for hosts
    /// and harnesses that must pin the model catalog scope.
    pub fn in_memory_without_env(data: &AuthStorageData, oauth: Arc<dyn OAuthIntegration>) -> Self {
        Self::in_memory_with_env_source(data, oauth, Arc::new(NoEnvCredentials))
    }

    /// In-memory storage with an injected environment source (hermetic tests).
    #[cfg(test)]
    pub(crate) fn in_memory_with_env(
        data: &AuthStorageData,
        oauth: Arc<dyn OAuthIntegration>,
        env_credentials: Arc<dyn EnvCredentialSource>,
    ) -> Self {
        Self::in_memory_with_env_source(data, oauth, env_credentials)
    }

    fn in_memory_with_env_source(
        data: &AuthStorageData,
        oauth: Arc<dyn OAuthIntegration>,
        env_credentials: Arc<dyn EnvCredentialSource>,
    ) -> Self {
        let backend: Arc<dyn AuthStorageBackend> =
            Arc::new(crate::auth::storage::InMemoryAuthStorageBackend::default());
        let content = serde_json::to_string_pretty(&data.0).unwrap_or_default();
        backend
            .with_lock(&mut |current| {
                let _ = current;
                Ok(((), Some(content.clone())))
            })
            .ok();
        let mut auth = Self {
            storage: backend,
            oauth,
            env_credentials,
            data: AuthStorageData::default(),
            runtime_overrides: HashMap::new(),
            stale_auth_sources: HashMap::new(),
            fallback_resolver: None,
            load_error: None,
            errors: Vec::new(),
            candidate_memos: std::sync::Mutex::new(HashMap::new()),
            project_dir: None,
            directory_selection: DirectorySelectionMemo::default(),
        };
        auth.reload();
        auth
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn drain_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    pub fn reload(&mut self) {
        self.directory_selection = DirectorySelectionMemo::default();
        // The pure-read arm: a locked protocol read on any cache miss, the
        // process-cached copy on a hit (see `AuthStorageBackend::read`).
        let result = self.storage.read();
        match result.and_then(|content| parse_storage_data(content.as_deref())) {
            Ok(mut data) => {
                // A refresh this store could not save serves in place of the
                // dead login it replaced.
                self.overlay_unsaved_refreshes(&mut data);
                self.data = data;
                self.load_error = None;
            }
            Err(error) => {
                self.load_error = Some(error.to_string());
                self.errors.push(error.to_string());
            }
        }
    }

    /// Pick up a credential write from another process (upstream #3000):
    /// `auth.json` is shared machine-wide, so a `/login` in one session must
    /// reach a long-lived worker serving another, or a rejected key wedges
    /// it until a restart. An external write is an explicit credential
    /// change, so the stored-source stale markings drop exactly as an
    /// in-process `set`/`remove` drops them (a re-login with the same key
    /// recovers too); an unreadable rewrite keeps the old state until a
    /// readable one replaces it. Returns whether the store reloaded.
    pub fn refresh_from_external_changes(&mut self) -> bool {
        if !self.storage.changed_externally() {
            return false;
        }
        self.reload();
        if self.load_error.is_some() {
            return false;
        }
        self.stale_auth_sources.retain(|_, tokens| {
            tokens.retain(|token| token.source != AuthSource::Stored);
            !tokens.is_empty()
        });
        true
    }

    /// Runtime API-key override (CLI `--api-key`); not persisted.
    pub fn set_runtime_api_key(&mut self, provider: &str, api_key: String) {
        self.clear_stale_auth_source(provider, AuthSource::Runtime);
        self.runtime_overrides.insert(provider.to_string(), api_key);
    }

    pub fn remove_runtime_api_key(&mut self, provider: &str) {
        self.clear_stale_auth_source(provider, AuthSource::Runtime);
        self.runtime_overrides.remove(provider);
    }

    /// Fallback resolver for keys from custom provider configs (models.json).
    pub fn set_fallback_resolver(&mut self, resolver: FallbackResolver) {
        self.fallback_resolver = Some(resolver);
    }

    fn stored_value_material(&self, credential: &AuthCredential) -> Option<String> {
        match credential {
            AuthCredential::ApiKey { key, .. } => {
                if key.starts_with('!') {
                    let resolved = resolve_config_value_uncached(key)?;
                    Some(format!("api_key:command:{key} {resolved}"))
                } else {
                    Some(format!(
                        "api_key:{key} {}",
                        resolve_config_value(key).unwrap_or_default()
                    ))
                }
            }
            AuthCredential::Oauth {
                access,
                refresh,
                expires,
                ..
            } => {
                let api_key = self
                    .oauth
                    .api_key_for("", credential)
                    .unwrap_or_else(|| access.clone());
                Some(format!(
                    "oauth:{api_key} {} {expires}",
                    refresh.clone().unwrap_or_default()
                ))
            }
            AuthCredential::McpStaticToken { bearer, .. } => {
                Some(format!("mcp_static_token:{bearer}"))
            }
        }
    }

    /// Memo marker for candidates whose value material could not be resolved into a
    /// key: the entry is keyed by everything else the candidate hashes.
    fn auth_source_lazy_value_key() -> &'static str {
        "value-lazy"
    }

    /// The memo key is the hashed material itself, so the memo is
    /// superseded exactly when the material is.
    fn reuse_auth_source_candidate(
        &self,
        source: AuthSource,
        provider: &str,
        key: String,
        build: impl FnOnce() -> AuthSourceCandidate,
    ) -> AuthSourceCandidate {
        let memo_slot = format!("{source:?}:{provider}");
        let mut memos = self
            .candidate_memos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((memo_key, candidate)) = memos.get(&memo_slot) {
            if memo_key == &key {
                return candidate.clone();
            }
        }
        let candidate = build();
        memos.insert(memo_slot, (key, candidate.clone()));
        candidate
    }

    fn runtime_candidate(&self, provider: &str) -> Option<AuthSourceCandidate> {
        let key = self.runtime_overrides.get(provider)?.clone();
        Some(self.reuse_auth_source_candidate(
            AuthSource::Runtime,
            provider,
            key.clone(),
            move || AuthSourceCandidate {
                source: AuthSource::Runtime,
                configured: true,
                label: None,
                identity_fingerprint: fingerprint(AuthSource::Runtime, "identity:runtime-override"),
                value_fingerprint: Some(fingerprint(
                    AuthSource::Runtime,
                    &format!("value:runtime-override {key}"),
                )),
                resolve_value_fingerprint: None,
            },
        ))
    }

    /// The installed credential source's candidate while it reports a
    /// login; its value is the source's revision, never the secret.
    fn credential_source_candidate(&self, provider: &str) -> Option<AuthSourceCandidate> {
        let status = super::credential_source(provider)?.status()?;
        let key = format!("{} {}", status.label, status.revision);
        Some(
            self.reuse_auth_source_candidate(AuthSource::CredentialSource, provider, key, || {
                AuthSourceCandidate {
                    source: AuthSource::CredentialSource,
                    configured: true,
                    label: Some(status.label),
                    identity_fingerprint: fingerprint(
                        AuthSource::CredentialSource,
                        &format!("identity:credential-source {provider}"),
                    ),
                    value_fingerprint: Some(fingerprint(
                        AuthSource::CredentialSource,
                        &format!("value:credential-source {}", status.revision),
                    )),
                    resolve_value_fingerprint: None,
                }
            }),
        )
    }

    fn stored_candidate(&self, provider: &str) -> Option<AuthSourceCandidate> {
        let credential = self.data.credential(provider)?;
        let value_material = self.stored_value_material(&credential);
        let key = format!(
            "identity:auth.json {}",
            value_material
                .as_deref()
                .unwrap_or(Self::auth_source_lazy_value_key()),
        );
        Some(
            self.reuse_auth_source_candidate(AuthSource::Stored, provider, key, || {
                AuthSourceCandidate {
                    source: AuthSource::Stored,
                    configured: true,
                    label: None,
                    identity_fingerprint: fingerprint(AuthSource::Stored, "identity:auth.json"),
                    value_fingerprint: value_material.map(|material| {
                        fingerprint(AuthSource::Stored, &format!("value:auth.json {material}"))
                    }),
                    resolve_value_fingerprint: None,
                }
            }),
        )
    }

    fn environment_candidate(&self, provider: &str) -> Option<AuthSourceCandidate> {
        let env_keys = self.env_credentials.key_names(provider);
        let api_key = self.env_credentials.api_key(provider)?;
        let label = env_keys
            .as_ref()
            .and_then(|keys| keys.first().cloned())
            .unwrap_or_else(|| "ambient credentials".to_string());
        let identity_material = env_keys
            .and_then(|keys| keys.first().cloned())
            .unwrap_or_else(|| self.env_credentials.ambient_identity_material(provider));
        // Env values are deliberately re-read on every call; the memo
        // only skips re-fingerprinting unchanged material.
        let key = format!("{identity_material} {api_key}");
        Some(
            self.reuse_auth_source_candidate(AuthSource::Environment, provider, key, || {
                AuthSourceCandidate {
                    source: AuthSource::Environment,
                    configured: false,
                    label: Some(label),
                    identity_fingerprint: fingerprint(
                        AuthSource::Environment,
                        &format!("identity:{identity_material}"),
                    ),
                    value_fingerprint: Some(fingerprint(
                        AuthSource::Environment,
                        &format!("value:{identity_material} {api_key}"),
                    )),
                    resolve_value_fingerprint: None,
                }
            }),
        )
    }

    fn fallback_candidate(&self, provider: &str) -> Option<AuthSourceCandidate> {
        let resolver = self.fallback_resolver.as_ref()?;
        let api_key = resolver(provider)?;
        Some(self.reuse_auth_source_candidate(
            AuthSource::Fallback,
            provider,
            api_key.clone(),
            || AuthSourceCandidate {
                source: AuthSource::Fallback,
                configured: false,
                label: Some("custom provider config".to_string()),
                identity_fingerprint: fingerprint(
                    AuthSource::Fallback,
                    &format!("identity:{provider}"),
                ),
                value_fingerprint: Some(fingerprint(
                    AuthSource::Fallback,
                    &format!("value:{provider} {api_key}"),
                )),
                resolve_value_fingerprint: None,
            },
        ))
    }

    /// Candidate priority: runtime first, then an installed credential
    /// source; prime-inference prefers environment over the directory's
    /// saved context over stored; everyone else prefers stored over
    /// environment; fallback last.
    fn auth_source_candidates(
        &self,
        provider: &str,
        include_fallback: bool,
    ) -> Vec<AuthSourceCandidate> {
        let fallback = include_fallback
            .then(|| self.fallback_candidate(provider))
            .flatten();
        if provider == PRIME_INFERENCE_PROVIDER_ID {
            // A saved context the session directory selects replaces the
            // stored login there: the stored key is another account's.
            let directory = self.directory_context_candidate(provider);
            let stored = directory
                .is_none()
                .then(|| self.stored_candidate(provider))
                .flatten();
            vec![
                self.runtime_candidate(provider),
                self.credential_source_candidate(provider),
                self.environment_candidate(provider),
                directory,
                stored,
                fallback,
            ]
        } else {
            vec![
                self.runtime_candidate(provider),
                self.credential_source_candidate(provider),
                self.stored_candidate(provider),
                self.environment_candidate(provider),
                fallback,
            ]
        }
        .into_iter()
        .flatten()
        .collect()
    }

    fn matching_stale(
        &self,
        provider: &str,
        candidate: &AuthSourceCandidate,
    ) -> Vec<&AuthSourceToken> {
        self.stale_auth_sources
            .get(provider)
            .map(|stale| {
                stale
                    .iter()
                    .filter(|token| {
                        token.source == candidate.source
                            && token.identity_fingerprint == candidate.identity_fingerprint
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_stale(&self, provider: &str, candidate: &AuthSourceCandidate) -> bool {
        let matching = self.matching_stale(provider, candidate);
        if matching.is_empty() {
            return false;
        }
        candidate.resolved_value_fingerprint().is_some_and(|value| {
            matching
                .iter()
                .any(|token| token.value_fingerprint == value)
        })
    }

    fn available_candidate(
        &self,
        provider: &str,
        include_fallback: bool,
    ) -> Option<AuthSourceCandidate> {
        self.auth_source_candidates(provider, include_fallback)
            .into_iter()
            .find(|candidate| !self.is_stale(provider, candidate))
    }

    fn token_for(provider: &str, candidate: &AuthSourceCandidate) -> Option<AuthSourceToken> {
        Some(AuthSourceToken {
            provider: provider.to_string(),
            source: candidate.source,
            identity_fingerprint: candidate.identity_fingerprint.clone(),
            value_fingerprint: candidate.resolved_value_fingerprint()?,
        })
    }

    pub fn list(&self) -> Vec<String> {
        self.data.keys()
    }

    pub fn has(&self, provider: &str) -> bool {
        self.data.get(provider).is_some()
    }

    /// Any form of auth configured (never refreshes tokens).
    pub fn has_auth(&self, provider: &str) -> bool {
        self.available_candidate(provider, true).is_some()
    }

    /// Status without credential values.
    pub fn get_auth_status(&self, provider: &str) -> AuthStatus {
        let candidates = self.auth_source_candidates(provider, true);
        let mut has_stale = false;
        for candidate in &candidates {
            if self.is_stale(provider, candidate) {
                has_stale = true;
                continue;
            }
            return AuthStatus {
                configured: candidate.configured,
                source: Some(candidate.source),
                label: candidate.label.clone(),
            };
        }
        if has_stale {
            AuthStatus {
                configured: false,
                source: Some(AuthSource::Stale),
                label: Some("expired".to_string()),
            }
        } else {
            AuthStatus::default()
        }
    }

    pub fn get_all(&self) -> AuthStorageData {
        self.data.clone()
    }

    /// Mark the current credential stale (e.g. the server rejected it).
    pub fn mark_auth_stale(&mut self, provider: &str) -> bool {
        let Some(candidate) = self.available_candidate(provider, true) else {
            return false;
        };
        let Some(token) = Self::token_for(provider, &candidate) else {
            return false;
        };
        self.mark_auth_source_stale(token)
    }

    pub fn mark_auth_source_stale(&mut self, token: AuthSourceToken) -> bool {
        if token.provider.is_empty() {
            return false;
        }
        let stale = self
            .stale_auth_sources
            .entry(token.provider.clone())
            .or_default();
        if !stale.contains(&token) {
            stale.push(token);
        }
        true
    }

    pub fn clear_auth_stale(&mut self, provider: &str) {
        self.stale_auth_sources.remove(provider);
    }

    fn clear_stale_auth_source(&mut self, provider: &str, source: AuthSource) {
        if let Some(stale) = self.stale_auth_sources.get_mut(provider) {
            stale.retain(|token| token.source != source);
            if stale.is_empty() {
                self.stale_auth_sources.remove(provider);
            }
        }
    }

    pub fn set(&mut self, provider: &str, credential: AuthCredential) {
        self.persist_provider_change(provider, Some(credential));
    }

    pub fn remove(&mut self, provider: &str) {
        self.persist_provider_change(provider, None);
    }

    pub fn logout(&mut self, provider: &str) {
        self.remove(provider);
    }

    /// Offer the provider's stored OAuth login to its installed credential
    /// source; when the source takes custody, remove the entry.
    ///
    /// The offer and the removal run under the document lock, on the file as
    /// it is now, not on this instance's earlier read: once one process has
    /// moved the login, the source may rotate (spend) its single-use refresh
    /// token at any time, so a process that read the file before the move
    /// must not offer the spent token again (the source would hold it a
    /// second time and present it later). A newer login written in the
    /// meantime is what is offered. The source's custody work (a store write,
    /// at most one identity lookup) holds the lock, once per login.
    pub(crate) fn offer_stored_login_to_source(&mut self, provider: &str) {
        let Some(source) = super::credential_source(provider) else {
            return;
        };
        // An unreadable file is never offered: what is taken into custody
        // must also leave the file.
        if self.load_error.is_some()
            || !matches!(
                self.data.credential(provider),
                Some(AuthCredential::Oauth {
                    refresh: Some(_),
                    ..
                })
            )
        {
            return;
        }
        let result = self.storage.with_lock(&mut |current| {
            let mut data = parse_storage_data(current.as_deref())?;
            let Some(AuthCredential::Oauth {
                access,
                refresh: Some(refresh),
                expires,
                ..
            }) = data.credential(provider)
            else {
                return Ok(((), None));
            };
            let login = super::StoredOAuthLogin {
                access,
                refresh,
                expires_ms: expires,
            };
            if source.adopt_stored_login(&login) != super::StoredLoginCustody::Adopted {
                return Ok(((), None));
            }
            data.remove(provider);
            Ok(((), Some(serde_json::to_string_pretty(&data.0)?)))
        });
        if let Err(error) = result {
            self.errors.push(error.to_string());
            return;
        }
        self.reload();
    }

    fn persist_provider_change(&mut self, provider: &str, credential: Option<AuthCredential>) {
        if self.load_error.is_some() {
            return;
        }
        let mut next_credential = credential;
        let result = self.storage.with_lock(&mut |current| {
            let mut data = parse_storage_data(current.as_deref())?;
            match next_credential.take() {
                Some(credential) => data.insert(provider, &credential),
                None => data.remove(provider),
            }
            let content = serde_json::to_string_pretty(&data.0)?;
            Ok(((), Some(content)))
        });
        if let Err(error) = result {
            self.errors.push(error.to_string());
            return;
        }
        self.forget_unsaved_refreshes(provider);
        // Reload from what we wrote.
        self.reload();
    }
}
