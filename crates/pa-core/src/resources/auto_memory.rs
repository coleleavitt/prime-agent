//! Auto memory (Claude Code's memdir): a per-project directory under the
//! agent dir where the model keeps memory files across sessions, with a
//! `MEMORY.md` index loaded into every session's prompt.
//!
//! The directory is `<agent dir>/projects/<sanitized project root>/memory/`;
//! the project root is the main checkout of the working directory's git
//! repository (every worktree of a repository shares one memory), else the
//! working directory. The model writes memory files with the tools it
//! already has (Python file writes, `edit`); there is no memory API.
//!
//! On by default; `autoMemoryEnabled: false` in settings or
//! `PRIME_AGENT_DISABLE_AUTO_MEMORY=1` turns it off, and so does
//! `--no-context-files`. Only top-level sessions carry it: a subagent's
//! prompt stays task-focused, as in Claude Code.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// The index file name.
pub const MEMORY_INDEX: &str = "MEMORY.md";
/// Index lines loaded into the prompt; the rest is cut with a warning.
pub const MAX_INDEX_LINES: usize = 200;
/// Index bytes loaded into the prompt (after the line cap).
pub const MAX_INDEX_BYTES: usize = 25_000;
/// The environment switch that turns auto memory off.
pub const DISABLE_ENV: &str = "PRIME_AGENT_DISABLE_AUTO_MEMORY";

/// Flag-tier (CLI/env) override for the auto-memory directory. Mirrors
/// Claude Code's flag-settings precedence hop between policy and user.
pub const DIRECTORY_FLAG_ENV: &str = "PRIME_AGENT_AUTO_MEMORY_DIRECTORY";

/// Directory-name budget before the sanitized root is shortened with a hash.
const MAX_SANITIZED_LEN: usize = 200;

/// Read filter applied on every memory tier ([`memory_file_names`]): exact
/// filenames in `excluded` and any file whose name starts with an entry in
/// `excluded_prefixes` is dropped before the model sees it. Claude Code
/// calls these `excluded`/`excludedPrefixes` on its `cQe` read filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryExcludes {
    /// Exact-match filenames to skip (e.g. `"draft.md"`).
    pub excluded: Vec<String>,
    /// Filename-prefix matches to skip (e.g. `"draft_"`).
    pub excluded_prefixes: Vec<String>,
}

impl MemoryExcludes {
    /// Whether `name` is allowed through the filter.
    #[must_use]
    pub fn allows(&self, name: &str) -> bool {
        !self.excluded.iter().any(|entry| entry == name)
            && !self
                .excluded_prefixes
                .iter()
                .any(|prefix| !prefix.is_empty() && name.starts_with(prefix.as_str()))
    }

    /// Whether the filter is empty (no entries on either list).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.excluded.is_empty() && self.excluded_prefixes.is_empty()
    }
}

/// Policy/enterprise hook for the auto-memory directory override. Prime-agent
/// has no managed-settings layer today; this trait lets a future one claim
/// the top of Claude Code's `policy > flag > user` precedence without any
/// caller churn.
pub trait AutoMemoryPolicy: Send + Sync {
    /// The policy-tier override, when the enterprise layer has one. The
    /// default returns `None` so the resolver falls through to flag/user.
    fn directory_override(&self) -> Option<PathBuf> {
        None
    }
}

/// Resolve the auto-memory directory with Claude Code's `policy > flag >
/// user` precedence for the primary (prime-agent-owned) tier. The Claude
/// Code overlay directory is NOT affected by this resolver — overlays
/// follow `CLAUDE_CODE_REMOTE_MEMORY_DIR` or `~/.claude`.
///
/// - `policy`: the first [`AutoMemoryPolicy::directory_override`] the caller
///   provides, when the enterprise layer has one. `None` without a policy.
/// - `flag`: [`DIRECTORY_FLAG_ENV`] when it names a non-empty absolute path.
///   Relative or empty values are refused with a debug log (Claude Code
///   does the same for its flag source).
/// - `user`: `user_setting` is the string from `autoMemoryDirectory` in
///   `<agent dir>/settings.json`, when non-empty. Relative paths resolve
///   against `agent_dir`; absolute paths are used as-is.
///
/// The default (`None` return) is `<agent dir>/projects/<slug>/memory/`,
/// resolved by [`memory_dir`].
#[must_use]
pub fn resolve_auto_memory_directory(
    policy: Option<&dyn AutoMemoryPolicy>,
    agent_dir: &Path,
    user_setting: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = policy.and_then(AutoMemoryPolicy::directory_override) {
        return Some(dir);
    }
    if let Some(value) = std::env::var_os(DIRECTORY_FLAG_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(&value);
        if path.is_absolute() {
            return Some(path);
        }
        tracing::debug!(
            target: "pa_core::auto_memory",
            env = DIRECTORY_FLAG_ENV,
            value = %path.display(),
            "PRIME_AGENT_AUTO_MEMORY_DIRECTORY is not an absolute path; ignored"
        );
    }
    let user = user_setting
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let user_path = PathBuf::from(user);
    if user_path.is_absolute() {
        Some(user_path)
    } else {
        Some(agent_dir.join(user_path))
    }
}

/// One session's auto memory: where it lives and the index as loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMemory {
    pub dir: PathBuf,
    /// The index text as the prompt carries it (capped); empty when the
    /// index does not exist or is empty.
    pub index: String,
    /// The index was cut to fit [`MAX_INDEX_LINES`] / [`MAX_INDEX_BYTES`].
    pub truncated: bool,
    /// Read-only overlays from other tiers (`claudeCodeMemory`). The model
    /// sees them under the primary memory in the prompt and reads their
    /// files from `overlay.dir`; prime-agent's own files win on name
    /// collision.
    pub overlays: Vec<AutoMemoryOverlay>,
}

/// Which category of overlay a tier is. Tiers flagged
/// [`OverlayKind::Draft`] carry a prominent "DRAFT — pending" header and
/// are always shadowed by both the primary tier AND every confirmed
/// overlay (proposals are never authoritative).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// A confirmed memory tier (project, user, `agent-memory-local`).
    #[default]
    Confirmed,
    /// Pending proposals awaiting user approval (CC's `memory/proposals/`).
    Draft,
}

impl AutoMemory {
    /// The index file's path.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.dir.join(MEMORY_INDEX)
    }
}

