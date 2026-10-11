//! The learning index: a day-partitioned roll-up of `span_end` records keyed
//! by failure fingerprint, plus the `refinement.committed` records naming the
//! fingerprints a committed refinement claimed to address (TS
//! `learning-index.ts`).
//!
//! It exists because `agent.jsonl` rotates by size: on a busy machine one
//! day fills every retained generation, so the evidence for a multi-week
//! trend is gone before the trend can form. The roll-up is written while the
//! raw lines still exist and is orders of magnitude smaller, so it survives.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::LearningError;
use crate::json::{number, stringify_pretty};

/// `schema` of a sealed day.
pub const LEARNING_INDEX_SCHEMA: u64 = 1;
/// Message of the record naming the fingerprints a committed refinement
/// addressed (written by `pa-ravo`).
pub const REFINEMENT_COMMITTED_MSG: &str = "refinement.committed";
/// Exposure unit: one assistant turn of the agent loop.
pub const TURN_SPAN_NAME: &str = "agent.turn";
/// Component and message of a finished span in the structured log.
const TRACE_COMPONENT: &str = "trace";
const SPAN_END_MSG: &str = "span_end";

const MAX_DURATION_SAMPLES: usize = 20_000;
const MAX_FINGERPRINTS_PER_DAY: usize = 50_000;
const MAX_COMMITS_PER_DAY: usize = 10_000;

/// One key's roll-up for one day.
#[derive(Debug, Clone, PartialEq)]
pub struct FingerprintDayStats {
    /// The ledger fingerprint id when the span carried one, else
    /// `span:<sha256(name NUL status NUL message)[..16]>`.
    pub fingerprint: String,
    pub name: String,
    pub status: String,
    /// Whether this key counts failures (only failures are compared).
    pub failure: bool,
    pub count: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// Normalized error text, so the reports can name the key.
    pub message: Option<String>,
    /// Durations were capped, so the percentiles are over a prefix.
    pub sampled: bool,
}

/// A `refinement.committed` record that claimed at least one fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementCommit {
    pub at: String,
    pub proposal_id: String,
    pub addressed: Vec<String>,
}

/// One sealed UTC day.
#[derive(Debug, Clone, PartialEq)]
pub struct LearningDay {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub sealed_at: String,
    /// `agent.turn` span ends this day: the denominator of every rate.
    pub turns: u64,
    pub fingerprints: Vec<FingerprintDayStats>,
    pub commits: Vec<RefinementCommit>,
    pub parse_errors: f64,
    pub source_files: Vec<String>,
}

impl FingerprintDayStats {
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("fingerprint".into(), Value::from(self.fingerprint.as_str()));
        out.insert("name".into(), Value::from(self.name.as_str()));
        out.insert("status".into(), Value::from(self.status.as_str()));
        out.insert("failure".into(), Value::from(self.failure));
        out.insert("count".into(), Value::from(self.count));
        out.insert("p50Ms".into(), number(self.p50_ms));
        out.insert("p95Ms".into(), number(self.p95_ms));
        if let Some(message) = &self.message {
            out.insert("message".into(), Value::from(message.as_str()));
        }
        if self.sampled {
            out.insert("sampled".into(), Value::from(true));
        }
        Value::Object(out)
    }
}

impl RefinementCommit {
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("at".into(), Value::from(self.at.as_str()));
        out.insert("proposalId".into(), Value::from(self.proposal_id.as_str()));
        out.insert("addressed".into(), Value::from(self.addressed.clone()));
        Value::Object(out)
    }
}

impl LearningDay {
    /// The day as the TS `LearningDay` object (key order included).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("schema".into(), Value::from(LEARNING_INDEX_SCHEMA));
        out.insert("day".into(), Value::from(self.day.as_str()));
        out.insert("sealedAt".into(), Value::from(self.sealed_at.as_str()));
        out.insert("turns".into(), Value::from(self.turns));
        out.insert(
            "fingerprints".into(),
            Value::Array(
                self.fingerprints
                    .iter()
                    .map(FingerprintDayStats::to_json)
                    .collect(),
            ),
        );
        out.insert(
            "commits".into(),
            Value::Array(self.commits.iter().map(RefinementCommit::to_json).collect()),
        );
        out.insert("parseErrors".into(), number(self.parse_errors));
        out.insert("sourceFiles".into(), Value::from(self.source_files.clone()));
        Value::Object(out)
    }
}

