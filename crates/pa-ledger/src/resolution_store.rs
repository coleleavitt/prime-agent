//! The durable per-repo resolution store (TS `FileResolutionStore`):
//! `<agentDir>/resolution/<basename>.<sha256(repoDir)[:16]>.json`, holding
//! `{version: 1, repo, records}` written `JSON.stringify(file, null, 2) +
//! "\n"`, mode 0600 in a 0700 directory, temp file + fsync + rename under the
//! `proper-lockfile`-compatible `<store>.lock` directory. A record's `fix` is
//! verbatim cell source, so the file is owner-only and never leaves the
//! machine. Every failure degrades to the session-only index.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fingerprint::hex;
use crate::resolution::{ResolutionRecord, ResolutionStore, bound_records};

/// The store directory's name under the agent dir.
pub const RESOLUTION_DIR_NAME: &str = "resolution";
const STORE_VERSION: u64 = 1;
const STORE_LOCK_ATTEMPTS: u32 = 40;
const STORE_LOCK_RETRY: Duration = Duration::from_millis(5);
const STORE_LOCK_STALE: Duration = Duration::from_secs(10);

/// `<agentDir>/resolution`.
#[must_use]
pub fn resolution_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(RESOLUTION_DIR_NAME)
}

/// `path.basename` of a directory path given as text (trailing separators ignored).
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or("")
}

/// `<agentDir>/resolution/<basename>.<sha256(repoDir)[:16]>.json`; the
/// basename keeps `[A-Za-z0-9._-]` and maps every other UTF-16 unit to `_`.
#[must_use]
pub fn resolution_store_path(repo_dir: &str, agent_dir: &Path) -> PathBuf {
    let hash = hex(&Sha256::digest(repo_dir.as_bytes())[..8]);
    let mut name = String::new();
    for ch in basename(repo_dir).chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            name.push(ch);
        } else {
            name.push_str(&"_".repeat(ch.len_utf16()));
        }
    }
    if name.is_empty() {
        name.push_str("repo");
    }
    resolution_dir(agent_dir).join(format!("{name}.{hash}.json"))
}

