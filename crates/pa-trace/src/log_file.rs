//! The local diagnostic log (TS `appendRotatingLog`): redacted lines appended
//! under a cross-process lock, owner-only files, and a bounded set of
//! retained generations. The newest rotated file stays `<path>.old`; older
//! generations are `<path>.old.<n>.gz`. Every failure is reported to the
//! caller, which treats it as best-effort.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use regex::Regex;

/// The TS agent log bound (`AGENT_LOG_MAX_BYTES`).
pub(crate) const AGENT_LOG_MAX_BYTES: u64 = 20 * 1024 * 1024;
/// Retained generations when `PRIME_AGENT_LOG_RETENTION` is unset.
const DEFAULT_RETENTION: u32 = 5;
/// The hard upper bound of retained generations.
pub(crate) const MAX_RETENTION: u32 = 100;
/// The retention override variable.
const RETENTION_ENV: &str = "PRIME_AGENT_LOG_RETENTION";
/// A held lock older than this was left by a crashed writer.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
/// Lock attempts before a batch is dropped (the TS bound: 200 x 5 ms).
const LOCK_ATTEMPTS: u32 = 200;
const LOCK_RETRY: Duration = Duration::from_millis(5);
const REDACTED: &str = "[REDACTED]";

/// One append-only log file and its rotation policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    retention: u32,
}

impl RotatingLog {
    /// The agent log at `path` with the configured retention.
    pub(crate) fn new(path: PathBuf) -> Self {
        RotatingLog {
            path,
            max_bytes: AGENT_LOG_MAX_BYTES,
            retention: configured_retention(std::env::var(RETENTION_ENV).ok().as_deref()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(path: PathBuf, max_bytes: u64, retention: u32) -> Self {
        RotatingLog {
            path,
            max_bytes,
            retention: retention.clamp(1, MAX_RETENTION),
        }
    }

    /// Append `lines` (each without its newline) as one locked batch,
    /// rotating first when the file is past its bound.
    pub(crate) fn append(&self, lines: &[String]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let lock_target = append_suffix(&self.path, ".rotation-lock");
        prepare_secure_file(&lock_target)?;
        let Some(_lock) = RotationLock::acquire(&append_suffix(&lock_target, ".lock"))? else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "log rotation lock is busy",
            ));
        };
        prepare_secure_file(&self.path)?;
        if fs::metadata(&self.path)?.len() > self.max_bytes {
            self.rotate()?;
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&redact_local_log(line));
            text.push('\n');
        }
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(text.as_bytes())?;
        set_owner_only(&self.path)
    }

    fn rotate(&self) -> io::Result<()> {
        let compressed_generations = self.retention.saturating_sub(2);
        for generation in compressed_generations + 1..MAX_RETENTION {
            remove_if_present(&self.generation_path(generation))?;
        }
        for generation in (1..compressed_generations).rev() {
            let source = self.generation_path(generation);
            if source.exists() {
                let target = self.generation_path(generation + 1);
                fs::rename(&source, &target)?;
                set_owner_only(&target)?;
            }
        }
        let previous = append_suffix(&self.path, ".old");
        if compressed_generations > 0 && previous.exists() {
            let compressed = self.generation_path(1);
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&fs::read(&previous)?)?;
            write_owner_only(&compressed, &encoder.finish()?)?;
        }
        remove_if_present(&previous)?;
        if self.retention > 1 {
            fs::rename(&self.path, &previous)?;
            set_owner_only(&previous)?;
        } else {
            remove_if_present(&self.path)?;
        }
        write_owner_only(&self.path, b"")
    }

    fn generation_path(&self, generation: u32) -> PathBuf {
        append_suffix(&self.path, &format!(".old.{generation}.gz"))
    }
}

