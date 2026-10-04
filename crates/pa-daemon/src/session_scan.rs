//! The saved-session roster scan (`list_sessions`), gated by a bounded
//! first-line header read: a parseable non-`session` first record skips the
//! file, while an unparseable or blank first line leaves the fold to decide
//! (TS never invalidates on a parse failure).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pa_core::session::catalog_cache::{
    self, CatalogEntry, CatalogFile, CatalogFileKey, CatalogSessionRow, CatalogUsage,
    SessionCatalogCache,
};

use crate::session_store::{
    parse_session_header_line, read_first_line_bounded_from, read_session_info_from, SessionInfo,
    SESSION_LIST_HEADER_READ_MAX_BYTES,
};
use crate::session_usage::SessionUsageSummary;

/// The version of the rules that derive a [`SessionInfo`] from a session
/// file's content (the scan fold and the row build). A catalog cache serves
/// only rows recorded under this exact version: bump it on any change to
/// what a file lists as, or cached rows keep the old rules.
const SESSION_ROW_VERSION: u32 = 1;

/// The bounded header read's verdict for one roster file.
enum HeaderGate {
    /// The complete first line is a valid `session` header: the fold fills
    /// the row.
    Header,
    /// A parseable first record that is not the `session` header: TS marks
    /// the file invalid (`acc.invalid`) — skip it without the fold.
    NotAHeader,
    /// The first line does not end within the bound (an over-long header, an
    /// unreadable file): the fold decides.
    Unjudged,
}

fn bounded_header_gate(file: &mut fs::File) -> HeaderGate {
    let Some(line) = read_first_line_bounded_from(file, SESSION_LIST_HEADER_READ_MAX_BYTES) else {
        return HeaderGate::Unjudged;
    };
    let Ok(text) = std::str::from_utf8(&line) else {
        // A full read of the file would fail on the same bytes (`read_to_string`).
        return HeaderGate::NotAHeader;
    };
    if text.trim().is_empty() {
        // A blank first line judges nothing: the fold skips blank lines and
        // may find the header on a later one.
        return HeaderGate::Unjudged;
    }
    // Parse failures never invalidate the file (TS `foldSessionScanLine`):
    // a later `session` header may still produce the row, so the fold decides.
    if serde_json::from_str::<serde_json::Value>(text).is_err() {
        return HeaderGate::Unjudged;
    }
    // A parseable first record that is not the `session` header marks the
    // file invalid (TS `acc.invalid`): the gate skips it without the fold.
    if parse_session_header_line(text).is_some() {
        HeaderGate::Header
    } else {
        HeaderGate::NotAHeader
    }
}

/// One file's roster row: `None` when the file produces none. One open serves
/// the gate and the fold, pinning both to the same inode (a rename racing two
/// handles could judge one file and fold another).
fn roster_session_info(path: &Path) -> Option<SessionInfo> {
    let mut file = fs::File::open(path).ok()?;
    match bounded_header_gate(&mut file) {
        HeaderGate::NotAHeader => None,
        HeaderGate::Header | HeaderGate::Unjudged => read_session_info_from(&mut file, path),
    }
}

/// List every valid session file in a directory, most recently modified first.
/// The rich-field fold runs sequentially: a measured parallel fold loses to
/// cross-core cacheline/futex costs on a loaded box (425ms vs 137ms over 1412 files).
#[must_use]
pub fn list_sessions(session_dir: &Path) -> Vec<SessionInfo> {
    list_sessions_with(session_dir, |_, _, _| true)
}

/// [`list_sessions`] with the saved-catalog stream's per-file callback:
/// `on_row` receives rows as each fold completes, so rows reach the client
/// DURING the scan; the metadata pass runs first, so the first row is the
/// newest session. `false` stops the scan.
pub fn list_sessions_with(
    session_dir: &Path,
    on_row: impl FnMut(usize, usize, &SessionInfo) -> bool,
) -> Vec<SessionInfo> {
    list_sessions_through(session_dir, catalog_cache::installed(), on_row)
}

