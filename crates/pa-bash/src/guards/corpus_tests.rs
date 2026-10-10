//! The guard parity harness: replays `tests/corpus/guards.jsonl` (every input
//! the Python guard suites fed the guards, judged by all six Python guards in
//! a neutral context before the port deleted them) against the rules in the
//! same neutral context.
//!
//! The rules were redesigned to refuse on evidence, so a verdict may differ
//! from the Python one only where `tests/corpus/deltas.jsonl` records the
//! difference and its justification ([`Category`]). Messages were rewritten
//! to name the evidence; the harness compares verdicts and error classes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::context::GuardContext;
use crate::script::Script;
use crate::verdict::GuardKind;

/// Why a verdict differs from the Python oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    /// The Python guard refused code it could not read (a script file, a
    /// shell reading a pipe, a login shell, `eval "$x"`) whose visible text
    /// does not show this guard's danger.
    NoEvidence,
    /// The model resolves the construct the Python guard gave up on (a `cd`,
    /// an `eval` or `sh -c` payload, a variable, an alias, a function), and
    /// what runs is not the danger here: the neutral context has no
    /// repository, and its paths stay in the workspace.
    Resolved,
    /// A variable the kernel environment does not set expands to nothing.
    UnsetVariable,
    /// Bash does not run the text as a command: a syntax error, an argument
    /// of another program, a positional parameter, an option the program
    /// rejects.
    NotRun,
    /// A deliberate narrowing of the policy: output that never reaches the
    /// transcript, a non-secret file, a dry run, a setting that cannot arm a
    /// force push, a `PATH` shadow, xargs handing a download to `sh` as file
    /// names.
    Narrowed,
    /// Refused now, allowed by the Python guard.
    New,
}

impl Category {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "no-evidence" => Category::NoEvidence,
            "resolved" => Category::Resolved,
            "unset-variable" => Category::UnsetVariable,
            "not-run" => Category::NotRun,
            "narrowed" => Category::Narrowed,
            "new" => Category::New,
            _ => return None,
        })
    }
}

struct Record {
    script: String,
    command: Option<String>,
    prefix: Option<String>,
    refused: BTreeMap<String, String>,
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn load() -> Vec<Record> {
    std::fs::read_to_string(corpus_dir().join("guards.jsonl"))
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
                    (
                        guard.clone(),
                        verdict[0].as_str().expect("error name").to_string(),
                    )
                })
                // A Python verdict that is not a refusal class is the Python
                // guard crashing (`OverflowError` from `chr()` on
                // `$'\UFFFFFFFF'`): the escape stays literal here.
                .filter(|(_, error)| error.ends_with("RefusalError"))
                .collect();
            Record {
                script: text("script").expect("script"),
                command: text("command"),
                prefix: text("prefix"),
                refused,
            }
        })
        .collect()
}

/// `(record index, guard key)` → (refused now, category).
fn deltas() -> BTreeMap<(usize, String), (bool, Category)> {
    std::fs::read_to_string(corpus_dir().join("deltas.jsonl"))
        .expect("deltas.jsonl")
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("delta parses");
            let index = usize::try_from(value["i"].as_u64().expect("index")).expect("index fits");
            let guard = value["guard"].as_str().expect("guard").to_string();
            let refused = value["refused"].as_bool().expect("refused");
            let category = value["category"]
                .as_str()
                .and_then(Category::parse)
                .unwrap_or_else(|| panic!("delta {index}/{guard} has no known category"));
            ((index, guard), (refused, category))
        })
        .collect()
}

/// The neutral context the corpus was judged in: an empty, non-git working
/// directory, an empty HOME, `PATH=/usr/bin:/bin`, no CDPATH.
fn neutral() -> (tempfile::TempDir, GuardContext) {
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
    (root, GuardContext::new(work, env))
}

/// The guards the Python oracle judged.
const ORACLE_GUARDS: [GuardKind; 6] = [
    GuardKind::DestructiveGit,
    GuardKind::DestructiveChmod,
    GuardKind::ForcePush,
    GuardKind::SecretEcho,
    GuardKind::PipeToShell,
    GuardKind::Sudo,
];

#[test]
fn every_verdict_matches_the_oracle_or_a_recorded_delta() {
    let records = load();
    let deltas = deltas();
    let (_root, context) = neutral();
    let mut report = Vec::new();
    let mut used = BTreeSet::new();
    for (index, record) in records.iter().enumerate() {
        let script = Script {
            command: record.command.as_deref().unwrap_or(&record.script),
            script: &record.script,
            prefix: record.prefix.as_deref(),
        };
        for guard in ORACLE_GUARDS {
            let key = (index, guard.key().to_string());
            let actual = super::check(guard, &script, &context)
                .err()
                .map(|refusal| refusal.guard.error_name().to_string());
            let oracle = record.refused.get(guard.key()).cloned();
            let expected = match deltas.get(&key) {
                Some((refused, _)) => {
                    used.insert(key.clone());
                    refused.then(|| guard.error_name().to_string())
                }
                None => oracle.clone(),
            };
            if actual != expected {
                report.push(format!(
                    "#{index} [{}] {:?}\n    oracle: {oracle:?}, expected: {expected:?}, actual: {actual:?}",
                    guard.key(),
                    record.script.chars().take(160).collect::<String>()
                ));
            }
        }
    }
    let stale: Vec<_> = deltas.keys().filter(|key| !used.contains(*key)).collect();
    assert!(
        report.is_empty(),
        "{} verdicts differ:\n{}",
        report.len(),
        report.join("\n")
    );
    assert!(stale.is_empty(), "deltas for unknown records: {stale:?}");
}

/// A delta records a real difference: the oracle's verdict is the other
/// one, and a loss of a refusal is never `New`.
#[test]
fn every_delta_flips_the_oracle_verdict() {
    let records = load();
    for ((index, guard), (refused, category)) in deltas() {
        let oracle = records[index].refused.contains_key(&guard);
        assert_ne!(oracle, refused, "#{index} [{guard}] repeats the oracle");
        assert_eq!(
            category == Category::New,
            refused,
            "#{index} [{guard}] {category:?}"
        );
    }
}

#[test]
fn every_guard_judges_every_corpus_input_without_panicking() {
    let (_root, context) = neutral();
    for record in load() {
        let script = Script {
            command: record.command.as_deref().unwrap_or(&record.script),
            script: &record.script,
            prefix: record.prefix.as_deref(),
        };
        for guard in GuardKind::ALL {
            let _ = super::check(guard, &script, &context);
        }
    }
}
