//! The headless modes' session model: the print and RPC root selects its
//! model from the CLI flags, a resumed session, and the settings defaults,
//! and its thinking level from the flag and the settings default.

use std::path::PathBuf;

use pa_types::ai::Model;

use crate::models::ModelRegistry;
use crate::session::SessionContext;

/// The invocation's model flags and the directories its settings live in.
#[derive(Debug, Clone, Default)]
pub struct HeadlessModelFlags {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// `--provider`.
    pub provider: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// `--thinking`.
    pub thinking: Option<pa_types::ai::ModelThinkingLevel>,
}

/// Select the headless session model: an explicit `--model` (with
/// `--provider`) through the CLI resolver, else a resumed session's own
/// model, else the saved settings default (never a synthesized fallback),
/// else the preferred available model.
///
/// # Errors
///
/// Returns the user-facing selection failure: an unresolvable flag, an
/// unavailable saved default, or no authenticated model at all.
pub async fn select_headless_model(
    registry: &mut ModelRegistry,
    config: &HeadlessModelFlags,
    session: Option<&SessionContext>,
) -> Result<Model, String> {
    // Explicit selectors retain the CLI resolver's provider inference and do
    // not inherit an unrelated saved provider.
    if let Some(model_name) = config.model.as_deref() {
        let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
        let resolved =
            crate::models::resolve_cli_model(config.provider.as_deref(), model_name, &available);
        if let Some(error) = resolved.error {
            return Err(error);
        }
        return resolved
            .model
            .ok_or_else(|| "No matching model found.".to_string());
    }
    // A resumed session with messages restores its own model before the
    // defaults chain (TS `createAgentSession`), through the daemon restore's
    // bounded catalog-readiness wait; a miss falls through on the record.
    if let Some((provider, model_id)) = session
        .filter(|context| !context.messages.is_empty())
        .and_then(|context| context.model.as_ref())
    {
        if let Some(model) = crate::models::find_session_model_with_readiness_wait(
            registry,
            provider,
            model_id,
            crate::models::SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
        )
        .await
        {
            return Ok(model);
        }
        eprintln!("Could not restore model {provider}/{model_id}");
    }
    // A missed restore's readiness window may have refreshed the catalog, so
    // the defaults chain resolves against the refreshed view.
    let settings = crate::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
    let saved_provider = settings.get_default_provider();
    let flag_provider = config.provider.as_deref();
    let saved_model = settings.get_default_model().filter(|_| {
        flag_provider.is_none_or(|provider| {
            saved_provider.is_some_and(|saved| provider.eq_ignore_ascii_case(saved))
        })
    });
    if let Some(saved_model) = saved_model {
        // Saved IDs must exist in the authenticated catalog. Do not synthesize
        // a fallback model, especially an unauthorized private Prime model.
        // The lookup is scoped like TS findInitialModel's saved default: the
        // flag, when given, has already matched the saved provider above.
        let scope = flag_provider.or(saved_provider);
        let found = available.iter().find(|model| {
            scope.is_none_or(|provider| model.provider.eq_ignore_ascii_case(provider))
                && (model.id.eq_ignore_ascii_case(saved_model)
                    || format!("{}/{}", model.provider, model.id).eq_ignore_ascii_case(saved_model))
        });
        match found {
            Some(model) => return Ok(model.clone()),
            // Claude Code's `model` (claudeCodeSettings) is a preference,
            // not this agent's configuration: unavailable, the usual
            // default applies.
            None if settings.default_model_from_claude_code() => {}
            None => {
                return Err(format!(
                    "Configured default model \"{saved_model}\" is unavailable. Check settings.json, models.json, and provider authentication."
                ));
            }
        }
    }
    // A settings defaultProvider without a defaultModel does not scope the
    // no-flag choice (TS reads the two keys together); only the flag does.
    let candidates: Vec<Model> = available
        .into_iter()
        .filter(|model| {
            flag_provider.is_none_or(|provider| model.provider.eq_ignore_ascii_case(provider))
        })
        .collect();
    crate::models::find_preferred_default_model(&candidates)
        .or_else(|| candidates.first())
        .cloned()
        .ok_or_else(|| match flag_provider {
            Some(provider) => format!("No available models for provider \"{provider}\". Check models.json and provider authentication."),
            None => "No authenticated models available. Configure a provider or select a model explicitly.".to_string(),
        })
}

/// Resolve the session thinking level: the CLI flag, then the settings
/// default, then "medium" — always clamped to what the model supports.
#[must_use]
pub fn resolve_headless_thinking_level(
    config: &HeadlessModelFlags,
    model: &Model,
) -> pa_agent::types::ThinkingLevel {
    use pa_types::ai::ModelThinkingLevel;
    let settings = crate::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let requested = config
        .thinking
        .or_else(|| {
            settings
                .get_default_thinking_level()
                .map(crate::settings::ThinkingLevelSetting::model_level)
        })
        .unwrap_or(ModelThinkingLevel::Medium);
    let clamped = pa_ai::models::clamp_thinking_level(model, requested);
    crate::session_engine::provider_adapter::map_thinking_level(clamped)
}

#[cfg(test)]
mod tests;
