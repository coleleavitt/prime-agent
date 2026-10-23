//! File checkpoints: the history `/rewind` restores code from (Claude Code's
//! file history, `~/.claude/file-history/<session>/`).
//!
//! The kernel's edit helpers report every file write to the host before
//! and after it lands (`file_history.before_write` /
//! `file_history.after_write`, see [`register_host_handlers`]). Before a
//! write the host backs up the file's prior bytes, content-addressed, into
//! the session's artifact tree (`<session artifacts>/file-history/blobs/
//! <sha256>`) and appends one record to the session's write log
//! (`file-history/log.jsonl`); after it, the log records the new content's
//! hash. The directory is private (`0700`, files `0600`) and every leaf
//! operation resolves against a pinned directory handle
//! ([`crate::platform::private_fs`]); off unix that discipline has no
//! platform proof, so the history fails closed (nothing is recorded and
//! the edits proceed).
//!
//! A checkpoint is a point in time, not a copy of the tree: the state of a
//! file at time `t` is the prior content of its first recorded write at or
//! after `t` (or its current content when none). A user prompt's
//! checkpoint is the time its session entry was written, so rewinding the
//! code to before a prompt restores every file the agent wrote since to
//! that state. Restores are recorded as writes too, so a later rewind to
//! an earlier prompt still sees through them.
//!
//! Only host-mediated writes are tracked: like Claude Code, a file changed
//! by `bash()`, by plain Python I/O, or by hand is not. A file whose
//! content no longer matches the agent's last recorded write was changed
//! outside the agent since; a restore leaves it untouched unless forced.
//!
//! The history lives in the session's artifact directory, so it goes when
//! the session's artifacts do (a deleted session). Backups are capped per
//! file ([`MAX_BACKUP_BYTES`]) and per session ([`MAX_HISTORY_BYTES`]); a
//! write past either cap is still recorded, as not restorable.

mod host;
mod restore;
mod store;

use std::path::{Path, PathBuf};

pub use host::{AFTER_WRITE_REQUEST, BEFORE_WRITE_REQUEST, register_host_handlers};
pub use restore::{
    EXTERNALLY_MODIFIED_REASON,
    PlannedAction,
    PlannedFile,
    RestorePlan,
    RestoreReport,
    SkippedFile,
};
use serde::{Deserialize, Serialize};

/// The history's directory name inside a session's artifact directory.
pub const FILE_HISTORY_DIR_NAME: &str = "file-history";

/// The largest file whose prior content is backed up (16 MiB); a larger
/// file's write is recorded as not restorable.
pub const MAX_BACKUP_BYTES: u64 = 16 * 1024 * 1024;

/// The most backup bytes one session keeps (1 GiB); writes past it are
/// recorded as not restorable.
pub const MAX_HISTORY_BYTES: u64 = 1024 * 1024 * 1024;

/// Setting this environment variable to a non-empty value other than `0`
/// or `false` turns file checkpointing off for new sessions (Claude Code's
/// `CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING`).
pub const DISABLE_ENV: &str = "PRIME_AGENT_DISABLE_FILE_CHECKPOINTING";

/// Whether file checkpointing is on, given the `fileCheckpointingEnabled`
/// setting (on when unset) and [`DISABLE_ENV`].
#[must_use]
pub fn checkpointing_enabled(setting: Option<bool>) -> bool {
    let disabled_by_env = std::env::var(DISABLE_ENV).is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    });
    setting.unwrap_or(true) && !disabled_by_env
}

/// One file's recorded state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum FileState {
    /// No file at the path.
    Absent,
    /// A regular file with this content; `stored` when its bytes are kept
    /// as a backup (a post-write state records only the hash).
    Content {
        sha256: String,
        size: u64,
        stored: bool,
    },
    /// A file past [`MAX_BACKUP_BYTES`]: neither hashed nor kept.
    TooLarge { size: u64 },
}

/// The session's file history, rooted at
/// `<session artifact dir>/file-history`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistory {
    dir: PathBuf,
}

impl FileHistory {
    /// The history of the session whose artifact directory is
    /// `artifact_dir`. Nothing is created until the first write.
    #[must_use]
    pub fn for_artifact_dir(artifact_dir: &Path) -> Self {
        Self {
            dir: artifact_dir.join(FILE_HISTORY_DIR_NAME),
        }
    }

    /// The history directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether anything was ever recorded for this session.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.dir.join(store::LOG_FILE).is_file()
    }

    /// Record `path`'s current state, backing its bytes up, before a write
    /// replaces it. `path` must be absolute.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is relative or not a regular file, or
    /// when the history cannot be written (off unix: always).
    pub fn before_write(&self, path: &Path, now_ms: u64) -> std::io::Result<()> {
        let _guard = store::lock();
        let store = store::Store::open(&self.dir, /*create*/ true)?;
        store.record_current(path, store::Op::Pre, now_ms)
    }

    /// Record `path`'s state after a write landed (its hash, no backup).
    ///
    /// # Errors
    ///
    /// Returns an error when the path is relative or not a regular file, or
    /// when the history cannot be written.
    pub fn after_write(&self, path: &Path, now_ms: u64) -> std::io::Result<()> {
        let _guard = store::lock();
        let store = store::Store::open(&self.dir, /*create*/ true)?;
        store.record_current(path, store::Op::Post, now_ms)
    }

    /// What restoring the code to its state at `at_ms` would change; empty
    /// when nothing was recorded.
    ///
    /// # Errors
    ///
    /// Returns an error when the history exists but cannot be read.
    pub fn plan(&self, at_ms: u64) -> std::io::Result<RestorePlan> {
        let _guard = store::lock();
        match store::Store::open(&self.dir, /*create*/ false) {
            Ok(store) => restore::plan(&store, at_ms),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RestorePlan::default())
            }
            Err(error) => Err(error),
        }
    }

    /// [`Self::plan`] for each of `points`, in order, reading the log and
    /// every recorded file once (the `/rewind` picker's rows).
    ///
    /// # Errors
    ///
    /// Returns an error when the history exists but cannot be read.
    pub fn plans(&self, points: &[u64]) -> std::io::Result<Vec<RestorePlan>> {
        let _guard = store::lock();
        match store::Store::open(&self.dir, /*create*/ false) {
            Ok(store) => restore::plans(&store, points),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(vec![RestorePlan::default(); points.len()])
            }
            Err(error) => Err(error),
        }
    }

    /// Restore every recorded file to its state at `at_ms`. A file changed
    /// outside the agent since its last recorded write is skipped unless
    /// `force`; every restore is itself recorded at `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error when the history exists but cannot be read; a
    /// file that cannot be restored is reported in
    /// [`RestoreReport::skipped`] instead.
    pub fn restore(&self, at_ms: u64, force: bool, now_ms: u64) -> std::io::Result<RestoreReport> {
        let _guard = store::lock();
        match store::Store::open(&self.dir, /*create*/ false) {
            Ok(store) => restore::apply(&store, at_ms, force, now_ms),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RestoreReport::default())
            }
            Err(error) => Err(error),
        }
    }
}

/// Milliseconds since the Unix epoch, the history's clock.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests;
