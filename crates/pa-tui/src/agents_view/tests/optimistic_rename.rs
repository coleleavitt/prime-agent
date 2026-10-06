//! Optimistic renames (upstream #2099): the row shows the new name the moment the rename is
//! submitted and input stays live while the daemon's write runs; truth confirms the overlay, a
//! failure reverts it, and one writer per session lets the newest name land last.

use super::*;

use super::super::rename::RenameTarget;

fn shown_name(mode: &AgentsViewMode, session_id: &str) -> Option<String> {
    mode.rows
        .iter()
        .find(|row| row.summary.get("sessionId").and_then(Value::as_str) == Some(session_id))
        .and_then(|row| row.summary.get("sessionName"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn submit_rename(mode: &mut AgentsViewMode, name: &str) {
    mode.handle_key("ctrl+r");
    mode.handle_key("ctrl+u");
    for ch in name.chars() {
        mode.handle_key(ch.to_string().as_str());
    }
    mode.handle_key("enter");
}

fn live_rename(name: &str) -> Rename {
    Rename {
        target: RenameTarget::Live {
            active_session_id: "p-live".to_string(),
        },
        name: name.to_string(),
        session_id: Some("p".to_string()),
    }
}

#[test]
fn the_new_name_shows_before_the_write_lands_and_truth_confirms_it() {
    let mut mode = mode_with_parent_and_child();
    submit_rename(&mut mode, "fresh");

    assert_eq!(mode.pending_rename.take(), Some(live_rename("fresh")));
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("fresh"));
    assert_eq!(mode.status_text(), Some("Renaming to fresh..."));
    assert!(
        matches!(mode.composer, Composer::Search),
        "input stays live"
    );

    mode.rename_result(live_rename("fresh"), Ok(()));
    assert_eq!(mode.status_text(), Some("Renamed to fresh"));
    assert_eq!(
        shown_name(&mode, "p").as_deref(),
        Some("fresh"),
        "the overlay holds until the roster carries the name"
    );
    mode.roster[0]["summary"]["sessionName"] = serde_json::json!("fresh");
    mode.rebuild_rows();
    assert!(
        mode.pending_renames.is_empty(),
        "truth confirmed the rename"
    );
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("fresh"));
}

#[test]
fn a_failed_rename_reverts_to_the_daemon_name() {
    let mut mode = mode_with_parent_and_child();
    submit_rename(&mut mode, "taken");
    let rename = mode.pending_rename.take().expect("dispatched");
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("taken"));

    mode.rename_result(rename, Err("name already in use".to_string()));

    assert_eq!(
        mode.status_text(),
        Some("Failed to rename agent: name already in use")
    );
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("p name"));
    assert!(mode.pending_renames.is_empty());
    assert_eq!(mode.pending_rename, None);
}

#[test]
fn one_writer_per_session_lets_the_newest_name_land_last() {
    let mut mode = mode_with_parent_and_child();
    submit_rename(&mut mode, "one");
    let first = mode
        .pending_rename
        .take()
        .expect("the first write dispatches");
    submit_rename(&mut mode, "two");
    assert_eq!(
        mode.pending_rename, None,
        "no second write while the first is in flight"
    );
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("two"));

    // The first write landing must not flip the row back, and it hands the newest name
    // to the follow-up write.
    mode.rename_result(first, Ok(()));
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("two"));
    let second = mode.pending_rename.take();
    assert_eq!(second, Some(live_rename("two")));

    // A roster push still carrying the first name confirms nothing while the newest write runs.
    mode.roster[0]["summary"]["sessionName"] = serde_json::json!("one");
    mode.rebuild_rows();
    assert_eq!(shown_name(&mode, "p").as_deref(), Some("two"));

    mode.rename_result(second.expect("the follow-up write"), Ok(()));
    assert_eq!(mode.pending_rename, None);
    assert_eq!(mode.status_text(), Some("Renamed to two"));
}

#[test]
fn a_saved_session_confirms_on_its_catalog_row() {
    let mut mode = mode_with_parent_and_child();
    mode.saved = vec![saved_catalog_row("/x/old.jsonl", "old", "old name")];
    mode.rebuild_rows();
    mode.request_rename(
        RenameTarget::Saved {
            session_path: "/x/old.jsonl".to_string(),
        },
        Some("old".to_string()),
        "renamed".to_string(),
    );
    let rename = mode.pending_rename.take().expect("dispatched");
    assert_eq!(shown_name(&mode, "old").as_deref(), Some("renamed"));

    mode.rename_result(rename, Ok(()));

    assert!(
        mode.pending_renames.is_empty(),
        "the patched catalog row confirms"
    );
    assert_eq!(shown_name(&mode, "old").as_deref(), Some("renamed"));
}
