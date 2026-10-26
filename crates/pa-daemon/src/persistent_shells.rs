//! The kernel's persistent `bash()` shells (prime-agent-runtime `rlm/bash.py`,
//! `bash(..., persistent=True)`): every session's shells live in one home,
//! `<agent dir>/persistent-shells/`, as a `<id>.json` record assigned to the
//! session that started it, plus the shell's `<id>.out` and `<id>.rc`. The
//! daemon reads the records to list each saved session's live shells and every
//! shell machine-wide, reads a shell's output, and kills one on request.
//! Nothing in the session lifecycle kills them: a deleted session's shells keep
//! running as orphans, still assigned to it.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use pa_core::platform::process::{Signal, process_group_exists, signal_process_group};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{response_failure, response_line, response_success};

const HOME_DIR: &str = "persistent-shells";
/// How long a shell may exit on its own after SIGTERM before its group gets SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// The kernel activity rows cap the command at the same length.
const COMMAND_CAP: usize = 512;
/// The output read serves its lines from at most this many trailing bytes.
const TAIL_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shell {
    id: String,
    pid: u32,
    start_id: String,
    command: String,
    started_at: String,
    session_id: String,
}

fn shell_file(agent_dir: &Path, id: &str, kind: &str) -> PathBuf {
    agent_dir.join(HOME_DIR).join(format!("{id}.{kind}"))
}

fn registered_shells(agent_dir: &Path) -> Vec<Shell> {
    let Ok(entries) = std::fs::read_dir(agent_dir.join(HOME_DIR)) else {
        return Vec::new();
    };
    let mut shells: Vec<Shell> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|raw| serde_json::from_slice(&raw).ok())
        .collect();
    shells.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    shells
}

fn find_shell(agent_dir: &Path, id: &str) -> Option<Shell> {
    registered_shells(agent_dir)
        .into_iter()
        .find(|shell| shell.id == id)
}

/// Whether the shell's pid still runs under the recorded start identity.
fn runs_as_registered(shell: &Shell) -> bool {
    matches!(crate::lease::is_process_alive(shell.pid), Ok(true))
        && crate::lease::get_process_start_id(shell.pid).as_deref() == Some(shell.start_id.as_str())
}

/// An extant group with no verified leader is not safely attributable to this record.
fn group_unverified(shell: &Shell) -> bool {
    !runs_as_registered(shell) && process_group_exists(shell.pid as i32)
}

/// One shell as a row with the kernel bash activity row field names.
fn activity_row(shell: &Shell, status: &str) -> Value {
    json!({
        "id": shell.id,
        "command": shell.command.chars().take(COMMAND_CAP).collect::<String>(),
        "pid": shell.pid,
        "startedAt": shell.started_at,
        "status": status,
        "persistent": true,
    })
}

/// The live persistent shells by assigned session id: a recycled pid or an
/// unreadable identity never counts as live.
pub(crate) fn live_shell_rows_by_session(agent_dir: &Path) -> HashMap<String, Vec<Value>> {
    let mut rows: HashMap<String, Vec<Value>> = HashMap::new();
    for shell in registered_shells(agent_dir)
        .into_iter()
        .filter(runs_as_registered)
    {
        let row = activity_row(&shell, "running");
        rows.entry(shell.session_id).or_default().push(row);
    }
    rows
}

