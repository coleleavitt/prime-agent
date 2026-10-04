//! Workspace Recall against real git repos: marks, the first-cell block,
//! build claims, and the session gating, ported from the TS product's
//! `workspace-recall.test.ts`.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_core::features::SessionFeature;
use pa_recall::{
    is_fully_verifiable, read_recall_mark, read_recall_skip, recall_mark_path, recall_skip_path,
    render_recall_block, witness_workspace, workspace_digest, ClaimStatus, MarkOutcome,
    MarkSkipReason, MarkWriter, RecallMarkFile, SkipReason, RECALL_BLOCK_MAX_BYTES,
    RECALL_DIGEST_ALGORITHM,
};
use support::*;

fn written(outcome: &MarkOutcome) -> &RecallMarkFile {
    match outcome {
        MarkOutcome::Written { mark, .. } => mark,
        MarkOutcome::Skipped { reason, .. } => panic!("mark skipped: {reason:?}"),
    }
}

fn skipped(session_id: &str, reason: SkipReason) -> MarkOutcome {
    MarkOutcome::Skipped {
        session_id: session_id.to_string(),
        reason,
    }
}

#[tokio::test]
async fn a_untouched_workspace_is_unchanged_and_the_build_claim_current() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;

    let session_b = Process::new();
    let ctx = context(&repo, &agent, "session-b");
    let block = session_b
        .tool_result(&ctx, "call-1")
        .await
        .expect("a block");
    assert!(block.contains("HEAD unchanged"));
    assert!(block.contains("Changed: none."));
    assert!(block.contains("3 unchanged since the mark"));
    assert!(block.contains("Unverifiable: none."));
    assert!(block.contains(&format!("- CURRENT: `{TSGO_CLAIM}` exited 0")));
    assert!(block.ends_with("</workspace_recall>"));

    // Only the first ipython result of a session carries the block.
    assert_eq!(session_b.tool_result(&ctx, "call-2").await, None);
}

#[tokio::test]
async fn b_a_file_mutated_outside_the_agent_is_changed_and_expires_the_claim() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    write(&repo.join("a.txt"), "alpha, edited outside the agent\n");

    let block = Process::new()
        .tool_result(&context(&repo, &agent, "session-c"), "call-1")
        .await
        .expect("a block");
    assert_eq!(section(&block, "Changed"), ["a.txt"]);
    assert!(block.contains("2 unchanged since the mark"));
    assert!(block.contains(&format!(
        "- EXPIRED (changed paths: a.txt): `{TSGO_CLAIM}` exited 0"
    )));
    assert!(!block.contains("CURRENT"));
}

#[cfg(unix)]
#[tokio::test]
async fn c_an_unreadable_file_is_unverifiable_and_never_counted_unchanged() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    write(&repo.join("a.txt"), "alpha, edited outside the agent\n");
    let b = repo.join("b.txt");
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&b).is_ok() {
        // Running as root: permissions do not stop the read.
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }

    let block = Process::new()
        .tool_result(&context(&repo, &agent, "session-d"), "call-1")
        .await
        .expect("a block");
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(section(&block, "Unverifiable"), ["b.txt"]);
    assert_eq!(section(&block, "Changed"), ["a.txt"]);
    // c.txt is the only path hashed on both sides and equal.
    assert!(block.contains("1 unchanged since the mark"));
    assert!(block.contains("EXPIRED"));
    assert!(!block.contains("CURRENT"));
}

#[tokio::test]
async fn d_no_block_once_the_repo_is_no_longer_a_git_worktree() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    assert_ne!(
        pa_recall::find_recall_repo(&repo).as_deref(),
        Some(repo.as_path())
    );

    let result = Process::new()
        .tool_result(&context(&repo, &agent, "session-e"), "call-1")
        .await;
    assert_eq!(result, None);
}

#[tokio::test]
async fn witnesses_against_the_mark_from_before_the_session_even_when_the_session_marked_first() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    write(&repo.join("a.txt"), "alpha, edited between sessions\n");

    let session_b = Process::new();
    let ctx = context(&repo, &agent, "session-b");
    // Session B answers once without ipython; its run end rewrites the mark
    // with the edit already in it.
    written(&session_b.agent_end(&ctx));
    let block = session_b
        .tool_result(&ctx, "call-1")
        .await
        .expect("a block");
    assert_eq!(section(&block, "Changed"), ["a.txt"]);
    assert!(block.contains("EXPIRED (changed paths: a.txt)"));
}

#[tokio::test]
async fn keeps_the_prior_mark_when_the_witness_runs_while_the_first_mark_is_still_writing() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    write(&repo.join("a.txt"), "alpha, edited between sessions\n");

    let session_b = Process::new();
    let ctx = context(&repo, &agent, "session-b");
    session_b.recall.on_agent_end(&ctx);
    let block = session_b
        .tool_result(&ctx, "call-1")
        .await
        .expect("a block");
    written(&session_b.next_settled());
    assert_eq!(section(&block, "Changed"), ["a.txt"]);
}

#[tokio::test]
async fn skips_resumed_sessions_children_other_tools_and_repos_without_a_mark() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    assert_eq!(
        Process::new()
            .tool_result(&context(&repo, &agent, "fresh"), "call-1")
            .await,
        None
    );

    mark_session_a(&repo, &agent).await;
    let process = Process::new();
    let resumed = context(&repo, &agent, "resumed");
    assert_eq!(
        process
            .tool_result_with(&resumed, "call-1", "ipython", serde_json::Value::Null, 1)
            .await,
        None
    );
    let child = child_context(&repo, &agent, "child");
    assert_eq!(process.tool_result(&child, "call-1").await, None);

    let other_tool = context(&repo, &agent, "other-tool");
    assert_eq!(
        process
            .tool_result_with(&other_tool, "call-1", "bash", serde_json::Value::Null, 0)
            .await,
        None
    );
    // A non-ipython result must not consume the session's witness.
    let block = process.tool_result(&other_tool, "call-2").await;
    assert!(block.is_some_and(|block| block.contains("<workspace_recall>")));

    let before = std::fs::read_to_string(recall_mark_path(&root(&repo), &agent)).unwrap();
    assert_eq!(
        process.agent_end(&child),
        skipped("child", SkipReason::ChildSession)
    );
    assert_eq!(
        std::fs::read_to_string(recall_mark_path(&root(&repo), &agent)).unwrap(),
        before
    );
}

