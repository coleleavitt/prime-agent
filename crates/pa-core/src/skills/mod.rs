//! Skill discovery, validation, and prompt formatting.

pub mod claude_commands;
pub mod claude_skills;
pub mod diagnostics;
pub mod discovery;
pub mod dynamic;
pub mod frontmatter;
pub mod loader;
pub mod prompt_templates;

use std::path::PathBuf;

use serde::Serialize;

/// Max name length per the Agent Skills spec.
pub const MAX_NAME_LENGTH: usize = 64;
/// Max description length per the Agent Skills spec.
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

pub use diagnostics::{ResourceCollision, ResourceDiagnostic};
pub use discovery::load_skills_from_dir;
pub use loader::{LoadSkillsOptions, LoadSkillsResult, load_skills};
pub use pa_types::slash_commands::parse_slash_command;
pub use prompt_templates::{
    LoadPromptTemplatesOptions,
    PromptTemplate,
    TemplateArguments,
    expand_prompt_template,
    expand_prompt_template_with_shell,
    is_claude_command,
    load_prompt_templates,
    parse_command_args,
    substitute_args,
};

/// Source provenance for a resource (the TS source-info shape).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SourceOrigin {
    Package,
    TopLevel,
}

/// Markdown entry test: the file name carries a lowercase `.md` extension
/// (TS `endsWith('.md')` parity: mixed-case suffixes stay undiscovered, and
/// a bare `.md` name has no extension and does not count).
#[must_use]
pub fn skill_markdown_name(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|ext| ext == "md")
}

pub fn create_synthetic_source_info(
    path: &str,
    source: &str,
    scope: SourceScope,
    base_dir: Option<&str>,
) -> SourceInfo {
    SourceInfo {
        path: path.to_string(),
        source: source.to_string(),
        scope,
        origin: SourceOrigin::TopLevel,
        base_dir: base_dir.map(str::to_string),
    }
}

/// Python runtime metadata for a Python-backed skill.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SkillPythonMetadata {
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

/// A discovered skill.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
    pub kind: SkillKind,
    pub python: Option<SkillPythonMetadata>,
    /// `when_to_use` (Claude Code's key): when the model should load the
    /// skill; the inventory shows it after the description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// `user-invocable` (Claude Code's key, default on): off keeps the skill
    /// out of the user's slash commands; the model may still load it.
    #[serde(skip_serializing_if = "is_true")]
    pub user_invocable: bool,
    /// A Claude Code skill's specifics (`.claude/skills/<name>/SKILL.md`):
    /// its body takes Claude Code's argument and variable substitution.
    #[serde(skip)]
    pub claude: Option<claude_skills::ClaudeSkillOptions>,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SkillKind {
    Markdown,
    Python,
}

impl Skill {
    #[must_use]
    pub fn is_python(&self) -> bool {
        self.kind == SkillKind::Python
    }

    /// The kind label the prompt inventory and telemetry share
    /// (`<type>` in `format_skills_for_prompt`, `skill_kind` in events).
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        if self.is_python() {
            "python"
        } else {
            "markdown"
        }
    }
}

/// Runtime info for kernel-side Python skill preparation.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PythonSkillRuntimeInfo {
    pub name: String,
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

/// The runtime info for every Python skill in the list.
///
/// # Panics
///
/// The `expect` cannot fire: a skill is marked `Python` only when its metadata was parsed.
#[must_use]
pub fn get_python_skill_runtime_info(skills: &[Skill]) -> Vec<PythonSkillRuntimeInfo> {
    skills
        .iter()
        .filter(|skill| skill.is_python())
        .map(|skill| {
            let python = skill.python.as_ref().expect("python skill has metadata");
            PythonSkillRuntimeInfo {
                name: skill.name.clone(),
                import_name: python.import_name.clone(),
                package_path: python.package_path.clone(),
                pyproject_path: python.pyproject_path.clone(),
            }
        })
        .collect()
}

pub(crate) fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

pub(crate) fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".to_string());
    } else if description.len() > MAX_DESCRIPTION_LENGTH {
        errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.len()
        ));
    }
    errors
}

pub(crate) use validate_description as validate_skill_description;
pub(crate) use validate_name as validate_skill_name;

