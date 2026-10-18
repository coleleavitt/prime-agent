//! Claude Code's resume-picker scoping: the picker opens on the current project (the repository's
//! worktree holding the launch directory), `Ctrl+A` shows every project, `Ctrl+W` every worktree
//! of the repository, an empty project says so instead of widening, and `--from-pr` keeps the
//! sessions linked to a pull request.

use std::path::Path;

use super::*;

fn type_query(mode: &mut AgentsViewMode, text: &str) {
    for ch in text.chars() {
        mode.handle_key(&ch.to_string());
    }
}

fn picker(cwd: &str, resume_picker: bool, pr_filter: Option<PrFilter>) -> AgentsViewMode {
    AgentsViewMode::new(AgentsViewOptions {
        socket_path: PathBuf::from("/tmp/agents-view-test.sock"),
        cwd: PathBuf::from(cwd),
        session_dir: None,
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: None,
        scope: None,
        query: None,
        expanded_ancestors: Vec::new(),
        selected_row_identity: None,
        selected_key: None,
        status_message: None,
        keybindings: crate::keybindings::KeybindingsManager::new(),
        show_hardware_cursor: false,
        incident_notice_state: None,
        create_config: serde_json::json!({}),
        resume_picker,
        pr_filter,
    })
}

fn row_in(id: &str, cwd: &str) -> serde_json::Value {
    let mut row = saved_catalog_row(&format!("/s/{id}.jsonl"), id, id);
    row["cwd"] = serde_json::json!(cwd);
    row
}

