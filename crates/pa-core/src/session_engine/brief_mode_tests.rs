//! Brief mode: the command, the `user_message.send` request, the change
//! rows a resume restores, and the stop enforcement.

use pa_agent::types::{AgentContext, AssistantContent, AssistantMessage, TextContent, UserMessage};
use tokio::sync::Mutex;

use super::*;
use crate::kernel::shared::HostRequestPayload;
use crate::session::manager::SessionManager;

fn assistant(stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: "All done.".to_string(),
            text_signature: None,
        })],
        api: "test".into(),
        provider: "test".into(),
        model: "m".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        discarded_usage: None,
    }
}

/// A run that ends on `stop_reason` after a prompt sent at `prompt_ms`.
fn turn(stop_reason: StopReason, prompt_ms: i64) -> ShouldStopAfterTurnContext {
    let message = assistant(stop_reason);
    ShouldStopAfterTurnContext {
        message: message.clone(),
        tool_results: Vec::new(),
        context: AgentContext {
            system_prompt: String::new(),
            messages: vec![
                AgentMessage::Standard(Message::User(UserMessage {
                    content: pa_agent::types::UserContent::Text("hi".into()),
                    timestamp: prompt_ms,
                })),
                AgentMessage::Standard(Message::Assistant(message)),
            ],
            tools: Vec::new(),
        },
        new_messages: Vec::new(),
    }
}

#[test]
fn the_command_parses_like_claude_codes_toggle() {
    assert_eq!(
        ["", "on", " off ", "status"].map(|args| parse_brief_command(args).unwrap()),
        [
            BriefCommand::Toggle,
            BriefCommand::On,
            BriefCommand::Off,
            BriefCommand::Status
        ]
    );
    assert_eq!(
        parse_brief_command("loud"),
        Err("Usage: /brief [on|off|status]".to_string())
    );
}

#[test]
fn a_request_is_validated() {
    assert_eq!(
        parse_user_message(&json!({ "message": "Fixed in src/a.rs:3", "attachments": ["a.log"], "status": "proactive" }))
            .unwrap(),
        UserMessageRequest {
            message: "Fixed in src/a.rs:3".to_string(),
            attachments: vec!["a.log".to_string()],
            proactive: true,
        }
    );
    for bad in [
        json!({}),
        json!({ "message": "  " }),
        json!({ "message": "m", "attachments": "a.log" }),
        json!({ "message": "m", "attachments": [""] }),
        json!({ "message": "m", "status": "urgent" }),
    ] {
        assert!(parse_user_message(&bad).is_err(), "{bad} must be refused");
    }
}

#[test]
fn the_newest_change_row_wins_on_resume() {
    let entry = |enabled: bool| {
        let row = brief_mode_change_row(enabled);
        FileEntry::CustomMessage {
            payload: pa_types::session::CustomMessageEntry {
                custom_type: row.custom_type,
                content: row.content,
                details: row.details,
                display: row.display,
                rest: serde_json::Map::default(),
            },
            base: serde_json::from_value::<pa_types::session::EntryBase>(json!({ "id": "e" }))
                .unwrap(),
        }
    };
    let entries = [entry(true), entry(false), entry(true)];
    assert_eq!(brief_mode_in_entries(entries.iter()), Some(true));
    assert_eq!(brief_mode_in_entries(entries[..2].iter()), Some(false));
    assert_eq!(brief_mode_in_entries([].iter()), None);
    let reminder = brief_mode_reminder_row(true);
    assert!(!reminder.display);
    let pa_types::ai::UserContent::Text(text) = &reminder.content else {
        panic!("a text row");
    };
    assert!(text.starts_with("<system-reminder>\nBrief mode is now enabled."));
    assert!(text.contains("## Talking to the user"));
}

#[test]
fn a_turn_that_never_messaged_the_user_is_continued_once() {
    let state = BriefState::new(true);
    let natural = turn(StopReason::Stop, 1_000);
    let text = enforcement_text(&state, &natural).expect("enforced");
    assert!(text.contains(
        "You ended the turn without calling rlm.send_user_message. In brief mode, plain assistant text is hidden"
    ));

    // A reminded run is not reminded twice.
    let mut reminded = natural.clone();
    let row = super::super::plan_mode::hidden_row(BRIEF_ENFORCEMENT_CUSTOM_TYPE, &text);
    reminded.new_messages.push(
        super::super::session_message_to_loop(&super::super::SessionAgentMessage::Custom(row))
            .unwrap(),
    );
    assert_eq!(enforcement_text(&state, &reminded), None);

    // A cut-off reply, a tool batch, or brief mode off: no enforcement.
    assert_eq!(
        enforcement_text(&state, &turn(StopReason::Length, 1_000)),
        None
    );
    assert_eq!(enforcement_text(&BriefState::new(false), &natural), None);

    // A message sent after the prompt satisfies it; one before does not.
    state.note_sent();
    assert_eq!(enforcement_text(&state, &natural), None);
    let later = turn(StopReason::Stop, pa_agent::now_ms() + 60_000);
    assert!(enforcement_text(&state, &later).is_some());
}

async fn rows(session: &Mutex<SessionManager>) -> Vec<pa_types::session::CustomMessageEntry> {
    session
        .lock()
        .await
        .get_all_entries()
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::CustomMessage { payload, .. } => Some(payload.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_request_records_a_displayed_row_only_in_brief_mode() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("diff.txt"), "+x\n").unwrap();
    let session = Arc::new(Mutex::new(SessionManager::in_memory(dir.path())));
    let state = Arc::new(BriefState::new(false));
    let mut handlers = HostRequestHandlers::default();
    register_host_handlers(
        &mut handlers,
        UserMessageContext {
            cwd: dir.path().to_path_buf(),
            state: Arc::clone(&state),
            rows: Arc::new(super::super::presented_artifact::PresentedArtifacts::new()),
            session: Arc::clone(&session),
        },
    );
    let handler = handlers
        .get(SEND_USER_MESSAGE_REQUEST)
        .expect("registered")
        .clone();
    let send = |data: Value| handler(HostRequestPayload { data });

    let refused = send(json!({ "message": "hello" })).await.unwrap_err();
    assert_eq!(refused.to_string(), NOT_IN_BRIEF_MODE);
    assert!(rows(&session).await.is_empty());

    state.set(true);
    let receipt = send(json!({ "message": "Done: see the diff", "attachments": ["diff.txt"] }))
        .await
        .unwrap();
    assert_eq!(receipt, json!({ "delivered": true, "attachments": 1 }));
    let recorded = rows(&session).await;
    assert_eq!(recorded.len(), 1);
    let row = &recorded[0];
    assert_eq!(
        (row.custom_type.as_str(), row.display, &row.content),
        (
            USER_MESSAGE_CUSTOM_TYPE,
            true,
            &pa_types::ai::UserContent::Text("Done: see the diff".to_string())
        )
    );
    let details = row.details.as_ref().unwrap();
    assert_eq!(details["status"], "normal");
    assert_eq!(
        details["attachments"],
        json!([{
            "path": dir.path().join("diff.txt").display().to_string(),
            "size": 3,
            "isImage": false,
        }])
    );
    assert!(state.sent_since(1));

    // A missing attachment fails the call and records nothing.
    assert!(
        send(json!({ "message": "m", "attachments": ["nope.png"] }))
            .await
            .is_err()
    );
    assert_eq!(rows(&session).await.len(), 1);
}