#[tokio::test]
async fn honours_the_kill_switch_at_runtime() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;

    let process = Process::new();
    process.enabled.store(false, Ordering::SeqCst);
    assert_eq!(
        process
            .tool_result(&context(&repo, &agent, "off"), "call-1")
            .await,
        None
    );
}

#[tokio::test]
async fn writes_nothing_at_run_end_once_the_kill_switch_is_set() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let writes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&writes);
    let counting: MarkWriter = Arc::new(move |root, agent_dir, claims, now| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { pa_recall::write_recall_mark(&root, &agent_dir, &claims, now).await })
    });
    let process = Process::with(ProcessOptions {
        write_mark: Some(counting),
        ..ProcessOptions::default()
    });
    process.enabled.store(false, Ordering::SeqCst);

    assert_eq!(
        process.agent_end(&context(&repo, &agent, "switched-off")),
        skipped("switched-off", SkipReason::Disabled)
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(!recall_mark_path(&root(&repo), &agent).exists());
}

#[tokio::test]
async fn flush_returns_within_its_bound_when_a_mark_never_settles() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let started_tx = std::sync::Mutex::new(started_tx);
    let writer: MarkWriter = Arc::new(move |_root, _agent_dir, _claims, _now| {
        let _ = started_tx.lock().unwrap().send(());
        Box::pin(std::future::pending())
    });
    let process = Process::with(ProcessOptions {
        write_mark: Some(writer),
        ..ProcessOptions::default()
    });
    process
        .recall
        .on_agent_end(&context(&repo, &agent, "stuck"));
    started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the mark started");

    let started = Instant::now();
    process
        .recall
        .flush(Instant::now() + Duration::from_millis(500));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn flush_waits_for_a_pending_mark_to_land() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let process = Process::new();
    process
        .recall
        .on_agent_end(&context(&repo, &agent, "exiting"));
    process
        .recall
        .flush(Instant::now() + Duration::from_secs(30));
    assert!(read_recall_mark(&root(&repo), &agent).is_some());
}

#[tokio::test]
async fn reports_why_a_mark_was_skipped_when_the_write_itself_failed() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let mark_path = recall_mark_path(&root(&repo), &agent);
    std::fs::create_dir_all(&mark_path).unwrap();
    write(&mark_path.join("occupied"), "not a mark\n");

    let outcome = Process::new().agent_end(&context(&repo, &agent, "blocked"));
    assert!(
        matches!(
            &outcome,
            MarkOutcome::Skipped {
                reason: SkipReason::Mark(MarkSkipReason::WriteFailed(_)),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        pa_recall::write_recall_mark(&root(&repo), &repo, &[], pa_recall_now()).await,
        Err(MarkSkipReason::NotRepo)
    );
}

#[tokio::test]
async fn leaves_a_repo_alone_for_every_process_after_a_git_timeout() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;

    let timing_out: MarkWriter = Arc::new(|_root, _agent_dir, _claims, _now| {
        Box::pin(async { Err(MarkSkipReason::GitTimeout) })
    });
    let timed_out = Process::with(ProcessOptions {
        write_mark: Some(timing_out),
        ..ProcessOptions::default()
    });
    assert_eq!(
        timed_out.agent_end(&context(&repo, &agent, "slow-a")),
        skipped("slow-a", SkipReason::Mark(MarkSkipReason::GitTimeout))
    );
    assert!(read_recall_skip(&root(&repo), &agent, pa_recall_now()).is_some());

    let writes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&writes);
    let counting: MarkWriter = Arc::new(move |root, agent_dir, claims, now| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { pa_recall::write_recall_mark(&root, &agent_dir, &claims, now).await })
    });
    let next = Process::with(ProcessOptions {
        write_mark: Some(counting),
        ..ProcessOptions::default()
    });
    let ctx = context(&repo, &agent, "slow-b");
    let started = Instant::now();
    assert_eq!(next.tool_result(&ctx, "call-1").await, None);
    assert!(started.elapsed() < Duration::from_millis(500));
    next.tool_call(&ctx, "cell-1", &format!("await bash({TSGO_CLAIM:?})"))
        .await;
    assert_eq!(
        next.agent_end(&ctx),
        skipped("slow-b", SkipReason::Mark(MarkSkipReason::GitTimeout))
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ignores_a_deadline_entry_left_in_the_shared_skip_file() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    write(
        &recall_skip_path(&root(&repo), &agent),
        &serde_json::json!({
            "schema": 1,
            "reason": "deadline",
            "until": pa_recall::format_iso(pa_recall_now() + 60_000),
        })
        .to_string(),
    );
    assert_eq!(
        read_recall_skip(&root(&repo), &agent, pa_recall_now()),
        None
    );
    let block = Process::new()
        .tool_result(&context(&repo, &agent, "stale-deadline"), "call-1")
        .await
        .expect("a block");
    assert!(block.contains(&format!("- CURRENT: `{TSGO_CLAIM}`")));
}

