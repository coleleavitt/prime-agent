//! Where permission settings come from, and how they layer.
//!
//! The `permissions` key, Claude Code's shape, read from (highest first):
//!
//! | source | file | scope |
//! |---|---|---|
//! | Claude Code managed | `/etc/claude-code/managed-settings.json` (macOS: `/Library/Application Support/ClaudeCode/`) | opt-in |
//! | local | `<cwd>/.prime/agent/settings.local.json` | project |
//! | project | `<cwd>/.prime/agent/settings.json` | project |
//! | user | `<agent dir>/settings.json` | user |
//! | Claude Code local | `<cwd>/.claude/settings.local.json` | project, opt-in |
//! | Claude Code project | `<cwd>/.claude/settings.json` | project, opt-in |
//! | Claude Code user | `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json` | user, opt-in |
//!
//! The Claude Code files are read only when the user settings opt in
//! (`"claudeCodeSettings": true`, top level or inside `permissions`): their
//! rules would otherwise start refusing or asking in sessions that never
//! did. The opt-in and the files' locations are pa-core's shared reader
//! ([`pa_core::settings::claude_code`]), which the hooks and the other
//! Claude Code keys go through too.
//!
//! Rules from every source apply together (deny over ask over allow, not
//! source over source); `defaultMode` and `additionalDirectories` take the
//! highest source that sets them. A project-scoped file can only tighten an
//! untrusted workspace: its `deny` and `ask` rules apply, its `allow`
//! rules, `defaultMode` and `additionalDirectories` wait for workspace
//! trust. No project-scoped file may default to `bypassPermissions`, and
//! only the user settings choose the fallback.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::mode::PermissionMode;
use crate::paths::PathAnchors;
use crate::rule::{RuleBehavior, RuleValue, Validation, validate};

/// The settings file "always allow" and `/permissions allow` write to,
/// relative to the session's working directory.
pub const LOCAL_SETTINGS_FILE: &str = ".prime/agent/settings.local.json";

/// One settings source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    ClaudeManaged,
    Local,
    Project,
    User,
    ClaudeLocal,
    ClaudeProject,
    ClaudeUser,
    /// Answers and commands of this session, not written to a file.
    Session,
}

impl SourceKind {
    /// How a source is named to the user (Claude Code's `VHe` wording).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::ClaudeManaged => "Claude Code managed settings",
            SourceKind::Local => "local settings",
            SourceKind::Project => "project settings",
            SourceKind::User => "user settings",
            SourceKind::ClaudeLocal => "Claude Code local settings",
            SourceKind::ClaudeProject => "Claude Code project settings",
            SourceKind::ClaudeUser => "Claude Code user settings",
            SourceKind::Session => "this session",
        }
    }

    /// Whether the file lives in the project (and so needs workspace trust
    /// to loosen anything).
    #[must_use]
    pub fn project_scoped(self) -> bool {
        match self {
            SourceKind::Local
            | SourceKind::Project
            | SourceKind::ClaudeLocal
            | SourceKind::ClaudeProject => true,
            SourceKind::ClaudeManaged
            | SourceKind::User
            | SourceKind::ClaudeUser
            | SourceKind::Session => false,
        }
    }
}

/// An action no rule covers: what it gets in the default mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Run it (prime-agent's behavior without permission rules).
    Allow,
    /// Ask, as Claude Code does (read-only commands still run).
    Ask,
}

impl Fallback {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Fallback::Allow => "allow",
            Fallback::Ask => "ask",
        }
    }
}

/// One rule with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedRule {
    pub behavior: RuleBehavior,
    pub value: RuleValue,
    pub source: SourceKind,
    /// What `/`-anchored path patterns resolve against.
    pub root: PathBuf,
}

/// The files a session reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePaths {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub home: Option<PathBuf>,
    /// Claude Code's user settings directory.
    pub claude_dir: Option<PathBuf>,
    /// Claude Code's managed settings file.
    pub managed: Option<PathBuf>,
}

