//! "✓-but-failed" bash detector: a tool result exited cleanly
//! (`is_error = false`) but its output contains a failure marker. The
//! TUI renders a green check; the agent moves on; the next turn hits
//! the same wall. These are the failures that silently rot an agent
//! session.
//!
//! Heuristic — exit-0 with failure prose:
//!
//! - `Executable doesn't exist` (Playwright)
//! - `error[E####]` (Rust)
//! - `thread '…' panicked at` or `panic!:` (Rust runtime)
//! - `FAIL\b` (cargo-test, pytest, jest)
//! - `Traceback (most recent call last)` NOT caught by the existing
//!   Python-traceback path — defensive, almost never fires
//! - `^Error:` at line start
//! - A single-tool allowlist of chatter-to-stderr-not-failure phrases
//!   (git `warning:`, cargo `note:`, pip `WARNING:`) is stripped
//!   BEFORE matching so a legitimate warning line doesn't trigger a
//!   false positive.
//!
//! This is deliberately conservative. A false positive costs the
//! agent a spurious ledger entry; a false negative means a silent
//! failure keeps being invisible. Precision over recall.

use std::sync::OnceLock;

use regex::Regex;

/// The single matching failure line (first one found, line-bounded)
/// if the output looks like a silent failure, else `None`. Exit code
/// is passed in so the caller doesn't have to pre-filter; this helper
/// is an identity function for `exit != 0`.
#[must_use]
pub fn silent_failure_line(exit_code: i32, output: &str) -> Option<String> {
    if exit_code != 0 {
        return None;
    }
    silent_failure_line_bare(output)
}

/// The exit-code-agnostic core: match the first failure line in
/// `output`, after stripping known-chatter phrases. For a caller that
/// already knows the result exited cleanly.
#[must_use]
pub fn silent_failure_line_bare(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if looks_like_chatter(trimmed) {
            continue;
        }
        if is_failure_line(trimmed) {
            return Some(clip(trimmed));
        }
    }
    None
}

fn is_failure_line(line: &str) -> bool {
    // Fast prefix/substring checks first — the regex runs only when
    // the fast path misses, since bash output is dominated by
    // unmatched lines and allocating a Regex match per line is wasteful.
    if line.contains("Executable doesn't exist")
        || line.contains("thread '") && line.contains("panicked at")
        || line.contains("panic!:")
        || line.starts_with("Error:")
        || line.starts_with("Error ")
        || line.contains("Please run the following command")
    {
        return true;
    }
    failure_regex().is_match(line)
}

/// Combined regex: rust compile error codes, cargo-test FAIL lines,
/// pytest/jest FAIL lines, standalone `FAILED` tokens.
fn failure_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?x)
            # Rust compiler errors.
            error\[E[0-9]{3,5}\]
          | # Test-framework FAIL lines: start of line, word-bound FAIL
            ^\s*FAIL(?:ED)?\b
          | # pytest short-summary line.
            ^\s*=+\s+FAILURES\s+=+
          | # Rust test harness.
            ^\s*test\s+\S+\s+\.\.\.\s+FAILED\s*$
            ",
        )
        .expect("silent-failure regex compiles")
    })
}

/// Known chatter — stderr lines that look alarming but aren't
/// failures. Ordered by frequency so the common cases short-circuit
/// first.
fn looks_like_chatter(line: &str) -> bool {
    // Git fetch / tool progress, cargo notes, pip warnings — all
    // regularly emit on exit 0.
    line.starts_with("warning:")
        || line.starts_with("note:")
        || line.starts_with("WARNING:")
        || line.starts_with("info:")
        || line.starts_with("Compiling ")
        || line.starts_with("    Finished ")
        || line.starts_with("    Checking ")
        || line.starts_with("    Blocking ")
        || line.starts_with("Downloading ")
        || line.starts_with("    Downloaded ")
        || line.starts_with("    Updating ")
        || line.starts_with("hint:")
        || line.starts_with("remote:")
        || line.contains("(use `cargo ")
}

