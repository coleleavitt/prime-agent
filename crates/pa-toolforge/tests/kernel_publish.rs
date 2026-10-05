//! Verifier: `rlm.toolforge.publish` over a REAL kernel, through the session
//! seam. The feature is installed the way the composition root installs it;
//! one turn's `ipython` cell publishes a skill and calls it in the same cell,
//! then a shadowing name is refused with the runtime's `ToolforgeRejected`.
//! Skipped (with a note) without a bootstrapped kernel venv.
#![cfg(unix)]
// The session-engine futures are large by design (as in pa-core's own
// live-kernel verifiers).
#![allow(clippy::too_many_lines, clippy::large_futures)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::{recording_installer, SLUGIFY_DOC, SLUGIFY_EXIT_TEST, SLUGIFY_SOURCE};
use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::PromptOptions;
use pa_toolforge::{ledger_path, load_ledger, PublishStatus, ToolforgeFeature, ToolforgeOverrides};
use serde_json::{json, Value};

/// The bootstrapped kernel python (`PA_CORE_KERNEL_PYTHON`, else the venv
/// under `HOME`).
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let candidate =
        PathBuf::from(std::env::var_os("HOME")?).join(".prime/agent/kernel-venv/bin/python");
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

fn py(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

/// The cell: publish, call in the same cell, then a refused publish.
fn publish_cell(receipt: &Path) -> String {
    format!(
        "import json
_published = await rlm.toolforge.publish(\"slugify\", {source}, {doc}, {exit_test})
_receipt = {{
    \"same_cell\": slugify.run(\"A B\") == \"a-b\",
    \"import_name\": _published.import_name,
    \"version\": _published.version,
    \"gate\": [[run.phase, run.outcome, run.ok] for run in _published.gate],
}}
try:
    await rlm.toolforge.publish(\"json\", {source}, \"doc\", {exit_test})
    _receipt[\"rejected\"] = None
except rlm.toolforge.ToolforgeRejected as error:
    _receipt[\"rejected\"] = error.reason
open({receipt}, \"w\").write(json.dumps(_receipt, sort_keys=True))
print(\"TOOLFORGE_CELL_OK\")",
        source = py(SLUGIFY_SOURCE),
        doc = py(SLUGIFY_DOC),
        exit_test = py(SLUGIFY_EXIT_TEST),
        receipt = py(&receipt.display().to_string()),
    )
}

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux model")
}

#[tokio::test]
async fn a_published_skill_is_callable_in_the_cell_that_published_it() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let receipt = dir.path().join("toolforge-receipt.json");

    // This binary carries one test, so the process env is ours to set.
    std::env::set_var("PRIME_AGENT_KERNEL_PYTHON", &kernel_python);
    std::env::remove_var("PRIME_AGENT_CODING_AGENT_DIR");
    std::env::remove_var("PRIME_API_KEY");

    // Installed exactly as `pa-cli` installs it, but promoting without
    // touching the shared kernel venv.
    let installs = Arc::new(Mutex::new(Vec::new()));
    assert!(pa_core::features::install(vec![Arc::new(
        ToolforgeFeature::with_overrides(ToolforgeOverrides {
            installer: Some(recording_installer(&installs)),
            ..ToolforgeOverrides::default()
        })
    )]));

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));
    let model = scripted_model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        Some("publishing a skill"),
        vec![(
            "call-1",
            "ipython",
            json!({ "code": publish_cell(&receipt) }),
        )],
    )));
    provider.push_text_turn("published");

    let engine = create_session(SessionEngineConfig {
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
    })
    .await
    .expect("create_session");

    engine
        .prompt("publish the slugify skill", PromptOptions::default())
        .await
        .expect("prompt");

    let transcript = serde_json::to_string(&engine.session.entries().await).expect("entries json");
    let raw = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|_| panic!("the kernel cell wrote no receipt; transcript: {transcript}"));
    let payload: Value = serde_json::from_str(&raw).expect("receipt json");
    assert_eq!(
        payload,
        json!({
            "same_cell": true,
            "import_name": "slugify",
            "version": 1,
            "gate": [["negative", "raised", true], ["positive", "clean", true]],
            "rejected": "toolforge name \"json\" collides with a Python builtin, keyword, stdlib module or kernel-bound name (json); pick a name nothing else answers to",
        })
    );
    let package = agent_dir.join("skills").join("slugify");
    assert_eq!(*installs.lock().unwrap(), vec![package.clone()]);
    assert!(package.join("SKILL.md").is_file());
    assert!(transcript.contains("TOOLFORGE_CELL_OK"));
    let ledger = load_ledger(&ledger_path(&agent_dir));
    assert_eq!(
        ledger
            .records
            .iter()
            .map(|record| (
                record.name.as_str(),
                record.status,
                record.session_id.is_some()
            ))
            .collect::<Vec<_>>(),
        vec![("slugify", PublishStatus::Published, true)]
    );
}
