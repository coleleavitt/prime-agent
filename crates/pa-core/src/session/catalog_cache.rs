//! The saved-session catalog cache seam: a persisted or shared cache the
//! catalog scan (pa-daemon's saved-session listing) consults before it folds
//! a session file, so a fresh process can serve unchanged files without
//! reparsing them.
//!
//! Nothing is installed in the native product: the scan folds every file,
//! exactly as it always has. The composition root installs one cache, once,
//! before any listing runs; native crates never name the implementation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// One session file as the catalog scan sees it: where it is, the identity
/// key a cached row is valid for, and the version of the fold that derives
/// rows from the file's content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogFile<'a> {
    /// The scanned directory.
    pub session_dir: &'a Path,
    /// The session file (inside `session_dir`).
    pub path: &'a Path,
    /// The file's identity: session files are append-only, so an unchanged
    /// key means unchanged content.
    pub key: CatalogFileKey,
    /// The producer's fold version: a row recorded under another version
    /// was derived by different rules and must never serve.
    pub fold_version: u32,
}

/// `(size, mtimeMs)`, the content identity of an append-only session file.
/// `mtime_ms` is the fractional-millisecond float Node reports as
/// `Stats.mtimeMs` (`sec * 1e3 + nsec / 1e6`), so a key recorded by either
/// product compares equal for the same file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogFileKey {
    pub size: u64,
    pub mtime_ms: f64,
}

impl CatalogFileKey {
    /// The key for a file's metadata; `None` when the platform reports no
    /// modification time or one before the epoch (never cached).
    #[must_use]
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self {
            size: metadata.len(),
            mtime_ms: mtime_ms(metadata.modified().ok()?)?,
        })
    }
}

fn mtime_ms(modified: SystemTime) -> Option<f64> {
    let since = modified.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some(since.as_secs() as f64 * 1e3 + f64::from(since.subsec_nanos()) / 1e6)
}

/// The token usage a saved session's own work recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
}

/// One saved-session row, everything the catalog derives from the file's
/// content (the path is the caller's; ledger-derived fields are not part of
/// the file's row).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSessionRow {
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    /// The persisted `session_state` status (`active`, `archived`, ...).
    pub state: Option<String>,
    /// `(provider, model id)`.
    pub model: Option<(String, String)>,
    pub thinking_level: Option<String>,
    pub parent_session_path: Option<String>,
    pub rlm_depth: u32,
    pub created: String,
    pub modified: String,
    pub message_count: usize,
    pub first_message: String,
    /// The capped transcript search corpus.
    pub all_messages_text: String,
    pub usage: Option<CatalogUsage>,
}

/// What a file scanned to.
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogEntry {
    /// A session row.
    Session(Box<CatalogSessionRow>),
    /// The file is not a session (the scan produced no row): it is not
    /// rescanned until it changes.
    NotASession,
}

/// A cache the saved-session catalog consults per file.
///
/// The catalog calls [`Self::lookup`] for every file it lists; a hit is
/// served as the file's row without folding the file. On a miss the catalog
/// folds the file and, when the file's key did not change during the fold,
/// hands the result to [`Self::record`]. After a complete listing it calls
/// [`Self::scan_finished`] with every file it listed, so entries for files
/// that disappeared can be dropped.
///
/// Implementations are a cache, never a source of truth: a hit must have
/// been recorded for exactly the same [`CatalogFileKey`] and
/// [`CatalogFile::fold_version`]. All methods run on the scan's thread (a
/// blocking-pool thread, never a paint path) and must not block on anything
/// but local disk; durable writes belong on the implementation's own
/// background worker.
pub trait SessionCatalogCache: Send + Sync {
    /// The cached entry for `file`, or `None` to fold it.
    fn lookup(&self, file: &CatalogFile<'_>) -> Option<CatalogEntry>;

    /// Remember what `file` scanned to.
    fn record(&self, file: &CatalogFile<'_>, entry: CatalogEntry);

    /// A listing of `session_dir` completed; `listed` is every session file
    /// it saw.
    fn scan_finished(&self, session_dir: &Path, listed: &[PathBuf]);
}

static INSTALLED: OnceLock<Box<dyn SessionCatalogCache>> = OnceLock::new();

/// Install the process's catalog cache. The composition root calls this
/// once, before any listing runs; a later call is ignored and returns
/// `false`.
pub fn install(cache: Box<dyn SessionCatalogCache>) -> bool {
    INSTALLED.set(cache).is_ok()
}

/// The installed cache; `None` (the native product) means every listing
/// folds every file.
#[must_use]
pub fn installed() -> Option<&'static dyn SessionCatalogCache> {
    INSTALLED.get().map(Box::as_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_matches_node_mtime_ms() {
        // Node: `mtimeMs = tv_sec * 1e3 + tv_nsec / 1e6`, in doubles.
        // `JSON.stringify` of that value prints the literal below.
        let modified =
            SystemTime::UNIX_EPOCH + std::time::Duration::new(1_789_177_896, 715_123_456);
        assert_eq!(mtime_ms(modified), Some(1_789_177_896_715.123_5));
        let before_epoch = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(mtime_ms(before_epoch), None);
    }
}
