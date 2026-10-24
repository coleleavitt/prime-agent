//! Agent definition discovery: Claude Code's `agents/` directories, read
//! fresh on every lookup (a definition written mid-session is spawnable at
//! once).
//!
//! Search order, first definition of a name wins:
//!
//! 1. project: `.prime/agents/*.md`, then `.claude/agents/*.md`, in the
//!    working directory and then each ancestor up to the repository root
//!    (the nearest directory wins; outside a repository only the working
//!    directory is searched);
//! 2. user: `<agent dir>/agents/*.md` (`~/.prime/agent/agents`), then
//!    `~/.claude/agents/*.md`.
//!
//! A file that does not parse is skipped with a diagnostic; a later file
//! with a taken name is shadowed with one.

use std::path::{Path, PathBuf};

use super::definition::{AgentDefinition, AgentSource, parse_agent_definition};
use crate::settings::SettingsManager;

/// The project-relative definition directories, in precedence order.
pub const PROJECT_AGENT_DIRS: [&str; 2] = [".prime/agents", ".claude/agents"];

/// Where to look for definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSearch {
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The agent directory (`~/.prime/agent` or its override).
    pub agent_dir: PathBuf,
    /// The home directory whose `.claude/agents` is read; `None` skips it.
    pub home: Option<PathBuf>,
}

impl AgentSearch {
    /// The search for a session: `home` is the platform home directory
    /// (a test process's isolated one).
    #[must_use]
    pub fn for_session(cwd: &Path, agent_dir: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            home: pa_types::platform::home_dir(),
        }
    }

    /// The directories searched, in precedence order, with their scope.
    #[must_use]
    pub fn directories(&self) -> Vec<(PathBuf, AgentSource)> {
        let mut dirs = Vec::new();
        for dir in project_ancestors(&self.cwd) {
            for relative in PROJECT_AGENT_DIRS {
                dirs.push((dir.join(relative), AgentSource::Project));
            }
        }
        dirs.push((self.agent_dir.join("agents"), AgentSource::User));
        if let Some(home) = &self.home {
            dirs.push((home.join(".claude").join("agents"), AgentSource::User));
        }
        dirs
    }
}

/// The working directory and its ancestors up to the repository root (the
/// first directory holding `.git`); just the working directory outside a
/// repository.
fn project_ancestors(cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    for dir in cwd.ancestors() {
        chain.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            return chain;
        }
    }
    vec![cwd.to_path_buf()]
}

/// The discovered definitions and the files that were skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCatalog {
    /// The winning definition per name, sorted by name.
    pub agents: Vec<AgentDefinition>,
    /// One line per skipped or shadowed file.
    pub diagnostics: Vec<String>,
}

impl AgentCatalog {
    /// The definition named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|agent| agent.name == name)
    }
}

/// The `*.md` files of one directory, sorted by file name; a missing or
/// unreadable directory has none.
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
        .collect();
    files.sort();
    files
}

/// Discover every definition `search` reaches, after the command line's
/// `--agents` ones (the launch layer), which win a name collision.
#[must_use]
pub fn discover_agents(search: &AgentSearch) -> AgentCatalog {
    let launch = crate::settings::launch::LaunchSettings::current();
    discover_agents_with(
        search,
        launch.as_ref().and_then(|launch| launch.agents.as_ref()),
    )
}

/// [`discover_agents`] with explicit `--agents` definitions.
#[must_use]
pub fn discover_agents_with(
    search: &AgentSearch,
    cli_agents: Option<&serde_json::Map<String, serde_json::Value>>,
) -> AgentCatalog {
    let mut catalog = AgentCatalog::default();
    for (name, value) in cli_agents.into_iter().flatten() {
        match super::definition::parse_agent_json(name, value) {
            Ok(definition) => catalog.agents.push(definition),
            Err(reason) => catalog.diagnostics.push(format!("--agents: {reason}")),
        }
    }
    let mut seen_dirs: Vec<PathBuf> = Vec::new();
    for (dir, source) in search.directories() {
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen_dirs.contains(&canonical) {
            continue;
        }
        seen_dirs.push(canonical);
        for path in markdown_files(&dir) {
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    catalog
                        .diagnostics
                        .push(format!("{}: unreadable: {error}", path.display()));
                    continue;
                }
            };
            match parse_agent_definition(&content, &path, source) {
                Ok(definition) => {
                    if let Some(winner) = catalog.get(&definition.name) {
                        catalog.diagnostics.push(format!(
                            "{}: agent \"{}\" is shadowed by {}",
                            path.display(),
                            definition.name,
                            winner.path.display()
                        ));
                    } else {
                        catalog.agents.push(definition);
                    }
                }
                Err(reason) => catalog
                    .diagnostics
                    .push(format!("{}: {reason}", path.display())),
            }
        }
    }
    catalog.agents.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// The agent a top-level session runs as: `--agent`, else the `agent`