/// [`list_sessions_with`] over an explicit catalog cache (`None` folds every
/// file).
fn list_sessions_through(
    session_dir: &Path,
    cache: Option<&dyn SessionCatalogCache>,
    mut on_row: impl FnMut(usize, usize, &SessionInfo) -> bool,
) -> Vec<SessionInfo> {
    let Ok(read) = fs::read_dir(session_dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, SystemTime, Option<CatalogFileKey>)> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|path| {
            let metadata = fs::metadata(&path).ok()?;
            let modified = metadata.modified().ok()?;
            Some((path, modified, CatalogFileKey::from_metadata(&metadata)))
        })
        .collect();
    files.sort_by_key(|(_, modified, _)| std::cmp::Reverse(*modified));
    let total = files.len();
    let listed: Vec<PathBuf> = match cache {
        Some(_) => files.iter().map(|(path, _, _)| path.clone()).collect(),
        None => Vec::new(),
    };
    let mut infos = Vec::new();
    for (index, (path, _, key)) in files.into_iter().enumerate() {
        let info = match (cache, key) {
            (Some(cache), Some(key)) => cached_roster_session_info(
                cache,
                &CatalogFile {
                    session_dir,
                    path: &path,
                    key,
                    fold_version: SESSION_ROW_VERSION,
                },
            ),
            (None, _) | (Some(_), None) => roster_session_info(&path),
        };
        if let Some(info) = info {
            // `false` stops the scan: the consumer is gone, so the remaining
            // folds serve nobody — the scan returns the rows it has.
            if !on_row(index, total, &info) {
                return infos;
            }
            infos.push(info);
        }
    }
    if let Some(cache) = cache {
        cache.scan_finished(session_dir, &listed);
    }
    infos
}

/// One file's row through the catalog cache: a hit is served without the
/// fold; a miss folds and records the result when the file's key held
/// across the fold (a racing append must never be cached under the old key).
fn cached_roster_session_info(
    cache: &dyn SessionCatalogCache,
    file: &CatalogFile<'_>,
) -> Option<SessionInfo> {
    match cache.lookup(file) {
        Some(CatalogEntry::Session(row)) => return Some(info_from_row(file.path, *row)),
        Some(CatalogEntry::NotASession) => return None,
        None => {}
    }
    let info = roster_session_info(file.path);
    let unchanged = fs::metadata(file.path)
        .ok()
        .and_then(|metadata| CatalogFileKey::from_metadata(&metadata))
        == Some(file.key);
    if unchanged {
        let entry = match &info {
            Some(info) => CatalogEntry::Session(Box::new(row_from_info(info))),
            None => CatalogEntry::NotASession,
        };
        cache.record(file, entry);
    }
    info
}

fn row_from_info(info: &SessionInfo) -> CatalogSessionRow {
    CatalogSessionRow {
        id: info.id.clone(),
        cwd: info.cwd.clone(),
        name: info.name.clone(),
        state: info.state.clone(),
        model: info.model.clone(),
        thinking_level: info.thinking_level.clone(),
        parent_session_path: info.parent_session_path.clone(),
        rlm_depth: info.rlm_depth,
        created: info.created.clone(),
        modified: info.modified.clone(),
        message_count: info.message_count,
        first_message: info.first_message.clone(),
        all_messages_text: info.all_messages_text.clone(),
        usage: info.usage.as_ref().map(|usage| CatalogUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost: usage.cost,
        }),
    }
}

