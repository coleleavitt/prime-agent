//! The launch directory's git branch for the branch filter, read from the repository's `HEAD`
//! file (no `git` spawn on the view's path): the nearest `.git` above the directory, a linked
//! worktree's `.git` file followed to its own `HEAD`. The repository's worktrees (Claude Code's
//! `git worktree list` grouping of the resume picker) are read the same way, from the common git
//! directory's `worktrees/<name>/gitdir` records.

use std::path::{Path, PathBuf};

/// The repository a directory belongs to and the branch its `HEAD` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitHead {
    /// The working tree's top directory (the directory holding `.git`).
    pub(super) root: PathBuf,
    /// `None` on a detached `HEAD`.
    pub(super) branch: Option<String>,
}

/// The working tree holding `dir` and its git directory (a linked worktree's `.git` file
/// followed to its admin directory), or `None` outside a repository.
fn locate(dir: &Path) -> Option<(PathBuf, PathBuf)> {
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
    Some((root.to_path_buf(), git_dir))
}

/// The git head of `dir`, or `None` outside a repository.
pub(super) fn read_git_head(dir: &Path) -> Option<GitHead> {
    let (root, git_dir) = locate(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(GitHead {
        root,
        branch: head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_string)
            .filter(|branch| !branch.is_empty()),
    })
}

/// Every worktree of the repository holding `dir` (Claude Code's `git worktree list`): the
/// main worktree, then each linked worktree its common git directory records, in name order.
/// Empty outside a repository; a linked worktree whose directory is gone still lists (git lists
/// it as prunable), so its saved sessions keep their group.
pub(super) fn read_worktrees(dir: &Path) -> Vec<PathBuf> {
    let Some((root, git_dir)) = locate(dir) else {
        return Vec::new();
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(pointer) => normalize(&git_dir.join(pointer.trim())),
        Err(_) => git_dir.clone(),
    };
    let mut worktrees = Vec::new();
    // The main worktree holds the common directory as its `.git`; a repository whose git
    // directory lives elsewhere names no main worktree here, except the one `dir` is in.
    match common.file_name().zip(common.parent()) {
        Some((name, parent)) if name == ".git" => worktrees.push(parent.to_path_buf()),
        _ if common == git_dir => worktrees.push(root.clone()),
        _ => {}
    }
    let mut linked: Vec<PathBuf> = std::fs::read_dir(common.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pointer = std::fs::read_to_string(entry.path().join("gitdir")).ok()?;
            let dot_git = PathBuf::from(pointer.trim());
            let dot_git = if dot_git.is_absolute() {
                dot_git
            } else {
                entry.path().join(dot_git)
            };
            normalize(&dot_git).parent().map(Path::to_path_buf)
        })
        .collect();
    linked.sort();
    for worktree in linked {
        if !worktrees.contains(&worktree) {
            worktrees.push(worktree);
        }
    }
    if !worktrees.contains(&root) {
        worktrees.push(root);
    }
    worktrees
}

/// Fold `.` and `..` components lexically (`commondir` is usually `../..`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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

    #[test]
    fn lists_the_main_and_linked_worktrees_from_either_one() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(main.join("src")).unwrap();
        let linked = dir.path().join("wt");
        std::fs::create_dir_all(&linked).unwrap();
        let admin = main.join(".git/worktrees/wt");
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        // A removed linked worktree still lists (git calls it prunable).
        let gone = main.join(".git/worktrees/gone");
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::write(
            gone.join("gitdir"),
            format!("{}\n", dir.path().join("gone/.git").display()),
        )
        .unwrap();
        let expected = vec![main.clone(), dir.path().join("gone"), linked.clone()];
        assert_eq!(read_worktrees(&main.join("src")), expected);
        assert_eq!(read_worktrees(&linked), expected);
        assert_eq!(read_worktrees(dir.path()), Vec::<PathBuf>::new());
    }
}
