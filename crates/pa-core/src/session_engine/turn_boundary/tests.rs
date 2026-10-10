//! The turn-boundary unit battery: the model-info reports, the compact
//! family round trips, the refine scheduling, and the context usage.
use super::*;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;
use crate::session_engine::tool_bridge::bridge_tool;
use crate::tools::tool_definition::ToolDefinition;
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::ThinkingLevel;
use pa_types::ai::{
    AssistantContentBlock, AssistantMessage, StopReason, TextContent, Usage, UserContent,
};
use pa_types::session::AgentMessage as SessionMessage;

fn payload(data: Value) -> HostRequestPayload {
    HostRequestPayload {
        data,
        cell_source_code: None,
    }
}

fn model_info() -> ModelInfo {
    ModelInfo {
        id: "faux-1".to_string(),
        provider: "faux".to_string(),
        input: vec![pa_types::ai::ModelInput::Text],
    }
}

fn agent_model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "faux-1".to_string(),
        name: "Faux".to_string(),
        api: "test".to_string(),
        provider: "faux".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 100_000,
        max_tokens: 1_000,
        max_tokens_explicit: false,
    }
}

fn user_entry(text: &str) -> SessionMessage {
    SessionMessage::User(pa_types::ai::UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 1,
        rest: serde_json::Map::default(),
    })
}

fn assistant_entry(text: &str) -> SessionMessage {
    SessionMessage::Assistant(AssistantMessage {
        content: vec![AssistantContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
            rest: serde_json::Map::default(),
        })],
        api: "test".to_string(),
        provider: "faux".to_string(),
        model: "faux-1".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage {
            input: 40,
            output: 10,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 50,
            cost: pa_types::ai::UsageCost::default(),
        },
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 1,
        rest: serde_json::Map::default(),
        discarded_usage: None,
    })
}

/// A persisted session manager with a small conversation to summarize
/// (`end_with_compaction` flips it into the `already compacted` shape).
fn session_with_history(end_with_compaction: bool) -> SessionManager {
    let dir = crate::test_support::ThreadTempDir::new();
    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut session = SessionManager::in_memory(dir.path());
    session.materialize_session_file(Some(session_dir));
    for text in ["alpha task", "beta task", "gamma task"] {
        session.append_message(user_entry(text)).unwrap();
        session
            .append_message(assistant_entry(&format!("{text} done")))
            .unwrap();
    }
    if end_with_compaction {
        let first_kept = session.get_all_entries()[1]
            .id()
            .unwrap_or_default()
            .to_string();
        session
            .append_compaction(pa_types::session::CompactionEntry {
                summary: "summary".to_string(),
                first_kept_entry_id: first_kept,
                tokens_before: 10,
                ..Default::default()
            })
            .unwrap();
    }
    session
}

impl SessionManager {
    /// Strip the conversation entries (the fresh-session prepare case).
    fn without_history(mut self) -> Self {
        let dir = crate::test_support::ThreadTempDir::new();
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        self = SessionManager::in_memory(dir.path());
        self.materialize_session_file(Some(session_dir));
        self
    }
}

fn registered(requests: &Arc<TurnBoundaryRequests>) -> HostRequestHandlers {
    let mut handlers = HostRequestHandlers::default();
    requests.register_model_info_handler(&mut handlers, model_info());
    // A tiny keep-recent budget: a short conversation has history to
    // summarize above it.
    requests.register_compact_handlers(&mut handlers, 1);
    requests.register_refine_handlers(&mut handlers);
    handlers
}

fn handler(
    handlers: &HostRequestHandlers,
    request_type: &str,
) -> crate::kernel::shared::HostHandlerFn {
    handlers.get(request_type).expect("registered").clone()
}

#[tokio::test]
async fn model_info_reports_the_bound_model() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    // Before the runtime binds, the registration-time facts answer.
    let response = handler(&handlers, "model.info")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(response["id"], "faux-1");
    assert_eq!(response["provider"], "faux");
    assert_eq!(response["input"], json!(["text"]));
    // A bound runtime stays authoritative.
    let session = Arc::new(Mutex::new(SessionManager::in_memory(std::path::Path::new(
        "/tmp",
    ))));
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: None,
            messages: None,
        },
        stream_fn: None,
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: ModelInfo {
            id: "other-1".to_string(),
            provider: "other".to_string(),
            input: Vec::new(),
        },
    });
    let response = handler(&handlers, "model.info")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(response["id"], "other-1");
    assert_eq!(response["provider"], "other");
    assert_eq!(response["input"], json!([]));
}

