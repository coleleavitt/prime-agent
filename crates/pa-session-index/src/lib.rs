//! The persisted saved-session catalog index (the TS product's
//! `session-catalog-index.ts`): what each session file listed as, keyed by
//! its `(size, mtimeMs)`, so a fresh process lists unchanged files without
//! folding them. It plugs into the native catalog scan through
//! [`pa_core::session::catalog_cache::SessionCatalogCache`]; see `README.md`.
//!
//! A session directory's index is read once per process, on that
//! directory's first lookup (the scan's blocking-pool thread). Writes happen
//! only after a listing learned something (a new, changed, or deleted file)
//! and run on a writer thread, so no listing waits on them.

mod format;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use format::IndexedEntry;
pub use format::{SESSION_INDEX_FILE, SESSION_SEARCH_INDEX_FILE};
use pa_core::session::catalog_cache::{CatalogEntry, CatalogFile, SessionCatalogCache};

/// The persisted catalog index of every session directory this process
/// lists. Cloning shares the index (the composition root keeps a clone to
/// drain writes at exit).
#[derive(Clone, Default)]
pub struct SessionIndex {
    dirs: Arc<Mutex<HashMap<PathBuf, Arc<DirIndex>>>>,
}

/// One session directory's entries and its writer state.
struct DirIndex {
    dir: PathBuf,
    state: Mutex<DirState>,
    /// Signalled when the writer goes idle.
    idle: Condvar,
}

#[derive(Default)]
struct DirState {
    /// By file name.
    entries: HashMap<String, Arc<IndexedEntry>>,
    /// Bumped by every change to `entries`.
    revision: u64,
    /// The revision the files on disk hold (or the last attempt's).
    persisted: u64,
    writing: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicked holder leaves a cache, never a source of truth: keep going.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionIndex {
    /// An empty index; nothing is read until the first lookup.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait until every pending index write has finished, or `deadline`
    /// passes. Returns whether all writers are idle.
    #[must_use]
    pub fn flush(&self, deadline: Instant) -> bool {
        let dirs: Vec<Arc<DirIndex>> = lock(&self.dirs).values().cloned().collect();
        dirs.iter().all(|dir| {
            let mut state = lock(&dir.state);
            while state.writing {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                state = dir
                    .idle
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            true
        })
    }

    /// The directory's index, read from disk on first use.
    fn dir(&self, session_dir: &Path) -> Arc<DirIndex> {
        let mut dirs = lock(&self.dirs);
        if let Some(dir) = dirs.get(session_dir) {
            return Arc::clone(dir);
        }
        let started = Instant::now();
        let read = |name: &str| fs::read_to_string(session_dir.join(name)).unwrap_or_default();
        let entries = format::parse(&read(SESSION_INDEX_FILE), &read(SESSION_SEARCH_INDEX_FILE));
        tracing::debug!(
            target: "pa_session_index",
            entries = entries.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
            "session index loaded"
        );
        let dir = Arc::new(DirIndex {
            dir: session_dir.to_path_buf(),
            state: Mutex::new(DirState {
                entries: entries
                    .into_iter()
                    .map(|(file, entry)| (file, Arc::new(entry)))
                    .collect(),
                ..DirState::default()
            }),
            idle: Condvar::new(),
        });
        dirs.insert(session_dir.to_path_buf(), Arc::clone(&dir));
        dir
    }
}

/// The file's name when it sits directly in the scanned directory (the
/// index stores names, as the TS index does).
fn file_name<'a>(file: &CatalogFile<'a>) -> Option<&'a str> {
    let relative = file.path.strip_prefix(file.session_dir).ok()?;
    let mut components = relative.components();
    let name = components.next()?.as_os_str().to_str()?;
    components.next().is_none().then_some(name)
}

impl SessionCatalogCache for SessionIndex {
    fn lookup(&self, file: &CatalogFile<'_>) -> Option<CatalogEntry> {
        let name = file_name(file)?;
        let dir = self.dir(file.session_dir);
        let state = lock(&dir.state);
        let indexed = state.entries.get(name)?;
        (indexed.size == file.key.size
            // The exact key the entry was recorded at, bit for bit.
            && indexed.mtime_ms.to_bits() == file.key.mtime_ms.to_bits()
            && indexed.fold_version == file.fold_version)
            .then(|| indexed.entry.clone())
    }

