//! `bash.run`'s follow window and its cancellation: the run follows the job
//! it just started for a short window, so a quick command answers with its
//! whole life (result and reap) in the one request, and a cancel arriving
//! meanwhile (the kernel was interrupted while it waited) kills the job the
//! way a cancelled one-shot `await bash(...)` does.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::platform::Signal;
use crate::runner::{Job, JobEvent};

/// A cancelled run's teardown, as the kernel's cancelled one-shot await:
/// SIGTERM, SIGKILL after this grace, then this long for a confirmed group
/// exit.
const CANCEL_TERM_GRACE: Duration = Duration::from_millis(500);
const CANCEL_KILL_WAIT: Duration = Duration::from_secs(2);
/// After a confirmed group exit, how long the run waits for the job's reaped
/// event (the reap follows the leader's exit at once).
const CANCEL_SETTLE: Duration = Duration::from_secs(2);

/// Ends a `bash.run` early from another thread (the host's `host_cancel`,
/// the sidecar's cancel frame). Cancelling before the job starts keeps it
/// from starting; cancelling after kills it, and the run answers once its
/// process group is gone.
#[derive(Debug, Clone, Default)]
pub struct RunCancel(Arc<Mutex<CancelState>>);

#[derive(Debug, Default)]
enum CancelState {
    /// No job yet, not cancelled.
    #[default]
    Waiting,
    /// The run is following this job.
    Following(Arc<Job>),
    Cancelled,
}

