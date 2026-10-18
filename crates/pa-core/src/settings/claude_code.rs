//! Claude Code's settings files as a settings source, behind one opt-in.
//!
//! With `"claudeCodeSettings": true` in the user settings
//! (`<agent dir>/settings.json`, top level or inside `permissions`, where
//! pa-permissions introduced it) a session also reads Claude Code's
//! layered settings files:
//!
//! | scope | file | needs workspace trust |
//! |---|---|---|
//! | managed | `/etc/claude-code/managed-settings.json` (macOS: `/Library/Application Support/ClaudeCode/`, Windows: `C:\Program Files\ClaudeCode\`) | no |
//! | local | `<cwd>/.claude/settings.local.json` | yes |
//! | project | `<cwd>/.claude/settings.json` | yes |
//! | user | `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json` | no |
//!
//! Claude Code's precedence, highest first: managed, local, project, user.
//! The opt-in is read from the user settings only, so a repository cannot
//! turn it on. Off by default: installing Claude Code never changes this
//! agent.
//!
//! Every consumer reads the files through [`ClaudeCodeSettings`]: the
//! permission rules (pa-permissions), the hooks (pa-hooks), and the keys
//! [`crate::settings::SettingsManager`] layers under this agent's own
//! settings (`statusLine`, `outputStyle`, `model`, `cleanupPeriodDays`,
//! `permissions.additionalDirectories`, `env`, `disableAllHooks`). A
//! project-scoped file applies only in a trusted workspace; while the
//! opt-in is on, its keys that run code or loosen anything
//! ([`GATED_PROJECT_KEYS`]) are part of what the workspace trust covers.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The opt-in key (top level, or inside `permissions`).
pub const OPT_IN_KEY: &str = "claudeCodeSettings";

/// The project-file keys a session honors that run code or loosen the
/// session: an untrusted workspace contributes none of them, and the trust
/// hash covers them. `permissions` counts through its loosening part only
/// (`allow`, `defaultMode`, `additionalDirectories`).
pub const GATED_PROJECT_KEYS: &[&str] = &["hooks", "statusLine", "env", "model", "outputStyle"];

/// The `permissions` keys a project file needs trust for.
const GATED_PERMISSION_KEYS: &[&str] = &["allow", "defaultMode", "additionalDirectories"];

/// One of Claude Code's settings scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaudeScope {
    Managed,
    Local,
    Project,
    User,
}

impl ClaudeScope {
    /// Every scope, highest precedence first.
    pub const ALL: [ClaudeScope; 4] = [
        ClaudeScope::Managed,
        ClaudeScope::Local,
        ClaudeScope::Project,
        ClaudeScope::User,
    ];

    /// How the scope is named to the user.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ClaudeScope::Managed => "Claude Code managed settings",
            ClaudeScope::Local => "Claude Code local settings",
            ClaudeScope::Project => "Claude Code project settings",
            ClaudeScope::User => "Claude Code user settings",
        }
    }

    /// Whether the file lives in the project (and so needs trust).
    #[must_use]
    pub fn project_scoped(self) -> bool {
        match self {
            ClaudeScope::Local | ClaudeScope::Project => true,
            ClaudeScope::Managed | ClaudeScope::User => false,
        }
    }

    /// The `--setting-sources` name that selects the scope; `None` for the
    /// managed file, which is always read.
    #[must_use]
    pub fn launch_source(self) -> Option<super::launch::SettingSource> {
        match self {
            ClaudeScope::Managed => None,
            ClaudeScope::Local => Some(super::launch::SettingSource::Local),
            ClaudeScope::Project => Some(super::launch::SettingSource::Project),
            ClaudeScope::User => Some(super::launch::SettingSource::User),
        }
    }
}

/// Where a session's Claude Code settings files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePaths {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// Claude Code's user directory (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
    pub claude_dir: Option<PathBuf>,
    /// The managed settings file.
    pub managed: Option<PathBuf>,
}

