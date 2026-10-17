//! Coordinator mode: who gets it, what its gate refuses and lets through,
//! its prompt section, and the nested memory a worker hands back.

use serde_json::json;

use super::*;

fn config(extra: &[&str]) -> CoordinatorConfig {
    CoordinatorConfig {
        extra_tools: extra.iter().map(ToString::to_string).collect(),
        ..CoordinatorConfig::default()
    }
}

#[test]
fn only_a_root_session_without_an_agent_definition_coordinates() {
    let on = ExperimentalSettings {
        coordinator_mode: true,
        coordinator_force_worker_inherit_model: true,
        coordinator_extra_tools: vec!["Bash".to_string()],
        ..ExperimentalSettings::default()
    };
    assert_eq!(
        CoordinatorConfig::resolve(&on, 0, false),
        Some(CoordinatorConfig {
            extra_tools: vec!["Bash".to_string()],
            force_worker_inherit_model: true,
            ..CoordinatorConfig::default()
        })
    );
    assert_eq!(CoordinatorConfig::resolve(&on, 1, false), None);
    assert_eq!(CoordinatorConfig::resolve(&on, 0, true), None);
    assert_eq!(
        CoordinatorConfig::resolve(&ExperimentalSettings::default(), 0, false),
        None
    );
}

#[tokio::test]
async fn the_gate_refuses_worker_capabilities_and_passes_orchestration() {
    let gate = coordinator_gate(config(&[]), None);
    let refused = |request: &str| {
        let gate = Arc::clone(&gate);
        let request = request.to_string();
        async move {
            gate.review(&request, &json!({ "server": "github" }))
                .await
                .is_some()
        }
    };
    for request in [
        "bash.run",
        "file.edit",
        "web.search",
        "web.fetch",
        "mcp.session.call_tool",
        "mcp.integration.call_tool",
        "computer_use.app",
        "artifact.present",
        "worktree.enter",
        "system_router.run",
    ] {
        assert!(refused(request).await, "{request} is a worker's");
    }
    for request in [
        "rlm.run",
        "rlm.collect",
        "rlm.interrupt_subagent",
        "agent_message.send",
        "rlm.list_subagents",
        "factory.run",
        "vision.read",
        "bash.output",
        "mcp.session.list_tools",
        "user_message.send",
    ] {
        assert!(!refused(request).await, "{request} is orchestration");
    }
    assert_eq!(
        gate.review("bash.run", &Value::Null).await,
        Some(
            "Coordinator mode: this session coordinates workers and does not run shell commands \
             (bash()) itself. Delegate it to a worker with rlm.spawn (or continue one with \
             rlm.send_message)."
                .to_string()
        )
    );
}

#[test]
fn extra_tools_let_their_capability_through() {
    let github = json!({ "server": "github" });
    let slack = json!({ "server": "slack" });
    let extra = config(&["Bash", "mcp__slack", "web.*", "WebSearch"]);
    assert_eq!(coordinator_refusal(&extra, "bash.run", &Value::Null), None);
    assert_eq!(coordinator_refusal(&extra, "web.fetch", &Value::Null), None);
    assert_eq!(
        coordinator_refusal(&extra, "web.search", &Value::Null),
        None
    );
    assert_eq!(
        coordinator_refusal(&extra, "mcp.session.call_tool", &slack),
        None
    );
    assert!(coordinator_refusal(&extra, "mcp.session.call_tool", &github).is_some());
    assert!(coordinator_refusal(&extra, "file.edit", &Value::Null).is_some());
    let every_server = config(&["mcp__*__any_tool"]);
    assert_eq!(
        coordinator_refusal(&every_server, "mcp.integration.call_tool", &github),
        None
    );
    let raw = config(&["computer_use.*", "file.edit"]);
    assert_eq!(
        coordinator_refusal(&raw, "computer_use.get_state", &Value::Null),
        None
    );
    assert_eq!(coordinator_refusal(&raw, "file.edit", &Value::Null), None);
}

#[tokio::test]
async fn the_gate_defers_what_it_allows_to_the_inner_gate() {
    struct RefuseCollect;
    impl HostRequestPolicy for RefuseCollect {
        fn review(&self, request_type: &str, _data: &Value) -> GateFuture<Option<String>> {
            let verdict = (request_type == "rlm.collect").then(|| "inner".to_string());
            Box::pin(async move { verdict })
        }

        fn after(
            &self,
            _request_type: &str,
            _data: &Value,
            _outcome: Result<&Value, &str>,
        ) -> GateFuture<()> {
            Box::pin(async {})
        }
    }
    let gate = coordinator_gate(config(&[]), Some(Arc::new(RefuseCollect)));
    assert_eq!(
        gate.review("rlm.collect", &Value::Null).await,
        Some("inner".to_string())
    );
    assert!(gate.review("bash.run", &Value::Null).await.is_some());
    assert_eq!(gate.review("rlm.run", &Value::Null).await, None);
}

