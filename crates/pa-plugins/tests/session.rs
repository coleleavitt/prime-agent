// Pedantic-gate dispositions as pa-core's tests (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! The plugins feature in a real session, installed the way `pa-cli`
//! installs it, with the fixture plugin in the agent dir: it names itself,
//! publishes its status panel, announces its tools and adds prompt context,
//! refuses the `forbidden` tool, runs its `/wasm` command, and, over a real
//! kernel, refuses a destructive `bash()` command, asks the user about
//! another, follows what ran with a note on the cell's result, and answers
//! `plugin.list` / `plugin.call` with a declared tool's arguments validated
//! (what the `wasm-plugins` skill wraps; a test kernel is a given
//! interpreter, so the skill itself is not installed into it). Its own test
//! binary: installation is process-global.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentMessage, Message, ToolResultContent};
use pa_core::features::{FeatureStatus, FeatureStatusSink, SessionFeature};
use pa_core::session_engine::PromptOptions;
use pa_core::session_engine::engine::{SessionEngine, SessionEngineConfig, create_session};
use pa_core::session_engine::session_commands::{SessionCommandParams, execute_session_command};
use pa_core::session_engine::slash_commands::{SlashCommandRegistry, parse_session_command};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::{ExecutionMode, ToolDefinition, ToolExecutionResult};
use serde_json::json;

const FIXTURE: &str = include_str!("fixtures/fixture-plugin.wat");

fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        assert!(pa_core::features::install(vec![
            Arc::new(pa_plugins::PluginsFeature::new()) as Arc<dyn SessionFeature>
        ]));
    });
}

/// An agent dir with the fixture installed as `plugins/fixture.wasm` (the
/// component's text form: the test build parses either).
fn agent_dir(root: &std::path::Path) -> std::path::PathBuf {
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(agent_dir.join("plugins")).unwrap();
    std::fs::write(agent_dir.join("plugins").join("fixture.wasm"), FIXTURE).unwrap();
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

/// Run `text` as a session command: the result row's text, or the error.
async fn run_command(engine: &SessionEngine, text: &str) -> Result<String, String> {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text)
        .unwrap_or_else(|| panic!("{text} is a session command"));
    let mut autonomous = pa_core::autonomous::create_autonomous_runtime_state(None, None);
    let model: pa_types::ai::Model = serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap();
    let global_harness = tempfile::tempdir().unwrap();
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: global_harness.path().to_path_buf(),
        autonomous: &mut autonomous,
    };
    let execution = execute_session_command(engine, &mut params, &command, &[]).await;
    if let Some(error) = execution.error {
        return Err(error);
    }
    match &execution.messages.last().expect("a result row").content {
        pa_types::ai::UserContent::Text(text) => Ok(text.clone()),
        pa_types::ai::UserContent::Blocks(_) => Ok(String::new()),
    }
}

