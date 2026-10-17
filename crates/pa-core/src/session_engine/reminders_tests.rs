use std::sync::Arc;

use pa_agent::types::AgentMessage;

use super::*;

fn message(value: serde_json::Value) -> AgentMessage {
    serde_json::from_value(value).expect("message json")
}

fn user(text: &str) -> AgentMessage {
    message(serde_json::json!({ "role": "user", "content": text, "timestamp": 1 }))
}

fn assistant(content: &serde_json::Value, input_tokens: u64) -> AgentMessage {
    message(serde_json::json!({
        "role": "assistant",
        "content": content,
        "usage": { "input": input_tokens, "output": 10, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0 },
        "stopReason": "toolUse",
        "timestamp": 2,
    }))
}

fn tool_turn(code: &str) -> AgentMessage {
    assistant(
        &serde_json::json!([{
            "type": "toolCall", "id": "c", "name": "python_repl", "arguments": { "code": code },
        }]),
        100,
    )
}

fn text_turn(text: &str) -> AgentMessage {
    assistant(&serde_json::json!([{ "type": "text", "text": text }]), 100)
}

fn reminder_row(kinds: &[ReminderKind]) -> AgentMessage {
    let row = ReminderRow {
        text: "<system-reminder>\nx\n</system-reminder>".to_string(),
        kinds: kinds.to_vec(),
    };
    crate::session_engine::session_message_to_loop(&pa_types::session::AgentMessage::Custom(
        row.custom_message(),
    ))
    .expect("loop row")
}

fn only(toggles: impl FnOnce(&mut ReminderToggles)) -> ReminderToggles {
    let mut all_off = ReminderToggles {
        changed_files: false,
        read_dedup: false,
        task_list: false,
        silent_turns: false,
        elapsed_time: false,
        tokens_left: false,
    };
    toggles(&mut all_off);
    all_off
}

fn reminders(toggles: ReminderToggles, tasks_file: Option<PathBuf>) -> Reminders {
    Reminders::new(
        toggles,
        Arc::new(FileReadTracker::new()),
        true,
        tasks_file,
        1_000,
    )
}

#[test]
fn defaults_match_claude_code() {
    let defaults = ReminderToggles::default();
    assert!(defaults.changed_files && defaults.task_list && defaults.tokens_left);
    assert!(!defaults.silent_turns && !defaults.elapsed_time && !defaults.read_dedup);
}

#[test]
fn task_list_turns_count_from_the_last_write_and_reminder() {
    let mut messages = vec![user("go"), tool_turn("await tasks.create('a')")];
    messages.extend((0..3).map(|_| tool_turn("print(1)")));
    assert_eq!(task_list_turns(&messages), (3, 4));
    messages.push(reminder_row(&[ReminderKind::TaskList]));
    messages.push(tool_turn("print(2)"));
    assert_eq!(task_list_turns(&messages), (4, 1));
    // Never written: every assistant turn counts.
    assert_eq!(task_list_turns(&[tool_turn("x"), tool_turn("y")]), (2, 2));
}

#[test]
fn the_task_list_nag_fires_at_ten_and_ten_and_lists_open_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("tasks.json");
    std::fs::write(
        &store,
        serde_json::json!({
            "version": 1, "next_id": 3,
            "tasks": [
                { "id": "1", "subject": "Write the parser", "status": "in_progress" },
                { "id": "2", "subject": "Dropped idea", "status": "deleted" },
            ],
        })
        .to_string(),
    )
    .unwrap();
    let nag = reminders(only(|toggles| toggles.task_list = true), Some(store));
    let mut messages = vec![user("go")];
    messages.extend((0..9).map(|_| tool_turn("print(1)")));
    assert_eq!(nag.row(&messages, ReminderPoint::Boundary), None);
    messages.push(tool_turn("print(1)"));
    let row = nag
        .row(&messages, ReminderPoint::Boundary)
        .expect("due at 10");
    assert_eq!(row.kinds, vec![ReminderKind::TaskList]);
    assert!(
        row.text
            .starts_with("<system-reminder>\nThe task list hasn't been used recently.")
    );
    assert!(row.text.ends_with(
        "Here are the existing tasks:\n\n#1. [in_progress] Write the parser\n</system-reminder>"
    ));
    // The next one waits another ten turns.
    messages.push(reminder_row(&[ReminderKind::TaskList]));
    messages.extend((0..9).map(|_| tool_turn("print(1)")));
    assert_eq!(nag.row(&messages, ReminderPoint::Boundary), None);
}

#[test]
fn no_tasks_skill_means_no_nag() {
    let nag = Reminders::new(
        only(|toggles| toggles.task_list = true),
        Arc::new(FileReadTracker::new()),
        false,
        None,
        0,
    );
    let messages: Vec<_> = (0..20).map(|_| tool_turn("print(1)")).collect();
    assert_eq!(nag.row(&messages, ReminderPoint::Boundary), None);
}

