//! One agent definition file: Claude Code's `.claude/agents/<name>.md`
//! shape, YAML frontmatter over a Markdown body that becomes the agent's
//! instructions.
//!
//! Frontmatter keys read (Claude Code's names): `name` and `description`
//! (required), `tools` and `disallowedTools` (a comma-separated string or a
//! list), `model`, `effort`, `color`, `isolation` (`worktree`), `memory`,
//! `skills`, `maxTurns`, `background`, `initialPrompt`. Other keys
//! (`permissionMode`, `hooks`, `mcpServers`) are accepted and ignored.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Where a definition was discovered; earlier scopes win a name collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentSource {
    /// `.prime/agents` or `.claude/agents` in the working directory or an
    /// ancestor inside the repository.
    Project,
    /// `<agent dir>/agents` or `~/.claude/agents`.
    User,
}

impl AgentSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// How a spawned agent is isolated from its parent's checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentIsolation {
    /// A fresh git worktree of the parent's repository.
    Worktree,
}

impl AgentIsolation {
    /// Parse the `isolation` value (`worktree`); anything else is `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "worktree" => Some(Self::Worktree),
            _ => None,
        }
    }
}

/// One parsed agent definition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefinition {
    /// The agent type (`rlm.spawn(agent=...)`).
    pub name: String,
    /// When to use the agent (the listing line).
    pub description: String,
    /// The Markdown body: the agent's instructions.
    pub prompt: String,
    /// The allowed tools; `None` allows every tool.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    /// A model selector; `None` (or `inherit`) keeps the parent's.
    pub model: Option<String>,
    /// The reasoning effort, a thinking level (`low` .. `max`).
    pub effort: Option<String>,
    pub color: Option<String>,
    pub isolation: Option<AgentIsolation>,
    /// Claude Code's persistent agent memory scope (`user`, `project`,
    /// `local`; see [`super::AgentMemory`]).
    pub memory: Option<String>,
    /// Skills the agent loads before its task.
    pub skills: Vec<String>,
    pub max_turns: Option<u32>,
    /// Whether the agent is meant to run in the background (every
    /// `rlm.spawn` child does).
    pub background: bool,
    /// A first user turn sent before the task prompt.
    pub initial_prompt: Option<String>,
    pub source: AgentSource,
    pub path: PathBuf,
}

/// A list-valued key: a YAML list of strings or one comma-separated string.
fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(text) => Some(
            text.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

fn string_key(frontmatter: &Value, key: &str) -> Option<String> {
    frontmatter
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Whether `name` is a usable agent type: letters, digits, `-`, `_`, `.`,
/// `:`, at most 64 characters.
#[must_use]
pub fn is_valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// Parse one definition file's text. `Err` names what is missing or
/// invalid (the discovery diagnostic).
///
/// # Errors
///
/// No frontmatter, a missing or invalid `name`, or a missing `description`.
pub fn parse_agent_definition(
    content: &str,
    path: &Path,
    source: AgentSource,
) -> Result<AgentDefinition, String> {
    let (frontmatter, body) = crate::skills::frontmatter::parse_frontmatter(content);
    if frontmatter
        .as_object()
        .is_none_or(serde_json::Map::is_empty)
    {
        return Err("missing YAML frontmatter".to_string());
    }
    let name = string_key(&frontmatter, "name")
        .ok_or_else(|| "missing required field \"name\"".to_string())?;
    if !is_valid_agent_name(&name) {
        return Err(format!("invalid agent name {name:?}"));
    }
    let description = string_key(&frontmatter, "description")
        .ok_or_else(|| "missing required field \"description\"".to_string())?;
    let model = string_key(&frontmatter, "model").filter(|model| model != "inherit");
    let isolation = match string_key(&frontmatter, "isolation") {
        None => None,
        Some(value) => Some(
            AgentIsolation::parse(&value)
                .ok_or_else(|| format!("unsupported isolation {value:?} (use \"worktree\")"))?,
        ),
    };
    let max_turns = frontmatter
        .get("maxTurns")
        .and_then(Value::as_u64)
        .and_then(|turns| u32::try_from(turns).ok())
        .filter(|turns| *turns > 0);
    Ok(AgentDefinition {
        name,
        description,
        prompt: body.trim().to_string(),
        tools: string_list(frontmatter.get("tools")),
        disallowed_tools: string_list(frontmatter.get("disallowedTools")).unwrap_or_default(),
        model,
        effort: string_key(&frontmatter, "effort").map(|effort| effort.to_lowercase()),
        color: string_key(&frontmatter, "color"),
        isolation,
        memory: string_key(&frontmatter, "memory"),
        skills: string_list(frontmatter.get("skills")).unwrap_or_default(),
        max_turns,
        background: frontmatter.get("background").and_then(Value::as_bool) == Some(true),
        initial_prompt: string_key(&frontmatter, "initialPrompt"),
        source,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_definition_parses_every_key() {
        let text = "---\nname: reviewer\ndescription: Reviews diffs for bugs\ntools: Read, Grep, Bash\n\
                    disallowedTools: [Agent]\nmodel: anthropic/claude-sonnet-4-5\neffort: High\ncolor: blue\n\
                    isolation: worktree\nmemory: project\nskills:\n  - tasks\nmaxTurns: 12\nbackground: true\n\
                    initialPrompt: Read the diff first.\npermissionMode: plan\n---\n\nYou review code.\n";
        let parsed =
            parse_agent_definition(text, Path::new("/r/reviewer.md"), AgentSource::Project)
                .unwrap();
        assert_eq!(
            parsed,
            AgentDefinition {
                name: "reviewer".into(),
                description: "Reviews diffs for bugs".into(),
                prompt: "You review code.".into(),
                tools: Some(vec!["Read".into(), "Grep".into(), "Bash".into()]),
                disallowed_tools: vec!["Agent".into()],
                model: Some("anthropic/claude-sonnet-4-5".into()),
                effort: Some("high".into()),
                color: Some("blue".into()),
                isolation: Some(AgentIsolation::Worktree),
                memory: Some("project".into()),
                skills: vec!["tasks".into()],
                max_turns: Some(12),
                background: true,
                initial_prompt: Some("Read the diff first.".into()),
                source: AgentSource::Project,
                path: PathBuf::from("/r/reviewer.md"),
            }
        );
    }

    #[test]
    fn inherit_model_and_absent_lists_are_defaults() {
        let parsed = parse_agent_definition(
            "---\nname: a\ndescription: d\nmodel: inherit\n---\nbody",
            Path::new("a.md"),
            AgentSource::User,
        )
        .unwrap();
        assert_eq!(
            (
                parsed.model,
                parsed.tools,
                parsed.disallowed_tools,
                parsed.isolation
            ),
            (None, None, Vec::<String>::new(), None)
        );
    }

    #[test]
    fn invalid_files_name_the_problem() {
        let errors = [
            "no frontmatter",
            "---\ndescription: d\n---\nb",
            "---\nname: bad name\ndescription: d\n---\nb",
            "---\nname: a\n---\nb",
            "---\nname: a\ndescription: d\nisolation: container\n---\nb",
        ]
        .map(|text| {
            parse_agent_definition(text, Path::new("x.md"), AgentSource::User).unwrap_err()
        });
        assert_eq!(
            errors,
            [
                "missing YAML frontmatter".to_string(),
                "missing required field \"name\"".to_string(),
                "invalid agent name \"bad name\"".to_string(),
                "missing required field \"description\"".to_string(),
                "unsupported isolation \"container\" (use \"worktree\")".to_string(),
            ]
        );
    }
}
