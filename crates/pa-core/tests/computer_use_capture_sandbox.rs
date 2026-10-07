//! Computer-use screenshots are written by the host (`pa-computer-use`)
//! into `<agent dir>/tmp/computer-use` and read back by the kernel (the
//! `attach_image` skill opens the PNG). Under the opt-in OS sandbox the
//! kernel stays able to read them: the sandbox confines writes, never reads
//! of the same user's files. Real enforcement on this machine's kernel;
//! skips (with a message) where Landlock is unavailable.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;

use pa_os_sandbox::{Confinement, NetworkAccess, SandboxError, SandboxPaths, SandboxPolicy};

#[test]
fn a_sandboxed_kernel_reads_the_hosts_captures_but_cannot_write_there() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let scratch = root.join("scratch");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    // What the host's capture directory looks like: private to the user.
    let captures = pa_computer_use::capture_dir(&root.join("agent"));
    std::fs::create_dir_all(&captures).unwrap();
    std::fs::set_permissions(&captures, std::fs::Permissions::from_mode(0o700)).unwrap();
    let shot = captures.join("shot.png");
    std::fs::write(&shot, b"\x89PNG-bytes").unwrap();
    std::fs::set_permissions(&shot, std::fs::Permissions::from_mode(0o600)).unwrap();

    for confinement in [Confinement::ReadOnly, Confinement::WorkspaceWrite] {
        let policy = SandboxPolicy {
            confinement,
            network: NetworkAccess::Denied,
            writable_roots: Vec::new(),
        };
        let paths = SandboxPaths {
            workspace: workspace.clone(),
            scratch: vec![scratch.clone()],
        };
        let prepared = match pa_os_sandbox::prepare(&policy, &paths) {
            Ok(prepared) => prepared,
            Err(SandboxError::Unsupported { reason }) => {
                eprintln!("skipping the sandbox capture-read test: {reason}");
                return;
            }
            Err(error) => panic!("sandbox setup failed: {error}"),
        };
        let read = prepared
            .command("/bin/cat")
            .arg(&shot)
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            read.status.success(),
            "{confinement:?}: {}",
            String::from_utf8_lossy(&read.stderr)
        );
        assert_eq!(read.stdout, b"\x89PNG-bytes", "{confinement:?}");
        let planted = captures.join("planted.png");
        let write = prepared
            .command("/bin/sh")
            .args(["-c", &format!("printf x > '{}'", planted.display())])
            .current_dir(&workspace)
            .output()
            .unwrap();
        assert!(
            !write.status.success() && !planted.exists(),
            "{confinement:?}"
        );
    }
}
