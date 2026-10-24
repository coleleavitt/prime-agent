//! The launch layer: what the command line set for this run, beside the
//! settings files (Claude Code's `flagSettings` and `cliArg` sources).
//!
//! `prime-agent` parses `--settings`, `--setting-sources`, the permission
//! flags (`--allowedTools`, `--disallowedTools`, `--tools`,
//! `--permission-mode`, `--dangerously-skip-permissions`,
//! `--allow-dangerously-skip-permissions`), `--mcp-config`,
//! `--strict-mcp-config`, `--agents`, `--agent`, `--bare` and `--betas`
//! into one [`LaunchSettings`] and publishes it in its own environment
//! ([`LAUNCH_SETTINGS_ENV`]) before any session starts. Everything a
//! session reads goes through [`LaunchSettings::current`], so an in-process
//! print session and a daemon worker (which receives the variable with the
//! client's launch environment on `create`) see the same layer:
//!
//! - the settings manager layers `flag_settings` over the files and drops
//!   the scopes `setting_sources` leaves out ([`SettingSource`]);
//! - the permissions feature reads `permissions` as its `cliArg` source;
//! - the MCP manager adds `mcp_servers` (alone under `strict_mcp_config`);
//! - agent discovery adds `agents`, and the main session runs as `agent`.
//!
//! The variable is the CLI's, never a model's: the kernel and everything it
//! spawns do not inherit it (`kernel::shared::KernelEnvironment`), the
//! daemon supervisor is spawned without it, and a worker gets only the copy
//! its creating client forwarded.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The environment variable carrying the launch layer (JSON).
pub const LAUNCH_SETTINGS_ENV: &str = "PRIME_AGENT_LAUNCH_SETTINGS";

/// A settings scope `--setting-sources` can select (Claude Code's names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingSource {
    /// `~/.prime/agent/settings.json` (and Claude Code's user file).
    User,
    /// `.prime/agent/settings.json` (and `.claude/settings.json`).
    Project,
    /// `.prime/agent/settings.local.json` (and `.claude/settings.local.json`).
    Local,
}

impl SettingSource {
    pub const ALL: [SettingSource; 3] = [
        SettingSource::User,
        SettingSource::Project,
        SettingSource::Local,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SettingSource::User => "user",
            SettingSource::Project => "project",
            SettingSource::Local => "local",
        }
    }

    /// Parse `--setting-sources`' comma-separated list. An empty list is
    /// valid (no settings files at all), as in Claude Code.
    ///
    /// # Errors
    ///
    /// An unknown source name, worded as Claude Code words it.
    pub fn parse_list(text: &str) -> Result<Vec<SettingSource>, String> {
        let mut sources = Vec::new();
        for name in text
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let source = match name {
                "user" => SettingSource::User,
                "project" => SettingSource::Project,
                "local" => SettingSource::Local,
                other => {
                    return Err(format!(
                        "Invalid setting source: {other}. Valid options are: user, project, local"
                    ));
                }
            };
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        Ok(sources)
    }
}

/// The permission flags of the command line (Claude Code's `cliArg`
/// source).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CliPermissions {
    /// `--allowedTools` rules.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// `--disallowedTools` rules.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    /// `--tools`: the built-in tools the run may use (`None`: all of them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// `--permission-mode` (or `bypassPermissions` from
    /// `--dangerously-skip-permissions`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `--allow-dangerously-skip-permissions` (or
    /// `--dangerously-skip-permissions`): bypass is in the mode cycle.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_bypass: bool,
}

impl CliPermissions {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &CliPermissions::default()
    }
}

