//! `prime-agent learning` (TS `cli/learning-command.ts`): seal complete days
//! of the structured log into the learning index, then report whether the
//! fingerprints a refinement claimed to address got rarer than the ones it
//! did not; `learning trajectory` reports the Engineer Trajectory Index.
//!
//! Sealing is the point of running it: `agent.jsonl` rotates by size, so the
//! roll-up has to be written while the raw lines are still on disk.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::chart::{render_ascii_chart, ChartOptions, ChartSeries};
use crate::index::{read_learning_index, seal_learning_days, SealResult};
use crate::js::{pad_end, pad_start, to_exponential, to_fixed, to_precision};
use crate::json::stringify_pretty;
use crate::report::{
    build_learning_report, FingerprintTrend, LearningReport, Window, DEFAULT_MIN_COHORT_N,
};
use crate::store::{
    agent_log_path, learning_index_dir, read_backfill_days, trajectory_backfill_dir,
    trajectory_index_path, write_trajectory_index,
};
use crate::trajectory::{
    seal_trajectory_windows, SealTrajectoryOptions, TrajectoryLabel, TrajectoryLabelKind,
    TrajectoryRateWindow, TrajectoryStoreFile, DEFAULT_MIN_TRAJECTORY_WINDOWS,
    DEFAULT_TRAJECTORY_INTERNALIZED_GAP, PRIME_CORPUS,
};

/// The command line.
pub const LEARNING_USAGE: &str = "learning [--log <path>] [--index <dir>] [--min-n <n>] [--limit <n>] [--no-seal] [--no-chart] [--json]";
/// One-line summary.
pub const LEARNING_SUMMARY: &str =
    "Report whether addressed failure fingerprints actually got rarer";
/// The help description.
pub const LEARNING_DESCRIPTION: &str = "Seals complete days of the structured log into a day-partitioned roll-up under ~/.prime/agent/learning, then compares the change in failure rate for fingerprints named in a refinement.committed against every other observed fingerprint. The roll-up is written while the raw log lines still exist, because agent.jsonl rotates by size. The p-value is withheld rather than printed when either cohort is below the minimum size. The `trajectory` subcommand instead buckets the sealed days into observed ISO-week windows and reports per-fingerprint NEW/DROPPED/PERSISTS labels (every claim confound-flagged; no label below the minimum window count).";
/// The help option rows.
pub const LEARNING_OPTIONS: &[&str] = &[
    "--log <path>          Read this JSONL log instead of the default agent log",
    "--index <dir>         Read and write roll-ups in this directory",
    "--min-n <n>           Withhold the p-value below this cohort size (default: 5)",
    "--limit <n>           Show at most this many fingerprint rows (default: 40)",
    "--no-seal             Report on the existing index without reading the log",
    "--no-chart            Skip the ASCII chart",
    "--json                Print the report as JSON",
    "trajectory            Report the Engineer Trajectory Index (NEW/DROPPED/PERSISTS over ISO-week windows)",
    "  --include-backfill  Fold in the offline cross-tool backfill days (confound-flagged, CLI table only)",
    "  --min-windows <n>   Withhold every label below this many observed windows (default: 4)",
    "  --gap <m>           Absent-window count that makes a recurring fingerprint DROPPED (default: 2)",
];
/// The help examples.
pub const LEARNING_EXAMPLES: &[&str] = &[
    "learning",
    "learning --min-n 10 --json",
    "learning --no-seal --no-chart",
    "learning trajectory",
    "learning trajectory --include-backfill --json",
];

const MAX_ROWS: usize = 40;
const SMALLEST_REPORTABLE_P: f64 = 1e-6;

/// Where the command writes and what time it is.
pub trait LearningCommandIo {
    fn stdout(&mut self, line: &str);
    fn stderr(&mut self, line: &str);
    /// Epoch milliseconds, read once per run.
    fn now(&self) -> u64;
}

/// Which report a run printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningSubcommand {
    Report,
    Trajectory,
}

impl LearningSubcommand {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Trajectory => "trajectory",
        }
    }
}

