//! The Engineer Trajectory Index (TS `distill/trajectory-index.ts`): a
//! derived diff over the sealed learning days. It buckets them into observed
//! ISO-week windows and labels each failure fingerprint NEW, DROPPED
//! (internalized) or PERSISTS (stable gap), with a new-minus-retired rate.
//!
//! Two honesty guardrails are structural: every label carries a non-empty
//! `confounds` list (`task-mix` always, plus `tool-surface` /
//! `measurement-instrument` when the span crosses a corpus boundary or a data
//! gap), and no label is emitted below a minimum observed-window count.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use serde_json::{Map, Value};

use crate::index::LearningDay;

/// `version` of the store file and `schema` of a window.
pub const TRAJECTORY_STORE_VERSION: u64 = 1;
/// No label below this many observed windows.
pub const DEFAULT_MIN_TRAJECTORY_WINDOWS: u64 = 4;
/// Consecutive absent windows before a recurring fingerprint is internalized.
pub const DEFAULT_TRAJECTORY_INTERNALIZED_GAP: u64 = 2;
/// The persisted window list keeps the newest this many.
pub const DEFAULT_MAX_TRAJECTORY_WINDOWS: usize = 52;
/// The kill switch: on unless `0`, `off`, `false` or `no`.
pub const TRAJECTORY_INDEX_ENV: &str = "PRIME_AGENT_TRAJECTORY_INDEX";
/// The corpus of the machine's own sealed days.
pub const PRIME_CORPUS: &str = "prime";

/// A fingerprint naming a security or credential class: never marked
/// internalized for the prompt, so its reminder always fires.
static SECURITY_CLASS_FINGERPRINT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)auth|credential|secret|token|password|permission[- ]denied|unauthor")
        .expect("the security-class pattern compiles")
});

/// Whether the index is on for a `PRIME_AGENT_TRAJECTORY_INDEX` value.
#[must_use]
pub fn trajectory_index_enabled(value: Option<&str>) -> bool {
    let value = value.map(|value| value.trim().to_lowercase());
    !matches!(value.as_deref(), Some("0" | "off" | "false" | "no"))
}

/// Whether the index is on in this process's environment (read per call).
#[must_use]
pub fn trajectory_index_enabled_from_env() -> bool {
    trajectory_index_enabled(std::env::var(TRAJECTORY_INDEX_ENV).ok().as_deref())
}

/// Whether a fingerprint's name or message names a security class.
#[must_use]
pub fn matches_security_class(name: &str, message: &str) -> bool {
    let text = [name, message]
        .into_iter()
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    !text.is_empty() && SECURITY_CLASS_FINGERPRINT.is_match(&text)
}

// ---------------------------------------------------------------------------
// ISO weeks (ISO-8601 week-year, UTC, Monday start)

const DAY_MS: i64 = 86_400_000;

/// Days since the epoch of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The year of an epoch day.
fn year_of(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i64::from(month <= 2)
}

/// Monday = 0 .. Sunday = 6 of an epoch day (1970-01-01 was a Thursday).
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

/// The ISO week key `YYYY-Www` of an epoch day, using the week-year.
fn iso_week_of_days(days: i64) -> String {
    let thursday = days + (3 - weekday(days));
    let iso_year = year_of(thursday);
    let first_thursday = days_from_civil(iso_year, 1, 4);
    let week1_monday = first_thursday - weekday(first_thursday);
    let week = (thursday - week1_monday).div_euclid(7) + 1;
    format!("{iso_year}-W{week:02}")
}

/// The ISO week key of a `YYYY-MM-DD` day (`None` when it is not one).
#[must_use]
pub fn iso_week(day: &str) -> Option<String> {
    if !crate::index::is_day_key(day) {
        return None;
    }
    let field = |range: std::ops::Range<usize>| day[range].parse::<i64>().ok();
    let (year, month, date) = (field(0..4)?, field(5..7)?, field(8..10)?);
    Some(iso_week_of_days(days_from_civil(year, month, date)))
}

/// The ISO week key of an epoch-millisecond instant.
#[must_use]
pub fn iso_week_of_millis(millis: u64) -> String {
    let millis = i64::try_from(millis).unwrap_or(i64::MAX);
    iso_week_of_days(millis.div_euclid(DAY_MS))
}

