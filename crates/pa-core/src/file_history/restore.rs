//! Planning and applying a restore to a point in time.

use std::io::{self, Write};
use std::path::Path;

use super::FileState;
use super::store::{Current, Op, Record, Store, read_current, sha256_hex};

/// Files past this size get approximate line stats (whole-file counts)
/// instead of a line diff.
const DIFF_STATS_LIMIT_BYTES: usize = 1024 * 1024;

/// The reason a file changed outside the agent is left alone.
pub const EXTERNALLY_MODIFIED_REASON: &str = "changed outside the agent since its last edit";

/// What a restore to one point would change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// The files whose present state differs from the point's, in the
    /// order the agent first wrote them.
    pub files: Vec<PlannedFile>,
}

impl RestorePlan {
    /// Nothing would change.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Lines the restore would add, over every restorable file.
    #[must_use]
    pub fn insertions(&self) -> u64 {
        self.files.iter().map(|file| file.insertions).sum()
    }

    /// Lines the restore would remove, over every restorable file.
    #[must_use]
    pub fn deletions(&self) -> u64 {
        self.files.iter().map(|file| file.deletions).sum()
    }
}

/// One file a restore would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub action: PlannedAction,
    /// Lines the restore adds to the present file.
    pub insertions: u64,
    /// Lines the restore removes from the present file.
    pub deletions: u64,
    /// The present content is not what the agent last wrote: the file was
    /// changed by hand, by `bash()`, or by plain Python I/O since.
    pub externally_modified: bool,
    /// The backup to restore ([`PlannedAction::Restore`] only).
    target_sha256: Option<String>,
}

/// How a planned file is restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedAction {
    /// Write the backed-up content back.
    Restore,
    /// The file did not exist at that point: delete it.
    Delete,
    /// The point's content was not kept (too large, or past the session's
    /// backup cap), or the present path is not a regular file.
    Unrestorable { reason: String },
}

/// What a restore did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Files written back to their earlier content.
    pub restored: Vec<String>,
    /// Files the agent created after the point, deleted.
    pub deleted: Vec<String>,
    /// Files left as they are, with the reason.
    pub skipped: Vec<SkippedFile>,
}

/// One file a restore left untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

pub(super) fn plan(store: &Store, at_ms: u64) -> io::Result<RestorePlan> {
    let records = store.records()?;
    let mut paths: Vec<&str> = Vec::new();
    for record in &records {
        if !paths.contains(&record.path.as_str()) {
            paths.push(&record.path);
        }
    }
    let mut files = Vec::new();
    for path in paths {
        let of_path: Vec<&Record> = records
            .iter()
            .filter(|record| record.path == path)
            .collect();
        // The point's state: the prior content of the first write at or
        // after it. A file the agent never wrote since is as it was.
        let Some(target) = of_path
            .iter()
            .find(|record| record.op == Op::Pre && record.t >= at_ms)
        else {
            continue;
        };
        if let Some(planned) = plan_file(store, path, &target.state, of_path.last().copied())? {
            files.push(planned);
        }
    }
    Ok(RestorePlan { files })
}

fn plan_file(
    store: &Store,
    path: &str,
    target: &FileState,
    last: Option<&Record>,
) -> io::Result<Option<PlannedFile>> {
    let current = match read_current(Path::new(path)) {
        Ok(current) => current,
        Err(error) => {
            return Ok(Some(PlannedFile {
                path: path.to_string(),
                action: PlannedAction::Unrestorable {
                    reason: error.to_string(),
                },
                insertions: 0,
                deletions: 0,
                externally_modified: false,
                target_sha256: None,
            }));
        }
    };
    let current_sha = match &current {
        Current::Content(bytes) => Some(sha256_hex(bytes)),
        Current::Absent | Current::TooLarge(_) => None,
    };
    let unchanged = match (target, &current) {
        (FileState::Absent, Current::Absent) => true,
        (FileState::Content { sha256, .. }, Current::Content(_)) => {
            current_sha.as_deref() == Some(sha256.as_str())
        }
        _ => false,
    };
    if unchanged {
        return Ok(None);
    }
    // The agent's last word on the file: a completed write's content. A
    // write whose completion was never reported proves nothing.
    let externally_modified = last.is_some_and(|last| {
        last.op == Op::Post
            && match (&last.state, &current) {
                (FileState::Absent, Current::Absent) => false,
                (FileState::Content { sha256, .. }, Current::Content(_)) => {
                    current_sha.as_deref() != Some(sha256.as_str())
                }
                (FileState::TooLarge { size }, Current::TooLarge(current)) => size != current,
                _ => true,
            }
    });
    let current_bytes: &[u8] = match &current {
        Current::Content(bytes) => bytes,
        Current::Absent | Current::TooLarge(_) => &[],
    };
    let planned = match target {
        FileState::Absent => PlannedFile {
            path: path.to_string(),
            action: PlannedAction::Delete,
            insertions: 0,
            deletions: count_lines(current_bytes),
            externally_modified,
            target_sha256: None,
        },
        FileState::Content {
            sha256,
            stored: true,
            ..
        } => {
            let (insertions, deletions) = match store.read_blob(sha256) {
                Ok(target_bytes) => line_stats(current_bytes, &target_bytes),
                Err(error) => {
                    return Ok(Some(PlannedFile {
                        path: path.to_string(),
                        action: PlannedAction::Unrestorable {
                            reason: format!("backup unreadable: {error}"),
                        },
                        insertions: 0,
                        deletions: 0,
                        externally_modified,
                        target_sha256: None,
                    }));
                }
            };
            PlannedFile {
                path: path.to_string(),
                action: PlannedAction::Restore,
                insertions,
                deletions,
                externally_modified,
                target_sha256: Some(sha256.clone()),
            }
        }
        FileState::Content { stored: false, .. } => PlannedFile {
            path: path.to_string(),
            action: PlannedAction::Unrestorable {
                reason: "its backup was not kept (the session's checkpoint storage was full)"
                    .to_string(),
            },
            insertions: 0,
            deletions: 0,
            externally_modified,
            target_sha256: None,
        },
        FileState::TooLarge { .. } => PlannedFile {
            path: path.to_string(),
            action: PlannedAction::Unrestorable {
                reason: "it was too large to back up".to_string(),
            },
            insertions: 0,
            deletions: 0,
            externally_modified,
            target_sha256: None,
        },
    };
    Ok(Some(planned))
}

