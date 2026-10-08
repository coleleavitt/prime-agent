//! The file operations the machine library performs, with the failures
//! Python's `pathlib` raised for them.
//!
//! The library's sentences embed `str(error)` of the `OSError` or
//! `UnicodeDecodeError` a read or write hit (`<path>: unreadable ([Errno
//! 21] Is a directory: '<path>')`), so the errors here carry exactly that
//! text, plus the parts the kernel client needs to raise the same
//! exception class.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::super::pyvalue::py_str_repr;

/// A failed file operation, as Python raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// `OSError(errno, strerror, filename)`.
    Os {
        errno: i32,
        strerror: String,
        filename: String,
    },
    /// `UnicodeDecodeError('utf-8', data, start, end, reason)`.
    Decode {
        start: usize,
        end: usize,
        reason: &'static str,
        /// The undecodable bytes (`data[start:end]`).
        bytes: Vec<u8>,
    },
}

impl std::fmt::Display for FsError {
    /// `str(error)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os {
                errno,
                strerror,
                filename,
            } => write!(f, "[Errno {errno}] {strerror}: {}", py_str_repr(filename)),
            Self::Decode {
                start,
                end,
                reason,
                bytes,
            } => {
                if end - start == 1 {
                    write!(
                        f,
                        "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
                        bytes.first().copied().unwrap_or_default()
                    )
                } else {
                    write!(
                        f,
                        "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
                        end - 1
                    )
                }
            }
        }
    }
}

/// `strerror(errno)`: the text Rust's `io::Error` shows before its
/// ` (os error N)` suffix (both come from the C library).
fn strerror(errno: i32) -> String {
    let text = std::io::Error::from_raw_os_error(errno).to_string();
    let suffix = format!(" (os error {errno})");
    text.strip_suffix(&suffix).unwrap_or(&text).to_string()
}

/// An `io::Error` from an operation on `path`, as Python's `OSError`.
#[must_use]
pub fn os_error(error: &std::io::Error, path: &Path) -> FsError {
    let errno = error.raw_os_error().unwrap_or(0);
    FsError::Os {
        errno,
        strerror: if errno == 0 {
            error.to_string()
        } else {
            strerror(errno)
        },
        filename: path.display().to_string(),
    }
}

/// `bytes.decode("utf-8")` (strict).
///
/// # Errors
///
/// Returns the `UnicodeDecodeError` Python reports for the first
/// undecodable sequence.
pub fn decode_utf8(raw: Vec<u8>) -> Result<String, FsError> {
    String::from_utf8(raw).map_err(|error| {
        let raw = error.as_bytes();
        let utf8 = error.utf8_error();
        let start = utf8.valid_up_to();
        let (end, reason) = match utf8.error_len() {
            None => (raw.len(), "unexpected end of data"),
            Some(length) => {
                let lead = raw[start];
                let reason = if (0x80..0xc2).contains(&lead) || lead > 0xf4 {
                    "invalid start byte"
                } else {
                    "invalid continuation byte"
                };
                (start + length, reason)
            }
        };
        FsError::Decode {
            start,
            end,
            reason,
            bytes: raw[start..end].to_vec(),
        }
    })
}

/// What a text-mode write puts on disk: `\n` becomes the platform's line
/// separator.
fn text_bytes(text: &str) -> Vec<u8> {
    if cfg!(windows) {
        text.replace('\n', "\r\n").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// The kernel's working directory: every path the kernel names is
/// relative to it (the host process runs elsewhere), while errors and
/// results spell the path as the kernel named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fs {
    cwd: PathBuf,
}

impl Fs {
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    /// The process's own working directory (the CLI, the one-shot).
    #[must_use]
    pub fn here() -> Self {
        Self::new(std::env::current_dir().unwrap_or_default())
    }

    fn real(&self, path: &Path) -> PathBuf {
        self.cwd.join(path)
    }

    /// `path.is_dir()`.
    #[must_use]
    pub fn is_dir(&self, path: &Path) -> bool {
        self.real(path).is_dir()
    }

    /// `path.is_file()`.
    #[must_use]
    pub fn is_file(&self, path: &Path) -> bool {
        self.real(path).is_file()
    }

    /// `path.exists()`.
    #[must_use]
    pub fn exists(&self, path: &Path) -> bool {
        self.real(path).exists()
    }

    /// The names in a directory (`None` when it cannot be listed).
    #[must_use]
    pub fn names(&self, path: &Path) -> Option<Vec<String>> {
        let entries = std::fs::read_dir(self.real(path)).ok()?;
        Some(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
        )
    }

    /// `path.read_bytes()`.
    ///
    /// # Errors
    ///
    /// Returns the `OSError` the read raises.
    pub fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        std::fs::read(self.real(path)).map_err(|error| os_error(&error, path))
    }

    /// `path.read_text(encoding="utf-8")`: a strict decode, then universal
    /// newlines (`\r\n` and a lone `\r` read as `\n` on every platform).
    ///
    /// # Errors
    ///
    /// Returns the `OSError` or `UnicodeDecodeError` the read raises.
    pub fn read_text(&self, path: &Path) -> Result<String, FsError> {
        let text = decode_utf8(self.read_bytes(path)?)?;
        Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
    }

    /// `path.write_bytes(data)`.
    ///
    /// # Errors
    ///
    /// Returns the `OSError` the write raises.
    pub fn write_bytes(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        std::fs::write(self.real(path), data).map_err(|error| os_error(&error, path))
    }

    /// `path.write_text(text, encoding="utf-8")`.
    ///
    /// # Errors
    ///
    /// Returns the `OSError` the write raises.
    pub fn write_text(&self, path: &Path, text: &str) -> Result<(), FsError> {
        self.write_bytes(path, &text_bytes(text))
    }

    /// `open(path, "x", encoding="utf-8").write(text)`: `Ok(false)` when
    /// the path already exists (`FileExistsError`, a dangling symlink
    /// included).
    ///
    /// # Errors
    ///
    /// Returns any other `OSError` the creation or write raises.
    pub fn create_new_text(&self, path: &Path, text: &str) -> Result<bool, FsError> {
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.real(path))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(os_error(&error, path)),
        };
        file.write_all(&text_bytes(text))
            .map_err(|error| os_error(&error, path))?;
        Ok(true)
    }

    /// `path.mkdir(parents=True, exist_ok=True)`, with the filename
    /// pathlib's recursion reports on failure.
    ///
    /// # Errors
    ///
    /// Returns the `OSError` the creation raises (an existing
    /// non-directory included: `FileExistsError` for the path itself).
    pub fn mkdir_parents(&self, path: &Path) -> Result<(), FsError> {
        if path.as_os_str().is_empty() {
            // `Path(".")`: the working directory exists.
            return Ok(());
        }
        match std::fs::create_dir(self.real(path)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match path.parent().filter(|parent| *parent != path) {
                    Some(parent) => {
                        self.mkdir_parents(parent)?;
                        match std::fs::create_dir(self.real(path)) {
                            Ok(()) => Ok(()),
                            Err(_) if self.is_dir(path) => Ok(()),
                            Err(error) => Err(os_error(&error, path)),
                        }
                    }
                    None => Err(os_error(&error, path)),
                }
            }
            Err(_) if self.is_dir(path) => Ok(()),
            Err(error) => Err(os_error(&error, path)),
        }
    }
}
