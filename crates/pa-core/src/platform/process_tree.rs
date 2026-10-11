//! Whole-tree process teardown: stop a process and every descendant it spawned, including
//! descendants in their own process groups (detached daemon workers, their kernels), which a
//! group kill never reaches. Owners that spawn such trees (test harnesses, supervisors that
//! must not leak their children) use this before deleting the directories the tree writes to.

use std::path::Path;
#[cfg(unix)]
use std::time::{Duration, Instant};

/// How long [`kill_process_trees`] waits for the signalled processes to disappear.
#[cfg(unix)]
const GONE_WAIT: Duration = Duration::from_secs(5);

/// Every live process (other than this one) whose working directory, argv, or environment
/// names a path under `root`: the processes still using a directory, including detached ones
/// that a parent-child walk cannot reach. Linux reads `/proc`; elsewhere nothing is found.
#[must_use]
pub fn processes_referencing(root: &Path) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        linux::processes_referencing(root)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        Vec::new()
    }
}

/// Stop each of `roots` and all of their descendants: the whole tree is frozen first (SIGSTOP,
/// so nothing forks while it is walked), then killed (SIGKILL), then awaited (bounded) until
/// every signalled process is gone or a zombie. Returns the pids signalled. Best effort: a pid
/// that exits mid-walk is skipped, and this process is never signalled.
#[cfg(unix)]
// The signalled pids are informational: teardown callers act on the effect, not the list.
#[allow(clippy::must_use_candidate)]
pub fn kill_process_trees(roots: &[u32]) -> Vec<u32> {
    let own = std::process::id();
    let mut tree: Vec<u32> = Vec::new();
    for &pid in roots {
        if pid != own && pid != 0 && !tree.contains(&pid) {
            signal(pid, libc::SIGSTOP);
            tree.push(pid);
        }
    }
    loop {
        let fresh: Vec<u32> = parent_table()
            .into_iter()
            .filter(|(pid, parent)| tree.contains(parent) && !tree.contains(pid) && *pid != own)
            .map(|(pid, _)| pid)
            .collect();
        if fresh.is_empty() {
            break;
        }
        for pid in fresh {
            signal(pid, libc::SIGSTOP);
            tree.push(pid);
        }
    }
    for &pid in &tree {
        signal(pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + GONE_WAIT;
    while tree.iter().any(|pid| running(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    tree
}

/// Elsewhere the platform tree kill (`taskkill /F /T` on Windows) stops each root's tree.
#[cfg(not(unix))]
// The signalled pids are informational: teardown callers act on the effect, not the list.
#[allow(clippy::must_use_candidate)]
pub fn kill_process_trees(roots: &[u32]) -> Vec<u32> {
    let own = std::process::id();
    roots
        .iter()
        .copied()
        .filter(|pid| *pid != own && *pid != 0)
        .filter(|pid| i32::try_from(*pid).is_ok_and(super::process::kill_process_group_or_pid))
        .collect()
}

/// [`kill_process_trees`] for one root.
// The signalled pids are informational: teardown callers act on the effect, not the list.
#[allow(clippy::must_use_candidate)]
pub fn kill_process_tree(root: u32) -> Vec<u32> {
    kill_process_trees(&[root])
}

/// [`kill_process_tree`] for a spawned child the caller still owns: only while it is running.
/// A child that already exited (or was reaped) no longer anchors its pid, so its tree is left
/// alone rather than risking a signal to an unrelated process that reused the pid.
pub fn kill_child_tree(child: &mut std::process::Child) {
    if matches!(child.try_wait(), Ok(None)) {
        kill_process_tree(child.id());
    }
}

#[cfg(unix)]
fn signal(pid: u32, signal: libc::c_int) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: `kill` takes plain integers; a stale pid only yields ESRCH.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// `(pid, parent pid)` for every process.
#[cfg(target_os = "linux")]
fn parent_table() -> Vec<(u32, u32)> {
    linux::parent_table()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parent_table() -> Vec<(u32, u32)> {
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            Some((pid, parent))
        })
        .collect()
}

/// Whether `pid` is still running: gone and zombie processes are not.
#[cfg(target_os = "linux")]
fn running(pid: u32) -> bool {
    linux::state(pid).is_some_and(|state| state != 'Z' && state != 'X')
}

#[cfg(all(unix, not(target_os = "linux")))]
fn running(pid: u32) -> bool {
    super::process::pid_exists(pid)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    fn pids() -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    }

    /// `(state, parent pid)` from `/proc/<pid>/stat` (`pid (comm) state ppid ...`; `comm` may
    /// hold spaces and parentheses, so the fields after its last `)` are read).
    fn stat(pid: u32) -> Option<(char, u32)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let parent = fields.next()?.parse().ok()?;
        Some((state, parent))
    }

    pub(super) fn state(pid: u32) -> Option<char> {
        stat(pid).map(|(state, _)| state)
    }

    pub(super) fn parent_table() -> Vec<(u32, u32)> {
        pids()
            .into_iter()
            .filter_map(|pid| Some((pid, stat(pid)?.1)))
            .collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    }

    pub(super) fn processes_referencing(root: &Path) -> Vec<u32> {
        let own = std::process::id();
        let roots: Vec<std::path::PathBuf> = match root.canonicalize() {
            Ok(canonical) if canonical != root => vec![root.to_path_buf(), canonical],
            _ => vec![root.to_path_buf()],
        };
        pids()
            .into_iter()
            .filter(|pid| *pid != own)
            .filter(|pid| {
                let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
                let argv = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
                roots.iter().any(|root| {
                    let needle = root.as_os_str().as_encoded_bytes();
                    cwd.as_ref().is_some_and(|cwd| cwd.starts_with(root))
                        || contains(&argv, needle)
                        || contains(&environ, needle)
                })
            })
            .collect()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    fn alive(pid: u32) -> bool {
        running(pid)
    }

    /// A detached grandchild in its own process group (the daemon-worker shape) dies with the
    /// tree, and a process merely working in a directory is found by that directory.
    #[test]
    fn the_whole_tree_dies_including_a_detached_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        // The child forks a grandchild into a new session (`setsid`), records its pid, and
        // waits; the grandchild works in `dir`.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "cd {dir} && setsid sh -c 'echo $$ > {pid}; exec sleep 600' & wait",
                dir = dir.path().display(),
                pid = pid_file.display()
            ))
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let grandchild: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the grandchild never started");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(processes_referencing(dir.path()).contains(&grandchild));

        let signalled = kill_process_tree(child.id());
        let _ = child.wait();
        assert!(signalled.contains(&grandchild), "{signalled:?}");
        assert!(!alive(grandchild), "the detached grandchild survived");
        assert!(processes_referencing(dir.path()).is_empty());
    }

    #[test]
    fn this_process_is_never_signalled() {
        assert_eq!(kill_process_tree(std::process::id()), Vec::<u32>::new());
    }
}
