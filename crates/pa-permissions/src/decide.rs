//! The decision: an action against the rules and the mode, in Claude
//! Code's order (`Fpn`): a deny rule refuses; an ask rule asks (even in
//! `bypassPermissions`); then the mode (`bypassPermissions` allows, `plan`
//! refuses edits, `acceptEdits` allows edits in the working directories);
//! then an allow rule allows; then read-only commands run; and what is left
//! takes the fallback (`dontAsk` refuses it).

use std::path::{Path, PathBuf};

use crate::bash::BashPattern;
use crate::mode::PermissionMode;
use crate::paths::{PathAnchors, PathPattern, absolute, is_within};
use crate::rule::{BASH_TOOL, RuleBehavior, RuleValue};
use crate::settings::{Fallback, PermissionSettings, SourceKind, SourcedRule};
use crate::shell::{self, SimpleCommand, join_raw};

/// What an action asks to do, in Claude Code's tool vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A shell command (the kernel's `bash()`), run in `cwd`.
    Bash {
        command: String,
        cwd: PathBuf,
    },
    /// Write or edit a file.
    Edit {
        path: PathBuf,
    },
    /// Read a file.
    Read {
        path: PathBuf,
    },
    WebFetch {
        url: String,
    },
    WebSearch {
        query: String,
    },
    /// A tool of an MCP server.
    Mcp {
        server: String,
        tool: String,
    },
    /// Any other named tool (a model tool call).
    Tool {
        name: String,
    },
}

impl Action {
    /// The tool name rules name it by.
    #[must_use]
    pub fn tool_name(&self) -> String {
        match self {
            Action::Bash { .. } => BASH_TOOL.to_string(),
            Action::Edit { .. } => "Edit".to_string(),
            Action::Read { .. } => "Read".to_string(),
            Action::WebFetch { .. } => "WebFetch".to_string(),
            Action::WebSearch { .. } => "WebSearch".to_string(),
            Action::Mcp { server, tool } => format!("mcp__{server}__{tool}"),
            Action::Tool { name } => name.clone(),
        }
    }

    /// What it acts on, for messages.
    fn subject(&self) -> Option<String> {
        match self {
            Action::Bash { command, .. } => Some(command.trim().to_string()),
            Action::Edit { path } | Action::Read { path } => Some(path.display().to_string()),
            Action::WebFetch { url } => Some(url.clone()),
            Action::WebSearch { query } => Some(query.clone()),
            Action::Mcp { .. } | Action::Tool { .. } => None,
        }
    }

    /// Whether an action no rule covers takes the fallback (otherwise it
    /// runs): reads and other tools are not governed.
    fn governed(&self) -> bool {
        match self {
            Action::Bash { .. }
            | Action::Edit { .. }
            | Action::WebFetch { .. }
            | Action::WebSearch { .. }
            | Action::Mcp { .. } => true,
            Action::Read { .. } | Action::Tool { .. } => false,
        }
    }
}

/// The verdict on an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Ask the user; `remember` is the allow rule an "always allow" answer
    /// adds (offered only where an allow rule would stop the question).
    Ask {
        reason: String,
        remember: Option<String>,
    },
    Deny {
        reason: String,
    },
}

/// How a rule's content matches.
#[derive(Debug, Clone)]
enum Matcher {
    /// A bare rule: every use of the tool.
    Any,
    Bash(BashPattern),
    Path(PathPattern),
    /// `domain:<host>` (`*` wildcards allowed).
    Domain(String),
    /// Any other content: a `*` wildcard over the action's subject.
    Text(String),
    /// Content that cannot match (an unresolvable path pattern).
    Never,
}

/// One compiled rule.
#[derive(Debug, Clone)]
struct CompiledRule {
    behavior: RuleBehavior,
    value: RuleValue,
    source: SourceKind,
    /// The tool it covers: `Write`, `MultiEdit` and `NotebookEdit` rules
    /// cover `Edit`, `Glob` and `Grep` rules cover `Read`.
    tool: String,
    matcher: Matcher,
}

