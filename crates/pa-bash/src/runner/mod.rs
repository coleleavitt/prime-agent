//! The job runner: spawns checked kernel commands, keeps one kernel's jobs,
//! and answers the activity view.
//!
//! A [`JobTable`] belongs to one kernel (the host keeps one per kernel
//! manager; the sidecar one per kernel process). Live jobs stay until their
//! group is reaped; the last [`HISTORY_CAP`] finished ones stay listable.
//! Only the newest [`FULL_HISTORY`] of those keep their whole output buffer:
//! older ones keep the tail the activity view reads, so the host does not
//! grow by megabytes per finished command (and every later spawn's fork
//! does not pay to copy that memory's page tables).

mod buffer;
mod clock;
mod fence;
mod job;
mod journal;
mod pyjson;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::context::GuardContext;
use crate::pipeline::{check, Allowances};
use crate::platform::{self, Signal};
use crate::sandbox::JobSandbox;
use crate::script::Script;
use crate::shell::{child_env, resolve_shell, ShellError};

pub(crate) use clock::iso_utc;
pub(crate) use job::{Job, JobEvent};
pub(crate) use pyjson::{dumps as python_json, splitlines};

/// Finished jobs kept listable after their group is reaped.
const HISTORY_CAP: usize = 64;
/// Reaped jobs, newest first, that keep their whole buffer (a kernel reads a
/// job's output right after its reap; nothing reads it later than that).
const FULL_HISTORY: usize = 4;
/// Output bytes an older reaped job keeps: four times the activity tail's
/// wire cap, so the tail it serves is the one the whole buffer gave.
const COMPACT_KEEP: usize = 4 * ACTIVITY_FRAME_CAP;
/// The activity view's wire cap on one serialized response.
const ACTIVITY_FRAME_CAP: usize = 16_384;
/// The activity list's per-row command cap.
const ACTIVITY_COMMAND_CAP: usize = 512;
/// The inventory's row cap.
pub(crate) const INVENTORY_LIMIT: usize = 100;

/// One command to start (already checked).
#[derive(Debug, Clone)]
pub struct SpawnRequest<'a> {
    pub script: Script<'a>,
    pub context: GuardContext,
    /// The kernel process the command belongs to (journaled as `kernelPid`).
    pub kernel_pid: u32,
    /// An argv prefix the command runs under (the plan-mode read-only
    /// sandbox), empty for none.
    pub sandbox_prefix: Vec<String>,
}

/// Why a command did not start. The kernel raises each as the Python
/// exception it names.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// No shell could be chosen.
    #[error(transparent)]
    Shell(#[from] ShellError),
    /// The OS refused the pipe, socket or spawn.
    #[error(transparent)]
    Os(std::io::Error),
    /// The session's OS sandbox is on but cannot be enforced here: nothing
    /// may start unconfined.
    #[error("bash(): {0}")]
    Sandbox(String),
    /// The journal is configured with a bad owner pid; the spawned process
    /// was killed before it ran anything.
    #[error(
        "bash(): orphan-journal enrollment failed (journal configured but the pid could not be \
         recorded); the spawned process was killed"
    )]
    Enrollment,
}

/// An activity request the kernel rejects, with the Python exception it maps
/// to (the reason string is `str(exception)`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivityError {
    #[error("'Unknown kernel bash activity'")]
    UnknownActivity,
    #[error("lines must be an integer between 1 and 200")]
    BadLines,
    #[error("Unknown kernel bash action")]
    UnknownAction,
}

impl ActivityError {
    /// The Python exception class.
    #[must_use]
    pub fn error_name(&self) -> &'static str {
        match self {
            ActivityError::UnknownActivity => "KeyError",
            ActivityError::BadLines | ActivityError::UnknownAction => "ValueError",
        }
    }
}

#[derive(Debug, Default)]
struct Jobs {
    /// Every listable job in spawn order: live ones and the recent history.
    all: Vec<Arc<Job>>,
    /// Ids of reaped jobs, oldest reaped first (the history eviction order).
    reaped: VecDeque<String>,
}

/// One kernel's jobs.
#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Mutex<Jobs>,
    /// What every process the table starts runs under.
    sandbox: Mutex<JobSandbox>,
    /// The kernel environment last sent with a key: later requests name the
    /// key instead of resending an unchanged environment.
    env: Mutex<Option<(String, BTreeMap<String, String>)>>,
}

impl JobTable {
    /// An empty table. The first one in a process also starts compiling the
    /// guards' patterns on a background thread (tens of milliseconds in a
    /// release build), so the kernel's first `bash()` does not pay for it.
    #[must_use]
    pub fn new() -> Self {
        static WARM: std::sync::Once = std::sync::Once::new();
        WARM.call_once(|| {
            std::thread::spawn(|| {
                let context = GuardContext::new("/", std::collections::BTreeMap::new());
                let _ = check(&Script::bare("true"), &Allowances::none(), &context);
            });
        });
        Self::default()
    }