/// How a run ended: a report with its inference (`Reported`), a report with
/// everything withheld (`Withheld`, exit 2), or an error (exit 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningOutcome {
    Reported,
    Withheld,
    Failed,
}

impl LearningOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Withheld => "withheld",
            Self::Failed => "failed",
        }
    }

    fn code(self) -> i32 {
        match self {
            Self::Reported => 0,
            Self::Withheld => 2,
            Self::Failed => 1,
        }
    }
}

/// What a run did, for its adoption event (counts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearningRunReport {
    pub subcommand: LearningSubcommand,
    pub outcome: LearningOutcome,
    /// Sealed days the report read.
    pub days: usize,
    /// Days this run sealed.
    pub sealed: usize,
    pub backfill: bool,
}

/// The exit code and, for a parsed invocation, its run report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearningCommandOutcome {
    pub exit_code: i32,
    pub report: Option<LearningRunReport>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct UsageError(String);

fn take_value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, UsageError> {
    match args.get(index) {
        Some(value) if !value.starts_with('-') => Ok(value),
        _ => Err(UsageError(format!("{option} requires a value."))),
    }
}

/// `Number.parseInt(raw, 10)` as a positive safe integer.
fn positive_integer(raw: &str, option: &str) -> Result<u64, UsageError> {
    let trimmed = raw.trim_start();
    let (negative, body) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits: String = body.chars().take_while(char::is_ascii_digit).collect();
    let parsed = digits
        .parse::<u64>()
        .ok()
        .filter(|value| *value < (1 << 53));
    match parsed {
        Some(value) if !negative && value > 0 => Ok(value),
        _ => Err(UsageError(format!("{option} requires a positive integer."))),
    }
}

/// A `--name value` / `--name=value` option, when `arg` is one.
fn option_value(
    args: &[String],
    index: &mut usize,
    name: &str,
) -> Result<Option<String>, UsageError> {
    let arg = &args[*index];
    if arg == name {
        *index += 1;
        return take_value(args, *index, name).map(|value| Some(value.to_string()));
    }
    match arg
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('='))
    {
        Some(value) => Ok(Some(value.to_string())),
        None => Ok(None),
    }
}

struct LearningOptions {
    log_path: Option<String>,
    index_dir: Option<String>,
    min_cohort_n: u64,
    json: bool,
    seal: bool,
    chart: bool,
    limit: usize,
}

fn parse_learning_args(args: &[String]) -> Result<LearningOptions, UsageError> {
    let mut options = LearningOptions {
        log_path: None,
        index_dir: None,
        min_cohort_n: DEFAULT_MIN_COHORT_N,
        json: false,
        seal: true,
        chart: true,
        limit: MAX_ROWS,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => options.json = true,
            "--no-seal" => options.seal = false,
            "--no-chart" => options.chart = false,
            _ => {
                if let Some(value) = option_value(args, &mut index, "--log")? {
                    if value.is_empty() {
                        return Err(UsageError("--log requires a value.".to_string()));
                    }
                    options.log_path = Some(value);
                } else if let Some(value) = option_value(args, &mut index, "--index")? {
                    if value.is_empty() {
                        return Err(UsageError("--index requires a value.".to_string()));
                    }
                    options.index_dir = Some(value);
                } else if let Some(value) = option_value(args, &mut index, "--min-n")? {
                    options.min_cohort_n = positive_integer(&value, "--min-n")?;
                } else if let Some(value) = option_value(args, &mut index, "--limit")? {
                    options.limit =
                        usize::try_from(positive_integer(&value, "--limit")?).unwrap_or(usize::MAX);
                } else if arg.starts_with('-') {
                    return Err(UsageError(format!("Unknown option for learning: {arg}")));
                } else {
                    return Err(UsageError(format!("learning takes no operands: {arg}")));
                }
            }
        }
        index += 1;
    }
    Ok(options)
}

fn fixed(value: f64, digits: usize) -> String {
    if value.is_finite() {
        to_fixed(value, digits)
    } else {
        "-".to_string()
    }
}

