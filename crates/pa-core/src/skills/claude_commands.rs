//! Claude Code custom slash commands: the markdown files under
//! `.claude/commands/` (project, and each additional working directory) and
//! `~/.claude/commands/` (user) become prompt templates with Claude Code's
//! argument rules and inline shell expansion.
//!
//! Matches Claude Code 2.1.296's legacy commands loader: subdirectories
//! namespace a command (`frontend/review.md` is `/frontend:review`), the
//! frontmatter carries `description`, `argument-hint`, `arguments`,
//! `allowed-tools` and `model`, the description falls back to the body's
//! first line, `$ARGUMENTS`/`$ARGUMENTS[N]`/`$N` (zero-based)/`$name`
//! substitute, arguments nothing consumed are appended as `ARGUMENTS: ...`,
//! and `` !`cmd` `` / ```` ```! ```` blocks run in the session's shell
//! (unless `disableSkillShellExecution` is set) with their output spliced in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::frontmatter::parse_frontmatter;
use super::prompt_templates::{PromptTemplate, TemplateArguments};
use super::{SourceScope, create_synthetic_source_info, skill_markdown_name};

/// Claude Code's directory for user/project configuration.
pub const CLAUDE_DIR_NAME: &str = ".claude";
/// The commands directory under a Claude config dir.
pub const CLAUDE_COMMANDS_DIR: &str = "commands";
/// The description Claude Code shows for a command without one.
const DEFAULT_DESCRIPTION: &str = "Custom command";
/// Claude Code's Bash default timeout bounds one inline shell command.
const SHELL_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Directory nesting past this depth is not walked (a symlink loop guard).
const MAX_COMMAND_DEPTH: usize = 8;

/// Whether `` !`cmd` `` blocks in a command body run (Claude Code's
/// `disableSkillShellExecution` turns them off; the text then stays as is).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellExpansion {
    #[default]
    Enabled,
    Disabled,
}

/// The Claude Code specifics of a command loaded from a commands directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeCommandOptions {
    /// `arguments`: names that substitute as `$name`, in argument order.
    pub argument_names: Vec<String>,
    /// `allowed-tools`, kept for the permission layer (prime-agent's kernel
    /// tools have no per-command grants yet).
    pub allowed_tools: Vec<String>,
    /// `model`: the model Claude Code would switch to for this command
    /// (recorded, not applied: a prime-agent template never switches models).
    pub model: Option<String>,
    pub shell: ShellExpansion,
}

/// The `~/.claude` directory: `CLAUDE_CONFIG_DIR` when set, else the home's.
#[must_use]
pub fn user_claude_dir() -> Option<PathBuf> {
    user_claude_dir_in(pa_types::platform::home_dir().as_deref())
}

/// [`user_claude_dir`] for a given home directory: `CLAUDE_CONFIG_DIR` when
/// set, else `<home>/.claude`.
#[must_use]
pub fn user_claude_dir_in(home: Option<&Path>) -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(CLAUDE_DIR_NAME)))
}

/// Where commands load from, in precedence order (first name wins).
#[derive(Debug, Clone, Default)]
pub struct ClaudeCommandRoots {
    /// Project roots whose `.claude/commands` load (the working directory
    /// when its workspace is trusted, then each additional directory).
    pub project_roots: Vec<PathBuf>,
    /// The user config dir (`~/.claude`); `None` skips user commands.
    pub user_dir: Option<PathBuf>,
    pub shell: ShellExpansion,
}

/// Load every command under the roots: project commands first, then user
/// commands; a name already loaded is skipped.
#[must_use]
pub fn load_claude_commands(roots: &ClaudeCommandRoots) -> Vec<PromptTemplate> {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut push = |found: Vec<PromptTemplate>| {
        for template in found {
            if !templates.iter().any(|known| known.name == template.name) {
                templates.push(template);
            }
        }
    };
    for root in &roots.project_roots {
        let dir = root.join(CLAUDE_DIR_NAME).join(CLAUDE_COMMANDS_DIR);
        push(load_commands_dir(&dir, SourceScope::Project, roots.shell));
    }
    if let Some(user) = &roots.user_dir {
        let dir = user.join(CLAUDE_COMMANDS_DIR);
        push(load_commands_dir(&dir, SourceScope::User, roots.shell));
    }
    templates
}