#[tokio::test]
async fn compact_run_validates_and_reports_no_active_turn() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    // Non-string instructions error with the exact TS message.
    let error = handler(&handlers, "compact.run")(payload(json!({
        "instructions": 5
    })))
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "compact.run instructions must be a string when provided"
    );
    // Without a bound runtime there is no turn to schedule against.
    let response = handler(&handlers, "compact.run")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(response["scheduled"], false);
    assert_eq!(
        response["reason"],
        "no active turn; compaction can only be requested while a turn is running"
    );
    // An idle agent answers the same way, and status stays clear.
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: None,
            messages: None,
        },
        stream_fn: None,
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let response = handler(&handlers, "compact.run")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(response["scheduled"], false);
    let status = handler(&handlers, "compact.status")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(status["scheduled"], false);
}

#[tokio::test]
async fn compact_run_prepare_skips_report_the_ts_reasons() {
    for (case, expected) in [
        // A branch ending in a compaction has nothing new to summarize.
        (session_with_history(true), "already compacted"),
        // A fresh session has no summarizable history.
        (
            session_with_history(false).without_history(),
            "session is too short to compact",
        ),
    ] {
        // A fresh request cell per case: the runtime binds once.
        let requests = Arc::new(TurnBoundaryRequests::new());
        registered(&requests);
        let session = Arc::new(Mutex::new(case));
        let provider = Arc::new(ScriptedProvider::new(agent_model()));
        provider.push_tool_call_turn(None, vec![("call-1", "probe", json!({}))]);
        provider.push_text_turn("done");
        let probe = probe_tool(&requests, "probe", "compact.run", json!({}));
        let agent = Arc::new(Agent::new(AgentOptions {
            initial_state: AgentInitialState {
                system_prompt: Some("s".to_string()),
                model: Some(agent_model()),
                thinking_level: Some(ThinkingLevel::Off),
                tools: Some(vec![probe.tool.clone()]),
                messages: None,
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        }));
        requests.bind(TurnBoundaryRuntime {
            agent,
            session,
            context_window: Some(100_000),
            model_info: model_info(),
        });
        // The empty-history case: strip the conversation entries (the
        // handler sees them through the bound session).
        let agent = requests.bound().expect("bound").agent.clone();
        agent.prompt("run the probe tool").await.unwrap();
        agent.wait_for_idle().await;
        let response = probe.result().await;
        assert_eq!(response["scheduled"], false, "case={expected}");
        assert_eq!(response["reason"], expected);
        assert!(requests.take_compaction().await.is_none());
    }
}

#[tokio::test]
async fn compact_run_schedules_inside_a_tool_call_and_status_sees_it() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "probe", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe",
        "compact.run",
        json!({ "instructions": "keep the failing test names" }),
    );
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![probe.tool.clone()]),
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let agent = requests.bound().expect("bound").agent.clone();
    agent.prompt("run the probe tool").await.unwrap();
    agent.wait_for_idle().await;

    // The handler ran inside the tool call (mid-turn) and scheduled.
    let response = probe.result().await;
    assert_eq!(response["scheduled"], true);
    assert_eq!(
        response["note"],
        "Compaction runs when the current turn ends; you resume automatically afterwards. Continue working normally."
    );
    // compact.status reports the estimate over the session entries and
    // the scheduled flag (the estimate is the chars/4 sum: no usage).
    let status = handler(&handlers, "compact.status")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(status["scheduled"], true);
    assert_eq!(status["context_window"], 100_000);
    assert!(status["tokens"].as_u64().unwrap() > 0, "{status}");
    assert!(status["percent"].as_f64().unwrap() > 0.0, "{status}");
    assert_eq!(
        requests.take_compaction().await,
        Some(PendingCompaction {
            instructions: Some("keep the failing test names".to_string())
        })
    );
    assert!(requests.take_compaction().await.is_none());
}

