//! The `hooks` settings key, in Claude Code's shape:
//!
//! ```json
//! { "hooks": { "PreToolUse": [
//!     { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/bin/guard", "timeout": 10 }] }
//! ] } }
//! ```
//!
//! A command may run in the background (`"async": true`): the session does
//! not wait for it, and its context rides the next prompt. With
//! `"asyncRewake": true` (implies `async`) an exit code 2 delivers its
//! stderr to the model as a notice that wakes an idle session.
//!
//! User hooks (`<agent dir>/settings.json`) come first, then a trusted
//! project's (`.prime/agent/settings.json`; `hooks` is a gated key, so an
//! untrusted project contributes none). With the Claude Code opt-in
//! (`claudeCodeSettings`, read through `pa_core::settings::claude_code`)
//! Claude Code's files follow: `~/.claude/settings.json`, then a trusted
//! project's `.claude/settings.json` and `.claude/settings.local.json`, then
//! the managed settings. Every command carries its source's label
//! ([`HookSource`]), which its warnings and messages name.
//!
//! `disableAllHooks: true` turns every hook off; set anywhere but in the
//! managed settings it leaves the managed hooks running (Claude Code's
//! rule). The managed `allowManagedHooksOnly: true` keeps only the managed
//! hooks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pa_core::settings::claude_code::ClaudeScope;
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
    /// A tool call failed (an error result, a refused or failed host
    /// request), instead of `PostToolUse`.
    PostToolUseFailure,
    PostToolBatch,
    /// A tool call or host request is about to be put to the user.
    PermissionRequest,
    Stop,
    /// The turn ended on a model API error.
    StopFailure,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Notification,
    SessionEnd,
    /// `--init` / `--maintenance` (`pa_core::features::run_setup`).
    Setup,
    /// An MCP server asks the user for input.
    Elicitation,
    /// An MCP elicitation was answered, before the answer is sent.
    ElicitationResult,
    /// Instruction files loaded into the session.
    InstructionsLoaded,
    /// The session's working directory moved.
    CwdChanged,
    /// A watched file changed.
    FileChanged,
    /// `/add-dir` added a working directory.
    DirectoryAdded,
    /// Creates a worktree instead of git (`pa_core::worktree`).
    WorktreeCreate,
    /// Removes a worktree a `WorktreeCreate` hook made.
    WorktreeRemove,
}

/// Claude Code's events this crate does not fire, and why: a configured
/// one is reported once and skipped (and the `/hooks` viewer lists them).
pub const EVENTS_WITHOUT_TRIGGER: [(&str, &str); 9] = [
    (
        "PermissionDenied",
        "fires after Claude Code's auto-mode classifier denies a call; Prime Agent has no classifier",
    ),
    (
        "UserPromptExpansion",
        "slash commands expand inside the session engine without a feature seam yet",
    ),
    ("PreModelSwitch", "model switches have no feature seam yet"),
    ("PostModelSwitch", "model switches have no feature seam yet"),
    (
        "TeammateIdle",
        "Prime Agent has no agent teams (children and siblings are not teammates)",
    ),
    (
        "TaskCreated",
        "the tasks skill keeps its list in the kernel; the host does not see task changes",
    ),
    (
        "TaskCompleted",
        "the tasks skill keeps its list in the kernel; the host does not see task changes",
    ),
    (
        "ConfigChange",
        "settings are read once per session; nothing watches them",
    ),
    (
        "MessageDisplay",
        "the client renders messages in its own process, where hooks do not run",
    ),
];