fn info_from_row(path: &Path, row: CatalogSessionRow) -> SessionInfo {
    let CatalogSessionRow {
        id,
        cwd,
        name,
        state,
        model,
        thinking_level,
        parent_session_path,
        rlm_depth,
        created,
        modified,
        message_count,
        first_message,
        all_messages_text,
        usage,
    } = row;
    SessionInfo {
        path: path.to_path_buf(),
        id,
        cwd,
        name,
        state,
        model,
        thinking_level,
        parent_session_path,
        rlm_depth,
        created,
        modified,
        message_count,
        first_message,
        all_messages_text,
        usage: usage.map(|usage| SessionUsageSummary {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost: usage.cost,
        }),
        // Ledger-derived, never the file's: the listing arm attaches it.
        deleted_descendant_usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{read_session_info, session_file_name, SessionFile};
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pa-daemon-scan-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write one valid session file with `message_count` user/assistant turns
    /// and return its path. The rewrite stamps the filesystem mtime.
    fn write_session(dir: &Path, cwd: &str, name: Option<&str>, message_count: usize) -> PathBuf {
        let mut session = SessionFile::create(cwd, None, 0);
        if let Some(name) = name {
            session.append_session_info(name);
        }
        for turn in 0..message_count {
            session.append_message(&json!({
                "role": "user", "content": format!("user {turn}"), "timestamp": (turn + 1) as u64
            }));
            session.append_message(&json!({
                "role": "assistant", "content": format!("assistant {turn}"),
                "provider": "p", "model": "m", "timestamp": (turn + 1) as u64
            }));
        }
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        session.rewrite().unwrap();
        path
    }

    /// The scan this module replaced: sequential folds in directory order, stable-sorted by
    /// mtime. The scan's oracle.
    fn sequential_list_sessions(session_dir: &Path) -> Vec<SessionInfo> {
        let Ok(read) = fs::read_dir(session_dir) else {
            return Vec::new();
        };
        let mut infos: Vec<(SessionInfo, SystemTime)> = read
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .filter_map(|path| {
                let modified = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
                read_session_info(&path).map(|info| (info, modified))
            })
            .collect();
        infos.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        infos.into_iter().map(|(info, _)| info).collect()
    }

    #[test]
    fn rows_match_the_sequential_fold_and_order() {
        let dir = temp_dir();
        write_session(&dir, "/repo/a", Some("alpha"), 3);
        write_session(&dir, "/repo/b", None, 1);
        write_session(&dir, "/repo/c", Some("gamma"), 12);
        let scanned = list_sessions(&dir);
        assert_eq!(scanned, sequential_list_sessions(&dir));
        assert_eq!(scanned.len(), 3);
        assert_eq!(scanned[0].name.as_deref(), Some("gamma"));
    }

    #[test]
    fn empty_dir_lists_nothing() {
        let dir = temp_dir();
        assert!(list_sessions(&dir).is_empty());
    }

    #[test]
    fn missing_dir_lists_nothing() {
        assert!(list_sessions(&temp_dir().join("absent")).is_empty());
    }

    #[test]
    fn skips_a_file_whose_first_parseable_line_is_not_the_session_header() {
        let dir = temp_dir();
        // A session header preceded by a parseable non-session record: TS marks
        // the file invalid (`acc.invalid`) — skipped even though a header follows.
        let mistyped = dir.join("mistyped.jsonl");
        let mut session = SessionFile::create("/repo/mistyped", None, 0);
        session.set_path(mistyped.clone());
        session.rewrite().unwrap();
        let header_line = fs::read_to_string(&mistyped).unwrap();
        fs::write(
            &mistyped,
            format!("{{\"type\":\"message\",\"id\":\"x1\"}}\n{header_line}"),
        )
        .unwrap();
        // An unparseable first line with no header anywhere is no skip either: the fold decides
        // and finds no row.
        let foreign = dir.join("foreign.jsonl");
        fs::write(&foreign, "not a session file at all\n").unwrap();
        assert!(list_sessions(&dir).is_empty());
    }

    #[test]
    fn still_lists_a_file_with_an_unparseable_first_line_and_a_later_header() {
        let dir = temp_dir();
        // TS never invalidates on a parse failure: a truncated or foreign
        // first line must not hide a recoverable session.
        let path = write_session(&dir, "/repo/junk-first", None, 1);
        let content = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("not a session file at all\n{content}")).unwrap();
        let scanned = list_sessions(&dir);
        assert_eq!(scanned, sequential_list_sessions(&dir));
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].cwd, "/repo/junk-first");
    }

    #[test]
    fn still_lists_a_file_with_a_leading_blank_line() {
        let dir = temp_dir();
        let path = write_session(&dir, "/repo/blank-first", Some("blank"), 1);
        let content = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("\n{content}")).unwrap();
        let scanned = list_sessions(&dir);
        assert_eq!(scanned, sequential_list_sessions(&dir));
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].name.as_deref(), Some("blank"));
    }

    #[test]
    fn still_lists_a_file_with_an_over_long_header_line() {
        let dir = temp_dir();
        // A cwd long enough to push the serialized header past the 512-byte
        // bound: the bounded read refuses to judge the line, and the row
        // must still come out exactly as the fold produces it.
        let long_cwd = format!("/repo/{}", "x".repeat(600));
        write_session(&dir, &long_cwd, Some("wide"), 1);
        let scanned = list_sessions(&dir);
        assert_eq!(scanned.len(), 1);
        assert_eq!(
            scanned[0],
            read_session_info(&dir.join(session_file_name(&scanned[0].id))).unwrap()
        );
        assert_eq!(scanned[0].cwd, long_cwd);
    }

    #[test]
    // Runs on demand: `cargo test -p pa-daemon --release -- --ignored roster_scan_wall_clock
    // --nocapture`.
    #[ignore = "wall-clock probe, not a correctness test: prints timings of a real sessions dir (PA_ROSTER_BENCH_DIR)"]
    fn roster_scan_wall_clock() {
        let dir = match std::env::var_os("PA_ROSTER_BENCH_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => Path::new(&std::env::var_os("HOME").unwrap_or_default())
                .join(".prime/agent/sessions"),
        };
        let warm = list_sessions(&dir);
        eprintln!("roster_scan_wall_clock: {} rows (warm pass)", warm.len());
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let infos = list_sessions(&dir);
            eprintln!(
                "roster_scan_wall_clock: {} rows in {:?}",
                infos.len(),
                started.elapsed()
            );
        }
    }

    #[test]
    fn list_sessions_with_emits_rows_newest_first_with_scan_counts() {
        let dir = temp_dir();
        let base = SystemTime::now() - std::time::Duration::from_hours(1);
        for (index, name) in ["old row", "mid row", "new row"].iter().enumerate() {
            let path = write_session(&dir, "/repo/x", Some(name), 1);
            let file = fs::File::options().write(true).open(&path).unwrap();
            let modified = base + std::time::Duration::from_secs(60 * (index as u64 + 1));
            file.set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
        let mut seen: Vec<(usize, usize, Option<String>)> = Vec::new();
        let rows = list_sessions_with(&dir, |index, total, info| {
            seen.push((index, total, info.name.clone()));
            true
        });
        assert_eq!(rows.len(), 3);
        assert_eq!(seen.len(), 3, "every row emitted as its fold completes");
        assert_eq!(
            seen[0],
            (0, 3, Some("new row".to_string())),
            "the newest row emits first with scan counts 1-of-3 (the entry anchor's row is frame #1): {seen:?}"
        );
        assert_eq!(
            seen.last().map(|(index, total, _)| (*index, *total)),
            Some((2, 3)),
            "the oldest row emits last: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .map(|(_, _, name)| name.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("new row".to_string()),
                Some("mid row".to_string()),
                Some("old row".to_string()),
            ],
            "the emission order is newest first: {seen:?}"
        );
    }

    #[test]
    fn list_sessions_with_stops_when_the_consumer_stops() {
        let dir = temp_dir();
        write_session(&dir, "/repo/a", Some("first"), 1);
        write_session(&dir, "/repo/b", Some("second"), 1);
        write_session(&dir, "/repo/c", Some("third"), 1);
        let mut emitted = 0usize;
        let rows = list_sessions_with(&dir, |_, _, _| {
            emitted += 1;
            emitted < 2
        });
        assert_eq!(emitted, 2, "the scan stops at the consumer's false");
        assert_eq!(rows.len(), 1, "only the emitted row returns: {rows:?}");
    }

    /// A stub catalog cache: serves canned entries by path and records what
    /// the scan hands it.
    #[derive(Default)]
    struct StubCache {
        served: std::collections::HashMap<PathBuf, CatalogEntry>,
        recorded: std::sync::Mutex<Vec<(PathBuf, CatalogFileKey, u32, CatalogEntry)>>,
        finished: std::sync::Mutex<Vec<(PathBuf, Vec<PathBuf>)>>,
    }

    impl SessionCatalogCache for StubCache {
        fn lookup(&self, file: &CatalogFile<'_>) -> Option<CatalogEntry> {
            self.served.get(file.path).cloned()
        }

        fn record(&self, file: &CatalogFile<'_>, entry: CatalogEntry) {
            self.recorded.lock().unwrap().push((
                file.path.to_path_buf(),
                file.key,
                file.fold_version,
                entry,
            ));
        }

        fn scan_finished(&self, session_dir: &Path, listed: &[PathBuf]) {
            self.finished
                .lock()
                .unwrap()
                .push((session_dir.to_path_buf(), listed.to_vec()));
        }
    }

    fn file_key(path: &Path) -> CatalogFileKey {
        CatalogFileKey::from_metadata(&fs::metadata(path).unwrap()).unwrap()
    }

    #[test]
    fn a_catalog_cache_miss_folds_records_and_reports_the_listing() {
        let dir = temp_dir();
        let session = write_session(&dir, "/repo/a", Some("alpha"), 2);
        let foreign = dir.join("foreign.jsonl");
        fs::write(&foreign, "{\"type\":\"message\"}\n").unwrap();
        let cache = StubCache::default();
        let rows = list_sessions_through(&dir, Some(&cache), |_, _, _| true);
        let native = list_sessions_through(&dir, None, |_, _, _| true);
        assert_eq!(
            rows, native,
            "a miss lists exactly what the native scan lists"
        );
        let mut recorded = cache.recorded.into_inner().unwrap();
        recorded.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = vec![
            (
                session.clone(),
                file_key(&session),
                SESSION_ROW_VERSION,
                CatalogEntry::Session(Box::new(row_from_info(&native[0]))),
            ),
            (
                foreign.clone(),
                file_key(&foreign),
                SESSION_ROW_VERSION,
                CatalogEntry::NotASession,
            ),
        ];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(recorded, expected);
        let mut finished = cache.finished.into_inner().unwrap();
        for (_, listed) in &mut finished {
            listed.sort();
        }
        let mut listed = vec![session, foreign];
        listed.sort();
        assert_eq!(finished, vec![(dir, listed)]);
    }

    #[test]
    fn a_catalog_cache_hit_serves_its_row_without_recording() {
        let dir = temp_dir();
        let session = write_session(&dir, "/repo/a", Some("alpha"), 2);
        let hidden = write_session(&dir, "/repo/b", Some("beta"), 1);
        let mut row = row_from_info(&read_session_info(&session).unwrap());
        row.name = Some("served from the cache".to_string());
        let mut cache = StubCache::default();
        cache.served.insert(
            session.clone(),
            CatalogEntry::Session(Box::new(row.clone())),
        );
        cache.served.insert(hidden, CatalogEntry::NotASession);
        let rows = list_sessions_through(&dir, Some(&cache), |_, _, _| true);
        assert_eq!(rows, vec![info_from_row(&session, row)]);
        assert!(cache.recorded.into_inner().unwrap().is_empty());
    }

    #[test]
    fn a_stopped_listing_does_not_report_a_finished_scan() {
        let dir = temp_dir();
        write_session(&dir, "/repo/a", Some("first"), 1);
        write_session(&dir, "/repo/b", Some("second"), 1);
        let cache = StubCache::default();
        let rows = list_sessions_through(&dir, Some(&cache), |_, _, _| false);
        assert!(rows.is_empty());
        assert!(cache.finished.into_inner().unwrap().is_empty());
    }

    #[test]
    fn rows_round_trip_through_the_catalog_row() {
        let dir = temp_dir();
        let path = write_session(&dir, "/repo/a", Some("alpha"), 3);
        let info = read_session_info(&path).unwrap();
        assert_eq!(info_from_row(&path, row_from_info(&info)), info);
    }
}
