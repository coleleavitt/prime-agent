//! Language server configuration: Claude Code's server shape, read from the
//! `lspServers` setting and a trusted project's `.lsp.json`.
//!
//! ```json
//! {
//!   "rust": {
//!     "command": "rust-analyzer",
//!     "args": [],
//!     "extensionToLanguage": { ".rs": "rust" },
//!     "env": { "RA_LOG": "error" },
//!     "initializationOptions": {},
//!     "settings": { "rust-analyzer": { "check": { "command": "clippy" } } },
//!     "workspaceFolder": ".",
//!     "startupTimeout": 30000,
//!     "shutdownTimeout": 5000,
//!     "requestTimeout": 60000,
//!     "restartOnCrash": true,
//!     "maxRestarts": 3,
//!     "diagnostics": true
//!   }
//! }
//! ```
//!
//! `.lsp.json` (the project root of the session's directory) holds that
//! object; the `lspServers` setting (user `settings.json`, or a trusted
//! project's `.prime/agent/settings.json`) holds the same object. A server
//! named in both comes from the setting. `.lsp.json` runs commands, so it is
//! read only in a trusted workspace (and is part of the trust fingerprint:
//! `pa-cli` gates it). `lspDiagnosticsWaitMs` bounds how long the turn
//! boundary waits for the diagnostics of the files a cell wrote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// The project file Claude Code's plugins configure servers in.
pub const PROJECT_CONFIG_FILE: &str = ".lsp.json";
/// The settings key holding servers by name.
pub const SETTINGS_KEY: &str = "lspServers";
/// The settings key bounding the boundary's diagnostics wait (milliseconds).
pub const DIAGNOSTICS_WAIT_KEY: &str = "lspDiagnosticsWaitMs";
/// The boundary's default diagnostics wait.
pub const DEFAULT_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(2);
/// The most the diagnostics wait may be set to.
pub const MAX_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(30);
/// Claude Code's default request timeout.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
/// The startup timeout when none is configured (Claude Code waits forever).
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_mins(1);
/// The graceful shutdown bound when none is configured.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Claude Code's default crash recovery bound.
pub const DEFAULT_MAX_RESTARTS: u32 = 3;

/// How the client talks to the server. Claude Code's schema has `socket`
/// too, but its client speaks stdio only; so does this one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Stdio,
    Socket,
}

/// One server's configuration (Claude Code's `lspServers` entry).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extension (with its dot) to LSP language id.
    pub extension_to_language: BTreeMap<String, String>,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub initialization_options: Option<Value>,
    /// Answered to `workspace/configuration` and pushed with
    /// `workspace/didChangeConfiguration` after startup.
    #[serde(default)]
    pub settings: Option<Value>,
    /// The server's root (relative to the session's directory); default the
    /// session's directory.
    #[serde(default)]
    pub workspace_folder: Option<String>,
    #[serde(default)]
    pub startup_timeout: Option<u64>,
    #[serde(default)]
    pub shutdown_timeout: Option<u64>,
    #[serde(default)]
    pub request_timeout: Option<u64>,
    #[serde(default)]
    pub restart_on_crash: Option<bool>,
    #[serde(default)]
    pub max_restarts: Option<u32>,
    /// `false` keeps navigation but never injects this server's diagnostics.
    #[serde(default)]
    pub diagnostics: Option<bool>,
}

impl ServerConfig {
    /// Check Claude Code's rules; the message names what is wrong.
    ///
    /// # Errors
    ///
    /// An empty command, a command with spaces that is not an absolute
    /// path, no extension mapping, or the socket transport.
    pub fn validate(&self) -> Result<(), String> {
        let command = self.command.trim();
        if command.is_empty() {
            return Err("missing required 'command' field".to_string());
        }
        if command.contains(' ') && !Path::new(command).is_absolute() {
            return Err(
                "Command should not contain spaces. Use args array for arguments.".to_string(),
            );
        }
        if self.extension_to_language.is_empty() {
            return Err("missing required 'extensionToLanguage' field".to_string());
        }
        if self.transport == Transport::Socket {
            return Err("transport 'socket' is not supported; use 'stdio'".to_string());
        }
        Ok(())
    }