#[tokio::test]
async fn refine_run_and_status_round_trip_inside_a_tool_call() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "probe", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe",
        "refine.run",
        json!({ "instructions": "create a memory about the failing gate", "global": true }),
    );
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![probe.tool.clone()]),
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    // refine.status before the turn: not pending, never in flight (the
    // consumption runs synchronously between turns).
    let status = handler(&handlers, "refine.status")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(status["pending"], false);
    assert_eq!(status["in_flight"], false);

    let agent = requests.bound().expect("bound").agent.clone();
    agent.prompt("run the probe tool").await.unwrap();
    agent.wait_for_idle().await;

    let response = probe.result().await;
    assert_eq!(response["scheduled"], true);
    assert_eq!(
        response["note"],
        "Refinement runs when the current turn ends; applied edits are appended to your context as a refinement notice and you resume automatically. Continue working normally."
    );
    let status = handler(&handlers, "refine.status")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(status["pending"], true);
    assert_eq!(
        requests.take_refine().await,
        Some(PendingRefine {
            instructions: Some("create a memory about the failing gate".to_string()),
            global: true,
            trigger: None,
            plan_id: None,
        })
    );
    assert!(requests.take_refine().await.is_none());
}

/// A feature queues its own refine through a requester; the agent's
/// `refine.run` in the same turn joins it instead of replacing it: the
/// queued instructions stay with the agent's appended, and the trigger
/// rides on, marked joined.
#[tokio::test]
async fn a_feature_request_keeps_its_trigger_when_refine_run_joins_it() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    let requester = RefineRequester::new(&requests);
    let trigger = RefineTrigger {
        data: json!({ "reason": "recurrence" }),
        joined_by_agent: false,
    };
    let queued = trigger.clone();
    let updated = tokio::task::spawn_blocking(move || {
        requester.update(|pending| {
            assert_eq!(pending, None);
            Some(PendingRefine {
                instructions: Some("stop the recurring failure".to_string()),
                global: false,
                trigger: Some(queued),
                plan_id: None,
            })
        })
    })
    .await
    .unwrap();
    assert!(updated);
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "probe-a", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe-a",
        "refine.run",
        json!({ "instructions": "and note the tactic" }),
    );
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![probe.tool.clone()]),
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let agent = requests.bound().expect("bound").agent.clone();
    agent.prompt("run the probe").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(probe.result().await["scheduled"], true);
    assert_eq!(
        requests.take_refine().await,
        Some(PendingRefine {
            instructions: Some("stop the recurring failure\n\nand note the tactic".to_string()),
            global: false,
            trigger: Some(RefineTrigger {
                joined_by_agent: true,
                ..trigger
            }),
            plan_id: None,
        })
    );
    // The session is gone: the requester reports it.
    let requester = RefineRequester::new(&requests);
    drop(handlers);
    drop(requests);
    assert!(
        !tokio::task::spawn_blocking(move || requester.update(|pending| pending))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn refine_run_validates_and_merges_into_a_pending_request() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    let error = handler(&handlers, "refine.run")(payload(json!({ "instructions": 5 })))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "refine.run instructions must be a string when provided"
    );
    let error = handler(&handlers, "refine.run")(payload(json!({ "global": "yes" })))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "refine.run global must be a boolean when provided"
    );
    // Merge semantics through the real handler path: two `refine.run` calls inside one
    // turn — the second without instructions keeps the first ones and ORs the global
    // flag (TS merge).
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "probe-a", json!({}))]);
    provider.push_tool_call_turn(None, vec![("call-2", "probe-b", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe-a",
        "refine.run",
        json!({ "instructions": "first observation" }),
    );
    let probe_again = probe_tool(
        &requests,
        "probe-b",
        "refine.run",
        json!({ "global": true }),
    );
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![probe.tool.clone(), probe_again.tool.clone()]),
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let agent = requests.bound().expect("bound").agent.clone();
    agent.prompt("run both probes").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(probe.result().await["scheduled"], true);
    assert_eq!(probe_again.result().await["scheduled"], true);
    assert_eq!(
        requests.take_refine().await,
        Some(PendingRefine {
            instructions: Some("first observation".to_string()),
            global: true,
            trigger: None,
            plan_id: None,
        })
    );
}

