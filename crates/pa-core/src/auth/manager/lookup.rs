//! The API-key lookup + OAuth refresh arm (moved with its concern): the
//! resolution walk over the candidate sources (runtime override, an
//! installed credential source, prime inference env and then the directory's saved context before stored,
//! stored, environment, fallback) with the staleness gate, the OAuth
//! expiry refresh under the per-provider single-flight, and the
//! passthrough `get_api_key` (TS getApiKey).

use super::{
    AuthApiKeyResult,
    AuthCredential,
    AuthSourceCandidate,
    AuthStorage,
    OAuthRefreshError,
    PRIME_INFERENCE_PROVIDER_ID,
    now_epoch_ms,
    parse_storage_data,
    refresh_flight,
    refresh_token_revoked,
    remember_revoked_refresh_token,
    resolve_config_value,
    resolve_config_value_uncached,
};
use crate::auth::{CredentialSourceError, credential_source};

impl AuthStorage {
    pub fn get_api_key_with_source_token(
        &mut self,
        provider_id: &str,
        include_fallback: bool,
    ) -> AuthApiKeyResult {
        self.refresh_from_external_changes();
        self.save_unsaved_refreshes();
        // 1. Runtime override.
        if let Some(candidate) = self.runtime_candidate(provider_id) {
            if !self.is_stale(provider_id, &candidate) {
                if let Some(api_key) = self.runtime_overrides.get(provider_id).cloned() {
                    return AuthApiKeyResult {
                        api_key: Some(api_key),
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: Some("api_key"),
                        oauth_refresh_failed: false,
                        headers: None,
                    };
                }
            }
        }

        // 1b. A login auth.json still holds is offered to the provider's
        // installed credential source first (a one-time migration into
        // it). The source then owns the provider while it reports a login:
        // its failure is the authentication failure, never a fall-through
        // to a credential it may have superseded.
        self.offer_stored_login_to_source(provider_id);
        if let Some(candidate) = self.credential_source_candidate(provider_id) {
            if !self.is_stale(provider_id, &candidate) {
                match credential_source(provider_id)
                    .map_or(Err(CredentialSourceError::NotConfigured), |source| {
                        source.credential()
                    }) {
                    Ok(credential) => {
                        return AuthApiKeyResult {
                            api_key: Some(credential.api_key),
                            source_token: Self::token_for(provider_id, &candidate),
                            credential_type: Some("oauth"),
                            oauth_refresh_failed: false,
                            headers: (!credential.headers.is_empty()).then_some(credential.headers),
                        };
                    }
                    Err(CredentialSourceError::NotConfigured) => {}
                    Err(CredentialSourceError::Unavailable(message)) => {
                        tracing::warn!(
                            provider = provider_id,
                            error = %message,
                            "the provider's credential source produced no credential"
                        );
                        return AuthApiKeyResult {
                            credential_type: Some("oauth"),
                            oauth_refresh_failed: true,
                            ..AuthApiKeyResult::default()
                        };
                    }
                }
            }
        }

        let env_key = self.env_credentials.api_key(provider_id);
        let env_candidate = self.environment_candidate(provider_id);

        // 2. Prime-inference: environment before stored.
        if provider_id == PRIME_INFERENCE_PROVIDER_ID {
            if let (Some(api_key), Some(candidate)) = (env_key.clone(), env_candidate.clone()) {
                if !self.is_stale(provider_id, &candidate) {
                    return AuthApiKeyResult {
                        api_key: Some(api_key),
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: Some("api_key"),
                        oauth_refresh_failed: false,
                        headers: None,
                    };
                }
            }
        }

        // 2b. Prime-inference: the saved context the session directory
        // selects. Its key replaces the stored login there (the stored key
        // is another account's), so a stale context key skips stored too.
        let directory_candidate = self.directory_context_candidate(provider_id);
        if let Some(candidate) = &directory_candidate {
            if !self.is_stale(provider_id, candidate) {
                return AuthApiKeyResult {
                    api_key: self.directory_context_api_key(),
                    source_token: Self::token_for(provider_id, candidate),
                    credential_type: Some("api_key"),
                    oauth_refresh_failed: false,
                    headers: None,
                };
            }
        }

        // 3. Stored credential.
        if let Some(credential) = self
            .data
            .credential(provider_id)
            .filter(|_| directory_candidate.is_none())
        {
            if let Some(candidate) =
                self.stored_candidate(provider_id, /*resolved_command_value*/ None)
            {
                if !self.is_stale(provider_id, &candidate) {
                    match &credential {
                        AuthCredential::ApiKey { key, .. } => {
                            let has_stale_record =
                                !self.matching_stale(provider_id, &candidate).is_empty();
                            let api_key = if key.starts_with('!') && has_stale_record {
                                resolve_config_value_uncached(key)
                            } else {
                                resolve_config_value(key)
                            };
                            // The source token reuses the value this lookup
                            // just resolved (TS's `resolvedCommandValue`):
                            // a command key resolves once per lookup, never
                            // again through the candidate's lazy fingerprint.
                            // A FAILED resolution produced no value: the
                            // deferred fingerprint is dropped too — the
                            // source token is None either way, and a failed
                            // command still retries once per lookup (TS
                            // #2497), not twice.
                            let candidate = api_key
                                .as_deref()
                                .and_then(|resolved| {
                                    self.stored_candidate(provider_id, Some(resolved))
                                })
                                .unwrap_or_else(|| AuthSourceCandidate {
                                    resolve_value_fingerprint: None,
                                    ..candidate.clone()
                                });
                            return AuthApiKeyResult {
                                api_key,
                                source_token: Self::token_for(provider_id, &candidate),
                                credential_type: Some("api_key"),
                                oauth_refresh_failed: false,
                                headers: None,
                            };
                        }
                        AuthCredential::Oauth { expires, .. } => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(i64::MAX, |d| d.as_millis() as i64);
                            if now_ms >= *expires {
                                if let Some(refreshed) = self.refresh_oauth(provider_id) {
                                    let candidate = self.stored_candidate(
                                        provider_id,
                                        /*resolved_command_value*/ None,
                                    );
                                    return AuthApiKeyResult {
                                        api_key: self.oauth.api_key_for(provider_id, &refreshed),
                                        source_token: candidate
                                            .and_then(|c| Self::token_for(provider_id, &c)),
                                        credential_type: Some("oauth"),
                                        oauth_refresh_failed: false,
                                        headers: None,
                                    };
                                }
                                // Refresh failed: keep credentials for a
                                // later retry; the caller reports the
                                // failed refresh, not a missing key.
                                return AuthApiKeyResult {
                                    credential_type: Some("oauth"),
                                    oauth_refresh_failed: true,
                                    headers: None,
                                    ..AuthApiKeyResult::default()
                                };
                            }
                            return AuthApiKeyResult {
                                api_key: self.oauth.api_key_for(provider_id, &credential),
                                source_token: Self::token_for(provider_id, &candidate),
                                credential_type: Some("oauth"),
                                oauth_refresh_failed: false,
                                headers: None,
                            };
                        }
                        // A pasted MCP static token IS the api key for its
                        // `mcp:<server>` provider: the bearer value, used verbatim.
                        AuthCredential::McpStaticToken { bearer, .. } => {
                            return AuthApiKeyResult {
                                api_key: Some(bearer.clone()),
                                source_token: Self::token_for(provider_id, &candidate),
                                credential_type: Some("mcp_static_token"),
                                oauth_refresh_failed: false,
                                headers: None,
                            };
                        }
                    }
                }
            }
        }