    /// The configured extensions, lowercased, each with its leading dot.
    #[must_use]
    pub fn extensions(&self) -> Vec<(String, String)> {
        self.extension_to_language
            .iter()
            .map(|(extension, language)| (normalize_extension(extension), language.clone()))
            .collect()
    }

    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
            .map_or(DEFAULT_REQUEST_TIMEOUT, Duration::from_millis)
    }

    #[must_use]
    pub fn startup_timeout(&self) -> Duration {
        self.startup_timeout
            .map_or(DEFAULT_STARTUP_TIMEOUT, Duration::from_millis)
    }

    #[must_use]
    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
            .map_or(DEFAULT_SHUTDOWN_TIMEOUT, Duration::from_millis)
    }

    #[must_use]
    pub fn max_restarts(&self) -> u32 {
        self.max_restarts.unwrap_or(DEFAULT_MAX_RESTARTS)
    }

    #[must_use]
    pub fn injects_diagnostics(&self) -> bool {
        self.diagnostics != Some(false)
    }

    /// The server's root directory: `workspaceFolder` against `cwd`, else
    /// `cwd`.
    #[must_use]
    pub fn root(&self, cwd: &Path) -> PathBuf {
        match self.workspace_folder.as_deref().map(str::trim) {
            Some(folder) if !folder.is_empty() => cwd.join(folder),
            Some(_) | None => cwd.to_path_buf(),
        }
    }
}

/// An extension as the lookup keys it: lowercase, with a leading dot.
#[must_use]
pub fn normalize_extension(extension: &str) -> String {
    let lower = extension.trim().to_lowercase();
    if lower.starts_with('.') {
        lower
    } else {
        format!(".{lower}")
    }
}

/// Where a server's configuration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Settings,
    ProjectFile,
}

impl ConfigSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::ProjectFile => PROJECT_CONFIG_FILE,
        }
    }
}

/// One configured server.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredServer {
    pub name: String,
    pub config: ServerConfig,
    pub source: ConfigSource,
}

/// Every server one session may start, and what could not be read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspConfig {
    /// In claim order: an extension two servers map belongs to the first.
    pub servers: Vec<ConfiguredServer>,
    /// One line per entry or file that could not be used.
    pub errors: Vec<String>,
    pub diagnostics_wait: Duration,
}

impl LspConfig {
    /// Read the configuration of a session in `cwd` under `agent_dir`:
    /// the merged `lspServers` setting, then a trusted workspace's
    /// `.lsp.json`. Reads files: call it off the async runtime.
    #[must_use]
    pub fn load(cwd: &Path, agent_dir: &Path) -> Self {
        let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
        let extra = &settings.settings().extra;
        let project = if pa_core::workspace_trust::evaluate(cwd, agent_dir).is_trusted() {
            read_project_file(cwd)
        } else {
            None
        };
        Self::from_parts(
            extra.get(SETTINGS_KEY),
            project.as_ref(),
            extra.get(DIAGNOSTICS_WAIT_KEY),
        )
    }

    /// Compose the configuration from the setting's value, the project
    /// file's parsed content (or its read error), and the wait setting.
    #[must_use]
    pub fn from_parts(
        settings: Option<&Value>,
        project: Option<&Result<Value, String>>,
        wait: Option<&Value>,
    ) -> Self {
        let mut config = Self {
            diagnostics_wait: wait
                .and_then(Value::as_u64)
                .map_or(DEFAULT_DIAGNOSTICS_WAIT, |ms| {
                    Duration::from_millis(ms).min(MAX_DIAGNOSTICS_WAIT)
                }),
            ..Self::default()
        };
        if let Some(value) = settings {
            config.add_servers(value, ConfigSource::Settings);
        }
        match project {
            Some(Ok(value)) => config.add_servers(value, ConfigSource::ProjectFile),
            Some(Err(error)) => config.errors.push(error.clone()),
            None => {}
        }
        config
    }

