//! What a workspace's trust covers, and the content hash a decision is
//! pinned to.
//!
//! The hash covers exactly what the gate holds back: the gated subset of
//! the project settings (canonical JSON, safe keys excluded, so a theme
//! change does not ask again), the project system-prompt files, and every
//! file under the gated directories (relative path plus bytes).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::GatedItem;

/// Tool-written caches inside gated directories: never imported or read
/// as configuration, so they stay out of the hash (an editable install or
/// a test run would otherwise revoke trust).
const IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];
const IGNORED_DIR_SUFFIX: &str = ".egg-info";
const IGNORED_FILE_SUFFIX: &str = ".pyc";
/// Symlinked directories can loop; the walk stops descending past this.
const MAX_TREE_DEPTH: usize = 32;

/// A gated directory, hashed as a tree.
#[derive(Debug, Clone)]
struct GatedTree {
    label: String,
    root: PathBuf,
}

/// The gated content of one workspace.
#[derive(Debug, Default)]
pub(super) struct TrustInputs {
    /// The gated project settings subset (keys outside the safe list).
    settings: serde_json::Map<String, Value>,
    system_prompt: Option<PathBuf>,
    append_system_prompt: Option<PathBuf>,
    trees: Vec<(GatedItem, GatedTree)>,
    /// With the Claude Code opt-in: each project `.claude/settings*.json`
    /// file's keys that run code or loosen the session.
    claude_settings: Vec<(String, serde_json::Map<String, Value>)>,
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn dir_has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// The project settings document's gated keys (post-migration, so a legacy
/// key that migrates into a gated one is gated too). An unparseable
/// document loads as empty settings, so it gates nothing.
fn gated_settings(project_dir: &Path) -> serde_json::Map<String, Value> {
    let Ok(content) = std::fs::read_to_string(project_dir.join("settings.json")) else {
        return serde_json::Map::new();
    };
    let Ok(Value::Object(mut document)) = serde_json::from_str::<Value>(&content) else {
        return serde_json::Map::new();
    };
    crate::settings::merge::migrate(&mut document);
    document.retain(|key, value| !super::is_untrusted_safe_setting(key, value));
    document
}

/// A `skills` settings entry that names a path (not an override pattern
/// or a glob), resolved like the package manager resolves it: tilde, then
/// relative to the project config dir.
fn settings_skill_path(entry: &str, project_dir: &Path) -> Option<PathBuf> {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with(['!', '+', '-']) || entry.contains(['*', '?', '[']) {
        return None;
    }
    let home = || pa_types::platform::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let path = if entry == "~" {
        home()
    } else if let Some(rest) = entry.strip_prefix("~/") {
        home().join(rest)
    } else if Path::new(entry).is_absolute() {
        PathBuf::from(entry)
    } else {
        project_dir.join(entry)
    };
    path.exists().then_some(path)
}

/// Every project skill source the package resolution would load: the
/// project `skills/` dir, each ancestor `.agents/skills/` up to the git
/// root (the user's own `~/.agents/skills` excluded), and the project
/// `skills` settings entries that name a path.
fn project_skill_trees(
    cwd: &Path,
    project_dir: &Path,
    settings: &serde_json::Map<String, Value>,
) -> Vec<(GatedItem, GatedTree)> {
    let config_dir = crate::settings::CONFIG_DIR_NAME;
    let mut trees = Vec::new();
    let skills_dir = project_dir.join("skills");
    if dir_has_entries(&skills_dir) {
        trees.push((
            GatedItem::ProjectSkills(format!("{config_dir}/skills/")),
            GatedTree {
                label: "skills".to_string(),
                root: skills_dir,
            },
        ));
    }
    let user_agents_skills = pa_types::platform::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agents")
        .join("skills");
    for dir in crate::packages::resolve::discovery::collect_ancestor_agents_skill_dirs(cwd) {
        if dir == user_agents_skills || !dir_has_entries(&dir) {
            continue;
        }
        let shown = dir.strip_prefix(cwd).map_or_else(
            |_| dir.display().to_string(),
            |relative| relative.display().to_string(),
        );
        trees.push((
            GatedItem::ProjectSkills(format!("{shown}/")),
            GatedTree {
                label: format!("agents-skills:{}", dir.display()),
                root: dir,
            },
        ));
    }
    let entries = settings.get("skills").and_then(Value::as_array);
    for entry in entries.into_iter().flatten().filter_map(Value::as_str) {
        if let Some(root) = settings_skill_path(entry, project_dir) {
            trees.push((
                GatedItem::ProjectSkills(format!(
                    "{config_dir}/settings.json skills entry {entry}"
                )),
                GatedTree {
                    label: format!("settings-skills:{entry}"),
                    root,
                },
            ));
        }
    }
    trees
}

impl TrustInputs {
    pub(super) fn collect(cwd: &Path, agent_dir: &Path) -> TrustInputs {
        let project_dir = cwd.join(crate::settings::CONFIG_DIR_NAME);
        // Run from the home directory, the project config dir IS the agent
        // dir: that configuration is the user's own.
        if same_dir(&project_dir, agent_dir) {
            return TrustInputs::default();
        }
        let file = |name: &str| {
            let path = project_dir.join(name);
            path.is_file().then_some(path)
        };
        let mut trees = Vec::new();
        let prompts = project_dir.join("prompts");
        if dir_has_entries(&prompts) {
            trees.push((
                GatedItem::PromptTemplates,
                GatedTree {
                    label: "prompts".to_string(),
                    root: prompts,
                },
            ));
        }
        let settings = gated_settings(&project_dir);
        trees.extend(project_skill_trees(cwd, &project_dir, &settings));
        for (dir, label) in super::gated_project_dirs() {
            let root = project_dir.join(dir);
            if dir_has_entries(&root) {
                trees.push((
                    GatedItem::ProjectDir {
                        dir: dir.to_string(),
                        label: label.to_string(),
                    },
                    GatedTree {
                        label: format!("project-dir:{dir}"),
                        root,
                    },
                ));
            }
        }
        // The project's output styles rewrite the system prompt.
        let styles = project_dir.join(crate::prompts::output_style::OUTPUT_STYLES_DIR);
        if dir_has_entries(&styles) {
            trees.push((
                GatedItem::ProjectDir {
                    dir: crate::prompts::output_style::OUTPUT_STYLES_DIR.to_string(),
                    label: "output styles".to_string(),
                },
                GatedTree {
                    label: "output-styles".to_string(),
                    root: styles,
                },
            ));
        }
        // Claude Code's project directories this harness reads: custom
        // commands (their shell blocks run at expansion) and output styles.
        for (dir, label) in [
            (
                crate::skills::claude_commands::CLAUDE_COMMANDS_DIR,
                "custom commands",
            ),
            (
                crate::prompts::output_style::OUTPUT_STYLES_DIR,
                "output styles",
            ),
            (crate::skills::claude_skills::CLAUDE_SKILLS_DIR, "skills"),
        ] {
            let root = cwd
                .join(crate::skills::claude_commands::CLAUDE_DIR_NAME)
                .join(dir);
            if dir_has_entries(&root) {
                trees.push((
                    GatedItem::ClaudeDir {
                        dir: dir.to_string(),
                        label: label.to_string(),
                    },
                    GatedTree {
                        label: format!("claude:{dir}"),
                        root,
                    },
                ));
            }
        }
        // The ancestors' `.claude/skills` up to the repository root load as
        // project skills too (Claude Code's walk).
        let own_skills = cwd
            .join(crate::skills::claude_commands::CLAUDE_DIR_NAME)
            .join(crate::skills::claude_skills::CLAUDE_SKILLS_DIR);
        for root in crate::skills::claude_skills::project_claude_dirs(
            cwd,
            crate::skills::claude_skills::CLAUDE_SKILLS_DIR,
            pa_types::platform::home_dir().as_deref(),
        ) {
            if root == own_skills || !dir_has_entries(&root) {
                continue;
            }
            trees.push((
                GatedItem::ProjectSkills(format!("{}/", root.display())),
                GatedTree {
                    label: format!("claude-skills:{}", root.display()),
                    root,
                },
            ));
        }
        // Project files a composition root gated (each hashed as a
        // one-file tree).
        for (path, label) in super::gated_project_files() {
            let root = cwd.join(path);
            if root.is_file() {
                trees.push((
                    GatedItem::ProjectFile {
                        path: path.to_string(),
                        label: label.to_string(),
                    },
                    GatedTree {
                        label: format!("project-file:{path}"),
                        root,
                    },
                ));
            }
        }
        TrustInputs {
            settings,
            system_prompt: file("SYSTEM.md"),
            append_system_prompt: file("APPEND_SYSTEM.md"),
            trees,
            claude_settings: claude_settings(cwd, agent_dir),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.settings.is_empty()
            && self.system_prompt.is_none()
            && self.append_system_prompt.is_none()
            && self.trees.is_empty()
            && self.claude_settings.is_empty()
    }

    pub(super) fn gated(&self) -> Vec<GatedItem> {
        let mut gated = Vec::new();
        if !self.settings.is_empty() {
            gated.push(GatedItem::SettingsKeys(
                self.settings.keys().cloned().collect(),
            ));
        }
        if self.system_prompt.is_some() {
            gated.push(GatedItem::SystemPrompt);
        }
        if self.append_system_prompt.is_some() {
            gated.push(GatedItem::AppendSystemPrompt);
        }
        gated.extend(self.trees.iter().map(|(item, _)| item.clone()));
        gated.extend(
            self.claude_settings
                .iter()
                .map(|(file, keys)| GatedItem::ClaudeSettings {
                    file: file.clone(),
                    keys: keys.keys().cloned().collect(),
                }),
        );
        gated
    }

    /// `sha256:<hex>` over every gated input, each framed by label and
    /// length so no two different inputs share a byte stream.
    pub(super) fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"prime-agent-workspace-trust/v1\0");
        let mut settings = Value::Object(self.settings.clone());
        sort_keys(&mut settings);
        frame(&mut hasher, "settings", settings.to_string().as_bytes());
        for (label, path) in [
            ("SYSTEM.md", &self.system_prompt),
            ("APPEND_SYSTEM.md", &self.append_system_prompt),
        ] {
            let bytes = path
                .as_ref()
                .and_then(|path| std::fs::read(path).ok())
                .unwrap_or_default();
            frame(&mut hasher, label, &bytes);
        }
        for (_, tree) in &self.trees {
            frame(
                &mut hasher,
                &format!("tree:{}", tree.label),
                &tree_digest(&tree.root),
            );
        }
        for (file, keys) in &self.claude_settings {
            let mut keys = Value::Object(keys.clone());
            sort_keys(&mut keys);
            frame(
                &mut hasher,
                &format!("claude-settings:{file}"),
                keys.to_string().as_bytes(),
            );
        }
        format!("sha256:{}", hex(&hasher.finalize()))
    }
}