/// The epoch day of the Monday that begins a `YYYY-Www` key.
fn iso_week_monday(key: &str) -> Option<i64> {
    let (year, week) = key.split_once("-W")?;
    if year.len() != 4 || week.len() != 2 {
        return None;
    }
    let (year, week) = (year.parse::<i64>().ok()?, week.parse::<i64>().ok()?);
    let jan4 = days_from_civil(year, 1, 4);
    Some(jan4 - weekday(jan4) + (week - 1) * 7)
}

/// Whether `later` is the ISO week right after `earlier`.
fn consecutive_weeks(earlier: &str, later: &str) -> bool {
    match (iso_week_monday(earlier), iso_week_monday(later)) {
        (Some(a), Some(b)) => b - a == 7,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The store's records

/// Per (window, fingerprint) aggregate (failures only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryFingerprintWindow {
    pub fingerprint: String,
    pub name: String,
    pub message: String,
    pub count: u64,
    /// Running total of every failure count through this window.
    pub cumulative_ordinal: u64,
}

/// One sealed window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryWindow {
    pub window: String,
    pub sealed_at: String,
    pub days: Vec<String>,
    pub turns: u64,
    pub corpus: String,
    pub fingerprints: Vec<TrajectoryFingerprintWindow>,
}

/// A label's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrajectoryLabelKind {
    New,
    Dropped,
    Persists,
}

impl TrajectoryLabelKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Dropped => "dropped",
            Self::Persists => "persists",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "new" => Some(Self::New),
            "dropped" => Some(Self::Dropped),
            "persists" => Some(Self::Persists),
            _ => None,
        }
    }
}

/// The emitted diff for one fingerprint across the sealed windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryLabel {
    pub fingerprint: String,
    pub name: String,
    pub message: String,
    pub corpus: String,
    pub label: Option<TrajectoryLabelKind>,
    /// Why a label was withheld; exclusive with a label.
    pub withheld: Option<String>,
    pub since_window: String,
    pub last_window: String,
    pub windows_present: u64,
    pub windows_recurring: u64,
    pub claimed_by_refinement: bool,
    pub domain_active: bool,
    pub security_class: bool,
    /// Never empty: always at least `task-mix`.
    pub confounds: Vec<String>,
}

/// One window's appeared/retired step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryRateWindow {
    pub window: String,
    pub appeared: u64,
    /// `None` across an observed-week gap.
    pub retired: Option<u64>,
    pub new_minus_retired: Option<i64>,
}

/// The store file (`<agentDir>/learning/trajectory.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryStoreFile {
    pub sealed_at: String,
    /// The number of observed prime windows the labels were computed over.
    pub windows_observed: u64,
    pub min_windows: u64,
    pub windows: Vec<TrajectoryWindow>,
    pub labels: Vec<TrajectoryLabel>,
    pub rate: Vec<TrajectoryRateWindow>,
}

impl TrajectoryFingerprintWindow {
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("fingerprint".into(), Value::from(self.fingerprint.as_str()));
        out.insert("name".into(), Value::from(self.name.as_str()));
        out.insert("message".into(), Value::from(self.message.as_str()));
        out.insert("failure".into(), Value::from(true));
        out.insert("count".into(), Value::from(self.count));
        out.insert(
            "cumulativeOrdinal".into(),
            Value::from(self.cumulative_ordinal),
        );
        Value::Object(out)
    }
}

impl TrajectoryWindow {
    pub(crate) fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("schema".into(), Value::from(TRAJECTORY_STORE_VERSION));
        out.insert("window".into(), Value::from(self.window.as_str()));
        out.insert("sealedAt".into(), Value::from(self.sealed_at.as_str()));
        out.insert("days".into(), Value::from(self.days.clone()));
        out.insert("turns".into(), Value::from(self.turns));
        out.insert("corpus".into(), Value::from(self.corpus.as_str()));
        out.insert(
            "fingerprints".into(),
            Value::Array(
                self.fingerprints
                    .iter()
                    .map(TrajectoryFingerprintWindow::to_json)
                    .collect(),
            ),
        );
        Value::Object(out)
    }
}

