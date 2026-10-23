//! The files the model has read, host-side: what the changed-file and
//! re-read reminders compare against (Claude Code's `readFileState`).
//!
//! The kernel reports the files each cell opened (`fileAccess` on the
//! cell's `done` frame); the executor hands them to [`FileReadTracker::sink`],
//! which only queues them. At the next turn boundary the tracker stats and
//! hashes the queued paths (the baseline), then re-checks every tracked file:
//! one whose content changed since the model last read or wrote it is
//! reported once, with the changed lines when the old copy is small enough to
//! keep. A cell's own writes refresh the baseline silently (the model made
//! that change). Compaction clears the tracker: what the model read before it
//! is no longer in its context.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::kernel::shared::{FileAccessSink, KernelFileAccess};

/// Files larger than this are not tracked at all.
pub const MAX_TRACKED_FILE_BYTES: u64 = 1024 * 1024;
/// Files up to this size keep their content, so a change can show its lines.
pub const MAX_KEPT_CONTENT_BYTES: usize = 64 * 1024;
/// How many files the tracker follows (the least recently read drop first).
pub const MAX_TRACKED_FILES: usize = 256;
/// A changed region longer than this many lines is not shown.
const MAX_SNIPPET_LINES: usize = 40;
/// Lines of unchanged context around a changed region.
const SNIPPET_CONTEXT_LINES: usize = 2;

#[derive(Debug, Clone)]
struct FileRecord {
    modified: Option<SystemTime>,
    len: u64,
    hash: [u8; 32],
    content: Option<String>,
}

/// One tracked file whose content changed since the model last saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: PathBuf,
    /// The changed lines of the new content (`cat -n` numbering), when the
    /// old copy was kept and the region is short.
    pub snippet: Option<String>,
}

/// What one boundary found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNotes {
    pub changed: Vec<ChangedFile>,
    /// Files a cell re-read whose content had not changed since the
    /// previous read (each reported once until the tracker clears).
    pub rereads: Vec<PathBuf>,
}

#[derive(Default)]
struct TrackerState {
    pending: Vec<KernelFileAccess>,
    records: HashMap<PathBuf, FileRecord>,
    /// Least recently touched first.
    order: VecDeque<PathBuf>,
    noted_rereads: HashSet<PathBuf>,
}

impl TrackerState {
    fn touch(&mut self, path: &Path, record: FileRecord) {
        self.order.retain(|known| known != path);
        self.order.push_back(path.to_path_buf());
        self.records.insert(path.to_path_buf(), record);
        while self.order.len() > MAX_TRACKED_FILES {
            if let Some(oldest) = self.order.pop_front() {
                self.records.remove(&oldest);
            }
        }
    }

    fn forget(&mut self, path: &Path) {
        self.order.retain(|known| known != path);
        self.records.remove(path);
    }
}

/// The session's read-file state.
#[derive(Default)]
pub struct FileReadTracker {
    state: Mutex<TrackerState>,
}

fn snapshot(path: &Path) -> Option<FileRecord> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_TRACKED_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // Binary files are not reminder material.
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    let hash: [u8; 32] = Sha256::digest(&bytes).into();
    let content = (bytes.len() <= MAX_KEPT_CONTENT_BYTES)
        .then(|| String::from_utf8(bytes).ok())
        .flatten();
    Some(FileRecord {
        modified: metadata.modified().ok(),
        len: metadata.len(),
        hash,
        content,
    })
}