    /// Confine every later command, check probe and helper to `sandbox`
    /// (the host sets the kernel's own sandbox at each kernel start).
    pub fn set_sandbox(&self, sandbox: JobSandbox) {
        *self
            .sandbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = sandbox;
    }

    /// The sandbox commands start under now.
    #[must_use]
    pub fn sandbox(&self) -> JobSandbox {
        self.sandbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Remember `env` under `key` (the one key a later request may name).
    pub(crate) fn remember_env(&self, key: &str, env: &BTreeMap<String, String>) {
        *self
            .env
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((key.to_string(), env.clone()));
    }

    /// The environment remembered under `key`, if it is still the latest.
    pub(crate) fn remembered_env(&self, key: &str) -> Option<BTreeMap<String, String>> {
        self.env
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|(remembered, _)| remembered == key)
            .map(|(_, env)| env.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Jobs> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start `request` (its client ran the guards).
    ///
    /// # Errors
    ///
    /// [`SpawnError`]: no shell, an OS failure, the sandbox, or a bad journal.
    pub(crate) fn start(&self, request: &SpawnRequest<'_>) -> Result<Arc<Job>, SpawnError> {
        let context = &request.context;
        let shell = resolve_shell(context)?;
        let token = random_hex(32);
        let (token_a, token_b) = token.split_at(token.len() / 2);
        // Without a status channel (Windows) the command runs as written and
        // its result is final at shell exit.
        let script = if platform::STATUS_CHANNEL {
            fence::status_script(request.script.script, token_a, token_b, &shell)
        } else {
            request.script.script.to_string()
        };
        let mut argv = request.sandbox_prefix.clone();
        argv.push(shell.to_string_lossy().into_owned());
        argv.push("-c".to_string());
        argv.push(script);
        let (program, arguments) = argv.split_first().ok_or_else(|| {
            SpawnError::Os(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty argv",
            ))
        })?;
        let (mut command, containment) = context
            .sandbox()
            .job_command(program)
            .map_err(SpawnError::Sandbox)?;
        command
            .args(arguments)
            .current_dir(context.cwd())
            .env_clear()
            .envs(child_env(context));
        let journal = journal::Journal::from_env(context.env(), request.kernel_pid);
        let spawned = platform::spawn(command, containment).map_err(SpawnError::Os)?;
        let Ok(journal) = journal else {
            // Fail closed: a configured journal that cannot enroll the pid must
            // not let the command run (the host reaper would never see it).
            spawned.abort();
            return Err(SpawnError::Enrollment);
        };
        journal.record(spawned.pid(), true);
        // Journal first, then open the gate: the child does not run the
        // command until this byte arrives.
        spawned.channel.open_gate();
        let job = Job::start(
            job::Launch {
                id: random_hex(16),
                command: request.script.command.to_string(),
                token,
                journal,
                no_output_warn: no_output_warn(context),
            },
            spawned,
        );
        let mut jobs = self.lock();
        prune(&mut jobs);
        jobs.all.push(Arc::clone(&job));
        Ok(job)
    }

    /// The job with this id, live or recent.
    #[must_use]
    pub(crate) fn get(&self, id: &str) -> Option<Arc<Job>> {
        let mut jobs = self.lock();
        prune(&mut jobs);
        jobs.all.iter().find(|job| job.id == id).cloned()
    }

    fn live(&self) -> Vec<Arc<Job>> {
        let mut jobs = self.lock();
        prune(&mut jobs);
        jobs.all
            .iter()
            .filter(|job| !job.is_reaped())
            .cloned()
            .collect()
    }

    /// SIGKILL every live job's group (the kernel is going away). A job whose
    /// signal is not delivered keeps its active journal record for the reaper.
    pub fn kill_all(&self) {
        for job in self.live() {
            job.kill_now();
        }
    }

    /// Bounded snapshots of the live jobs' progress, oldest first.
    #[must_use]
    pub fn inventory(&self, limit: usize) -> Vec<Value> {
        let mut live = self.live();
        live.sort_by_key(|job| job.started);
        live.into_iter()
            .take(limit.min(INVENTORY_LIMIT))
            .map(|job| {
                let mut fields = Map::new();
                fields.insert("bash.pid".into(), job.pid.into());
                fields.insert("bash.pgid".into(), job.pid.into());
                fields.insert("bash.started_at".into(), iso_utc(job.started_at).into());
                fields.extend(job.snapshot());
                json!({"id": job.id, "fields": fields})
            })
            .collect()
    }

    /// The activity view (`bash_activity` list/tail/kill), shaped and capped
    /// exactly as the kernel answered it.
    ///
    /// # Errors
    ///
    /// [`ActivityError`] for an unknown id or action, or a bad line count.
    pub fn activity(
        &self,
        action: &str,
        activity_id: Option<&str>,
        lines: &Value,
    ) -> Result<Value, ActivityError> {
        if action == "list" {
            return Ok(self.activity_list());
        }
        let job = self
            .get(activity_id.unwrap_or_default())
            .ok_or(ActivityError::UnknownActivity)?;
        match action {
            "tail" => {
                let lines = lines
                    .as_u64()
                    .filter(|_| !lines.is_boolean())
                    .filter(|lines| (1..=200).contains(lines))
                    .ok_or(ActivityError::BadLines)?;
                Ok(json!({"activityId": activity_id, "tail": activity_tail(&job, lines)}))
            }
            "kill" => {
                let killed = job.kill(Signal::TERM, Duration::from_secs(5));
                Ok(json!({"activityId": activity_id, "killed": killed}))
            }
            _ => Err(ActivityError::UnknownAction),
        }
    }

    fn activity_list(&self) -> Value {
        let jobs: Vec<Arc<Job>> = {
            let mut jobs = self.lock();
            prune(&mut jobs);
            jobs.all.clone()
        };
        let mut rows: Vec<Value> = jobs
            .iter()
            .map(|job| {
                let finished = job.finished();
                let duration =
                    finished.map_or_else(|| job.started.elapsed(), |(_, duration)| duration);
                json!({
                    "id": job.id,
                    "command": job.command.chars().take(ACTIVITY_COMMAND_CAP).collect::<String>(),
                    "pid": job.pid,
                    "startedAt": iso_utc(job.started_at),
                    "durationMs": u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                    "status": if job.is_reaped() { "finished" } else { "running" },
                    "exitCode": finished.map(|(code, _)| code),
                })
            })
            .collect();
        // Long commands are truncated per row first, then rows drop until the
        // response fits: finished rows before running ones, so a live process
        // never falls off the list while it is the one the user can act on.
        while python_json(&json!({"activities": rows})).len() > ACTIVITY_FRAME_CAP && rows.len() > 1
        {
            let victim = rows
                .iter()
                .position(|row| row["status"] != "running")
                .unwrap_or(0);
            rows.remove(victim);
        }
        json!({"activities": rows})
    }
}

/// The last `lines` lines of a job's output, cut to the wire cap from the
/// oldest end (escaping can grow one character to six bytes).
fn activity_tail(job: &Job, lines: u64) -> String {
    let (text, _) = job.output();
    let all = splitlines(&text);
    let keep = usize::try_from(lines).unwrap_or(usize::MAX);
    let tail = all[all.len().saturating_sub(keep)..].join("\n");
    let bytes = tail.as_bytes();
    let mut payload: Vec<char> =
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(ACTIVITY_FRAME_CAP)..])
            .chars()
            .collect();
    loop {
        let text: String = payload.iter().collect();
        let size = python_json(&json!({"tail": text})).len();
        if size <= ACTIVITY_FRAME_CAP {
            return text;
        }
        let excess = size - ACTIVITY_FRAME_CAP;
        let keep = payload.len().saturating_sub(excess / 6 + 1).max(1);
        payload.drain(..payload.len() - keep);
    }
}

