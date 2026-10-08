// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(clippy::large_futures, clippy::too_many_lines)]
// Drives kernel processes; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration tests for plan mode's no-OS-sandbox fallback: the
//! host-only `plan_guard` frame arms the runtime's write guard before the
//! kernel serves anything, a toggle re-sends it with the same host-held token,
//! a kernel that cannot arm the guard never starts while plan mode is on, and
//! the host refuses the guarded kernel's `bash()` jobs. (Where the OS sandbox
//! is available plan mode runs on it instead: `kernel_plan_sandbox.rs`.) The
//! live tests need the kernel Python (ambient product state) and skip without it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::plan_guard::{
    KernelPlanGuard, PlanEnforcement, PlanMode, PlanModeApplied, PlanModeSwitch,
};
use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, KernelManagerOptions, KernelShutdownOptions,
};
use serde_json::Value;

/// Speaks protocol v4 and journals every request frame to `frames.jsonl`
/// next to itself. `plan_guard` frames are answered `ok` (echoing the
/// requested state) unless the file `refuse` exists next to the script.
const FAKE_RUNTIME: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

here = os.path.dirname(os.path.abspath(__file__))

def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

send({"event": "ready", "protocol": 5, "python": "3.13.0"})
for line in sys.stdin:
    try:
        frame = json.loads(line)
    except Exception:
        continue
    with open(os.path.join(here, "frames.jsonl"), "a") as journal:
        journal.write(json.dumps(frame) + "\n")
    rid = frame.get("id")
    kind = frame.get("type")
    if kind == "plan_guard":
        if os.path.exists(os.path.join(here, "refuse")):
            send({"event": "done", "id": rid, "status": "error", "reason": "PermissionError: plan_guard: invalid token"})
        else:
            send({"event": "done", "id": rid, "status": "ok", "enabled": frame.get("enabled")})
    elif kind == "shutdown":
        if isinstance(rid, str):
            send({"event": "done", "id": rid, "status": "ok"})
        break
    elif isinstance(rid, str):
        send({"event": "done", "id": rid, "status": "ok"})
"#;

fn fake_runtime(dir: &Path) -> PathBuf {
    let path = dir.join("fake-kernel");
    std::fs::write(&path, FAKE_RUNTIME).expect("write fake runtime");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn journaled_frames(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("frames.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("journaled frame"))
        .collect()
}

fn fake_manager(dir: &Path, mode: &PlanModeSwitch) -> ReplKernelManager {
    ReplKernelManager::new(KernelManagerOptions {
        python: Some(fake_runtime(dir)),
        cwd: Some(dir.to_path_buf()),
        session_id: Some("plan-guard-test".to_string()),
        plan_guard: Some(KernelPlanGuard {
            mode: mode.clone(),
            writable_roots: vec![dir.join("artifacts")],
            protected_roots: vec![dir.join("repo")],
            no_sandbox_reason: "OS sandbox unavailable: test".to_string(),
        }),
        ..Default::default()
    })
}

async fn stop(manager: &ReplKernelManager) {
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await;
}

/// `(type, enabled, [writable_roots, protected_roots])` of each journaled
/// frame, plus the distinct tokens the `plan_guard` frames carried.
fn summarize(frames: &[Value]) -> (Vec<(String, Value, Value)>, Vec<String>) {
    let mut tokens: Vec<String> = Vec::new();
    let summary = frames
        .iter()
        .map(|frame| {
            if let Some(token) = frame.get("token").and_then(Value::as_str) {
                if !tokens.iter().any(|seen| seen == token) {
                    tokens.push(token.to_string());
                }
            }
            (
                frame["type"].as_str().unwrap_or_default().to_string(),
                frame.get("enabled").cloned().unwrap_or(Value::Null),
                if frame["type"] == "plan_guard" {
                    serde_json::json!([frame["writable_roots"], frame["protected_roots"]])
                } else {
                    Value::Null
                },
            )
        })
        .collect();
    (summary, tokens)
}

#[tokio::test]
async fn plan_mode_arms_the_guard_before_the_first_request_and_toggles_reuse_the_token() {
    let dir = tempfile::TempDir::new().unwrap();
    let mode = PlanModeSwitch::new(true);
    let manager = fake_manager(dir.path(), &mode);
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("fake kernel starts with the guard armed");
    manager
        .execute("1", ExecuteOptions::default())
        .await
        .unwrap();
    mode.set(false);
    assert_eq!(manager.sync_plan_guard().await.unwrap(), Some(false));
    stop(&manager).await;

    let roots = serde_json::json!([
        [dir.path().join("artifacts").to_string_lossy()],
        [dir.path().join("repo").to_string_lossy()],
    ]);
    let (summary, tokens) = summarize(&journaled_frames(dir.path()));
    assert_eq!(
        summary,
        vec![
            ("plan_guard".to_string(), Value::Bool(true), roots.clone()),
            ("execute".to_string(), Value::Null, Value::Null),
            ("plan_guard".to_string(), Value::Bool(false), roots),
            ("shutdown".to_string(), Value::Null, Value::Null),
        ]
    );
    assert_eq!(
        tokens.len(),
        1,
        "one host-held token per kernel: {tokens:?}"
    );
    assert_eq!(tokens[0].len(), 64);
}

#[tokio::test]
async fn plan_mode_off_sends_no_guard_frame() {
    let dir = tempfile::TempDir::new().unwrap();
    let manager = fake_manager(dir.path(), &PlanModeSwitch::new(false));
    manager.start(KernelStartOptions::default()).await.unwrap();
    manager
        .execute("1", ExecuteOptions::default())
        .await
        .unwrap();
    stop(&manager).await;
    let (summary, _) = summarize(&journaled_frames(dir.path()));
    assert_eq!(
        summary,
        vec![
            ("execute".to_string(), Value::Null, Value::Null),
            ("shutdown".to_string(), Value::Null, Value::Null),
        ]
    );
}

#[tokio::test]
async fn a_kernel_that_cannot_arm_the_guard_never_starts_in_plan_mode() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("refuse"), "").unwrap();
    let manager = fake_manager(dir.path(), &PlanModeSwitch::new(true));
    let error = manager
        .start(KernelStartOptions::default())
        .await
        .expect_err("plan mode must not run an unguarded kernel");
    let message = format!("{error:#}");
    assert!(
        message.starts_with("plan mode is on, but the kernel could not arm its write guard: "),
        "{message}"
    );
    assert!(!manager.is_running());
}