/// The p-value, or why it is withheld. The approximation is good to about
/// 1e-7, so anything smaller prints as a bound.
#[must_use]
pub fn format_significance(report: &LearningReport) -> String {
    let p_value = match (&report.insufficient_evidence, report.p_value) {
        (None, Some(p_value)) => p_value,
        (reason, _) => {
            return format!(
                "p-value: withheld - {}",
                reason.as_deref().unwrap_or("no test was run")
            );
        }
    };
    let p = if p_value < SMALLEST_REPORTABLE_P {
        format!("< {}", to_exponential(SMALLEST_REPORTABLE_P, 0))
    } else if p_value < 0.001 {
        format!("= {}", to_exponential(p_value, 2))
    } else {
        format!("= {}", to_precision(p_value, 3))
    };
    format!(
        "Mann-Whitney U (one-sided, treated < untreated): U = {}, p {p}",
        fixed(report.u.unwrap_or(0.0), 1)
    )
}

fn format_rows(trends: &[FingerprintTrend], limit: usize) -> Vec<String> {
    let shown = &trends[..limit.min(trends.len())];
    if shown.is_empty() {
        return vec!["  (no failure fingerprints observed in either window)".to_string()];
    }
    let id_width = shown
        .iter()
        .map(|trend| crate::js::js_len(&trend.fingerprint))
        .max()
        .unwrap_or(0)
        .max(11);
    let name_width = shown
        .iter()
        .map(|trend| crate::js::js_len(&trend.name))
        .max()
        .unwrap_or(0)
        .max(4);
    let header = format!(
        "  {}  {}  {}  {}  {}  {}",
        pad_end("fingerprint", id_width),
        pad_end("span", name_width),
        pad_end("cohort", 7),
        pad_start("before/1k", 9),
        pad_start("after/1k", 9),
        pad_start("delta", 9)
    );
    let mut lines = vec![
        header.clone(),
        format!("  {}", "-".repeat(crate::js::js_len(&header) - 2)),
    ];
    for trend in shown {
        lines.push(format!(
            "  {}  {}  {}  {}  {}  {}",
            pad_end(&trend.fingerprint, id_width),
            pad_end(&trend.name, name_width),
            pad_end(if trend.treated { "treated" } else { "control" }, 7),
            pad_start(&fixed(trend.before_rate, 2), 9),
            pad_start(&fixed(trend.after_rate, 2), 9),
            pad_start(&fixed(trend.delta, 2), 9)
        ));
    }
    if trends.len() > shown.len() {
        lines.push(format!(
            "  ... {} more (raise --limit)",
            trends.len() - shown.len()
        ));
    }
    lines
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// The report as the command prints it (TS `formatLearningReport`).
#[must_use]
pub fn format_learning_report(
    report: &LearningReport,
    index_dir: &str,
    seal: Option<&SealResult>,
    limit: usize,
) -> String {
    let mut out = vec![format!("learning index  {index_dir}")];
    let range = match (report.days.first(), report.days.last()) {
        (Some(first), Some(last)) => format!(" ({first} .. {last})"),
        _ => String::new(),
    };
    out.push(format!(
        "  {} sealed day{}{range}, {} turns in the compared windows, {} refinement commit{}",
        report.days.len(),
        plural(report.days.len()),
        report.turns_before + report.turns_after,
        report.commits,
        plural(report.commits)
    ));
    if let Some(seal) = seal {
        let parse_errors = if seal.parse_errors > 0 {
            format!(", {} unparsable log lines", seal.parse_errors)
        } else {
            String::new()
        };
        out.push(format!(
            "  sealed {} new day{}, kept {}, left {} open{parse_errors}",
            seal.written.len(),
            plural(seal.written.len()),
            seal.skipped.len(),
            seal.open.len()
        ));
    }
    if let (Some(pivot_day), Some(pivot_at)) = (&report.pivot_day, &report.pivot_at) {
        out.push(format!(
            "  pivot {pivot_at} (day {pivot_day} excluded from both windows), before {} turns / after {} turns",
            report.turns_before, report.turns_after
        ));
    }
    if !report.unobserved_treated.is_empty() {
        out.push(format!(
            "  {} addressed fingerprint(s) were never observed and are not scored",
            report.unobserved_treated.len()
        ));
    }
    out.push(String::new());
    out.extend(format_rows(&report.fingerprints, limit));
    out.push(String::new());
    let cohort_row = |label: &str, n: usize, median: f64, mean: f64| {
        format!(
            "  {}  {}  {}  {}",
            pad_end(label, 9),
            pad_start(&n.to_string(), 3),
            pad_start(&fixed(median, 2), 13),
            pad_start(&fixed(mean, 2), 11)
        )
    };
    out.push(format!(
        "  {}    n   median delta   mean delta",
        pad_end("cohort", 9)
    ));
    out.push(cohort_row(
        "treated",
        report.treated.n(),
        report.treated.median_delta,
        report.treated.mean_delta,
    ));
    out.push(cohort_row(
        "control",
        report.untreated.n(),
        report.untreated.median_delta,
        report.untreated.mean_delta,
    ));
    out.push(String::new());
    out.push(format!("  {}", format_significance(report)));
    out.join("\n")
}

/// The treated/control chart of a report's series.
#[must_use]
pub fn learning_chart_lines(report: &LearningReport) -> Vec<String> {
    if report.series.is_empty() {
        return Vec::new();
    }
    let marker_index = report
        .series
        .iter()
        .position(|point| point.window == Window::Pivot);
    render_ascii_chart(
        &[
            ChartSeries {
                label: "treated".to_string(),
                mark: 'T',
                points: report
                    .series
                    .iter()
                    .map(|point| Some(point.treated_rate))
                    .collect(),
            },
            ChartSeries {
                label: "control".to_string(),
                mark: 'u',
                points: report
                    .series
                    .iter()
                    .map(|point| Some(point.untreated_rate))
                    .collect(),
            },
        ],
        &ChartOptions {
            x_labels: report
                .series
                .iter()
                .map(|point| point.day.clone())
                .collect(),
            marker_index,
            value_label: Some("mean failures per 1000 turns, per fingerprint".to_string()),
            ..ChartOptions::default()
        },
    )
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run `prime-agent learning <args>` against `agent_dir`.
// The TS `runLearningCommand` in order: parse, seal, read, report, print.
#[allow(clippy::too_many_lines)]
pub fn run_learning_command(
    args: &[String],
    agent_dir: &Path,
    io: &mut dyn LearningCommandIo,
) -> LearningCommandOutcome {
    if args.first().map(String::as_str) == Some("trajectory") {
        return run_trajectory(&args[1..], agent_dir, io);
    }
    let options = match parse_learning_args(args) {
        Ok(options) => options,
        Err(error) => {
            io.stderr(&format!("Error: {error}"));
            io.stderr(&format!("Usage: prime-agent {LEARNING_USAGE}"));
            return LearningCommandOutcome {
                exit_code: 1,
                report: None,
            };
        }
    };
    let index_dir = options
        .index_dir
        .as_ref()
        .map_or_else(|| learning_index_dir(agent_dir), PathBuf::from);
    let now_ms = io.now();
    let failed = |sealed: usize| LearningRunReport {
        subcommand: LearningSubcommand::Report,
        outcome: LearningOutcome::Failed,
        days: 0,
        sealed,
        backfill: false,
    };
    let mut seal = None;
    if options.seal {
        let log_path = options
            .log_path
            .as_ref()
            .map_or_else(|| agent_log_path(agent_dir), PathBuf::from);
        let files = pa_trace::retained_log_files(&log_path);
        if files.is_empty() && options.log_path.is_some() {
            io.stderr(&format!(
                "Error: no log file at {}",
                display_path(&log_path)
            ));
            return LearningCommandOutcome {
                exit_code: 1,
                report: Some(failed(0)),
            };
        }
        match seal_learning_days(&files, &index_dir, now_ms, false) {
            Ok(result) => seal = Some(result),
            Err(error) => {
                io.stderr(&format!(
                    "Error: could not seal {}: {}",
                    display_path(&log_path),
                    error.reason()
                ));
                return LearningCommandOutcome {
                    exit_code: 1,
                    report: Some(failed(0)),
                };
            }
        }
    }
    let sealed = seal.as_ref().map_or(0, |seal| seal.written.len());
    let days = read_learning_index(&index_dir);
    if days.is_empty() {
        io.stderr(&format!(
            "Error: no sealed days in {}",
            display_path(&index_dir)
        ));
        return LearningCommandOutcome {
            exit_code: 1,
            report: Some(failed(sealed)),
        };
    }
    let report = build_learning_report(&days, options.min_cohort_n, now_ms);
    let outcome = if report.p_value.is_some() {
        LearningOutcome::Reported
    } else {
        LearningOutcome::Withheld
    };
    let index_text = display_path(&index_dir);
    if options.json {
        let mut extra = vec![("indexDir", Value::from(index_text.as_str()))];
        if let Some(seal) = &seal {
            extra.push(("seal", seal.to_json()));
        }
        io.stdout(&stringify_pretty(&report.to_json(extra)));
    } else {
        io.stdout(&format_learning_report(
            &report,
            &index_text,
            seal.as_ref(),
            options.limit,
        ));
        if options.chart {
            io.stdout("");
            for line in learning_chart_lines(&report) {
                io.stdout(&line);
            }
        }
    }
    LearningCommandOutcome {
        exit_code: outcome.code(),
        report: Some(LearningRunReport {
            subcommand: LearningSubcommand::Report,
            outcome,
            days: days.len(),
            sealed,
            backfill: false,
        }),
    }
}

// ---------------------------------------------------------------------------
// `learning trajectory`

struct TrajectoryOptions {
    index_dir: Option<String>,
    include_backfill: bool,
    min_windows: u64,
    gap: u64,
    seal: bool,
    json: bool,
    limit: usize,
}

fn parse_trajectory_args(args: &[String]) -> Result<TrajectoryOptions, UsageError> {
    let mut options = TrajectoryOptions {
        index_dir: None,
        include_backfill: false,
        min_windows: DEFAULT_MIN_TRAJECTORY_WINDOWS,
        gap: DEFAULT_TRAJECTORY_INTERNALIZED_GAP,
        seal: true,
        json: false,
        limit: MAX_ROWS,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--json" => options.json = true,
            "--no-seal" => options.seal = false,
            "--include-backfill" => options.include_backfill = true,
            _ => {
                if let Some(value) = option_value(args, &mut index, "--index")? {
                    if value.is_empty() {
                        return Err(UsageError("--index requires a value.".to_string()));
                    }
                    options.index_dir = Some(value);
                } else if let Some(value) = option_value(args, &mut index, "--min-windows")? {
                    options.min_windows = positive_integer(&value, "--min-windows")?;
                } else if let Some(value) = option_value(args, &mut index, "--gap")? {
                    options.gap = positive_integer(&value, "--gap")?;
                } else if let Some(value) = option_value(args, &mut index, "--limit")? {
                    options.limit =
                        usize::try_from(positive_integer(&value, "--limit")?).unwrap_or(usize::MAX);
                } else if arg.starts_with('-') {
                    return Err(UsageError(format!(
                        "Unknown option for learning trajectory: {arg}"
                    )));
                } else {
                    return Err(UsageError(format!(
                        "learning trajectory takes no operands: {arg}"
                    )));
                }
            }
        }
        index += 1;
    }
    Ok(options)
}

/// One printable row of the trajectory table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryReportRow {
    pub fingerprint: String,
    pub name: String,
    pub corpus: String,
    pub label: String,
    pub since_window: String,
    pub last_window: String,
    pub windows_recurring: u64,
    pub confounds: String,
    pub security_class: bool,
}

