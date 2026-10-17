#![cfg(unix)]

use std::path::{Path, PathBuf};

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    history: FileHistory,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    let history = FileHistory::for_artifact_dir(&dir.path().join("artifacts"));
    Fixture {
        _dir: dir,
        root,
        history,
    }
}

/// One agent write the way the edit skill does it: report, write, report.
fn agent_write(history: &FileHistory, path: &Path, content: &str, at: u64) {
    history.before_write(path, at).unwrap();
    std::fs::write(path, content).unwrap();
    history.after_write(path, at).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn restore_returns_each_file_to_its_state_at_the_point() {
    let f = fixture();
    let edited = f.root.join("edited.txt");
    let created = f.root.join("nested/created.txt");
    std::fs::write(&edited, "one\ntwo\n").unwrap();

    // Prompt 1 at t=100: the agent edits a file.
    agent_write(&f.history, &edited, "one\nTWO\n", 110);
    // Prompt 2 at t=200: it edits again and creates a file.
    agent_write(&f.history, &edited, "one\nTWO\nthree\n", 210);
    std::fs::create_dir_all(created.parent().unwrap()).unwrap();
    agent_write(&f.history, &created, "new\n", 220);

    // Back to before prompt 2: the first edit stays, the creation goes.
    let plan = f.history.plan(200).unwrap();
    assert_eq!(plan.files.len(), 2, "{plan:?}");
    assert_eq!(plan.files[0].action, PlannedAction::Restore);
    assert_eq!((plan.files[0].insertions, plan.files[0].deletions), (0, 1));
    assert_eq!(plan.files[1].action, PlannedAction::Delete);
    let report = f.history.restore(200, false, 300).unwrap();
    assert_eq!(report.restored, vec![edited.to_string_lossy().to_string()]);
    assert_eq!(report.deleted, vec![created.to_string_lossy().to_string()]);
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(read(&edited), "one\nTWO\n");
    assert!(!created.exists());

    // The restore was recorded: back to before prompt 1 sees through it.
    let report = f.history.restore(100, false, 400).unwrap();
    assert_eq!(report.restored.len(), 1, "{report:?}");
    assert_eq!(read(&edited), "one\ntwo\n");
    assert!(f.history.plan(100).unwrap().is_empty());

    // And forward again: the state at t=250 (after prompt 2's writes).
    let report = f.history.restore(250, false, 500).unwrap();
    assert_eq!(read(&edited), "one\nTWO\nthree\n", "{report:?}");
    assert_eq!(read(&created), "new\n");
}

#[test]
fn plans_for_several_points_match_their_single_plans() {
    let f = fixture();
    let path = f.root.join("file.txt");
    std::fs::write(&path, "a\n").unwrap();
    agent_write(&f.history, &path, "a\nb\n", 110);
    agent_write(&f.history, &path, "a\nb\nc\n", 210);
    let points = [100, 200, 300];
    let plans = f.history.plans(&points).unwrap();
    let singles: Vec<RestorePlan> = points
        .iter()
        .map(|&point| f.history.plan(point).unwrap())
        .collect();
    assert_eq!(plans, singles);
    assert_eq!((plans[0].insertions(), plans[0].deletions()), (0, 2));
    assert_eq!((plans[1].insertions(), plans[1].deletions()), (0, 1));
    assert!(plans[2].is_empty());
}

#[test]
fn a_file_changed_outside_the_agent_is_skipped_unless_forced() {
    let f = fixture();
    let path = f.root.join("file.txt");
    std::fs::write(&path, "base\n").unwrap();
    agent_write(&f.history, &path, "agent\n", 110);
    // A hand (or bash) edit after the agent's last write.
    std::fs::write(&path, "hand\n").unwrap();

    let plan = f.history.plan(100).unwrap();
    assert!(plan.files[0].externally_modified);
    let report = f.history.restore(100, false, 200).unwrap();
    assert_eq!(
        report.skipped,
        vec![SkippedFile {
            path: path.to_string_lossy().to_string(),
            reason: EXTERNALLY_MODIFIED_REASON.to_string(),
        }]
    );
    assert_eq!(read(&path), "hand\n");

    let report = f.history.restore(100, true, 300).unwrap();
    assert_eq!(report.restored.len(), 1);
    assert_eq!(read(&path), "base\n");
}

#[test]
fn files_untouched_since_the_point_are_not_planned() {
    let f = fixture();
    let path = f.root.join("file.txt");
    std::fs::write(&path, "a\n").unwrap();
    agent_write(&f.history, &path, "b\n", 110);
    assert!(f.history.plan(200).unwrap().is_empty());
    // Restoring to the state the file is already in plans nothing.
    std::fs::write(&path, "a\n").unwrap();
    assert!(f.history.plan(100).unwrap().is_empty());
}

#[test]
fn a_missing_history_plans_and_restores_nothing() {
    let f = fixture();
    assert!(!f.history.exists());
    assert!(f.history.plan(0).unwrap().is_empty());
    assert_eq!(
        f.history.restore(0, false, 1).unwrap(),
        RestoreReport::default()
    );
}

#[test]
fn backups_are_content_addressed_and_private() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let one = f.root.join("one.txt");
    let two = f.root.join("two.txt");
    std::fs::write(&one, "same\n").unwrap();
    std::fs::write(&two, "same\n").unwrap();
    agent_write(&f.history, &one, "x\n", 10);
    agent_write(&f.history, &two, "y\n", 20);
    let blobs: Vec<_> = std::fs::read_dir(f.history.dir().join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    assert_eq!(blobs.len(), 1, "identical content is kept once");
    assert_eq!(
        blobs[0].file_name().to_string_lossy(),
        store::sha256_hex(b"same\n")
    );
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(f.history.dir()), 0o700);
    assert_eq!(mode(&blobs[0].path()), 0o600);
    assert_eq!(mode(&f.history.dir().join(store::LOG_FILE)), 0o600);
}

