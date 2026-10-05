//! The pi plugin's request-setting commands (anthropic-auth `packages/pi`
//! `commands.ts`, core `fast.ts` / `cache1h.ts`): `/claude-fast` (fast mode:
//! `speed:"fast"` and its beta on the Opus models that take it) and
//! `/claude-cache` (the one-hour prompt cache and its mode), persisted in
//! the plugin's settings file the requests read. Same arguments, same
//! text.

use serde_json::{json, Map, Value};

use super::convert::CacheMode;
use super::settings::{request_settings, PluginSettings, SettingsError};

/// `/claude-fast`.
pub(crate) const FAST_COMMAND: &str = "claude-fast";
pub(crate) const FAST_DESCRIPTION: &str =
    "Show or configure Anthropic fast mode for supported Opus models";
pub(crate) const FAST_HINT: &str = "[on|off]";
/// `/claude-cache`.
pub(crate) const CACHE_COMMAND: &str = "claude-cache";
pub(crate) const CACHE_DESCRIPTION: &str = "Show or configure Claude 1-hour prompt cache mode";
pub(crate) const CACHE_HINT: &str = "[on|off|mode explicit|automatic|hybrid]";

/// What a command's arguments ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Status,
    Enable,
    Disable,
    Mode(CacheMode),
    Usage,
}

/// `argumentsText.trim().split(/\s+/).filter(Boolean)`.
fn words(args: &str) -> Vec<&str> {
    args.split_whitespace().collect()
}

/// `parseFastModeCommandAction`.
pub(crate) fn parse_fast(args: &str) -> Action {
    match words(args).as_slice() {
        [] => Action::Status,
        ["on"] => Action::Enable,
        ["off"] => Action::Disable,
        _ => Action::Usage,
    }
}

/// `parseCache1hCommandAction`.
pub(crate) fn parse_cache(args: &str) -> Action {
    match words(args).as_slice() {
        [] => Action::Status,
        ["on"] => Action::Enable,
        ["off"] => Action::Disable,
        ["mode", "explicit"] => Action::Mode(CacheMode::Explicit),
        ["mode", "automatic"] => Action::Mode(CacheMode::Automatic),
        ["mode", "hybrid"] => Action::Mode(CacheMode::Hybrid),
        _ => Action::Usage,
    }
}

fn enabled_word(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

/// `buildFastModeStatusSummary`.
fn fast_status(enabled: bool) -> String {
    [
        "## Claude Fast Mode Status".to_string(),
        String::new(),
        format!("- Enabled: {}", enabled_word(enabled)),
        "- Persisted: ~/.config/opencode/anthropic-auth.json".to_string(),
        "- Scope: adds Anthropic fast mode to supported Opus requests".to_string(),
        "- Supported models: claude-opus-4-6, claude-opus-4-7, claude-opus-4-8, claude-opus-5, and claude-opus-5-5".to_string(),
        "- Request changes: adds `speed: \"fast\"` and the `fast-mode-2026-02-01` beta header".to_string(),
        "- Note: fast and standard speeds do not share prompt-cache prefixes".to_string(),
    ]
    .join("\n")
}

/// `executeFastModeCommand`.
pub(crate) fn fast_text(action: Action, enabled: bool) -> String {
    match action {
        Action::Enable => format!("## Claude Fast Mode Enabled\n\n{}", fast_status(true)),
        Action::Disable => format!("## Claude Fast Mode Disabled\n\n{}", fast_status(false)),
        Action::Status | Action::Mode(_) => fast_status(enabled),
        Action::Usage => format!(
            "## Claude Fast Mode Usage\n\nUsage: `/claude-fast`, `/claude-fast on`, or `/claude-fast off`.\n\n{}",
            fast_status(enabled)
        ),
    }
}

fn mode_word(mode: CacheMode) -> &'static str {
    match mode {
        CacheMode::Explicit => "explicit",
        CacheMode::Automatic => "automatic",
        CacheMode::Hybrid => "hybrid",
    }
}

