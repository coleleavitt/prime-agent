//! `/rewind` against the real `pa-daemon` binary: `get_rewind_checkpoints`
//! lists the active branch's prompts with their code summaries, and `rewind`
//! restores the code from the session's file checkpoints and moves the
//! conversation like `navigate_tree`. The scripted engine runs no kernel, so
//! the test records the agent's writes the way the kernel's edit skill
//! reports them (`pa_core::file_history`, into the session's artifact tree).
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pa_core::file_history::FileHistory;
use pa_types::platform::test_isolation::TestState;
use serde_json::{Value, json};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        pa_core::platform::process_tree::kill_child_tree(&mut self.child);
        let _ = self.child.wait();
    }
}

// The timeout panic path cannot wait on the child; the test process exits
// immediately afterwards, reaping it.
#[allow(clippy::zombie_processes)]
fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
    let child = TestState::for_agent_dir(agent_dir)
        .apply(&mut Command::new(env!("CARGO_BIN_EXE_pa-daemon")))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return Daemon { child };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("supervisor socket never appeared");
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = UnixStream::connect(socket) {
                let writer = stream.try_clone().expect("clone stream");
                let mut client = Client {
                    reader: BufReader::new(stream),
                    writer,
                };
                let hello = client.read_line();
                assert_eq!(hello["type"], "daemon_hello", "expected hello: {hello}");
                let capabilities = hello["serverCapabilities"]
                    .as_array()
                    .expect("capabilities");
                assert!(capabilities.contains(&json!("rewind")), "{hello}");
                return client;
            }
            assert!(Instant::now() < deadline, "daemon socket never accepted");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).expect("read daemon line");
        assert!(read > 0, "daemon closed the socket");
        serde_json::from_str(line.trim()).expect("a valid daemon line")
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        let mut line = serde_json::to_string(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }))
        .expect("serialize");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("write");
        self.writer.flush().expect("flush");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "no response for {id} arrived");
            let line = self.read_line();
            if line["type"] == "response" && line["id"] == id {
                return line;
            }
        }
    }
}