impl ClaudePaths {
    /// The paths of a session at `cwd` on this machine.
    #[must_use]
    pub fn resolve(cwd: &Path) -> Self {
        let home = pa_types::platform::home_dir();
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".claude")));
        let managed = if cfg!(target_os = "macos") {
            PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
        } else if cfg!(windows) {
            PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json")
        } else {
            PathBuf::from("/etc/claude-code/managed-settings.json")
        };
        ClaudePaths {
            cwd: cwd.to_path_buf(),
            home,
            claude_dir,
            managed: Some(managed),
        }
    }

    /// The file of `scope`; `None` when the scope has no location here.
    #[must_use]
    pub fn file(&self, scope: ClaudeScope) -> Option<PathBuf> {
        match scope {
            ClaudeScope::Managed => self.managed.clone(),
            ClaudeScope::Local => Some(self.cwd.join(".claude").join("settings.local.json")),
            ClaudeScope::Project => Some(self.cwd.join(".claude").join("settings.json")),
            ClaudeScope::User => self
                .claude_dir
                .as_ref()
                .map(|dir| dir.join("settings.json")),
        }
    }

    /// What relative paths in `scope`'s file resolve against: the project
    /// for project files, the home directory for the others.
    #[must_use]
    pub fn root(&self, scope: ClaudeScope) -> PathBuf {
        if scope.project_scoped() {
            self.cwd.clone()
        } else {
            self.home.clone().unwrap_or_else(|| self.cwd.clone())
        }
    }
}

/// A settings document: `Ok(None)` when the file is absent or empty.
///
/// # Errors
///
/// The file cannot be read, does not parse, or is not a JSON object.
pub fn read_document(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(document)) => Ok(Some(document)),
        Ok(_) => Err(format!("{}: not a JSON object", path.display())),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Whether a user settings document opts into Claude Code's files.
#[must_use]
pub fn opted_in_document(user_settings: &Value) -> bool {
    let flag = |value: Option<&Value>| value.and_then(Value::as_bool) == Some(true);
    flag(user_settings.get(OPT_IN_KEY))
        || flag(
            user_settings
                .get("permissions")
                .and_then(|permissions| permissions.get(OPT_IN_KEY)),
        )
}

/// Whether the user settings in `agent_dir` opt into Claude Code's files.
#[must_use]
pub fn opted_in(agent_dir: &Path) -> bool {
    read_document(&agent_dir.join("settings.json"))
        .ok()
        .flatten()
        .is_some_and(|document| opted_in_document(&Value::Object(document)))
}

/// The part of a project file's document that needs workspace trust
/// (empty when nothing does).
#[must_use]
pub fn gated_subset(document: &Map<String, Value>) -> Map<String, Value> {
    let mut gated: Map<String, Value> = document
        .iter()
        .filter(|(key, _)| GATED_PROJECT_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(Value::Object(permissions)) = document.get("permissions") {
        let loosening: Map<String, Value> = permissions
            .iter()
            .filter(|(key, _)| GATED_PERMISSION_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !loosening.is_empty() {
            gated.insert("permissions".to_string(), Value::Object(loosening));
        }
    }
    gated
}

/// One Claude Code settings file as read.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeFile {
    pub scope: ClaudeScope,
    pub path: PathBuf,
    /// The document; `None` when absent, empty or unreadable.
    pub document: Option<Map<String, Value>>,
    /// Why the file could not be read.
    pub problem: Option<String>,
}

/// One variable `env` sets, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    pub scope: ClaudeScope,
}

/// The variables a project-scoped file may not set (Claude Code's list of
/// the locations a process finds its configuration and home through).
const PROJECT_FORBIDDEN_ENV: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "HOMESHARE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "ALLUSERSPROFILE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "CDPATH",
    "PWD",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "CLAUDE_CONFIG_DIR",
];