/// One window's summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryReportWindow {
    pub window: String,
    pub corpus: String,
    pub days: usize,
    pub turns: u64,
}

/// The sealed file shaped for printing (TS `buildTrajectoryReport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryReport {
    pub windows_observed: u64,
    pub min_windows: u64,
    pub all_withheld: bool,
    pub rows: Vec<TrajectoryReportRow>,
    pub rate: Vec<TrajectoryRateWindow>,
    pub windows: Vec<TrajectoryReportWindow>,
}

impl TrajectoryReport {
    /// The report as the TS object; `extra` keys follow it.
    #[must_use]
    pub fn to_json(&self, extra: Vec<(&str, Value)>) -> Value {
        let mut out = Map::new();
        out.insert("windowsObserved".into(), Value::from(self.windows_observed));
        out.insert("minWindows".into(), Value::from(self.min_windows));
        out.insert("allWithheld".into(), Value::from(self.all_withheld));
        let rows = self
            .rows
            .iter()
            .map(|row| {
                let mut out = Map::new();
                out.insert("fingerprint".into(), Value::from(row.fingerprint.as_str()));
                out.insert("name".into(), Value::from(row.name.as_str()));
                out.insert("corpus".into(), Value::from(row.corpus.as_str()));
                out.insert("label".into(), Value::from(row.label.as_str()));
                out.insert("sinceWindow".into(), Value::from(row.since_window.as_str()));
                out.insert("lastWindow".into(), Value::from(row.last_window.as_str()));
                out.insert(
                    "windowsRecurring".into(),
                    Value::from(row.windows_recurring),
                );
                out.insert("confounds".into(), Value::from(row.confounds.as_str()));
                out.insert("securityClass".into(), Value::from(row.security_class));
                Value::Object(out)
            })
            .collect();
        out.insert("rows".into(), Value::Array(rows));
        out.insert(
            "rate".into(),
            Value::Array(
                self.rate
                    .iter()
                    .map(TrajectoryRateWindow::to_json)
                    .collect(),
            ),
        );
        let windows = self
            .windows
            .iter()
            .map(|window| {
                let mut out = Map::new();
                out.insert("window".into(), Value::from(window.window.as_str()));
                out.insert("corpus".into(), Value::from(window.corpus.as_str()));
                out.insert("days".into(), Value::from(window.days));
                out.insert("turns".into(), Value::from(window.turns));
                Value::Object(out)
            })
            .collect();
        out.insert("windows".into(), Value::Array(windows));
        for (key, value) in extra {
            out.insert(key.to_string(), value);
        }
        Value::Object(out)
    }
}

