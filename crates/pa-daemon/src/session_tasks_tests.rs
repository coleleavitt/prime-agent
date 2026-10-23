//! The worker scheduler's battery: idle-only delivery, the fire's queue
//! shape, the loop-tick settle, and the bind (durable tasks across a
//! restart, the missed one-shot notice). The clock is injected; nothing
//! sleeps.

use std::sync::atomic::{AtomicI64, Ordering};

use pa_core::cron::tasks::book::TaskBook;
use pa_core::cron::tasks::expr::UtcZone;
use pa_core::cron::tasks::{SessionTasks, TaskFlags, host};
use serde_json::{Value, json};

use super::*;

/// 2026-10-17T10:02:00Z.
const T0: i64 = 1_792_231_320_000;
/// 2026-10-17T10:10:00Z.
const T10: i64 = 1_792_231_800_000;

struct Rig {
    scheduler: Arc<SessionTaskScheduler>,
    core: Arc<Mutex<SessionCore>>,
    clock: Arc<AtomicI64>,
    input_pauses: crate::session_input_pause::InputPauseTable,
}

fn rig(store: Option<crate::session_store::SessionFile>) -> Rig {
    let clock = Arc::new(AtomicI64::new(T0));
    let now = Arc::clone(&clock);
    let tasks = SessionTasks::new(
        TaskBook::default(),
        Arc::new(UtcZone::utc()),
        Arc::new(move || now.load(Ordering::SeqCst)),
        TaskFlags::default(),
    );
    let core = Arc::new(Mutex::new(SessionCore::test_core(store, "/w".to_string())));
    let input_pauses = crate::session_input_pause::InputPauseTable::new();
    let scheduler = SessionTaskScheduler::new(
        tasks,
        SchedulerWiring {
            core: Arc::clone(&core),
            work_notify: Arc::new(Notify::new()),
            idle_notify: Arc::new(Notify::new()),
            user_bash: Arc::new(crate::user_bash::UserBash::new()),
            input_pauses: input_pauses.clone(),
            recovery: Arc::new(Mutex::new(None)),
        },
    );
    Rig {
        scheduler,
        core,
        clock,
        input_pauses,
    }
}

fn queued(core: &Arc<Mutex<SessionCore>>) -> Vec<Value> {
    core.lock_or_recover()
        .follow_up
        .iter()
        .map(|item| {
            json!({
                "message": item.message,
                "preview": item.preview,
                "queueKey": item.queue_key,
                "injected": item.policy == crate::worker::TurnPolicy::Injected,
                "row": item.custom_message,
                "settles": item.done.is_some(),
            })
        })
        .collect()
}