/// Whether `text` is a `YYYY-MM-DD` key.
pub(crate) fn is_day_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

fn entry_day(entry: &Map<String, Value>) -> Option<&str> {
    let ts = entry.get("ts")?.as_str()?;
    let day = ts.get(..10)?;
    is_day_key(day).then_some(day)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// The fingerprints a commit record names: the TS array, or the
/// comma-joined string `pa-ravo` records (a `tracing` field holds no array).
fn addressed_of(entry: &Map<String, Value>) -> Vec<String> {
    match entry.get("addressed") {
        Some(Value::String(joined)) => joined
            .split(',')
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect(),
        other => string_list(other),
    }
}

/// What a span end rolls up under (TS `spanFingerprintKey`): an explicit
/// ledger fingerprint wins, so the index and the ledger agree on identity;
/// otherwise a hash of the name, status and normalized error text, stable
/// across runs and never colliding with a ledger id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanKey {
    pub fingerprint: String,
    pub failure: bool,
    pub message: String,
}

/// The [`SpanKey`] of one `span_end` entry.
#[must_use]
pub fn span_fingerprint_key(entry: &Map<String, Value>) -> SpanKey {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = entry.get("status").and_then(Value::as_str).unwrap_or("ok");
    let message = pa_ledger::normalize_failure_message(
        entry
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let explicit = entry
        .get("attrs")
        .and_then(Value::as_object)
        .and_then(|attrs| attrs.get(pa_ledger::FAILURE_FINGERPRINT_ATTR))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    if let Some(explicit) = explicit {
        return SpanKey {
            fingerprint: explicit.to_string(),
            failure: true,
            message,
        };
    }
    // An in-cell traceback ends `kernel.cell` as an error and is separately
    // fingerprinted onto the enclosing `tool.execute`, so one failure counts
    // under two keys. Left in deliberately (TS): the shadow lands in the
    // control cohort, which only biases toward the null.
    let digest = Sha256::digest(format!("{name}\0{status}\0{message}").as_bytes());
    let hex = digest.iter().take(8).fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    SpanKey {
        fingerprint: format!("span:{hex}"),
        failure: status == "error",
        message,
    }
}

struct Bucket {
    stats: FingerprintDayStats,
    durations: Vec<f64>,
}

struct DayAccumulator {
    turns: u64,
    keys: IndexMap<String, Bucket>,
    commits: Vec<RefinementCommit>,
    files: BTreeSet<String>,
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "at most MAX_DURATION_SAMPLES durations"
    )]
    let rank = (fraction * sorted.len() as f64).ceil();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a rank within the sample count"
    )]
    let index = (rank.max(1.0) as usize - 1).min(sorted.len() - 1);
    sorted[index]
}

fn accumulate(days: &mut HashMap<String, DayAccumulator>, entry: &Map<String, Value>, file: &str) {
    let Some(day) = entry_day(entry) else {
        return;
    };
    let accumulator = days
        .entry(day.to_string())
        .or_insert_with(|| DayAccumulator {
            turns: 0,
            keys: IndexMap::new(),
            commits: Vec::new(),
            files: BTreeSet::new(),
        });
    accumulator.files.insert(file.to_string());
    let msg = entry.get("msg").and_then(Value::as_str).unwrap_or_default();
    if msg == REFINEMENT_COMMITTED_MSG {
        let addressed = addressed_of(entry);
        // A commit that claimed nothing treats no fingerprint: neither a
        // commit nor a pivot.
        if !addressed.is_empty() && accumulator.commits.len() < MAX_COMMITS_PER_DAY {
            let proposal_id = entry
                .get("proposalId")
                .or_else(|| entry.get("proposal_id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            accumulator.commits.push(RefinementCommit {
                at: entry
                    .get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                proposal_id: proposal_id.to_string(),
                addressed,
            });
        }
        return;
    }
    let component = entry
        .get("component")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if component != TRACE_COMPONENT || msg != SPAN_END_MSG || name.is_empty() {
        return;
    }
    if name == TURN_SPAN_NAME {
        accumulator.turns += 1;
    }
    let key = span_fingerprint_key(entry);
    if !accumulator.keys.contains_key(&key.fingerprint) {
        if accumulator.keys.len() >= MAX_FINGERPRINTS_PER_DAY {
            return;
        }
        let stats = FingerprintDayStats {
            fingerprint: key.fingerprint.clone(),
            name: name.to_string(),
            status: entry
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("ok")
                .to_string(),
            failure: key.failure,
            count: 0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            message: Some(key.message),
            sampled: false,
        };
        accumulator.keys.insert(
            key.fingerprint.clone(),
            Bucket {
                stats,
                durations: Vec::new(),
            },
        );
    }
    let Some(bucket) = accumulator.keys.get_mut(&key.fingerprint) else {
        return;
    };
    bucket.stats.count += 1;
    let duration = entry
        .get("durationMs")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);
    if bucket.durations.len() < MAX_DURATION_SAMPLES {
        bucket.durations.push(duration);
    } else {
        bucket.stats.sampled = true;
    }
}

