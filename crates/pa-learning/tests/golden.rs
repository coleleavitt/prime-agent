//! Replays the TS goldens (`tests/fixtures/golden/*.json`, made by
//! `generate.ts` from the TS sources under node): the roll-up, the sealed day
//! files byte for byte, the cohort reports, the statistics and their
//! formatting, the chart, the trajectory windows/labels/rate and its prompt
//! helpers, and the `learning` / `learning trajectory` command output.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use pa_core::refinement::HarnessState;
use pa_learning::command::{LearningCommandIo, run_learning_command};
use pa_learning::{
    ChartOptions,
    ChartSeries,
    CorpusDay,
    LearningDay,
    LearningReport,
    SealTrajectoryOptions,
    build_learning_report,
    mann_whitney_one_sided,
    matches_security_class,
    normal_cdf,
    normalize_day,
    read_learning_index,
    render_ascii_chart,
    roll_up_learning_days,
    seal_learning_days,
    seal_trajectory_windows,
    span_fingerprint_key,
    trajectory_class_for_entries,
    trajectory_internalized_fingerprints,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Whole-value equality where a number may differ from the fixture by the
/// last bit `serde_json`'s default (not correctly rounded) float parser can
/// lose; exact output bytes are compared separately (the `--json` runs and
/// the sealed files).
#[track_caller]
fn assert_json_eq(actual: &Value, expected: &Value, context: &str) {
    fn same(actual: &Value, expected: &Value) -> bool {
        match (actual, expected) {
            (Value::Number(left), Value::Number(right)) => {
                left == right
                    || match (left.as_f64(), right.as_f64()) {
                        (Some(left), Some(right)) => left.to_bits().abs_diff(right.to_bits()) <= 1,
                        _ => false,
                    }
            }
            (Value::Array(left), Value::Array(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| same(left, right))
            }
            (Value::Object(left), Value::Object(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|((left_key, left), (right_key, right))| {
                            left_key == right_key && same(left, right)
                        })
            }
            _ => actual == expected,
        }
    }
    assert!(
        same(actual, expected),
        "{context}\n  left: {actual}\n right: {expected}"
    );
}

fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Captured output, with a run's temp dir replaced by `<dir>`.
struct Captured {
    dir: String,
    now: u64,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl LearningCommandIo for Captured {
    fn stdout(&mut self, line: &str) {
        self.stdout.push(line.replace(&self.dir, "<dir>"));
    }
    fn stderr(&mut self, line: &str) {
        self.stderr.push(line.replace(&self.dir, "<dir>"));
    }
    fn now(&self) -> u64 {
        self.now
    }
}

fn run(args: &[String], agent_dir: &Path, dir: &Path, now: u64) -> Value {
    let mut io = Captured {
        dir: dir.to_string_lossy().into_owned(),
        now,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let outcome = run_learning_command(args, agent_dir, &mut io);
    json!({ "code": outcome.exit_code, "stdout": io.stdout, "stderr": io.stderr })
}

// --- the TS test corpus ----------------------------------------------------

const TOTAL_TURNS: u64 = 8000;
const TURNS_PER_DAY: u64 = 500;
const COMMIT_TURN: u64 = 4000;
const BASE_DAY_MS: u64 = 1_785_542_400_000; // 2026-08-01

fn ids() -> Vec<String> {
    (0..20).map(|index| format!("fp{index:02}")).collect()
}

fn iso_at(turn: u64) -> String {
    pa_ledger::iso_from_millis(
        BASE_DAY_MS + (turn / TURNS_PER_DAY) * 86_400_000 + (turn % TURNS_PER_DAY) * 150_000,
    )
}

fn hex(seed: u64, length: usize) -> String {
    let text = format!("{seed:0length$x}");
    text[text.len() - length..].to_string()
}

/// `seedLog` of `generate.ts`, line for line (`JSON.stringify` key order).
fn seed_log(addressed: &[String], improved: &[String]) -> String {
    let ids = ids();
    let mut lines: Vec<String> = Vec::new();
    for turn in 0..TOTAL_TURNS {
        let ts = iso_at(turn);
        let session = turn % 8;
        let trace_id = format!("{}{}", hex(session, 8), hex(turn, 24));
        #[allow(clippy::cast_precision_loss)]
        let duration = 800.0 + (turn % 400) as f64 + (turn % 7) as f64 / 8.0;
        lines.push(
            json!({
                "level": "info", "component": "trace", "msg": "span_end", "ts": ts,
                "name": "agent.turn", "traceId": trace_id, "spanId": hex(turn, 16),
                "durationMs": serde_json::to_value(pa_types::JsNumber(duration)).unwrap(),
                "status": "ok",
                "attrs": { "session.id": format!("s{session}"), "turn.index": turn / 8 }
            })
            .to_string(),
        );
        if turn == COMMIT_TURN {
            lines.push(
                json!({
                    "ts": ts, "level": "info", "component": "coding-agent.refinement",
                    "msg": "refinement.committed", "traceId": trace_id,
                    "proposalId": "champion-1", "addressed": addressed,
                    "deepScore": 88, "missed": 0
                })
                .to_string(),
            );
        }
        for (index, id) in ids.iter().enumerate() {
            let period = if turn >= COMMIT_TURN && improved.contains(id) {
                1000
            } else {
                125
            };
            if turn % period != (index as u64 * 6) % 125 {
                continue;
            }
            lines.push(
                json!({
                    "level": "warn", "component": "trace", "msg": "span_end", "ts": ts,
                    "name": "tool.execute", "traceId": trace_id,
                    "spanId": hex(turn * 32 + index as u64, 16), "parentSpanId": hex(turn, 16),
                    "durationMs": 40 + index, "status": "error",
                    "error": format!("AttributeError: object has no attribute '{id}'"),
                    "attrs": { "failure.fingerprint": id, "tool.name": "ipython" }
                })
                .to_string(),
            );
        }
    }
    format!("{}\n", lines.join("\n"))
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn write_log(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name).join("agent.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn days_json(days: &[LearningDay], dir: &Path) -> Value {
    let dir = dir.to_string_lossy();
    Value::Array(
        days.iter()
            .map(|day| {
                let mut value = day.to_json();
                value["sourceFiles"] = Value::from(
                    day.source_files
                        .iter()
                        .map(|file| file.replace(dir.as_ref(), "<dir>"))
                        .collect::<Vec<_>>(),
                );
                value
            })
            .collect(),
    )
}

fn report_of(text: &str, dir: &Path, name: &str, min_cohort_n: u64, now: u64) -> LearningReport {
    let days = roll_up_learning_days(&[write_log(dir, name, text)], now)
        .unwrap()
        .days;
    build_learning_report(&days, min_cohort_n, now)
}

// One golden file, replayed scenario by scenario.
#[allow(clippy::too_many_lines)]
#[test]
fn the_corpus_rolls_up_seals_and_reports_like_the_ts_product() {
    let expected = golden("corpus.json");
    let now = expected["afterCorpusMs"].as_u64().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("corpus");
    let ids = ids();
    let (treated, untreated) = (ids[..10].to_vec(), ids[10..].to_vec());
    let treated_log = seed_log(&treated, &treated);
    let swapped_log = seed_log(&untreated, &treated);
    assert_eq!(
        json!({ "treated": sha256_hex(&treated_log), "swapped": sha256_hex(&swapped_log) }),
        expected["logSha256"]
    );
    let none_log = treated_log
        .split('\n')
        .filter(|raw| !raw.contains("refinement.committed"))
        .collect::<Vec<_>>()
        .join("\n");

    let treated_path = write_log(&dir, "treated", &treated_log);
    let rolled = roll_up_learning_days(std::slice::from_ref(&treated_path), now).unwrap();
    assert_json_eq(
        &days_json(&rolled.days, &dir),
        &expected["rolledDays"],
        "rolled days",
    );
    assert_eq!(json!(rolled.parse_errors), expected["parseErrors"]);

    let reports = json!({
        "treated": report_of(&treated_log, &dir, "r-treated", 5, now).to_json(Vec::new()),
        "swapped": report_of(&swapped_log, &dir, "r-swapped", 5, now).to_json(Vec::new()),
        "strict": report_of(&treated_log, &dir, "r-strict", 11, now).to_json(Vec::new()),
        "none": report_of(&none_log, &dir, "r-none", 5, now).to_json(Vec::new()),
    });
    assert_json_eq(&reports, &expected["reports"], "reports");

    let seal_dir = dir.join("seal-days");
    let inside_last_day = expected["insideLastDayMs"].as_u64().unwrap();
    let first = seal_learning_days(
        std::slice::from_ref(&treated_path),
        &seal_dir,
        inside_last_day,
        false,
    )
    .unwrap();
    let second =
        seal_learning_days(std::slice::from_ref(&treated_path), &seal_dir, now, false).unwrap();
    let file = |day: &str| {
        std::fs::read_to_string(seal_dir.join(format!("{day}.json")))
            .unwrap()
            .replace(dir.to_string_lossy().as_ref(), "<dir>")
    };
    let seal = json!({
        "first": { "written": first.written, "skipped": first.skipped, "open": first.open, "parseErrors": first.parse_errors },
        "second": { "written": second.written, "skipped": second.skipped, "open": second.open, "parseErrors": second.parse_errors },
        "firstDayFile": file("2026-08-01"),
        "commitDayFile": file("2026-08-09"),
    });
    assert_eq!(seal, expected["seal"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            (mode(&seal_dir), mode(&seal_dir.join("2026-08-01.json"))),
            (0o700, 0o600)
        );
    }
    // Read back, a sealed day is the day that was written.
    let reread = read_learning_index(&seal_dir);
    assert_eq!(
        reread
            .iter()
            .map(|day| day.day.as_str())
            .collect::<Vec<_>>(),
        rolled
            .days
            .iter()
            .map(|day| day.day.as_str())
            .collect::<Vec<_>>()
    );
    let first_file: Value = serde_json::from_str(&file("2026-08-01")).unwrap();
    assert_json_eq(
        &days_json(&reread[..1], &dir)[0],
        &first_file,
        "re-read day",
    );

    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let treated_arg = treated_path.to_string_lossy().into_owned();
    let runs: Vec<(&str, Vec<String>)> = vec![
        (
            "text",
            vec![
                "--log".into(),
                treated_arg.clone(),
                "--index".into(),
                path("cmd-text"),
            ],
        ),
        (
            "json",
            vec![
                "--log".into(),
                treated_arg.clone(),
                "--index".into(),
                path("cmd-json"),
                "--json".into(),
            ],
        ),
        (
            "withheld",
            vec![
                "--log".into(),
                treated_arg.clone(),
                "--index".into(),
                path("cmd-withheld"),
                "--min-n".into(),
                "11".into(),
                "--no-chart".into(),
            ],
        ),
        (
            "limited",
            vec![
                "--log".into(),
                treated_arg,
                "--index".into(),
                path("cmd-limited"),
                "--limit=3".into(),
                "--min-n=2".into(),
            ],
        ),
        ("unknown", vec!["--nope".into()]),
        ("operand", vec!["extra".into()]),
        ("missing-value", vec!["--log".into()]),
        ("bad-integer", vec!["--min-n".into(), "0".into()]),
        (
            "no-log",
            vec![
                "--log".into(),
                dir.join("absent")
                    .join("agent.jsonl")
                    .to_string_lossy()
                    .into_owned(),
                "--index".into(),
                path("cmd-absent"),
            ],
        ),
        (
            "no-days",
            vec!["--no-seal".into(), "--index".into(), path("cmd-empty")],
        ),
    ];
    let agent_dir = root.path().join("agent");
    for (name, args) in runs {
        assert_eq!(
            run(&args, &agent_dir, &dir, now),
            expected["commands"][name],
            "{name}"
        );
    }
}

// --- a handcrafted log across rotated generations --------------------------

#[test]
fn rotated_generations_roll_up_like_the_ts_product() {
    let expected = golden("misc-log.json");
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("agent.jsonl");
    let lines = |key: &str| -> String {
        expected["generations"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let gzip = |path: PathBuf, text: String| {
        let mut encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::default(),
        );
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap();
    };
    gzip(
        root.path().join("agent.jsonl.old.2.gz"),
        format!("{}\n", lines("gz2")),
    );
    gzip(
        root.path().join("agent.jsonl.old.1.gz"),
        format!("{}\n", lines("gz1")),
    );
    std::fs::write(
        root.path().join("agent.jsonl.old"),
        format!("{}\n", lines("old")),
    )
    .unwrap();
    std::fs::write(&log, lines("live")).unwrap();
    let files = pa_trace::retained_log_files(&log);
    let names: Vec<String> = files
        .iter()
        .map(|file| file.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(json!(names), expected["files"]);
    let rolled = roll_up_learning_days(&files, expected["nowMs"].as_u64().unwrap()).unwrap();
    let days: Vec<Value> = rolled
        .days
        .iter()
        .map(|day| {
            let mut value = day.to_json();
            value["sourceFiles"] = json!(
                day.source_files
                    .iter()
                    .map(|file| Path::new(file)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned())
                    .collect::<Vec<_>>()
            );
            value
        })
        .collect();
    assert_json_eq(&Value::Array(days), &expected["days"], "days");
    assert_eq!(json!(rolled.parse_errors), expected["parseErrors"]);
    for case in expected["keys"].as_array().unwrap() {
        let mut entry =
            json!({ "ts": "", "level": "info", "component": "trace", "msg": "span_end" });
        for (key, value) in case["entry"].as_object().unwrap() {
            entry[key] = value.clone();
        }
        let key = span_fingerprint_key(entry.as_object().unwrap());
        assert_eq!(
            json!({ "fingerprint": key.fingerprint, "failure": key.failure, "message": key.message }),
            case["key"]
        );
    }
}

// --- statistics, significance and the chart ---------------------------------

fn numbers(value: &Value) -> Vec<f64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|number| number.as_f64().unwrap())
        .collect()
}

#[test]
fn statistics_and_their_formatting_match_the_ts_product() {
    let expected = golden("stats.json");
    for case in expected["mannWhitney"].as_array().unwrap() {
        let result = mann_whitney_one_sided(&numbers(&case["lower"]), &numbers(&case["higher"]));
        let actual = result.map_or(Value::Null, |result| {
            let number = |value: f64| serde_json::to_value(pa_types::JsNumber(value)).unwrap();
            json!({ "u": number(result.u), "z": number(result.z), "pValue": number(result.p_value) })
        });
        assert_json_eq(&actual, &case["result"], &case.to_string());
    }
    for case in expected["normalCdf"].as_array().unwrap() {
        assert_json_eq(
            &serde_json::to_value(pa_types::JsNumber(normal_cdf(case["z"].as_f64().unwrap())))
                .unwrap(),
            &case["p"],
            &case.to_string(),
        );
    }
    for case in expected["significance"].as_array().unwrap() {
        let mut report = build_learning_report(&[], 5, 0);
        report.p_value = case["pValue"].as_f64();
        report.u = case["u"].as_f64();
        report.insufficient_evidence = case["insufficientEvidence"].as_str().map(str::to_string);
        assert_eq!(
            pa_learning::command::format_significance(&report),
            case["text"].as_str().unwrap()
        );
    }
    for case in expected["charts"].as_array().unwrap() {
        let series: Vec<ChartSeries> = case["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|series| ChartSeries {
                label: series["label"].as_str().unwrap().to_string(),
                mark: series["mark"].as_str().unwrap().chars().next().unwrap(),
                points: series["points"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(Value::as_f64)
                    .collect(),
            })
            .collect();
        let options = &case["options"];
        let count = |key: &str| {
            options[key]
                .as_u64()
                .map(|value| usize::try_from(value).unwrap())
        };
        let lines = render_ascii_chart(
            &series,
            &ChartOptions {
                height: count("height"),
                width: count("width"),
                x_labels: options["xLabels"]
                    .as_array()
                    .map(|labels| {
                        labels
                            .iter()
                            .map(|label| label.as_str().unwrap().to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                marker_index: count("markerIndex"),
                value_label: options["valueLabel"].as_str().map(str::to_string),
            },
        );
        assert_eq!(json!(lines), case["lines"], "{case}");
    }
}

// --- the Engineer Trajectory Index -------------------------------------------

fn day_of(value: &Value) -> LearningDay {
    normalize_day(value).unwrap()
}

#[test]
fn trajectory_windows_labels_and_prompt_helpers_match_the_ts_product() {
    let expected = golden("trajectory.json");
    let now = expected["nowMs"].as_u64().unwrap();
    for case in expected["isoWeeks"].as_array().unwrap() {
        assert_eq!(
            pa_learning::iso_week(case["day"].as_str().unwrap()).as_deref(),
            case["week"].as_str()
        );
    }
    for case in expected["security"].as_array().unwrap() {
        assert_eq!(
            json!(matches_security_class(
                case["name"].as_str().unwrap(),
                case["message"].as_str().unwrap()
            )),
            case["matches"],
            "{case}"
        );
    }
    for (name, case) in expected["sealed"].as_object().unwrap() {
        let input = &case["input"];
        let days: Vec<LearningDay> = input["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(day_of)
            .collect();
        let backfill: Vec<CorpusDay> = input["backfill"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| CorpusDay {
                corpus: entry["corpus"].as_str().unwrap().to_string(),
                day: day_of(&entry["day"]),
            })
            .collect();
        let file = seal_trajectory_windows(&SealTrajectoryOptions {
            days: &days,
            backfill_days: &backfill,
            min_windows: input["minWindows"].as_u64(),
            internalized_gap: input["gap"].as_u64(),
            now_ms: now,
        });
        assert_json_eq(&file.to_json(), &case["file"], name);
        assert_json_eq(
            &pa_learning::command::build_trajectory_report(&file).to_json(Vec::new()),
            &case["report"],
            name,
        );
        assert_eq!(
            json!(pa_learning::format_trajectory_lines(&file, 3)),
            case["lines"],
            "{name}"
        );
        let mut internalized: Vec<String> = trajectory_internalized_fingerprints(Some(&file))
            .into_iter()
            .collect();
        internalized.sort();
        assert_eq!(json!(internalized), case["internalized"], "{name}");
    }
    let labels_days: Vec<LearningDay> = expected["sealed"]["labels"]["input"]["days"]
        .as_array()
        .unwrap()
        .iter()
        .map(day_of)
        .collect();
    let labels_file = seal_trajectory_windows(&SealTrajectoryOptions {
        days: &labels_days,
        now_ms: now,
        ..SealTrajectoryOptions::default()
    });
    let mut state: HarnessState = pa_core::refinement::empty_harness_state();
    for key in ["trustWindows", "failures"] {
        state
            .extensions
            .insert(key.to_string(), expected["classState"][key].clone());
    }
    let class_of: serde_json::Map<String, Value> =
        trajectory_class_for_entries(&labels_file, &state)
            .into_iter()
            .map(|(id, class)| {
                let name = match class {
                    pa_learning::EntryClass::StableGap => "stable-gap",
                    pa_learning::EntryClass::New => "new",
                    pa_learning::EntryClass::Internalized => "internalized",
                };
                (id, Value::from(name))
            })
            .collect();
    let mut sorted: Vec<(String, Value)> = class_of.into_iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        Value::Object(sorted.into_iter().collect()),
        expected["classOf"]
    );
}

// The TS generator's run sequence, step by step: each run sees the last's files.
#[allow(clippy::too_many_lines)]
#[test]
fn the_trajectory_command_prints_and_stores_like_the_ts_product() {
    let expected = golden("trajectory-command.json");
    let now = golden("trajectory.json")["nowMs"].as_u64().unwrap();
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agent");
    let index_dir = agent_dir.join("learning").join("days");
    let args = |values: &[&str]| -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    };
    let fp = |fingerprint: &str, count: u64, name: &str| json!({ "fingerprint": fingerprint, "name": name, "status": "error", "failure": true, "count": count, "p50Ms": 0, "p95Ms": 0, "message": "" });
    let day = |day: &str, turns: u64, fingerprints: Vec<Value>| {
        day_of(
            &json!({ "schema": 1, "day": day, "sealedAt": "", "turns": turns, "fingerprints": fingerprints, "commits": [], "parseErrors": 0, "sourceFiles": [] }),
        )
    };
    let weeks = [
        "2025-01-06",
        "2025-01-13",
        "2025-01-20",
        "2025-01-27",
        "2025-02-03",
    ];
    let mut runs = serde_json::Map::new();
    let mut record = |name: &str, values: &[&str]| {
        runs.insert(
            name.to_string(),
            run(&args(values), &agent_dir, &agent_dir, now),
        );
    };
    record("no-days", &["trajectory", "--no-seal"]);
    record("bogus", &["trajectory", "--bogus"]);
    record("operand", &["trajectory", "x"]);
    for week in &weeks[..2] {
        pa_learning::write_learning_day(&index_dir, &day(week, 5, vec![fp("fpA", 2, "fpA")]))
            .unwrap();
    }
    record("withheld", &["trajectory", "--no-seal", "--json"]);
    for week in &weeks[2..5] {
        pa_learning::write_learning_day(
            &index_dir,
            &day(
                week,
                5,
                vec![
                    fp("fpStay", 2, "fpStay"),
                    fp("a1b2c3d4e5f60718", 1, "git push rejected"),
                ],
            ),
        )
        .unwrap();
    }
    record("table", &["trajectory", "--no-seal"]);
    let store_path = agent_dir.join("learning").join("trajectory.json");
    let store_after_table = std::fs::read_to_string(&store_path).unwrap();
    let gitignore = std::fs::read_to_string(agent_dir.join("learning").join(".gitignore")).unwrap();
    let opencode = agent_dir.join("learning").join("backfill").join("opencode");
    for week in &weeks[..4] {
        pa_learning::write_learning_day(
            &opencode,
            &day(week, 3, vec![fp("bf-token", 2, "exc:ValueError")]),
        )
        .unwrap();
    }
    std::fs::write(
        agent_dir
            .join("learning")
            .join("backfill")
            .join("not-a-corpus.json"),
        "{}",
    )
    .unwrap();
    record(
        "backfill",
        &["trajectory", "--no-seal", "--include-backfill", "--json"],
    );
    record(
        "backfill-table",
        &[
            "trajectory",
            "--no-seal",
            "--include-backfill",
            "--limit",
            "2",
            "--min-windows=2",
            "--gap",
            "1",
        ],
    );
    let store_after_backfill = std::fs::read_to_string(&store_path).unwrap();
    assert_eq!(
        json!({
            "runs": runs,
            "storeAfterTable": store_after_table,
            "gitignore": gitignore,
            "storeAfterBackfill": store_after_backfill,
        }),
        expected
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
