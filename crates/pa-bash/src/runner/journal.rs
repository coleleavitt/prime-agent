//! The orphan-process journal: one JSON line per spawned command, active at
//! spawn and inactive once its whole process group is gone, so a host that
//! loses the kernel (or this process) can reap what was left behind (TS
//! `core/orphan-process-journal.ts`; pa-core's `kernel::orphan_journal`
//! reads it).
//!
//! Tracking is best-effort: a journal that cannot be written leaves the
//! command untracked, never failed. Only a misconfigured owner pid refuses.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde_json::Value;

use super::clock::iso_utc;
use super::pyjson::dumps;

/// Where the host asks commands to be journaled.
const JOURNAL_ENV: &str = "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL";
/// The host process that owns the kernel (and reaps its orphans).
const OWNER_ENV: &str = "PRIME_AGENT_KERNEL_OWNER_PID";

/// Where one kernel's commands are journaled.
#[derive(Debug, Clone)]
pub(crate) struct Journal {
    target: Option<(String, i64)>,
    kernel_pid: u32,
    env: BTreeMap<String, String>,
}

/// The journal is configured but its owner pid is not a number: the command
/// must not run (the host reaper would never see it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnrollmentRefused;

impl Journal {
    /// The journal the kernel environment names, for commands of the kernel
    /// process `kernel_pid`.
    ///
    /// # Errors
    ///
    /// [`EnrollmentRefused`] when the journal is configured with an owner pid
    /// that is not an integer.
    pub(crate) fn from_env(
        env: &BTreeMap<String, String>,
        kernel_pid: u32,
    ) -> Result<Self, EnrollmentRefused> {
        let path = env.get(JOURNAL_ENV).filter(|value| !value.is_empty());
        let owner = env.get(OWNER_ENV).filter(|value| !value.is_empty());
        let target = match (path, owner) {
            (Some(path), Some(owner)) => {
                let owner = parse_python_int(owner).ok_or(EnrollmentRefused)?;
                Some((path.clone(), owner))
            }
            (Some(_) | None, None) | (None, Some(_)) => None,
        };
        Ok(Self {
            target,
            kernel_pid,
            env: env.clone(),
        })
    }

    /// Append one record for `pid`. Active records carry the process start
    /// identity when it can be read (identity-free records stay valid).
    pub(crate) fn record(&self, pid: u32, active: bool) {
        let Some((path, owner)) = &self.target else {
            return;
        };
        let start_id = if active {
            process_start_id(pid, &self.env)
        } else {
            None
        };
        let mut record = serde_json::Map::new();
        record.insert("version".into(), Value::from(1));
        record.insert("pid".into(), Value::from(pid));
        record.insert("ownerPid".into(), Value::from(*owner));
        // The host reaps bash children per kernel pid when it kills or loses
        // that kernel.
        record.insert("kernelPid".into(), Value::from(self.kernel_pid));
        if let Some(start_id) = start_id {
            record.insert("processStartId".into(), Value::from(start_id));
        }
        record.insert("active".into(), Value::from(active));
        record.insert("recordedAt".into(), Value::from(iso_utc(SystemTime::now())));
        let mut line = dumps(&Value::Object(record));
        line.push('\n');
        let _ = append(path, line.as_bytes());
    }
}

/// Python `int(text)` for the decimal forms a pid variable carries.
fn parse_python_int(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
    if digits.is_empty() || !digits.trim_start_matches('-').chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    digits.replace('_', "").parse().ok()
}

fn append(path: &str, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// The process start identity the host compares before reaping a journaled
/// pid, so a recycled pid is never killed: `proc:<starttime>` from
/// `/proc/<pid>/stat`, else `ps:<lstart>` (pinned to the C locale and UTC),
/// or on Windows `win:<StartTime ticks>`.
fn process_start_id(pid: u32, env: &BTreeMap<String, String>) -> Option<String> {
    if cfg!(windows) {
        return windows_start_id(pid, env);
    }
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(close) = stat.rfind(')') {
            let fields: Vec<&str> = stat.get(close + 2..).unwrap_or_default().split(' ').collect();
            if let Some(start) = fields.get(19).filter(|field| !field.is_empty()) {
                return Some(format!("proc:{start}"));
            }
        }
    }
    let ps = if cfg!(target_os = "macos") { "/bin/ps" } else { "ps" };
    let mut command = Command::new(ps);
    command
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env_clear()
        .envs(env)
        .envs([("LC_ALL", "C"), ("LC_TIME", "C"), ("LANG", "C"), ("TZ", "UTC")]);
    let out = run_bounded(command)?;
    let out = out.trim();
    (!out.is_empty()).then(|| format!("ps:{out}"))
}

fn windows_start_id(pid: u32, env: &BTreeMap<String, String>) -> Option<String> {
    let root = env
        .get("SystemRoot")
        .cloned()
        .unwrap_or_else(|| r"C:\Windows".to_string());
    let powershell = std::path::Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let mut command = Command::new(powershell);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "([System.Diagnostics.Process]::GetProcessById({pid})).StartTime.ToUniversalTime().Ticks"
            ),
        ])
        .env_clear()
        .envs(env)
        .env("NoDefaultCurrentDirectoryInExePath", "1");
    let out = run_bounded(command)?;
    let out = out.trim();
    (!out.is_empty() && out.chars().all(|c| c.is_ascii_digit())).then(|| format!("win:{out}"))
}

/// Run a short helper with a 5 s bound and return its stdout.
fn run_bounded(mut command: Command) -> Option<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        let _ = sender.send(text);
    });
    let text = receiver.recv_timeout(std::time::Duration::from_secs(5)).ok();
    if text.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Python `test_journal_bad_owner_pid_rejects` / `test_unconfigured_journal_stays_permissive`.
    #[test]
    fn only_a_bad_owner_pid_refuses() {
        assert_eq!(
            Journal::from_env(&env(&[(JOURNAL_ENV, "/tmp/j"), (OWNER_ENV, "notanint")]), 1).err(),
            Some(EnrollmentRefused)
        );
        assert!(Journal::from_env(&env(&[]), 1).is_ok());
        assert!(Journal::from_env(&env(&[(OWNER_ENV, "notanint")]), 1).is_ok());
    }

    /// Python `test_journal_configured_but_unwritable_runs_untracked`.
    #[test]
    fn an_unwritable_journal_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::from_env(
            &env(&[(JOURNAL_ENV, &dir.path().display().to_string()), (OWNER_ENV, "7")]),
            9,
        )
        .expect("configured");
        journal.record(std::process::id(), false);
    }

    #[test]
    fn records_use_python_json_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.jsonl");
        let journal = Journal::from_env(
            &env(&[(JOURNAL_ENV, &path.display().to_string()), (OWNER_ENV, "7")]),
            9,
        )
        .expect("configured");
        journal.record(std::process::id(), true);
        journal.record(std::process::id(), false);
        let text = std::fs::read_to_string(&path).expect("journal");
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["active"], Value::Bool(true));
        assert_eq!(lines[0]["ownerPid"], Value::from(7));
        assert_eq!(lines[0]["kernelPid"], Value::from(9));
        assert!(lines[0]["processStartId"].as_str().is_some_and(|id| id.starts_with("proc:") || id.starts_with("ps:")));
        assert_eq!(lines[1].get("processStartId"), None);
        assert!(text.starts_with("{\"version\": 1, \"pid\": "));
    }
}