#[test]
fn the_section_is_claude_codes_coordinator_prompt_in_kernel_terms() {
    let plain = coordinator_section(&config(&[]), &CoordinatorPromptFacts::default());
    assert!(plain.starts_with("# Coordinator mode\n\nYou are an AI assistant that orchestrates"));
    for needle in [
        "You are a **coordinator**.",
        "- **`rlm.spawn(prompt, name=...)`** - Spawn a new worker",
        "- **`rlm.interrupt_subagent(target)`** - Stop a running worker",
        MODEL_INHERIT,
        "<task-type>local_agent</task-type>",
        "### Always synthesize",
        "### Executing user-approved actions",
    ] {
        assert!(plain.contains(needle), "missing {needle:?}");
    }
    assert!(!plain.contains("Scratchpad directory"));
    assert!(!plain.contains(SKILL_GUIDANCE));

    let forced = coordinator_section(
        &CoordinatorConfig {
            force_worker_inherit_model: true,
            skill_guidance: true,
            ..CoordinatorConfig::default()
        },
        &CoordinatorPromptFacts {
            scratchpad: Some("/tmp/scratch".to_string()),
            has_agent_types: true,
        },
    );
    assert!(forced.contains(MODEL_FORCED));
    assert!(!forced.contains(MODEL_INHERIT));
    assert!(forced.contains("Scratchpad directory: /tmp/scratch"));
    assert!(forced.contains(SKILL_GUIDANCE));
    assert!(forced.contains("The agent types listed in the session context"));
}

#[test]
fn a_workers_nested_instruction_files_are_read_from_its_session_dir() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        json!({ "type": "custom_message", "customType": "nested_instructions",
                "details": { "paths": ["/repo/pkg/CLAUDE.md", "/repo/pkg/rules/a.md"] } }),
        json!({ "type": "message", "message": { "role": "custom", "customType": "nested_instructions",
                "details": { "paths": ["/repo/web/AGENTS.md", "/repo/pkg/CLAUDE.md"] } } }),
        json!({ "type": "custom_message", "customType": "plan_mode_change",
                "details": { "paths": ["/repo/x/CLAUDE.md"] } }),
    ];
    let text = lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        dir.path().join("child.jsonl"),
        format!("{text}\nnot json\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("notes.txt"), "nested_instructions").unwrap();
    assert_eq!(
        worker_nested_files(dir.path()),
        vec![
            PathBuf::from("/repo/pkg/CLAUDE.md"),
            PathBuf::from("/repo/web/AGENTS.md")
        ]
    );
    let propagation = NestedMemoryPropagation::default();
    propagation.note_worker_session_dir(dir.path());
    propagation.note_worker_session_dir(dir.path());
    assert_eq!(propagation.drain().len(), 2, "each file once");
    assert!(propagation.drain().is_empty());
    assert!(worker_nested_files(&dir.path().join("missing")).is_empty());
}

#[test]
fn worker_rows_name_the_workers_and_count_once() {
    let row = |custom_type: &str, details: Value| {
        pa_agent::types::AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
            role: "custom".to_string(),
            payload: json!({ "customType": custom_type, "details": details, "timestamp": 7 }),
        })
    };
    let rows = vec![
        row(
            super::super::rlm_notices::RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE,
            json!({ "childId": "sub-1", "sessionName": "auth-research" }),
        ),
        row(
            super::super::agent_messaging::AGENT_MESSAGE_CUSTOM_TYPE,
            json!({ "from": { "sessionName": "tests", "sessionId": "s-2" } }),
        ),
        row("plan_mode_change", json!({ "childId": "ignored" })),
    ];
    assert_eq!(
        worker_refs(&rows),
        vec!["sub-1", "auth-research", "s-2", "tests"]
    );
    let propagation = NestedMemoryPropagation::default();
    assert!(propagation.first_sight(&rows[0]));
    assert!(!propagation.first_sight(&rows[0]));
    assert!(propagation.first_sight(&rows[1]));
}
