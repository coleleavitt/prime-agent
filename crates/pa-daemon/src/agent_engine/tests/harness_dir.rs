//! The worker's global harness store is `<agentDir>/harness`, the directory the
//! kernel (`rlm.harness`), print mode, the system-prompt digest, and TS v0.9.8
//! `getGlobalHarnessStateDir()` all use, never the agent dir itself.
use pa_core::refinement::{
    HarnessEntry,
    HarnessScope,
    RefinementKind,
    empty_harness_state,
    get_global_harness_state_dir,
    get_harness_state_path,
    load_harness_state,
    save_harness_state,
};
use pa_core::session_engine::slash_commands::{SlashCommandRegistry, parse_session_command};

use super::*;

fn reviewer() -> HarnessEntry {
    HarnessEntry {
        id: "reviewer".to_string(),
        kind: RefinementKind::Subagent,
        title: "API reviewer".to_string(),
        content: "API reviewer spec".to_string(),
        path: "general".to_string(),
        scope: Some(HarnessScope::Global),
        reference: Map::new(),
        arguments: Map::new(),
        metadata: Map::new(),
        source: "refine".to_string(),
        created_at: "t0".to_string(),
        updated_at: "t0".to_string(),
        version: 1,
        extensions: Map::new(),
    }
}

#[test]
fn harness_command_edits_the_canonical_global_store() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    let global = get_global_harness_state_dir(&agent_dir);
    let mut state = empty_harness_state();
    state
        .entries
        .entry(RefinementKind::Subagent)
        .or_default()
        .insert("reviewer".to_string(), reviewer());
    save_harness_state(&global, &state).unwrap();
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: agent_dir.clone(),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(r#"{"responses": [{"text": "unused"}]}"#.to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
        cron_store: None,
        queued_steering_probe: None,
    })
    .unwrap();

    let command = parse_session_command(
        &SlashCommandRegistry::builtin(),
        "/harness disable reviewer",
    )
    .unwrap();
    let execution = engine.execute_session_command(&command).unwrap();

    assert_eq!(execution.error, None);
    assert_eq!(
        execution.messages.last().unwrap().content.text(),
        "Disabled global:subagent:reviewer."
    );
    let mut expected = reviewer();
    expected.set_enabled(false);
    let stored = load_harness_state(&global, HarnessScope::Global);
    let stored_entry = &stored.entries[&RefinementKind::Subagent]["reviewer"];
    // The flip stamps the edit time; everything else is the seeded entry.
    expected.updated_at.clone_from(&stored_entry.updated_at);
    assert_eq!(stored_entry, &expected);
    assert!(
        !get_harness_state_path(&agent_dir).exists(),
        "nothing lands at the agent dir's root"
    );
}
