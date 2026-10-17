//! `/output-style`, `/add-dir` and Claude Code custom commands through a
//! whole session engine: the system prompt the build assembles, the
//! commands' result rows, the persisted state a resume reads back, and the
//! hidden rows the next turn carries.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;

use crate::session::manager::SessionManager;
use crate::session_engine::engine::{SessionEngine, SessionEngineConfig, create_session};
use crate::session_engine::session_commands::{
    SessionCommandExecution,
    SessionCommandParams,
    execute_session_command,
};
use crate::session_engine::slash_commands::{SlashCommandRegistry, parse_session_command};

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
        max_tokens_explicit: false,
    }
}

fn wire_model() -> pa_types::ai::Model {
    serde_json::from_value(serde_json::json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    cwd: std::path::PathBuf,
    agent_dir: std::path::PathBuf,
    sessions: std::path::PathBuf,
}

fn fixture(settings: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
    Fixture {
        sessions: dir.path().join("sessions"),
        cwd,
        agent_dir,
        _dir: dir,
    }
}

async fn engine(
    fixture: &Fixture,
    session: SessionManager,
    add_dirs: Vec<std::path::PathBuf>,
) -> SessionEngine {
    let provider = Arc::new(ScriptedProvider::new(model()));
    create_session(SessionEngineConfig {
        cwd: fixture.cwd.clone(),
        agent_dir: fixture.agent_dir.clone(),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(session),
        rlm_depth: Some(0),
        additional_directories: add_dirs,
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn run(engine: &SessionEngine, text: &str) -> SessionCommandExecution {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text).unwrap();
    let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
    let model = wire_model();
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: engine.feature_context.agent_dir.join("harness"),
        autonomous: &mut autonomous,
    };
    execute_session_command(engine, &mut params, &command).await
}

fn result_text(execution: &SessionCommandExecution) -> String {
    execution.messages.last().unwrap().content.text()
}

async fn next_turn_texts(engine: &SessionEngine) -> Vec<String> {
    engine
        .session
        .take_next_turn_rows()
        .await
        .iter()
        .map(|message| format!("{message:?}"))
        .collect()
}

#[tokio::test]
async fn the_configured_style_shapes_the_prompt_and_the_command_switches_it() {
    let fixture = fixture(r#"{"outputStyle": "Explanatory"}"#);
    let engine = engine(
        &fixture,
        SessionManager::persisted(&fixture.cwd, &fixture.sessions),
        Vec::new(),
    )
    .await;
    assert!(
        engine
            .system_prompt
            .contains("# Output Style: Explanatory\n")
    );
    // Built-ins keep the coding guidelines.
    assert!(
        engine
            .system_prompt
            .contains(crate::prompts::layers::OPINIONATED_LAYER.trim())
    );

    let listing = run(&engine, "/output-style").await;
    assert_eq!(listing.error, None);
    assert!(
        result_text(&listing)
            .starts_with("Output style: Explanatory\n\nAvailable styles:\n- default\n- Proactive:")
    );
    assert!(result_text(&listing).contains("- Explanatory (current): Claude explains"));

    let same = run(&engine, "/output-style explanatory").await;
    assert_eq!(result_text(&same), "Output style is already Explanatory");
    let unknown = run(&engine, "/output-style nope").await;
    assert_eq!(
        result_text(&unknown),
        "Unknown output style \"nope\". Available styles: default, Proactive, Concise, Explanatory, Learning"
    );

    let switched = run(&engine, "/output-style learning").await;
    assert_eq!(result_text(&switched), "Output style set to Learning");
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.agent_dir.join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["outputStyle"], "Learning");
    let reset = run(&engine, "/output-style default").await;
    assert_eq!(result_text(&reset), "Output style set to default");
    // Only the latest switch rides the next turn.
    let rows = next_turn_texts(&engine).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].contains("The output style was reset to the default."));
}

#[tokio::test]
async fn a_custom_style_without_keep_coding_instructions_drops_the_guidelines() {
    let fixture = fixture(r#"{"outputStyle": "pirate"}"#);
    let styles = fixture.agent_dir.join("output-styles");
    std::fs::create_dir_all(&styles).unwrap();
    std::fs::write(
        styles.join("pirate.md"),
        "---\ndescription: Arr\n---\nAnswer as a pirate.",
    )
    .unwrap();
    let engine = engine(
        &fixture,
        SessionManager::in_memory(&fixture.cwd),
        Vec::new(),
    )
    .await;
    assert!(
        engine
            .system_prompt
            .contains("# Output Style: pirate\nAnswer as a pirate.")
    );
    assert!(
        !engine
            .system_prompt
            .contains(crate::prompts::layers::OPINIONATED_LAYER.trim())
    );
}

#[tokio::test]
async fn add_dir_validates_lists_persists_and_survives_resume() {
    let fixture = fixture("{}");
    let root = fixture.cwd.parent().unwrap().to_path_buf();
    let (launch, added) = (root.join("launch"), root.join("added"));
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(added.join(".claude").join("commands")).unwrap();
    std::fs::write(
        added.join(".claude").join("commands").join("shared.md"),
        "Shared $0",
    )
    .unwrap();
    let session = SessionManager::persisted(&fixture.cwd, &fixture.sessions);
    let engine = engine(&fixture, session, vec![launch.clone()]).await;
    assert!(engine.system_prompt.contains(&format!(
        "Additional working directories:\n - {}",
        launch.display()
    )));

    let refused = run(&engine, "/add-dir .").await;
    assert_eq!(
        refused.error.as_deref(),
        Some(". is already the current working directory.")
    );
    let ok = run(&engine, "/add-dir ../added").await;
    assert_eq!(ok.error, None);
    assert_eq!(
        result_text(&ok),
        format!("Added {} as a working directory.", added.display())
    );
    assert_eq!(
        engine.additional_directories.list(),
        vec![launch.clone(), added.clone()]
    );
    let rows = next_turn_texts(&engine).await;
    assert!(
        rows[0].contains("Additional working directories added:"),
        "{rows:?}"
    );

    // A resume restores the session's own addition (and loads its commands).
    let file = engine
        .session
        .session_handle()
        .lock()
        .await
        .get_session_file()
        .unwrap()
        .to_path_buf();
    drop(engine);
    let resumed = self::engine(
        &fixture,
        SessionManager::open(&fixture.cwd, &fixture.sessions, &file),
        Vec::new(),
    )
    .await;
    assert_eq!(resumed.additional_directories.list(), vec![added.clone()]);
    assert!(resumed.prompt_templates.iter().any(|t| t.name == "shared"));
    assert_eq!(
        crate::skills::expand_prompt_template("/shared it", &resumed.prompt_templates),
        "Shared it"
    );
}
