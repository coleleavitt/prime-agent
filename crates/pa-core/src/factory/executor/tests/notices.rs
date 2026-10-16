//! The session notice lane: one `<task-notification>` per run milestone
//! (completion, failure, user stop, restart interruption), never repeated
//! by a later restart, and withdrawn once the model read the state itself.

use std::sync::Arc;

use serde_json::json;

use super::fake::{Case, done, running};
use crate::factory::executor::ports::FactoryNotice;

fn store_chain(case: &Case) {
    case.store_factory(
        json!({ "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
        "sw",
    );
}

fn notices(case: &Case) -> Vec<FactoryNotice> {
    case.host.with(|state| state.factory_notices.clone())
}

#[tokio::test]
async fn a_completed_run_queues_exactly_one_completion_notice() {
    let case = Case::new();
    case.host.outcome("a", done("A-ANSWER"));
    case.host.outcome("b", done("FINAL-ANSWER"));
    store_chain(&case);
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    assert_eq!(case.settle(&result).await["state"], "done");
    case.wait_until(|| !notices(&case).is_empty()).await;
    // Later reads and a second stop never re-announce the terminal state.
    case.status(&run_id);
    case.executor.stop(&run_id).await.expect("stop");
    tokio::task::yield_now().await;
    let notices = notices(&case);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let notice = &notices[0];
    assert_eq!(
        (
            notice.kind.as_str(),
            notice.status(),
            notice.run_state.as_str()
        ),
        ("finished", "completed", "done")
    );
    // Only the final state's answer is the run's result.
    assert_eq!(
        notice.answers,
        vec![("b".to_string(), "FINAL-ANSWER".to_string())]
    );
    let text = notice.text();
    assert!(
        text.starts_with(&format!(
            "<task-notification>\n<task-id>{run_id}</task-id>\n<task-type>factory_run</task-type>\n<status>completed</status>\n<summary>Factory run \"sw\" completed</summary>\n<result>run complete: 2 state(s)"
        )),
        "{text}"
    );
    assert!(text.contains("\n\nb: FINAL-ANSWER\n\n"), "{text}");
    assert!(text.ends_with("</result>\n</task-notification>"), "{text}");
    // The ledger records the delivery.
    let milestones = case.all_events_of(&result, "milestone");
    assert_eq!(milestones.len(), 1);
    assert_eq!(milestones[0]["stage"], "shown");
}

#[tokio::test]
async fn a_user_stop_notifies_cancelled_and_a_model_stop_does_not() {
    let case = Case::new();
    case.host.outcome("a", running());
    store_chain(&case);
    let model_run = case.start().await;
    case.wait_until(|| case.host.spawn_calls("a").len() == 1)
        .await;
    case.executor
        .stop(&Case::run_id(&model_run))
        .await
        .expect("stop");
    let user_run = case.start().await;
    case.wait_until(|| case.host.spawn_calls("a").len() == 2)
        .await;
    let user_id = Case::run_id(&user_run);
    case.executor.stop_by_user(&user_id).await.expect("stop");
    case.executor
        .stop_by_user(&user_id)
        .await
        .expect("idempotent");
    let notices = notices(&case);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].run_id, user_id);
    assert_eq!(notices[0].status(), "cancelled");
    assert_eq!(notices[0].run_state, "stopped");
    assert_eq!(
        notices[0].summary(),
        "Factory run \"sw\" cancelled by the user"
    );
}

#[tokio::test]
async fn reading_a_settled_run_withdraws_its_queued_notice() {
    let case = Case::new();
    case.host.outcome("a", done("A"));
    case.host.outcome("b", running());
    store_chain(&case);
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.wait_until(|| case.host.spawn_calls("b").len() == 1)
        .await;
    // A live run's read withdraws nothing: no notice is owed yet.
    case.executor.observed(&run_id);
    case.executor.stop_by_user(&run_id).await.expect("stop");
    case.executor.observed(&run_id);
    case.executor.observed("no-such-run");
    assert_eq!(
        case.host.with(|state| state.withdrawals.clone()),
        vec![(run_id, "stopped".to_string())]
    );
}

#[tokio::test]
async fn a_host_restart_announces_one_interruption_per_restart() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let dir = temp.path().join("factory-runs");
    let case = Case::with_store(Some(dir.clone()));
    case.host.outcome("a", running());
    store_chain(&case);
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.wait_until(|| case.host.spawn_calls("a").len() == 1)
        .await;
    case.executor.flush().await;
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);

    // Restart 1: the in-flight run pauses as interrupted, and the session
    // hears it once.
    let restarted = Case::over(Arc::clone(&host), Arc::clone(&clock), Some(dir.clone()));
    restarted
        .wait_until(|| notices(&restarted).len() == 1)
        .await;
    let notice = notices(&restarted)[0].clone();
    assert_eq!(notice.run_id, run_id);
    assert_eq!(
        (
            notice.kind.as_str(),
            notice.status(),
            notice.run_state.as_str()
        ),
        ("interrupted", "interrupted", "paused")
    );
    assert!(
        notice
            .detail
            .contains("resume with await rlm.factory.resume("),
        "{notice:?}"
    );
    restarted.executor.flush().await;
    drop(restarted);

    // Restart 2: the run was already paused; nothing new is owed.
    let again = Case::over(Arc::clone(&host), Arc::clone(&clock), Some(dir.clone()));
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }
    assert_eq!(notices(&again).len(), 1, "no duplicate after a restart");

    // Resumed and interrupted again: that is a new transition, one more
    // notice.
    again.resume(&result).await.expect("resume");
    again
        .wait_until(|| again.host.spawn_calls("a").len() == 2)
        .await;
    again.executor.flush().await;
    drop(again);
    let third = Case::over(host, clock, Some(dir));
    third.wait_until(|| notices(&third).len() == 2).await;
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }
    let kinds: Vec<String> = notices(&third)
        .iter()
        .map(|notice| notice.kind.clone())
        .collect();
    assert_eq!(kinds, ["interrupted", "interrupted"]);
}