impl TrajectoryLabel {
    pub(crate) fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("fingerprint".into(), Value::from(self.fingerprint.as_str()));
        out.insert("name".into(), Value::from(self.name.as_str()));
        out.insert("message".into(), Value::from(self.message.as_str()));
        out.insert("corpus".into(), Value::from(self.corpus.as_str()));
        out.insert(
            "label".into(),
            self.label
                .map_or(Value::Null, |label| Value::from(label.as_str())),
        );
        out.insert(
            "sinceWindow".into(),
            Value::from(self.since_window.as_str()),
        );
        out.insert("lastWindow".into(), Value::from(self.last_window.as_str()));
        out.insert("windowsPresent".into(), Value::from(self.windows_present));
        out.insert(
            "windowsRecurring".into(),
            Value::from(self.windows_recurring),
        );
        out.insert(
            "claimedByRefinement".into(),
            Value::from(self.claimed_by_refinement),
        );
        out.insert("domainActive".into(), Value::from(self.domain_active));
        out.insert("securityClass".into(), Value::from(self.security_class));
        out.insert("confounds".into(), Value::from(self.confounds.clone()));
        // `{...base, withheld}` appends the key last.
        if let Some(withheld) = &self.withheld {
            out.insert("withheld".into(), Value::from(withheld.as_str()));
        }
        Value::Object(out)
    }
}

impl TrajectoryRateWindow {
    pub(crate) fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("window".into(), Value::from(self.window.as_str()));
        out.insert("appeared".into(), Value::from(self.appeared));
        out.insert(
            "retired".into(),
            self.retired.map_or(Value::Null, Value::from),
        );
        out.insert(
            "newMinusRetired".into(),
            self.new_minus_retired.map_or(Value::Null, Value::from),
        );
        Value::Object(out)
    }
}

impl TrajectoryStoreFile {
    /// The file as the TS object (key order included).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("version".into(), Value::from(TRAJECTORY_STORE_VERSION));
        out.insert("sealedAt".into(), Value::from(self.sealed_at.as_str()));
        out.insert("windowsObserved".into(), Value::from(self.windows_observed));
        out.insert("minWindows".into(), Value::from(self.min_windows));
        out.insert(
            "windows".into(),
            Value::Array(self.windows.iter().map(TrajectoryWindow::to_json).collect()),
        );
        out.insert(
            "labels".into(),
            Value::Array(self.labels.iter().map(TrajectoryLabel::to_json).collect()),
        );
        out.insert(
            "rate".into(),
            Value::Array(
                self.rate
                    .iter()
                    .map(TrajectoryRateWindow::to_json)
                    .collect(),
            ),
        );
        Value::Object(out)
    }

    /// Only the prime corpus: backfill never reaches the store or the prompt.
    #[must_use]
    pub fn prime_only(&self) -> Self {
        Self {
            windows: self
                .windows
                .iter()
                .filter(|window| window.corpus == PRIME_CORPUS)
                .cloned()
                .collect(),
            labels: self
                .labels
                .iter()
                .filter(|label| label.corpus == PRIME_CORPUS)
                .cloned()
                .collect(),
            ..self.clone()
        }
    }

    /// What the store keeps of a file: prime only, and the newest
    /// [`DEFAULT_MAX_TRAJECTORY_WINDOWS`] windows by week key.
    #[must_use]
    pub fn bounded(&self) -> Self {
        let mut prime = self.prime_only();
        prime
            .windows
            .sort_by(|left, right| pa_ravo::locale_compare(&left.window, &right.window));
        let excess = prime
            .windows
            .len()
            .saturating_sub(DEFAULT_MAX_TRAJECTORY_WINDOWS);
        prime.windows.drain(..excess);
        prime
    }
}

// ---------------------------------------------------------------------------
// Sealing (pure over its inputs)

/// One backfill day and the corpus it came from (`backfill:<name>`).
#[derive(Debug, Clone, PartialEq)]
pub struct CorpusDay {
    pub corpus: String,
    pub day: LearningDay,
}

