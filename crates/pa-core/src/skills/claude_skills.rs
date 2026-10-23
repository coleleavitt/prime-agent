//! Claude Code skills: the instruction documents under `.claude/skills/<name>/SKILL.md`.
//!
//! Matches Claude Code 2.1.296's skills loader (`xAr`, `xO`, `ive`, `aYt`):
//!
//! - a skill is a directory holding `SKILL.md`; its name is the directory's name (the frontmatter
//!   `name` is only a display name), and nothing below it is searched for further skills;
//! - the project directories are every `.claude/skills` from the working directory up to the
//!   repository root (`getProjectDirsUpToHome`: the walk stops before the home directory and
//!   outside a repository at the filesystem root), nearest first, then each additional working
//!   directory's; the user directory is `~/.claude/skills` (`CLAUDE_CONFIG_DIR` aware);
//! - the user directory loads first, then the project directories: the first skill of a name wins;
//! - the frontmatter carries `description` (falling back to the body's first line),
//!   `when_to_use`, `argument-hint`, `arguments`, `allowed-tools`, `model`,
//!   `disable-model-invocation` and `user-invocable`;
//! - an invocation's text is `Base directory for this skill: <dir>` and the body, with the
//!   arguments substituted like a custom command's, then `${CLAUDE_SKILL_DIR}`,
//!   `${CLAUDE_PROJECT_DIR}` and `${CLAUDE_SESSION_ID}`, then the `` !`cmd` `` blocks run
//!   (unless `disableSkillShellExecution` is set).
//!
//! A Claude Code skill is instruction-only here: a `pyproject.toml` beside it never makes it a
//! Python skill (nothing installs into the kernel venv).

use std::path::{Path, PathBuf};

use super::claude_commands::{
    CLAUDE_DIR_NAME,
    ShellExpansion,
    UnusedArguments,
    argument_names,
    expand_shell_blocks,
    first_line_description,
    string_list,
    substitute_claude_arguments_with,
};
use super::diagnostics::ResourceDiagnostic;
use super::frontmatter::parse_frontmatter;
use super::{Skill, SkillKind, SourceScope, create_synthetic_source_info};

/// The skills directory under a Claude config dir.
pub const CLAUDE_SKILLS_DIR: &str = "skills";
/// The skill file inside a skill directory.
pub const SKILL_FILE: &str = "SKILL.md";
/// Claude Code's description fallback when a skill's body has no text.
const DEFAULT_DESCRIPTION: &str = "Skill";
/// The source label in the skill's provenance.
const SOURCE: &str = "claude";

/// The Claude Code specifics of a skill loaded from a `.claude/skills` directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeSkillOptions {
    /// `arguments`: names that substitute as `$name`, in argument order.
    pub argument_names: Vec<String>,
    /// `argument-hint`, shown after the slash command.
    pub argument_hint: Option<String>,
    /// `allowed-tools`, recorded (the kernel's tools have no per-skill grants).
    pub allowed_tools: Vec<String>,
    /// `model`, recorded (an invocation never switches models here).
    pub model: Option<String>,
    pub shell: ShellExpansion,
    /// Whether `/<name>` invokes the skill besides `/skill:<name>`: off when the name belongs to
    /// a builtin command or a Prime Agent prompt template.
    pub bare_command: bool,
}

/// Where Claude Code skills load from.
#[derive(Debug, Clone, Default)]
pub struct ClaudeSkillRoots {
    /// The user skills directory (`~/.claude/skills`); `None` skips it.
    pub user_dir: Option<PathBuf>,
    /// The project skills directories, in precedence order.
    pub project_dirs: Vec<PathBuf>,
    pub shell: ShellExpansion,
}

