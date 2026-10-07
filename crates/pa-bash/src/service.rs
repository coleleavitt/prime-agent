//! The kernel-facing request surface: the `bash.*` requests the kernel's thin
//! client sends (through the host, or to the sidecar), as JSON in and out.
//! One implementation serves both transports.
//!
//! Every reply carries `status`: `ok`, `refused` (a guard refused; `error`
//! names the kernel's exception class, `message` its text, `warning` the
//! one-time late-bypass warning) or `error` (`error` names the Python
//! exception class to raise, `message` its text, `errno`/`filename` for an
//! `OSError`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::context::GuardContext;
use crate::pipeline::{check, Allowances};
use crate::platform::Signal;
use crate::runner::{iso_utc, Job, JobTable, SpawnError, SpawnRequest};
use crate::script::Script;
use crate::shell::{child_env, resolve_shell, ShellError};
use crate::verdict::{GuardKind, Refusal};

/// The request types [`handle`] serves (the host registers exactly these).
pub const REQUEST_TYPES: [&str; 13] = [
    "bash.check",
    "bash.isDestructiveGitDiscard",
    "bash.shell",
    "bash.childEnv",
    "bash.spawn",
    "bash.follow",
    "bash.output",
    "bash.kill",
    "bash.confirmExit",
    "bash.groupAlive",
    "bash.killAll",
    "bash.inventory",
    "bash.activity",
];

/// The longest a `bash.follow` waits for an event before answering empty.
const MAX_FOLLOW_WAIT: Duration = Duration::from_secs(30);

/// Answer one request against `table`. Blocking: `bash.follow` waits for the
/// job's next event and `bash.confirmExit` for its group's death, so callers
/// on an async runtime run it on a blocking thread.
#[must_use]
pub fn handle(table: &JobTable, request: &Value) -> Value {
    let kind = request
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "bash.check" => checked(table, request).map_or_else(|reply| reply, |_| ok(json!({}))),
        "bash.isDestructiveGitDiscard" => {
            let command = request
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            ok(json!({"discard": crate::guards::is_destructive_git_discard(command)}))
        }
        "bash.shell" => match parse(table, request) {
            Ok(parsed) => match resolve_shell(&parsed.context) {
                Ok(shell) => ok(json!({"shell": shell.to_string_lossy()})),
                Err(error) => shell_error(&error),
            },
            Err(reply) => reply,
        },
        "bash.childEnv" => match parse(table, request) {
            Ok(parsed) => ok(json!({"env": child_env(&parsed.context)})),
            Err(reply) => reply,
        },
        "bash.spawn" => spawn(table, request),
        "bash.follow" => with_job(table, request, |job| {
            let cursor = request.get("cursor").and_then(Value::as_u64).unwrap_or(0);
            let wait = request
                .get("waitMs")
                .and_then(Value::as_u64)
                .map_or(MAX_FOLLOW_WAIT, Duration::from_millis)
                .min(MAX_FOLLOW_WAIT);
            let (events, cursor, done) =
                job.follow(usize::try_from(cursor).unwrap_or(usize::MAX), wait);
            let events: Vec<Value> = events
                .iter()
                .map(crate::runner::JobEvent::to_json)
                .collect();
            ok(json!({"events": events, "cursor": cursor, "done": done}))
        }),
        "bash.output" => with_job(table, request, |job| {
            if request.get("bytesOnly").and_then(Value::as_bool) == Some(true) {
                return ok(json!({"bytes": job.output_bytes()}));
            }
            let (output, bytes) = job.output();
            ok(json!({"output": output, "bytes": bytes}))
        }),
        "bash.kill" => with_job(table, request, |job| {
            let signal = request
                .get("signal")
                .and_then(Value::as_i64)
                .and_then(|signal| i32::try_from(signal).ok())
                .map_or(Signal::TERM, Signal);
            let grace = millis(request, "graceMs").unwrap_or(Duration::from_secs(5));
            ok(json!({"killed": job.kill(signal, grace)}))
        }),
        "bash.confirmExit" => with_job(table, request, |job| {
            let term_grace = millis(request, "termGraceMs").unwrap_or(Duration::from_millis(500));
            let kill_wait = millis(request, "killWaitMs").unwrap_or(Duration::from_secs(2));
            ok(json!({"dead": job.confirm_group_exit(term_grace, kill_wait)}))
        }),
        "bash.groupAlive" => with_job(table, request, |job| {
            ok(json!({"alive": job.group_alive()}))
        }),
        "bash.killAll" => {
            table.kill_all();
            ok(json!({}))
        }
        "bash.inventory" => {
            let limit = request
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(crate::runner::INVENTORY_LIMIT, |limit| {
                    usize::try_from(limit).unwrap_or(usize::MAX)
                });
            ok(json!({"records": table.inventory(limit)}))
        }
        "bash.activity" => {
            let action = request
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = request.get("activityId").and_then(Value::as_str);
            let lines = request.get("lines").cloned().unwrap_or_else(|| json!(50));
            match table.activity(action, id, &lines) {
                Ok(Value::Object(mut fields)) => {
                    fields.insert("status".into(), "ok".into());
                    Value::Object(fields)
                }
                Ok(other) => ok(json!({"result": other})),
                Err(error) => error_reply(error.error_name(), &error.to_string()),
            }
        }
        other => error_reply(
            "RuntimeError",
            &format!("unknown bash request type {other:?}"),
        ),
    }
}

