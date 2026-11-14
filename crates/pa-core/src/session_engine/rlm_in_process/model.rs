//! Child model resolution over the model registry: the TS
//! `_resolveRlmSubagentModel` half every host shares — the daemon's
//! supervisor host (which adds its allowlist gate on the selector), the
//! daemonless print host, and this in-process host (no daemon allowlist:
//! the resident guest owns its model surface) — plus the spawn-time
//! thinking-support check and the requested child cwd.

use std::path::{Path, PathBuf};

use pa_types::ai::{Model, get_supported_thinking_levels, thinking_level_from_str};

use crate::kernel::rlm_runtime::{RlmModelInfo, find_rlm_model_matches};
use crate::models::registry::ModelRegistry;

/// Close matches listed in model-resolution errors (TS suggestion limit).
const MODEL_ERROR_SUGGESTION_LIMIT: usize = 3;

/// A resolved child model: the selector the roster reports and the model
/// the child engine runs.
#[derive(Debug, Clone)]
pub struct ResolvedChildModel {
    pub selector: String,
    pub model: pa_agent::types::Model,
}

/// The searchable catalog as `rlm.find_models` sees it.
fn catalog(registry: &ModelRegistry) -> Vec<&Model> {
    registry.get_rlm_searchable_models()
}

/// The `provider/id` info list model-resolution errors suggest from.
fn catalog_infos(registry: &ModelRegistry) -> Vec<RlmModelInfo> {
    catalog(registry)
        .into_iter()
        .map(|model| RlmModelInfo {
            provider: model.provider.clone(),
            id: model.id.clone(),
            name: if model.name.is_empty() {
                model.id.clone()
            } else {
                model.name.clone()
            },
        })
        .collect()
}

/// Resolve the child model reference (TS `_resolveRlmSubagentModel`):
/// `None` inherits the parent's live model, a reference resolves exactly
/// like the TS — parent equality first, then an exact catalog selector,
/// then a unique short-form match, then a parent short form — else the
/// TS unavailable-model error with close matches.
///
/// # Errors
///
/// Returns an error when no parent model is bound and no reference was
/// supplied, when the reference matches no catalog model, or when the
/// resolved model fails the wire-shape conversion.
pub fn resolve_child_model(
    registry: &ModelRegistry,
    reference: Option<&str>,
    parent_model: Option<&pa_agent::types::Model>,
    target: &str,
) -> anyhow::Result<ResolvedChildModel> {
    let Some(reference) = reference
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
    else {
        return parent_model
            .cloned()
            .map(|model| ResolvedChildModel {
                selector: format!("{}/{}", model.provider, model.id),
                model,
            })
            .ok_or_else(|| anyhow::anyhow!("No model selected. Use /model to pick one."));
    };
    let normalized = reference.to_lowercase();
    if let Some(parent) = parent_model
        && format!("{}/{}", parent.provider, parent.id).to_lowercase() == normalized
    {
        return Ok(ResolvedChildModel {
            selector: format!("{}/{}", parent.provider, parent.id),
            model: parent.clone(),
        });
    }
    let catalog = catalog(registry);
    let selector_of = |model: &Model| format!("{}/{}", model.provider, model.id);
    if let Some(exact) = catalog
        .iter()
        .copied()
        .find(|model| selector_of(model).to_lowercase() == normalized)
    {
        return agent_model_of(exact, target);
    }
    // Short form: the full selector ends with "/<reference>".
    let short_matches: Vec<&Model> = catalog
        .iter()
        .copied()
        .filter(|model| {
            selector_of(model)
                .to_lowercase()
                .ends_with(&format!("/{normalized}"))
        })
        .collect();
    match short_matches.len() {
        1 => agent_model_of(short_matches[0], target),
        0 => parent_model
            .filter(|parent| {
                format!("{}/{}", parent.provider, parent.id)
                    .to_lowercase()
                    .ends_with(&format!("/{normalized}"))
            })
            .map(|parent| ResolvedChildModel {
                selector: format!("{}/{}", parent.provider, parent.id),
                model: parent.clone(),
            })
            .ok_or_else(|| model_unavailable_error(reference, target, &catalog_infos(registry))),
        _ => Err(model_unavailable_error(
            reference,
            target,
            &catalog_infos(registry),
        )),
    }
}