/// Variable prefixes the harness owns: the kernel's own variables win, so
/// a settings file cannot point the kernel at another agent dir or store.
const HARNESS_ENV_PREFIXES: &[&str] = &["PRIME_AGENT_", "RLM_"];

/// Variables an override of is reported (never applied silently).
const WATCHED_ENV: &[&str] = &["PATH", "HOME", "USERPROFILE"];

/// Why `name` cannot be a variable name (Claude Code's check: not empty, no
/// `=`, no control characters).
fn invalid_env_name(name: &str) -> bool {
    name.is_empty()
        || name
            .chars()
            .any(|c| c == '=' || c.is_control() || ('\u{7f}'..='\u{9f}').contains(&c))
}

/// Claude Code's settings files of one session.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClaudeCodeSettings {
    /// Every file, highest precedence first (absent ones included).
    pub files: Vec<ClaudeFile>,
    /// Whether the workspace is trusted (project files apply).
    pub trusted: bool,
}

impl ClaudeCodeSettings {
    /// Read the files at `paths`. A project file that is the user file (a
    /// session in the home directory) is read once, as the user file.
    #[must_use]
    pub fn load(paths: &ClaudePaths, trusted: bool) -> Self {
        let user_file = paths
            .file(ClaudeScope::User)
            .and_then(|file| std::fs::canonicalize(file).ok());
        let mut files = Vec::new();
        for scope in ClaudeScope::ALL {
            let Some(path) = paths.file(scope) else {
                continue;
            };
            if !scope
                .launch_source()
                .is_none_or(super::launch::source_enabled)
            {
                continue;
            }
            if scope.project_scoped()
                && user_file.is_some()
                && std::fs::canonicalize(&path).ok() == user_file
            {
                continue;
            }
            let (document, problem) = match read_document(&path) {
                Ok(document) => (document, None),
                Err(problem) => (None, Some(problem)),
            };
            files.push(ClaudeFile {
                scope,
                path,
                document,
                problem,
            });
        }
        ClaudeCodeSettings { files, trusted }
    }

    /// The documents that apply, highest precedence first: project files
    /// only in a trusted workspace.
    pub fn applying(&self) -> impl Iterator<Item = (ClaudeScope, &Map<String, Value>)> {
        self.files.iter().filter_map(|file| {
            let document = file.document.as_ref()?;
            (!file.scope.project_scoped() || self.trusted).then_some((file.scope, document))
        })
    }

    /// The document of `scope`, if it applies.
    #[must_use]
    pub fn document(&self, scope: ClaudeScope) -> Option<&Map<String, Value>> {
        self.applying()
            .find(|(applying, _)| *applying == scope)
            .map(|(_, document)| document)
    }

