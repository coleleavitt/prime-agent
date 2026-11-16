//! The magic-context source: the active rows of `CortexKit` magic-context's
//! `memories` table, read-only. Archived rows (merged, superseded, expired)
//! are magic-context's own forgetting and are never loaded.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::corpus::{BrainDoc, Source};

/// The columns the brain reads; pinned so a newer magic-context schema with
/// more columns still loads.
const ACTIVE_ROWS: &str = "SELECT id, project_path, category, content, updated_at \
     FROM memories WHERE status = 'active' AND (expires_at IS NULL OR expires_at > ?1)";

/// The active rows of the database at `db`.
pub(crate) fn load(db: &Path) -> rusqlite::Result<Vec<BrainDoc>> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(500))?;
    conn.pragma_update(None, "query_only", true)?;
    let now_ms = i64::try_from(pa_ledger::now_millis()).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(ACTIVE_ROWS)?;
    let rows = statement.query_map([now_ms], |row| {
        let id: i64 = row.get(0)?;
        let project: String = row.get(1)?;
        let category: String = row.get(2)?;
        let content: String = row.get(3)?;
        let updated_ms: i64 = row.get(4)?;
        Ok(BrainDoc {
            key: format!("mc:{id}"),
            source: Source::MagicContext,
            memory_id: None,
            session_id: None,
            project: Some(project),
            slug: None,
            title: category.to_lowercase().replace('_', " "),
            content,
            path: format!("magic-context/memories/{id}"),
            category: Some(category),
            updated_at: pa_ledger::iso_from_millis(u64::try_from(updated_ms).unwrap_or(0)),
            declared_superseded_by: None,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database with magic-context's `memories` columns the brain reads,
    /// plus one it does not, to pin that extra columns are ignored.
    fn database(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("context.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
               id INTEGER PRIMARY KEY, project_path TEXT NOT NULL, category TEXT NOT NULL,
               content TEXT NOT NULL, updated_at INTEGER NOT NULL, status TEXT DEFAULT 'active',
               expires_at INTEGER, superseded_by_memory_id INTEGER, seen_count INTEGER);
             INSERT INTO memories (id, project_path, category, content, updated_at, status, expires_at)
               VALUES (1, 'git:abc', 'CONFIG_VALUES', 'daemon socket under /run', 1794424683956, 'active', NULL),
                      (2, 'git:abc', 'ARCHITECTURE', 'old layout', 1, 'archived', NULL),
                      (3, 'git:def', 'KNOWN_ISSUES', 'expired issue', 1, 'active', 2);",
        )
        .unwrap();
        path
    }

    #[test]
    fn only_live_active_rows_load_as_documents() {
        let dir = tempfile::tempdir().unwrap();
        let docs = load(&database(dir.path())).unwrap();
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc.key, "mc:1");
        assert_eq!(doc.source, Source::MagicContext);
        assert_eq!(doc.project.as_deref(), Some("git:abc"));
        assert_eq!(doc.title, "config values");
        assert_eq!(doc.category.as_deref(), Some("CONFIG_VALUES"));
        assert!(doc.updated_at.starts_with("2026-11-"));
    }

    #[test]
    fn a_missing_database_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("absent.db")).is_err());
    }
}
