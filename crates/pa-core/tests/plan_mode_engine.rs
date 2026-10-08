// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Real Landlock enforcement on the running kernel; Linux-only.
#![cfg(target_os = "linux")]

//! `/plan` through a whole session engine with a live kernel: switching plan mode restarts the
//! kernel under a different OS sandbox, and a session that keeps no state snapshot (no artifact
//! dir) starts that kernel with an empty namespace. The user sees a warning row next to the
//! change row, and the model gets the same "starting fresh" notice a failed state revive gives
//! it. Skips, with a message, without the kernel Python or without Landlock.

use std::path::PathBuf;

use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus};
use pa_core::os_sandbox::SessionSandbox;
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use pa_core::session_engine::session_commands::{execute_session_command, SessionCommandParams};
use pa_core::session_engine::slash_commands::{parse_session_command, SlashCommandRegistry};
use pa_types::ai::UserContent;

fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = PathBuf::from(format!("{home}/.prime/agent/kernel-venv/bin/python"));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping the plan-mode engine test",
        candidate.display()
    );
    None
}

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

/// Run `text` as a session command; the displayed rows' texts after the echo.
async fn run_command(engine: &SessionEngine, text: &str) -> Vec<String> {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text).unwrap();
    let mut autonomous = pa_core::autonomous::create_autonomous_runtime_state(None, None);
    let model = wire_model();
    let global_harness = tempfile::tempdir().expect("temp dir");
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: global_harness.path().to_path_buf(),
        autonomous: &mut autonomous,
    };
    let execution = execute_session_command(engine, &mut params, &command).await;
    assert_eq!(execution.error, None);
    execution
        .messages
        .iter()
        .skip(1)
        .map(|row| match &row.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(_) => String::new(),
        })
        .collect()
}

#[tokio::test]
async fn a_toggle_that_resets_an_unsnapshotted_namespace_says_so() {
    if kernel_python().is_none() {
        return;
    }
    let probe = SessionSandbox::for_plan_mode(None);
    if probe.status_label().ends_with("(unavailable)") {
        eprintln!(
            "skipping the plan-mode engine test: {}",
            probe.prompt_line()
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    // In memory: no artifact dir, so no state snapshot to carry the namespace across.
    let provider = std::sync::Arc::new(pa_agent::scripted::ScriptedProvider::new(model()));
    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir,
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(SessionManager::in_memory(&cwd)),
        ..Default::default()
    })
    .await
    .unwrap();
    let provisioner = engine.kernel_provisioner_weak().upgrade().unwrap();
    let manager = provisioner.ensure(None, None).await.unwrap();
    let defined = manager
        .execute("kept = 1", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(defined.status, ExecuteStatus::Ok);

    let rows = run_command(&engine, "/plan on").await;
    assert_eq!(
        rows,
        vec![
            "Plan mode on: the agent investigates and plans; file edits are blocked until plan \
             mode is turned off (/plan off)."
                .to_string(),
            "Switching plan mode restarted the Python kernel under a different OS sandbox. This \
             session keeps no state snapshot, so the kernel's variables, imports and loaded data \
             were lost."
                .to_string(),
        ]
    );
    let manager = provisioner.ensure(None, None).await.unwrap();
    let gone = manager
        .execute("'kept' in globals()", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(gone.result.as_deref(), Some("False"));
    engine.dispose_kernel().await;
}
