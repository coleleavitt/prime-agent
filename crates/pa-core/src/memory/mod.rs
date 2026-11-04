//! Active memory: a content-addressed file tree the model reads and writes
//! through `rlm.memory.*`, modelled on Claude Code 2.1.318's `memory_*`
//! tool family served by its in-process `memory` MCP server. Prime-agent
//! has one tree per user — `<agent dir>/memory/personal/` — shared across
//! projects and sessions, separate from the per-project auto memory tree
//! under `resources::auto_memory` (that one indexes `MEMORY.md` into every
//! session's prompt; this one is the model's own scratchpad).
//!
//! The CC-side cloud backend (POST `/memories*` on an org account) is NOT
//! ported: a live probe on this fork's own user showed `/v1/code/local/memory`
//! returning 404 ("account not provisioned"). The local file store carries
//! the whole API; the account backend can be added behind an opt-in later.
//!
//! Path and content rules follow CC exactly so the model's habits transfer
//! between tools: paths are relative POSIX, NFC-normalized, at most 20
//! segments and 1024 bytes, no `.` / `..` / control characters; content is
//! capped at 100 KiB.

mod host;
mod local;
mod path;

use std::path::{Path, PathBuf};

pub use host::{MEMORY_HOST_REQUESTS, register_host_handlers};
pub use local::{LocalStore, SearchHit};
pub use path::{MAX_PATH_BYTES, MAX_PATH_SEGMENTS, MemoryPath, PathError};

/// Content cap, bytes. Matches CC's `102400`.
pub const MAX_CONTENT_BYTES: usize = 100 * 1024;

/// The personal memory tree for this user. `<agent dir>/memory/personal/`.
#[must_use]
pub fn personal_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("memory").join("personal")
}

/// What one memory op can go wrong with.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("invalid path: {0}")]
    InvalidPath(#[from] PathError),
    #[error(
        "content too large: {size} bytes exceeds the {MAX_CONTENT_BYTES}-byte cap; split the entry into several files"
    )]
    ContentTooLarge { size: usize },
    #[error("memory file does not exist: {path}")]
    NotFound { path: String },
    #[error("memory file already exists: {path}")]
    AlreadyExists { path: String },
    #[error("search pattern matched no files")]
    NoMatches,
    #[error("the substring to replace did not match once in {path}: found {occurrences} times")]
    ReplaceNotUnique { path: String, occurrences: usize },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl MemoryError {
    /// The short error code Claude Code uses in its JSON response; stable
    /// across releases.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            MemoryError::InvalidPath(_) => "invalid_path",
            MemoryError::ContentTooLarge { .. } => "content_too_large",
            MemoryError::NotFound { .. } => "not_found",
            MemoryError::AlreadyExists { .. } => "already_exists",
            MemoryError::NoMatches => "no_matches",
            MemoryError::ReplaceNotUnique { .. } => "replace_not_unique",
            MemoryError::Io(_) => "io_error",
        }
    }
}