/// What to seal.
#[derive(Debug, Clone, Default)]
pub struct SealTrajectoryOptions<'a> {
    pub days: &'a [LearningDay],
    /// Cross-tool days (the CLI's `--include-backfill`); never persisted.
    pub backfill_days: &'a [CorpusDay],
    pub min_windows: Option<u64>,
    pub internalized_gap: Option<u64>,
    pub now_ms: u64,
}

struct CorpusContext<'a> {
    current_week: &'a str,
    min_windows: u64,
    internalized_gap: u64,
    sealed_at: &'a str,
    index_mixes_corpora: bool,
}

struct CorpusResult {
    windows: Vec<TrajectoryWindow>,
    labels: Vec<TrajectoryLabel>,
    rate: Vec<TrajectoryRateWindow>,
}

fn stamp_confounds(corpus: &str, index_mixes_corpora: bool, span_has_gap: bool) -> Vec<String> {
    let backfill = corpus.starts_with("backfill:");
    let mut confounds = vec!["task-mix".to_string()];
    if backfill || index_mixes_corpora {
        confounds.push("tool-surface".to_string());
    }
    if backfill || index_mixes_corpora || span_has_gap {
        confounds.push("measurement-instrument".to_string());
    }
    confounds
}

/// Windows, labels and rate of one corpus. Corpora are never summed: each
/// is windowed and labelled on its own.
// The TS `computeCorpus` step by step; its stages share the per-window
// tables, so splitting them would only pass the tables around.
#[allow(clippy::too_many_lines)]
fn compute_corpus(
    corpus: &str,
    days: &[&LearningDay],
    context: &CorpusContext<'_>,
) -> CorpusResult {
    // Bucket sealed days into observed weeks, leaving the current week open.
    let mut by_week: HashMap<String, Vec<&LearningDay>> = HashMap::new();
    let mut addressed: HashSet<&str> = HashSet::new();
    for day in days {
        for commit in &day.commits {
            addressed.extend(commit.addressed.iter().map(String::as_str));
        }
        let Some(week) = iso_week(&day.day) else {
            continue;
        };
        if week == context.current_week {
            continue;
        }
        by_week.entry(week).or_default().push(day);
    }
    let mut ordered_weeks: Vec<String> = by_week.keys().cloned().collect();
    ordered_weeks.sort();
    let observed = ordered_weeks.len();

    let mut windows: Vec<TrajectoryWindow> = Vec::new();
    let mut per_window_counts: Vec<HashMap<String, u64>> = Vec::new();
    let mut carried: HashMap<String, (String, String)> = HashMap::new();
    let mut carried_day: HashMap<String, String> = HashMap::new();
    let mut window_turns: Vec<u64> = Vec::new();
    let mut cumulative = 0u64;
    for week in &ordered_weeks {
        let mut day_list = by_week.get(week).cloned().unwrap_or_default();
        day_list.sort_by(|left, right| pa_ravo::locale_compare(&left.day, &right.day));
        let mut counts: indexmap::IndexMap<String, u64> = indexmap::IndexMap::new();
        let mut window_failure_total = 0u64;
        let mut turns = 0u64;
        for day in &day_list {
            turns += day.turns;
            for stat in day.fingerprints.iter().filter(|stat| stat.failure) {
                *counts.entry(stat.fingerprint.clone()).or_insert(0) += stat.count;
                window_failure_total += stat.count;
                // Name and message come from the newest day naming it.
                let newer = carried_day
                    .get(&stat.fingerprint)
                    .is_none_or(|seen| day.day.as_str() >= seen.as_str());
                if newer {
                    carried.insert(
                        stat.fingerprint.clone(),
                        (stat.name.clone(), stat.message.clone().unwrap_or_default()),
                    );
                    carried_day.insert(stat.fingerprint.clone(), day.day.clone());
                }
            }
        }
        cumulative += window_failure_total;
        let mut entries: Vec<(&String, &u64)> = counts.iter().collect();
        entries.sort_by(|(a_id, a_count), (b_id, b_count)| {
            b_count
                .cmp(a_count)
                .then_with(|| pa_ravo::locale_compare(a_id, b_id))
        });
        let fingerprints = entries
            .into_iter()
            .map(|(fingerprint, count)| {
                let (name, message) = carried.get(fingerprint).cloned().unwrap_or_default();
                TrajectoryFingerprintWindow {
                    fingerprint: fingerprint.clone(),
                    name,
                    message,
                    count: *count,
                    cumulative_ordinal: cumulative,
                }
            })
            .collect();
        windows.push(TrajectoryWindow {
            window: week.clone(),
            sealed_at: context.sealed_at.to_string(),
            days: day_list.iter().map(|day| day.day.clone()).collect(),
            turns,
            corpus: corpus.to_string(),
            fingerprints,
        });
        per_window_counts.push(counts.into_iter().collect());
        window_turns.push(turns);
    }

    // Adjacent-window gaps, so a span crossing one is flagged.
    let gap_before: Vec<bool> = ordered_weeks
        .iter()
        .enumerate()
        .map(|(index, week)| index > 0 && !consecutive_weeks(&ordered_weeks[index - 1], week))
        .collect();
    let all_fingerprints: BTreeSet<&String> =
        per_window_counts.iter().flat_map(HashMap::keys).collect();
    let gap = usize::try_from(context.internalized_gap).unwrap_or(usize::MAX);
    let recent_turns: u64 = window_turns
        .iter()
        .skip(window_turns.len().saturating_sub(gap))
        .sum();
    let domain_active = recent_turns > 0;

    let mut labels = Vec::new();
    for fingerprint in all_fingerprints {
        let presence: Vec<u64> = per_window_counts
            .iter()
            .map(|counts| counts.get(fingerprint).copied().unwrap_or(0))
            .collect();
        let present: Vec<usize> = presence
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(index, _)| index)
            .collect();
        let (Some(&first), Some(&last)) = (present.first(), present.last()) else {
            continue;
        };
        let windows_present = present.len() as u64;
        let windows_recurring = presence
            .iter()
            .filter(|count| **count >= pa_ledger::DEFAULT_RECURRENCE_THRESHOLD)
            .count() as u64;
        let (name, message) = carried.get(fingerprint).cloned().unwrap_or_default();
        let claimed_by_refinement = addressed.contains(fingerprint.as_str());
        let security_class = matches_security_class(&name, &message);
        let span_has_gap = gap_before
            .iter()
            .enumerate()
            .any(|(index, gap)| *gap && index > first && index <= last);
        let mut label = TrajectoryLabel {
            fingerprint: fingerprint.clone(),
            name,
            message,
            corpus: corpus.to_string(),
            label: None,
            withheld: None,
            since_window: ordered_weeks[first].clone(),
            last_window: ordered_weeks[last].clone(),
            windows_present,
            windows_recurring,
            claimed_by_refinement,
            domain_active,
            security_class,
            confounds: stamp_confounds(corpus, context.index_mixes_corpora, span_has_gap),
        };
        let observed_count = observed as u64;
        if observed_count < context.min_windows {
            label.withheld = Some(format!(
                "fewer than {} observed windows ({observed})",
                context.min_windows
            ));
        } else if windows_recurring >= 1 && last + gap < observed {
            // Recurred, then gone for the last `gap` windows.
            if claimed_by_refinement {
                label.withheld = Some("absent but a committed refinement claimed it".to_string());
            } else if !domain_active {
                label.withheld = Some("absent but domain inactive (task-mix)".to_string());
            } else {
                label.label = Some(TrajectoryLabelKind::Dropped);
            }
        } else if first + 1 == observed {
            label.label = Some(TrajectoryLabelKind::New);
        } else if windows_recurring * 2 > observed_count {
            label.label = Some(TrajectoryLabelKind::Persists);
        }
        labels.push(label);
    }

    let rate = compute_rate(&ordered_weeks, &per_window_counts, &gap_before);
    CorpusResult {
        windows,
        labels,
        rate,
    }
}

