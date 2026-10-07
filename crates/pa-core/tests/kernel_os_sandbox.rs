// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Real Landlock enforcement on the running kernel; Linux-only.
#![cfg(target_os = "linux")]

//! The OS sandbox around the real Python kernel: a cell and its `bash()` children write inside
//! the workspace and fail with EACCES outside it (`workspace-write`), every workspace write fails
//! under `read-only` while the kernel itself keeps working, and network off refuses a TCP
//! connect to a local listener. Off is unchanged (`kernel_lifecycle.rs` and every other kernel
//! test run with no sandbox). Skips, with a message, without the kernel Python, without
//! Landlock, or without a writable directory outside `/tmp` (the sandbox keeps `/tmp` and
//! `$TMPDIR` writable, so the probes need a directory outside both).

use std::net::TcpListener;
use std::path::{Path, PathBuf};

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus};
use pa_core::os_sandbox::{SandboxMode, SessionSandbox};
use pa_core::settings::{SandboxSettings, Settings, SettingsManager};

fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = PathBuf::from(format!("{home}/.prime/agent/kernel-venv/bin/python"));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping the kernel sandbox test",
        candidate.display()
    );
    None
}

/// A fresh directory outside `/tmp` and `$TMPDIR` (both stay writable in the sandbox) to hold
/// `workspace/` and `outside/`.
fn outside_tmp_base() -> Option<tempfile::TempDir> {
    let base = Path::new("/var/tmp");
    let usable = !base.starts_with(std::env::temp_dir()) && !base.starts_with("/tmp");
    let dir = usable.then(|| tempfile::tempdir_in(base).ok()).flatten();
    if dir.is_none() {
        eprintln!("/var/tmp is not usable; skipping the kernel sandbox test");
    }
    dir
}

struct Fixture {
    _base: tempfile::TempDir,
    workspace: PathBuf,
    outside: PathBuf,
    /// The session artifact dir (snapshot, local harness): writable in every mode.
    artifacts: PathBuf,
    provisioner: IpythonKernelProvisioner,
}

/// A provisioner whose kernel runs under `mode` (network as given), or `None` (after saying
/// why) where the test cannot run.
fn fixture(mode: SandboxMode, network: bool) -> Option<Fixture> {
    let python = kernel_python()?;
    let base = outside_tmp_base()?;
    let root = base.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    let artifacts = root.join("artifacts");
    for dir in [&workspace, &outside, &artifacts] {
        std::fs::create_dir(dir).unwrap();
    }
    let settings = SettingsManager::in_memory(&Settings {
        sandbox: Some(SandboxSettings {
            mode: Some(mode.wire_name().to_string()),
            network: Some(network),
            writable_roots: None,
        }),
        ..Default::default()
    });
    let sandbox = SessionSandbox::resolve(&settings, None, &workspace).expect("an enabled mode");
    if sandbox.status_label().ends_with("(unavailable)") {
        eprintln!(
            "skipping the kernel sandbox test: {}",
            sandbox.prompt_line()
        );
        return None;
    }
    let provisioner = IpythonKernelProvisioner::new(
        &workspace,
        IpythonKernelProvisionerOptions {
            python: Some(python),
            snapshot_dir: Some(artifacts.clone()),
            sandbox: Some(sandbox),
            ..Default::default()
        },
    );
    Some(Fixture {
        _base: base,
        workspace,
        outside,
        artifacts,
        provisioner,
    })
}

/// Run one cell; its `result` text.
async fn cell(fixture: &Fixture, code: &str) -> String {
    let manager = fixture.provisioner.ensure(None, None).await.unwrap();
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    result.result.unwrap_or_default()
}

/// A cell that tries `open(path, 'w')`: `ok`, or the errno it failed with.
fn python_write(path: &Path) -> String {
    format!(
        "def _probe(path):\n    try:\n        with open(path, 'w') as f:\n            f.write('x')\n        return 'ok'\n    except OSError as error:\n        return f'errno {{error.errno}}'\n_probe({:?})",
        path.display().to_string()
    )
}

/// A cell that runs `printf x > path` through `bash()`: the exit code and whether the shell
/// reported `Permission denied`.
fn bash_write(path: &Path) -> String {
    format!(
        "_r = await bash(\"printf x > '{}'\")\n(_r.exit_code, 'Permission denied' in _r.output)",
        path.display()
    )
}