impl SourcePaths {
    /// The paths of a session at `cwd` on this machine.
    #[must_use]
    pub fn resolve(cwd: &Path, agent_dir: &Path) -> Self {
        let claude = pa_core::settings::claude_code::ClaudePaths::resolve(cwd);
        SourcePaths {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            home: claude.home,
            claude_dir: claude.claude_dir,
            managed: claude.managed,
        }
    }

    /// Where path patterns and paths resolve.
    #[must_use]
    pub fn anchors(&self) -> PathAnchors {
        PathAnchors {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
        }
    }

    /// The local settings file "always allow" writes to.
    #[must_use]
    pub fn local_file(&self) -> PathBuf {
        self.cwd.join(LOCAL_SETTINGS_FILE)
    }

    /// Every source file, highest first, with its kind and settings root.
    fn files(&self, claude_code: bool) -> Vec<(SourceKind, PathBuf, PathBuf)> {
        let home_root = self.home.clone().unwrap_or_else(|| self.cwd.clone());
        let mut files = Vec::new();
        if claude_code {
            if let Some(managed) = &self.managed {
                files.push((
                    SourceKind::ClaudeManaged,
                    managed.clone(),
                    PathBuf::from("/"),
                ));
            }
        }
        files.push((SourceKind::Local, self.local_file(), self.cwd.clone()));
        files.push((
            SourceKind::Project,
            self.cwd.join(".prime/agent/settings.json"),
            self.cwd.clone(),
        ));
        files.push((
            SourceKind::User,
            self.agent_dir.join("settings.json"),
            home_root.clone(),
        ));
        if claude_code {
            files.push((
                SourceKind::ClaudeLocal,
                self.cwd.join(".claude/settings.local.json"),
                self.cwd.clone(),
            ));
            files.push((
                SourceKind::ClaudeProject,
                self.cwd.join(".claude/settings.json"),
                self.cwd.clone(),
            ));
            if let Some(dir) = &self.claude_dir {
                files.push((SourceKind::ClaudeUser, dir.join("settings.json"), home_root));
            }
        }
        files
    }
}

/// The effective permission settings of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts the listing reports (bypass enabled, bypass disabled, Claude Code \
              files read, workspace trusted), not a state machine"
)]
pub struct PermissionSettings {
    pub rules: Vec<SourcedRule>,
    pub default_mode: PermissionMode,
    pub fallback: Fallback,
    /// Working directories besides the session's (acceptEdits covers them).
    pub additional_directories: Vec<PathBuf>,
    /// The user enabled `bypassPermissions` (it is in the cycle).
    pub bypass_available: bool,
    /// A source disabled `bypassPermissions` outright.
    pub bypass_disabled: bool,
    /// Whether the Claude Code files were read.
    pub claude_code: bool,
    /// Whether the workspace is trusted (project-scoped files may loosen).
    pub project_trusted: bool,
    /// Rules and keys that were skipped, and why.
    pub problems: Vec<String>,
    /// The files read, highest first, and whether each exists.
    pub files: Vec<(SourceKind, PathBuf, bool)>,
}

/// The modification times of a session's source files: a change reloads.
pub type Stamp = Vec<(PathBuf, Option<SystemTime>)>;

/// The current stamp of `paths`' files.
#[must_use]
pub fn stamp(paths: &SourcePaths) -> Stamp {
    paths
        .files(true)
        .into_iter()
        .map(|(_, path, _)| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
            (path, modified)
        })
        .collect()
}

