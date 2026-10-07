//! Real enforcement on this machine's kernel: confined `/bin/sh` children
//! try to write, and to connect, and the kernel refuses. Skips (with a
//! message) where Landlock is unavailable, so other kernels' CI stays green.
#![cfg(target_os = "linux")]

use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use pa_os_sandbox::{
    Confinement, NetworkAccess, PreparedSandbox, SandboxError, SandboxPaths, SandboxPolicy,
};

struct Layout {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    scratch: PathBuf,
    extra: PathBuf,
    outside: PathBuf,
}

fn layout() -> Layout {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let make = |name: &str| {
        let path = root.join(name);
        std::fs::create_dir(&path).unwrap();
        path
    };
    Layout {
        workspace: make("workspace"),
        scratch: make("scratch"),
        extra: make("extra"),
        outside: make("outside"),
        _dir: dir,
    }
}

/// The prepared sandbox, or `None` (after saying why) without Landlock.
fn prepare(
    layout: &Layout,
    confinement: Confinement,
    network: NetworkAccess,
) -> Option<PreparedSandbox> {
    let policy = SandboxPolicy {
        confinement,
        network,
        writable_roots: vec![layout.extra.clone()],
    };
    let paths = SandboxPaths {
        workspace: layout.workspace.clone(),
        scratch: vec![layout.scratch.clone()],
    };
    match pa_os_sandbox::prepare(&policy, &paths) {
        Ok(prepared) => Some(prepared),
        Err(SandboxError::Unsupported { reason }) => {
            eprintln!("skipping Landlock enforcement test: {reason}");
            None
        }
        Err(error) => panic!("sandbox setup failed: {error}"),
    }
}

/// Run `script` under `sh -c` in the sandbox: (exit code, stdout+stderr).
fn sh(prepared: &PreparedSandbox, cwd: &Path, script: &str) -> (Option<i32>, String) {
    let output = prepared
        .command("/bin/sh")
        .args(["-c", script])
        .current_dir(cwd)
        .output()
        .expect("spawn the confined shell");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code(), text)
}

/// `printf` into `dir/name`: whether the write landed, and the shell's
/// message when it did not.
fn write_into(prepared: &PreparedSandbox, cwd: &Path, dir: &Path, name: &str) -> (bool, bool) {
    let target = dir.join(name);
    let (code, output) = sh(
        prepared,
        cwd,
        &format!("printf ok > '{}'", target.display()),
    );
    let refused = code != Some(0) && output.contains("Permission denied");
    (target.exists(), refused)
}

#[test]
fn workspace_write_allows_the_workspace_and_refuses_everything_else_with_eacces() {
    let layout = layout();
    let Some(prepared) = prepare(&layout, Confinement::WorkspaceWrite, NetworkAccess::Allowed)
    else {
        return;
    };
    let cwd = &layout.workspace;
    let results = [
        write_into(&prepared, cwd, &layout.workspace, "inside.txt"),
        write_into(&prepared, cwd, &layout.scratch, "scratch.txt"),
        write_into(&prepared, cwd, &layout.extra, "extra.txt"),
        write_into(&prepared, cwd, &layout.outside, "outside.txt"),
    ];
    // (landed, refused with EACCES's "Permission denied")
    assert_eq!(
        results,
        [(true, false), (true, false), (true, false), (false, true)]
    );
    // Reading outside the writable roots still works.
    std::fs::write(layout.outside.join("readable.txt"), "visible").unwrap();
    let (code, output) = sh(
        &prepared,
        cwd,
        &format!("cat '{}'", layout.outside.join("readable.txt").display()),
    );
    assert_eq!((code, output.as_str()), (Some(0), "visible"));
}

#[test]
fn read_only_refuses_writes_to_the_workspace_and_extra_roots() {
    let layout = layout();
    let Some(prepared) = prepare(&layout, Confinement::ReadOnly, NetworkAccess::Allowed) else {
        return;
    };
    let cwd = &layout.workspace;
    let results = [
        write_into(&prepared, cwd, &layout.workspace, "inside.txt"),
        write_into(&prepared, cwd, &layout.extra, "extra.txt"),
        write_into(&prepared, cwd, &layout.outside, "outside.txt"),
        // The confined process's own scratch stays writable in every mode.
        write_into(&prepared, cwd, &layout.scratch, "scratch.txt"),
    ];
    assert_eq!(
        results,
        [(false, true), (false, true), (false, true), (true, false)]
    );
    // Deleting and renaming are writes too.
    std::fs::write(layout.workspace.join("keep.txt"), "kept").unwrap();
    let (code, _) = sh(&prepared, cwd, "rm -f keep.txt || exit 3; exit 0");
    assert_eq!(
        (code, layout.workspace.join("keep.txt").exists()),
        (Some(3), true)
    );
}

/// Connect to `port` on loopback from the sandbox (bash's `/dev/tcp` or
/// `/dev/udp`).
fn connect(
    prepared: &PreparedSandbox,
    cwd: &Path,
    protocol: &str,
    port: u16,
) -> (Option<i32>, String) {
    let output = prepared
        .command("/bin/bash")
        .args([
            "-c",
            &format!("exec 3<>/dev/{protocol}/127.0.0.1/{port} && printf hi >&3"),
        ])
        .current_dir(cwd)
        .output()
        .expect("spawn the confined bash");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn network_off_refuses_a_tcp_connect_to_a_local_listener() {
    if !Path::new("/bin/bash").exists() {
        eprintln!("skipping: /bin/bash not found");
        return;
    }
    let layout = layout();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let Some(denied) = prepare(&layout, Confinement::WorkspaceWrite, NetworkAccess::Denied) else {
        return;
    };
    for protocol in ["tcp", "udp"] {
        let (code, stderr) = connect(&denied, &layout.workspace, protocol, port);
        assert_eq!(
            (code, stderr.contains("Operation not permitted")),
            (Some(1), true),
            "{protocol}: {stderr}"
        );
    }
    // The control: the same connect with network allowed reaches the listener.
    let allowed = prepare(&layout, Confinement::WorkspaceWrite, NetworkAccess::Allowed).unwrap();
    let (code, stderr) = connect(&allowed, &layout.workspace, "tcp", port);
    assert_eq!(code, Some(0), "{stderr}");
    let (mut stream, _) = listener.accept().unwrap();
    let mut received = String::new();
    stream.read_to_string(&mut received).unwrap();
    assert_eq!(received, "hi");
}

#[test]
fn network_off_keeps_unix_sockets_and_pipes_working() {
    let layout = layout();
    let Some(prepared) = prepare(&layout, Confinement::WorkspaceWrite, NetworkAccess::Denied)
    else {
        return;
    };
    // A pipeline and a here-document both need pipes; `printf` to /dev/null
    // needs the device allowlist.
    let (code, output) = sh(
        &prepared,
        &layout.workspace,
        "printf 'a\\nb\\n' | wc -l > /dev/null && cat <<EOF\npiped\nEOF",
    );
    assert_eq!((code, output.as_str()), (Some(0), "piped\n"));
}

#[test]
fn the_restriction_is_inherited_by_grandchildren() {
    let layout = layout();
    let Some(prepared) = prepare(&layout, Confinement::WorkspaceWrite, NetworkAccess::Allowed)
    else {
        return;
    };
    let target = layout.outside.join("grandchild.txt");
    let (code, output) = sh(
        &prepared,
        &layout.workspace,
        &format!("/bin/sh -c \"printf x > '{}'\"", target.display()),
    );
    assert_eq!(
        (code, output.contains("Permission denied"), target.exists()),
        (Some(1), true, false),
        "{output}"
    );
}
