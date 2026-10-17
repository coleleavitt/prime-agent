//! Other products' sessions (`list_external_sessions` rows): their own
//! section after the saved ones, the project filter, and the open that
//! imports before it resumes.

use super::*;

fn external_row(id: &str, cwd: &str, name: Option<&str>, prompt: &str) -> serde_json::Value {
    let mut row = serde_json::json!({
        "source": "claude-code",
        "sourceLabel": "Claude Code",
        "id": format!("claude-code:{id}"),
        "externalId": id,
        "path": format!("/home/u/.claude/projects/-w/{id}.jsonl"),
        "cwd": cwd,
        "created": "2024-01-02T00:00:00.000Z",
        "modified": "2024-01-02T00:00:00.000Z",
        "messageCount": 4,
        "firstMessage": prompt,
        "gitBranch": "main",
    });
    if let Some(name) = name {
        row["name"] = serde_json::json!(name);
    }
    row
}

fn frame_text(mode: &mut AgentsViewMode) -> Vec<String> {
    mode.render_frame(120, 30).0.iter().map(flat).collect()
}

#[test]
fn external_rows_list_in_their_own_section_after_the_saved_ones() {
    let mut mode = mode_with_anchor(None, vec![]);
    mode.saved = vec![saved_catalog_row("/x/s1.jsonl", "s1", "saved chat")];
    mode.apply_external_loaded(vec![
        external_row("cc-1", "/tmp", Some("Parser fix"), "fix the parser"),
        external_row("cc-2", "/tmp", None, "untitled prompt"),
        // A row without a source is not an external one.
        saved_catalog_row("/x/stray.jsonl", "stray", "stray"),
    ]);
    assert_eq!(mode.external.len(), 2);
    let sections: Vec<(Section, String)> = mode
        .rows
        .iter()
        .map(|row| (row.section, row.title.clone()))
        .collect();
    assert_eq!(
        sections,
        [
            (Section::Inactive, "saved chat".to_string()),
            (Section::External, "Parser fix".to_string()),
            (Section::External, "untitled prompt".to_string()),
        ]
    );
    let external = &mode.rows[1].summary;
    assert_eq!(
        external["sessionFile"],
        Value::Null,
        "never resumed as a session file"
    );
    assert_eq!(external["externalSource"], "claude-code");
    assert_eq!(external["externalId"], "cc-1");
    let text = frame_text(&mut mode);
    assert!(
        text.iter()
            .any(|line| line.trim_start().starts_with("Claude Code (2)")),
        "{text:#?}"
    );
}

#[test]
fn the_project_toggle_filters_external_rows_like_saved_ones() {
    let mut mode = mode_with_anchor(None, vec![]);
    mode.apply_external_loaded(vec![
        external_row("here", "/tmp", Some("Here"), "p"),
        external_row("there", "/elsewhere", Some("There"), "p"),
    ]);
    assert_eq!(mode.rows.len(), 2, "all projects by default");
    mode.saved_scope = SavedScope::CurrentProject;
    mode.rebuild_rows();
    let titles: Vec<&str> = mode.rows.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, ["Here"]);
}

#[test]
fn opening_an_external_row_imports_it_then_resumes_the_new_file() {
    let mut mode = mode_with_anchor(None, vec![]);
    mode.apply_external_loaded(vec![external_row(
        "cc-1",
        "/w/repo",
        Some("Parser fix"),
        "p",
    )]);
    mode.selected = 0;
    mode.handle_key("enter");
    assert!(
        mode.opened.is_none(),
        "nothing opens before the import lands"
    );
    let request = mode.pending_import.take().expect("the import is queued");
    assert_eq!(request.source, "claude-code");
    assert_eq!(request.external_id, "cc-1");
    assert_eq!(
        mode.status_text(),
        Some("Importing the Claude Code session...")
    );
    // A second open while the import runs waits for it.
    mode.handle_key("enter");
    assert!(mode.pending_import.is_none());
    assert_eq!(
        mode.status_text(),
        Some("Still importing the Claude Code session...")
    );

    mode.import_result(
        &request,
        Err("Unknown Claude Code session cc-1".to_string()),
    );
    assert!(mode.opened.is_none());
    assert_eq!(
        mode.status_text(),
        Some("Could not import the Claude Code session: Unknown Claude Code session cc-1")
    );

    mode.handle_key("enter");
    let request = mode.pending_import.take().expect("a retry imports again");
    mode.import_result(&request, Ok(PathBuf::from("/sessions/new.jsonl")));
    let opened = mode.opened.as_ref().expect("the imported session opens");
    assert_eq!(
        opened.selection,
        SessionSelection::Resume(PathBuf::from("/sessions/new.jsonl"))
    );
    assert_eq!(opened.cwd.as_deref(), Some("/w/repo"));
    assert!(mode.actions.contains(&"external_session_imported"));
}