#[tokio::test]
async fn hashes_skip_worktree_and_assume_unchanged_files_git_status_never_reports() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["update-index", "--skip-worktree", "b.txt"]);
    git(&repo, &["update-index", "--assume-unchanged", "c.txt"]);
    mark_session_a(&repo, &agent).await;

    let untouched = Process::new()
        .tool_result(&context(&repo, &agent, "flags-a"), "call-1")
        .await
        .expect("a block");
    assert!(untouched.contains("Changed: none."));
    assert!(untouched.contains("3 unchanged since the mark"));
    assert!(untouched.contains(&format!("- CURRENT: `{TSGO_CLAIM}`")));

    write(&repo.join("b.txt"), "bravo, local override\n");
    write(&repo.join("c.txt"), "charlie, local override\n");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let edited = Process::new()
        .tool_result(&context(&repo, &agent, "flags-b"), "call-1")
        .await
        .expect("a block");
    assert_eq!(section(&edited, "Changed"), ["b.txt", "c.txt"]);
    assert!(edited.contains("1 unchanged since the mark"));
    assert!(edited.contains("EXPIRED (changed paths: b.txt, c.txt)"));
}

#[tokio::test]
async fn a_bit_set_after_the_mark_is_unchanged_while_the_file_is_heads_blob() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    git(&repo, &["update-index", "--skip-worktree", "b.txt"]);
    git(&repo, &["update-index", "--assume-unchanged", "c.txt"]);

    let toggled = Process::new()
        .tool_result(&context(&repo, &agent, "toggle-a"), "call-1")
        .await
        .expect("a block");
    assert!(toggled.contains("Changed: none."));
    assert!(toggled.contains("3 unchanged since the mark"));
    assert!(toggled.contains("Unverifiable: none."));
    assert!(toggled.contains(&format!(
        "- EXPIRED (skip-worktree or assume-unchanged bits changed): `{TSGO_CLAIM}`"
    )));
    assert!(!toggled.contains("CURRENT"));

    write(&repo.join("b.txt"), "bravo, hidden by skip-worktree\n");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let edited = Process::new()
        .tool_result(&context(&repo, &agent, "toggle-b"), "call-1")
        .await
        .expect("a block");
    assert_eq!(section(&edited, "Changed"), ["b.txt"]);
    assert!(edited.contains("2 unchanged since the mark"));
    assert!(edited.contains("EXPIRED (changed paths: b.txt)"));
}

#[tokio::test]
async fn counts_a_tagged_set_too_large_to_hash_and_never_calls_a_claim_current() {
    let temp = TempDirs::new();
    let repo = temp.dir("prime-agent-recall-ignorestat-");
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "core.ignoreStat", "true"]);
    for index in 0..150 {
        write(
            &repo.join(format!("f{index:03}.txt")),
            &format!("file {index}\n"),
        );
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    assert_eq!(
        git(&repo, &["ls-files", "-v"])
            .lines()
            .filter(|line| line.starts_with("h "))
            .count(),
        150
    );

    let written = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    assert_eq!(written.mark.state.dirty, Vec::new());
    assert_eq!(written.mark.state.dirty_overflow, 150);
    assert_eq!(written.snapshot.unhashed_tagged_paths.len(), 150);
    assert!(!is_fully_verifiable(&written.snapshot.state));

    write(
        &repo.join("f120.txt"),
        "edited where git status cannot see it\n",
    );
    let block = Process::new()
        .tool_result(&context(&repo, &agent, "ignorestat"), "call-1")
        .await
        .expect("a block");
    assert!(block.contains("Changed: none detected (150 paths could not be compared)."));
    assert!(block.contains(
        "Unverifiable, not listed: 150 skip-worktree or assume-unchanged paths, too many to hash or list."
    ));
    assert!(!block.contains("Unverifiable: none."));
    assert!(!block.contains("f120.txt"));
    assert!(block.contains("Unchanged since the mark: not reported"));
    assert!(block.contains(&format!(
        "- EXPIRED (cannot verify: 150 unverifiable paths): `{TSGO_CLAIM}`"
    )));
    assert!(!block.contains("CURRENT"));
}

#[tokio::test]
async fn counts_every_skip_worktree_entry_of_a_sparse_checkout_too_large_to_check() {
    let temp = TempDirs::new();
    let repo = temp.dir("prime-agent-recall-sparse-");
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["init", "-q"]);
    let sparse: Vec<String> = (0..1001)
        .map(|index| format!("s/f{index:04}.txt"))
        .collect();
    for path in &sparse {
        write(&repo.join(path), &format!("{path}\n"));
    }
    write_three_files(&repo);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let mut args = vec!["update-index", "--skip-worktree", "--"];
    args.extend(sparse.iter().map(String::as_str));
    git(&repo, &args);
    std::fs::remove_dir_all(repo.join("s")).unwrap();

    let written = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    assert_eq!(written.mark.state.dirty_overflow, 1001);
    assert_eq!(
        written.mark.state.absent,
        pa_recall::AbsentPresence::Unchecked
    );
    assert!(!is_fully_verifiable(&written.snapshot.state));
    let report = witness_workspace(&root(&repo), &written.mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.unhashed_tagged, 1001);
    assert_eq!(report.unverifiable, Vec::<String>::new());
    assert!(matches!(report.claims[..], [ref verdict] if verdict.status != ClaimStatus::Current));
    // Unchecked presence is still recorded presence: the claim is carried forward.
    assert_eq!(
        write_mark_ok(&repo, &agent, &[]).await.mark.claims,
        written.mark.claims
    );
}

