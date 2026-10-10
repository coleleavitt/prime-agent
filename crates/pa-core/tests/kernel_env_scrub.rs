// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration test: the kernel child (and everything a cell spawns through `bash()`)
//! must not inherit the daemon worker's identity (`PRIME_AGENT_INTERNAL_DAEMON_*`: role, token,
//! supervisor socket, ...) or its session lease. A `prime-agent` run started from a cell would
//! otherwise present the live worker's token to the supervisor and believe it is a worker. The
//! orphan-process journal stays: `bash()` enrolls its process groups there. Its own test binary:
//! it mutates the process environment, which no other test in this process reads. The kernel
//! Python is ambient product state; skipped when absent.

use std::path::PathBuf;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

#[tokio::test]
async fn the_kernel_does_not_inherit_the_daemon_worker_identity() {
    let Some(python) = kernel_python() else {
        return;
    };
    for (key, value) in [
        ("PRIME_AGENT_INTERNAL_DAEMON_WORKER", "1"),
        ("PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN", "worker-secret"),
        (
            "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
            "/tmp/sup.sock",
        ),
        ("PRIME_AGENT_INTERNAL_SESSION_LEASES", "1"),
        ("PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID", "owner"),
        ("PRIME_AGENT_TEST_UNRELATED", "kept"),
    ] {
        std::env::set_var(key, value);
    }
    let dir = tempfile::TempDir::new().unwrap();
    let provisioner = IpythonKernelProvisioner::new(
        dir.path(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            ..Default::default()
        },
    );
    let manager = provisioner.ensure(None, None).await.unwrap();
    let result = manager
        .execute(
            "import os\n\
             sorted(k for k in os.environ if k.startswith('PRIME_AGENT_INTERNAL_DAEMON') \
             or k.startswith('PRIME_AGENT_INTERNAL_SESSION_LEASE') or k == 'PRIME_AGENT_TEST_UNRELATED')",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        (result.status, result.result.as_deref()),
        (ExecuteStatus::Ok, Some("['PRIME_AGENT_TEST_UNRELATED']"))
    );
    provisioner.dispose(None).await;
}
