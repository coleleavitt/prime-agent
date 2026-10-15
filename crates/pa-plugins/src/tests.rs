//! One plugin instance over the fixture component (`tests/fixtures`): every
//! export, the host imports, the deadline, the memory cap, and recovery
//! after a trap.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wasmtime::component::Component;

use super::bindings::PluginPre;
use super::bindings::prime_agent::plugin::types as wit;
use super::state::State;
use super::*;

/// The fixture plugin, compiled from its component text.
fn fixture() -> Arc<PluginPre<State>> {
    let engine = crate::engine::engine().expect("engine");
    let component = Component::new(engine, include_str!("../tests/fixtures/fixture-plugin.wat"))
        .expect("the fixture component parses");
    crate::engine::prepare_component(engine, &component).expect("the fixture links")
}

fn info(session_id: &str) -> wit::SessionInfo {
    wit::SessionInfo {
        session_id: session_id.to_string(),
        cwd: "/work".to_string(),
        rlm_depth: 0,
        model: "test/m".to_string(),
    }
}

/// A plugin path whose manifest is `manifest` (the component is given).
fn plugin_path(dir: &Path, manifest: &str) -> PathBuf {
    std::fs::write(dir.join("fixture.toml"), manifest).unwrap();
    dir.join("fixture.wasm")
}

type Statuses = Arc<Mutex<Vec<pa_core::features::FeatureStatus>>>;

/// Capture the feature statuses published for `session_id`; keep the sink.
fn capture(session_id: &str) -> (Statuses, pa_core::features::FeatureStatusSink) {
    let seen: Statuses = Arc::default();
    let sink: pa_core::features::FeatureStatusSink = {
        let seen = Arc::clone(&seen);
        Arc::new(move |status| seen.lock().unwrap().push(status))
    };
    pa_core::features::register_feature_status_sink(session_id, &sink);
    (seen, sink)
}

fn ready_status() -> pa_core::features::FeatureStatus {
    pa_core::features::FeatureStatus {
        feature: "plugin:fixture".to_string(),
        line: Some("fixture ready".to_string()),
        status: json!({ "panel": ["panel row one", "panel row two"] }),
    }
}

#[test]
fn the_fixture_loads_names_itself_and_publishes_its_status() {
    let dir = tempfile::tempdir().unwrap();
    let (seen, _sink) = capture("s-load");
    let plugin = SessionPlugin::load(&plugin_path(dir.path(), ""), fixture(), info("s-load"))
        .expect("the fixture loads");
    assert_eq!(
        (plugin.name(), plugin.description(), plugin.session_id()),
        ("fixture", "test plugin", "s-load")
    );
    assert_eq!(*seen.lock().unwrap(), vec![ready_status()]);
}

