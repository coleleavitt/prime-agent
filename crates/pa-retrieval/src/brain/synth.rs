//! Synthesis: one side-model call consolidates a scope's memories into the
//! short summary every later session of that scope sees in its digest.
//! The rules follow Claude Code 2.1.316's `auto_dream` consolidation (merge,
//! newest evidence wins, absolute dates, no speculation) and magic-context's
//! curate pass (never invent); the output passes jfc's verifier checks
//! (bounded lines, nothing credential-shaped) before it is stored. The
//! automatic run uses Claude Code's gate: at least a day since the last
//! synthesis and at least five of the scope's sessions since then.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;

use super::controls::{GLOBAL_SCOPE, MemoryControls, ScopeSummary};
use super::corpus::{BrainDoc, Source};
use super::project::{Project, project_of, session_cwd};
use super::supersede::Superseded;

/// The most lines a summary keeps.
pub(crate) const MAX_LINES: usize = 20;
/// The longest line a summary keeps (the digest's per-line bound).
pub(crate) const MAX_LINE_CHARS: usize = 140;
/// The memory text one synthesis reads at most.
const INPUT_BUDGET_CHARS: usize = 60_000;
/// One memory's text at most, inside that budget.
const MEMORY_CHARS: usize = 600;
/// The automatic gate: at least this long since the last synthesis...
pub(crate) const MIN_AGE: Duration = Duration::from_hours(24);
/// ...and at least this many sessions of the scope started since.
pub(crate) const MIN_SESSIONS: usize = 5;
/// Consecutive failures after which the automatic run stops (a manual
/// `brain.synthesize` resets the count).
pub(crate) const MAX_FAILURES: u32 = 3;
/// `0`/`off`/`false` turns the automatic run off.
pub(crate) const AUTO_ENV: &str = "PRIME_AGENT_BRAIN_AUTO_SYNTHESIZE";

/// The marker line the model writes when nothing durable remains.
const EMPTY_MARKER: &str = "(no durable memories)";

/// The consolidator's system prompt.
pub(crate) const SYNTHESIS_SYSTEM_PROMPT: &str = "You are the memory consolidator of a coding agent. You receive the agent's stored memories for one scope (each with its source, date and key) and the previous summary of that scope, if any. Write the summary the agent will read at the start of every future session in this scope.

Rules:
- Output only bullet lines that start with \"- \": at most 20 lines, each at most 140 characters. No headings, no preamble, no closing remarks.
- Keep what stays useful in future work: the user's preferences and corrections, project conventions, build and test commands, architecture facts, known pitfalls, open threads.
- Merge memories that say the same thing. When memories contradict each other, keep the one with the newest date and drop the older claim.
- Never invent facts or speculate: every line must be supported by the memories given. Prefer the previous summary's wording when it is still true.
- Write dates as absolute dates (2026-11-15), never relative ones.
- Never include secrets, tokens, passwords, keys or credentials, even when a memory contains one.
- Drop one-off task state (process ids, temporary paths, finished work) unless it is still an open thread.
- If nothing durable remains, output exactly one line: - (no durable memories)";

/// Which summary a synthesis writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SynthScope {
    /// Global harness memories and personal files, across projects.
    Global,
    /// One project's memories plus the global ones.
    Project(Project),
}

impl SynthScope {
    /// The key the summary is stored under.
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Global => GLOBAL_SCOPE.to_string(),
            Self::Project(project) => project.summary_key(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Global => "all projects".to_string(),
            Self::Project(project) => format!("the project {}", project.summary_key()),
        }
    }

    fn covers(&self, doc: &BrainDoc) -> bool {
        match self {
            Self::Global => matches!(doc.source, Source::Global | Source::Personal),
            Self::Project(project) => {
                matches!(doc.source, Source::Global | Source::Personal)
                    || project.contains(doc.project.as_deref(), doc.slug.as_deref())
            }
        }
    }
}

/// The memories a synthesis of `scope` reads: covered, not excluded, not
/// superseded; pinned first, then newest first.
pub(crate) fn select<'a>(
    docs: impl Iterator<Item = &'a BrainDoc>,
    scope: &SynthScope,
    controls: &MemoryControls,
    superseded: &std::collections::BTreeMap<String, Superseded>,
) -> Vec<&'a BrainDoc> {
    let mut selected: Vec<&BrainDoc> = docs
        .filter(|doc| {
            scope.covers(doc)
                && !controls.excluded.contains(&doc.key)
                && !superseded.contains_key(&doc.key)
        })
        .collect();
    selected.sort_by(|a, b| {
        controls
            .pinned
            .contains(&b.key)
            .cmp(&controls.pinned.contains(&a.key))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.key.cmp(&b.key))
    });
    selected
}

