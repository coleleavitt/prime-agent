//! A command's output persisted to a file (Claude Code's background task
//! output files): the table's output directory gets `<job id>.output`, written
//! as the pump commits each chunk, so the model can read a long or detached
//! command's whole output by path after the in-memory buffer dropped its
//! middle. Private to the user (directory 0700, file 0600 on Unix) and capped:
//! past [`OUTPUT_FILE_CAP`] bytes one marker line ends the file. The directory
//! is pruned as files are added ([`prune_dir`]): the oldest files of finished
//! commands go once more than [`MAX_OUTPUT_FILES`] files or
//! [`MAX_OUTPUT_DIR_BYTES`] bytes are kept.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The most command output bytes one file keeps.
pub(crate) const OUTPUT_FILE_CAP: u64 = 64 * 1024 * 1024;
/// The most output files one directory keeps.
pub(crate) const MAX_OUTPUT_FILES: usize = 200;
/// The most output bytes one directory keeps.
pub(crate) const MAX_OUTPUT_DIR_BYTES: u64 = 512 * 1024 * 1024;

/// Delete the oldest `<id>.output` files in `dir` (by modification time)
/// until at most `max_files` files and `max_bytes` bytes remain. A file whose
/// id is in `live` (a command still running) is never deleted, and other
/// files in the directory are never touched. Best effort: an unreadable
/// entry is skipped.
pub(crate) fn prune_dir(
    dir: &Path,
    live: &std::collections::HashSet<String>,
    max_files: usize,
    max_bytes: u64,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf, bool)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let id = path
                .file_name()?
                .to_str()?
                .strip_suffix(".output")?
                .to_string();
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    meta.len(),
                    path,
                    live.contains(&id),
                )
            })
        })
        .collect();
    // Newest first: the kept ones are a prefix.
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    let (mut count, mut bytes) = (0usize, 0u64);
    for (_, len, path, is_live) in files {
        if is_live || (count < max_files && bytes.saturating_add(len) <= max_bytes) {
            count += 1;
            bytes = bytes.saturating_add(len);
            continue;
        }
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug)]
pub(crate) struct OutputFile {
    file: File,
    written: u64,
    /// The cap was reached (or a write failed): nothing more is written.
    closed: bool,
}

impl OutputFile {
    /// Create `<dir>/<id>.output` (and `dir`), or `None` when the file cannot
    /// be made: the command still runs, only without its file.
    pub(crate) fn create(dir: &Path, id: &str) -> Option<(PathBuf, OutputFile)> {
        create_private_dir(dir).ok()?;
        let path = dir.join(format!("{id}.output"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).ok()?;
        Some((
            path,
            OutputFile {
                file,
                written: 0,
                closed: false,
            },
        ))
    }

    /// Append a committed chunk, up to the cap.
    pub(crate) fn write(&mut self, chunk: &[u8]) {
        if self.closed {
            return;
        }
        let room = OUTPUT_FILE_CAP.saturating_sub(self.written);
        let take = usize::try_from(room).unwrap_or(usize::MAX).min(chunk.len());
        if self.file.write_all(&chunk[..take]).is_err() {
            self.closed = true;
            return;
        }
        self.written += take as u64;
        if take < chunk.len() {
            self.closed = true;
            let _ = write!(
                self.file,
                "\n... [output file capped at {OUTPUT_FILE_CAP} bytes; later output was not \
                 saved] ...\n"
            );
        }
    }
}

/// `dir` and its missing parents, the leaf private to the user.
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
    fn the_file_holds_the_output_up_to_the_cap() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("tasks");
        let (path, mut file) = OutputFile::create(&dir, "abc").unwrap();
        assert_eq!(path, dir.join("abc.output"));
        file.write(b"hello ");
        file.write(b"world\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world\n");
        // A second job with the same id never clobbers the first file.
        assert!(OutputFile::create(&dir, "abc").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
    }

    #[test]
    fn pruning_drops_the_oldest_finished_files_past_the_caps() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        for (index, id) in ["a", "b", "c", "d"].iter().enumerate() {
            let path = dir.join(format!("{id}.output"));
            std::fs::write(&path, "xxxx").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(base + std::time::Duration::from_secs(index as u64))
                .unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "keep").unwrap();
        // "a" is the oldest but still running: it stays and counts toward
        // the cap, so of the finished ones only the newest two survive
        // (three files, one over the cap of two, never a live one).
        let live: std::collections::HashSet<String> = ["a".to_string()].into();
        prune_dir(dir, &live, 2, u64::MAX);
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left, ["a.output", "c.output", "d.output", "notes.txt"]);
        // The byte cap: one 4-byte file fits in 6 bytes besides the live one.
        prune_dir(dir, &std::collections::HashSet::new(), 10, 6);
        assert!(dir.join("d.output").exists());
        assert!(!dir.join("c.output").exists());
        assert!(!dir.join("a.output").exists());
    }

    #[test]
    fn past_the_cap_one_marker_ends_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let (path, mut file) = OutputFile::create(temp.path(), "big").unwrap();
        file.written = OUTPUT_FILE_CAP - 3;
        file.write(b"abcdef");
        file.write(b"never");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("abc\n... [output file capped at"),
            "{text}"
        );
        assert!(!text.contains("never"));
    }
}
