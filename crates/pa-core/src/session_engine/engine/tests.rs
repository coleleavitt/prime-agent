//! The engine unit battery: the scripted tool loop, the child-depth
//! stamp, the MCP-gating unlock, and the goal/heartbeat handler
//! registration.
use super::*;
use crate::session_engine::tool_bridge::{bridge_tool, ToolDefinitionBridge};
use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};
use pa_agent::scripted::ScriptedProvider;

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
        execute: Arc::new(|_id, params, _signal, _on_update| {
            let text = params
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            Box::pin(async move { Ok(ToolExecutionResult::text(format!("echo: {text}"))) })
        }),
    }
}

#[tokio::test]
async fn engine_runs_tool_loop_and_persists() {
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
        Some("checking"),
        vec![("call-1", "echo", serde_json::json!({ "text": "hi" }))],
    );
    provider.push_text_turn("all done");

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
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
        agent_dir: tmp.path().join("agent"),
        mcp_manager: None,
        model: Some(model),
        thinking_level: None,
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(echo_definition())],
        custom_system_prompt: None,
        prompt_guidelines: vec![],
        generic_mcp_servers: vec![],
        allow_recursion: None,
        session_manager: None,
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: vec![],
        additional_prompt_paths: vec![],
        resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
        extra_builtin_skill_overrides: vec![],
        rlm_subagent_host: None,
        rlm_depth: None,
        telemetry: None,
        model_info: None,
        on_background_work_settled: None,
        prewarm_ipython_kernel: None,
        queued_goal_context_purge: None,
        rlm_token_allowance: None,
    })
    .await
    .unwrap();

    // The system prompt is the layered assembly: static core layer
    // first, dynamic tail after.
    assert!(engine.system_prompt.starts_with("# prime-agent harness"));
    assert!(engine
        .system_prompt
        .contains("Recursive agent depth: 0 (root)"));

    let outcome = engine
        .prompt("run the echo tool", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(outcome, PromptOutcome::Prompt);
    engine.session.agent().wait_for_idle().await;

    let state = engine.session.agent().state().await;
    assert!(state.messages.iter().any(|message| match message {
        pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::ToolResult(result)) => {
            result.tool_name == "echo"
        }
        _ => false,
    }));
    assert!(state.messages.iter().any(|message| match message {
        pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(assistant)) => {
            assistant.content.iter().any(|block| {
                matches!(
                    block,
                    pa_agent::types::AssistantContent::Text(text) if text.text == "all done"
                )
            })
        }
        _ => false,
    }));
    let entries = engine.session.entries().await;
    assert!(entries.iter().any(|entry| matches!(
        entry,
        pa_types::session::FileEntry::Message {
            message: pa_types::session::AgentMessage::User(user),
            ..
        } if user.content.text() == "run the echo tool"
    )));
    let _ = ToolDefinitionBridge::new;
}

/// A spawned child's prompt stamps its recursion depth: `create_session`
/// at depth N reads "depth: N (not root)", never the root identity.
#[tokio::test]
async fn spawned_child_prompt_stamps_its_depth() {
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
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
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
        agent_dir: tmp.path().join("agent"),
        mcp_manager: None,
        model: Some(model),
        thinking_level: None,
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(echo_definition())],
        custom_system_prompt: None,
        prompt_guidelines: vec![],
        generic_mcp_servers: vec![],
        allow_recursion: None,
        session_manager: None,
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: vec![],
        additional_prompt_paths: vec![],
        resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
        extra_builtin_skill_overrides: vec![],
        rlm_subagent_host: None,
        rlm_depth: Some(2),
        telemetry: None,
        model_info: None,
        on_background_work_settled: None,
        prewarm_ipython_kernel: None,
        queued_goal_context_purge: None,
        rlm_token_allowance: None,
    })
    .await
    .unwrap();

    assert!(engine
        .system_prompt
        .contains("Recursive agent depth: 2 (not root)"));
    assert!(!engine.system_prompt.contains("depth: 0 (root)"));
}