fn compute_rate(
    ordered_weeks: &[String],
    per_window_counts: &[HashMap<String, u64>],
    gap_before: &[bool],
) -> Vec<TrajectoryRateWindow> {
    let mut rate = Vec::new();
    for (index, week) in ordered_weeks.iter().enumerate() {
        let here = &per_window_counts[index];
        // First appearance: absent from every earlier window.
        let appeared = here
            .keys()
            .filter(|id| {
                !per_window_counts[..index]
                    .iter()
                    .any(|earlier| earlier.contains_key(*id))
            })
            .count() as u64;
        let step = if index == 0 {
            TrajectoryRateWindow {
                window: week.clone(),
                appeared,
                retired: Some(0),
                new_minus_retired: Some(i64::try_from(appeared).unwrap_or(i64::MAX)),
            }
        } else if gap_before[index] {
            // A gap makes the transition uncountable.
            TrajectoryRateWindow {
                window: week.clone(),
                appeared,
                retired: None,
                new_minus_retired: None,
            }
        } else {
            let retired = per_window_counts[index - 1]
                .keys()
                .filter(|id| !here.contains_key(*id))
                .count() as u64;
            TrajectoryRateWindow {
                window: week.clone(),
                appeared,
                retired: Some(retired),
                new_minus_retired: Some(
                    i64::try_from(appeared).unwrap_or(i64::MAX)
                        - i64::try_from(retired).unwrap_or(i64::MAX),
                ),
            }
        };
        rate.push(step);
    }
    rate
}

