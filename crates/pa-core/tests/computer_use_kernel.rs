// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Verifier: the bundled computer-use skill's Python package is a thin
//! client of the host in a REAL kernel. One `ipython` cell imports the
//! skill from the source tree and calls its public API; every call rides a
//! `computer_use.*` host request to `pa-computer-use` and comes back as the
//! skill's return shapes and `ComputerUseError`s.
//!
//! The desktop is never touched: the test hides every backend from the
//! host's detection (no `WAYLAND_DISPLAY`/`NIRI_SOCKET`/`DISPLAY`, a `PATH`
//! without `xdotool`), so the requests take the no-backend answers, and the
//! allowlist summary still reads this agent dir's settings file.
#![cfg(target_os = "linux")]

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

fn client_cell(skill_src: &Path, receipt_path: &Path) -> String {
    format!(
        r#"import json, sys, traceback
try:
    sys.path.insert(0, {skill_src:?})
    import computer_use
    from computer_use import ComputerUseError

    def caught(error):
        return [type(error).__name__, error.code, error.message]

    state = await computer_use.get_state(emit=False)
    permissions = await computer_use.permissions_status()
    try:
        await computer_use.get_app("org.example.Editor")
        bind = None
    except ComputerUseError as error:
        bind = caught(error)
    try:
        await computer_use.list_apps()
        listed = None
    except ComputerUseError as error:
        listed = caught(error)
    try:
        computer_use.App("org.example.Editor", "Editor", 1).is_frontmost()
        frontmost = None
    except ComputerUseError as error:
        frontmost = caught(error)
    open({receipt:?}, "w").write(json.dumps({{
        "state": state,
        "permissions": permissions,
        "bind": bind,
        "listed": listed,
        "frontmost": frontmost,
    }}))
except BaseException:
    open({receipt:?}, "w").write(json.dumps({{"traceback": traceback.format_exc()}}))
print("COMPUTER_USE_OK")"#,
        skill_src = skill_src.display().to_string(),
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

#[tokio::test]
async fn the_kernel_client_is_served_by_the_host() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let settings = agent_dir.join("settings");
    std::fs::create_dir_all(&settings).expect("settings dir");
    std::fs::write(
        settings.join("computer-use.toml"),
        "[apps]\nallowed = [\"org.example.Editor\"]\nblocked = [\"com.example.Bank\"]\n",
    )
    .expect("allowlist");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let empty_path = dir.path().join("empty-path");
    std::fs::create_dir_all(&empty_path).expect("empty PATH dir");
    let receipt_path = dir.path().join("computer-use-receipt.json");
    let skill_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills/computer-use/src")
        .canonicalize()
        .expect("the skill source tree");

    let _env = EnvOverride::apply(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(kernel_python.display().to_string()),
        ),
        ("PRIME_AGENT_CODING_AGENT_DIR", None),
        ("PRIME_AGENT_EXECUTABLE", None),
        ("PRIME_API_KEY", None),
        ("WAYLAND_DISPLAY", None),
        ("NIRI_SOCKET", None),
        ("DISPLAY", None),
        ("PATH", Some(empty_path.display().to_string())),
    ]);

    let session = SessionManager::in_memory(&cwd);
    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        Some("checking computer use"),
        vec![(
            "call-1",
            "ipython",
            json!({ "code": client_cell(&skill_src, &receipt_path) }),
        )],
    )));
    provider.push_text_turn("checked");

    let engine = create_session(SessionEngineConfig {
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
        .prompt("check computer use", PromptOptions::default())
        .await
        .expect("prompt");

    let raw = std::fs::read_to_string(&receipt_path)
        .unwrap_or_else(|_| panic!("kernel cell wrote no receipt at {}", receipt_path.display()));
    let receipt: Value = serde_json::from_str(&raw).expect("receipt json");
    assert!(
        receipt.get("traceback").is_none(),
        "the cell raised: {}",
        receipt["traceback"]
    );
    let no_backend = "computer use backend unavailable: no macOS frameworks, no niri Wayland \
                      session, and no Linux X11 tools on this host";
    let unprobed = json!({
        "accessibility": "unknown",
        "screen_recording": "unknown",
        "help": [
            "System Settings > Privacy & Security > Accessibility: add Prime Agent (app control and input).",
            "System Settings > Privacy & Security > Screen Recording: add Prime Agent (window capture).",
            "Prime Agent needs both grants.",
            "Call get_state() to re-check; restart Prime Agent if a fresh Screen Recording grant does not take effect."
        ],
    });
    assert_eq!(receipt["state"]["apps"], json!([]));
    assert_eq!(receipt["state"]["platform"], Value::Null);
    assert_eq!(receipt["state"]["permissions"], unprobed);
    assert_eq!(
        receipt["state"]["allowlist"]["allowed"],
        json!(["org.example.Editor"])
    );
    assert_eq!(
        receipt["state"]["allowlist"]["blocked"],
        json!(["com.example.Bank"])
    );
    assert_eq!(receipt["permissions"], unprobed);
    for key in ["bind", "frontmost"] {
        assert_eq!(
            receipt[key],
            json!(["ComputerUseError", "TRANSPORT_ERROR", no_backend]),
            "{key}"
        );
    }
    assert_eq!(
        receipt["listed"],
        json!([
            "ComputerUseError",
            "TRANSPORT_ERROR",
            "computer use backend unavailable: listing apps needs the macOS workspace"
        ])
    );
}
