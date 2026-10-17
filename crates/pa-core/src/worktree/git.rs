//! The git commands the worktree helper runs: always in a directory it
//! names, with the repository-selection environment scrubbed (an inherited
//! `GIT_DIR` must not redirect a worktree add into another repository) and
//! terminal prompts off.

use std::path::Path;
use std::process::Command;

use super::WorktreeError;

/// One finished git command.
pub(super) struct GitOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    /// The trimmed stdout of a successful command, else the error.
    pub(super) fn ok(self, what: &str) -> Result<String, WorktreeError> {
        if self.success {
            Ok(self.stdout.trim().to_string())
        } else {
            Err(WorktreeError::Git {
                command: what.to_string(),
                stderr: self.stderr.trim().to_string(),
            })
        }
    }
}

/// Run `git <args>` in `cwd`.
pub(super) fn run(cwd: &Path, args: &[&str]) -> Result<GitOutput, WorktreeError> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    crate::git_env::scrub_repository_selection(&mut command);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null());
    // A test run's fixture repositories never see this machine's git
    // configuration (global hooks, signing, excludes), whichever crate's
    // tests drive the helper.
    if pa_types::platform::test_isolation::is_test_process() {
        crate::git_env::isolate_fixture(&mut command, &std::env::temp_dir());
    }
    let output = command.output().map_err(|error| WorktreeError::Io {
        what: "run git".to_string(),
        error,
    })?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `git <args>` in `cwd`, its trimmed stdout on success.
pub(super) fn stdout(cwd: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    run(cwd, args)?.ok(&format!("git {}", args.join(" ")))
}

/// The commit `rev` names, when it names one.
pub(super) fn resolve_commit(cwd: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    let output = run(cwd, &["rev-parse", "--verify", "--quiet", &spec]).ok()?;
    let commit = output.stdout.trim();
    (output.success && !commit.is_empty()).then(|| commit.to_string())
}

/// Whether the local branch exists.
pub(super) fn branch_exists(cwd: &Path, branch: &str) -> bool {
    let reference = format!("refs/heads/{branch}");
    run(cwd, &["show-ref", "--verify", "--quiet", &reference]).is_ok_and(|output| output.success)
}
