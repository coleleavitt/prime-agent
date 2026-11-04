//! Local file-backed memory store: all memory lives under one directory,
//! files are plain UTF-8, no index file. Writes are atomic (temp + rename);
//! deletes remove empty parent directories up to the root. No locking: a
//! single session serves a single kernel at a time, so there is no concurrent
//! writer in practice.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{MAX_CONTENT_BYTES, MemoryError, MemoryPath};

/// The root directory of a store.
#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
}

/// A hit from [`LocalStore::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

/// An entry from [`LocalStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub path: String,
    pub size: u64,
    /// Seconds since the Unix epoch; absent when the OS did not supply it.
    pub modified_unix: Option<u64>,
}

impl LocalStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read the file at `path`.
    ///
    /// # Errors
    /// Returns [`MemoryError::NotFound`] when `path` does not exist and
    /// [`MemoryError::Io`] when the file cannot be read.
    pub fn read(&self, path: &MemoryPath) -> Result<String, MemoryError> {
        let resolved = path.resolve(&self.root);
        match fs::read_to_string(&resolved) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(MemoryError::NotFound {
                path: path.to_string(),
            }),
            Err(error) => Err(MemoryError::Io(error)),
        }
    }

    /// Write `content` to `path`, replacing any existing file.
    ///
    /// # Errors
    /// Returns [`MemoryError::ContentTooLarge`] when `content` exceeds
    /// [`crate::memory::MAX_CONTENT_BYTES`], and [`MemoryError::Io`] when
    /// the write fails.
    pub fn write(&self, path: &MemoryPath, content: &str) -> Result<(), MemoryError> {
        check_content_size(content)?;
        let resolved = path.resolve(&self.root);
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&resolved, content.as_bytes())?;
        Ok(())
    }

    /// Append `content` to `path`, creating the file if needed.
    ///
    /// # Errors
    /// Returns [`MemoryError::ContentTooLarge`] when the resulting file
    /// would exceed [`crate::memory::MAX_CONTENT_BYTES`], and
    /// [`MemoryError::Io`] when the write fails.
    pub fn append(&self, path: &MemoryPath, content: &str) -> Result<(), MemoryError> {
        let resolved = path.resolve(&self.root);
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = match fs::metadata(&resolved) {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(MemoryError::Io(error)),
        };
        let total = existing as usize + content.len();
        if total > MAX_CONTENT_BYTES {
            return Err(MemoryError::ContentTooLarge { size: total });
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    /// Replace the single occurrence of `old` with `new` in `path`.
    ///
    /// # Errors
    /// Returns [`MemoryError::NotFound`] when `path` does not exist,
    /// [`MemoryError::ReplaceNotUnique`] when `old` does not match
    /// exactly once in the file, [`MemoryError::ContentTooLarge`] when
    /// the replacement would push the file past the content cap, and
    /// [`MemoryError::Io`] when the write fails.
    pub fn str_replace(&self, path: &MemoryPath, old: &str, new: &str) -> Result<(), MemoryError> {
        let current = self.read(path)?;
        let occurrences = count_substring(&current, old);
        if occurrences != 1 {
            return Err(MemoryError::ReplaceNotUnique {
                path: path.to_string(),
                occurrences,
            });
        }
        let updated = current.replacen(old, new, 1);
        check_content_size(&updated)?;
        let resolved = path.resolve(&self.root);
        atomic_write(&resolved, updated.as_bytes())?;
        Ok(())
    }

    /// Delete `path`, and the enclosing directories once they are empty.
    ///
    /// # Errors
    /// Returns [`MemoryError::NotFound`] when `path` does not exist and
    /// [`MemoryError::Io`] when the removal fails.
    pub fn delete(&self, path: &MemoryPath) -> Result<(), MemoryError> {
        let resolved = path.resolve(&self.root);
        match fs::remove_file(&resolved) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(MemoryError::NotFound {
                    path: path.to_string(),
                });
            }
            Err(error) => return Err(MemoryError::Io(error)),
        }
        prune_empty_parents(&resolved, &self.root);
        Ok(())
    }

    /// Every file at or below `prefix`, sorted by path. An empty prefix
    /// lists the whole tree; a missing directory lists nothing.
    ///
    /// # Errors
    /// Returns [`MemoryError::Io`] when a directory under the tree cannot
    /// be read.
    pub fn list(&self, prefix: Option<&MemoryPath>) -> Result<Vec<ListEntry>, MemoryError> {
        let base = prefix.map_or(self.root.clone(), |path| path.resolve(&self.root));
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        walk_files(&base, &self.root, &mut out)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Lines containing `query` across every file, sorted by path then line
    /// number. Case-sensitive, literal (no regex), capped at `limit` hits.
    ///
    /// # Errors
    /// Returns [`MemoryError::Io`] when the tree cannot be walked or a
    /// file cannot be read.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, MemoryError> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits = Vec::new();
        if !self.root.exists() {
            return Ok(hits);
        }
        search_tree(&self.root, &self.root, query, limit, &mut hits)?;
        hits.sort_by(|a, b| (&a.path, a.line_number).cmp(&(&b.path, b.line_number)));
        Ok(hits)
    }
}