/// Every registered shell with its assigned `sessionId`: running while its
/// leader runs as registered, unverified when a group remains without that
/// proof, otherwise finished with its recorded exit code (if any). Its
/// `durationMs` runs from `startedAt` to now, or to the exit code's write
/// once finished.
pub(crate) fn shell_rows(agent_dir: &Path) -> Vec<Value> {
    registered_shells(agent_dir)
        .iter()
        .map(|shell| {
            let running = runs_as_registered(shell);
            let status = if running {
                "running"
            } else if group_unverified(shell) {
                "unverified"
            } else {
                "finished"
            };
            let mut row = activity_row(shell, status);
            row["sessionId"] = json!(shell.session_id);
            let rc = shell_file(agent_dir, &shell.id, "rc");
            let ended_ms = if status == "finished" {
                row["exitCode"] = json!(
                    std::fs::read_to_string(&rc)
                        .ok()
                        .and_then(|rc| rc.trim().parse::<i64>().ok())
                );
                std::fs::metadata(&rc)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .and_then(|since| u64::try_from(since.as_millis()).ok())
            } else {
                Some(crate::util::now_ms())
            };
            row["durationMs"] = json!(
                crate::util::iso_to_unix_ms(&shell.started_at)
                    .zip(ended_ms)
                    .map(|(started, ended)| ended.saturating_sub(started))
            );
            row
        })
        .collect()
}

