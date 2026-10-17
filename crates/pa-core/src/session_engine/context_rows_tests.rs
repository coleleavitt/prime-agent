use std::sync::Mutex;

use super::*;
use crate::prompts::session_context::{EnvironmentFields, InstructionFile, SessionContextInputs};
use crate::session_engine::file_tracker::FileReadTracker;
use crate::session_engine::reminders::{ReminderToggles, Reminders};

fn context(cwd: &str, rules: &str, date: &str) -> SessionContext {
    SessionContext::build(SessionContextInputs {
        agent_types: None,
        instruction_files: vec![InstructionFile {
            path: format!("{cwd}/AGENTS.md"),
            content: rules.to_string(),
        }],
        environment: EnvironmentFields {
            working_directory: cwd.to_string(),
            additional_directories: Vec::new(),
            platform: "linux".to_string(),
            ..Default::default()
        },
        date: date.to_string(),
        ..Default::default()
    })
}

fn to_messages(rows: Vec<CustomMessage>) -> Vec<AgentMessage> {
    rows.into_iter().filter_map(to_loop).collect()
}

fn quiet_reminders() -> Reminders {
    Reminders::new(
        ReminderToggles {
            changed_files: false,
            read_dedup: false,
            task_list: false,
            silent_turns: false,
            elapsed_time: false,
            tokens_left: false,
        },
        Arc::new(FileReadTracker::new()),
        false,
        None,
        0,
    )
}

/// A session whose source answers whatever the test sets.
fn rows_over(current: Arc<Mutex<SessionContext>>) -> SessionContextRows {
    SessionContextRows::new(
        Arc::new(move || current.lock().unwrap().clone()),
        quiet_reminders(),
    )
}

fn custom_type_of(message: &AgentMessage) -> &str {
    match message {
        AgentMessage::Custom(custom) => custom.payload["customType"].as_str().unwrap(),
        AgentMessage::Standard(_) => "standard",
    }
}

#[test]
fn the_first_turn_carries_the_full_row_then_nothing_until_a_change() {
    let current = Arc::new(Mutex::new(context("/w", "Rule one.", "2026-10-17")));
    let rows = rows_over(Arc::clone(&current));
    let mut conversation = to_messages(rows.turn_start_rows(&[]));
    assert_eq!(conversation.len(), 1);
    assert_eq!(
        custom_type_of(&conversation[0]),
        SESSION_CONTEXT_CUSTOM_TYPE
    );
    let first = conversation[0].clone();
    // Unchanged: nothing more, and the row already sent is never rewritten.
    assert!(rows.turn_start_rows(&conversation).is_empty());
    assert_eq!(conversation[0], first);

    *current.lock().unwrap() = context("/w", "Rule two.", "2026-10-17");
    let update = rows.turn_start_rows(&conversation);
    assert_eq!(update.len(), 1);
    assert_eq!(update[0].custom_type, SESSION_CONTEXT_UPDATE_CUSTOM_TYPE);
    assert!(!update[0].display);
    let pa_types::ai::UserContent::Text(text) = &update[0].content else {
        panic!("text row");
    };
    assert!(text.contains("## /w/AGENTS.md\n\nRule two."));
    assert!(
        !text.contains("Working directory"),
        "only what changed: {text}"
    );
    conversation.extend(to_messages(update));
    // The update's fingerprint is the new baseline.
    assert!(rows.turn_start_rows(&conversation).is_empty());
    assert_eq!(conversation[0], first);
}

#[test]
fn a_conversation_without_the_full_row_gets_it_again() {
    let current = Arc::new(Mutex::new(context("/w", "Rule one.", "2026-10-17")));
    let rows = rows_over(Arc::clone(&current));
    let full = to_messages(rows.turn_start_rows(&[]));
    *current.lock().unwrap() = context("/w2", "Rule one.", "2026-10-17");
    // A compaction kept the delta but summarized the full row away.
    let after_compaction = to_messages(rows.turn_start_rows(&full));
    let again = rows.turn_start_rows(&after_compaction);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].custom_type, SESSION_CONTEXT_CUSTOM_TYPE);
    let pa_types::ai::UserContent::Text(text) = &again[0].content else {
        panic!("text row");
    };
    assert!(text.contains("Working directory: /w2"));
}

#[test]
fn announced_reads_the_newest_fingerprint() {
    let first = context("/w", "a", "2026-10-17");
    let second = context("/w", "b", "2026-10-18");
    let rows = to_messages(vec![
        context_row_for(&first, &Announced::default()).unwrap(),
        context_row_for(&second, &announced(&[])).unwrap(),
    ]);
    let seen = announced(&rows);
    assert!(seen.full);
    assert_eq!(seen.latest, Some(second.fingerprint()));
}

