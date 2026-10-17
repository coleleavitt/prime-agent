//! Instruction files: the AGENTS.md / CLAUDE.md family a session loads into
//! its prompt, with Claude Code's loading rules.
//!
//! Discovery, in load order (later files take precedence when they
//! disagree, so the nearest directory is last):
//!
//! 1. User scope: `<agent dir>/AGENTS.md` (first of `AGENTS.md`, `AGENTS.MD`,
//!    `CLAUDE.md`, `CLAUDE.MD`), then `<agent dir>/rules/**/*.md`; with
//!    `claudeCodeUserInstructions` on, also `~/.claude/CLAUDE.md` and
//!    `~/.claude/rules/**/*.md`.
//! 2. Every directory from the filesystem root down to the working
//!    directory: `AGENTS.md` (or `AGENTS.MD`), `CLAUDE.md` (or `CLAUDE.MD`),
//!    `.prime/agent/AGENTS.md`, `.claude/CLAUDE.md`, the rules under
//!    `.prime/agent/rules/` and `.claude/rules/`, then the private
//!    `AGENTS.local.md` and `CLAUDE.local.md`. The home directory's own
//!    `.claude/` is user scope, never read as a project directory.
//!
//! Each file's `@path` imports load right after it ([`imports`]), up to
//! [`imports::MAX_IMPORT_DEPTH`] hops, each file at most once (a cycle or a
//! file named twice loads once). Project and local files import only from
//! inside the directory they configure, so a checked-out repository cannot
//! pull `~/.ssh` into the prompt; user-scope files import from anywhere.
//! A file whose text repeats one already loaded (a copied CLAUDE.md beside
//! its AGENTS.md) is skipped.
//!
//! Rules with `paths:` frontmatter, and the instruction files of
//! directories below the working directory, load later, when the agent
//! first touches a file they cover ([`NestedInstructions`]).

pub mod imports;
pub mod rules;

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use self::rules::RuleMatcher;

/// The most bytes read from one instruction file; the rest is cut with a note.
pub const MAX_INSTRUCTION_FILE_BYTES: usize = 256 * 1024;

const AGENTS_FAMILY: [&str; 2] = ["AGENTS.md", "AGENTS.MD"];
const CLAUDE_FAMILY: [&str; 2] = ["CLAUDE.md", "CLAUDE.MD"];
const GLOBAL_FAMILY: [&str; 4] = ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];
const LOCAL_FILES: [&str; 2] = ["AGENTS.local.md", "CLAUDE.local.md"];
const CLAUDE_DIR: &str = ".claude";
const RULES_DIR: &str = "rules";

/// Whose instructions a file holds (Claude Code's memory types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionKind {
    /// The user's own, for every project (the agent dir, `~/.claude`).
    User,
    /// Checked into a project directory.
    Project,
    /// The user's private notes for one project (`*.local.md`).
    Local,
}

impl InstructionKind {
    /// Claude Code's description of the kind, shown beside each file.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            InstructionKind::User => "user's private global instructions for all projects",
            InstructionKind::Project => "project instructions, checked into the codebase",
            InstructionKind::Local => "user's private project instructions, not checked in",
        }
    }

    /// The wire spelling (`user` | `project` | `local`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            InstructionKind::User => "user",
            InstructionKind::Project => "project",
            InstructionKind::Local => "local",
        }
    }
}

/// One loaded instruction file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub kind: InstructionKind,
    /// The file's text (frontmatter stripped for rules); its imports are
    /// separate entries right after it.
    pub content: String,
    /// The file whose `@path` imported this one.
    pub parent: Option<PathBuf>,
    /// A conditional rule's `paths:` globs (empty: always loaded).
    pub globs: Vec<String>,
}

/// Where to look.
#[derive(Debug, Clone, Default)]
pub struct InstructionOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// The user's home (`~/` imports, the `~/.claude` user scope).
    pub home_dir: Option<PathBuf>,
    /// Read `~/.claude/CLAUDE.md` and `~/.claude/rules/` as user scope
    /// (the `claudeCodeUserInstructions` setting; off by default, so a
    /// Claude Code install does not silently change this agent's prompt).
    pub claude_user_files: bool,
}