/// What the command line set for this run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent command-line switches, not a state machine"
)]
pub struct LaunchSettings {
    /// `--settings`: one settings document layered over every file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_settings: Option<Map<String, Value>>,
    /// `--setting-sources`: the file scopes read (`None`: all of them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting_sources: Option<Vec<SettingSource>>,
    #[serde(skip_serializing_if = "CliPermissions::is_empty")]
    pub permissions: CliPermissions,
    /// `--mcp-config`: MCP servers by name (Claude Code's `mcpServers`
    /// entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Map<String, Value>>,
    /// `--strict-mcp-config`: only the `--mcp-config` servers.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub strict_mcp_config: bool,
    /// `--agents`: agent definitions by name (`description`, `prompt`,
    /// `tools`, `model`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Map<String, Value>>,
    /// `--agent`: the agent the main session runs as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// `--bare`: no hooks, plugins, LSP, auto-memory or instruction-file
    /// discovery.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bare: bool,
    /// `--betas`: extra `anthropic-beta` headers for Anthropic requests.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub betas: Vec<String>,
}

impl LaunchSettings {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &LaunchSettings::default()
    }

    /// The layer this process was launched with; `None` when the command
    /// line set nothing (or the variable does not parse, which is logged:
    /// it is written by this binary only).
    #[must_use]
    pub fn current() -> Option<LaunchSettings> {
        let raw = std::env::var(LAUNCH_SETTINGS_ENV).ok()?;
        Self::from_env_value(&raw)
    }

    /// Parse the variable's value; `None` for an empty or malformed one.
    #[must_use]
    pub fn from_env_value(raw: &str) -> Option<LaunchSettings> {
        if raw.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<LaunchSettings>(raw) {
            Ok(settings) if settings.is_empty() => None,
            Ok(settings) => Some(settings),
            Err(error) => {
                tracing::warn!(
                    target: "pa_core::settings",
                    "{LAUNCH_SETTINGS_ENV} does not parse: {error}"
                );
                None
            }
        }
    }

    /// The variable's value for this layer.
    #[must_use]
    pub fn to_env_value(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Publish this layer in this process's environment (the CLI calls it
    /// once at startup, before any thread reads settings); an empty layer
    /// removes the variable.
    pub fn publish(&self) {
        if self.is_empty() {
            std::env::remove_var(LAUNCH_SETTINGS_ENV);
        } else {
            std::env::set_var(LAUNCH_SETTINGS_ENV, self.to_env_value());
        }
    }

    /// Whether `source`'s files are read (`--setting-sources`).
    #[must_use]
    pub fn reads_source(&self, source: SettingSource) -> bool {
        self.setting_sources
            .as_ref()
            .is_none_or(|sources| sources.contains(&source))
    }
}

/// Split a Claude Code tool list (`--allowedTools`, `--disallowedTools`,
/// `--tools`): names separated by commas or whitespace, except inside a
/// rule's parentheses (`Bash(git log *), Edit` is two rules).
#[must_use]
pub fn split_tool_list(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => {
                current.push(ch);
                escaped = true;
            }
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut current)),
            ch if ch.is_whitespace() && depth == 0 => {
                items.push(std::mem::take(&mut current));
            }
            ch => current.push(ch),
        }
    }
    items.push(current);
    items
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Whether `source`'s files are read in this process.
#[must_use]
pub fn source_enabled(source: SettingSource) -> bool {
    LaunchSettings::current().is_none_or(|launch| launch.reads_source(source))
}

/// Whether this run is `--bare`.
#[must_use]
pub fn bare() -> bool {
    LaunchSettings::current().is_some_and(|launch| launch.bare)
}