fn label_rank(label: &TrajectoryLabel) -> u8 {
    match label.label {
        Some(TrajectoryLabelKind::Persists) => 0,
        Some(TrajectoryLabelKind::New) => 1,
        Some(TrajectoryLabelKind::Dropped) => 2,
        None => 3,
    }
}

/// Shape a sealed file into printable rows, strongest signal first.
#[must_use]
pub fn build_trajectory_report(file: &TrajectoryStoreFile) -> TrajectoryReport {
    let mut labels: Vec<&TrajectoryLabel> = file.labels.iter().collect();
    labels.sort_by(|left, right| {
        label_rank(left)
            .cmp(&label_rank(right))
            .then_with(|| right.windows_recurring.cmp(&left.windows_recurring))
            .then_with(|| pa_ravo::locale_compare(&left.corpus, &right.corpus))
            .then_with(|| pa_ravo::locale_compare(&left.fingerprint, &right.fingerprint))
    });
    let rows = labels
        .into_iter()
        .map(|label| TrajectoryReportRow {
            fingerprint: label.fingerprint.clone(),
            name: label.name.clone(),
            corpus: label.corpus.clone(),
            label: match (label.label, &label.withheld) {
                (Some(kind), _) => kind.as_str().to_string(),
                (None, Some(withheld)) => format!("withheld: {withheld}"),
                (None, None) => "-".to_string(),
            },
            since_window: label.since_window.clone(),
            last_window: label.last_window.clone(),
            windows_recurring: label.windows_recurring,
            confounds: label.confounds.join(", "),
            security_class: label.security_class,
        })
        .collect();
    let prime: Vec<&TrajectoryLabel> = file
        .labels
        .iter()
        .filter(|label| label.corpus == PRIME_CORPUS)
        .collect();
    TrajectoryReport {
        windows_observed: file.windows_observed,
        min_windows: file.min_windows,
        all_withheld: prime.iter().all(|label| label.label.is_none()),
        rows,
        rate: file.rate.clone(),
        windows: file
            .windows
            .iter()
            .map(|window| TrajectoryReportWindow {
                window: window.window.clone(),
                corpus: window.corpus.clone(),
                days: window.days.len(),
                turns: window.turns,
            })
            .collect(),
    }
}

