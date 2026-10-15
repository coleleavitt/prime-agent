//! The `hooks` settings key, in Claude Code's shape:
//!
//! ```json
//! { "hooks": { "PreToolUse": [
//!     { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/bin/guard", "timeout": 10 }] }
//! ] } }
//! ```
//!
//! User hooks (`<agent dir>/settings.json`) come first, then a trusted
//! project's (`.prime/agent/settings.json`; `hooks` is a gated key, so an
//! untrusted project contributes none).

use std::path::Path;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

/// The default per-command timeout (Claude Code's).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The events this crate fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEvent {
    pub const ALL: [HookEvent; 5] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Stop,
    ];

    /// The settings key and the `hook_event_name` hooks receive.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
        }
    }

    /// Whether the event's matchers filter on a tool name.
    fn matches_tools(self) -> bool {
        matches!(self, HookEvent::PreToolUse | HookEvent::PostToolUse)
    }
}

/// One hook command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub command: String,
    pub timeout: Duration,
}

/// A matcher group: the tool names it applies to and its commands.
#[derive(Debug, Clone)]
struct Group {
    /// `None` matches every tool (an empty or `*` matcher, or an event
    /// without tool names).
    matcher: Option<Regex>,
    commands: Vec<HookCommand>,
}

#[derive(Deserialize)]
struct RawGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<RawCommand>,
}

#[derive(Deserialize)]
struct RawCommand {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    /// Seconds.
    #[serde(default)]
    timeout: Option<f64>,
}

/// Every configured hook, by event.
#[derive(Debug, Clone, Default)]
pub struct HooksConfig {
    groups: Vec<(HookEvent, Group)>,
    /// Entries that were skipped, for the session's diagnostics.
    pub problems: Vec<String>,
}

impl HooksConfig {
    /// The hooks configured for a session at `cwd`.
    #[must_use]
    pub fn load(cwd: &Path, agent_dir: &Path) -> Self {
        let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
        let mut config = HooksConfig::default();
        config.add(settings.global_settings().extra.get("hooks"), "user");
        if settings.project_scope_trusted() {
            config.add(settings.project_settings().extra.get("hooks"), "project");
        }
        config
    }

    /// Parse one settings document's `hooks` value.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let mut config = HooksConfig::default();
        config.add(Some(value), "settings");
        config
    }

    fn add(&mut self, value: Option<&Value>, scope: &str) {
        let Some(value) = value else {
            return;
        };
        let Some(events) = value.as_object() else {
            self.problems
                .push(format!("{scope} hooks: expected an object of events"));
            return;
        };
        for (key, groups) in events {
            let Some(event) = HookEvent::ALL.into_iter().find(|event| event.name() == key) else {
                self.problems
                    .push(format!("{scope} hooks: event {key} is not supported"));
                continue;
            };
            let groups: Vec<RawGroup> = match serde_json::from_value(groups.clone()) {
                Ok(groups) => groups,
                Err(error) => {
                    self.problems.push(format!("{scope} hooks.{key}: {error}"));
                    continue;
                }
            };
            for group in groups {
                let matcher = match group.matcher.as_deref().map(str::trim) {
                    None | Some("" | "*") => None,
                    Some(_) if !event.matches_tools() => None,
                    // Anchored: `Bash` matches the tool `Bash`, not `BashOutput`
                    // (Claude Code matches the whole name).
                    Some(pattern) => match Regex::new(&format!("^(?:{pattern})$")) {
                        Ok(regex) => Some(regex),
                        Err(error) => {
                            self.problems
                                .push(format!("{scope} hooks.{key}: matcher {pattern:?}: {error}"));
                            continue;
                        }
                    },
                };
                let commands = group
                    .hooks
                    .into_iter()
                    .filter_map(|command| {
                        if command
                            .kind
                            .as_deref()
                            .is_some_and(|kind| kind != "command")
                        {
                            self.problems.push(format!(
                                "{scope} hooks.{key}: only `command` hooks are supported"
                            ));
                            return None;
                        }
                        let text = command.command.filter(|text| !text.trim().is_empty())?;
                        let timeout = command
                            .timeout
                            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                            .map_or(DEFAULT_TIMEOUT, Duration::from_secs_f64);
                        Some(HookCommand {
                            command: text,
                            timeout,
                        })
                    })
                    .collect();
                self.groups.push((event, Group { matcher, commands }));
            }
        }
    }

    /// Whether any command listens for `event`.
    #[must_use]
    pub fn has(&self, event: HookEvent) -> bool {
        self.groups
            .iter()
            .any(|(listening, group)| *listening == event && !group.commands.is_empty())
    }

    /// The commands `event` runs for `tool` (`None` for events without a
    /// tool), in configuration order, each command once.
    #[must_use]
    pub fn commands(&self, event: HookEvent, tool: Option<&str>) -> Vec<HookCommand> {
        let mut commands: Vec<HookCommand> = Vec::new();
        for (listening, group) in &self.groups {
            if *listening != event {
                continue;
            }
            let applies = match (&group.matcher, tool) {
                (None, _) => true,
                (Some(matcher), Some(tool)) => matcher.is_match(tool),
                (Some(_), None) => false,
            };
            if !applies {
                continue;
            }
            for command in &group.commands {
                if !commands
                    .iter()
                    .any(|known| known.command == command.command)
                {
                    commands.push(command.clone());
                }
            }
        }
        commands
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn command(text: &str, seconds: u64) -> HookCommand {
        HookCommand {
            command: text.to_string(),
            timeout: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn matchers_select_commands_by_tool_name() {
        let config = HooksConfig::from_value(&json!({
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard", "timeout": 5 }] },
                { "matcher": "python_repl|mcp__.*", "hooks": [{ "type": "command", "command": "audit" }] },
                { "matcher": "*", "hooks": [{ "type": "command", "command": "log" }, { "type": "command", "command": "guard" }] },
            ],
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "recall" }] }],
        }));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        assert_eq!(
            config.commands(HookEvent::PreToolUse, Some("Bash")),
            vec![command("guard", 5), command("log", 60)]
        );
        assert_eq!(
            config.commands(HookEvent::PreToolUse, Some("BashOutput")),
            vec![command("log", 60), command("guard", 60)]
        );
        assert_eq!(
            config.commands(HookEvent::PreToolUse, Some("mcp__github__search")),
            vec![
                command("audit", 60),
                command("log", 60),
                command("guard", 60)
            ]
        );
        assert_eq!(
            config.commands(HookEvent::UserPromptSubmit, None),
            vec![command("recall", 60)]
        );
        assert!(config.has(HookEvent::UserPromptSubmit));
        assert!(!config.has(HookEvent::Stop));
    }

    #[test]
    fn unsupported_entries_are_reported_and_skipped() {
        let config = HooksConfig::from_value(&json!({
            "Notification": [{ "hooks": [{ "type": "command", "command": "x" }] }],
            "PreToolUse": [
                { "matcher": "(", "hooks": [{ "type": "command", "command": "x" }] },
                { "hooks": [{ "type": "prompt", "prompt": "judge it" }, { "type": "command", "command": "kept" }] },
            ],
            "Stop": "not a list",
        }));
        assert_eq!(
            config.commands(HookEvent::PreToolUse, Some("Bash")),
            vec![command("kept", 60)]
        );
        assert_eq!(config.problems.len(), 4, "{:?}", config.problems);
    }
}
