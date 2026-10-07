//! Runner behaviour the kernel's Python suite used to pin through the
//! runtime's internals (the pump, the status reader, the gate, the cargo-lock
//! probe), now pinned on the runner itself.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::{Job, JobEvent, JobTable, SpawnRequest};
use crate::context::GuardContext;
use crate::platform;
use crate::script::Script;

fn context(extra: &[(&str, String)]) -> GuardContext {
    let mut env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    for (name, value) in extra {
        env.insert((*name).to_string(), value.clone());
    }
    GuardContext::new(std::env::temp_dir(), env)
}

fn spawn(table: &JobTable, command: &str, context: GuardContext) -> Arc<Job> {
    table
        .start(&SpawnRequest {
            script: Script::bare(command),
            context,
            kernel_pid: std::process::id(),
            sandbox_prefix: Vec::new(),
        })
        .expect("spawn")
}

/// Every event until the job is reaped.
fn events(job: &Job) -> Vec<JobEvent> {
    let mut all = Vec::new();
    let mut cursor = 0;
    loop {
        let (events, next, done) = job.follow(cursor, Duration::from_secs(10));
        all.extend(events);
        cursor = next;
        if done {
            return all;
        }
    }
}

fn finished(events: &[JobEvent]) -> (i32, String) {
    events
        .iter()
        .find_map(|event| match event {
            JobEvent::Finished {
                exit_code, output, ..
            } => Some((*exit_code, output.to_string())),
            JobEvent::Progress { .. } | JobEvent::Reaped { .. } => None,
        })
        .expect("a finished event")
}

/// Python `test_sentinel_like_output_and_echoed_wrapper_do_not_truncate`: a
/// look-alike marker and an echoed wrapper (`set -x`, the shell's own
/// cmdline) stay output; only this invocation's marker ends it.
#[test]
fn lookalike_markers_and_an_echoed_wrapper_do_not_truncate() {
    let table = JobTable::new();
    let command = "set -x\n\
                   printf '\\036prime-agent-complete:not-this-invocation\\037'\n\
                   if [ -r /proc/$$/cmdline ]; then cat /proc/$$/cmdline; fi\n\
                   printf '\\nafter-sentinel-lookalike\\n'";
    let job = spawn(&table, command, context(&[]));
    let (exit_code, output) = finished(&events(&job));
    assert_eq!(exit_code, 0);
    assert!(output.contains("\u{1e}prime-agent-complete:not-this-invocation\u{1f}"));
    assert!(output.contains("after-sentinel-lookalike"));
}

/// Python `test_cargo_lock_detection_survives_output_chunk_boundary`.
#[test]
fn the_cargo_lock_text_is_found_across_reads() {
    let table = JobTable::new();
    let job = spawn(
        &table,
        "printf 'Blocking waiting for file lock on build direc'; sleep 0.2; printf 'tory'",
        context(&[]),
    );
    let all = events(&job);
    let waits: Vec<_> = all
        .iter()
        .filter_map(|event| match event {
            JobEvent::Progress { msg, fields } if *msg == "cargo_lock_wait" => Some(fields.clone()),
            JobEvent::Progress { .. } | JobEvent::Finished { .. } | JobEvent::Reaped { .. } => None,
        })
        .collect();
    assert_eq!(waits.len(), 1, "{all:?}");
    assert_eq!(waits[0]["bash.wait_reason"], "cargo_build_lock");
}

/// Python `test_delivered_status_wins_when_shell_dies_during_completion` /
/// `..._when_reporter_is_slow`: once the foreground status is delivered, the
/// shell's later death (it waits for `sleep 30 &`) cannot replace it.
#[test]
fn a_delivered_status_wins_over_a_later_shell_death() {
    let table = JobTable::new();
    let job = spawn(&table, "sleep 30 & true", context(&[]));
    let (events_so_far, mut cursor, _) = job.follow(0, Duration::from_secs(10));
    let mut seen = events_so_far;
    while !seen
        .iter()
        .any(|event| matches!(event, JobEvent::Finished { .. }))
    {
        let (more, next, _) = job.follow(cursor, Duration::from_secs(10));
        seen.extend(more);
        cursor = next;
    }
    // The shell itself dies (its background child keeps the group alive).
    let leader = rustix::process::Pid::from_raw(i32::try_from(job.pid).expect("pid")).expect("pid");
    rustix::process::kill_process(leader, rustix::process::Signal::TERM).expect("signal the shell");
    seen.extend(events(&job).into_iter().skip(cursor));
    assert_eq!(finished(&seen).0, 0);
}