#[test]
fn every_review_export_answers() {
    let dir = tempfile::tempdir().unwrap();
    let plugin =
        SessionPlugin::load(&plugin_path(dir.path(), ""), fixture(), info("s-review")).unwrap();
    let tool = |name: &str| wit::ToolCall {
        tool_call_id: "c1".to_string(),
        tool_name: name.to_string(),
        args_json: "{}".to_string(),
    };
    let request = |request_type: &str, data: &str| wit::HostRequest {
        request_type: request_type.to_string(),
        data_json: data.to_string(),
    };
    let answers = (
        plugin
            .call(|p, s| p.call_review_tool_call(s, "s-review", &tool("forbidden")))
            .unwrap(),
        plugin
            .call(|p, s| p.call_review_tool_call(s, "s-review", &tool("python_repl")))
            .unwrap(),
        plugin
            .call(|p, s| {
                p.call_review_host_request(
                    s,
                    "s-review",
                    &request("bash.run", r#"{"command":"rm -rf /tmp/x"}"#),
                )
            })
            .unwrap(),
        plugin
            .call(|p, s| {
                p.call_review_host_request(
                    s,
                    "s-review",
                    &request("bash.run", r#"{"command":"ls"}"#),
                )
            })
            .unwrap(),
        plugin
            .call(|p, s| {
                p.call_review_host_request(
                    s,
                    "s-review",
                    &request("computer_use.click", r#"{"note":"rm -rf"}"#),
                )
            })
            .unwrap(),
    );
    assert_eq!(
        answers,
        (
            wit::Verdict::Block("fixture blocks forbidden".to_string()),
            wit::Verdict::Allow,
            wit::Verdict::Block("fixture: destructive command".to_string()),
            wit::Verdict::Allow,
            wit::Verdict::Allow,
        )
    );
}

#[test]
fn prompt_context_after_tool_call_and_call_answer() {
    let dir = tempfile::tempdir().unwrap();
    let plugin =
        SessionPlugin::load(&plugin_path(dir.path(), ""), fixture(), info("s-other")).unwrap();
    let outcome = wit::ToolResult {
        tool_call_id: "c1".to_string(),
        tool_name: "python_repl".to_string(),
        args_json: "{}".to_string(),
        is_error: false,
        text: "out".to_string(),
    };
    let answers = (
        plugin
            .call(|p, s| p.call_prompt_context(s, "s-other", "forbidden prompt"))
            .unwrap(),
        plugin
            .call(|p, s| p.call_prompt_context(s, "s-other", "hello"))
            .unwrap(),
        plugin
            .call(|p, s| p.call_after_tool_call(s, "s-other", &outcome))
            .unwrap(),
        plugin
            .call(|p, s| p.call_call(s, "s-other", "echo", r#"{"a":1}"#))
            .unwrap(),
        plugin
            .call(|p, s| p.call_call(s, "s-other", "nope", "null"))
            .unwrap(),
    );
    assert_eq!(
        answers,
        (
            wit::PromptDecision::Block("fixture refuses".to_string()),
            wit::PromptDecision::Context(Some("fixture context".to_string())),
            Some("fixture note".to_string()),
            Ok(r#"{"a":1}"#.to_string()),
            Err("unknown method".to_string()),
        )
    );
}

#[test]
fn a_trap_drops_the_instance_and_the_next_call_starts_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let (seen, _sink) = capture("s-trap");
    let plugin =
        SessionPlugin::load(&plugin_path(dir.path(), ""), fixture(), info("s-trap")).unwrap();
    let trapped = plugin
        .call(|p, s| p.call_call(s, "s-trap", "trap", "null"))
        .unwrap_err();
    assert!(
        trapped.starts_with("plugin fixture failed: ") && trapped.contains("unreachable"),
        "{trapped}"
    );
    // A fresh instance answers, and it saw `on-session-start` again.
    assert_eq!(
        plugin
            .call(|p, s| p.call_call(s, "s-trap", "echo", "1"))
            .unwrap(),
        Ok("1".to_string())
    );
    assert_eq!(*seen.lock().unwrap(), vec![ready_status(), ready_status()]);
}

#[test]
fn a_call_past_its_deadline_traps() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = SessionPlugin::load(
        &plugin_path(dir.path(), "timeout_ms = 100\n"),
        fixture(),
        info("s-spin"),
    )
    .unwrap();
    let started = Instant::now();
    let error = plugin
        .call(|p, s| p.call_call(s, "s-spin", "spin", "null"))
        .unwrap_err();
    assert!(error.contains("interrupt"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline stopped it: {:?}",
        started.elapsed()
    );
}

#[test]
fn memory_growth_past_the_cap_traps() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = SessionPlugin::load(
        &plugin_path(dir.path(), "memory_mib = 8\n"),
        fixture(),
        info("s-grow"),
    )
    .unwrap();
    let error = plugin
        .call(|p, s| p.call_call(s, "s-grow", "grow", "null"))
        .unwrap_err();
    // The limiter refuses the grow; the fixture traps on the refusal.
    assert!(error.contains("unreachable"), "{error}");
}

#[test]
fn status_for_another_session_is_refused() {
    let (seen, _sink) = capture("s-victim");
    let manifest = crate::manifest::Manifest::default();
    let mut state = State::new("fixture", "s-owner", &manifest).unwrap();
    crate::bindings::prime_agent::plugin::host::Host::set_status(
        &mut state,
        "s-victim".to_string(),
        Some("spoofed".to_string()),
        Vec::new(),
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn project_plugins_load_only_in_a_trusted_workspace() {
    pa_core::workspace_trust::register_gated_project_dir(PLUGINS_DIR, "WebAssembly plugins");
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(agent_dir.join(PLUGINS_DIR)).unwrap();
    let project = cwd
        .join(pa_core::settings::CONFIG_DIR_NAME)
        .join(PLUGINS_DIR);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    for (dir, name) in [
        (agent_dir.join(PLUGINS_DIR), "b.wasm"),
        (agent_dir.join(PLUGINS_DIR), "a.wasm"),
        (agent_dir.join(PLUGINS_DIR), "notes.txt"),
        (project.clone(), "p.wasm"),
    ] {
        std::fs::write(dir.join(name), "x").unwrap();
    }
    let user = vec![
        agent_dir.join(PLUGINS_DIR).join("a.wasm"),
        agent_dir.join(PLUGINS_DIR).join("b.wasm"),
    ];
    assert_eq!(plugin_files(&agent_dir, &cwd), user);
    pa_core::workspace_trust::record(
        &cwd,
        &agent_dir,
        pa_core::workspace_trust::TrustDecision::Trusted,
    )
    .unwrap();
    let mut trusted = user;
    trusted.push(project.join("p.wasm"));
    assert_eq!(plugin_files(&agent_dir, &cwd), trusted);
}
