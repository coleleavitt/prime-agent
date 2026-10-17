//! Read and Edit rule content: gitignore-style path patterns, Claude Code's
//! anchors:
//!
//! | pattern | anchored at |
//! |---|---|
//! | `//abs/path` | the filesystem root |
//! | `~/path` | the home directory |
//! | `/path` | the rule's settings root (the project for project settings, the home for user settings) |
//! | `./path` | the session's working directory |
//! | `path` | the working directory, gitignore rules (no `/` matches at any depth) |
//!
//! A pattern matches a path or any of its parent directories (`Edit(build)`
//! covers `build/out.js`).

use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// A compiled path pattern.
#[derive(Debug, Clone)]
pub struct PathPattern {
    root: PathBuf,
    matcher: Gitignore,
}

/// Where relative patterns and paths resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAnchors {
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The home directory, when it resolves.
    pub home: Option<PathBuf>,
}

/// `path` made absolute against `cwd` (a leading `~/` against `home`), with
/// `.` and `..` resolved lexically.
#[must_use]
pub fn absolute(path: &str, anchors: &PathAnchors) -> PathBuf {
    let joined = if let Some(rest) = path.strip_prefix("~/") {
        match &anchors.home {
            Some(home) => home.join(rest),
            None => anchors.cwd.join(path),
        }
    } else if path == "~" {
        anchors
            .home
            .clone()
            .unwrap_or_else(|| anchors.cwd.join(path))
    } else {
        anchors.cwd.join(path)
    };
    normalize(&joined)
}

/// `path` with `.` and `..` resolved lexically.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

impl PathPattern {
    /// Compile `content`, whose `/`-anchored form is relative to
    /// `settings_root`.
    ///
    /// # Errors
    ///
    /// The pattern is not a valid glob, or `~/` cannot resolve.
    pub fn new(content: &str, settings_root: &Path, anchors: &PathAnchors) -> Result<Self, String> {
        let content = content.trim();
        let (root, pattern) = if let Some(rest) = content.strip_prefix("//") {
            (PathBuf::from("/"), format!("/{rest}"))
        } else if let Some(rest) = content.strip_prefix("~/") {
            let home = anchors
                .home
                .clone()
                .ok_or_else(|| format!("{content}: the home directory does not resolve"))?;
            (home, format!("/{rest}"))
        } else if content.starts_with('/') {
            (settings_root.to_path_buf(), content.to_string())
        } else if let Some(rest) = content.strip_prefix("./") {
            (anchors.cwd.clone(), format!("/{rest}"))
        } else {
            (anchors.cwd.clone(), content.to_string())
        };
        let root = normalize(&root);
        let mut builder = GitignoreBuilder::new(&root);
        builder
            .add_line(None, &pattern)
            .map_err(|error| format!("{content}: {error}"))?;
        let matcher = builder
            .build()
            .map_err(|error| format!("{content}: {error}"))?;
        Ok(PathPattern { root, matcher })
    }

    /// Whether the pattern covers the absolute `path` (or a parent of it).
    #[must_use]
    pub fn matches(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        self.matcher
            .matched_path_or_any_parents(relative, false)
            .is_ignore()
    }
}

/// Whether `path` lies in `dir` (or is it).
#[must_use]
pub fn is_within(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors() -> PathAnchors {
        PathAnchors {
            cwd: PathBuf::from("/work/proj"),
            home: Some(PathBuf::from("/home/me")),
        }
    }

    fn matches(content: &str, path: &str) -> bool {
        PathPattern::new(content, Path::new("/work/proj"), &anchors())
            .unwrap()
            .matches(Path::new(path))
    }

    /// Claude Code's documented path rule examples.
    #[test]
    fn path_rules_anchor_and_match_like_gitignore() {
        let table: &[(&str, &str, bool)] = &[
            ("./.env", "/work/proj/.env", true),
            ("./.env", "/work/proj/sub/.env", false),
            (".env", "/work/proj/sub/.env", true),
            ("*.env", "/work/proj/a/b/prod.env", true),
            ("**/*.env", "/work/proj/a/prod.env", true),
            ("./secrets/**", "/work/proj/secrets/a/key.pem", true),
            ("./secrets/**", "/work/proj/other/secrets/a", false),
            ("secrets", "/work/proj/x/secrets/key", true),
            ("src/**", "/work/proj/src/main.rs", true),
            ("src/**", "/work/proj/lib/src/main.rs", false),
            ("/docs/**", "/work/proj/docs/a.md", true),
            ("/docs/**", "/work/proj/x/docs/a.md", false),
            ("~/.zshrc", "/home/me/.zshrc", true),
            ("~/.ssh/**", "/home/me/.ssh/id_ed25519", true),
            ("//etc/passwd", "/etc/passwd", true),
            ("//etc/**", "/etc/ssh/sshd_config", true),
            ("//etc/**", "/work/proj/etc/x", false),
            // Outside the anchor never matches.
            ("*.env", "/elsewhere/prod.env", false),
        ];
        for (content, path, expected) in table {
            assert_eq!(matches(content, path), *expected, "{content} vs {path}");
        }
    }

    #[test]
    fn paths_resolve_against_the_working_directory_and_home() {
        let anchors = anchors();
        assert_eq!(
            absolute("src/../lib/./a.rs", &anchors),
            PathBuf::from("/work/proj/lib/a.rs")
        );
        assert_eq!(absolute("/tmp/x", &anchors), PathBuf::from("/tmp/x"));
        assert_eq!(absolute("~/x", &anchors), PathBuf::from("/home/me/x"));
        assert!(is_within(
            Path::new("/work/proj/a"),
            Path::new("/work/proj")
        ));
        assert!(!is_within(
            Path::new("/work/project"),
            Path::new("/work/proj")
        ));
    }
}