/// A read-only memory tier mounted beside the primary auto memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMemoryOverlay {
    /// Human-readable label for the prompt section heading (e.g.
    /// `Claude Code project memory`).
    pub label: String,
    /// The tier's memory directory on disk.
    pub dir: PathBuf,
    /// The tier's [`MEMORY_INDEX`] body as the prompt carries it (capped);
    /// empty when the file is missing or empty.
    pub index: String,
    /// The tier's index was cut to the line/byte caps.
    pub truncated: bool,
    /// Names of overlay files (relative to [`Self::dir`]) that another
    /// tier (primary, or for proposals also any confirmed overlay) already
    /// owns under the same filename; those tiers win and these entries
    /// are hidden from the model.
    pub shadowed_files: Vec<String>,
    /// Whether this overlay is a confirmed tier or a draft/proposal tier.
    pub kind: OverlayKind,
}

/// A directory to mount as an overlay.
#[derive(Debug, Clone)]
pub struct OverlaySource {
    pub label: String,
    pub dir: PathBuf,
    /// Overlay category; `Confirmed` by default, `Draft` for
    /// `memory/proposals/`-style tiers.
    pub kind: OverlayKind,
}

impl OverlaySource {
    /// A confirmed-tier overlay source.
    #[must_use]
    pub fn confirmed(label: impl Into<String>, dir: PathBuf) -> Self {
        Self {
            label: label.into(),
            dir,
            kind: OverlayKind::Confirmed,
        }
    }

    /// A draft-tier overlay source (`memory/proposals/` and friends).
    #[must_use]
    pub fn draft(label: impl Into<String>, dir: PathBuf) -> Self {
        Self {
            label: label.into(),
            dir,
            kind: OverlayKind::Draft,
        }
    }
}

/// Whether the environment turned auto memory off (`1`/`true`/`yes`/`on`).
#[must_use]
pub fn disabled_by_env() -> bool {
    std::env::var(DISABLE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The per-project directory under the agent dir, `<agent
/// dir>/projects/<sanitized project root>`. Auto memory and prompt history
/// are siblings inside it, so every session of a repository shares one.
#[must_use]
pub fn project_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    agent_dir
        .join("projects")
        .join(sanitize(&project_root(cwd)))
}

/// The memory directory for a working directory. NFC-normalized at the final
/// path (Claude Code's `Ai(...).normalize("NFC")` on its own memory dir).
#[must_use]
pub fn memory_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    normalize_nfc(&project_dir(agent_dir, cwd).join("memory"))
}

/// NFC-normalize every component of `path` (CC's post-join `.normalize("NFC")`).
#[must_use]
pub fn normalize_nfc(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let normalized: String = text.nfc().collect();
    PathBuf::from(normalized)
}

/// Load the session's auto memory, creating the directory so the model can
/// write to it without checking first. A directory that cannot be created
/// still loads (the prompt names it; the model's write reports the error).
#[must_use]
pub fn load_auto_memory(agent_dir: &Path, cwd: &Path) -> AutoMemory {
    load_auto_memory_with(
        agent_dir,
        cwd,
        &AutoMemoryLoad {
            overlay_sources: &[],
            directory_override: None,
            excludes: &MemoryExcludes {
                excluded: Vec::new(),
                excluded_prefixes: Vec::new(),
            },
        },
    )
}

/// Load the session's auto memory with any extra read-only tiers mounted
/// as overlays (e.g. Claude Code's memory at `~/.claude/projects/<slug>/
/// memory/` under `claudeCodeMemory`). Prime-agent's own memory wins on
/// filename collision; shadowed overlay entries are recorded so the prompt
/// can hide them.
///
/// Phase-1 compatibility shim; new callers use [`load_auto_memory_with`].
#[must_use]
pub fn load_auto_memory_with_overlays(
    agent_dir: &Path,
    cwd: &Path,
    overlay_sources: &[OverlaySource],
) -> AutoMemory {
    load_auto_memory_with(
        agent_dir,
        cwd,
        &AutoMemoryLoad {
            overlay_sources,
            directory_override: None,
            excludes: &MemoryExcludes::default(),
        },
    )
}

/// Load options for [`load_auto_memory_with`].
#[derive(Debug, Clone, Copy)]
pub struct AutoMemoryLoad<'a> {
    /// Read-only overlays mounted beside the primary tier.
    pub overlay_sources: &'a [OverlaySource],
    /// `autoMemoryDirectory`'s resolved value (see
    /// [`resolve_auto_memory_directory`]). When `None`, the primary tier
    /// lives at `<agent dir>/projects/<slug>/memory/`.
    pub directory_override: Option<&'a Path>,
    /// Read filter applied to every tier's file enumeration.
    pub excludes: &'a MemoryExcludes,
}

/// Load the session's auto memory with the full option set.
#[must_use]
pub fn load_auto_memory_with(
    agent_dir: &Path,
    cwd: &Path,
    options: &AutoMemoryLoad<'_>,
) -> AutoMemory {
    let dir = options
        .directory_override
        .map(normalize_nfc)
        .unwrap_or_else(|| memory_dir(agent_dir, cwd));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "pa_core::auto_memory",
            dir = %dir.display(),
            %error,
            "the auto-memory directory could not be created"
        );
    }
    let raw = std::fs::read_to_string(dir.join(MEMORY_INDEX)).unwrap_or_default();
    let (index, truncated) = cap_index(&raw);
    let primary_names = memory_file_names(&dir, options.excludes);
    let mut confirmed_names = primary_names.clone();
    let mut overlays: Vec<AutoMemoryOverlay> = Vec::new();
    // Confirmed overlays first, so their confirmed filenames shadow any
    // same-named proposals under them. This mirrors CC's rule that a
    // proposal is pending until promoted.
    for source in options
        .overlay_sources
        .iter()
        .filter(|s| s.kind == OverlayKind::Confirmed)
    {
        if let Some(overlay) = load_overlay(source, &confirmed_names, options.excludes) {
            let additions: Vec<String> = memory_file_names(&source.dir, options.excludes)
                .into_iter()
                .filter(|name| !confirmed_names.contains(name))
                .collect();
            confirmed_names.extend(additions);
            overlays.push(overlay);
        }
    }
    for source in options
        .overlay_sources
        .iter()
        .filter(|s| s.kind == OverlayKind::Draft)
    {
        if let Some(overlay) = load_overlay(source, &confirmed_names, options.excludes) {
            overlays.push(overlay);
        }
    }
    AutoMemory {
        dir,
        index,
        truncated,
        overlays,
    }
}

