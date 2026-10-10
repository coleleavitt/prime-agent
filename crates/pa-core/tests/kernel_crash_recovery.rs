// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration tests for recovery from an unexpected kernel exit (a native `os._exit()`
//! inside a cell, an OOM kill): the cell reports how the kernel died, the next `ensure()` serves a
//! fresh kernel, and the exit is handed out once for the restart notice. The kernel Python is
//! ambient product state; skipped when absent.

use std::path::PathBuf;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus, KernelExitedError};

/// The kernel Python with prime-agent-runtime installed; set
/// `PA_CORE_KERNEL_PYTHON` to point at an explicit interpreter instead.
fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

#[tokio::test]
async fn a_cell_that_kills_the_kernel_reports_the_exit_and_the_next_call_gets_a_fresh_kernel() {
    let Some(python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let provisioner = IpythonKernelProvisioner::new(
        dir.path(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            ..Default::default()
        },
    );
    let first = provisioner.ensure(None, None).await.unwrap();
    let first_pid = first.process_id();
    let error = first
        .execute(
            "import os\nos.write(2, b'native crash marker\\n')\nos._exit(7)",
            ExecuteOptions::default(),
        )
        .await
        .expect_err("the cell's kernel died");
    let exited = error
        .downcast_ref::<KernelExitedError>()
        .unwrap_or_else(|| panic!("a typed kernel-exit error, got: {error:#}"));
    assert_eq!(
        (
            exited.exit.exit_code,
            exited.exit.signal,
            exited.exit.request_type
        ),
        (Some(7), None, Some("execute"))
    );
    assert!(exited.exit.request_id.is_some(), "{:?}", exited.exit);
    assert!(
        exited.exit.stderr_tail.contains("native crash marker"),
        "{:?}",
        exited.exit.stderr_tail
    );
    let message = error.to_string();
    assert!(
        message.starts_with(&format!(
            "Kernel process exited unexpectedly (exit code 7) while serving execute request {}. A fresh kernel starts on the next call",
            exited.exit.request_id.as_deref().unwrap_or_default()
        )),
        "{message}"
    );
    assert!(message.contains("native crash marker"), "{message}");

    // The next call starts a fresh kernel instead of failing forever.
    let second = provisioner.ensure(None, None).await.unwrap();
    assert_ne!(second.process_id(), first_pid);
    let result = second
        .execute("1 + 1", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(
        (result.status, result.result.as_deref()),
        (ExecuteStatus::Ok, Some("2"))
    );
    // The exit is handed out exactly once (the one-time restart notice).
    assert_eq!(
        provisioner
            .take_unreported_exit()
            .map(|exit| exit.exit_code),
        Some(Some(7))
    );
    assert_eq!(provisioner.take_unreported_exit(), None);
}