#[tokio::test]
async fn names_a_clean_file_hidden_with_skip_worktree_and_removed() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    mark_session_a(&repo, &agent).await;
    git(&repo, &["update-index", "--skip-worktree", "b.txt"]);
    std::fs::remove_file(repo.join("b.txt")).unwrap();
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");

    let block = Process::new()
        .tool_result(&context(&repo, &agent, "hidden"), "call-1")
        .await
        .expect("a block");
    assert_eq!(section(&block, "Changed"), ["b.txt"]);
    assert!(block.contains("2 unchanged since the mark"));
    assert!(block.contains(&format!(
        "- EXPIRED ({}): `{TSGO_CLAIM}`",
        pa_recall::RECALL_PRESENCE_CHANGED
    )));
    assert!(!block.contains("CURRENT"));
}

fn expired(reason: &str) -> ClaimStatus {
    ClaimStatus::Expired {
        reason: reason.to_string(),
    }
}

fn statuses(report: &pa_recall::RecallWitnessReport) -> Vec<ClaimStatus> {
    report
        .claims
        .iter()
        .map(|verdict| verdict.status.clone())
        .collect()
}

#[tokio::test]
async fn a_sparse_checkout_stays_current_untouched_and_expires_when_it_narrows_or_widens() {
    let temp = TempDirs::new();
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let full = create_packages_repo(&temp);
    let full_mark = write_mark_ok(&full, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    assert!(matches!(
        &full_mark.state.absent,
        pa_recall::AbsentPresence::Checked(absent) if absent.count == 0 && absent.paths == Some(Vec::new())
    ));
    git(&full, &["sparse-checkout", "set", "--cone", "pkga"]);
    assert!(!full.join("pkgb").exists());
    let narrowed = witness_workspace(&root(&full), &full_mark, &agent)
        .await
        .unwrap();
    assert_eq!(narrowed.changed, pkg_paths("pkgb"));
    assert_eq!(narrowed.unchanged_count, Some(6));
    assert_eq!(
        statuses(&narrowed),
        [expired(pa_recall::RECALL_PRESENCE_CHANGED)]
    );
    let mut later = claim(TSGO_CLAIM);
    later.at = Some(pa_recall::format_iso(pa_recall_now() + 1000));
    let narrowed_mark = write_mark_ok(&full, &agent, &[later]).await.mark;
    git(&full, &["sparse-checkout", "disable"]);
    assert!(full.join("pkgb/f0.ts").exists());
    let disabled = witness_workspace(&root(&full), &narrowed_mark, &agent)
        .await
        .unwrap();
    assert_eq!(disabled.changed, pkg_paths("pkgb"));
    assert_eq!(
        statuses(&disabled),
        [expired(pa_recall::RECALL_PRESENCE_CHANGED)]
    );

    let sparse = create_packages_repo(&temp);
    git(&sparse, &["sparse-checkout", "set", "--cone", "pkga"]);
    let sparse_mark = write_mark_ok(&sparse, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    assert_eq!(sparse_mark.state.dirty, Vec::new());
    assert_eq!(sparse_mark.state.dirty_overflow, 0);
    assert_eq!(sparse_mark.state.absent.listed_paths(), pkg_paths("pkgb"));
    let untouched = witness_workspace(&root(&sparse), &sparse_mark, &agent)
        .await
        .unwrap();
    assert_eq!(untouched.changed, Vec::<String>::new());
    assert_eq!(untouched.unchanged_count, Some(11));
    assert_eq!(statuses(&untouched), [ClaimStatus::Current]);

    git(&sparse, &["sparse-checkout", "add", "pkgb"]);
    assert!(sparse.join("pkgb/f0.ts").exists());
    let widened = witness_workspace(&root(&sparse), &sparse_mark, &agent)
        .await
        .unwrap();
    assert_eq!(widened.changed, pkg_paths("pkgb"));
    assert_eq!(widened.unchanged_count, Some(6));
    assert_eq!(
        statuses(&widened),
        [expired(pa_recall::RECALL_PRESENCE_CHANGED)]
    );
}

#[tokio::test]
async fn swapping_cones_with_as_many_absent_paths_names_every_moved_path() {
    let temp = TempDirs::new();
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let repo = create_packages_repo(&temp);
    git(&repo, &["sparse-checkout", "set", "--cone", "pkga"]);
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    assert_eq!(mark.state.absent.listed_paths(), pkg_paths("pkgb"));
    git(&repo, &["sparse-checkout", "set", "--cone", "pkgb"]);

    let swapped = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert!(!swapped.tracked_tree_changed);
    assert_eq!(
        swapped.changed,
        [pkg_paths("pkga"), pkg_paths("pkgb")].concat()
    );
    assert_eq!(swapped.unchanged_count, Some(1));
    assert_eq!(
        statuses(&swapped),
        [expired(pa_recall::RECALL_PRESENCE_CHANGED)]
    );
}

#[tokio::test]
async fn a_skip_worktree_file_absent_at_the_mark_and_restored_is_changed() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["update-index", "--skip-worktree", "b.txt"]);
    std::fs::remove_file(repo.join("b.txt")).unwrap();
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    assert_eq!(mark.state.dirty, Vec::new());
    assert_eq!(mark.state.dirty_overflow, 0);
    assert_eq!(mark.state.absent.listed_paths(), ["b.txt"]);
    write(&repo.join("b.txt"), "bravo\n");

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.changed, ["b.txt"]);
    assert_eq!(report.unchanged_count, Some(2));
    assert_eq!(
        statuses(&report),
        [expired(pa_recall::RECALL_PRESENCE_CHANGED)]
    );
}