/// Load one overlay from disk; returns `None` when the directory does not
/// exist (a Claude Code install that has never written memory for this
/// project is silently skipped).
fn load_overlay(
    source: &OverlaySource,
    shadowing_names: &[String],
    excludes: &MemoryExcludes,
) -> Option<AutoMemoryOverlay> {
    if !source.dir.is_dir() {
        return None;
    }
    let raw = std::fs::read_to_string(source.dir.join(MEMORY_INDEX)).unwrap_or_default();
    let (index, truncated) = cap_index(&raw);
    let overlay_names = memory_file_names(&source.dir, excludes);
    let shadowed_files: Vec<String> = overlay_names
        .into_iter()
        .filter(|name| shadowing_names.iter().any(|p| p == name))
        .collect();
    if !shadowed_files.is_empty() {
        tracing::debug!(
            target: "pa_core::auto_memory",
            label = %source.label,
            dir = %source.dir.display(),
            kind = ?source.kind,
            shadowed = ?shadowed_files,
            "overlay memory files shadowed by a higher-authority tier"
        );
    }
    Some(AutoMemoryOverlay {
        label: source.label.clone(),
        dir: source.dir.clone(),
        index,
        truncated,
        shadowed_files,
        kind: source.kind,
    })
}

/// Names of `*.md` files directly in `dir`, excluding [`MEMORY_INDEX`]
/// itself and any name the filter drops. A dir that cannot be read reports
/// an empty set so the overlay still loads.
fn memory_file_names(dir: &Path, excludes: &MemoryExcludes) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name == MEMORY_INDEX {
                return None;
            }
            if !std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                return None;
            }
            if !excludes.allows(&name) {
                return None;
            }
            entry.file_type().ok().filter(std::fs::FileType::is_file)?;
            Some(name)
        })
        .collect()
}

/// The user's home directory as Claude Code's `-home-<user>` project slug
/// (`sanitize` of the home path). Returns `None` when the home directory
/// is not known on this platform.
#[must_use]
pub fn home_user_slug() -> Option<String> {
    pa_types::platform::home_dir().as_deref().map(sanitize)
}

/// Overlay sources for Claude Code's memory tiers at `cwd`, given Claude
/// Code's memory tree root (`CLAUDE_CODE_REMOTE_MEMORY_DIR` when set,
/// else `~/.claude`). Returns an empty vector when the root is not known.
///
/// Includes, in decreasing authority:
/// - `<memory_root>/projects/<same-slug>/memory` — project confirmed tier
/// - `<memory_root>/projects/<home-slug>/memory` — user confirmed tier
/// - `<memory_root>/agent-memory-local/` — cross-agent confirmed tier
/// - `<memory_root>/projects/<same-slug>/memory/proposals` — draft tier
///
/// A missing directory is silently skipped by `load_overlay`.
#[must_use]
pub fn claude_code_overlay_sources(memory_root: &Path, cwd: &Path) -> Vec<OverlaySource> {
    let project_slug = sanitize(&project_root(cwd));
    let project_memory = memory_root
        .join("projects")
        .join(&project_slug)
        .join("memory");
    let mut sources = vec![OverlaySource::confirmed(
        "Claude Code project memory",
        normalize_nfc(&project_memory),
    )];
    if let Some(home_slug) = home_user_slug() {
        if home_slug != project_slug {
            sources.push(OverlaySource::confirmed(
                "Claude Code user memory",
                normalize_nfc(&memory_root.join("projects").join(&home_slug).join("memory")),
            ));
        }
    }
    sources.push(OverlaySource::confirmed(
        "Claude Code agent-memory-local",
        normalize_nfc(&memory_root.join("agent-memory-local")),
    ));
    sources.push(OverlaySource::draft(
        "Claude Code pending proposals (DRAFT — not confirmed)",
        normalize_nfc(&project_memory.join("proposals")),
    ));
    sources
}

/// The index cut to the line cap, then to the byte cap at a line boundary,
/// with Claude Code's warning appended when anything was cut.
#[must_use]
pub fn cap_index(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (String::new(), false);
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let line_count = lines.len();
    let mut kept = lines[..line_count.min(MAX_INDEX_LINES)].join("\n");
    let mut cut_reason = (line_count > MAX_INDEX_LINES)
        .then(|| format!("{line_count} lines (limit: {MAX_INDEX_LINES})"));
    if kept.len() > MAX_INDEX_BYTES {
        let mut end = MAX_INDEX_BYTES;
        while !kept.is_char_boundary(end) {
            end -= 1;
        }
        let cut = kept[..end].rfind('\n').unwrap_or(end);
        kept.truncate(cut);
        cut_reason = Some(format!(
            "{} bytes (limit: {MAX_INDEX_BYTES})",
            trimmed.len()
        ));
    }
    match cut_reason {
        Some(reason) => (
            format!(
                "{kept}\n\n> WARNING: {MEMORY_INDEX} is {reason}. Only part of it was loaded. Keep \
                 index entries to one line under ~200 characters; move detail into topic files."
            ),
            true,
        ),
        None => (kept, false),
    }
}

/// The main checkout of `cwd`'s repository (a linked worktree's `.git`
/// file names `<main>/.git/worktrees/<name>`), else the nearest directory
/// holding `.git`, else `cwd` itself.
#[must_use]
pub fn project_root(cwd: &Path) -> PathBuf {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        if git.is_dir() {
            return dir.to_path_buf();
        }
        if git.is_file() {
            return worktree_main_checkout(&git).unwrap_or_else(|| dir.to_path_buf());
        }
    }
    cwd
}

fn worktree_main_checkout(git_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let gitdir = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let gitdir = git_file.parent()?.join(gitdir);
    // `<main>/.git/worktrees/<name>` -> `<main>`.
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let dot_git = worktrees.parent()?;
    let main = dot_git.parent()?;
    Some(std::fs::canonicalize(main).unwrap_or_else(|_| main.to_path_buf()))
}

/// Claude Code's project-directory naming, exactly: every UTF-16 code unit
/// that is not an ASCII letter or digit becomes `-` (JS `replace` over a
/// string, so a character outside the BMP becomes two); a name longer than
/// 200 keeps its head plus `-` and the base-36 absolute value of the path's
/// 32-bit string hash (`r*31 + c`, DJB2-like; CC's `sre`/`Math.abs().toString(36)`),
/// so `~/.claude/projects/<name>` is the directory Claude Code writes.
///
/// NFC normalization is applied to the input path's string before mapping
/// (`Vtt(...).normalize("NFC")` in CC): a repo whose name combines marks in
/// NFD form on disk still produces CC's NFC slug.
#[must_use]
pub fn sanitize(path: &Path) -> String {
    let text: String = path.to_string_lossy().nfc().collect();
    let units: Vec<u16> = text.encode_utf16().collect();
    let sanitized: String = units
        .iter()
        .map(|&unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LEN {
        return sanitized;
    }
    let hash = units.iter().fold(0_i32, |hash, &unit| {
        hash.wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit))
    });
    format!(
        "{}-{}",
        &sanitized[..MAX_SANITIZED_LEN],
        base36(hash.unsigned_abs())
    )
}

