use super::*;

#[test]
fn kernel_requests_present_as_claude_code_tools() {
    let request = |request_type: &str, data: Value| HostRequestObservation {
        request_type: request_type.to_string(),
        data,
    };
    assert_eq!(
        host_request_as_tool(&request(
            "bash.run",
            json!({ "type": "bash.run", "command": "ls -la", "env": { "SECRET": "x" } })
        )),
        Some(("Bash".to_string(), json!({ "command": "ls -la" })))
    );
    assert_eq!(
        host_request_as_tool(&request(
            "mcp.session.call_tool",
            json!({ "server": "github", "tool": "search", "arguments": { "q": "x" } })
        )),
        Some(("mcp__github__search".to_string(), json!({ "q": "x" })))
    );
    assert_eq!(
        host_request_as_tool(&request("bash.follow", json!({ "id": "j1" }))),
        None
    );
}

/// The feature over POSIX shell hooks, driven through its seams directly.
#[cfg(unix)]
mod seams {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use pa_agent::types::CustomAgentMessage;
    use pa_core::features::{
        CompactionTrigger,
        FeatureNoticeSink,
        ToolBatchCall,
        register_feature_notice_sink,
    };

    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        context: Arc<SessionFeatureContext>,
    }

    fn model() -> pa_agent::types::Model {
        serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    /// A session whose agent dir configures `hooks` (`$ROOT` in a command
    /// is the fixture's directory).
    fn fixture(session_id: &str, hooks: &Value) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let agent_dir = root.join("agent");
        let cwd = root.join("project");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let hooks = hooks
            .to_string()
            .replace("$ROOT", &root.display().to_string());
        std::fs::write(
            agent_dir.join("settings.json"),
            format!(r#"{{ "hooks": {hooks} }}"#),
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            root,
            context: Arc::new(SessionFeatureContext {
                agent_dir,
                cwd,
                session_id: session_id.to_string(),
                python_skill_import_names: Vec::new(),
                model: model(),
                telemetry: None,
                rlm_depth: 0,
                session_artifact_dir: None,
            }),
        }
    }

    fn command(text: &str) -> Value {
        json!([{ "type": "command", "command": text }])
    }

    /// The JSON a hook wrote to `path`, once it is there.
    async fn written(path: &Path) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(value) = serde_json::from_str(&text) {
                    return value;
                }
            }
            assert!(
                Instant::now() < deadline,
                "no hook wrote {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn subagents_start_and_stop_once_with_claude_codes_fields() {
        let fixture = fixture(
            "hooks-subagents",
            &json!({
                "SubagentStart": [{ "matcher": "review.*", "hooks": command("cat > $ROOT/start.json") }],
                "SubagentStop": [{ "hooks": command("cat >> $ROOT/stop.jsonl") }],
            }),
        );
        let feature = HooksFeature::new();
        let request = |request_type: &str| HostRequestObservation {
            request_type: request_type.to_string(),
            data: json!({ "type": request_type }),
        };
        let spawned = json!({ "rlm_child_id": "sub-1", "name": "reviewer", "session_dir": "/s/sub-1", "model": "m" });
        assert_eq!(
            feature
                .after_host_request(&fixture.context, &request("rlm.run"), Ok(&spawned))
                .await,
            None
        );
        let started = written(&fixture.root.join("start.json")).await;
        assert_eq!(
            (
                &started["hook_event_name"],
                &started["agent_id"],
                &started["agent_type"]
            ),
            (&json!("SubagentStart"), &json!("sub-1"), &json!("reviewer"))
        );
        let collected = json!({ "results": [{ "rlm_child_id": "sub-1", "status": "done", "settled": true, "answer_text": "LGTM" }] });
        for _ in 0..2 {
            feature
                .after_host_request(&fixture.context, &request("rlm.collect"), Ok(&collected))
                .await;
        }
        let stopped = written(&fixture.root.join("stop.jsonl")).await;
        assert_eq!(
            (
                &stopped["hook_event_name"],
                &stopped["agent_id"],
                &stopped["agent_type"],
                &stopped["agent_session_dir"],
                &stopped["last_assistant_message"],
                &stopped["stop_hook_active"],
            ),
            (
                &json!("SubagentStop"),
                &json!("sub-1"),
                &json!("reviewer"),
                &json!("/s/sub-1"),
                &json!("LGTM"),
                &json!(false),
            )
        );
        // The second settled reply fired nothing more.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let lines = std::fs::read_to_string(fixture.root.join("stop.jsonl")).unwrap();
        assert_eq!(lines.matches("SubagentStop").count(), 1, "{lines}");
    }

    #[tokio::test]
    async fn a_background_completion_notice_fires_notification() {
        let fixture = fixture(
            "hooks-notification",
            &json!({
                "Notification": [{ "matcher": "background_task_completed", "hooks": command("cat > $ROOT/note.json") }],
            }),
        );
        let feature = HooksFeature::new();
        let row = |custom_type: &str, content: &str| {
            AgentMessage::Custom(CustomAgentMessage {
                role: "custom".to_string(),
                payload: json!({ "customType": custom_type, "content": content, "display": true }),
            })
        };
        // Rows that are not completion notices fire nothing.
        feature.on_message_end(&fixture.context, &row("feature_context", "ctx"));
        feature.on_message_end(
            &fixture.context,
            &row(
                "async_bash_completion",
                "[bash-done pid:7 exit:0]\n\nCommand: \"make\"",
            ),
        );
        let note = written(&fixture.root.join("note.json")).await;
        assert_eq!(
            (
                &note["hook_event_name"],
                &note["notification_type"],
                &note["title"],
                &note["message"]
            ),
            (
                &json!("Notification"),
                &json!("background_task_completed"),
                &json!("Background command finished"),
                &json!("[bash-done pid:7 exit:0]\n\nCommand: \"make\""),
            )
        );
    }

    #[tokio::test]
    async fn an_async_rewake_hook_delivers_its_stderr_as_a_notice() {
        let fixture = fixture(
            "hooks-rewake",
            &json!({
                "UserPromptSubmit": [{ "hooks": [
                    { "type": "command", "command": "cat >/dev/null; echo 'lint found 2 errors' >&2; exit 2", "asyncRewake": true },
                    { "type": "command", "command": "cat >/dev/null; sleep 0.2; printf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"async-ctx\"}}'", "async": true },
                ] }],
            }),
        );
        let seen: Arc<Mutex<Vec<pa_types::session::CustomMessage>>> = Arc::default();
        let sink: FeatureNoticeSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |row| seen.lock().unwrap().push(row))
        };
        register_feature_notice_sink(&fixture.context.session_id, &sink);
        let feature = HooksFeature::new();
        // The background hooks never block the prompt or refuse it (an
        // async exit code 2 is not a refusal).
        assert!(matches!(
            feature.prompt_context(&fixture.context, "go").await,
            PromptDecision::Continue { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        let notice = loop {
            if let Some(row) = seen.lock().unwrap().first().cloned() {
                break row;
            }
            assert!(Instant::now() < deadline, "the rewake notice never came");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            notice.content,
            pa_types::ai::UserContent::Text(
                "Async UserPromptSubmit hook reported:\n\nlint found 2 errors".to_string()
            )
        );
        assert_eq!(notice.details, Some(json!({ "feature": "hooks" })));
        // The async hook's context rides the next prompt.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let PromptDecision::Continue { context } =
            feature.prompt_context(&fixture.context, "again").await
        else {
            panic!("the prompt was refused");
        };
        assert!(
            context.as_deref().unwrap_or_default().contains("async-ctx"),
            "{context:?}"
        );
    }

    #[tokio::test]
    async fn stop_post_tool_batch_and_compaction_answer_the_session() {
        let fixture = fixture(
            "hooks-lifecycle",
            &json!({
                "Stop": [{ "hooks": command("cat > $ROOT/stop.json; echo 'run the tests' >&2; exit 2") }],
                "PostToolBatch": [{ "hooks": command("cat > $ROOT/batch.json; printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolBatch\",\"additionalContext\":\"batch ctx\"}}'") }],
                "PreCompact": [{ "matcher": "auto", "hooks": command("cat > $ROOT/pre.json; echo 'keep the TODO list'") }],
                "PostCompact": [{ "hooks": command("cat > $ROOT/post.json") }],
            }),
        );
        let feature = HooksFeature::new();
        // A running kernel command is a background task.
        feature
            .after_host_request(
                &fixture.context,
                &HostRequestObservation {
                    request_type: "bash.run".to_string(),
                    data: json!({ "command": "npm run dev" }),
                },
                Ok(&json!({ "status": "ok", "job": { "id": "j1", "pid": 9 }, "events": [] })),
            )
            .await;
        let stop = AgentStopObservation {
            last_assistant_message: "All done.".to_string(),
            continued: true,
        };
        assert_eq!(
            feature.before_agent_stop(&fixture.context, &stop).await,
            Some("Stop hook feedback:\nrun the tests".to_string())
        );
        let stopped = written(&fixture.root.join("stop.json")).await;
        assert_eq!(
            (
                &stopped["stop_hook_active"],
                &stopped["last_assistant_message"],
                &stopped["background_tasks"]
            ),
            (
                &json!(true),
                &json!("All done."),
                &json!([{ "id": "j1", "type": "bash", "status": "running", "description": "npm run dev", "command": "npm run dev" }]),
            )
        );
        let batch = ToolBatchObservation {
            tool_calls: vec![
                ToolBatchCall {
                    tool_call_id: "t1".to_string(),
                    tool_name: "python_repl".to_string(),
                },
                ToolBatchCall {
                    tool_call_id: "t2".to_string(),
                    tool_name: "read".to_string(),
                },
            ],
        };
        assert_eq!(
            feature.after_tool_batch(&fixture.context, &batch).await,
            Some("batch ctx".to_string())
        );
        assert_eq!(
            written(&fixture.root.join("batch.json")).await["tool_calls"],
            json!([
                { "tool_name": "python_repl", "tool_use_id": "t1" },
                { "tool_name": "read", "tool_use_id": "t2" },
            ])
        );
        let manual = CompactionObservation {
            trigger: CompactionTrigger::Manual,
            custom_instructions: None,
        };
        // The matcher selects the trigger.
        assert_eq!(
            feature.before_compaction(&fixture.context, &manual).await,
            None
        );
        let auto = CompactionObservation {
            trigger: CompactionTrigger::Auto,
            custom_instructions: Some("focus".to_string()),
        };
        assert_eq!(
            feature.before_compaction(&fixture.context, &auto).await,
            Some("keep the TODO list".to_string())
        );
        let pre = written(&fixture.root.join("pre.json")).await;
        assert_eq!(
            (&pre["trigger"], &pre["custom_instructions"]),
            (&json!("auto"), &json!("focus"))
        );
        feature.after_compaction(&fixture.context, &auto, "the summary");
        let post = written(&fixture.root.join("post.json")).await;
        assert_eq!(
            (
                &post["hook_event_name"],
                &post["trigger"],
                &post["compact_summary"]
            ),
            (&json!("PostCompact"), &json!("auto"), &json!("the summary"))
        );
    }

    #[test]
    fn session_end_runs_outside_a_runtime_and_flush_waits_for_it() {
        let fixture = fixture(
            "hooks-session-end",
            &json!({
                "SessionEnd": [{ "hooks": command("sleep 0.2; cat > $ROOT/end.json") }],
            }),
        );
        let feature = HooksFeature::new();
        feature.on_session_end(&fixture.context, "other");
        feature.flush(Instant::now() + Duration::from_secs(10));
        let ended: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.root.join("end.json")).unwrap())
                .unwrap();
        assert_eq!(
            (
                &ended["hook_event_name"],
                &ended["reason"],
                &ended["session_id"]
            ),
            (
                &json!("SessionEnd"),
                &json!("other"),
                &json!("hooks-session-end")
            )
        );
    }
}