/// Python `test_gate_eof_without_journal_prevents_command_execution`: a host
/// that dies between the spawn and the journal never opens the gate, and the
/// command never runs.
#[test]
fn a_closed_gate_never_runs_the_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("ran");
    let script = super::fence::status_script(
        &format!("touch {}", marker.display()),
        "a",
        "b",
        std::path::Path::new("/bin/sh"),
    );
    let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", &script])
        .current_dir(dir.path())
        .env_clear()
        .envs(&env);
    let spawned = platform::spawn(command).expect("spawn");
    let platform::Spawned {
        mut process,
        channel,
        output,
    } = spawned;
    drop(channel);
    drop(output);
    assert_eq!(process.wait(), 127);
    assert!(!marker.exists());
}

/// The journal records the job active at spawn and inactive once its group
/// is reaped (Python `test_env_prefix_and_journal`, the record half).
#[test]
fn the_journal_brackets_the_job() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("journal.jsonl");
    let table = JobTable::new();
    let job = spawn(
        &table,
        "echo hi",
        context(&[
            (
                "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL",
                journal.display().to_string(),
            ),
            ("PRIME_AGENT_KERNEL_OWNER_PID", "4242".to_string()),
        ]),
    );
    let _ = events(&job);
    let records: Vec<serde_json::Value> = std::fs::read_to_string(&journal)
        .expect("journal")
        .lines()
        .map(|line| serde_json::from_str(line).expect("record"))
        .collect();
    let active: Vec<bool> = records
        .iter()
        .map(|record| record["active"].as_bool().expect("active"))
        .collect();
    assert_eq!(active, vec![true, false]);
    assert!(records
        .iter()
        .all(|record| record["pid"] == job.pid && record["ownerPid"] == 4242));
}

/// The kernel's sandbox prepared for `workspace` (writable) with nothing
/// else writable, or `None` (after saying why) where this machine cannot
/// confine.
#[cfg(target_os = "linux")]
fn workspace_sandbox(workspace: &std::path::Path) -> Option<crate::sandbox::JobSandbox> {
    use pa_os_sandbox::{Confinement, NetworkAccess, SandboxError, SandboxPaths, SandboxPolicy};
    let policy = SandboxPolicy {
        confinement: Confinement::WorkspaceWrite,
        network: NetworkAccess::Denied,
        writable_roots: Vec::new(),
    };
    let paths = SandboxPaths {
        workspace: workspace.to_path_buf(),
        scratch: Vec::new(),
    };
    match pa_os_sandbox::prepare(&policy, &paths) {
        Ok(prepared) => Some(crate::sandbox::JobSandbox::Confined(Arc::new(prepared))),
        Err(SandboxError::Unsupported { reason }) => {
            eprintln!("skipping the confined-spawn test: {reason}");
            None
        }
        Err(error) => panic!("sandbox setup failed: {error}"),
    }
}

/// The host spawns `bash()` commands (and the guards' probes) itself, so it
/// applies the kernel's sandbox to each: a confined command writes its
/// workspace and nothing outside it, and so does a probe.
#[cfg(target_os = "linux")]
#[test]
fn a_confined_spawn_writes_only_inside_its_roots() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("root");
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&outside).expect("outside");
    let Some(sandbox) = workspace_sandbox(&workspace) else {
        return;
    };
    let table = JobTable::new();
    table.set_sandbox(sandbox.clone());
    let env = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let context = GuardContext::new(&workspace, env).with_sandbox(sandbox);
    let command = format!(
        "printf in > '{}'; printf out > '{}'",
        workspace.join("job.txt").display(),
        outside.join("job.txt").display()
    );
    let job = table
        .start(&SpawnRequest {
            script: Script::bare(&command),
            context: context.clone(),
            kernel_pid: std::process::id(),
            sandbox_prefix: Vec::new(),
        })
        .expect("spawn");
    let (exit_code, output) = finished(&events(&job));
    let probe = crate::probe::run_probe(
        &context,
        &format!("printf out > '{}'", outside.join("probe.txt").display()),
        &workspace,
        crate::probe::ProbeLimits {
            timeout: Duration::from_secs(10),
            kill_grace: Duration::from_secs(1),
            output_cap: None,
        },
    );
    assert_eq!(
        (
            exit_code,
            output.contains("Permission denied"),
            workspace.join("job.txt").exists(),
            outside.join("job.txt").exists(),
            matches!(probe, crate::probe::ProbeOutcome::Finished { status: Some(code), .. } if code != 0),
            outside.join("probe.txt").exists(),
        ),
        (1, true, true, false, true, false)
    );
}

