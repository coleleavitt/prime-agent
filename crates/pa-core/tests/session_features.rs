//! The session-feature lifecycle seam end to end: a stub feature installed
//! the way the composition root installs one observes a session's tool
//! calls, appends text to the result the model sees, hears the run end,
//! and is flushed. Its own test binary, because installation is
//! process-global.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentMessage, Message, ToolResultContent};
use pa_core::features::{
    FeatureFuture, SessionFeature, SessionFeatureContext, ToolCallObservation,
    ToolResultObservation,
};
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::session_engine::PromptOptions;
use pa_core::{ExecutionMode, ToolDefinition, ToolExecutionResult};

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    SessionStart {
        history: usize,
        artifact_dir: Option<std::path::PathBuf>,
    },
    MessageEnd(String),
    Before(ToolCallObservation),
    After(ToolResultObservation),
    AgentEnd {
        session_id: String,
        rlm_depth: u32,
    },
    Flush,
}

#[derive(Default)]
struct Stub {
    seen: Mutex<Vec<Seen>>,
}

impl SessionFeature for Stub {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, history: &[AgentMessage]) {
        self.seen.lock().unwrap().push(Seen::SessionStart {
            history: history.len(),
            artifact_dir: context.session_artifact_dir.clone(),
        });
    }

    fn on_message_end(&self, _context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        self.seen
            .lock()
            .unwrap()
            .push(Seen::MessageEnd(message.role().to_string()));
    }

    fn before_tool_call(
        &self,
        _context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<()> {
        self.seen.lock().unwrap().push(Seen::Before(call.clone()));
        Box::pin(async {})
    }

    fn after_tool_call(
        &self,
        _context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        self.seen.lock().unwrap().push(Seen::After(result.clone()));
        Box::pin(async { Some("<stub>appended</stub>".to_string()) })
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        self.seen.lock().unwrap().push(Seen::AgentEnd {
            session_id: context.session_id.clone(),
            rlm_depth: context.rlm_depth,
        });
    }

    fn flush(&self, _deadline: Instant) {
        self.seen.lock().unwrap().push(Seen::Flush);
    }
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition {
        name: "echo".to_string(),
        label: "Echo".to_string(),
        description: "Echoes its input".to_string(),
        prompt_snippet: String::new(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(|_id, _params, _signal, _on_update| {
            Box::pin(async move {
                Ok(ToolExecutionResult {
                    host_facts: serde_json::json!({ "fact": 1 }),
                    details: Some(serde_json::json!({ "status": "ok" })),
                    ..ToolExecutionResult::text("echo: hi")
                })
            })
        }),
    }
}

// One end-to-end scenario, asserted as a whole sequence.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn an_installed_feature_observes_tool_calls_and_run_ends() {
    let stub = Arc::new(Stub::default());
    assert!(pa_core::features::install(vec![
        Arc::clone(&stub) as Arc<dyn SessionFeature>
    ]));

    let model = pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
    };
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "echo", serde_json::json!({ "text": "hi" }))],
    );
    provider.push_tool_call_turn(
        None,
        vec![("call-2", "echo", serde_json::json!({ "text": "again" }))],
    );
    provider.push_text_turn("all done");

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir: tmp.path().join("agent"),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(echo_definition())],
        conversation_log_path: Some(tmp.path().join("sessions").join("project").join("s1.jsonl")),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    engine
        .prompt("run the echo tool twice", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    let session_id = engine.session.session_id().await;
    pa_core::features::flush_installed(Duration::from_secs(1));

    let result = |id: &str, text: &str, earlier: usize| ToolResultObservation {
        tool_call_id: id.to_string(),
        tool_name: "echo".to_string(),
        args: serde_json::json!({ "text": text }),
        is_error: false,
        content: vec![ToolResultContent::text("echo: hi")],
        details: serde_json::json!({ "status": "ok" }),
        host_facts: serde_json::json!({ "fact": 1 }),
        earlier_results_of_tool: earlier,
    };
    let call = |id: &str, text: &str| ToolCallObservation {
        tool_call_id: id.to_string(),
        tool_name: "echo".to_string(),
        args: serde_json::json!({ "text": text }),
    };
    assert_eq!(
        *stub.seen.lock().unwrap(),
        vec![
            Seen::SessionStart {
                history: 0,
                artifact_dir: Some(
                    tmp.path()
                        .join("sessions")
                        .join("session-artifacts")
                        .join("s1")
                ),
            },
            // A custom (non-LLM) row the engine adds before the prompt.
            Seen::MessageEnd("custom".to_string()),
            Seen::MessageEnd("user".to_string()),
            Seen::MessageEnd("assistant".to_string()),
            Seen::Before(call("call-1", "hi")),
            Seen::After(result("call-1", "hi", 0)),
            Seen::MessageEnd("toolResult".to_string()),
            Seen::MessageEnd("assistant".to_string()),
            Seen::Before(call("call-2", "again")),
            Seen::After(result("call-2", "again", 1)),
            Seen::MessageEnd("toolResult".to_string()),
            Seen::MessageEnd("assistant".to_string()),
            Seen::AgentEnd {
                session_id,
                rlm_depth: 0
            },
            Seen::Flush,
        ]
    );

    let state = engine.session.agent().state().await;
    let first_result = state
        .messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(result.content.clone()),
            _ => None,
        })
        .expect("a tool result");
    assert_eq!(
        first_result,
        vec![
            ToolResultContent::text("echo: hi"),
            ToolResultContent::text("<stub>appended</stub>"),
        ]
    );
}