fn load_commands_dir(dir: &Path, scope: SourceScope, shell: ShellExpansion) -> Vec<PromptTemplate> {
    let mut files = Vec::new();
    collect_markdown(dir, 0, &mut files);
    files.sort();
    let mut templates: Vec<PromptTemplate> = files
        .iter()
        .filter_map(|file| load_command_file(dir, file, scope, shell))
        .collect();
    templates.sort_by(|left, right| left.name.cmp(&right.name));
    templates
}

fn collect_markdown(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_COMMAND_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_markdown(&path, depth + 1, out);
        } else if meta.is_file() && skill_markdown_name(&entry.file_name().to_string_lossy()) {
            out.push(path);
        }
    }
}

/// The command name: the path below the commands dir, `:`-joined, without
/// `.md`; a `SKILL.md` names its directory.
fn command_name(base: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(base).ok()?;
    let mut segments: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    let last = segments.pop()?;
    if last != "SKILL.md" {
        segments.push(last.trim_end_matches(".md").to_string());
    }
    (!segments.is_empty()).then(|| segments.join(":"))
}

/// Claude Code's description fallback: the first non-empty body line, a
/// heading's text, cut at 100 characters.
pub(super) fn first_line_description(body: &str) -> Option<String> {
    let line = body.lines().map(str::trim).find(|line| !line.is_empty())?;
    let heading = line
        .strip_prefix('#')
        .map(|rest| rest.trim_start_matches('#'))
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim)
        .filter(|rest| !rest.is_empty());
    let text = heading.unwrap_or(line);
    let chars: Vec<char> = text.chars().collect();
    Some(if chars.len() > 100 {
        chars[..97].iter().collect::<String>() + "..."
    } else {
        text.to_string()
    })
}

/// Split a tool list on commas and whitespace outside parentheses, so a
/// rule's own specifier stays whole (`Bash(git diff:*), Read`).
fn split_tool_list(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut current)),
            ch if ch.is_whitespace() && depth == 0 => items.push(std::mem::take(&mut current)),
            ch => current.push(ch),
        }
    }
    items.push(current);
    items.retain(|item| !item.is_empty());
    items
}

pub(super) fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(text)) => split_tool_list(text),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// `arguments`: a list or a space-separated string of names; numeric and
/// empty names are dropped (they would shadow `$N`).
pub(super) fn argument_names(value: Option<&serde_json::Value>) -> Vec<String> {
    let valid = |name: &str| !name.trim().is_empty() && !name.chars().all(|c| c.is_ascii_digit());
    match value {
        Some(serde_json::Value::String(text)) => text
            .split_whitespace()
            .filter(|name| valid(name))
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|name| valid(name))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn load_command_file(
    base: &Path,
    file: &Path,
    scope: SourceScope,
    shell: ShellExpansion,
) -> Option<PromptTemplate> {
    let raw = std::fs::read_to_string(file).ok()?;
    let (frontmatter, body) = parse_frontmatter(&raw);
    let name = command_name(base, file)?;
    let text = |key: &str| {
        frontmatter.get(key).and_then(|value| match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(text) => Some(text.clone()),
            other => Some(other.to_string()),
        })
    };
    let description = text("description")
        .filter(|description| !description.trim().is_empty())
        .or_else(|| first_line_description(&body))
        .unwrap_or_else(|| DEFAULT_DESCRIPTION.to_string());
    let model = text("model")
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty());
    let options = ClaudeCommandOptions {
        argument_names: argument_names(frontmatter.get("arguments")),
        allowed_tools: string_list(frontmatter.get("allowed-tools")),
        model,
        shell,
    };
    Some(PromptTemplate {
        name,
        description,
        argument_hint: text("argument-hint"),
        content: body,
        source_info: create_synthetic_source_info(
            &file.display().to_string(),
            "claude",
            scope,
            Some(&base.display().to_string()),
        ),
        file_path: file.display().to_string(),
        arguments: TemplateArguments::ClaudeCode(options),
    })
}

// -- argument substitution ------------------------------------------------

/// Stands for a literal `$` until the end of substitution.
const DOLLAR: char = '\u{e000}';
/// Brackets a substituted value so later passes never read into it.
const FENCE: char = '\u{e001}';