#[tokio::test]
async fn workspace_write_confines_cells_and_bash_to_the_workspace() {
    let Some(fixture) = fixture(SandboxMode::WorkspaceWrite, false) else {
        return;
    };
    let results = [
        cell(&fixture, &python_write(&fixture.workspace.join("cell.txt"))).await,
        cell(&fixture, &python_write(&fixture.outside.join("cell.txt"))).await,
        cell(&fixture, &bash_write(&fixture.workspace.join("bash.txt"))).await,
        cell(&fixture, &bash_write(&fixture.outside.join("bash.txt"))).await,
        // `subprocess` children inherit the restriction too.
        cell(
            &fixture,
            &format!(
                "import subprocess\nsubprocess.run(['/bin/sh', '-c', \"printf x > '{}'\"], capture_output=True).returncode",
                fixture.outside.join("subprocess.txt").display()
            ),
        )
        .await,
        // The kernel's own scratch stays writable.
        cell(
            &fixture,
            "import tempfile\nwith tempfile.NamedTemporaryFile('w') as f:\n    f.write('x')\n'tmp ok'",
        )
        .await,
    ];
    assert_eq!(
        results,
        [
            "'ok'".to_string(),
            "'errno 13'".to_string(),
            "(0, False)".to_string(),
            "(1, True)".to_string(),
            "1".to_string(),
            "'tmp ok'".to_string(),
        ]
    );
    assert_eq!(
        (
            fixture.workspace.join("bash.txt").exists(),
            fixture.outside.join("bash.txt").exists(),
            fixture.outside.join("subprocess.txt").exists(),
        ),
        (true, false, false)
    );
    fixture.provisioner.dispose(None).await;
}

#[tokio::test]
async fn read_only_refuses_workspace_writes_while_the_kernel_keeps_working() {
    let Some(fixture) = fixture(SandboxMode::ReadOnly, false) else {
        return;
    };
    std::fs::write(fixture.workspace.join("input.txt"), "readable").unwrap();
    let results = [
        cell(&fixture, "kept = 1 + 1\nkept").await,
        cell(&fixture, "open('input.txt').read()").await,
        cell(&fixture, &python_write(&fixture.workspace.join("cell.txt"))).await,
        cell(&fixture, &bash_write(&fixture.workspace.join("bash.txt"))).await,
        cell(&fixture, &python_write(&fixture.outside.join("cell.txt"))).await,
    ];
    assert_eq!(
        results,
        [
            "2".to_string(),
            "'readable'".to_string(),
            "'errno 13'".to_string(),
            "(1, True)".to_string(),
            "'errno 13'".to_string(),
        ]
    );
    // The namespace snapshot still lands in the session artifact dir.
    fixture.provisioner.dispose(None).await;
    assert!(fixture.artifacts.join("kernel-state.dill").exists());
}

/// A cell connecting to `port` on loopback: `connected` or the errno.
fn connect_cell(port: u16) -> String {
    format!(
        "import socket\ndef _connect():\n    try:\n        socket.create_connection(('127.0.0.1', {port}), timeout=5).close()\n        return 'connected'\n    except OSError as error:\n        return f'errno {{error.errno}}'\n_connect()"
    )
}

/// A cell whose `bash()` command connects to `port` on loopback (the kernel's own Python as
/// the client): the command's exit code. The host runs `bash()` jobs, so this checks the host
/// spawns them under the kernel's sandbox.
fn bash_connect_cell(port: u16) -> String {
    format!(
        r#"import sys
_r = await bash(sys.executable + " -c 'import socket; socket.create_connection((\"127.0.0.1\", {port}), timeout=5).close()'")
_r.exit_code"#
    )
}

#[tokio::test]
async fn network_off_refuses_a_tcp_connect_and_network_on_allows_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let Some(denied) = fixture(SandboxMode::WorkspaceWrite, false) else {
        return;
    };
    let refused = cell(&denied, &connect_cell(port)).await;
    let bash_refused = cell(&denied, &bash_connect_cell(port)).await;
    denied.provisioner.dispose(None).await;
    let allowed = fixture(SandboxMode::WorkspaceWrite, true).expect("the same machine");
    let connected = cell(&allowed, &connect_cell(port)).await;
    let bash_connected = cell(&allowed, &bash_connect_cell(port)).await;
    allowed.provisioner.dispose(None).await;
    // EPERM: the seccomp filter refuses the inet socket itself.
    assert_eq!(
        (
            refused.as_str(),
            bash_refused.as_str(),
            connected.as_str(),
            bash_connected.as_str()
        ),
        ("'errno 1'", "1", "'connected'", "0")
    );
}
