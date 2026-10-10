//! `/harness` through a whole session engine (#1118): the list and the flip
//! of one entry's flag, kept out of the transcript and the model's context,
//! and the next system-prompt digest without the disabled entry.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;

use crate::refinement::{
    empty_harness_state, load_harness_state, save_harness_state, HarnessEntry, HarnessScope,
    RefinementKind,
};
use crate::session::manager::SessionManager;
use crate::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use crate::session_engine::session_commands::{
    execute_session_command, SessionCommandExecution, SessionCommandParams,
};
use crate::session_engine::slash_commands::{parse_session_command, SlashCommandRegistry};

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
        max_tokens_explicit: false,
    }
}

fn wire_model() -> pa_types::ai::Model {
    serde_json::from_value(serde_json::json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

fn subagent(id: &str, title: &str) -> HarnessEntry {
    HarnessEntry {
        id: id.to_string(),
        kind: RefinementKind::Subagent,
        title: title.to_string(),
        content: format!("{title} spec"),
        path: "general".to_string(),
        scope: Some(HarnessScope::Global),
        reference: serde_json::Map::new(),
        arguments: serde_json::Map::new(),
        metadata: serde_json::Map::new(),
        source: "refine".to_string(),
        created_at: "t0".to_string(),
        updated_at: "t0".to_string(),
        version: 1,
        extensions: serde_json::Map::new(),
    }
}

async fn run(
    engine: &SessionEngine,
    global: &std::path::Path,
    text: &str,
) -> SessionCommandExecution {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text).unwrap();
    let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
    let model = wire_model();
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: global.to_path_buf(),
        autonomous: &mut autonomous,
    };
    execute_session_command(engine, &mut params, &command).await
}

#[tokio::test]
async fn harness_lists_and_disables_entries_off_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = dir.path().join("global-harness");
    let mut state = empty_harness_state();
    let subagents = state.entries.entry(RefinementKind::Subagent).or_default();
    subagents.insert("reviewer".to_string(), subagent("reviewer", "API reviewer"));
    subagents.insert("planner".to_string(), subagent("planner", "Planner"));
    save_harness_state(&global, &state).unwrap();

    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: dir.path().join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(SessionManager::persisted(
            &cwd,
            &dir.path().join("sessions"),
        )),
        ..Default::default()
    })
    .await
    .unwrap();

    let listed = run(&engine, &global, "/harness").await;
    assert_eq!(listed.error, None);
    assert!(
        listed.messages.iter().all(|row| !row.display),
        "harness rows stay out of the transcript: {:?}",
        listed.messages
    );
    let result = listed.messages.last().unwrap();
    assert_eq!(
        result.content.text(),
        "Continual harness entries:\n[on]  global:subagent:planner - Planner\n[on]  global:subagent:reviewer - API reviewer"
    );
    assert!(crate::session_engine::messages::convert_to_llm(&[
        pa_types::session::AgentMessage::Custom(result.clone())
    ])
    .is_empty());

    let disabled = run(&engine, &global, "/harness disable reviewer").await;
    assert_eq!(disabled.error, None);
    let result = disabled.messages.last().unwrap();
    assert_eq!(result.content.text(), "Disabled global:subagent:reviewer.");
    let details = result.details.as_ref().unwrap();
    assert_eq!(details["harness"]["changed"]["enabled"], false);
    assert_eq!(
        details["harness"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| (
                entry["id"].as_str().unwrap(),
                entry["enabled"].as_bool().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![("planner", true), ("reviewer", false)]
    );
    let stored = load_harness_state(&global, HarnessScope::Global);
    assert!(!stored.entries[&RefinementKind::Subagent]["reviewer"].is_enabled());
    // The stored entry is kept whole: disabling is not deleting.
    assert_eq!(
        stored.entries[&RefinementKind::Subagent]["reviewer"].content,
        "API reviewer spec"
    );

    // The next prompt's digest offers the planner only.
    let digest = crate::refinement::ranking::format_harness_state_for_prompt(
        &stored,
        &crate::refinement::ranking::HarnessStatePromptOptions::default(),
    );
    assert!(
        digest.contains("subagent: 1 (+1 disabled, not available)"),
        "{digest}"
    );
    assert!(!digest.contains("API reviewer"), "{digest}");

    let failed = run(&engine, &global, "/harness enable nope").await;
    assert_eq!(
        failed.error.as_deref(),
        Some("No harness entry matches nope")
    );
    assert!(failed.messages.iter().all(|row| !row.display));
    let usage = run(&engine, &global, "/harness frobnicate").await;
    assert_eq!(
        usage.error.as_deref(),
        Some("Usage: /harness [list | enable <entry> | disable <entry>]")
    );
}