impl HookEvent {
    pub const ALL: [HookEvent; 24] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::PostToolBatch,
        HookEvent::PermissionRequest,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::Notification,
        HookEvent::SessionEnd,
        HookEvent::Setup,
        HookEvent::Elicitation,
        HookEvent::ElicitationResult,
        HookEvent::InstructionsLoaded,
        HookEvent::CwdChanged,
        HookEvent::FileChanged,
        HookEvent::DirectoryAdded,
        HookEvent::WorktreeCreate,
        HookEvent::WorktreeRemove,
    ];

    /// The settings key and the `hook_event_name` hooks receive.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PostToolBatch => "PostToolBatch",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::Notification => "Notification",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Setup => "Setup",
            HookEvent::Elicitation => "Elicitation",
            HookEvent::ElicitationResult => "ElicitationResult",
            HookEvent::InstructionsLoaded => "InstructionsLoaded",
            HookEvent::CwdChanged => "CwdChanged",
            HookEvent::FileChanged => "FileChanged",
            HookEvent::DirectoryAdded => "DirectoryAdded",
            HookEvent::WorktreeCreate => "WorktreeCreate",
            HookEvent::WorktreeRemove => "WorktreeRemove",
        }
    }

    /// Whether the event's matchers filter on a subject (Claude Code's): the
    /// tool name for the tool events, the `source` for `SessionStart` and
    /// `DirectoryAdded`, the `agent_type` for `SubagentStart`/`SubagentStop`,
    /// the `trigger` for `PreCompact`/`PostCompact`/`Setup`, the
    /// `notification_type` for `Notification`, the `reason` for
    /// `SessionEnd`, the `error` for `StopFailure`, the server for the
    /// elicitation events, the `load_reason` for `InstructionsLoaded`, the
    /// file name for `FileChanged`. The other events ignore matchers.
    fn has_matcher(self) -> bool {
        match self {
            HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure
            | HookEvent::PermissionRequest
            | HookEvent::SessionStart
            | HookEvent::SubagentStart
            | HookEvent::SubagentStop
            | HookEvent::PreCompact
            | HookEvent::PostCompact
            | HookEvent::Notification
            | HookEvent::SessionEnd
            | HookEvent::StopFailure
            | HookEvent::Setup
            | HookEvent::Elicitation
            | HookEvent::ElicitationResult
            | HookEvent::InstructionsLoaded
            | HookEvent::FileChanged
            | HookEvent::DirectoryAdded => true,
            HookEvent::UserPromptSubmit
            | HookEvent::PostToolBatch
            | HookEvent::Stop
            | HookEvent::CwdChanged
            | HookEvent::WorktreeCreate
            | HookEvent::WorktreeRemove => false,
        }
    }

    /// Whether the event is about one tool call (its hooks' `if` applies).
    #[must_use]
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionRequest
        )
    }

    /// Whether `prompt` and `agent` hooks may run on the event: Claude
    /// Code runs them only where a conversation exists (and an `agent` hook
    /// cannot answer a `PermissionRequest`).
    #[must_use]
    pub fn takes_model_hooks(self, kind: &HookKind) -> bool {
        let conversational = matches!(
            self,
            HookEvent::UserPromptSubmit
                | HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PostToolBatch
                | HookEvent::PermissionRequest
                | HookEvent::Stop
                | HookEvent::SubagentStop
        );
        match kind {
            HookKind::Command { .. } | HookKind::Http { .. } => true,
            HookKind::Prompt { .. } => conversational,
            HookKind::Agent { .. } => conversational && self != HookEvent::PermissionRequest,
        }
    }

    /// Whether a failing hook (`onFailure: "block"`) may block the event:
    /// Claude Code ignores it on the events that cannot be blocked.
    #[must_use]
    pub fn failure_can_block(self) -> bool {
        !matches!(
            self,
            HookEvent::Stop
                | HookEvent::SubagentStop
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::StopFailure
        )
    }
}

/// What runs for one hook entry (`type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookKind {
    /// `command`: a shell command (or `args`: an executable without a
    /// shell).
    Command {
        args: Option<Vec<String>>,
        shell: CommandShell,
    },
    /// `http`: the input is POSTed to `url`.
    Http {
        headers: Vec<(String, String)>,
        /// The variables header values may interpolate.
        allowed_env_vars: Vec<String>,
        /// The settings' HTTP hook policy.
        policy: Arc<crate::http::HttpPolicy>,
    },
    /// `prompt`: the session's model judges the input (`$ARGUMENTS`).
    Prompt {
        model: Option<String>,
        continue_on_block: bool,
    },
    /// `agent`: the session's model checks with read-only tools.
    Agent { model: Option<String> },
}

impl HookKind {
    /// The `type` spelling.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            HookKind::Command { .. } => "command",
            HookKind::Http { .. } => "http",
            HookKind::Prompt { .. } => "prompt",
            HookKind::Agent { .. } => "agent",
        }
    }
}

