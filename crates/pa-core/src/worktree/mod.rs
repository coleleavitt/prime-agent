//! Managed git worktrees (Claude Code's `--worktree` / `EnterWorktree`): the
//! one helper every isolation path shares, `prime-agent --worktree`, the
//! mid-session `rlm.enter_worktree`, and a spawned child's isolation.
//!
//! A managed worktree lives at `<repo>/.prime/worktrees/<name>` (Claude Code
//! uses `.claude/worktrees/`; the fork keeps its state under `.prime/`) on the
//! branch `worktree-<name>` (`/` in a name becomes `+`). The main repository is
//! resolved through git's common dir, so a request made from inside another
//! worktree still creates beside the main checkout. Creating:
//!
//! - an existing registered worktree at that path is reused (`existed`);
//! - a new one branches from [`BaseRef::Fresh`] (the remote's default branch
//!   as last fetched, `origin/HEAD`, else `HEAD`; nothing is fetched) or
//!   [`BaseRef::Head`] (the requesting directory's `HEAD`);
//! - an existing `worktree-<name>` branch is checked out as it is, never
//!   reset;
//! - the base commit is recorded in the worktree's git dir
//!   (`PRIME_AGENT_BASE`) so a later [`changes`] counts the commits made on
//!   top of it;
//! - `.prime/worktrees/` is added to the repository's `info/exclude` (local,
//!   never committed) so the main checkout's status stays clean;
//! - `.worktreeinclude` files are copied ([`include`]).
//!
//! Removing is never destructive by default: [`remove`] keeps a worktree with
//! uncommitted files or new commits unless the caller passes `discard`, and
//! deletes the branch only when its tip is reachable from another ref (or on
//! `discard`).
//!
//! A `WorktreeCreate`/`WorktreeRemove` hook (installed through
//! [`install_hooks`]; `pa-hooks` provides one) replaces git entirely, as in
//! Claude Code: the hook creates the directory and prints its path.

mod git;
pub mod include;
mod name;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub use include::{WORKTREE_INCLUDE_FILE, copy_included_files};
pub use name::{BRANCH_PREFIX, MAX_NAME_LEN, branch_for, flatten, random_name, validate_name};
use serde::{Deserialize, Serialize};

/// The managed worktrees' directory, relative to the repository root.
pub const WORKTREES_DIR: &str = ".prime/worktrees";
/// The file in a worktree's git dir that records the commit it was created
/// from (Claude Code writes `CLAUDE_BASE`).
pub const BASE_MARKER_FILE: &str = "PRIME_AGENT_BASE";
/// The `info/exclude` line that keeps managed worktrees out of the main
/// checkout's status.
const EXCLUDE_LINE: &str = "/.prime/worktrees/";

/// Why a worktree operation failed.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("Invalid worktree name: {0}")]
    InvalidName(String),
    #[error(
        "Cannot create a worktree: {0} is not in a git repository and no WorktreeCreate hook is \
         configured"
    )]
    NotARepository(PathBuf),
    #[error(
        "Cannot create a worktree: {0} is a symlink, which could redirect the worktree outside \
         the repository. Remove the symlink and retry."
    )]
    Symlink(PathBuf),
    #[error(
        "Cannot use worktree \"{name}\": {path} exists but is not a registered worktree of this \
         repository. Remove it or pick another name."
    )]
    NotAWorktree { name: String, path: PathBuf },
    #[error(
        "Branch \"{branch}\" for worktree \"{name}\" is already checked out elsewhere in this \
         repository. Pick another worktree name."
    )]
    BranchCheckedOut { name: String, branch: String },
    #[error("{command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("WorktreeCreate hook: {0}")]
    Hook(String),
    #[error("Could not {what}: {error}")]
    Io {
        what: String,
        #[source]
        error: std::io::Error,
    },
}

/// Which commit a new worktree branches from (Claude Code's `worktree.baseRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BaseRef {
    /// The remote's default branch as last fetched (`origin/HEAD`), else `HEAD`.
    #[default]
    Fresh,
    /// The requesting directory's `HEAD` (unpushed commits included).
    Head,
}

