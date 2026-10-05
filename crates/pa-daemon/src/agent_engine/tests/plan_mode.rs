//! Plan mode in the worker's engine: `/plan` emits the durable change row
//! the worker persists, and a rebuild restores the mode from the worker's
//! session file (the engine's own session is in-memory).
use super::*;

fn engine_over(
    dir: &std::path::Path,
    session_file: Option<std::path::PathBuf>,
) -> AgentSessionEngine {
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.to_path_buf(),
        agent_dir,
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file,
        faux_script: Some(r#"{"responses": [{"text": "a plan"}]}"#.to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
        cron_store: None,
        queued_steering_probe: None,
    })
    .unwrap()
}

fn plan_mode_on(engine: &AgentSessionEngine) -> bool {
    let guard = engine.session.blocking_lock();
    guard.as_deref().expect("session built").plan_mode_enabled()
}

/// The `plan_mode_change` rows a run emitted, as their `details.enabled`.
fn change_rows(events: &[EngineEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::CustomMessage(row) if row["customType"] == "plan_mode_change" => {
                Some(row["details"]["enabled"].clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn plan_command_emits_the_durable_change_row() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().unwrap();
    let engine = engine_over(dir.path(), None);
    let mut events = Vec::new();
    admit(&engine, "/plan on".to_string(), &mut events);
    assert_eq!(change_rows(&events), vec![json!(true)]);
    assert!(plan_mode_on(&engine));
    let mut events = Vec::new();
    admit(&engine, "/plan".to_string(), &mut events);
    assert_eq!(change_rows(&events), vec![json!(false)]);
    assert!(!plan_mode_on(&engine));
}

#[test]
fn a_rebuild_restores_plan_mode_from_the_session_file() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().unwrap();
    let mut store = crate::session_store::SessionFile::create("/tmp", None, 0);
    let session_path = dir.path().join("session.jsonl");
    store.set_path(session_path.clone());
    store.append_entry(
        "custom_message",
        json!({
            "customType": "plan_mode_change",
            "content": "Plan mode on",
            "display": true,
            "details": { "enabled": true },
        }),
    );
    store.rewrite().expect("write session file");
    let engine = engine_over(dir.path(), Some(session_path));
    let mut events = Vec::new();
    admit(&engine, "what would you change?".to_string(), &mut events);
    assert!(plan_mode_on(&engine));
    // Restoring is not a change: no new row.
    assert_eq!(change_rows(&events), Vec::<Value>::new());
}