    fn record(&self, file: &CatalogFile<'_>, entry: CatalogEntry) {
        let Some(name) = file_name(file) else {
            return;
        };
        let dir = self.dir(file.session_dir);
        let mut state = lock(&dir.state);
        state.entries.insert(
            name.to_string(),
            Arc::new(IndexedEntry {
                size: file.key.size,
                mtime_ms: file.key.mtime_ms,
                fold_version: file.fold_version,
                entry,
            }),
        );
        state.revision += 1;
    }

    fn scan_finished(&self, session_dir: &Path, listed: &[PathBuf]) {
        let listed: HashSet<&str> = listed
            .iter()
            .filter(|path| path.parent() == Some(session_dir))
            .filter_map(|path| path.file_name()?.to_str())
            .collect();
        let dir = self.dir(session_dir);
        let mut state = lock(&dir.state);
        let before = state.entries.len();
        state
            .entries
            .retain(|file, _| listed.contains(file.as_str()));
        if state.entries.len() != before {
            state.revision += 1;
        }
        if state.revision == state.persisted || state.writing {
            // Nothing new, or the running writer loops until it holds the
            // latest revision.
            return;
        }
        state.writing = true;
        drop(state);
        let writer = Arc::clone(&dir);
        let spawned = std::thread::Builder::new()
            .name("session-index-writer".to_string())
            .spawn(move || write_until_current(&writer));
        if let Err(error) = spawned {
            tracing::debug!(target: "pa_session_index", %error, "session index writer did not start");
            let mut state = lock(&dir.state);
            state.writing = false;
            dir.idle.notify_all();
        }
    }
}

/// The writer thread: persist snapshots until the files hold the latest
/// revision, then go idle.
fn write_until_current(dir: &DirIndex) {
    loop {
        let (revision, mut snapshot) = {
            let mut state = lock(&dir.state);
            if state.persisted == state.revision {
                // Under the same lock as the check: a listing that finishes
                // after this sees `writing == false` and starts a writer.
                state.writing = false;
                dir.idle.notify_all();
                return;
            }
            let snapshot: Vec<(String, Arc<IndexedEntry>)> = state
                .entries
                .iter()
                .map(|(file, entry)| (file.clone(), Arc::clone(entry)))
                .collect();
            (state.revision, snapshot)
        };
        // Newest first, then by name: a stable file for an unchanged catalog.
        snapshot.sort_by(|(a_file, a), (b_file, b)| {
            b.mtime_ms
                .total_cmp(&a.mtime_ms)
                .then_with(|| a_file.cmp(b_file))
        });
        let started = Instant::now();
        let (metadata, search) = format::render(
            snapshot
                .iter()
                .map(|(file, entry)| (file.as_str(), entry.as_ref())),
        );
        // The corpus tier first: a reader that sees the new metadata tier
        // finds the matching corpus lines already in place.
        let result = write_atomically(&dir.dir.join(SESSION_SEARCH_INDEX_FILE), &search)
            .and_then(|()| write_atomically(&dir.dir.join(SESSION_INDEX_FILE), &metadata));
        match result {
            Ok(()) => tracing::debug!(
                target: "pa_session_index",
                entries = snapshot.len(),
                bytes = metadata.len() + search.len(),
                elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
                "session index written"
            ),
            // Best-effort, like TS: a failed write only costs the next
            // process its fold.
            Err(error) => {
                tracing::debug!(target: "pa_session_index", %error, "session index write failed");
            }
        }
        lock(&dir.state).persisted = revision;
    }
}

/// Replace `path` with `contents` through a `<path>.<pid>.tmp` sibling (the
/// TS temp name). The files carry transcript text: the temp is owner-only,
/// and the rename carries the mode.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        pa_core::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temp)?;
        file.write_all(contents.as_bytes())?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