impl FileReadTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The kernel executor's listener: it only queues the access (the
    /// file I/O runs at the boundary, off the async runtime).
    #[must_use]
    pub fn sink(self: &Arc<Self>) -> FileAccessSink {
        let tracker = Arc::downgrade(self);
        Arc::new(move |access: &KernelFileAccess| {
            if let Some(tracker) = tracker.upgrade() {
                tracker.queue(access.clone());
            }
        })
    }

    /// Queue one cell's file access for the next boundary.
    pub fn queue(&self, access: KernelFileAccess) {
        if !access.is_empty() {
            self.lock().pending.push(access);
        }
    }

    /// Forget everything (compaction: the model no longer holds what it read).
    pub fn clear(&self) {
        *self.lock() = TrackerState::default();
    }

    /// How many files are tracked (tests, diagnostics).
    #[must_use]
    pub fn tracked_count(&self) -> usize {
        self.lock().records.len()
    }

    /// Record the queued accesses, then re-check the tracked files. Blocking
    /// file I/O: run it off the async runtime.
    pub fn boundary(&self) -> FileNotes {
        let pending = std::mem::take(&mut self.lock().pending);
        let mut notes = FileNotes::default();
        for access in pending {
            self.record_access(&access, &mut notes);
        }
        let tracked: Vec<(PathBuf, FileRecord)> = {
            let state = self.lock();
            state
                .order
                .iter()
                .rev()
                .filter_map(|path| Some((path.clone(), state.records.get(path)?.clone())))
                .collect()
        };
        for (path, record) in tracked {
            match std::fs::metadata(&path) {
                Ok(metadata)
                    if metadata.len() == record.len
                        && metadata.modified().ok() == record.modified =>
                {
                    continue;
                }
                Ok(_) => {}
                Err(_) => {
                    self.lock().forget(&path);
                    continue;
                }
            }
            let Some(current) = snapshot(&path) else {
                self.lock().forget(&path);
                continue;
            };
            if current.hash != record.hash {
                let snippet = match (&record.content, &current.content) {
                    (Some(old), Some(new)) => change_snippet(old, new),
                    _ => None,
                };
                notes.changed.push(ChangedFile {
                    path: path.clone(),
                    snippet,
                });
                // A changed file's next unchanged re-read is news again.
                self.lock().noted_rereads.remove(&path);
            }
            let mut state = self.lock();
            if let Some(slot) = state.records.get_mut(&path) {
                *slot = current;
            }
        }
        notes
    }

    fn record_access(&self, access: &KernelFileAccess, notes: &mut FileNotes) {
        for path in &access.writes {
            match snapshot(path) {
                Some(record) => self.lock().touch(path, record),
                None => self.lock().forget(path),
            }
        }
        for path in &access.reads {
            if access.writes.contains(path) {
                continue;
            }
            let Some(record) = snapshot(path) else {
                continue;
            };
            let mut state = self.lock();
            let unchanged = state
                .records
                .get(path)
                .is_some_and(|known| known.hash == record.hash);
            if unchanged && state.noted_rereads.insert(path.clone()) {
                notes.rereads.push(path.clone());
            }
            state.touch(path, record);
        }
    }
}