#[test]
fn silent_turns_nudge_after_five_and_at_most_three_times() {
    let nudge = reminders(only(|toggles| toggles.silent_turns = true), None);
    let mut messages = vec![user("go"), text_turn("Starting.")];
    messages.extend((0..4).map(|_| tool_turn("print(1)")));
    assert_eq!(silent_turns(&messages), (4, 0));
    assert_eq!(nudge.row(&messages, ReminderPoint::Boundary), None);
    messages.push(tool_turn("print(1)"));
    let row = nudge
        .row(&messages, ReminderPoint::Boundary)
        .expect("due at 5");
    assert_eq!(row.text, system_reminder(SILENT_TURN_TEXT));
    // Turn start never nudges (the user just spoke).
    assert_eq!(nudge.row(&messages, ReminderPoint::TurnStart), None);
    for _ in 0..2 {
        messages.push(reminder_row(&[ReminderKind::SilentTurns]));
        messages.extend((0..5).map(|_| tool_turn("print(1)")));
        assert!(nudge.row(&messages, ReminderPoint::Boundary).is_some());
    }
    messages.push(reminder_row(&[ReminderKind::SilentTurns]));
    messages.extend((0..5).map(|_| tool_turn("print(1)")));
    assert_eq!(silent_turns(&messages).1, 3);
    assert_eq!(
        nudge.row(&messages, ReminderPoint::Boundary),
        None,
        "three per stretch"
    );
    // A reply to the user starts a new stretch.
    messages.push(text_turn("Found it."));
    messages.extend((0..5).map(|_| tool_turn("print(1)")));
    assert!(nudge.row(&messages, ReminderPoint::Boundary).is_some());
}

#[test]
fn tokens_left_counts_down_from_the_window() {
    let countdown = reminders(only(|toggles| toggles.tokens_left = true), None);
    let messages = vec![user("go"), assistant(&serde_json::json!([]), 400)];
    let row = countdown.row(&messages, ReminderPoint::Boundary).unwrap();
    // 1000 window - (400 input + 10 output).
    assert_eq!(
        row.text,
        "<system-reminder>\n<total_tokens>590 tokens left</total_tokens>\n</system-reminder>"
    );
    assert_eq!(row.kinds, vec![ReminderKind::TokensLeft]);
    // Nothing to count before the first reply.
    assert_eq!(countdown.row(&[user("go")], ReminderPoint::TurnStart), None);
}

#[test]
fn elapsed_time_is_a_boundary_note() {
    let clock = reminders(only(|toggles| toggles.elapsed_time = true), None);
    let row = clock.row(&[user("go")], ReminderPoint::Boundary).unwrap();
    assert_eq!(row.text, system_reminder("Elapsed time so far: 0m 00s"));
    assert_eq!(clock.row(&[user("go")], ReminderPoint::TurnStart), None);
    assert_eq!(
        elapsed_text(std::time::Duration::from_secs(605)),
        "Elapsed time so far: 10m 05s"
    );
}

#[test]
fn changed_files_and_rereads_share_one_wrapper() {
    let dir = tempfile::tempdir().unwrap();
    let changed = dir.path().join("changed.txt");
    let same = dir.path().join("same.txt");
    std::fs::write(&changed, "a\nb\n").unwrap();
    std::fs::write(&same, "s\n").unwrap();
    let files = Arc::new(FileReadTracker::new());
    let both = Reminders::new(
        only(|toggles| {
            toggles.changed_files = true;
            toggles.read_dedup = true;
        }),
        Arc::clone(&files),
        false,
        None,
        0,
    );
    let read = |paths: &[&std::path::Path]| crate::kernel::shared::KernelFileAccess {
        reads: paths.iter().map(|path| path.to_path_buf()).collect(),
        writes: Vec::new(),
    };
    files.queue(read(&[&changed, &same]));
    assert_eq!(both.row(&[], ReminderPoint::Boundary), None);
    std::fs::write(&changed, "a\nB\n").unwrap();
    let file = std::fs::File::options().write(true).open(&changed).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    files.queue(read(&[&same]));
    let row = both.row(&[], ReminderPoint::TurnStart).unwrap();
    assert_eq!(
        row.kinds,
        vec![ReminderKind::ChangedFiles, ReminderKind::Reread]
    );
    assert_eq!(row.text.matches("<system-reminder>").count(), 1);
    assert!(row.text.contains(&format!(
        "Note: {} changed on disk since you last read it.",
        changed.display()
    )));
    assert!(row.text.contains(
        "Here are the relevant changes (shown with line numbers):\n     1\ta\n     2\tB"
    ));
    assert!(row.text.contains(&format!(
        "Note: re-read without a change since your last read: {}.",
        same.display()
    )));
}

#[test]
fn every_toggle_off_sends_nothing() {
    let silent = reminders(only(|_| {}), None);
    let messages: Vec<_> = (0..30).map(|_| tool_turn("print(1)")).collect();
    assert_eq!(silent.row(&messages, ReminderPoint::Boundary), None);
}
