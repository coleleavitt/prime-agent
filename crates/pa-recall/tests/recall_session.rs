//! Workspace Recall inside a real session engine: installed the way
//! `pa-cli` installs it, the block lands on the first `ipython` result only
//! and the run end rewrites the mark. Its own test binary, because feature
//! installation is process-global.

mod support;

use std::sync::Arc;
use std::time::Duration;

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentMessage, Message, ToolResultContent};
use pa_core::features::SessionFeature;
use pa_core::session_engine::PromptOptions;
use pa_core::session_engine::engine::{SessionEngineConfig, create_session};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::{ExecutionMode, ToolDefinition, ToolExecutionResult};
use pa_recall::{RecallOptions, WorkspaceRecall, read_recall_mark};
use support::*;

/// A stand-in `ipython` tool: no kernel, just a cell result.
fn ipython_definition() -> ToolDefinition {
    ToolDefinition {
        name: "ipython".to_string(),
        label: "ipython".to_string(),
        description: "Execute a test IPython cell".to_string(),
        prompt_snippet: String::new(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "code": { "type": "string" } },
            "required": ["code"]
        }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(|_id, _params, _signal, _on_update| {
            Box::pin(async move { Ok(ToolExecutionResult::text("cell ran")) })
        }),
    }
}

fn text_of(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Install recall process-wide once, the way `pa-cli` does.
fn install_recall() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let recall = WorkspaceRecall::new(RecallOptions {
            enabled: Some(Arc::new(|| true)),
            ..RecallOptions::default()
        });
        assert!(pa_core::features::install(vec![
            Arc::new(recall) as Arc<dyn SessionFeature>
        ]));
    });
}

fn scripted_model() -> pa_agent::types::Model {
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

#[tokio::test]
async fn the_block_lands_on_the_first_ipython_result_and_the_run_end_rewrites_the_mark() {
    install_recall();

    let temp = TempDirs::new();
    let repo = temp.dir("prime-agent-recall-session-");
    let agent = temp.dir("prime-agent-recall-agentdir-");
    write_three_files(&repo);
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "a.txt", "b.txt", "c.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let seeded = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    write(&repo.join("b.txt"), "bravo, edited before the session\n");

    let model = scripted_model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        None,
        vec![("recall-1", "ipython", serde_json::json!({ "code": "ls" }))],
    );
    provider.push_tool_call_turn(
        None,
        vec![(
            "recall-2",
            "ipython",
            serde_json::json!({ "code": "ls again" }),
        )],
    );
    provider.push_text_turn("done");
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd: repo.clone(),
        agent_dir: agent.clone(),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(ipython_definition())],
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    engine
        .prompt("orient yourself", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let state = engine.session.agent().state().await;
    let texts: Vec<String> = state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(text_of(&result.content)),
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 2);
    assert!(
        texts[0].starts_with("cell ran\n<workspace_recall>"),
        "{}",
        texts[0]
    );
    let block = &texts[0][texts[0].find("<workspace_recall>").unwrap()..];
    assert_eq!(section(block, "Changed"), ["b.txt"]);
    assert!(block.contains(&format!("EXPIRED (changed paths: b.txt): `{TSGO_CLAIM}`")));
    assert_eq!(texts[1], "cell ran");

    pa_core::features::flush_installed(Duration::from_secs(30));
    let mark = read_recall_mark(&root(&repo), &agent).expect("the run end wrote a mark");
    let dirty: Vec<&str> = mark
        .state
        .dirty
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    assert_eq!(dirty, ["b.txt"]);
    let commands: Vec<&str> = mark
        .claims
        .iter()
        .map(|claim| claim.command.as_str())
        .collect();
    assert_eq!(commands, [TSGO_CLAIM]);
    assert!(mark.written_at >= seeded.mark.written_at);
}

/// The kernel Python the sandbox bootstrap installed; the live test is
/// skipped without it.
fn kernel_python() -> Option<std::path::PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

/// The whole chain on a live kernel: a cell's `bash("make")` exits 0 while
/// the workspace holds still, the runtime reports it on the cell's `done`
/// frame, and the run end's mark records it as a build claim.
#[tokio::test]
async fn a_live_kernel_cells_build_command_becomes_a_claim() {
    if kernel_python().is_none() {
        return;
    }
    install_recall();
    let temp = TempDirs::new();
    let repo = temp.dir("prime-agent-recall-kernel-");
    let agent = temp.dir("prime-agent-recall-agentdir-");
    write(&repo.join("Makefile"), "all:\n\t@true\n");
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "Makefile"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let model = scripted_model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(
        None,
        vec![(
            "build-1",
            "ipython",
            serde_json::json!({ "code": "from rlm import bash\nr = await bash(\"make\")\nprint(r.exit_code)" }),
        )],
    );
    provider.push_text_turn("built");
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd: repo.clone(),
        agent_dir: agent.clone(),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    engine
        .prompt("build it", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    let state = engine.session.agent().state().await;
    let cell = state
        .messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(text_of(&result.content)),
            _ => None,
        })
        .expect("the cell ran");
    assert!(cell.starts_with('0'), "{cell}");
    engine.dispose_kernel().await;

    pa_core::features::flush_installed(Duration::from_secs(60));
    let mark = read_recall_mark(&root(&repo), &agent).expect("the run end wrote a mark");
    let commands: Vec<&str> = mark
        .claims
        .iter()
        .map(|claim| claim.command.as_str())
        .collect();
    assert_eq!(commands, ["make"]);
}
