//! The plan-mode plan file (Claude Code's `plansDirectory`): where a
//! session's plan is written so the exit-plan approval dialog can show it
//! and `/plan view` can read it.
//!
//! Claude Code resolves the directory from the `plansDirectory` setting
//! (project-relative, and refused when it escapes the project root), and
//! otherwise uses a default under the user's state (CC's `~/.claude/...`).
//! Prime Agent's default is `<agent dir>/plans`. The file is named for the
//! session (`<dir>/<session>.md`), matching CC's `<plansDir>/<slug>.md`.

use std::path::{Path, PathBuf};

use pa_types::platform::same_directory;

/// Resolve the plan-file directory: the `plansDirectory` setting resolved
/// against `project_root` when it stays inside it, else `<agent_dir>/plans`.
/// A setting that escapes the project root (or holds a backslash off
/// Windows, as Claude Code refuses) falls back to the default.
#[must_use]
pub fn resolve_directory(setting: Option<&str>, project_root: &Path, agent_dir: &Path) -> PathBuf {
    let default = || agent_dir.join("plans");
    let Some(setting) = setting.map(str::trim).filter(|setting| !setting.is_empty()) else {
        return default();
    };
    // Claude Code refuses a backslash in the path off Windows (`$e`).
    if !cfg!(windows) && setting.contains('\\') {
        return default();
    }
    let resolved = project_root.join(setting);
    if within(&resolved, project_root) {
        resolved
    } else {
        default()
    }
}

/// Whether `candidate` is `root` or lives under it (Claude Code's `$e`
/// within-root check, without the git-root escape logic).
fn within(candidate: &Path, root: &Path) -> bool {
    if same_directory(candidate, root) {
        return true;
    }
    // A lexical check: `..` segments that climb above the root escape it.
    let Ok(relative) = candidate.strip_prefix(root) else {
        return false;
    };
    let mut depth: i64 = 0;
    for component in relative.components() {
        match component {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            std::path::Component::CurDir => {}
            _ => depth += 1,
        }
    }
    true
}

/// A session id reduced to a safe file stem: every character outside
/// `[A-Za-z0-9._-]` becomes `-` (a session id is already filesystem-safe,
/// but a renamed or imported one may not be).
#[must_use]
pub fn session_slug(session_id: &str) -> String {
    let slug: String = session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "session".to_string()
    } else {
        slug
    }
}

/// One session's plan file.
#[derive(Debug, Clone)]
pub struct PlanFile {
    directory: PathBuf,
    session_id: String,
}

impl PlanFile {
    /// The plan file for `session_id`, its directory resolved from the
    /// `plansDirectory` `setting`, `project_root` and `agent_dir`.
    #[must_use]
    pub fn resolve(
        setting: Option<&str>,
        project_root: &Path,
        agent_dir: &Path,
        session_id: &str,
    ) -> Self {
        PlanFile {
            directory: resolve_directory(setting, project_root, agent_dir),
            session_id: session_id.to_string(),
        }
    }

    /// The plan file's path (`<directory>/<session>.md`).
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.directory
            .join(format!("{}.md", session_slug(&self.session_id)))
    }

    /// The plan's current text, or `None` when no plan has been written.
    #[must_use]
    pub fn read(&self) -> Option<String> {
        std::fs::read_to_string(self.path()).ok()
    }

    /// Write `plan` to the file, creating the directory. Returns the path.
    ///
    /// # Errors
    ///
    /// The directory could not be created or the file could not be written.
    pub fn write(&self, plan: &str) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.directory)?;
        let path = self.path();
        std::fs::write(&path, plan)?;
        Ok(path)
    }

    /// Remove the plan file; absent is success.
    ///
    /// # Errors
    ///
    /// The file existed but could not be removed.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_directory_is_under_the_agent_dir() {
        let root = Path::new("/project");
        let agent = Path::new("/home/.prime/agent");
        assert_eq!(
            resolve_directory(None, root, agent),
            Path::new("/home/.prime/agent/plans")
        );
        assert_eq!(
            resolve_directory(Some("   "), root, agent),
            Path::new("/home/.prime/agent/plans")
        );
    }

    #[test]
    fn a_setting_resolves_inside_the_project_root() {
        let root = Path::new("/project");
        let agent = Path::new("/home/.prime/agent");
        assert_eq!(
            resolve_directory(Some("docs/plans"), root, agent),
            Path::new("/project/docs/plans")
        );
        // A setting escaping the root falls back to the default.
        assert_eq!(
            resolve_directory(Some("../outside"), root, agent),
            Path::new("/home/.prime/agent/plans")
        );
        assert_eq!(
            resolve_directory(Some("a/../../etc"), root, agent),
            Path::new("/home/.prime/agent/plans")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_backslash_setting_is_refused_off_windows() {
        let root = Path::new("/project");
        let agent = Path::new("/home/.prime/agent");
        assert_eq!(
            resolve_directory(Some("a\\b"), root, agent),
            Path::new("/home/.prime/agent/plans")
        );
    }

    #[test]
    fn the_session_slug_is_file_safe() {
        assert_eq!(session_slug("01a1-abc_DEF.9"), "01a1-abc_DEF.9");
        assert_eq!(session_slug("a/b c:d"), "a-b-c-d");
        assert_eq!(session_slug(""), "session");
    }

    #[test]
    fn write_read_and_clear_round_trip() {
        let temp = std::env::temp_dir().join(format!("pa-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let agent = temp.join("agent");
        let root = temp.join("project");
        let plan = PlanFile::resolve(None, &root, &agent, "sess-1");
        assert_eq!(plan.path(), agent.join("plans").join("sess-1.md"));
        assert_eq!(plan.read(), None);
        let path = plan.write("# Plan\n\nDo the thing.").unwrap();
        assert_eq!(path, plan.path());
        assert_eq!(plan.read().as_deref(), Some("# Plan\n\nDo the thing."));
        plan.clear().unwrap();
        assert_eq!(plan.read(), None);
        // Clearing an absent file is success.
        plan.clear().unwrap();
        let _ = std::fs::remove_dir_all(&temp);
    }
}
