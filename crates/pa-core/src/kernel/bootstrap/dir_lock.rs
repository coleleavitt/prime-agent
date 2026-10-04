//! Cross-process bootstrap lock for the kernel venv: a `link(2)`-published
//! lock file whose owner is a live process (pid plus process start
//! identity); stale locks are renamed aside, verified, then reclaimed.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::venv::{
    BOOTSTRAP_LOCK_NAME, BOOTSTRAP_LOCK_RETRY_MS, BOOTSTRAP_LOCK_STALE_WITHOUT_PID_MS,
};

/// The recorded owner still holds the lock (TS `isProcessIdentityAlive`): its
/// pid is live and, when the lock recorded a start identity, the pid still
/// names that process. A pid alone cannot tell a live owner from an
/// unrelated process the OS gave the same pid after the owner died. Fails
/// safe toward alive when no identity was recorded or the current one
/// cannot be read.
fn is_owner_alive(owner: &LockOwner) -> bool {
    if !crate::platform::process::pid_exists(owner.pid) {
        return false;
    }
    match (
        owner.start_id.as_deref(),
        pa_types::platform::process::process_start_id(owner.pid),
    ) {
        (Some(recorded), Some(current)) => recorded == current,
        (None, _) | (_, None) => true,
    }
}

/// The owner a lock file records: the pid on the first line and, optionally,
/// the process start identity on the second (TS `parseOwner`).
/// Newline-delimited because the macOS/BSD identity (`ps` lstart) contains
/// spaces; a pid-only lock written before identities existed still reads.
#[derive(Debug, PartialEq, Eq)]
struct LockOwner {
    pid: u32,
    start_id: Option<String>,
}

fn parse_owner(raw: &str) -> Option<LockOwner> {
    let mut lines = raw.lines();
    let pid = strict_pid(lines.next())?;
    let start_id = lines
        .next()
        .map(str::trim)
        .filter(|start_id| !start_id.is_empty())
        .map(str::to_string);
    Some(LockOwner { pid, start_id })
}

/// This process's lock content: its pid and, when the platform exposes one
/// that fits the line format, its start identity.
fn owner_content() -> String {
    let pid = std::process::id();
    match pa_types::platform::process::process_start_id(pid)
        .filter(|start_id| !start_id.is_empty() && !start_id.contains(['\r', '\n']))
    {
        Some(start_id) => format!("{pid}\n{start_id}\n"),
        None => format!("{pid}\n"),
    }
}

/// A `link(2)`-published lock file: born with owner content, EEXIST the only
/// collision signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirLockAttempt {
    Acquired,
    Held,
    Reclaimed,
}

fn strict_pid(raw: Option<&str>) -> Option<u32> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parsed: u32 = trimmed.parse().ok()?;
    (parsed > 0).then_some(parsed)
}

