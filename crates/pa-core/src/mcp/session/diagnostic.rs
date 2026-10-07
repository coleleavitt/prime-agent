//! Redaction-safe diagnostics for MCP connections: the bounded stderr tail of
//! a stdio server (kept only while its startup handshake runs) and the
//! sanitizer every surfaced child or SDK message passes through.

use std::sync::{Arc, Mutex, OnceLock};

use pa_types::sync::MutexExt;
use tokio::io::AsyncReadExt;

/// Bytes of stderr a stdio server's tail keeps (and a sanitized tail shows).
pub(crate) const STDERR_BYTE_LIMIT: usize = 8 * 1024;
/// Lines a stderr tail keeps.
pub(crate) const STDERR_LINE_LIMIT: usize = 40;
/// Bytes of an SDK error message a startup diagnostic keeps.
pub(crate) const ORIGINAL_ERROR_BYTE_LIMIT: usize = 1024;
const REDACTED: &str = "[REDACTED]";

fn ansi_escape() -> &'static fancy_regex::Regex {
    static PATTERN: OnceLock<fancy_regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        fancy_regex::Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)?)")
            .expect("ANSI escape pattern")
    })
}

fn is_stripped_control(c: char) -> bool {
    matches!(c, '\x00'..='\x08' | '\x0b' | '\x0c' | '\x0e'..='\x1f' | '\x7f'..='\u{9f}')
}

/// Python `str.splitlines` boundaries that survive control stripping.
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\u{2028}' | '\u{2029}')
}

/// Make child/SDK text safe to surface: normalize line endings and tabs,
/// drop ANSI escapes and control characters, replace every secret (and every
/// private configuration value) with `[REDACTED]`, keep the last
/// [`STDERR_LINE_LIMIT`] non-blank lines and at most `byte_limit` trailing
/// bytes. Secrets must arrive longest first, so a secret that contains
/// another is redacted whole.
pub(crate) fn sanitize_diagnostic(
    value: &str,
    secrets: &[String],
    private_values: &[String],
    byte_limit: usize,
) -> String {
    let normalized = value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', " ");
    let mut value: String = ansi_escape()
        .replace_all(&normalized, "")
        .chars()
        .filter(|&c| !is_stripped_control(c))
        .collect();
    for secret in secrets {
        if !secret.is_empty() {
            value = value.replace(secret.as_str(), REDACTED);
        }
    }
    for private in private_values {
        if private.chars().count() >= 4 {
            value = value.replace(private.as_str(), REDACTED);
        } else if !private.is_empty() {
            // A short value is only redacted as a whole word: a blanket
            // replace would shred unrelated text.
            let pattern = format!(r"(?<!\w){}(?!\w)", fancy_regex::escape(private));
            if let Ok(regex) = fancy_regex::Regex::new(&pattern) {
                value = regex.replace_all(&value, REDACTED).into_owned();
            }
        }
    }
    let lines: Vec<&str> = value
        .split(is_line_break)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let keep_from = lines.len().saturating_sub(STDERR_LINE_LIMIT);
    let mut joined = lines[keep_from..].join("\n");
    if joined.len() > byte_limit {
        let mut start = joined.len() - byte_limit;
        // Drop a character cut by the byte cut (Python decodes the tail
        // bytes with `errors="ignore"`).
        while !joined.is_char_boundary(start) {
            start += 1;
        }
        joined = joined[start..].to_string();
    }
    joined.trim().to_string()
}

/// The bounded tail of a stdio server's stderr. Drained continuously (so a
/// chatty child never blocks on a full pipe); bytes are kept only while
/// capture is on, which a successful startup turns off for good.
#[derive(Clone)]
pub(crate) struct StderrTail {
    state: Arc<Mutex<TailState>>,
    /// Flips to `true` when the pipe reaches EOF.
    eof: tokio::sync::watch::Sender<bool>,
}

impl Default for StderrTail {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            eof: tokio::sync::watch::Sender::new(false),
        }
    }
}

#[derive(Default)]
struct TailState {
    buffer: Vec<u8>,
    stopped: bool,
}

