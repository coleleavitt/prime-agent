//! The launch directory's git branch for the branch filter, read from the repository's `HEAD`
//! file (no `git` spawn on the view's path): the nearest `.git` above the directory, a linked
//! worktree's `.git` file followed to its own `HEAD`.

use std::path::{Path, PathBuf};

/// The repository a directory belongs to and the branch its `HEAD` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitHead {
    /// The working tree's top directory (the directory holding `.git`).
    pub(super) root: PathBuf,
    /// `None` on a detached `HEAD`.
    pub(super) branch: Option<String>,
}

/// The git head of `dir`, or `None` outside a repository.
pub(super) fn read_git_head(dir: &Path) -> Option<GitHead> {
    let start = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let root = start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())?;
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        let target = PathBuf::from(target);
        if target.is_absolute() {
            target
        } else {
            root.join(target)
        }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(GitHead {
        root: root.to_path_buf(),
        branch: head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_string)
            .filter(|branch| !branch.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_branch_from_the_nearest_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(
            read_git_head(&repo.join("src/deep")),
            Some(GitHead {
                root: repo.clone(),
                branch: Some("feature/x".to_string()),
            })
        );
        std::fs::write(repo.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(read_git_head(&repo).and_then(|head| head.branch), None);
    }

    #[test]
    fn follows_a_linked_worktree_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let admin = dir.path().join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        assert_eq!(
            read_git_head(&worktree).and_then(|head| head.branch),
            Some("topic".to_string())
        );
    }
}
