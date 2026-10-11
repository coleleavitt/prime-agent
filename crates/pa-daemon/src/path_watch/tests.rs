//! Port of the TS `rlm-path-watch.test.ts` registry cases plus the Rust
//! port's own shared-watcher and disposal guarantees.
use super::*;

/// A sink feeding a channel the test awaits (observable readiness, never a
/// fixed sleep).
fn channel_sink() -> (
    PathWatchSink,
    tokio::sync::mpsc::UnboundedReceiver<PathWatchEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(move |event| {
            let _ = tx.send(event);
        }),
        rx,
    )
}

async fn next_event(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PathWatchEvent>,
) -> PathWatchEvent {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("a watch event arrives")
        .expect("the sink stays open")
}

/// The change batches until every `expected` path was reported (a
/// platform may split one burst across windows).
async fn changed_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PathWatchEvent>,
    expected: &[String],
) -> Vec<PathWatchChange> {
    let mut changes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    while !expected.iter().all(|path| seen.contains(path)) {
        match next_event(rx).await {
            PathWatchEvent::Changed(change) => {
                seen.extend(change.paths.iter().cloned());
                changes.push(change);
            }
            PathWatchEvent::Failed(failure) => panic!("unexpected failure: {failure:?}"),
        }
    }
    changes
}

#[tokio::test]
async fn registers_a_watch_and_delivers_a_debounced_change_batch() {
    let dir = tempfile::TempDir::new().unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let registry = PathWatchRegistry::default();
    let (sink, mut rx) = channel_sink();
    let watch = registry
        .register(&shared, /*recursive*/ false, sink)
        .unwrap();
    assert_eq!(
        (watch.path.clone(), watch.recursive, watch.status),
        (shared.display().to_string(), false, PathWatchStatus::Active)
    );
    std::fs::write(shared.join("signal-1.txt"), "one").unwrap();
    std::fs::write(shared.join("signal-2.txt"), "two").unwrap();
    let expected = [
        shared.join("signal-1.txt").display().to_string(),
        shared.join("signal-2.txt").display().to_string(),
    ];
    let changes = changed_until(&mut rx, &expected).await;
    assert!(
        changes
            .iter()
            .all(|change| change.watch_id == watch.watch_id
                && change.path == watch.path
                && !change.truncated)
    );
    assert!(format_path_watch_changed(&changes[0]).starts_with(&format!(
        "[watch-path id:{} path:{}]\n\nChanged paths:\n- ",
        watch.watch_id,
        pa_core::session_engine::agent_messaging::sanitize_message_header_value(&watch.path)
    )));
    assert_eq!(
        registry
            .list()
            .iter()
            .map(|info| info.watch_id.clone())
            .collect::<Vec<_>>(),
        vec![watch.watch_id.clone()]
    );
    let cancelled = registry.cancel(&watch.watch_id).unwrap();
    assert_eq!(cancelled.status, PathWatchStatus::Completed);
    // A finished watch answers unchanged; an unknown id errors.
    assert_eq!(registry.cancel(&watch.watch_id).unwrap(), cancelled);
    assert_eq!(
        registry.cancel("watch_unknown").unwrap_err().to_string(),
        "Unknown path watch: watch_unknown"
    );
}

#[tokio::test]
async fn a_missing_path_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("missing");
    let (sink, _rx) = channel_sink();
    let error = PathWatchRegistry::default()
        .register(&missing, /*recursive*/ false, sink)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Watched path does not exist: {}", missing.display())
    );
}

