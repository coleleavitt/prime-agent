//! The 1 s tool-path deadline against a git whose `status` hangs. Its own
//! test binary with a single test, because it swaps `PATH` process-wide.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use pa_recall::{MarkOutcome, read_recall_skip, recall_skip_path};
use support::*;

/// A directory whose `git` hangs on `status` and passes everything else to
/// the real git.
fn slow_status_git_dir(temp: &TempDirs) -> std::path::PathBuf {
    let output = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(output.stdout).unwrap().trim().to_string();
    let bin = temp.dir("prime-agent-recall-slow-git-");
    let script = bin.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"status\" ]; then exec sleep 5; fi\ndone\nexec '{real_git}' \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn set_path(path: &str) {
    // Single-test binary: nothing else reads the environment concurrently.
    std::env::set_var("PATH", path);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one scenario across the 60 s window, step by step
async fn a_missed_deadline_stays_in_memory_for_60_s_for_this_process_only() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    let original_at = pa_recall::read_recall_mark(&root(&repo), &agent)
        .and_then(|mark| mark.claims.first().map(|claim| claim.at.clone()));
    let tsgo_at = |mark: &pa_recall::RecallMarkFile| {
        mark.claims
            .iter()
            .find(|claim| claim.command == TSGO_CLAIM)
            .map(|claim| claim.at.clone())
    };
    let fast_path = std::env::var("PATH").unwrap();
    let slow_path = format!("{}:{fast_path}", slow_status_git_dir(&temp).display());

    let offset = Arc::new(AtomicI64::new(0));
    let clock_offset = Arc::clone(&offset);
    let slow = Process::with(ProcessOptions {
        clock: Some(Arc::new(move || {
            pa_recall_now() + clock_offset.load(Ordering::SeqCst)
        })),
        ..ProcessOptions::default()
    });

    // The witness misses its deadline: no block, and nothing on disk.
    set_path(&slow_path);
    let started = Instant::now();
    assert_eq!(
        slow.tool_result(&context(&repo, &agent, "deadline-a"), "call-1")
            .await,
        None
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(
        read_recall_skip(&root(&repo), &agent, pa_recall_now()),
        None
    );
    assert!(!recall_skip_path(&root(&repo), &agent).exists());

    // git is fast again, but this process still leaves the tool path
    // alone; its marks still run.
    set_path(&fast_path);
    let ctx = context(&repo, &agent, "deadline-b");
    let held = Instant::now();
    assert_eq!(slow.tool_result(&ctx, "call-1").await, None);
    assert!(held.elapsed() < Duration::from_millis(500));
    assert!(matches!(slow.agent_end(&ctx), MarkOutcome::Written { .. }));

    // A build cell's digest is skipped too, so no claim is recorded.
    let cell = format!("await bash({TSGO_CLAIM:?})");
    let facts = serde_json::json!({ "bashCommands": [{ "command": TSGO_CLAIM, "exitCode": 0 }] });
    slow.tool_call(&ctx, "cell-1", &cell).await;
    slow.tool_result_with(&ctx, "cell-1", "ipython", facts.clone(), 1)
        .await;
    match slow.agent_end(&ctx) {
        MarkOutcome::Written { mark, .. } => assert_eq!(tsgo_at(&mark), original_at),
        MarkOutcome::Skipped { reason, .. } => panic!("mark skipped: {reason:?}"),
    }

    // Another process sharing the agent dir does not share the miss.
    let next = Process::new()
        .tool_result(&context(&repo, &agent, "deadline-c"), "call-1")
        .await;
    assert!(next.is_some_and(|block| block.contains("<workspace_recall>")));

    offset.store(59_000, Ordering::SeqCst);
    assert_eq!(
        slow.tool_result(&context(&repo, &agent, "deadline-d"), "call-1")
            .await,
        None
    );
    offset.store(61_000, Ordering::SeqCst);
    let expired = slow
        .tool_result(&context(&repo, &agent, "deadline-e"), "call-1")
        .await;
    assert!(expired.is_some_and(|block| block.contains("<workspace_recall>")));

    // A build cell whose own digest misses the deadline holds the repo the same way.
    let cell_side = Process::with(ProcessOptions {
        clock: Some({
            let offset = Arc::clone(&offset);
            Arc::new(move || pa_recall_now() + offset.load(Ordering::SeqCst))
        }),
        ..ProcessOptions::default()
    });
    offset.store(0, Ordering::SeqCst);
    let cell_ctx = context(&repo, &agent, "cell-deadline");
    set_path(&slow_path);
    let started = Instant::now();
    cell_side.tool_call(&cell_ctx, "cell-2", &cell).await;
    assert!(started.elapsed() >= Duration::from_millis(900));
    set_path(&fast_path);
    assert!(!recall_skip_path(&root(&repo), &agent).exists());
    // Within the window: the next build cell takes no digest, so its
    // command is no claim.
    offset.store(59_000, Ordering::SeqCst);
    cell_side.tool_call(&cell_ctx, "cell-3", &cell).await;
    cell_side
        .tool_result_with(&cell_ctx, "cell-3", "ipython", facts.clone(), 1)
        .await;
    let held_mark = match cell_side.agent_end(&cell_ctx) {
        MarkOutcome::Written { mark, .. } => mark,
        MarkOutcome::Skipped { reason, .. } => panic!("mark skipped: {reason:?}"),
    };
    // Past it: the digest runs again and the claim is recorded.
    offset.store(61_000, Ordering::SeqCst);
    cell_side.tool_call(&cell_ctx, "cell-4", &cell).await;
    cell_side
        .tool_result_with(&cell_ctx, "cell-4", "ipython", facts, 1)
        .await;
    let retried_mark = match cell_side.agent_end(&cell_ctx) {
        MarkOutcome::Written { mark, .. } => mark,
        MarkOutcome::Skipped { reason, .. } => panic!("mark skipped: {reason:?}"),
    };
    assert_eq!(tsgo_at(&held_mark), original_at);
    assert_ne!(tsgo_at(&retried_mark), original_at);
}