/// The user message of one synthesis, and how many memories it carries.
pub(crate) fn prompt(
    scope: &SynthScope,
    previous: &[String],
    docs: &[&BrainDoc],
) -> (String, usize) {
    let mut out = format!("Scope: {}\n\n<previous_summary>\n", scope.label());
    for line in previous {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("</previous_summary>\n\n<memories>\n");
    let mut used = 0;
    let mut budget = INPUT_BUDGET_CHARS;
    for doc in docs {
        let text: String = format!("{}: {}", doc.title, doc.content)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MEMORY_CHARS)
            .collect();
        let line = format!(
            "[{} {} {}] {text}\n",
            doc.source.as_str(),
            doc.updated_at.get(..10).unwrap_or(&doc.updated_at),
            doc.key
        );
        if line.len() > budget {
            break;
        }
        budget -= line.len();
        out.push_str(&line);
        used += 1;
    }
    out.push_str("</memories>\n");
    (out, used)
}

fn credential_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"(?ix)
              sk-(?:ant-)?[a-z0-9_-]{16,}
            | gh[pousr]_[a-z0-9]{20,}
            | \bAKIA[0-9A-Z]{16}\b
            | -----BEGIN [A-Z ]*PRIVATE\ KEY-----
            | \b(?:password|passwd|secret|api[_-]?key|token)\s*[:=]\s*\S{6,}
            | \bbearer\s+[a-z0-9._-]{20,}
            | \b[a-z0-9+/]{48,}={0,2}(?:\s|$)",
        )
        .expect("the credential pattern is a valid regex")
    })
}

/// The summary lines of a model reply: its bullets, each made one line and
/// bounded; credential-shaped lines dropped; at most [`MAX_LINES`].
pub(crate) fn parse(reply: &str) -> Vec<String> {
    reply
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))
                .map(str::trim)
        })
        .filter(|line| !line.is_empty() && *line != EMPTY_MARKER)
        .filter(|line| !credential_pattern().is_match(line))
        .map(|line| {
            if line.chars().count() <= MAX_LINE_CHARS {
                line.to_string()
            } else {
                let cut: String = line.chars().take(MAX_LINE_CHARS - 3).collect();
                format!("{}...", cut.trim_end())
            }
        })
        .take(MAX_LINES)
        .collect()
}

/// Whether the automatic run is switched off.
pub(crate) fn auto_disabled() -> bool {
    std::env::var(AUTO_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    })
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// Milliseconds since the epoch of an ISO-8601 UTC timestamp
/// (`2026-11-15T03:04:05.678Z`); `None` when it does not parse.
pub(crate) fn iso_millis(iso: &str) -> Option<u64> {
    let bytes = iso.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| iso.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(secs).ok().map(|secs| secs * 1_000)
}

/// The number of sessions of `scope` whose transcript started after
/// `since_ms` (the transcript file's creation, else modification, time).
pub(crate) fn sessions_since(agent_dir: &Path, scope: &SynthScope, since_ms: u64) -> usize {
    let mut count = 0;
    for dir in [
        agent_dir.join("sessions"),
        agent_dir.join("sessions-archive"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let started = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.created().or_else(|_| meta.modified()).ok())
                .map_or(0, millis);
            if started <= since_ms {
                continue;
            }
            let in_scope = match scope {
                SynthScope::Global => true,
                SynthScope::Project(project) => path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|id| session_cwd(agent_dir, id))
                    .is_some_and(|cwd| {
                        let other = project_of(&cwd);
                        project.contains(other.identity.as_deref(), Some(&other.slug))
                    }),
            };
            if in_scope {
                count += 1;
            }
        }
    }
    count
}

/// Whether the automatic run should synthesize `scope` now.
pub(crate) fn due(agent_dir: &Path, scope: &SynthScope, summary: Option<&ScopeSummary>) -> bool {
    if summary.is_some_and(|summary| summary.failures >= MAX_FAILURES) {
        return false;
    }
    let now = millis(SystemTime::now());
    let min_age = u64::try_from(MIN_AGE.as_millis()).unwrap_or(u64::MAX);
    let last = summary
        .and_then(|summary| iso_millis(&summary.updated_at))
        .unwrap_or(0);
    let last_attempt = summary
        .and_then(|summary| summary.last_attempt_at.as_deref())
        .and_then(iso_millis)
        .unwrap_or(0);
    if now.saturating_sub(last) < min_age || now.saturating_sub(last_attempt) < min_age {
        return false;
    }
    sessions_since(agent_dir, scope, last) >= MIN_SESSIONS
}

/// An exclusive run marker under the harness dir, removed on drop; a marker
/// older than [`STALE_LOCK`] is taken over (a crashed run).
pub(crate) struct RunLock {
    path: PathBuf,
}

const STALE_LOCK: Duration = Duration::from_mins(15);