#[tokio::test]
async fn absent_entries_past_the_cap_are_a_count_and_digest_counted_unverifiable() {
    let temp = TempDirs::new();
    let repo = temp.dir("prime-agent-recall-sparse-cap-");
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["init", "-q"]);
    let sparse: Vec<String> = (0..150).map(|index| format!("s/f{index:03}.txt")).collect();
    for path in &sparse {
        write(&repo.join(path), &format!("{path}\n"));
    }
    write_three_files(&repo);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let mut args = vec!["update-index", "--skip-worktree", "--"];
    args.extend(sparse.iter().map(String::as_str));
    git(&repo, &args);
    std::fs::remove_dir_all(repo.join("s")).unwrap();

    let written = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    assert!(matches!(
        &written.mark.state.absent,
        pa_recall::AbsentPresence::Checked(absent) if absent.count == 150 && absent.paths.is_none()
    ));
    let raw = std::fs::read_to_string(recall_mark_path(&root(&repo), &agent)).unwrap();
    assert!(!raw.contains("s/f000.txt"));
    assert_eq!(written.mark.state.dirty_overflow, 150);
    assert!(!is_fully_verifiable(&written.snapshot.state));

    let report = witness_workspace(&root(&repo), &written.mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.unhashed_tagged, 150);
    assert_eq!(report.uncompared_count, 150);
    assert_eq!(
        statuses(&report),
        [expired("cannot verify: 150 unverifiable paths")]
    );
    let block = render_recall_block(&report, RECALL_BLOCK_MAX_BYTES);
    assert!(block.contains(
        "Unverifiable, not listed: 150 skip-worktree or assume-unchanged paths, too many to hash or list."
    ));
    assert!(!block.contains("Unverifiable: none."));
}

/// Rewrite `mark` as a mark from before absent skip-worktree paths were
/// recorded, its claims digested without them.
fn write_legacy_mark(
    repo: &std::path::Path,
    agent: &std::path::Path,
    mark: &RecallMarkFile,
) -> RecallMarkFile {
    let mut legacy = mark.clone();
    legacy.state.absent = pa_recall::AbsentPresence::NotRecorded;
    let legacy_digest = workspace_digest(&legacy.state);
    for claim in &mut legacy.claims {
        claim.digest_at_claim.clone_from(&legacy_digest);
    }
    let text = legacy.to_json().to_string();
    assert!(!text.contains("absentSkipWorktree"));
    write(&recall_mark_path(&root(repo), agent), &text);
    let read = read_recall_mark(&root(repo), agent).expect("legacy mark readable");
    assert_eq!(read, legacy);
    read
}

#[tokio::test]
async fn never_calls_a_claim_current_against_a_mark_from_before_presence_was_recorded() {
    let temp = TempDirs::new();
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let repo = create_packages_repo(&temp);
    git(&repo, &["sparse-checkout", "set", "--cone", "pkga"]);
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    let legacy = write_legacy_mark(&repo, &agent, &mark);
    assert!(!is_fully_verifiable(&legacy.state));
    assert!(is_fully_verifiable(&mark.state));

    let predates = format!(
        "cannot verify: {}",
        pa_recall::RECALL_MARK_PREDATES_PRESENCE
    );
    let untouched = witness_workspace(&root(&repo), &legacy, &agent)
        .await
        .unwrap();
    assert_eq!(untouched.changed, Vec::<String>::new());
    assert_eq!(untouched.unverifiable, pkg_paths("pkgb"));
    assert_eq!(
        untouched.changed_unknown_reason.as_deref(),
        Some(pa_recall::RECALL_MARK_PREDATES_PRESENCE)
    );
    assert_eq!(untouched.unchanged_count, None);
    assert_eq!(statuses(&untouched), [expired(&predates)]);

    git(&repo, &["sparse-checkout", "add", "pkgb"]);
    let widened = witness_workspace(&root(&repo), &legacy, &agent)
        .await
        .unwrap();
    assert_eq!(statuses(&widened), [expired(&predates)]);
    let block = render_recall_block(&widened, RECALL_BLOCK_MAX_BYTES);
    assert!(block.contains(&format!(
        "Changed: unknown \u{2014} {}.",
        pa_recall::RECALL_MARK_PREDATES_PRESENCE
    )));
    assert!(!block.contains("Changed: none."));

    let mut forged = mark.to_json();
    forged["absentSkipWorktree"] =
        serde_json::json!({ "count": 1, "digest": "0".repeat(32), "paths": ["pkgb/f0.ts"] });
    write(&recall_mark_path(&root(&repo), &agent), &forged.to_string());
    assert_eq!(read_recall_mark(&root(&repo), &agent), None);
}

#[tokio::test]
async fn a_file_absent_at_a_legacy_mark_and_restored_with_heads_content_is_unverifiable() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["update-index", "--skip-worktree", "b.txt"]);
    std::fs::remove_file(repo.join("b.txt")).unwrap();
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    assert_eq!(mark.state.dirty, Vec::new());
    let legacy = write_legacy_mark(&repo, &agent, &mark);
    write(&repo.join("b.txt"), "bravo\n");

    let report = witness_workspace(&root(&repo), &legacy, &agent)
        .await
        .unwrap();
    assert_eq!(report.changed, Vec::<String>::new());
    assert_eq!(report.unverifiable, ["b.txt"]);
    assert_eq!(report.uncompared_count, 1);
    assert_eq!(
        statuses(&report),
        [expired(&format!(
            "cannot verify: {}",
            pa_recall::RECALL_MARK_PREDATES_PRESENCE
        ))]
    );
}

#[tokio::test]
async fn carries_no_claim_forward_from_a_legacy_mark() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    let legacy = write_legacy_mark(&repo, &agent, &mark);
    assert_eq!(legacy.claims.len(), 1);

    let rewritten = write_mark_ok(&repo, &agent, &[]).await;
    assert_eq!(
        rewritten.previous.map(|previous| previous.claims.len()),
        Some(1)
    );
    assert_eq!(rewritten.mark.claims, Vec::new());

    let mut later = claim(TSGO_CLAIM);
    later.at = Some(pa_recall::format_iso(pa_recall_now() + 1000));
    let reclaimed = write_mark_ok(&repo, &agent, &[later]).await;
    let carried = write_mark_ok(&repo, &agent, &[]).await;
    assert_eq!(carried.mark.claims, reclaimed.mark.claims);
    let report = witness_workspace(&root(&repo), &carried.mark, &agent)
        .await
        .unwrap();
    assert_eq!(statuses(&report), [ClaimStatus::Current]);
}