#[test]
fn a_tampered_backup_is_refused() {
    let f = fixture();
    let path = f.root.join("file.txt");
    std::fs::write(&path, "orig\n").unwrap();
    agent_write(&f.history, &path, "new\n", 10);
    let blob = f
        .history
        .dir()
        .join("blobs")
        .join(store::sha256_hex(b"orig\n"));
    std::fs::write(&blob, "evil\n").unwrap();
    let report = f.history.restore(0, false, 20).unwrap();
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert_eq!(read(&path), "new\n");
}

#[test]
fn a_restore_refuses_a_path_that_became_a_symlink() {
    let f = fixture();
    let path = f.root.join("file.txt");
    let elsewhere = f.root.join("elsewhere.txt");
    std::fs::write(&path, "orig\n").unwrap();
    agent_write(&f.history, &path, "new\n", 10);
    std::fs::write(&elsewhere, "new\n").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let report = f.history.restore(0, true, 20).unwrap();
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert_eq!(read(&elsewhere), "new\n", "the link target is untouched");
}

#[test]
fn oversized_files_are_recorded_but_not_restorable() {
    let f = fixture();
    let path = f.root.join("big.bin");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_BACKUP_BYTES + 1).unwrap();
    drop(file);
    agent_write(&f.history, &path, "small\n", 10);
    let plan = f.history.plan(0).unwrap();
    assert!(
        matches!(plan.files[0].action, PlannedAction::Unrestorable { .. }),
        "{plan:?}"
    );
    assert!(
        std::fs::read_dir(f.history.dir().join("blobs"))
            .unwrap()
            .next()
            .is_none(),
        "nothing past the cap is kept"
    );
}

#[test]
fn relative_paths_and_directories_are_refused() {
    let f = fixture();
    assert!(
        f.history
            .before_write(Path::new("relative.txt"), 1)
            .is_err()
    );
    assert!(f.history.before_write(&f.root, 1).is_err());
}

#[test]
fn the_disable_switch_and_setting_turn_checkpointing_off() {
    // Read-only: the process env is shared, so only the setting arm and
    // the env-free default are asserted here.
    if std::env::var_os(DISABLE_ENV).is_none() {
        assert!(checkpointing_enabled(None));
        assert!(checkpointing_enabled(Some(true)));
    }
    assert!(!checkpointing_enabled(Some(false)));
}

#[tokio::test]
async fn the_host_requests_record_writes() {
    let f = fixture();
    let path = f.root.join("file.txt");
    std::fs::write(&path, "before\n").unwrap();
    let mut handlers = crate::kernel::shared::HostRequestHandlers::new();
    register_host_handlers(&mut handlers, &f.history);
    let call = |request: &str, path: &Path| {
        let handler = handlers.get(request).cloned().expect("registered");
        handler(crate::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "type": request, "path": path.to_string_lossy() }),
        })
    };
    call(BEFORE_WRITE_REQUEST, &path).await.unwrap();
    std::fs::write(&path, "after\n").unwrap();
    call(AFTER_WRITE_REQUEST, &path).await.unwrap();
    assert!(call(BEFORE_WRITE_REQUEST, Path::new("rel")).await.is_err());
    f.history.restore(0, false, now_ms()).unwrap();
    assert_eq!(read(&path), "before\n");
}
