//! The kernel-facing request surface: the `bash.*` requests the kernel's thin
//! client sends (through the host, or to the sidecar), as JSON in and out.
//! One implementation serves both transports.
//!
//! `bash.run` is the one-request path of a `bash()` call: it checks, spawns
//! and follows the job for a short window, so a quick command's whole life is
//! one request; a longer one continues with `bash.follow`.
//!
//! Every reply carries `status`: `ok`, `refused` (a guard refused; `error`
//! names the kernel's exception class, `message` its text, `warning` the
//! one-time late-bypass warning) or `error` (`error` names the Python
//! exception class to raise, `message` its text, `errno`/`filename` for an
//! `OSError`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::context::GuardContext;
use crate::pipeline::{check, Allowances};
use crate::platform::Signal;
use crate::run::RunCancel;
use crate::runner::{iso_utc, Job, JobEvent, JobTable, SpawnError, SpawnRequest};
use crate::script::Script;
use crate::shell::{child_env, resolve_shell, ShellError};
use crate::verdict::{GuardKind, Refusal};

/// The request types [`handle`] serves (the host registers exactly these).
pub const REQUEST_TYPES: [&str; 13] = [
    "bash.check",
    "bash.isDestructiveGitDiscard",
    "bash.shell",
    "bash.childEnv",
    "bash.run",
    "bash.follow",
    "bash.output",
    "bash.kill",
    "bash.confirmExit",
    "bash.groupAlive",
    "bash.killAll",
    "bash.inventory",
    "bash.activity",
];

/// The error a request naming an environment key the host does not hold gets
/// (the client resends the environment whole).
const ENV_UNKNOWN: &str = "EnvUnknown";

/// The longest a `bash.follow` (or a `bash.run`'s follow window) waits
/// before answering.
const MAX_FOLLOW_WAIT: Duration = Duration::from_secs(30);

/// Answer one request against `table`. Blocking: `bash.run` and
/// `bash.follow` wait for the job's events and `bash.confirmExit` for its
/// group's death, so callers on an async runtime run it on a blocking thread.
#[must_use]
pub fn handle(table: &JobTable, request: &Value) -> Value {
    handle_cancellable(table, request, &RunCancel::default())
}

/// [`handle`], with `cancel` able to end a `bash.run` early from another
/// thread: its job is killed (TERM, then KILL) and the run answers once the
/// process group is gone, with `cancelled: true`.
#[must_use]
pub fn handle_cancellable(table: &JobTable, request: &Value, cancel: &RunCancel) -> Value {
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
        "bash.run" => run(table, request, cancel),
        "bash.follow" => with_job(table, request, |job| {
            let cursor = request.get("cursor").and_then(Value::as_u64).unwrap_or(0);
            let wait = millis(request, "waitMs")
                .unwrap_or(MAX_FOLLOW_WAIT)
                .min(MAX_FOLLOW_WAIT);
            let (events, cursor, done) =
                job.follow(usize::try_from(cursor).unwrap_or(usize::MAX), wait);
            let events = events_json(&events, spill_dir(request));
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
    // The environment rides along whole, or (unchanged since the request
    // that sent it) as the key it was sent under; an unknown key asks the
    // client to send it whole again.
    let env_key = request.get("envKey").and_then(Value::as_str);
    let env: BTreeMap<String, String> = match (request.get("env"), env_key) {
        (Some(env), key) => {
            let env = env
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|value| (name.clone(), value.to_string()))
                })
                .collect();
            if let Some(key) = key {
                table.remember_env(key, &env);
            }
            env
        }
        (None, Some(key)) => table.remembered_env(key).ok_or_else(|| {
            error_reply(ENV_UNKNOWN, "the bash host no longer has this environment")
        })?,
        (None, None) => BTreeMap::new(),
    };
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

/// A result at least this long travels in a spill file when the client
/// offers a directory for one: a file write and read beat escaping it into
/// (and parsing it out of) the reply frame.
const SPILL_MIN_BYTES: usize = 64 * 1024;