fn check_content_size(content: &str) -> Result<(), MemoryError> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(MemoryError::ContentTooLarge {
            size: content.len(),
        });
    }
    Ok(())
}

fn count_substring(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut cursor = 0;
    while let Some(offset) = haystack[cursor..].find(needle) {
        count += 1;
        cursor += offset + needle.len();
    }
    count
}

/// Write `bytes` through a sibling temp file and rename on top of `path`.
/// Not a strict atomic contract (no `fsync` on the parent directory): the
/// memory tree is scratchpad state, so the usual filesystem semantics are
/// enough, and we avoid pulling in a crate for one call.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "memory write: empty file name")
    })?;
    let temp = parent.join(format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let mut handle = fs::File::create(&temp)?;
    handle.write_all(bytes)?;
    handle.sync_data()?;
    drop(handle);
    fs::rename(&temp, path)
}

fn prune_empty_parents(file: &Path, root: &Path) {
    let mut cursor = file.parent();
    while let Some(dir) = cursor {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cursor = dir.parent();
    }
}

fn walk_files(dir: &Path, root: &Path, out: &mut Vec<ListEntry>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_files(&path, root, out)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .ok()
                .and_then(relative_posix)
                .unwrap_or_default();
            let metadata = entry.metadata()?;
            let modified_unix = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            out.push(ListEntry {
                path: relative,
                size: metadata.len(),
                modified_unix,
            });
        }
    }
    Ok(())
}

fn relative_posix(path: &Path) -> Option<String> {
    let mut out = String::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(name.to_str()?);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn search_tree(
    dir: &Path,
    root: &Path,
    query: &str,
    limit: usize,
    hits: &mut Vec<SearchHit>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        if hits.len() >= limit {
            return Ok(());
        }
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            search_tree(&path, root, query, limit, hits)?;
        } else if file_type.is_file() {
            search_file(&path, root, query, limit, hits)?;
        }
    }
    Ok(())
}

