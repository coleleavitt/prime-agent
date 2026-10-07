//! The guard parity harness: replays `tests/corpus/guards.jsonl` (every input
//! the Python guard suites fed the guards, judged by all six Python guards in a
//! neutral context; see `tests/corpus/capture.py`) against the Rust guards in
//! the same neutral context, and requires the same verdict, error class and
//! message for every input and every ported guard.
//!
//! `PA_BASH_CORPUS_GUARDS=sudo,force_push` checks the named guards whether or
//! not they are marked ported (for work in progress); `PA_BASH_CORPUS_LIMIT`
//! bounds how many mismatches are printed per guard (default 20).

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

fn selected() -> Vec<GuardKind> {
    match std::env::var("PA_BASH_CORPUS_GUARDS") {
        Ok(names) if !names.trim().is_empty() => GuardKind::ALL
            .into_iter()
            .filter(|guard| {
                names
                    .split(',')
                    .any(|name| name.trim() == corpus_key(*guard))
            })
            .collect(),
        Ok(_) | Err(_) => GuardKind::ALL
            .into_iter()
            .filter(|guard| super::ported(*guard))
            .collect(),
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
fn every_ported_guard_matches_the_python_verdicts() {
    let (records, messages) = load();
    let neutral = neutral();
    let limit: usize = std::env::var("PA_BASH_CORPUS_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let mut report = String::new();
    let mut total = 0usize;
    for guard in selected() {
        let key = corpus_key(guard);
        let mut mismatches = 0usize;
        for record in &records {
            let script = Script {
                command: record.command.as_deref().unwrap_or(&record.script),
                script: &record.script,
                prefix: record.prefix.as_deref(),
            };
            let expected = record.refused.get(key).map(|(error, id)| {
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