/// A sandbox the setting asks for but this machine cannot enforce starts
/// nothing, probes included.
#[test]
fn an_unavailable_sandbox_starts_nothing() {
    let table = JobTable::new();
    let sandbox =
        crate::sandbox::JobSandbox::Unavailable("OS sandbox unavailable: test".to_string());
    table.set_sandbox(sandbox.clone());
    let started = table.start(&SpawnRequest {
        script: Script::bare("true"),
        context: context(&[]).with_sandbox(sandbox.clone()),
        kernel_pid: std::process::id(),
        sandbox_prefix: Vec::new(),
    });
    assert_eq!(
        started.err().map(|error| error.to_string()),
        Some("bash(): OS sandbox unavailable: test".to_string())
    );
    let probe = crate::probe::run_probe(
        &context(&[]).with_sandbox(sandbox),
        "true",
        std::path::Path::new("/"),
        crate::probe::ProbeLimits {
            timeout: Duration::from_secs(10),
            kill_grace: Duration::from_secs(1),
            output_cap: None,
        },
    );
    assert_eq!(probe, crate::probe::ProbeOutcome::Unavailable);
}

/// The fence's `printf` is the shell builtin in bash (no process per
/// command), reached so a user function or alias named `printf` cannot
/// swallow the marker or the status: the result still arrives at foreground
/// completion, not when the background child lets the shell exit.
#[test]
fn a_printf_function_or_alias_cannot_swallow_the_fence() {
    let Some(bash) = crate::shell::which("bash", Some("/usr/bin:/bin")) else {
        eprintln!("no bash; skipping");
        return;
    };
    let table = JobTable::new();
    let command = "printf() { echo hijacked; }\n\
                   shopt -s expand_aliases\n\
                   alias printf='echo aliased'\n\
                   echo out\n\
                   sleep 30 >/dev/null 2>&1 &";
    let job = spawn(
        &table,
        command,
        context(&[("SHELL", bash.to_string_lossy().into_owned())]),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut cursor = 0;
    let mut result = None;
    while result.is_none() && std::time::Instant::now() < deadline {
        let (events, next, _) = job.follow(cursor, Duration::from_secs(1));
        cursor = next;
        result = events.iter().find_map(|event| match event {
            JobEvent::Finished {
                exit_code, output, ..
            } => Some((*exit_code, output.to_string())),
            JobEvent::Progress { .. } | JobEvent::Reaped { .. } => None,
        });
    }
    job.kill_now();
    assert_eq!(result, Some((0, "out\n".to_string())));
}

/// The result is the output as of the fence; output written after it (an
/// EXIT trap) stays out of the result but in the job's output, whether it
/// lands before or after the result is read.
#[test]
fn output_past_the_fence_stays_out_of_the_result() {
    let table = JobTable::new();
    let job = spawn(&table, "trap 'echo late' EXIT\necho early", context(&[]));
    let events = events(&job);
    assert_eq!(finished(&events), (0, "early\n".to_string()));
    assert_eq!(job.output(), ("early\nlate\n".to_string(), 11));
}

/// An old reaped job keeps only its buffer's tail: the activity tail it
/// serves and its byte count are the ones the whole buffer gave.
#[test]
fn a_compacted_job_serves_the_same_activity_tail() {
    let table = JobTable::new();
    let job = spawn(
        &table,
        "i=0; while [ $i -lt 30000 ]; do echo \"line $i of the output\"; i=$((i+1)); done",
        context(&[]),
    );
    let _ = events(&job);
    let tail = |table: &JobTable| table.activity("tail", Some(&job.id), &serde_json::json!(200));
    let before = tail(&table);
    let bytes = job.output_bytes();
    job.compact(super::COMPACT_KEEP);
    assert!(job.output().0.len() < 2 * super::COMPACT_KEEP);
    assert_eq!(tail(&table), before);
    assert_eq!(job.output_bytes(), bytes);
}

/// Only the newest reaped jobs keep their whole buffer.
#[test]
fn older_reaped_jobs_are_compacted() {
    let table = JobTable::new();
    let command = "head -c 200000 /dev/zero | tr '\\0' x";
    let jobs: Vec<_> = (0..super::FULL_HISTORY + 2)
        .map(|_| {
            let job = spawn(&table, command, context(&[]));
            let _ = events(&job);
            job
        })
        .collect();
    let _ = table.inventory(1); // prunes
    let sizes: Vec<bool> = jobs
        .iter()
        .map(|job| job.output().0.len() > super::COMPACT_KEEP + 100)
        .collect();
    let mut expected = vec![false; 2];
    expected.extend(vec![true; super::FULL_HISTORY]);
    assert_eq!(sizes, expected);
    assert!(jobs.iter().all(|job| job.output_bytes() == 200_000));
}