/// The newest `lines` lines of the shell's output (read from its last
/// [`TAIL_BYTES`]); `None` for an unknown shell. The kernel creates the `.out`
/// before it writes the record, so a failed read is an error, never an empty tail.
pub(crate) fn output_tail(
    agent_dir: &Path,
    id: &str,
    lines: usize,
) -> Result<Option<String>, String> {
    let Some(shell) = find_shell(agent_dir, id) else {
        return Ok(None);
    };
    let bytes = read_tail(&shell_file(agent_dir, &shell.id, "out"))
        .map_err(|error| format!("Could not read persistent shell output: {error}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    Ok(Some(all[all.len().saturating_sub(lines)..].join("\n")))
}

/// The shell's output as-is (its last [`TAIL_BYTES`], newlines preserved),
/// for a full `.output()` read; `None` for an unknown shell. Unlike
/// [`output_tail`], it does not collapse to whole lines, so a trailing
/// newline survives.
pub(crate) fn output_raw(agent_dir: &Path, id: &str) -> Result<Option<String>, String> {
    let Some(shell) = find_shell(agent_dir, id) else {
        return Ok(None);
    };
    let bytes = read_tail(&shell_file(agent_dir, &shell.id, "out"))
        .map_err(|error| format!("Could not read persistent shell output: {error}"))?;
    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
}

fn read_tail(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Kill a shell only while the recorded leader identity still proves ownership.
/// A durable leaderless group may belong to a reused PGID: refuse and retain it.
pub(crate) fn kill_shell(agent_dir: &Path, id: &str) -> Result<bool, String> {
    let Some(shell) = find_shell(agent_dir, id) else {
        return Ok(false);
    };
    if !runs_as_registered(&shell) {
        return if group_unverified(&shell) {
            Err("Cannot verify persistent shell group ownership; no signal sent".to_string())
        } else {
            Ok(true)
        };
    }
    if !signal_process_group(shell.pid as i32, Signal::Term) {
        return Err("Could not signal persistent shell group".to_string());
    }
    // The grace belongs to the whole group: a leader that exits on TERM must not cut short
    // its members' own TERM handling.
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if !process_group_exists(shell.pid as i32) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    kill_group(&shell)?;
    Ok(true)
}

/// The KILL after a verified TERM. A leader that exited during the grace still names the group
/// verified before the TERM: a PGID is not reused while its group has members. A leader pid
/// that runs again under another start identity was recycled, so its group is refused.
fn kill_group(shell: &Shell) -> Result<(), String> {
    let leader_exited = matches!(crate::lease::is_process_alive(shell.pid), Ok(false));
    if !runs_as_registered(shell) && !leader_exited {
        return if group_unverified(shell) {
            Err(
                "Cannot verify persistent shell group ownership; no further signal sent"
                    .to_string(),
            )
        } else {
            Ok(())
        };
    }
    if signal_process_group(shell.pid as i32, Signal::Kill)
        || !process_group_exists(shell.pid as i32)
    {
        Ok(())
    } else {
        Err("Could not signal persistent shell group".to_string())
    }
}

/// A fresh persistent-shell id. Opaque: the reader keys on the record's `id`
/// field, never on a pid-derived name, so a uuid avoids the spawn-time
/// chicken-and-egg of naming files after a pid we do not have yet.
#[cfg(unix)]
fn new_shell_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Single-quote a value for a POSIX shell (`'…'`, with embedded quotes escaped).
#[cfg(unix)]
fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Spawn a detached persistent shell assigned to `session_id`: a `setsid`'d
/// `<shell> -c <script>` whose stdout+stderr go to `<id>.out` and whose own
/// exit code lands in `<id>.rc`, recorded in `<id>.json`. It is deliberately
/// neither a kernel bash job nor orphan-journaled, so nothing in the
/// kernel/worker/daemon lifecycle kills it — only its own exit or
/// [`kill_shell`]. `script` is the shell body to run; `command` is the
/// user-facing string kept in the record. Returns the fields the kernel
/// handle needs (`id`, `pid`, `outputPath`, `startedAt`).
#[cfg(unix)]
pub(crate) fn spawn(
    agent_dir: &Path,
    session_id: &str,
    shell: &str,
    script: &str,
    command: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<Value, String> {
    use std::process::{Command, Stdio};

    let home = agent_dir.join(HOME_DIR);
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("Could not create the persistent-shell home: {error}"))?;
    let id = new_shell_id();
    let out_path = shell_file(agent_dir, &id, "out");
    let rc_path = shell_file(agent_dir, &id, "rc");

    // The reader treats a missing `.out` as an error, so it must exist before
    // the record a lister can find (comment on `output_tail`).
    let out = std::fs::File::create(&out_path)
        .map_err(|error| format!("Could not create the persistent shell output: {error}"))?;
    let errout = out
        .try_clone()
        .map_err(|error| format!("Could not open the persistent shell output: {error}"))?;

    // The shell records its own exit code so the group's end survives the
    // worker that spawned it.
    let wrapped = format!(
        "{script}\n__pa_rc=$?\nprintf '%s' \"$__pa_rc\" > {rc}\nexit \"$__pa_rc\"\n",
        rc = single_quote(&rc_path.to_string_lossy()),
    );

    let mut spawner = Command::new(shell);
    spawner.arg("-c").arg(&wrapped);
    spawner.current_dir(cwd);
    spawner.env_clear();
    for (key, value) in env {
        spawner.env(key, value);
    }
    spawner.stdin(Stdio::null());
    spawner.stdout(Stdio::from(out));
    spawner.stderr(Stdio::from(errout));
    // setsid: its own session and process group, reparented to init when the
    // worker exits, so `signal_process_group(pid, …)` reaches the whole group.
    pa_core::platform::process::set_new_session(&mut spawner);

    let child = spawner
        .spawn()
        .map_err(|error| format!("Could not spawn the persistent shell: {error}"))?;
    let pid = child.id();
    // Reap the child when it exits (no zombie) without ever killing it; if the
    // worker dies first the shell reparents to init, which reaps it instead.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });

    let start_id = pa_types::platform::process::process_start_id(pid).unwrap_or_default();
    let started_at = crate::util::now_iso();
    let record = json!({
        "id": id,
        "pid": pid,
        "startId": start_id,
        "command": command,
        "startedAt": started_at,
        "sessionId": session_id,
    });
    std::fs::write(shell_file(agent_dir, &id, "json"), record.to_string())
        .map_err(|error| format!("Could not write the persistent shell record: {error}"))?;

    Ok(json!({
        "id": id,
        "pid": pid,
        "outputPath": out_path.to_string_lossy(),
        "startedAt": started_at,
    }))
}

/// Persistent shells are POSIX-only (they rely on `setsid` session detachment
/// and process-group signalling).
#[cfg(not(unix))]
pub(crate) fn spawn(
    _agent_dir: &Path,
    _session_id: &str,
    _shell: &str,
    _script: &str,
    _command: &str,
    _cwd: &Path,
    _env: &[(String, String)],
) -> Result<Value, String> {
    Err("Persistent shells are POSIX-only".to_string())
}

/// One persistent-shell supervisor arm, run off the runtime (file reads and
/// the kill's grace block): `None` answers an unknown shell.
pub(crate) async fn respond(
    command_id: &str,
    type_name: &str,
    run: impl FnOnce() -> Result<Option<Value>, String> + Send + 'static,
) -> Value {
    let response = match tokio::task::spawn_blocking(run).await {
        Ok(Ok(Some(data))) => response_success(Some(command_id), type_name, Some(data)),
        Ok(Ok(None)) => response_failure(
            Some(command_id),
            type_name,
            "Unknown persistent shell",
            None,
        ),
        Ok(Err(error)) => response_failure(Some(command_id), type_name, &error, None),
        Err(error) => response_failure(Some(command_id), type_name, &error.to_string(), None),
    };
    response_line(&response)
}

#[cfg(all(test, unix))]
mod test_support {
    use std::path::Path;
    use std::process::{Child, Command};

    /// A setsid'd `sleep` registered in `agent_dir`'s persistent-shell home,
    /// assigned to `session_id`, under `start_id` (`None`: its real start identity).
    pub(super) fn registered_sleeper(
        agent_dir: &Path,
        session_id: &str,
        start_id: Option<&str>,
    ) -> Child {
        let mut command = Command::new("sleep");
        command.arg("60");
        pa_core::platform::process::set_new_session(&mut command);
        let child = command.spawn().expect("spawn sleeper");
        let start_id = start_id.map_or_else(
            || crate::lease::get_process_start_id(child.id()).expect("start id"),
            str::to_string,
        );
        register(agent_dir, child.id(), &start_id, session_id);
        child
    }

    /// Write one record for shell `pid` under `start_id`, assigned to
    /// `session_id`; returns its id.
    pub(super) fn register(agent_dir: &Path, pid: u32, start_id: &str, session_id: &str) -> String {
        let home = agent_dir.join(super::HOME_DIR);
        std::fs::create_dir_all(&home).expect("persistent-shell home");
        let id = format!("{pid:032}");
        let record = serde_json::json!({
            "id": id,
            "pid": pid,
            "startId": start_id,
            "command": "sleep 60",
            "startedAt": "2026-01-01T00:00:00+00:00",
            "sessionId": session_id,
        });
        std::fs::write(home.join(format!("{id}.json")), record.to_string()).expect("write record");
        id
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command};

    use nix::sys::signal::{Signal as NixSignal, kill};
    use nix::unistd::Pid;

    use super::test_support::{register, registered_sleeper};
    use super::*;

    /// Send `child` our own SIGTERM and return the signal it died by: an
    /// earlier SIGKILL is already pending and wins.
    fn fatal_signal_after_own_term(mut child: Child) -> Option<i32> {
        let _ = kill(Pid::from_raw(child.id() as i32), NixSignal::SIGTERM);
        child.wait().unwrap().signal()
    }

    /// A registered `sleep` leading its own process group, plus one member in
    /// that group.
    fn registered_group(agent_dir: &std::path::Path) -> (Child, Child, String) {
        let mut leader_command = Command::new("sleep");
        leader_command.arg("60");
        pa_core::platform::process::set_new_process_group(&mut leader_command);
        let leader = leader_command.spawn().unwrap();
        let start_id = crate::lease::get_process_start_id(leader.id()).unwrap();
        let id = register(agent_dir, leader.id(), &start_id, "s1");
        let member = Command::new("sleep")
            .arg("60")
            .process_group(leader.id() as i32)
            .spawn()
            .unwrap();
        (leader, member, id)
    }

    #[test]
    fn recycled_pid_is_neither_reported_nor_killed() {
        let root = tempfile::tempdir().unwrap();
        let stranger = registered_sleeper(root.path(), "s1", Some("proc:not-this-process"));
        let id = format!("{:032}", stranger.id());

        assert!(live_shell_rows_by_session(root.path()).is_empty());
        assert_eq!(shell_rows(root.path())[0]["status"], "unverified");
        assert!(kill_shell(root.path(), &id).is_err());

        assert_eq!(
            fatal_signal_after_own_term(stranger),
            Some(NixSignal::SIGTERM as i32),
            "never signalled"
        );
    }

    #[test]
    fn kill_stops_the_whole_group_and_leaves_the_shell_listed_as_finished() {
        let root = tempfile::tempdir().unwrap();
        let (mut leader, member, id) = registered_group(root.path());
        std::fs::write(shell_file(root.path(), &id, "out"), "line\n").unwrap();

        let _outcome = kill_shell(root.path(), &id);

        assert!(!leader.wait().unwrap().success(), "the leader was killed");
        let member_status = member.wait_with_output().unwrap().status;
        assert_eq!(
            member_status.signal(),
            Some(NixSignal::SIGTERM as i32),
            "the group's TERM reached the member"
        );
        assert_eq!(shell_rows(root.path())[0]["status"], "finished");
        assert_eq!(
            output_tail(root.path(), &id, 1),
            Ok(Some("line".to_string()))
        );
        assert_eq!(kill_shell(root.path(), "unknown"), Ok(false));
    }

    #[test]
    fn kill_escalates_to_a_member_that_outlives_its_leaders_term() {
        use std::io::BufRead;
        let root = tempfile::tempdir().unwrap();
        let (mut leader, member, id) = registered_group(root.path());
        let mut stubborn = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 60"])
            .process_group(leader.id() as i32)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(stubborn.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();

        assert_eq!(kill_shell(root.path(), &id), Ok(true));

        leader.wait().unwrap();
        member.wait_with_output().unwrap();
        assert_eq!(
            stubborn.wait().unwrap().signal(),
            Some(NixSignal::SIGKILL as i32),
            "the member that ignored TERM got the group's KILL"
        );
    }

    #[test]
    fn a_leader_exiting_on_term_leaves_its_members_the_full_grace() {
        use std::io::BufRead;
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("cleaned-up");
        let (leader, member, id) = registered_group(root.path());
        let mut cleanup = Command::new("sh")
            .args([
                "-c",
                "trap 'sleep 1; touch \"$0\"; exit 0' TERM; echo ready; while :; do sleep 0.05; done",
            ])
            .arg(&marker)
            .process_group(leader.id() as i32)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(cleanup.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        // Reaped as the kernel reaps them: only live members keep the group.
        let reapers: Vec<_> = [leader, member, cleanup]
            .into_iter()
            .map(|mut child| std::thread::spawn(move || child.wait().unwrap()))
            .collect();

        assert_eq!(kill_shell(root.path(), &id), Ok(true));

        let statuses: Vec<_> = reapers.into_iter().map(|r| r.join().unwrap()).collect();
        assert!(
            marker.exists() && statuses[2].success(),
            "the member finished its TERM cleanup after the leader exited: {statuses:?}"
        );
    }

    #[test]
    fn exited_leaders_group_is_unverified_and_never_signalled() {
        let root = tempfile::tempdir().unwrap();
        let (mut leader, member, id) = registered_group(root.path());
        leader.kill().unwrap();
        leader.wait().unwrap();
        // The same observable state also describes an unrelated group after
        // durable PID/PGID reuse. No old leader start-id can distinguish them.
        let record = shell_file(root.path(), &id, "json");
        let mut stale: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        stale["startId"] = json!("proc:old-unrelated-leader");
        std::fs::write(&record, stale.to_string()).unwrap();

        assert_eq!(shell_rows(root.path())[0]["status"], "unverified");
        assert!(kill_shell(root.path(), &id).is_err());
        assert!(record.exists());
        assert_eq!(
            fatal_signal_after_own_term(member),
            Some(NixSignal::SIGTERM as i32),
            "no earlier KILL reached the unverified group"
        );
    }

    #[test]
    fn pid_recycled_during_the_grace_is_not_killed() {
        let root = tempfile::tempdir().unwrap();
        let mut stranger = registered_sleeper(root.path(), "s1", Some("proc:not-this-process"));
        let shells = registered_shells(root.path());

        assert!(kill_group(&shells[0]).is_err());
        assert!(signal_process_group(stranger.id() as i32, Signal::Term));

        let status = stranger.wait().unwrap();
        assert_eq!(
            status.signal(),
            Some(NixSignal::SIGTERM as i32),
            "no SIGKILL came first"
        );
    }

    #[test]
    fn shell_rows_list_every_shell_with_its_session_and_the_output_tail_reads_the_newest_lines() {
        let root = tempfile::tempdir().unwrap();
        let mut sleeper = registered_sleeper(root.path(), "s1", None);
        let mut done = Command::new("true").spawn().unwrap();
        done.wait().unwrap();
        let finished = register(root.path(), done.id(), "proc:0", "gone");
        std::fs::write(shell_file(root.path(), &finished, "rc"), "3\n").unwrap();
        std::fs::write(
            shell_file(root.path(), &finished, "out"),
            "one\ntwo\nthree\n",
        )
        .unwrap();

        let rows = shell_rows(root.path());

        let row = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap().clone();
        let running = row(&format!("{:032}", sleeper.id()));
        assert_eq!(
            (
                &running["status"],
                &running["sessionId"],
                running.get("exitCode")
            ),
            (&json!("running"), &json!("s1"), None)
        );
        let done_row = row(&finished);
        assert_eq!(
            (
                &done_row["status"],
                &done_row["sessionId"],
                &done_row["exitCode"]
            ),
            (&json!("finished"), &json!("gone"), &json!(3))
        );
        assert!(running["durationMs"].is_u64() && done_row["durationMs"].is_u64());
        assert_eq!(
            output_tail(root.path(), &finished, 2),
            Ok(Some("two\nthree".to_string()))
        );
        assert_eq!(output_tail(root.path(), "unknown", 2), Ok(None));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[test]
    fn an_unreadable_output_file_is_an_error_not_an_empty_tail() {
        let root = tempfile::tempdir().unwrap();
        let mut sleeper = registered_sleeper(root.path(), "s1", None);
        let id = format!("{:032}", sleeper.id());
        std::fs::create_dir(shell_file(root.path(), &id, "out")).unwrap();

        let tail = output_tail(root.path(), &id, 5);

        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert!(tail.is_err(), "{tail:?}");
    }

    #[test]
    fn spawn_detaches_writes_the_record_and_round_trips_through_the_reader() {
        let root = tempfile::tempdir().unwrap();
        let spawned = spawn(
            root.path(),
            "s1",
            "sh",
            "echo hi; sleep 60",
            "echo hi; sleep 60",
            root.path(),
            &[],
        )
        .expect("spawn persistent shell");
        let id = spawned["id"].as_str().unwrap().to_string();
        let pid = spawned["pid"].as_u64().unwrap() as u32;

        // The record and output exist; the reader lists it as this session's
        // running shell and reads its tail — proving the write/read contract.
        assert!(shell_file(root.path(), &id, "json").exists());
        assert!(
            shell_file(root.path(), &id, "out").exists(),
            "the .out exists before the record"
        );
        let mut waited = 0;
        while output_tail(root.path(), &id, 1) != Ok(Some("hi".to_string())) && waited < 200 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            waited += 1;
        }
        let rows = shell_rows(root.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (&rows[0]["status"], &rows[0]["sessionId"]),
            (&json!("running"), &json!("s1"))
        );
        assert_eq!(output_tail(root.path(), &id, 1), Ok(Some("hi".to_string())));

        // It is a real detached group the reader can end.
        assert!(process_group_exists(pid as i32));
        assert_eq!(kill_shell(root.path(), &id), Ok(true));
    }
}