impl RunCancel {
    fn lock(&self) -> MutexGuard<'_, CancelState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Cancel the run: SIGTERM its job now (SIGKILL after the grace), or keep
    /// it from starting. Idempotent.
    pub fn cancel(&self) {
        let previous = std::mem::replace(&mut *self.lock(), CancelState::Cancelled);
        if let CancelState::Following(job) = previous {
            job.kill(Signal::TERM, CANCEL_TERM_GRACE);
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(*self.lock(), CancelState::Cancelled)
    }

    /// Hand the started job to the canceller. False when the run was already
    /// cancelled (the job is signalled here then).
    fn attach(&self, job: &Arc<Job>) -> bool {
        let mut state = self.lock();
        if matches!(*state, CancelState::Cancelled) {
            drop(state);
            job.kill(Signal::TERM, CANCEL_TERM_GRACE);
            return false;
        }
        *state = CancelState::Following(Arc::clone(job));
        true
    }
}

/// What the window saw.
#[derive(Debug)]
pub(crate) struct Followed {
    pub events: Vec<JobEvent>,
    /// The `bash.follow` cursor after `events`.
    pub cursor: usize,
    /// The reaped event is among `events`: nothing more will happen.
    pub done: bool,
    pub cancelled: bool,
}

/// Follow `job` until it is reaped or `window` passes, whichever is first;
/// a cancel ends the window, confirms the group's death and collects the
/// events up to the reap (bounded).
pub(crate) fn follow_window(job: &Arc<Job>, window: Duration, cancel: &RunCancel) -> Followed {
    let mut followed = Followed {
        events: Vec::new(),
        cursor: 0,
        done: false,
        cancelled: !cancel.attach(job),
    };
    let deadline = Instant::now() + window;
    while !followed.cancelled && !followed.done {
        let remaining = deadline.saturating_duration_since(Instant::now());
        collect(job, &mut followed, remaining);
        followed.cancelled = cancel.is_cancelled();
        if remaining.is_zero() {
            break;
        }
    }
    if followed.cancelled {
        job.confirm_group_exit(CANCEL_TERM_GRACE, CANCEL_KILL_WAIT);
        let settle = Instant::now() + CANCEL_SETTLE;
        while !followed.done {
            let remaining = settle.saturating_duration_since(Instant::now());
            collect(job, &mut followed, remaining);
            if remaining.is_zero() {
                break;
            }
        }
    }
    followed
}

fn collect(job: &Job, followed: &mut Followed, wait: Duration) {
    let (events, cursor, done) = job.follow(followed.cursor, wait);
    followed.events.extend(events);
    followed.cursor = cursor;
    followed.done = done;
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::RunCancel;
    use crate::runner::JobTable;
    use crate::service::{handle, handle_cancellable};

    fn request(kind: &str, command: &str, extra: &Value) -> Value {
        let mut request = json!({
            "type": kind,
            "command": command,
            "script": command,
            "cwd": std::env::temp_dir(),
            "env": {"PATH": "/usr/bin:/bin", "FOO": "from-env"},
            "launchBypass": [],
            "kernelPid": std::process::id(),
            "allow": [],
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            request[key] = value.clone();
        }
        request
    }

    fn run(table: &JobTable, command: &str, extra: &Value) -> Value {
        handle(table, &request("bash.run", command, extra))
    }

    /// The reply's events reduced to what does not vary run to run.
    fn shape(events: &Value) -> Vec<Value> {
        events
            .as_array()
            .into_iter()
            .flatten()
            .map(|event| match event["type"].as_str() {
                Some("finished") => json!({
                    "type": "finished",
                    "exitCode": event["exitCode"],
                    "output": event["output"],
                }),
                Some("reaped") => json!({"type": "reaped", "bytes": event["bytes"]}),
                _ => json!({"type": event["type"], "msg": event["msg"]}),
            })
            .collect()
    }

    fn follow_to_end(table: &JobTable, id: &Value, mut cursor: Value) -> Vec<Value> {
        let mut all = Vec::new();
        loop {
            let reply = handle(
                table,
                &json!({"type": "bash.follow", "id": id, "cursor": cursor, "waitMs": 10_000}),
            );
            all.extend(shape(&reply["events"]));
            cursor = reply["cursor"].clone();
            if reply["done"] == true {
                return all;
            }
        }
    }

    #[test]
    fn a_short_command_finishes_and_is_reaped_in_one_run() {
        let table = JobTable::new();
        let reply = run(&table, "echo hi", &json!({"waitMs": 10_000}));
        assert_eq!(reply["status"], "ok", "{reply}");
        assert_eq!(reply["done"], true);
        assert_eq!(
            shape(&reply["events"]),
            vec![
                json!({"type": "progress", "msg": "command_progress"}),
                json!({"type": "finished", "exitCode": 0, "output": "hi\n"}),
                json!({"type": "reaped", "bytes": 3}),
            ]
        );
        assert_eq!(reply["cursor"], 3);
        let job = &reply["job"];
        assert_eq!(job["pid"], job["pgid"]);
        assert!(job["id"].as_str().is_some_and(|id| id.len() == 32));
    }

    #[test]
    fn a_longer_command_continues_through_follow() {
        let table = JobTable::new();
        let reply = run(&table, "sleep 0.3; echo done", &json!({"waitMs": 0}));
        assert_eq!(reply["status"], "ok", "{reply}");
        assert_eq!(reply["done"], false);
        let mut events = shape(&reply["events"]);
        events.extend(follow_to_end(
            &table,
            &reply["job"]["id"],
            reply["cursor"].clone(),
        ));
        assert_eq!(
            events,
            vec![
                json!({"type": "progress", "msg": "command_progress"}),
                json!({"type": "finished", "exitCode": 0, "output": "done\n"}),
                json!({"type": "reaped", "bytes": 5}),
            ]
        );
    }

    #[test]
    fn a_closed_window_answers_with_the_events_so_far() {
        let table = JobTable::new();
        let started = Instant::now();
        let reply = run(&table, "echo early; sleep 30", &json!({"waitMs": 200}));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(waited < Duration::from_secs(10), "{waited:?}");
        assert_eq!(reply["done"], false);
        assert_eq!(
            shape(&reply["events"]),
            vec![json!({"type": "progress", "msg": "command_progress"})]
        );
        let killed = handle(
            &table,
            &json!({"type": "bash.kill", "id": reply["job"]["id"], "signal": 9, "graceMs": 0}),
        );
        assert_eq!(killed, json!({"status": "ok", "killed": true}));
    }

    #[test]
    fn a_refusal_is_the_checks_refusal_and_starts_nothing() {
        let table = JobTable::new();
        let refused = run(&table, "sudo id", &json!({"waitMs": 1000}));
        let checked = handle(&table, &request("bash.check", "sudo id", &json!({})));
        assert_eq!(refused, checked);
        assert_eq!(refused["status"], "refused");
        assert_eq!(refused["error"], "PrivilegeEscalationRefusalError");
        assert_eq!(table.inventory(10), Vec::<Value>::new());
    }

    #[test]
    fn a_validated_script_skips_the_guards() {
        let table = JobTable::new();
        let reply = run(
            &table,
            "echo would-refuse; exit 0; sudo id",
            &json!({"waitMs": 10_000, "guards": false}),
        );
        assert_eq!(reply["status"], "ok", "{reply}");
        assert_eq!(shape(&reply["events"])[1]["output"], "would-refuse\n");
    }

    fn cancel_after(cancel: &RunCancel, delay: Duration) -> std::thread::JoinHandle<()> {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            cancel.cancel();
        })
    }

