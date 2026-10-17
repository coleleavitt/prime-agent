//! The session context rows and reminders end to end: what the provider
//! actually receives, without inference or a Python kernel.
use pa_agent::scripted::ScriptedProvider;

use super::*;
use crate::session_engine::tool_bridge::bridge_tool;
use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};

fn model() -> Model {
    Model {
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

/// A tool that rewrites `path` when called (an edit the model did not make
/// through the kernel, like a formatter or the user's editor).
fn touch_tool(path: std::path::PathBuf) -> ToolDefinition {
    ToolDefinition {
        name: "touch".to_string(),
        label: "Touch".to_string(),
        description: "Rewrites the watched file".to_string(),
        prompt_snippet: String::new(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(move |_id, _params, _signal, _on_update| {
            let path = path.clone();
            Box::pin(async move {
                std::fs::write(&path, "line one\nline TWO\n").unwrap();
                let file = std::fs::File::options().write(true).open(&path).unwrap();
                file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
                    .unwrap();
                Ok(ToolExecutionResult::text("touched"))
            })
        }),
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    cwd: std::path::PathBuf,
    provider: Arc<ScriptedProvider>,
    engine: SessionEngine,
}

async fn fixture(settings: Option<serde_json::Value>, tools: Vec<ToolDefinition>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "RULES_V1").unwrap();
    if let Some(settings) = settings {
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir,
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        tools: tools.into_iter().map(bridge_tool).collect(),
        daemonless: true,
        prewarm_ipython_kernel: Some(false),
        ..Default::default()
    })
    .await
    .unwrap();
    Fixture {
        _tmp: tmp,
        cwd,
        provider,
        engine,
    }
}

fn text_of(message: &pa_agent::types::Message) -> String {
    serde_json::to_string(message).unwrap()
}

#[tokio::test]
async fn the_first_turn_carries_the_context_and_later_turns_keep_the_prefix() {
    let fixture = fixture(None, Vec::new()).await;
    fixture.provider.push_text_turn("one");
    fixture.provider.push_text_turn("two");
    fixture
        .engine
        .prompt("hello", PromptOptions::default())
        .await
        .unwrap();
    fixture
        .engine
        .prompt("again", PromptOptions::default())
        .await
        .unwrap();
    let calls = fixture.provider.calls();
    assert_eq!(calls.len(), 2);
    let cwd = fixture.cwd.display().to_string();
    // The system prompt names no directory, date or project rule.
    let system = calls[0].system_prompt.as_deref().unwrap();
    assert!(!system.contains(&cwd));
    assert!(!system.contains("RULES_V1"));
    assert_eq!(calls[1].system_prompt.as_deref(), Some(system));
    // The first message is the context row, then the prompt.
    let first = text_of(&calls[0].messages[0]);
    assert!(first.contains("<system-reminder>"));
    assert!(first.contains(&format!("Working directory: {cwd}")));
    assert!(first.contains("RULES_V1"));
    assert!(first.contains("Today's date is "));
    // (The first-turn harness digest rides between them.)
    assert!(text_of(calls[0].messages.last().unwrap()).contains("hello"));
    // Nothing changed: the second request extends the first, byte for byte.
    assert_eq!(
        calls[1].messages[..calls[0].messages.len()],
        calls[0].messages[..]
    );
    assert_eq!(calls[1].messages.len(), calls[0].messages.len() + 2);
    assert!(text_of(calls[1].messages.last().unwrap()).contains("again"));
    // The row persists hidden, with its fingerprint.
    let entries = fixture.engine.session.entries().await;
    let rows: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            pa_types::session::FileEntry::CustomMessage { payload, .. }
                if payload.custom_type
                    == crate::session_engine::context_rows::SESSION_CONTEXT_CUSTOM_TYPE =>
            {
                Some(payload.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "one full row for the session");
    assert!(!rows[0].display);
    assert!(rows[0].details.as_ref().unwrap()["fingerprint"].is_object());
}

#[tokio::test]
async fn a_changed_instruction_file_rides_the_next_turn_as_an_update() {
    let fixture = fixture(None, Vec::new()).await;
    fixture.provider.push_text_turn("one");
    fixture.provider.push_text_turn("two");
    fixture
        .engine
        .prompt("hello", PromptOptions::default())
        .await
        .unwrap();
    std::fs::write(fixture.cwd.join("AGENTS.md"), "RULES_V2").unwrap();
    fixture
        .engine
        .prompt("again", PromptOptions::default())
        .await
        .unwrap();
    let calls = fixture.provider.calls();
    let earlier = calls[0].messages.len();
    assert_eq!(calls[1].messages[..earlier], calls[0].messages[..]);
    let update = text_of(&calls[1].messages[earlier + 1]);
    assert!(
        update.contains("These instruction files changed"),
        "{update}"
    );
    assert!(update.contains("RULES_V2"));
    assert!(!update.contains("Working directory"));
    assert!(text_of(&calls[1].messages[earlier + 2]).contains("again"));
}

#[tokio::test]
async fn a_tool_batch_brings_the_changed_file_note_and_the_token_countdown() {
    let tmp = tempfile::tempdir().unwrap();
    let watched = tmp.path().join("watched.txt");
    std::fs::write(&watched, "line one\nline two\n").unwrap();
    let fixture = fixture(None, vec![touch_tool(watched.clone())]).await;
    // The model read the file in an earlier cell (what the kernel reports).
    fixture
        .engine
        .session
        .context_rows()
        .unwrap()
        .reminders()
        .files()
        .queue(crate::kernel::shared::KernelFileAccess {
            reads: vec![watched.clone()],
            writes: Vec::new(),
        });
    // The tool turn reports its usage: the countdown counts from it.
    let mut steps = pa_agent::scripted::tool_call_turn_steps(
        &model(),
        None,
        vec![("call-1", "touch", serde_json::json!({}))],
    );
    for step in &mut steps {
        if let pa_agent::scripted::ScriptStep::Event(event) = step {
            if let pa_agent::stream::AssistantMessageEvent::Done { message, .. } = event.as_mut() {
                message.usage.input = 4_000;
            }
        }
    }
    fixture
        .provider
        .push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
    fixture.provider.push_text_turn("done");
    fixture
        .engine
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    let calls = fixture.provider.calls();
    assert_eq!(calls.len(), 2);
    let after_batch = text_of(calls[1].messages.last().unwrap());
    assert!(
        after_batch.contains(&format!(
            "Note: {} changed on disk since you last read it.",
            watched.display()
        )),
        "{after_batch}"
    );
    assert!(after_batch.contains("line TWO"));
    // 100k window - (4000 input + 0 output).
    assert!(after_batch.contains("<total_tokens>96000 tokens left</total_tokens>"));
    assert_eq!(after_batch.matches("<system-reminder>").count(), 1);
}

#[tokio::test]
async fn reminders_switched_off_add_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let watched = tmp.path().join("watched.txt");
    std::fs::write(&watched, "line one\nline two\n").unwrap();
    let fixture = fixture(
        Some(serde_json::json!({
            "reminders": { "changedFiles": false, "tokensLeft": false, "taskList": false }
        })),
        vec![touch_tool(watched.clone())],
    )
    .await;
    fixture
        .engine
        .session
        .context_rows()
        .unwrap()
        .reminders()
        .files()
        .queue(crate::kernel::shared::KernelFileAccess {
            reads: vec![watched],
            writes: Vec::new(),
        });
    fixture
        .provider
        .push_tool_call_turn(None, vec![("call-1", "touch", serde_json::json!({}))]);
    fixture.provider.push_text_turn("done");
    fixture
        .engine
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    let calls = fixture.provider.calls();
    // The second call ends with the tool result: no reminder row joined.
    assert!(matches!(
        calls[1].messages.last(),
        Some(pa_agent::types::Message::ToolResult(_))
    ));
}
