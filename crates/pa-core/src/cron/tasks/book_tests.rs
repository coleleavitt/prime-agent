use super::super::expr::tests::{ms, utc_zone};
use super::super::prompts::{
    AUTONOMOUS_LOOP_DYNAMIC_SENTINEL,
    AUTONOMOUS_LOOP_SENTINEL,
    AUTONOMOUS_PREAMBLE,
};
use super::*;

const MINUTE: i64 = 60_000;

/// A book whose ids come from `ids` in order (the jitter keys on them).
fn new_book(ids: &[&str]) -> TaskBook {
    let mut ids: std::collections::VecDeque<String> =
        ids.iter().map(|id| (*id).to_string()).collect();
    let mut book = TaskBook::default();
    book.set_id_source(Box::new(move || {
        ids.pop_front().unwrap_or_else(random_task_id)
    }));
    book
}

fn cron(expression: &str, recurring: bool) -> NewTask<'_> {
    NewTask {
        cron: expression,
        prompt: "check the deploy",
        recurring,
        durable: false,
        origin: TaskOrigin::Model,
    }
}

#[test]
fn recurring_tasks_fire_late_by_their_id_jitter() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    // Fraction 0: the cron match itself.
    let mut book = new_book(&["00000000", "80000000", "ffffffff", "ffffffff"]);
    let created = book.create(&cron("*/10 * * * *", true), t0, &zone).unwrap();
    assert_eq!(created.next_fire_at, Some(ms("2026-10-17T10:10:00Z")));
    // Fraction 0.5 of 10% of a 10-minute period: 30 s late.
    let created = book.create(&cron("*/10 * * * *", true), t0, &zone).unwrap();
    assert_eq!(created.next_fire_at, Some(ms("2026-10-17T10:10:30Z")));
    // Hourly at the top fraction: just under 6 minutes late.
    let created = book.create(&cron("0 * * * *", true), t0, &zone).unwrap();
    let late = created.next_fire_at.unwrap() - ms("2026-10-17T11:00:00Z");
    assert!((5 * MINUTE + 59_000..6 * MINUTE).contains(&late), "{late}");
    // Daily: 10% of a day is capped at 15 minutes.
    let mut daily = new_book(&["ffffffff"]);
    let created = daily.create(&cron("0 9 * * *", true), t0, &zone).unwrap();
    assert_eq!(created.next_fire_at, Some(ms("2026-10-18T09:15:00Z")));
}

/// A five-minute step fires 15 s early so the wakeup lands inside the
/// five-minute prompt cache.
#[test]
fn five_minute_steps_fire_inside_the_cache_window() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    let mut book = new_book(&["ffffffff"]);
    let created = book.create(&cron("*/5 * * * *", true), t0, &zone).unwrap();
    assert_eq!(created.next_fire_at, Some(t0 + 5 * MINUTE - 15_000));
}

#[test]
fn one_shots_on_round_minutes_fire_up_to_90_seconds_early() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    let mut book = new_book(&["80000000", "80000000", "ffffffff"]);
    let created = book
        .create(&cron("30 14 17 10 *", false), t0, &zone)
        .unwrap();
    assert_eq!(created.next_fire_at, Some(ms("2026-10-17T14:29:15Z")));
    // Off the round minutes: exact.
    let created = book
        .create(&cron("7 14 17 10 *", false), t0, &zone)
        .unwrap();
    assert_eq!(created.next_fire_at, Some(ms("2026-10-17T14:07:00Z")));
    // Never before the creation time.
    let close = ms("2026-10-17T14:29:50Z");
    let created = book
        .create(&cron("30 14 17 10 *", false), close, &zone)
        .unwrap();
    assert_eq!(created.next_fire_at, Some(close));
}

#[test]
fn validation_uses_claude_codes_messages() {
    let zone = utc_zone();
    let mut book = new_book(&[]);
    assert_eq!(
        book.create(&cron("* * * *", true), 0, &zone),
        Err("Invalid cron expression '* * * *'. Expected 5 fields: M H DoM Mon DoW.".to_string())
    );
    assert_eq!(
        book.create(&cron("0 0 31 2 *", true), 0, &zone),
        Err("Cron expression '0 0 31 2 *' does not match any calendar date.".to_string())
    );
    let durable = NewTask {
        durable: true,
        ..cron("0 9 * * *", true)
    };
    assert_eq!(
        book.create(&durable, 0, &zone),
        Err("durable tasks need a saved session".to_string())
    );
    for _ in 0..MAX_TASKS {
        book.create(&cron("0 9 * * *", true), 0, &zone).unwrap();
    }
    assert_eq!(
        book.create(&cron("0 9 * * *", true), 0, &zone),
        Err("Too many scheduled jobs (max 50). Cancel one first.".to_string())
    );
}