/// Format skills for a system prompt (Agent Skills XML standard); skills
/// with disableModelInvocation are excluded.
#[must_use]
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = skill_inventory_preamble();
    lines[0] = format!("\n\n{}", lines[0]);
    lines.push("<available_skills>".to_string());
    lines.extend(visible.into_iter().map(skill_inventory_entry));
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// The lines ahead of the `<available_skills>` inventory (ending in a blank
/// line).
#[must_use]
pub fn skill_inventory_preamble() -> Vec<String> {
    vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        "Use python_repl to inspect a skill's file when the task matches its description.".to_string(),
        "Skills with a python_import are prepared in the persistent Python kernel when available and can be called directly by that import name.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
    ]
}

/// One skill's `<skill>` entry in the inventory (multi-line, indented).
#[must_use]
pub fn skill_inventory_entry(skill: &Skill) -> String {
    let mut lines = vec![
        "  <skill>".to_string(),
        format!("    <name>{}</name>", escape_xml(&skill.name)),
        format!("    <type>{}</type>", skill.kind_label()),
    ];
    if let Some(python) = &skill.python {
        lines.push(format!(
            "    <python_import>{}</python_import>",
            escape_xml(&python.import_name)
        ));
    }
    lines.push(format!(
        "    <description>{}</description>",
        escape_xml(&inventory_description(skill))
    ));
    lines.push(format!(
        "    <location>{}</location>",
        escape_xml(&skill.file_path.display().to_string())
    ));
    if skill.claude.is_some() {
        lines.push(format!(
            "    <load>await rlm.load_skill(\"{}\", args=\"\")</load>",
            escape_xml(&skill.name)
        ));
    }
    lines.push("  </skill>".to_string());
    lines.join("\n")
}

/// Claude Code's listing cap for one skill's description
/// (`skillListingMaxDescChars`' default).
const MAX_LISTED_DESCRIPTION_CHARS: usize = 1536;

/// The description the inventory lists: `<description> - <when_to_use>`
/// (Claude Code's `pvt`), cut at Claude Code's listing cap.
fn inventory_description(skill: &Skill) -> String {
    let text = match &skill.when_to_use {
        Some(when) => format!("{} - {when}", skill.description),
        None => skill.description.clone(),
    };
    if text.chars().count() > MAX_LISTED_DESCRIPTION_CHARS {
        let mut cut: String = text
            .chars()
            .take(MAX_LISTED_DESCRIPTION_CHARS - 1)
            .collect();
        cut.push('\u{2026}');
        cut
    } else {
        text
    }
}

pub(crate) fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The slash command names a skill answers to, for the command listings:
/// `skill:<name>`, and `<name>` for a Claude Code skill whose name no command
/// takes; none for a skill that is not user-invocable.
#[must_use]
pub fn skill_command_names(skill: &Skill) -> Vec<String> {
    if !skill.user_invocable {
        return Vec::new();
    }
    let mut names = vec![format!("skill:{}", skill.name)];
    if skill
        .claude
        .as_ref()
        .is_some_and(|options| options.bare_command)
    {
        names.push(skill.name.clone());
    }
    names
}

/// The argument hint a skill's command shows (a Claude Code skill's
/// `argument-hint`).
#[must_use]
pub fn skill_argument_hint(skill: &Skill) -> Option<&str> {
    skill
        .claude
        .as_ref()
        .and_then(|options| options.argument_hint.as_deref())
}

/// The skill a user's slash command invokes, and its arguments: `/skill:<name>`, a builtin
/// command that runs a bundled skill (`/code-review`, `/review`), or `/<name>` for a Claude Code
/// skill whose name no command takes. A skill that is not user-invocable answers to none.
#[must_use]
pub fn resolve_skill_command<'a>(text: &str, skills: &'a [Skill]) -> Option<(&'a Skill, String)> {
    let (name, args) = parse_slash_command(text)?;
    let skill = if let Some(skill_name) = name
        .strip_prefix("skill:")
        .or_else(|| pa_types::slash_commands::skill_slash_command(&name))
    {
        skills.iter().find(|skill| skill.name == skill_name)?
    } else {
        skills.iter().find(|skill| {
            skill.name == name
                && skill
                    .claude
                    .as_ref()
                    .is_some_and(|options| options.bare_command)
        })?
    };
    skill.user_invocable.then_some((skill, args))
}

