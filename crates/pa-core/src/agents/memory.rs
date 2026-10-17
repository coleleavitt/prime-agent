//! Persistent agent memory: Claude Code's `memory` frontmatter on an agent
//! definition (`user`, `project`, `local`). A session running as such an
//! agent gets a memory directory of its own, its `MEMORY.md` index in the
//! system prompt under Claude Code's instructions (the auto-memory
//! guidance, titled "Persistent Agent Memory", with the scope's note), and
//! the directory writable under the OS sandbox.
//!
//! Where the memory lives (`<agent>` is the agent type with every
//! character outside `A-Za-z0-9-_` replaced by `-`, Claude Code's rule):
//!
//! | scope | Prime Agent directory | Claude Code's directory |
//! |---|---|---|
//! | `user` | `<agent dir>/agent-memory/<agent>/` | `~/.claude/agent-memory/<agent>/` |
//! | `project` | `<project>/.prime/agent-memory/<agent>/` | `<project>/.claude/agent-memory/<agent>/` |
//! | `local` | `<project>/.prime/agent-memory-local/<agent>/` | `<project>/.claude/agent-memory-local/<agent>/` |
//!
//! `<project>` is the project root auto memory uses (the main checkout of a
//! git worktree, so an agent isolated in a worktree keeps its project's
//! memory). The Prime Agent directory is the memory unless it does not
//! exist yet and Claude Code's does: then Claude Code's directory is the
//! memory, read and written in place, so an agent shared with Claude Code
//! keeps one memory. `local` is meant to stay out of version control (Claude
//! Code's `.claude/agent-memory-local/`); add `.prime/agent-memory-local/`
//! to the project's `.gitignore`.
//!
//! A `project` or `local` directory must resolve inside the project (Claude
//! Code's check): one that leads elsewhere through a link, or whose
//! `MEMORY.md` is a link or a special file, is read-only for the session
//! (the index is not read, nothing is created, and the sandbox does not
//! open it). Agent memory follows auto memory's switches
//! (`autoMemoryEnabled`, `PRIME_AGENT_DISABLE_AUTO_MEMORY`,
//! `--no-context-files`).

use std::path::{Path, PathBuf};

use super::definition::AgentDefinition;
use crate::resources::auto_memory::{
    MEMORY_INDEX,
    MemorySection,
    cap_index,
    render_memory_section,
};

/// The prompt section's title (Claude Code's).
pub const AGENT_MEMORY_TITLE: &str = "Persistent Agent Memory";

/// An agent memory's scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMemoryScope {
    User,
    Project,
    Local,
}

impl AgentMemoryScope {
    /// Parse a definition's `memory` value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// Whether the directory lives in the project.
    #[must_use]
    pub fn in_project(self) -> bool {
        match self {
            Self::User => false,
            Self::Project | Self::Local => true,
        }
    }

    /// The scope's guideline line (Claude Code's wording).
    #[must_use]
    pub fn guideline(self) -> &'static str {
        match self {
            Self::User => {
                "- Since this memory is user-scope, keep learnings general since they apply across \
                 all projects"
            }
            Self::Project => {
                "- Since this memory is project-scope and shared with your team via version \
                 control, tailor your memories to this project"
            }
            Self::Local => {
                "- Since this memory is local-scope (not checked into version control), tailor \
                 your memories to this project and machine"
            }
        }
    }
}

/// An agent type as a directory name (Claude Code's: every character
/// outside `A-Za-z0-9-_` becomes `-`; empty is `unknown`).
#[must_use]
pub fn agent_dir_name(agent: &str) -> String {
    let name: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name
    }
}

/// Where a session's agent memories live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMemoryRoots {
    /// The agent directory (`~/.prime/agent`).
    pub agent_dir: PathBuf,
    /// The project root (auto memory's).
    pub project: PathBuf,
    /// Claude Code's user directory (`~/.claude`); `None` skips it.
    pub claude_user_dir: Option<PathBuf>,
}