#[tokio::test]
async fn says_changed_is_unknown_when_head_moved_and_the_commits_cannot_be_listed() {
    let temp = TempDirs::new();
    let repo = temp.dir("prime-agent-recall-unborn-");
    let agent = temp.dir("prime-agent-recall-agentdir-");
    git(&repo, &["init", "-q"]);
    write_three_files(&repo);
    write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-q", "-m", "first commit after the mark"],
    );
    write(&repo.join("a.txt"), "alpha, edited after the commit\n");

    let block = Process::new()
        .tool_result(&context(&repo, &agent, "moved"), "call-1")
        .await
        .expect("a block");
    assert!(block.contains("HEAD moved: no commit -> "));
    assert!(block.contains(
        "Changed: unknown \u{2014} commits between the mark and HEAD could not be listed."
    ));
    assert!(!block.contains("Changed: none."));
    assert_eq!(section(&block, "Changed among compared paths"), ["a.txt"]);
    assert!(block.contains(&format!(
        "- EXPIRED (HEAD moved; commits between the mark and HEAD could not be listed): `{TSGO_CLAIM}`"
    )));
}

#[tokio::test]
async fn leaves_the_agent_dir_out_of_the_workspace_when_it_lives_inside_the_repo() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let inner_agent = repo.join(".prime").join("agent");
    std::fs::create_dir_all(&inner_agent).unwrap();
    written(&Process::new().agent_end(&context(&repo, &inner_agent, "inner-a")));
    write_mark_ok(&repo, &inner_agent, &[claim(TSGO_CLAIM)]).await;
    write(&inner_agent.join("session.jsonl"), "{}\n");

    let block = Process::new()
        .tool_result(&context(&repo, &inner_agent, "inner-b"), "call-1")
        .await
        .expect("a block");
    assert!(block.contains("Changed: none."));
    assert!(block.contains("3 unchanged since the mark"));
    assert!(block.contains(&format!("- CURRENT: `{TSGO_CLAIM}`")));
    assert_eq!(
        read_recall_mark(&root(&repo), &inner_agent).map(|mark| mark.state.dirty),
        Some(Vec::new())
    );
}

#[tokio::test]
async fn a_path_past_the_marks_recorded_window_is_unverifiable_not_changed() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    for index in 0..250 {
        write(
            &repo.join(format!("u/f{index:03}.txt")),
            &format!("untracked {index}\n"),
        );
    }
    let mark = write_mark_ok(&repo, &agent, &[]).await.mark;
    assert_eq!(mark.state.dirty_overflow, 50);
    std::fs::remove_file(repo.join("u/f000.txt")).unwrap();

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.changed, ["u/f000.txt"]);
    assert!(report.unverifiable.contains(&"u/f200.txt".to_string()));
    assert_eq!(report.unverifiable.len(), 50);
    assert_eq!(report.uncompared_count, 50);
    assert_eq!(report.unchanged_count, None);
}

#[tokio::test]
async fn keeps_a_claim_current_when_the_workspace_matches_it_exactly_after_a_partial_mark() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    write(&repo.join("z.txt"), "zulu, dirty when the claim was made\n");
    let claimed = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    for index in 0..250 {
        write(
            &repo.join(format!("gen/f{index:03}.txt")),
            &format!("generated {index}\n"),
        );
    }
    let partial = write_mark_ok(&repo, &agent, &[]).await;
    assert_eq!(partial.mark.state.dirty_overflow, 51);
    assert_eq!(partial.mark.state.dirty_digest("z.txt"), None);
    assert_eq!(partial.mark.claims, claimed.mark.claims);
    std::fs::remove_dir_all(repo.join("gen")).unwrap();

    let report = witness_workspace(&root(&repo), &partial.mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.unverifiable, ["z.txt"]);
    assert_eq!(statuses(&report), [ClaimStatus::Current]);
    assert!(render_recall_block(&report, RECALL_BLOCK_MAX_BYTES)
        .contains(&format!("- CURRENT: `{TSGO_CLAIM}`")));

    write(&repo.join("z.txt"), "zulu, edited after the claim\n");
    let edited = witness_workspace(&root(&repo), &partial.mark, &agent)
        .await
        .unwrap();
    assert!(matches!(edited.claims[..], [ref verdict] if verdict.status != ClaimStatus::Current));
}

#[tokio::test]
async fn says_no_change_was_detected_never_a_bare_none_when_a_partial_mark_hides_an_edit() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    for index in 0..250 {
        write(
            &repo.join(format!("u/f{index:03}.txt")),
            &format!("untracked {index}\n"),
        );
    }
    write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    write(&repo.join("a.txt"), "alpha, edited after a partial mark\n");

    let block = Process::new()
        .tool_result(&context(&repo, &agent, "partial-edit"), "call-1")
        .await
        .expect("a block");
    assert!(!block.contains("Changed: none."));
    assert!(block.contains("Changed: none detected (52 paths could not be compared)."));
    assert_eq!(
        section(&block, "Unverifiable").first().map(String::as_str),
        Some("a.txt")
    );
    assert!(block.contains("EXPIRED"));
    assert!(!block.contains("CURRENT"));
}

fn unrecorded(untracked: &[String], mark: &RecallMarkFile) -> Vec<String> {
    untracked
        .iter()
        .filter(|path| mark.state.dirty_digest(path).is_none())
        .cloned()
        .collect()
}

