//! User-correction + self-correction taps.
//!
//! The pa-daemon retrieval tick drives both after each settled turn.
//! Each produces [`pa_retrieval::signal::Signal`]s attributed to the
//! memory ids in the digest of the turn that preceded the correction
//! event.
//!
//! # Where the signals come from
//!
//! - **User correction** — the user's next message after an assistant
//!   turn contains any of a small set of correction markers
//!   (`actually`, `didn't work`, `still broken`, `you missed`, …).
//!   Reads session jsonl only; no joins past the turn N → turn N+1
//!   boundary.
//!
//! - **Self correction** — turn N's bash emitted a `SilentFailure`
//!   fingerprint (caught by [`crate::silent_failure`]) AND turn N+1's
//!   bash matches a corrective pattern (`playwright install`,
//!   `pip install`, `cargo clean`, `rustup install`, `npm install`).
//!   The corrective command is ground truth that turn N's failure was
//!   real AND fixable, so the memories shown in turn N were insufficient.
//!
//! # Memory attribution
//!
//! Both taps resolve "memories shown in turn N" through the
//! `digest_memories.jsonl` sidecar [`pa_core::session_engine::harness_digest`]
//! writes on every digest render. The join is by TIMESTAMP: find the
//! most recent sidecar entry with timestamp ≤ the correction event.
//! Imprecise if two digests rendered in <1 ms of each other, but at
//! prime-agent's cadence (minutes per turn) that never happens.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::{fs, io};

use pa_retrieval::Outcome;
use pa_retrieval::signal::{Signal, SignalCause};
use regex::Regex;
use serde::Deserialize;

/// Cursor sidecar names.
pub const USER_CORRECTION_CURSOR: &str = "user_correction_tap.cursor";
pub const SELF_CORRECTION_CURSOR: &str = "self_correction_tap.cursor";

/// Lower-case prose markers of a user correction. Tuned to this user's
/// actual corpus style (mined from ~50 recent sessions, 2026-11-07):
/// "no just <verb>", "slop", "garbage", "still doesn't", "this is
/// wrong", "undo"/"revert"/"redo", "I meant", "wait" / "hold on".
///
/// Three precision gates stop UI/log noise from the agent's own
/// output (which can leak into user-role rows through paste):
///
/// 1. A leading line-drawing or bullet character (`│`, `├`, `●`, …)
///    is disqualifying — those rows are shell/TUI renders.
/// 2. "No <abstract-noun>" shapes like "No alias information exists"
///    are table/data output, not corrections; the `no` variant
///    requires a following verb/pronoun/determiner, not a bare noun.
/// 3. `actually` / `incorrect` / `that's wrong` stay in the matrix
///    as a safety net for the CC-style shapes the user occasionally
///    does type.
///
/// False positives here are expensive: every hit emits Misled signals
/// for every memory in the digest at that turn, so a wrong match
/// demotes healthy memories. The regex trades recall for precision.
/// Case-insensitive at match time.
fn correction_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?xi)
            (?:
              # Classic CC-style markers (kept as a safety net).
              \b(?:actually|didn'?t\ work|
                  still\ broken|
                  still\ (?:doesn'?t|does\ not)\ work|
                  you\ (?:missed|forgot|got\ this\ wrong)|
                  try\ again|
                  that(?:'s|\ is)\ not\ (?:right|what)|
                  nope[,.!]|
                  incorrect|
                  wrong\ (?:answer|again))\b
              |
              # This user's actual style: 'no' + verb/pronoun/determiner
              # (not 'No <abstract-noun>' which is table output).
              \bno\ (?:just|you|you're|the|that|that's|it|it's|its|wait|
                      don'?t|do\ not|we|i|idk|wrong|forget)\b
              |
              # Pejoratives the user leans on.
              \b(?:slop|garbage|trash)\b
              |
              # Explicit undo/revert/redo asks.
              \b(?:undo|revert|redo|roll[-\ ]?back|do\ over|back\ out)\b
              |
              # 'this is <bad>' — clear verdict.
              \bthis\ is\ (?:wrong|broken|failing|bad|slop|garbage|
                             incorrect|trash|not\ what)\b
              |
              # Interruptions: the user stopping the agent.
              \b(?:wait,|hold\ on|hang\ on)\b
              |
              # 'I meant' — intent correction.
              \bi\ (?:meant|said)\b
            )
            ",
        )
        .expect("user-correction regex compiles")
    })
}