/// The login chain's prompt-gating end to end: a settings-declared OAuth
/// server stays gated, an endpoint-bound credential unlocks it in the NEXT
/// session, and one bound to another endpoint does not.
#[tokio::test]
async fn oauth_creds_unlock_generic_mcp_gating_in_new_sessions() {
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
        }
    }

    fn config(
        cwd: &std::path::Path,
        agent_dir: &std::path::Path,
        stream_fn: pa_agent::stream::StreamFn,
    ) -> SessionEngineConfig {
        SessionEngineConfig {
            plan_mode: None,
            on_late_sent_agent_message: None,
            semantic_edges: None,
            cron_store: None,
            queued_steering_probe: None,
            image_model_router: None,
            steering_mode: None,
            follow_up_mode: None,
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            mcp_manager: None,
            model: Some(model()),
            thinking_level: None,
            stream_fn: Some(stream_fn),
            tools: vec![],
            custom_system_prompt: None,
            prompt_guidelines: vec![],
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: None,
            extra_host_handlers: None,
            conversation_log_path: None,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
            extra_builtin_skill_overrides: vec![],
            rlm_subagent_host: None,
            rlm_depth: None,
            telemetry: None,
            model_info: None,
            on_background_work_settled: None,
            prewarm_ipython_kernel: None,
            queued_goal_context_purge: None,
            rlm_token_allowance: None,
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    // The settings declaration the daemon worker's MCP manager also
    // resolves (an OAuth HTTP server, like `mcp add ... --oauth`).
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({
            "mcpServers": {
                "fixture-oauth": {
                    "type": "http",
                    "url": "https://fixture.example/mcp",
                    "oauth": true,
                },
            },
        })
        .to_string(),
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));

    let engine = create_session(config(&cwd, &agent_dir, provider.stream_fn()))
        .await
        .unwrap();
    assert!(!engine.system_prompt.contains("# Generic MCP Connections"));

    // The persisted credential begin_login leaves behind (the TS
    // McpCredentials shape, endpoint-bound).
    let write_credential = |endpoint: &str| {
        std::fs::write(
            agent_dir.join("auth.json"),
            serde_json::json!({
                "mcp:fixture-oauth": {
                    "type": "oauth",
                    "access": "fixture-access",
                    "refresh": "fixture-refresh",
                    "expires": 999_999_999_999_999_i64,
                    "endpoint": endpoint,
                    "tokenEndpoint": "https://fixture.example/token",
                    "clientId": "fixture-client",
                },
            })
            .to_string(),
        )
        .unwrap();
    };

    // A credential bound to another endpoint stays gated: the token
    // must prove where it belongs.
    write_credential("https://other.example/mcp");
    let engine = create_session(config(&cwd, &agent_dir, provider.stream_fn()))
        .await
        .unwrap();
    assert!(!engine.system_prompt.contains("# Generic MCP Connections"));

    write_credential("https://fixture.example/mcp");
    let engine = create_session(config(&cwd, &agent_dir, provider.stream_fn()))
        .await
        .unwrap();
    assert!(engine.system_prompt.contains("# Generic MCP Connections"));
    assert!(engine.system_prompt.contains("`fixture-oauth`"));
}