impl RunLock {
    /// Take the lock, or `None` while another run holds it.
    pub(crate) fn acquire(agent_dir: &Path) -> Option<Self> {
        let dir = agent_dir.join("harness");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("brain_synthesis.lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Some(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK);
                    if !stale {
                        return None;
                    }
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn doc(key: &str, source: Source, updated_at: &str, project: Option<&str>) -> BrainDoc {
        BrainDoc {
            key: key.to_string(),
            source,
            memory_id: None,
            session_id: None,
            project: project.map(str::to_string),
            slug: None,
            title: key.to_string(),
            content: format!("content of {key}"),
            path: String::new(),
            category: None,
            updated_at: updated_at.to_string(),
            declared_superseded_by: None,
        }
    }

    #[test]
    fn replies_parse_into_bounded_safe_lines() {
        let long = "word ".repeat(60);
        let reply = format!(
            "Here is the summary:\n- builds use mold\n* tests need a sandbox HOME\n- {long}\n- token = abcdefghijklmnop\n- api key sk-ant-REDACTEDREDACTEDREDACTED1234\nnot a bullet\n- (no durable memories)\n"
        );
        let lines = parse(&reply);
        assert_eq!(lines[0], "builds use mold");
        assert_eq!(lines[1], "tests need a sandbox HOME");
        assert_eq!(lines[2].chars().count(), MAX_LINE_CHARS);
        assert!(lines[2].ends_with("..."));
        assert_eq!(
            lines.len(),
            3,
            "credential-shaped lines and the empty marker are dropped"
        );
        let many = (0..40).fold(String::new(), |mut text, n| {
            use std::fmt::Write as _;
            let _ = writeln!(text, "- line {n}");
            text
        });
        assert_eq!(parse(&many).len(), MAX_LINES);
    }

    #[test]
    fn selection_covers_the_scope_pinned_then_newest() {
        let project = Project {
            identity: Some("git:abc".to_string()),
            slug: "-repo".to_string(),
        };
        let docs = [
            doc("global:old", Source::Global, "2026-01-01", None),
            doc("global:new", Source::Global, "2026-11-01", None),
            doc("mc:1", Source::MagicContext, "2026-11-02", Some("git:abc")),
            doc(
                "mc:2",
                Source::MagicContext,
                "2026-11-03",
                Some("git:other"),
            ),
            doc("global:gone", Source::Global, "2026-11-04", None),
        ];
        let mut controls = MemoryControls::default();
        controls.pinned.insert("global:old".to_string());
        let superseded = BTreeMap::from([(
            "global:gone".to_string(),
            Superseded {
                by: "global:new".to_string(),
                reason: super::super::supersede::Reason::Recorded,
            },
        )]);
        let keys: Vec<_> = select(
            docs.iter(),
            &SynthScope::Project(project),
            &controls,
            &superseded,
        )
        .iter()
        .map(|doc| doc.key.as_str())
        .collect();
        assert_eq!(keys, ["global:old", "mc:1", "global:new"]);
        let global: Vec<_> = select(docs.iter(), &SynthScope::Global, &controls, &superseded)
            .iter()
            .map(|doc| doc.key.as_str())
            .collect();
        assert_eq!(global, ["global:old", "global:new"]);
    }

    #[test]
    fn the_prompt_carries_previous_lines_and_respects_the_budget() {
        let docs: Vec<BrainDoc> = (0..2_000)
            .map(|n| {
                let mut d = doc(
                    &format!("global:m{n}"),
                    Source::Global,
                    "2026-11-01T00:00:00Z",
                    None,
                );
                d.content = "z ".repeat(400);
                d
            })
            .collect();
        let refs: Vec<&BrainDoc> = docs.iter().collect();
        let (text, used) = prompt(&SynthScope::Global, &["kept line".to_string()], &refs);
        assert!(text.contains("- kept line\n"));
        assert!(text.contains("[global 2026-11-01 global:m0]"));
        assert!(used > 0 && used < docs.len());
        assert!(text.len() < INPUT_BUDGET_CHARS + 1_000);
    }

    #[test]
    fn iso_timestamps_convert_to_epoch_millis() {
        assert_eq!(iso_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso_millis("2026-11-15T00:00:00.000Z"),
            Some(1_794_700_800_000)
        );
        assert_eq!(iso_millis("garbage"), None);
    }

    #[test]
    fn the_gate_needs_age_sessions_and_few_failures() {
        let agent = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(agent.path().join("sessions")).unwrap();
        for n in 0..MIN_SESSIONS {
            std::fs::write(
                agent.path().join(format!("sessions/s{n}.jsonl")),
                "{\"type\":\"session\",\"id\":\"x\",\"cwd\":\"/nowhere\"}\n",
            )
            .unwrap();
        }
        assert!(
            due(agent.path(), &SynthScope::Global, None),
            "never synthesized, enough sessions"
        );
        let fresh = ScopeSummary {
            updated_at: pa_ledger::now_iso(),
            ..ScopeSummary::default()
        };
        assert!(
            !due(agent.path(), &SynthScope::Global, Some(&fresh)),
            "synthesized just now"
        );
        let failing = ScopeSummary {
            failures: MAX_FAILURES,
            ..ScopeSummary::default()
        };
        assert!(!due(agent.path(), &SynthScope::Global, Some(&failing)));
        std::fs::remove_file(agent.path().join("sessions/s0.jsonl")).unwrap();
        assert!(
            !due(agent.path(), &SynthScope::Global, None),
            "too few sessions"
        );
    }

    #[test]
    fn the_run_lock_is_exclusive_and_released_on_drop() {
        let agent = tempfile::tempdir().unwrap();
        let first = RunLock::acquire(agent.path()).unwrap();
        assert!(RunLock::acquire(agent.path()).is_none());
        drop(first);
        assert!(RunLock::acquire(agent.path()).is_some());
    }
}