/// Guard against UI/log rows that leaked into user-role content via paste.
/// Any leading line-drawing/bullet character disqualifies the row.
fn looks_like_pasted_ui(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    matches!(
        first,
        '│' | '├'
            | '└'
            | '┐'
            | '┘'
            | '┌'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '─'
            | '●'
            | '■'
            | '○'
            | '⎿'
            | '◇'
            | '★'
            | '✓'
            | '✗'
            | '>'
    )
}

/// Corrective bash patterns: an agent self-correction after a failure.
/// Matched at the start of a (whitespace-stripped) command line, so a
/// chained `foo && pip install …` matches.
///
/// These are the NARROW signal (dependency fixup, environment fixup):
/// when the agent runs one of these right after a silent failure, the
/// failure was certainly in the environment the memories didn't warn
/// about. Kept as a precision gate for `is_narrow_corrective`; the main
/// self-correction signal now rides `is_recovery` (silent failure then
/// the SAME command class succeeds).
const CORRECTIVE_PREFIXES: &[&str] = &[
    "playwright install",
    "pip install",
    "pip3 install",
    "python -m pip install",
    "uv pip install",
    "uv add",
    "cargo clean",
    "cargo install",
    "cargo fetch",
    "cargo update",
    "npm install",
    "npm ci",
    "pnpm install",
    "yarn install",
    "bun install",
    "rustup install",
    "rustup target add",
    "apt install",
    "apt-get install",
    "brew install",
    "git pull",
];