/// A session state that is not idle, and how to set it.
type BusyState = (&'static str, fn(&mut SessionCore));

/// A due task never interrupts a busy session: it waits until the session
/// is idle, then lands on the follow-up lane as an injected row.
#[test]
fn a_due_task_waits_for_an_idle_session() {
    let rig = rig(None);
    rig.scheduler.bind();
    let created = host::cron_create(
        rig.scheduler.tasks(),
        &json!({ "cron": "*/10 * * * *", "prompt": "check the deploy" }),
    )
    .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    rig.clock.store(T10 + 120_000, Ordering::SeqCst);

    let busy_states: [BusyState; 4] = [
        ("a turn", |core| core.busy = true),
        ("a compaction", |core| core.compacting = true),
        ("queued input", |core| {
            core.pending_next_turn.push(json!({}));
        }),
        ("a closing session", |core| core.shutdown_requested = true),
    ];
    for (state, set) in busy_states {
        let mut snapshot = SessionCore::test_core(None, "/w".to_string());
        set(&mut snapshot);
        {
            let mut core = rig.core.lock_or_recover();
            core.busy = snapshot.busy;
            core.compacting = snapshot.compacting;
            core.pending_next_turn = snapshot.pending_next_turn;
            core.shutdown_requested = snapshot.shutdown_requested;
        }
        assert!(!rig.scheduler.idle(), "{state}");
        assert_eq!(rig.scheduler.deliver_due(), 0, "{state}");
        assert!(queued(&rig.core).is_empty(), "{state}");
    }
    {
        let mut core = rig.core.lock_or_recover();
        core.busy = false;
        core.compacting = false;
        core.pending_next_turn.clear();
        core.shutdown_requested = false;
    }
    let pause = rig.input_pauses.acquire_internal("test-session", "pause");
    assert_eq!(rig.scheduler.deliver_due(), 0, "an input pause");
    rig.input_pauses
        .release_internal(&pause, "test-session", "pause");

    assert_eq!(rig.scheduler.deliver_due(), 1);
    let items = queued(&rig.core);
    assert_eq!(items.len(), 1);
    let row = &items[0]["row"];
    assert_eq!(items[0]["message"], "check the deploy");
    assert_eq!(items[0]["preview"], "Scheduled task: check the deploy");
    assert_eq!(items[0]["queueKey"], format!("scheduled_task:{id}"));
    assert_eq!(items[0]["injected"], true);
    assert_eq!(items[0]["settles"], false);
    assert_eq!(row["role"], "custom");
    assert_eq!(row["customType"], "scheduled_task_fire");
    assert_eq!(row["display"], true);
    assert_eq!(row["details"]["taskId"], id);
    assert_eq!(row["details"]["kind"], "cron");
    assert_eq!(row["details"]["humanSchedule"], "Every 10 minutes");
    assert_eq!(
        row["details"]["header"],
        "Running scheduled task (Oct 17 10:12am)"
    );
    // Delivered once: the recurring task re-armed from the fire.
    rig.core.lock_or_recover().follow_up.clear();
    assert_eq!(rig.scheduler.deliver_due(), 0);
}

/// A self-paced tick's fire carries a settle; the settle arms the keepalive
/// when the model did not re-arm, and a withdrawn tick ends the loop.
#[test]
fn a_loop_tick_settle_drives_the_keepalive() {
    let rig = rig(None);
    rig.scheduler.bind();
    host::schedule_wakeup(
        rig.scheduler.tasks(),
        &json!({ "delay_seconds": 600, "prompt": "/loop check CI", "reason": "CI", "noop": false }),
    )
    .unwrap();
    rig.clock.store(T0 + 600_000, Ordering::SeqCst);
    assert_eq!(rig.scheduler.deliver_due(), 1);
    let items = queued(&rig.core);
    assert_eq!(items[0]["settles"], true);
    assert_eq!(items[0]["preview"], "Loop wakeup: /loop check CI");
    assert!(
        items[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("# /loop: self-paced")
    );
    assert_eq!(
        rig.scheduler.tick_settled(Some(&TurnSettle::Completed)),
        TickSettle::KeepaliveArmed {
            scheduled_for: T0 + 600_000 + 1_200_000
        }
    );
    rig.core.lock_or_recover().follow_up.clear();
    rig.clock.store(T0 + 1_800_000, Ordering::SeqCst);
    assert_eq!(rig.scheduler.deliver_due(), 1);
    assert_eq!(
        rig.scheduler
            .tick_settled(Some(&TurnSettle::Withdrawn("cleared".to_string()))),
        TickSettle::Ended
    );
    assert!(!rig.scheduler.has_pending());
}

#[test]
fn an_interrupt_cancels_pending_wakeups_only() {
    let rig = rig(None);
    rig.scheduler.bind();
    host::cron_create(
        rig.scheduler.tasks(),
        &json!({ "cron": "0 9 * * *", "prompt": "standup" }),
    )
    .unwrap();
    host::schedule_wakeup(
        rig.scheduler.tasks(),
        &json!({ "delay_seconds": 600, "prompt": "/loop x", "reason": "x" }),
    )
    .unwrap();
    rig.scheduler.user_abort();
    let jobs = host::cron_list(rig.scheduler.tasks()).unwrap();
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(jobs["jobs"][0]["kind"], "cron");
    assert!(rig.scheduler.has_pending());
}

/// A persisted session file under `<dir>/sessions/`, so its artifact
/// partition is `<dir>/session-artifacts/<id>`.
fn saved_session(dir: &std::path::Path) -> crate::session_store::SessionFile {
    let mut session = crate::session_store::SessionFile::create("/w", None, 0);
    let path = dir
        .join("sessions")
        .join(crate::session_store::session_file_name(
            session.session_id(),
        ));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    session.set_path(path);
    session.rewrite().unwrap();
    session
}

/// Durable tasks survive a worker restart (a daemon restart, a resume): the
/// next worker bound to the session loads them; a one-shot whose time
/// passed meanwhile is surfaced once, at the first idle pass.
#[test]
fn durable_tasks_resume_with_the_session() {
    let dir = crate::test_support::TestDir::new("pa-session-tasks-");
    let session = saved_session(&dir);
    let session_id = session.session_id().to_string();
    let first = rig(Some(session));
    first.scheduler.bind();
    host::cron_create(
        first.scheduler.tasks(),
        &json!({ "cron": "0 9 * * *", "prompt": "standup", "durable": true }),
    )
    .unwrap();
    host::cron_create(
        first.scheduler.tasks(),
        &json!({ "cron": "30 11 17 10 *", "prompt": "check the release", "recurring": false, "durable": true }),
    )
    .unwrap();
    host::cron_create(
        first.scheduler.tasks(),
        &json!({ "cron": "*/5 * * * *", "prompt": "session only" }),
    )
    .unwrap();
    let file = dir
        .join("session-artifacts")
        .join(&session_id)
        .join("scheduled_tasks.json");
    assert!(file.is_file());
    drop(first);

    let reopened = crate::session_store::SessionFile::open(
        &dir.join("sessions")
            .join(crate::session_store::session_file_name(&session_id)),
    )
    .unwrap();
    let second = rig(Some(reopened));
    second.clock.store(T0 + 3 * 3_600_000, Ordering::SeqCst);
    second.scheduler.bind();
    let jobs = host::cron_list(second.scheduler.tasks()).unwrap();
    let prompts: Vec<&str> = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["prompt"].as_str().unwrap())
        .collect();
    assert_eq!(prompts, vec!["standup"]);
    assert_eq!(second.scheduler.deliver_due(), 1);
    let items = queued(&second.core);
    assert_eq!(items[0]["row"]["details"]["kind"], "missed");
    assert!(
        items[0]["message"]
            .as_str()
            .unwrap()
            .contains("[30 11 17 10 *, created Oct 17 10:02am]\ncheck the release")
    );
}

/// A subagent keeps session-only tasks; durable ones are refused.
#[test]
fn subagents_cannot_keep_durable_tasks() {
    let dir = crate::test_support::TestDir::new("pa-session-tasks-sub-");
    let rig = rig(Some(saved_session(&dir)));
    rig.core.lock_or_recover().runtime_kind = "subagent".to_string();
    rig.scheduler.bind();
    let refused = host::cron_create(
        rig.scheduler.tasks(),
        &json!({ "cron": "0 9 * * *", "prompt": "x", "durable": true }),
    )
    .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "durable tasks are not supported for subagents"
    );
}
