//! The factory run notice lane end to end: a real executor announces into a
//! worker's queue through `WorkerFactoryNotices` — one row per terminal
//! transition, an idle wake, no repeat after a restart, and the kernel's
//! own read withdrawing the still-queued row.

use std::sync::atomic::{AtomicU64, Ordering};

use pa_core::factory::executor::ports::{
    FactoryChildren,
    FactoryNotice,
    FactoryNotices,
    FactorySpawn,
    PortFuture,
    SystemClock,
};
use pa_core::factory::executor::{FactoryExecutor, FactoryExecutorConfig, RunRequest};
use pa_core::factory::pyvalue::PyValue;
use pa_core::session_engine::rlm_host::RlmChildResult;

use super::*;

/// Children that settle `done` with `ANSWER` at their first collect, or
/// stay running (each collect waits briefly, like the real bounded wait).
struct ScriptedChildren {
    settle: bool,
    counter: AtomicU64,
}

impl ScriptedChildren {
    fn new(settle: bool) -> Arc<Self> {
        Arc::new(Self {
            settle,
            counter: AtomicU64::new(0),
        })
    }
}

impl FactoryChildren for ScriptedChildren {
    fn spawn(&self, _request: FactorySpawn) -> PortFuture<Result<String, String>> {
        let id = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(format!("child-{id}")) })
    }

    fn collect(
        &self,
        child_ids: Vec<String>,
        timeout_ms: u64,
    ) -> PortFuture<Result<Vec<RlmChildResult>, String>> {
        let settle = self.settle;
        Box::pin(async move {
            if !settle {
                tokio::time::sleep(Duration::from_millis(timeout_ms.min(20))).await;
            }
            Ok(child_ids
                .into_iter()
                .map(|rlm_child_id| RlmChildResult {
                    rlm_child_id,
                    session_name: None,
                    session_dir: None,
                    status: if settle { "done" } else { "running" },
                    settled: settle,
                    answer_preview: settle.then(|| "ANSWER".to_string()),
                    answer_text: settle.then(|| "ANSWER".to_string()),
                    error: None,
                    duration_ms: None,
                    tool_use_count: None,
                    replied_since_task: None,
                })
                .collect())
        })
    }

    fn delete(&self, _child_id: String) -> PortFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// The worker's lane exactly as `serve()` wires it, over this worker.
fn lane(worker: &Worker, notify: &Arc<Notify>) -> Arc<dyn FactoryNotices> {
    let (core, recovery, notify) = (
        Arc::clone(&worker.core),
        Arc::clone(&worker.recovery),
        Arc::clone(notify),
    );
    let (withdraw_core, withdraw_recovery) = (Arc::clone(&core), Arc::clone(&recovery));
    Arc::new(crate::factory_notices::WorkerFactoryNotices::new(
        Arc::new(move |row| admit_factory_run_notice(&recovery, &core, &notify, row, || false)),
        Arc::new(move |run_id, run_state| {
            withdraw_factory_run_notice(
                &withdraw_recovery,
                &withdraw_core,
                run_id,
                run_state,
                None,
            );
        }),
    ))
}

fn executor(
    children: Arc<ScriptedChildren>,
    notices: Arc<dyn FactoryNotices>,
    store_dir: Option<PathBuf>,
) -> FactoryExecutor {
    FactoryExecutor::new(FactoryExecutorConfig {
        children,
        notices,
        clock: Arc::new(SystemClock),
        store_dir,
    })
}

async fn start(executor: &FactoryExecutor) -> String {
    let spec = json!({
        "run": { "failure_policy": "escalate", "max_parallel": 2 },
        "states": [{ "id": "build", "entry": true, "subagent": { "prompt": "Build it." } }],
        "transitions": []
    });
    let result = executor
        .run(RunRequest {
            spec_id: "sw".to_string(),
            name: Some("build".to_string()),
            spec: PyValue::from_json(&spec),
            subagents: std::collections::HashMap::new(),
            library: None,
        })
        .await
        .expect("run starts");
    result["run_id"].as_str().unwrap().to_string()
}

