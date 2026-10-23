//! The on-disk history: the append-only write log and the content-addressed
//! backups, every leaf operation pinned to an open directory handle.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{FileState, MAX_BACKUP_BYTES, MAX_HISTORY_BYTES};
use crate::platform::private_fs;

/// The write log's file name inside the history directory.
pub(super) const LOG_FILE: &str = "log.jsonl";

/// The backups' directory name inside the history directory.
const BLOBS_DIR: &str = "blobs";

/// Serializes every history operation in the process: the kernel's write
/// reports and a client's restore never interleave their log appends.
static LOCK: Mutex<()> = Mutex::new(());

pub(super) fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Which side of a write a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Op {
    /// The file's state before a write replaced it (backed up).
    Pre,
    /// The file's state after the write landed (hash only).
    Post,
}

/// One log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Record {
    /// Milliseconds since the Unix epoch.
    pub t: u64,
    pub op: Op,
    pub path: String,
    pub state: FileState,
}

/// An opened history: the pinned history and backup directories.
pub(super) struct Store {
    dir: File,
    blobs: File,
}

impl Store {
    /// Open the history at `dir`; `create` makes the private directories
    /// when missing, otherwise a missing history answers `NotFound`.
    pub(super) fn open(dir: &Path, create: bool) -> io::Result<Self> {
        if create {
            crate::platform::perms::create_dir_all_private(dir)?;
        }
        let dir = private_fs::open_dir_no_follow(dir)?;
        let blobs = match private_fs::open_dir_no_follow_at(&dir, OsStr::new(BLOBS_DIR)) {
            Ok(blobs) => blobs,
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                match private_fs::create_dir_private_at(&dir, OsStr::new(BLOBS_DIR)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                private_fs::open_dir_no_follow_at(&dir, OsStr::new(BLOBS_DIR))?
            }
            Err(error) => return Err(error),
        };
        Ok(Self { dir, blobs })
    }

    /// Every well-formed record, in log order (a torn or foreign line is
    /// skipped).
    pub(super) fn records(&self) -> io::Result<Vec<Record>> {
        let mut file = match private_fs::open_read_at(&self.dir, LOG_FILE) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str::<Record>(line).ok())
            .collect())
    }

    fn append(&self, record: &Record) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        // One write per record: an O_APPEND write lands whole.
        private_fs::open_append_at(&self.dir, LOG_FILE)?.write_all(&line)
    }

    /// Record `path`'s current state as one side of a write; a pre-write
    /// state backs the bytes up (within the caps).
    pub(super) fn record_current(&self, path: &Path, op: Op, now_ms: u64) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "file history needs an absolute path, got {}",
                    path.display()
                ),
            ));
        }
        let state = match read_current(path)? {
            Current::Absent => FileState::Absent,
            Current::TooLarge(size) => FileState::TooLarge { size },
            Current::Content(bytes) => {
                let sha256 = sha256_hex(&bytes);
                let stored = op == Op::Pre && self.store_blob(&sha256, &bytes)?;
                FileState::Content {
                    sha256,
                    size: bytes.len() as u64,
                    stored,
                }
            }
        };
        self.append(&Record {
            t: now_ms,
            op,
            path: path.to_string_lossy().into_owned(),
            state,
        })
    }

    /// Keep `bytes` as backup `sha256` unless the session's backups would
    /// pass [`MAX_HISTORY_BYTES`]; `true` when the backup exists afterwards.
    fn store_blob(&self, sha256: &str, bytes: &[u8]) -> io::Result<bool> {
        if private_fs::open_read_at(&self.blobs, sha256).is_ok() {
            return Ok(true);
        }
        if self.stored_bytes()? + bytes.len() as u64 > MAX_HISTORY_BYTES {
            return Ok(false);
        }
        let temp = format!(".{sha256}.{}.tmp", std::process::id());
        let mut file = private_fs::create_replace_at(&self.blobs, &temp)?;
        file.write_all(bytes)?;
        drop(file);
        private_fs::rename_at(&self.blobs, &temp, sha256)?;
        Ok(true)
    }

    /// The bytes of the distinct backups the log references.
    fn stored_bytes(&self) -> io::Result<u64> {
        let mut seen = std::collections::HashSet::new();
        Ok(self
            .records()?
            .into_iter()
            .filter_map(|record| match record.state {
                FileState::Content {
                    sha256,
                    size,
                    stored: true,
                } => seen.insert(sha256).then_some(size),
                FileState::Content { .. } | FileState::Absent | FileState::TooLarge { .. } => None,
            })
            .sum())
    }

    /// Read backup `sha256`, verifying its content against its name.
    pub(super) fn read_blob(&self, sha256: &str) -> io::Result<Vec<u8>> {
        if !is_sha256_hex(sha256) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed backup name",
            ));
        }
        let mut bytes = Vec::new();
        private_fs::open_read_at(&self.blobs, sha256)?.read_to_end(&mut bytes)?;
        if sha256_hex(&bytes) != sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backup content does not match its hash",
            ));
        }
        Ok(bytes)
    }
}

/// A path's present content, as the history reads it.
pub(super) enum Current {
    Absent,
    TooLarge(u64),
    Content(Vec<u8>),
}

/// Read `path` for the history: a missing path is [`Current::Absent`]; a
/// path that is not a regular file (a directory, a FIFO whose read would
/// block) is refused.
pub(super) fn read_current(path: &Path) -> io::Result<Current> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Current::Absent),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_BACKUP_BYTES {
        return Ok(Current::TooLarge(metadata.len()));
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Current::Absent),
        Err(error) => return Err(error),
    };
    if bytes.len() as u64 > MAX_BACKUP_BYTES {
        return Ok(Current::TooLarge(bytes.len() as u64));
    }
    Ok(Current::Content(bytes))
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256_hex(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