/// JS `Number.prototype.toString(36)` for a non-negative integer.
fn base36(mut value: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

const FRONTMATTER_EXAMPLE: &str = "\
```markdown
---
name: {{memory name}}
description: {{one-line description: used to decide relevance in future conversations, so be specific}}
type: {{user, feedback, project, reference}}
---

{{memory content: for feedback/project types, structure as the rule or fact, then **Why:** and **How to apply:** lines}}
```";

const TYPES_SECTION: &str = "\
## Types of memory

There are several discrete types of memory that you can store in your memory system:

<types>
<type>
    <name>user</name>
    <description>Contain information about the user's role, goals, responsibilities, and knowledge. Great user memories help you tailor your future behavior to the user's preferences and perspective. Your goal in reading and writing these memories is to build up an understanding of who the user is and how you can be most helpful to them specifically. For example, you should collaborate with a senior software engineer differently than a student who is coding for the very first time. Keep in mind, that the aim here is to be helpful to the user. Avoid writing memories about the user that could be viewed as a negative judgement or that are not relevant to the work you're trying to accomplish together.</description>
    <when_to_save>When you learn any details about the user's role, preferences, responsibilities, or knowledge</when_to_save>
    <how_to_use>When your work should be informed by the user's profile or perspective. For example, if the user is asking you to explain a part of the code, you should answer that question in a way that is tailored to the specific details that they will find most valuable or that helps them build their mental model in relation to domain knowledge they already have.</how_to_use>
    <examples>
    user: I'm a data scientist investigating what logging we have in place
    assistant: [saves user memory: user is a data scientist, currently focused on observability/logging]

    user: I've been writing Go for ten years but this is my first time touching the React side of this repo
    assistant: [saves user memory: deep Go expertise, new to React and this project's frontend; frame frontend explanations in terms of backend analogues]
    </examples>
</type>
<type>
    <name>feedback</name>
    <description>Guidance the user has given you about how to approach work, both what to avoid and what to keep doing. These are a very important type of memory to read and write as they allow you to remain coherent and responsive to the way you should approach work in the project. Record from failure AND success: if you only save corrections, you will avoid past mistakes but drift away from approaches the user has already validated, and may grow overly cautious.</description>
    <when_to_save>Any time the user corrects your approach (\"no not that\", \"don't\", \"stop doing X\") OR confirms a non-obvious approach worked (\"yes exactly\", \"perfect, keep doing that\", accepting an unusual choice without pushback). Corrections are easy to notice; confirmations are quieter, so watch for them. In both cases, save what is applicable to future conversations, especially if surprising or not obvious from the code. Include *why* so you can judge edge cases later.</when_to_save>
    <how_to_use>Let these memories guide your behavior so that the user does not need to offer the same guidance twice.</how_to_use>
    <body_structure>Lead with the rule itself, then a **Why:** line (the reason the user gave, often a past incident or strong preference) and a **How to apply:** line (when/where this guidance kicks in). Knowing *why* lets you judge edge cases instead of blindly following the rule.</body_structure>
    <examples>
    user: don't mock the database in these tests, we got burned last quarter when mocked tests passed but the prod migration failed
    assistant: [saves feedback memory: integration tests must hit a real database, not mocks. Reason: prior incident where mock/prod divergence masked a broken migration]

    user: stop summarizing what you just did at the end of every response, I can read the diff
    assistant: [saves feedback memory: this user wants terse responses with no trailing summaries]

    user: yeah the single bundled PR was the right call here, splitting this one would've just been churn
    assistant: [saves feedback memory: for refactors in this area, user prefers one bundled PR over many small ones. Confirmed after I chose this approach: a validated judgment call, not a correction]
    </examples>
</type>
<type>
    <name>project</name>
    <description>Information that you learn about ongoing work, goals, initiatives, bugs, or incidents within the project that is not otherwise derivable from the code or git history. Project memories help you understand the broader context and motivation behind the work the user is doing within this working directory.</description>
    <when_to_save>When you learn who is doing what, why, or by when. These states change relatively quickly so try to keep your understanding of this up to date. Always convert relative dates in user messages to absolute dates when saving (e.g., \"Thursday\" -> \"2026-03-05\"), so the memory remains interpretable after time passes.</when_to_save>
    <how_to_use>Use these memories to more fully understand the details and nuance behind the user's request and make better informed suggestions.</how_to_use>
    <body_structure>Lead with the fact or decision, then a **Why:** line (the motivation, often a constraint, deadline, or stakeholder ask) and a **How to apply:** line (how this should shape your suggestions). Project memories decay fast, so the why helps future-you judge whether the memory is still load-bearing.</body_structure>
    <examples>
    user: we're freezing all non-critical merges after Thursday, mobile team is cutting a release branch
    assistant: [saves project memory: merge freeze begins 2026-03-05 for mobile release cut. Flag any non-critical PR work scheduled after that date]

    user: the reason we're ripping out the old auth middleware is that legal flagged it for storing session tokens in a way that doesn't meet the new compliance requirements
    assistant: [saves project memory: auth middleware rewrite is driven by legal/compliance requirements around session token storage, not tech-debt cleanup; scope decisions should favor compliance over ergonomics]
    </examples>
</type>
<type>
    <name>reference</name>
    <description>Stores pointers to where information can be found in external systems. These memories allow you to remember where to look to find up-to-date information outside of the project directory.</description>
    <when_to_save>When you learn about resources in external systems and their purpose. For example, that bugs are tracked in a specific project in Linear or that feedback can be found in a specific Slack channel.</when_to_save>
    <how_to_use>When the user references an external system or information that may be in an external system.</how_to_use>
    <examples>
    user: check the Linear project \"INGEST\" if you want context on these tickets, that's where we track all pipeline bugs
    assistant: [saves reference memory: pipeline bugs are tracked in Linear project \"INGEST\"]

    user: the Grafana board at grafana.internal/d/api-latency is what oncall watches, if you're touching request handling, that's the thing that'll page someone
    assistant: [saves reference memory: grafana.internal/d/api-latency is the oncall latency dashboard; check it when editing request-path code]
    </examples>
</type>
</types>";

const NOT_TO_SAVE: &str = "\
## What NOT to save in memory

- Code patterns, conventions, architecture, file paths, or project structure: these can be derived by reading the current project state.
- Git history, recent changes, or who-changed-what: `git log` / `git blame` are authoritative.
- Debugging solutions or fix recipes: the fix is in the code; the commit message has the context.
- Anything already documented in AGENTS.md or CLAUDE.md files.
- Ephemeral task details: in-progress work, temporary state, current conversation context.

These exclusions apply even when the user explicitly asks you to save. If they ask you to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it: that is the part worth keeping.";

const WHEN_TO_ACCESS: &str = "\
## When to access memories
- When memories seem relevant, or the user references prior-conversation work.
- You MUST access memory when the user explicitly asks you to check, recall, or remember.
- If the user says to *ignore* or *not use* memory: Do not apply remembered facts, cite, compare against, or mention memory content.
- Memory records can become stale over time. Use memory as context for what was true at a given point in time. Before answering the user or building assumptions based solely on information in memory records, verify that the memory is still correct and up-to-date by reading the current state of the files or resources. If a recalled memory conflicts with current information, trust what you observe now, and update or remove the stale memory rather than acting on it.

## Before recommending from memory

A memory that names a specific function, file, or flag is a claim that it existed *when the memory was written*. It may have been renamed, removed, or never merged. Before recommending it:

- If the memory names a file path: check the file exists.
- If the memory names a function or flag: grep for it.
- If the user is about to act on your recommendation (not just asking about history), verify first.

\"The memory says X exists\" is not the same as \"X exists now.\"

A memory that summarizes repo state (activity logs, architecture snapshots) is frozen in time. If the user asks about *recent* or *current* state, prefer `git log` or reading the code over recalling the snapshot.

## Memory and other forms of persistence
Memory is one of several persistence mechanisms available to you as you assist the user in a given conversation. The distinction is often that memory can be recalled in future conversations and should not be used for persisting information that is only useful within the scope of the current conversation.
- When to use tasks instead of memory: when you need to break your work in the current conversation into discrete steps or keep track of your progress, use the `tasks` skill instead of saving to memory. Memory should be reserved for information that will be useful in future conversations.";

/// The prompt section teaching the model its memory (Claude Code's
/// `# auto memory` section, with the index appended).
#[must_use]
pub fn memory_prompt_section(memory: &AutoMemory) -> String {
    let mut section = render_memory_section(&MemorySection {
        title: "auto memory",
        dir: &memory.dir,
        index: &memory.index,
        extra_guidelines: &[],
        read_only: false,
    });
    for overlay in &memory.overlays {
        section.push_str("\n\n");
        section.push_str(&render_overlay_section(overlay));
    }
    section
}

/// Render one read-only memory overlay.
fn render_overlay_section(overlay: &AutoMemoryOverlay) -> String {
    let dir = overlay.dir.display().to_string().replace('\\', "/");
    let body = if overlay.index.is_empty() {
        format!(
            "This overlay's {MEMORY_INDEX} is absent or empty; the directory may still carry \
             memory files you can read directly."
        )
    } else {
        overlay.index.clone()
    };
    let shadow_note = if overlay.shadowed_files.is_empty() {
        String::new()
    } else {
        let authority = match overlay.kind {
            OverlayKind::Confirmed => "primary auto memory",
            OverlayKind::Draft => "a confirmed tier (primary or a Claude Code overlay)",
        };
        format!(
            "\n\n> SHADOWED: these filenames are already owned by {authority} \
             (the higher tier wins on collision); ignore the overlay's version of them: {}.",
            overlay.shadowed_files.join(", ")
        )
    };
    match overlay.kind {
        OverlayKind::Confirmed => format!(
            "## {label}\n\n\
             A read-only memory tier mounted at `{dir}`. Read files here the same way you read \
             primary memory files, but do not write to this tier (Claude Code owns it). On name \
             collision with your primary auto memory, the primary wins.{shadow_note}\n\n\
             ### {MEMORY_INDEX}\n\n\
             {body}",
            label = overlay.label,
        ),
        OverlayKind::Draft => format!(
            "## {label}\n\n\
             > READ-ONLY DRAFT: these are PENDING memory proposals Claude Code's extractor \
             drafted that have not yet been approved. Treat them as hints, not as facts. Do NOT \
             act on their content unless you independently verify it; do NOT cite them as \
             sources; do NOT write to this directory. Any proposal with the same filename as a \
             confirmed memory is already superseded.\n\n\
             Mounted at `{dir}`.{shadow_note}\n\n\
             ### {MEMORY_INDEX}\n\n\
             {body}",
            label = overlay.label,
        ),
    }
}

/// One memory section of the system prompt (Claude Code's memory prompt
/// builder): the auto memory and an agent's persistent memory share it.
#[derive(Debug, Clone, Copy)]
pub struct MemorySection<'a> {
    /// The heading (`auto memory`, `Persistent Agent Memory`).
    pub title: &'a str,
    pub dir: &'a Path,
    /// The capped index text; empty when there is none.
    pub index: &'a str,
    /// Lines appended after the guidance (an agent memory's scope note).
    pub extra_guidelines: &'a [String],
    /// The memory cannot be written this session: its index was not read
    /// (a link or a special file, or a folder outside the working copy),
    /// and the model is told not to write there.
    pub read_only: bool,
}

