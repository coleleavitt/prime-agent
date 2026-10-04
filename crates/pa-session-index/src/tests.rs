use std::time::Duration;

use pa_core::session::catalog_cache::{CatalogFileKey, CatalogSessionRow, CatalogUsage};

use super::*;

const TS_INDEX: &str = include_str!("../tests/fixtures/ts-session-index.ndjson");
const TS_SEARCH_INDEX: &str = include_str!("../tests/fixtures/ts-session-search-index.ndjson");

const FULL: &str = "019a0000-0000-7000-8000-000000000001.jsonl";
const MINIMAL: &str = "019a0000-0000-7000-8000-000000000002.jsonl";
const FOREIGN: &str = "foreign.jsonl";

/// The rows `tests/fixtures/generate.ts` hands the TS writer.
fn full_row() -> CatalogSessionRow {
    CatalogSessionRow {
        id: "019a0000-0000-7000-8000-000000000001".to_string(),
        cwd: "/work/repo".to_string(),
        name: Some("alpha \"quoted\" \u{e9}".to_string()),
        state: Some("active".to_string()),
        model: Some(("anthropic".to_string(), "claude-opus-4-5".to_string())),
        thinking_level: None,
        parent_session_path: Some("/home/user/.prime/agent/sessions/parent.jsonl".to_string()),
        rlm_depth: 1,
        created: "2026-09-01T10:00:00.000Z".to_string(),
        modified: "2026-09-01T10:05:00.250Z".to_string(),
        message_count: 4,
        first_message: "fix the login bug\nin auth.rs".to_string(),
        all_messages_text: "fix the login bug\nin auth.rs fixed in auth.rs".to_string(),
        usage: Some(CatalogUsage {
            input_tokens: 1_200,
            output_tokens: 80,
            cost: 0.012_345,
        }),
    }
}

fn minimal_row() -> CatalogSessionRow {
    CatalogSessionRow {
        id: "019a0000-0000-7000-8000-000000000002".to_string(),
        cwd: "/work/other".to_string(),
        name: None,
        state: None,
        model: None,
        thinking_level: None,
        parent_session_path: None,
        rlm_depth: 0,
        created: "2026-08-01T00:00:00.000Z".to_string(),
        modified: "2026-08-01T00:00:00.000Z".to_string(),
        message_count: 0,
        first_message: "(no messages)".to_string(),
        all_messages_text: String::new(),
        usage: Some(CatalogUsage {
            input_tokens: 3,
            output_tokens: 0,
            cost: 0.0,
        }),
    }
}

fn golden_entries() -> Vec<(&'static str, IndexedEntry)> {
    let entry = |size, mtime_ms, entry| IndexedEntry {
        size,
        mtime_ms,
        fold_version: 1,
        entry,
    };
    vec![
        (
            FULL,
            entry(
                20_480,
                1_789_177_896_715.123_5,
                CatalogEntry::Session(Box::new(full_row())),
            ),
        ),
        (
            MINIMAL,
            entry(
                512,
                1_785_000_000_000.0,
                CatalogEntry::Session(Box::new(minimal_row())),
            ),
        ),
        (
            FOREIGN,
            entry(26, 1_784_000_000_000.5, CatalogEntry::NotASession),
        ),
    ]
}

#[test]
fn both_tiers_are_the_ts_files_plus_the_fold_version() {
    let entries = golden_entries();
    let (metadata, search) = format::render(entries.iter().map(|(file, entry)| (*file, entry)));
    // The only bytes this crate adds to a TS line: the trailing `foldVersion`.
    let as_ts = |text: &str| text.replace(",\"foldVersion\":1}", "}");
    assert_eq!(as_ts(&metadata), TS_INDEX);
    assert_eq!(as_ts(&search), TS_SEARCH_INDEX);
}

#[test]
fn ts_written_lines_are_never_served() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(SESSION_INDEX_FILE), TS_INDEX).unwrap();
    fs::write(dir.path().join(SESSION_SEARCH_INDEX_FILE), TS_SEARCH_INDEX).unwrap();
    let index = SessionIndex::new();
    for (file, entry) in golden_entries() {
        let path = dir.path().join(file);
        let lookup = index.lookup(&catalog_file(dir.path(), &path, &entry));
        assert_eq!(lookup, None, "{file}");
    }
}