fn forbidden_tool(runs: Arc<AtomicUsize>) -> ToolDefinition {
    ToolDefinition {
        name: "forbidden".to_string(),
        label: "Forbidden".to_string(),
        description: "The fixture plugin refuses it".to_string(),
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

#[tokio::test]
async fn the_fixture_plugin_shapes_a_session() {
    install();
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = agent_dir(tmp.path());
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(None, vec![("call-1", "forbidden", json!({}))]);
    provider.push_text_turn("refused");
    let runs = Arc::new(AtomicUsize::new(0));
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(forbidden_tool(Arc::clone(&runs)))],
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    let session_id = engine.session.session_id().await;
    let statuses: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
    let sink: FeatureStatusSink = {
        let statuses = Arc::clone(&statuses);
        Arc::new(move |status| statuses.lock().unwrap().push(status))
    };
    pa_core::features::register_feature_status_sink(&session_id, &sink);

    let refused = engine
        .prompt("forbidden prompt", PromptOptions::default())
        .await
        .expect_err("the fixture refuses the prompt");
    assert_eq!(
        refused.to_string(),
        "The prompt was refused: plugin fixture: fixture refuses"
    );
    engine
        .prompt("call the forbidden tool", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a refused tool never runs");

    let state = engine.session.agent().state().await;
    let context_rows: Vec<String> = state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(row) if row.payload["customType"] == "feature_context" => {
                row.payload["content"].as_str().map(str::to_string)
            }
            _ => None,
        })
        .collect();
    // The first admitted prompt announces the plugin's tools; the refused
    // one before it announced nothing.
    assert_eq!(
        context_rows,
        vec![
            "The user's WebAssembly plugins offer these tools. Call one from the Python REPL as \
             `await wasm_plugins.call(\"<plugin>\", \"<tool>\", **args)`; `await \
             wasm_plugins.tools()` lists them again.\n\
             - fixture: echo(text: string) - Echo the arguments back\n\nfixture context"
                .to_string()
        ]
    );
    let result: Vec<String> = state
        .messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(
                result
                    .content
                    .iter()
                    .map(|block| match block {
                        ToolResultContent::Text(text) => text.text.clone(),
                        ToolResultContent::Image(image) => format!("{image:?}"),
                    })
                    .collect(),
            ),
            _ => None,
        })
        .expect("a tool result");
    assert_eq!(
        result,
        vec!["plugin fixture: fixture blocks forbidden".to_string()]
    );
    // `/wasm` lists the plugins' commands and runs one.
    assert_eq!(
        run_command(&engine, "/wasm").await,
        Ok("fixture - test plugin\n  /wasm fixture hello <text> - Echo the arguments".to_string())
    );
    assert_eq!(
        run_command(&engine, "/wasm fixture hello  big   world").await,
        Ok("big   world".to_string())
    );
    assert_eq!(
        run_command(&engine, "/wasm fixture nope").await,
        Err("plugin fixture has no command nope; /wasm fixture lists them".to_string())
    );
    assert_eq!(
        run_command(&engine, "/wasm other x").await,
        Err("no plugin named other is loaded; /wasm lists them".to_string())
    );
    // The plugin loaded off the creation path, maybe before the sink
    // existed: a held status reaches the sink when it registers. Its panel
    // is what the TUI shows under the editor.
    assert_eq!(
        *statuses.lock().unwrap(),
        vec![FeatureStatus {
            feature: "plugin:fixture".to_string(),
            line: Some("fixture ready".to_string()),
            status: json!({ "panel": ["panel row one", "panel row two"] }),
        }]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_fixture_plugin_reviews_the_kernels_bash_and_answers_calls() {
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
    let agent_dir = agent_dir(tmp.path());
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let receipt = tmp.path().join("receipt.json");
    let cell = format!(
        "import json\nfrom rlm import host_request\nout = {{}}\n\
         for key, request in [\n\
         \x20   ('destructive', lambda: bash('rm -rf /nonexistent-plugin-target')),\n\
         \x20   ('harmless', lambda: bash('echo harmless-marker')),\n\
         \x20   ('asked', lambda: bash('echo ask-me')),\n\
         \x20   ('listed', lambda: host_request('plugin.list', {{}})),\n\
         \x20   ('echoed', lambda: host_request('plugin.call', {{'plugin': 'fixture', 'method': 'echo', 'data': {{'text': 'hi'}}}})),\n\
         \x20   ('invalid', lambda: host_request('plugin.call', {{'plugin': 'fixture', 'method': 'echo', 'data': {{'k': 1}}}})),\n\
         \x20   ('unknown', lambda: host_request('plugin.call', {{'plugin': 'fixture', 'method': 'nope', 'data': {{}}}})),\n\
         ]:\n\
         \x20   try:\n\
         \x20       out[key] = ['ok', await request()]\n\
         \x20   except Exception as error:\n\
         \x20       out[key] = ['error', str(error)]\n\
         open({receipt:?}, 'w').write(json.dumps(out, default=str))\n",
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
    // A client surface allows the plugin's question.
    let session_id = engine.session.session_id().await;
    let surface = pa_core::features::register_confirmation_surface(&session_id);
    let answerer = {
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let question = loop {
                if let Some(question) = pa_core::features::pending_confirmations(&session_id).pop()
                {
                    break question;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            };
            assert_eq!(
                (question.action.as_str(), question.reason.as_str()),
                ("Run `echo ask-me`", "plugin fixture: fixture asks first")
            );
            assert!(pa_core::features::answer_confirmation(
                &session_id,
                &question.id,
                true
            ));
        })
    };
    engine
        .prompt("run the cell", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    answerer.await.unwrap();
    drop(surface);
    // Every `bash.run` the plugin let run left its note on the cell's result.
    let state = engine.session.agent().state().await;
    let cell_result: Vec<String> = state
        .messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text(text) => Some(text.text.clone()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .expect("the cell's result");
    assert_eq!(
        cell_result
            .iter()
            .filter(|text| *text == "plugin fixture: fixture after bash.run")
            .count(),
        2,
        "{cell_result:?}"
    );
    engine.dispose_kernel().await;

    let raw = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|_| panic!("the cell wrote no receipt at {}", receipt.display()));
    let out: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(out["destructive"][0], "error", "{out}");
    assert!(
        out["destructive"][1]
            .as_str()
            .unwrap_or_default()
            .contains("refused: plugin fixture: fixture: destructive command"),
        "{out}"
    );
    assert_eq!(out["harmless"][0], "ok", "{out}");
    assert!(
        out["harmless"][1]
            .as_str()
            .unwrap_or_default()
            .contains("harmless-marker"),
        "{out}"
    );
    assert_eq!(out["listed"][0], "ok", "{out}");
    assert_eq!(out["listed"][1][0]["name"], "fixture", "{out}");
    assert_eq!(out["listed"][1][0]["description"], "test plugin", "{out}");
    assert_eq!(
        out["listed"][1][0]["tools"][0]["signature"], "echo(text: string)",
        "{out}"
    );
    assert_eq!(out["listed"][1][0]["commands"][0]["name"], "hello", "{out}");
    assert_eq!(out["asked"][0], "ok", "{out}");
    assert_eq!(out["echoed"], json!(["ok", { "text": "hi" }]), "{out}");
    assert_eq!(out["invalid"][0], "error", "{out}");
    assert!(
        out["invalid"][1]
            .as_str()
            .unwrap_or_default()
            .contains("Required property 'text' is missing"),
        "{out}"
    );
    assert_eq!(out["unknown"][0], "error", "{out}");
    assert!(
        out["unknown"][1]
            .as_str()
            .unwrap_or_default()
            .contains("plugin fixture: unknown method"),
        "{out}"
    );
}
