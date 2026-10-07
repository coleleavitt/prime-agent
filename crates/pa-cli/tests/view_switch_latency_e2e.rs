// large_futures: stack-resident futures on hot paths by design.
// too_many_lines: style gate, not correctness. Casts: 64-bit targets;
// narrowing sits at bounded OS/protocol boundaries.
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Latency regression guard for the operators' agents-view round trip
//! (operator ruling 2026-09-24: the Esc handoff took close to 5-10
//! seconds): the handoff is a fresh surface attaching to the live session
//! — a daemon `attach` plus the client-side rebuild. This test times that
//! second surface under a seeded store and asserts a sub-second round trip.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pa_types::daemon::DaemonCommand;

/// A healthy attach is tens of milliseconds; one full second is the regression ceiling on an
/// idle box.
const VIEW_SWITCH_CEILING: Duration = Duration::from_millis(1_000);

/// A loaded box slows every attach, the trivial one included: the seeded round trip may take
/// this many times the control's (a two-message session with no children, attached the same
/// way right after). The regression class does work proportional to the seeded store, which
/// the control does not have, so it still lands far past this multiple.
const CONTROL_FACTOR: u32 = 5;

/// Enough rows that the context-tree cache warm does real work, without
/// doubling the daemon-side guard's fixture cost (box-shaped per the
/// operator's real store: 66 children).
const SEEDED_CHILDREN: usize = 60;
const SEEDED_MESSAGES_PER_CHILD: usize = 50;
/// The transcript the round trip re-attaches to: a multi-hundred-message
/// session file (the snapshot ships the whole history).
const SEEDED_TRANSCRIPT_MESSAGES: usize = 400;