fn is_narrow_corrective(cmd: &str) -> bool {
    let trimmed = cmd.trim_start();
    CORRECTIVE_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

/// The broader "retry" shape: command N failed silently, command N+1
/// is of the SAME command CLASS (first token or `python -m <mod>`) and
/// did NOT re-emit a silent-failure signature. That's agent
/// self-correction: it re-ran the same thing after presumably fixing
/// code in between, and the retry succeeded. This matches the real
/// corpus (260 Tracebacks vs 0 `pip install` prefixes in one session's
/// bash rolls) far better than `is_narrow_corrective` alone.
fn is_retry_recovery(prior_cmd: &str, follow_cmd: &str, follow_text: &str) -> bool {
    // Same command CLASS (first argv token, or `python -m <module>`).
    let class = |s: &str| -> String {
        let trimmed = s.trim_start();
        // Strip a leading `!` (ipython-cell bang).
        let trimmed = trimmed.trim_start_matches('!').trim_start();
        let first = trimmed.split_whitespace().next().unwrap_or("");
        // Collapse `python -m <mod>` to `python:<mod>` so unittest/pytest
        // etc. count as distinct classes.
        if first == "python" || first == "python3" {
            let tokens: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(idx) = tokens.iter().position(|t| *t == "-m") {
                if let Some(module) = tokens.get(idx + 1) {
                    return format!("{first}:{module}");
                }
            }
        }
        first.to_string()
    };
    let prior_class = class(prior_cmd);
    let follow_class = class(follow_cmd);
    if prior_class.is_empty() || prior_class != follow_class {
        return false;
    }
    // The retry must NOT contain a silent-failure signature of its own:
    // otherwise it's a second failure, not a recovery.
    if crate::silent_failure::silent_failure_line_bare(follow_text).is_some() {
        return false;
    }
    true
}

/// A tap over one session's jsonl + the shared digest-memories sidecar.
#[derive(Debug, Clone)]
pub struct CorrectionTap {
    /// The agent dir's `harness/` directory.
    pub harness_dir: PathBuf,
    /// The active session's jsonl file.
    pub session_jsonl: PathBuf,
}

impl CorrectionTap {
    /// Build a tap pointing at the newest session jsonl in `agent_dir/sessions/`.
    /// Returns `None` when no session file exists yet (first boot).
    #[must_use]
    pub fn for_latest_session(agent_dir: &Path) -> Option<Self> {
        let harness_dir = agent_dir.join("harness");
        let sessions_dir = agent_dir.join("sessions");
        let newest = fs::read_dir(&sessions_dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())?
            .path();
        Some(Self {
            harness_dir,
            session_jsonl: newest,
        })
    }

    /// Scan for user-correction events since the last drain. Returns one
    /// Signal per (`memory_id`, correction turn) pair.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] on cursor-write failure.
    pub fn drain_user_corrections(&self) -> io::Result<Vec<Signal>> {
        let cursor_path = self.harness_dir.join(USER_CORRECTION_CURSOR);
        let (new_messages, new_offset) = tail_jsonl(&self.session_jsonl, &cursor_path)?;
        if new_messages.is_empty() {
            return Ok(Vec::new());
        }
        let sidecar = load_digest_sidecar(&self.harness_dir);
        let mut signals = Vec::new();
        for msg in &new_messages {
            if msg.role != "user" {
                continue;
            }
            // Precision gate: a leading line-drawing glyph means this
            // user-role row is a pasted agent/UI render, not the user
            // speaking. Match the FIRST non-whitespace line only — a
            // chat-style message won't start with a `│`.
            if looks_like_pasted_ui(msg.text()) {
                continue;
            }
            let text = msg.text().to_lowercase();
            if !correction_regex().is_match(&text) {
                continue;
            }
            // Find the most recent digest render BEFORE this user message.
            let Some(ids) = memories_before(&sidecar, &msg.timestamp) else {
                continue;
            };
            for memory_id in ids {
                signals.push(Signal {
                    memory_id: memory_id.clone(),
                    outcome: Outcome::Misled,
                    cause: SignalCause::Superseded {
                        // Reuse the Superseded cause for now — the precise
                        // cause ("UserCorrection") isn't in pa-retrieval's
                        // enum yet; the memory_id + Misled is the
                        // load-bearing signal, cause is audit trail.
                        successor: format!("user_correction@{}", msg.timestamp),
                    },
                });
            }
        }
        write_cursor(&cursor_path, new_offset)?;
        Ok(signals)
    }

    /// Scan for self-correction events since the last drain.
    ///
    /// Two disjoint pattern families (both emit `Misled` at the memory
    /// ids visible in the digest at failure time):
    ///
    /// - **Narrow fixup** — turn N's bash emits a `SilentFailure` AND
    ///   turn N+1's bash matches one of [`CORRECTIVE_PREFIXES`]
    ///   (`pip install`, `cargo clean`, …). High precision, low recall.
    /// - **Retry recovery** — turn N's bash emits a `SilentFailure`,
    ///   turn N+1 re-runs the SAME command class, and turn N+1's
    ///   output no longer carries a silent-failure signature. Catches
    ///   the common case where the agent fixed code between runs and
    ///   the retry succeeded. High recall against this corpus (260
    ///   `Traceback`s vs 0 install-prefixes in a 24-hour session's
    ///   bash rolls, measured 2026-11-07), still precision-gated by
    ///   the class + no-regression check.
    ///
    /// Attribution is always to the digest memories active at the
    /// failing turn, not the recovery turn.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] on cursor-write failure.
    pub fn drain_self_corrections(&self) -> io::Result<Vec<Signal>> {
        let cursor_path = self.harness_dir.join(SELF_CORRECTION_CURSOR);
        let (messages, new_offset) = tail_jsonl(&self.session_jsonl, &cursor_path)?;
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let sidecar = load_digest_sidecar(&self.harness_dir);
        let mut signals = Vec::new();

        // Walk the stream keeping track of the LAST (toolCall,
        // toolResult) pair that emitted a silent failure; when the
        // next pair satisfies either `is_narrow_corrective` on the
        // follow call or `is_retry_recovery` across the two pairs, we
        // emit a Misled signal and reset.
        //
        // `last_fail_pair = Some((prior_call_cmd, prior_result_ts))` holds
        // the command text and timestamp of the last silent-failure pair
        // so attribution keys on the FAILING turn, not the recovery turn.
        let mut last_fail_pair: Option<LastFail> = None;
        // The command text of the toolCall most recently seen (so we
        // can pair it with its toolResult on the next iteration).
        let mut pending_call: Option<PendingCall> = None;
        for msg in &messages {
            match msg.role.as_str() {
                "toolCall" => {
                    pending_call = Some(PendingCall {
                        cmd: msg.bash_command(),
                    });
                }
                "toolResult" => {
                    let Some(pc) = pending_call.take() else {
                        continue;
                    };
                    if msg.is_error {
                        // A hard failure: not the "silent" case we want,
                        // and it resets the chain so a later recovery
                        // isn't pinned on an unrelated memory digest.
                        last_fail_pair = None;
                        continue;
                    }
                    let this_result_has_failure =
                        crate::silent_failure::silent_failure_line_bare(msg.text()).is_some();
                    // If a prior failure exists, check if THIS pair is
                    // the recovery.
                    if let Some(fail) = &last_fail_pair {
                        let narrow = is_narrow_corrective(&pc.cmd);
                        let retry = is_retry_recovery(&fail.cmd, &pc.cmd, msg.text());
                        if narrow || retry {
                            if let Some(ids) = memories_before(&sidecar, &fail.timestamp) {
                                for memory_id in ids {
                                    signals.push(Signal {
                                        memory_id: memory_id.clone(),
                                        outcome: Outcome::Misled,
                                        cause: SignalCause::Superseded {
                                            successor: format!(
                                                "self_correction@{}",
                                                fail.timestamp
                                            ),
                                        },
                                    });
                                }
                            }
                            // Consumed: don't fire twice for the same
                            // failing turn.
                            last_fail_pair = None;
                            continue;
                        }
                    }
                    // Not a recovery (no prior failure, or neither path
                    // matched). If this pair IS a silent failure, open
                    // a new window.
                    if this_result_has_failure {
                        last_fail_pair = Some(LastFail {
                            cmd: pc.cmd,
                            timestamp: msg.timestamp.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        write_cursor(&cursor_path, new_offset)?;
        Ok(signals)
    }
}

/// The toolCall's bash command text, waiting for its toolResult to pair.
#[derive(Debug, Clone)]
struct PendingCall {
    cmd: String,
}

/// A silent-failure (toolCall, toolResult) pair — the "prior" of a
/// potential recovery.
#[derive(Debug, Clone)]
struct LastFail {
    cmd: String,
    timestamp: String,
}

/// A single row from a session jsonl, viewed only as the fields the
/// taps care about.
#[derive(Debug, Clone, Deserialize)]
struct SessionRow {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    message: Option<InnerMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct InnerMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

/// What the tap uses after deserialising.
#[derive(Debug, Clone)]
struct SessionMessage {
    role: String,
    timestamp: String,
    content: serde_json::Value,
    is_error: bool,
}

impl SessionMessage {
    fn text(&self) -> &str {
        match &self.content {
            serde_json::Value::String(s) => s.as_str(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .find_map(|p| p.get("text").and_then(|v| v.as_str()))
                .unwrap_or_default(),
            _ => "",
        }
    }

    fn bash_command(&self) -> String {
        // Tool call: content is the input value; for python_repl/bash the
        // "command" field holds the shell string.
        if let Some(cmd) = self
            .content
            .get("command")
            .and_then(serde_json::Value::as_str)
        {
            return cmd.to_string();
        }
        // ipython cell runs: `code` field; look for the first bash-style line.
        if let Some(code) = self.content.get("code").and_then(serde_json::Value::as_str) {
            for line in code.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('!') {
                    return trimmed.trim_start_matches('!').trim().to_string();
                }
            }
            return code.to_string();
        }
        String::new()
    }
}

fn parse_session_row(bytes: &[u8]) -> Option<SessionMessage> {
    let row: SessionRow = serde_json::from_slice(bytes).ok()?;
    let inner = row.message?;
    Some(SessionMessage {
        role: inner.role,
        timestamp: row.timestamp,
        content: inner.content,
        is_error: inner.is_error,
    })
}

fn tail_jsonl(path: &Path, cursor_path: &Path) -> io::Result<(Vec<SessionMessage>, u64)> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let cursor = fs::read_to_string(cursor_path)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let total = bytes.len();
    if cursor >= total {
        return Ok((Vec::new(), total as u64));
    }
    let tail = &bytes[cursor..];
    let mut messages = Vec::new();
    for line in tail.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(msg) = parse_session_row(line) {
            messages.push(msg);
        }
    }
    Ok((messages, total as u64))
}

fn write_cursor(path: &Path, offset: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("cursor.tmp");
    fs::write(&tmp, offset.to_string())?;
    fs::rename(&tmp, path)
}

#[derive(Debug, Clone, Deserialize)]
struct DigestSidecarEntry {
    timestamp: String,
    #[serde(default)]
    memory_ids: Vec<String>,
}

fn load_digest_sidecar(harness_dir: &Path) -> Vec<DigestSidecarEntry> {
    let path = harness_dir.join("digest_memories.jsonl");
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<DigestSidecarEntry>(line) {
            out.push(entry);
        }
    }
    out
}

fn memories_before<'a>(sidecar: &'a [DigestSidecarEntry], timestamp: &str) -> Option<&'a [String]> {
    sidecar
        .iter()
        .rfind(|e| e.timestamp.as_str() <= timestamp)
        .map(|e| e.memory_ids.as_slice())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn user_msg(ts: &str, text: &str) -> String {
        serde_json::json!({
            "timestamp": ts,
            "message": {"role":"user","content":[{"type":"text","text": text}]},
        })
        .to_string()
    }

    fn tool_call(ts: &str, cmd: &str) -> String {
        serde_json::json!({
            "timestamp": ts,
            "message": {
                "role":"toolCall",
                "toolName":"bash",
                "content": {"command": cmd},
            },
        })
        .to_string()
    }

    fn tool_result(ts: &str, text: &str, is_error: bool) -> String {
        serde_json::json!({
            "timestamp": ts,
            "message": {
                "role":"toolResult",
                "toolName":"bash",
                "isError": is_error,
                "content": [{"type":"text","text": text}],
            },
        })
        .to_string()
    }

    fn sidecar_line(ts: &str, memory_ids: &[&str]) -> String {
        serde_json::json!({
            "timestamp": ts,
            "state_fingerprint": "abc",
            "memory_ids": memory_ids,
        })
        .to_string()
    }

    fn tap_in(dir: &Path) -> CorrectionTap {
        let harness = dir.join("harness");
        let sessions = dir.join("sessions");
        fs::create_dir_all(&harness).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        let session = sessions.join("test.jsonl");
        fs::write(&session, "").unwrap();
        CorrectionTap {
            harness_dir: harness,
            session_jsonl: session,
        }
    }

    #[test]
    fn user_correction_regex_catches_common_phrases() {
        let re = correction_regex();
        for s in [
            "actually, that's wrong",
            "no, that didn't work",
            "you missed the install step",
            "that's not right",
            "still broken",
            "try again please",
            "nope, wrong answer",
            "Still doesn't work",
        ] {
            assert!(re.is_match(&s.to_lowercase()), "should match: {s}");
        }
        for s in ["looks good", "thanks that worked", "run the tests"] {
            assert!(!re.is_match(&s.to_lowercase()), "should NOT match: {s}");
        }
    }

    #[test]
    fn user_correction_emits_misled_for_prior_digest_memories() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:00:00.000Z", &["memory_a", "memory_b"]),
            ),
        );
        write(
            &tap.session_jsonl,
            &format!(
                "{}\n{}\n",
                user_msg("2026-11-07T19:59:55.000Z", "run the tests please"),
                user_msg("2026-11-07T20:00:10.000Z", "actually, that didn't work"),
            ),
        );
        let signals = tap.drain_user_corrections().unwrap();
        assert_eq!(signals.len(), 2);
        assert!(signals.iter().all(|s| matches!(s.outcome, Outcome::Misled)));
        let ids: Vec<_> = signals.iter().map(|s| s.memory_id.clone()).collect();
        assert!(ids.contains(&"memory_a".to_string()));
        assert!(ids.contains(&"memory_b".to_string()));
    }

    #[test]
    fn user_correction_without_a_sidecar_entry_before_it_emits_nothing() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        // No digest sidecar.
        write(
            &tap.session_jsonl,
            &format!(
                "{}\n",
                user_msg("2026-11-07T20:00:10.000Z", "actually, that didn't work"),
            ),
        );
        let signals = tap.drain_user_corrections().unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn self_correction_emits_misled_when_silent_failure_precedes_corrective_cmd() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:00:00.000Z", &["memory_browser"]),
            ),
        );
        // A full pair: failing toolCall + its silent-failure toolResult,
        // then the corrective toolCall + its clean result.
        let failing_call = tool_call("2026-11-07T20:00:04.000Z", "python -m playwright run");
        let failing = tool_result(
            "2026-11-07T20:00:05.000Z",
            "BrowserType.launch: Executable doesn't exist at /home/x/chrome",
            false,
        );
        let corrective_call = tool_call("2026-11-07T20:00:06.000Z", "playwright install");
        let corrective_result = tool_result("2026-11-07T20:00:07.000Z", "done\n", false);
        write(
            &tap.session_jsonl,
            &format!("{failing_call}\n{failing}\n{corrective_call}\n{corrective_result}\n"),
        );
        let signals = tap.drain_self_corrections().unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].memory_id, "memory_browser");
        assert!(matches!(signals[0].outcome, Outcome::Misled));
    }

    /// Guards the 2026-11-07 retry-recovery pattern: silent failure in
    /// turn N, same command class re-runs cleanly in turn N+1. This is
    /// the HIGH-RECALL case the old `is_corrective`-only path missed
    /// (260 Tracebacks vs 0 install-prefixes per session's bash rolls).
    #[test]
    fn self_correction_emits_misled_on_retry_recovery_same_class_clean_output() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:10:00.000Z", &["memory_cargo_flags"]),
            ),
        );
        let fail_call = tool_call("2026-11-07T20:10:01.000Z", "cargo build --release");
        let fail = tool_result(
            "2026-11-07T20:10:02.000Z",
            "error[E0308]: mismatched types\n",
            false,
        );
        let retry_call = tool_call("2026-11-07T20:10:10.000Z", "cargo build --release");
        let retry = tool_result(
            "2026-11-07T20:10:11.000Z",
            "   Compiling foo\n    Finished `release` profile\n",
            false,
        );
        write(
            &tap.session_jsonl,
            &format!("{fail_call}\n{fail}\n{retry_call}\n{retry}\n"),
        );
        let signals = tap.drain_self_corrections().unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].memory_id, "memory_cargo_flags");
    }

    /// Guards against false positives: two consecutive silent failures
    /// of the SAME command class must NOT emit a signal — the agent
    /// hasn't recovered, it's still broken.
    #[test]
    fn self_correction_does_not_fire_on_two_failures_in_a_row() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:20:00.000Z", &["memory_x"]),
            ),
        );
        let c1 = tool_call("2026-11-07T20:20:01.000Z", "cargo build");
        let r1 = tool_result("2026-11-07T20:20:02.000Z", "error[E0308]: boom\n", false);
        let c2 = tool_call("2026-11-07T20:20:05.000Z", "cargo build");
        let r2 = tool_result(
            "2026-11-07T20:20:06.000Z",
            "error[E0308]: still boom\n",
            false,
        );
        write(&tap.session_jsonl, &format!("{c1}\n{r1}\n{c2}\n{r2}\n"));
        let signals = tap.drain_self_corrections().unwrap();
        assert!(signals.is_empty(), "two failures = not a recovery");
    }

    /// Guards against UI-paste false positives: a user-role row whose
    /// text starts with a line-drawing or bullet glyph is a pasted
    /// agent/UI render, not the user speaking, and must not trigger
    /// the user-correction tap.
    #[test]
    fn user_correction_skips_rows_that_look_like_pasted_ui() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:30:00.000Z", &["memory_m"]),
            ),
        );
        // A quoted TUI render that happens to contain the word 'slop'.
        let pasted = r#"{"type":"message","timestamp":"2026-11-07T20:30:05.000Z","message":{"role":"user","content":[{"type":"text","text":"│ slop detected in output │"}]}}"#;
        write(&tap.session_jsonl, &format!("{pasted}\n"));
        let signals = tap.drain_user_corrections().unwrap();
        assert!(
            signals.is_empty(),
            "a pasted UI row must not emit a correction"
        );
    }

    /// The real-corpus style check: the user's actual phrases
    /// ("no just", "slop", "garbage", "still doesn't work") must
    /// match the regex.
    #[test]
    fn user_correction_regex_matches_real_corpus_phrases() {
        let re = correction_regex();
        for probe in [
            "no just email reports right now please",
            "no you're generating slop like you make Page",
            "clean up the slop and stuff right idk",
            "like get rid of all the slop",
            "it's garbage",
            "this is wrong and stuff",
            "still doesn't work right",
            "undo that right idk",
            "wait, that's not what I meant",
            "I meant the other one",
        ] {
            assert!(
                re.is_match(&probe.to_lowercase()),
                "regex missed real phrase: {probe}"
            );
        }
        for noise in [
            // CRM/data table noise that STARTS with "No <abstract-noun>".
            "No alias information exists for the licensing entity.",
            "No active appointment information exists.",
            "No custom properties set for this repository.",
            // Plain prose that merely happens to use these words.
            "the no-op path is fine",
        ] {
            assert!(
                !re.is_match(&noise.to_lowercase()),
                "regex falsely flagged: {noise}"
            );
        }
    }

    #[test]
    fn self_correction_without_silent_failure_before_corrective_is_ignored() {
        // A `pip install` that follows a CLEAN previous bash is not a
        // self-correction — maybe the agent is just installing a new dep.
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:00:00.000Z", &["memory_browser"]),
            ),
        );
        let clean = tool_result("2026-11-07T20:00:05.000Z", "all good\n", false);
        let corrective = tool_call("2026-11-07T20:00:06.000Z", "pip install requests");
        write(&tap.session_jsonl, &format!("{clean}\n{corrective}\n"));
        let signals = tap.drain_self_corrections().unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn missing_session_file_is_empty_not_error() {
        let dir = tempdir().unwrap();
        let tap = CorrectionTap {
            harness_dir: dir.path().join("harness"),
            session_jsonl: dir.path().join("sessions").join("never.jsonl"),
        };
        fs::create_dir_all(&tap.harness_dir).unwrap();
        assert!(tap.drain_user_corrections().unwrap().is_empty());
        assert!(tap.drain_self_corrections().unwrap().is_empty());
    }

    #[test]
    fn cursor_advances_so_a_correction_only_fires_once() {
        let dir = tempdir().unwrap();
        let tap = tap_in(dir.path());
        write(
            &tap.harness_dir.join("digest_memories.jsonl"),
            &format!(
                "{}\n",
                sidecar_line("2026-11-07T20:00:00.000Z", &["memory_a"]),
            ),
        );
        write(
            &tap.session_jsonl,
            &format!(
                "{}\n",
                user_msg("2026-11-07T20:00:10.000Z", "no, that didn't work"),
            ),
        );
        let first = tap.drain_user_corrections().unwrap();
        assert_eq!(first.len(), 1);
        let second = tap.drain_user_corrections().unwrap();
        assert!(second.is_empty(), "cursor advanced; no replay");
    }

    #[test]
    fn narrow_corrective_prefixes_match_as_a_whole_word_at_start() {
        assert!(is_narrow_corrective("pip install requests"));
        assert!(is_narrow_corrective("  pip install requests"));
        assert!(is_narrow_corrective("playwright install"));
        assert!(is_narrow_corrective("cargo clean"));
        assert!(!is_narrow_corrective("pip --help"));
        assert!(!is_narrow_corrective("echo pip install"));
    }

    #[test]
    fn retry_recovery_matches_same_class_and_no_new_failure() {
        // Same first-token + clean follow-up output = recovery.
        assert!(is_retry_recovery(
            "cargo build",
            "cargo build --release",
            "    Finished `release` profile\n"
        ));
        // Different class = no recovery.
        assert!(!is_retry_recovery("cargo build", "ls -la", "a\nb\nc\n"));
        // Same class but follow-up ALSO has a silent failure = no recovery.
        assert!(!is_retry_recovery(
            "cargo build",
            "cargo build",
            "   Compiling foo\nerror[E0308]: mismatched types"
        ));
        // `python -m <mod>` collapses to a module-aware class.
        assert!(is_retry_recovery(
            "python -m pytest tests/foo.py",
            "python -m pytest tests/foo.py -v",
            "1 passed in 0.5s\n"
        ));
        assert!(!is_retry_recovery(
            "python -m pytest tests/foo.py",
            "python -m unittest discover",
            "ran 3 tests ok\n"
        ));
        // A bang-prefixed ipython shell line still classes correctly.
        assert!(is_retry_recovery(
            "!cargo build",
            "cargo build",
            "   Finished\n"
        ));
    }
}