#[tokio::test]
async fn removing_the_watched_file_fails_the_watch() {
    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("gone.txt");
    std::fs::write(&target, "seed").unwrap();
    let registry = PathWatchRegistry::default();
    let (sink, mut rx) = channel_sink();
    let watch = registry
        .register(&target, /*recursive*/ false, sink)
        .unwrap();
    std::fs::remove_file(&target).unwrap();
    let failure = loop {
        match next_event(&mut rx).await {
            PathWatchEvent::Failed(failure) => break failure,
            PathWatchEvent::Changed(_) => {}
        }
    };
    assert_eq!(
        failure,
        PathWatchFailure {
            watch_id: watch.watch_id.clone(),
            path: watch.path.clone(),
            recursive: false,
            error: "Watched path was removed".to_string(),
        }
    );
    assert!(format_path_watch_failed(&failure).starts_with("[watch-path-failed id:"));
    assert_eq!(
        registry
            .get(&watch.watch_id)
            .map(|info| (info.status, info.error)),
        Some((
            PathWatchStatus::Failed,
            Some("Watched path was removed".to_string())
        ))
    );
}

/// Recursive watches see nested changes; two watches on one root share the
/// platform subscription and cancelling one keeps the other live.
#[tokio::test]
async fn recursive_and_shared_roots_route_to_every_covering_watch() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("tree");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    let registry = PathWatchRegistry::default();
    let (deep_sink, mut deep_rx) = channel_sink();
    let (flat_sink, _flat_rx) = channel_sink();
    let deep = registry
        .register(&root, /*recursive*/ true, deep_sink)
        .unwrap();
    let flat = registry
        .register(&root, /*recursive*/ false, flat_sink)
        .unwrap();
    assert!(deep.recursive);
    registry.cancel(&flat.watch_id).unwrap();
    let nested = root.join("nested").join("deep.txt");
    std::fs::write(&nested, "deep").unwrap();
    changed_until(&mut deep_rx, &[nested.display().to_string()]).await;
}

#[tokio::test]
async fn the_active_limit_holds_and_dispose_releases_everything() {
    let dir = tempfile::TempDir::new().unwrap();
    let registry = PathWatchRegistry::default();
    for index in 0..PATH_WATCH_MAX_ACTIVE {
        let path = dir.path().join(format!("f{index}"));
        std::fs::write(&path, "x").unwrap();
        let (sink, _rx) = channel_sink();
        registry.register(&path, /*recursive*/ false, sink).unwrap();
    }
    let (sink, _rx) = channel_sink();
    let error = registry
        .register(dir.path(), /*recursive*/ false, sink)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Too many active path watches: limit is {PATH_WATCH_MAX_ACTIVE}")
    );
    assert_eq!(registry.active_count(), (PATH_WATCH_MAX_ACTIVE, true));
    registry.dispose();
    assert_eq!(
        (registry.active_count(), registry.list()),
        ((0, false), Vec::new())
    );
}

#[test]
fn the_changed_path_list_caps_at_its_byte_budget() {
    let long = "p".repeat(1_000);
    let paths: Vec<String> = (0..40).map(|index| format!("{long}{index:02}")).collect();
    let (kept, truncated) = cap_path_list(&paths);
    // 1_003 bytes per entry: 32 fit in 32 KiB, the 33rd does not.
    assert_eq!((kept.len(), truncated), (32, true));
    assert_eq!(cap_path_list(&paths[..3]), (paths[..3].to_vec(), false));
}

#[test]
fn relative_paths_resolve_against_the_session_cwd() {
    assert_eq!(
        (
            resolve_watch_path("relative", Path::new("/work")),
            resolve_watch_path("/abs/x", Path::new("/work"))
        ),
        (PathBuf::from("/work/relative"), PathBuf::from("/abs/x"))
    );
}

#[test]
fn the_host_response_is_the_ts_wire_shape() {
    let info = PathWatchInfo {
        watch_id: "watch_a1".to_string(),
        path: "/tmp/shared".to_string(),
        recursive: false,
        status: PathWatchStatus::Failed,
        created_at: "2026-09-14T12:00:00.000Z".to_string(),
        error: Some("Watched path was removed".to_string()),
    };
    assert_eq!(
        info.host_response(),
        json!({
            "watch_id": "watch_a1",
            "path": "/tmp/shared",
            "recursive": false,
            "status": "failed",
            "created_at": "2026-09-14T12:00:00.000Z",
            "error": "Watched path was removed",
        })
    );
}