fn catalog_file<'a>(dir: &'a Path, path: &'a Path, entry: &IndexedEntry) -> CatalogFile<'a> {
    CatalogFile {
        session_dir: dir,
        path,
        key: CatalogFileKey {
            size: entry.size,
            mtime_ms: entry.mtime_ms,
        },
        fold_version: entry.fold_version,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// Record every golden entry through `index` and finish the listing.
fn record_golden(index: &SessionIndex, dir: &Path) -> Vec<PathBuf> {
    let mut listed = Vec::new();
    for (file, entry) in golden_entries() {
        let path = dir.join(file);
        index.record(&catalog_file(dir, &path, &entry), entry.entry.clone());
        listed.push(path);
    }
    index.scan_finished(dir, &listed);
    listed
}

#[test]
fn a_fresh_process_serves_what_an_earlier_one_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let earlier = SessionIndex::new();
    record_golden(&earlier, dir.path());
    assert!(earlier.flush(deadline()));
    let fresh = SessionIndex::new();
    for (file, entry) in golden_entries() {
        let path = dir.path().join(file);
        let lookup = fresh.lookup(&catalog_file(dir.path(), &path, &entry));
        assert_eq!(lookup, Some(entry.entry), "{file}");
    }
}

#[test]
fn another_key_or_fold_version_misses() {
    let dir = tempfile::tempdir().unwrap();
    let earlier = SessionIndex::new();
    record_golden(&earlier, dir.path());
    assert!(earlier.flush(deadline()));
    let fresh = SessionIndex::new();
    let (file, entry) = golden_entries().remove(0);
    let path = dir.path().join(file);
    let grown = IndexedEntry {
        size: entry.size + 1,
        ..entry.clone()
    };
    let touched = IndexedEntry {
        mtime_ms: entry.mtime_ms + 0.5,
        ..entry.clone()
    };
    let refolded = IndexedEntry {
        fold_version: entry.fold_version + 1,
        ..entry.clone()
    };
    for changed in [grown, touched, refolded] {
        assert_eq!(
            fresh.lookup(&catalog_file(dir.path(), &path, &changed)),
            None
        );
    }
    assert_eq!(
        fresh.lookup(&catalog_file(dir.path(), &path, &entry)),
        Some(entry.entry)
    );
}

#[test]
fn files_that_left_the_listing_leave_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let index = SessionIndex::new();
    let listed = record_golden(&index, dir.path());
    index.scan_finished(dir.path(), &listed[..1]);
    assert!(index.flush(deadline()));
    let entries = golden_entries();
    let kept = &entries[..1];
    let (metadata, search) = format::render(kept.iter().map(|(file, entry)| (*file, entry)));
    let read = |name| fs::read_to_string(dir.path().join(name)).unwrap();
    assert_eq!(read(SESSION_INDEX_FILE), metadata);
    assert_eq!(read(SESSION_SEARCH_INDEX_FILE), search);
}

#[test]
fn a_listing_that_learned_nothing_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let index = SessionIndex::new();
    let listed = record_golden(&index, dir.path());
    assert!(index.flush(deadline()));
    fs::remove_file(dir.path().join(SESSION_INDEX_FILE)).unwrap();
    fs::remove_file(dir.path().join(SESSION_SEARCH_INDEX_FILE)).unwrap();
    index.scan_finished(dir.path(), &listed);
    assert!(index.flush(deadline()));
    assert!(!dir.path().join(SESSION_INDEX_FILE).exists());
    assert!(!dir.path().join(SESSION_SEARCH_INDEX_FILE).exists());
}

#[test]
fn files_outside_the_scanned_directory_are_not_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let index = SessionIndex::new();
    let (_, entry) = golden_entries().remove(0);
    let nested = dir.path().join("nested").join(FULL);
    let file = catalog_file(dir.path(), &nested, &entry);
    index.record(&file, entry.entry.clone());
    assert_eq!(index.lookup(&file), None);
}

#[test]
fn the_written_tiers_are_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let index = SessionIndex::new();
    record_golden(&index, dir.path());
    assert!(index.flush(deadline()));
    for name in [SESSION_INDEX_FILE, SESSION_SEARCH_INDEX_FILE] {
        let path = dir.path().join(name);
        assert!(path.is_file(), "{name}");
        #[cfg(unix)]
        assert_eq!(pa_core::platform::perms::file_mode(&path), Some(0o600));
    }
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