/// The kernel Python with prime-agent-runtime installed; set
/// `PA_CORE_KERNEL_PYTHON` to point at an explicit interpreter instead.
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
        "kernel python {} not found; skipping live plan-guard test",
        candidate.display()
    );
    None
}

async fn cell(provisioner: &IpythonKernelProvisioner, code: &str) -> (ExecuteStatus, String) {
    let manager = provisioner.ensure(None, None).await.expect("kernel");
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .expect("execute");
    let error = result
        .error
        .map(|error| format!("{}: {}", error.ename, error.evalue))
        .unwrap_or_default();
    (result.status, format!("{}{error}", result.stdout))
}

/// The live fallback, chosen by an injected assessment: this machine "has no
/// OS sandbox", so plan mode arms the in-kernel guard instead of restarting
/// the kernel under one.
#[tokio::test]
async fn without_an_os_sandbox_a_live_kernel_refuses_writes_and_commands_and_cells_cannot_lift_it()
{
    let Some(python) = kernel_python() else {
        return;
    };
    // The workspace sits under a temp dir (a default writable root): the
    // kernel's cwd stays guarded anyway.
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let target = repo.join("main.py");
    std::fs::write(&target, "before").unwrap();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    let mode = PlanModeSwitch::new(true);
    let provisioner = IpythonKernelProvisioner::new(
        repo.clone(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            snapshot_dir: Some(artifacts.clone()),
            plan_mode: Some(PlanMode {
                switch: mode.clone(),
                enforcement: PlanEnforcement::KernelGuard {
                    reason: "OS sandbox unavailable: injected".to_string(),
                },
            }),
            ..Default::default()
        },
    );
    let target_literal = format!("{:?}", target.to_string_lossy());

    let (status, out) = cell(
        &provisioner,
        &format!("open({target_literal}, 'w').write('edited')"),
    )
    .await;
    assert_eq!(status, ExecuteStatus::Error, "{out}");
    assert!(
        out.starts_with("PlanModeError: Plan mode is active"),
        "{out}"
    );
    // Reads keep working, and kernel code can neither claim the controller
    // nor see the guard off.
    let (status, out) = cell(
        &provisioner,
        &format!(
            "import rlm.plan_guard as pg\n\
             print(open({target_literal}).read())\n\
             try:\n    pg.claim_host_controller()\nexcept PermissionError:\n    print('refused')\n\
             print(pg.is_enabled())"
        ),
    )
    .await;
    assert_eq!(
        (status, out.as_str()),
        (ExecuteStatus::Ok, "before\nrefused\nTrue\n")
    );
    // Without an OS sandbox no command can run read-only: `bash()` refuses
    // before any process exists, and a cell that disables that check meets
    // the host's own refusal of the job.
    let (status, out) = cell(
        &provisioner,
        "try:\n    await bash('echo edited > main.py')\nexcept pg.PlanModeError:\n    print('refused')\n\
         pg.check_bash = lambda command: None\n\
         try:\n    await bash('echo edited > main.py')\nexcept RuntimeError as error:\n    print(str(error).split(' (')[0])",
    )
    .await;
    assert_eq!(
        (status, out.as_str()),
        (
            ExecuteStatus::Ok,
            "refused\nbash(): Plan mode is active: running commands is blocked\n"
        )
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
    // The guarded kernel still snapshots into the session artifact dir.
    let manager = provisioner.ensure(None, None).await.unwrap();
    let _ = manager.snapshot_state().await;
    assert!(
        pa_core::kernel::snapshot_path_in(&artifacts).exists(),
        "the snapshot write was refused: {}",
        manager.kernel_stderr()
    );

    let pid = manager.process_id();
    mode.set(false);
    assert_eq!(
        provisioner.sync_plan_mode().await.unwrap(),
        PlanModeApplied::InPlace
    );
    let (status, out) = cell(
        &provisioner,
        &format!(
            "open({target_literal}, 'w').write('edited')\n\
             print((await bash('cat main.py')).output)"
        ),
    )
    .await;
    assert_eq!((status, out.as_str()), (ExecuteStatus::Ok, "edited\n"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
    // The fallback switches in place: the same kernel process throughout.
    assert_eq!(
        provisioner
            .manager()
            .and_then(|manager| manager.process_id()),
        pid
    );
    provisioner.dispose(None).await;
}