#[tokio::test]
async fn create_session_registers_goal_and_heartbeat_handlers() {
    let dir = tempfile::TempDir::new().unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "ok",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let model = registration.get_model();
    let agent_model = crate::session_engine::provider_adapter::json_round_trip(&model).unwrap();
    let stream_fn = crate::session_engine::provider_adapter::real_stream_fn(None, model.clone());
    let engine = create_session(SessionEngineConfig {
        cron_store: None,
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().to_path_buf(),
        model: Some(agent_model),
        stream_fn: Some(stream_fn),
        tools: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    let names: Vec<String> = engine
        .session
        .agent()
        .state()
        .await
        .tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert!(
        names.iter().any(|name| name == "ipython"),
        "tools: {names:?}"
    );
}

/// Upstream #969: with `lengthContinuations` set, a reply cut off at the
/// output-token limit continues in a visible follow-up turn; unset (the TS
/// v0.9.8 default) the truncated reply ends the turn.
#[tokio::test]
async fn the_length_continuations_setting_auto_continues_a_truncated_reply() {
    async fn run(settings: Option<&str>) -> Vec<String> {
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
        let mut steps = pa_agent::scripted::text_turn_steps(&model, "the first half");
        if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
            if let pa_agent::stream::AssistantMessageEvent::Done { reason, message } = &mut **event
            {
                *reason = pa_agent::types::StopReason::Length;
                message.stop_reason = pa_agent::types::StopReason::Length;
            }
        }
        provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
        provider.push_text_turn("the second half");
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        if let Some(settings) = settings {
            std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
        }
        let engine = create_session(SessionEngineConfig {
            cwd: tmp.path().to_path_buf(),
            agent_dir,
            model: Some(model),
            stream_fn: Some(provider.stream_fn()),
            tools: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
        engine
            .session
            .prompt("write it", crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        engine.session.agent().wait_for_idle().await;
        engine
            .session
            .agent()
            .state()
            .await
            .messages
            .iter()
            .map(|message| match message {
                pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::User(user)) => {
                    format!(
                        "user: {}",
                        match &user.content {
                            pa_agent::types::UserContent::Text(text) => text.clone(),
                            pa_agent::types::UserContent::Parts(parts) => parts
                                .iter()
                                .filter_map(|part| match part {
                                    pa_agent::types::UserPart::Text(text) =>
                                        Some(text.text.clone()),
                                    pa_agent::types::UserPart::Image(_) => None,
                                })
                                .collect(),
                        }
                    )
                }
                pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                    assistant,
                )) => format!(
                    "assistant: {}",
                    assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            pa_agent::types::AssistantContent::Text(text) =>
                                Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<String>()
                ),
                _ => "other".to_string(),
            })
            // The harness digest row leads every transcript.
            .filter(|row| row != "other")
            .collect()
    }
    let on = run(Some(r#"{"lengthContinuations": 2}"#)).await;
    assert_eq!(
        on,
        vec![
            "user: write it".to_string(),
            "assistant: the first half".to_string(),
            "user: [auto-continue 1/2: the previous reply was cut off at the output-token limit]\n\nContinue exactly where the previous reply stopped. Do not repeat what was already written.".to_string(),
            "assistant: the second half".to_string(),
        ]
    );
    assert_eq!(
        run(None).await,
        vec![
            "user: write it".to_string(),
            "assistant: the first half".to_string(),
        ]
    );
}

/// Upstream #1798: `repetitionGuard` picks the guarded channels: `"all"`
/// stops a looping reply, the reasoning-only default and `false` let reply
/// text stream to its end.
#[tokio::test]
async fn the_repetition_guard_setting_picks_the_guarded_channels() {
    async fn reply(settings: Option<&str>) -> (pa_agent::types::StopReason, Option<String>) {
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
        provider.push_text_turn(&"the ".repeat(5_000));
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        if let Some(settings) = settings {
            std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
        }
        let engine = create_session(SessionEngineConfig {
            cwd: tmp.path().to_path_buf(),
            agent_dir,
            model: Some(model),
            stream_fn: Some(provider.stream_fn()),
            tools: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
        engine
            .session
            .prompt("go", crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        engine.session.agent().wait_for_idle().await;
        let messages = engine.session.agent().state().await.messages;
        match messages.last() {
            Some(pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            ))) => (assistant.stop_reason, assistant.stop_reason_raw.clone()),
            other => panic!("the turn ends on the reply: {other:?}"),
        }
    }
    // `"all"` guards reply text: the loop settles as a guarded error.
    assert_eq!(
        reply(Some(r#"{"repetitionGuard": "all"}"#)).await,
        (
            pa_agent::types::StopReason::Error,
            Some(pa_agent::repetition_guard::REPETITION_STOP_REASON.to_string())
        )
    );
    // The default guards reasoning only, and `false` turns it off: a text
    // loop (which a user may have asked for) streams to its end.
    assert_eq!(reply(None).await, (pa_agent::types::StopReason::Stop, None));
    assert_eq!(
        reply(Some(r#"{"repetitionGuard": false}"#)).await,
        (pa_agent::types::StopReason::Stop, None)
    );
}

/// Upstream #1192: a subagent funded by a grant stops at the first turn
/// boundary after its own spend reaches the grant (the crossing turn is
/// kept); a root takes its pool from the global `rlmTokenBudget` setting
/// and is never stopped by it.
#[tokio::test]
async fn a_funded_subagent_stops_once_its_grant_is_spent() {
    async fn run(
        allowance: Option<u64>,
        depth: u32,
        settings: Option<&str>,
    ) -> (usize, Option<u64>) {
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
        // Two tool-call turns of 80 tokens each, then a natural stop.
        for call in ["call-1", "call-2"] {
            let mut steps = pa_agent::scripted::tool_call_turn_steps(
                &model,
                None,
                vec![(call, "echo", serde_json::json!({ "text": "hi" }))],
            );
            if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
                if let pa_agent::stream::AssistantMessageEvent::Done { message, .. } = &mut **event
                {
                    message.usage.input = 60;
                    message.usage.output = 20;
                }
            }
            provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
        }
        provider.push_text_turn("all done");
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        if let Some(settings) = settings {
            std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
        }
        let engine = create_session(SessionEngineConfig {
            cwd: tmp.path().to_path_buf(),
            agent_dir,
            model: Some(model),
            stream_fn: Some(provider.stream_fn()),
            tools: vec![bridge_tool(echo_definition())],
            rlm_depth: Some(depth),
            rlm_token_allowance: allowance,
            ..Default::default()
        })
        .await
        .unwrap();
        engine
            .session
            .prompt("go", crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        engine.session.agent().wait_for_idle().await;
        (
            provider.calls().len(),
            engine
                .rlm
                .token_budget
                .get()
                .map(|budget| budget.remaining()),
        )
    }
    // A 100-token grant: the second 80-token turn crosses it and the run
    // stops there; the third request never goes out.
    assert_eq!(run(Some(100), 1, None).await, (2, Some(0)));
    // An unfunded subagent and an unbudgeted root run to the natural stop.
    assert_eq!(run(None, 1, None).await, (3, None));
    assert_eq!(run(None, 0, None).await, (3, None));
    // A budgeted root keeps its whole pool for delegation and is never
    // stopped by its own spend.
    assert_eq!(
        run(None, 0, Some(r#"{"rlmTokenBudget": 500}"#)).await,
        (3, Some(500))
    );
}

/// The delegation budget's spend and grant totals are durable: a resumed
/// child keeps counting against the grant it was spawned with (even when
/// the resume carries no allowance, as after a daemon restart), and a
/// restarted root does not refill its pool.
#[tokio::test]
async fn the_delegation_budget_survives_a_resume() {
    async fn run(engine: &SessionEngine) {
        engine
            .session
            .prompt("go", crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        engine.session.agent().wait_for_idle().await;
    }
    async fn session_file(engine: &SessionEngine) -> std::path::PathBuf {
        engine
            .session
            .session_handle()
            .lock()
            .await
            .get_session_file()
            .unwrap()
            .to_path_buf()
    }

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
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    let session_dir = tmp.path().join("sessions").join("w");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"rlmTokenBudget": {"total": 500, "perDepth": [200]}}"#,
    )
    .unwrap();
    // One session lifetime: `turns` 80-token tool-call turns then a stop;
    // answers the provider calls made and the budget engine.
    let open =
        |file: Option<std::path::PathBuf>, allowance: Option<u64>, depth: u32, turns: usize| {
            let model = model.clone();
            let cwd = tmp.path().to_path_buf();
            let agent_dir = agent_dir.clone();
            let session_dir = session_dir.clone();
            async move {
                let provider = Arc::new(ScriptedProvider::new(model.clone()));
                for index in 0..turns {
                    let call = format!("call-{index}");
                    let mut steps = pa_agent::scripted::tool_call_turn_steps(
                        &model,
                        None,
                        vec![(call.as_str(), "echo", serde_json::json!({ "text": "hi" }))],
                    );
                    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
                        if let pa_agent::stream::AssistantMessageEvent::Done { message, .. } =
                            &mut **event
                        {
                            message.usage.input = 60;
                            message.usage.output = 20;
                        }
                    }
                    provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
                }
                provider.push_text_turn("all done");
                let session_manager = match &file {
                    Some(file) => SessionManager::open(&cwd, &session_dir, file),
                    None => SessionManager::persisted(&cwd, &session_dir),
                };
                let engine = create_session(SessionEngineConfig {
                    cwd,
                    agent_dir,
                    model: Some(model),
                    stream_fn: Some(provider.stream_fn()),
                    tools: vec![bridge_tool(echo_definition())],
                    rlm_depth: Some(depth),
                    rlm_token_allowance: allowance,
                    session_manager: Some(session_manager),
                    ..Default::default()
                })
                .await
                .unwrap();
                (provider, engine)
            }
        };
    // A child funded with 200 spends 80 and stops after the next turn.
    let (_, child) = open(None, Some(200), 1, 1).await;
    run(&child).await;
    let child_file = session_file(&child).await;
    drop(child);
    // Resumed without the allowance (a daemon restart): it still holds
    // its 200 grant with 80 already spent.
    let (provider, child) = open(Some(child_file), None, 1, 2).await;
    assert_eq!(
        child
            .rlm
            .token_budget
            .get()
            .map(|budget| budget.remaining()),
        Some(120)
    );
    run(&child).await;
    assert_eq!(
        provider.calls().len(),
        2,
        "the second resumed turn crosses the remaining 120"
    );
    drop(child);

    // A root's grants are never returned, across a restart too.
    let (_, root) = open(None, None, 0, 0).await;
    run(&root).await;
    let budget = root.rlm.token_budget.get().unwrap();
    assert_eq!(budget.reserve_child_grant(None).unwrap(), 200);
    let root_file = session_file(&root).await;
    drop(root);
    let (_, root) = open(Some(root_file), None, 0, 0).await;
    assert_eq!(
        root.rlm.token_budget.get().map(|budget| budget.remaining()),
        Some(300)
    );
}

/// A telemetry wiring over a mock sink (flushes per event).
fn mock_telemetry() -> (
    super::super::telemetry::TelemetryWiring,
    std::sync::Arc<pa_telemetry::MockSink>,
) {
    let mock = std::sync::Arc::new(pa_telemetry::MockSink::new());
    let mut config = pa_telemetry::TelemetryClientConfig::new("install-1");
    config.batch_size = 1;
    config.flush_interval = std::time::Duration::from_mins(10);
    config.sinks = vec![mock.clone() as Arc<dyn pa_telemetry::TelemetrySink>];
    (
        super::super::telemetry::TelemetryWiring {
            client: pa_telemetry::TelemetryClient::spawn(config).expect("spawn client"),
            execution_mode: None,
            now: None,
            telemetry_enabled: None,
        },
        mock,
    )
}

/// One event's properties as JSON, after a flush.
async fn tracked(
    wiring: &super::super::telemetry::TelemetryWiring,
    mock: &pa_telemetry::MockSink,
    name: &str,
) -> Vec<serde_json::Value> {
    wiring.client.flush().await.unwrap();
    mock.events()
        .iter()
        .filter(|event| event.name == name)
        .map(|event| serde_json::to_value(&event.properties).unwrap())
        .collect()
}

/// The recently added settings ride `agent started` as categories and
/// counts: a session reports how it was configured, never a model id, a
/// budget figure, or a path.
#[tokio::test]
async fn agent_started_reports_the_settings_adoption() {
    async fn started(settings: Option<&str>) -> serde_json::Value {
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
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        if let Some(settings) = settings {
            std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
        }
        let (wiring, mock) = mock_telemetry();
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        let _engine = create_session(SessionEngineConfig {
            cwd: tmp.path().to_path_buf(),
            agent_dir,
            model: Some(model),
            stream_fn: Some(provider.stream_fn()),
            rlm_depth: Some(0),
            telemetry: Some(super::super::telemetry::TelemetryWiring {
                client: wiring.client.clone(),
                execution_mode: None,
                now: None,
                telemetry_enabled: None,
            }),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut events = tracked(&wiring, &mock, "agent started").await;
        assert_eq!(events.len(), 1);
        let mut properties = events.remove(0);
        // Only the settings adoption keys (the rest is pinned elsewhere).
        let object = properties.as_object_mut().unwrap();
        object.retain(|key, _| {
            [
                "length_continuations",
                "repetition_guard",
                "rlm_token_budget",
                "fallback_model_count",
                "context_cap_source",
                "kernel_environment",
            ]
            .contains(&key.as_str())
        });
        properties
    }
    assert_eq!(
        started(Some(
            r#"{
                "lengthContinuations": 3,
                "repetitionGuard": "all",
                "rlmTokenBudget": { "total": 1000, "perDepth": [100] },
                "fallbackModels": ["openai/gpt-x", "anthropic/claude-y", "openai/gpt-x"],
                "compaction": { "maxContextTokens": 50000 },
                "kernel": { "environment": "scrub-credentials" }
            }"#
        ))
        .await,
        serde_json::json!({
            "length_continuations": 3,
            "repetition_guard": "all",
            "rlm_token_budget": "per_depth",
            "fallback_model_count": 2,
            "context_cap_source": "global",
            "kernel_environment": "scrub_credentials",
        })
    );
    assert_eq!(
        started(None).await,
        serde_json::json!({
            "length_continuations": 0,
            "repetition_guard": "reasoning",
            "rlm_token_budget": "off",
            "fallback_model_count": 0,
            "context_cap_source": "none",
            "kernel_environment": "inherit",
        })
    );
}

/// A length auto-continue counts once on `agent session ended`.
#[tokio::test]
async fn a_length_auto_continue_counts_on_session_end() {
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
    let mut steps = pa_agent::scripted::text_turn_steps(&model, "the first half");
    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
        if let pa_agent::stream::AssistantMessageEvent::Done { reason, message } = &mut **event {
            *reason = pa_agent::types::StopReason::Length;
            message.stop_reason = pa_agent::types::StopReason::Length;
        }
    }
    provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
    provider.push_text_turn("the second half");
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"lengthContinuations": 2}"#,
    )
    .unwrap();
    let (wiring, mock) = mock_telemetry();
    let engine = create_session(SessionEngineConfig {
        cwd: tmp.path().to_path_buf(),
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        rlm_depth: Some(0),
        telemetry: Some(super::super::telemetry::TelemetryWiring {
            client: wiring.client.clone(),
            execution_mode: None,
            now: None,
            telemetry_enabled: None,
        }),
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("go", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    engine.telemetry.as_ref().unwrap().end().await.unwrap();
    let ended = tracked(&wiring, &mock, "agent session ended").await;
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["length_continuation_count"], serde_json::json!(1));
}