/// The interpreter a `command` hook runs in (`shell`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandShell {
    /// `sh -c` (`cmd /C` on Windows): the default.
    Default,
    /// `bash`: the user's `$SHELL`.
    Bash,
    /// `powershell`: `pwsh -Command`.
    PowerShell,
}

/// What a failing hook does (`onFailure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Reported; the action goes ahead (the default).
    Continue,
    /// Counts as exit code 2: the guarded action is refused.
    Block,
}

/// How the session waits for a hook command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    /// The event waits for the command's answer (the default).
    Blocking,
    /// `"async": true`: runs in the background; its context rides the next
    /// prompt, and its decisions are not applied.
    Async,
    /// `"asyncRewake": true`: [`HookMode::Async`], and an exit code 2
    /// delivers its stderr to the model as a notice that wakes an idle
    /// session.
    AsyncRewake,
}

/// Where a hook command was configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSource {
    /// `<agent dir>/settings.json`.
    User,
    /// `.prime/agent/settings.json`.
    Project,
    /// One of Claude Code's settings files.
    ClaudeCode(ClaudeScope),
    /// A `hooks` value parsed directly ([`HooksConfig::from_value`]).
    Inline,
}

impl HookSource {
    /// How the source is named in warnings and messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            HookSource::User => "user settings",
            HookSource::Project => "project settings",
            HookSource::ClaudeCode(scope) => scope.label(),
            HookSource::Inline => "settings",
        }
    }
}

/// One hook entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    /// What it runs: the command line, the `http` hook's URL, or the
    /// `prompt`/`agent` hook's prompt. Commands are deduplicated by it.
    pub command: String,
    pub kind: HookKind,
    pub timeout: Duration,
    pub mode: HookMode,
    /// Where it was configured.
    pub source: HookSource,
    /// Variables it runs with on top of the host's (Claude Code's `env`).
    pub env: Arc<Vec<(String, String)>>,
    /// `once`: runs once per session, then is skipped.
    pub once: bool,
    /// `statusMessage`: shown while it runs.
    pub status_message: Option<String>,
    /// `if`: a permission rule the tool call must match for it to run.
    pub condition: Option<crate::rule::ToolRule>,
    pub on_failure: OnFailure,
}

impl HookCommand {
    /// A blocking shell command from inline settings, with the defaults.
    #[must_use]
    pub fn shell(command: &str, timeout: Duration) -> Self {
        Self {
            command: command.to_string(),
            kind: HookKind::Command {
                args: None,
                shell: CommandShell::Default,
            },
            timeout,
            mode: HookMode::Blocking,
            source: HookSource::Inline,
            env: Arc::default(),
            once: false,
            status_message: None,
            condition: None,
            on_failure: OnFailure::Continue,
        }
    }

    /// The entry's identity for `once` and deduplication.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.type_name(), self.command)
    }
}

/// A matcher group: the subjects it applies to and its commands.
#[derive(Debug, Clone)]
struct Group {
    /// `None` matches every subject (an empty or `*` matcher, or an event
    /// without matchers).
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
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(rename = "allowedEnvVars", default)]
    allowed_env_vars: Option<Vec<String>>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(rename = "continueOnBlock", default)]
    continue_on_block: bool,
    /// Seconds.
    #[serde(default)]
    timeout: Option<f64>,
    #[serde(rename = "async", default)]
    run_async: bool,
    #[serde(rename = "asyncRewake", default)]
    async_rewake: bool,
    #[serde(default)]
    once: bool,
    #[serde(rename = "statusMessage", default)]
    status_message: Option<String>,
    #[serde(rename = "if", default)]
    condition: Option<String>,
    #[serde(rename = "onFailure", default)]
    on_failure: Option<String>,
}

/// The default timeout of a `prompt` hook (Claude Code's).
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// The default timeout of an `agent` hook (Claude Code's).
pub const AGENT_TIMEOUT: Duration = Duration::from_secs(60);