        // 4. Environment for non-prime-inference providers.
        if provider_id != PRIME_INFERENCE_PROVIDER_ID {
            if let (Some(api_key), Some(candidate)) = (env_key, env_candidate) {
                if !self.is_stale(provider_id, &candidate) {
                    return AuthApiKeyResult {
                        api_key: Some(api_key),
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: None,
                        oauth_refresh_failed: false,
                        headers: None,
                    };
                }
            }
        }

        // 5. Fallback resolver.
        if include_fallback {
            if let Some(candidate) = self.fallback_candidate(provider_id) {
                if !self.is_stale(provider_id, &candidate) {
                    let api_key = self
                        .fallback_resolver
                        .as_ref()
                        .and_then(|resolver| resolver(provider_id));
                    return AuthApiKeyResult {
                        api_key,
                        source_token: Self::token_for(provider_id, &candidate),
                        credential_type: None,
                        oauth_refresh_failed: false,
                        headers: None,
                    };
                }
            }
        }

        AuthApiKeyResult::default()
    }

    pub fn get_api_key(&mut self, provider_id: &str) -> Option<String> {
        self.get_api_key_with_source_token(provider_id, true)
            .api_key
    }

    /// The stored OAuth credential when it is fresh — a peer's completed
    /// refresh or a concurrent `/login` — else `None` (the expired
    /// credential stays stored for a later retry).
    fn stored_unexpired_credential(&self, provider_id: &str) -> Option<AuthCredential> {
        self.data.credential(provider_id).filter(|credential| {
            matches!(
                credential,
                AuthCredential::Oauth { expires, .. } if now_epoch_ms() < *expires
            )
        })
    }

    /// Refresh an expired OAuth credential, returning the new credential on success.
    ///
    /// Load-then-lock shape: the token fetch never runs under the document lock
    /// (TS refreshes inside `withLockAsync`; its single-threaded runtime pays nothing,
    /// but a fetch under this port's lock would stall every same-process auth read
    /// and write). The phases:
    ///
    /// 1. LOAD: the document through the read arm (no document lock).
    /// 2. CLAIM: [`refresh_flight`]'s in-process gate, then the store's cross-process
    ///    claim ([`crate::auth::AuthStorageBackend::claim_refresh`]). Both stay held through WRITE:
    ///    a waiter admitted before the new credential is written would re-check, still
    ///    read the expired one, and spend the single-use refresh token a second time.
    /// 3. FETCH: the expiry re-checked under the claim (a released holder wrote a fresh
    ///    credential; a failed re-read fails closed), then the token call outside the
    ///    document lock.
    /// 4. WRITE: the locked read-modify-write, holding the lock only for the re-read,
    ///    insert, and atomic write. A peer that refreshed meanwhile keeps its fresher
    ///    credential, and the commit lands only while the document still holds the
    ///    entry this refresh rotated: a logout or a new login that landed during the
    ///    fetch is never overwritten or resurrected by the fetched grant. A failed write still returns the fetched credential: the
    ///    provider rotated the refresh token, so the stored one is dead. The backend
    ///    keeps it (taking over the claim) until a retried save lands it, and every
    ///    phase above reads the document with kept refreshes laid over it.
    fn refresh_oauth(&mut self, provider_id: &str) -> Option<AuthCredential> {
        // LOAD: no document lock.
        let Ok(content) = self.storage.read() else {
            // A failed read: reload, then serve only a CURRENT credential.
            // A reload that fails too keeps the last read's data (TS
            // `reload` catches the error and leaves `this.data`), and that
            // in-memory copy may be the very credential this refresh set
            // out to rotate — expired. Serving it would stream a known
            // stale token; TS's catch path returns nothing and lets the
            // environment/fallback arms serve instead.
            self.reload();
            return self.stored_unexpired_credential(provider_id);
        };
        let Ok(mut data) = parse_storage_data(content.as_deref()) else {
            self.reload();
            return self.stored_unexpired_credential(provider_id);
        };
        self.overlay_unsaved_refreshes(&mut data);
        let Some(credential) = data.credential(provider_id) else {
            self.reload();
            return None;
        };
        let AuthCredential::Oauth { expires, .. } = &credential else {
            self.reload();
            return None;
        };
        if now_epoch_ms() < *expires {
            self.reload();
            return Some(credential);
        }
        // CLAIM: one flight per provider in this process, then across every
        // process sharing the store; both held until this function returns.
        let _flight = refresh_flight(provider_id);
        let claim = match self.storage.claim_refresh(provider_id) {
            Ok(claim) => claim,
            Err(error) => {
                // An uncertain claim never spends a refresh token: the stored
                // credential stays for a later retry.
                tracing::warn!(
                    provider = provider_id,
                    error = %error,
                    "the OAuth refresh claim was not granted"
                );
                self.reload();
                return self.stored_unexpired_credential(provider_id);
            }
        };
        // FETCH: the previous holder may have just written a fresh credential;
        // re-check before spending a refresh token, and spend the one stored now.
        // A failed re-read fails closed: the LOAD-time snapshot may be stale (a
        // peer can have rotated the single-use token while this waited for the
        // claim), so no fetch runs on it; only a current unexpired credential
        // serves.
        let Some(document) = self
            .storage
            .read()
            .ok()
            .and_then(|content| parse_storage_data(content.as_deref()).ok())
        else {
            self.reload();
            return self.stored_unexpired_credential(provider_id);
        };
        // The document entry this refresh rotates: the commit lands only while
        // the document still holds it, so a logout or a new login that lands
        // while the (possibly aborted, detached) fetch runs stays authoritative.
        let rotating = document.credential(provider_id);
        let mut data = document;
        self.overlay_unsaved_refreshes(&mut data);
        if let Some(credential) = data.credential(provider_id).filter(|credential| {
            matches!(
                credential,
                AuthCredential::Oauth { expires, .. } if now_epoch_ms() < *expires
            )
        }) {
            self.reload();
            return Some(credential);
        }
        // The login left the document while this waited for the claim (an
        // installed credential source took custody of it, or a logout):
        // there is nothing here to spend.
        if !matches!(
            data.credential(provider_id),
            Some(AuthCredential::Oauth { .. })
        ) {
            self.reload();
            return None;
        }
        // A refresh token the endpoint revoked is never presented again.
        if refresh_token_revoked(provider_id, &data) {
            self.reload();
            return None;
        }
        let new_credential = match self.oauth.refresh(provider_id, &data) {
            Ok(credential) => credential,
            Err(error) => {
                if error == OAuthRefreshError::Revoked {
                    remember_revoked_refresh_token(provider_id, &data);
                }
                // Refresh failed: keep credentials for a later retry (a
                // new login, when revoked); a peer or a concurrent `/login`
                // may have landed a fresh credential, so reload and serve it
                // before failing.
                self.reload();
                return self.stored_unexpired_credential(provider_id);
            }
        };
        // WRITE: the locked read-modify-write, guarded by the rotated entry's
        // identity: the current entry decides whether the fetched grant lands
        // at all.
        let mut refreshed: Option<AuthCredential> = None;
        let result = self.storage.with_lock(&mut |current| {
            let mut data = parse_storage_data(current.as_deref())?;
            let current_credential = data.credential(provider_id);
            let still_original = current_credential == rotating;
            if let Some(credential) = current_credential.filter(|credential| {
                matches!(
                    credential,
                    AuthCredential::Oauth { expires, .. } if now_epoch_ms() < *expires
                )
            }) {
                // A peer refreshed or a concurrent `/login` landed a
                // fresher credential while this fetch ran: it stands and
                // this attempt writes nothing, serving it instead.
                refreshed = Some(credential);
                return Ok(((), None));
            }
            if !still_original {
                // The credential changed while this fetch ran — a logout
                // removed the grant, a new login replaced it: the current
                // entry is authoritative and the fetched grant is
                // discarded, never resurrected over it.
                return Ok(((), None));
            }
            data.insert(provider_id, &new_credential);
            let content = serde_json::to_string_pretty(&data.0)?;
            refreshed = Some(new_credential.clone());
            Ok(((), Some(content)))
        });
        if let Err(error) = result {
            self.keep_unsaved_refresh(provider_id, &new_credential, &error, claim);
            self.reload();
            return Some(new_credential);
        }
        // The document holds a live credential now: a released claim's
        // waiters re-check and read it, and a stale recovery file can be
        // forgotten under the claim during the reload.
        drop(claim);
        // Reload from what we wrote: the in-memory snapshot must not serve the
        // pre-refresh credential (a rotated refresh token is single-use).
        self.reload();
        refreshed
    }
}
