// Pedantic-gate dispositions as pa-core's tests (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]
// The hooks are POSIX shell scripts.
#![cfg(unix)]

//! The hooks feature in real sessions, installed the way `pa-cli` installs
//! it, with Claude Code-shaped hooks in the agent dir's `settings.json`:
//! `SessionStart` and `UserPromptSubmit` context, a refused prompt, a
//! `PreToolUse` refusal (exit 2), a `PostToolUse` note, `Stop`, and, over a
//! real kernel, a `PreToolUse` `Bash` hook that denies a destructive
//! `bash()` through `permissionDecision` and asks the user about another,
//! and a `PostToolUse` `Bash` hook whose note reaches the cell's result.
//! Its own test binary: installation is process-global.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentMessage, Message, ToolResultContent};
use pa_core::features::SessionFeature;
use pa_core::session_engine::PromptOptions;
use pa_core::session_engine::engine::{SessionEngineConfig, create_session};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::{ExecutionMode, ToolDefinition, ToolExecutionResult};
use serde_json::json;

fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        assert!(pa_core::features::install(vec![
            Arc::new(pa_hooks::HooksFeature::new()) as Arc<dyn SessionFeature>
        ]));
    });
}

fn script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path.display().to_string()
}

/// An agent dir whose settings configure one hook per event; `stop_marker`
/// is the file the `Stop` hook writes.
fn agent_dir(root: &Path, stop_marker: &Path) -> std::path::PathBuf {
    let agent_dir = root.join("agent");
    let bin = root.join("hooks");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let start = script(&bin, "start.sh", "cat >/dev/null\necho start-context");
    let submit = script(
        &bin,
        "submit.sh",
        "input=$(cat)\ncase \"$input\" in *forbidden*) echo 'prompt refused by hook' >&2; exit 2;; esac\necho submit-context",
    );
    let danger = script(
        &bin,
        "danger.sh",
        "cat >/dev/null\necho 'no danger' >&2\nexit 2",
    );
    let guard = script(
        &bin,
        "guard.sh",
        "input=$(cat)\ncase \"$input\" in *'rm -rf'*) printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"guard denies rm -rf\"}}';; *ask-me*) printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"guard asks first\"}}';; esac",
    );
    let post_bash = script(
        &bin,
        "post-bash.sh",
        "input=$(cat)\ncase \"$input\" in *hook-harmless*'\"exit_code\":0'*) printf '%s' '{\"decision\":\"block\",\"reason\":\"post-bash saw it exit 0\"}';; esac",
    );
    let post = script(
        &bin,
        "post.sh",
        "cat >/dev/null\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"post note\"}'",
    );
    let stop = script(
        &bin,
        "stop.sh",
        &format!("cat > {:?}", stop_marker.display().to_string()),
    );
    let command = |path: &str| json!([{ "type": "command", "command": path }]);
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({
            "hooks": {
                "SessionStart": [{ "hooks": command(&start) }],
                "UserPromptSubmit": [{ "hooks": command(&submit) }],
                "PreToolUse": [
                    { "matcher": "danger", "hooks": command(&danger) },
                    { "matcher": "Bash", "hooks": command(&guard) },
                ],
                "PostToolUse": [
                    { "matcher": "echo", "hooks": command(&post) },
                    { "matcher": "Bash", "hooks": command(&post_bash) },
                ],
                "Stop": [{ "hooks": command(&stop) }],
            }
        })
        .to_string(),
    )
    .unwrap();
    agent_dir
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
        context_window: 100_000,
        max_tokens: 100,
        max_tokens_explicit: false,
    }
}

fn tool(name: &str, runs: Arc<AtomicUsize>) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        label: name.to_string(),
        description: "test tool".to_string(),
        prompt_snippet: String::new(),
        parameters: json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(move |_id, _params, _signal, _on_update| {
            runs.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(ToolExecutionResult::text("ran")) })
        }),
    }
}