/// The events as reply JSON. With `spill` (the request's `spillDir`: a
/// directory the client reads, its own temp directory), a long result's text
/// is written to a new file there and the event names it (`outputFile`)
/// instead of carrying it; the client reads and removes the file.
fn events_json(events: &[JobEvent], spill: Option<&Path>) -> Vec<Value> {
    events
        .iter()
        .map(|event| match (event, spill) {
            (JobEvent::Finished { output, .. }, Some(dir)) if output.len() >= SPILL_MIN_BYTES => {
                let mut json = event.to_json_without_output();
                match spill_file(dir, output) {
                    Ok(path) => json["outputFile"] = path.to_string_lossy().into_owned().into(),
                    Err(_) => json["output"] = (**output).into(),
                }
                json
            }
            _ => event.to_json(),
        })
        .collect()
}

/// Write `text` to a new file (create-new, owner-only) in `dir`.
fn spill_file(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    let path = dir.join(format!(
        "pa-bash-output-{}.txt",
        crate::runner::random_hex(12)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    if let Err(error) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

fn spill_dir(request: &Value) -> Option<&Path> {
    request
        .get("spillDir")
        .and_then(Value::as_str)
        .filter(|dir| !dir.is_empty())
        .map(Path::new)
}

/// `bash.run`: check, spawn, then follow the job until it is reaped or the
/// request's `waitMs` window closes; a cancel kills the job and answers once
/// its group is gone.
fn run(table: &JobTable, request: &Value, cancel: &RunCancel) -> Value {
    let parsed = match parse(table, request) {
        Ok(parsed) => parsed,
        Err(reply) => return reply,
    };
    if cancel.is_cancelled() {
        return ok(json!({"cancelled": true}));
    }
    let script = Script {
        command: &parsed.command,
        script: &parsed.script,
        prefix: parsed.prefix.as_deref(),
    };
    // `guards: false`: a handle the kernel built from a script its caller
    // declared validated (the kernel's private `_validated` path).
    if request.get("guards").and_then(Value::as_bool) != Some(false) {
        // The guards' probes carry the calling cell's trace context; the
        // command itself carries its own `bash.command` span's.
        let check_traceparent = request
            .get("checkTraceparent")
            .and_then(Value::as_str)
            .map(str::to_string);
        let check_context = parsed.context.clone().with_traceparent(check_traceparent);
        if let Err(refusal) = check(&script, &parsed.allow, &check_context) {
            return refused(&refusal);
        }
    }
    let kernel_pid = request
        .get("kernelPid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .unwrap_or_else(std::process::id);
    let spawn = SpawnRequest {
        script,
        context: parsed.context.clone(),
        kernel_pid,
    };
    let job = match table.start(&spawn) {
        Ok(job) => job,
        Err(error) => return spawn_error(error),
    };
    let window = millis(request, "waitMs")
        .unwrap_or(Duration::ZERO)
        .min(MAX_FOLLOW_WAIT);
    let followed = crate::run::follow_window(&job, window, cancel);
    let mut reply = json!({
        "job": {
            "id": job.id,
            "pid": job.pid,
            "pgid": job.pid,
            "startedAt": iso_utc(job.started_at),
        },
        "events": events_json(&followed.events, spill_dir(request)),
        "cursor": followed.cursor,
        "done": followed.done,
    });
    if followed.cancelled {
        reply["cancelled"] = true.into();
    }
    ok(reply)
}

fn spawn_error(error: SpawnError) -> Value {
    match error {
        SpawnError::Shell(error) => shell_error(&error),
        SpawnError::Os(error) => json!({
            "status": "error",
            "error": "OSError",
            "message": error.to_string(),
            "errno": error.raw_os_error(),
        }),
        error @ (SpawnError::Enrollment | SpawnError::Sandbox(_)) => {
            error_reply("RuntimeError", &error.to_string())
        }
    }
}