/// The lines of `new` around its change from `old`, numbered like `cat -n`;
/// `None` when the region is too long to be worth showing.
#[must_use]
pub fn change_snippet(old: &str, new: &str) -> Option<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let max_suffix = old_lines.len().min(new_lines.len()) - prefix;
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take(max_suffix)
        .take_while(|(left, right)| left == right)
        .count();
    let changed_end = new_lines.len() - suffix;
    if changed_end.saturating_sub(prefix) > MAX_SNIPPET_LINES {
        return None;
    }
    let start = prefix.saturating_sub(SNIPPET_CONTEXT_LINES);
    let end = (changed_end + SNIPPET_CONTEXT_LINES).min(new_lines.len());
    if start >= end {
        return Some("(the file is now empty)".to_string());
    }
    Some(
        new_lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6}\t{line}", start + offset + 1))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(reads: &[&Path], writes: &[&Path]) -> KernelFileAccess {
        KernelFileAccess {
            reads: reads.iter().map(|path| path.to_path_buf()).collect(),
            writes: writes.iter().map(|path| path.to_path_buf()).collect(),
        }
    }

    /// Rewrite `path` so its mtime or size visibly changes.
    fn rewrite(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        file.set_modified(later).unwrap();
    }

    #[test]
    fn a_file_changed_after_a_read_is_reported_once_with_its_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        let tracker = Arc::new(FileReadTracker::new());
        (tracker.sink())(&access(&[&path], &[]));
        assert_eq!(tracker.boundary(), FileNotes::default());
        rewrite(&path, "one\ntwo\nTHREE\nfour\n");
        let notes = tracker.boundary();
        assert_eq!(
            notes.changed,
            vec![ChangedFile {
                path,
                snippet: Some("     1\tone\n     2\ttwo\n     3\tTHREE\n     4\tfour".to_string()),
            }]
        );
        assert_eq!(tracker.boundary(), FileNotes::default(), "reported once");
    }

    #[test]
    fn the_models_own_write_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.py");
        std::fs::write(&path, "x = 1\n").unwrap();
        let tracker = FileReadTracker::new();
        tracker.queue(access(&[&path], &[]));
        tracker.boundary();
        rewrite(&path, "x = 2\n");
        tracker.queue(access(&[], &[&path]));
        assert_eq!(tracker.boundary(), FileNotes::default());
    }

    #[test]
    fn an_unchanged_reread_is_noted_once_and_a_changed_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::write(&path, "same\n").unwrap();
        let tracker = FileReadTracker::new();
        tracker.queue(access(&[&path], &[]));
        assert!(
            tracker.boundary().rereads.is_empty(),
            "a first read is no re-read"
        );
        tracker.queue(access(&[&path], &[]));
        assert_eq!(tracker.boundary().rereads, vec![path.clone()]);
        tracker.queue(access(&[&path], &[]));
        assert!(tracker.boundary().rereads.is_empty(), "noted once");
        rewrite(&path, "different\n");
        tracker.queue(access(&[&path], &[]));
        let notes = tracker.boundary();
        assert!(notes.rereads.is_empty());
        assert!(
            notes.changed.is_empty(),
            "the re-read already saw the new content"
        );
    }

    #[test]
    fn a_read_and_write_in_one_cell_is_no_reread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rw.txt");
        std::fs::write(&path, "a\n").unwrap();
        let tracker = FileReadTracker::new();
        tracker.queue(access(&[&path], &[]));
        tracker.boundary();
        tracker.queue(access(&[&path], &[&path]));
        assert!(tracker.boundary().rereads.is_empty());
    }

    #[test]
    fn deleted_binary_and_oversized_files_drop_silently() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("gone.txt");
        let binary = dir.path().join("blob.bin");
        std::fs::write(&text, "bye\n").unwrap();
        std::fs::write(&binary, [0_u8, 1, 2]).unwrap();
        let tracker = FileReadTracker::new();
        tracker.queue(access(&[&text, &binary], &[]));
        tracker.boundary();
        assert_eq!(tracker.tracked_count(), 1, "the binary file is not tracked");
        std::fs::remove_file(&text).unwrap();
        assert_eq!(tracker.boundary(), FileNotes::default());
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn clear_forgets_every_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\n").unwrap();
        let tracker = FileReadTracker::new();
        tracker.queue(access(&[&path], &[]));
        tracker.boundary();
        tracker.clear();
        rewrite(&path, "b\n");
        assert_eq!(tracker.boundary(), FileNotes::default());
    }

    #[test]
    fn snippets_number_the_new_lines_and_skip_long_regions() {
        assert_eq!(
            change_snippet("a\nb\nc\nd\ne\nf\n", "a\nb\nc\nX\ne\nf\n").as_deref(),
            Some("     2\tb\n     3\tc\n     4\tX\n     5\te\n     6\tf")
        );
        assert_eq!(
            change_snippet("a\nb\n", "a\nb\nc\n").as_deref(),
            Some("     1\ta\n     2\tb\n     3\tc")
        );
        assert_eq!(
            change_snippet("a\n", "").as_deref(),
            Some("(the file is now empty)")
        );
        let long = "line\n".repeat(100);
        assert_eq!(change_snippet("", &long), None);
    }
}
