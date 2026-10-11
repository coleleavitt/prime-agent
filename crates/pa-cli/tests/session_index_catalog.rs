//! The persisted saved-session catalog index (`feature = "session-index"`)
//! behind the native catalog scan: every listing equals the native fold's,
//! cold (recording), warm (served), and after a session grows.
#![cfg(feature = "session-index")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pa_daemon::session_store::{SessionFile, list_sessions, read_session_info, session_file_name};
use serde_json::json;

fn write_session(dir: &Path, cwd: &str, name: Option<&str>, turns: u64) -> PathBuf {
    let mut session = SessionFile::create(cwd, None, 0);
    session.append_model_change("anthropic", "claude-opus-4-5");
    if let Some(name) = name {
        session.append_session_info(name);
    }
    for turn in 0..turns {
        session.append_message(&json!({
            "role": "user", "content": format!("question {turn}"), "timestamp": 1_000 + turn
        }));
        session.append_message(&json!({
            "role": "assistant",
            "content": [{"type": "text", "text": format!("answer {turn}")}],
            "provider": "anthropic", "model": "claude-opus-4-5",
            "usage": {
                "input": 10, "output": 5, "cacheRead": 100, "cacheWrite": 0, "totalTokens": 115,
                "cost": {"input": 0.001, "output": 0.002, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.003}
            },
            "stopReason": "stop", "timestamp": 1_000 + turn
        }));
    }
    let path = dir.join(session_file_name(session.session_id()));
    session.set_path(path.clone());
    session.rewrite().unwrap();
    path
}

#[test]
fn the_index_lists_exactly_the_native_rows() {
    let dir = tempfile::tempdir().unwrap();
    let grown = write_session(dir.path(), "/work/a", Some("alpha"), 3);
    write_session(dir.path(), "/work/b", None, 1);
    write_session(dir.path(), "/work/c", Some("gamma"), 0);
    std::fs::write(dir.path().join("foreign.jsonl"), "{\"type\":\"message\"}\n").unwrap();
    // Nothing installed yet: the native scan.
    let native = list_sessions(dir.path());
    assert_eq!(native.len(), 3);

    let index = pa_session_index::SessionIndex::new();
    assert!(pa_core::session::catalog_cache::install(Box::new(
        index.clone()
    )));
    assert_eq!(
        list_sessions(dir.path()),
        native,
        "cold: folded and recorded"
    );
    assert!(index.flush(Instant::now() + Duration::from_secs(30)));
    assert!(
        dir.path()
            .join(pa_session_index::SESSION_INDEX_FILE)
            .is_file()
    );
    assert_eq!(list_sessions(dir.path()), native, "warm: served");

    // A grown session misses and folds again; the others stay served.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&grown)
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({
            "type": "message", "id": "later001", "parentId": null,
            "timestamp": "2026-09-01T10:00:00.000Z",
            "message": {"role": "user", "content": "one more thing", "timestamp": 9_000}
        })
    )
    .unwrap();
    drop(file);
    let after = list_sessions(dir.path());
    let mut expected: Vec<_> = native
        .iter()
        .map(|info| {
            if info.path == grown {
                read_session_info(&grown).unwrap()
            } else {
                info.clone()
            }
        })
        .collect();
    expected.sort_by_key(|info| info.path != grown);
    assert_eq!(after, expected);
}
