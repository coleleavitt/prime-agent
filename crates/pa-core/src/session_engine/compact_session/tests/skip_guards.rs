//! Compact-session tests, the skip-and-abort guard family: the
//! pre-aborted and in-flight aborts, the too-short skip, and the
//! already-compacted skip.
use super::*;

/// An already-aborted signal cancels the run before any summarizer
/// request: the abort marker error surfaces and nothing commits.
#[tokio::test]
async fn execute_compaction_with_pre_aborted_signal_never_runs_the_summarizer() {
    let registration = faux_registration();
    let model = registration.get_model();
    let controller = pa_agent::abort::AbortController::new();
    controller.abort();
    let signal = controller.signal();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let error = execute_compaction(
        &mut session,
        CompactOptions {
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 20,
                max_context_tokens: None,
                ..Default::default()
            },
            abort: Some(&signal),
            harness_digest: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap_err();
    assert!(pa_agent::abort::is_abort_error(&error), "{error:#}");
    assert!(
        session
            .get_entries()
            .iter()
            .all(|entry| !matches!(entry, FileEntry::Compaction { .. }))
    );
    registration.unregister();
}

/// A signal that aborts while the summarizer is in flight cancels the run
/// before it commits: the resolved summary never lands as an entry.
#[tokio::test]
async fn execute_compaction_with_late_abort_cancels_before_the_commit() {
    let registration = faux_registration();
    let model = registration.get_model();
    // The delayed response holds the summarizer in flight while the
    // abort lands mid-run.
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Delayed {
        message: pa_ai::faux::faux_assistant_text_message(
            "## Goal\nsummarized goal",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
        delay_ms: 200,
    }]);
    let controller = pa_agent::abort::AbortController::new();
    let signal = controller.signal();
    let aborter = {
        let controller = controller.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            controller.abort();
        })
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let error = execute_compaction(
        &mut session,
        CompactOptions {
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 20,
                max_context_tokens: None,
                ..Default::default()
            },
            abort: Some(&signal),
            harness_digest: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap_err();
    aborter.await.unwrap();
    assert!(pa_agent::abort::is_abort_error(&error), "{error:#}");
    assert!(
        session
            .get_entries()
            .iter()
            .all(|entry| !matches!(entry, FileEntry::Compaction { .. })),
        "the late abort never commits the compaction"
    );
    registration.unregister();
}

#[tokio::test]
async fn execute_compaction_skips_short_sessions() {
    let registration = faux_registration();
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    let outcome = execute_compaction(
        &mut session,
        CompactOptions {
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings::default(),
            abort: None,
            harness_digest: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        CompactOutcome::Skipped("Session is too short to compact — try again once it grows")
    );
    assert!(
        session
            .get_entries()
            .iter()
            .all(|entry| !matches!(entry, FileEntry::Compaction { .. }))
    );
    registration.unregister();
}

#[tokio::test]
async fn execute_compaction_skips_when_already_compacted() {
    let registration = faux_registration();
    let model = registration.get_model();
    let tmp = tempfile::tempdir().unwrap();
    let mut session = session_with_turns(tmp.path(), 3);
    session
        .append_compaction(pa_types::session::CompactionEntry {
            summary: "summary".to_string(),
            first_kept_entry_id: "e1".to_string(),
            tokens_before: 100,
            ..Default::default()
        })
        .unwrap();
    let outcome = execute_compaction(
        &mut session,
        CompactOptions {
            model,
            api_key: None,
            custom_instructions: None,
            settings: super::super::compaction::CompactionSettings {
                keep_recent_tokens: 200,
                max_context_tokens: None,
                ..Default::default()
            },
            abort: None,
            harness_digest: None,
            auxiliary: None,
            semantic_edges: None,
            summary_delta: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome, CompactOutcome::Skipped("Already compacted"));
    registration.unregister();
}

/// Ambient context rows (the harness digest, the session context and its
/// updates, the reminders) are never summarizer input: a conversation whose
/// only pre-cut rows are ambient has no history to summarize, so a split
/// turn needs only its prefix summary.
#[test]
fn ambient_context_rows_are_not_summarizer_input() {
    let tmp = tempfile::tempdir().unwrap();
    let mut session = SessionManager::in_memory(tmp.path());
    for custom_type in [
        "harness_digest",
        super::super::super::context_rows::SESSION_CONTEXT_CUSTOM_TYPE,
        super::super::super::context_rows::SESSION_CONTEXT_UPDATE_CUSTOM_TYPE,
        super::super::super::reminders::SYSTEM_REMINDER_CUSTOM_TYPE,
        "goal_continuation",
    ] {
        session
            .append_custom_message(custom_type, UserContent::Text("x".into()), false, None)
            .unwrap();
    }
    let kept: Vec<String> = session
        .get_entries()
        .iter()
        .filter_map(super::super::message_from_entry)
        .filter_map(|message| match message {
            AgentMessage::Custom(row) => Some(row.custom_type),
            _ => None,
        })
        .collect();
    assert_eq!(kept, ["goal_continuation"]);
}
