//! Port of the TS `agent-session-cwd.test.ts` and `session-manager-cwd.test.ts`
//! cases over the worker.
use super::*;
use crate::worker::{Worker, WorkerConfig};
use std::sync::Arc;

fn worker_in(dir: &Path) -> Arc<Worker> {
    Arc::new(Worker::new(
        WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "cwd-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ok"] })),
            decision_child: false,
        },
        None,
    ))
}

async fn created(dir: &Path, cwd: &Path, extra: Value) -> Arc<Worker> {
    let worker = worker_in(dir);
    let mut payload = json!({ "cwd": cwd.display().to_string(), "name": "cwd" });
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        payload.extend(extra.clone());
    }
    let response = worker.dispatch("create", &payload).await;
    assert!(response.success, "create failed: {response:?}");
    worker
}

#[test]
fn input_resolves_against_the_current_cwd() {
    let current = Path::new("/work/project");
    assert_eq!(
        (
            resolve_cwd_input("child", current).unwrap(),
            resolve_cwd_input("../other/./x", current).unwrap(),
            resolve_cwd_input("/abs", current).unwrap(),
        ),
        (
            PathBuf::from("/work/project/child"),
            PathBuf::from("/work/other/x"),
            PathBuf::from("/abs"),
        )
    );
}

/// TS "resolves a relative /cwd against the session cwd and retargets its
/// owners", plus the queued `[cwd-changed]` notice and the branch record.
#[tokio::test]
async fn a_relative_cwd_retargets_the_session_and_queues_the_notice() {
    let dir = tempfile::TempDir::new().unwrap();
    let child = dir.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let worker = created(dir.path(), dir.path(), json!({})).await;
    let response = worker
        .dispatch(
            "set_cwd",
            &json!({ "activeSessionId": "cwd-session", "cwd": "child" }),
        )
        .await;
    assert!(response.success, "set_cwd failed: {response:?}");
    let core = worker.core.lock().unwrap();
    let notices: Vec<String> = core
        .pending_next_turn
        .iter()
        .filter(|row| is_cwd_notice(row))
        .filter_map(|row| row["content"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        (
            response.data.as_ref().map(|data| data["cwd"].clone()),
            core.cwd.clone(),
            notices,
            core.store.as_ref().and_then(branch_cwd),
        ),
        (
            Some(json!(child.display().to_string())),
            child.display().to_string(),
            vec![cwd_changed_notice(
                &dir.path().display().to_string(),
                &child.display().to_string()
            )],
            Some(child.display().to_string()),
        )
    );
}

/// TS "rejects a missing or non-directory path without changing state".
#[tokio::test]
async fn a_file_or_missing_path_is_refused_without_changing_state() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("file.txt"), "x").unwrap();
    let worker = created(dir.path(), dir.path(), json!({})).await;
    let mut errors = Vec::new();
    for input in ["file.txt", "nope"] {
        let response = worker
            .dispatch(
                "set_cwd",
                &json!({ "activeSessionId": "cwd-session", "cwd": input }),
            )
            .await;
        errors.push(response.error.unwrap_or_default());
    }
    let core = worker.core.lock().unwrap();
    assert_eq!(
        (errors, core.cwd.clone(), core.pending_next_turn.len()),
        (
            vec![
                format!("Not a directory: {}", dir.path().join("file.txt").display()),
                format!("Not a directory: {}", dir.path().join("nope").display()),
            ],
            dir.path().display().to_string(),
            0,
        )
    );
}

#[tokio::test]
async fn a_running_turn_refuses_the_change() {
    let dir = tempfile::TempDir::new().unwrap();
    let worker = created(dir.path(), dir.path(), json!({})).await;
    worker.core.lock().unwrap().busy = true;
    let response = worker
        .dispatch(
            "set_cwd",
            &json!({ "activeSessionId": "cwd-session", "cwd": "." }),
        )
        .await;
    worker.core.lock().unwrap().busy = false;
    assert_eq!(response.error.as_deref(), Some(CWD_BUSY_ERROR));
}

/// TS "resumes a session in its persisted /cwd directory", the header
/// fallback when the created-in directory is gone, and the override pin.
#[tokio::test]
async fn a_resume_follows_the_recorded_cwd_unless_pinned() {
    let dir = tempfile::TempDir::new().unwrap();
    let header = dir.path().join("header");
    let child = dir.path().join("child");
    std::fs::create_dir(&header).unwrap();
    std::fs::create_dir(&child).unwrap();
    // One level down: a session file implies its artifacts at `<dir>/../session-artifacts`,
    // which must land in the test dir, not the shared temp dir.
    std::fs::create_dir(dir.path().join("sessions")).unwrap();
    let path = dir.path().join("sessions").join("session.jsonl");
    let first = created(
        &dir.path().join("w1"),
        &header,
        json!({ "sessionPath": path.display().to_string() }),
    )
    .await;
    let moved = first
        .dispatch(
            "set_cwd",
            &json!({ "activeSessionId": "cwd-session", "cwd": "../child" }),
        )
        .await;
    assert!(moved.success, "set_cwd failed: {moved:?}");
    drop(first);
    std::fs::remove_dir(&header).unwrap();
    let resumed = created(
        &dir.path().join("w2"),
        &header,
        json!({ "sessionPath": path.display().to_string() }),
    )
    .await;
    let resumed_cwd = resumed.core.lock().unwrap().cwd.clone();
    drop(resumed);
    let pinned = created(
        &dir.path().join("w3"),
        dir.path(),
        json!({ "sessionPath": path.display().to_string(), "cwdOverride": true }),
    )
    .await;
    let pinned_core = pinned.core.lock().unwrap();
    assert_eq!(
        (
            resumed_cwd,
            pinned_core.cwd.clone(),
            pinned_core.cwd_override
        ),
        (
            child.display().to_string(),
            dir.path().display().to_string(),
            true,
        )
    );
}

/// TS "returns to the branch's directory when navigating before the /cwd
/// entry": a tree move recomputes the cwd from the target branch and drops
/// the queued notice.
#[tokio::test]
async fn a_tree_move_before_the_entry_returns_to_the_branch_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let child = dir.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let worker = created(dir.path(), dir.path(), json!({})).await;
    let turn = worker
        .dispatch(
            "prompt_and_wait",
            &json!({ "activeSessionId": "cwd-session", "message": "hi" }),
        )
        .await;
    assert!(turn.success, "turn failed: {turn:?}");
    let before = {
        let core = worker.core.lock().unwrap();
        let store = core.store.as_ref().unwrap();
        store
            .branch()
            .iter()
            .rev()
            .find(|entry| {
                entry.type_ == "message" && entry.fields["message"]["role"] == json!("user")
            })
            .map(|entry| entry.id.clone())
            .expect("the user message entry")
    };
    let moved = worker
        .dispatch(
            "set_cwd",
            &json!({ "activeSessionId": "cwd-session", "cwd": "child" }),
        )
        .await;
    assert!(moved.success, "set_cwd failed: {moved:?}");
    let navigated = worker
        .dispatch(
            "navigate_tree",
            &json!({ "activeSessionId": "cwd-session", "targetId": before }),
        )
        .await;
    assert!(navigated.success, "navigate failed: {navigated:?}");
    let core = worker.core.lock().unwrap();
    assert_eq!(
        (
            core.cwd.clone(),
            core.pending_next_turn.iter().any(is_cwd_notice)
        ),
        (dir.path().display().to_string(), false)
    );
}
