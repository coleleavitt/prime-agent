//! The guard parity harness: replays `tests/corpus/guards.jsonl` (every input
//! the Python guard suites fed the guards, judged by all six Python guards in a
//! neutral context, captured from the Python guards before the port deleted
//! them) against the Rust guards in the same neutral context, and requires the
//! same verdict, error class and message for every input and every guard.
//!
//! `PA_BASH_CORPUS_LIMIT` bounds how many mismatches are printed per guard
//! (default 20).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::context::GuardContext;
use crate::script::Script;
use crate::verdict::GuardKind;

/// The guard's key in the corpus files.
fn corpus_key(guard: GuardKind) -> &'static str {
    match guard {
        GuardKind::DestructiveGit => "destructive_git",
        GuardKind::DestructiveChmod => "destructive_chmod",
        GuardKind::ForcePush => "force_push",
        GuardKind::SecretEcho => "secret_echo",
        GuardKind::PipeToShell => "pipe_to_shell",
        GuardKind::Sudo => "sudo",
    }
}

struct Record {
    script: String,
    command: Option<String>,
    prefix: Option<String>,
    refused: BTreeMap<String, (String, usize)>,
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn load() -> (Vec<Record>, Vec<String>) {
    let dir = corpus_dir();
    let messages: Vec<String> = serde_json::from_str(
        &std::fs::read_to_string(dir.join("messages.json")).expect("messages.json"),
    )
    .expect("messages.json parses");
    let records = std::fs::read_to_string(dir.join("guards.jsonl"))
        .expect("guards.jsonl")
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("corpus line parses");
            let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
            let refused = value["refused"]
                .as_object()
                .expect("refused map")
                .iter()
                .map(|(guard, verdict)| {
                    let error = verdict[0].as_str().expect("error name").to_string();
                    let message = usize::try_from(verdict[1].as_u64().expect("message id"))
                        .expect("message id fits");
                    (guard.clone(), (error, message))
                })
                .collect();
            Record {
                script: text("script").expect("script"),
                command: text("command"),
                prefix: text("prefix"),
                refused,
            }
        })
        .collect();
    (records, messages)
}

/// The neutral context the corpus was judged in: an empty, non-git working
/// directory, an empty HOME, `PATH=/usr/bin:/bin`, no CDPATH.
struct Neutral {
    _root: tempfile::TempDir,
    root: String,
    context: GuardContext,
}

fn neutral() -> Neutral {
    let root = tempfile::Builder::new()
        .prefix("pa-bash-corpus-")
        .tempdir()
        .expect("temp root");
    let real = root.path().canonicalize().expect("canonical root");
    let work = real.join("work");
    let home = real.join("home");
    std::fs::create_dir(&work).expect("work dir");
    std::fs::create_dir(&home).expect("home dir");
    let env = BTreeMap::from([
        ("HOME".to_string(), home.display().to_string()),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
    ]);
    Neutral {
        _root: root,
        root: real.display().to_string(),
        context: GuardContext::new(work, env),
    }
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return format!("{text:?}");
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head:?}... ({} chars)", text.chars().count())
}

#[test]
fn every_guard_matches_the_python_verdicts() {
    let (records, messages) = load();
    let neutral = neutral();
    let limit: usize = std::env::var("PA_BASH_CORPUS_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let mut report = String::new();
    let mut total = 0usize;
    for guard in GuardKind::ALL {
        let key = corpus_key(guard);
        let mut mismatches = 0usize;
        for record in &records {
            let script = Script {
                command: record.command.as_deref().unwrap_or(&record.script),
                script: &record.script,
                prefix: record.prefix.as_deref(),
            };
            // A Python verdict that is not a refusal class is the Python guard
            // crashing (`OverflowError` from `chr()` on `$'\UFFFFFFFF'` in the
            // chmod and secret-echo decoders). The port fixes the crash: the
            // escape stays literal and the command is judged like any other.
            let expected = record
                .refused
                .get(key)
                .filter(|(error, _)| error.ends_with("RefusalError"))
                .map(|(error, id)| {
                    (
                        error.clone(),
                        messages[*id].replace("<ROOT>", &neutral.root),
                    )
                });
            let actual = super::check(guard, &script, &neutral.context)
                .err()
                .map(|refusal| (refusal.guard.error_name().to_string(), refusal.message));
            if expected != actual {
                mismatches += 1;
                if mismatches <= limit {
                    use std::fmt::Write as _;
                    let _ = writeln!(
                        report,
                        "[{key}] {}\n    expected: {}\n    actual:   {}",
                        shorten(&record.script, 160),
                        expected.map_or_else(
                            || "allowed".to_string(),
                            |(e, m)| format!("{e}: {}", shorten(&m, 240))
                        ),
                        actual.map_or_else(
                            || "allowed".to_string(),
                            |(e, m)| format!("{e}: {}", shorten(&m, 240))
                        ),
                    );
                }
            }
        }
        if mismatches > 0 {
            use std::fmt::Write as _;
            let _ = writeln!(
                report,
                "[{key}] {mismatches} of {} inputs differ",
                records.len()
            );
        }
        total += mismatches;
    }
    assert!(total == 0, "guard corpus mismatches:\n{report}");
}

#[test]
fn every_guard_judges_every_corpus_input_without_panicking() {
    let (records, _) = load();
    let neutral = neutral();
    for record in &records {
        let script = Script {
            command: record.command.as_deref().unwrap_or(&record.script),
            script: &record.script,
            prefix: record.prefix.as_deref(),
        };
        for guard in GuardKind::ALL {
            let _ = super::check(guard, &script, &neutral.context);
        }
    }
}

/// A named input family, its builder, and the small size measured.
type Shape = (&'static str, fn(usize) -> String, usize);

/// The fastest of three runs of `guard` on `text`.
fn best_time(guard: GuardKind, text: &str, context: &GuardContext) -> f64 {
    let script = Script::bare(text);
    (0..3)
        .map(|_| {
            let start = std::time::Instant::now();
            let _ = super::check(guard, &script, context);
            start.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

/// Here-document shapes the Python guards scanned in linear time (their
/// cost-lock tests): a line of thousands of openers, and thousands of
/// openers whose delimiter never comes. Eight times the input must cost
/// about eight times the time, not sixty-four (the ratio bound is loose so
/// scheduling noise cannot trip it; a quadratic scan measured 45-70).
#[test]
fn heredoc_shapes_scan_in_linear_time() {
    let neutral = neutral();
    let shapes: [Shape; 2] = [
        ("openers", |n| format!("cat {}body", "<<A ".repeat(n)), 1000),
        ("unterminated", |n| "cat <<'EOF'\nenv\n".repeat(n), 500),
    ];
    let mut slow = Vec::new();
    for (name, make, n) in shapes {
        for guard in GuardKind::ALL {
            let small = best_time(guard, &make(n), &neutral.context);
            let large = best_time(guard, &make(n * 8), &neutral.context);
            let ratio = large / small.max(1e-4);
            if ratio > 24.0 {
                slow.push(format!("{name} {}: {ratio:.1}", corpus_key(guard)));
            }
        }
    }
    assert_eq!(slow, Vec::<String>::new());
}