/// The worktree directory holding `cwd` (TS `findGitPaths(cwd).repoDir`):
/// the nearest ancestor with a `.git` directory holding `HEAD`, or a
/// `gitdir:` file whose target holds `HEAD`. A `.git` without `HEAD` ends
/// the search with `None`.
#[must_use]
pub fn find_repo_dir(cwd: &Path) -> Option<PathBuf> {
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

/// The store for the repo containing `cwd`, or `None` outside a git
/// worktree (no stable key for "this project" then).
#[must_use]
pub fn open_resolution_store(cwd: &Path, agent_dir: &Path) -> Option<FileResolutionStore> {
    let Some(repo_dir) = find_repo_dir(cwd) else {
        tracing::debug!(cwd = %cwd.display(), "resolution store disabled: not inside a git repository");
        return None;
    };
    let repo = repo_dir.to_string_lossy().into_owned();
    Some(FileResolutionStore::new(
        resolution_store_path(&repo, agent_dir),
        repo,
    ))
}

#[derive(Clone)]
struct Cached {
    modified: Option<SystemTime>,
    size: u64,
    records: Vec<ResolutionRecord>,
}

/// One JSON file per repo, at most 64 records of at most 1200 UTF-16 units
/// per cell.
pub struct FileResolutionStore {
    path: PathBuf,
    repo: String,
    cached: Mutex<Option<Cached>>,
}

impl FileResolutionStore {
    /// A store at `path` for the repo at `repo`.
    #[must_use]
    pub fn new(path: PathBuf, repo: String) -> Self {
        Self {
            path,
            repo,
            cached: Mutex::new(None),
        }
    }

    /// The store file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Option<Cached>> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Parse the file; anything unreadable or malformed is an empty store.
    fn read(&self) -> Vec<ResolutionRecord> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) => {
                // The first write of a repo's store reads a file not there yet.
                if error.kind() != std::io::ErrorKind::NotFound {
                    self.warn(
                        "resolution store is unreadable; ignoring it",
                        Some(&error.to_string()),
                    );
                }
                return Vec::new();
            }
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.warn(
                    "resolution store is unreadable; ignoring it",
                    Some(&error.to_string()),
                );
                return Vec::new();
            }
        };
        let records = parsed
            .as_object()
            .filter(|file| file.get("version").and_then(Value::as_f64) == Some(1.0))
            .and_then(|file| file.get("records"))
            .and_then(Value::as_array);
        let Some(records) = records else {
            self.warn(
                "resolution store has an unexpected shape; ignoring it",
                None,
            );
            return Vec::new();
        };
        records
            .iter()
            .filter_map(ResolutionRecord::from_value)
            .collect()
    }

    fn mutate(&self, update: impl FnOnce(Vec<ResolutionRecord>) -> Option<Vec<ResolutionRecord>>) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        if let Err(error) = pa_core::platform::perms::create_dir_all_private(dir)
            .and_then(|()| pa_core::platform::restrict_dir(dir))
        {
            self.warn(
                "resolution store write failed; hint stays session-local",
                Some(&error.to_string()),
            );
            return;
        }
        let mut lock = None;
        for attempt in 0..STORE_LOCK_ATTEMPTS {
            match pa_core::platform::LockDir::acquire(&self.path, STORE_LOCK_STALE) {
                Ok(acquired) => {
                    lock = Some(acquired);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if attempt + 1 < STORE_LOCK_ATTEMPTS {
                        std::thread::sleep(STORE_LOCK_RETRY);
                    }
                }
                Err(error) => {
                    self.warn(
                        "resolution store write failed; hint stays session-local",
                        Some(&error.to_string()),
                    );
                    return;
                }
            }
        }
        let Some(_lock) = lock else {
            tracing::warn!(path = %self.path.display(), "resolution store is locked by another session; hint not persisted");
            return;
        };
        // Re-read under the lock: the cache may predate another session's write.
        let Some(next) = update(self.read()) else {
            return;
        };
        if let Err(error) = self.write(&bound_records(next)) {
            self.warn(
                "resolution store write failed; hint stays session-local",
                Some(&error.to_string()),
            );
        }
    }

    fn write(&self, records: &[ResolutionRecord]) -> std::io::Result<()> {
        let file = serde_json::json!({
            "version": STORE_VERSION,
            "repo": self.repo,
            "records": records,
        });
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?
        );
        let mut suffix = [0u8; 4];
        getrandom::fill(&mut suffix).map_err(std::io::Error::other)?;
        let mut temp = self.path.as_os_str().to_os_string();
        temp.push(format!(".{}.{}.tmp", std::process::id(), hex(&suffix)));
        let temp = PathBuf::from(temp);
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            pa_core::platform::set_private_mode(&mut options);
            let mut handle = options.open(&temp)?;
            handle.write_all(text.as_bytes())?;
            handle.sync_all()?;
            drop(handle);
            pa_core::platform::rename_onto(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
        // The create mode is masked by the umask, so pin it after the rename.
        pa_core::platform::restrict_file(&self.path)?;
        *self.cache() = None;
        Ok(())
    }

    fn warn(&self, message: &str, error: Option<&str>) {
        if let Some(error) = error {
            tracing::warn!(path = %self.path.display(), error, "{message}");
        } else {
            tracing::warn!(path = %self.path.display(), "{message}");
        }
    }
}

impl ResolutionStore for FileResolutionStore {
    fn load(&self) -> Vec<ResolutionRecord> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    self.warn(
                        "resolution store cannot be stat'd",
                        Some(&error.to_string()),
                    );
                }
                return Vec::new();
            }
        };
        let modified = metadata.modified().ok();
        let size = metadata.len();
        if let Some(cached) = self.cache().as_ref() {
            if cached.modified == modified && cached.size == size {
                return cached.records.clone();
            }
        }
        let records = self.read();
        *self.cache() = Some(Cached {
            modified,
            size,
            records: records.clone(),
        });
        records
    }

    fn save(&self, record: &ResolutionRecord) {
        self.mutate(|records| {
            let mut kept: Vec<ResolutionRecord> = records
                .into_iter()
                .filter(|held| held.fingerprint_id != record.fingerprint_id)
                .collect();
            kept.push(record.clone());
            Some(kept)
        });
    }

    fn forget(&self, fingerprint_id: &str) {
        self.mutate(|records| {
            let before = records.len();
            let kept: Vec<ResolutionRecord> = records
                .into_iter()
                .filter(|held| held.fingerprint_id != fingerprint_id)
                .collect();
            (kept.len() != before).then_some(kept)
        });
    }
}
