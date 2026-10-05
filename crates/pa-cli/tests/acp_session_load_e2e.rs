// too_many_lines: style gate only.
#![allow(clippy::too_many_lines)]

//! ACP persisted sessions e2e (upstream #1116, #1600/#1601, #2804):
//! `initialize` advertises `loadSession` and `sessionCapabilities.list`,
//! `session/list` serves the saved catalog as ACP `SessionInfo` rows, and
//! `session/load` binds a saved session, replays its transcript as
//! `session/update` notifications before the response, and lets the next
//! prompt continue it. Both residency cases run: a dormant saved file (the
//! bound worker switches to it) and a session a live worker still serves
//! (the connection attaches to that worker).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_mins(1);

struct AcpChild {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
    stderr: Option<std::process::ChildStderr>,
}

impl AcpChild {
    /// A daemon-attached ACP child on `home`'s sandboxed supervisor socket,
    /// hosting the scripted faux worker of `<home>/worker-script.json`.
    fn spawn(home: &Path, args: &[&str]) -> AcpChild {
        let mut child = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .args(args)
            .arg("--daemon-socket")
            .arg(home.join("daemon.sock"))
            .env("HOME", home)
            .env("DO_NOT_TRACK", "1")
            .env("PRIME_AGENT_FAUX_SCRIPT", home.join("worker-script.json"))
            .env(
                pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
                "15000",
            )
            .current_dir(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("binary present");
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        AcpChild {
            stdin: child.stdin.take(),
            stderr: child.stderr.take(),
            child,
            lines,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let mut line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        id
    }

    /// The answer to request `id` plus the notifications before it, in order.
    fn wait_response(&mut self, id: u64) -> (Value, Vec<Value>) {
        let deadline = Instant::now() + TIMEOUT;
        let mut notifications = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for response {id}");
            let line = self.lines.recv_timeout(left).expect("the ACP stream open");
            let frame: Value = serde_json::from_str(&line).expect("valid JSON line");
            if frame.get("id").and_then(Value::as_u64) == Some(id)
                && (frame.get("result").is_some() || frame.get("error").is_some())
            {
                return (frame, notifications);
            }
            notifications.push(frame);
        }
    }

    /// Read frames until a `session/update` of `kind` arrives (the
    /// readiness signal, never a timer); the frames before it are dropped.
    fn wait_update(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no {kind} update arrived");
            let line = self.lines.recv_timeout(left).expect("the ACP stream open");
            let frame: Value = serde_json::from_str(&line).expect("valid JSON line");
            if frame["params"]["update"]["sessionUpdate"] == kind {
                return frame["params"]["update"].clone();
            }
        }
    }

    fn call(&mut self, method: &str, params: &Value) -> (Value, Vec<Value>) {
        let id = self.request(method, params);
        self.wait_response(id)
    }

    /// EOF, then the exit.
    fn finish(mut self) {
        self.stdin = None;
        assert!(self.child.wait().expect("the ACP child exits").success());
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(mut stderr) = self.stderr.take() {
            use std::io::Read;
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            if !text.is_empty() {
                eprintln!("ACP child stderr: {text}");
            }
        }
    }
}