/// One queued factory notice row, as the lanes hold it.
#[derive(Debug)]
struct NoticeRow {
    message: String,
    preview: Option<String>,
    policy: TurnPolicy,
    queue_visible: bool,
    custom_message: Value,
}

/// The factory notices queued on the follow-up lane.
fn queued_notices(worker: &Worker) -> Vec<NoticeRow> {
    worker
        .core
        .lock()
        .unwrap()
        .follow_up
        .iter()
        .filter_map(|item| {
            let row = item.custom_message.as_ref()?;
            (row.get("customType").and_then(Value::as_str) == Some("factory_run_notice")).then(
                || NoticeRow {
                    message: item.message.clone(),
                    preview: item.preview.clone(),
                    policy: item.policy,
                    queue_visible: item.queue_visible,
                    custom_message: row.clone(),
                },
            )
        })
        .collect()
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

fn worker_with_journal() -> crate::test_support::InTestDir<Arc<Worker>> {
    let dir = crate::test_support::TestDir::new("pa-worker-factory-notice-");
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "target-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({ "responses": ["ack"] })),
    };
    let worker = Arc::new(Worker::new(config, None));
    *worker.recovery.lock().unwrap() =
        Some(WorkerRecoveryJournal::open(&worker.config.recovery_journal_path).unwrap());
    crate::test_support::InTestDir::new(worker, dir)
}

async fn created_idle_worker() -> crate::test_support::InTestDir<Arc<Worker>> {
    let worker = worker_with_journal();
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "target" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    worker.dispatch("clear_queue", &json!({})).await;
    worker
}

#[tokio::test]
async fn a_completed_run_wakes_an_idle_session_with_one_task_notification() {
    let worker = created_idle_worker().await;
    let notify = Arc::new(Notify::new());
    let executor = executor(ScriptedChildren::new(true), lane(&worker, &notify), None);
    let run_id = start(&executor).await;
    wait_for(|| !queued_notices(&worker).is_empty()).await;
    executor.join(&run_id).await;
    assert_eq!(
        executor.snapshot_run(&run_id).unwrap().state.as_str(),
        "done"
    );
    let queued = queued_notices(&worker);
    assert_eq!(queued.len(), 1, "exactly one notice: {queued:?}");
    let item = &queued[0];
    assert_eq!(
        item.message,
        format!(
            "<task-notification>\n<task-id>{run_id}</task-id>\n<task-type>factory_run</task-type>\n\
             <status>completed</status>\n<summary>Factory run \"build\" completed</summary>\n\
             <result>run complete: 1 state(s), 0 transition(s) fired\n\nbuild: ANSWER\n\n\
             Full status: await rlm.factory.status('{run_id}')</result>\n</task-notification>"
        )
    );
    assert_eq!(
        item.preview.as_deref(),
        Some("Factory run: Factory run \"build\" completed")
    );
    // An idle session wakes into an invisible injected turn.
    assert_eq!(
        (item.policy, item.queue_visible),
        (TurnPolicy::Injected, false)
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .is_ok(),
        "the runner was woken"
    );
    let row = &item.custom_message;
    assert_eq!(row["details"]["status"], "completed");
    assert_eq!(row["details"]["runState"], "done");
    assert_eq!(row["display"], true);
    // The admission is live work a crash replays.
    assert!(WorkerRecoveryJournal::read_interrupted(
        &worker.config.recovery_journal_path
    ));
    // The kernel reading the finished run withdraws the undelivered row.
    executor.observed(&run_id);
    assert!(queued_notices(&worker).is_empty());
    assert!(!WorkerRecoveryJournal::read_interrupted(
        &worker.config.recovery_journal_path
    ));
}

