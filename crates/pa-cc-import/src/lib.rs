//! Claude Code sessions in prime-agent (`pa-cc-import`).
//!
//! Claude Code records every session as a JSONL transcript under
//! `~/.claude/projects/<sanitized cwd>/<uuid>.jsonl` (`CLAUDE_CONFIG_DIR`
//! moves `~/.claude`). This crate lists those sessions beside prime-agent's
//! saved ones (through the native `pa_core::session::external_sessions`
//! seam: the daemon's `list_external_sessions` and the resume selector) and
//! imports one into a new prime-agent session file to continue it, leaving
//! the transcript untouched. See `README.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_core::session::external_sessions::{
    ExternalMatch,
    ExternalScope,
    ExternalSessionRow,
    ExternalSessionSource,
    ImportedSession,
};
use pa_core::session::manager::{format_iso_now, get_session_file_path};
use pa_types::session::FileEntry;

mod convert;
pub mod listing;
mod transcript;

pub use convert::{IMPORT_NOTE_CUSTOM_TYPE, IMPORTED_FROM_KEY, TOOL_RESULT_CUSTOM_TYPE};
pub use listing::{SessionSummary, find_by_id, list_all, list_project, project_dir, summarize};

/// The source id rows and imported headers carry.
pub const SOURCE_ID: &str = "claude-code";
/// The label the agents view groups the rows under.
pub const SOURCE_LABEL: &str = "Claude Code";

/// Claude Code's projects directory: `$CLAUDE_CONFIG_DIR/projects`, else
/// `~/.claude/projects`. A test process may never resolve the real one.
#[must_use]
pub fn projects_root() -> Option<PathBuf> {
    let root = pa_core::skills::claude_commands::user_claude_dir()?.join("projects");
    pa_types::platform::test_isolation::guard_state_path("Claude Code transcripts", &root);
    Some(root)
}

/// Install the Claude Code session source (once, before any listing).
pub fn install() {
    pa_core::session::external_sessions::install(Arc::new(ClaudeCodeSessions::default()));
}

/// The Claude Code session source; `root` overrides the projects directory
/// (tests), `None` resolves it at each call.
#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeSessions {
    pub root: Option<PathBuf>,
}

impl ClaudeCodeSessions {
    fn root(&self) -> Option<PathBuf> {
        self.root.clone().or_else(projects_root)
    }
}

fn row(summary: SessionSummary) -> ExternalSessionRow {
    ExternalSessionRow {
        source: SOURCE_ID.to_string(),
        source_label: SOURCE_LABEL.to_string(),
        id: summary.id,
        path: summary.path,
        cwd: summary.cwd,
        title: summary.title,
        first_prompt: summary.prompt,
        created: summary.created,
        modified: summary.modified,
        git_branch: summary.git_branch,
        message_count: summary.message_count,
    }
}

/// A session id or a unique id prefix (at least 8 characters).
fn looks_like_id(selector: &str) -> bool {
    selector.len() >= 8
        && selector.len() <= 36
        && selector.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Resolve a resume selector: an exact session id (any project), a unique
/// id prefix in this project, then an exact title (custom or AI title,
/// ignoring case; this project first, then every project).
#[must_use]
pub fn find(root: &Path, selector: &str, cwd: &Path) -> ExternalMatch {
    let selector = selector.trim();
    if selector.is_empty() {
        return ExternalMatch::None;
    }
    if looks_like_id(selector) {
        if selector.len() == 36 {
            if let Some(found) = find_by_id(root, selector).and_then(|path| summarize(&path)) {
                return ExternalMatch::One(Box::new(row(found)));
            }
        }
        let prefixed: Vec<SessionSummary> = list_project(root, cwd)
            .into_iter()
            .filter(|summary| summary.id.starts_with(selector))
            .collect();
        match prefixed.as_slice() {
            [] => {}
            [only] => return ExternalMatch::One(Box::new(row(only.clone()))),
            many => return ExternalMatch::Many(many.len()),
        }
    }
    let wanted = selector.to_lowercase();
    let titled = |rows: Vec<SessionSummary>| -> Vec<SessionSummary> {
        rows.into_iter()
            .filter(|summary| {
                [&summary.custom_title, &summary.ai_title]
                    .into_iter()
                    .flatten()
                    .any(|title| title.trim().to_lowercase() == wanted)
            })
            .collect()
    };
    let mut matches = titled(list_project(root, cwd));
    if matches.is_empty() {
        matches = titled(list_all(root));
    }
    match matches.as_slice() {
        [] => ExternalMatch::None,
        [only] => ExternalMatch::One(Box::new(row(only.clone()))),
        many => ExternalMatch::Many(many.len()),
    }
}

/// An earlier import of `id` at `leaf_uuid` in `session_dir` (its header's
/// `importedFrom`), so reopening an unchanged session never duplicates it.
fn earlier_import(session_dir: &Path, id: &str, leaf_uuid: &str) -> Option<ImportedSession> {
    let read = std::fs::read_dir(session_dir).ok()?;
    for entry in read.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(header) = pa_core::session::manager::read_session_header(&path) else {
            continue;
        };
        let Some(source) = header.rest.get(IMPORTED_FROM_KEY) else {
            continue;
        };
        let field = |key: &str| source.get(key).and_then(serde_json::Value::as_str);
        if field("source") == Some(SOURCE_ID)
            && field("sessionId") == Some(id)
            && field("leafUuid") == Some(leaf_uuid)
        {
            return Some(ImportedSession {
                path,
                session_id: header.id,
                cwd: header.cwd,
                reused: true,
            });
        }
    }
    None
}