/// Stops the sandbox's supervisor when the test ends.
struct Sandbox {
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new(responses: &Value) -> Sandbox {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(
            home.path().join("worker-script.json"),
            json!({ "engine": "faux", "responses": responses }).to_string(),
        )
        .unwrap();
        Sandbox { home }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    fn sessions_dir(&self) -> PathBuf {
        self.path().join(".prime/agent/sessions")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let socket = self.path().join("daemon.sock");
        let Ok(mut stream) = pa_types::platform::transport::connect_blocking(&socket) else {
            return;
        };
        let frame = json!({
            "type": "command",
            "id": "shutdown-test",
            "protocol": { "name": "prime-agent.daemon", "version": pa_types::daemon::DAEMON_PROTOCOL_VERSION },
            "command": { "type": "shutdown" },
        });
        let _ = writeln!(stream, "{frame}");
        let _ = stream.flush();
    }
}

fn initialize(client: &mut AcpChild) -> Value {
    let (response, _) = client.call(
        "initialize",
        &json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    response["result"].clone()
}

/// A saved session file: header, one user turn, one assistant answer.
fn write_saved_session(dir: &Path, id: &str, cwd: &Path, question: &str, answer: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let usage = json!({
        "input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    });
    let lines = [
        json!({
            "type": "session", "id": id, "version": 3,
            "timestamp": "2026-09-01T10:00:00.000Z", "cwd": cwd.to_string_lossy(),
        }),
        json!({
            "type": "message", "id": "e1", "parentId": null,
            "timestamp": "2026-09-01T10:00:01.000Z",
            "message": { "role": "user", "content": question, "timestamp": 1_788_256_801_000_u64 },
        }),
        json!({
            "type": "message", "id": "e2", "parentId": "e1",
            "timestamp": "2026-09-01T10:00:02.000Z",
            "message": {
                "role": "assistant", "content": [{ "type": "text", "text": answer }],
                "usage": usage, "stopReason": "stop", "timestamp": 1_788_256_802_000_u64,
            },
        }),
    ];
    let path = dir.join(format!("{id}.jsonl"));
    let body = lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{body}\n")).unwrap();
    path
}

/// The `session/update` bodies among `frames`, without the correlation `_meta`.
fn replayed_updates(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .map(|frame| {
            let mut update = frame["params"]["update"].clone();
            update.as_object_mut().unwrap().remove("_meta");
            update
        })
        .filter(|update| update["sessionUpdate"] != "session_info_update")
        .collect()
}

fn prompt_ends_turn(client: &mut AcpChild, session_id: &str, text: &str) -> Vec<Value> {
    let (response, notifications) = client.call(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] }),
    );
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    notifications
}

#[test]
fn acp_session_list_and_load_replay_a_dormant_saved_session() {
    let sandbox = Sandbox::new(&json!(["Everest."]));
    let home = sandbox.path().to_path_buf();
    let other_cwd = home.join("elsewhere");
    std::fs::create_dir_all(&other_cwd).unwrap();
    let saved = write_saved_session(
        &sandbox.sessions_dir(),
        "acp-load-fixture",
        &home,
        "Name a river.",
        "The Nile.",
    );
    write_saved_session(
        &sandbox.sessions_dir(),
        "acp-other-cwd",
        &other_cwd,
        "Name a lake.",
        "Baikal.",
    );

    let mut client = AcpChild::spawn(&home, &["--mode", "acp", "--no-session"]);
    let capabilities = initialize(&mut client)["agentCapabilities"].clone();
    assert_eq!(capabilities["loadSession"], true);
    assert_eq!(
        capabilities["sessionCapabilities"],
        json!({ "close": {}, "list": {} })
    );

    let (listed, _) = client.call("session/list", &json!({ "cwd": home.to_string_lossy() }));
    assert_eq!(
        listed["result"],
        json!({ "sessions": [{
            "sessionId": "acp-load-fixture",
            "cwd": home.to_string_lossy(),
            "title": "Name a river.",
            "updatedAt": "2026-09-01T10:00:02.000Z",
        }] }),
        "{listed}"
    );
    let (all, _) = client.call("session/list", &json!({}));
    let ids: Vec<&str> = all["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{all}"))
        .iter()
        .filter_map(|row| row["sessionId"].as_str())
        .collect();
    assert_eq!(ids.len(), 2, "both saved sessions: {all}");
    let (bad_cursor, _) = client.call("session/list", &json!({ "cursor": "not-a-cursor" }));
    assert_eq!(bad_cursor["error"]["code"], -32602, "{bad_cursor}");

    // An unknown id is invalid params and frees the slot for the real load.
    let (unknown, _) = client.call(
        "session/load",
        &json!({ "sessionId": "nope", "cwd": home.to_string_lossy(), "mcpServers": [] }),
    );
    assert_eq!(
        unknown["error"],
        json!({
            "code": -32602,
            "message": "Invalid params",
            "data": { "reason": "Unknown ACP session: nope" },
        }),
        "{unknown}"
    );