/// setting (Claude Code's), resolved against the definitions; `None` when
/// neither names one or the name is unknown (logged).
#[must_use]
pub fn main_session_agent(
    cwd: &Path,
    agent_dir: &Path,
    settings: &SettingsManager,
) -> Option<AgentDefinition> {
    let launch = crate::settings::launch::LaunchSettings::current().unwrap_or_default();
    let name = launch.agent.clone().or_else(|| {
        settings
            .settings()
            .extra
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    })?;
    let catalog = discover_agents_with(
        &AgentSearch::for_session(cwd, agent_dir),
        launch.agents.as_ref(),
    );
    let found = catalog.get(&name).cloned();
    if found.is_none() {
        tracing::warn!(target: "pa_core::agents", agent = name, "the session's agent is not defined");
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\nbody of {description}"),
        )
        .unwrap();
    }

    #[test]
    fn project_definitions_shadow_user_ones_and_bad_files_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let cwd = repo.join("crates/x");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let agent_dir = root.path().join("agent");
        let home = root.path().join("home");
        // The nearest project dir wins, `.prime` before `.claude`.
        write(&cwd.join(".claude/agents/a.md"), "alpha", "nearest claude");
        write(&repo.join(".prime/agents/a.md"), "alpha", "repo prime");
        write(&repo.join(".claude/agents/b.md"), "beta", "repo claude");
        write(&agent_dir.join("agents/b.md"), "beta", "user prime");
        write(&home.join(".claude/agents/c.md"), "gamma", "user claude");
        std::fs::write(repo.join(".prime/agents/bad.md"), "no frontmatter").unwrap();
        // Above the repository root: never searched.
        write(&root.path().join(".claude/agents/d.md"), "delta", "outside");

        let catalog = discover_agents(&AgentSearch {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            home: Some(home),
        });
        let found: Vec<(String, String, AgentSource)> = catalog
            .agents
            .iter()
            .map(|agent| (agent.name.clone(), agent.description.clone(), agent.source))
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "alpha".to_string(),
                    "nearest claude".to_string(),
                    AgentSource::Project
                ),
                (
                    "beta".to_string(),
                    "repo claude".to_string(),
                    AgentSource::Project
                ),
                (
                    "gamma".to_string(),
                    "user claude".to_string(),
                    AgentSource::User
                ),
            ]
        );
        assert_eq!(
            catalog.diagnostics,
            vec![
                format!(
                    "{}: agent \"alpha\" is shadowed by {}",
                    repo.join(".prime/agents/a.md").display(),
                    cwd.join(".claude/agents/a.md").display()
                ),
                format!(
                    "{}: missing YAML frontmatter",
                    repo.join(".prime/agents/bad.md").display()
                ),
                format!(
                    "{}: agent \"beta\" is shadowed by {}",
                    agent_dir.join("agents/b.md").display(),
                    repo.join(".claude/agents/b.md").display()
                ),
            ]
        );
    }

    #[test]
    fn outside_a_repository_only_the_working_directory_is_searched() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("work");
        write(&cwd.join(".prime/agents/a.md"), "here", "d");
        write(&root.path().join(".prime/agents/b.md"), "parent", "d");
        let search = AgentSearch {
            cwd,
            agent_dir: root.path().join("agent"),
            home: None,
        };
        let names: Vec<String> = discover_agents(&search)
            .agents
            .into_iter()
            .map(|agent| agent.name)
            .collect();
        assert_eq!(names, vec!["here".to_string()]);
    }

    /// `--agents` definitions win a name collision with the files.
    #[test]
    fn command_line_agents_shadow_files() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("w");
        write(&cwd.join(".claude/agents/a.md"), "alpha", "from file");
        let cli = serde_json::json!({
            "alpha": {"description": "from flag", "prompt": "p"},
            "broken": {"description": "no prompt"},
        });
        let catalog = discover_agents_with(
            &AgentSearch {
                cwd,
                agent_dir: root.path().join("agent"),
                home: None,
            },
            cli.as_object(),
        );
        let alpha = catalog.get("alpha").unwrap();
        assert_eq!(
            (alpha.description.as_str(), alpha.source),
            ("from flag", AgentSource::CliArg)
        );
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|line| line.starts_with("--agents: "))
        );
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|line| line.contains("shadowed"))
        );
    }
}
