//! Rules directories (`.claude/rules/`, `.prime/agent/rules/`): every
//! `*.md` file below one is an instruction file. A rule whose frontmatter
//! names `paths:` globs is conditional: it loads only once the agent touches
//! a file those globs match (relative to the directory that holds the rules
//! directory), the way Claude Code's path-scoped rules do.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// How deep a rules directory is walked (subdirectories group rules).
const MAX_RULES_DEPTH: usize = 8;

/// The `*.md` files under `dir`, sorted by path so the load order is stable.
#[must_use]
pub fn rule_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk(dir, 0, &mut files);
    files.sort();
    files
}

fn walk(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > MAX_RULES_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `metadata` follows symlinks: a linked rule file or directory counts.
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            walk(&path, depth + 1, files);
        } else if metadata.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            files.push(path);
        }
    }
}

/// The `paths:` globs a rule's frontmatter names: a list of strings, or one
/// string of comma-separated globs (commas inside `{...}` stay in the glob).
#[must_use]
pub fn frontmatter_globs(frontmatter: &serde_json::Value) -> Vec<String> {
    let raw: Vec<String> = match frontmatter.get("paths") {
        Some(serde_json::Value::String(text)) => vec![text.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    raw.iter()
        .flat_map(|text| split_outside_braces(text))
        .map(|glob| glob.trim().to_string())
        .filter(|glob| !glob.is_empty())
        .collect()
}

fn split_outside_braces(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// A conditional rule's compiled globs, gitignore-style: a glob without a
/// `/` matches at any depth, a leading `/` anchors it to the base directory,
/// and `*` never crosses a `/`.
#[derive(Debug, Clone)]
pub struct RuleMatcher {
    base: PathBuf,
    set: GlobSet,
}

impl RuleMatcher {
    /// `None` when no glob compiles.
    #[must_use]
    pub fn new(base: &Path, globs: &[String]) -> Option<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut any = false;
        for glob in globs {
            let pattern = normalize_glob(glob);
            if let Ok(compiled) = GlobBuilder::new(&pattern).literal_separator(true).build() {
                builder.add(compiled);
                any = true;
            }
        }
        let set = builder.build().ok()?;
        any.then(|| Self {
            base: base.to_path_buf(),
            set,
        })
    }

    /// Whether `path` (absolute) lies under the base and matches a glob.
    #[must_use]
    pub fn matches(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.base) else {
            return false;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        !relative.is_empty() && self.set.is_match(relative.as_str())
    }
}

fn normalize_glob(glob: &str) -> String {
    let glob = glob.trim_start_matches("./");
    let glob = glob
        .strip_suffix('/')
        .map_or_else(|| glob.to_string(), |directory| format!("{directory}/**"));
    if let Some(anchored) = glob.strip_prefix('/') {
        return anchored.to_string();
    }
    if glob.contains('/') || glob.starts_with("**") {
        glob
    } else {
        format!("**/{glob}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(globs: &[&str]) -> RuleMatcher {
        RuleMatcher::new(
            Path::new("/repo"),
            &globs.iter().map(|g| (*g).to_string()).collect::<Vec<_>>(),
        )
        .expect("globs compile")
    }

    #[test]
    fn globs_come_from_a_list_or_a_comma_string() {
        let list = serde_json::json!({ "paths": ["src/**/*.rs", " docs/*.md "] });
        assert_eq!(frontmatter_globs(&list), vec!["src/**/*.rs", "docs/*.md"]);
        let text = serde_json::json!({ "paths": "src/*.{ts,tsx}, lib/**" });
        assert_eq!(frontmatter_globs(&text), vec!["src/*.{ts,tsx}", "lib/**"]);
        assert!(frontmatter_globs(&serde_json::json!({ "name": "x" })).is_empty());
    }

    #[test]
    fn globs_match_gitignore_style_under_the_base() {
        let rust = matcher(&["*.rs"]);
        assert!(rust.matches(Path::new("/repo/a.rs")));
        assert!(rust.matches(Path::new("/repo/deep/er/b.rs")));
        assert!(!rust.matches(Path::new("/other/b.rs")));

        let anchored = matcher(&["/src/*.ts"]);
        assert!(anchored.matches(Path::new("/repo/src/a.ts")));
        assert!(!anchored.matches(Path::new("/repo/src/deep/a.ts")));
        assert!(!anchored.matches(Path::new("/repo/lib/src/a.ts")));

        let tree = matcher(&["crates/pa-core/**/*.rs", "docs/"]);
        assert!(tree.matches(Path::new("/repo/crates/pa-core/src/lib.rs")));
        assert!(tree.matches(Path::new("/repo/docs/x/y.md")));
        assert!(!tree.matches(Path::new("/repo/crates/pa-tui/src/lib.rs")));
    }

    #[test]
    fn rule_files_walk_subdirectories_in_sorted_order() {
        let tmp = tempfile::tempdir().unwrap();
        let rules = tmp.path().join("rules");
        std::fs::create_dir_all(rules.join("group")).unwrap();
        std::fs::write(rules.join("b.md"), "b").unwrap();
        std::fs::write(rules.join("a.md"), "a").unwrap();
        std::fs::write(rules.join("notes.txt"), "x").unwrap();
        std::fs::write(rules.join("group").join("c.md"), "c").unwrap();
        let names: Vec<_> = rule_files(&rules)
            .iter()
            .map(|p| {
                p.strip_prefix(&rules)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, vec!["a.md", "b.md", "group/c.md"]);
        assert!(rule_files(&tmp.path().join("missing")).is_empty());
    }
}