impl CompiledRule {
    fn describe(&self) -> String {
        format!("'{}' from {}", self.value, self.source.label())
    }
}

/// A `*` wildcard match of `text` against `pattern` (no other syntax).
fn glob_text(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if text.len() < first.len() + last.len() || !text.starts_with(first) || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(index) => rest = &rest[index + middle.len()..],
            None => return false,
        }
    }
    true
}

/// A URL's host.
fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

/// Programs that only read (Claude Code's read-only set, abridged).
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "wc",
    "pwd",
    "echo",
    "printf",
    "which",
    "whereis",
    "type",
    "file",
    "stat",
    "du",
    "df",
    "tree",
    "uniq",
    "cut",
    "tr",
    "diff",
    "cmp",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "date",
    "whoami",
    "id",
    "uname",
    "hostname",
    "printenv",
    "true",
    "false",
    "test",
    "[",
    "jq",
    "nl",
    "od",
    "xxd",
    "strings",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "column",
    "seq",
    "sleep",
];

/// Read-only `git` subcommands.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "describe",
    "grep",
    "shortlog",
];

/// Programs that write their file arguments.
const WRITE_PROGRAMS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "touch", "mkdir", "ln", "chmod", "chown", "truncate", "tee",
    "install",
];

/// Programs that read their file arguments.
const READ_PROGRAMS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "diff",
    "cmp",
    "stat",
    "file",
    "sort",
    "uniq",
    "cut",
    "base64",
    "xxd",
    "od",
    "strings",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "source",
    ".",
    "cp",
    "nl",
    "jq",
];

/// Programs whose first argument is a pattern or script, not a file.
const PATTERN_FIRST_PROGRAMS: &[&str] = &["grep", "egrep", "fgrep", "rg", "ag", "sed", "jq"];

/// Claude Code's acceptEdits file commands (`vTr`).
const ACCEPT_EDITS_PROGRAMS: &[&str] = &["mkdir", "touch", "rm", "rmdir", "mv", "cp", "sed"];

/// Pseudo-files output may go to without writing anything.
const DEVICE_TARGETS: &[&str] = &["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"];

/// The arguments of a part's program that are not options.
fn operands(part: &SimpleCommand) -> Vec<String> {
    let words = part.core_words(true);
    let mut operands = Vec::new();
    let mut options_done = false;
    for word in words.iter().skip(1) {
        if !options_done && word.value == "--" {
            options_done = true;
            continue;
        }
        if !options_done && word.value.starts_with('-') && word.value != "-" {
            continue;
        }
        operands.push(word.value.clone());
    }
    operands
}

/// The files a part writes: output redirections and a write program's
/// operands (`sed` only with `-i`).
fn written_files(part: &SimpleCommand) -> Vec<String> {
    let mut files: Vec<String> = part
        .writes
        .iter()
        .filter(|target| !DEVICE_TARGETS.contains(&target.as_str()))
        .cloned()
        .collect();
    let program = part.program().unwrap_or_default();
    if WRITE_PROGRAMS.contains(&program) {
        files.extend(operands(part));
    } else if program == "sed"
        && part
            .core_words(true)
            .iter()
            .any(|word| word.value.starts_with("-i") || word.value == "--in-place")
    {
        files.extend(operands(part).into_iter().skip(1));
    }
    files
}

/// The files a part reads: input redirections and a read program's
/// operands.
fn read_files(part: &SimpleCommand) -> Vec<String> {
    let mut files = part.reads.clone();
    let program = part.program().unwrap_or_default();
    if READ_PROGRAMS.contains(&program) {
        let mut operands = operands(part);
        if program == "cp" {
            operands.pop();
        }
        files.extend(operands);
    } else if PATTERN_FIRST_PROGRAMS.contains(&program) {
        files.extend(operands(part).into_iter().skip(1));
    }
    files
}