/// The `permissions` object of a settings file; `Ok(None)` when the file or
/// the key is absent.
fn read_permissions(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let Some(document) = pa_core::settings::claude_code::read_document(path)? else {
        return Ok(None);
    };
    match document.get("permissions") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(permissions)) => Ok(Some(permissions.clone())),
        Some(_) => Err(format!(
            "{}: `permissions` must be an object",
            path.display()
        )),
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Whether the user settings opt into the Claude Code files (pa-core's
/// shared opt-in).
fn claude_code_opt_in(paths: &SourcePaths) -> bool {
    pa_core::settings::claude_code::opted_in(&paths.agent_dir)
}

/// Load the permission settings of a session. `project_trusted` is the
/// workspace's trust (see the module docs).
#[must_use]
pub fn load(paths: &SourcePaths, project_trusted: bool) -> PermissionSettings {
    let claude_code = claude_code_opt_in(paths);
    let mut settings = PermissionSettings {
        rules: Vec::new(),
        default_mode: PermissionMode::Default,
        fallback: Fallback::Allow,
        additional_directories: Vec::new(),
        bypass_available: false,
        bypass_disabled: false,
        claude_code,
        project_trusted,
        problems: Vec::new(),
        files: Vec::new(),
    };
    let mut documents = Vec::new();
    for (kind, path, root) in paths.files(claude_code) {
        match read_permissions(&path) {
            Ok(permissions) => {
                settings.files.push((kind, path.clone(), path.is_file()));
                if let Some(permissions) = permissions {
                    documents.push((kind, root, permissions));
                }
            }
            Err(problem) => {
                settings.files.push((kind, path, true));
                settings.problems.push(problem);
            }
        }
    }
    let managed_only = documents.iter().any(|(kind, _, permissions)| {
        *kind == SourceKind::ClaudeManaged
            && permissions
                .get("allowManagedPermissionRulesOnly")
                .and_then(Value::as_bool)
                == Some(true)
    });
    let mut mode_set = false;
    let mut directories_set = false;
    for (kind, root, permissions) in &documents {
        let document = Document {
            kind: *kind,
            root,
            permissions,
            loosening_allowed: !kind.project_scoped() || project_trusted,
        };
        if !managed_only || *kind == SourceKind::ClaudeManaged {
            add_rules(&mut settings, &document);
        }
        if permissions
            .get("disableBypassPermissionsMode")
            .and_then(Value::as_str)
            == Some("disable")
        {
            settings.bypass_disabled = true;
        }
        apply_default_mode(&mut settings, &document, &mut mode_set);
        if !directories_set {
            directories_set = apply_directories(&mut settings, &document, &paths.anchors());
        }
        if *kind == SourceKind::User {
            apply_fallback(&mut settings, &document);
        }
    }
    if settings.bypass_disabled {
        settings.bypass_available = false;
        if settings.default_mode == PermissionMode::BypassPermissions {
            settings.default_mode = PermissionMode::Default;
            settings
                .problems
                .push("bypassPermissions is disabled (disableBypassPermissionsMode)".to_string());
        }
    }
    settings
}

/// One source's `permissions` object while layering.
struct Document<'a> {
    kind: SourceKind,
    root: &'a Path,
    permissions: &'a Map<String, Value>,
    /// Whether it may loosen (a user file, or a trusted workspace's).
    loosening_allowed: bool,
}

/// Add a source's valid rules (its allow rules only when it may loosen).
fn add_rules(settings: &mut PermissionSettings, document: &Document<'_>) {
    let label = document.kind.label();
    for behavior in RuleBehavior::ALL {
        let texts = strings(document.permissions.get(behavior.key()));
        if behavior == RuleBehavior::Allow && !document.loosening_allowed && !texts.is_empty() {
            settings.problems.push(format!(
                "{label}: {} allow rule(s) wait for workspace trust",
                texts.len()
            ));
            continue;
        }
        for text in texts {
            match validate(&text, behavior) {
                Validation::Invalid { error, suggestion } => {
                    settings.problems.push(format!(
                        "{label}: {} rule {text:?} skipped: {error}{}",
                        behavior.key(),
                        suggestion.map_or_else(String::new, |hint| format!(" ({hint})"))
                    ));
                    continue;
                }
                Validation::Warning(warning) => {
                    settings.problems.push(format!("{label}: {warning}"));
                }
                Validation::Valid => {}
            }
            settings.rules.push(SourcedRule {
                behavior,
                value: RuleValue::parse(&text),
                source: document.kind,
                root: document.root.to_path_buf(),
            });
        }
    }
}