/// `PRIME_AGENT_LOG_RETENTION` as an integer clamped to 1..=100; anything
/// unparsable keeps the default (TS `configuredLogRetention`).
fn configured_retention(value: Option<&str>) -> u32 {
    let Some(value) = value else {
        return DEFAULT_RETENTION;
    };
    // `Number.parseInt`: optional sign, then the leading digits.
    let trimmed = value.trim_start();
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return DEFAULT_RETENTION;
    }
    if negative {
        return 1;
    }
    digits.parse::<u64>().map_or(MAX_RETENTION, |parsed| {
        u32::try_from(parsed.clamp(1, u64::from(MAX_RETENTION))).unwrap_or(MAX_RETENTION)
    })
}

static REDACTIONS: LazyLock<[(Regex, &'static str); 6]> = LazyLock::new(|| {
    let pattern = |source: &str| Regex::new(source).expect("valid redaction pattern");
    [
        (
            pattern(r"(?i)(\b(?:Bearer|Basic)\s+)[A-Za-z0-9._~+/=-]+"),
            "${1}[REDACTED]",
        ),
        (
            pattern(
                r#"(?i)(\b(?:authorization|api[-_]?key|access[-_]?token|refresh[-_]?token|client[-_]?secret|password|token|cookie|set-cookie|code)\b\s*["']?\s*[:=]\s*["']?)([^\s"',;}]+)"#,
            ),
            "${1}[REDACTED]",
        ),
        (
            pattern(r"(?i)([?&](?:token|access_token|refresh_token|api_key|code)=)[^&#\s]+"),
            "${1}[REDACTED]",
        ),
        (pattern(r"(?i)(https?://)[^/@\s]+@"), "${1}[REDACTED]@"),
        (
            pattern(r"\b(?:sk-(?:ant-)?[A-Za-z0-9_-]{16,}|gh[opusr]_[A-Za-z0-9]{16,})\b"),
            REDACTED,
        ),
        (
            pattern(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b"),
            REDACTED,
        ),
    ]
});

/// Redact the high-confidence credential forms before a line reaches disk
/// (TS `redactLocalLog`).
pub(crate) fn redact_local_log(message: &str) -> String {
    let mut text = message.to_string();
    for (pattern, replacement) in REDACTIONS.iter() {
        if let std::borrow::Cow::Owned(replaced) = pattern.replace_all(&text, *replacement) {
            text = replaced;
        }
    }
    text
}

/// The `<target>.lock` directory lock the TS logger (and proper-lockfile)
/// use, so a process running either implementation serializes with this
/// one. Released on drop.
struct RotationLock {
    path: PathBuf,
}

impl RotationLock {
    /// `Ok(None)` when another writer held the lock for every attempt.
    fn acquire(path: &Path) -> io::Result<Option<RotationLock>> {
        for _ in 0..LOCK_ATTEMPTS {
            match fs::create_dir(path) {
                Ok(()) => {
                    return Ok(Some(RotationLock {
                        path: path.to_path_buf(),
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > LOCK_STALE_AFTER);
                    if stale {
                        // A writer that crashed while holding the lock left it behind.
                        let _ = fs::remove_dir_all(path);
                        continue;
                    }
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

impl Drop for RotationLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Create the owner-only logs directory and an owner-only `path`.
fn prepare_secure_file(path: &Path) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
        set_owner_only_dir(directory)?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    set_owner_only(path)
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)?;
    set_owner_only(path)
}

/// `0600` on POSIX; Windows keeps the profile directory's ACLs.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "POSIX modes only; the signature is the unix one"
    )
)]
fn set_owner_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// `0700` on POSIX; Windows keeps the profile directory's ACLs.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "POSIX modes only; the signature is the unix one"
    )
)]
fn set_owner_only_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    fn gunzip(path: &Path) -> String {
        let mut text = String::new();
        flate2::read::GzDecoder::new(fs::File::open(path).expect("open gz"))
            .read_to_string(&mut text)
            .expect("gunzip");
        text
    }

    /// Expected outputs are the TS `redactLocalLog` results for the same inputs.
    #[test]
    fn redacts_the_ts_credential_forms() {
        let cases = [
            (
                "Authorization: Bearer abc.def-123",
                "Authorization: [REDACTED] [REDACTED]",
            ),
            (
                "header Basic dXNlcjpwYXNz rest",
                "header Basic [REDACTED] rest",
            ),
            (
                r#"{"apiKey":"k-1","x":1}"#,
                r#"{"apiKey":"[REDACTED]","x":1}"#,
            ),
            ("password=hunter2;", "password=[REDACTED];"),
            ("GET /cb?code=xyz&state=1", "GET /cb?code=[REDACTED]"),
            ("https://user:pw@host/path", "https://[REDACTED]@host/path"),
            ("key sk-ant-abcdefghijklmnopqrst end", "key [REDACTED] end"),
            ("ghp_abcdefghijklmnopqrstu", "[REDACTED]"),
            ("jwt eyJhbGc.eyJzdWI.sig_part ok", "jwt [REDACTED] ok"),
            (
                r#"{"name":"tool.execute","attrs":{"tool.name":"bash"}}"#,
                r#"{"name":"tool.execute","attrs":{"tool.name":"bash"}}"#,
            ),
        ];
        let actual: Vec<String> = cases
            .iter()
            .map(|(input, _)| redact_local_log(input))
            .collect();
        let expected: Vec<String> = cases
            .iter()
            .map(|(_, output)| (*output).to_string())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn retention_parses_like_parse_int_and_clamps() {
        let values = [
            None,
            Some("3"),
            Some("0"),
            Some("-4"),
            Some("500"),
            Some("x"),
            Some("7days"),
        ];
        let parsed: Vec<u32> = values
            .iter()
            .map(|value| configured_retention(*value))
            .collect();
        assert_eq!(parsed, vec![5, 3, 1, 1, 100, 5, 7]);
    }

    #[test]
    fn appends_owner_only_lines_and_releases_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("logs").join("agent.jsonl");
        let log = RotatingLog::with_limits(path.clone(), 1024, 5);
        log.append(&["one".to_string(), "two token=abc".to_string()])
            .expect("append");
        assert_eq!(read(&path), "one\ntwo token=[REDACTED]\n");
        assert!(!append_suffix(&path, ".rotation-lock.lock").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
            assert_eq!(
                (mode(&path), mode(path.parent().expect("dir"))),
                (0o600, 0o700)
            );
        }
    }

    #[test]
    fn rotation_keeps_plain_old_and_bounded_gzip_generations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.jsonl");
        let log = RotatingLog::with_limits(path.clone(), 4, 4);
        for line in ["gen-a", "gen-b", "gen-c", "gen-d", "gen-e"] {
            log.append(&[line.to_string()]).expect("append");
        }
        // Every append past 4 bytes rotates: retention 4 keeps the live file,
        // `.old`, and two compressed generations.
        assert_eq!(
            (
                read(&path),
                read(&append_suffix(&path, ".old")),
                gunzip(&append_suffix(&path, ".old.1.gz")),
                gunzip(&append_suffix(&path, ".old.2.gz")),
                append_suffix(&path, ".old.3.gz").exists(),
            ),
            (
                "gen-e\n".to_string(),
                "gen-d\n".to_string(),
                "gen-c\n".to_string(),
                "gen-b\n".to_string(),
                false,
            )
        );
    }

    #[test]
    fn retention_one_keeps_only_the_live_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.jsonl");
        let log = RotatingLog::with_limits(path.clone(), 4, 1);
        log.append(&["first".to_string()]).expect("append");
        log.append(&["second".to_string()]).expect("append");
        assert_eq!(
            (read(&path), append_suffix(&path, ".old").exists()),
            ("second\n".to_string(), false)
        );
    }

    #[test]
    fn a_stale_lock_left_by_a_crashed_writer_is_reclaimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.jsonl");
        let lock = append_suffix(&path, ".rotation-lock.lock");
        fs::create_dir_all(&lock).expect("stale lock");
        let old = SystemTime::now() - Duration::from_secs(60);
        fs::File::open(&lock)
            .and_then(|handle| handle.set_modified(old))
            .expect("backdate lock");
        RotatingLog::with_limits(path.clone(), 1024, 5)
            .append(&["after".to_string()])
            .expect("append");
        assert_eq!(read(&path), "after\n");
    }
}