/// Whether a part only reads.
fn read_only(part: &SimpleCommand) -> bool {
    if part
        .writes
        .iter()
        .any(|target| !DEVICE_TARGETS.contains(&target.as_str()))
    {
        return false;
    }
    let words = part.core_words(true);
    let Some(program) = part.program() else {
        return false;
    };
    let values: Vec<&str> = words.iter().map(|word| word.value.as_str()).collect();
    match program {
        "git" => {
            let subcommand = values.iter().skip(1).find(|value| !value.starts_with('-'));
            subcommand.is_some_and(|subcommand| READ_ONLY_GIT.contains(subcommand))
                && !values
                    .iter()
                    .any(|value| *value == "-c" || value.starts_with("--output"))
        }
        "find" => !values.iter().any(|value| {
            matches!(
                *value,
                "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-delete"
                    | "-fprint"
                    | "-fprintf"
                    | "-fls"
                    | "-fprint0"
            )
        }),
        "sort" => !values
            .iter()
            .any(|value| value.starts_with("-o") || *value == "--output"),
        program => READ_ONLY_PROGRAMS.contains(&program),
    }
}

/// The allow rule that would cover `part`: its program and subcommand with
/// a trailing wildcard (`Bash(npm test *)`).
fn suggested_bash_rule(part: &SimpleCommand) -> Option<String> {
    let words = part.core_words(false);
    let first = words.first()?;
    if first.raw.contains('=') {
        return None;
    }
    let mut prefix = vec![first.raw.as_str()];
    if let Some(second) = words.get(1) {
        let plain = second
            .raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':');
        if plain && !second.raw.starts_with('-') {
            prefix.push(second.raw.as_str());
        }
    }
    Some(RuleValue::scoped(BASH_TOOL, &format!("{} *", prefix.join(" "))).to_string())
}

/// The allow rule that would cover an edit of `path`: its directory.
fn suggested_edit_rule(path: &Path, anchors: &PathAnchors) -> String {
    let dir = path.parent().unwrap_or(path);
    let content = match dir.strip_prefix(&anchors.cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => "./**".to_string(),
        Ok(relative) => format!("./{}/**", relative.display()),
        Err(_) => format!("/{}/**", dir.display()),
    };
    RuleValue::scoped("Edit", &content).to_string()
}

/// A session's permission policy: its rules compiled, its mode, and where
/// it works.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<CompiledRule>,
    mode: PermissionMode,
    fallback: Fallback,
    working_dirs: Vec<PathBuf>,
    anchors: PathAnchors,
    /// Path patterns that could not compile.
    pub problems: Vec<String>,
}

impl Policy {
    /// Compile `settings` (plus this session's own rules) for a session in
    /// `mode`.
    #[must_use]
    pub fn new(
        settings: &PermissionSettings,
        session_rules: &[SourcedRule],
        mode: PermissionMode,
        anchors: &PathAnchors,
    ) -> Self {
        let mut problems = Vec::new();
        let rules = settings
            .rules
            .iter()
            .chain(session_rules)
            .map(|rule| {
                let tool = match rule.value.tool.as_str() {
                    "Write" | "MultiEdit" | "NotebookEdit" => "Edit".to_string(),
                    "Glob" | "Grep" => "Read".to_string(),
                    other => other.to_string(),
                };
                let matcher = match (&rule.value.content, tool.as_str()) {
                    (None, _) => Matcher::Any,
                    (Some(content), BASH_TOOL) => Matcher::Bash(BashPattern::new(content)),
                    (Some(content), "Edit" | "Read") => {
                        match PathPattern::new(content, &rule.root, anchors) {
                            Ok(pattern) => Matcher::Path(pattern),
                            Err(problem) => {
                                problems.push(format!(
                                    "{} rule {}: {problem}",
                                    rule.source.label(),
                                    rule.value
                                ));
                                Matcher::Never
                            }
                        }
                    }
                    (Some(content), "WebFetch") => match content.strip_prefix("domain:") {
                        Some(domain) => Matcher::Domain(domain.trim().to_ascii_lowercase()),
                        None => Matcher::Text(content.clone()),
                    },
                    (Some(content), _) => Matcher::Text(content.clone()),
                };
                CompiledRule {
                    behavior: rule.behavior,
                    value: rule.value.clone(),
                    source: rule.source,
                    tool,
                    matcher,
                }
            })
            .collect();
        let mut working_dirs = vec![anchors.cwd.clone()];
        working_dirs.extend(settings.additional_directories.iter().cloned());
        Policy {
            rules,
            mode,
            fallback: settings.fallback,
            working_dirs,
            anchors: anchors.clone(),
            problems,
        }
    }