/// The session file the worker wrote (the only one in the directory).
fn session_file(session_dir: &Path) -> PathBuf {
    let files: Vec<PathBuf> = std::fs::read_dir(session_dir)
        .expect("read session dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    assert_eq!(files.len(), 1, "one session file: {files:?}");
    files[0].clone()
}

/// One agent write, reported the way the edit skill reports it.
fn agent_write(history: &FileHistory, path: &Path, content: &str) {
    let now = pa_core::file_history::now_ms();
    history.before_write(path, now).expect("before_write");
    std::fs::write(path, content).expect("write");
    history.after_write(path, now).expect("after_write");
    // Past the millisecond the write was stamped with: the next prompt's
    // entry is strictly later.
    std::thread::sleep(Duration::from_millis(5));
}

#[test]
fn rewind_restores_code_and_conversation_to_before_a_prompt() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::create_dir_all(&work).expect("work dir");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        json!({
            "responses": [
                { "text": "first answer", "delayMs": 10 },
                { "text": "second answer", "delayMs": 10 },
            ],
            "branchSummary": { "responses": [
                { "summary": "explored the second prompt", "delayMs": 10 },
            ] },
        })
        .to_string(),
    )
    .expect("write script");
    let created = client.request(
        "c1",
        &json!({
            "type": "create",
            "config": {
                "cwd": work.to_string_lossy(),
                "sessionDir": session_dir.to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["sessionId"].as_str())
        .expect("session id")
        .to_string();
    let prompt = |client: &mut Client, id: &str, message: &str| {
        let response = client.request(
            id,
            &json!({
                "type": "prompt_and_wait",
                "activeSessionId": session_id,
                "message": message,
            }),
        );
        assert_eq!(response["success"], true, "prompt {id} failed: {response}");
    };

    // Before any checkpoint exists the rows carry no code summary.
    prompt(&mut client, "p1", "first prompt");
    let listed = client.request(
        "g0",
        &json!({ "type": "get_rewind_checkpoints", "activeSessionId": session_id }),
    );
    assert_eq!(listed["success"], true, "{listed}");
    assert_eq!(listed["data"]["codeRestore"], json!(false));
    assert!(listed["data"]["checkpoints"][0].get("code").is_none());

    let log = session_file(&session_dir);
    let artifacts = agent_dir
        .join("session-artifacts")
        .join(log.file_stem().expect("stem"));
    let history = FileHistory::for_artifact_dir(&artifacts);
    let edited = work.join("edited.txt");
    let created_file = work.join("created.txt");
    std::fs::write(&edited, "base\n").expect("seed");
    // The first prompt's turn edits the file.
    agent_write(&history, &edited, "base\nfirst\n");
    // The second prompt's turn edits it again and creates another.
    prompt(&mut client, "p2", "second prompt");
    agent_write(&history, &edited, "base\nfirst\nsecond\n");
    agent_write(&history, &created_file, "created\n");

    let listed = client.request(
        "g1",
        &json!({ "type": "get_rewind_checkpoints", "activeSessionId": session_id }),
    );
    assert_eq!(listed["success"], true, "{listed}");
    assert_eq!(listed["data"]["codeRestore"], json!(true));
    let rows = listed["data"]["checkpoints"].as_array().expect("rows");
    let texts: Vec<&str> = rows
        .iter()
        .map(|row| row["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, vec!["first prompt", "second prompt"]);
    let path = |path: &Path| json!(path.to_string_lossy());
    assert_eq!(
        rows[1]["code"],
        json!({
            "filesChanged": [path(&edited), path(&created_file)],
            "insertions": 0,
            "deletions": 2,
            "externallyModified": [],
            "unrestorable": [],
        })
    );
    assert_eq!(
        rows[0]["code"]["filesChanged"],
        json!([path(&edited), path(&created_file)])
    );
    assert_eq!(rows[0]["code"]["deletions"], json!(3));
    let second_id = rows[1]["entryId"].as_str().unwrap().to_string();
    let first_id = rows[0]["entryId"].as_str().unwrap().to_string();

    // An entry that is not a prompt on the branch is refused.
    let refused = client.request(
        "r0",
        &json!({
            "type": "rewind",
            "activeSessionId": session_id,
            "entryId": "no-such-entry",
            "restoreCode": true,
        }),
    );
    assert_eq!(refused["success"], false, "{refused}");

    // Code and conversation back to before the second prompt.
    let rewound = client.request(
        "r1",
        &json!({
            "type": "rewind",
            "activeSessionId": session_id,
            "entryId": second_id,
            "restoreCode": true,
            "restoreConversation": true,
        }),
    );
    assert_eq!(rewound["success"], true, "rewind failed: {rewound}");
    assert_eq!(
        rewound["data"]["code"],
        json!({
            "restored": [path(&edited)],
            "deleted": [path(&created_file)],
            "skipped": [],
        })
    );
    assert_eq!(
        rewound["data"]["conversation"]["editorText"],
        json!("second prompt")
    );
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), "base\nfirst\n");
    assert!(!created_file.exists());
    let listed = client.request(
        "g2",
        &json!({ "type": "get_rewind_checkpoints", "activeSessionId": session_id }),
    );
    let rows = listed["data"]["checkpoints"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "the second prompt left the branch: {listed}");

    // A hand edit since the agent's last write is left alone unless forced.
    std::fs::write(&edited, "hand edit\n").unwrap();
    let skipped = client.request(
        "r2",
        &json!({
            "type": "rewind",
            "activeSessionId": session_id,
            "entryId": first_id,
            "restoreCode": true,
        }),
    );
    assert_eq!(skipped["success"], true, "{skipped}");
    assert_eq!(
        skipped["data"]["code"]["skipped"][0]["reason"],
        json!(pa_core::file_history::EXTERNALLY_MODIFIED_REASON)
    );
    assert!(skipped["data"].get("conversation").is_none());
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), "hand edit\n");
    let forced = client.request(
        "r3",
        &json!({
            "type": "rewind",
            "activeSessionId": session_id,
            "entryId": first_id,
            "restoreCode": true,
            "force": true,
        }),
    );
    assert_eq!(forced["success"], true, "{forced}");
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), "base\n");

    // "Summarize from here": the conversation moves with a branch summary.
    let summarized = client.request(
        "r4",
        &json!({
            "type": "rewind",
            "activeSessionId": session_id,
            "entryId": first_id,
            "summarize": true,
        }),
    );
    assert_eq!(summarized["success"], true, "{summarized}");
    assert_eq!(
        summarized["data"]["conversation"]["summaryEntry"]["summary"],
        json!("explored the second prompt")
    );
}