/// The gated keys of the project's Claude Code settings files, when the
/// user settings opt into them (a file that is the user's own
/// `~/.claude/settings.json`, in a session at the home directory, is not
/// the project's).
fn claude_settings(cwd: &Path, agent_dir: &Path) -> Vec<(String, serde_json::Map<String, Value>)> {
    use crate::settings::claude_code::{self, ClaudePaths, ClaudeScope};
    if !claude_code::opted_in(agent_dir) {
        return Vec::new();
    }
    let paths = ClaudePaths::resolve(cwd);
    let user_file = paths
        .file(ClaudeScope::User)
        .and_then(|file| std::fs::canonicalize(file).ok());
    let mut gated = Vec::new();
    for (scope, shown) in [
        (ClaudeScope::Project, ".claude/settings.json"),
        (ClaudeScope::Local, ".claude/settings.local.json"),
    ] {
        let Some(path) = paths.file(scope) else {
            continue;
        };
        if user_file.is_some() && std::fs::canonicalize(&path).ok() == user_file {
            continue;
        }
        let Ok(Some(document)) = claude_code::read_document(&path) else {
            continue;
        };
        let keys = claude_code::gated_subset(&document);
        if !keys.is_empty() {
            gated.push((shown.to_string(), keys));
        }
    }
    gated
}