fn text(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

impl RawCommand {
    /// The entry as a hook; `Err` says why it is skipped.
    fn into_hook(
        self,
        event: HookEvent,
        source: HookSource,
        env: &Arc<Vec<(String, String)>>,
        http: &Arc<crate::http::HttpPolicy>,
    ) -> Result<Option<HookCommand>, String> {
        let kind_name = self.kind.as_deref().unwrap_or("command");
        let (command, kind, default_timeout) = match kind_name {
            "command" => {
                let Some(command) = text(self.command) else {
                    return Ok(None);
                };
                let shell = match self.shell.as_deref() {
                    None | Some("sh") => CommandShell::Default,
                    Some("bash") => CommandShell::Bash,
                    Some("powershell") => CommandShell::PowerShell,
                    Some(other) => return Err(format!("unknown shell {other:?}")),
                };
                let kind = HookKind::Command {
                    args: self.args,
                    shell,
                };
                (command, kind, DEFAULT_TIMEOUT)
            }
            "http" => {
                let Some(url) = text(self.url) else {
                    return Err("an `http` hook needs a `url`".to_string());
                };
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!("`http` hook url {url:?} is not http(s)"));
                }
                let kind = HookKind::Http {
                    headers: self.headers.unwrap_or_default().into_iter().collect(),
                    allowed_env_vars: self.allowed_env_vars.unwrap_or_default(),
                    policy: Arc::clone(http),
                };
                (url, kind, DEFAULT_TIMEOUT)
            }
            "prompt" | "agent" => {
                let Some(prompt) = text(self.prompt) else {
                    return Err(format!("a `{kind_name}` hook needs a `prompt`"));
                };
                let model = text(self.model);
                if kind_name == "prompt" {
                    let kind = HookKind::Prompt {
                        model,
                        continue_on_block: self.continue_on_block,
                    };
                    (prompt, kind, PROMPT_TIMEOUT)
                } else {
                    (prompt, HookKind::Agent { model }, AGENT_TIMEOUT)
                }
            }
            "mcp_tool" => {
                return Err(
                    "`mcp_tool` hooks are not supported (use a `command` hook that calls the \
                     server)"
                        .to_string(),
                );
            }
            other => return Err(format!("hook type {other:?} is not supported")),
        };
        if !event.takes_model_hooks(&kind) {
            return Err(format!(
                "{}-type hooks are not supported for {} events (no conversation is available); \
                 use a command-type hook instead",
                kind.type_name(),
                event.name()
            ));
        }
        let condition = match self.condition.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(rule) => Some(
                crate::rule::ToolRule::parse(rule)
                    .ok_or_else(|| format!("`if` {rule:?} is not a permission rule"))?,
            ),
        };
        let on_failure = match self.on_failure.as_deref() {
            None | Some("continue") => OnFailure::Continue,
            Some("block") => OnFailure::Block,
            Some(other) => return Err(format!("unknown onFailure {other:?}")),
        };
        let timeout = self
            .timeout
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map_or(default_timeout, Duration::from_secs_f64);
        // Only commands run in the background.
        let background = matches!(kind, HookKind::Command { .. });
        let mode = if self.async_rewake && background {
            HookMode::AsyncRewake
        } else if self.run_async && background {
            HookMode::Async
        } else {
            HookMode::Blocking
        };
        Ok(Some(HookCommand {
            command,
            kind,
            timeout,
            mode,
            source,
            env: Arc::clone(env),
            once: self.once,
            status_message: text(self.status_message),
            condition,
            on_failure,
        }))
    }
}

/// Every configured hook, by event.
#[derive(Debug, Clone, Default)]
pub struct HooksConfig {
    groups: Vec<(HookEvent, Group)>,
    /// Entries that were skipped, for the session's diagnostics.
    pub problems: Vec<String>,
    /// The environment every command runs with (Claude Code's `env`).
    env: Arc<Vec<(String, String)>>,
    /// The HTTP hook policy every source contributed to.
    http: Arc<crate::http::HttpPolicy>,
}

impl HooksConfig {
    /// The hooks configured for a session at `cwd`.
    #[must_use]
    pub fn load(cwd: &Path, agent_dir: &Path) -> Self {
        Self::from_settings(&pa_core::settings::SettingsManager::create(cwd, agent_dir))
    }

