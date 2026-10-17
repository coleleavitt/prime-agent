//! A command's output persisted to a file (Claude Code's background task
//! output files): the table's output directory gets `<job id>.output`, written
//! as the pump commits each chunk, so the model can read a long or detached
//! command's whole output by path after the in-memory buffer dropped its
//! middle. Private to the user (directory 0700, file 0600 on Unix) and capped:
//! past [`OUTPUT_FILE_CAP`] bytes one marker line ends the file.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The most command output bytes one file keeps.
pub(crate) const OUTPUT_FILE_CAP: u64 = 64 * 1024 * 1024;

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