#[tokio::test]
async fn clear_pending_drops_scheduled_requests() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    registered(&requests);
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "probe", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe",
        "refine.run",
        json!({ "instructions": "x" }),
    );
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![probe.tool.clone()]),
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let agent = requests.bound().expect("bound").agent.clone();
    agent.prompt("run the probe tool").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(probe.result().await["scheduled"], true);
    assert!(requests.refine_pending().await);
    requests.clear_pending().await;
    assert!(requests.take_refine().await.is_none());
    assert!(requests.take_compaction().await.is_none());
}

#[tokio::test]
async fn compact_run_merges_instructions_into_a_pending_request() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    registered(&requests);
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "probe-a", json!({}))]);
    provider.push_tool_call_turn(None, vec![("call-2", "probe-b", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe-a",
        "compact.run",
        json!({ "instructions": "first" }),
    );
    let probe_again = probe_tool(&requests, "probe-b", "compact.run", json!({}));
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: Some(vec![probe.tool.clone(), probe_again.tool.clone()]),
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let agent = requests.bound().expect("bound").agent.clone();
    agent.prompt("run both probes").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(probe.result().await["scheduled"], true);
    assert_eq!(probe_again.result().await["scheduled"], true);
    assert_eq!(
        requests.take_compaction().await,
        Some(PendingCompaction {
            instructions: Some("first".to_string())
        })
    );
}

#[test]
fn context_usage_anchors_on_the_last_valid_assistant_usage() {
    let dir = tempfile::TempDir::new().unwrap();
    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut session = SessionManager::in_memory(dir.path());
    session.materialize_session_file(Some(session_dir));
    let mut assistant = assistant_entry("done");
    let SessionMessage::Assistant(ref mut message) = assistant else {
        unreachable!();
    };
    message.usage = Usage {
        input: 100,
        output: 20,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 130,
        cost: pa_types::ai::UsageCost::default(),
    };
    session.append_message(assistant).unwrap();
    session
        .append_message(user_entry("a somewhat long trailing message"))
        .unwrap();
    let entries = session.get_all_entries().to_vec();
    assert!(context_usage(&entries, None).is_none());
    let usage = context_usage(&entries, Some(100_000)).unwrap();
    // The usage anchor plus the trailing estimate (chars/4).
    let trailing = "a somewhat long trailing message".chars().count() as u64 / 4;
    assert_eq!(usage.tokens, Some(130 + trailing));
    assert_eq!(usage.context_window, 100_000);
    let percent = usage.percent.unwrap();
    assert!((percent - (usage.tokens.unwrap() as f64 / 100_000.0 * 100.0)).abs() < 1e-9);
}

#[test]
fn context_usage_after_a_compaction_without_post_usage_is_null_tokens() {
    let mut session = session_with_history(false);
    let first_kept = session.get_all_entries()[1]
        .id()
        .unwrap_or_default()
        .to_string();
    session
        .append_compaction(pa_types::session::CompactionEntry {
            summary: "summary".to_string(),
            first_kept_entry_id: first_kept,
            tokens_before: 10,
            ..Default::default()
        })
        .unwrap();
    let entries = session.get_all_entries().to_vec();
    let usage = context_usage(&entries, Some(100_000)).unwrap();
    assert_eq!(usage.tokens, None);
    assert_eq!(usage.percent, None);
    assert_eq!(usage.context_window, 100_000);
}

/// One scripted probe: a tool whose execution calls a registered host handler with `data` (the
/// kernel-cell shape — host requests fire inside a tool call while the turn streams) and records
/// the response.
struct Probe {
    tool: Arc<dyn pa_agent::types::AgentTool>,
    slot: Arc<std::sync::Mutex<Option<Value>>>,
}

impl Probe {
    async fn result(&self) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(response) = self.slot.lock().unwrap().take() {
                return response;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "probe handler result never appeared"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

fn probe_tool(
    requests: &Arc<TurnBoundaryRequests>,
    name: &str,
    request_type: &str,
    data: Value,
) -> Probe {
    let handler = {
        let mut handlers = HostRequestHandlers::default();
        requests.register_model_info_handler(&mut handlers, model_info());
        requests.register_compact_handlers(&mut handlers, 1);
        requests.register_refine_handlers(&mut handlers);
        handlers.get(request_type).expect("registered").clone()
    };
    let slot: Arc<std::sync::Mutex<Option<Value>>> = Arc::new(std::sync::Mutex::new(None));
    let slot_in_tool = Arc::clone(&slot);
    let definition = ToolDefinition {
        name: name.to_string(),
        label: "Probe".to_string(),
        description: "Calls a host handler".to_string(),
        prompt_snippet: String::new(),
        parameters: json!({ "type": "object", "properties": {} }),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_id, _params, _signal, _on_update| {
            let handler = handler.clone();
            let slot = slot_in_tool.clone();
            let data = data.clone();
            Box::pin(async move {
                let response = match handler(payload(data)).await {
                    Ok(response) => response,
                    Err(error) => json!({ "__handler_error__": format!("{error:#}") }),
                };
                *slot.lock().unwrap() = Some(response);
                Ok(crate::tools::tool_definition::ToolExecutionResult::text(
                    "probed",
                ))
            })
        }),
    };
    Probe {
        tool: bridge_tool(definition),
        slot,
    }
}

/// A feature that queued a refinement hears when an aborted turn drops it
/// unserviced (TS `refine_failed` for a cancelled request); a refinement
/// the boundary takes to run is no drop.
#[tokio::test]
async fn a_dropped_pending_refine_is_reported_to_the_requesters_listeners() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let requester = RefineRequester::new(&requests);
    let dropped: Arc<std::sync::Mutex<Vec<PendingRefine>>> = Arc::default();
    let heard = Arc::clone(&dropped);
    assert!(
        requester.on_dropped(Arc::new(move |pending: &PendingRefine| {
            heard.lock().unwrap().push(pending.clone());
        }))
    );
    let pending = PendingRefine {
        instructions: Some("repair".to_string()),
        global: false,
        trigger: Some(RefineTrigger {
            data: json!({ "kind": "failure" }),
            joined_by_agent: false,
        }),
        plan_id: None,
    };
    requests.schedule_refine(pending.clone()).await;
    assert_eq!(requests.take_refine().await, Some(pending.clone()));
    requests.clear_pending().await;
    assert!(dropped.lock().unwrap().is_empty());
    requests.schedule_refine(pending.clone()).await;
    requests.clear_pending().await;
    assert_eq!(*dropped.lock().unwrap(), [pending]);
    // The session is gone: nothing to listen to.
    drop(requests);
    assert!(!requester.on_dropped(Arc::new(|_: &PendingRefine| {})));
}

/// A planning model for `refine.preview` (the reply comes from the seam).
fn planning_model() -> pa_types::ai::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "openai-completions", "provider": "faux",
        "baseUrl": "", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 1_000
    }))
    .unwrap()
}

