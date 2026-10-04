//! The git surface recall needs: repo discovery by a filesystem walk and a
//! bounded `git` runner. Every git failure degrades recall to "no mark, no
//! block"; nothing here retries.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Per-call git timeout.
pub const RECALL_GIT_TIMEOUT: Duration = Duration::from_secs(3);
const GIT_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Environment that would point git somewhere other than the directory
/// discovery resolved, or change pathspec matching.
const GIT_OVERRIDE_ENV: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// Why a git call gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GitFailure {
    /// git is missing, could not start, exited non-zero, or answered too much.
    #[error("git_unavailable")]
    Unavailable,
    /// git did not answer within [`RECALL_GIT_TIMEOUT`]; it was killed.
    #[error("git_timeout")]
    Timeout,
}

/// Run `git --no-optional-locks <args>` in `repo_root` and return its
/// stdout. The child is killed when the call times out or its future is
/// dropped (a missed tool-path deadline).
///
/// # Errors
///
/// [`GitFailure::Timeout`] past [`RECALL_GIT_TIMEOUT`];
/// [`GitFailure::Unavailable`] for every other failure.
pub async fn run_git(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, GitFailure> {
    let mut command = std::process::Command::new("git");
    command
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(repo_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in GIT_OVERRIDE_ENV {
        command.env_remove(name);
    }
    pa_core::platform::set_no_window(&mut command);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let output = match tokio::time::timeout(RECALL_GIT_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            tracing::debug!(args = args.join(" "), %error, "git could not be started; recall skipped");
            return Err(GitFailure::Unavailable);
        }
        Err(_) => {
            tracing::debug!(args = args.join(" "), "git call timed out; recall skipped");
            return Err(GitFailure::Timeout);
        }
    };
    if !output.status.success() || output.stdout.len() > GIT_MAX_OUTPUT_BYTES {
        tracing::debug!(args = args.join(" "), status = %output.status, "git call failed; recall skipped");
        return Err(GitFailure::Unavailable);
    }
    Ok(output.stdout)
}

/// The worktree directory holding `cwd`, by walking up to the nearest
/// `.git` (a directory with `HEAD`, or a `gitdir:` file whose target has
/// `HEAD`); `None` outside a worktree. Filesystem walk only, no git process.
#[must_use]
pub fn find_recall_repo(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git_path = dir.join(".git");
        if let Ok(metadata) = std::fs::metadata(&git_path) {
            if metadata.is_file() {
                let content = std::fs::read_to_string(&git_path).ok()?;
                if let Some(target) = content.trim().strip_prefix("gitdir: ") {
                    let git_dir = dir.join(target.trim());
                    return git_dir.join("HEAD").exists().then_some(dir);
                }
            } else if metadata.is_dir() {
                return git_path.join("HEAD").exists().then_some(dir);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The mark is keyed by the resolved toplevel, so a symlinked cwd and its
/// target share one mark. `None` when the path is not valid UTF-8 (the
/// mark key is a hash of its text).
#[must_use]
pub fn resolve_repo_root(repo_dir: &Path) -> Option<String> {
    let resolved = std::fs::canonicalize(repo_dir).unwrap_or_else(|_| repo_dir.to_path_buf());
    let text = resolved.to_str()?;
    // Windows canonical paths carry the verbatim prefix Node's realpath omits.
    Some(text.strip_prefix(r"\\?\").unwrap_or(text).to_string())
}