/// The trajectory report as the command prints it.
#[must_use]
// One printed section after another, as the TS formatter writes them.
#[allow(clippy::too_many_lines)]
pub fn format_trajectory_report(
    report: &TrajectoryReport,
    index_dir: &str,
    store_path: &str,
    limit: usize,
) -> String {
    let mut out = vec![
        format!("engineer trajectory  {store_path}"),
        format!("  learning days  {index_dir}"),
        format!(
            "  {} observed window{} (min {} to emit a label)",
            report.windows_observed,
            if report.windows_observed == 1 {
                ""
            } else {
                "s"
            },
            report.min_windows
        ),
    ];
    if !report.windows.is_empty() {
        let spans: Vec<String> = report
            .windows
            .iter()
            .map(|window| {
                format!(
                    "{}({}:{}d/{}t)",
                    window.window, window.corpus, window.days, window.turns
                )
            })
            .collect();
        out.push(format!("  windows: {}", spans.join(" ")));
    }
    out.push(String::new());
    let shown = &report.rows[..limit.min(report.rows.len())];
    if shown.is_empty() {
        out.push("  (no failure fingerprints observed in any sealed window)".to_string());
    } else {
        let width = |values: &mut dyn Iterator<Item = usize>, floor: usize| {
            values.max().unwrap_or(0).max(floor)
        };
        let id_width = width(
            &mut shown.iter().map(|row| crate::js::js_len(&row.fingerprint)),
            11,
        );
        let name_width = width(
            &mut shown.iter().map(|row| crate::js::js_len(&row.name).max(1)),
            4,
        );
        let label_width = width(
            &mut shown.iter().map(|row| crate::js::js_len(&row.label)),
            5,
        );
        let header = format!(
            "  {}  {}  {}  {}  {}  sec  confounds",
            pad_end("fingerprint", id_width),
            pad_end("span", name_width),
            pad_end("label", label_width),
            pad_end("window span", 21),
            pad_start("rec", 3)
        );
        out.push(header.clone());
        out.push(format!(
            "  {}",
            "-".repeat(crate::js::js_len(&header).saturating_sub(2))
        ));
        for row in shown {
            let name = if row.name.is_empty() {
                "-"
            } else {
                row.name.as_str()
            };
            out.push(format!(
                "  {}  {}  {}  {}  {}  {}  {}",
                pad_end(&row.fingerprint, id_width),
                pad_end(name, name_width),
                pad_end(&row.label, label_width),
                pad_end(&format!("{}..{}", row.since_window, row.last_window), 21),
                pad_start(&row.windows_recurring.to_string(), 3),
                if row.security_class { "yes" } else { " - " },
                row.confounds
            ));
        }
        if report.rows.len() > shown.len() {
            out.push(format!(
                "  ... {} more (raise --limit)",
                report.rows.len() - shown.len()
            ));
        }
    }
    out.push(String::new());
    out.push("  rate of change (new - retired per window):".to_string());
    if report.rate.is_empty() {
        out.push("    (none)".to_string());
    } else {
        for step in &report.rate {
            let net = step
                .new_minus_retired
                .map_or_else(|| "null (gap)".to_string(), |net| net.to_string());
            let retired = step
                .retired
                .map_or_else(|| "null".to_string(), |retired| retired.to_string());
            out.push(format!(
                "    {}  appeared {}, retired {retired}, net {net}",
                step.window, step.appeared
            ));
        }
    }
    out.join("\n")
}