    /// The highest-precedence value of `key` and its scope.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<(ClaudeScope, &Value)> {
        self.applying().find_map(|(scope, document)| {
            document
                .get(key)
                .filter(|value| !value.is_null())
                .map(|value| (scope, value))
        })
    }

    /// The merged settings, Claude Code's way: from the user file up to
    /// the managed one, objects merge key by key, arrays concatenate
    /// without repeats, and any other value is replaced.
    #[must_use]
    pub fn merged(&self) -> Map<String, Value> {
        let mut merged = Value::Object(Map::new());
        let documents: Vec<_> = self.applying().collect();
        for (_, document) in documents.into_iter().rev() {
            merge(&mut merged, &Value::Object(document.clone()));
        }
        match merged {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    /// Files that could not be read and project settings that wait for
    /// trust.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems: Vec<String> = self
            .files
            .iter()
            .filter_map(|file| file.problem.clone())
            .collect();
        if !self.trusted {
            for file in &self.files {
                let gated = file
                    .document
                    .as_ref()
                    .filter(|_| file.scope.project_scoped())
                    .map(gated_subset)
                    .unwrap_or_default();
                if !gated.is_empty() {
                    let keys: Vec<&str> = gated.keys().map(String::as_str).collect();
                    problems.push(format!(
                        "{}: {} wait for workspace trust",
                        file.scope.label(),
                        keys.join(", ")
                    ));
                }
            }
        }
        problems
    }

    /// Whether `disableAllHooks` is set to `true` by a source that applies;
    /// `Some(scope)` names the highest such source.
    #[must_use]
    pub fn disable_all_hooks(&self) -> Option<ClaudeScope> {
        self.applying().find_map(|(scope, document)| {
            (document.get("disableAllHooks") == Some(&Value::Bool(true))).then_some(scope)
        })
    }

    /// Whether the managed settings allow managed hooks only
    /// (`allowManagedHooksOnly`).
    #[must_use]
    pub fn managed_hooks_only(&self) -> bool {
        self.document(ClaudeScope::Managed)
            .and_then(|document| document.get("allowManagedHooksOnly"))
            == Some(&Value::Bool(true))
    }

    /// `permissions.additionalDirectories` of every applying file (relative
    /// entries against the file's root, `~/` against the home directory),
    /// higher sources first, without repeats.
    #[must_use]
    pub fn additional_directories(&self, paths: &ClaudePaths) -> Vec<PathBuf> {
        let mut directories: Vec<PathBuf> = Vec::new();
        for (scope, document) in self.applying() {
            let entries = document
                .get("permissions")
                .and_then(|permissions| permissions.get("additionalDirectories"))
                .and_then(Value::as_array);
            for entry in entries.into_iter().flatten().filter_map(Value::as_str) {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                let path = if entry == "~" {
                    paths.home.clone().unwrap_or_default()
                } else if let Some(rest) = entry.strip_prefix("~/") {
                    paths.home.clone().unwrap_or_default().join(rest)
                } else if Path::new(entry).is_absolute() {
                    PathBuf::from(entry)
                } else {
                    paths.root(scope).join(entry)
                };
                if !directories.contains(&path) {
                    directories.push(path);
                }
            }
        }
        directories
    }

    /// The session environment `env` sets, applied from the user file up
    /// to the managed one (a higher source's value wins), with the
    /// variables that were skipped or that override `PATH`/`HOME` reported.
    ///
    /// Values are strings (numbers and booleans are stringified, as Claude
    /// Code does); a name with `=` or control characters, any other value
    /// and a value holding NUL are skipped. A project file cannot set the
    /// home and configuration locations (`HOME`, `XDG_*`, ...; Claude
    /// Code's rule), and no file can set the harness's own
    /// `PRIME_AGENT_*`/`RLM_*` variables.
    #[must_use]
    pub fn env(&self) -> (Vec<EnvVar>, Vec<String>) {
        let mut vars: Vec<EnvVar> = Vec::new();
        let mut notes = Vec::new();
        let documents: Vec<_> = self.applying().collect();
        for (scope, document) in documents.into_iter().rev() {
            let label = scope.label();
            let Some(env) = document.get("env") else {
                continue;
            };
            let Some(env) = env.as_object() else {
                notes.push(format!("{label}: `env` must be an object; ignored"));
                continue;
            };
            for (name, value) in env {
                let value = match value {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    Value::Null | Value::Array(_) | Value::Object(_) => {
                        notes.push(format!("{label}: env {name} is not a string; ignored"));
                        continue;
                    }
                };
                let upper = name.to_ascii_uppercase();
                if invalid_env_name(name) || value.contains('\0') {
                    notes.push(format!(
                        "{label}: env {name:?} is not a valid variable; ignored"
                    ));
                    continue;
                }
                if HARNESS_ENV_PREFIXES
                    .iter()
                    .any(|prefix| upper.starts_with(prefix))
                {
                    notes.push(format!(
                        "{label}: env {name} is the harness's own variable; ignored"
                    ));
                    continue;
                }
                if scope.project_scoped() && PROJECT_FORBIDDEN_ENV.contains(&upper.as_str()) {
                    notes.push(format!(
                        "{label}: env {name} is ignored: project settings cannot set it (set it \
                         in ~/.claude/settings.json)"
                    ));
                    continue;
                }
                if WATCHED_ENV.contains(&upper.as_str()) {
                    notes.push(format!(
                        "{label}: env sets {name} for the kernel and its commands"
                    ));
                }
                vars.retain(|known| !known.name.eq_ignore_ascii_case(name));
                vars.push(EnvVar {
                    name: name.clone(),
                    value,
                    scope,
                });
            }
        }
        (vars, notes)
    }
}