/// A conditional rule waiting for a matching path.
#[derive(Debug, Clone)]
struct DeferredRule {
    path: PathBuf,
    kind: InstructionKind,
    base: PathBuf,
    globs: Vec<String>,
    matcher: RuleMatcher,
}

/// The loader's memory: what loaded, what waits.
#[derive(Debug, Default)]
struct LoadState {
    /// Canonical paths of every file loaded (or deliberately skipped).
    processed: HashSet<PathBuf>,
    /// Hashes of loaded contents (a copied file loads once).
    contents: HashSet<u64>,
    /// Directories whose instruction files were looked at.
    visited_dirs: HashSet<PathBuf>,
    deferred: Vec<DeferredRule>,
    /// Files delivered after session start, in delivery order.
    delivered: Vec<PathBuf>,
}

/// How a file came to load: its kind, the directory its imports must stay
/// in (`None`: anywhere), and whether it is a rule (frontmatter parsed).
#[derive(Debug, Clone, Copy)]
struct Origin<'a> {
    kind: InstructionKind,
    import_base: Option<&'a Path>,
    rule: bool,
}

impl<'a> Origin<'a> {
    fn plain(kind: InstructionKind, import_base: Option<&'a Path>) -> Self {
        Self {
            kind,
            import_base,
            rule: false,
        }
    }
}

#[derive(Debug)]
struct Loader {
    options: InstructionOptions,
    state: LoadState,
}

impl Loader {
    fn new(options: InstructionOptions) -> Self {
        Self {
            options,
            state: LoadState::default(),
        }
    }

    fn home(&self) -> Option<&Path> {
        self.options.home_dir.as_deref()
    }

    fn is_home(&self, dir: &Path) -> bool {
        self.home()
            .is_some_and(|home| canonical(home) == canonical(dir))
    }

    /// The session-start set.
    fn load_initial(&mut self) -> Vec<InstructionFile> {
        let mut out = Vec::new();
        let agent_dir = self.options.agent_dir.clone();
        if let Some(path) = first_existing(&agent_dir, &GLOBAL_FAMILY) {
            self.load_file(
                &path,
                Origin::plain(InstructionKind::User, None),
                0,
                None,
                &mut out,
            );
        }
        self.load_rules_dir(
            &agent_dir.join(RULES_DIR),
            InstructionKind::User,
            &agent_dir,
            &mut out,
        );
        if self.options.claude_user_files {
            if let Some(home) = self.options.home_dir.clone() {
                let claude = home.join(CLAUDE_DIR);
                self.load_file(
                    &claude.join("CLAUDE.md"),
                    Origin::plain(InstructionKind::User, None),
                    0,
                    None,
                    &mut out,
                );
                self.load_rules_dir(
                    &claude.join(RULES_DIR),
                    InstructionKind::User,
                    &home,
                    &mut out,
                );
            }
        }
        let cwd = self.options.cwd.clone();
        let mut ancestors: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
        ancestors.reverse();
        for dir in ancestors {
            self.load_dir(&dir, &mut out);
        }
        out
    }

    /// One directory's instruction files, in the documented order.
    fn load_dir(&mut self, dir: &Path, out: &mut Vec<InstructionFile>) {
        if !self.state.visited_dirs.insert(canonical(dir)) {
            return;
        }
        let base = Some(dir);
        for family in [&AGENTS_FAMILY[..], &CLAUDE_FAMILY[..]] {
            if let Some(path) = first_existing(dir, family) {
                self.load_file(
                    &path,
                    Origin::plain(InstructionKind::Project, base),
                    0,
                    None,
                    out,
                );
            }
        }
        let prime_dir = dir.join(crate::settings::CONFIG_DIR_NAME);
        self.load_file(
            &prime_dir.join("AGENTS.md"),
            Origin::plain(InstructionKind::Project, base),
            0,
            None,
            out,
        );
        let claude_dir = dir.join(CLAUDE_DIR);
        // The home directory's `.claude/` is Claude Code's user scope: read
        // only when the user opted in, and then as user files (above).
        let project_claude = !self.is_home(dir);
        if project_claude {
            self.load_file(
                &claude_dir.join("CLAUDE.md"),
                Origin::plain(InstructionKind::Project, base),
                0,
                None,
                out,
            );
        }
        self.load_rules_dir(
            &prime_dir.join(RULES_DIR),
            InstructionKind::Project,
            dir,
            out,
        );
        if project_claude {
            self.load_rules_dir(
                &claude_dir.join(RULES_DIR),
                InstructionKind::Project,
                dir,
                out,
            );
        }
        for name in LOCAL_FILES {
            self.load_file(
                &dir.join(name),
                Origin::plain(InstructionKind::Local, base),
                0,
                None,
                out,
            );
        }
    }