fn try_acquire_dir_lock(lock_path: &Path) -> anyhow::Result<DirLockAttempt> {
    std::fs::create_dir_all(lock_path.parent().unwrap_or(Path::new("/")))?;
    let token = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    let temp_path = lock_path.with_file_name(format!(
        "{}.candidate-{}",
        lock_path.file_name().unwrap_or_default().to_string_lossy(),
        token
    ));
    {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temp_path)?;
        file.write_all(owner_content().as_bytes())?;
    }
    // The primary signal: link() publishing the candidate under the lock path.
    if std::fs::hard_link(&temp_path, lock_path).is_ok() {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(DirLockAttempt::Acquired);
    }
    // Judge the incumbent: dead owner (or no owner readable) means stale.
    let judge = match std::fs::read_to_string(lock_path) {
        Ok(raw) => parse_owner(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&temp_path);
            return Ok(DirLockAttempt::Reclaimed);
        }
        Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
            // Legacy directory lock from the old protocol.
            let pid_file = lock_path.join("pid");
            match std::fs::read_to_string(pid_file) {
                Ok(raw) => parse_owner(&raw),
                Err(_) => None,
            }
        }
        Err(_) => None,
    };
    let stale = match judge {
        None => lock_missing_pid_is_stale(lock_path),
        Some(owner) => !is_owner_alive(&owner),
    };
    let result = if stale {
        let aside_path = lock_path.with_file_name(format!(
            "{}.stale-{}",
            lock_path.file_name().unwrap_or_default().to_string_lossy(),
            token
        ));
        match std::fs::rename(lock_path, &aside_path) {
            Ok(()) => {
                // A legacy directory lock moves aside whole.
                let removed = if aside_path.is_dir() {
                    std::fs::remove_dir_all(&aside_path)
                } else {
                    std::fs::remove_file(&aside_path)
                };
                if removed.is_ok() {
                    DirLockAttempt::Reclaimed
                } else {
                    DirLockAttempt::Held
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirLockAttempt::Reclaimed,
            Err(_) => DirLockAttempt::Held,
        }
    } else {
        DirLockAttempt::Held
    };
    let _ = std::fs::remove_file(&temp_path);
    Ok(result)
}

fn lock_missing_pid_is_stale(lock_path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(lock_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    std::time::SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age.as_millis() as u64 > BOOTSTRAP_LOCK_STALE_WITHOUT_PID_MS)
}

/// Serialize concurrent bootstraps across processes on the same venv.
pub(crate) async fn acquire_bootstrap_lock(venv: &Path) -> anyhow::Result<impl Drop> {
    let lock_dir = venv.with_file_name(format!(
        "{}{}",
        venv.file_name().unwrap_or_default().to_string_lossy(),
        BOOTSTRAP_LOCK_NAME
    ));
    std::fs::create_dir_all(lock_dir.parent().unwrap_or(Path::new("/")))?;
    loop {
        match try_acquire_dir_lock(&lock_dir)? {
            DirLockAttempt::Acquired => {
                struct Guard(PathBuf);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        let _ = std::fs::remove_file(&self.0);
                    }
                }
                return Ok(Guard(lock_dir));
            }
            DirLockAttempt::Held | DirLockAttempt::Reclaimed => {
                tokio::time::sleep(std::time::Duration::from_millis(BOOTSTRAP_LOCK_RETRY_MS)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This process is alive, so a pid-only judgement always calls its lock
    // held. A recorded start identity that is not this process stands in for
    // the OS having recycled a dead owner's pid.
    #[test]
    fn a_lock_whose_live_pid_now_names_another_process_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("venv.bootstrap.lock");
        std::fs::write(
            &lock_path,
            format!(
                "{}\nproc:not-the-process-that-took-this-lock\n",
                std::process::id()
            ),
        )
        .unwrap();
        assert_eq!(
            try_acquire_dir_lock(&lock_path).unwrap(),
            DirLockAttempt::Reclaimed
        );
        assert!(!lock_path.exists());
    }

    #[test]
    fn a_lock_records_the_pid_and_start_identity_on_separate_lines() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("venv.bootstrap.lock");
        assert_eq!(
            try_acquire_dir_lock(&lock_path).unwrap(),
            DirLockAttempt::Acquired
        );
        let pid = std::process::id();
        let expected = match pa_types::platform::process::process_start_id(pid) {
            Some(start_id) => format!("{pid}\n{start_id}\n"),
            None => format!("{pid}\n"),
        };
        assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), expected);
        // The live owner keeps holding it.
        assert_eq!(
            try_acquire_dir_lock(&lock_path).unwrap(),
            DirLockAttempt::Held
        );
    }

    #[test]
    fn the_owner_parses_with_and_without_a_start_identity() {
        // The macOS/BSD identity is `ps` lstart output, spaces included.
        assert_eq!(
            parse_owner("42\nps:Wed Sep 16 04:10:01 2026\n"),
            Some(LockOwner {
                pid: 42,
                start_id: Some("ps:Wed Sep 16 04:10:01 2026".to_string()),
            })
        );
        assert_eq!(
            parse_owner("42\n"),
            Some(LockOwner {
                pid: 42,
                start_id: None,
            })
        );
        assert_eq!(
            parse_owner("42"),
            Some(LockOwner {
                pid: 42,
                start_id: None,
            })
        );
        assert_eq!(parse_owner("not-a-pid\nproc:1\n"), None);
    }

    #[test]
    fn a_lock_without_a_start_identity_falls_back_to_the_pid() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("venv.bootstrap.lock");
        std::fs::write(&lock_path, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(
            try_acquire_dir_lock(&lock_path).unwrap(),
            DirLockAttempt::Held
        );
    }

    #[test]
    fn a_legacy_directory_lock_is_judged_by_its_identity_too() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("venv.bootstrap.lock");
        std::fs::create_dir(&lock_path).unwrap();
        std::fs::write(
            lock_path.join("pid"),
            format!(
                "{}\nproc:not-the-process-that-took-this-lock\n",
                std::process::id()
            ),
        )
        .unwrap();
        assert_eq!(
            try_acquire_dir_lock(&lock_path).unwrap(),
            DirLockAttempt::Reclaimed
        );
    }
}
