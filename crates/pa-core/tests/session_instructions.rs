// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Instruction files and auto memory through a real session: the system
//! prompt carries the session-start files and the memory section, and a
//! `python_repl` result whose cell opened a file below the working
//! directory brings that directory's AGENTS.md (and a matching path-scoped
//! rule) into the next model request as one hidden row, once.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentMessage, Message};
use pa_core::session_engine::PromptOptions;
use pa_core::session_engine::engine::{SessionEngineConfig, create_session};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::{ExecutionMode, ToolDefinition, ToolExecutionResult};
use serde_json::json;

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

/// A stand-in `python_repl` whose every cell "opened" `paths`.
fn python_tool(paths: Vec<String>) -> ToolDefinition {
    ToolDefinition {
        name: pa_types::ai::PYTHON_TOOL_NAME.to_string(),
        label: "python".to_string(),
        description: "fake kernel".to_string(),
        prompt_snippet: String::new(),
        parameters: json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(move |_id, _params, _signal, _on_update| {
            let paths = paths.clone();
            Box::pin(async move {
                let mut result = ToolExecutionResult::text("done");
                result.details = Some(json!({ "status": "ok", "accessedPaths": paths }));
                Ok(result)
            })
        }),
    }
}

fn user_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(match &user.content {
        pa_agent::types::UserContent::Text(text) => text.clone(),
        pa_agent::types::UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                pa_agent::types::UserPart::Text(text) => Some(text.text.as_str()),
                pa_agent::types::UserPart::Image(_) => None,
            })
            .collect(),
    })
}

#[tokio::test]
async fn nested_instruction_files_join_the_run_once_and_memory_rides_the_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let cwd = root.join("project");
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(cwd.join("pkg")).unwrap();
    std::fs::create_dir_all(cwd.join(".claude").join("rules")).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "TOP_LEVEL_RULES").unwrap();
    std::fs::write(cwd.join("pkg").join("AGENTS.md"), "PKG_RULES").unwrap();
    std::fs::write(
        cwd.join(".claude").join("rules").join("rust.md"),
        "---\npaths: \"*.rs\"\n---\nRUST_RULE",
    )
    .unwrap();
    let memory_dir = pa_core::resources::auto_memory::memory_dir(&agent_dir, &cwd);
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("MEMORY.md"),
        "- [Role](user_role.md) - MEMORY_SENTINEL\n",
    )
    .unwrap();

    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        None,
        vec![(
            "call-1",
            pa_types::ai::PYTHON_TOOL_NAME,
            json!({ "code": "open('pkg/lib.rs')" }),
        )],
    );
    provider.push_tool_call_turn(
        None,
        vec![(
            "call-2",
            pa_types::ai::PYTHON_TOOL_NAME,
            json!({ "code": "open('pkg/lib.rs')" }),
        )],
    );
    provider.push_text_turn("done");
    let touched = cwd.join("pkg").join("lib.rs").display().to_string();
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(python_tool(vec![touched]))],
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();

    assert!(engine.system_prompt.contains("TOP_LEVEL_RULES"));
    assert!(!engine.system_prompt.contains("PKG_RULES"));
    assert!(!engine.system_prompt.contains("RUST_RULE"));
    assert!(engine.system_prompt.contains("# auto memory"));
    assert!(engine.system_prompt.contains("MEMORY_SENTINEL"));

    engine
        .prompt("read the package", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let state = engine.session.agent().state().await;
    let nested: Vec<String> = state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(row) if row.payload["customType"] == "nested_instructions" => {
                Some(
                    row.payload["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            }
            _ => None,
        })
        .collect();
    assert_eq!(nested.len(), 1, "delivered once across both batches");
    assert!(nested[0].contains("PKG_RULES"));
    assert!(nested[0].contains("RUST_RULE"));

    // The second model request (after the first batch) carries the row.
    let calls = provider.calls();
    assert_eq!(calls.len(), 3);
    let seen = |index: usize| {
        calls[index]
            .messages
            .iter()
            .filter_map(user_text)
            .any(|text| text.contains("PKG_RULES"))
    };
    assert!(!seen(0));
    assert!(seen(1));
}

#[tokio::test]
async fn auto_memory_and_context_files_stay_out_when_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let cwd = root.join("project");
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "TOP_LEVEL_RULES").unwrap();
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));

    // A subagent carries the project files but no auto memory.
    let child = Box::pin(create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(model.clone()),
        stream_fn: Some(provider.stream_fn()),
        rlm_depth: Some(1),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    assert!(child.system_prompt.contains("TOP_LEVEL_RULES"));
    assert!(!child.system_prompt.contains("# auto memory"));

    // `--no-context-files` drops both.
    let bare = Box::pin(create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        model: Some(model.clone()),
        stream_fn: Some(provider.stream_fn()),
        resource_exclusions: pa_types::daemon::SessionResourceExclusions {
            no_context_files: true,
            ..Default::default()
        },
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    assert!(!bare.system_prompt.contains("TOP_LEVEL_RULES"));
    assert!(!bare.system_prompt.contains("# auto memory"));

    // The setting turns auto memory off.
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{ "autoMemoryEnabled": false }"#,
    )
    .unwrap();
    let off = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    assert!(off.system_prompt.contains("TOP_LEVEL_RULES"));
    assert!(!off.system_prompt.contains("# auto memory"));
}