/// A planning source whose every call answers `reply` (and counts the calls).
fn planning_source(
    global_dir: std::path::PathBuf,
    reply: &'static str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> RefinePlanningSource {
    Arc::new(move || {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(RefinePlanningContext {
            model: planning_model(),
            global_harness_dir: global_dir.clone(),
            refine_call: Box::new(move |_model, _system, _prompt| {
                Box::pin(async move {
                    match assistant_entry(reply) {
                        SessionMessage::Assistant(message) => Ok(message),
                        _ => unreachable!("assistant_entry builds an assistant"),
                    }
                })
            }),
        })
    })
}

/// Upstream #899: `refine.preview` returns the planned edits without
/// scheduling or writing anything, `refine.status` lists the held plan, and
/// `refine.run(plan_id=...)` pins exactly that plan (an unknown id refuses).
#[tokio::test]
async fn refine_preview_holds_a_plan_that_refine_run_pins() {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let handlers = registered(&requests);
    let session = Arc::new(Mutex::new(session_with_history(false)));
    let global_dir = tempfile::TempDir::new().unwrap();
    // Unbound, no planning source: the preview is unavailable.
    let error = handler(&handlers, "refine.preview")(payload(json!({})))
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "refine.preview is not available before the session is ready"
    );
    let provider = Arc::new(ScriptedProvider::new(agent_model()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reply = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"global:m1","title":"Tactic","content":"Use tactic A","reason":"seen twice"}]}"#;
    requests.set_refine_planning_source(planning_source(
        global_dir.path().to_path_buf(),
        reply,
        Arc::clone(&calls),
    ));
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(agent_model()),
            thinking_level: Some(ThinkingLevel::Off),
            tools: None,
            messages: None,
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session,
        context_window: Some(100_000),
        model_info: model_info(),
    });
    let error = handler(&handlers, "refine.preview")(payload(json!({ "global": "yes" })))
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "refine.preview global must be a boolean when provided"
    );
    let mut preview = handler(&handlers, "refine.preview")(payload(
        json!({ "global": true, "instructions": "note the tactic" }),
    ))
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let plan_id = preview["plan_id"].as_str().unwrap().to_string();
    preview["plan_id"] = json!("<id>");
    assert_eq!(
        preview,
        json!({
            "plan_id": "<id>",
            "summary": "note it",
            "rationale": "repeated",
            "expected_outcome": "recall",
            "scope": "global",
            "edits": [{
                "action": "create", "kind": "memory", "id": "m1",
                "title": "Tactic", "content": "Use tactic A", "reason": "seen twice"
            }]
        })
    );
    // Nothing scheduled, nothing written; the plan is held.
    let status = handler(&handlers, "refine.status")(payload(json!({})))
        .await
        .unwrap();
    assert_eq!(
        status,
        json!({ "pending": false, "in_flight": false, "preview_ids": [plan_id.clone()] })
    );
    assert_eq!(
        crate::refinement::load_harness_state(
            global_dir.path(),
            crate::refinement::HarnessScope::Global
        ),
        crate::refinement::empty_harness_state()
    );
    // An unknown plan id refuses up front.
    let error = handler(&handlers, "refine.run")(payload(json!({ "plan_id": "refine_nope" })))
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "refine.run plan_id refine_nope is unknown or expired; call refine.preview() again"
    );
    // Inside a turn, the held plan pins.
    provider.push_tool_call_turn(None, vec![("call-1", "probe", json!({}))]);
    provider.push_text_turn("done");
    let probe = probe_tool(
        &requests,
        "probe",
        "refine.run",
        json!({ "plan_id": plan_id, "instructions": "ignored: the plan carries its own" }),
    );
    let agent = requests.bound().expect("bound").agent.clone();
    agent.set_tools(vec![probe.tool.clone()]).await;
    agent.prompt("run the probe tool").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(probe.result().await["scheduled"], true);
    assert_eq!(
        requests.take_refine().await,
        Some(PendingRefine {
            instructions: None,
            global: false,
            trigger: None,
            plan_id: Some(plan_id.clone()),
        })
    );
    let held = requests
        .take_refine_preview(&plan_id)
        .expect("the plan is held");
    assert!(held.global);
    assert_eq!(held.instructions.as_deref(), Some("note the tactic"));
    assert!(requests.refine_preview_ids().is_empty());
}

/// At most [`MAX_REFINE_PREVIEWS`] plans are held; the oldest drops.
#[tokio::test]
async fn refine_previews_keep_the_newest_plans() {
    let requests = TurnBoundaryRequests::new();
    for index in 0..=MAX_REFINE_PREVIEWS {
        requests.remember_refine_preview(super::super::refine::PreviewedRefinement {
            plan_id: format!("refine_{index}"),
            global: false,
            instructions: None,
            proposal: crate::refinement::planner::RefinementProposal::default(),
            baseline_state: crate::refinement::empty_harness_state(),
        });
    }
    assert_eq!(
        requests.refine_preview_ids(),
        (1..=MAX_REFINE_PREVIEWS)
            .map(|index| format!("refine_{index}"))
            .collect::<Vec<_>>()
    );
    requests.clear_refine_previews();
    assert!(requests.refine_preview_ids().is_empty());
}
