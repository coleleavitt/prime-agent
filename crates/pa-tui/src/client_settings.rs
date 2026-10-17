//! The client-settings seam: pa-tui depends on pa-types only, so
//! interactive commands that read or persist user settings call this
//! trait; the composition root (pa-cli) implements it over the real
//! settings manager. Setters persist the global scope.

use anyhow::Result;

pub trait ClientSettings: Send + Sync {
    /// Master harness refinement switch, enabled by default.
    fn harness_refinements(&self) -> bool {
        true
    }
    /// Persist the master harness refinement switch.
    ///
    /// # Errors
    /// Returns an error if the settings store fails or is unavailable.
    fn set_harness_refinements(&self, _enabled: bool) -> Result<()> {
        anyhow::bail!("Harness refinement settings are unavailable")
    }

    /// Automatic compaction percentage ceiling; output headroom may trigger earlier.
    fn compaction_threshold_percent(&self) -> f64;
    /// Persist the percentage globally.
    ///
    /// # Errors
    /// Returns an error for invalid percentages or failed persistence.
    fn set_compaction_threshold_percent(&self, percent: f64) -> Result<()>;
    /// `theme` (TS `getTheme`/`setTheme`).
    fn theme(&self) -> Option<String>;
    /// Persists `theme` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_theme(&self, theme: &str) -> Result<()>;
    /// `terminal.showImages` (TS default true).
    fn show_images(&self) -> bool;
    /// Persists `terminal.showImages` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_show_images(&self, enabled: bool) -> Result<()>;
    /// `terminal.clearOnShrink` (TS default false).
    fn clear_on_shrink(&self) -> bool;
    /// Persists `terminal.clearOnShrink` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_clear_on_shrink(&self, enabled: bool) -> Result<()>;
    /// `terminal.showTerminalProgress` (TS default false).
    fn show_terminal_progress(&self) -> bool;
    /// Persists `terminal.showTerminalProgress` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_show_terminal_progress(&self, enabled: bool) -> Result<()>;
    /// `images.autoResize` (TS default true).
    fn image_auto_resize(&self) -> bool;
    /// Persists `images.autoResize` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_image_auto_resize(&self, enabled: bool) -> Result<()>;
    /// `images.blockImages` (TS default false).
    fn block_images(&self) -> bool;
    /// Persists `images.blockImages` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_block_images(&self, blocked: bool) -> Result<()>;
    /// `imageModel` ("provider/model-id" or a bare id; unset by default),
    /// read-only for the paste-time hint.
    fn image_model(&self) -> Option<String>;
    /// `enableSkillCommands` (TS default true).
    fn enable_skill_commands(&self) -> bool;
    /// Persists `enableSkillCommands` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_enable_skill_commands(&self, enabled: bool) -> Result<()>;
    /// `enableBuiltinSkills` (TS default true).
    fn enable_builtin_skills(&self) -> bool;
    /// Persists `enableBuiltinSkills` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_enable_builtin_skills(&self, enabled: bool) -> Result<()>;
    /// `showHardwareCursor` (TS default false).
    fn show_hardware_cursor(&self) -> bool;
    /// Persists `showHardwareCursor` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_show_hardware_cursor(&self, enabled: bool) -> Result<()>;
    /// `editorPaddingX` (TS default 0).
    fn editor_padding_x(&self) -> u64;
    /// Persists `editorPaddingX` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_editor_padding_x(&self, padding: u64) -> Result<()>;
    /// `autocompleteMaxVisible` (TS default 5).
    fn autocomplete_max_visible(&self) -> u64;
    /// Persists `autocompleteMaxVisible` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_autocomplete_max_visible(&self, max_visible: u64) -> Result<()>;
    /// `quietStartup` (TS default false).
    fn quiet_startup(&self) -> bool;
    /// Persists `quietStartup` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_quiet_startup(&self, quiet: bool) -> Result<()>;
    /// `idleEvictionMinutes` wire form (a number or `off`).
    fn idle_eviction_minutes(&self) -> String;
    /// Persists the `idleEvictionMinutes` wire form to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_idle_eviction_minutes(&self, value: &str) -> Result<()>;
    /// `markdown.mermaid` (`off`/`final`/`streaming`).
    fn mermaid_rendering_mode(&self) -> String;
    /// Persists `markdown.mermaid` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_mermaid_rendering_mode(&self, mode: &str) -> Result<()>;
    /// `treeFilterMode` (`default`/`no-tools`/`user-only`/`labeled-only`/`all`).
    fn tree_filter_mode(&self) -> String;
    /// Persists `treeFilterMode` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_tree_filter_mode(&self, mode: &str) -> Result<()>;
    /// `defaultServiceTier` (default "default"): the wire name of the
    /// persisted default tier.
    fn default_service_tier(&self) -> String;
    /// Persists the wire name to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the tier name is not a known service tier or
    /// the settings store fails.
    fn set_default_service_tier(&self, tier: &str) -> Result<()>;
    /// `factory.enabled` (the agent factory's opt-in gate; unset reads
    /// as disabled).
    fn factory_enabled(&self) -> bool;
    /// Persists `factory.enabled` to the global scope — the same shared
    /// settings key the daemon's `factory_activity` lane advertisement
    /// and the kernel's factory gate read.
    ///
    /// # Errors
    ///
    /// Returns an error when opening or persisting the settings store
    /// fails.
    fn set_factory_enabled(&self, enabled: bool) -> Result<()>;
    /// `chatDetail` (`overview`/`details`/`all`; TS #2709 default
    /// `details`): the conversation-detail level the chat starts at.
    /// `chatDetail` (`overview`/`details`/`all`; the default reads as
    /// `overview`, the collapse mode): the conversation-detail level the
    /// chat starts at.
    fn chat_detail(&self) -> String;
    /// Persists `chatDetail` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_chat_detail(&self, detail: &str) -> Result<()>;
    /// `warnings.anthropicExtraUsage` (TS default true).
    fn warnings_anthropic_extra_usage(&self) -> bool;
    /// Persists `warnings.anthropicExtraUsage` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_warnings_anthropic_extra_usage(&self, enabled: bool) -> Result<()>;
    /// `updateChannel` (`stable`/`nightly`; unset infers the channel from
    /// the running version).
    fn update_channel(&self) -> Option<String>;
    /// Persists `updateChannel` to the global scope.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails.
    fn set_update_channel(&self, channel: &str) -> Result<()>;
    /// The effective update channel for the running version, as its wire
    /// name (a preferred channel wins, else the prerelease infers).
    fn effective_update_channel(&self, version: &str) -> String;
    /// The `/telemetry status` report: on or off and why, the endpoint,
    /// and the installation id.
    fn telemetry_status(&self) -> String;
    /// Persists `telemetry.enabled` (the TS key) and returns the report
    /// after the write (it names an environment variable or project
    /// setting that still decides).
    ///
    /// # Errors
    ///
    /// Returns `Err` when opening or persisting the settings
    /// store fails.
    fn set_telemetry_enabled(&self, enabled: bool) -> Result<String>;
    /// Whether the first-run telemetry disclosure is due in this
    /// interactive view (TS agent-session-services: telemetry enabled,
    /// onboarding already shown, notice not yet shown). Defaults to
    /// false: a client without the disclosure surface never shows it.
    #[must_use]
    fn telemetry_notice_due(&self) -> bool {
        false
    }
    /// Persist the notice-shown marker once the disclosure renders
    /// (TS `setTelemetryNoticeShown`).
    ///
    /// # Errors
    ///
    /// Returns `Err` when opening or persisting the settings
    /// store fails.
    fn set_telemetry_notice_shown(&self) -> Result<()> {
        Ok(())
    }
    /// Claude Code's `statusLine` setting, validated; `None` is off.
    /// Read once per run (and on `/reload`), never on the paint path.
    #[must_use]
    fn status_line(&self) -> Option<StatusLineSettings> {
        None
    }
    /// `outputStyle` (for the status line payload): the style's name,
    /// `default` when unset.
    #[must_use]
    fn output_style(&self) -> String {
        "default".to_string()
    }
    /// `editorMode`: the prompt editor's key mode.
    #[must_use]
    fn editor_mode(&self) -> EditorMode {
        EditorMode::Normal
    }
    /// Persists `editorMode` to the global scope (`/vim`).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the settings store fails or is unavailable.
    fn set_editor_mode(&self, _mode: EditorMode) -> Result<()> {
        anyhow::bail!("Editor mode settings are unavailable")
    }
    /// The launch's `--add-dir` directories (absolute), carried on every
    /// create so the session engine joins them to its own list.
    #[must_use]
    fn launch_additional_directories(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

/// The prompt editor's key mode (`editorMode`, Claude Code's values).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EditorMode {
    #[default]
    Normal,
    Vim,
}

impl EditorMode {
    /// The settings value.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            EditorMode::Normal => "normal",
            EditorMode::Vim => "vim",
        }
    }
}