pub(super) fn apply(
    store: &Store,
    at_ms: u64,
    force: bool,
    now_ms: u64,
) -> io::Result<RestoreReport> {
    let mut report = RestoreReport::default();
    for file in plan(store, at_ms)?.files {
        let skip = |reason: String| SkippedFile {
            path: file.path.clone(),
            reason,
        };
        if let PlannedAction::Unrestorable { reason } = &file.action {
            report.skipped.push(skip(reason.clone()));
            continue;
        }
        if file.externally_modified && !force {
            report
                .skipped
                .push(skip(EXTERNALLY_MODIFIED_REASON.to_string()));
            continue;
        }
        let path = Path::new(&file.path);
        if let Err(reason) = check_destination(path) {
            report.skipped.push(skip(reason));
            continue;
        }
        let outcome = match (&file.action, &file.target_sha256) {
            (PlannedAction::Restore, Some(sha256)) => store
                .read_blob(sha256)
                .map_err(|error| format!("backup unreadable: {error}"))
                .and_then(|bytes| {
                    // The restore is a write too: its prior content is kept,
                    // so a later rewind sees through it.
                    let _ = store.record_current(path, Op::Pre, now_ms);
                    write_file(path, &bytes).map_err(|error| error.to_string())
                }),
            (PlannedAction::Delete, _) => {
                let _ = store.record_current(path, Op::Pre, now_ms);
                match std::fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error.to_string()),
                }
            }
            (PlannedAction::Restore, None) | (PlannedAction::Unrestorable { .. }, _) => {
                Err("no backup to restore".to_string())
            }
        };
        match outcome {
            Ok(()) => {
                let _ = store.record_current(path, Op::Post, now_ms);
                match file.action {
                    PlannedAction::Delete => report.deleted.push(file.path.clone()),
                    PlannedAction::Restore | PlannedAction::Unrestorable { .. } => {
                        report.restored.push(file.path.clone());
                    }
                }
            }
            Err(reason) => report.skipped.push(skip(reason)),
        }
    }
    Ok(report)
}

/// A restore only ever replaces a regular file in place or recreates a
/// missing one: a path that became a symlink or anything else is refused.
fn check_destination(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err("the path is now a symlink".to_string())
        }
        Ok(metadata) if !metadata.is_file() => Err("the path is not a regular file".to_string()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Write `bytes` over the file in place (its permissions and links stay),
/// or create it, with its directory, when missing.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
        }
        Err(error) => return Err(error),
    };
    file.write_all(bytes)?;
    file.flush()
}

/// Lines added and removed turning `current` into `target`.
fn line_stats(current: &[u8], target: &[u8]) -> (u64, u64) {
    if current.len() > DIFF_STATS_LIMIT_BYTES || target.len() > DIFF_STATS_LIMIT_BYTES {
        return (count_lines(target), count_lines(current));
    }
    let current = String::from_utf8_lossy(current);
    let target = String::from_utf8_lossy(target);
    let mut insertions = 0;
    let mut deletions = 0;
    for part in crate::tools::jsdiff::diff_lines(&current, &target) {
        if part.added {
            insertions += part.count as u64;
        } else if part.removed {
            deletions += part.count as u64;
        }
    }
    (insertions, deletions)
}

fn count_lines(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count() as u64;
    if bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}