#[test]
fn the_boundary_adds_a_date_change_once() {
    let current = Arc::new(Mutex::new(context("/w", "Rule one.", "2026-10-17")));
    let today = Arc::new(Mutex::new("2026-10-17".to_string()));
    let clock = Arc::clone(&today);
    let rows =
        rows_over(Arc::clone(&current)).with_today(Arc::new(move || clock.lock().unwrap().clone()));
    let mut conversation = to_messages(rows.turn_start_rows(&[]));
    assert!(rows.boundary_rows(&conversation).is_empty());
    *today.lock().unwrap() = "2026-10-18".to_string();
    let change = rows.boundary_rows(&conversation);
    assert_eq!(change.len(), 1);
    let pa_types::ai::UserContent::Text(text) = &change[0].content else {
        panic!("text row");
    };
    assert!(text.contains("The date has changed. Today's date is now 2026-10-18."));
    conversation.extend(to_messages(change));
    assert!(rows.boundary_rows(&conversation).is_empty());
    // The next turn start, on the same date, has nothing to add either.
    *current.lock().unwrap() = context("/w", "Rule one.", "2026-10-18");
    assert!(rows.turn_start_rows(&conversation).is_empty());
}

#[test]
fn a_lost_full_row_returns_at_the_boundary() {
    let current = Arc::new(Mutex::new(context("/w", "Rule one.", "2026-10-17")));
    let rows = rows_over(current);
    let at_boundary = rows.boundary_rows(&[]);
    assert_eq!(at_boundary.len(), 1);
    assert_eq!(at_boundary[0].custom_type, SESSION_CONTEXT_CUSTOM_TYPE);
}

#[test]
fn a_model_switch_is_an_environment_update() {
    use crate::session_engine::context_source::ModelFacts;
    let facts = Arc::new(Mutex::new(ModelFacts {
        label: Some("a/one".to_string()),
        vision_capable: Some(true),
        context_window: 1_000,
    }));
    let seen = Arc::clone(&facts);
    let rows = SessionContextRows::new(
        Arc::new(move || {
            let facts = seen.lock().unwrap().clone();
            let mut current = context("/w", "r", "2026-10-17");
            current.environment.model = facts.label;
            current.environment.vision_capable = facts.vision_capable;
            current
        }),
        quiet_reminders(),
    )
    .with_model(facts);
    let conversation = to_messages(rows.turn_start_rows(&[]));
    rows.set_model(ModelFacts {
        label: Some("b/two".to_string()),
        vision_capable: Some(true),
        context_window: 2_000,
    });
    let update = rows.turn_start_rows(&conversation);
    let pa_types::ai::UserContent::Text(text) = &update[0].content else {
        panic!("text row");
    };
    assert!(text.contains("# Environment update\nModel: b/two (was a/one)"));
    assert!(!text.contains("Image input"), "unchanged: {text}");
}

#[test]
fn a_moved_cwd_reaches_the_source() {
    let cwd = Arc::new(Mutex::new(std::path::PathBuf::from("/a")));
    let seen = Arc::clone(&cwd);
    let rows = SessionContextRows::new(
        Arc::new(move || {
            let cwd = seen.lock().unwrap().display().to_string();
            context(&cwd, "r", "2026-10-17")
        }),
        quiet_reminders(),
    )
    .with_cwd(cwd);
    let conversation = to_messages(rows.turn_start_rows(&[]));
    rows.set_cwd(std::path::Path::new("/b"));
    let update = rows.turn_start_rows(&conversation);
    let pa_types::ai::UserContent::Text(text) = &update[0].content else {
        panic!("text row");
    };
    assert!(text.contains("# Environment update\nWorking directory: /b (was /a)"));
}

/// The installed observer hears each produced row's kind (the session
/// telemetry's counters), never its text.
#[tokio::test]
async fn the_row_observer_hears_each_row_kind() {
    let current = Arc::new(Mutex::new(context("/w", "Rule one.", "2026-10-17")));
    let rows = Arc::new(rows_over(Arc::clone(&current)));
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let heard = Arc::clone(&heard);
        rows.observe_rows(Arc::new(move |kind| {
            heard.lock().unwrap().push(kind.to_string());
        }));
    }
    let conversation = rows.turn_start_messages(Vec::new()).await;
    *current.lock().unwrap() = context("/w", "Rule two.", "2026-10-17");
    let _ = rows.turn_start_messages(conversation).await;
    assert_eq!(
        *heard.lock().unwrap(),
        vec![
            SESSION_CONTEXT_CUSTOM_TYPE.to_string(),
            SESSION_CONTEXT_UPDATE_CUSTOM_TYPE.to_string()
        ]
    );
}