fn ok(mut fields: Value) -> Value {
    if let Some(object) = fields.as_object_mut() {
        object.insert("status".into(), "ok".into());
    }
    fields
}

fn error_reply(error: &str, message: &str) -> Value {
    json!({"status": "error", "error": error, "message": message})
}

fn shell_error(error: &ShellError) -> Value {
    let name = match error {
        ShellError::NotAbsolute => "ValueError",
        ShellError::NoWindowsShell => "RuntimeError",
    };
    error_reply(name, &error.to_string())
}

fn refused(refusal: &Refusal) -> Value {
    json!({
        "status": "refused",
        "error": refusal.guard.error_name(),
        "message": refusal.message,
        "warning": refusal.late_bypass_warning,
    })
}

fn millis(request: &Value, key: &str) -> Option<Duration> {
    request
        .get(key)
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
}

fn with_job(table: &JobTable, request: &Value, answer: impl FnOnce(&Arc<Job>) -> Value) -> Value {
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match table.get(id) {
        Some(job) => answer(&job),
        None => error_reply("KeyError", "'Unknown kernel bash job'"),
    }
}

/// The parts of a check or spawn request.
struct Parsed {
    command: String,
    script: String,
    prefix: Option<String>,
    allow: Allowances,
    context: GuardContext,
}

fn guard_list(request: &Value, key: &str) -> Vec<GuardKind> {
    request
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(GuardKind::from_key)
        .collect()
}

fn parse(table: &JobTable, request: &Value) -> Result<Parsed, Value> {
    let text = |key: &str| request.get(key).and_then(Value::as_str).map(str::to_string);
    let script = text("script").unwrap_or_default();
    let command = text("command").unwrap_or_else(|| script.clone());
    let cwd = text("cwd")
        .ok_or_else(|| error_reply("RuntimeError", "bash request needs the kernel cwd"))?;
    let env: BTreeMap<String, String> = request
        .get("env")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.clone(), value.to_string()))
        })
        .collect();
    let mut context = GuardContext::new(cwd, env)
        .with_traceparent(text("traceparent"))
        .with_sandbox(table.sandbox());
    for guard in guard_list(request, "launchBypass") {
        context = context.with_launch_bypass(guard);
    }
    let allow = guard_list(request, "allow")
        .into_iter()
        .fold(Allowances::none(), Allowances::allow);
    Ok(Parsed {
        command,
        script,
        prefix: text("prefix"),
        allow,
        context,
    })
}

fn checked(table: &JobTable, request: &Value) -> Result<Parsed, Value> {
    let parsed = parse(table, request)?;
    let script = Script {
        command: &parsed.command,
        script: &parsed.script,
        prefix: parsed.prefix.as_deref(),
    };
    check(&script, &parsed.allow, &parsed.context).map_err(|refusal| refused(&refusal))?;
    Ok(parsed)
}

fn spawn(table: &JobTable, request: &Value) -> Value {
    let parsed = match parse(table, request) {
        Ok(parsed) => parsed,
        Err(reply) => return reply,
    };
    let kernel_pid = request
        .get("kernelPid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .unwrap_or_else(std::process::id);
    let sandbox_prefix: Vec<String> = request
        .get("sandboxPrefix")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let spawn = SpawnRequest {
        script: Script {
            command: &parsed.command,
            script: &parsed.script,
            prefix: parsed.prefix.as_deref(),
        },
        allow: parsed.allow.clone(),
        context: parsed.context.clone(),
        kernel_pid,
        sandbox_prefix,
    };
    // A client that ran `bash.check` itself (the kernel checks before it
    // builds the handle, so plan mode can classify in between) skips the
    // second pass; the kernel controls its allowances either way.
    let checked = request.get("guards").and_then(Value::as_bool) == Some(false);
    let started = if checked {
        table.start_unchecked(&spawn)
    } else {
        table.spawn(&spawn)
    };
    match started {
        Ok(job) => ok(json!({"job": {
            "id": job.id,
            "pid": job.pid,
            "pgid": job.pid,
            "startedAt": iso_utc(job.started_at),
        }})),
        Err(SpawnError::Refused(refusal)) => refused(&refusal),
        Err(SpawnError::Shell(error)) => shell_error(&error),
        Err(SpawnError::Os(error)) => json!({
            "status": "error",
            "error": "OSError",
            "message": error.to_string(),
            "errno": error.raw_os_error(),
        }),
        Err(error @ (SpawnError::Enrollment | SpawnError::Sandbox(_))) => {
            error_reply("RuntimeError", &error.to_string())
        }
    }
}
