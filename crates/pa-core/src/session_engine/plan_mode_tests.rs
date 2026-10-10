//! Plan mode through a whole session engine: the host refuses `edit` while
//! the mode is on, `/plan` switches it and records the durable change, the
//! model hears about it per turn, and a resumed session restores it.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;
use pa_types::session::FileEntry;

use super::*;
use crate::session::manager::SessionManager;
use crate::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use crate::session_engine::session_commands::{
    execute_session_command, SessionCommandExecution, SessionCommandParams,
};
use crate::session_engine::slash_commands::{parse_session_command, SlashCommandRegistry};
use crate::session_engine::tool_bridge::bridge_tool;
use crate::session_engine::PromptOptions;

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

async fn engine(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    provider: &Arc<ScriptedProvider>,
    session_manager: SessionManager,
    plan_mode: Option<bool>,
) -> SessionEngine {
    create_session(SessionEngineConfig {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(
            crate::tools::edit::create_edit_tool_definition(&cwd.to_string_lossy()),
        )],
        session_manager: Some(session_manager),
        plan_mode,
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn run_command(engine: &SessionEngine, text: &str) -> SessionCommandExecution {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text).unwrap();
    let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
    let model = wire_model();
    // A command may write the global harness: never into the shared temp root.
    let global_harness = tempfile::tempdir().expect("temp dir");
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: global_harness.path().to_path_buf(),
        autonomous: &mut autonomous,
    };
    execute_session_command(engine, &mut params, &command).await
}

fn edit_call(id: &str) -> (&str, &str, serde_json::Value) {
    (
        id,
        "edit",
        serde_json::json!({
            "path": "main.py",
            "edits": [{ "oldText": "before", "newText": "after" }]
        }),
    )
}

/// The rows of each provider request, as one JSON string per request.
fn requests(provider: &ScriptedProvider) -> Vec<String> {
    provider
        .calls()
        .iter()
        .map(|call| serde_json::to_string(&call.messages).unwrap())
        .collect()
}

fn change_rows(entries: &[FileEntry]) -> Vec<bool> {
    entries.iter().filter_map(plan_mode_of_entry).collect()
}

