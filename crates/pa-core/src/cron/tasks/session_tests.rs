use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::json;

use super::super::expr::tests::{ms, utc_zone};
use super::super::host;
use super::*;

/// A book over UTC whose clock the test moves.
fn tasks(flags: TaskFlags) -> (Arc<SessionTasks>, Arc<AtomicI64>) {
    let now = Arc::new(AtomicI64::new(ms("2026-10-17T10:02:00Z")));
    let clock = Arc::clone(&now);
    let tasks = SessionTasks::new(
        TaskBook::default(),
        Arc::new(utc_zone()),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        flags,
    );
    (tasks, now)
}

/// Durable tasks survive a restart: a fresh book bound to the same session
/// partition loads them (Claude Code's file shape), session-only ones end.
#[test]
fn durable_tasks_survive_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let (first, _) = tasks(TaskFlags::default());
    assert!(first.bind("s-1", Ok(dir.path().to_path_buf())).is_none());
    let durable = host::cron_create(
        &first,
        &json!({ "cron": "0 9 * * 1-5", "prompt": "standup notes", "durable": true }),
    )
    .unwrap();
    assert_eq!(durable["durable"], true);
    assert!(
        durable["text"]
            .as_str()
            .unwrap()
            .contains("Persisted to the session's scheduled_tasks.json")
    );
    host::cron_create(&first, &json!({ "cron": "*/5 * * * *", "prompt": "poll" })).unwrap();
    let file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("scheduled_tasks.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        file,
        json!({ "tasks": [{
            "id": durable["id"],
            "cron": "0 9 * * 1-5",
            "prompt": "standup notes",
            "createdAt": ms("2026-10-17T10:02:00Z"),
            "recurring": true,
        }] })
    );

    let (second, _) = tasks(TaskFlags::default());
    assert!(second.bind("s-1", Ok(dir.path().to_path_buf())).is_none());
    let jobs = host::cron_list(&second).unwrap();
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(jobs["jobs"][0]["prompt"], "standup notes");
    assert_eq!(jobs["jobs"][0]["human_schedule"], "Weekdays at 9:00 AM");

    // Deleting the last durable task removes the file.
    host::cron_delete(&second, &json!({ "id": durable["id"] })).unwrap();
    assert!(!dir.path().join("scheduled_tasks.json").exists());
}

/// A session replacement starts the new session from its own tasks.
#[test]
fn binding_another_session_drops_session_only_tasks() {
    let (tasks, _) = tasks(TaskFlags::default());
    tasks.bind("s-1", Err("in memory".to_string()));
    host::cron_create(&tasks, &json!({ "cron": "*/5 * * * *", "prompt": "poll" })).unwrap();
    // Re-binding the same session keeps them.
    tasks.bind("s-1", Err("in memory".to_string()));
    assert!(tasks.has_pending());
    tasks.bind("s-2", Err("in memory".to_string()));
    assert!(!tasks.has_pending());
    let refused = host::cron_create(
        &tasks,
        &json!({ "cron": "*/5 * * * *", "prompt": "poll", "durable": true }),
    )
    .unwrap_err();
    assert_eq!(refused.to_string(), "in memory");
}

#[test]
fn the_host_requests_validate_like_claude_codes_tools() {
    let (tasks, now) = tasks(TaskFlags::default());
    tasks.bind("s-1", Err("in memory".to_string()));
    let missing = host::schedule_wakeup(&tasks, &json!({ "prompt": "/loop x" })).unwrap_err();
    assert_eq!(
        missing.to_string(),
        "`delay_seconds` and `reason` are required when `stop` is not true."
    );
    let missing =
        host::schedule_wakeup(&tasks, &json!({ "delay_seconds": 90, "reason": "r" })).unwrap_err();
    assert_eq!(
        missing.to_string(),
        "`prompt` is required when `stop` is not true."
    );
    let scheduled = host::schedule_wakeup(
        &tasks,
        &json!({ "delay_seconds": 30, "prompt": "/loop x", "reason": "CI", "noop": true }),
    )
    .unwrap();
    assert_eq!(
        scheduled,
        json!({
            "scheduled_for": now.load(Ordering::SeqCst) + 60_000,
            "clamped_delay_seconds": 60,
            "was_clamped": true,
            "text": "Next wakeup scheduled for 10:03:00 (in 60s) (clamped to 60s from your requested value). If you owe the user a status update this tick, write it now as ordinary response text; then end the turn: the host wakes you when the wakeup fires or a notice arrives.",
        })
    );
    let stopped = host::schedule_wakeup(&tasks, &json!({ "stop": true })).unwrap();
    assert_eq!(stopped["cancelled_wakeups"], 1);
    let unknown = host::cron_delete(&tasks, &json!({ "id": "nope" })).unwrap_err();
    assert_eq!(unknown.to_string(), "No scheduled job with id 'nope'.");
    let bad =
        host::cron_create(&tasks, &json!({ "cron": "61 * * * *", "prompt": "x" })).unwrap_err();
    assert_eq!(
        bad.to_string(),
        "Invalid cron expression '61 * * * *'. Expected 5 fields: M H DoM Mon DoW."
    );
}

#[test]
fn disabled_scheduling_refuses_everything() {
    let (tasks, _) = tasks(TaskFlags {
        disabled: true,
        ..TaskFlags::default()
    });
    assert_eq!(
        host::cron_list(&tasks).unwrap_err().to_string(),
        DISABLED_MESSAGE
    );
    assert_eq!(
        tasks.run_loop_command("5m poll"),
        Err(DISABLED_MESSAGE.to_string())
    );
}

#[test]
fn loop_commands_schedule_on_the_host() {
    let (tasks, now) = tasks(TaskFlags::default());
    tasks.bind("s-1", Err("in memory".to_string()));
    let fixed = tasks.run_loop_command("7m check the deploy").unwrap();
    assert!(
        fixed.text.contains(
            "(Every 6 minutes, cron `*/6 * * * *`) (7m does not divide evenly; rounded to 6m)"
        ),
        "{}",
        fixed.text
    );
    let turn = fixed.turn.unwrap();
    assert_eq!(
        (turn.mode, turn.content.as_str()),
        ("fixed", "check the deploy")
    );
    assert_eq!(tasks.list()[0].kind, "loop");

    let dynamic = tasks.run_loop_command("babysit the PR").unwrap();
    let turn = dynamic.turn.unwrap();
    assert_eq!(turn.mode, "dynamic");
    assert!(turn.content.contains("prompt=\"/loop babysit the PR\""));

    let autonomous = tasks.run_loop_command("10m").unwrap();
    assert!(
        autonomous
            .turn
            .unwrap()
            .content
            .contains("# Autonomous loop check")
    );
    // The preamble went out with the command: the first fire is the short tick.
    now.store(ms("2026-10-17T10:20:00Z"), Ordering::SeqCst);
    let fires = tasks.take_due();
    let sentinel = fires
        .iter()
        .find(|fire| fire.prompt == super::super::prompts::AUTONOMOUS_LOOP_SENTINEL)
        .unwrap();
    assert!(
        tasks
            .resolve_fire_prompt(&sentinel.prompt)
            .starts_with("# Autonomous loop tick")
    );

    let stop = tasks.run_loop_command("stop").unwrap();
    assert_eq!(
        stop.text,
        "Loop stopped: cancelled 2 scheduled loop task(s)."
    );
    assert_eq!(
        tasks.run_loop_command("stop").unwrap().text,
        "No loop is running."
    );
}
