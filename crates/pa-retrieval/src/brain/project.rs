//! Project identity: magic-context's `git:<root commit>` key (so its rows,
//! this agent dir's sessions and the auto-memory directory agree on what
//! "this project" is), plus the auto-memory slug of the project root.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};

use pa_core::resources::auto_memory::{project_root, sanitize};

/// One working directory's project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// `git:<lexicographically smallest root commit>` (magic-context's
    /// identity: stable across worktrees, clones and grafted histories), or
    /// `None` outside a repository with commits.
    pub(crate) identity: Option<String>,
    /// The auto-memory directory name of the project root
    /// (`<agent dir>/projects/<slug>`).
    pub(crate) slug: String,
}

impl Project {
    /// Whether a memory recorded under `identity` / `slug` belongs here.
    pub(crate) fn contains(&self, identity: Option<&str>, slug: Option<&str>) -> bool {
        identity.is_some_and(|identity| self.identity.as_deref() == Some(identity))
            || slug.is_some_and(|slug| slug == self.slug)
    }

    /// The key a synthesized summary of this project is stored under.
    pub(crate) fn summary_key(&self) -> String {
        self.identity
            .clone()
            .unwrap_or_else(|| format!("slug:{}", self.slug))
    }
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Project>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Project>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// The project of `cwd`, cached per directory for the process.
pub(crate) fn project_of(cwd: &Path) -> Project {
    if let Some(project) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(cwd)
    {
        return project.clone();
    }
    let root = project_root(cwd);
    let project = Project {
        identity: git_root_commit(&root).map(|commit| format!("git:{commit}")),
        slug: sanitize(&root),
    };
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(cwd.to_path_buf(), project.clone());
    project
}

/// The smallest root commit of `dir`'s repository (`git rev-list
/// --max-parents=0 HEAD`; a grafted history has several roots and git's
/// order varies, so magic-context pins the minimum).
fn git_root_commit(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    root_commit_from(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn root_commit_from(rev_list: &str) -> Option<String> {
    rev_list
        .lines()
        .map(str::trim)
        .filter(|line| {
            (7..=64).contains(&line.len()) && line.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .min()
        .map(str::to_ascii_lowercase)
}

/// The working directory a session started in: the `cwd` of its transcript
/// header (`sessions/<id>.jsonl`, else `sessions-archive/<id>.jsonl`).
pub(crate) fn session_cwd(agent_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let file = [
        agent_dir.join("sessions"),
        agent_dir.join("sessions-archive"),
    ]
    .into_iter()
    .map(|dir| dir.join(format!("{session_id}.jsonl")))
    .find(|path| path.is_file())?;
    let mut header = String::new();
    BufReader::new(std::fs::File::open(file).ok()?)
        .read_line(&mut header)
        .ok()?;
    let header: serde_json::Value = serde_json::from_str(&header).ok()?;
    if header.get("type").and_then(serde_json::Value::as_str) != Some("session") {
        return None;
    }
    header
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_root_commit_wins_and_noise_is_ignored() {
        assert_eq!(
            root_commit_from("f00d\nbeef1234567\nabc1234567890\nnot-a-hash\n"),
            Some("abc1234567890".to_string())
        );
        assert_eq!(root_commit_from(""), None);
    }

    #[test]
    fn a_session_cwd_comes_from_its_header() {
        let agent = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(agent.path().join("sessions-archive")).unwrap();
        std::fs::write(
            agent.path().join("sessions-archive/s1.jsonl"),
            "{\"type\":\"session\",\"id\":\"s1\",\"cwd\":\"/work/repo\"}\n{\"type\":\"x\"}\n",
        )
        .unwrap();
        assert_eq!(
            session_cwd(agent.path(), "s1"),
            Some(PathBuf::from("/work/repo"))
        );
        assert_eq!(session_cwd(agent.path(), "missing"), None);
    }

    #[test]
    fn membership_matches_identity_or_slug() {
        let project = Project {
            identity: Some("git:abc".to_string()),
            slug: "-work-repo".to_string(),
        };
        assert!(project.contains(Some("git:abc"), None));
        assert!(project.contains(None, Some("-work-repo")));
        assert!(!project.contains(Some("git:def"), Some("-other")));
        assert_eq!(project.summary_key(), "git:abc");
    }
}
