// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Verifier: the kernel's `rlm.harness` API is served by the host's store
//! over the kernel protocol (synchronous `harness.*` host requests) in a
//! REAL kernel: one `ipython` cell writes a session-local and a global entry,
//! disables one, and reads them back; the host-side files show the writes.
//! The kernel venv's runtime is an
//! installed copy and the test binary exports no host executable, so no
//! request can fall back to the out-of-kernel one-shot.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::PromptOptions;
use serde_json::{json, Value};

/// The kernel Python with prime-agent-runtime installed (the bootstrapped
/// kernel venv). Skipped (with a note) on machines without one;
/// `PA_CORE_KERNEL_PYTHON` points at an explicit one.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

/// Scoped process-env overrides: applied on construction, restored on drop.
/// This binary carries exactly one live-kernel test, so nothing races.
struct EnvOverride {
    saved: Vec<(String, Option<String>)>,
}

impl EnvOverride {
    fn apply(pairs: &[(&str, Option<String>)]) -> Self {
        let saved = pairs
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
            .collect();
        for (key, value) in pairs {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        EnvOverride { saved }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn harness_cell(receipt_path: &Path) -> String {
    format!(
        r#"import json
local = rlm.harness.create_memory("Kernel note", "Written from the kernel.", id="kernel_note")
shared = rlm.harness.create_memory("Shared lesson", "Every session.", id="shared_lesson", global_=True)
rlm.harness.disable_memory("global:shared_lesson")
try:
    rlm.harness.create_memory("Kernel note", "again", id="kernel_note")
    duplicate = None
except ValueError as err:
    duplicate = str(err)
payload = {{
    "local_scope": local.scope,
    "local_source": local.source,
    "shared_scope": shared.scope,
    "shared_enabled": rlm.harness.get("memory", "global:shared_lesson").enabled,
    "duplicate": duplicate,
    "listed": [entry.id for entry in rlm.harness.list("memory")],
    "file": str(rlm.harness.file_path),
}}
open({receipt:?}, "w").write(json.dumps(payload))
print("HARNESS_STORE_OK")"#,
        receipt = receipt_path.display().to_string(),
    )
}

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux loop model")
}

fn stored(path: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("no harness store at {}", path.display())),
    )
    .expect("store json")
}

#[tokio::test]
async fn the_kernel_harness_api_is_served_by_the_host_store() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let receipt_path = dir.path().join("harness-receipt.json");

    let _env = EnvOverride::apply(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(kernel_python.display().to_string()),
        ),
        ("PRIME_AGENT_CODING_AGENT_DIR", None),
        ("PRIME_AGENT_EXECUTABLE", None),
        ("RLM_HARNESS_STATE_DIR", None),
        ("RLM_GLOBAL_HARNESS_STATE_DIR", None),
        ("RLM_SESSION_DIR", None),
        ("PRIME_API_KEY", None),
    ]);

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));
    let local_store = session
        .get_session_artifact_dir()
        .expect("a persisted session has an artifact dir")
        .join("harness")
        .join("harness_state.json");
    let global_store = agent_dir.join("harness").join("harness_state.json");

    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        Some("writing harness entries"),
        vec![(
            "call-1",
            "ipython",
            json!({ "code": harness_cell(&receipt_path) }),
        )],
    )));
    provider.push_text_turn("stored");

    let engine = create_session(SessionEngineConfig {
        sandbox_mode: None,
        plan_mode: None,
        on_late_sent_agent_message: None,
        semantic_edges: None,
        cron_store: None,
        queued_steering_probe: None,
        image_model_router: None,
        steering_mode: None,
        follow_up_mode: None,
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(model),
        thinking_level: None,
        stream_fn: Some(provider.stream_fn()),
        tools: Vec::new(),
        custom_system_prompt: None,
        prompt_guidelines: Vec::new(),
        generic_mcp_servers: Vec::new(),
        allow_recursion: None,
        session_manager: Some(session),
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: Vec::new(),
        additional_prompt_paths: Vec::new(),
        resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
        extra_builtin_skill_overrides: Vec::new(),
        rlm_subagent_host: None,
        rlm_depth: None,
        telemetry: None,
        model_info: None,
        mcp_manager: None,
        prewarm_ipython_kernel: None,
        on_background_work_settled: None,
        queued_goal_context_purge: None,
        rlm_token_allowance: None,
    })
    .await
    .expect("create_session");

    engine
        .prompt("store the notes", PromptOptions::default())
        .await
        .expect("prompt");

    let raw = std::fs::read_to_string(&receipt_path)
        .unwrap_or_else(|_| panic!("kernel cell wrote no receipt at {}", receipt_path.display()));
    let receipt: Value = serde_json::from_str(&raw).expect("receipt json");
    assert_eq!(
        receipt,
        json!({
            "local_scope": "local",
            "local_source": "kernel",
            "shared_scope": "global",
            "shared_enabled": false,
            "duplicate": "memory entry 'kernel_note' already exists",
            "listed": ["kernel_note"],
            "file": local_store.display().to_string(),
        })
    );

    let local = stored(&local_store);
    assert_eq!(
        local["entries"]["memory"]["kernel_note"]["content"],
        json!("Written from the kernel.")
    );
    let global = stored(&global_store);
    assert_eq!(
        global["entries"]["memory"]["shared_lesson"]["enabled"],
        json!(false)
    );
    assert!(
        !crate_lock(&local_store).exists() && !crate_lock(&global_store).exists(),
        "every write released the store lock"
    );
}

fn crate_lock(store: &Path) -> PathBuf {
    pa_core::platform::LockDir::path_for(store)
}