impl BaseRef {
    /// The `worktree.baseRef` setting value (`fresh`/`head`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "fresh" => Some(Self::Fresh),
            "head" => Some(Self::Head),
            _ => None,
        }
    }
}

/// The session a hook-run worktree operation belongs to (the hook's input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHookContext {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_id: Option<String>,
}

/// A `WorktreeCreate`/`WorktreeRemove` hook provider. `None` means no such
/// hook is configured for the context (git runs instead).
pub trait WorktreeHooks: Send + Sync {
    /// Create the worktree `name`; the directory the hook created.
    fn create(&self, context: &WorktreeHookContext, name: &str) -> Option<Result<PathBuf, String>>;

    /// Remove a worktree a create hook made.
    fn remove(&self, context: &WorktreeHookContext, path: &Path) -> Option<Result<(), String>>;
}

static HOOKS: RwLock<Option<Arc<dyn WorktreeHooks>>> = RwLock::new(None);

/// Install the process's worktree hook provider (the composition root does,
/// once, when the hooks feature is built).
pub fn install_hooks(hooks: Arc<dyn WorktreeHooks>) {
    *HOOKS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hooks);
}

fn hooks() -> Option<Arc<dyn WorktreeHooks>> {
    HOOKS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// What to create.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// The worktree's name; `None` generates one ([`random_name`]).
    pub name: Option<String>,
    /// The directory the request comes from (its repository is used).
    pub from: PathBuf,
    pub base: BaseRef,
    /// The session context a `WorktreeCreate` hook receives; `None` never
    /// consults the hooks.
    pub hooks: Option<WorktreeHookContext>,
}

/// A managed worktree: what creating it produced, and what exiting it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    /// `None` for a hook-made worktree.
    pub branch: Option<String>,
    /// The main repository root (`None` for a hook-made worktree outside git).
    pub repo_root: Option<PathBuf>,
    /// The commit the worktree started from (`None` when unknown).
    pub base_commit: Option<String>,
    /// The worktree already existed and was reused.
    pub existed: bool,
    /// A `WorktreeCreate` hook made it.
    pub hook_based: bool,
    /// `.worktreeinclude` files copied in (new git worktrees only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copied: Vec<String>,
}

/// The main repository root of the checkout `cwd` is in (through git's common
/// dir, so a linked worktree resolves to the main checkout).
///
/// # Errors
///
/// [`WorktreeError::NotARepository`] outside git.
pub fn main_repo_root(cwd: &Path) -> Result<PathBuf, WorktreeError> {
    let not_a_repo = || WorktreeError::NotARepository(cwd.to_path_buf());
    let common = git::stdout(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|_| not_a_repo())?;
    let common = PathBuf::from(common);
    if common.file_name().is_some_and(|name| name == ".git") {
        if let Some(root) = common.parent() {
            return Ok(root.to_path_buf());
        }
    }
    git::stdout(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|_| not_a_repo())
}

/// The managed worktrees' directory of a repository.
#[must_use]
pub fn worktrees_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(WORKTREES_DIR)
}

/// Whether `path` is a managed worktree directory of `repo_root`
/// (directly under its `.prime/worktrees/`).
#[must_use]
pub fn is_managed_path(repo_root: &Path, path: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(path).parent() == Some(canonical(&worktrees_dir(repo_root)).as_path())
}

