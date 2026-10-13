//! `learning backfill` writes the same day files the TS-era
//! `backfill.py` wrote for the same corpora (`fixtures/backfill`).

use std::path::{Path, PathBuf};

use pa_learning::backfill::{BackfillOptions, Corpus, parse_backfill_args, run_backfill};

const SEALED_AT: &str = "2026-10-13T00:00:00.000Z";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/backfill")
}

/// Every file under `root`, by relative path, with its bytes.
fn tree(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((relative, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn backfill_matches_the_ts_era_script_byte_for_byte() {
    let out = tempfile::tempdir().unwrap();
    let home = fixture().join("home");
    let args: Vec<String> = [
        "--home",
        home.to_str().unwrap(),
        "--out",
        out.path().to_str().unwrap(),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let options = parse_backfill_args(&args, Path::new("/unused-agent-dir")).unwrap();
    let mut progress = Vec::new();
    let written = run_backfill(&options, SEALED_AT, &mut |line| {
        progress.push(line.to_string())
    })
    .unwrap();
    assert_eq!(
        progress,
        [
            "walking prime...",
            "walking claude...",
            "walking opencode (this can be large; be patient)...",
        ]
    );
    let expected = tree(&fixture().join("expected"));
    assert_eq!(written, expected.len());
    assert_eq!(tree(out.path()), expected);
}

#[cfg(unix)]
#[test]
fn day_files_and_dirs_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let out = tempfile::tempdir().unwrap();
    let options = BackfillOptions {
        out_dir: out.path().join("backfill"),
        prime_dir: fixture().join("home/.prime/agent/sessions"),
        claude_dir: PathBuf::from("/nonexistent"),
        opencode_store: PathBuf::from("/nonexistent"),
        corpora: vec![Corpus::Prime],
    };
    run_backfill(&options, SEALED_AT, &mut |_| {}).unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&options.out_dir), 0o700);
    assert_eq!(mode(&options.out_dir.join("prime")), 0o700);
    assert_eq!(mode(&options.out_dir.join("prime/2026-10-01.json")), 0o600);
}

/// The lines the TS-era script crashed on (a JSONL line that is not an
/// object, a non-object `message`) are skipped, and the rest still counts.
#[test]
fn records_that_are_not_objects_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("s.jsonl"),
        concat!(
            "[1, 2, 3]\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-10-05T01:00:00Z\",\"message\":\"text\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-10-05T02:00:00Z\",\"message\":{\"role\":\"user\"}}\n",
        ),
    )
    .unwrap();
    let options = BackfillOptions {
        out_dir: root.path().join("out"),
        prime_dir: sessions,
        claude_dir: PathBuf::from("/nonexistent"),
        opencode_store: PathBuf::from("/nonexistent"),
        corpora: vec![Corpus::Prime],
    };
    assert_eq!(run_backfill(&options, SEALED_AT, &mut |_| {}).unwrap(), 1);
    let day: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(options.out_dir.join("prime/2026-10-05.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(day["turns"], 1);
}

#[test]
fn unknown_flags_and_corpora_are_refused() {
    let parse = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse_backfill_args(&args, Path::new("/agent"))
    };
    assert_eq!(
        parse(&["--corpus", "vscode"]).unwrap_err(),
        "unknown corpus \"vscode\" (choose prime, claude or opencode)"
    );
    assert_eq!(parse(&["--bogus"]).unwrap_err(), "unknown option --bogus");
    assert_eq!(parse(&["--out"]).unwrap_err(), "--out requires a value");
    let defaults = parse(&["--home", "/h", "--corpus", "claude", "--corpus", "claude"]).unwrap();
    assert_eq!(
        defaults,
        BackfillOptions {
            out_dir: PathBuf::from("/h/.prime/agent/learning/backfill"),
            prime_dir: PathBuf::from("/h/.prime/agent/sessions"),
            claude_dir: PathBuf::from("/h/.claude/projects"),
            opencode_store: PathBuf::from("/h/.local/share/opencode/storage"),
            corpora: vec![Corpus::Claude],
        }
    );
}