fn finish(day: String, accumulator: DayAccumulator, sealed_at: &str) -> LearningDay {
    let mut fingerprints: Vec<FingerprintDayStats> = accumulator
        .keys
        .into_values()
        .map(|mut bucket| {
            bucket.durations.sort_by(f64::total_cmp);
            bucket.stats.p50_ms = percentile(&bucket.durations, 0.5);
            bucket.stats.p95_ms = percentile(&bucket.durations, 0.95);
            bucket.stats
        })
        .collect();
    fingerprints.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| pa_ravo::locale_compare(&left.fingerprint, &right.fingerprint))
    });
    LearningDay {
        day,
        sealed_at: sealed_at.to_string(),
        turns: accumulator.turns,
        fingerprints,
        commits: accumulator.commits,
        parse_errors: 0.0,
        source_files: accumulator.files.into_iter().collect(),
    }
}

/// Every day rolled up from a set of log files.
#[derive(Debug, Clone, PartialEq)]
pub struct RollUp {
    pub days: Vec<LearningDay>,
    /// Lines that were not a JSON object with string `msg` and `component`.
    pub parse_errors: u64,
}

/// Roll every `span_end` and `refinement.committed` line in `files` (oldest
/// first) up into day partitions; `now_ms` stamps `sealedAt`.
///
/// # Errors
///
/// [`LearningError::Read`] when a file cannot be read or decompressed.
pub fn roll_up_learning_days(files: &[PathBuf], now_ms: u64) -> Result<RollUp, LearningError> {
    let mut days: HashMap<String, DayAccumulator> = HashMap::new();
    let mut parse_errors = 0u64;
    for file in files {
        let content = pa_trace::read_log_text(file).map_err(|source| LearningError::Read {
            path: file.clone(),
            source,
        })?;
        let label = file.to_string_lossy();
        for raw in content.split('\n') {
            if raw.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(raw) {
                Ok(Value::Object(entry))
                    if entry.get("msg").is_some_and(Value::is_string)
                        && entry.get("component").is_some_and(Value::is_string) =>
                {
                    accumulate(&mut days, &entry, &label);
                }
                _ => parse_errors += 1,
            }
        }
    }
    let sealed_at = pa_ledger::iso_from_millis(now_ms);
    let mut keys: Vec<String> = days.keys().cloned().collect();
    keys.sort();
    let days = keys
        .into_iter()
        .filter_map(|key| {
            let accumulator = days.remove(&key)?;
            Some(finish(key, accumulator, &sealed_at))
        })
        .collect();
    Ok(RollUp { days, parse_errors })
}

/// A day file read back (TS `normalizeDay`); `None` when it is not a day.
#[must_use]
pub fn normalize_day(value: &Value) -> Option<LearningDay> {
    let raw = value.as_object()?;
    let day = raw.get("day")?.as_str().filter(|day| is_day_key(day))?;
    let mut fingerprints = Vec::new();
    for item in raw
        .get("fingerprints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(stat) = item.as_object() else {
            continue;
        };
        let (Some(fingerprint), Some(count)) = (
            stat.get("fingerprint").and_then(Value::as_str),
            stat.get("count").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let text = |key: &str, fallback: &str| {
            stat.get(key)
                .and_then(Value::as_str)
                .unwrap_or(fallback)
                .to_string()
        };
        fingerprints.push(FingerprintDayStats {
            fingerprint: fingerprint.to_string(),
            name: text("name", ""),
            status: text("status", "ok"),
            failure: stat.get("failure") == Some(&Value::Bool(true)),
            count: truncated_count(count),
            p50_ms: stat.get("p50Ms").and_then(Value::as_f64).unwrap_or(0.0),
            p95_ms: stat.get("p95Ms").and_then(Value::as_f64).unwrap_or(0.0),
            message: stat
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            sampled: false,
        });
    }
    let mut commits = Vec::new();
    for item in raw
        .get("commits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(commit) = item.as_object() else {
            continue;
        };
        let addressed = string_list(commit.get("addressed"));
        // A day sealed before claimless commits were skipped still holds them.
        if addressed.is_empty() {
            continue;
        }
        commits.push(RefinementCommit {
            at: commit
                .get("at")
                .and_then(Value::as_str)
                .unwrap_or(day)
                .to_string(),
            proposal_id: commit
                .get("proposalId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            addressed,
        });
    }
    Some(LearningDay {
        day: day.to_string(),
        sealed_at: raw
            .get("sealedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        turns: raw
            .get("turns")
            .and_then(Value::as_f64)
            .map_or(0, truncated_count),
        fingerprints,
        commits,
        parse_errors: raw
            .get("parseErrors")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        source_files: string_list(raw.get("sourceFiles")),
    })
}

/// `Math.max(0, Math.trunc(value))` as a count.
fn truncated_count(value: f64) -> u64 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative count; `as` saturates past u64::MAX"
    )]
    let count = value.trunc() as u64;
    count
}