#[tokio::test]
async fn counts_the_gone_unrecorded_paths_of_a_partial_mark_as_uncompared() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let untracked = write_untracked_files(&repo);
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    let gone = unrecorded(&untracked, &mark);
    assert_eq!(gone.len(), 50);
    assert_eq!(mark.state.dirty_overflow, 50);
    for path in &gone {
        std::fs::remove_file(repo.join(path)).unwrap();
    }

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.changed, Vec::<String>::new());
    assert_eq!(report.unverifiable, Vec::<String>::new());
    assert_eq!(report.uncompared_count, 50);
    assert_eq!(report.unrecorded_up_to, None);
    let block = render_recall_block(&report, RECALL_BLOCK_MAX_BYTES);
    assert!(block.contains("Changed: none detected (50 paths could not be compared)."));
    assert!(!block.contains("Unverifiable: none."));
    assert!(block.contains("Unverifiable, not listed: 50 paths the mark left unrecorded."));
    assert!(block.contains(&format!(
        "- EXPIRED (cannot verify: 50 unverifiable paths): `{TSGO_CLAIM}`"
    )));
    assert!(render_recall_block(&report, 420).contains("Changed: 0. Unverifiable: 50."));

    // A recorded path that changed has a mark digest, so it takes nothing
    // off the unrecorded count.
    let recorded = untracked
        .iter()
        .find(|path| mark.state.dirty_digest(path).is_some())
        .unwrap();
    write(&repo.join(recorded), "edited after the mark\n");
    let edited = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(edited.changed, std::slice::from_ref(recorded));
    assert_eq!(edited.uncompared_count, 50);
    assert_eq!(edited.unrecorded_up_to, None);
}

#[tokio::test]
async fn bounds_the_unrecorded_paths_when_a_commits_new_paths_cancel_their_count() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let untracked = write_untracked_files(&repo);
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    for path in unrecorded(&untracked, &mark) {
        std::fs::remove_file(repo.join(path)).unwrap();
    }
    let added: Vec<String> = (0..50).map(|index| format!("v/g{index:02}.txt")).collect();
    for path in &added {
        write(&repo.join(path), &format!("{path}\n"));
    }
    git(&repo, &["add", "v"]);
    git(&repo, &["commit", "-q", "-m", "add unrelated files"]);

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.changed, added);
    assert_eq!(report.unverifiable, Vec::<String>::new());
    assert_eq!(report.uncompared_count, 0);
    assert_eq!(report.unrecorded_up_to, Some(50));
    let block = render_recall_block(&report, RECALL_BLOCK_MAX_BYTES);
    assert!(!block.contains("Unverifiable: none."));
    assert!(block.contains(
        "Unverifiable, not listed: up to 50 paths the mark left unrecorded (some may be among the listed paths)."
    ));
    assert!(render_recall_block(&report, 300)
        .contains("Changed: 50. Unverifiable: 0 (up to 50 unrecorded)."));
}

#[tokio::test]
async fn states_the_unrecorded_count_as_a_lower_bound_when_fewer_new_paths_take_from_it() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let untracked = write_untracked_files(&repo);
    let mark = write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)])
        .await
        .mark;
    for path in unrecorded(&untracked, &mark) {
        std::fs::remove_file(repo.join(path)).unwrap();
    }
    write(&repo.join("added.txt"), "added after the mark\n");
    git(&repo, &["add", "added.txt"]);
    git(&repo, &["commit", "-q", "-m", "add one file"]);

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.changed, ["added.txt"]);
    assert_eq!(report.uncompared_count, 49);
    assert_eq!(report.unrecorded_up_to, Some(50));
    let block = render_recall_block(&report, RECALL_BLOCK_MAX_BYTES);
    assert!(!block.contains("Unverifiable, not listed: 49 paths the mark left unrecorded."));
    assert!(block.contains(
        "Unverifiable, not listed: 49 paths the mark left unrecorded (up to 50; some may be among the listed paths)."
    ));
    assert!(render_recall_block(&report, 300).contains("Unverifiable: 49 (up to 50 unrecorded)."));
}

#[tokio::test]
async fn bounds_the_unrecorded_count_when_an_unverifiable_path_without_a_mark_digest_may_be_new() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let untracked = write_untracked_files(&repo);
    let mark = write_mark_ok(&repo, &agent, &[]).await.mark;
    for path in unrecorded(&untracked, &mark) {
        std::fs::remove_file(repo.join(path)).unwrap();
    }
    let recorded = untracked
        .iter()
        .find(|path| mark.state.dirty_digest(path).is_some())
        .unwrap();
    std::fs::remove_file(repo.join(recorded)).unwrap();
    write(&repo.join("a.txt"), "alpha, edited after a partial mark\n");

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert_eq!(report.unverifiable, ["a.txt"]);
    assert_eq!(report.uncompared_count, 50);
    assert_eq!(report.unrecorded_up_to, Some(50));
    assert!(render_recall_block(&report, RECALL_BLOCK_MAX_BYTES).contains(
        "Unverifiable, not listed: 49 paths the mark left unrecorded (up to 50; some may be among the listed paths)."
    ));
}

#[tokio::test]
async fn does_not_count_unrecorded_paths_again_once_a_commit_names_them_changed() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let untracked = write_untracked_files(&repo);
    let mark = write_mark_ok(&repo, &agent, &[]).await.mark;
    let gone = unrecorded(&untracked, &mark);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "commit the untracked files"]);

    let report = witness_workspace(&root(&repo), &mark, &agent)
        .await
        .unwrap();
    assert!(report.head_moved);
    assert_eq!(report.changed, gone);
    assert_eq!(report.unverifiable, Vec::<String>::new());
    assert_eq!(report.uncompared_count, 0);
    // Committed paths cannot be told from new ones, so they are bounded.
    assert_eq!(report.unrecorded_up_to, Some(50));
    let block = render_recall_block(&report, RECALL_BLOCK_MAX_BYTES);
    assert!(!block.contains("Unverifiable, not listed: 50 paths"));
    assert!(block
        .contains("up to 50 paths the mark left unrecorded (some may be among the listed paths)"));
    assert!(render_recall_block(&report, 300)
        .contains("Changed: 50. Unverifiable: 0 (up to 50 unrecorded)."));
}

