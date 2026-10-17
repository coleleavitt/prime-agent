//! Git worktrees the harness creates and removes on its own: one isolated
//! checkout per `rlm.spawn(..., isolation="worktree")` child (and the seam
//! a session-level `--worktree` shares), Claude Code's layout under this
//! product's config directory.
//!
//! A worktree lives at `<repo>/.prime/worktrees/<slug>` on a new branch
//! `worktree-<slug>` cut from the repository's `HEAD`. Removal is
//! conservative: [`remove_worktree_if_unchanged`] removes the checkout and
//! its branch only when the worktree has no uncommitted changes and its
//! branch still points at the commit it was cut from, so an agent's work is
//! never discarded; a changed worktree stays for the caller to report.
//!
//! Every git call scrubs the repository-selection environment
//! ([`crate::git_env::scrub_repository_selection`]): the repository is the
//! one the named directory belongs to, whatever `GIT_DIR` the process
//! inherited.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

/// The directory under a repository root that holds harness worktrees.
pub const WORKTREES_DIR: &str = ".prime/worktrees";

/// The branch prefix of a harness worktree (Claude Code's `worktree-<slug>`).
pub const WORKTREE_BRANCH_PREFIX: &str = "worktree-";

/// One harness-created worktree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWorktree {
    /// The worktree's checkout directory.
    pub path: PathBuf,
    /// The branch the worktree has checked out.
    pub branch: String,
    /// The main repository's top-level directory.
    pub repo_root: PathBuf,
    /// The commit the branch was cut from.
    pub base_commit: String,
}

/// Whether `slug` is a safe single path component and branch suffix:
/// ASCII letters, digits, `.`, `_`, `-`, at most 64 characters, not a
/// dot-only name.
#[must_use]
pub fn is_valid_worktree_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && !slug.chars().all(|c| c == '.')
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    crate::git_env::scrub_repository_selection(&mut command);
    command
}

/// Run git in `cwd`, answering its trimmed stdout or the failure with git's
/// stderr.
fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = git(cwd)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The top-level directory of the repository `cwd` belongs to.
///
/// # Errors
///
/// `cwd` is not inside a git work tree, or git cannot run.
pub fn repository_root(cwd: &Path) -> Result<PathBuf> {
    let top = run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|error| anyhow!("{} is not inside a git repository ({error})", cwd.display()))?;
    Ok(PathBuf::from(top))
}

/// Refuse a symlinked `.prime` or `.prime/worktrees`: a repository-committed
/// symlink there could redirect worktree creation outside the repository.
fn assert_no_symlinked_parents(repo_root: &Path) -> Result<()> {
    let mut path = repo_root.to_path_buf();
    for component in Path::new(WORKTREES_DIR).components() {
        path.push(component);
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            bail!(
                "cannot create a worktree: {} is a symlink; remove it and retry",
                path.display()
            );
        }
    }
    Ok(())
}

