//! `PostToolUse` for the kernel's `bash()`: a command's result reaches the
//! host as the `finished` event of a `bash.run` reply (a short command) or
//! of a later `bash.follow` reply for the same job, so the session tracks
//! each running job's command from its `bash.run` until its result arrives,
//! then presents the pair as Claude Code's `Bash` tool call and response.
//! The jobs still running are the session's background tasks (`Stop`'s
//! `background_tasks`); a job also ends when the kernel reports its
//! detached completion (`bash.completed`) or kills it.

use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

/// The most jobs tracked per session; the oldest is forgotten first (its
/// result then fires no hook).
const MAX_TRACKED_JOBS: usize = 64;

/// The most of a spilled result read back for a hook.
const MAX_SPILLED_OUTPUT: u64 = 64 * 1024;

/// A finished kernel command, as `PostToolUse` presents it.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedCommand {
    pub command: String,
    /// Claude Code's `Bash` `tool_response`.
    pub response: Value,
}

/// One running kernel command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningJob {
    pub id: String,
    pub pid: Option<u64>,
    pub command: String,
}

/// One session's running kernel commands, by job id, in start order.
#[derive(Debug, Default)]
pub struct KernelJobs {
    running: Mutex<Vec<RunningJob>>,
}

/// The `finished` event of a `bash.*` reply, when it carries one.
fn finished_event(reply: &Value) -> Option<&Value> {
    reply
        .get("events")
        .and_then(Value::as_array)?
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("finished"))
}

/// The command's output: inline, or read back (capped) from the file a
/// large result spilled to.
fn output_of(event: &Value) -> String {
    if let Some(output) = event.get("output").and_then(Value::as_str) {
        return output.to_string();
    }
    let Some(path) = event.get("outputFile").and_then(Value::as_str) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| {
        use std::io::Read;
        file.take(MAX_SPILLED_OUTPUT).read_to_end(&mut bytes)
    });
    match read {
        Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("(the output spilled to {path} could not be read: {error})"),
    }
}

fn response(reply: &Value, event: &Value) -> Value {
    json!({
        "stdout": output_of(event),
        // The kernel's commands write one merged stream.
        "stderr": "",
        "interrupted": reply.get("cancelled").and_then(Value::as_bool).unwrap_or(false),
        "isImage": false,
        "exit_code": event.get("exitCode").cloned().unwrap_or(Value::Null),
    })
}