    fn cancelled_run(command: &str) -> (JobTable, Value, Duration) {
        let table = JobTable::new();
        let cancel = RunCancel::default();
        let canceller = cancel_after(&cancel, Duration::from_millis(300));
        let started = Instant::now();
        let reply = handle_cancellable(
            &table,
            &request("bash.run", command, &json!({"waitMs": 30_000})),
            &cancel,
        );
        let elapsed = started.elapsed();
        canceller.join().unwrap();
        (table, reply, elapsed)
    }

    #[test]
    fn a_cancel_during_the_window_kills_the_job_and_settles() {
        let (table, reply, elapsed) = cancelled_run("sleep 30");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(reply["status"], "ok", "{reply}");
        assert_eq!(reply["cancelled"], true);
        assert_eq!(reply["done"], true);
        assert_eq!(
            shape(&reply["events"]),
            vec![
                json!({"type": "finished", "exitCode": -15, "output": ""}),
                json!({"type": "reaped", "bytes": 0}),
            ]
        );
        let alive = handle(
            &table,
            &json!({"type": "bash.groupAlive", "id": reply["job"]["id"]}),
        );
        assert_eq!(alive, json!({"status": "ok", "alive": false}));
    }

    #[test]
    fn a_cancelled_job_that_ignores_term_is_killed() {
        let (table, reply, elapsed) = cancelled_run("trap '' TERM; sleep 30");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(reply["cancelled"], true);
        assert_eq!(reply["done"], true);
        let events = shape(&reply["events"]);
        assert_eq!(events[0]["exitCode"], -9, "{events:?}");
        let alive = handle(
            &table,
            &json!({"type": "bash.groupAlive", "id": reply["job"]["id"]}),
        );
        assert_eq!(alive, json!({"status": "ok", "alive": false}));
    }

    #[test]
    fn a_cancel_before_the_start_starts_nothing() {
        let table = JobTable::new();
        let cancel = RunCancel::default();
        cancel.cancel();
        let reply = handle_cancellable(
            &table,
            &request("bash.run", "echo never", &json!({"waitMs": 1000})),
            &cancel,
        );
        assert_eq!(reply, json!({"status": "ok", "cancelled": true}));
        assert_eq!(table.inventory(10), Vec::<Value>::new());
    }

    #[test]
    fn a_long_result_travels_in_a_spill_file_when_offered() {
        let table = JobTable::new();
        let dir = tempfile::tempdir().unwrap();
        let command = "head -c 100000 /dev/zero | tr '\\0' x";
        let spilled = run(
            &table,
            command,
            &json!({"waitMs": 10_000, "spillDir": dir.path()}),
        );
        let finished = &spilled["events"][1];
        assert_eq!(finished["type"], "finished");
        assert_eq!(finished.get("output"), None);
        let path = finished["outputFile"].as_str().unwrap();
        assert!(path.starts_with(dir.path().to_str().unwrap()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "x".repeat(100_000));
        let inline = run(&table, command, &json!({"waitMs": 10_000}));
        assert_eq!(inline["events"][1]["output"], "x".repeat(100_000));
        assert_eq!(inline["events"][1].get("outputFile"), None);
        // A short result stays inline even when a directory is offered.
        let short = run(
            &table,
            "echo hi",
            &json!({"waitMs": 10_000, "spillDir": dir.path()}),
        );
        assert_eq!(short["events"][1]["output"], "hi\n");
    }

    #[test]
    fn an_unchanged_environment_travels_as_its_key() {
        let table = JobTable::new();
        let first = run(
            &table,
            "echo $FOO",
            &json!({"waitMs": 10_000, "envKey": "k1"}),
        );
        assert_eq!(shape(&first["events"])[1]["output"], "from-env\n");
        let mut keyed = request(
            "bash.run",
            "echo $FOO",
            &json!({"waitMs": 10_000, "envKey": "k1"}),
        );
        keyed.as_object_mut().unwrap().remove("env");
        let second = handle(&table, &keyed);
        assert_eq!(shape(&second["events"])[1]["output"], "from-env\n");
        keyed["envKey"] = "stale".into();
        assert_eq!(
            handle(&table, &keyed),
            json!({
                "status": "error",
                "error": "EnvUnknown",
                "message": "the bash host no longer has this environment",
            })
        );
    }
}
