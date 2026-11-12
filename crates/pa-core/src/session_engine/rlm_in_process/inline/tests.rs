use super::*;

/// The print invocation's recipe over one directory; these tests never
/// admit a child, so the child engine seam refuses.
fn recipe(directory: &std::path::Path, faux: Option<Value>) -> InlineRecipe {
    InlineRecipe {
        cwd: directory.to_path_buf(),
        agent_dir: directory.join("agent"),
        faux,
        build_child: Arc::new(|_| {
            Box::pin(async { Err("this test builds no child engine".to_string()) })
        }),
    }
}

/// The scripted print root over its node: a faux provider answering the
/// script's responses, and the node's own engine wiring.
async fn faux_root_engine(
    node: &Arc<InlineSessionNode>,
    manager: crate::session::manager::SessionManager,
    script: &Value,
) -> InlineBuiltEngine {
    let steps = crate::session_engine::headless::faux_response_steps(
        script["responses"].as_array().expect("a responses array"),
    )
    .unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            provider: Some("faux".to_string()),
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux Model".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(steps);
    let model = registration.get_model();
    let wiring = inline_engine_wiring(node);
    let engine = crate::session_engine::engine::create_session(
        crate::session_engine::engine::SessionEngineConfig {
            cwd: node.recipe.cwd.clone(),
            agent_dir: node.recipe.agent_dir.clone(),
            model: Some(
                crate::session_engine::provider_adapter::json_round_trip(&model)
                    .expect("the faux model crosses the loop boundary"),
            ),
            stream_fn: Some(crate::session_engine::provider_adapter::real_stream_fn(
                None,
                model.clone(),
            )),
            session_manager: Some(manager),
            rlm_subagent_host: Some(wiring.rlm_subagent_host),
            extra_host_handlers: Some(wiring.extra_host_handlers),
            on_late_sent_agent_message: Some(wiring.on_late_sent_agent_message),
            bash_notice_wake: Some(Arc::clone(&wiring.bash_wake)),
            on_background_work_settled: Some(wiring.bash_wake),
            daemonless: true,
            model_info: Some(model.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    InlineBuiltEngine {
        engine,
        model,
        api_key: None,
    }
}

#[tokio::test]
async fn spawn_waiting_for_admission_rejects_a_disposed_parent() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("agent")).unwrap();
    let manager = crate::session::manager::SessionManager::in_memory(directory.path());
    let mut node = inline_root_node(
        &manager,
        recipe(
            directory.path(),
            Some(json!({
                "responses": ["parent"],
                "children": { "late": { "responses": ["child"] } },
            })),
        ),
    );
    Arc::get_mut(&mut node).unwrap().max_depth = 2;
    let gate = node.spawn_gate.lock().await;
    let mut spawn = InlineHost(Arc::clone(&node)).spawn(RlmSpawnRequest {
        child_id: None,
        prompt: "finish the task".to_string(),
        name: Some("late".to_string()),
        model: None,
        thinking: None,
        cwd: None,
        spawned_by_request_id: None,
        target: crate::session_engine::rlm_host::RlmSpawnTarget::Local,
        plan_mode: false,
        agent: None,
        worktree: None,
        factory: None,
    });
    std::future::poll_fn(|context| {
        assert!(spawn.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    node.disposed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    drop(gate);
    let result = spawn.await;
    let admitted = node.children.lock().await.len();
    dispose_inline_subagents(&node).await;
    let error = result.expect_err("a disposed parent must not admit a queued spawn");
    assert!(error.to_string().contains("disposed"), "{error:#}");
    assert_eq!(admitted, 0);
}

/// The fake python tool of the landing test: records one late receipt
/// through the print path (the factored async fn) while its cell's
/// result row has not landed.
struct LateReceiptPythonTool {
    node: Arc<InlineSessionNode>,
}

impl pa_agent::types::AgentTool for LateReceiptPythonTool {
    fn name(&self) -> &'static str {
        "python_repl"
    }

    fn description(&self) -> &'static str {
        "Runs a python cell."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({ "type": "object", "properties": {} }))
    }

    fn execute(
        self: Arc<Self>,
        tool_call_id: String,
        _params: serde_json::Value,
        _signal: pa_agent::abort::AbortSignal,
        _on_update: pa_agent::types::AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>> {
        let node = Arc::clone(&self.node);
        Box::pin(async move {
            record_late_sent_receipt(
                node,
                tool_call_id,
                crate::kernel::shared::KernelSentAgentMessage {
                    id: "agentmsg_1".to_string(),
                    message: "Ping.".to_string(),
                    delivery_status: crate::kernel::shared::SentDeliveryStatus::Delivered,
                    receiver_role: Some(crate::kernel::shared::ReceiverRole::Parent),
                    target: crate::kernel::shared::SentAgentMessageTarget {
                        active_session_id: "a1".to_string(),
                        session_id: "s1".to_string(),
                        session_name: Some("Worker".to_string()),
                    },
                },
            )
            .await?;
            Ok(pa_agent::types::AgentToolResult::text("cell done"))
        })
    }
}

#[tokio::test]
async fn detached_receipt_failure_holds_quiescence_and_wakes_the_error_path() {
    let directory = tempfile::tempdir().unwrap();
    let manager = crate::session::manager::SessionManager::in_memory(directory.path());
    let node = inline_root_node(&manager, recipe(directory.path(), None));
    let notified = node.activity.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let sink = late_sent_agent_message_sink(&node);
    sink(
        "call-1",
        crate::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_1".to_string(),
            message: "Ping.".to_string(),
            delivery_status: crate::kernel::shared::SentDeliveryStatus::Delivered,
            receiver_role: Some(crate::kernel::shared::ReceiverRole::Parent),
            target: crate::kernel::shared::SentAgentMessageTarget {
                active_session_id: "a1".to_string(),
                session_id: "s1".to_string(),
                session_name: None,
            },
        },
    );
    assert!(has_unsettled_rlm_work(&node).await);
    assert!(
        tokio::time::timeout(std::time::Duration::ZERO, notified)
            .await
            .is_ok(),
        "admission must wake an armed quiescence scan before the recorder runs"
    );
    let completed = node.activity.notified();
    tokio::pin!(completed);
    completed.as_mut().enable();
    tokio::time::timeout(std::time::Duration::from_secs(5), completed)
        .await
        .expect("receipt completion must wake the parked drive");
    assert_eq!(
        node.pending_receipts
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        node.late_receipt_error
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .contains("without a live print session engine")
    );
}

#[tokio::test]
async fn a_receipt_on_an_idle_child_holds_and_wakes_ancestor_quiescence() {
    let directory = tempfile::tempdir().unwrap();
    let manager = crate::session::manager::SessionManager::in_memory(directory.path());
    let root = inline_root_node(&manager, recipe(directory.path(), None));
    let mut child = inline_root_node(&manager, recipe(directory.path(), None));
    Arc::get_mut(&mut child).unwrap().parent = Some(Arc::downgrade(&root));
    let notified = root.activity.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    late_sent_agent_message_sink(&child)(
        "call-1",
        crate::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_1".to_string(),
            message: "Ping.".to_string(),
            delivery_status: crate::kernel::shared::SentDeliveryStatus::Delivered,
            receiver_role: Some(crate::kernel::shared::ReceiverRole::Parent),
            target: crate::kernel::shared::SentAgentMessageTarget {
                active_session_id: "a1".to_string(),
                session_id: "s1".to_string(),
                session_name: None,
            },
        },
    );
    // Even with no active child turn in the root's roster, pending work holds it.
    assert!(has_unsettled_rlm_work(&root).await);
    assert!(
        tokio::time::timeout(std::time::Duration::ZERO, notified)
            .await
            .is_ok(),
        "child receipt admission must wake the ancestor before recording starts"
    );
    let completed = root.activity.notified();
    tokio::pin!(completed);
    completed.as_mut().enable();
    tokio::time::timeout(std::time::Duration::from_secs(5), completed)
        .await
        .expect("a child's completed recorder must wake its ancestor");
    assert_eq!(
        root.pending_receipts
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(
        root.late_receipt_error
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .contains("without a live print session engine")
    );
}

#[tokio::test]
async fn late_receipts_record_through_the_print_path_apply_on_landing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("agent")).unwrap();
    let manager = crate::session::manager::SessionManager::persisted(
        directory.path(),
        &directory.path().join("sessions"),
    );
    let script = json!({
        "responses": [
            { "content": [
                { "type": "toolCall", "id": "call-1", "name": "python_repl", "arguments": {} }
            ] },
            "all done",
        ],
    });
    let node = inline_root_node(&manager, recipe(directory.path(), Some(script.clone())));
    let headless = faux_root_engine(&node, manager, &script).await;
    let engine = Arc::new(headless.engine);
    adopt_built_engine(&node, &engine, &headless.model).await;
    // The scripted turn runs a fake python tool in place of the
    // kernel-backed one: the tool records the receipt while its cell's
    // result row has not landed.
    engine
        .session
        .agent()
        .set_tools(vec![Arc::new(LateReceiptPythonTool {
            node: Arc::clone(&node),
        })])
        .await;

    let outcome = engine
        .prompt(
            "run the cell",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, crate::session_engine::PromptOutcome::Prompt);
    engine.session.agent().wait_for_idle().await;

    // The landed row carries the receipt in live state.
    let state = engine.session.agent().state().await;
    let result = state
        .messages
        .iter()
        .find_map(|row| match row {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::ToolResult(
                result,
            )) if result.tool_call_id == "call-1" => Some(result.clone()),
            _ => None,
        })
        .expect("the landed tool result");
    let details = result.details.as_ref().expect("the receipt rides the row");
    assert_eq!(
        details["sentAgentMessages"][0]["id"], "agentmsg_1",
        "the live state row carries the receipt"
    );

    let entries = engine.session.entries().await;
    assert!(
        entries.iter().any(|entry| matches!(
            entry,
            pa_types::session::FileEntry::Custom { payload, .. }
                if payload.custom_type == "ipython_sent_agent_message"
        )),
        "the receipt persists as a custom entry"
    );
    engine.dispose_kernel().await;
}

#[tokio::test]
async fn delivery_to_a_disposed_session_rejects_without_queuing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("agent")).unwrap();
    let manager = crate::session::manager::SessionManager::in_memory(directory.path());
    let script = json!({ "responses": ["parent"] });
    let node = inline_root_node(&manager, recipe(directory.path(), Some(script.clone())));
    let headless = faux_root_engine(&node, manager, &script).await;
    let engine = Arc::new(headless.engine);
    adopt_built_engine(&node, &engine, &headless.model).await;
    let endpoint = node.endpoint();
    let row = create_agent_session_message_row(&AgentSessionMessageRowPayload {
        id: "late-message",
        prompt: "late message",
        message: "late message",
        from: &endpoint,
        from_relationship: None,
        target: Some(&endpoint),
        timestamp: 0,
    });
    dispose_inline_subagents(&node).await;
    assert!(!engine.session.agent().has_queued_messages());
    let result = deliver_row(&node, row);
    let queued = engine.session.agent().has_queued_messages();
    engine.dispose_kernel().await;
    let error = result.expect_err("a disposed session must reject a retained delivery target");
    assert!(error.to_string().contains("disposed"), "{error:#}");
    assert!(
        !queued,
        "rejected delivery must not enqueue an unread message"
    );
}