fn frame(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Recursively sort object keys: the hash must not depend on key order
/// (the workspace's `serde_json` preserves insertion order).
fn sort_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (key, mut child) in entries {
                sort_keys(&mut child);
                map.insert(key, child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sort_keys),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// One file under a gated tree, with the stat facts the memo keys on.
struct TreeFile {
    relative: String,
    path: PathBuf,
    stamp: [u64; 4],
}

fn stat_stamp(metadata: &std::fs::Metadata) -> [u64; 4] {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        [
            metadata.len(),
            modified,
            metadata.ino(),
            (metadata.ctime() as u64).wrapping_mul(1_000_000_000) + metadata.ctime_nsec() as u64,
        ]
    }
    #[cfg(not(unix))]
    {
        [metadata.len(), modified, 0, 0]
    }
}

fn is_ignored(name: &str, is_dir: bool) -> bool {
    if is_dir {
        IGNORED_DIR_NAMES.contains(&name) || name.ends_with(IGNORED_DIR_SUFFIX)
    } else {
        name.ends_with(IGNORED_FILE_SUFFIX)
    }
}

fn walk(
    dir: &Path,
    relative: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<TreeFile>,
) {
    if depth > MAX_TREE_DEPTH {
        return;
    }
    if let Ok(canonical) = std::fs::canonicalize(dir) {
        if !visited.insert(canonical) {
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        // Follow symlinks: the loaders do, so the hash covers what they read.
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if is_ignored(&name, metadata.is_dir()) {
            continue;
        }
        let child = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        if metadata.is_dir() {
            walk(&path, &child, depth + 1, visited, out);
        } else if metadata.is_file() {
            out.push(TreeFile {
                relative: child,
                stamp: stat_stamp(&metadata),
                path,
            });
        }
    }
}

/// A tree's stat stamp and the content digest computed under it.
type TreeMemo = Mutex<HashMap<PathBuf, ([u8; 32], Vec<u8>)>>;

/// Memo of tree digests by root: a tree whose every file stat is
/// unchanged is not re-read (settings are opened many times per turn).
fn tree_memo() -> &'static TreeMemo {
    static MEMO: OnceLock<TreeMemo> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tree_digest(root: &Path) -> Vec<u8> {
    let mut files = Vec::new();
    match std::fs::metadata(root) {
        // A settings entry can name one skill file.
        Ok(metadata) if metadata.is_file() => files.push(TreeFile {
            relative: String::new(),
            stamp: stat_stamp(&metadata),
            path: root.to_path_buf(),
        }),
        Ok(_) | Err(_) => walk(root, "", 0, &mut HashSet::new(), &mut files),
    }
    let mut stamp_hasher = Sha256::new();
    for file in &files {
        frame(&mut stamp_hasher, &file.relative, &[]);
        for part in file.stamp {
            stamp_hasher.update(part.to_le_bytes());
        }
    }
    let stamp: [u8; 32] = stamp_hasher.finalize().into();
    let memo = tree_memo();
    if let Some((cached_stamp, digest)) = memo
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
    {
        if *cached_stamp == stamp {
            return digest.clone();
        }
    }
    let mut hasher = Sha256::new();
    for file in &files {
        let bytes = std::fs::read(&file.path).unwrap_or_default();
        frame(&mut hasher, &file.relative, &bytes);
    }
    let digest = hasher.finalize().to_vec();
    memo.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), (stamp, digest.clone()));
    digest
}