/// Resolve the child model reference to a `provider/id` selector: `None`
/// inherits the parent selector; a reference resolves parent equality
/// first, then an exact catalog selector, then a unique short-form match,
/// then a parent short form — else the unavailable-model error with close
/// matches. The daemon gates the result on its `allowedModels` allowlist;
/// the daemonless print host does not.
///
/// # Errors
///
/// Returns an error when no parent model is bound and no reference was
/// supplied, or when the reference matches no catalog model.
pub fn resolve_child_selector(
    registry: &ModelRegistry,
    reference: Option<&str>,
    parent_model: Option<&str>,
    target: &str,
) -> anyhow::Result<String> {
    let Some(reference) = reference else {
        return parent_model
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("No model selected. Use /model to pick one."));
    };
    let reference = reference.trim();
    let normalized = reference.to_lowercase();
    if let Some(parent) = parent_model
        && parent.to_lowercase() == normalized
    {
        return Ok(parent.to_string());
    }
    let candidates = catalog_infos(registry);
    let selector_of = |model: &RlmModelInfo| format!("{}/{}", model.provider, model.id);
    if let Some(exact) = candidates
        .iter()
        .find(|model| selector_of(model).to_lowercase() == normalized)
    {
        return Ok(selector_of(exact));
    }
    // Short form: the full selector ends with "/<reference>".
    let short_matches: Vec<&RlmModelInfo> = candidates
        .iter()
        .filter(|model| {
            selector_of(model)
                .to_lowercase()
                .ends_with(&format!("/{normalized}"))
        })
        .collect();
    match short_matches.len() {
        1 => return Ok(selector_of(short_matches[0])),
        0 => {
            if let Some(parent) = parent_model
                && parent.to_lowercase().ends_with(&format!("/{normalized}"))
            {
                return Ok(parent.to_string());
            }
        }
        _ => {}
    }
    Err(model_unavailable_error(reference, target, &candidates))
}

/// The directory a child starts in: a requested `cwd` resolves against the
/// session cwd and is canonicalized (project-context discovery walks its
/// ancestors), otherwise the child shares the session cwd.
///
/// # Errors
///
/// Returns an error naming `operation` when the requested directory does
/// not exist.
pub fn resolve_child_cwd(
    session_cwd: &Path,
    requested: Option<&str>,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    let Some(requested) = requested else {
        return Ok(session_cwd.to_path_buf());
    };
    let joined = session_cwd.join(requested);
    match joined.canonicalize() {
        Ok(cwd) if cwd.is_dir() => Ok(cwd),
        _ => anyhow::bail!("{operation} cwd {} is not a directory", joined.display()),
    }
}

/// The catalog model in the agent loop's shape.
fn agent_model_of(model: &Model, target: &str) -> anyhow::Result<ResolvedChildModel> {
    let selector = format!("{}/{}", model.provider, model.id);
    let agent_model = super::super::provider_adapter::json_round_trip(model).ok_or_else(|| {
        anyhow::anyhow!("Requested {target} model \"{selector}\" failed the wire-shape conversion")
    })?;
    Ok(ResolvedChildModel {
        selector,
        model: agent_model,
    })
}

/// Rejection message for an unresolved model reference (TS
/// `formatRlmModelUnavailableError`): the unavailability, the selector
/// form, and close matches so the caller can retry with a full selector.
fn model_unavailable_error(
    reference: &str,
    target: &str,
    catalog: &[RlmModelInfo],
) -> anyhow::Error {
    let base = format!(
        "Requested {target} model \"{reference}\" is unavailable, unauthenticated, or expired"
    );
    let hint =
        "selectors use the form \"provider/model-id\" (e.g. \"prime-inference/z-ai/glm-5.3\")";
    let close_matches = find_rlm_model_matches(reference, catalog, MODEL_ERROR_SUGGESTION_LIMIT);
    if close_matches.is_empty() {
        anyhow::anyhow!("{base}; {hint}")
    } else {
        let selectors = close_matches
            .iter()
            .map(|match_| format!("\"{}\"", match_.selector))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("{base}; {hint}; close matches: {selectors}")
    }
}

