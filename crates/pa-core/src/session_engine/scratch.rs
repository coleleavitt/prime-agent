//! The session's private scratch directories (Claude Code's per-session
//! scratchpad and tasks directories).
//!
//! A persistent session keeps them in its artifact partition
//! (`<sessions root>/session-artifacts/<id>/`), which the session's delete
//! already removes, so nothing new has to remember them:
//!
//! - `scratchpad/`: temporary files the model writes instead of `/tmp`
//!   (intermediate results, scripts, outputs that do not belong in the
//!   project). Named in the system prompt's environment section and in the
//!   kernel's `PRIME_AGENT_SCRATCHPAD`; writable under the OS sandbox.
//! - `tasks/`: each kernel `bash()` command's persisted output
//!   (`<job id>.output`, `pa-bash`'s output file), named in the kernel's
//!   `PRIME_AGENT_TASKS_DIR`, in the `[bash-done]` notice and in the activity
//!   listing.
//!
//! A session without an artifact partition (not persisted) uses
//! `<temp>/prime-agent-scratch-<uid>/<session id>/` instead. Both are private
//! to the user (0700 on Unix).

use std::path::{Path, PathBuf};

/// The scratchpad's directory name inside the session's scratch root.
pub const SCRATCHPAD_DIR: &str = "scratchpad";
/// The persisted command outputs' directory name.
pub const TASKS_DIR: &str = "tasks";
/// The kernel environment variable naming the scratchpad.
pub const SCRATCHPAD_ENV: &str = "PRIME_AGENT_SCRATCHPAD";
/// The kernel environment variable naming the tasks directory (the host's
/// job table persists `bash()` output there).
pub const TASKS_ENV: &str = "PRIME_AGENT_TASKS_DIR";

/// One session's scratch directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScratch {
    pub scratchpad: PathBuf,
    pub tasks: PathBuf,
}

impl SessionScratch {
    /// The directories of a session: inside its artifact partition, else
    /// under the system temp directory.
    #[must_use]
    pub fn for_session(artifact_dir: Option<&Path>, session_id: &str) -> Self {
        let root =
            artifact_dir.map_or_else(|| temp_root().join(sanitize(session_id)), Path::to_path_buf);
        Self {
            scratchpad: root.join(SCRATCHPAD_DIR),
            tasks: root.join(TASKS_DIR),
        }
    }

    /// Create the scratchpad (private to the user); the tasks directory is
    /// created by the first persisted command.
    ///
    /// # Errors
    ///
    /// The directory cannot be created.
    pub fn ensure(&self) -> std::io::Result<()> {
        create_private_dir(&self.scratchpad)
    }

    /// The kernel environment variables naming the directories.
    #[must_use]
    pub fn kernel_env(&self) -> [(String, String); 2] {
        [
            (
                SCRATCHPAD_ENV.to_string(),
                self.scratchpad.to_string_lossy().to_string(),
            ),
            (
                TASKS_ENV.to_string(),
                self.tasks.to_string_lossy().to_string(),
            ),
        ]
    }

    /// The environment section's line (Claude Code's wording).
    #[must_use]
    pub fn prompt_line(&self) -> String {
        format!(
            "Scratchpad directory: {} \u{2014} always use it for temporary files (intermediate \
             results, scripts, outputs that don't belong in the project) instead of `/tmp` or \
             other system temp directories; it is session-specific, isolated from the project, \
             and private to you (`os.environ[\"{SCRATCHPAD_ENV}\"]` in the kernel). Only use \
             `/tmp` if the user explicitly asks.",
            self.scratchpad.display().to_string().replace('\\', "/")
        )
    }
}

/// `<temp>/prime-agent-scratch-<uid>` (the user id keeps two accounts'
/// directories apart on a shared temp directory).
fn temp_root() -> PathBuf {
    #[cfg(unix)]
    let name = format!(
        "prime-agent-scratch-{}",
        crate::platform::process::current_user_id()
    );
    #[cfg(not(unix))]
    let name = "prime-agent-scratch".to_string();
    std::env::temp_dir().join(name)
}

/// A session id as one path component.
fn sanitize(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

/// `dir` and its missing parents, each created private to the user.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_persistent_session_keeps_its_scratch_in_its_artifact_partition() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join("session-artifacts").join("abc");
        let scratch = SessionScratch::for_session(Some(&artifacts), "abc");
        assert_eq!(
            scratch,
            SessionScratch {
                scratchpad: artifacts.join("scratchpad"),
                tasks: artifacts.join("tasks"),
            }
        );
        scratch.ensure().unwrap();
        assert!(scratch.scratchpad.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&scratch.scratchpad)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        assert_eq!(
            scratch.kernel_env(),
            [
                (
                    SCRATCHPAD_ENV.to_string(),
                    artifacts.join("scratchpad").to_string_lossy().to_string()
                ),
                (
                    TASKS_ENV.to_string(),
                    artifacts.join("tasks").to_string_lossy().to_string()
                ),
            ]
        );
    }

    #[test]
    fn an_unpersisted_session_scratches_under_the_temp_dir() {
        let scratch = SessionScratch::for_session(None, "a/b c");
        assert!(scratch.scratchpad.starts_with(std::env::temp_dir()));
        assert!(scratch.scratchpad.ends_with("a_b_c/scratchpad"));
        assert!(scratch.prompt_line().starts_with("Scratchpad directory: "));
    }
}
