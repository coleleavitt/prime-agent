//! A `bash()` command never keeps its host's controlling terminal, whether
//! the host's sandbox is off or on: the real `prime-agent
//! --prime-agent-bash-host`, given a pty as its controlling terminal, starts
//! each command through its exec'd launcher (no fork of the host), which
//! makes the command a session leader with no terminal (a background read
//! of the user's terminal would otherwise stop it with SIGTTIN) and, with
//! the sandbox on, confines it.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Fields 5-7 of `/proc/<pid>/stat`: process group, session, tty number.
fn group_session_tty(pid: u32) -> (String, String, String) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let after_name = &stat[stat.rfind(')').expect("comm") + 2..];
    let fields: Vec<&str> = after_name.split(' ').collect();
    (
        fields[2].to_string(),
        fields[3].to_string(),
        fields[4].to_string(),
    )
}

/// Run `script` through a bash host whose controlling terminal is a fresh
/// pty, with `settings` as its global settings: the command's output, and
/// the host's own tty number (non-zero: it has the terminal).
fn run_on_a_terminal_host(
    home: &Path,
    cwd: &Path,
    settings: &Value,
    script: &str,
) -> (String, String) {
    let agent_dir = home.join(".prime").join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    let pty = nix::pty::openpty(None, None).expect("pty");
    let slave = pty.slave.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    pa_types::platform::test_isolation::TestState::for_agent_dir(&agent_dir).apply(&mut command);
    command
        .arg("--prime-agent-bash-host")
        .current_dir(cwd)
        .env("HOME", home)
        .env("TMPDIR", cwd)
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: runs between fork and exec in the child only: `setsid` and the
    // TIOCSCTTY ioctl, both async-signal-safe, make the pty slave the host's
    // controlling terminal.
    unsafe {
        command.pre_exec(move || {
            nix::unistd::setsid()?;
            if libc::ioctl(slave, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut host = command.spawn().expect("spawn the bash host");
    drop(pty.slave);
    let host_tty = group_session_tty(host.id()).2;
    let request = json!({
        "id": "1",
        "data": {
            "type": "bash.run",
            "command": script,
            "script": script,
            "cwd": cwd,
            "env": {"PATH": "/usr/bin:/bin"},
            "launchBypass": [],
            "kernelPid": std::process::id(),
            "allow": [],
            "waitMs": 30_000,
        },
    });
    let mut stdin = host.stdin.take().unwrap();
    writeln!(stdin, "{request}").unwrap();
    let mut reply = String::new();
    BufReader::new(host.stdout.take().unwrap())
        .read_line(&mut reply)
        .unwrap();
    drop(stdin);
    host.wait().unwrap();
    let reply: Value = serde_json::from_str(&reply).expect("a reply line");
    let output = reply["data"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|event| event["output"].as_str())
        .unwrap_or_else(|| panic!("no output in {reply}"))
        .to_string();
    (output, host_tty)
}

/// Prints `<group> <session> <tty> <pid>` for the command's shell, then
/// whether it could open `/dev/tty`, then whether it could write outside
/// its working directory.
const PROBE: &str = "set -- $(cut -d' ' -f5,6,7 /proc/$$/stat); echo \"$1 $2 $3 $$\"; \
                     if (exec 3</dev/tty) 2>/dev/null; then echo tty; else echo no-tty; fi; \
                     if (printf x > ../outside.txt) 2>/dev/null; then echo wrote; else echo refused; fi";

fn check(settings: &Value, expected_write: &str) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (home, cwd) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let (output, host_tty) = run_on_a_terminal_host(&home, &cwd, settings, PROBE);
    let lines: Vec<&str> = output.lines().collect();
    let ids: Vec<&str> = lines[0].split(' ').collect();
    let pid = ids[3];
    assert_eq!(
        (host_tty != "0", ids[..3].to_vec(), lines[1..].to_vec(),),
        (true, vec![pid, pid, "0"], vec!["no-tty", expected_write]),
        "{output}"
    );
}

#[test]
fn an_unconfined_command_of_a_terminal_host_leads_a_session_without_its_terminal() {
    check(&json!({}), "wrote");
}

#[test]
fn a_confined_command_of_a_terminal_host_leads_a_session_without_its_terminal() {
    let policy = pa_os_sandbox::SandboxPolicy {
        confinement: pa_os_sandbox::Confinement::ReadOnly,
        network: pa_os_sandbox::NetworkAccess::Denied,
        writable_roots: Vec::new(),
    };
    if let Err(error) = pa_os_sandbox::assess(&policy) {
        eprintln!("skipping: {error}");
        return;
    }
    // Read-only: only the scratch (`TMPDIR`, the working directory here)
    // is writable; `workspace-write` would grant all of `/tmp`.
    check(&json!({"sandbox": {"mode": "read-only"}}), "refused");
}
