//! The Claude Code session list: one row per top-level transcript in a
//! project directory (`<projects>/<sanitized cwd>/<uuid>.jsonl`), skipping
//! subagent (sidechain) transcripts and files without a conversation, like
//! Claude Code's resume picker. Rows are cached per file by size and
//! modification time, so a repeated listing re-reads only changed files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use pa_core::resources::auto_memory::sanitize;
use pa_core::session::manager::format_iso;

use crate::transcript::{Detail, Transcript};

/// One Claude Code session as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session uuid (the file stem).
    pub id: String,
    pub path: PathBuf,
    /// The working directory the transcript records ("" when none).
    pub cwd: String,
    /// The named title: agent name, custom title, AI title, summary.
    pub title: Option<String>,
    /// The last (else the first) prompt, for an untitled session.
    pub prompt: Option<String>,
    pub created: String,
    pub modified: String,
    pub git_branch: Option<String>,
    pub message_count: u64,
    /// The custom-title / ai-title (the titles a resume selector matches).
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
}

impl SessionSummary {
    /// The title a list shows: the named title, else the prompt.
    #[must_use]
    pub fn display_title(&self) -> Option<&str> {
        self.title.as_deref().or(self.prompt.as_deref())
    }
}

fn mtime(metadata: &std::fs::Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

fn iso(time: SystemTime) -> String {
    let millis = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        });
    format_iso(millis)
}

/// Summarize one transcript file; `None` for a sidechain transcript or one
/// with no conversation.
#[must_use]
pub fn summarize(path: &Path) -> Option<SessionSummary> {
    type Cache = Mutex<HashMap<PathBuf, (u64, Option<SystemTime>, Option<SessionSummary>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let metadata = std::fs::metadata(path).ok()?;
    let key = (metadata.len(), mtime(&metadata));
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some((size, time, row)) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
    {
        if (*size, *time) == key {
            return row.clone();
        }
    }
    let row = summarize_uncached(path, &metadata);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf(), (key.0, key.1, row.clone()));
    row
}

fn summarize_uncached(path: &Path, metadata: &std::fs::Metadata) -> Option<SessionSummary> {
    let transcript = Transcript::read(path, Detail::Listing).ok()?;
    if transcript.is_sidechain() {
        return None;
    }
    let conversation = transcript.conversation();
    if conversation.is_empty() {
        return None;
    }
    let file_time = mtime(metadata).map(iso).unwrap_or_default();
    let (created, modified) = transcript.time_span().map_or_else(
        || (file_time.clone(), file_time.clone()),
        |(first, last)| (first.to_string(), last.to_string()),
    );
    Some(SessionSummary {
        id: path.file_stem()?.to_string_lossy().to_string(),
        path: path.to_path_buf(),
        cwd: transcript.cwd().unwrap_or_default().to_string(),
        title: transcript.title().map(str::to_string),
        prompt: transcript.prompt_title(),
        created,
        modified,
        git_branch: transcript.git_branch().map(str::to_string),
        message_count: conversation
            .iter()
            .filter(|entry| entry.counts_as_message())
            .count() as u64,
        custom_title: transcript.custom_title.clone(),
        ai_title: transcript.ai_title.clone(),
    })
}

/// The transcripts directly in `dir` (subagent transcripts live in
/// subdirectories and are never listed).
fn transcripts_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                && path.file_stem().is_some_and(|stem| stem.len() == 36)
                && path.is_file()
        })
        .collect()
}

fn newest_first(mut rows: Vec<SessionSummary>) -> Vec<SessionSummary> {
    rows.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// Claude Code's project directory for `cwd`.
#[must_use]
pub fn project_dir(projects_root: &Path, cwd: &Path) -> PathBuf {
    projects_root.join(sanitize(cwd))
}

/// The sessions recorded for `cwd`, newest first. A transcript whose own
/// cwd is another directory (a sanitized-name collision) is left out.
#[must_use]
pub fn list_project(projects_root: &Path, cwd: &Path) -> Vec<SessionSummary> {
    let rows = transcripts_in(&project_dir(projects_root, cwd))
        .iter()
        .filter_map(|path| summarize(path))
        .filter(|row| {
            row.cwd.is_empty() || pa_types::platform::same_directory(Path::new(&row.cwd), cwd)
        })
        .collect();
    newest_first(rows)
}

/// Every project's sessions, newest first.
#[must_use]
pub fn list_all(projects_root: &Path) -> Vec<SessionSummary> {
    let Ok(read) = std::fs::read_dir(projects_root) else {
        return Vec::new();
    };
    let rows = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .flat_map(|dir| transcripts_in(&dir))
        .filter_map(|path| summarize(&path))
        .collect();
    newest_first(rows)
}

/// The transcript of session `id` in any project.
#[must_use]
pub fn find_by_id(projects_root: &Path, id: &str) -> Option<PathBuf> {
    let file = format!("{id}.jsonl");
    std::fs::read_dir(projects_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&file))
        .find(|path| path.is_file())
}