/// Neutralize the shell-expansion openers inside an argument value, so a
/// typed argument can never inject a `` !`cmd` `` block (Claude Code's `bx`).
fn escape_bangs(value: &str) -> String {
    let value = value.replace("`!", "` !").replace("!`", "! `");
    let mut out = String::with_capacity(value.len());
    let mut previous: Option<char> = None;
    for ch in value.chars() {
        if ch == '!' && previous.is_none_or(char::is_whitespace) {
            out.push('\\');
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}

fn fence(value: &str) -> String {
    let cleaned = escape_bangs(&value.replace([DOLLAR, FENCE], "\u{fffd}"));
    format!(
        "{FENCE}{}{FENCE}",
        cleaned.replace('$', &DOLLAR.to_string())
    )
}

/// Split the raw argument string the way Claude Code does: shell words,
/// else whitespace.
fn split_arguments(raw: &str) -> Vec<String> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let words = super::prompt_templates::parse_command_args(raw);
    if words.is_empty() {
        raw.split_whitespace().map(str::to_string).collect()
    } else {
        words
    }
}

fn regex(pattern: &str) -> fancy_regex::Regex {
    fancy_regex::Regex::new(pattern).expect("static substitution pattern")
}

/// Replace every match of `pattern`, the callback returning the replacement
/// (or `None` to keep the match as is).
fn replace_matches(
    text: &str,
    pattern: &fancy_regex::Regex,
    mut replace: impl FnMut(&fancy_regex::Captures<'_>) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for captures in pattern.captures_iter(text).flatten() {
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.push_str(&text[last..whole.start()]);
        match replace(&captures) {
            Some(replacement) => out.push_str(&replacement),
            None => out.push_str(whole.as_str()),
        }
        last = whole.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Substitute a command's arguments into its body (Claude Code's rules):
/// `\$` escapes a placeholder, `$name` takes the named argument,
/// `$ARGUMENTS[N]` and `$N` the zero-based Nth word, `$ARGUMENTS` the whole
/// string; a body that consumed nothing gets `ARGUMENTS: <args>` appended.
#[must_use]
pub fn substitute_claude_arguments(content: &str, raw_args: &str, names: &[String]) -> String {
    substitute_claude_arguments_with(content, raw_args, names, UnusedArguments::Append)
}

/// What [`substitute_claude_arguments_with`] does with arguments no
/// placeholder consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnusedArguments {
    /// Append them as an `ARGUMENTS: ...` line (Claude Code's rule).
    Append,
    /// Leave them out: the caller delivers them itself (a skill block's
    /// trailing user message).
    Omit,
}

/// [`substitute_claude_arguments`] with the unconsumed-arguments rule
/// chosen by the caller.
#[must_use]
pub fn substitute_claude_arguments_with(
    content: &str,
    raw_args: &str,
    names: &[String],
    unused: UnusedArguments,
) -> String {
    let words = split_arguments(raw_args);
    let mut text = content.replace([DOLLAR, FENCE], "\u{fffd}");
    let mut by_length: Vec<(usize, &String)> = names.iter().enumerate().collect();
    by_length.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));
    let mut tokens = vec!["\\d".to_string(), "ARGUMENTS".to_string()];
    tokens.extend(
        by_length
            .iter()
            .map(|(_, name)| format!("{}(?![\\[\\w])", fancy_regex::escape(name))),
    );
    let escaped = regex(&format!(r"(?<!\\)\\\$(?=(?:{}))", tokens.join("|")));
    text = replace_matches(&text, &escaped, |_| Some(DOLLAR.to_string()));
    let mut used = false;
    for (index, name) in &by_length {
        let pattern = regex(&format!(r"\${}(?![\[\w])", fancy_regex::escape(name)));
        text = replace_matches(&text, &pattern, |_| {
            used = true;
            Some(fence(words.get(*index).map_or("", String::as_str)))
        });
    }
    text = replace_matches(&text, &regex(r"\$ARGUMENTS\[(\d+)\]"), |captures| {
        let index: usize = captures.get(1)?.as_str().parse().ok()?;
        match words.get(index) {
            Some(word) => {
                used = true;
                Some(fence(word))
            }
            None => Some(format!("{DOLLAR}{}", &captures.get(0)?.as_str()[1..])),
        }
    });
    text = replace_matches(&text, &regex(r"\$(\d+)(?!\w)"), |captures| {
        let index: usize = captures.get(1)?.as_str().parse().ok()?;
        words.get(index).map(|word| {
            used = true;
            fence(word)
        })
    });
    if text.contains("$ARGUMENTS") {
        used = true;
        text = text.replace("$ARGUMENTS", &fence(raw_args));
    }
    if !used && !raw_args.is_empty() && unused == UnusedArguments::Append {
        text.push_str("\n\nARGUMENTS: ");
        text.push_str(&fence(raw_args));
    }
    text.replace(DOLLAR, "$").replace(FENCE, "")
}

// -- inline shell expansion -----------------------------------------------

