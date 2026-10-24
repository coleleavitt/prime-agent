//! The saved catalog's "current project" (Claude Code's resume picker, `Dln` with the
//! `/resume` loader `e$n`): the sessions of the launch directory's repository grouped by git
//! worktree. With several worktrees the project is every worktree of the repository (a session
//! under a worktree, its subdirectories included, belongs to the longest worktree holding it) and
//! the worktree filter (`Ctrl+W`, on by default) narrows it to the worktree holding the launch
//! directory. With one worktree (or none) the project is the launch directory itself, plus the
//! sessions of removed managed worktrees under it (`.prime/worktrees/<name>`, Claude Code's
//! `.claude/worktrees/<name>`), which Claude Code sweeps into the same list.

use std::path::{Path, PathBuf};

/// The managed worktree directories a single-worktree project keeps the sessions of: the
/// fork's (`pa_core::worktree::WORKTREES_DIR`) and Claude Code's.
const MANAGED_WORKTREE_DIRS: [&str; 2] = [".prime/worktrees", ".claude/worktrees"];

/// The launch directory's repository worktrees, read once when the view opens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ProjectWorktrees {
    /// Every worktree of the repository (empty outside one).
    worktrees: Vec<PathBuf>,
    /// The worktree holding the launch directory (the longest match).
    current: Option<PathBuf>,
}

impl ProjectWorktrees {
    pub(super) fn new(worktrees: Vec<PathBuf>, cwd: &Path) -> Self {
        let current = longest_holding(&worktrees, cwd).map(Path::to_path_buf);
        Self { worktrees, current }
    }

    /// Whether the repository has more than one worktree: only then does the worktree filter
    /// exist (Claude Code shows `Ctrl+W` only then).
    pub(super) fn several(&self) -> bool {
        self.worktrees.len() > 1
    }

    /// Whether a saved session started in `session_cwd` belongs to the current project of a
    /// view launched in `cwd`; `all_worktrees` widens it to every worktree of the repository.
    pub(super) fn holds(&self, session_cwd: &Path, cwd: &Path, all_worktrees: bool) -> bool {
        let same = |left: &Path, right: &Path| pa_types::platform::same_directory(left, right);
        if self.several() {
            let owner = longest_holding(&self.worktrees, session_cwd);
            if all_worktrees {
                return owner.is_some() || same(session_cwd, cwd);
            }
            let current = self.current.as_deref().unwrap_or(cwd);
            return match owner {
                Some(owner) => owner == current,
                None => same(session_cwd, current),
            };
        }
        if same(session_cwd, cwd) {
            return true;
        }
        let base = self.current.as_deref().unwrap_or(cwd);
        MANAGED_WORKTREE_DIRS.iter().any(|dir| {
            let managed = base.join(dir);
            session_cwd.starts_with(&managed) && session_cwd != managed
        })
    }
}

/// The longest of `worktrees` that is `path` or one of its ancestors.
fn longest_holding<'a>(worktrees: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    worktrees
        .iter()
        .filter(|worktree| path.starts_with(worktree))
        .max_by_key(|worktree| worktree.components().count())
        .map(PathBuf::as_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn several() -> ProjectWorktrees {
        ProjectWorktrees::new(
            vec![
                PathBuf::from("/repo"),
                PathBuf::from("/repo/.prime/worktrees/topic"),
                PathBuf::from("/elsewhere/wt"),
            ],
            Path::new("/repo/src"),
        )
    }

    #[test]
    fn several_worktrees_default_to_the_one_holding_the_launch_directory() {
        let project = several();
        let cwd = Path::new("/repo/src");
        assert!(project.several());
        // The worktree root and its subdirectories belong; a nested worktree is its own group.
        assert!(project.holds(Path::new("/repo"), cwd, false));
        assert!(project.holds(Path::new("/repo/docs"), cwd, false));
        assert!(!project.holds(Path::new("/repo/.prime/worktrees/topic"), cwd, false));
        assert!(!project.holds(Path::new("/elsewhere/wt/sub"), cwd, false));
        assert!(!project.holds(Path::new("/other"), cwd, false));
    }

    #[test]
    fn all_worktrees_widen_to_every_worktree_of_the_repository() {
        let project = several();
        let cwd = Path::new("/repo/src");
        assert!(project.holds(Path::new("/repo/.prime/worktrees/topic/x"), cwd, true));
        assert!(project.holds(Path::new("/elsewhere/wt"), cwd, true));
        assert!(!project.holds(Path::new("/repository"), cwd, true));
        assert!(!project.holds(Path::new("/other"), cwd, true));
    }

    #[test]
    fn one_worktree_is_the_launch_directory_and_its_removed_managed_worktrees() {
        let project = ProjectWorktrees::new(vec![PathBuf::from("/repo")], Path::new("/repo"));
        let cwd = Path::new("/repo");
        assert!(!project.several());
        assert!(project.holds(Path::new("/repo"), cwd, false));
        // Claude Code loads only the launch directory's own sessions here: subdirectories stay
        // out.
        assert!(!project.holds(Path::new("/repo/src"), cwd, false));
        assert!(project.holds(Path::new("/repo/.prime/worktrees/gone"), cwd, false));
        assert!(project.holds(Path::new("/repo/.claude/worktrees/gone/sub"), cwd, false));
        assert!(!project.holds(Path::new("/repo/.prime/worktrees"), cwd, false));
    }

    #[test]
    fn outside_a_repository_the_project_is_the_launch_directory() {
        let project = ProjectWorktrees::new(Vec::new(), Path::new("/tmp/x"));
        assert!(project.holds(Path::new("/tmp/x"), Path::new("/tmp/x"), false));
        assert!(!project.holds(Path::new("/tmp/x/y"), Path::new("/tmp/x"), false));
        assert!(!project.holds(Path::new("/tmp"), Path::new("/tmp/x"), false));
    }
}