/// Recurring tasks re-arm from the fire, so a session that was busy across
/// several periods gets one fire, not a backlog.
#[test]
fn a_late_recurring_fire_does_not_stack() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    let mut book = new_book(&["00000000"]);
    book.create(&cron("*/10 * * * *", true), t0, &zone).unwrap();
    assert!(book.take_due(ms("2026-10-17T10:09:59Z"), &zone).is_empty());
    let late = ms("2026-10-17T10:41:00Z");
    let fires = book.take_due(late, &zone);
    assert_eq!(fires.len(), 1);
    assert_eq!(
        fires[0].kind,
        FireKind::Cron {
            recurring: true,
            final_fire: false
        }
    );
    assert_eq!(fires[0].header, "Running scheduled task (Oct 17 10:41am)");
    assert!(book.take_due(late, &zone).is_empty());
    assert_eq!(book.next_due_at(&zone), Some(ms("2026-10-17T10:50:00Z")));
    assert_eq!(book.list(&zone)[0].last_fired_at, Some(late));
}

#[test]
fn one_shots_delete_themselves_after_firing() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    let mut book = new_book(&["00000000"]);
    book.create(&cron("7 14 17 10 *", false), t0, &zone)
        .unwrap();
    let fires = book.take_due(ms("2026-10-17T14:07:00Z"), &zone);
    assert_eq!(
        fires[0].kind,
        FireKind::Cron {
            recurring: false,
            final_fire: true
        }
    );
    assert!(!book.has_pending());
}

/// Recurring tasks expire after seven days: they fire one final time, then
/// leave the book.
#[test]
fn recurring_tasks_expire_after_seven_days() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    let mut book = new_book(&["00000000"]);
    book.create(&cron("0 * * * *", true), t0, &zone).unwrap();
    let day6 = ms("2026-10-23T11:00:00Z");
    book.next_fire.insert("00000000".to_string(), day6);
    assert_eq!(
        book.take_due(day6, &zone)[0].kind,
        FireKind::Cron {
            recurring: true,
            final_fire: false
        }
    );
    let day7 = ms("2026-10-24T10:02:00Z");
    book.next_fire.insert("00000000".to_string(), day7);
    assert_eq!(
        book.take_due(day7, &zone)[0].kind,
        FireKind::Cron {
            recurring: true,
            final_fire: true
        }
    );
    assert!(!book.has_pending());
}

#[test]
fn durable_tasks_persist_and_record_their_last_fire() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:02:00Z");
    let mut book = new_book(&["00000000", "11111111"]);
    book.set_durability(Durability::Available);
    assert_eq!(book.take_durable_changes(), None);
    book.create(
        &NewTask {
            durable: true,
            ..cron("*/10 * * * *", true)
        },
        t0,
        &zone,
    )
    .unwrap();
    book.create(&cron("*/10 * * * *", true), t0, &zone).unwrap();
    let saved = book.take_durable_changes().unwrap();
    assert_eq!(
        saved,
        vec![DurableTask {
            id: "00000000".to_string(),
            cron: "*/10 * * * *".to_string(),
            prompt: "check the deploy".to_string(),
            created_at: t0,
            last_fired_at: None,
            recurring: true,
        }]
    );
    let fire_at = ms("2026-10-17T10:10:00Z");
    book.take_due(fire_at, &zone);
    assert_eq!(
        book.take_durable_changes().unwrap()[0].last_fired_at,
        Some(fire_at)
    );
}