#[tokio::test]
async fn a_busy_session_keeps_the_notice_as_a_visible_follow_up() {
    let worker = created_idle_worker().await;
    let notify = Arc::new(Notify::new());
    let lane = lane(&worker, &notify);
    // The busy sample and the admission happen in one step (no await in
    // between: the lane admits before its future is polled).
    worker.core.lock().unwrap().busy = true;
    let delivered = lane.notify(FactoryNotice {
        run_id: "run-1".to_string(),
        kind: "failed".to_string(),
        detail: "state build failed: boom".to_string(),
        node: Some("build".to_string()),
        label: "build".to_string(),
        run_state: "failed".to_string(),
        answers: Vec::new(),
    });
    let queued = queued_notices(&worker);
    assert_eq!(queued.len(), 1);
    assert_eq!(
        (queued[0].policy, queued[0].queue_visible),
        (TurnPolicy::Queued, true)
    );
    assert!(
        queued[0]
            .message
            .contains("<status>failed</status>\n<summary>Factory run \"build\" failed</summary>\n<result>state build failed: boom"),
        "{}",
        queued[0].message
    );
    assert_eq!(delivered.await, Ok(()));
    // A read of a different state, or of another run, keeps it.
    withdraw_factory_run_notice(&worker.recovery, &worker.core, "run-1", "paused", None);
    withdraw_factory_run_notice(&worker.recovery, &worker.core, "run-2", "failed", None);
    assert_eq!(queued_notices(&worker).len(), 1);
    withdraw_factory_run_notice(&worker.recovery, &worker.core, "run-1", "failed", None);
    assert!(queued_notices(&worker).is_empty());
}

/// A closed session admits nothing, and the executor hears the refusal.
#[tokio::test]
async fn a_closed_session_refuses_the_notice() {
    let worker = created_idle_worker().await;
    let notify = Arc::new(Notify::new());
    let (core, recovery) = (Arc::clone(&worker.core), Arc::clone(&worker.recovery));
    let closed = crate::factory_notices::WorkerFactoryNotices::new(
        Arc::new(move |row| admit_factory_run_notice(&recovery, &core, &notify, row, || true)),
        Arc::new(|_, _| {}),
    );
    let refused = closed
        .notify(FactoryNotice {
            run_id: "run-1".to_string(),
            kind: "finished".to_string(),
            detail: "run complete".to_string(),
            node: None,
            label: "build".to_string(),
            run_state: "done".to_string(),
            answers: Vec::new(),
        })
        .await;
    assert!(refused.is_err());
    assert!(queued_notices(&worker).is_empty());
}

#[tokio::test]
async fn a_restart_interruption_notifies_once_and_a_second_restart_stays_quiet() {
    let worker = created_idle_worker().await;
    let notify = Arc::new(Notify::new());
    let store = worker
        .config
        .recovery_journal_path
        .with_file_name("factory-runs");
    let first = executor(
        ScriptedChildren::new(false),
        lane(&worker, &notify),
        Some(store.clone()),
    );
    let run_id = start(&first).await;
    first.flush().await;
    assert!(
        queued_notices(&worker).is_empty(),
        "a running run owes nothing"
    );
    drop(first);

    // The host restarts: the in-flight run pauses as interrupted.
    let second = executor(
        ScriptedChildren::new(false),
        lane(&worker, &notify),
        Some(store.clone()),
    );
    wait_for(|| !queued_notices(&worker).is_empty()).await;
    second.flush().await;
    let queued = queued_notices(&worker);
    assert_eq!(queued.len(), 1);
    assert!(
        queued[0].message.contains(
            "<status>interrupted</status>\n<summary>Factory run \"build\" interrupted</summary>"
        ),
        "{}",
        queued[0].message
    );
    assert!(
        queued[0]
            .message
            .contains(&format!("resume with await rlm.factory.resume('{run_id}')")),
        "{}",
        queued[0].message
    );
    drop(second);

    // Another restart finds the run already paused: no second notice.
    let third = executor(
        ScriptedChildren::new(false),
        lane(&worker, &notify),
        Some(store),
    );
    third.flush().await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        queued_notices(&worker).len(),
        1,
        "no duplicate after restart"
    );
    assert_eq!(
        third.snapshot_run(&run_id).unwrap().state.as_str(),
        "paused"
    );
}