impl StderrTail {
    /// Drain `stderr` into this tail until EOF.
    pub(crate) fn drain(&self, mut stderr: tokio::process::ChildStderr) {
        let state = Arc::clone(&self.state);
        let eof = self.eof.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => state.lock_or_recover().append(&chunk[..read]),
                }
            }
            eof.send_replace(true);
        });
    }

    /// Wait (at most `bound`) until the pipe reaches EOF, so a tail read
    /// after the child exited holds everything it wrote.
    pub(crate) async fn wait_eof(&self, bound: std::time::Duration) {
        let mut eof = self.eof.subscribe();
        let _ = tokio::time::timeout(bound, eof.wait_for(|done| *done)).await;
    }

    /// Stop keeping bytes and forget the ones kept (startup succeeded).
    pub(crate) fn stop_capture(&self) {
        let mut state = self.state.lock_or_recover();
        state.stopped = true;
        state.buffer.clear();
    }

    /// The sanitized tail.
    pub(crate) fn tail(&self, secrets: &[String], private_values: &[String]) -> String {
        let raw = self.state.lock_or_recover().buffer.clone();
        sanitize_diagnostic(
            &String::from_utf8_lossy(&raw),
            secrets,
            private_values,
            STDERR_BYTE_LIMIT,
        )
    }
}

impl TailState {
    fn append(&mut self, bytes: &[u8]) {
        if self.stopped {
            return;
        }
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > STDERR_BYTE_LIMIT {
            let excess = self.buffer.len() - STDERR_BYTE_LIMIT;
            self.buffer.drain(..excess);
        }
        let starts = line_starts(&self.buffer);
        if starts.len() > STDERR_LINE_LIMIT {
            let cut = starts[starts.len() - STDERR_LINE_LIMIT];
            self.buffer.drain(..cut);
        }
    }
}

/// Start offsets of each line in `bytes` (Python `bytes.splitlines`: `\n`,
/// `\r`, and `\r\n` end a line).
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    if bytes.is_empty() {
        return starts;
    }
    starts.push(0);
    let mut index = 0;
    while index < bytes.len() {
        let end = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => Some(index + 2),
            b'\r' | b'\n' => Some(index + 1),
            _ => None,
        };
        match end {
            Some(next) => {
                if next < bytes.len() {
                    starts.push(next);
                }
                index = next;
            }
            None => index += 1,
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn sanitize_strips_escapes_controls_and_redacts() {
        let raw =
            "\x1b[31mImportError:\x00 broken sk-secret-value\x1b[0m\r\nnext\tline /opt/fixture.py";
        assert_eq!(
            sanitize_diagnostic(
                raw,
                &strings(&["sk-secret-value"]),
                &strings(&["/opt/fixture.py"]),
                STDERR_BYTE_LIMIT
            ),
            "ImportError: broken [REDACTED]\nnext line [REDACTED]"
        );
    }

    #[test]
    fn short_private_values_are_redacted_only_as_whole_words() {
        assert_eq!(
            sanitize_diagnostic(
                "run py in pyramid",
                &[],
                &strings(&["py"]),
                STDERR_BYTE_LIMIT
            ),
            "run [REDACTED] in pyramid"
        );
    }

    #[test]
    fn sanitize_keeps_the_last_lines_and_bytes_on_a_char_boundary() {
        let raw = (0..100).fold(String::new(), |mut raw, index| {
            let _ = writeln!(raw, "line {index}");
            raw
        });
        let kept = sanitize_diagnostic(&raw, &[], &[], STDERR_BYTE_LIMIT);
        let expected: Vec<String> = (60..100).map(|index| format!("line {index}")).collect();
        assert_eq!(kept, expected.join("\n"));
        assert_eq!(sanitize_diagnostic("aé", &[], &[], 1), "");
        assert_eq!(sanitize_diagnostic("aéb", &[], &[], 2), "b");
    }

    #[test]
    fn the_tail_is_bounded_by_bytes_and_lines_and_cleared_on_stop() {
        let tail = StderrTail::default();
        for index in 0..300 {
            tail.state.lock_or_recover().append(
                format!("oversized diagnostic line {index:04} {}\n", "x".repeat(100)).as_bytes(),
            );
        }
        tail.state.lock_or_recover().append(b"sentinel tail\n");
        let kept = tail.tail(&[], &[]);
        assert!(kept.ends_with("sentinel tail"), "{kept}");
        assert_eq!(kept.lines().count(), STDERR_LINE_LIMIT);
        assert!(kept.len() <= STDERR_BYTE_LIMIT);
        tail.stop_capture();
        tail.state.lock_or_recover().append(b"after stop\n");
        assert_eq!(tail.tail(&[], &[]), "");
    }
}