impl AgentMemoryRoots {
    /// The roots of a session at `cwd` on this machine.
    #[must_use]
    pub fn for_session(cwd: &Path, agent_dir: &Path) -> Self {
        Self {
            agent_dir: agent_dir.to_path_buf(),
            project: crate::resources::auto_memory::project_root(cwd),
            claude_user_dir: crate::settings::claude_code::ClaudePaths::resolve(cwd).claude_dir,
        }
    }

    /// Prime Agent's directory for `agent`'s `scope` memory.
    #[must_use]
    pub fn own_dir(&self, scope: AgentMemoryScope, agent: &str) -> PathBuf {
        let name = agent_dir_name(agent);
        match scope {
            AgentMemoryScope::User => self.agent_dir.join("agent-memory").join(name),
            AgentMemoryScope::Project => self.project.join(".prime/agent-memory").join(name),
            AgentMemoryScope::Local => self.project.join(".prime/agent-memory-local").join(name),
        }
    }

    /// Claude Code's directory for `agent`'s `scope` memory.
    #[must_use]
    pub fn claude_dir(&self, scope: AgentMemoryScope, agent: &str) -> Option<PathBuf> {
        let name = agent_dir_name(agent);
        match scope {
            AgentMemoryScope::User => self
                .claude_user_dir
                .as_ref()
                .map(|dir| dir.join("agent-memory").join(name)),
            AgentMemoryScope::Project => Some(self.project.join(".claude/agent-memory").join(name)),
            AgentMemoryScope::Local => {
                Some(self.project.join(".claude/agent-memory-local").join(name))
            }
        }
    }
}

/// One agent's memory as a session loads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMemory {
    pub agent: String,
    pub scope: AgentMemoryScope,
    pub dir: PathBuf,
    /// The index text as the prompt carries it (capped).
    pub index: String,
    pub truncated: bool,
    /// The memory is Claude Code's directory (shared with Claude Code).
    pub shared_with_claude_code: bool,
    /// The memory cannot be written this session (see the module docs).
    pub read_only: bool,
}

impl AgentMemory {
    /// The system prompt section for this memory.
    #[must_use]
    pub fn prompt_section(&self) -> String {
        render_memory_section(&MemorySection {
            title: AGENT_MEMORY_TITLE,
            dir: &self.dir,
            index: &self.index,
            extra_guidelines: &[self.scope.guideline().to_string()],
            read_only: self.read_only,
        })
    }
}

/// The memory `definition` asks for: its agent type and scope (`None`
/// without a `memory` key; an unknown scope is logged and ignored, as
/// Claude Code does).
#[must_use]
pub fn memory_request(definition: &AgentDefinition) -> Option<(String, AgentMemoryScope)> {
    let value = definition.memory.as_deref()?;
    let Some(scope) = AgentMemoryScope::parse(value) else {
        tracing::warn!(
            target: "pa_core::agent_memory",
            agent = definition.name.as_str(),
            "invalid memory value {value:?} (valid: user, project, local); no agent memory"
        );
        return None;
    };
    Some((definition.name.clone(), scope))
}

/// The instructions of a session running as `definition` (appended to its
/// system prompt): the definition's, then its memory's section. A memory
/// the agent can write puts `Read`, `Write` and `Edit` in a restricted tool
/// scope (Claude Code adds them so the agent can keep its memory).
#[must_use]
pub fn agent_prompt(definition: &AgentDefinition, memory: Option<&AgentMemory>) -> String {
    let Some(memory) = memory else {
        return super::agent_instructions(definition);
    };
    let mut definition = definition.clone();
    if !memory.read_only {
        if let Some(tools) = definition.tools.as_mut() {
            for tool in ["Read", "Write", "Edit"] {
                if !tools.iter().any(|known| known.trim() == tool) {
                    tools.push(tool.to_string());
                }
            }
        }
    }
    format!(
        "{}\n\n{}",
        super::agent_instructions(&definition),
        memory.prompt_section()
    )
}

/// The nearest existing ancestor of `path`, canonicalized, with the
/// missing components re-joined: where `path` resolves (or would).
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(current) {
            return Some(
                missing
                    .iter()
                    .rev()
                    .fold(resolved, |acc, part| acc.join(part)),
            );
        }
        missing.push(current.file_name()?.to_os_string());
        current = current.parent()?;
    }
}