/// The skills loaded from the roots and what was skipped.
#[derive(Debug, Default)]
pub struct ClaudeSkillsLoad {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// The project directories Claude Code searches (`getProjectDirsUpToHome`): the working
/// directory and each ancestor, nearest first, stopping before `home`, after the repository root
/// (the first directory holding `.git`), or at the filesystem root.
#[must_use]
pub fn project_ancestors(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        if home.is_some_and(|home| pa_types::platform::same_directory(dir, home)) {
            break;
        }
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs
}

/// `<dir>/.claude/<sub>` for each of [`project_ancestors`] that exists, nearest first.
#[must_use]
pub fn project_claude_dirs(cwd: &Path, sub: &str, home: Option<&Path>) -> Vec<PathBuf> {
    project_ancestors(cwd, home)
        .into_iter()
        .map(|dir| dir.join(CLAUDE_DIR_NAME).join(sub))
        .filter(|candidate| candidate.is_dir())
        .collect()
}

/// Load every skill under the roots: the user directory first, then the project directories; the
/// first skill of a name wins and a later one is reported as a collision.
#[must_use]
pub fn load_claude_skills(roots: &ClaudeSkillRoots) -> ClaudeSkillsLoad {
    let mut load = ClaudeSkillsLoad::default();
    let mut seen_files: Vec<PathBuf> = Vec::new();
    let sources = roots
        .user_dir
        .iter()
        .map(|dir| (dir, SourceScope::User))
        .chain(
            roots
                .project_dirs
                .iter()
                .map(|dir| (dir, SourceScope::Project)),
        );
    for (dir, scope) in sources {
        for skill in load_skills_dir(dir, scope, roots.shell, &mut load.diagnostics) {
            let real =
                std::fs::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
            if seen_files.contains(&real) {
                continue;
            }
            seen_files.push(real);
            if let Some(winner) = load.skills.iter().find(|known| known.name == skill.name) {
                load.diagnostics.push(collision(winner, &skill));
                continue;
            }
            load.skills.push(skill);
        }
    }
    load
}

/// The collision diagnostic for `loser`, shadowed by `winner` of the same name.
#[must_use]
pub fn collision(winner: &Skill, loser: &Skill) -> ResourceDiagnostic {
    ResourceDiagnostic::Collision {
        message: format!("name \"{}\" collision", loser.name),
        path: loser.file_path.display().to_string(),
        collision: super::ResourceCollision {
            resource_type: "skill",
            name: loser.name.clone(),
            winner_path: winner.file_path.display().to_string(),
            loser_path: loser.file_path.display().to_string(),
        },
    }
}

/// Add the Claude Code skills to the session's skills: a Prime Agent skill of the same name wins
/// (the Claude Code copy is reported as a collision).
pub fn merge_claude_skills(
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<ResourceDiagnostic>,
    claude: ClaudeSkillsLoad,
) {
    diagnostics.extend(claude.diagnostics);
    for skill in claude.skills {
        if let Some(winner) = skills.iter().find(|known| known.name == skill.name) {
            diagnostics.push(collision(winner, &skill));
            continue;
        }
        skills.push(skill);
    }
}

/// Decide which Claude Code skills answer to a bare `/<name>`: not one whose name a builtin
/// command or a Prime Agent prompt template already takes.
pub fn assign_bare_commands(skills: &mut [Skill], prompt_template_names: &[&str]) {
    let builtins = pa_types::slash_commands::SlashCommandRegistry::builtin_cached();
    for skill in skills {
        let name = skill.name.as_str();
        let taken = builtins.is_builtin(name)
            || pa_types::slash_commands::is_session_slash_command_name(name)
            || pa_types::slash_commands::feature_slash_commands()
                .iter()
                .any(|command| command.name == name || command.aliases.contains(&name))
            || prompt_template_names.contains(&name);
        if let Some(options) = skill.claude.as_mut() {
            options.bare_command = !taken;
        }
    }
}

/// A skill name usable in a slash command and the skill block's attributes.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.chars().any(|ch| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, '"' | '<' | '>' | '&' | '/' | '\\')
        })
}