/// The text a skill's invocation delivers: a Prime Agent skill's body after the
/// references note; a Claude Code skill's body with its arguments and variables substituted and
/// its shell blocks run ([`claude_skills::render_claude_skill`]). `None` when the skill file
/// cannot be read.
pub async fn skill_invocation_text(
    skill: &Skill,
    args: &str,
    unused: claude_commands::UnusedArguments,
    variables: &claude_skills::SkillVariables<'_>,
) -> Option<String> {
    let content = std::fs::read_to_string(&skill.file_path).ok()?;
    let body = frontmatter::strip_frontmatter(&content).trim().to_string();
    if let Some(options) = &skill.claude {
        return Some(
            claude_skills::render_claude_skill(
                options,
                &skill.base_dir,
                &body,
                args,
                unused,
                variables,
            )
            .await,
        );
    }
    let text = format!(
        "References are relative to {}.

{body}",
        skill.base_dir.display()
    );
    Some(match unused {
        claude_commands::UnusedArguments::Append if !args.trim().is_empty() => {
            format!(
                "{text}

ARGUMENTS: {args}"
            )
        }
        claude_commands::UnusedArguments::Append | claude_commands::UnusedArguments::Omit => text,
    })
}

/// Expand a user's skill command ([`resolve_skill_command`]) into the `<skill ...>` message
/// block, its arguments following as the user's message: a non-skill input, an unknown skill
/// name (the surfaces show their own unknown-command notice), and a skill file that fails to
/// read all pass through. Returns the skill the expansion used.
pub async fn expand_skill_command<'a>(
    text: &str,
    skills: &'a [Skill],
    variables: &claude_skills::SkillVariables<'_>,
) -> (String, Option<&'a Skill>) {
    let Some((skill, args)) = resolve_skill_command(text, skills) else {
        return (text.to_string(), None);
    };
    let Some(body) = skill_invocation_text(
        skill,
        &args,
        claude_commands::UnusedArguments::Omit,
        variables,
    )
    .await
    else {
        return (text.to_string(), None);
    };
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        body
    );
    let expanded = if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    };
    (expanded, Some(skill))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_rules() {
        assert!(validate_name("web-search", "web-search").is_empty());
        assert!(!validate_name("web_search", "web_search").is_empty());
        assert!(!validate_name("-lead", "-lead").is_empty());
        assert!(!validate_name("a--b", "a--b").is_empty());
        assert!(!validate_name("web", "other").is_empty());
    }

    #[test]
    fn description_validation_rules() {
        assert_eq!(validate_description(""), vec!["description is required"]);
        assert!(validate_description("ok").is_empty());
        let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert_eq!(validate_description(&long).len(), 1);
    }

    fn temp_skill(name: &str, dir: &std::path::Path) -> Skill {
        let file_path = dir.join("SKILL.md");
        std::fs::write(
            &file_path,
            format!("---\nname: {name}\ndescription: test skill\n---\nUse {name} well."),
        )
        .expect("write skill file");
        Skill {
            name: name.to_string(),
            description: "test skill".to_string(),
            file_path: file_path.clone(),
            base_dir: dir.to_path_buf(),
            source_info: create_synthetic_source_info(
                &file_path.display().to_string(),
                "user",
                SourceScope::User,
                None,
            ),
            disable_model_invocation: false,
            kind: SkillKind::Markdown,
            python: None,
            when_to_use: None,
            user_invocable: true,
            claude: None,
        }
    }

    fn variables(dir: &std::path::Path) -> claude_skills::SkillVariables<'_> {
        claude_skills::SkillVariables {
            project_dir: dir,
            session_id: "session-1",
        }
    }

    #[tokio::test]
    async fn expands_a_known_skill_command_with_and_without_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill = temp_skill("web-search", dir.path());
        let skills = [skill];
        let (expanded, used) =
            expand_skill_command("/skill:web-search", &skills, &variables(dir.path())).await;
        assert_eq!(used.map(|skill| skill.name.as_str()), Some("web-search"));
        let parsed = pa_types::skill_blocks::parse_skill_block(&expanded).expect("parses");
        assert_eq!(parsed.name, "web-search");
        // TS block shape: the references note rides inside the body,
        // ahead of the frontmatter-stripped skill content.
        assert_eq!(
            parsed.content,
            format!(
                "References are relative to {}.\n\nUse web-search well.",
                dir.path().display()
            )
        );
        assert_eq!(parsed.user_message, None);
        assert!(parsed.location.ends_with("SKILL.md"));

        let (expanded, used) = expand_skill_command(
            "/skill:web-search find rust tuis",
            &skills,
            &variables(dir.path()),
        )
        .await;
        assert_eq!(used.map(|skill| skill.name.as_str()), Some("web-search"));
        let parsed = pa_types::skill_blocks::parse_skill_block(&expanded).expect("parses");
        assert_eq!(
            parsed.user_message.as_deref(),
            Some("find rust tuis"),
            "args follow the block after a blank line"
        );
    }

    /// `/code-review` and `/review` expand exactly like `/skill:code-review`;
    /// without the skill loaded they pass through for the model.
    #[tokio::test]
    async fn skill_commands_expand_to_their_bundled_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = [temp_skill("code-review", dir.path())];
        let (via_skill, _) = expand_skill_command(
            "/skill:code-review high --fix",
            &skills,
            &variables(dir.path()),
        )
        .await;
        for text in ["/code-review high --fix", "/review high --fix"] {
            let (expanded, used) =
                expand_skill_command(text, &skills, &variables(dir.path())).await;
            assert_eq!(used.map(|skill| skill.name.as_str()), Some("code-review"));
            assert_eq!(expanded, via_skill);
        }
        let parsed = pa_types::skill_blocks::parse_skill_block(&via_skill).expect("parses");
        assert_eq!(parsed.user_message.as_deref(), Some("high --fix"));
        let (unchanged, used) = expand_skill_command("/review", &[], &variables(dir.path())).await;
        assert_eq!((unchanged.as_str(), used.is_none()), ("/review", true));
        assert!(
            expand_skill_command("/model", &skills, &variables(dir.path()))
                .await
                .1
                .is_none()
        );
    }

    #[test]
    fn an_empty_frontmatter_block_is_a_warned_skip_not_a_panic() {
        // TS no-frontmatter semantics: the whole document is the body, the
        // description is missing, the skill is NOT loaded (warning).
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("bare");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(skill_dir.join("SKILL.md"), "---\n---\nUse it well.").expect("write");
        let result = crate::skills::load_skills_from_dir(dir.path(), "user");
        assert!(result.skills.is_empty());
        assert!(result.diagnostics.iter().any(
            |d| matches!(d, crate::skills::ResourceDiagnostic::Warning { message, .. }
                if message == "description is required")
        ));
    }

    #[test]
    fn an_empty_frontmatter_name_falls_back_to_the_parent_directory() {
        // TS `frontmatter.name || parentDirName`.
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("named-dir");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: \"\"\ndescription: The named dir skill\n---\nbody",
        )
        .expect("write");
        let result = crate::skills::load_skills_from_dir(dir.path(), "user");
        assert_eq!(
            result
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["named-dir"]
        );
    }

    #[tokio::test]
    async fn non_skill_and_unknown_inputs_pass_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = [temp_skill("web-search", dir.path())];
        for text in [
            "hello world",
            "/compact",
            "/skill:missing do something",
            "/skill:",
        ] {
            let (expanded, used) =
                expand_skill_command(text, &skills, &variables(dir.path())).await;
            assert_eq!(expanded, text, "{text} must pass through");
            assert!(used.is_none(), "{text} must not report a skill");
        }
    }

    #[tokio::test]
    async fn an_unreadable_skill_file_passes_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut skill = temp_skill("gone", dir.path());
        std::fs::remove_file(&skill.file_path).expect("remove skill file");
        skill.file_path = dir.path().join("missing-SKILL.md");
        let skills = [skill];
        let (expanded, used) =
            expand_skill_command("/skill:gone", &skills, &variables(dir.path())).await;
        assert_eq!(expanded, "/skill:gone");
        assert!(used.is_none());
    }

    #[test]
    fn prompt_formatting_excludes_hidden() {
        let skill = |name: &str, disable: bool| Skill {
            name: name.to_string(),
            description: format!("Does {name}"),
            file_path: PathBuf::from("/s/SKILL.md"),
            base_dir: PathBuf::from("/s"),
            source_info: create_synthetic_source_info("/s", "user", SourceScope::User, None),
            disable_model_invocation: disable,
            kind: SkillKind::Markdown,
            python: None,
            when_to_use: None,
            user_invocable: true,
            claude: None,
        };
        let hidden = format_skills_for_prompt(&[skill("hidden", true)]);
        assert_eq!(hidden, "");
        let formatted = format_skills_for_prompt(&[skill("web-search", false)]);
        assert!(formatted.contains("<name>web-search</name>"));
        assert!(formatted.contains("<type>markdown</type>"));
        assert!(formatted.contains("<description>Does web-search</description>"));
    }
}