    #[must_use]
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Whether `rule` names `action`'s tool.
    fn covers_tool(rule: &CompiledRule, action: &Action) -> bool {
        match (rule.value.mcp(), action) {
            (
                Some((server, tool)),
                Action::Mcp {
                    server: called,
                    tool: called_tool,
                },
            ) => server == called && tool.is_none_or(|tool| tool == called_tool),
            (Some(_), _) => false,
            (None, _) => rule.tool == action.tool_name(),
        }
    }

    /// Whether `rule` matches a non-Bash `action`.
    fn matches(rule: &CompiledRule, action: &Action) -> bool {
        if !Self::covers_tool(rule, action) {
            return false;
        }
        match (&rule.matcher, action) {
            (Matcher::Any, _) => true,
            (Matcher::Path(pattern), Action::Edit { path } | Action::Read { path }) => {
                pattern.matches(path)
            }
            (Matcher::Domain(domain), Action::WebFetch { url }) => {
                host_of(url).is_some_and(|host| glob_text(domain, &host))
            }
            (Matcher::Text(text), _) => action
                .subject()
                .is_some_and(|subject| glob_text(text, &subject)),
            (Matcher::Bash(_) | Matcher::Path(_) | Matcher::Domain(_) | Matcher::Never, _) => false,
        }
    }

    fn rules_of(&self, behavior: RuleBehavior) -> impl Iterator<Item = &CompiledRule> {
        self.rules
            .iter()
            .filter(move |rule| rule.behavior == behavior)
    }

    /// Decide `action`.
    #[must_use]
    pub fn decide(&self, action: &Action) -> Decision {
        match action {
            Action::Bash { command, cwd } => self.decide_bash(command, cwd),
            _ => self.decide_simple(action),
        }
    }

    fn deny(action: &Action, rule: &CompiledRule) -> Decision {
        let tool = action.tool_name();
        let reason = match action.subject() {
            Some(subject) => format!(
                "Permission to use {tool} with {subject} has been denied by permission rule {}.",
                rule.describe()
            ),
            None => format!(
                "Permission to use {tool} has been denied by permission rule {}.",
                rule.describe()
            ),
        };
        Decision::Deny { reason }
    }

    /// An ask, or (in `dontAsk`) its refusal.
    fn ask(&self, tool: &str, reason: String, remember: Option<String>) -> Decision {
        if self.mode == PermissionMode::DontAsk {
            return Decision::Deny {
                reason: format!(
                    "{reason} (the permission mode is dontAsk: what would ask is refused; an \
                     allow rule for {tool} lets it run)"
                ),
            };
        }
        Decision::Ask { reason, remember }
    }

    fn ask_rule(&self, tool: &str, rule: &CompiledRule) -> Decision {
        self.ask(
            tool,
            format!(
                "Permission rule {} requires approval for this {tool} command",
                rule.describe()
            ),
            None,
        )
    }

    fn unapproved(tool: &str) -> String {
        format!("Prime Agent requested permissions to use {tool}, but you haven't granted it yet.")
    }