/// Drop reaped jobs past the history cap, oldest reaped first.
fn prune(jobs: &mut Jobs) {
    let newly_reaped: Vec<String> = jobs
        .all
        .iter()
        .filter(|job| job.is_reaped() && !jobs.reaped.contains(&job.id))
        .map(|job| job.id.clone())
        .collect();
    jobs.reaped.extend(newly_reaped);
    while jobs.reaped.len() > HISTORY_CAP {
        if let Some(oldest) = jobs.reaped.pop_front() {
            jobs.all.retain(|job| job.id != oldest);
        }
    }
    let older = jobs.reaped.len().saturating_sub(FULL_HISTORY);
    for id in jobs.reaped.iter().take(older) {
        if let Some(job) = jobs.all.iter().find(|job| &job.id == id) {
            job.compact(COMPACT_KEEP);
        }
    }
}

/// `PRIME_AGENT_BASH_NO_OUTPUT_WARN_MS` from the kernel environment: unset or
/// unparsable keeps the default, `0` (or negative) disables the warning.
fn no_output_warn(context: &GuardContext) -> Option<Duration> {
    let Some(raw) = context.var("PRIME_AGENT_BASH_NO_OUTPUT_WARN_MS") else {
        return Some(job::DEFAULT_NO_OUTPUT_WARN);
    };
    match raw.trim().parse::<i64>() {
        Ok(ms) if ms <= 0 => None,
        Ok(ms) => Some(Duration::from_millis(ms.unsigned_abs())),
        Err(_) => Some(job::DEFAULT_NO_OUTPUT_WARN),
    }
}

/// `bytes` random bytes from the OS, hex-encoded (`secrets.token_hex`).
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    // The OS random source does not fail on supported platforms; a zeroed
    // token would only weaken forgery resistance of one fence.
    let _ = getrandom::fill(&mut buffer);
    buffer
        .iter()
        .fold(String::with_capacity(bytes * 2), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(all(test, unix))]
mod tests;