fn search_file(
    file: &Path,
    root: &Path,
    query: &str,
    limit: usize,
    hits: &mut Vec<SearchHit>,
) -> io::Result<()> {
    let mut handle = fs::File::open(file)?;
    let metadata = handle.metadata()?;
    // Skip huge files; memory content is capped at 100 KiB anyway, so
    // anything well over that is a foreign file in the tree.
    if metadata.len() as usize > MAX_CONTENT_BYTES * 4 {
        return Ok(());
    }
    handle.seek(SeekFrom::Start(0))?;
    let mut text = String::with_capacity(metadata.len() as usize);
    handle.read_to_string(&mut text)?;
    let relative = file
        .strip_prefix(root)
        .ok()
        .and_then(relative_posix)
        .unwrap_or_default();
    for (index, line) in text.lines().enumerate() {
        if hits.len() >= limit {
            return Ok(());
        }
        if line.contains(query) {
            hits.push(SearchHit {
                path: relative.clone(),
                line_number: index + 1,
                line: line.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, LocalStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LocalStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    #[test]
    fn write_then_read_round_trips() {
        let (_root, store) = store();
        let path = MemoryPath::parse("notes/hello.md").unwrap();
        store.write(&path, "hi\n").unwrap();
        assert_eq!(store.read(&path).unwrap(), "hi\n");
    }

    #[test]
    fn append_concatenates() {
        let (_root, store) = store();
        let path = MemoryPath::parse("log.txt").unwrap();
        store.append(&path, "one\n").unwrap();
        store.append(&path, "two\n").unwrap();
        assert_eq!(store.read(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn str_replace_requires_unique_match() {
        let (_root, store) = store();
        let path = MemoryPath::parse("n.md").unwrap();
        store.write(&path, "cat dog cat").unwrap();
        assert!(matches!(
            store.str_replace(&path, "cat", "fox"),
            Err(MemoryError::ReplaceNotUnique { occurrences: 2, .. })
        ));
        store.write(&path, "cat dog").unwrap();
        store.str_replace(&path, "cat", "fox").unwrap();
        assert_eq!(store.read(&path).unwrap(), "fox dog");
    }

    #[test]
    fn read_of_missing_file_errors_as_not_found() {
        let (_root, store) = store();
        let path = MemoryPath::parse("absent.md").unwrap();
        let error = store.read(&path).unwrap_err();
        assert_eq!(error.code(), "not_found");
    }

    #[test]
    fn content_cap_refuses_write_and_append() {
        let (_root, store) = store();
        let path = MemoryPath::parse("big.md").unwrap();
        let huge = "x".repeat(MAX_CONTENT_BYTES + 1);
        let error = store.write(&path, &huge).unwrap_err();
        assert_eq!(error.code(), "content_too_large");
        store
            .write(&path, &"x".repeat(MAX_CONTENT_BYTES - 1))
            .unwrap();
        let error = store.append(&path, "xx").unwrap_err();
        assert_eq!(error.code(), "content_too_large");
    }

    #[test]
    fn delete_removes_file_and_empty_parents() {
        let (root, store) = store();
        let path = MemoryPath::parse("deep/nested/file.md").unwrap();
        store.write(&path, "hi").unwrap();
        store.delete(&path).unwrap();
        assert!(!root.path().join("deep").exists());
    }

    #[test]
    fn list_returns_sorted_entries_under_prefix() {
        let (_root, store) = store();
        store
            .write(&MemoryPath::parse("a/x.md").unwrap(), "x")
            .unwrap();
        store
            .write(&MemoryPath::parse("a/y.md").unwrap(), "y")
            .unwrap();
        store
            .write(&MemoryPath::parse("b/z.md").unwrap(), "z")
            .unwrap();
        let all = store.list(None).unwrap();
        assert_eq!(
            all.iter().map(|entry| &entry.path).collect::<Vec<_>>(),
            vec!["a/x.md", "a/y.md", "b/z.md"],
        );
        let under_a = store.list(Some(&MemoryPath::parse("a").unwrap())).unwrap();
        assert_eq!(under_a.len(), 2);
    }

    #[test]
    fn search_finds_lines_across_files() {
        let (_root, store) = store();
        store
            .write(
                &MemoryPath::parse("one.md").unwrap(),
                "line one\nmatch here\n",
            )
            .unwrap();
        store
            .write(
                &MemoryPath::parse("two.md").unwrap(),
                "no hit\nalso a match\n",
            )
            .unwrap();
        let hits = store.search("match", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "one.md");
        assert_eq!(hits[0].line_number, 2);
    }
}