    let (loaded, replay) = client.call(
        "session/load",
        &json!({ "sessionId": "acp-load-fixture", "cwd": home.to_string_lossy(), "mcpServers": [] }),
    );
    assert!(loaded["result"]["configOptions"].is_array(), "{loaded}");
    assert!(loaded["result"].get("sessionId").is_none(), "{loaded}");
    assert_eq!(
        replayed_updates(&replay),
        vec![
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "Name a river." } }),
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "prime-agent-replay-assistant-1", "content": { "type": "text", "text": "The Nile." } }),
        ]
    );
    assert!(
        replay
            .iter()
            .filter(|frame| frame["method"] == "session/update")
            .all(|frame| frame["params"]["sessionId"] == "acp-load-fixture"),
        "every replayed update addresses the loaded session: {replay:?}"
    );

    let (second, _) = client.call(
        "session/load",
        &json!({ "sessionId": "acp-other-cwd", "cwd": other_cwd.to_string_lossy(), "mcpServers": [] }),
    );
    assert_eq!(
        second["error"]["data"]["details"],
        "prime-agent ACP mode hosts one session per connection; start another prime-agent process for a second session",
        "{second}"
    );

    prompt_ends_turn(&mut client, "acp-load-fixture", "Name a mountain.");
    let transcript = std::fs::read_to_string(&saved).unwrap();
    assert!(
        transcript.contains("Name a mountain.") && transcript.contains("Everest."),
        "the loaded session continues in its own file: {transcript}"
    );
    client.finish();
}

#[test]
fn acp_session_load_attaches_a_session_a_live_worker_serves() {
    let sandbox = Sandbox::new(&json!(["The Nile.", "Everest."]));
    let home = sandbox.path().to_path_buf();

    // A resident session: it outlives its client, and `session/new` names
    // its persisted id so a later client can load it.
    let mut first = AcpChild::spawn(&home, &["--mode", "acp"]);
    initialize(&mut first);
    let (created, _) = first.call("session/new", &json!({ "mcpServers": [] }));
    let session_id = created["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    // The admitted session advertises what a prompt can execute (upstream
    // #1308): the session builtins, not the TUI-only ones.
    let advertised = first.wait_update("available_commands_update");
    let names: Vec<&str> = advertised["availableCommands"]
        .as_array()
        .unwrap_or_else(|| panic!("{advertised}"))
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect();
    assert!(
        names.contains(&"compact") && names.contains(&"goal") && !names.contains(&"model"),
        "{advertised}"
    );
    prompt_ends_turn(&mut first, &session_id, "Name a river.");
    first.finish();
    let saved = std::fs::read_dir(sandbox.sessions_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("the saved session file");
    let header: Value = serde_json::from_str(
        std::fs::read_to_string(&saved)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        header["id"],
        json!(session_id),
        "session/new names the saved id"
    );

    let mut second = AcpChild::spawn(&home, &["--mode", "acp", "--no-session"]);
    initialize(&mut second);
    let (loaded, replay) = second.call(
        "session/load",
        &json!({ "sessionId": session_id, "cwd": home.to_string_lossy(), "mcpServers": [] }),
    );
    assert!(loaded["result"]["configOptions"].is_array(), "{loaded}");
    assert_eq!(
        replayed_updates(&replay),
        vec![
            json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "Name a river." } }),
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "prime-agent-replay-assistant-1", "content": { "type": "text", "text": "The Nile." } }),
        ]
    );
    let turn = prompt_ends_turn(&mut second, &session_id, "Name a mountain.");
    // The costed answer reports the context fill (upstream #1351).
    let usage = turn
        .iter()
        .map(|frame| &frame["params"]["update"])
        .find(|update| update["sessionUpdate"] == "usage_update")
        .unwrap_or_else(|| panic!("a usage_update after the answer: {turn:?}"));
    assert!(
        usage["used"].as_u64().is_some_and(|used| used > 0)
            && usage["size"].as_u64() > usage["used"].as_u64(),
        "{usage}"
    );
    let transcript = std::fs::read_to_string(&saved).unwrap();
    assert!(
        transcript.contains("Name a mountain.") && transcript.contains("Everest."),
        "the live worker's session continues: {transcript}"
    );
    second.finish();
}
