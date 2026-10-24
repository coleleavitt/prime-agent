//! The side-model session UX switches (Claude Code's setting names where it
//! has one), read from the merged document's top-level keys:
//!
//! ```json
//! { "sessionTitlesEnabled": true, "awaySummaryEnabled": true,
//!   "promptSuggestionEnabled": false, "sideUxModel": "provider/model-id" }
//! ```
//!
//! Defaults follow Claude Code: generated session titles and the away
//! summary are on, prompt suggestions are off (Claude Code gates them behind
//! a server flag that is off by default). An environment variable wins over
//! the document (`1`/`true`/`on` enables, `0`/`false`/`off` disables):
//! [`SESSION_TITLES_ENV`], [`AWAY_SUMMARY_ENV`], [`PROMPT_SUGGESTION_ENV`].

use serde_json::Value;

use super::experimental::env_flag;

/// Generated session titles (Claude Code turns them off with
/// `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`).
pub const SESSION_TITLES_KEY: &str = "sessionTitlesEnabled";
/// The away summary and its focus tracking (Claude Code's key).
pub const AWAY_SUMMARY_KEY: &str = "awaySummaryEnabled";
/// The predicted next prompt (Claude Code's key).
pub const PROMPT_SUGGESTION_KEY: &str = "promptSuggestionEnabled";
/// `provider/model-id` for the session-title and `/rename` calls; unset
/// uses `auxiliaryModel`, then the session's own model.
pub const SIDE_UX_MODEL_KEY: &str = "sideUxModel";

/// Turns generated session titles on or off.
pub const SESSION_TITLES_ENV: &str = "PRIME_AGENT_SESSION_TITLES";
/// Turns the away summary on or off (Claude Code's
/// `CLAUDE_CODE_ENABLE_AWAY_SUMMARY`).
pub const AWAY_SUMMARY_ENV: &str = "PRIME_AGENT_ENABLE_AWAY_SUMMARY";
/// Turns prompt suggestions on or off (Claude Code's
/// `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION`).
pub const PROMPT_SUGGESTION_ENV: &str = "PRIME_AGENT_ENABLE_PROMPT_SUGGESTION";

/// The resolved switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideUxSettings {
    pub session_titles: bool,
    pub away_summary: bool,
    pub prompt_suggestions: bool,
    /// `provider/model-id` the title and name calls use.
    pub model: Option<String>,
}

impl Default for SideUxSettings {
    fn default() -> Self {
        Self {
            // Opt-in (off by default): generating a title continues on the
            // session's own model when no sideUxModel/auxiliaryModel is set,
            // so leaving it on adds a model call to every new session's first
            // prompt. Enable with `sessionTitlesEnabled` / PRIME_AGENT_SESSION_TITLES.
            session_titles: false,
            away_summary: true,
            prompt_suggestions: false,
            model: None,
        }
    }
}

impl SideUxSettings {
    /// Resolve from the merged document's top-level `extra` keys and the
    /// environment (`env` looks a variable up; production passes
    /// `std::env::var`). A value of the wrong type reads as unset.
    #[must_use]
    pub fn resolve(
        extra: &serde_json::Map<String, Value>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let defaults = Self::default();
        let flag = |key: &str, env_name: &str, default: bool| {
            env_flag(env(env_name).as_deref())
                .or_else(|| extra.get(key).and_then(Value::as_bool))
                .unwrap_or(default)
        };
        Self {
            session_titles: flag(
                SESSION_TITLES_KEY,
                SESSION_TITLES_ENV,
                defaults.session_titles,
            ),
            away_summary: flag(AWAY_SUMMARY_KEY, AWAY_SUMMARY_ENV, defaults.away_summary),
            prompt_suggestions: flag(
                PROMPT_SUGGESTION_KEY,
                PROMPT_SUGGESTION_ENV,
                defaults.prompt_suggestions,
            ),
            model: extra
                .get(SIDE_UX_MODEL_KEY)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|selector| !selector.is_empty())
                .map(str::to_string),
        }
    }
}

impl super::manager::SettingsManager {
    /// The side-model UX switches, with the process environment's overrides.
    #[must_use]
    pub fn side_ux(&self) -> SideUxSettings {
        SideUxSettings::resolve(&self.settings().extra, &|name| std::env::var(name).ok())
    }

    /// The selector the session-title and `/rename` calls use:
    /// `sideUxModel`, else `auxiliaryModel`; `None` uses the session model.
    #[must_use]
    pub fn side_ux_model(&self) -> Option<String> {
        self.side_ux().model.or_else(|| {
            self.get_auxiliary_model()
                .map(|selector| selector.trim().to_string())
                .filter(|selector| !selector.is_empty())
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn resolve(document: &Value, env: &[(&str, &str)]) -> SideUxSettings {
        let extra = document.as_object().cloned().unwrap_or_default();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        SideUxSettings::resolve(&extra, &|name| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    #[test]
    fn defaults_are_opt_in_for_titles() {
        assert_eq!(resolve(&json!({}), &[]), SideUxSettings::default());
        // Session titles are off by default (opt-in): leaving them on adds a
        // model call to every new session's first prompt when no dedicated
        // side model is configured. The recap away-summary stays on.
        assert_eq!(
            SideUxSettings::default(),
            SideUxSettings {
                session_titles: false,
                away_summary: true,
                prompt_suggestions: false,
                model: None,
            }
        );
    }

    #[test]
    fn the_document_sets_each_switch_and_the_environment_wins() {
        let document = json!({
            "sessionTitlesEnabled": false,
            "awaySummaryEnabled": false,
            "promptSuggestionEnabled": true,
            "sideUxModel": " anthropic/claude-haiku-4-5 ",
        });
        assert_eq!(
            resolve(&document, &[]),
            SideUxSettings {
                session_titles: false,
                away_summary: false,
                prompt_suggestions: true,
                model: Some("anthropic/claude-haiku-4-5".to_string()),
            }
        );
        let overridden = resolve(
            &document,
            &[
                (SESSION_TITLES_ENV, "on"),
                (AWAY_SUMMARY_ENV, "1"),
                (PROMPT_SUGGESTION_ENV, "off"),
            ],
        );
        assert!(overridden.session_titles && overridden.away_summary);
        assert!(!overridden.prompt_suggestions);
    }

    #[test]
    fn wrong_types_read_as_unset() {
        assert_eq!(
            resolve(
                &json!({ "awaySummaryEnabled": "no", "sideUxModel": 3, "sessionTitlesEnabled": 0 }),
                &[(PROMPT_SUGGESTION_ENV, "maybe")],
            ),
            SideUxSettings::default()
        );
    }
}