/// A requested thinking level must be supported by the resolved model
/// (the TS spawn-time check). A model outside the local catalog cannot be
/// checked and passes.
///
/// # Errors
///
/// Returns an error when the resolved model's supported levels do not
/// include the requested level (the message lists the supported levels).
pub fn assert_thinking_supported(
    registry: &ModelRegistry,
    level: Option<&str>,
    selector: &str,
) -> anyhow::Result<()> {
    let Some(level) = level else {
        return Ok(());
    };
    let Some((provider, id)) = selector.split_once('/') else {
        return Ok(());
    };
    let Some(model) = registry
        .get_rlm_searchable_models()
        .into_iter()
        .find(|model| model.provider == provider && model.id == id)
    else {
        return Ok(());
    };
    let supported = get_supported_thinking_levels(model);
    let Some(requested) = thinking_level_from_str(level) else {
        return Ok(());
    };
    if supported.contains(&requested) {
        return Ok(());
    }
    let levels = supported
        .iter()
        .map(|level| level.wire_name())
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(
        "Requested thinking level \"{level}\" is not supported by model \"{selector}\"; supported levels: {levels}"
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The worker-path catalog: auth.json, models.json, and the private
    /// authorization cache under one agent dir.
    fn catalog_registry(dir: &Path) -> ModelRegistry {
        let auth = crate::auth::AuthStorage::create(dir);
        let mut registry = ModelRegistry::create(auth, dir.join("models.json"));
        registry.load_private_authorization_from_cache();
        registry
    }

    /// A models.json custom provider, like the pa-core registry tests.
    fn write_catalog(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("models.json"),
            json!({
                "providers": {
                    "test-provider": {
                        "baseUrl": "http://localhost:9",
                        "apiKey": "test-key",
                        "api": "openai-completions",
                        "models": [
                            { "id": "glm-5.3", "name": "GLM 5.3", "contextWindow": 1000, "maxTokens": 100 },
                            { "id": "glm-5.3-turbo", "name": "GLM Turbo", "contextWindow": 1000, "maxTokens": 100 }
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn resolves_parent_exact_and_short_form_references() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            None,
            Some("test-provider/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3");
        // Parent equality short-circuits even a catalog refresh miss.
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("Test-Provider/GLM-5.3"),
            Some("test-provider/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3");
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("test-provider/glm-5.3-turbo"),
            Some("test-provider/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3-turbo");
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("glm-5.3-turbo"),
            Some("test-provider/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3-turbo");
        let error = resolve_child_selector(&catalog_registry(dir.path()), None, None, "subagent")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "No model selected. Use /model to pick one."
        );
    }

    #[test]
    fn unmatched_references_carry_the_ts_error_with_close_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        // TS parity (4649 regression suite): a prefix reference lists
        // close matches, a matching-nothing reference does not (the
        // reference prefixes the test provider, never real defaults).
        let error = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("test-provi"),
            Some("test-provider/glm-5.3"),
            "subagent",
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with(
                "Requested subagent model \"test-provi\" is unavailable, unauthenticated, or expired; selectors use the form \"provider/model-id\""
            ),
            "{message}"
        );
        // Close matches list full selectors, nearest first.
        assert!(
            message.contains("close matches: \"test-provider/glm-5.3\""),
            "{message}"
        );
        let error = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("zzz"),
            Some("test-provider/glm-5.3"),
            "subagent",
        )
        .unwrap_err();
        assert!(!error.to_string().contains("close matches:"), "{error}");
    }

    #[test]
    fn thinking_support_follows_the_resolved_model() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        // The custom catalog model is non-reasoning: only "off" is supported.
        assert_thinking_supported(
            &catalog_registry(dir.path()),
            Some("off"),
            "test-provider/glm-5.3",
        )
        .unwrap();
        let error = assert_thinking_supported(
            &catalog_registry(dir.path()),
            Some("high"),
            "test-provider/glm-5.3",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Requested thinking level \"high\" is not supported by model \"test-provider/glm-5.3\"; supported levels: off"
        );
        // A model outside the catalog (scripted verification models) passes.
        assert_thinking_supported(
            &catalog_registry(dir.path()),
            Some("high"),
            "scripted/faux-1",
        )
        .unwrap();
    }

    /// The child-propagation regression: a spawned child resolves an
    /// entitled private `internal/*` model through the worker's same disk caches.
    #[test]
    fn a_spawned_child_resolves_an_entitled_private_model() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        // The worker-path auth (auth.json): one Prime Inference key+team.
        std::fs::write(
            dir.path().join("auth.json"),
            json!({
                "prime-inference": {
                    "type": "api_key",
                    "key": "child-key",
                    "primeTeam": { "teamId": "team-7", "name": "Team 7" }
                }
            })
            .to_string(),
        )
        .unwrap();
        // The authorized private model: the compiled fallback lacks it, the
        // fingerprint-scoped disk cache carries it.
        let mut auth = crate::auth::AuthStorage::create(dir.path());
        let api_key = auth.get_api_key("prime-inference").expect("api key");
        // The stored team selection survives ambient env credentials (a
        // dogfood box's ambient `PRIME_API_KEY` changes the pair); the
        // cache below is written for whatever pair resolves.
        let team_id = auth
            .get_provider_headers("prime-inference")
            .and_then(|headers| headers.get("X-Prime-Team-ID").cloned())
            .expect("stored team selection");
        let fingerprint =
            crate::models::private_prime_authorization_fingerprint(&api_key, &team_id);
        crate::models::write_private_prime_authorization_cache(
            &dir.path().join("models.json"),
            &crate::models::PrivatePrimeAuthorizationCache {
                fingerprint,
                // The durable wire items (an `/models` payload shape): the
                // read path parses and builds the model from them.
                items: vec![json!({
                    "id": "internal/glm-5.3-fast",
                    "display_name": "GLM 5.3 Fast",
                    "pricing": {
                        "input_usd_per_mtok": 0.42, "output_usd_per_mtok": 2.1,
                        "cache_read_usd_per_mtok": 0.0, "cache_write_usd_per_mtok": 0.0,
                    },
                    "specs": {
                        "context_window": 400_000, "max_output_tokens": 131_072,
                        "supports_reasoning": true,
                        "modalities": { "input": ["text"], "output": ["text"] },
                    },
                })],
                models: Vec::new(),
                refreshed_at: 1,
            },
        );

        assert!(
            catalog_infos(&catalog_registry(dir.path()))
                .iter()
                .any(|model| model.id == "internal/glm-5.3-fast")
        );
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("prime-inference/internal/glm-5.3-fast"),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, "prime-inference/internal/glm-5.3-fast");
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some("internal/glm-5.3-fast"),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, "prime-inference/internal/glm-5.3-fast");
        // The spawn-time thinking check follows the resolved entitlement.
        assert_thinking_supported(
            &catalog_registry(dir.path()),
            Some("high"),
            "prime-inference/internal/glm-5.3-fast",
        )
        .unwrap();
        assert_thinking_supported(
            &catalog_registry(dir.path()),
            Some("off"),
            "prime-inference/internal/glm-5.3-fast",
        )
        .unwrap();
    }

    /// The catalog-repo child-resolution regression: a fetched entry the compiled fallback lacks
    /// resolves through the on-disk provider catalog, once its provider is auth-configured.
    #[test]
    fn a_spawned_child_resolves_a_catalog_repo_entry_the_compiled_fallback_lacks() {
        const PROBE_ID: &str = "gpt-6-probe";
        assert!(
            !pa_ai::models_generated::get_models("openai-codex")
                .iter()
                .any(|model| model.id == PROBE_ID),
            "the probe entry is compiled in; pick another id"
        );
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        // The provider auth the resolution filters on: a models.json
        // provider entry (headers alone satisfy the config validation and
        // has_configured_auth).
        std::fs::write(
            dir.path().join("models.json"),
            json!({
                "providers": {
                    "openai-codex": {
                        "apiKey": "codex-key",
                        "headers": { "X-Probe": "catalog-chain" }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        // The layer-A disk cache the startup refresh writes beside
        // models.json: one catalog-repo entry riding the compiled
        // openai-codex transport tuple (the pinning invariant).
        std::fs::write(
            dir.path()
                .join("models")
                .join("provider-model-catalog.v1.json"),
            json!({
                "url": pa_models::fetch::MODEL_CATALOG_URL,
                "scope": pa_models::cache::PUBLIC_SCOPE,
                "fetchedAt": 1,
                "payload": { "schemaVersion": 1, "models": [
                    {
                        "id": PROBE_ID, "name": "GPT-6 Probe",
                        "api": "openai-codex-responses", "provider": "openai-codex",
                        "baseUrl": "https://chatgpt.com/backend-api",
                        "reasoning": true,
                        "thinkingLevelMap": { "minimal": null, "xhigh": "xhigh", "max": "max" },
                        "input": ["text"],
                        "cost": { "input": 2, "output": 10, "cacheRead": 0.2, "cacheWrite": 2.5 },
                        "contextWindow": 272_000, "maxTokens": 128_000,
                    }
                ]}
            })
            .to_string(),
        )
        .unwrap();
        let selector = format!("openai-codex/{PROBE_ID}");
        let catalog = catalog_infos(&catalog_registry(dir.path()));
        let probe = catalog
            .iter()
            .find(|model| model.selector() == selector)
            .expect("the fetched entry lists for find_models");
        assert_eq!(probe.name, "GPT-6 Probe");
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some(&selector),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, selector);
        let resolved = resolve_child_selector(
            &catalog_registry(dir.path()),
            Some(PROBE_ID),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
        )
        .unwrap();
        assert_eq!(resolved, selector);
        // The spawn-time thinking check follows the fetched entry's map:
        // xhigh is explicitly mapped, minimal is explicitly nulled out.
        assert_thinking_supported(&catalog_registry(dir.path()), Some("xhigh"), &selector).unwrap();
        let error =
            assert_thinking_supported(&catalog_registry(dir.path()), Some("minimal"), &selector)
                .unwrap_err();
        assert!(
            error.to_string().contains("not supported by model"),
            "{error}"
        );
    }
}