    fn in_working_dirs(&self, path: &Path) -> bool {
        self.working_dirs.iter().any(|dir| is_within(path, dir))
    }

    fn decide_simple(&self, action: &Action) -> Decision {
        let tool = action.tool_name();
        if let Some(rule) = self
            .rules_of(RuleBehavior::Deny)
            .find(|rule| Self::matches(rule, action))
        {
            return Self::deny(action, rule);
        }
        if let Some(rule) = self
            .rules_of(RuleBehavior::Ask)
            .find(|rule| Self::matches(rule, action))
        {
            return self.ask_rule(&tool, rule);
        }
        match (self.mode, action) {
            (PermissionMode::BypassPermissions, _) => return Decision::Allow,
            (PermissionMode::Plan, Action::Edit { path }) => {
                return Decision::Deny {
                    reason: format!(
                        "Cannot edit {} while in plan mode: the agent plans and edits nothing \
                         until the user leaves plan mode.",
                        path.display()
                    ),
                };
            }
            (PermissionMode::AcceptEdits, Action::Edit { path }) if self.in_working_dirs(path) => {
                return Decision::Allow;
            }
            _ => {}
        }
        if self
            .rules_of(RuleBehavior::Allow)
            .any(|rule| Self::matches(rule, action))
        {
            return Decision::Allow;
        }
        if !action.governed() {
            return Decision::Allow;
        }
        let remember = match action {
            Action::Edit { path } => Some(suggested_edit_rule(path, &self.anchors)),
            Action::WebFetch { url } => host_of(url)
                .map(|host| RuleValue::scoped("WebFetch", &format!("domain:{host}")).to_string()),
            Action::WebSearch { .. } | Action::Mcp { .. } => Some(tool.clone()),
            Action::Bash { .. } | Action::Read { .. } | Action::Tool { .. } => None,
        };
        self.fallback_decision(&tool, Self::unapproved(&tool), remember)
    }

    fn fallback_decision(&self, tool: &str, reason: String, remember: Option<String>) -> Decision {
        match (self.mode, self.fallback) {
            (PermissionMode::DontAsk, _) | (_, Fallback::Ask) => self.ask(tool, reason, remember),
            (_, Fallback::Allow) => Decision::Allow,
        }
    }

    /// Whether a Bash rule matches one part (in any of its readings).
    fn bash_rule_matches(rule: &CompiledRule, part: &SimpleCommand) -> bool {
        match &rule.matcher {
            Matcher::Any => true,
            Matcher::Bash(pattern) => {
                let readings: Vec<String> = if rule.behavior == RuleBehavior::Allow {
                    vec![
                        part.text.clone(),
                        part.without_redirections(),
                        join_raw(part.core_words(false)),
                    ]
                } else {
                    vec![
                        part.text.clone(),
                        part.without_redirections(),
                        join_raw(part.core_words(true)),
                    ]
                };
                let xargs = rule.behavior != RuleBehavior::Allow;
                readings
                    .iter()
                    .any(|reading| pattern.matches(reading, xargs))
            }
            Matcher::Path(_) | Matcher::Domain(_) | Matcher::Text(_) | Matcher::Never => false,
        }
    }

    /// The first rule of `behavior` that covers the command or one of its
    /// parts (paths included: an Edit rule over a file a part writes, a
    /// Read rule over one it reads), with what it matched.
    fn bash_hit(
        &self,
        behavior: RuleBehavior,
        command: &str,
        parts: &[SimpleCommand],
        anchors: &PathAnchors,
    ) -> Option<(&CompiledRule, Action)> {
        let whole = Action::Bash {
            command: command.to_string(),
            cwd: anchors.cwd.clone(),
        };
        for rule in self
            .rules_of(behavior)
            .filter(|rule| rule.tool == BASH_TOOL)
        {
            let exact_whole = matches!(&rule.matcher, Matcher::Bash(pattern) if pattern.matches(command.trim(), false));
            if exact_whole {
                return Some((rule, whole));
            }
            if let Some(part) = parts
                .iter()
                .find(|part| Self::bash_rule_matches(rule, part))
            {
                return Some((
                    rule,
                    Action::Bash {
                        command: part.text.clone(),
                        cwd: anchors.cwd.clone(),
                    },
                ));
            }
        }
        for part in parts {
            let edits = written_files(part).into_iter().map(|file| Action::Edit {
                path: absolute(&file, anchors),
            });
            let reads = read_files(part).into_iter().map(|file| Action::Read {
                path: absolute(&file, anchors),
            });
            for action in edits.chain(reads) {
                if let Some(rule) = self
                    .rules_of(behavior)
                    .find(|rule| Self::matches(rule, &action))
                {
                    return Some((rule, action));
                }
            }
        }
        None
    }