fn text(message: &AgentMessage) -> Option<(String, String)> {
    let join = |content: &[ToolResultContent]| {
        content
            .iter()
            .map(|block| match block {
                ToolResultContent::Text(text) => text.text.clone(),
                ToolResultContent::Image(image) => format!("{image:?}"),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    match message {
        AgentMessage::Custom(row) if row.payload["customType"] == "feature_context" => Some((
            "context".to_string(),
            row.payload["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )),
        AgentMessage::Standard(Message::ToolResult(result)) => Some((
            format!("{} error={}", result.tool_name, result.is_error),
            join(&result.content),
        )),
        _ => None,
    }
}

#[tokio::test]
async fn hooks_shape_a_session() {
    install();
    let tmp = tempfile::tempdir().unwrap();
    let stop_marker = tmp.path().join("stopped.json");
    let agent_dir = agent_dir(tmp.path(), &stop_marker);
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        None,
        vec![
            ("call-1", "danger", json!({})),
            ("call-2", "echo", json!({})),
        ],
    );
    provider.push_text_turn("done");
    let danger_runs = Arc::new(AtomicUsize::new(0));
    let echo_runs = Arc::new(AtomicUsize::new(0));
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![
            bridge_tool(tool("danger", Arc::clone(&danger_runs))),
            bridge_tool(tool("echo", Arc::clone(&echo_runs))),
        ],
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();

    let refused = engine
        .prompt("a forbidden request", PromptOptions::default())
        .await
        .expect_err("the UserPromptSubmit hook refuses it");
    assert_eq!(
        refused.to_string(),
        "The prompt was refused: prompt refused by hook"
    );
    engine
        .prompt("use the tools", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    assert_eq!(
        (
            danger_runs.load(Ordering::SeqCst),
            echo_runs.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
    let state = engine.session.agent().state().await;
    let rows: Vec<(String, String)> = state.messages.iter().filter_map(text).collect();
    assert_eq!(
        rows,
        vec![
            // The refused prompt left the start context for the first
            // admitted one.
            (
                "context".to_string(),
                "start-context\n\nsubmit-context".to_string()
            ),
            ("danger error=true".to_string(), "no danger".to_string()),
            (
                "echo error=false".to_string(),
                "ran | post note".to_string()
            ),
        ]
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let stopped = loop {
        if let Ok(text) = std::fs::read_to_string(&stop_marker) {
            if !text.is_empty() {
                break text;
            }
        }
        assert!(Instant::now() < deadline, "the Stop hook never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let stopped: serde_json::Value = serde_json::from_str(&stopped).unwrap();
    assert_eq!(
        (
            &stopped["hook_event_name"],
            &stopped["stop_hook_active"],
            &stopped["session_id"]
        ),
        (
            &json!("Stop"),
            &json!(false),
            &json!(engine.session.session_id().await)
        )
    );
}

#[tokio::test]
async fn the_first_prompt_carries_the_session_start_context() {
    install();
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = agent_dir(tmp.path(), &tmp.path().join("stopped.json"));
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_text_turn("hello");
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    engine.prompt("hi", PromptOptions::default()).await.unwrap();
    engine.session.agent().wait_for_idle().await;
    let state = engine.session.agent().state().await;
    let rows: Vec<(String, String)> = state.messages.iter().filter_map(text).collect();
    assert_eq!(
        rows,
        vec![(
            "context".to_string(),
            "start-context\n\nsubmit-context".to_string()
        )]
    );
}

#[tokio::test]
async fn bash_hooks_deny_ask_about_and_follow_the_kernels_commands() {
    let Some(kernel_python) =
        pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
    else {
        return;
    };
    install();
    // Process env: this is the only test in the binary that sets it.
    std::env::set_var(
        "PRIME_AGENT_KERNEL_PYTHON",
        kernel_python.display().to_string(),
    );
    std::env::remove_var("PRIME_AGENT_CODING_AGENT_DIR");
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = agent_dir(tmp.path(), &tmp.path().join("stopped.json"));
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let receipt = tmp.path().join("receipt.json");
    let cell = format!(
        "import json\nout = {{}}\n\
         for key, command in [('destructive', 'rm -rf /nonexistent-hook-target'), ('asked', 'echo ask-me'), ('refused', 'echo ask-me again'), ('harmless', 'echo hook-harmless')]:\n\
         \x20   try:\n\
         \x20       out[key] = ['ok', str(await bash(command))]\n\
         \x20   except Exception as error:\n\
         \x20       out[key] = ['error', str(error)]\n\
         open({receipt:?}, 'w').write(json.dumps(out))\n",
        receipt = receipt.display().to_string(),
    );
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "python_repl", json!({ "code": cell }))],
    );
    provider.push_text_turn("done");
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        daemonless: true,
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    // A client surface answers the hook's questions: the first is allowed,
    // the second refused.
    let session_id = engine.session.session_id().await;
    let surface = pa_core::features::register_confirmation_surface(&session_id);
    let answerer = {
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let mut answers = vec![false, true];
            while let Some(allow) = answers.pop() {
                let question = loop {
                    if let Some(question) =
                        pa_core::features::pending_confirmations(&session_id).pop()
                    {
                        break question;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                assert_eq!(question.reason, "guard asks first");
                assert!(
                    question.action.starts_with("Run `echo ask-me"),
                    "{question:?}"
                );
                assert!(pa_core::features::answer_confirmation(
                    &session_id,
                    &question.id,
                    allow
                ));
            }
        })
    };
    engine
        .prompt("run the cell", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    answerer.await.unwrap();
    drop(surface);
    let state = engine.session.agent().state().await;
    let cell_result = state
        .messages
        .iter()
        .filter_map(text)
        .find(|(tool, _)| tool.starts_with("python_repl"))
        .map(|(_, text)| text)
        .unwrap_or_default();
    assert!(
        cell_result.contains("Hook feedback on `echo hook-harmless`: post-bash saw it exit 0"),
        "{cell_result}"
    );
    engine.dispose_kernel().await;
    let raw = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|_| panic!("the cell wrote no receipt at {}", receipt.display()));
    let out: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        out["destructive"],
        json!(["error", "bash.run refused: guard denies rm -rf"]),
        "{out}"
    );
    assert_eq!(out["asked"][0], "ok", "{out}");
    assert_eq!(
        out["refused"],
        json!([
            "error",
            "bash.run refused: guard asks first (the user refused it)"
        ]),
        "{out}"
    );
    assert_eq!(out["harmless"][0], "ok", "{out}");
    assert!(
        out["harmless"][1]
            .as_str()
            .unwrap_or_default()
            .contains("hook-harmless"),
        "{out}"
    );
}
