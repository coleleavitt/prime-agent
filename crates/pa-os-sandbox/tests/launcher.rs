//! The exec'd launcher end to end: confined and session-leading children
//! start as a fresh image of this test binary (its `main` dispatches
//! [`LAUNCHER_FLAG`] first, as `prime-agent`'s does), which applies the
//! restriction itself and execs the program, so the spawning process never
//! forks. Skips (with a message) where Landlock is unavailable.
//!
//! A custom harness: the binary must be able to act as the launcher, which
//! libtest's `main` cannot.

use std::ffi::OsStr;

use pa_os_sandbox::{LAUNCHER_FLAG, launch_main};

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(LAUNCHER_FLAG)) {
        std::process::exit(launch_main(std::env::args_os().skip(2)));
    }
    #[cfg(target_os = "linux")]
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read as _;
    use std::net::TcpListener;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use pa_os_sandbox::launch::{LAUNCH_ACK, LAUNCH_NAK};
    use pa_os_sandbox::{
        Confinement,
        Launcher,
        NetworkAccess,
        PreparedSandbox,
        SandboxError,
        SandboxPaths,
        SandboxPolicy,
    };

    type Check = (&'static str, fn(&Layout, &PreparedSandbox));

    pub(super) fn run() {
        let filter: Option<String> = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
        pa_os_sandbox::set_launcher(Launcher::new(
            std::env::current_exe().expect("this test binary"),
            vec![pa_os_sandbox::LAUNCHER_FLAG.into()],
        ))
        .expect("the first launcher");
        let checks: [Check; 6] = [
            (
                "a_confined_command_starts_through_the_launcher_and_writes_only_its_roots",
                a_confined_command_starts_through_the_launcher_and_writes_only_its_roots,
            ),
            (
                "the_launcher_refuses_network_when_it_is_off",
                the_launcher_refuses_network_when_it_is_off,
            ),
            (
                "a_session_command_leads_its_session_and_acknowledges_first",
                a_session_command_leads_its_session_and_acknowledges_first,
            ),
            (
                "a_launcher_that_cannot_start_the_command_says_why",
                a_launcher_that_cannot_start_the_command_says_why,
            ),
            (
                "a_confined_spawn_does_not_copy_the_host",
                a_confined_spawn_does_not_copy_the_host,
            ),
            (
                "a_vanished_writable_root_does_not_block_the_launch",
                a_vanished_writable_root_does_not_block_the_launch,
            ),
        ];
        let mut failed = Vec::new();
        for (name, check) in checks {
            if filter
                .as_deref()
                .is_some_and(|filter| !name.contains(filter))
            {
                continue;
            }
            let layout = layout();
            let Some(prepared) = prepare(&layout, NetworkAccess::Denied) else {
                return;
            };
            let outcome = std::panic::catch_unwind(|| check(&layout, &prepared));
            println!(
                "test {name} ... {}",
                if outcome.is_ok() { "ok" } else { "FAILED" }
            );
            if outcome.is_err() {
                failed.push(name);
            }
        }
        if !failed.is_empty() {
            eprintln!("failed: {failed:?}");
            std::process::exit(1);
        }
    }

    pub(super) struct Layout {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        outside: PathBuf,
    }

    fn layout() -> Layout {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        Layout {
            workspace,
            outside,
            _dir: dir,
        }
    }

    fn prepare(layout: &Layout, network: NetworkAccess) -> Option<PreparedSandbox> {
        let policy = SandboxPolicy {
            confinement: Confinement::WorkspaceWrite,
            network,
            writable_roots: Vec::new(),
        };
        let paths = SandboxPaths {
            workspace: layout.workspace.clone(),
            scratch: Vec::new(),
        };
        match pa_os_sandbox::prepare(&policy, &paths) {
            Ok(prepared) => Some(prepared),
            Err(SandboxError::Unsupported { reason }) => {
                eprintln!("skipping the launcher tests: {reason}");
                None
            }
            Err(error) => panic!("sandbox setup failed: {error}"),
        }
    }

    fn sh(command: &mut Command, cwd: &Path, script: &str) -> (Option<i32>, String) {
        let output = command
            .args(["-c", script])
            .current_dir(cwd)
            .output()
            .expect("spawn the confined shell");
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.code(), text)
    }

    fn a_confined_command_starts_through_the_launcher_and_writes_only_its_roots(
        layout: &Layout,
        prepared: &PreparedSandbox,
    ) {
        let mut command = prepared.command("/bin/sh");
        let program = command.get_program().to_os_string();
        let (code, output) = sh(
            &mut command,
            &layout.workspace,
            &format!(
                "printf in > inside.txt; printf out > '{}'; printf 'grand' | sh -c 'cat > \"$1\"' _ '{}'",
                layout.outside.join("outside.txt").display(),
                layout.outside.join("grandchild.txt").display()
            ),
        );
        assert_eq!(
            (
                program,
                code,
                output.matches("Permission denied").count(),
                layout.workspace.join("inside.txt").exists(),
                layout.outside.join("outside.txt").exists(),
                layout.outside.join("grandchild.txt").exists(),
            ),
            (
                std::env::current_exe().unwrap().into_os_string(),
                Some(1),
                2,
                true,
                false,
                false,
            ),
            "{output}"
        );
    }

    fn the_launcher_refuses_network_when_it_is_off(layout: &Layout, prepared: &PreparedSandbox) {
        if !Path::new("/bin/bash").exists() {
            eprintln!("skipping: /bin/bash not found");
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (code, output) = sh(
            &mut prepared.command("/bin/bash"),
            &layout.workspace,
            &format!("exec 3<>/dev/tcp/127.0.0.1/{port} && printf hi >&3"),
        );
        assert_eq!(
            (code, output.contains("Operation not permitted")),
            (Some(1), true),
            "{output}"
        );
    }

    /// The spawner's end of a session command's stdin, after reading the
    /// launcher's first byte (and any error text).
    fn spawn_session(mut command: Command) -> (std::process::Child, UnixStream, Vec<u8>) {
        let (parent, child_end) = UnixStream::pair().unwrap();
        let child = command
            .stdin(Stdio::from(OwnedFd::from(child_end)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Dropping the command closes this process's copy of the child end,
        // so the launcher's exit ends the read below.
        drop(command);
        let mut first = [0u8; 1];
        let mut received = Vec::new();
        if (&parent).read(&mut first).unwrap() == 1 {
            received.push(first[0]);
            if first[0] != LAUNCH_ACK {
                (&parent).read_to_end(&mut received).unwrap();
            }
        }
        (child, parent, received)
    }

    /// The child leads a new session (no controlling terminal; its group id
    /// is its pid) and is confined; the acknowledgement arrives on stdin
    /// before the program runs, and nothing else is written there.
    fn a_session_command_leads_its_session_and_acknowledges_first(
        layout: &Layout,
        prepared: &PreparedSandbox,
    ) {
        let mut command =
            pa_os_sandbox::session_command("/bin/sh", Some(prepared)).expect("a launcher");
        command.args([
            "-c",
            "set -- $(cut -d' ' -f5,6 /proc/$$/stat); echo \"$$ $1 $2\"; printf x > '../outside/session.txt'",
        ]);
        command.current_dir(&layout.workspace);
        let (child, parent, received) = spawn_session(command);
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        drop(parent);
        assert_eq!(
            (
                received,
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).contains("Permission denied"),
                layout.outside.join("session.txt").exists(),
            ),
            (
                vec![LAUNCH_ACK],
                format!("{pid} {pid} {pid}\n"),
                true,
                false
            )
        );
    }

    /// A launcher that cannot set the child up answers with the reason
    /// instead of running the command: here `setsid` fails because the
    /// child already leads a process group.
    fn a_launcher_that_cannot_start_the_command_says_why(
        layout: &Layout,
        prepared: &PreparedSandbox,
    ) {
        let marker = layout.workspace.join("ran.txt");
        let mut command =
            pa_os_sandbox::session_command("/bin/sh", Some(prepared)).expect("a launcher");
        command
            .args(["-c", &format!("printf ran > '{}'", marker.display())])
            .process_group(0);
        let (child, _parent, received) = spawn_session(command);
        let status = child.wait_with_output().unwrap().status;
        let mut expected = vec![LAUNCH_NAK];
        expected.extend_from_slice(b"setsid: Operation not permitted (os error 1)");
        assert_eq!(
            (received, status.code(), marker.exists()),
            (expected, Some(127), false)
        );
    }

    fn median(mut samples: Vec<Duration>) -> Duration {
        samples.sort();
        samples[samples.len() / 2]
    }

    /// Confinement used to run between fork and exec, so a confined spawn
    /// cost what a fork of the host costs (growing with its resident
    /// memory). Through the launcher the spawn is a `posix_spawn`: several
    /// times cheaper than a fork of the same inflated host, interleaved.
    fn a_confined_spawn_does_not_copy_the_host(layout: &Layout, prepared: &PreparedSandbox) {
        let ballast = std::hint::black_box(vec![1u8; 192 * 1024 * 1024]);
        let mut confined = Vec::new();
        let mut forked = Vec::new();
        for _ in 0..25 {
            let mut command = prepared.command("/bin/true");
            command.current_dir(&layout.workspace);
            let started = Instant::now();
            let mut child = command.spawn().unwrap();
            confined.push(started.elapsed());
            assert!(child.wait().unwrap().success());

            let mut fork = Command::new("true");
            fork.env("PATH", "/usr/bin:/bin").current_dir("/");
            let started = Instant::now();
            let mut child = fork.spawn().unwrap();
            forked.push(started.elapsed());
            child.wait().unwrap();
        }
        drop(ballast);
        let (confined, forked) = (median(confined), median(forked));
        assert!(
            confined * 3 < forked,
            "a confined spawn ({confined:?}) costs about what a fork does ({forked:?})"
        );
    }

    /// A writable root removed after the sandbox was prepared (a deleted
    /// scratch directory) cannot be written either way; the launch still
    /// runs the command instead of refusing it.
    fn a_vanished_writable_root_does_not_block_the_launch(
        layout: &Layout,
        _prepared: &PreparedSandbox,
    ) {
        let scratch = layout.outside.join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let policy = SandboxPolicy {
            confinement: Confinement::ReadOnly,
            network: NetworkAccess::Allowed,
            writable_roots: Vec::new(),
        };
        let paths = SandboxPaths {
            workspace: layout.workspace.clone(),
            scratch: vec![scratch.clone()],
        };
        let prepared = pa_os_sandbox::prepare(&policy, &paths).unwrap();
        std::fs::remove_dir(&scratch).unwrap();
        let (code, output) = sh(
            &mut prepared.command("/bin/sh"),
            &layout.workspace,
            "printf ran; printf x > here.txt",
        );
        assert_eq!(
            (
                code,
                output.starts_with("ran"),
                output.contains("Permission denied")
            ),
            (Some(1), true, true),
            "{output}"
        );
    }
}