    /// A rules directory: unconditional rules load now, conditional ones wait.
    fn load_rules_dir(
        &mut self,
        rules_dir: &Path,
        kind: InstructionKind,
        base: &Path,
        out: &mut Vec<InstructionFile>,
    ) {
        let import_base = (kind != InstructionKind::User).then_some(base);
        for path in rules::rule_files(rules_dir) {
            let Some(text) = read_instruction_text(&path) else {
                continue;
            };
            let (frontmatter, _) = crate::skills::frontmatter::parse_frontmatter(&text);
            let globs = rules::frontmatter_globs(&frontmatter);
            if globs.is_empty() {
                let origin = Origin {
                    kind,
                    import_base,
                    rule: true,
                };
                self.load_file(&path, origin, 0, None, out);
                continue;
            }
            if self.state.processed.contains(&canonical(&path)) {
                continue;
            }
            if let Some(matcher) = RuleMatcher::new(base, &globs) {
                self.state.deferred.push(DeferredRule {
                    path,
                    kind,
                    base: base.to_path_buf(),
                    globs,
                    matcher,
                });
            }
        }
    }

    /// Load one file (when it exists and is new) and its imports.
    fn load_file(
        &mut self,
        path: &Path,
        origin: Origin<'_>,
        depth: usize,
        parent: Option<&Path>,
        out: &mut Vec<InstructionFile>,
    ) {
        let Origin {
            kind,
            import_base,
            rule: is_rule,
        } = origin;
        if !path.is_file() {
            return;
        }
        let key = canonical(path);
        if !self.state.processed.insert(key) {
            return;
        }
        let Some(text) = read_instruction_text(path) else {
            return;
        };
        let (globs, content) = if is_rule {
            let (frontmatter, body) = crate::skills::frontmatter::parse_frontmatter(&text);
            (rules::frontmatter_globs(&frontmatter), body)
        } else {
            (Vec::new(), text)
        };
        let content = content.trim_end().to_string();
        if content.trim().is_empty() || !self.state.contents.insert(content_hash(&content)) {
            return;
        }
        let targets = imports::import_targets(&content);
        out.push(InstructionFile {
            path: path.to_path_buf(),
            kind,
            content,
            parent: parent.map(Path::to_path_buf),
            globs: globs.clone(),
        });
        if depth >= imports::MAX_IMPORT_DEPTH {
            return;
        }
        for target in targets {
            let Some(resolved) = imports::resolve_import(&target, path, self.home()) else {
                continue;
            };
            let resolved = lexical(&resolved);
            if let Some(base) = import_base {
                if !canonical(&resolved).starts_with(canonical(base)) {
                    tracing::debug!(
                        target: "pa_core::instructions",
                        import = %resolved.display(),
                        from = %path.display(),
                        "an import outside the project directory is not loaded"
                    );
                    continue;
                }
            }
            let before = out.len();
            let imported = Origin {
                rule: false,
                ..origin
            };
            self.load_file(&resolved, imported, depth + 1, Some(path), out);
            // An imported file of a conditional rule is conditional with it.
            for file in &mut out[before..] {
                if file.globs.is_empty() {
                    file.globs.clone_from(&globs);
                }
            }
        }
    }

