//! `.worktreeinclude`: the git-ignored files a new worktree gets copied from
//! the main checkout (Claude Code's `.worktreeinclude`).
//!
//! The file sits at the repository root and holds gitignore-syntax patterns.
//! A file is copied when git ignores it (it is untracked and matched by the
//! repository's ignore rules) AND a `.worktreeinclude` pattern matches it:
//! `.env`, `config/local.toml`, a generated `node_modules/` the user wants
//! reused. Tracked files are already in the worktree's checkout and are never
//! copied. Symlinks are skipped, and nothing is written through a link inside
//! the worktree.

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::git;

/// The include file's name at the repository root.
pub const WORKTREE_INCLUDE_FILE: &str = ".worktreeinclude";

/// Copy the main checkout's `.worktreeinclude` files into `worktree`; the
/// copied paths (repository-relative, `/`-separated) in listing order.
/// Failures of single files are skipped (best effort, like Claude Code).
#[must_use]
pub fn copy_included_files(repo_root: &Path, worktree: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(repo_root.join(WORKTREE_INCLUDE_FILE)) else {
        return Vec::new();
    };
    let patterns: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let Some(matcher) = build_matcher(repo_root, &patterns) else {
        return Vec::new();
    };
    let Ok(worktree_real) = std::fs::canonicalize(worktree) else {
        return Vec::new();
    };
    let mut copied = Vec::new();
    for relative in included_files(repo_root, &matcher, &patterns) {
        if copy_one(repo_root, &worktree_real, &relative) {
            copied.push(relative);
        }
    }
    copied
}

fn build_matcher(repo_root: &Path, patterns: &[&str]) -> Option<Gitignore> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(repo_root);
    for pattern in patterns {
        // A malformed line is skipped, as git skips it.
        let _ = builder.add_line(None, pattern);
    }
    builder.build().ok()
}

fn matches(matcher: &Gitignore, relative: &str, is_dir: bool) -> bool {
    matcher
        .matched_path_or_any_parents(Path::new(relative), is_dir)
        .is_ignore()
}

/// The ignored, untracked files the patterns select. Git lists a wholly
/// ignored directory as one `dir/` entry; such a directory is expanded only
/// when a pattern could select something inside it (so a huge ignored build
/// directory no pattern names is never walked).
fn included_files(repo_root: &Path, matcher: &Gitignore, patterns: &[&str]) -> Vec<String> {
    let Ok(listing) = git::stdout(
        repo_root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    ) else {
        return Vec::new();
    };
    let entries: Vec<&str> = listing
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .collect();
    let mut files: Vec<String> = entries
        .iter()
        .filter(|entry| !entry.ends_with('/') && matches(matcher, entry, false))
        .map(|entry| (*entry).to_string())
        .collect();
    let dirs: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| entry.ends_with('/'))
        .filter(|dir| {
            matches(matcher, dir.trim_end_matches('/'), true) || pattern_reaches_into(patterns, dir)
        })
        .collect();
    if dirs.is_empty() {
        return files;
    }
    let mut args = vec![
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "-z",
        "--",
    ];
    args.extend(dirs.iter().copied());
    if let Ok(inner) = git::stdout(repo_root, &args) {
        files.extend(
            inner
                .split('\0')
                .filter(|entry| !entry.is_empty() && matches(matcher, entry, false))
                .map(str::to_string),
        );
    }
    files
}

/// Whether some pattern could select a path inside `dir` (`dir/` as git
/// lists it), after Claude Code's rule: the pattern spells a path into the
/// directory, its literal prefix (up to the first glob character) does, or
/// it is a `**/name/...` pattern whose first segment is one of the
/// directory's segments (a segment prefix when it holds a glob). A bare name such as
/// `.env` does not open an ignored directory: the dependency and build trees
/// stay unwalked.
fn pattern_reaches_into(patterns: &[&str], dir: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.starts_with(dir) {
            return true;
        }
        if let Some(glob) = pattern.find(['*', '?', '[']).filter(|&at| at > 0) {
            if dir.starts_with(&pattern[..glob]) {
                return true;
            }
        }
        let mut floating = pattern;
        while let Some(rest) = floating.strip_prefix("**/") {
            floating = rest;
        }
        if floating == pattern {
            return false;
        }
        // The floating pattern's first segment names the directory it
        // reaches into (`**/secrets/key.pem` opens any `.../secrets/`).
        let floating = floating.split('/').next().unwrap_or_default();
        let glob = floating.find(['*', '?', '[']);
        let literal = glob
            .map_or(floating, |at| &floating[..at])
            .replace('\\', "")
            .to_lowercase();
        if literal.is_empty() {
            return false;
        }
        let mut segments = dir.trim_end_matches('/').split('/').map(str::to_lowercase);
        match glob {
            None => segments.any(|segment| segment == literal),
            Some(_) => segments.any(|segment| segment.starts_with(&literal)),
        }
    })
}

/// Copy one file, refusing a symlinked source and any destination whose
/// parent resolves outside the worktree (a committed symlink inside it).
fn copy_one(repo_root: &Path, worktree_real: &Path, relative: &str) -> bool {
    let source = repo_root.join(relative);
    let Ok(metadata) = std::fs::symlink_metadata(&source) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let destination = worktree_real.join(relative);
    let Some(parent) = destination.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let inside = std::fs::canonicalize(parent)
        .is_ok_and(|resolved: PathBuf| resolved.starts_with(worktree_real));
    if !inside || std::fs::symlink_metadata(&destination).is_ok() {
        return false;
    }
    std::fs::copy(&source, &destination).is_ok()
}

#[cfg(test)]
mod tests {
    use super::pattern_reaches_into;

    #[test]
    fn only_patterns_that_can_select_inside_a_directory_expand_it() {
        assert!(pattern_reaches_into(&["config/local.toml"], "config/"));
        assert!(pattern_reaches_into(&["/config/*.toml"], "config/"));
        assert!(pattern_reaches_into(
            &["**/secrets/key.pem"],
            "app/secrets/"
        ));
        assert!(pattern_reaches_into(&["**/sec*/x"], "app/secrets/"));
        assert!(!pattern_reaches_into(&["**/secrets/key.pem"], "app/other/"));
        assert!(!pattern_reaches_into(&[".env"], "target/"));
        assert!(!pattern_reaches_into(&["config/local.toml"], "target/"));
        assert!(!pattern_reaches_into(&["/.env"], "target/"));
    }
}