    fn add_servers(&mut self, value: &Value, source: ConfigSource) {
        let Some(entries) = value.as_object() else {
            self.errors.push(format!(
                "{}: expected an object of servers by name",
                source.as_str()
            ));
            return;
        };
        for (name, entry) in entries {
            if self.servers.iter().any(|server| &server.name == name) {
                continue;
            }
            let parsed = serde_json::from_value::<ServerConfig>(entry.clone())
                .map_err(|error| error.to_string())
                .and_then(|config| config.validate().map(|()| config));
            match parsed {
                Ok(config) => self.servers.push(ConfiguredServer {
                    name: name.clone(),
                    config,
                    source,
                }),
                Err(error) => self
                    .errors
                    .push(format!("{}: server '{name}': {error}", source.as_str())),
            }
        }
    }
}

/// `.lsp.json` in `cwd`: `None` when absent, else its JSON or why it could
/// not be read.
fn read_project_file(cwd: &Path) -> Option<Result<Value, String>> {
    let path = cwd.join(PROJECT_CONFIG_FILE);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(format!("{PROJECT_CONFIG_FILE}: {error}"))),
    };
    Some(
        serde_json::from_str(&content)
            .map_err(|error| format!("{PROJECT_CONFIG_FILE}: invalid JSON: {error}")),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn settings_win_over_the_project_file_and_bad_entries_are_reported() {
        let settings = json!({
            "rust": {"command": "rust-analyzer", "extensionToLanguage": {"rs": "rust"}},
            "bad": {"command": "my server", "extensionToLanguage": {".x": "x"}},
        });
        let project = Ok(json!({
            "rust": {"command": "other", "extensionToLanguage": {".rs": "rust"}},
            "py": {"command": "pyright-langserver", "args": ["--stdio"], "extensionToLanguage": {".PY": "python"}},
            "empty": {"command": "x", "extensionToLanguage": {}},
            "sock": {"command": "x", "transport": "socket", "extensionToLanguage": {".s": "s"}},
        }));
        let config = LspConfig::from_parts(Some(&settings), Some(&project), Some(&json!(500)));
        let names: Vec<_> = config
            .servers
            .iter()
            .map(|server| {
                (
                    server.name.as_str(),
                    server.config.command.as_str(),
                    server.source,
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("rust", "rust-analyzer", ConfigSource::Settings),
                ("py", "pyright-langserver", ConfigSource::ProjectFile),
            ]
        );
        assert_eq!(
            config.errors,
            vec![
                "settings: server 'bad': Command should not contain spaces. Use args array for arguments.".to_string(),
                ".lsp.json: server 'empty': missing required 'extensionToLanguage' field".to_string(),
                ".lsp.json: server 'sock': transport 'socket' is not supported; use 'stdio'".to_string(),
            ]
        );
        assert_eq!(
            config.servers[1].config.extensions(),
            vec![(".py".to_string(), "python".to_string())]
        );
        assert_eq!(
            config.servers[0].config.extensions(),
            vec![(".rs".to_string(), "rust".to_string())]
        );
        assert_eq!(config.diagnostics_wait, Duration::from_millis(500));
    }

    #[test]
    fn an_unreadable_project_file_is_an_error_line() {
        let config = LspConfig::from_parts(None, Some(&Err("bad".to_string())), None);
        assert_eq!(config.errors, vec!["bad".to_string()]);
        assert_eq!(config.diagnostics_wait, DEFAULT_DIAGNOSTICS_WAIT);
    }

    #[test]
    fn the_project_file_needs_a_trusted_workspace() {
        let home = tempfile::tempdir().unwrap();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let cwd = home.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        pa_core::workspace_trust::register_gated_project_file(
            PROJECT_CONFIG_FILE,
            "language servers",
        );
        std::fs::write(
            cwd.join(PROJECT_CONFIG_FILE),
            r#"{"py": {"command": "pyls", "extensionToLanguage": {".py": "python"}}}"#,
        )
        .unwrap();
        assert_eq!(LspConfig::load(&cwd, &agent_dir).servers, Vec::new());
        pa_core::workspace_trust::record(
            &cwd,
            &agent_dir,
            pa_core::workspace_trust::TrustDecision::Trusted,
        )
        .unwrap();
        let loaded = LspConfig::load(&cwd, &agent_dir);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].source, ConfigSource::ProjectFile);
    }
}
