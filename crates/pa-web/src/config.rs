//! The `web` settings key and its environment overrides.
//!
//! ```json
//! "web": {
//!   "searchBackend": "auto",          // "auto" | "anthropic" | "camoufox"
//!   "searchModel": "anthropic/claude-haiku-4-5",
//!   "fetchModel": "anthropic/claude-haiku-4-5",
//!   "camoufoxPython": "python3"       // interpreter with `camoufox` installed
//! }
//! ```
//!
//! `PRIME_AGENT_WEB_SEARCH_BACKEND` overrides `searchBackend`;
//! `PRIME_AGENT_WEB_CAMOUFOX_PYTHON` overrides `camoufoxPython` when the
//! settings carry none. `web.fetch` prompts fall back to the `auxiliaryModel`
//! setting when `fetchModel` is unset.

use std::path::Path;

use serde_json::Value;

/// The environment variable that overrides `web.searchBackend`.
pub const SEARCH_BACKEND_ENV: &str = "PRIME_AGENT_WEB_SEARCH_BACKEND";
/// The environment variable that overrides `web.camoufoxPython`.
pub const CAMOUFOX_PYTHON_ENV: &str = "PRIME_AGENT_WEB_CAMOUFOX_PYTHON";

/// Which search backend serves `web.search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendChoice {
    /// The headless-Camoufox browser search when it is available, else
    /// Anthropic's server-side search when a model resolves.
    #[default]
    Auto,
    Anthropic,
    Camoufox,
}

impl BackendChoice {
    /// Parse a configured name; `None` for an unknown one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "anthropic" => Some(Self::Anthropic),
            "camoufox" => Some(Self::Camoufox),
            _ => None,
        }
    }
}

/// The resolved `web` configuration of one session directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebSettings {
    pub search_backend: BackendChoice,
    /// A `searchBackend` value that names no backend (reported, then `Auto`).
    pub unknown_backend: Option<String>,
    /// `provider/id` of the model that runs Anthropic searches.
    pub search_model: Option<String>,
    /// `provider/id` of the model that answers `web.fetch` prompts.
    pub fetch_model: Option<String>,
    /// The `auxiliaryModel` setting: the answering model's fallback.
    pub auxiliary_model: Option<String>,
    /// The global `allowedModels` pin.
    pub allowed_models: Option<Vec<String>>,
    /// The Python interpreter that runs the Camoufox search (one with
    /// `camoufox` installed); `None` uses the backend's default.
    pub camoufox_python: Option<String>,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

impl WebSettings {
    /// Read the settings of `cwd` under `agent_dir` and the process
    /// environment. Reads files: call it off the async runtime.
    #[must_use]
    pub fn load(cwd: &Path, agent_dir: &Path) -> Self {
        let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
        let web = settings.settings().extra.get("web").cloned();
        let mut loaded = Self::from_parts(web.as_ref(), settings.get_auxiliary_model(), &|name| {
            std::env::var(name).ok()
        });
        loaded.allowed_models = settings.get_allowed_models();
        loaded
    }

    /// Compose the settings from the `web` value, the `auxiliaryModel`
    /// setting and an environment lookup.
    #[must_use]
    pub fn from_parts(
        web: Option<&Value>,
        auxiliary_model: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let field = |key: &str| non_empty(web.and_then(|web| web.get(key)).and_then(Value::as_str));
        let configured_backend =
            non_empty(env(SEARCH_BACKEND_ENV).as_deref()).or_else(|| field("searchBackend"));
        let (search_backend, unknown_backend) = match configured_backend {
            None => (BackendChoice::Auto, None),
            Some(name) => match BackendChoice::parse(&name) {
                Some(choice) => (choice, None),
                None => (BackendChoice::Auto, Some(name)),
            },
        };
        let camoufox_python =
            field("camoufoxPython").or_else(|| non_empty(env(CAMOUFOX_PYTHON_ENV).as_deref()));
        Self {
            search_backend,
            unknown_backend,
            search_model: field("searchModel"),
            fetch_model: field("fetchModel"),
            auxiliary_model: non_empty(auxiliary_model),
            allowed_models: None,
            camoufox_python,
        }
    }

    /// The model selector that answers `web.fetch` prompts, when any.
    #[must_use]
    pub fn answer_model_selector(&self) -> Option<&str> {
        self.fetch_model
            .as_deref()
            .or(self.auxiliary_model.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn nothing_configured_is_auto_with_no_overrides() {
        let settings = WebSettings::from_parts(None, None, &env_of(&[]));
        assert_eq!(settings, WebSettings::default());
    }

    #[test]
    fn settings_win_over_the_environment_and_the_backend_env_wins_over_settings() {
        let web = json!({
            "searchBackend": "anthropic",
            "searchModel": " anthropic/claude-haiku-4-5 ",
            "camoufoxPython": "settings-python",
        });
        let settings = WebSettings::from_parts(
            Some(&web),
            Some("anthropic/claude-haiku-4-5"),
            &env_of(&[(SEARCH_BACKEND_ENV, "camoufox")]),
        );
        assert_eq!(
            settings,
            WebSettings {
                search_backend: BackendChoice::Camoufox,
                unknown_backend: None,
                search_model: Some("anthropic/claude-haiku-4-5".into()),
                fetch_model: None,
                auxiliary_model: Some("anthropic/claude-haiku-4-5".into()),
                allowed_models: None,
                camoufox_python: Some("settings-python".into()),
            }
        );
        assert_eq!(
            settings.answer_model_selector(),
            Some("anthropic/claude-haiku-4-5")
        );
    }

    #[test]
    fn the_camoufox_python_env_stands_in_when_settings_carry_none() {
        let settings = WebSettings::from_parts(
            None,
            None,
            &env_of(&[(CAMOUFOX_PYTHON_ENV, "/opt/cf/bin/python")]),
        );
        assert_eq!(
            settings.camoufox_python.as_deref(),
            Some("/opt/cf/bin/python")
        );
    }

    #[test]
    fn an_unknown_backend_is_reported_and_behaves_as_auto() {
        let web = json!({ "searchBackend": "bing" });
        let settings = WebSettings::from_parts(Some(&web), None, &env_of(&[]));
        assert_eq!(settings.search_backend, BackendChoice::Auto);
        assert_eq!(settings.unknown_backend.as_deref(), Some("bing"));
    }

    #[test]
    fn fetch_model_wins_over_the_auxiliary_model() {
        let web = json!({ "fetchModel": "p/fetch" });
        let settings = WebSettings::from_parts(Some(&web), Some("p/aux"), &env_of(&[]));
        assert_eq!(settings.answer_model_selector(), Some("p/fetch"));
    }
}