    fn decide_bash(&self, command: &str, cwd: &Path) -> Decision {
        let anchors = PathAnchors {
            cwd: if cwd.as_os_str().is_empty() {
                self.anchors.cwd.clone()
            } else {
                cwd.to_path_buf()
            },
            home: self.anchors.home.clone(),
        };
        let parsed = shell::parse(command);
        let parts = &parsed.commands;
        if let Some((rule, action)) = self.bash_hit(RuleBehavior::Deny, command, parts, &anchors) {
            return Self::deny(&action, rule);
        }
        if let Some((rule, action)) = self.bash_hit(RuleBehavior::Ask, command, parts, &anchors) {
            return self.ask_rule(&action.tool_name(), rule);
        }
        if self.mode == PermissionMode::BypassPermissions {
            return Decision::Allow;
        }
        let verifiable = !parsed.unverifiable;
        let all_read_only = verifiable && parts.iter().all(read_only);
        if self.mode == PermissionMode::Plan && !all_read_only {
            return self.ask(
                BASH_TOOL,
                format!(
                    "Current permission mode ({}) requires approval for this Bash command",
                    self.mode.title()
                ),
                None,
            );
        }
        if self.mode == PermissionMode::AcceptEdits
            && verifiable
            && !parts.is_empty()
            && parts.iter().all(|part| {
                ACCEPT_EDITS_PROGRAMS.contains(&part.program().unwrap_or_default())
                    && written_files(part)
                        .iter()
                        .all(|file| self.in_working_dirs(&absolute(file, &anchors)))
            })
        {
            return Decision::Allow;
        }
        let allow_rules: Vec<&CompiledRule> = self
            .rules_of(RuleBehavior::Allow)
            .filter(|rule| rule.tool == BASH_TOOL)
            .collect();
        if allow_rules.iter().any(|rule| {
            matches!(rule.matcher, Matcher::Any)
                || matches!(&rule.matcher, Matcher::Bash(pattern) if pattern.is_exact() && pattern.matches(command.trim(), false))
        }) {
            return Decision::Allow;
        }
        let unapproved: Vec<&SimpleCommand> = parts
            .iter()
            .filter(|part| {
                !read_only(part)
                    && !allow_rules
                        .iter()
                        .any(|rule| Self::bash_rule_matches(rule, part))
            })
            .collect();
        if verifiable && unapproved.is_empty() && !parts.is_empty() {
            return Decision::Allow;
        }
        let reason = match unapproved.as_slice() {
            [_, _, ..] => format!(
                "This Bash command contains multiple operations. The following parts require \
                 approval: {}",
                unapproved
                    .iter()
                    .map(|part| part.without_redirections())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ if !verifiable => format!(
                "This Bash command runs commands the permission rules cannot check \
                 (substitutions or heredocs). {}",
                Self::unapproved(BASH_TOOL)
            ),
            _ => Self::unapproved(BASH_TOOL),
        };
        let remember = match unapproved.as_slice() {
            [part] if verifiable => suggested_bash_rule(part),
            _ => None,
        };
        self.fallback_decision(BASH_TOOL, reason, remember)
    }
}
