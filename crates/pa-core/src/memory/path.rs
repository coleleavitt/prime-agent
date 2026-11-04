//! Memory paths: logical POSIX-style paths rooted at the personal memory
//! directory. Parsing follows Claude Code's own rules so the model's habits
//! transfer: NFC-normalized, forward slashes only, no `.` / `..`, no control
//! characters, no leading or trailing slash, at most [`MAX_PATH_SEGMENTS`]
//! segments, at most [`MAX_PATH_BYTES`] bytes after normalization.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Segment cap. Matches CC's `20`.
pub const MAX_PATH_SEGMENTS: usize = 20;
/// Byte cap on the normalized path. Matches CC's `1024`.
pub const MAX_PATH_BYTES: usize = 1024;

/// A validated memory path. Always relative; always safe to join onto the
/// personal memory root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoryPath {
    normalized: String,
}

/// Why a memory path was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path is absolute; memory paths are relative")]
    Absolute,
    #[error("path contains `.` or `..` segments")]
    DotSegment,
    #[error("path contains control characters or NUL bytes")]
    ControlChar,
    #[error("path contains an empty segment (double slash or trailing slash)")]
    EmptySegment,
    #[error("path uses a backslash; memory paths use forward slashes only")]
    Backslash,
    #[error("path has {segments} segments; the cap is {MAX_PATH_SEGMENTS}")]
    TooManySegments { segments: usize },
    #[error("path is {bytes} bytes after NFC; the cap is {MAX_PATH_BYTES}")]
    TooLong { bytes: usize },
}

impl MemoryPath {
    /// Parse a user-facing path string into a validated memory path.
    ///
    /// # Errors
    /// Returns a [`PathError`] variant naming why the path was rejected
    /// (empty, absolute, dot segment, backslash, control character,
    /// empty segment, over the segment or byte cap).
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.contains('\\') {
            return Err(PathError::Backslash);
        }
        if raw.starts_with('/') {
            return Err(PathError::Absolute);
        }
        let normalized: String = raw.nfc().collect();
        if normalized.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong {
                bytes: normalized.len(),
            });
        }
        if normalized.bytes().any(|byte| byte == 0 || byte < 0x20) {
            return Err(PathError::ControlChar);
        }
        let segments: Vec<&str> = normalized.split('/').collect();
        if segments.len() > MAX_PATH_SEGMENTS {
            return Err(PathError::TooManySegments {
                segments: segments.len(),
            });
        }
        for segment in &segments {
            if segment.is_empty() {
                return Err(PathError::EmptySegment);
            }
            if *segment == "." || *segment == ".." {
                return Err(PathError::DotSegment);
            }
        }
        Ok(Self { normalized })
    }

    /// The validated POSIX-style path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.normalized
    }

    /// Join onto a filesystem root, segment by segment. The result stays
    /// inside `root` because the parse rules forbid `.`, `..`, absolute
    /// prefixes and empty segments.
    #[must_use]
    pub fn resolve(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        for segment in self.normalized.split('/') {
            path.push(segment);
        }
        path
    }
}

impl std::fmt::Display for MemoryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_paths_parse() {
        for path in [
            "notes.md",
            "notes/python/pandas.md",
            "日本語/メモ.md",
            "one/two/three",
        ] {
            MemoryPath::parse(path).unwrap_or_else(|error| panic!("{path}: {error}"));
        }
    }

    #[test]
    fn rejects_shape_hazards() {
        for (path, expected) in [
            ("", PathError::Empty),
            ("/abs", PathError::Absolute),
            ("one//two", PathError::EmptySegment),
            ("one/", PathError::EmptySegment),
            ("one/./two", PathError::DotSegment),
            ("one/../two", PathError::DotSegment),
            ("one\\two", PathError::Backslash),
            ("one/\u{0001}/two", PathError::ControlChar),
            ("one\0two", PathError::ControlChar),
        ] {
            assert_eq!(MemoryPath::parse(path), Err(expected), "{path}");
        }
    }

    #[test]
    fn enforces_segment_and_byte_caps() {
        let twenty_one = (0..=MAX_PATH_SEGMENTS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("/");
        assert!(matches!(
            MemoryPath::parse(&twenty_one),
            Err(PathError::TooManySegments { .. })
        ));
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert!(matches!(
            MemoryPath::parse(&long),
            Err(PathError::TooLong { .. })
        ));
    }

    #[test]
    fn nfc_normalizes_combining_forms() {
        // "é" as U+00E9 (NFC) vs "e" + U+0301 (NFD) should land on the same bytes.
        let composed = MemoryPath::parse("caf\u{00E9}.md").unwrap();
        let decomposed = MemoryPath::parse("cafe\u{0301}.md").unwrap();
        assert_eq!(composed, decomposed);
    }

    #[test]
    fn resolve_stays_under_root() {
        let path = MemoryPath::parse("notes/deep/file.md").unwrap();
        let resolved = path.resolve(Path::new("/root"));
        assert_eq!(resolved, PathBuf::from("/root/notes/deep/file.md"));
    }
}
