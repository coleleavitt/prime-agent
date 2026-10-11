//! A spawned binary never touches the user's real agent state.
//!
//! On 2026-10-08 a `cargo test` run started from a prime-agent session
//! inherited the session's `PRIME_AGENT_CODING_AGENT_DIR`; 32 spawned
//! binaries that set `HOME` but not the agent dir traced into the real
//! `~/.prime/agent/logs/agent.jsonl`, read the real `auth.json` and flushed
//! the real harness ledger. A sentinel dir stands in for the real home here
//! (`PA_TEST_PROTECTED_HOME`): the guards protect it exactly like the
//! passwd home, and these tests require it byte-identical afterwards.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pa_types::platform::test_isolation::{PROTECTED_HOME_ENV, TestState};

/// A fake real home holding the live-state files a leak would touch.
fn sentinel_home(root: &Path) -> PathBuf {
    let home = root.join("real-home");
    let agent = home.join(".prime").join("agent");
    std::fs::create_dir_all(agent.join("logs")).unwrap();
    std::fs::write(
        agent.join("auth.json"),
        r#"{"sentinel":{"type":"api_key","key":"s"}}"#,
    )
    .unwrap();
    std::fs::write(agent.join("settings.json"), "{}").unwrap();
    std::fs::write(
        agent.join("logs").join("agent.jsonl"),
        "{\"sentinel\":true}\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".anthropic-accounts")).unwrap();
    std::fs::write(
        home.join(".anthropic-accounts").join("accounts.json"),
        r#"{"version":1,"accounts":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude").join(".credentials.json"), "{}").unwrap();
    home
}

/// Every entry under `dir` with its kind, contents and mtime.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (bool, Vec<u8>, std::time::SystemTime)> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let is_dir = metadata.is_dir();
        let contents = if metadata.is_file() {
            std::fs::read(&path).unwrap()
        } else {
            Vec::new()
        };
        entries.insert(
            path.clone(),
            (is_dir, contents, metadata.modified().unwrap()),
        );
        if is_dir {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
    entries
}

/// One `get_state` round trip over rpc, then stdin closes and the child exits.
fn rpc_get_state(command: &mut Command) -> (std::process::ExitStatus, String, Option<String>) {
    let mut child = command
        .args(["--mode", "rpc", "--no-session"])
        .env("PRIME_AGENT_FAUX_SCRIPT", r#"{"responses":["unused"]}"#)
        .env("DO_NOT_TRACK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary present");
    let mut stdin = child.stdin.take().expect("stdin piped");
    // A refused start exits before reading: the write may meet a closed pipe.
    let _ = stdin.write_all(b"{\"type\":\"get_state\",\"id\":\"s\"}\n");
    let stdout = child.stdout.take().expect("stdout piped");
    let response = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .find(|line| line.contains(r#""type":"response""#));
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status, stderr, response)
}

#[test]
fn a_binary_with_a_leaked_real_agent_dir_refuses_to_start_and_touches_nothing() {
    let root = tempfile::tempdir().unwrap();
    let real_home = sentinel_home(root.path());
    let before = snapshot(&real_home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .current_dir(root.path())
        .env("HOME", root.path().join("sandbox-home"))
        .env(
            "PRIME_AGENT_CODING_AGENT_DIR",
            real_home.join(".prime").join("agent"),
        )
        .env(PROTECTED_HOME_ENV, &real_home);
    let (status, stderr, response) = rpc_get_state(&mut command);
    assert_eq!(
        status.code(),
        Some(78),
        "stderr: {stderr} response: {response:?}"
    );
    assert!(
        stderr.contains("refusing to start") && stderr.contains("agent dir"),
        "the refusal names what it refused: {stderr}"
    );
    assert_eq!(response, None, "a refused binary never answers");
    assert!(before == snapshot(&real_home), "the real home changed");
}

#[test]
fn a_binary_with_the_real_home_and_no_overrides_refuses_to_start() {
    let root = tempfile::tempdir().unwrap();
    let real_home = sentinel_home(root.path());
    let before = snapshot(&real_home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .current_dir(root.path())
        .env("HOME", &real_home)
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .env_remove("ANTHROPIC_ACCOUNTS_DIR")
        .env_remove("ANTHROPIC_ACCOUNTS_FILE")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env(PROTECTED_HOME_ENV, &real_home);
    let (status, stderr, _) = rpc_get_state(&mut command);
    assert_eq!(status.code(), Some(78), "stderr: {stderr}");
    for refused in [
        "agent dir",
        "Anthropic account store",
        "Claude Code credentials",
    ] {
        assert!(stderr.contains(refused), "{refused} is refused: {stderr}");
    }
    assert!(before == snapshot(&real_home), "the real home changed");
}

#[test]
fn test_state_keeps_a_leaked_environment_off_the_real_home() {
    let root = tempfile::tempdir().unwrap();
    let real_home = sentinel_home(root.path());
    let before = snapshot(&real_home);
    let state = TestState::new(root.path().join("test"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    // The leaks of an inherited environment: TestState overrides each one.
    command
        .env("HOME", &real_home)
        .env(
            "PRIME_AGENT_CODING_AGENT_DIR",
            real_home.join(".prime").join("agent"),
        )
        .env(
            "ANTHROPIC_ACCOUNTS_DIR",
            real_home.join(".anthropic-accounts"),
        )
        .env("CLAUDE_CONFIG_DIR", real_home.join(".claude"));
    state.apply(&mut command);
    command
        .current_dir(root.path())
        .env(PROTECTED_HOME_ENV, &real_home);
    let (status, stderr, response) = rpc_get_state(&mut command);
    let response = response.unwrap_or_else(|| panic!("no rpc response; stderr: {stderr}"));
    assert!(response.contains("\"success\":true"), "{response}");
    assert!(status.success(), "{status:?} stderr: {stderr}");
    assert!(before == snapshot(&real_home), "the real home changed");
    assert!(
        state.agent_dir().join("auth.json").exists(),
        "the binary ran on the isolated agent dir"
    );
}