/// Every sealed day in `index_dir`, oldest first; unreadable or malformed
/// files are skipped (the raw log can always be re-sealed).
#[must_use]
pub fn read_learning_index(index_dir: &Path) -> Vec<LearningDay> {
    let Ok(entries) = std::fs::read_dir(index_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.strip_suffix(".json").is_some_and(is_day_key))
        .collect();
    names.sort();
    let mut days: Vec<LearningDay> = names
        .iter()
        .filter_map(|name| {
            let text = std::fs::read_to_string(index_dir.join(name)).ok()?;
            normalize_day(&serde_json::from_str(&text).ok()?)
        })
        .collect();
    days.sort_by(|left, right| left.day.cmp(&right.day));
    days
}

/// Write one day atomically (temp file + rename), `0600` in a `0700`
/// directory; answers the file written.
///
/// # Errors
///
/// [`LearningError::Write`] when the directory or the file cannot be made.
pub fn write_learning_day(index_dir: &Path, day: &LearningDay) -> Result<PathBuf, LearningError> {
    let write_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| LearningError::Write { path, source }
    };
    crate::fs::create_private_dir(index_dir).map_err(write_error(index_dir))?;
    let target = index_dir.join(format!("{}.json", day.day));
    let temp = index_dir.join(format!("{}.json.{}.tmp", day.day, std::process::id()));
    let content = format!("{}\n", stringify_pretty(&day.to_json()));
    let written = crate::fs::write_private_file(&temp, content.as_bytes())
        .and_then(|()| std::fs::rename(&temp, &target));
    if let Err(source) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(write_error(&target)(source));
    }
    Ok(target)
}

/// What one seal did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SealResult {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
    /// Days still accumulating (the current UTC day), deliberately unsealed.
    pub open: Vec<String>,
    pub parse_errors: u64,
}

impl SealResult {
    pub(crate) fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("written".into(), Value::from(self.written.clone()));
        out.insert("skipped".into(), Value::from(self.skipped.clone()));
        out.insert("open".into(), Value::from(self.open.clone()));
        out.insert("parseErrors".into(), Value::from(self.parse_errors));
        Value::Object(out)
    }
}

/// Seal every complete day found in `files` into `index_dir` (TS
/// `sealLearningDays`). The current UTC day stays open because more lines
/// will land in it; an already sealed day is kept unless `force`.
///
/// # Errors
///
/// A read error of [`roll_up_learning_days`] or a write error of
/// [`write_learning_day`].
pub fn seal_learning_days(
    files: &[PathBuf],
    index_dir: &Path,
    now_ms: u64,
    force: bool,
) -> Result<SealResult, LearningError> {
    let today = pa_ledger::iso_from_millis(now_ms)[..10].to_string();
    let rolled = roll_up_learning_days(files, now_ms)?;
    let mut result = SealResult {
        parse_errors: rolled.parse_errors,
        ..SealResult::default()
    };
    for day in rolled.days {
        if day.day >= today {
            result.open.push(day.day);
            continue;
        }
        if !force && index_dir.join(format!("{}.json", day.day)).exists() {
            result.skipped.push(day.day);
            continue;
        }
        write_learning_day(index_dir, &day)?;
        result.written.push(day.day);
    }
    Ok(result)
}