/// Take a source's `defaultMode` unless a higher source set one (`mode_set`).
fn apply_default_mode(
    settings: &mut PermissionSettings,
    document: &Document<'_>,
    mode_set: &mut bool,
) {
    let label = document.kind.label();
    let Some(text) = document
        .permissions
        .get("defaultMode")
        .and_then(Value::as_str)
    else {
        return;
    };
    match PermissionMode::parse(text) {
        None => settings
            .problems
            .push(format!("{label}: unknown defaultMode {text:?}")),
        Some(_) if !document.loosening_allowed => settings
            .problems
            .push(format!("{label}: defaultMode waits for workspace trust")),
        Some(PermissionMode::BypassPermissions) if document.kind.project_scoped() => {
            settings.problems.push(format!(
                "{label}: defaultMode bypassPermissions is honored only from user settings"
            ));
        }
        Some(mode) => {
            if mode == PermissionMode::BypassPermissions {
                settings.bypass_available = true;
            }
            if !*mode_set {
                settings.default_mode = mode;
                *mode_set = true;
            }
        }
    }
}

/// Take a source's `additionalDirectories` (relative ones against its
/// root); whether it set them.
fn apply_directories(
    settings: &mut PermissionSettings,
    document: &Document<'_>,
    anchors: &PathAnchors,
) -> bool {
    if !document.permissions.contains_key("additionalDirectories") {
        return false;
    }
    if !document.loosening_allowed {
        settings.problems.push(format!(
            "{}: additionalDirectories wait for workspace trust",
            document.kind.label()
        ));
        return false;
    }
    settings.additional_directories = strings(document.permissions.get("additionalDirectories"))
        .iter()
        .map(|dir| {
            if dir.starts_with('/') || dir.starts_with('~') {
                crate::paths::absolute(dir, anchors)
            } else {
                crate::paths::normalize(&document.root.join(dir))
            }
        })
        .collect();
    true
}

/// Take the user settings' `fallback`.
fn apply_fallback(settings: &mut PermissionSettings, document: &Document<'_>) {
    match document.permissions.get("fallback").and_then(Value::as_str) {
        None => {}
        Some("allow") => settings.fallback = Fallback::Allow,
        Some("ask") => settings.fallback = Fallback::Ask,
        Some(other) => settings.problems.push(format!(
            "{}: unknown fallback {other:?} (allow or ask)",
            document.kind.label()
        )),
    }
}

/// Add `rule` to `behavior`'s list in the settings file at `path` (created
/// when missing); `false` when it was already there.
///
/// # Errors
///
/// The file cannot be read, parsed or written.
pub fn persist_rule(path: &Path, behavior: RuleBehavior, rule: &str) -> Result<bool, String> {
    edit_document(path, |permissions| {
        let list = permissions
            .entry(behavior.key())
            .or_insert_with(|| json!([]));
        if !list.is_array() {
            *list = json!([]);
        }
        let Some(list) = list.as_array_mut() else {
            return false;
        };
        if list.iter().any(|known| known.as_str() == Some(rule)) {
            return false;
        }
        list.push(Value::String(rule.to_string()));
        true
    })
}

/// Remove `rule` from every list of the settings file at `path`; `false`
/// when it was in none.
///
/// # Errors
///
/// The file cannot be read, parsed or written.
pub fn remove_rule(path: &Path, rule: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    edit_document(path, |permissions| {
        let mut removed = false;
        for behavior in RuleBehavior::ALL {
            if let Some(list) = permissions
                .get_mut(behavior.key())
                .and_then(Value::as_array_mut)
            {
                let before = list.len();
                list.retain(|known| known.as_str() != Some(rule));
                removed |= list.len() != before;
            }
        }
        removed
    })
}

/// Apply `change` to the `permissions` object of the settings file at
/// `path`, writing it back (atomically) when the change reports one.
fn edit_document(
    path: &Path,
    change: impl FnOnce(&mut Map<String, Value>) -> bool,
) -> Result<bool, String> {
    let mut document = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?,
        Ok(_) => json!({}),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let Some(object) = document.as_object_mut() else {
        return Err(format!("{}: not a JSON object", path.display()));
    };
    let permissions = object.entry("permissions").or_insert_with(|| json!({}));
    if !permissions.is_object() {
        *permissions = json!({});
    }
    let Some(permissions) = permissions.as_object_mut() else {
        return Err(format!(
            "{}: `permissions` is not an object",
            path.display()
        ));
    };
    if !change(permissions) {
        return Ok(false);
    }
    let text = serde_json::to_string_pretty(&document)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })?;
    Ok(true)
}