    /// The hooks `settings` configure (see the module docs for the order
    /// and the switches).
    #[must_use]
    pub fn from_settings(settings: &pa_core::settings::SettingsManager) -> Self {
        let claude = settings.claude_code();
        let mut http = crate::http::HttpPolicy::default();
        http.merge(&settings.global_settings().extra);
        if settings.project_scope_trusted() {
            http.merge(&settings.project_settings().extra);
        }
        if let Some(claude) = claude {
            for scope in [
                ClaudeScope::User,
                ClaudeScope::Project,
                ClaudeScope::Local,
                ClaudeScope::Managed,
            ] {
                if let Some(document) = claude.document(scope) {
                    http.merge(document);
                }
            }
        }
        let mut config = HooksConfig {
            env: Arc::new(settings.get_claude_code_env()),
            http: Arc::new(http),
            ..HooksConfig::default()
        };
        let managed = claude
            .and_then(|claude| claude.document(ClaudeScope::Managed))
            .and_then(|document| document.get("hooks"));
        let managed_source = HookSource::ClaudeCode(ClaudeScope::Managed);
        if let Some(source) = settings.disable_all_hooks() {
            config
                .problems
                .push(format!("hooks are off: disableAllHooks is set in {source}"));
            if source != managed_source.label() {
                config.add(managed, managed_source);
            }
            return config;
        }
        if claude
            .is_some_and(pa_core::settings::claude_code::ClaudeCodeSettings::managed_hooks_only)
        {
            config.problems.push(
                "only managed hooks run: allowManagedHooksOnly is set in Claude Code managed \
                 settings"
                    .to_string(),
            );
            config.add(managed, managed_source);
            return config;
        }
        config.add(
            settings.global_settings().extra.get("hooks"),
            HookSource::User,
        );
        if settings.project_scope_trusted() {
            config.add(
                settings.project_settings().extra.get("hooks"),
                HookSource::Project,
            );
        }
        if let Some(claude) = claude {
            // Claude Code's order, lowest precedence first; `document`
            // leaves out an untrusted workspace's project files.
            for scope in [
                ClaudeScope::User,
                ClaudeScope::Project,
                ClaudeScope::Local,
                ClaudeScope::Managed,
            ] {
                config.add(
                    claude
                        .document(scope)
                        .and_then(|document| document.get("hooks")),
                    HookSource::ClaudeCode(scope),
                );
            }
        }
        config
    }