/// A Claude Code `model` value as an Anthropic model id: a full
/// `claude-*` id (its `[1m]` context suffix dropped). Claude Code's aliases
/// (`opus`, `sonnet`, `haiku`, `opusplan`, `default`) name whatever its
/// release maps them to, so they are not guessed at.
#[must_use]
pub fn claude_model_id(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value
        .strip_suffix("[1m]")
        .or_else(|| value.strip_suffix("[1M]"))
        .unwrap_or(value);
    (value.len() > "claude-".len()
        && value.starts_with("claude-")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')))
    .then(|| value.to_string())
}

/// The provider a Claude Code `model` runs on.
pub const CLAUDE_MODEL_PROVIDER: &str = "anthropic";

/// What Claude Code's files contribute to the effective settings: only
/// what this agent's own settings (global and project) leave unset.
///
/// - `statusLine` and `outputStyle`: the highest Claude Code source's.
/// - `model`: a full `claude-*` id becomes `defaultProvider: anthropic` /
///   `defaultModel`, when neither of those is set; a soft preference (a
///   session falls back when the model is unavailable).
/// - `cleanupPeriodDays` (user and managed files): the age, in days, past
///   which sessions are archived (`sessionArchiveMaxAgeDays`; this agent
///   archives where Claude Code deletes). `0` is not taken (Claude Code
///   reads it as "keep no transcripts").
#[derive(Debug, Clone, Default)]
pub struct ClaudeLayer {
    /// The layer under this agent's own settings.
    pub settings: super::Settings,
    /// Whether `defaultModel` came from Claude Code's `model`.
    pub model: bool,
    /// `cleanupPeriodDays` as a `sessionArchiveMaxAgeDays` value.
    pub cleanup_period_days: Option<Value>,
    /// Values that were not taken, and why.
    pub notes: Vec<String>,
}

impl ClaudeLayer {
    /// Whether the layer changes nothing in the merged settings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.model && self.settings.status_line.is_none() && self.settings.output_style.is_none()
    }

    /// The layer `claude` contributes under `own`.
    #[must_use]
    pub fn derive(claude: &ClaudeCodeSettings, own: &super::Settings) -> Self {
        let mut layer = ClaudeLayer::default();
        if own.status_line.is_none() {
            match claude.get("statusLine") {
                Some((_, value @ Value::Object(_))) => {
                    layer.settings.status_line = Some(value.clone());
                }
                Some((scope, _)) => layer
                    .notes
                    .push(format!("{}: statusLine must be an object", scope.label())),
                None => {}
            }
        }
        if own.output_style.is_none() {
            if let Some((_, Value::String(style))) = claude.get("outputStyle") {
                layer.settings.output_style = Some(style.clone());
            }
        }
        if own.default_model.is_none() && own.default_provider.is_none() {
            if let Some((scope, Value::String(model))) = claude.get("model") {
                match claude_model_id(model) {
                    Some(id) => {
                        layer.settings.default_provider = Some(CLAUDE_MODEL_PROVIDER.to_string());
                        layer.settings.default_model = Some(id);
                        layer.model = true;
                    }
                    None => layer.notes.push(format!(
                        "{}: model {model:?} is not a full Claude model id (set defaultModel \
                         instead)",
                        scope.label()
                    )),
                }
            }
        }
        if own.session_archive_max_age_days.is_none() {
            let cleanup = claude.applying().find_map(|(scope, document)| {
                (!scope.project_scoped())
                    .then(|| {
                        document
                            .get("cleanupPeriodDays")
                            .map(|value| (scope, value))
                    })
                    .flatten()
            });
            match cleanup {
                Some((_, Value::Number(days))) if days.as_u64().is_some_and(|days| days > 0) => {
                    layer.cleanup_period_days = Some(Value::Number(days.clone()));
                }
                Some((scope, value)) => layer.notes.push(format!(
                    "{}: cleanupPeriodDays {value} is not a positive whole number of days",
                    scope.label()
                )),
                None => {}
            }
        }
        layer
    }
}

