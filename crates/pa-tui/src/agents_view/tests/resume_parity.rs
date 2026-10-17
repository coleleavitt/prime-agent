//! Claude Code resume-picker parity: the branch filter (`Ctrl+B`), the Branch column, the
//! session preview (`Ctrl+V`), and the transcript corpus prepared on arrival.

use super::*;

fn branch_row(path: &str, id: &str, name: &str, cwd: &str, branch: &str) -> serde_json::Value {
    let mut row = saved_catalog_row(path, id, name);
    row["cwd"] = serde_json::json!(cwd);
    row["gitBranch"] = serde_json::json!(branch);
    row
}

fn titles(mode: &AgentsViewMode) -> Vec<String> {
    let mut titles: Vec<String> = mode.rows.iter().map(|row| row.title.clone()).collect();
    titles.sort();
    titles
}

#[test]
fn the_branch_filter_keeps_this_repositorys_sessions_on_its_branch() {
    let mut mode = mode_with_anchor(None, Vec::new());
    mode.git_head = Some(git_head::GitHead {
        root: PathBuf::from("/work/repo"),
        branch: Some("topic".to_string()),
    });
    mode.saved = vec![
        branch_row("/s/a.jsonl", "a", "on topic", "/work/repo", "topic"),
        branch_row(
            "/s/b.jsonl",
            "b",
            "in a subdir",
            "/work/repo/crates",
            "topic",
        ),
        branch_row("/s/c.jsonl", "c", "on main", "/work/repo", "main"),
        branch_row("/s/d.jsonl", "d", "other repo", "/work/other", "topic"),
    ];
    mode.rebuild_rows();
    assert_eq!(titles(&mode).len(), 4);
    mode.handle_key("ctrl+b");
    assert_eq!(titles(&mode), vec!["in a subdir", "on topic"]);
    assert_eq!(
        mode.status_text(),
        Some("Saved sessions: branch topic only")
    );
    mode.status = None;
    assert!(flat(&mode.render_hints(200, None)).contains("Ctrl+B branch:topic"));
    mode.handle_key("ctrl+b");
    assert_eq!(titles(&mode).len(), 4);
    assert_eq!(mode.status_text(), Some("Saved sessions: every branch"));
}

#[test]
fn the_branch_filter_needs_a_branch() {
    let mut mode = mode_with_anchor(None, Vec::new());
    mode.git_head = None;
    mode.saved = vec![branch_row("/s/a.jsonl", "a", "one", "/tmp", "main")];
    mode.rebuild_rows();
    mode.handle_key("ctrl+b");
    assert!(!mode.branch_only);
    assert_eq!(
        mode.status_text(),
        Some("Not on a git branch: there is no branch to filter by")
    );
    mode.status = None;
    assert!(!flat(&mode.render_hints(200, None)).contains("branch:"));
}

#[test]
fn the_branch_column_renders_only_with_a_branch() {
    let mut mode = mode_with_anchor(None, Vec::new());
    mode.saved = vec![saved_catalog_row("/s/a.jsonl", "a", "plain")];
    mode.rebuild_rows();
    let (frame, _) = mode.render_frame(160, 30);
    let text: Vec<String> = frame.iter().map(flat).collect();
    assert!(
        !text.iter().any(|line| line.contains("Branch")),
        "{text:#?}"
    );
    mode.saved = vec![branch_row("/s/a.jsonl", "a", "plain", "/tmp", "feature/x")];
    mode.rebuild_rows();
    let (frame, _) = mode.render_frame(160, 30);
    let text: Vec<String> = frame.iter().map(flat).collect();
    assert!(text.iter().any(|line| line.contains("Branch")), "{text:#?}");
    assert!(
        text.iter()
            .any(|line| line.contains("plain") && line.contains("feature/x")),
        "{text:#?}"
    );
}

#[test]
fn streamed_rows_are_searchable_by_their_transcript() {
    let mut mode = mode_with_anchor(None, Vec::new());
    let mut row = saved_catalog_row("/s/a.jsonl", "a", "unrelated title");
    row["allMessagesText"] = serde_json::json!("We chased the  Backoff ceiling");
    mode.buffer_saved_stream_item(row);
    mode.flush_saved_stream();
    mode.query = "backoff ceiling".to_string();
    mode.query_changed(true);
    assert_eq!(titles(&mode), vec!["unrelated title"]);
}

#[test]
fn the_preview_shows_the_conversation_and_opens_on_enter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.jsonl");
    std::fs::write(
        &path,
        [
            r#"{"type":"session","id":"p","timestamp":"t","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"1","message":{"role":"user","content":"why is the build red"}}"#,
            r#"{"type":"message","id":"2","message":{"role":"assistant","content":[{"type":"text","text":"A flaky test."}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let mut mode = mode_with_anchor(None, Vec::new());
    let mut row = saved_catalog_row(&path.to_string_lossy(), "p", "red build");
    row["gitBranch"] = serde_json::json!("main");
    mode.saved = vec![row];
    mode.rebuild_rows();
    mode.selected = mode
        .rows
        .iter()
        .position(|row| row.title == "red build")
        .expect("the row");
    assert!(flat(&mode.render_hints(200, None)).contains("Ctrl+V preview"));
    mode.handle_key("ctrl+v");
    let (frame, _) = mode.render_frame(100, 30);
    let text: Vec<String> = frame.iter().map(flat).collect();
    assert!(
        text.iter()
            .any(|line| line.contains("Preview \u{b7} red build")),
        "{text:#?}"
    );
    assert!(
        text.iter()
            .any(|line| line.contains("why is the build red")),
        "{text:#?}"
    );
    assert!(
        text.iter().any(|line| line.contains("A flaky test.")),
        "{text:#?}"
    );
    assert!(
        text.iter()
            .any(|line| line.contains("3 messages \u{b7} main")),
        "{text:#?}"
    );
    assert!(
        flat(text.last().map(|_| &frame[frame.len() - 1]).unwrap()).contains("Enter resume"),
        "{text:#?}"
    );
    // Any other key closes the preview without acting.
    mode.handle_key("escape");
    assert!(mode.preview.is_none());
    assert!(mode.running, "escape closed the preview, not the view");
    mode.handle_key("ctrl+v");
    mode.handle_key("enter");
    assert!(mode.preview.is_none());
    assert!(
        matches!(
            mode.opened.as_ref().map(|opened| &opened.selection),
            Some(SessionSelection::Resume(opened)) if opened == &path
        ),
        "enter opens the previewed session"
    );
}