/// Create (or reuse) a managed worktree. Blocking: runs git.
///
/// # Errors
///
/// A bad name, no repository (and no hook), a symlinked managed directory,
/// a path that is not a worktree, a branch checked out elsewhere, or git's
/// own failure.
pub fn create(request: &CreateRequest) -> Result<Worktree, WorktreeError> {
    let name = request.name.clone().unwrap_or_else(random_name);
    validate_name(&name)?;
    if let Some(context) = &request.hooks {
        if let Some(created) = hooks().and_then(|hooks| hooks.create(context, &name)) {
            let path = created.map_err(WorktreeError::Hook)?;
            if !path.is_absolute() || !path.is_dir() {
                return Err(WorktreeError::Hook(format!(
                    "the hook's path {} is not an existing absolute directory (the hook must \
                     print the directory it created as the last line of its stdout)",
                    path.display()
                )));
            }
            let base_commit = git::resolve_commit(&path, "HEAD");
            return Ok(Worktree {
                name,
                repo_root: main_repo_root(&path).ok(),
                path,
                branch: None,
                base_commit,
                existed: false,
                hook_based: true,
                copied: Vec::new(),
            });
        }
    }
    let repo_root = main_repo_root(&request.from)?;
    let dir = worktrees_dir(&repo_root);
    let path = dir.join(flatten(&name));
    let branch = branch_for(&name);
    for component in [repo_root.join(".prime"), dir.clone(), path.clone()] {
        if std::fs::symlink_metadata(&component).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(WorktreeError::Symlink(component));
        }
    }
    if path.exists() {
        return reuse(&repo_root, name, path);
    }
    let base_commit = match request.base {
        BaseRef::Head => git::resolve_commit(&request.from, "HEAD"),
        BaseRef::Fresh => fresh_base(&repo_root),
    };
    std::fs::create_dir_all(&dir).map_err(|error| WorktreeError::Io {
        what: format!("create {}", dir.display()),
        error,
    })?;
    exclude_managed_dir(&repo_root);
    let path_text = path.to_string_lossy().to_string();
    let reused_branch = git::branch_exists(&repo_root, &branch);
    let added = if reused_branch {
        // An existing branch keeps its commits: check it out, never `-B`.
        git::run(&repo_root, &["worktree", "add", &path_text, &branch])?
    } else {
        let base = base_commit.clone().unwrap_or_else(|| "HEAD".to_string());
        git::run(
            &repo_root,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                &branch,
                &path_text,
                &base,
            ],
        )?
    };
    if !added.success {
        let stderr = added.stderr.trim().to_string();
        if stderr.contains("is already checked out at")
            || stderr.contains("is already used by worktree at")
        {
            return Err(WorktreeError::BranchCheckedOut { name, branch });
        }
        return Err(WorktreeError::Git {
            command: "git worktree add".to_string(),
            stderr,
        });
    }
    // A reused branch starts from its own tip: commits counted later are the
    // ones made from now on.
    let base_commit = if reused_branch {
        git::resolve_commit(&path, "HEAD")
    } else {
        base_commit.or_else(|| git::resolve_commit(&path, "HEAD"))
    };
    if let Some(base) = &base_commit {
        write_base_marker(&path, base);
    }
    let copied = copy_included_files(&repo_root, &path);
    Ok(Worktree {
        name,
        path,
        branch: Some(branch),
        repo_root: Some(repo_root),
        base_commit,
        existed: false,
        hook_based: false,
        copied,
    })
}

/// An existing directory at the managed path: reused when git lists it as a
/// worktree of this repository.
fn reuse(repo_root: &Path, name: String, path: PathBuf) -> Result<Worktree, WorktreeError> {
    if !is_registered(repo_root, &path) {
        return Err(WorktreeError::NotAWorktree { name, path });
    }
    let branch = git::run(&path, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout.trim().to_string())
        .filter(|branch| !branch.is_empty());
    let base_commit = read_base_marker(&path).or_else(|| git::resolve_commit(&path, "HEAD"));
    Ok(Worktree {
        name,
        path,
        branch,
        repo_root: Some(repo_root.to_path_buf()),
        base_commit,
        existed: true,
        hook_based: false,
        copied: Vec::new(),
    })
}

/// Whether git lists `path` as one of `repo_root`'s worktrees.
#[must_use]
pub fn is_registered(repo_root: &Path, path: &Path) -> bool {
    let Ok(listing) = git::stdout(repo_root, &["worktree", "list", "--porcelain", "-z"]) else {
        return false;
    };
    let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    listing
        .split('\0')
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| {
            let listed = Path::new(listed);
            std::fs::canonicalize(listed).unwrap_or_else(|_| listed.to_path_buf()) == wanted
        })
}