// The TS `runTrajectorySubcommand` in order: parse, seal, read, the
// `trajectory.seal` span, print.
#[allow(clippy::too_many_lines)]
fn run_trajectory(
    args: &[String],
    agent_dir: &Path,
    io: &mut dyn LearningCommandIo,
) -> LearningCommandOutcome {
    let options = match parse_trajectory_args(args) {
        Ok(options) => options,
        Err(error) => {
            io.stderr(&format!("Error: {error}"));
            io.stderr("Usage: prime-agent learning trajectory [--index <dir>] [--include-backfill] [--min-windows <n>] [--gap <m>] [--no-seal] [--limit <n>] [--json]");
            return LearningCommandOutcome {
                exit_code: 1,
                report: None,
            };
        }
    };
    let index_dir = options
        .index_dir
        .as_ref()
        .map_or_else(|| learning_index_dir(agent_dir), PathBuf::from);
    let now_ms = io.now();
    let failed = |days: usize| LearningCommandOutcome {
        exit_code: 1,
        report: Some(LearningRunReport {
            subcommand: LearningSubcommand::Trajectory,
            outcome: LearningOutcome::Failed,
            days,
            sealed: 0,
            backfill: options.include_backfill,
        }),
    };
    let mut sealed = 0;
    if options.seal {
        let files = pa_trace::retained_log_files(&agent_log_path(agent_dir));
        match seal_learning_days(&files, &index_dir, now_ms, false) {
            Ok(result) => sealed = result.written.len(),
            Err(error) => {
                io.stderr(&format!(
                    "Error: could not seal the learning index: {}",
                    error.reason()
                ));
                return failed(0);
            }
        }
    }
    let days = read_learning_index(&index_dir);
    let backfill_days = if options.include_backfill {
        read_backfill_days(&trajectory_backfill_dir(agent_dir))
    } else {
        Vec::new()
    };
    if days.is_empty() && backfill_days.is_empty() {
        io.stderr(&format!(
            "Error: no sealed days in {}",
            display_path(&index_dir)
        ));
        return failed(0);
    }
    let store_path = trajectory_index_path(agent_dir);
    let span = tracing::info_span!(
        "trajectory.seal",
        windows = tracing::field::Empty,
        labelled = tracing::field::Empty,
        withheld = tracing::field::Empty,
        backfill = options.include_backfill
    );
    let report = span.in_scope(|| {
        // The persisted store is always prime-only.
        let seal_options = SealTrajectoryOptions {
            days: &days,
            backfill_days: &[],
            min_windows: Some(options.min_windows),
            internalized_gap: Some(options.gap),
            now_ms,
        };
        let disk_file = seal_trajectory_windows(&seal_options);
        write_trajectory_index(&disk_file, agent_dir);
        let display_file = if options.include_backfill {
            seal_trajectory_windows(&SealTrajectoryOptions {
                backfill_days: &backfill_days,
                ..seal_options
            })
        } else {
            disk_file
        };
        let prime_labels = || {
            display_file
                .labels
                .iter()
                .filter(|label| label.corpus == PRIME_CORPUS)
        };
        span.record("windows", display_file.windows_observed);
        span.record(
            "labelled",
            prime_labels().filter(|label| label.label.is_some()).count(),
        );
        span.record(
            "withheld",
            prime_labels()
                .filter(|label| label.withheld.is_some())
                .count(),
        );
        build_trajectory_report(&display_file)
    });
    let outcome = if report.all_withheld {
        LearningOutcome::Withheld
    } else {
        LearningOutcome::Reported
    };
    let index_text = display_path(&index_dir);
    let store_text = display_path(&store_path);
    if options.json {
        io.stdout(&stringify_pretty(&report.to_json(vec![
            ("indexDir", Value::from(index_text.as_str())),
            ("storePath", Value::from(store_text.as_str())),
        ])));
    } else {
        io.stdout(&format_trajectory_report(
            &report,
            &index_text,
            &store_text,
            options.limit,
        ));
    }
    LearningCommandOutcome {
        exit_code: outcome.code(),
        report: Some(LearningRunReport {
            subcommand: LearningSubcommand::Trajectory,
            outcome,
            days: days.len(),
            sealed,
            backfill: options.include_backfill,
        }),
    }
}
