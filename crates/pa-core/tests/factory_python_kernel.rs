// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Verifier: a factory python state runs in the session kernel's Python
//! environment. One `ipython` cell stores a machine whose entry state is a
//! python callable, runs it through `rlm.factory`, and waits for the run;
//! the host-side executor starts the call from the session's kernel (its
//! interpreter, in the session's working directory, isolated mode), and the
//! call's outputs route the run to completion.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use pa_agent::scripted::{ScriptedProvider, ScriptedTurn, tool_call_turn_steps};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::PromptOptions;
use pa_core::session_engine::engine::{SessionEngineConfig, create_session};
use serde_json::{Value, json};

/// The kernel Python with prime-agent-runtime installed (the bootstrapped
/// kernel venv). Skipped (with a note) on machines without one;
/// `PA_CORE_KERNEL_PYTHON` points at an explicit one.
fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
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
                Some(value) => pa_types::env::set_var(key, value),
                None => pa_types::env::remove_var(key),
            }
        }
        EnvOverride { saved }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => pa_types::env::set_var(key, value),
                None => pa_types::env::remove_var(key),
            }
        }
    }
}

fn factory_cell(receipt_path: &Path) -> String {
    format!(
        r#"import json, sys
rlm.harness.create_factory(
    "py-plan",
    "A python entry state.",
    id="py-plan",
    machine={{
        "states": [
            {{"id": "plan", "entry": True,
              "python": {{"import": "factory_kernel_tools", "callable": "where"}},
              "outputs": [{{"name": "where", "type": "text"}}]}},
        ],
    }},
)
result = await rlm.factory.run("py-plan")
watched = {{"state": "running"}}
for _ in range(60):
    watched = await rlm.factory.watch(result["run_id"], 1)
    if watched["state"] != "running":
        break
status = await rlm.factory.status(result["run_id"])
plan = next(node for node in status["nodes"] if node["id"] == "plan")
open({receipt:?}, "w").write(json.dumps({{
    "state": status["state"],
    "answer": plan.get("answer_preview"),
    "child": plan["instances"][0]["child"],
    "kernel_python": sys.executable,
}}))
print("FACTORY_PYTHON_OK")"#,
        receipt = receipt_path.display().to_string(),
    )
}

const TOOLS: &str = r#"import json, os, sys

def where(inputs):
    print("planning")  # stderr, never the outputs
    with open("observed.json", "w") as handle:
        json.dump({"cwd": os.getcwd(), "python": sys.executable,
                   "isolated": sys.flags.isolated, "inputs": inputs,
                   "rlm_env": sorted(key for key in os.environ if key.startswith("RLM_"))}, handle)
    return {"where": "here"}
"#;

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux loop model")
}

#[tokio::test]
async fn a_python_state_runs_in_the_session_kernels_environment() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    // The factory is opt-in.
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "factory": { "enabled": true } }).to_string(),
    )
    .expect("settings");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    std::fs::write(cwd.join("factory_kernel_tools.py"), TOOLS).expect("tools module");
    let receipt_path = dir.path().join("factory-receipt.json");

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

    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        Some("running the factory"),
        vec![(
            "call-1",
            "ipython",
            json!({ "code": factory_cell(&receipt_path) }),
        )],
    )));
    provider.push_text_turn("ran");

    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(session),
        daemonless: true,
        ..SessionEngineConfig::default()
    })
    .await
    .expect("create_session");

    engine
        .prompt("run the factory", PromptOptions::default())
        .await
        .expect("prompt");

    let raw = std::fs::read_to_string(&receipt_path)
        .unwrap_or_else(|_| panic!("kernel cell wrote no receipt at {}", receipt_path.display()));
    let receipt: Value = serde_json::from_str(&raw).expect("receipt json");
    let kernel = receipt["kernel_python"].clone();
    assert_eq!(
        receipt,
        json!({
            "state": "done",
            "answer": "{\"where\":\"here\"}",
            "child": null,
            "kernel_python": kernel,
        })
    );
    let observed: Value = serde_json::from_str(
        &std::fs::read_to_string(cwd.join("observed.json")).expect("the call ran in the cwd"),
    )
    .expect("observed json");
    assert_eq!(
        observed,
        json!({
            "cwd": std::fs::canonicalize(&cwd).unwrap().display().to_string(),
            "python": kernel,
            "isolated": 1,
            "inputs": {},
            "rlm_env": [],
        })
    );
}