/// Claude Code's settings merge (see [`ClaudeCodeSettings::merged`]).
fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                if value.is_null() {
                    continue;
                }
                match base.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(over)) => {
            for value in over {
                if !base.contains(value) {
                    base.push(value.clone());
                }
            }
        }
        (base, over) => *base = over.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Tree {
        _dir: tempfile::TempDir,
        paths: ClaudePaths,
    }

    impl Tree {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("project");
            let home = dir.path().join("home");
            std::fs::create_dir_all(cwd.join(".claude")).unwrap();
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            let paths = ClaudePaths {
                cwd,
                claude_dir: Some(home.join(".claude")),
                home: Some(home),
                managed: Some(dir.path().join("managed-settings.json")),
            };
            Tree { _dir: dir, paths }
        }

        fn write(&self, scope: ClaudeScope, value: &Value) {
            std::fs::write(self.paths.file(scope).unwrap(), value.to_string()).unwrap();
        }
    }

    #[test]
    fn the_opt_in_is_read_at_the_top_level_or_under_permissions() {
        assert!(opted_in_document(&json!({ "claudeCodeSettings": true })));
        assert!(opted_in_document(
            &json!({ "permissions": { "claudeCodeSettings": true } })
        ));
        assert!(!opted_in_document(&json!({ "claudeCodeSettings": "yes" })));
        assert!(!opted_in_document(&json!({})));
        let dir = tempfile::tempdir().unwrap();
        assert!(!opted_in(dir.path()));
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"claudeCodeSettings":true}"#,
        )
        .unwrap();
        assert!(opted_in(dir.path()));
    }

    #[test]
    fn precedence_is_managed_local_project_user_and_arrays_concatenate() {
        let tree = Tree::new();
        tree.write(
            ClaudeScope::User,
            &json!({ "model": "u", "outputStyle": "Explanatory", "permissions": { "allow": ["A"] } }),
        );
        tree.write(
            ClaudeScope::Project,
            &json!({ "model": "p", "permissions": { "allow": ["B", "A"] } }),
        );
        tree.write(ClaudeScope::Local, &json!({ "model": "l" }));
        tree.write(ClaudeScope::Managed, &json!({ "cleanupPeriodDays": 9 }));
        let settings = ClaudeCodeSettings::load(&tree.paths, true);
        assert_eq!(
            settings.get("model"),
            Some((ClaudeScope::Local, &json!("l")))
        );
        assert_eq!(
            settings.merged(),
            json!({
                "model": "l",
                "outputStyle": "Explanatory",
                "permissions": { "allow": ["A", "B"] },
                "cleanupPeriodDays": 9,
            })
            .as_object()
            .unwrap()
            .clone()
        );
        // Untrusted: the project files drop out, and say why.
        let untrusted = ClaudeCodeSettings::load(&tree.paths, false);
        assert_eq!(
            untrusted.get("model"),
            Some((ClaudeScope::User, &json!("u")))
        );
        assert_eq!(
            untrusted.problems(),
            vec![
                "Claude Code local settings: model wait for workspace trust".to_string(),
                "Claude Code project settings: model, permissions wait for workspace trust"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn a_broken_file_is_a_problem_not_a_failure() {
        let tree = Tree::new();
        std::fs::write(tree.paths.file(ClaudeScope::User).unwrap(), "{nope").unwrap();
        tree.write(ClaudeScope::Project, &json!(["not", "an", "object"]));
        let settings = ClaudeCodeSettings::load(&tree.paths, true);
        assert_eq!(settings.applying().count(), 0);
        assert_eq!(settings.problems().len(), 2);
    }

    #[test]
    fn env_layers_upward_and_refuses_what_a_project_cannot_set() {
        let tree = Tree::new();
        tree.write(
            ClaudeScope::User,
            &json!({ "env": { "A": "user", "PATH": "/opt/bin", "N": 3, "B": true } }),
        );
        tree.write(
            ClaudeScope::Project,
            &json!({ "env": {
                "A": "project",
                "HOME": "/tmp/x",
                "PRIME_AGENT_CODING_AGENT_DIR": "/x",
                "BAD=NAME": "v",
                "L": ["list"],
            } }),
        );
        let (vars, notes) = ClaudeCodeSettings::load(&tree.paths, true).env();
        let pairs: Vec<(&str, &str, ClaudeScope)> = vars
            .iter()
            .map(|var| (var.name.as_str(), var.value.as_str(), var.scope))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("PATH", "/opt/bin", ClaudeScope::User),
                ("N", "3", ClaudeScope::User),
                ("B", "true", ClaudeScope::User),
                ("A", "project", ClaudeScope::Project),
            ]
        );
        assert_eq!(
            notes,
            vec![
                "Claude Code user settings: env sets PATH for the kernel and its commands"
                    .to_string(),
                "Claude Code project settings: env HOME is ignored: project settings cannot set \
                 it (set it in ~/.claude/settings.json)"
                    .to_string(),
                "Claude Code project settings: env PRIME_AGENT_CODING_AGENT_DIR is the harness's \
                 own variable; ignored"
                    .to_string(),
                "Claude Code project settings: env \"BAD=NAME\" is not a valid variable; ignored"
                    .to_string(),
                "Claude Code project settings: env L is not a string; ignored".to_string(),
            ]
        );
        // An untrusted workspace's project env never applies.
        let (vars, _) = ClaudeCodeSettings::load(&tree.paths, false).env();
        assert!(vars.iter().all(|var| var.scope == ClaudeScope::User));
        assert_eq!(
            vars.iter()
                .find(|var| var.name == "A")
                .map(|var| var.value.as_str()),
            Some("user")
        );
    }

    #[test]
    fn additional_directories_resolve_against_each_files_root() {
        let tree = Tree::new();
        tree.write(
            ClaudeScope::User,
            &json!({ "permissions": { "additionalDirectories": ["notes", "/abs"] } }),
        );
        tree.write(
            ClaudeScope::Project,
            &json!({ "permissions": { "additionalDirectories": ["../shared", "/abs"] } }),
        );
        let paths = &tree.paths;
        assert_eq!(
            ClaudeCodeSettings::load(paths, true).additional_directories(paths),
            vec![
                paths.cwd.join("../shared"),
                PathBuf::from("/abs"),
                paths.home.clone().unwrap().join("notes"),
            ]
        );
        assert_eq!(
            ClaudeCodeSettings::load(paths, false).additional_directories(paths),
            vec![
                paths.home.clone().unwrap().join("notes"),
                PathBuf::from("/abs")
            ]
        );
    }

    #[test]
    fn disable_all_hooks_and_managed_hooks_only_name_their_source() {
        let tree = Tree::new();
        let settings = ClaudeCodeSettings::load(&tree.paths, true);
        assert_eq!(
            (settings.disable_all_hooks(), settings.managed_hooks_only()),
            (None, false)
        );
        tree.write(ClaudeScope::Project, &json!({ "disableAllHooks": true }));
        tree.write(
            ClaudeScope::Managed,
            &json!({ "allowManagedHooksOnly": true }),
        );
        let settings = ClaudeCodeSettings::load(&tree.paths, true);
        assert_eq!(
            (settings.disable_all_hooks(), settings.managed_hooks_only()),
            (Some(ClaudeScope::Project), true)
        );
    }

    #[test]
    fn model_ids_are_taken_and_aliases_are_not() {
        assert_eq!(
            [
                "claude-sonnet-4-5",
                "claude-opus-4-1-20250805[1m]",
                "opus",
                "sonnet[1m]",
                "claude-",
                "claude-x y",
            ]
            .map(claude_model_id),
            [
                Some("claude-sonnet-4-5".to_string()),
                Some("claude-opus-4-1-20250805".to_string()),
                None,
                None,
                None,
                None,
            ]
        );
    }

    #[test]
    fn the_layer_fills_only_what_the_agents_own_settings_leave_unset() {
        let tree = Tree::new();
        tree.write(
            ClaudeScope::User,
            &json!({
                "statusLine": { "type": "command", "command": "echo cc" },
                "outputStyle": "Explanatory",
                "model": "claude-sonnet-4-5",
                "cleanupPeriodDays": 14,
            }),
        );
        let claude = ClaudeCodeSettings::load(&tree.paths, true);
        let layer = ClaudeLayer::derive(&claude, &crate::settings::Settings::default());
        assert_eq!(
            (
                layer.settings.status_line.clone(),
                layer.settings.output_style.clone(),
                layer.settings.default_provider.clone(),
                layer.settings.default_model.clone(),
                layer.model,
                layer.cleanup_period_days,
            ),
            (
                Some(json!({ "type": "command", "command": "echo cc" })),
                Some("Explanatory".to_string()),
                Some("anthropic".to_string()),
                Some("claude-sonnet-4-5".to_string()),
                true,
                Some(json!(14)),
            )
        );
        let own = crate::settings::Settings {
            status_line: Some(json!({ "type": "command", "command": "echo own" })),
            output_style: Some("Learning".to_string()),
            default_provider: Some("openai".to_string()),
            session_archive_max_age_days: Some(json!(3)),
            ..Default::default()
        };
        let layer = ClaudeLayer::derive(&claude, &own);
        assert!(layer.is_empty());
        assert_eq!(layer.cleanup_period_days, None);
        // A project's cleanupPeriodDays is not this agent's (a global-only
        // setting); an alias model is reported, not guessed.
        tree.write(
            ClaudeScope::User,
            &json!({ "model": "opus", "cleanupPeriodDays": 0 }),
        );
        tree.write(ClaudeScope::Project, &json!({ "cleanupPeriodDays": 5 }));
        let claude = ClaudeCodeSettings::load(&tree.paths, true);
        let layer = ClaudeLayer::derive(&claude, &crate::settings::Settings::default());
        assert_eq!(
            (layer.model, layer.cleanup_period_days, layer.notes),
            (
                false,
                None,
                vec![
                    "Claude Code user settings: model \"opus\" is not a full Claude model id \
                     (set defaultModel instead)"
                        .to_string(),
                    "Claude Code user settings: cleanupPeriodDays 0 is not a positive whole \
                     number of days"
                        .to_string(),
                ]
            )
        );
    }

    #[test]
    fn a_session_in_the_home_directory_reads_the_user_file_once() {
        let tree = Tree::new();
        let paths = ClaudePaths {
            cwd: tree.paths.home.clone().unwrap(),
            ..tree.paths.clone()
        };
        tree.write(ClaudeScope::User, &json!({ "model": "u" }));
        let settings = ClaudeCodeSettings::load(&paths, true);
        let scopes: Vec<ClaudeScope> = settings.applying().map(|(scope, _)| scope).collect();
        assert_eq!(scopes, vec![ClaudeScope::User]);
    }
}