struct Supervisor {
    child: Child,
    socket: PathBuf,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        graceful_shutdown(&self.socket);
        pa_core::platform::process_tree::kill_child_tree(&mut self.child);
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Stop the daemon on `socket` by protocol; kill the child when it fails.
fn graceful_shutdown(socket: &Path) {
    use std::io::{BufRead, BufReader, Write};
    let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    let mut hello = String::new();
    let _ = reader.read_line(&mut hello); // daemon_hello
    let command = serde_json::json!({
        "type": "command",
        "id": "test-shutdown",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let Ok(mut line) = serde_json::to_string(&command) else {
        return;
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() {
        return;
    }
    let _ = writer.flush();
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)));
    let mut response = String::new();
    let _ = reader.read_line(&mut response);
}

#[allow(clippy::zombie_processes)]
fn spawn_supervisor(dir: &Path) -> Supervisor {
    let socket = dir.join("daemon.sock");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(&socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for var in [
        pa_daemon::worker::WORKER_ROLE_ENV,
        pa_daemon::worker::WORKER_TOKEN_ENV,
        pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
        pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
        pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
        pa_daemon::worker::WORKER_SOCKET_ENV,
        pa_daemon::worker::WORKER_INSTANCE_ID_ENV,
        pa_daemon::worker::WORKER_SCRIPT_ENV,
    ] {
        command.env_remove(var);
    }
    command.env(
        pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
        "15000",
    );
    let child = command.spawn().expect("spawn prime-agent --mode daemon");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if socket.exists() {
            return Supervisor { child, socket };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

/// Create a live session FROM a pre-written session file (the
/// operator-scale attach snapshot): the worker opens the file's history.
async fn create_session_from_file_via_daemon(
    socket: &Path,
    script_path: &Path,
    script: &serde_json::Value,
    cwd: &Path,
    session_dir: &Path,
    session_path: &Path,
) -> String {
    std::fs::write(script_path, script.to_string()).expect("write script");
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(socket)
        .await
        .expect("connect supervisor");
    let data = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: Some(session_path.display().to_string()),
            continue_recent: None,
            no_session: None,
            name: None,
            config: Some(serde_json::json!({
                "cwd": cwd.display().to_string(),
                "sessionDir": session_dir.display().to_string(),
                "script": script_path.display().to_string(),
            })),
            telemetry_disabled: None,
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: serde_json::Map::default(),
        })
        .await
        .expect("create session from file");
    client.close();
    data.get("activeSessionId")
        .or_else(|| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .expect("session id")
        .to_string()
}

/// Seed one persisted child session file (a version-3 header plus a parent-chained message run).
fn seed_child_session(dir: &Path, child_id: &str) {
    std::fs::create_dir_all(dir).expect("child dir");
    let file = dir.join(format!("{child_id}.jsonl"));
    let mut content = String::new();
    content.push_str(
        &serde_json::json!({
            "type": "session", "version": 3, "id": child_id,
            "timestamp": "2024-01-01T00:00:00.000Z", "cwd": "/tmp",
        })
        .to_string(),
    );
    content.push('\n');
    let mut parent_id: Option<String> = None;
    for index in 0..SEEDED_MESSAGES_PER_CHILD {
        let entry_id = format!("{child_id}-m{index}");
        content.push_str(
            &serde_json::json!({
                "type": "message",
                "id": entry_id,
                "parentId": parent_id,
                "timestamp": "2024-01-01T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": "seeded child turn",
                    "usage": {
                        "input": 1000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 1100,
                        "cost": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "total": 2 },
                    },
                },
            })
            .to_string(),
        );
        content.push('\n');
        parent_id = Some(entry_id);
    }
    std::fs::write(&file, content).expect("write seeded child session");
}

/// The test's interactive options: a headless chat attach to the live
/// session (the same surface the agents-view Esc handoff reopens).
fn chat_options(
    socket: &Path,
    dir: &Path,
    session_dir: &Path,
    script_path: &Path,
    session: pa_tui::interactive::SessionSelection,
) -> pa_tui::interactive::InteractiveOptions {
    pa_tui::interactive::InteractiveOptions {
        sandbox_mode: None,
        resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
        initial_plan_mode: false,
        models: None,
        socket_path: socket.to_path_buf(),
        cwd: dir.to_path_buf(),
        session_dir: Some(session_dir.to_path_buf()),
        script_path: Some(script_path.to_path_buf()),
        model_selection: pa_tui::interactive::ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session,
        show_images: true,
        fullscreen_mouse: true,
        initial_message: None,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

#[tokio::test]
async fn agents_view_round_trip_reattaches_under_the_ceiling() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let supervisor = spawn_supervisor(dir.path());

    // The multi-hundred-message transcript, written directly (the
    // operator-scale attach snapshot ships the whole history).
    let durable = format!("01a0vs-{:012x}", u64::from(std::process::id()));
    let transcript_path = session_dir.join(format!("{durable}.jsonl"));
    {
        let filler = "a".repeat(1024);
        let mut content = String::new();
        content.push_str(
            &serde_json::json!({
                "type": "session", "version": 3, "id": durable,
                "timestamp": "2024-01-01T00:00:00.000Z", "cwd": "/tmp",
            })
            .to_string(),
        );
        content.push('\n');
        let mut parent_id = String::new();
        for index in 0..SEEDED_TRANSCRIPT_MESSAGES {
            let entry_id = format!("{durable}-m{index}");
            let is_assistant = index % 2 == 1;
            content.push_str(
                &serde_json::json!({
                    "type": "message",
                    "id": entry_id,
                    "parentId": parent_id,
                    "timestamp": "2024-01-01T00:00:00.000Z",
                    "message": {
                        "role": if is_assistant { "assistant" } else { "user" },
                        "content": if is_assistant { &filler } else { "question" },
                        "usage": if is_assistant {
                            serde_json::json!({
                                "input": 1000, "output": 100, "cacheRead": 0, "cacheWrite": 0,
                                "totalTokens": 1100,
                                "cost": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "total": 2 },
                            })
                        } else { serde_json::Value::Null },
                    },
                })
                .to_string(),
            );
            content.push('\n');
            parent_id = entry_id;
        }
        std::fs::write(&transcript_path, content).expect("write seeded transcript");
    }
    let script = serde_json::json!({ "responses": [
        { "text": "hello from scripted", "delayMs": 20 },
    ] });
    let script_path = dir.path().join("script.json");
    let active = create_session_from_file_via_daemon(
        &supervisor.socket,
        &script_path,
        &script,
        dir.path(),
        &session_dir,
        &transcript_path,
    )
    .await;

    // A seeded artifact tree: the attach warm does real background work.
    let artifacts_root = agent_dir.join("session-artifacts").join(&durable);
    for index in 0..SEEDED_CHILDREN {
        let child_id = format!("viewswitch-child-{index:03}");
        seed_child_session(&artifacts_root.join(format!("sub-{child_id}")), &child_id);
    }

    // The first chat surface: attach, one scripted turn, idle.
    let first_plan = pa_tui::interactive::HeadlessPlan {
        steps: vec![
            pa_tui::interactive::HeadlessStep::Submit("hi".to_string()),
            pa_tui::interactive::HeadlessStep::WaitIdle { timeout_ms: 30_000 },
        ],
        width: 100,
        height: 30,
    };
    let first = chat_options(
        &supervisor.socket,
        dir.path(),
        &session_dir,
        &script_path,
        pa_tui::interactive::SessionSelection::Attach(active.clone()),
    );
    pa_tui::interactive::run_interactive(first, pa_tui::interactive::UiMode::Headless(first_plan))
        .await
        .expect("first interactive run");

    // The Esc handoff's own path: a fresh surface attaching to the same
    // live session; the wall time is the reported 5-10s round trip.
    let second_plan = pa_tui::interactive::HeadlessPlan {
        steps: Vec::new(),
        width: 100,
        height: 30,
    };
    let second = chat_options(
        &supervisor.socket,
        dir.path(),
        &session_dir,
        &script_path,
        pa_tui::interactive::SessionSelection::Attach(active.clone()),
    );
    let started = Instant::now();
    let outcome = pa_tui::interactive::run_interactive(
        second,
        pa_tui::interactive::UiMode::Headless(second_plan),
    )
    .await
    .expect("second interactive run");
    let elapsed = started.elapsed();

    // The control: the same fresh-surface attach against a trivial session on the same daemon,
    // under the same box conditions (warmed once, then timed).
    let control_id = format!("01a0vc-{:012x}", u64::from(std::process::id()));
    let control_path = session_dir.join(format!("{control_id}.jsonl"));
    std::fs::write(
        &control_path,
        format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "session", "version": 3, "id": control_id,
                "timestamp": "2024-01-01T00:00:00.000Z", "cwd": "/tmp",
            }),
            serde_json::json!({
                "type": "message", "id": format!("{control_id}-m0"), "parentId": "",
                "timestamp": "2024-01-01T00:00:00.000Z",
                "message": { "role": "user", "content": "question" },
            }),
        ),
    )
    .expect("write control transcript");
    let control_active = create_session_from_file_via_daemon(
        &supervisor.socket,
        &script_path,
        &script,
        dir.path(),
        &session_dir,
        &control_path,
    )
    .await;
    let mut control = Duration::ZERO;
    for _ in 0..2 {
        let options = chat_options(
            &supervisor.socket,
            dir.path(),
            &session_dir,
            &script_path,
            pa_tui::interactive::SessionSelection::Attach(control_active.clone()),
        );
        let plan = pa_tui::interactive::HeadlessPlan {
            steps: Vec::new(),
            width: 100,
            height: 30,
        };
        let started = Instant::now();
        pa_tui::interactive::run_interactive(options, pa_tui::interactive::UiMode::Headless(plan))
            .await
            .expect("control interactive run");
        control = started.elapsed();
    }
    let allowed = VIEW_SWITCH_CEILING.max(control * CONTROL_FACTOR);
    eprintln!(
        "view-switch latency guard: the Esc-handoff re-attach round trip took {elapsed:?} \
         (control {control:?}, allowed {allowed:?})"
    );
    assert!(
        elapsed <= allowed,
        "the agents-view Esc round trip took {elapsed:?} (control {control:?}, allowed \
         {allowed:?}) — the attach/snapshot path regressed (the operator's 5-10s handoff class)"
    );
    assert!(!outcome.frames.is_empty(), "frames were captured");
}