fn cell_facts() -> serde_json::Value {
    serde_json::json!({
        "bashCommands": [
            { "command": TSGO_CLAIM, "exitCode": 0 },
            { "command": "npm test", "exitCode": 1 },
            { "command": "echo done", "exitCode": 0 },
            { "command": "cargo test", "exitCode": 0, "commandTruncated": true },
        ]
    })
}

fn tsgo_cell() -> String {
    format!("await bash({TSGO_CLAIM:?})")
}

#[tokio::test]
async fn records_a_build_command_that_exited_0_while_the_workspace_held_still() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    write(&repo.join("a.txt"), "alpha, dirty before the cell\n");
    let process = Process::new();
    let ctx = context(&repo, &agent, "claims-a");

    process.tool_call(&ctx, "cell-1", &tsgo_cell()).await;
    process
        .tool_result_with(&ctx, "cell-1", "ipython", cell_facts(), 0)
        .await;
    let outcome = process.agent_end(&ctx);
    let mark = written(&outcome);
    let commands: Vec<&str> = mark
        .claims
        .iter()
        .map(|claim| claim.command.as_str())
        .collect();
    assert_eq!(commands, [TSGO_CLAIM]);
    assert_eq!(
        mark.claims[0].digest_at_claim,
        workspace_digest(&mark.state)
    );

    let block = Process::new()
        .tool_result(&context(&repo, &agent, "claims-b"), "call-1")
        .await
        .expect("a block");
    assert!(block.contains(&format!("- CURRENT: `{TSGO_CLAIM}` exited 0")));
}

#[tokio::test]
async fn records_nothing_when_the_workspace_changed_while_the_cell_ran() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let process = Process::new();
    let ctx = context(&repo, &agent, "claims-moved");

    process.tool_call(&ctx, "cell-1", &tsgo_cell()).await;
    write(&repo.join("a.txt"), "alpha, written by the cell\n");
    process
        .tool_result_with(&ctx, "cell-1", "ipython", cell_facts(), 0)
        .await;
    assert_eq!(written(&process.agent_end(&ctx)).claims, Vec::new());
}

#[tokio::test]
async fn a_cell_that_names_no_build_command_records_no_claim() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let process = Process::new();
    let ctx = context(&repo, &agent, "claims-none");

    process.tool_call(&ctx, "cell-1", "print('hello')").await;
    process
        .tool_result_with(&ctx, "cell-1", "ipython", cell_facts(), 0)
        .await;
    assert_eq!(written(&process.agent_end(&ctx)).claims, Vec::new());
}

#[tokio::test]
async fn stores_digests_and_never_file_content() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let sentinel = format!("recall-sentinel-{}", pa_recall_now());
    write(&repo.join("a.txt"), &format!("{sentinel}\n"));
    write(
        &repo.join("untracked.txt"),
        &format!("{sentinel} untracked\n"),
    );

    write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    let raw = std::fs::read_to_string(recall_mark_path(&root(&repo), &agent)).unwrap();
    assert!(!raw.contains(&sentinel));
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["digestAlgorithm"], RECALL_DIGEST_ALGORITHM);

    let mark = read_recall_mark(&root(&repo), &agent).unwrap();
    let mut paths: Vec<&str> = mark
        .state
        .dirty
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["a.txt", "untracked.txt"]);
    let is_digest =
        |text: &str| text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit());
    assert!(mark.state.dirty.iter().all(|(_, digest)| is_digest(digest)));
    assert!(is_digest(&mark.state.tracked_tree_digest));
    assert!(mark.state.head.is_some_and(|head| head.len() >= 40));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(recall_mark_path(&root(&repo), &agent))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn records_only_build_commands_that_exited_0() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    let mut failed = claim("npm test");
    failed.exit_code = 1;
    let result = write_mark_ok(
        &repo,
        &agent,
        &[claim(TSGO_CLAIM), failed, claim("echo done")],
    )
    .await;
    let commands: Vec<&str> = result
        .mark
        .claims
        .iter()
        .map(|claim| claim.command.as_str())
        .collect();
    assert_eq!(commands, [TSGO_CLAIM]);
}

/// The mark file is the TS product's `JSON.stringify(mark, null, 2)` plus a
/// newline: a mark the TS binary wrote reads back, and re-serializes to the
/// same bytes.
#[tokio::test]
async fn a_mark_file_round_trips_byte_for_byte() {
    let temp = TempDirs::new();
    let repo = create_repo(&temp);
    let agent = temp.dir("prime-agent-recall-agentdir-");
    write(&repo.join("a.txt"), "alpha, dirty\n");
    write_mark_ok(&repo, &agent, &[claim(TSGO_CLAIM)]).await;
    let raw = std::fs::read_to_string(recall_mark_path(&root(&repo), &agent)).unwrap();
    let mark = read_recall_mark(&root(&repo), &agent).unwrap();
    assert_eq!(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&mark.to_json()).unwrap()
        ),
        raw
    );
    let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(&raw)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        [
            "schema",
            "digestAlgorithm",
            "repoRoot",
            "head",
            "trackedTreeDigest",
            "dirty",
            "dirtyOverflow",
            "absentSkipWorktree",
            "claims",
            "writtenAt"
        ]
    );
}