/// Whether `dir` resolves inside `root` (links followed).
fn inside(dir: &Path, root: &Path) -> bool {
    match (resolve(dir), std::fs::canonicalize(root)) {
        (Some(dir), Ok(root)) => dir.starts_with(root),
        _ => false,
    }
}

/// Load `agent`'s `scope` memory, creating its directory so the model can
/// write without checking first (not for a read-only one).
#[must_use]
pub fn load_agent_memory(
    roots: &AgentMemoryRoots,
    agent: &str,
    scope: AgentMemoryScope,
) -> AgentMemory {
    let own = roots.own_dir(scope, agent);
    let claude = roots.claude_dir(scope, agent);
    let (dir, shared_with_claude_code) = match claude {
        Some(claude) if !own.exists() && claude.is_dir() => (claude, true),
        _ => (own, false),
    };
    let index_path = dir.join(MEMORY_INDEX);
    let read_only = scope.in_project()
        && (!inside(&dir, &roots.project)
            || std::fs::symlink_metadata(&index_path)
                .is_ok_and(|meta| !meta.file_type().is_file()));
    if read_only {
        tracing::warn!(
            target: "pa_core::agent_memory",
            dir = %dir.display(),
            "agent memory leads out of the working copy or its index is not a regular file; \
             read-only this session"
        );
        return AgentMemory {
            agent: agent.to_string(),
            scope,
            dir,
            index: String::new(),
            truncated: false,
            shared_with_claude_code,
            read_only,
        };
    }
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "pa_core::agent_memory",
            dir = %dir.display(),
            %error,
            "the agent-memory directory could not be created"
        );
    }
    let raw = std::fs::read_to_string(&index_path).unwrap_or_default();
    let (index, truncated) = cap_index(&raw);
    AgentMemory {
        agent: agent.to_string(),
        scope,
        dir,
        index,
        truncated,
        shared_with_claude_code,
        read_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(base: &Path) -> AgentMemoryRoots {
        let project = base.join("repo");
        std::fs::create_dir_all(&project).unwrap();
        AgentMemoryRoots {
            agent_dir: base.join("agent"),
            project,
            claude_user_dir: Some(base.join("home/.claude")),
        }
    }

    #[test]
    fn scopes_and_names_follow_claude_code() {
        assert_eq!(
            ["user", " project ", "local", "team"].map(AgentMemoryScope::parse),
            [
                Some(AgentMemoryScope::User),
                Some(AgentMemoryScope::Project),
                Some(AgentMemoryScope::Local),
                None
            ]
        );
        assert_eq!(
            agent_dir_name("plugin:code-reviewer.v2"),
            "plugin-code-reviewer-v2"
        );
        assert_eq!(agent_dir_name(""), "unknown");
    }

    #[test]
    fn each_scope_has_its_own_directory_created_on_load() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        let dirs = [
            AgentMemoryScope::User,
            AgentMemoryScope::Project,
            AgentMemoryScope::Local,
        ]
        .map(|scope| load_agent_memory(&roots, "reviewer", scope).dir);
        assert_eq!(
            dirs,
            [
                tmp.path().join("agent/agent-memory/reviewer"),
                tmp.path().join("repo/.prime/agent-memory/reviewer"),
                tmp.path().join("repo/.prime/agent-memory-local/reviewer"),
            ]
        );
        assert!(dirs.iter().all(|dir| dir.is_dir()));
    }

    #[test]
    fn the_index_is_loaded_capped_and_claude_codes_directory_is_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        let claude = tmp.path().join("repo/.claude/agent-memory/reviewer");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join(MEMORY_INDEX), "- [Style](style.md) - tabs\n").unwrap();
        let memory = load_agent_memory(&roots, "reviewer", AgentMemoryScope::Project);
        assert_eq!(
            (
                memory.dir.clone(),
                memory.index.as_str(),
                memory.shared_with_claude_code,
                memory.read_only
            ),
            (claude, "- [Style](style.md) - tabs", true, false)
        );
        // Once Prime Agent's own directory exists, it is the memory.
        let own = roots.own_dir(AgentMemoryScope::Project, "reviewer");
        std::fs::create_dir_all(&own).unwrap();
        let memory = load_agent_memory(&roots, "reviewer", AgentMemoryScope::Project);
        assert_eq!(
            (
                memory.dir,
                memory.index.as_str(),
                memory.shared_with_claude_code
            ),
            (own, "", false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_project_memory_that_leads_out_of_the_project_is_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join(MEMORY_INDEX), "secret").unwrap();
        std::fs::create_dir_all(roots.project.join(".prime")).unwrap();
        std::os::unix::fs::symlink(&outside, roots.project.join(".prime/agent-memory")).unwrap();
        let memory = load_agent_memory(&roots, "reviewer", AgentMemoryScope::Project);
        assert_eq!((memory.read_only, memory.index.as_str()), (true, ""));
        assert!(!outside.join("reviewer").exists(), "nothing is created");
        assert!(
            memory
                .prompt_section()
                .contains("Treat this agent memory as read-only in this session")
        );
        // A user memory is the user's own: links are followed.
        let user = tmp.path().join("agent");
        std::fs::create_dir_all(&user).unwrap();
        std::os::unix::fs::symlink(&outside, user.join("agent-memory")).unwrap();
        let memory = load_agent_memory(&roots, "reviewer", AgentMemoryScope::User);
        assert!(!memory.read_only);
    }

    #[test]
    fn an_agent_with_memory_reads_its_section_and_may_keep_it() {
        let definition = crate::agents::parse_agent_definition(
            "---\nname: reviewer\ndescription: d\ntools: Grep\nmemory: user\n---\nYou review.",
            Path::new("reviewer.md"),
            crate::agents::AgentSource::User,
        )
        .unwrap();
        assert_eq!(
            memory_request(&definition),
            Some(("reviewer".to_string(), AgentMemoryScope::User))
        );
        let invalid = AgentDefinition {
            memory: Some("team".into()),
            ..definition.clone()
        };
        assert_eq!(memory_request(&invalid), None);
        assert_eq!(
            agent_prompt(&definition, None),
            crate::agents::agent_instructions(&definition)
        );
        let memory = AgentMemory {
            agent: "reviewer".into(),
            scope: AgentMemoryScope::User,
            dir: PathBuf::from("/agent/agent-memory/reviewer"),
            index: String::new(),
            truncated: false,
            shared_with_claude_code: false,
            read_only: false,
        };
        let prompt = agent_prompt(&definition, Some(&memory));
        assert!(prompt.starts_with(
            "# Agent: reviewer\n\nYou review.\n\nTool scope (Claude Code tool names): Grep, Read, \
             Write, Edit."
        ));
        assert!(prompt.contains(
            "\n\n# Persistent Agent Memory\n\nYou have a persistent, file-based memory system at \
             `/agent/agent-memory/reviewer`."
        ));
        assert!(prompt.ends_with(
            "## MEMORY.md\n\nYour MEMORY.md is currently empty. When you save new memories, they \
             will appear here."
        ));
    }

    #[test]
    fn the_prompt_section_is_claude_codes_with_the_scope_note() {
        let memory = AgentMemory {
            agent: "reviewer".into(),
            scope: AgentMemoryScope::Local,
            dir: PathBuf::from("/repo/.prime/agent-memory-local/reviewer"),
            index: "- [A](a.md) - b".into(),
            truncated: false,
            shared_with_claude_code: false,
            read_only: false,
        };
        let section = memory.prompt_section();
        assert!(section.starts_with(
            "# Persistent Agent Memory\n\nYou have a persistent, file-based memory system at \
             `/repo/.prime/agent-memory-local/reviewer`."
        ));
        assert!(section.ends_with(
            "- Since this memory is local-scope (not checked into version control), tailor your \
             memories to this project and machine\n\n## MEMORY.md\n\n- [A](a.md) - b"
        ));
        assert!(section.contains("## How to save memories"));
    }
}
