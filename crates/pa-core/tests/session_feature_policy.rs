// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! The session-feature policy seams end to end: an installed feature refuses
//! a tool call (the model sees the reason as the tool's error and the tool
//! never runs), adds hidden context ahead of an admitted prompt, refuses a
//! prompt, and, over a real kernel, refuses host requests the kernel sends,
//! the kernel's `bash()` included. Its own test binary, because
//! installation is process-global.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentMessage, Message, ToolResultContent};
use pa_core::features::{
    FeatureFuture,
    FeatureVerdict,
    HostRequestObservation,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
};
use pa_core::session_engine::PromptOptions;
use pa_core::session_engine::engine::{SessionEngineConfig, create_session};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::{ExecutionMode, ToolDefinition, ToolExecutionResult};
use serde_json::json;

/// Refuses the `danger` tool, any host request of type `test.forbidden`, and
/// any request whose payload mentions `forbidden-marker`; refuses the prompt
/// `refuse me` and adds context to every other.
struct Policy;

impl SessionFeature for Policy {
    fn name(&self) -> &'static str {
        "policy"
    }

    fn review_tool_call(
        &self,
        _context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let verdict = if call.tool_name == "danger" {
            FeatureVerdict::Block("policy forbids the danger tool".to_string())
        } else {
            FeatureVerdict::Allow
        };
        Box::pin(async move { verdict })
    }

    fn review_host_request(
        &self,
        _context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let verdict = if request.request_type == "test.forbidden"
            || request.data.to_string().contains("forbidden-marker")
        {
            FeatureVerdict::Block("policy says no".to_string())
        } else {
            FeatureVerdict::Allow
        };
        Box::pin(async move { verdict })
    }

    fn prompt_context(
        &self,
        _context: &Arc<SessionFeatureContext>,
        prompt: &str,
    ) -> FeatureFuture<PromptDecision> {
        let decision = if prompt == "refuse me" {
            PromptDecision::Block("policy refuses this prompt".to_string())
        } else {
            PromptDecision::Continue {
                context: Some("policy context".to_string()),
            }
        };
        Box::pin(async move { decision })
    }
}

fn install_policy() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        assert!(pa_core::features::install(vec![
            Arc::new(Policy) as Arc<dyn SessionFeature>
        ]));
    });
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

fn danger_tool(runs: Arc<AtomicUsize>) -> ToolDefinition {
    ToolDefinition {
        name: "danger".to_string(),
        label: "Danger".to_string(),
        description: "Must never run".to_string(),
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

fn text_of(content: &pa_agent::types::UserContent) -> String {
    match content {
        pa_agent::types::UserContent::Text(text) => text.clone(),
        pa_agent::types::UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                pa_agent::types::UserPart::Text(text) => Some(text.text.as_str()),
                pa_agent::types::UserPart::Image(_) => None,
            })
            .collect(),
    }
}

#[tokio::test]
async fn a_feature_refuses_a_tool_call_and_shapes_prompts() {
    install_policy();
    let model = model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(None, vec![("call-1", "danger", json!({}))]);
    provider.push_text_turn("refused, moving on");
    let runs = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir: tmp.path().join("agent"),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(danger_tool(Arc::clone(&runs)))],
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();

    let refused = engine
        .prompt("refuse me", PromptOptions::default())
        .await
        .expect_err("the policy refuses the prompt");
    assert_eq!(
        refused.to_string(),
        "The prompt was refused: policy refuses this prompt"
    );

    engine
        .prompt("call the danger tool", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    assert_eq!(runs.load(Ordering::SeqCst), 0, "a refused tool never runs");

    let state = engine.session.agent().state().await;
    let rows: Vec<(String, String)> = state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(row) if row.payload["customType"] == "feature_context" => Some((
                "feature_context".to_string(),
                row.payload["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )),
            AgentMessage::Standard(Message::User(user)) => {
                Some(("user".to_string(), text_of(&user.content)))
            }
            AgentMessage::Standard(Message::ToolResult(result)) => Some((
                format!("toolResult error={}", result.is_error),
                result
                    .content
                    .iter()
                    .map(|block| match block {
                        ToolResultContent::Text(text) => text.text.clone(),
                        ToolResultContent::Image(image) => format!("{image:?}"),
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("feature_context".to_string(), "policy context".to_string()),
            ("user".to_string(), "call the danger tool".to_string()),
            (
                "toolResult error=true".to_string(),
                "policy forbids the danger tool".to_string()
            ),
        ],
        "the refused prompt left nothing; the admitted one carries the context row first"
    );
}

/// The kernel Python with prime-agent-runtime installed; skipped without one.
#[cfg(unix)]
fn kernel_python() -> Option<std::path::PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

#[cfg(unix)]
#[tokio::test]
async fn a_feature_refuses_the_kernels_host_requests_and_bash() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    install_policy();
    // Process env: this is the only test in the binary that sets it.
    std::env::set_var(
        "PRIME_AGENT_KERNEL_PYTHON",
        kernel_python.display().to_string(),
    );
    std::env::remove_var("PRIME_AGENT_CODING_AGENT_DIR");
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let receipt = tmp.path().join("receipt.json");
    let cell = format!(
        "import json\nfrom rlm import host_request\nout = {{}}\n\
         for key, request in [('custom', lambda: host_request('test.forbidden', {{}})),\n\
         ('bash_refused', lambda: bash('echo forbidden-marker')),\n\
         ('bash_allowed', lambda: bash('echo allowed-marker'))]:\n\
         \x20   try:\n\
         \x20       out[key] = 'ok: ' + str(await request())\n\
         \x20   except Exception as error:\n\
         \x20       out[key] = 'error: ' + str(error)\n\
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
        agent_dir: tmp.path().join("agent"),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        daemonless: true,
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    engine
        .prompt("run the cell", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    engine.dispose_kernel().await;

    let raw = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|_| panic!("the cell wrote no receipt at {}", receipt.display()));
    let out: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let field = |key: &str| out[key].as_str().unwrap_or_default().to_string();
    assert!(
        field("custom").starts_with("error: ")
            && field("custom").contains("refused: policy says no"),
        "{out}"
    );
    assert!(
        field("bash_refused").starts_with("error: ")
            && field("bash_refused").contains("refused: policy says no"),
        "{out}"
    );
    assert!(
        field("bash_allowed").starts_with("ok: ")
            && field("bash_allowed").contains("allowed-marker"),
        "{out}"
    );
}