/// Read a value that is either inline JSON or the path of a JSON file
/// (`--settings`, `--mcp-config`, `--agents`): text starting with `{` is
/// JSON, anything else a path (relative to `cwd`).
///
/// # Errors
///
/// The file cannot be read, or the JSON does not parse or is not an object.
pub fn read_json_object_arg(
    flag: &str,
    value: &str,
    cwd: &std::path::Path,
) -> Result<Map<String, Value>, String> {
    let trimmed = value.trim();
    let (text, origin) = if trimmed.starts_with('{') {
        (trimmed.to_string(), None)
    } else {
        let path = cwd.join(trimmed);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{flag}: cannot read {}: {error}", path.display()))?;
        (text, Some(path))
    };
    let where_ = origin
        .as_ref()
        .map_or_else(String::new, |path| format!(" in {}", path.display()));
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(format!("{flag}: expected a JSON object{where_}")),
        Err(error) => Err(format!("{flag}: invalid JSON{where_}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_lists_split_on_commas_and_spaces_outside_parentheses() {
        assert_eq!(
            split_tool_list("Bash(git log *), Edit  Read,,mcp__gh__get"),
            vec!["Bash(git log *)", "Edit", "Read", "mcp__gh__get"]
        );
        assert_eq!(
            split_tool_list(r"Bash(echo \(a, b\)) WebFetch(domain:x.dev)"),
            vec![r"Bash(echo \(a, b\))", "WebFetch(domain:x.dev)"]
        );
        assert_eq!(split_tool_list("  "), Vec::<String>::new());
    }

    #[test]
    fn setting_sources_parse_like_claude_code() {
        assert_eq!(
            SettingSource::parse_list("user, local,user"),
            Ok(vec![SettingSource::User, SettingSource::Local])
        );
        assert_eq!(SettingSource::parse_list(""), Ok(vec![]));
        assert_eq!(
            SettingSource::parse_list("user,policy"),
            Err("Invalid setting source: policy. Valid options are: user, project, local".into())
        );
    }

    #[test]
    fn the_layer_round_trips_through_its_variable_and_empty_is_absent() {
        let launch = LaunchSettings {
            setting_sources: Some(vec![SettingSource::Project]),
            permissions: CliPermissions {
                allowed_tools: vec!["Bash(git *)".into()],
                mode: Some("plan".into()),
                ..CliPermissions::default()
            },
            strict_mcp_config: true,
            ..LaunchSettings::default()
        };
        let raw = launch.to_env_value();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            serde_json::json!({
                "settingSources": ["project"],
                "permissions": {"allowedTools": ["Bash(git *)"], "mode": "plan"},
                "strictMcpConfig": true,
            })
        );
        assert_eq!(LaunchSettings::from_env_value(&raw), Some(launch.clone()));
        assert!(!launch.reads_source(SettingSource::User));
        assert!(launch.reads_source(SettingSource::Project));
        assert_eq!(LaunchSettings::from_env_value("{}"), None);
        assert_eq!(LaunchSettings::from_env_value("not json"), None);
        assert!(LaunchSettings::default().reads_source(SettingSource::Local));
    }

    /// The kernel (and every command it runs) never inherits the layer.
    #[test]
    fn the_kernel_does_not_inherit_the_layer() {
        for environment in [
            crate::kernel::shared::KernelEnvironment::Inherit,
            crate::kernel::shared::KernelEnvironment::ScrubCredentials,
        ] {
            assert!(!environment.passes_to_kernel(LAUNCH_SETTINGS_ENV));
            assert!(environment.passes_to_kernel("PATH"));
        }
    }

    #[test]
    fn json_object_args_read_inline_or_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.json"), r#"{"model": "m"}"#).unwrap();
        assert_eq!(
            read_json_object_arg("--settings", "s.json", dir.path()).unwrap()["model"],
            "m"
        );
        assert_eq!(
            read_json_object_arg("--settings", r#" {"a": 1}"#, dir.path()).unwrap()["a"],
            1
        );
        let missing = read_json_object_arg("--settings", "nope.json", dir.path()).unwrap_err();
        assert!(missing.starts_with("--settings: cannot read "), "{missing}");
        assert_eq!(
            read_json_object_arg("--agents", "{", dir.path())
                .unwrap_err()
                .split(':')
                .take(2)
                .collect::<Vec<_>>(),
            vec!["--agents", " invalid JSON"]
        );
        std::fs::write(dir.path().join("a.json"), "[1]").unwrap();
        assert!(
            read_json_object_arg("--mcp-config", "a.json", dir.path())
                .unwrap_err()
                .starts_with("--mcp-config: expected a JSON object in ")
        );
    }
}
