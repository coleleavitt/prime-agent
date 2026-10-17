//! The external-session seam: sessions another agent product recorded on
//! this machine, listed beside the saved catalog and imported into a new
//! session file on open.
//!
//! Nothing is installed in the native product: every listing is empty and
//! every import fails as unknown. The composition root installs each source,
//! once, before any session, worker or listing runs; native crates never
//! name an implementation. A source never writes into the other product's
//! files: an import reads them and writes one new session file into the
//! session directory it is given.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

/// Which external sessions a listing covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalScope {
    /// The sessions recorded for this working directory.
    Directory(PathBuf),
    /// Every project's sessions.
    All,
}

/// One external session as a listing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSessionRow {
    /// The source's id (`ExternalSessionSource::source`).
    pub source: String,
    /// The source's display label (`ExternalSessionSource::label`).
    pub source_label: String,
    /// The session's id in its own product.
    pub id: String,
    /// The session's file (read only).
    pub path: PathBuf,
    /// The directory the session worked in.
    pub cwd: String,
    /// The session's title by the source's own priority, when it has one.
    pub title: Option<String>,
    /// The prompt that stands in for a missing title.
    pub first_prompt: Option<String>,
    /// ISO-8601 timestamps.
    pub created: String,
    pub modified: String,
    pub git_branch: Option<String>,
    /// The user and assistant messages on the session's conversation path.
    pub message_count: u64,
}

/// A session an import wrote (or found already written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSession {
    /// The new session file.
    pub path: PathBuf,
    pub session_id: String,
    /// The imported session's working directory.
    pub cwd: String,
    /// An earlier import of the same, unchanged session was reused.
    pub reused: bool,
}

/// How a resume selector matched a source's sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalMatch {
    None,
    One(Box<ExternalSessionRow>),
    /// Several sessions match; the count.
    Many(usize),
}

/// One external session source.
pub trait ExternalSessionSource: Send + Sync {
    /// The stable id rows and imports carry (`claude-code`).
    fn source(&self) -> &str;
    /// The label a list groups the rows under.
    fn label(&self) -> &str;
    /// The sessions in scope, newest first. Never fails: unreadable files
    /// are skipped.
    fn list(&self, scope: &ExternalScope) -> Vec<ExternalSessionRow>;
    /// Resolve a resume selector (an id, or an exact title) against the
    /// sessions, this directory's first.
    fn find(&self, selector: &str, cwd: &Path) -> ExternalMatch;
    /// Convert session `id` into a new session file under `session_dir`
    /// (or reuse an earlier import of the same, unchanged session).
    ///
    /// # Errors
    ///
    /// The session is unknown or unreadable, or the file cannot be written.
    fn import(&self, id: &str, session_dir: &Path) -> Result<ImportedSession, String>;
}

fn registry() -> &'static RwLock<Vec<Arc<dyn ExternalSessionSource>>> {
    static SOURCES: OnceLock<RwLock<Vec<Arc<dyn ExternalSessionSource>>>> = OnceLock::new();
    SOURCES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Install one source; a second install of the same source id replaces it
/// in place (the first installed answers a resume selector first).
pub fn install(source: Arc<dyn ExternalSessionSource>) {
    let mut sources = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match sources
        .iter()
        .position(|known| known.source() == source.source())
    {
        Some(index) => sources[index] = source,
        None => sources.push(source),
    }
}

/// The installed sources, in installation order.
#[must_use]
pub fn sources() -> Vec<Arc<dyn ExternalSessionSource>> {
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Whether any source is installed (the daemon advertises the listing only
/// then).
#[must_use]
pub fn installed() -> bool {
    !sources().is_empty()
}

/// The installed source with this id.
#[must_use]
pub fn source(id: &str) -> Option<Arc<dyn ExternalSessionSource>> {
    sources().into_iter().find(|source| source.source() == id)
}

/// Every installed source's rows in scope, newest first.
#[must_use]
pub fn list_all(scope: &ExternalScope) -> Vec<ExternalSessionRow> {
    let mut rows: Vec<ExternalSessionRow> = sources()
        .iter()
        .flat_map(|source| source.list(scope))
        .collect();
    rows.sort_by(|a, b| b.modified.cmp(&a.modified));
    rows
}

/// Import `id` from the source `source`.
///
/// # Errors
///
/// The source is not installed, or its import failed.
pub fn import(source_id: &str, id: &str, session_dir: &Path) -> Result<ImportedSession, String> {
    let Some(source) = source(source_id) else {
        return Err(format!("Unknown session source \"{source_id}\""));
    };
    source.import(id, session_dir)
}

/// Resolve a resume selector against every installed source: the first
/// source with a match answers.
#[must_use]
pub fn find(selector: &str, cwd: &Path) -> Option<(Arc<dyn ExternalSessionSource>, ExternalMatch)> {
    sources()
        .into_iter()
        .find_map(|source| match source.find(selector, cwd) {
            ExternalMatch::None => None,
            found => Some((source, found)),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str, &'static str);

    impl ExternalSessionSource for Stub {
        fn source(&self) -> &'static str {
            self.0
        }

        fn label(&self) -> &'static str {
            "Stub"
        }

        fn list(&self, _scope: &ExternalScope) -> Vec<ExternalSessionRow> {
            vec![ExternalSessionRow {
                source: self.0.to_string(),
                source_label: "Stub".to_string(),
                id: "s1".to_string(),
                path: PathBuf::from("/x/s1.jsonl"),
                cwd: "/x".to_string(),
                title: None,
                first_prompt: Some("hi".to_string()),
                created: self.1.to_string(),
                modified: self.1.to_string(),
                git_branch: None,
                message_count: 2,
            }]
        }

        fn find(&self, selector: &str, _cwd: &Path) -> ExternalMatch {
            if selector == "s1" {
                ExternalMatch::One(Box::new(self.list(&ExternalScope::All).remove(0)))
            } else {
                ExternalMatch::None
            }
        }

        fn import(&self, id: &str, session_dir: &Path) -> Result<ImportedSession, String> {
            Ok(ImportedSession {
                path: session_dir.join(format!("{id}.jsonl")),
                session_id: id.to_string(),
                cwd: "/x".to_string(),
                reused: false,
            })
        }
    }

    // One test owns the process-wide registry (no other test in this
    // binary installs a source).
    #[test]
    fn installed_sources_list_find_and_import() {
        install(Arc::new(Stub("stub-a", "2026-01-01T00:00:00.000Z")));
        install(Arc::new(Stub("stub-b", "2026-02-01T00:00:00.000Z")));
        // A reinstall replaces the source with the same id.
        install(Arc::new(Stub("stub-a", "2026-01-02T00:00:00.000Z")));
        assert!(installed());
        let rows = list_all(&ExternalScope::All);
        let order: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.source.as_str(), row.modified.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("stub-b", "2026-02-01T00:00:00.000Z"),
                ("stub-a", "2026-01-02T00:00:00.000Z"),
            ]
        );
        let (source, found) = find("s1", Path::new("/x")).expect("a match");
        assert_eq!(source.source(), "stub-a");
        assert!(matches!(found, ExternalMatch::One(row) if row.id == "s1"));
        assert!(find("nope", Path::new("/x")).is_none());
        let imported = import("stub-b", "s1", Path::new("/sessions")).unwrap();
        assert_eq!(imported.path, PathBuf::from("/sessions/s1.jsonl"));
        assert_eq!(
            import("missing", "s1", Path::new("/sessions")).unwrap_err(),
            "Unknown session source \"missing\""
        );
    }
}