/// Claude Code's boolean frontmatter reading: `true`/`false`, or their strings.
fn frontmatter_bool(value: Option<&serde_json::Value>) -> Option<bool> {
    match value? {
        serde_json::Value::Bool(flag) => Some(*flag),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn frontmatter_text(frontmatter: &serde_json::Value, key: &str) -> Option<String> {
    frontmatter.get(key).and_then(|value| match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    })
}

fn load_skills_dir(
    dir: &Path,
    scope: SourceScope,
    shell: ShellExpansion,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut skill_dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    skill_dirs.sort();
    skill_dirs
        .iter()
        .filter_map(|skill_dir| load_skill(skill_dir, scope, shell, diagnostics))
        .collect()
}

fn load_skill(
    skill_dir: &Path,
    scope: SourceScope,
    shell: ShellExpansion,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Option<Skill> {
    let file = skill_dir.join(SKILL_FILE);
    if !file.is_file() {
        return None;
    }
    let name = skill_dir.file_name()?.to_string_lossy().to_string();
    if !valid_name(&name) {
        diagnostics.push(ResourceDiagnostic::Warning {
            message: format!("skill directory name \"{name}\" cannot be used as a skill name"),
            path: Some(file.display().to_string()),
        });
        return None;
    }
    let raw = match std::fs::read_to_string(&file) {
        Ok(raw) => raw,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::Warning {
                message: error.to_string(),
                path: Some(file.display().to_string()),
            });
            return None;
        }
    };
    let (frontmatter, body) = parse_frontmatter(&raw);
    let description = frontmatter_text(&frontmatter, "description")
        .filter(|description| !description.trim().is_empty())
        .or_else(|| first_line_description(&body))
        .unwrap_or_else(|| DEFAULT_DESCRIPTION.to_string());
    let model = frontmatter_text(&frontmatter, "model")
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty());
    let options = ClaudeSkillOptions {
        argument_names: argument_names(frontmatter.get("arguments")),
        argument_hint: frontmatter_text(&frontmatter, "argument-hint"),
        allowed_tools: string_list(frontmatter.get("allowed-tools")),
        model,
        shell,
        bare_command: true,
    };
    Some(Skill {
        name,
        description,
        file_path: file.clone(),
        base_dir: skill_dir.to_path_buf(),
        source_info: create_synthetic_source_info(
            &file.display().to_string(),
            SOURCE,
            scope,
            Some(&skill_dir.display().to_string()),
        ),
        disable_model_invocation: frontmatter_bool(frontmatter.get("disable-model-invocation"))
            .unwrap_or(false),
        kind: SkillKind::Markdown,
        python: None,
        when_to_use: frontmatter_text(&frontmatter, "when_to_use")
            .filter(|text| !text.trim().is_empty()),
        user_invocable: frontmatter_bool(frontmatter.get("user-invocable")).unwrap_or(true),
        claude: Some(options),
    })
}

/// The session values a Claude Code skill body's variables take.
#[derive(Debug, Clone, Copy)]
pub struct SkillVariables<'a> {
    /// `${CLAUDE_PROJECT_DIR}`: the session's working directory; the `` !`cmd` `` blocks run in
    /// it.
    pub project_dir: &'a Path,
    /// `${CLAUDE_SESSION_ID}`.
    pub session_id: &'a str,
}

/// A Claude Code skill's invocation text (Claude Code's `getPromptForCommand`): the base-directory
/// line and the body, the arguments substituted, then the skill, project and session variables,
/// then the shell blocks.
pub async fn render_claude_skill(
    options: &ClaudeSkillOptions,
    base_dir: &Path,
    body: &str,
    args: &str,
    unused: UnusedArguments,
    variables: &SkillVariables<'_>,
) -> String {
    let text = format!(
        "Base directory for this skill: {}\n\n{body}",
        base_dir.display()
    );
    let text = substitute_claude_arguments_with(&text, args, &options.argument_names, unused);
    let text = text
        .replace("${CLAUDE_SKILL_DIR}", &base_dir.display().to_string())
        .replace(
            "${CLAUDE_PROJECT_DIR}",
            &variables.project_dir.display().to_string(),
        )
        .replace("${CLAUDE_SESSION_ID}", variables.session_id);
    match options.shell {
        ShellExpansion::Enabled => expand_shell_blocks(&text, variables.project_dir).await,
        ShellExpansion::Disabled => text,
    }
}

#[cfg(test)]
#[path = "claude_skills_tests.rs"]
mod tests;