/// Claude Code's `statusLine` setting: `{ "type": "command", "command",
/// "padding", "refreshInterval", "hideVimModeIndicator" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLineSettings {
    pub command: String,
    /// Columns of left padding before each status row.
    pub padding: usize,
    /// Re-run every N seconds besides the event-driven runs (min 1s).
    pub refresh_interval: Option<std::time::Duration>,
    /// Hide the built-in vim mode indicator (the command renders it).
    pub hide_vim_mode_indicator: bool,
}

impl StatusLineSettings {
    /// Validate the raw setting: a `command` type with a non-blank
    /// command; anything else is off.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        if value.get("type").and_then(serde_json::Value::as_str) != Some("command") {
            return None;
        }
        let command = value
            .get("command")
            .and_then(serde_json::Value::as_str)
            .filter(|command| !command.trim().is_empty())?
            .to_string();
        let padding = value
            .get("padding")
            .and_then(serde_json::Value::as_u64)
            .map_or(0, |padding| usize::try_from(padding.min(40)).unwrap_or(0));
        let refresh_interval = value
            .get("refreshInterval")
            .and_then(serde_json::Value::as_f64)
            .filter(|seconds| seconds.is_finite() && *seconds >= 1.0)
            .map(std::time::Duration::from_secs_f64);
        Some(Self {
            command,
            padding,
            refresh_interval,
            hide_vim_mode_indicator: value
                .get("hideVimModeIndicator")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_settings_validate_the_claude_code_shape() {
        let parse = |value: serde_json::Value| StatusLineSettings::from_value(&value);
        assert_eq!(
            parse(serde_json::json!({
                "type": "command",
                "command": "~/.claude/statusline.sh",
                "padding": 2,
                "refreshInterval": 5,
                "hideVimModeIndicator": true
            })),
            Some(StatusLineSettings {
                command: "~/.claude/statusline.sh".to_string(),
                padding: 2,
                refresh_interval: Some(std::time::Duration::from_secs(5)),
                hide_vim_mode_indicator: true,
            })
        );
        assert_eq!(
            parse(
                serde_json::json!({ "type": "command", "command": "echo hi", "refreshInterval": 0 })
            ),
            Some(StatusLineSettings {
                command: "echo hi".to_string(),
                padding: 0,
                refresh_interval: None,
                hide_vim_mode_indicator: false,
            })
        );
        assert_eq!(
            parse(serde_json::json!({ "type": "static", "command": "x" })),
            None
        );
        assert_eq!(
            parse(serde_json::json!({ "type": "command", "command": " " })),
            None
        );
    }
}
