//! A kernel interpreter that is still open for writing at the spawn instant
//! (ETXTBSY: a concurrent bootstrap rewriting the venv, or a fork of this
//! process still holding the write handle until its exec) is transient: the
//! kernel start rides it out instead of failing the boot.
// ETXTBSY is the Linux exec-while-open-for-write refusal.
#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};

#[tokio::test]
async fn a_kernel_interpreter_busy_being_written_still_starts() {
    let dir = tempfile::TempDir::new().unwrap();
    let python = dir.path().join("python");
    std::fs::write(&python, "#!/bin/sh\nexit 37\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The interpreter is held open for writing while the boot starts, the
    // way a concurrent writer holds it. The hold outlasts the provisioner's
    // first boot retry (250 ms backoff), so only a spawn that rides out
    // ETXTBSY itself (20 x 25 ms) boots without failing.
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&python)
        .unwrap();
    writer.write_all(b"# appended by the writer\n").unwrap();
    let release = std::thread::spawn(move || {
        // Fault injection, not a readiness wait: the concurrent writer's
        // hold lasts this long.
        std::thread::sleep(Duration::from_millis(350));
        drop(writer);
    });
    let provisioner = IpythonKernelProvisioner::new(
        dir.path(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            ..Default::default()
        },
    );
    let error = provisioner
        .ensure(None, None)
        .await
        .expect_err("the fixture interpreter exits instead of serving");
    release.join().unwrap();
    let message = format!("{error:#}");
    assert!(
        message.contains("unexpected exit code=37"),
        "the interpreter ran once the writer let go: {message}"
    );
}