/// `origin/HEAD`'s commit when the remote's default branch was fetched, else `HEAD`.
fn fresh_base(repo_root: &Path) -> Option<String> {
    git::stdout(
        repo_root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|reference| git::resolve_commit(repo_root, &reference))
    .or_else(|| git::resolve_commit(repo_root, "HEAD"))
}

/// Add [`EXCLUDE_LINE`] to the repository's `info/exclude` once (best effort).
fn exclude_managed_dir(repo_root: &Path) {
    let Ok(common) = git::stdout(
        repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        return;
    };
    let exclude = Path::new(&common).join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|line| line.trim() == EXCLUDE_LINE) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::write(&exclude, format!("{current}{separator}{EXCLUDE_LINE}\n"));
}

/// The worktree's own git dir (`.git/worktrees/<name>`).
fn worktree_git_dir(path: &Path) -> Option<PathBuf> {
    git::stdout(path, &["rev-parse", "--absolute-git-dir"])
        .ok()
        .map(PathBuf::from)
}

fn write_base_marker(path: &Path, base: &str) {
    if let Some(dir) = worktree_git_dir(path) {
        let _ = std::fs::write(dir.join(BASE_MARKER_FILE), format!("{base}\n"));
    }
}

/// The base commit recorded when the worktree was created.
#[must_use]
pub fn read_base_marker(path: &Path) -> Option<String> {
    let dir = worktree_git_dir(path)?;
    let text = std::fs::read_to_string(dir.join(BASE_MARKER_FILE)).ok()?;
    let base = text.trim();
    (base.len() >= 7 && base.chars().all(|c| c.is_ascii_hexdigit())).then(|| base.to_string())
}

/// What a worktree holds beyond its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeChanges {
    /// Uncommitted entries (`git status --porcelain`, untracked included).
    pub changed_files: usize,
    /// Commits on `HEAD` that the base does not have.
    pub commits_ahead: usize,
}

impl WorktreeChanges {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.changed_files == 0 && self.commits_ahead == 0
    }

    /// `2 uncommitted files and 1 commit`, for prompts and refusals.
    #[must_use]
    pub fn describe(&self, branch: Option<&str>) -> String {
        let mut parts = Vec::new();
        if self.changed_files > 0 {
            let noun = if self.changed_files == 1 {
                "file"
            } else {
                "files"
            };
            parts.push(format!("{} uncommitted {noun}", self.changed_files));
        }
        if self.commits_ahead > 0 {
            let noun = if self.commits_ahead == 1 {
                "commit"
            } else {
                "commits"
            };
            parts.push(format!(
                "{} {noun} on {}",
                self.commits_ahead,
                branch.unwrap_or("the worktree branch")
            ));
        }
        if parts.is_empty() {
            "no changes".to_string()
        } else {
            parts.join(" and ")
        }
    }
}

/// Count a worktree's uncommitted files and the commits since `base`.
///
/// # Errors
///
/// git's failure (a missing or broken worktree): the caller must treat the
/// state as unknown, never as clean.
pub fn changes(path: &Path, base: Option<&str>) -> Result<WorktreeChanges, WorktreeError> {
    // Untrimmed: an entry's first column may be a space.
    let status = git::run(path, &["status", "--porcelain", "-z"])?;
    if !status.success {
        return Err(WorktreeError::Git {
            command: "git status".to_string(),
            stderr: status.stderr.trim().to_string(),
        });
    }
    let status = status.stdout;
    let mut changed_files = 0;
    let mut entries = status.split('\0').filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        changed_files += 1;
        // A rename or copy carries its source path as the next entry.
        if entry.starts_with(['R', 'C']) || entry.get(1..2).is_some_and(|y| matches!(y, "R" | "C"))
        {
            entries.next();
        }
    }
    let commits_ahead = match base {
        Some(base) => {
            let range = format!("{base}..HEAD");
            git::stdout(path, &["rev-list", "--count", &range])?
                .parse()
                .unwrap_or(0)
        }
        None => 0,
    };
    Ok(WorktreeChanges {
        changed_files,
        commits_ahead,
    })
}

