use super::*;

#[test]
fn a_rewind_request_needs_an_entry_and_something_to_restore() {
    assert!(RewindRequest::parse(&json!({ "restoreCode": true })).is_err());
    assert!(RewindRequest::parse(&json!({ "entryId": "e1" })).is_err());
    assert_eq!(
        RewindRequest::parse(&json!({
            "entryId": "e1",
            "restoreCode": true,
            "summarize": true,
            "customInstructions": "keep the API notes",
            "force": true,
        })),
        Ok(RewindRequest {
            entry_id: "e1".to_string(),
            restore_code: true,
            restore_conversation: false,
            summarize: true,
            custom_instructions: Some("keep the API notes".to_string()),
            force: true,
        })
    );
}

#[cfg(unix)]
#[test]
fn the_picker_rows_carry_each_prompts_code_summary() {
    let dir = tempfile::tempdir().unwrap();
    let history = FileHistory::for_artifact_dir(&dir.path().join("artifacts"));
    let edited = dir.path().join("edited.txt");
    let created = dir.path().join("created.txt");
    let touched = dir.path().join("touched.txt");
    std::fs::write(&edited, "a\n").unwrap();
    std::fs::write(&touched, "t\n").unwrap();
    let agent_write = |path: &std::path::Path, content: &str, at: u64| {
        history.before_write(path, at).unwrap();
        std::fs::write(path, content).unwrap();
        history.after_write(path, at).unwrap();
    };
    agent_write(&edited, "a\nb\n", 110);
    agent_write(&created, "new\n", 120);
    agent_write(&touched, "t2\n", 130);
    // A hand edit after the agent's: left alone by a restore.
    std::fs::write(&touched, "hand\n").unwrap();

    let prompts = vec![
        BranchUserPrompt {
            entry_id: "p1".to_string(),
            text: "first".to_string(),
            timestamp_ms: 100,
        },
        BranchUserPrompt {
            entry_id: "p2".to_string(),
            text: "second".to_string(),
            timestamp_ms: 200,
        },
    ];
    let plans = history.plans(&[100, 200]).unwrap();
    let rows = checkpoints_json(&prompts, Some(&plans));
    let path = |path: &std::path::Path| json!(path.to_string_lossy());
    assert_eq!(
        rows[0],
        json!({
            "entryId": "p1",
            "text": "first",
            "timestamp": 100,
            "code": {
                "filesChanged": [path(&edited), path(&created)],
                "insertions": 0,
                "deletions": 2,
                "externallyModified": [path(&touched)],
                "unrestorable": [],
            },
        })
    );
    assert_eq!(
        rows[1]["code"],
        json!({
            "filesChanged": [],
            "insertions": 0,
            "deletions": 0,
            "externallyModified": [],
            "unrestorable": [],
        })
    );
    // Without checkpoints the rows carry no code summary at all.
    assert!(checkpoints_json(&prompts, None)[0].get("code").is_none());
}

#[test]
fn a_restore_report_lists_what_happened_to_each_file() {
    let report = RestoreReport {
        restored: vec!["/w/a".to_string()],
        deleted: vec!["/w/b".to_string()],
        skipped: vec![pa_core::file_history::SkippedFile {
            path: "/w/c".to_string(),
            reason: pa_core::file_history::EXTERNALLY_MODIFIED_REASON.to_string(),
        }],
    };
    assert_eq!(
        report_json(&report),
        json!({
            "restored": ["/w/a"],
            "deleted": ["/w/b"],
            "skipped": [{
                "path": "/w/c",
                "reason": pa_core::file_history::EXTERNALLY_MODIFIED_REASON,
            }],
        })
    );
}
