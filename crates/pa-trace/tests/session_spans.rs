//! pa-core's session spans reach the recorder: opening a session file is a
//! `session.load` span with its path and entry count.

use std::path::Path;
use std::time::Duration;

use pa_core::session::manager::SessionManager;
use pa_trace::RecorderConfig;
use serde_json::{Value, json};
use tracing_subscriber::layer::SubscriberExt;

fn span_ends(log_path: &Path) -> Vec<serde_json::Map<String, Value>> {
    std::fs::read_to_string(log_path)
        .expect("agent.jsonl")
        .lines()
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(Value::Object(entry)) if entry["msg"] == "span_end" => Some(entry),
            _ => None,
        })
        .collect()
}

#[test]
fn opening_a_session_is_a_session_load_span() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("logs").join("agent.jsonl");
    let session_file = dir.path().join("sessions").join("s1.jsonl");
    let (layer, handle) = pa_trace::recorder(RecorderConfig {
        log_path: log_path.clone(),
        inbound: None,
        otlp: None,
    });
    let entries =
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let manager =
                SessionManager::open(dir.path(), &dir.path().join("sessions"), &session_file);
            manager.get_entries().len()
        });
    assert!(handle.flush(Duration::from_secs(30)));
    let spans = span_ends(&log_path);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        (
            spans[0]["name"].clone(),
            spans[0]["status"].clone(),
            spans[0]["attrs"].clone()
        ),
        (
            json!("session.load"),
            json!("ok"),
            // Every file row counts, the header included.
            json!({"session.path": session_file.display().to_string(), "session.entries": entries + 1}),
        )
    );
}