    /// The files a touched path brings in: the instruction files of the
    /// directories between the working directory and the path, then the
    /// waiting rules the path matches.
    fn touch(&mut self, path: &Path) -> Vec<InstructionFile> {
        let mut out = Vec::new();
        let path = canonical(&absolute(&self.options.cwd, path));
        let cwd = canonical(&self.options.cwd);
        if path != cwd && path.starts_with(&cwd) {
            let dir_path: &Path = if path.is_dir() {
                &path
            } else {
                path.parent().unwrap_or(&cwd)
            };
            let depth = dir_path
                .strip_prefix(&cwd)
                .map_or(0, |rest| rest.components().count());
            let mut dirs: Vec<PathBuf> = dir_path
                .ancestors()
                .take(depth)
                .map(Path::to_path_buf)
                .collect();
            dirs.reverse();
            for dir in dirs {
                self.load_dir(&dir, &mut out);
            }
        }
        let matching: Vec<DeferredRule> = self
            .state
            .deferred
            .iter()
            .filter(|rule| rule.matcher.matches(&path))
            .cloned()
            .collect();
        for rule in matching {
            let origin = Origin {
                kind: rule.kind,
                import_base: (rule.kind != InstructionKind::User).then_some(rule.base.as_path()),
                rule: true,
            };
            self.load_file(&rule.path, origin, 0, None, &mut out);
        }
        let processed = &self.state.processed;
        self.state
            .deferred
            .retain(|rule| !processed.contains(&canonical(&rule.path)));
        out
    }
}

/// Load the session-start instruction files.
#[must_use]
pub fn load_instructions(
    options: InstructionOptions,
) -> (Vec<InstructionFile>, NestedInstructions) {
    let mut loader = Loader::new(options);
    let files = loader.load_initial();
    (
        files,
        NestedInstructions {
            loader: Mutex::new(loader),
        },
    )
}

/// The instruction files a session has not loaded yet: those of directories
/// below the working directory and the conditional rules, delivered the
/// first time the agent touches a file they cover (Claude Code's nested
/// memory). One per session; the loader remembers what it delivered.
#[derive(Debug)]
pub struct NestedInstructions {
    loader: Mutex<Loader>,
}

impl NestedInstructions {
    /// The files `paths` bring in, each delivered once per session.
    pub fn touch<P: AsRef<Path>>(&self, paths: &[P]) -> Vec<InstructionFile> {
        let mut loader = self
            .loader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for path in paths {
            out.extend(loader.touch(path.as_ref()));
        }
        loader
            .state
            .delivered
            .extend(out.iter().map(|file| file.path.clone()));
        out
    }

    /// The files delivered after session start (a resumed session's
    /// earlier deliveries first), in order (for `/memory`).
    #[must_use]
    pub fn delivered(&self) -> Vec<PathBuf> {
        self.loader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .state
            .delivered
            .clone()
    }

    /// Record files an earlier run of this session already delivered (a
    /// resumed session's nested rows), so they are not delivered again.
    pub fn mark_delivered<P: AsRef<Path>>(&self, paths: &[P]) {
        let mut loader = self
            .loader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for path in paths {
            let key = canonical(path.as_ref());
            loader.state.processed.insert(key.clone());
            loader
                .state
                .deferred
                .retain(|rule| canonical(&rule.path) != key);
            let path = path.as_ref().to_path_buf();
            if !loader.state.delivered.contains(&path) {
                loader.state.delivered.push(path);
            }
        }
    }

    /// Conditional rules still waiting (for `/memory`).
    #[must_use]
    pub fn pending_rules(&self) -> Vec<(PathBuf, Vec<String>)> {
        let loader = self
            .loader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loader
            .state
            .deferred
            .iter()
            .map(|rule| (rule.path.clone(), rule.globs.clone()))
            .collect()
    }
}

/// The text the model reads for files delivered mid-session (Claude Code's
/// nested-memory attachment wording).
#[must_use]
pub fn render_delivered(files: &[InstructionFile]) -> String {
    files
        .iter()
        .map(|file| {
            format!(
                "Contents of {} ({}):\n\n{}",
                file.path.display(),
                file.kind.description(),
                file.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// A text file's contents, capped; `None` for unreadable or binary files.
fn read_instruction_text(path: &Path) -> Option<String> {
    use std::fmt::Write as _;
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_INSTRUCTION_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let truncated = bytes.len() > MAX_INSTRUCTION_FILE_BYTES;
    bytes.truncate(MAX_INSTRUCTION_FILE_BYTES);
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        let _ = write!(
            text,
            "\n\n[truncated: the file is larger than {MAX_INSTRUCTION_FILE_BYTES} bytes]"
        );
    }
    Some(text)
}

fn content_hash(content: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.trim().hash(&mut hasher);
    hasher.finish()
}

fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The canonical path when it resolves, else the lexically normalized one
/// (a deleted or not-yet-created file still compares).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical(path))
}

/// `path` with `.` and `..` components folded away (no filesystem access).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests;