#[tokio::test]
async fn plan_mode_refuses_edit_until_plan_off() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let file = cwd.join("main.py");
    std::fs::write(&file, "before\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    provider.push_tool_call_turn(None, vec![edit_call("call-1")]);
    provider.push_text_turn("here is the plan");
    provider.push_tool_call_turn(None, vec![edit_call("call-2")]);
    provider.push_text_turn("edited");
    let engine = engine(
        &cwd,
        &dir.path().join("agent"),
        &provider,
        SessionManager::in_memory(&cwd),
        None,
    )
    .await;

    let on = run_command(&engine, "/plan").await;
    assert_eq!(on.error, None);
    assert_eq!(
        on.messages
            .iter()
            .map(|row| row.custom_type.as_str())
            .collect::<Vec<_>>(),
        vec!["session_slash_command", PLAN_MODE_CHANGE_CUSTOM_TYPE]
    );
    engine
        .prompt("plan the rename", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");

    let off = run_command(&engine, "/plan off").await;
    assert_eq!(off.error, None);
    engine
        .prompt("go ahead", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");

    let sent = requests(&provider);
    assert_eq!(sent.len(), 4, "{sent:#?}");
    // The planning turn carries the context row; its edit was refused.
    assert!(sent[0].contains("<plan_mode>"), "{}", sent[0]);
    assert!(
        sent[1].contains("Plan mode is active: the edit tool is disabled."),
        "{}",
        sent[1]
    );
    // The first turn after `/plan off` carries the one-shot notice instead.
    assert!(sent[2].contains("<plan_mode_off>"), "{}", sent[2]);
    assert_eq!(
        sent[2].matches("<plan_mode>").count(),
        1,
        "only the old turn's row"
    );
    // Neither change row reaches the model.
    assert!(!sent[3].contains("Plan mode on: "), "{}", sent[3]);
    assert_eq!(
        change_rows(&engine.session.entries().await),
        vec![true, false]
    );
}

#[tokio::test]
async fn plan_command_reports_status_and_unchanged_state() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = engine(
        dir.path(),
        &dir.path().join("agent"),
        &provider,
        SessionManager::in_memory(dir.path()),
        None,
    )
    .await;
    let texts = |execution: &SessionCommandExecution| -> Vec<String> {
        execution
            .messages
            .iter()
            .skip(1)
            .map(|row| match &row.content {
                pa_types::ai::UserContent::Text(text) => text.clone(),
                pa_types::ai::UserContent::Blocks(_) => String::new(),
            })
            .collect()
    };
    assert_eq!(
        texts(&run_command(&engine, "/plan status").await),
        vec!["Plan mode is off.".to_string()]
    );
    assert_eq!(
        texts(&run_command(&engine, "/plan off").await),
        vec!["Plan mode is already off.".to_string()]
    );
    run_command(&engine, "/plan on").await;
    assert!(engine.plan_mode_enabled());
    let bad = run_command(&engine, "/plan maybe").await;
    assert_eq!(bad.error.as_deref(), Some("Usage: /plan [on|off|status]"));
    assert!(engine.plan_mode_enabled());
}

/// On a machine with no OS sandbox (an injected assessment), turning plan mode
/// on warns that only the in-kernel guard enforces it; turning it off does not.
#[tokio::test]
async fn without_an_os_sandbox_turning_plan_mode_on_warns_of_the_kernel_guard() {
    let _no_sandbox = crate::os_sandbox::test_seam::override_plan_assessment(|_| {
        Err(pa_os_sandbox::SandboxError::Unsupported {
            reason: "Landlock is not enabled".to_string(),
        })
    });
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = engine(
        dir.path(),
        &dir.path().join("agent"),
        &provider,
        SessionManager::in_memory(dir.path()),
        None,
    )
    .await;
    let rows = |execution: &SessionCommandExecution| -> Vec<(String, serde_json::Value)> {
        execution
            .messages
            .iter()
            .skip(1)
            .map(|row| {
                let text = match &row.content {
                    pa_types::ai::UserContent::Text(text) => text.clone(),
                    pa_types::ai::UserContent::Blocks(_) => String::new(),
                };
                let severity = row
                    .details
                    .as_ref()
                    .and_then(|details| details.get("severity").cloned())
                    .unwrap_or(serde_json::Value::Null);
                (text, severity)
            })
            .collect()
    };
    assert_eq!(
        engine.plan_mode_fallback(),
        Some("OS sandbox unavailable: Landlock is not enabled")
    );
    assert_eq!(
        (
            rows(&run_command(&engine, "/plan on").await),
            rows(&run_command(&engine, "/plan off").await),
        ),
        (
            vec![
                (
                    "Plan mode on: the agent investigates and plans; file edits are blocked \
                     until plan mode is turned off (/plan off)."
                        .to_string(),
                    serde_json::Value::Null
                ),
                (
                    "Plan mode is enforced inside the Python kernel only: this machine has no OS \
                     sandbox (OS sandbox unavailable: Landlock is not enabled). The agent's \
                     commands are refused, and code that calls the C library directly (ctypes) \
                     can still write files."
                        .to_string(),
                    serde_json::json!("warning")
                ),
            ],
            vec![(
                "Plan mode off: the agent may edit files again.".to_string(),
                serde_json::Value::Null
            )],
        )
    );
}

#[tokio::test]
async fn an_explicit_start_state_is_recorded_and_a_resume_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    provider.push_text_turn("planned");
    let first = engine(
        &cwd,
        &dir.path().join("agent"),
        &provider,
        SessionManager::persisted(&cwd, &sessions),
        Some(true),
    )
    .await;
    assert!(first.plan_mode_enabled());
    first
        .prompt("plan it", PromptOptions::default())
        .await
        .unwrap();
    first.session.agent().wait_for_idle().await;
    let file = first
        .session
        .shared_persistence()
        .lock()
        .await
        .get_session_file()
        .map(std::path::Path::to_path_buf)
        .expect("a persisted session");
    assert_eq!(change_rows(&first.session.entries().await), vec![true]);
    first.dispose_kernel().await;
    drop(first);

    // A resume with no explicit state restores the branch's newest change.
    let resumed = engine(
        &cwd,
        &dir.path().join("agent"),
        &provider,
        SessionManager::open(&cwd, &sessions, &file),
        None,
    )
    .await;
    assert!(resumed.plan_mode_enabled());
    // The restored state is not a new change.
    assert_eq!(change_rows(&resumed.session.entries().await), vec![true]);
}