/// A resumed session: a one-shot whose time passed is surfaced once as the
/// missed-task notice (and leaves the file); a recurring task catches up
/// with one fire.
#[test]
fn a_resumed_session_surfaces_missed_one_shots() {
    let zone = utc_zone();
    let created = ms("2026-10-17T10:00:00Z");
    let now = ms("2026-10-18T08:00:00Z");
    let mut book = new_book(&[]);
    book.set_durability(Durability::Available);
    let missed = book
        .load_durable(
            vec![
                DurableTask {
                    id: "aaaaaaaa".to_string(),
                    cron: "30 14 17 10 *".to_string(),
                    prompt: "remind me to check the deploy".to_string(),
                    created_at: created,
                    last_fired_at: None,
                    recurring: false,
                },
                DurableTask {
                    id: "bbbbbbbb".to_string(),
                    cron: "0 * * * *".to_string(),
                    prompt: "hourly sweep".to_string(),
                    created_at: created,
                    last_fired_at: Some(ms("2026-10-17T11:00:00Z")),
                    recurring: true,
                },
                DurableTask {
                    id: "cccccccc".to_string(),
                    cron: "0 9 18 10 *".to_string(),
                    prompt: "later today".to_string(),
                    created_at: created,
                    last_fired_at: None,
                    recurring: false,
                },
            ],
            now,
            &zone,
        )
        .unwrap();
    assert_eq!(missed.kind, FireKind::Missed);
    assert!(
        missed
            .prompt
            .ends_with("[30 14 17 10 *, created Oct 17 10:00am]\nremind me to check the deploy"),
        "{}",
        missed.prompt
    );
    assert!(missed.prompt.contains("Do NOT execute this prompt yet."));
    let saved = book.take_durable_changes().unwrap();
    assert_eq!(
        saved
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["bbbbbbbb", "cccccccc"]
    );
    let fires = book.take_due(now, &zone);
    assert_eq!(
        fires
            .iter()
            .map(|fire| fire.task_id.as_str())
            .collect::<Vec<_>>(),
        vec!["bbbbbbbb"]
    );
}

#[test]
fn wakeup_delays_clamp_like_claude_codes() {
    assert_eq!(clamp_delay(270.0), (270, false));
    assert_eq!(clamp_delay(270.4), (270, false));
    assert_eq!(clamp_delay(30.0), (60, true));
    assert_eq!(clamp_delay(5000.0), (3600, true));
    assert_eq!(clamp_delay(f64::NAN), (60, true));
    assert_eq!(clamp_delay(f64::INFINITY), (3600, true));
    assert_eq!(clamp_delay(f64::NEG_INFINITY), (60, true));
}

#[test]
fn one_wakeup_is_pending_at_a_time() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:00:00Z");
    let mut book = new_book(&["00000001", "00000002"]);
    let first = book.schedule_wakeup(
        1200.0,
        "/loop check CI",
        Some("watching CI"),
        false,
        t0,
        &zone,
    );
    assert_eq!(
        first,
        WakeupOutcome::Scheduled {
            scheduled_for: t0 + 1_200_000,
            clamped_seconds: 1200,
            was_clamped: false
        }
    );
    book.schedule_wakeup(
        270.0,
        "/loop check CI",
        Some("CI almost done"),
        false,
        t0,
        &zone,
    );
    let views = book.list(&zone);
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].id, "00000002");
    assert_eq!(views[0].kind, "wakeup");
    assert_eq!(views[0].next_fire_at, Some(t0 + 270_000));
    assert_eq!(views[0].reason.as_deref(), Some("CI almost done"));
}

/// A tick that ends without re-arming gets one 1200 s keepalive; the next
/// such tick ends the loop.
#[test]
fn keepalive_rearms_once_then_the_loop_ends() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:00:00Z");
    let mut book = new_book(&[]);
    book.schedule_wakeup(300.0, "/loop check CI", Some("CI"), false, t0, &zone);
    assert_eq!(book.tick_settled(t0, true, &zone), TickSettle::NoTick);
    let fired = t0 + 300_000;
    assert_eq!(book.take_due(fired, &zone).len(), 1);
    let settled = fired + 30_000;
    assert_eq!(
        book.tick_settled(settled, true, &zone),
        TickSettle::KeepaliveArmed {
            scheduled_for: settled + 1_200_000
        }
    );
    let keepalive_fires = book.take_due(settled + 1_200_000, &zone);
    assert_eq!(keepalive_fires[0].prompt, "/loop check CI");
    assert_eq!(
        book.tick_settled(settled + 1_230_000, true, &zone),
        TickSettle::Ended
    );
    assert!(!book.has_pending());
    // A model that re-arms during the tick keeps the loop going.
    book.schedule_wakeup(300.0, "/loop check CI", Some("CI"), false, t0, &zone);
    book.take_due(fired, &zone);
    book.schedule_wakeup(600.0, "/loop check CI", Some("CI"), true, fired, &zone);
    assert_eq!(book.tick_settled(fired, true, &zone), TickSettle::Rearmed);
    // With keepalive off, a tick that did not re-arm ends the loop.
    let mut off = book_without_keepalive(t0);
    assert_eq!(
        off.tick_settled(t0 + 300_000, false, &zone),
        TickSettle::Ended
    );
}