/// Create a worktree for `slug` in the repository `cwd` belongs to:
/// `<repo>/.prime/worktrees/<slug>` on a new branch `worktree-<slug>` from
/// `HEAD`.
///
/// # Errors
///
/// An invalid slug, `cwd` outside a repository, a repository without a
/// commit, an existing worktree path or branch, or git's own failure.
pub fn create_worktree(cwd: &Path, slug: &str) -> Result<GitWorktree> {
    if !is_valid_worktree_slug(slug) {
        bail!("invalid worktree name {slug:?}: use letters, digits, '.', '_' or '-'");
    }
    let repo_root = repository_root(cwd)?;
    let base_commit = run_git(&repo_root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|error| anyhow!("the repository has no commit to branch from ({error})"))?;
    assert_no_symlinked_parents(&repo_root)?;
    let path = repo_root.join(WORKTREES_DIR).join(slug);
    if path.exists() {
        bail!("the worktree path {} already exists", path.display());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("the worktree path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let branch = format!("{WORKTREE_BRANCH_PREFIX}{slug}");
    let path_arg = path.to_string_lossy().to_string();
    run_git(
        &repo_root,
        &["worktree", "add", "-b", &branch, &path_arg, &base_commit],
    )?;
    Ok(GitWorktree {
        path,
        branch,
        repo_root,
        base_commit,
    })
}

/// The directory inside `worktree` matching `cwd`'s place in the main
/// repository: a session started in `<repo>/crates/x` works in
/// `<worktree>/crates/x` when that directory exists there, else the
/// worktree root.
#[must_use]
pub fn worktree_cwd(worktree: &GitWorktree, cwd: &Path) -> PathBuf {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = canonical(cwd)
        .strip_prefix(canonical(&worktree.repo_root))
        .map(Path::to_path_buf)
        .ok();
    match relative {
        Some(relative) if !relative.as_os_str().is_empty() => {
            let candidate = worktree.path.join(relative);
            if candidate.is_dir() {
                candidate
            } else {
                worktree.path.clone()
            }
        }
        _ => worktree.path.clone(),
    }
}

/// Whether the worktree holds work: uncommitted or untracked changes, or
/// commits on its branch past the base.
///
/// # Errors
///
/// git cannot read the worktree's status or head.
pub fn worktree_has_changes(worktree: &GitWorktree) -> Result<bool> {
    let status = run_git(&worktree.path, &["status", "--porcelain"])?;
    if !status.is_empty() {
        return Ok(true);
    }
    let head = run_git(&worktree.path, &["rev-parse", "HEAD"])?;
    Ok(head != worktree.base_commit)
}

/// Remove the worktree and its branch unless it holds work. Answers whether
/// it was removed (`false`: it has changes and stays). A worktree whose
/// directory is already gone is pruned and its branch deleted.
///
/// # Errors
///
/// git fails to read or remove the worktree.
pub fn remove_worktree_if_unchanged(worktree: &GitWorktree) -> Result<bool> {
    if worktree.path.exists() && worktree_has_changes(worktree)? {
        return Ok(false);
    }
    if worktree.path.exists() {
        let path_arg = worktree.path.to_string_lossy().to_string();
        run_git(
            &worktree.repo_root,
            &["worktree", "remove", "--force", &path_arg],
        )?;
    } else {
        run_git(&worktree.repo_root, &["worktree", "prune"])?;
    }
    // The branch carries no commit past the base (checked above), so
    // deleting it loses nothing.
    run_git(&worktree.repo_root, &["branch", "-D", &worktree.branch])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::git_env::run_fixture_git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), "one\n").unwrap();
        crate::git_env::run_fixture_git(dir.path(), &["add", "."]);
        crate::git_env::run_fixture_git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn slugs_are_single_safe_components() {
        assert_eq!(
            [
                "agent-1a2b",
                "a.b_c",
                "",
                "..",
                "a/b",
                "a b",
                &"x".repeat(65)
            ]
            .map(is_valid_worktree_slug),
            [true, true, false, false, false, false, false]
        );
    }

    #[test]
    fn an_unchanged_worktree_is_created_and_removed_with_its_branch() {
        let repo = fixture_repo();
        let worktree = create_worktree(&repo.path().join("sub"), "agent-x1").unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(
            (
                std::fs::canonicalize(&worktree.repo_root).unwrap(),
                worktree.branch.as_str(),
                worktree.path.join("sub/file.txt").is_file(),
            ),
            (root, "worktree-agent-x1", true)
        );
        assert_eq!(
            std::fs::canonicalize(worktree_cwd(&worktree, &repo.path().join("sub"))).unwrap(),
            std::fs::canonicalize(worktree.path.join("sub")).unwrap()
        );
        assert!(remove_worktree_if_unchanged(&worktree).unwrap());
        assert!(!worktree.path.exists());
        let branches =
            crate::git_env::run_fixture_git(repo.path(), &["branch", "--list", "worktree-*"]);
        assert_eq!(branches.trim(), "");
    }

    #[test]
    fn a_worktree_with_changes_or_commits_stays() {
        let repo = fixture_repo();
        let dirty = create_worktree(repo.path(), "dirty").unwrap();
        std::fs::write(dirty.path.join("new.txt"), "work\n").unwrap();
        assert!(!remove_worktree_if_unchanged(&dirty).unwrap());
        assert!(dirty.path.join("new.txt").is_file());

        let committed = create_worktree(repo.path(), "committed").unwrap();
        std::fs::write(committed.path.join("c.txt"), "work\n").unwrap();
        crate::git_env::run_fixture_git(&committed.path, &["add", "."]);
        crate::git_env::run_fixture_git(&committed.path, &["commit", "-q", "-m", "work"]);
        assert!(worktree_has_changes(&committed).unwrap());
        assert!(!remove_worktree_if_unchanged(&committed).unwrap());
    }

    #[test]
    fn creation_refuses_outside_a_repository_and_a_taken_name() {
        let plain = tempfile::tempdir().unwrap();
        assert!(create_worktree(plain.path(), "a").is_err());
        let repo = fixture_repo();
        create_worktree(repo.path(), "taken").unwrap();
        let error = create_worktree(repo.path(), "taken").unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
    }
}