/// Clip a line for the fingerprint message. Long outputs (pytest
/// diffs, rust error contexts) pad the fingerprint without changing
/// the identity — cap at 240 bytes.
fn clip(line: &str) -> String {
    if line.len() <= 240 {
        return line.to_string();
    }
    let mut cut = 240;
    while !line.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!("{}…", &line[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playwright_install_box_is_a_silent_failure() {
        // The exact shape the user pasted — `playwright install` prints
        // this to stderr but the bash command exits 0 because the error
        // was swallowed by the wrapping process.
        let out = "\
BrowserType.launch: Executable doesn't exist at /home/x/.cache/ms-playwright/chrome
╔════════════════════════════════════════════════════════════╗
║ Looks like Playwright was just installed or updated.       ║
║ Please run the following command to download new browsers: ║
║                                                            ║
║     playwright install                                     ║
║                                                            ║
║ <3 Playwright Team                                         ║
╚════════════════════════════════════════════════════════════╝
";
        let line = silent_failure_line(0, out).expect("match");
        assert!(line.contains("Executable doesn't exist"), "{line}");
    }

    #[test]
    fn rust_compile_error_is_a_silent_failure() {
        let out = "\
    Compiling pa-retrieval v0.10.1
error[E0308]: mismatched types
   --> src/lib.rs:42:17
    |
42  |     let x: u8 = \"hi\";
    |                 ^^^^ expected `u8`
";
        let line = silent_failure_line(0, out).expect("match");
        assert!(line.contains("E0308"), "{line}");
    }

    #[test]
    fn pytest_fail_line_is_a_silent_failure() {
        let out = "============================= FAILURES =============================\n";
        let line = silent_failure_line(0, out).expect("match");
        assert!(line.contains("FAILURES"), "{line}");
    }

    #[test]
    fn cargo_test_failed_line_is_a_silent_failure() {
        let out = "test memory_sync_picks_up_create_and_delete_edits ... FAILED\n";
        let line = silent_failure_line(0, out).expect("match");
        assert!(line.contains("FAILED"), "{line}");
    }

    #[test]
    fn rust_panic_is_a_silent_failure() {
        let out = "thread 'main' panicked at src/lib.rs:17:5:\nassertion failed\n";
        let line = silent_failure_line(0, out).expect("match");
        assert!(line.contains("panicked at"), "{line}");
    }

    #[test]
    fn cargo_warnings_are_not_failures() {
        let out = "\
warning: unused import: `foo`
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.52s
note: `#[warn(unused_imports)]` on by default
";
        assert_eq!(silent_failure_line(0, out), None);
    }

    #[test]
    fn pip_warnings_are_not_failures() {
        let out = "WARNING: Running pip as the 'root' user can result in...\nSuccessfully installed foo-1.0\n";
        assert_eq!(silent_failure_line(0, out), None);
    }

    #[test]
    fn git_warnings_are_not_failures() {
        let out = "warning: redirecting to https://github.com/foo/bar.git/\nremote: Enumerating objects: 42\n";
        assert_eq!(silent_failure_line(0, out), None);
    }

    #[test]
    fn exit_non_zero_short_circuits_so_the_tool_error_path_handles_it() {
        // Non-zero exit → the existing `observe_tool_result` path owns
        // the fingerprint. The silent-failure helper refuses so the
        // ledger doesn't get a duplicate record.
        assert_eq!(silent_failure_line(1, "error[E0308]: mismatched types"), None);
    }

    #[test]
    fn empty_and_benign_output_is_not_a_failure() {
        assert_eq!(silent_failure_line(0, ""), None);
        assert_eq!(silent_failure_line(0, "   \n\t\n"), None);
        assert_eq!(silent_failure_line(0, "hello world\n"), None);
    }

    #[test]
    fn long_failure_line_is_clipped() {
        let long = format!("error[E0308]: {}", "x".repeat(400));
        let line = silent_failure_line(0, &long).expect("match");
        // 240 bytes of content + 3-byte UTF-8 '…'.
        assert!(line.len() <= 243, "line.len() = {}", line.len());
        assert!(line.ends_with('…'));
    }
}
