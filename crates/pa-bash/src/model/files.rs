//! Paths and the script files the model reads.

use std::path::{Component, Path, PathBuf};

/// Script files larger than this are not read (their code stays opaque).
const MAX_SCRIPT_BYTES: u64 = 256 * 1024;

/// `text` taken relative to `base`, with `.` and `..` folded lexically
/// (what `cd` does with its logical path).
pub(crate) fn join(base: &Path, text: &str) -> PathBuf {
    let joined = if text.starts_with('/') {
        PathBuf::from(text)
    } else {
        base.join(text)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

/// The text of the script at `path`, wherever it lives, when it is a
/// readable regular file (symlinks resolved) small enough to read. Code the
/// guard can read is judged, not trusted.
pub(crate) fn read_script(path: &Path) -> Option<String> {
    let real = path.canonicalize().ok()?;
    let metadata = std::fs::metadata(&real).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SCRIPT_BYTES {
        return None;
    }
    let bytes = std::fs::read(&real).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Glob matches past this are not enumerated.
const MAX_MATCHES: usize = 4096;

/// The paths the shell pattern `pattern` matches from `dir` (backslash
/// escapes literal; a leading dot must be matched explicitly), or `None`
/// past the match bound.
pub(crate) fn expand(dir: &Path, pattern: &str) -> Option<Vec<PathBuf>> {
    let absolute = pattern.starts_with('/');
    let mut current = vec![if absolute {
        PathBuf::from("/")
    } else {
        dir.to_path_buf()
    }];
    for component in pattern.split('/').filter(|component| !component.is_empty()) {
        let wild = component.contains(['*', '?', '[']);
        let mut next = Vec::new();
        for base in &current {
            if !wild {
                next.push(base.join(unescape(component)));
                continue;
            }
            let Ok(entries) = std::fs::read_dir(base) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.') || component.starts_with('.'))
                .filter(|name| super::value::pattern_matches(component, name))
                .collect();
            names.sort();
            for name in names {
                next.push(base.join(name));
                if next.len() > MAX_MATCHES {
                    return None;
                }
            }
        }
        current = next;
    }
    current.retain(|path| path.symlink_metadata().is_ok());
    Some(current)
}

/// A pattern's text with its escapes removed (what the shell keeps when
/// nothing matches).
pub(crate) fn unescape(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}