    /// Parse one settings document's `hooks` value.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let mut config = HooksConfig::default();
        config.add(Some(value), HookSource::Inline);
        config
    }

    fn add(&mut self, value: Option<&Value>, source: HookSource) {
        let scope = source.label();
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
                match EVENTS_WITHOUT_TRIGGER.iter().find(|(name, _)| name == key) {
                    Some((_, why)) => self.problems.push(format!(
                        "{scope} hooks: event {key} never fires in Prime Agent: {why}"
                    )),
                    None => self
                        .problems
                        .push(format!("{scope} hooks: event {key} is not supported")),
                }
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
                    Some(_) if !event.has_matcher() => None,
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
                let mut commands = Vec::new();
                for raw in group.hooks {
                    match raw.into_hook(event, source, &self.env, &self.http) {
                        Ok(Some(hook)) => commands.push(hook),
                        Ok(None) => {}
                        Err(problem) => self
                            .problems
                            .push(format!("{scope} hooks.{key}: {problem}")),
                    }
                }
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

    /// The commands `event` runs for `subject` (the tool name, the source,
    /// the agent type, ...; `None` for events without one), in
    /// configuration order, each command once.
    #[must_use]
    pub fn commands(&self, event: HookEvent, subject: Option<&str>) -> Vec<HookCommand> {
        let mut commands: Vec<HookCommand> = Vec::new();
        for (listening, group) in &self.groups {
            if *listening != event {
                continue;
            }
            let applies = match (&group.matcher, subject) {
                (None, _) => true,
                (Some(matcher), Some(subject)) => matcher.is_match(subject),
                (Some(_), None) => false,
            };
            if !applies {
                continue;
            }
            for command in &group.commands {
                if !commands.iter().any(|known| known.key() == command.key()) {
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
        HookCommand::shell(text, Duration::from_secs(seconds))
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
    fn every_claude_code_event_parses_and_matchers_select_their_subject() {
        let config = HooksConfig::from_value(&json!({
            "SessionStart": [{ "matcher": "resume", "hooks": [{ "type": "command", "command": "on-resume" }] }],
            "SubagentStart": [{ "matcher": "review.*", "hooks": [{ "type": "command", "command": "sub" }] }],
            "SubagentStop": [{ "hooks": [{ "type": "command", "command": "sub-stop" }] }],
            "PreCompact": [{ "matcher": "manual", "hooks": [{ "type": "command", "command": "pre" }] }],
            "PostCompact": [{ "hooks": [{ "type": "command", "command": "post" }] }],
            "Notification": [{ "matcher": "background_task_completed", "hooks": [{ "type": "command", "command": "note" }] }],
            "SessionEnd": [{ "hooks": [{ "type": "command", "command": "end" }] }],
            "PostToolBatch": [{ "matcher": "ignored", "hooks": [{ "type": "command", "command": "batch" }] }],
            "Stop": [{ "matcher": "ignored", "hooks": [
                { "type": "command", "command": "bg", "async": true },
                { "type": "command", "command": "wake", "asyncRewake": true, "timeout": 2 },
            ] }],
        }));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        assert_eq!(
            config.commands(HookEvent::SessionStart, Some("resume")),
            vec![command("on-resume", 60)]
        );
        assert!(
            config
                .commands(HookEvent::SessionStart, Some("startup"))
                .is_empty()
        );
        assert_eq!(
            config.commands(HookEvent::SubagentStart, Some("reviewer")),
            vec![command("sub", 60)]
        );
        assert!(
            config
                .commands(HookEvent::SubagentStart, Some("worker"))
                .is_empty()
        );
        assert!(
            config
                .commands(HookEvent::PreCompact, Some("auto"))
                .is_empty()
        );
        assert_eq!(
            config.commands(HookEvent::PreCompact, Some("manual")),
            vec![command("pre", 60)]
        );
        assert_eq!(
            config.commands(HookEvent::Notification, Some("background_task_completed")),
            vec![command("note", 60)]
        );
        // Events without matchers ignore them.
        assert_eq!(
            config.commands(HookEvent::PostToolBatch, None),
            vec![command("batch", 60)]
        );
        assert_eq!(
            config.commands(HookEvent::Stop, None),
            vec![
                HookCommand {
                    mode: HookMode::Async,
                    ..command("bg", 60)
                },
                HookCommand {
                    mode: HookMode::AsyncRewake,
                    ..command("wake", 2)
                },
            ]
        );
        for event in HookEvent::ALL {
            assert_eq!(
                config.has(event),
                matches!(
                    event,
                    HookEvent::SessionStart
                        | HookEvent::SubagentStart
                        | HookEvent::SubagentStop
                        | HookEvent::PreCompact
                        | HookEvent::PostCompact
                        | HookEvent::Notification
                        | HookEvent::SessionEnd
                        | HookEvent::PostToolBatch
                        | HookEvent::Stop
                ),
                "{event:?}"
            );
        }
    }

    #[test]
    fn unsupported_entries_are_reported_and_skipped() {
        let config = HooksConfig::from_value(&json!({
            "TeammateIdle": [{ "hooks": [{ "type": "command", "command": "x" }] }],
            "NoSuchEvent": [{ "hooks": [{ "type": "command", "command": "x" }] }],
            "PreToolUse": [
                { "matcher": "(", "hooks": [{ "type": "command", "command": "x" }] },
                { "hooks": [{ "type": "mcp_tool", "server": "s", "tool": "t" }, { "type": "command", "command": "kept" }] },
            ],
            "SessionStart": [{ "hooks": [{ "type": "prompt", "prompt": "judge it" }] }],
            "Stop": "not a list",
        }));
        assert_eq!(
            config.commands(HookEvent::PreToolUse, Some("Bash")),
            vec![command("kept", 60)]
        );
        assert!(!config.has(HookEvent::SessionStart));
        assert_eq!(config.problems.len(), 6, "{:?}", config.problems);
        assert!(
            config.problems[0].contains("TeammateIdle never fires in Prime Agent"),
            "{:?}",
            config.problems
        );
        assert!(
            config
                .problems
                .iter()
                .any(|problem| problem
                    .contains("prompt-type hooks are not supported for SessionStart")),
            "{:?}",
            config.problems
        );
    }

    /// Every hook type and field Claude Code's schema has, parsed.
    #[test]
    fn hook_types_and_fields_parse() {
        let config = HooksConfig::from_value(&json!({
            "PreToolUse": [{ "matcher": "Bash", "hooks": [
                { "type": "command", "command": "guard", "args": ["--strict"], "shell": "bash",
                  "once": true, "statusMessage": "Checking", "if": "Bash(git *)", "onFailure": "block" },
                { "type": "http", "url": "https://hooks.example.com/pre", "headers": { "Authorization": "Bearer $TOKEN" },
                  "allowedEnvVars": ["TOKEN"], "timeout": 5 },
                { "type": "prompt", "prompt": "Is $ARGUMENTS safe?", "model": "m2", "continueOnBlock": true },
                { "type": "agent", "prompt": "Verify it" },
                { "type": "http", "url": "ftp://nope" },
            ] }],
        }));
        assert_eq!(config.problems.len(), 1, "{:?}", config.problems);
        let hooks = config.commands(HookEvent::PreToolUse, Some("Bash"));
        assert_eq!(hooks.len(), 4);
        assert_eq!(
            hooks[0].kind,
            HookKind::Command {
                args: Some(vec!["--strict".to_string()]),
                shell: CommandShell::Bash,
            }
        );
        assert!(hooks[0].once);
        assert_eq!(hooks[0].status_message.as_deref(), Some("Checking"));
        assert_eq!(hooks[0].on_failure, OnFailure::Block);
        assert!(hooks[0].condition.is_some());
        assert_eq!(
            hooks[1].kind,
            HookKind::Http {
                headers: vec![("Authorization".to_string(), "Bearer $TOKEN".to_string())],
                allowed_env_vars: vec!["TOKEN".to_string()],
                policy: Arc::default(),
            }
        );
        assert_eq!(hooks[1].command, "https://hooks.example.com/pre");
        assert_eq!(hooks[1].timeout, Duration::from_secs(5));
        assert_eq!(
            hooks[2].kind,
            HookKind::Prompt {
                model: Some("m2".to_string()),
                continue_on_block: true,
            }
        );
        assert_eq!(hooks[2].timeout, PROMPT_TIMEOUT);
        assert_eq!(hooks[3].kind, HookKind::Agent { model: None });
        assert_eq!(hooks[3].timeout, AGENT_TIMEOUT);
        // An agent hook cannot answer a permission request.
        let config = HooksConfig::from_value(&json!({
            "PermissionRequest": [{ "hooks": [{ "type": "agent", "prompt": "x" }, { "type": "prompt", "prompt": "y" }] }],
        }));
        assert_eq!(config.problems.len(), 1, "{:?}", config.problems);
        assert_eq!(
            config
                .commands(HookEvent::PermissionRequest, Some("Bash"))
                .len(),
            1
        );
    }
}

/// Claude Code's settings files as hook sources: behind the opt-in, each
/// command labelled, project files only once the workspace is trusted, and
/// `disableAllHooks` / `allowManagedHooksOnly` as Claude Code reads them.
#[cfg(test)]
mod claude_code_sources {
    use std::path::{Path, PathBuf};

    use pa_core::settings::SettingsManager;
    use pa_core::settings::claude_code::{ClaudePaths, ClaudeScope};
    use serde_json::json;

    use super::*;

    struct Tree {
        _dir: tempfile::TempDir,
        cwd: PathBuf,
        agent_dir: PathBuf,
        paths: ClaudePaths,
    }

    fn stop(command: &str) -> Value {
        json!({ "Stop": [{ "hooks": [{ "type": "command", "command": command }] }] })
    }

    fn write(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    impl Tree {
        fn new(opt_in: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("repo");
            let agent_dir = dir.path().join("agent");
            let home = dir.path().join("home");
            std::fs::create_dir_all(cwd.join(".git")).unwrap();
            let paths = ClaudePaths {
                cwd: cwd.clone(),
                claude_dir: Some(home.join(".claude")),
                home: Some(home),
                managed: Some(dir.path().join("managed-settings.json")),
            };
            write(
                &agent_dir.join("settings.json"),
                &json!({ "claudeCodeSettings": opt_in, "hooks": stop("own") }),
            );
            write(
                &paths.file(ClaudeScope::User).unwrap(),
                &json!({ "hooks": stop("cc-user"), "env": { "CC_VAR": "1" } }),
            );
            write(
                &paths.file(ClaudeScope::Project).unwrap(),
                &json!({ "hooks": stop("cc-project") }),
            );
            write(
                paths.managed.as_ref().unwrap(),
                &json!({ "hooks": stop("cc-managed") }),
            );
            Tree {
                _dir: dir,
                cwd,
                agent_dir,
                paths,
            }
        }

        fn config(&self) -> HooksConfig {
            HooksConfig::from_settings(&SettingsManager::create_with_claude_paths(
                &self.cwd,
                &self.agent_dir,
                self.paths.clone(),
            ))
        }

        fn stop_sources(&self) -> Vec<(String, &'static str)> {
            self.config()
                .commands(HookEvent::Stop, None)
                .into_iter()
                .map(|hook| (hook.command, hook.source.label()))
                .collect()
        }

        fn trust(&self) {
            pa_core::workspace_trust::record(
                &self.cwd,
                &self.agent_dir,
                pa_core::workspace_trust::TrustDecision::Trusted,
            )
            .unwrap();
        }
    }

    fn owned(pairs: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
        pairs
            .iter()
            .map(|(command, label)| ((*command).to_string(), *label))
            .collect()
    }

    #[test]
    fn without_the_opt_in_only_the_agents_own_hooks_run() {
        let tree = Tree::new(false);
        assert_eq!(tree.stop_sources(), owned(&[("own", "user settings")]));
        assert!(
            tree.config().commands(HookEvent::Stop, None)[0]
                .env
                .is_empty()
        );
    }

    #[test]
    fn claude_code_hooks_carry_their_source_and_project_ones_wait_for_trust() {
        let tree = Tree::new(true);
        assert_eq!(
            tree.stop_sources(),
            owned(&[
                ("own", "user settings"),
                ("cc-user", "Claude Code user settings"),
                ("cc-managed", "Claude Code managed settings"),
            ])
        );
        tree.trust();
        assert_eq!(
            tree.stop_sources(),
            owned(&[
                ("own", "user settings"),
                ("cc-user", "Claude Code user settings"),
                ("cc-project", "Claude Code project settings"),
                ("cc-managed", "Claude Code managed settings"),
            ])
        );
        // Claude Code's `env` reaches every hook, its own ones too.
        let config = tree.config();
        for hook in config.commands(HookEvent::Stop, None) {
            assert_eq!(*hook.env, vec![("CC_VAR".to_string(), "1".to_string())]);
        }
    }

    #[test]
    fn disable_all_hooks_spares_managed_hooks_unless_managed_sets_it() {
        let tree = Tree::new(true);
        write(
            &tree.paths.file(ClaudeScope::User).unwrap(),
            &json!({ "hooks": stop("cc-user"), "disableAllHooks": true }),
        );
        let config = tree.config();
        assert_eq!(
            config.problems,
            vec!["hooks are off: disableAllHooks is set in Claude Code user settings".to_string()]
        );
        assert_eq!(
            tree.stop_sources(),
            owned(&[("cc-managed", "Claude Code managed settings")])
        );
        write(
            tree.paths.managed.as_ref().unwrap(),
            &json!({ "hooks": stop("cc-managed"), "disableAllHooks": true }),
        );
        assert_eq!(tree.stop_sources(), owned(&[]));
        // The agent's own settings can turn hooks off too.
        let tree = Tree::new(false);
        write(
            &tree.agent_dir.join("settings.json"),
            &json!({ "hooks": stop("own"), "disableAllHooks": true }),
        );
        assert_eq!(tree.stop_sources(), owned(&[]));
    }

    #[test]
    fn managed_hooks_only_keeps_the_managed_ones() {
        let tree = Tree::new(true);
        write(
            tree.paths.managed.as_ref().unwrap(),
            &json!({ "hooks": stop("cc-managed"), "allowManagedHooksOnly": true }),
        );
        assert_eq!(
            tree.stop_sources(),
            owned(&[("cc-managed", "Claude Code managed settings")])
        );
    }
}
