use pa_types::session::{CustomMessageEntry, EntryBase};

use super::*;

/// One detector case: the prompt and its `(word, start, end)` matches.
type KeywordCase = (&'static str, &'static [(&'static str, usize, usize)]);

/// Every row was produced by Claude Code 2.1.296's own detector (`Qp` in
/// chunk qwvy7ma3, run under node): `(prompt, [(word, start, end)])`.
const CC_KEYWORD_TABLE: &[KeywordCase] = &[
    ("ultracode fix the bug", &[("ultracode", 0, 9)]),
    ("please ULTRACODE this", &[("ULTRACODE", 7, 16)]),
    ("/ultracode", &[]),
    (" /ultracode", &[]),
    ("run `ultracode` now", &[]),
    ("say \"ultracode\" please", &[]),
    ("(ultracode) go", &[]),
    ("[ultracode] go", &[]),
    ("<b>ultracode</b>", &[("ultracode", 3, 12)]),
    ("use ultracode/foo", &[]),
    ("path/ultracode", &[]),
    ("pre-ultracode", &[]),
    ("ultracode-style", &[]),
    ("ultracode?", &[]),
    ("is it ultracode.", &[("ultracode", 6, 15)]),
    ("ultracode.md", &[]),
    ("don't ultracode it's fine", &[("ultracode", 6, 15)]),
    ("'ultracode' quoted", &[]),
    ("ultracoder", &[]),
    ("myultracode", &[]),
    ("{ultracode}", &[]),
    ("unclosed (ultracode", &[("ultracode", 10, 19)]),
    ("a [b [ultracode] c", &[]),
    (
        "ultracode ultracode",
        &[("ultracode", 0, 9), ("ultracode", 10, 19)],
    ),
    ("x\\ultracode", &[]),
    ("\u{fc}ber ultracode", &[("ultracode", 5, 14)]),
    ("\u{e9}ultracode", &[("ultracode", 1, 10)]),
    ("ultracode\u{e9}", &[("ultracode", 0, 9)]),
    ("<ultracode>", &[]),
    ("a < ultracode > b", &[("ultracode", 4, 13)]),
    ("ultracode, then tests", &[("ultracode", 0, 9)]),
    ("the users' ultracode run", &[("ultracode", 11, 20)]),
    ("ultracode_x", &[]),
    ("ultracode.", &[("ultracode", 0, 9)]),
    ("ultracode. Next", &[("ultracode", 0, 9)]),
    ("it's 'ultracode", &[("ultracode", 6, 15)]),
    ("Ultracode!", &[("Ultracode", 0, 9)]),
];

#[test]
fn keyword_detection_matches_claude_code() {
    for (prompt, expected) in CC_KEYWORD_TABLE {
        let expected: Vec<KeywordMatch> = expected
            .iter()
            .map(|(word, start, end)| KeywordMatch {
                word: (*word).to_string(),
                start: *start,
                end: *end,
            })
            .collect();
        assert_eq!(
            keyword_matches(prompt, "ultracode"),
            expected,
            "prompt {prompt:?}"
        );
        assert_eq!(has_ultracode_keyword(prompt), !expected.is_empty());
    }
}

fn texts(rows: &[CustomMessage]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| {
            let pa_types::ai::UserContent::Text(text) = &row.content else {
                panic!("reminder rows are text");
            };
            (row.custom_type.clone(), text.clone())
        })
        .collect()
}

fn agent_dir_with(settings: &serde_json::Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("settings.json"), settings.to_string()).expect("write");
    dir
}

#[test]
fn the_keyword_adds_one_hidden_reminder_with_the_size_guideline() {
    let state = UltracodeState::new(None);
    state.restore(None);
    assert!(state.turn_rows("fix the flaky test").is_empty());
    let rows = state.turn_rows("ultracode: fix the flaky test");
    assert_eq!(
        texts(&rows),
        [(
            ULTRACODE_KEYWORD_CUSTOM_TYPE.to_string(),
            format!(
                "<system-reminder>\n{KEYWORD_TEXT}\n\nThis session has the default workflow size guideline: medium \u{2014} keep workflows under 10 agents. This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale. The user can raise or remove it with the workflowSizeGuideline setting.\n</system-reminder>"
            )
        )]
    );
    assert!(!rows[0].display);
    // A steer gets the same row.
    assert_eq!(state.steer_rows("ultracode please").len(), 1);
}

#[test]
fn settings_turn_the_keyword_off_and_set_the_guideline() {
    let off = agent_dir_with(&serde_json::json!({"workflowKeywordTriggerEnabled": false}));
    let state = UltracodeState::new(Some(off.path().to_path_buf()));
    state.restore(None);
    assert!(state.turn_rows("ultracode it").is_empty());

    let large = agent_dir_with(&serde_json::json!({"workflowSizeGuideline": "large"}));
    let settings = UltracodeSettings::load(Some(large.path()));
    assert!(
        settings
            .guideline_text()
            .unwrap()
            .starts_with("This session has the workflow size guideline: large \u{2014} keep workflows under 50 agents.")
    );
    let unrestricted = UltracodeSettings {
        size_guideline: Some(WorkflowSizeGuideline::Unrestricted),
        ..UltracodeSettings::default()
    };
    assert_eq!(unrestricted.guideline_text(), None);
    let state = UltracodeState::new(None);
    state.restore(None);
    assert_eq!(
        keyword_row("ultracode", unrestricted, /*on*/ false).map(|row| texts(&[row])[0].1.clone()),
        Some(format!(
            "<system-reminder>\n{KEYWORD_TEXT}\n</system-reminder>"
        ))
    );
}

#[test]
fn standing_mode_reminds_on_entry_every_ten_turns_and_on_exit() {
    let state = UltracodeState::new(None);
    state.restore(None);
    assert!(!state.is_on());
    assert!(state.set(true));
    assert!(!state.set(true), "already on");
    let entry = texts(&state.turn_rows("hello"));
    assert_eq!(
        entry,
        [(
            ULTRACODE_REMINDER_CUSTOM_TYPE.to_string(),
            format!("<system-reminder>\n{ENTER_TEXT}\n</system-reminder>")
        )]
    );
    // While on, the keyword adds nothing: the mode already opted in.
    for _ in 1..TURNS_BETWEEN_REMINDERS {
        assert!(state.turn_rows("ultracode again").is_empty());
    }
    assert_eq!(
        texts(&state.turn_rows("tenth")),
        [(
            ULTRACODE_REMINDER_CUSTOM_TYPE.to_string(),
            format!("<system-reminder>\n{STILL_ON_TEXT}\n</system-reminder>")
        )]
    );
    assert!(state.steer_rows("ultracode").is_empty());
    assert!(state.set(false));
    assert_eq!(
        texts(&state.turn_rows("after")),
        [(
            ULTRACODE_EXIT_CUSTOM_TYPE.to_string(),
            format!("<system-reminder>\n{EXIT_TEXT}\n</system-reminder>")
        )]
    );
    assert!(state.turn_rows("later").is_empty());
}

#[test]
fn an_unseen_entry_reminder_is_withdrawn_instead_of_announcing_exit() {
    let state = UltracodeState::new(None);
    state.restore(None);
    state.set(true);
    state.set(false);
    assert!(state.turn_rows("nothing to say").is_empty());
}

fn change_entry(enabled: bool) -> FileEntry {
    let row = ultracode_change_row(enabled);
    FileEntry::CustomMessage {
        payload: CustomMessageEntry {
            custom_type: row.custom_type,
            content: row.content,
            details: row.details,
            display: row.display,
            rest: serde_json::Map::default(),
        },
        base: serde_json::from_value::<EntryBase>(serde_json::json!({ "id": "e" })).unwrap(),
    }
}

#[test]
fn restore_takes_the_branch_then_the_setting() {
    let entries = [change_entry(true), change_entry(false), change_entry(true)];
    assert_eq!(ultracode_in_entries(entries.iter()), Some(true));
    assert_eq!(ultracode_in_entries(entries[..2].iter()), Some(false));
    assert_eq!(ultracode_in_entries([].iter()), None);

    let state = UltracodeState::new(None);
    assert!(state.needs_restore());
    state.restore(Some(true));
    assert!(!state.needs_restore());
    assert!(state.is_on());
    assert_eq!(
        state.turn_rows("resumed").len(),
        1,
        "a restored mode announces itself"
    );
    state.restore(Some(false));
    assert!(state.is_on(), "restore runs once");

    let on = agent_dir_with(&serde_json::json!({"ultracode": true}));
    let state = UltracodeState::new(Some(on.path().to_path_buf()));
    state.restore(None);
    assert!(state.is_on());
    let state = UltracodeState::new(Some(on.path().to_path_buf()));
    state.restore(Some(false));
    assert!(
        !state.is_on(),
        "the branch's recorded change wins over the setting"
    );
}

#[test]
fn command_parsing() {
    assert_eq!(
        ["", "on", " off ", "status", "maybe"].map(parse_ultracode_command),
        [
            Ok(UltracodeCommand::On),
            Ok(UltracodeCommand::On),
            Ok(UltracodeCommand::Off),
            Ok(UltracodeCommand::Status),
            Err("Usage: /ultracode [on|off|status]".to_string()),
        ]
    );
}