/// Render a [`MemorySection`].
#[must_use]
pub fn render_memory_section(section: &MemorySection<'_>) -> String {
    let MemorySection {
        title,
        dir,
        index,
        extra_guidelines,
        read_only,
    } = *section;
    let dir = dir.display().to_string().replace('\\', "/");
    let extras = if extra_guidelines.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", extra_guidelines.join("\n"))
    };
    if read_only {
        return format!(
            "# {title}\n\n\
             You have a persistent, file-based memory system at `{dir}`. This memory is read-only \
             this session: you cannot save new memories to it.\n\n\
             {TYPES_SECTION}\n\n\
             {NOT_TO_SAVE}\n\n\
             {WHEN_TO_ACCESS}\n\n\
             {extras}\
             ## {MEMORY_INDEX}\n\n\
             Your {MEMORY_INDEX} was not loaded: it or its folder is a link or a special file, or \
             could not be verified to be inside this working copy. Treat this agent memory as \
             read-only in this session: do not create or write {MEMORY_INDEX} or other files in \
             its folder."
        );
    }
    let index_body = if index.is_empty() {
        format!(
            "Your {MEMORY_INDEX} is currently empty. When you save new memories, they will appear \
             here."
        )
    } else {
        index.to_string()
    };
    format!(
        "# {title}\n\n\
         You have a persistent, file-based memory system at `{dir}`. This directory already \
         exists: write to it directly from Python (`pathlib.Path.write_text`) or with `edit` (do \
         not run mkdir or check for its existence).\n\n\
         You should build up this memory system over time so that future conversations can have \
         a complete picture of who the user is, how they'd like to collaborate with you, what \
         behaviors to avoid or repeat, and the context behind the work the user gives you.\n\n\
         If the user explicitly asks you to remember something, save it immediately as whichever \
         type fits best. If they ask you to forget something, find and remove the relevant \
         entry.\n\n\
         {TYPES_SECTION}\n\n\
         {NOT_TO_SAVE}\n\n\
         ## How to save memories\n\n\
         Saving a memory is a two-step process:\n\n\
         **Step 1**: write the memory to its own file (e.g., `user_role.md`, \
         `feedback_testing.md`) using this frontmatter format:\n\n\
         {FRONTMATTER_EXAMPLE}\n\n\
         **Step 2**: add a pointer to that file in `{MEMORY_INDEX}`. `{MEMORY_INDEX}` is an \
         index, not a memory: each entry should be one line, under ~150 characters: \
         `- [Title](file.md) - one-line hook`. It has no frontmatter. Never write memory content \
         directly into `{MEMORY_INDEX}`.\n\n\
         - `{MEMORY_INDEX}` is always loaded into your conversation context: lines after \
         {MAX_INDEX_LINES} will be truncated, so keep the index concise\n\
         - Keep the name, description, and type fields in memory files up-to-date with the \
         content\n\
         - Organize memory semantically by topic, not chronologically\n\
         - Update or remove memories that turn out to be wrong or outdated\n\
         - Do not write duplicate memories. First check if there is an existing memory you can \
         update before writing a new one.\n\n\
         {WHEN_TO_ACCESS}\n\n\
         {extras}\
         ## {MEMORY_INDEX}\n\n\
         {index_body}"
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn the_index_is_capped_by_lines_then_bytes_with_a_warning() {
        assert_eq!(cap_index("  \n"), (String::new(), false));
        assert_eq!(
            cap_index("- [a](a.md) - hook\n"),
            ("- [a](a.md) - hook".to_string(), false)
        );

        let long = (0..250).fold(String::new(), |mut text, i| {
            let _ = writeln!(text, "- line {i}");
            text
        });
        let (index, truncated) = cap_index(&long);
        assert!(truncated);
        assert!(index.contains("- line 199\n"));
        assert!(!index.contains("- line 200"));
        assert!(index.ends_with("move detail into topic files."));
        assert!(index.contains("is 250 lines (limit: 200)"));

        let wide = (0..100).fold(String::new(), |mut text, i| {
            let _ = writeln!(text, "{i:03} {}", "x".repeat(400));
            text
        });
        let (index, truncated) = cap_index(&wide);
        assert!(truncated);
        let body = index.split("\n\n> WARNING").next().unwrap();
        assert!(body.len() <= MAX_INDEX_BYTES);
        assert!(body.ends_with(&"x".repeat(400)), "cut at a line boundary");
        assert!(index.contains("bytes (limit: 25000)"));
    }

    #[test]
    fn worktrees_share_their_main_checkout_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let main = root.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(main.join("src")).unwrap();
        let worktree = root.join("wt");
        std::fs::create_dir_all(worktree.join("sub")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        assert_eq!(project_root(&main.join("src")), main);
        assert_eq!(project_root(&worktree.join("sub")), main);
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_root(&plain), plain);

        let agent = root.join("agent");
        assert_eq!(
            memory_dir(&agent, &worktree),
            memory_dir(&agent, &main.join("src"))
        );
        assert!(memory_dir(&agent, &main).ends_with("memory"));
    }

    #[test]
    fn sanitizing_matches_claude_code_and_bounds_long_names() {
        assert_eq!(sanitize(Path::new("/home/u/my.repo")), "-home-u-my-repo");
        let long = PathBuf::from(format!("/{}", "a".repeat(300)));
        // Claude Code 2.1.296 `OO()` on the same paths (node-computed).
        assert_eq!(
            sanitize(&long),
            format!("-{}-vtkmfl", "a".repeat(MAX_SANITIZED_LEN - 1))
        );
        assert_eq!(
            sanitize(Path::new("/home/u/caf\u{e9}/\u{1F600}x")),
            "-home-u-caf----x"
        );
        let nested = PathBuf::from(format!("/srv/{}", "ab/".repeat(80)));
        assert!(sanitize(&nested).ends_with("b-ab--fgo8qh"));
    }

    #[test]
    fn claude_code_memory_disabled_skips_claude_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = tmp.path().join("agent");

        // Prime-agent memory with one entry.
        let prime_dir = memory_dir(&agent, &cwd);
        std::fs::create_dir_all(&prime_dir).unwrap();
        std::fs::write(
            prime_dir.join(MEMORY_INDEX),
            "- [Prime](prime.md) - primary tier\n",
        )
        .unwrap();
        std::fs::write(prime_dir.join("prime.md"), "prime body\n").unwrap();

        // Claude memory sitting right there, unreferenced.
        let claude_dir = tmp.path().join("claude").join("projects");
        let claude_proj = claude_dir
            .join(sanitize(&project_root(&cwd)))
            .join("memory");
        std::fs::create_dir_all(&claude_proj).unwrap();
        std::fs::write(
            claude_proj.join(MEMORY_INDEX),
            "- [CC](cc.md) - claude tier\n",
        )
        .unwrap();
        std::fs::write(claude_proj.join("cc.md"), "cc body\n").unwrap();

        // Setting off: no overlays loaded, prime-agent index alone.
        let memory = load_auto_memory_with_overlays(&agent, &cwd, &[]);
        assert_eq!(memory.index, "- [Prime](prime.md) - primary tier");
        assert!(memory.overlays.is_empty());
        let section = memory_prompt_section(&memory);
        assert!(!section.contains("Claude Code project memory"));
        assert!(!section.contains("cc.md"));
    }

    #[test]
    fn claude_code_memory_enabled_merges_both_tiers_with_prime_winning_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = tmp.path().join("agent");

        // Prime-agent memory with a file name that also exists in Claude's tier.
        let prime_dir = memory_dir(&agent, &cwd);
        std::fs::create_dir_all(&prime_dir).unwrap();
        std::fs::write(
            prime_dir.join(MEMORY_INDEX),
            "- [Shared](shared.md) - prime version wins\n",
        )
        .unwrap();
        std::fs::write(prime_dir.join("shared.md"), "prime body\n").unwrap();

        // Claude project tier: same filename, different body + a distinct one.
        let claude_dir = tmp.path().join("claude");
        let proj_slug = sanitize(&project_root(&cwd));
        let claude_proj = claude_dir.join("projects").join(&proj_slug).join("memory");
        std::fs::create_dir_all(&claude_proj).unwrap();
        std::fs::write(
            claude_proj.join(MEMORY_INDEX),
            "- [Shared](shared.md) - claude version\n- [Only in CC](cc_only.md) - unique\n",
        )
        .unwrap();
        std::fs::write(claude_proj.join("shared.md"), "claude body\n").unwrap();
        std::fs::write(claude_proj.join("cc_only.md"), "unique body\n").unwrap();

        let overlay_sources = claude_code_overlay_sources(&claude_dir, &cwd);
        let memory = load_auto_memory_with_overlays(&agent, &cwd, &overlay_sources);

        // Primary unchanged.
        assert_eq!(memory.index, "- [Shared](shared.md) - prime version wins");

        // Overlay captured; shadowed_files names the collided one so the prompt
        // tells the model prime-agent wins. Non-existent user-tier overlay is
        // skipped silently (load_overlay returns None).
        let project_overlay = memory
            .overlays
            .iter()
            .find(|o| o.label == "Claude Code project memory")
            .expect("project overlay present");
        assert_eq!(project_overlay.dir, claude_proj);
        assert!(project_overlay.index.contains("Only in CC"));
        assert!(project_overlay.index.contains("claude version")); // body carried verbatim
        assert_eq!(
            project_overlay.shadowed_files,
            vec!["shared.md".to_string()]
        );

        // On-disk file reads: prime-agent's shared.md keeps its own contents.
        // (The agent will read overlay files from overlay.dir; the primary dir
        // still holds "prime body", which is the collision winner.)
        let prime_body = std::fs::read_to_string(prime_dir.join("shared.md")).unwrap();
        assert_eq!(prime_body, "prime body\n");

        // Rendered section carries both tiers + the SHADOWED note.
        let section = memory_prompt_section(&memory);
        assert!(section.contains("# auto memory"));
        assert!(section.contains("## Claude Code project memory"));
        assert!(section.contains("SHADOWED"));
        assert!(section.contains("shared.md"));
        assert!(section.contains("Only in CC"));
    }

    #[test]
    fn loading_creates_the_directory_and_reads_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = tmp.path().join("agent");
        let memory = load_auto_memory(&agent, &cwd);
        assert!(memory.dir.is_dir());
        assert!(memory.index.is_empty());
        let section = memory_prompt_section(&memory);
        assert!(
            section.starts_with(
                "# auto memory\n\nYou have a persistent, file-based memory system at `"
            )
        );
        assert!(section.ends_with(
            "Your MEMORY.md is currently empty. When you save new memories, they will appear here."
        ));
        assert!(section.contains("<name>feedback</name>"));

        std::fs::write(
            memory.index_path(),
            "- [Role](user_role.md) - data scientist\n",
        )
        .unwrap();
        let memory = load_auto_memory(&agent, &cwd);
        assert_eq!(memory.index, "- [Role](user_role.md) - data scientist");
        assert!(
            memory_prompt_section(&memory)
                .ends_with("## MEMORY.md\n\n- [Role](user_role.md) - data scientist")
        );
    }

    #[test]
    fn excludes_drop_exact_names_and_prefix_matches_on_every_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = tmp.path().join("agent");
        let prime_dir = memory_dir(&agent, &cwd);
        std::fs::create_dir_all(&prime_dir).unwrap();
        for name in [
            "keep.md",
            "draft.md",
            "draft_a.md",
            "draft_b.md",
            "private_secret.md",
        ] {
            std::fs::write(prime_dir.join(name), "body\n").unwrap();
        }
        let excludes = MemoryExcludes {
            excluded: vec!["draft.md".to_string()],
            excluded_prefixes: vec!["draft_".to_string(), "private_".to_string()],
        };
        let kept = memory_file_names(&prime_dir, &excludes);
        assert_eq!(kept, vec!["keep.md".to_string()]);
        let empty = MemoryExcludes::default();
        let all = memory_file_names(&prime_dir, &empty);
        assert_eq!(all.len(), 5);
        assert!(excludes.allows("other.md"));
        assert!(!excludes.allows("draft.md"));
        assert!(!excludes.allows("draft_x.md"));
    }

    #[test]
    fn directory_override_precedence_policy_wins_then_flag_then_user() {
        let _env = crate::packages::test_support::lock_env();
        struct Policy(Option<PathBuf>);
        impl AutoMemoryPolicy for Policy {
            fn directory_override(&self) -> Option<PathBuf> {
                self.0.clone()
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let policy_dir = tmp.path().join("policy");
        let flag_dir = tmp.path().join("flag");
        let user_dir = tmp.path().join("user");

        // Env unset, user unset: None.
        unsafe {
            std::env::remove_var(DIRECTORY_FLAG_ENV);
        }
        assert_eq!(
            resolve_auto_memory_directory(None, &agent, None),
            None,
            "all unset returns the default (None)"
        );
        // User alone (absolute).
        assert_eq!(
            resolve_auto_memory_directory(None, &agent, user_dir.to_str()),
            Some(user_dir.clone())
        );
        // User alone (relative → joined under agent_dir).
        assert_eq!(
            resolve_auto_memory_directory(None, &agent, Some("rel/subdir")),
            Some(agent.join("rel/subdir"))
        );
        // Flag beats user.
        unsafe {
            std::env::set_var(DIRECTORY_FLAG_ENV, &flag_dir);
        }
        assert_eq!(
            resolve_auto_memory_directory(None, &agent, user_dir.to_str()),
            Some(flag_dir.clone())
        );
        // Policy beats flag.
        let policy = Policy(Some(policy_dir.clone()));
        assert_eq!(
            resolve_auto_memory_directory(Some(&policy), &agent, user_dir.to_str()),
            Some(policy_dir.clone())
        );
        // A policy that returns None falls through to flag.
        let policy_none = Policy(None);
        assert_eq!(
            resolve_auto_memory_directory(Some(&policy_none), &agent, user_dir.to_str()),
            Some(flag_dir.clone())
        );
        // A relative flag is refused and the resolver falls to user.
        unsafe {
            std::env::set_var(DIRECTORY_FLAG_ENV, "relative/path");
        }
        assert_eq!(
            resolve_auto_memory_directory(None, &agent, user_dir.to_str()),
            Some(user_dir.clone())
        );
        unsafe {
            std::env::remove_var(DIRECTORY_FLAG_ENV);
        }
    }

    #[test]
    fn directory_override_relocates_the_primary_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = tmp.path().join("agent");
        let override_dir = tmp.path().join("override");
        std::fs::create_dir_all(&override_dir).unwrap();
        std::fs::write(
            override_dir.join(MEMORY_INDEX),
            "- [Override](o.md) - override tier\n",
        )
        .unwrap();
        let memory = load_auto_memory_with(
            &agent,
            &cwd,
            &AutoMemoryLoad {
                overlay_sources: &[],
                directory_override: Some(&override_dir),
                excludes: &MemoryExcludes::default(),
            },
        );
        assert_eq!(memory.dir, normalize_nfc(&override_dir));
        assert_eq!(memory.index, "- [Override](o.md) - override tier");
    }

    #[test]
    fn claude_overlay_sources_include_agent_memory_local_and_proposals() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let memory_root = tmp.path().join("claude");
        let sources = claude_code_overlay_sources(&memory_root, &cwd);
        // Project confirmed is first.
        assert_eq!(sources[0].kind, OverlayKind::Confirmed);
        assert!(sources[0].dir.ends_with("memory"));
        // One of the sources is agent-memory-local (confirmed).
        let has_aml = sources
            .iter()
            .any(|s| s.dir.ends_with("agent-memory-local") && s.kind == OverlayKind::Confirmed);
        assert!(has_aml, "agent-memory-local overlay missing");
        // Proposals come last, as Draft.
        assert_eq!(sources.last().unwrap().kind, OverlayKind::Draft);
        assert!(sources.last().unwrap().dir.ends_with("proposals"));
    }

    #[test]
    fn proposals_overlay_is_shadowed_by_both_prime_and_confirmed_overlays() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent = tmp.path().join("agent");

        let prime_dir = memory_dir(&agent, &cwd);
        std::fs::create_dir_all(&prime_dir).unwrap();
        std::fs::write(prime_dir.join("primary.md"), "prime\n").unwrap();

        let confirmed_dir = tmp.path().join("confirmed");
        std::fs::create_dir_all(&confirmed_dir).unwrap();
        std::fs::write(confirmed_dir.join("confirmed_only.md"), "c\n").unwrap();

        let draft_dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&draft_dir).unwrap();
        // Three draft entries: one shadowed by prime, one by confirmed, one unique.
        for name in ["primary.md", "confirmed_only.md", "proposal_only.md"] {
            std::fs::write(draft_dir.join(name), "d\n").unwrap();
        }

        let sources = vec![
            OverlaySource::confirmed("Confirmed", confirmed_dir.clone()),
            OverlaySource::draft("Drafts", draft_dir.clone()),
        ];
        let memory = load_auto_memory_with(
            &agent,
            &cwd,
            &AutoMemoryLoad {
                overlay_sources: &sources,
                directory_override: None,
                excludes: &MemoryExcludes::default(),
            },
        );
        assert_eq!(memory.overlays.len(), 2);
        let confirmed = memory
            .overlays
            .iter()
            .find(|o| o.kind == OverlayKind::Confirmed)
            .expect("confirmed overlay present");
        assert!(
            confirmed.shadowed_files.is_empty(),
            "confirmed is not shadowed by prime because filenames differ"
        );
        let drafts = memory
            .overlays
            .iter()
            .find(|o| o.kind == OverlayKind::Draft)
            .expect("draft overlay present");
        let mut shadowed = drafts.shadowed_files.clone();
        shadowed.sort();
        assert_eq!(
            shadowed,
            vec!["confirmed_only.md".to_string(), "primary.md".to_string()]
        );

        // Rendering carries the DRAFT marker, not the confirmed overlay's
        // regular line.
        let section = memory_prompt_section(&memory);
        assert!(section.contains("READ-ONLY DRAFT"));
        assert!(section.contains("PENDING memory proposals"));
    }

    #[test]
    fn nfc_normalization_of_slugs_matches_cc() {
        // NFD "é" (e + U+0301 combining acute) must collapse to the same
        // slug CC produces from the NFC form (U+00E9). Each non-alnum unit
        // becomes '-'; NFC collapses combining pairs so the same path
        // string produces the same byte-length slug.
        let nfc = sanitize(Path::new("/home/caf\u{e9}"));
        let nfd = sanitize(Path::new("/home/cafe\u{0301}"));
        assert_eq!(nfc, nfd);
        assert_eq!(nfc, "-home-caf-");
    }

    #[test]
    fn memory_dir_is_nfc_normalized() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().to_path_buf();
        let nfd_cwd = tmp.path().join("cafe\u{0301}");
        std::fs::create_dir_all(&nfd_cwd).unwrap();
        let dir = memory_dir(&agent, &nfd_cwd);
        let text = dir.to_string_lossy();
        // No NFD combining-acute survives in the memory dir path.
        assert!(
            !text.contains('\u{0301}'),
            "NFD mark leaked into memory_dir: {text}"
        );
    }

    #[test]
    fn remote_memory_dir_env_redirects_overlay_root() {
        let _env = crate::packages::test_support::lock_env();
        // Sanity: ClaudePaths::resolve honors the env var.
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        unsafe {
            std::env::set_var(crate::settings::claude_code::REMOTE_MEMORY_DIR_ENV, &remote);
        }
        let paths = crate::settings::claude_code::ClaudePaths::resolve(tmp.path());
        assert_eq!(paths.memory_root(), Some(remote.as_path()));
        unsafe {
            std::env::remove_var(crate::settings::claude_code::REMOTE_MEMORY_DIR_ENV);
        }
    }

    #[test]
    fn remote_memory_dir_env_refuses_relative_and_empty() {
        let _env = crate::packages::test_support::lock_env();
        unsafe {
            std::env::set_var(
                crate::settings::claude_code::REMOTE_MEMORY_DIR_ENV,
                "relative/path",
            );
        }
        let paths = crate::settings::claude_code::ClaudePaths::resolve(Path::new("/tmp"));
        // Relative value refused; falls back to claude_dir.
        assert_eq!(paths.memory_root(), paths.claude_dir.as_deref());
        unsafe {
            std::env::set_var(crate::settings::claude_code::REMOTE_MEMORY_DIR_ENV, "");
        }
        let paths = crate::settings::claude_code::ClaudePaths::resolve(Path::new("/tmp"));
        assert_eq!(paths.memory_root(), paths.claude_dir.as_deref());
        unsafe {
            std::env::remove_var(crate::settings::claude_code::REMOTE_MEMORY_DIR_ENV);
        }
    }
}
