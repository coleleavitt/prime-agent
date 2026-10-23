use serde_json::json;

use super::*;

fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

fn theme() -> Theme {
    Theme::builtin("prime", crate::theme::ColorMode::TrueColor)
}

fn row_text(line: &Line) -> String {
    line.iter().map(|span| span.content.as_str()).collect()
}

fn rendered(selector: &RewindSelector) -> Vec<String> {
    selector
        .render(&theme(), 100, &kb())
        .iter()
        .map(row_text)
        .collect()
}

/// The daemon's rows: a prompt whose code restore changes two files, one
/// whose restore changes nothing, and one from a session without
/// checkpoints.
fn response() -> serde_json::Value {
    json!({
        "codeRestore": true,
        "checkpoints": [
            {
                "entryId": "e1",
                "text": "first\nprompt",
                "timestamp": 1,
                "code": {
                    "filesChanged": ["/w/src/main.rs", "/w/new.txt"],
                    "insertions": 3,
                    "deletions": 7,
                    "externallyModified": ["/w/hand.txt"],
                    "unrestorable": [{ "path": "/w/big.bin", "reason": "too large" }],
                },
            },
            {
                "entryId": "e2",
                "text": "second prompt",
                "timestamp": 2,
                "code": {
                    "filesChanged": [],
                    "insertions": 0,
                    "deletions": 0,
                    "externallyModified": [],
                    "unrestorable": [],
                },
            },
        ],
    })
}

#[test]
fn the_response_parses_into_checkpoints() {
    let checkpoints = RewindCheckpoint::from_response(&response());
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(
        checkpoints[0].code,
        Some(CodeSummary {
            files_changed: vec!["/w/src/main.rs".to_string(), "/w/new.txt".to_string()],
            insertions: 3,
            deletions: 7,
            externally_modified: vec!["/w/hand.txt".to_string()],
            unrestorable: vec!["/w/big.bin".to_string()],
        })
    );
    let without = RewindCheckpoint::from_response(&json!({
        "checkpoints": [{ "entryId": "e1", "text": "t", "timestamp": 1 }],
    }));
    assert_eq!(without[0].code, None);
}

#[test]
fn the_list_shows_each_prompts_code_summary() {
    let selector = RewindSelector::new(RewindCheckpoint::from_response(&response()));
    let rows = rendered(&selector);
    assert!(rows.iter().any(|row| {
        row.contains("Restore the code and/or conversation to the point before\u{2026}")
    }));
    let first = rows
        .iter()
        .find(|row| row.contains("first prompt"))
        .expect("the multi-line prompt renders on one row");
    assert!(first.contains("2 files changed +3 -7"), "{first}");
    let second = rows
        .iter()
        .find(|row| row.contains("second prompt"))
        .expect("second row");
    assert!(
        second.starts_with("\u{203a} "),
        "the latest prompt is preselected: {second}"
    );
    assert!(second.contains("No code changes"), "{second}");
    assert!(
        rows.iter()
            .any(|row| row.contains("\u{2191}/\u{2193} navigate · Enter continue · Esc cancel"))
    );
}

#[test]
fn enter_asks_how_to_restore_and_confirms_the_choice() {
    let mut selector = RewindSelector::new(RewindCheckpoint::from_response(&response()));
    selector.handle_key(&kb(), "up");
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::None
    );
    let rows = rendered(&selector);
    assert!(
        rows.iter()
            .any(|row| row.contains("Confirm you want to restore to the point before")),
        "{rows:?}"
    );
    for label in [
        "Restore code and conversation",
        "Restore conversation",
        "Restore code",
        "Summarize from here",
        "Never mind",
    ] {
        assert!(
            rows.iter().any(|row| row.contains(label)),
            "{label}: {rows:?}"
        );
    }
    assert!(
        rows.iter()
            .any(|row| row.contains("The code will be restored +3 -7 in main.rs and new.txt."))
    );
    assert!(rows.iter().any(|row| row.contains(
        "Rewinding does not affect files edited manually or via bash."
    )));
    assert!(
        rows.iter()
            .any(|row| row.contains("changed outside the agent since: hand.txt"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Not restorable (no backup kept): big.bin"))
    );
    // The fullest restore is focused first.
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::Rewind {
            entry_id: "e1".to_string(),
            option: RewindOption::Both,
        }
    );
    selector.handle_key(&kb(), "down");
    selector.handle_key(&kb(), "down");
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::Rewind {
            entry_id: "e1".to_string(),
            option: RewindOption::Code,
        }
    );
}

#[test]
fn a_prompt_without_code_changes_offers_the_conversation_only() {
    let mut selector = RewindSelector::new(RewindCheckpoint::from_response(&response()));
    selector.handle_key(&kb(), "enter");
    let rows = rendered(&selector);
    assert!(rows.iter().any(|row| {
        row.contains("Confirm you want to restore the conversation to the point before")
    }));
    assert!(!rows.iter().any(|row| row.contains("Restore code")));
    assert!(
        rows.iter()
            .any(|row| row.contains("The code will be unchanged."))
    );
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::Rewind {
            entry_id: "e2".to_string(),
            option: RewindOption::Conversation,
        }
    );
    selector.handle_key(&kb(), "down");
    let rows = rendered(&selector);
    assert!(
        rows.iter()
            .any(|row| row.contains("Messages after this point will be summarized."))
    );
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::Rewind {
            entry_id: "e2".to_string(),
            option: RewindOption::Summarize,
        }
    );
}

#[test]
fn never_mind_and_escape_step_back_to_the_list_then_close() {
    let mut selector = RewindSelector::new(RewindCheckpoint::from_response(&response()));
    selector.handle_key(&kb(), "enter");
    // Up from the first option wraps to "Never mind".
    selector.handle_key(&kb(), "up");
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::None
    );
    assert!(
        rendered(&selector)
            .iter()
            .any(|row| row.contains("second prompt"))
    );
    selector.handle_key(&kb(), "enter");
    assert_eq!(
        selector.handle_key(&kb(), "escape"),
        RewindSelectorAction::None
    );
    assert_eq!(
        selector.handle_key(&kb(), "escape"),
        RewindSelectorAction::Cancel
    );
}

#[test]
fn an_empty_session_says_there_is_nothing_to_rewind() {
    let mut selector = RewindSelector::new(Vec::new());
    assert!(selector.is_empty());
    assert!(
        rendered(&selector)
            .iter()
            .any(|row| row.contains("Nothing to rewind to yet."))
    );
    assert_eq!(
        selector.handle_key(&kb(), "enter"),
        RewindSelectorAction::Cancel
    );
}

#[test]
fn a_session_without_checkpoints_rewinds_the_conversation() {
    let selector = RewindSelector::new(RewindCheckpoint::from_response(&json!({
        "codeRestore": false,
        "checkpoints": [{ "entryId": "e1", "text": "only", "timestamp": 1 }],
    })));
    let rows = rendered(&selector);
    assert!(
        rows.iter()
            .any(|row| row.contains("Restore the conversation to the point before"))
    );
    let row = rows.iter().find(|row| row.contains("only")).unwrap();
    assert!(
        !row.contains("code"),
        "no code summary without checkpoints: {row}"
    );
}