/// One `` !`cmd` `` or ```` ```! ```` block found in an expanded body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellBlock {
    /// The exact text the block occupies (replaced by the command's output).
    pub raw: String,
    pub command: String,
}

/// Blank out ordinary inline code spans so a `` !`x` `` inside one is not a
/// shell block (spans opened by `!` or a backtick stay).
fn mask_inline_code(text: &str) -> String {
    let span = regex(r"`[^`\n]+`");
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in span.find_iter(text).flatten() {
        out.push_str(&text[last..found.start()]);
        let previous = text[..found.start()].chars().next_back();
        if matches!(previous, Some('!' | '`')) {
            out.push_str(found.as_str());
        } else {
            let inner = found.as_str().chars().count().saturating_sub(2);
            out.push('`');
            out.push_str(&" ".repeat(inner));
            out.push('`');
        }
        last = found.end();
    }
    out.push_str(&text[last..]);
    out
}

/// The shell blocks of an expanded command body, fenced blocks first.
#[must_use]
pub fn find_shell_blocks(text: &str) -> Vec<ShellBlock> {
    let mut blocks = Vec::new();
    let mut push = |raw: &str, command: &str| {
        let command = command.trim().replace("\r\n", "\n");
        if !command.is_empty() {
            blocks.push(ShellBlock {
                raw: raw.to_string(),
                command,
            });
        }
    };
    let fenced = regex(r"```!\s*\n?([\s\S]*?)\n?```");
    for captures in fenced.captures_iter(text).flatten() {
        if let (Some(whole), Some(command)) = (captures.get(0), captures.get(1)) {
            push(whole.as_str(), command.as_str());
        }
    }
    if text.contains("!`") {
        // The masked copy keeps byte offsets: matches index the original.
        let masked = mask_inline_code(text);
        let inline = regex(r"(?m)(?:^|(?<=\s))!`([^`]+)`");
        for captures in inline.captures_iter(&masked).flatten() {
            if let (Some(whole), Some(command)) = (captures.get(0), captures.get(1)) {
                push(
                    &text[whole.start()..whole.end()],
                    &text[command.start()..command.end()],
                );
            }
        }
    }
    blocks
}

/// Claude Code's output splice: stdout, then `[stderr]` and stderr.
fn format_output(stdout: &str, stderr: &str) -> String {
    let mut parts = Vec::new();
    if !stdout.trim().is_empty() {
        parts.push(stdout.trim().to_string());
    }
    if !stderr.trim().is_empty() {
        parts.push(format!("[stderr]\n{}", stderr.trim()));
    }
    parts.join("\n")
}

/// Run one block's command in `cwd`: the kernel `bash()` refusal guards
/// judge it first; a refusal, a spawn failure or the timeout splices in a
/// bracketed note instead of output.
async fn run_shell_block(command: &str, cwd: &Path) -> String {
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let context = pa_bash::GuardContext::new(cwd, env);
    if let Err(refusal) = pa_bash::check(
        &pa_bash::Script::bare(command),
        &pa_bash::Allowances::none(),
        &context,
    ) {
        return format!("[command refused: {}]", refusal.message);
    }
    let shell = match pa_bash::resolve_shell(&context) {
        Ok(shell) => shell,
        Err(error) => return format!("[command not run: {error}]"),
    };
    let child = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env_clear()
        .envs(pa_bash::child_env(&context))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => return format!("[command not run: {error}]"),
    };
    match tokio::time::timeout(SHELL_COMMAND_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => format_output(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ),
        Ok(Err(error)) => format!("[command failed: {error}]"),
        Err(_) => format!(
            "[command timed out after {}s]",
            SHELL_COMMAND_TIMEOUT.as_secs()
        ),
    }
}