impl KernelJobs {
    /// The commands still running, oldest first.
    #[must_use]
    pub fn running(&self) -> Vec<RunningJob> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Forget the jobs a request ended without a result reaching the host:
    /// a detached completion (`bash.completed`, by pid, whatever the host
    /// answered), a kill (`bash.kill`, by job id) or every job
    /// (`bash.killAll`).
    pub fn forget_ended(&self, request_type: &str, data: &Value) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        match request_type {
            "bash.completed" => {
                if let Some(pid) = data.get("pid").and_then(Value::as_u64) {
                    running.retain(|job| job.pid != Some(pid));
                }
            }
            "bash.kill" => {
                if let Some(id) = data.get("id").and_then(Value::as_str) {
                    running.retain(|job| job.id != id);
                }
            }
            "bash.killAll" => running.clear(),
            _ => {}
        }
    }

    /// Read one `bash.*` request's reply: `Some` when it carries a tracked
    /// (or just started) command's result.
    pub fn observe(
        &self,
        request_type: &str,
        data: &Value,
        reply: &Value,
    ) -> Option<FinishedCommand> {
        if reply.get("status").and_then(Value::as_str) != Some("ok") {
            return None;
        }
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        let command = match request_type {
            "bash.run" => {
                let command = data.get("command").and_then(Value::as_str)?.to_string();
                if finished_event(reply).is_some() {
                    command
                } else {
                    let job = reply.get("job")?;
                    let id = job.get("id").and_then(Value::as_str)?;
                    if running.len() >= MAX_TRACKED_JOBS {
                        running.remove(0);
                    }
                    running.push(RunningJob {
                        id: id.to_string(),
                        pid: job.get("pid").and_then(Value::as_u64),
                        command,
                    });
                    return None;
                }
            }
            "bash.follow" => {
                let job = data.get("id").and_then(Value::as_str)?;
                finished_event(reply)?;
                let index = running.iter().position(|running| running.id == job)?;
                running.remove(index).command
            }
            _ => return None,
        };
        drop(running);
        let event = finished_event(reply)?;
        Some(FinishedCommand {
            command,
            response: response(reply, event),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_reply(job: &str, events: &Value) -> Value {
        json!({ "status": "ok", "job": { "id": job, "pid": 1 }, "events": events, "cursor": 0, "done": false })
    }

    fn finished(output: &str, exit: i32) -> Value {
        json!([{ "type": "finished", "exitCode": exit, "duration": 0.1, "fields": {}, "output": output }])
    }

    #[test]
    fn a_short_command_finishes_in_its_run_reply() {
        let jobs = KernelJobs::default();
        let finished = jobs.observe(
            "bash.run",
            &json!({ "type": "bash.run", "command": "echo hi" }),
            &run_reply("j1", &finished("hi\n", 0)),
        );
        assert_eq!(
            finished,
            Some(FinishedCommand {
                command: "echo hi".to_string(),
                response: json!({
                    "stdout": "hi\n", "stderr": "", "interrupted": false,
                    "isImage": false, "exit_code": 0
                }),
            })
        );
    }

    #[test]
    fn a_long_command_finishes_in_a_later_follow_and_only_once() {
        let jobs = KernelJobs::default();
        let started = jobs.observe(
            "bash.run",
            &json!({ "command": "make" }),
            &run_reply("j2", &json!([])),
        );
        assert_eq!(started, None);
        let progress = json!({ "status": "ok", "events": [{ "type": "progress", "msg": "command_progress", "fields": {} }], "cursor": 1, "done": false });
        assert_eq!(
            jobs.observe("bash.follow", &json!({ "id": "j2" }), &progress),
            None
        );
        let mut done =
            json!({ "status": "ok", "events": finished("built", 2), "cursor": 2, "done": true });
        done["cancelled"] = json!(true);
        let finished = jobs
            .observe("bash.follow", &json!({ "id": "j2" }), &done)
            .unwrap();
        assert_eq!(finished.command, "make");
        assert_eq!(finished.response["stdout"], "built");
        assert_eq!(finished.response["exit_code"], 2);
        assert_eq!(finished.response["interrupted"], true);
        assert_eq!(
            jobs.observe("bash.follow", &json!({ "id": "j2" }), &done),
            None
        );
        // An unknown job, a refused run and other requests fire nothing.
        assert_eq!(
            jobs.observe("bash.follow", &json!({ "id": "nope" }), &done),
            None
        );
        let refused = json!({ "status": "refused", "error": "Destructive", "message": "no" });
        assert_eq!(
            jobs.observe("bash.run", &json!({ "command": "rm -rf /" }), &refused),
            None
        );
        assert_eq!(
            jobs.observe("bash.output", &json!({ "id": "j2" }), &done),
            None
        );
    }

    #[test]
    fn a_spilled_result_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let spilled = dir.path().join("out.txt");
        std::fs::write(&spilled, "big output").unwrap();
        let event = json!({ "type": "finished", "exitCode": 0, "outputFile": spilled });
        assert_eq!(output_of(&event), "big output");
        let missing = json!({ "type": "finished", "outputFile": dir.path().join("gone") });
        assert!(
            output_of(&missing).contains("could not be read"),
            "{}",
            output_of(&missing)
        );
    }

    #[test]
    fn running_jobs_end_on_a_detached_completion_or_a_kill() {
        let jobs = KernelJobs::default();
        for (n, pid) in [(1, 11), (2, 12), (3, 13)] {
            jobs.observe(
                "bash.run",
                &json!({ "command": format!("sleep {n}") }),
                &json!({ "status": "ok", "job": { "id": format!("j{n}"), "pid": pid }, "events": [] }),
            );
        }
        assert_eq!(
            jobs.running(),
            vec![
                RunningJob {
                    id: "j1".to_string(),
                    pid: Some(11),
                    command: "sleep 1".to_string()
                },
                RunningJob {
                    id: "j2".to_string(),
                    pid: Some(12),
                    command: "sleep 2".to_string()
                },
                RunningJob {
                    id: "j3".to_string(),
                    pid: Some(13),
                    command: "sleep 3".to_string()
                },
            ]
        );
        jobs.forget_ended(
            "bash.completed",
            &json!({ "pid": 12, "command": "sleep 2", "exitCode": 0 }),
        );
        jobs.forget_ended("bash.kill", &json!({ "id": "j1", "signal": 15 }));
        jobs.forget_ended("bash.output", &json!({ "id": "j3" }));
        assert_eq!(
            jobs.running()
                .into_iter()
                .map(|job| job.id)
                .collect::<Vec<_>>(),
            vec!["j3".to_string()]
        );
        jobs.forget_ended("bash.killAll", &json!({}));
        assert!(jobs.running().is_empty());
    }

    #[test]
    fn the_oldest_job_is_forgotten_past_the_cap() {
        let jobs = KernelJobs::default();
        for n in 0..=MAX_TRACKED_JOBS {
            jobs.observe(
                "bash.run",
                &json!({ "command": format!("c{n}") }),
                &run_reply(&format!("j{n}"), &json!([])),
            );
        }
        let done = json!({ "status": "ok", "events": finished("", 0), "done": true });
        assert_eq!(
            jobs.observe("bash.follow", &json!({ "id": "j0" }), &done),
            None
        );
        assert!(
            jobs.observe("bash.follow", &json!({ "id": "j1" }), &done)
                .is_some()
        );
    }
}