/// `buildCache1hStatusSummary`.
fn cache_status(enabled: bool, mode: CacheMode) -> String {
    [
        "## Claude Cache Status".to_string(),
        String::new(),
        format!("- Enabled: {}", enabled_word(enabled)),
        format!("- Mode: {}", mode_word(mode)),
        "- Persisted: ~/.config/opencode/anthropic-auth.json".to_string(),
        "- Scope: main sessions only; subagent sessions keep default ephemeral cache behavior".to_string(),
        "- Modes: explicit = existing OpenCode breakpoints; automatic = top-level cache_control only; hybrid = system + messages[0] + top-level cache_control".to_string(),
        "- TTL: 1h when enabled for main sessions; default ephemeral cache behavior otherwise".to_string(),
    ]
    .join("\n")
}

/// `executeCache1hCommand`.
pub(crate) fn cache_text(action: Action, enabled: bool, mode: CacheMode) -> String {
    match action {
        Action::Status => cache_status(enabled, mode),
        Action::Enable => format!("## Claude Cache Enabled\n\n{}", cache_status(true, mode)),
        Action::Disable => format!("## Claude Cache Disabled\n\n{}", cache_status(false, mode)),
        Action::Mode(next) => format!(
            "## Claude Cache Status\n\nMode updated to `{}`.\n\n{}",
            mode_word(next),
            cache_status(enabled, next)
        ),
        Action::Usage => format!(
            "## Claude Cache Usage\n\nUsage: `/claude-cache`, `/claude-cache on`, `/claude-cache off`, or `/claude-cache mode explicit|automatic|hybrid`.\n\n{}",
            cache_status(enabled, mode)
        ),
    }
}

/// `/claude-fast <args>`: the setting changed (when asked), and the text.
pub(crate) fn run_fast(settings: &PluginSettings, args: &str) -> Result<String, SettingsError> {
    let action = parse_fast(args);
    let enabled = request_settings(&settings.read()).fast_mode;
    let next = match action {
        Action::Enable => true,
        Action::Disable => false,
        _ => enabled,
    };
    if matches!(action, Action::Enable | Action::Disable) {
        // `setFastModePersistentEnabled`.
        settings.update(|config| {
            merge_section(config, "claudeFast", [("enabled", Value::Bool(next))]);
        })?;
    }
    Ok(fast_text(action, next))
}

/// `/claude-cache <args>`: the setting changed (when asked), and the text.
pub(crate) fn run_cache(settings: &PluginSettings, args: &str) -> Result<String, SettingsError> {
    let action = parse_cache(args);
    let current = request_settings(&settings.read());
    let enabled = match action {
        Action::Enable => true,
        Action::Disable => false,
        _ => current.cache_enabled,
    };
    let mode = match action {
        Action::Mode(mode) => mode,
        _ => current.cache_mode,
    };
    match action {
        // `setCache1hPersistentEnabled`: the mode written as it reads.
        Action::Enable | Action::Disable => settings.update(|config| {
            let mode = request_settings(config).cache_mode;
            merge_section(
                config,
                "claudeCache",
                [
                    ("enabled", Value::Bool(enabled)),
                    ("mode", json!(mode_word(mode))),
                ],
            );
        })?,
        // `setCache1hPersistentMode`.
        Action::Mode(next) => settings.update(|config| {
            let was = request_settings(config).cache_enabled;
            merge_section(
                config,
                "claudeCache",
                [
                    ("enabled", Value::Bool(was)),
                    ("mode", json!(mode_word(next))),
                ],
            );
        })?,
        Action::Status | Action::Usage => {}
    }
    Ok(cache_text(action, enabled, mode))
}

/// `{...(config[key] ?? {}), ...fields}`.
fn merge_section<const N: usize>(
    config: &mut Map<String, Value>,
    key: &str,
    fields: [(&str, Value); N],
) {
    let section = config
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()));
    if !section.is_object() {
        *section = Value::Object(Map::new());
    }
    if let Some(section) = section.as_object_mut() {
        for (name, value) in fields {
            section.insert(name.to_string(), value);
        }
    }
}

#[cfg(test)]
mod tests;
