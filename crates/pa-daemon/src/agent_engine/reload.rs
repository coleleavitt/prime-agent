//! The session's live-input reload (`/reload`'s daemon half): re-read the
//! request auth from the credential store and the MCP manager's settings
//! and auth state — the pieces another process (a `/login`, a `/mcp` key
//! flow) can change under a running session.
use pa_core::session_engine::provider_adapter::ProviderTarget;
use pa_types::sync::RwLockExt;

use super::AgentSessionEngine;

impl AgentSessionEngine {
    /// The stored request auth for the target's OWN model, applied in
    /// place: only the key and headers change, and only when the
    /// resolution serves a live credential for the model's provider —
    /// stored, or the ambient environment/runtime override (a logout
    /// that empties the store leaves the session on the env key, like a
    /// fresh resolution). The configured `models.json` fallback key —
    /// what the resolution serves when the store is unreadable OR
    /// resolves no credential from any live source — is not a credential
    /// and never replaces what the target already serves with. A
    /// resolved credential REPLACES the pair: its headers land even when
    /// it carries none, so a rotated credential without a team header
    /// clears the previous credential's `X-Prime-Team-ID` instead of
    /// serving it on the new key.
    fn refresh_request_auth(&self, target: &mut ProviderTarget) {
        let (api_key, headers, store_auth) =
            self.resolve_request_key_and_headers_and_store_health(&target.model);
        if store_auth {
            if let Some(api_key) = api_key {
                target.api_key = Some(api_key);
                target.headers = headers;
            }
        }
    }

    /// `/reload`'s session half: re-read the live inputs another process
    /// may have changed — the request auth from the credential store
    /// (rebinding the live target and an armed image route's pair in
    /// place: the model and service tier never change), and the MCP
    /// manager's settings and auth state. A rebind lands only off live
    /// STORE auth (stored, or the ambient environment/runtime override);
    /// the configured `models.json` fallback key never replaces a
    /// last-good pair. A failed auth-store read fails the reload before
    /// any target is touched.
    pub(crate) fn reload_live_inputs(&self) -> Result<(), String> {
        // The whole refresh is serialized against every other
        // resolve-and-install (the session's reload lock): overlapping
        // writers resolve the store at their own read times, so the last
        // writer to run must be the one leaving the newest store.
        let _serialized = self
            .reload_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The auth store re-read gates the rebinds below: the settings
        // re-read still applies on a failed reload.
        {
            let mut manager = self
                .mcp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let auth_reload = manager.reload_auth_storage();
            manager.refresh();
            auth_reload?;
        }
        // The armed route refreshes FIRST, the live slot SECOND: the
        // route lock fences the route's clone-and-install against this
        // refresh order (an attempt either installs the refreshed route
        // or lands before the slot refresh — never over it), and the
        // route's saved session target is the fallback a later
        // `clear_image_route` restores when resolution fails, so its auth
        // re-binds from the same store read.
        if let Some(route) = self
            .image_route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            self.refresh_request_auth(&mut route.target);
            if let Some(saved) = route.session_target.as_mut() {
                self.refresh_request_auth(saved);
            }
        }
        let live_model = self
            .provider_target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|target| target.model.clone());
        if let Some(model) = live_model {
            let (api_key, headers, store_auth) =
                self.resolve_request_key_and_headers_and_store_health(&model);
            if store_auth {
                let mut slot = self
                    .provider_target
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(target) = slot.as_mut() {
                    // The model may have moved under the store read: a
                    // switch or route swap installed a new target. The
                    // auth then re-resolves for whatever is live NOW,
                    // under the same gate; a cleared slot stays cleared.
                    let (api_key, headers) = if target.model == model {
                        (api_key, headers)
                    } else {
                        match self.resolve_request_key_and_headers_and_store_health(&target.model) {
                            (Some(key), headers, true) => (Some(key), headers),
                            // The moved-to model's provider has no live
                            // stored credential: the target keeps its
                            // last-good pair.
                            _ => (None, None),
                        }
                    };
                    // A resolved credential replaces the pair: headers
                    // clear when the fresh credential carries none.
                    if let Some(api_key) = api_key {
                        target.api_key = Some(api_key);
                        target.headers = headers;
                    }
                }
            }
        }
        // Re-scan the skill roots so a skill installed since the session
        // started is loadable without a restart (Claude Code's
        // `invalidateSkillDirs` re-aggregate for the in-session case).
        self.reload_skills();
        Ok(())
    }

    /// Re-scan the session's skill roots and adopt any newly installed skills
    /// into the running engine's overlay, so `rlm.load_skill` and `/skill:`
    /// resolve a skill added since the session started. Additive and
    /// best-effort: a re-scan failure or an engine not yet built leaves the
    /// loaded set unchanged. The model's prompt inventory still refreshes only
    /// on a new session, and a newly installed skill's kernel Python import (if
    /// any) likewise needs one.
    fn reload_skills(&self) {
        let Some(engine) = self.session.blocking_lock().clone() else {
            return;
        };
        let create = self.create_resources.read_or_recover().clone();
        let additional_directories = create
            .additional_directories
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        let loaded =
            pa_core::resources::load_resources(pa_core::resources::ResourceLoaderOptions {
                cwd: self.cwd(),
                agent_dir: self.config.agent_dir.clone(),
                additional_directories,
                additional_skill_paths: create.skills.clone(),
                no_skills: create.resource_exclusions.no_skills,
                ..Default::default()
            });
        let fresh = match loaded {
            Ok(resources) => resources.skills,
            Err(error) => {
                tracing::warn!(%error, "reload could not re-scan skills; keeping the loaded set");
                return;
            }
        };
        let added = engine.session.adopt_reloaded_skills(fresh);
        if added.is_empty() {
            tracing::debug!("reload re-scanned the skill roots; none new to the session");
            return;
        }
        let names: Vec<&str> = added.iter().map(|skill| skill.name.as_str()).collect();
        tracing::info!(
            count = added.len(),
            skills = ?names,
            "reload adopted newly installed skills; loadable without a restart"
        );
    }
}