fn write_session(path: &Path, entries: &[FileEntry]) -> std::io::Result<()> {
    use std::io::Write;

    let mut content = String::new();
    for entry in entries {
        content.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
        content.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = PathBuf::from(format!("{}.tmp{}", path.display(), std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        pa_core::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    pa_core::platform::rename_onto(&temp, path)
}

/// Import the transcript at `transcript` into a new session file under
/// `session_dir` (or reuse an earlier import of the same conversation).
///
/// # Errors
///
/// The transcript is unreadable or holds no conversation, or the session
/// file cannot be written.
pub fn import_transcript(transcript: &Path, session_dir: &Path) -> Result<ImportedSession, String> {
    let id = transcript
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .ok_or_else(|| format!("Not a Claude Code transcript: {}", transcript.display()))?;
    let parsed = transcript::Transcript::read(transcript, transcript::Detail::Full)
        .map_err(|error| format!("Could not read {}: {error}", transcript.display()))?;
    let leaf = parsed
        .leaf()
        .ok_or_else(|| format!("Claude Code session {id} has no conversation to continue"))?;
    if let Some(found) = earlier_import(session_dir, &id, &leaf.uuid) {
        return Ok(found);
    }
    let session_id = uuid::Uuid::now_v7().to_string();
    let path_text = transcript.display().to_string();
    let converted = convert::convert(
        &parsed,
        &convert::Source {
            session_id: &id,
            path: &path_text,
        },
        &session_id,
        &format_iso_now(),
    )
    .ok_or_else(|| format!("Claude Code session {id} has no conversation to continue"))?;
    let path = get_session_file_path(session_dir, &session_id);
    write_session(&path, &converted.entries)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    tracing::info!(
        target: "pa_cc_import",
        source = %id,
        leaf = %converted.leaf_uuid,
        session = %session_id,
        "imported a Claude Code session"
    );
    Ok(ImportedSession {
        path,
        session_id,
        cwd: converted.cwd,
        reused: false,
    })
}

impl ExternalSessionSource for ClaudeCodeSessions {
    fn source(&self) -> &'static str {
        SOURCE_ID
    }

    fn label(&self) -> &'static str {
        SOURCE_LABEL
    }

    fn list(&self, scope: &ExternalScope) -> Vec<ExternalSessionRow> {
        let Some(root) = self.root() else {
            return Vec::new();
        };
        let rows = match scope {
            ExternalScope::Directory(cwd) => list_project(&root, cwd),
            ExternalScope::All => list_all(&root),
        };
        rows.into_iter().map(row).collect()
    }

    fn find(&self, selector: &str, cwd: &Path) -> ExternalMatch {
        match self.root() {
            Some(root) => find(&root, selector, cwd),
            None => ExternalMatch::None,
        }
    }

    fn import(&self, id: &str, session_dir: &Path) -> Result<ImportedSession, String> {
        let root = self
            .root()
            .ok_or_else(|| "Claude Code's projects directory is unknown".to_string())?;
        if !looks_like_id(id) || id.len() != 36 {
            return Err(format!("Not a Claude Code session id: {id}"));
        }
        let transcript =
            find_by_id(&root, id).ok_or_else(|| format!("Unknown Claude Code session {id}"))?;
        import_transcript(&transcript, session_dir)
    }
}