/// How a removal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The worktree is gone; `branch_deleted` says whether its branch went too
    /// (it stays when its commits are reachable from no other ref).
    Removed { branch_deleted: bool },
    /// Nothing was removed: the worktree has work (`discard` not given).
    Kept(WorktreeChanges),
}

/// Remove a managed worktree. Without `discard`, a worktree with
/// uncommitted files or new commits is kept ([`RemoveOutcome::Kept`]).
/// Blocking: runs git (or the `WorktreeRemove` hook for a hook-made one).
///
/// # Errors
///
/// An unreadable worktree state without `discard`, git's or the hook's
/// failure, or a path outside the managed directory.
pub fn remove(
    worktree: &Worktree,
    discard: bool,
    hook_context: Option<&WorktreeHookContext>,
) -> Result<RemoveOutcome, WorktreeError> {
    if worktree.hook_based {
        let removed = hook_context
            .and_then(|context| hooks().and_then(|hooks| hooks.remove(context, &worktree.path)));
        return match removed {
            Some(Ok(())) => Ok(RemoveOutcome::Removed {
                branch_deleted: false,
            }),
            Some(Err(error)) => Err(WorktreeError::Hook(error)),
            None => Err(WorktreeError::Hook(
                "no WorktreeRemove hook is configured; the worktree was kept".to_string(),
            )),
        };
    }
    let Some(repo_root) = worktree.repo_root.as_deref() else {
        return Err(WorktreeError::NotARepository(worktree.path.clone()));
    };
    if !is_managed_path(repo_root, &worktree.path) {
        return Err(WorktreeError::NotAWorktree {
            name: worktree.name.clone(),
            path: worktree.path.clone(),
        });
    }
    if worktree.path.exists() {
        match changes(&worktree.path, worktree.base_commit.as_deref()) {
            Ok(found) if !found.is_clean() && !discard => return Ok(RemoveOutcome::Kept(found)),
            Ok(_) => {}
            Err(error) if !discard => return Err(error),
            Err(_) => {}
        }
    }
    let tip = worktree
        .branch
        .as_deref()
        .and_then(|branch| git::resolve_commit(repo_root, &format!("refs/heads/{branch}")));
    let path_text = worktree.path.to_string_lossy().to_string();
    if worktree.path.exists() {
        let mut args = vec!["worktree", "remove"];
        if discard {
            args.extend(["--force", "--force"]);
        }
        args.push(&path_text);
        git::run(repo_root, &args)?.ok("git worktree remove")?;
    } else {
        let _ = git::run(repo_root, &["worktree", "prune"]);
    }
    let branch_deleted = match (worktree.branch.as_deref(), tip) {
        (Some(branch), Some(tip)) if branch.starts_with(BRANCH_PREFIX) => {
            (discard || reachable_elsewhere(repo_root, branch, &tip))
                && git::run(repo_root, &["branch", "-D", "--end-of-options", branch])
                    .is_ok_and(|output| output.success)
        }
        _ => false,
    };
    Ok(RemoveOutcome::Removed { branch_deleted })
}

/// Whether a ref other than `branch` contains `tip` (deleting the branch then
/// loses no commit).
fn reachable_elsewhere(repo_root: &Path, branch: &str, tip: &str) -> bool {
    let own = format!("refs/heads/{branch}");
    git::stdout(
        repo_root,
        &["for-each-ref", "--contains", tip, "--format=%(refname)"],
    )
    .is_ok_and(|refs| {
        refs.lines()
            .any(|reference| !reference.is_empty() && reference != own && reference != "refs/stash")
    })
}

#[cfg(test)]
mod tests;