fn book_without_keepalive(t0: i64) -> TaskBook {
    let zone = utc_zone();
    let mut book = new_book(&[]);
    book.schedule_wakeup(300.0, "/loop x", Some("x"), false, t0, &zone);
    book.take_due(t0 + 300_000, &zone);
    book
}

#[test]
fn a_self_paced_loop_ages_out_after_seven_days() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:00:00Z");
    let mut book = new_book(&[]);
    let mut now = t0;
    // A wakeup every 50 minutes keeps one chain alive.
    while now < t0 + 7 * 24 * 60 * MINUTE {
        assert!(matches!(
            book.schedule_wakeup(3000.0, "/loop x", Some("x"), false, now, &zone),
            WakeupOutcome::Scheduled { .. }
        ));
        now += 50 * MINUTE;
    }
    assert_eq!(
        book.schedule_wakeup(3000.0, "/loop x", Some("x"), false, now, &zone),
        WakeupOutcome::AgedOut
    );
    assert!(!book.has_pending_wakeup());
}

#[test]
fn stopping_and_aborting_cancel_the_right_tasks() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:00:00Z");
    let mut book = new_book(&["0000000a", "0000000b", "0000000c"]);
    book.create(&cron("0 9 * * *", true), t0, &zone).unwrap();
    book.create(
        &NewTask {
            origin: TaskOrigin::Loop,
            ..cron("*/5 * * * *", true)
        },
        t0,
        &zone,
    )
    .unwrap();
    book.schedule_wakeup(600.0, "/loop x", Some("x"), false, t0, &zone);
    // stop=True: only the self-paced wakeup.
    assert_eq!(book.stop_dynamic_loop(), 1);
    assert_eq!(book.list(&zone).len(), 2);
    // /loop stop: the fixed loop too, never the model's own cron task.
    book.schedule_wakeup(600.0, "/loop x", Some("x"), false, t0, &zone);
    assert_eq!(book.stop_loops(), 2);
    let left = book.list(&zone);
    assert_eq!(
        left.iter().map(|task| task.kind).collect::<Vec<_>>(),
        vec!["cron"]
    );
    // A user interrupt cancels the pending wakeup only.
    book.schedule_wakeup(600.0, "/loop x", Some("x"), false, t0, &zone);
    assert_eq!(book.user_abort(), 1);
    assert_eq!(book.list(&zone).len(), 1);
}

#[test]
fn quiet_ticks_are_counted_in_the_header() {
    let zone = utc_zone();
    let t0 = ms("2026-10-17T10:00:00Z");
    let mut book = new_book(&[]);
    let mut now = t0;
    let mut headers = Vec::new();
    for noop in [true, true, false] {
        book.schedule_wakeup(600.0, "/loop x", Some("x"), noop, now, &zone);
        now += 600_000;
        headers.push(book.take_due(now, &zone)[0].header.clone());
    }
    assert_eq!(
        headers,
        vec![
            "Resuming /loop wakeup (Oct 17 10:10am) · 1 no-op tick since Oct 17 10:10am",
            "Resuming /loop wakeup (Oct 17 10:20am) · 2 no-op ticks since Oct 17 10:10am",
            "Resuming /loop wakeup (Oct 17 10:30am)",
        ]
    );
}

#[test]
fn fired_prompts_expand_sentinels_and_loop_reentry() {
    let mut book = new_book(&[]);
    let first = book.resolve_fire_prompt(AUTONOMOUS_LOOP_SENTINEL, false);
    assert!(first.starts_with(AUTONOMOUS_PREAMBLE.trim_end()));
    assert!(first.ends_with(&super::super::prompts::autonomous_cron_tick()));
    assert_eq!(
        book.resolve_fire_prompt(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL, false),
        super::super::prompts::autonomous_dynamic_tick()
    );
    let reentry = book.resolve_fire_prompt("/loop check the deploy", false);
    assert!(reentry.starts_with("# /loop: self-paced"));
    assert!(reentry.ends_with("## Input\n\ncheck the deploy"));
    // An interval typed into a re-entry is dropped: re-entry is self-paced.
    assert!(
        book.resolve_fire_prompt("/loop 5m check the deploy", false)
            .ends_with("## Input\n\ncheck the deploy")
    );
    assert_eq!(
        book.resolve_fire_prompt("plain prompt", false),
        "plain prompt"
    );
    assert_eq!(book.resolve_fire_prompt("/loopy", false), "/loopy");
}