fn listed(mode: &AgentsViewMode) -> Vec<String> {
    let mut ids: Vec<String> = mode
        .rows
        .iter()
        .filter_map(|row| row.summary.get("sessionId").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    ids.sort();
    ids
}

/// A repository at `/work/repo` with a linked worktree at `/work/repo-topic`, the view
/// launched in `/work/repo/src`.
fn with_two_worktrees(mut mode: AgentsViewMode) -> AgentsViewMode {
    mode.project = project_scope::ProjectWorktrees::new(
        vec![
            PathBuf::from("/work/repo"),
            PathBuf::from("/work/repo-topic"),
        ],
        Path::new("/work/repo/src"),
    );
    mode.saved = vec![
        row_in("root", "/work/repo"),
        row_in("subdir", "/work/repo/docs"),
        row_in("topic", "/work/repo-topic"),
        row_in("elsewhere", "/work/other"),
    ];
    mode.rebuild_rows();
    mode
}

#[test]
fn the_resume_picker_opens_on_the_current_project_and_the_plain_view_on_every_project() {
    let mut resume = picker("/work/here", true, None);
    let mut plain = picker("/work/here", false, None);
    for mode in [&mut resume, &mut plain] {
        mode.saved = vec![row_in("here", "/work/here"), row_in("there", "/work/there")];
        mode.rebuild_rows();
    }
    assert_eq!(resume.saved_scope, SavedScope::CurrentProject);
    assert_eq!(listed(&resume), vec!["here"]);
    assert_eq!(plain.saved_scope, SavedScope::AllProjects);
    assert_eq!(listed(&plain), vec!["here", "there"]);
    assert!(flat(&resume.render_hints(200, None)).contains("Ctrl+A saved:project"));
}

#[test]
fn ctrl_a_shows_every_project_and_ctrl_f_stays_an_alias() {
    let mut mode = picker("/work/here", true, None);
    mode.saved = vec![row_in("here", "/work/here"), row_in("there", "/work/there")];
    mode.rebuild_rows();
    mode.handle_key("ctrl+a");
    assert_eq!(listed(&mode), vec!["here", "there"]);
    assert_eq!(mode.status_text(), Some("Saved sessions: all projects"));
    mode.handle_key("ctrl+f");
    assert_eq!(listed(&mode), vec!["here"]);
    assert_eq!(mode.status_text(), Some("Saved sessions: current project"));
    // With a search typed, ctrl+a is the editor's and leaves the scope alone.
    type_query(&mut mode, "her");
    mode.handle_key("ctrl+a");
    assert_eq!(mode.saved_scope, SavedScope::CurrentProject);
}

#[test]
fn several_worktrees_group_by_worktree_and_ctrl_w_shows_them_all() {
    let mut mode = with_two_worktrees(picker("/work/repo/src", true, None));
    // The launch directory's worktree, its root and subdirectories included.
    assert_eq!(listed(&mode), vec!["root", "subdir"]);
    mode.status = None;
    assert!(flat(&mode.render_hints(200, None)).contains("Ctrl+W worktree:current"));
    mode.handle_key("ctrl+w");
    assert_eq!(listed(&mode), vec!["root", "subdir", "topic"]);
    assert_eq!(mode.status_text(), Some("Saved sessions: every worktree"));
    mode.status = None;
    assert!(flat(&mode.render_hints(200, None)).contains("Ctrl+W worktree:all"));
    mode.handle_key("ctrl+w");
    assert_eq!(listed(&mode), vec!["root", "subdir"]);
    // Every project still wins over the worktree filter.
    mode.handle_key("ctrl+a");
    assert_eq!(listed(&mode), vec!["elsewhere", "root", "subdir", "topic"]);
}

#[test]
fn one_worktree_has_no_worktree_filter_and_ctrl_w_stays_the_word_delete() {
    let mut mode = picker("/work/here", true, None);
    mode.project = project_scope::ProjectWorktrees::new(
        vec![PathBuf::from("/work/here")],
        Path::new("/work/here"),
    );
    mode.saved = vec![
        row_in("here", "/work/here"),
        row_in("subdir", "/work/here/src"),
        row_in("managed", "/work/here/.prime/worktrees/gone"),
    ];
    mode.rebuild_rows();
    // Claude Code lists the launch directory's own sessions and its removed managed worktrees'.
    assert_eq!(listed(&mode), vec!["here", "managed"]);
    assert!(!flat(&mode.render_hints(200, None)).contains("worktree:"));
    mode.handle_key("ctrl+w");
    assert!(!mode.all_worktrees);
    type_query(&mut mode, "gateway worker");
    mode.handle_key("ctrl+w");
    assert_eq!(mode.query, "gateway ");
}

#[test]
fn an_empty_project_says_so_and_offers_every_project() {
    let mut mode = picker("/work/empty", true, None);
    mode.saved = vec![row_in("there", "/work/there")];
    mode.rebuild_rows();
    assert!(mode.rows.is_empty(), "the picker does not widen on its own");
    let text: String = mode.render_list(120, 10, 0).iter().map(flat).collect();
    assert_eq!(
        text,
        "No conversations found in this project. Ctrl+A to show all projects."
    );
    mode.handle_key("ctrl+a");
    assert_eq!(listed(&mode), vec!["there"]);
}

#[test]
fn live_rows_are_never_scoped() {
    let mut mode = picker("/work/empty", true, None);
    let mut summary = parent_summary("live");
    summary["cwd"] = serde_json::json!("/work/there");
    mode.roster = vec![roster_entry("live", "idle", &summary)];
    mode.saved = vec![row_in("saved", "/work/there")];
    mode.rebuild_rows();
    assert_eq!(listed(&mode), vec!["live"]);
}

#[test]
fn from_pr_keeps_the_sessions_linked_to_a_pull_request() {
    let linked = |id: &str, number: u64| {
        let mut row = row_in(id, "/work/here");
        row["prNumber"] = serde_json::json!(number);
        row["prRepository"] = serde_json::json!("acme/app");
        row
    };
    let rows = vec![
        linked("a", 7),
        linked("b", 9),
        row_in("plain", "/work/here"),
    ];
    let mut any = picker("/work/here", true, Some(PrFilter::Any));
    any.saved = rows.clone();
    any.rebuild_rows();
    assert_eq!(listed(&any), vec!["a", "b"]);
    let mut seven = picker("/work/here", true, Some(PrFilter::Number(7)));
    seven.saved = rows;
    seven.rebuild_rows();
    assert_eq!(listed(&seven), vec!["a"]);
}

#[test]
fn pr_filter_values_parse_like_claude_codes() {
    assert_eq!(PrFilter::parse(None), Some(PrFilter::Any));
    assert_eq!(PrFilter::parse(Some("")), Some(PrFilter::Any));
    assert_eq!(PrFilter::parse(Some("42")), Some(PrFilter::Number(42)));
    assert_eq!(PrFilter::parse(Some("0")), None);
    assert_eq!(
        PrFilter::parse(Some("https://github.com/acme/app/pull/42")),
        Some(PrFilter::Number(42))
    );
    assert_eq!(
        PrFilter::parse(Some("https://gitlab.com/acme/app/-/merge_requests/5")),
        Some(PrFilter::Number(5))
    );
    assert_eq!(
        PrFilter::parse(Some("https://bitbucket.org/acme/app/pull-requests/3/diff")),
        Some(PrFilter::Number(3))
    );
    assert_eq!(PrFilter::parse(Some("login bug")), None);
    assert_eq!(PrFilter::parse(Some("pull/4")), None);
}
