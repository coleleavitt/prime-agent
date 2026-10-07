//! The path and character semantics the guard's decisions are defined in:
//! POSIX `os.path` (`join`, `split`, `normpath`, `realpath`, `expanduser`)
//! and Python's `str.isspace` / regex `\w`, so operand resolution reaches the
//! same paths and word boundaries fall in the same places.

use std::path::{Path, PathBuf};

use crate::context::GuardContext;

/// `str.isspace`: Unicode whitespace plus the ASCII information separators.
pub(super) fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// A regex `\w` character.
pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `str.strip()`.
pub(super) fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `os.path.basename`.
pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `os.path.isabs`.
pub(super) fn is_abs(path: &str) -> bool {
    path.starts_with('/')
}

/// `os.path.join(a, b)`.
pub(super) fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `os.path.split`.
fn split(path: &str) -> (String, String) {
    let cut = path.rfind('/').map_or(0, |index| index + 1);
    let (head, tail) = path.split_at(cut);
    let head = if !head.is_empty() && head.chars().any(|c| c != '/') {
        head.trim_end_matches('/')
    } else {
        head
    };
    (head.to_string(), tail.to_string())
}

/// `os.path.normpath`.
pub(super) fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(component);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Resolve a possibly relative path against the kernel's working directory
/// for a filesystem call (the kernel process's own cwd is the context's).
fn on_disk(context: &GuardContext, path: &str) -> PathBuf {
    if is_abs(path) {
        PathBuf::from(path)
    } else {
        context.cwd().join(path)
    }
}

/// `os.path.realpath` (non-strict): symlinks resolved as far as they exist,
/// missing components kept, relative input taken from the kernel's cwd.
/// `None` when a link cannot be read (the kernel's callers treat that
/// `OSError` as unresolvable).
pub(super) fn realpath(context: &GuardContext, path: &str) -> Option<String> {
    let mut seen = std::collections::HashMap::new();
    let (resolved, _) = join_realpath(context, String::new(), path, &mut seen)?;
    let absolute = if is_abs(&resolved) {
        resolved
    } else {
        join(&context.cwd().display().to_string(), &resolved)
    };
    Some(normpath(&absolute))
}

fn join_realpath(
    context: &GuardContext,
    mut path: String,
    rest: &str,
    seen: &mut std::collections::HashMap<String, Option<String>>,
) -> Option<(String, bool)> {
    let mut rest = rest;
    if is_abs(rest) {
        rest = &rest[1..];
        path = "/".to_string();
    }
    while !rest.is_empty() {
        let (name, remainder) = rest.split_once('/').unwrap_or((rest, ""));
        rest = remainder;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            if path.is_empty() {
                path = "..".to_string();
            } else {
                let (head, tail) = split(&path);
                path = if tail == ".." {
                    join(&join(&head, ".."), "..")
                } else {
                    head
                };
            }
            continue;
        }
        let newpath = join(&path, name);
        let is_link = std::fs::symlink_metadata(on_disk(context, &newpath))
            .is_ok_and(|meta| meta.file_type().is_symlink());
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            match cached {
                Some(resolved) => {
                    path.clone_from(resolved);
                    continue;
                }
                None => return Some((join(&newpath, rest), false)),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = std::fs::read_link(on_disk(context, &newpath)).ok()?;
        let (resolved, ok) = join_realpath(context, path, &target.to_string_lossy(), seen)?;
        if !ok {
            return Some((join(&resolved, rest), false));
        }
        seen.insert(newpath, Some(resolved.clone()));
        path = resolved;
    }
    Some((path, true))
}

/// The home directory `~` expands to: `HOME` when set, else the password
/// database entry of the current user.
fn user_home(context: &GuardContext) -> Option<String> {
    if let Some(home) = context.var("HOME") {
        return Some(home.to_string());
    }
    passwd_home()
}

#[cfg(unix)]
fn passwd_home() -> Option<String> {
    let uid = rustix::process::getuid().as_raw().to_string();
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() > 5 && fields[2] == uid).then(|| fields[5].to_string())
    })
}

#[cfg(not(unix))]
fn passwd_home() -> Option<String> {
    None
}

/// `os.path.expanduser` for the forms the guard expands (`~` and `~/...`;
/// `~user` stays unchanged, the callers refuse it before calling).
pub(super) fn expanduser(context: &GuardContext, path: &str) -> String {
    if !path.starts_with('~') {
        return path.to_string();
    }
    let cut = path[1..].find('/').map_or(path.len(), |index| index + 1);
    if cut != 1 {
        return path.to_string();
    }
    let Some(home) = user_home(context) else {
        return path.to_string();
    };
    let expanded = format!("{}{}", home.trim_end_matches('/'), &path[cut..]);
    if expanded.is_empty() {
        "/".to_string()
    } else {
        expanded
    }
}

/// `os.path.isfile` (following symlinks), relative to the kernel's cwd.
pub(super) fn is_file(context: &GuardContext, path: &str) -> bool {
    std::fs::metadata(on_disk(context, path)).is_ok_and(|meta| meta.is_file())
}

/// The kernel's working directory as text.
pub(super) fn cwd_text(context: &GuardContext) -> String {
    Path::new(context.cwd()).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_matches_posixpath() {
        let cases = [
            ("", "."),
            ("/", "/"),
            ("//", "//"),
            ("///a//b/", "/a/b"),
            ("//a/../b", "//b"),
            ("a/../..", ".."),
            ("/../a", "/a"),
            ("./a/./b/.", "a/b"),
        ];
        for (input, expected) in cases {
            assert_eq!(normpath(input), expected, "{input}");
        }
    }

    #[test]
    fn realpath_follows_links_and_keeps_missing_components() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().canonicalize().unwrap();
        std::fs::create_dir(real.join("target")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(real.join("target"), real.join("link")).unwrap();
        let context = GuardContext::new(real.clone(), std::collections::BTreeMap::new());
        let base = real.display().to_string();
        assert_eq!(
            realpath(&context, &format!("{base}/link/missing/../x")),
            Some(format!("{base}/target/x"))
        );
        assert_eq!(realpath(&context, "rel"), Some(format!("{base}/rel")));
    }
}