/// Compute the trajectory over sealed days (TS `sealTrajectoryWindows`):
/// prime windows drive the labels and rate; backfill corpora, when given,
/// are computed separately with every confound flagged, for the CLI table.
#[must_use]
pub fn seal_trajectory_windows(options: &SealTrajectoryOptions<'_>) -> TrajectoryStoreFile {
    let clamp = |value: Option<u64>, fallback: u64| value.map_or(fallback, |value| value.max(1));
    let min_windows = clamp(options.min_windows, DEFAULT_MIN_TRAJECTORY_WINDOWS);
    let internalized_gap = clamp(
        options.internalized_gap,
        DEFAULT_TRAJECTORY_INTERNALIZED_GAP,
    );
    let sealed_at = pa_ledger::iso_from_millis(options.now_ms);
    let current_week = iso_week_of_millis(options.now_ms);
    let context = CorpusContext {
        current_week: &current_week,
        min_windows,
        internalized_gap,
        sealed_at: &sealed_at,
        index_mixes_corpora: !options.backfill_days.is_empty(),
    };
    let mut by_corpus: std::collections::BTreeMap<&str, Vec<&LearningDay>> =
        std::collections::BTreeMap::new();
    for entry in options.backfill_days {
        by_corpus
            .entry(entry.corpus.as_str())
            .or_default()
            .push(&entry.day);
    }
    let prime_days: Vec<&LearningDay> = options.days.iter().collect();
    let prime = compute_corpus(PRIME_CORPUS, &prime_days, &context);
    let windows_observed = prime.windows.len() as u64;
    let mut windows = prime.windows;
    let mut labels = prime.labels;
    for (corpus, days) in by_corpus {
        let result = compute_corpus(corpus, &days, &context);
        windows.extend(result.windows);
        labels.extend(result.labels);
    }
    TrajectoryStoreFile {
        sealed_at,
        windows_observed,
        min_windows,
        windows,
        labels,
        rate: prime.rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_weeks_use_the_week_year() {
        let weeks = [
            "2025-01-06",
            "2021-01-01",
            "2020-12-28",
            "2019-12-30",
            "2026-08-01",
        ]
        .map(|day| iso_week(day).unwrap());
        assert_eq!(
            weeks,
            ["2025-W02", "2020-W53", "2020-W53", "2020-W01", "2026-W31"]
        );
        assert!(consecutive_weeks("2020-W53", "2021-W01"));
        assert!(!consecutive_weeks("2020-W52", "2021-W01"));
        assert_eq!(iso_week_of_millis(1_741_564_800_000), "2025-W11");
    }

    #[test]
    fn the_kill_switch_is_off_only_for_its_four_words() {
        let off = ["0", "off", "false", "no", "OFF", "  No "]
            .map(|value| trajectory_index_enabled(Some(value)));
        let on = [
            Some("1"),
            Some("on"),
            Some("true"),
            Some("yes"),
            Some(""),
            None,
        ]
        .map(trajectory_index_enabled);
        assert_eq!((off, on), ([false; 6], [true; 6]));
    }
}