/// Replace each shell block of `text` with its command's output, the
/// commands running concurrently in `cwd`.
pub async fn expand_shell_blocks(text: &str, cwd: &Path) -> String {
    let blocks = find_shell_blocks(text);
    if blocks.is_empty() {
        return text.to_string();
    }
    let outputs = futures::future::join_all(
        blocks
            .iter()
            .map(|block| run_shell_block(&block.command, cwd)),
    )
    .await;
    let mut expanded = text.to_string();
    for (block, output) in blocks.iter().zip(outputs) {
        expanded = expanded.replacen(&block.raw, &output, 1);
    }
    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn positional_arguments_are_zero_based_and_whole_string_substitutes() {
        assert_eq!(
            substitute_claude_arguments(
                "fix $0 then $1; all: $ARGUMENTS",
                "lint \"type check\"",
                &[]
            ),
            "fix lint then type check; all: lint \"type check\""
        );
        assert_eq!(
            substitute_claude_arguments("first $ARGUMENTS[0], missing $ARGUMENTS[5]", "a b", &[]),
            "first a, missing $ARGUMENTS[5]"
        );
        // A missing positional stays literal; `$10x` is not a placeholder.
        assert_eq!(
            substitute_claude_arguments("$3 and $0x", "a", &[]),
            "$3 and $0x\n\nARGUMENTS: a"
        );
    }

    #[test]
    fn named_arguments_and_escapes() {
        assert_eq!(
            substitute_claude_arguments(
                "issue $issue in $branch, cost \\$1, keep $issues",
                "42 main",
                &names(&["issue", "branch"])
            ),
            "issue 42 in main, cost $1, keep $issues"
        );
    }

    #[test]
    fn unconsumed_arguments_append_and_values_are_not_resubstituted() {
        assert_eq!(
            substitute_claude_arguments("Review the diff.", "carefully please", &[]),
            "Review the diff.\n\nARGUMENTS: carefully please"
        );
        assert_eq!(substitute_claude_arguments("Review.", "", &[]), "Review.");
        // A `$1` typed as an argument value is never read as a placeholder.
        assert_eq!(
            substitute_claude_arguments("echo $ARGUMENTS and $0", "$1 x", &[]),
            "echo $1 x and $1"
        );
    }

    #[test]
    fn argument_values_cannot_open_shell_blocks() {
        let expanded = substitute_claude_arguments("note: $ARGUMENTS", "!`rm -rf /`", &[]);
        assert_eq!(expanded, "note: \\! `rm -rf /`");
        assert_eq!(find_shell_blocks(&expanded), Vec::new());
    }

    #[test]
    fn shell_blocks_are_found_inline_and_fenced() {
        let text = "Status: !`git status --short`\nNot `x !`y` here\n```!\nls -a\n```\n";
        assert_eq!(
            find_shell_blocks(text),
            vec![
                ShellBlock {
                    raw: "```!\nls -a\n```".to_string(),
                    command: "ls -a".to_string(),
                },
                ShellBlock {
                    raw: "!`git status --short`".to_string(),
                    command: "git status --short".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn shell_blocks_splice_their_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let expanded = expand_shell_blocks(
            "Files: !`ls`\n```!\necho out; echo err >&2\n```",
            dir.path(),
        )
        .await;
        assert_eq!(expanded, "Files: marker.txt\nout\n[stderr]\nerr");
    }

    #[tokio::test]
    async fn refused_shell_blocks_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let expanded = expand_shell_blocks("!`sudo touch made`", dir.path()).await;
        assert!(expanded.starts_with("[command refused: "), "{expanded}");
        assert!(!dir.path().join("made").exists());
    }

    #[test]
    fn loads_namespaced_commands_with_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let commands = project.join(".claude").join("commands");
        std::fs::create_dir_all(commands.join("frontend")).unwrap();
        std::fs::write(
            commands.join("frontend").join("review.md"),
            "---\ndescription: Review a component\nargument-hint: <file>\nallowed-tools: Bash(git diff:*), Read\nmodel: sonnet\narguments: [file]\n---\nReview $file\n",
        )
        .unwrap();
        std::fs::write(commands.join("plain.md"), "# Plain heading\nBody").unwrap();
        let user = tmp.path().join("user-claude");
        std::fs::create_dir_all(user.join("commands")).unwrap();
        std::fs::write(user.join("commands").join("plain.md"), "user copy").unwrap();
        std::fs::write(user.join("commands").join("mine.md"), "").unwrap();
        let templates = load_claude_commands(&ClaudeCommandRoots {
            project_roots: vec![project],
            user_dir: Some(user),
            shell: ShellExpansion::Disabled,
        });
        let summary: Vec<(&str, &str, Option<&str>)> = templates
            .iter()
            .map(|template| {
                (
                    template.name.as_str(),
                    template.description.as_str(),
                    template.argument_hint.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("frontend:review", "Review a component", Some("<file>")),
                ("plain", "Plain heading", None),
                ("mine", "Custom command", None),
            ]
        );
        assert_eq!(
            templates[0].arguments,
            TemplateArguments::ClaudeCode(ClaudeCommandOptions {
                argument_names: names(&["file"]),
                allowed_tools: names(&["Bash(git diff:*)", "Read"]),
                model: Some("sonnet".to_string()),
                shell: ShellExpansion::Disabled,
            })
        );
        assert_eq!(templates[1].content, "# Plain heading\nBody");
    }
}
