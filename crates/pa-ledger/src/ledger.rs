//! The failure ledger (TS `ravo/failure-ledger.ts`): fingerprinted runtime
//! failures with their occurrence counts, the recurring-and-actionable set
//! that triggers a deterministic refine, and the durable observation ordinal
//! trust windows are measured on.
//!
//! Everything here is pure. Persistence is [`crate::harness`]; observing a
//! session is [`crate::extract`] and [`crate::feature`].

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::fingerprint::{failure_opponent_id, FailureFingerprint, FailureKind};
use crate::js::{collapse_js_whitespace, js_len, js_prefix, js_trim, JS_WHITESPACE_CLASS};
use crate::replay::{
    merge_replay_case, normalize_replay_cases, replay_probe_of, verified_replay_cases, ReplayCase,
};

/// Occurrences at which an actionable fingerprint recurs.
pub const DEFAULT_RECURRENCE_THRESHOLD: u64 = 2;
/// Records listed by [`format_failure_ledger_for_prompt`] by default.
pub const DEFAULT_PROMPT_LIMIT: usize = 12;
const MAX_PROMPT_EXCERPT_LENGTH: usize = 240;

/// One observed occurrence of a failure. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureObservation {
    pub fingerprint: FailureFingerprint,
    pub excerpt: String,
    /// Index of the message on the session's branch.
    pub entry_index: u64,
    /// Assistant turns on the branch when it was observed.
    pub turn: u64,
    /// ISO timestamp of the observation.
    pub at: String,
    /// Derived reproduction; never verified at observation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_case: Option<ReplayCase>,
}

/// A fingerprint's accumulated record. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureRecord {
    pub fingerprint: FailureFingerprint,
    pub count: u64,
    pub first_seen_turn: u64,
    pub last_seen_turn: u64,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub excerpt: String,
    pub addressed_by_proposal_ids: Vec<String>,
    /// Distinct valid-probe reproductions, oldest first; only the verified
    /// ones are evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_cases: Vec<ReplayCase>,
    /// Occurrences that classified non-actionable; `None` when the stored
    /// record omits the tally (then the excerpt decides one occurrence, as
    /// in TS `nonActionableCountOf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_actionable_count: Option<u64>,
}

/// The ledger as stored under the harness state's `failures` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureLedger {
    /// Always 1.
    pub schema: u64,
    /// Records by fingerprint id, in insertion order.
    pub failures: IndexMap<String, FailureRecord>,
    /// The scan cursor into the owning session's branch.
    pub last_scanned_entry_index: u64,
}

impl Default for FailureLedger {
    fn default() -> Self {
        Self {
            schema: 1,
            failures: IndexMap::new(),
            last_scanned_entry_index: 0,
        }
    }
}

impl FailureLedger {
    /// The ledger as the JSON value the harness state stores.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A provisional champion whose claimed fingerprints recurred inside its
/// observation window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvisionalRegression {
    pub champion_id: String,
    pub fingerprints: Vec<String>,
    pub committed_turn: Number,
    pub until_turn: Number,
}

/// A replay case that reproduced its recorded exception when the self-check
/// ran it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayVerification {
    pub fingerprint_id: String,
    pub source: String,
    pub verified_at: String,
}

/// The ledger's records changed by one update, and the ones it moved into
/// the recurring set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerUpdate {
    pub ledger: FailureLedger,
    /// Records at or over the threshold and actionable after the update but
    /// not before it, most frequent first.
    pub newly_recurring: Vec<FailureRecord>,
}

/// `Number.isSafeInteger(value) ? value : None`.
fn safe_integer(value: Option<&Value>) -> Option<i64> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    let number = value?.as_number()?;
    if let Some(int) = number.as_i64() {
        return (int.unsigned_abs() <= 9_007_199_254_740_991).then_some(int);
    }
    let float = number.as_f64()?;
    #[allow(clippy::cast_possible_truncation)]
    (float.fract() == 0.0 && float.abs() <= MAX_SAFE).then_some(float as i64)
}

fn non_negative(value: Option<&Value>) -> u64 {
    safe_integer(value).map_or(0, |int| u64::try_from(int.max(0)).unwrap_or(0))
}

/// Read a stored ledger leniently: anything malformed degrades to empty,
/// a malformed record is dropped, legacy fields are folded in.
#[must_use]
pub fn normalize_failure_ledger(value: &Value) -> FailureLedger {
    let mut ledger = FailureLedger::default();
    let Some(raw) = value.as_object() else {
        return ledger;
    };
    ledger.last_scanned_entry_index = non_negative(raw.get("lastScannedEntryIndex"));
    let Some(failures) = raw.get("failures").and_then(Value::as_object) else {
        return ledger;
    };
    for (id, raw_record) in failures {
        if let Some(record) = normalize_failure_record(id, raw_record) {
            ledger.failures.insert(id.clone(), record);
        }
    }
    ledger
}

fn normalize_failure_record(id: &str, value: &Value) -> Option<FailureRecord> {
    let raw = value.as_object()?;
    let raw_fingerprint = raw.get("fingerprint")?.as_object()?;
    let kind = FailureKind::from_wire(raw_fingerprint.get("kind")?.as_str()?)?;
    let message = raw_fingerprint.get("message")?.as_str()?.to_string();
    let string = |map: &Map<String, Value>, key: &str| {
        map.get(key).and_then(Value::as_str).map(str::to_string)
    };
    let fingerprint = FailureFingerprint {
        id: string(raw_fingerprint, "id")
            .filter(|stored| !stored.is_empty())
            .unwrap_or_else(|| id.to_string()),
        kind,
        message,
        source: string(raw_fingerprint, "source"),
        exception_class: string(raw_fingerprint, "exceptionClass"),
    };
    let count = non_negative(raw.get("count"));
    let excerpt = string(raw, "excerpt").unwrap_or_default();
    let tally = stored_non_actionable_count(
        &fingerprint,
        &excerpt,
        count,
        raw.get("nonActionableCount"),
        raw.get("nonActionable"),
    );
    Some(FailureRecord {
        replay_cases: normalize_replay_cases(raw.get("replayCases"), raw.get("replayCase")),
        count,
        first_seen_turn: non_negative(raw.get("firstSeenTurn")),
        last_seen_turn: non_negative(raw.get("lastSeenTurn")),
        first_seen_at: string(raw, "firstSeenAt").unwrap_or_default(),
        last_seen_at: string(raw, "lastSeenAt").unwrap_or_default(),
        addressed_by_proposal_ids: raw
            .get("addressedByProposalIds")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        non_actionable_count: (tally > 0).then_some(tally),
        excerpt,
        fingerprint,
    })
}

/// TS `nonActionableCountOf` over a stored record's raw fields.
fn stored_non_actionable_count(
    fingerprint: &FailureFingerprint,
    excerpt: &str,
    count: u64,
    tally: Option<&Value>,
    legacy: Option<&Value>,
) -> u64 {
    if count == 0 {
        return 0;
    }
    if classifies_non_actionable(fingerprint, "") {
        return count;
    }
    if let Some(stored) = safe_integer(tally) {
        return count.min(u64::try_from(stored.max(0)).unwrap_or(0));
    }
    if legacy == Some(&Value::Bool(true)) {
        return count;
    }
    u64::from(classifies_non_actionable(fingerprint, excerpt))
}

impl FailureRecord {
    /// Occurrences of this record that classified non-actionable.
    #[must_use]
    pub fn non_actionable_occurrences(&self) -> u64 {
        if self.count == 0 {
            return 0;
        }
        if classifies_non_actionable(&self.fingerprint, "") {
            return self.count;
        }
        match self.non_actionable_count {
            Some(tally) => self.count.min(tally),
            None => u64::from(classifies_non_actionable(&self.fingerprint, &self.excerpt)),
        }
    }

    /// Whether a harness edit could plausibly prevent this failure: unless a
    /// strict majority of its occurrences classified non-actionable.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        self.non_actionable_occurrences() * 2 <= self.count
    }

    /// The record's replay evidence: verified valid-probe cases, oldest first.
    #[must_use]
    pub fn verified_replay_cases(&self) -> Vec<&ReplayCase> {
        verified_replay_cases(&self.replay_cases)
    }

    /// The record with its tally normalized and its cases pruned to valid
    /// probes (every write of a record goes through this).
    fn normalized(&self) -> Self {
        let tally = self.non_actionable_occurrences();
        Self {
            replay_cases: self
                .replay_cases
                .iter()
                .filter(|replay| replay_probe_of(&replay.source).is_some())
                .cloned()
                .collect(),
            non_actionable_count: (tally > 0).then_some(tally),
            ..self.clone()
        }
    }
}

impl FailureObservation {
    /// Whether this one occurrence is something a harness edit could prevent.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !classifies_non_actionable(&self.fingerprint, &self.excerpt)
    }
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("invalid built-in pattern {pattern}: {error}"))
}

/// Outside the harness whatever the failure kind: aborts, denials, a dead
/// kernel, the network, timeouts, a lock.
static NON_ACTIONABLE_ANY_KIND: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r"(?-u:\b)(?:request|operation) was aborted(?-u:\b)",
        r"|(?-u:\b)was not approved(?-u:\b)",
        r"|(?-u:\b)(?:denied|declined|rejected) by (?:the )?user(?-u:\b)",
        r"|(?-u:\b)kernel has been shut ?down(?-u:\b)",
        r"|(?-u:\b)fetch failed(?-u:\b)",
        r"|(?-u:\b)fetch http (?:[0-9]{3}|#)(?:[^A-Za-z0-9_]|$)",
        r"|(?-u:\b)(?:enotfound|eai_again|econnrefused|econnreset|etimedout|ehostunreach|enetunreach)(?-u:\b)",
        r"|(?-u:\b)name or service not known(?-u:\b)",
        r"|(?-u:\b)socket hang up(?-u:\b)",
        r"|(?-u:\b)timed out(?-u:\b)",
        r"|(?-u:\b)database is locked(?-u:\b)",
    ))
});

/// Provider-side conditions: capacity, transport, refusal, and empty completions.
static NON_ACTIONABLE_PROVIDER: LazyLock<Regex> = LazyLock::new(|| {
    let s = format!("[{JS_WHITESPACE_CLASS}]");
    let sep = format!("[{JS_WHITESPACE_CLASS}_-]*");
    regex(
        &[
            format!(r"(?-u:\b)rate{sep}limit"),
            r"(?-u:\b)too many requests(?-u:\b)".to_string(),
            r"(?-u:\b)429(?-u:\b)".to_string(),
            r"(?-u:\b)overloaded".to_string(),
            r"(?-u:\b)(?:over|at|insufficient) capacity(?-u:\b)".to_string(),
            r"(?-u:\b)capacity (?:exceeded|constraint)".to_string(),
            format!(r"(?-u:\b)service{sep}unavailable"),
            format!(r"(?-u:\b)internal{sep}server{sep}(?:error|exception)"),
            r"(?-u:\b)bad gateway(?-u:\b)".to_string(),
            format!(r"(?-u:\b)gateway{sep}time-?out(?-u:\b)"),
            format!(r"(?-u:\b)(?:http|status(?: code)?)(?:{s}|[:=])*5[0-9][0-9](?-u:\b)"),
            r"(?-u:\b)terminated(?-u:\b)".to_string(),
            r"(?-u:\b)connection error(?-u:\b)".to_string(),
            r"(?-u:\b)refus(?:ed|al)(?-u:\b)".to_string(),
            format!(r"(?-u:\b)content{sep}filter"),
            r"(?-u:\b)empty (?:completion|response)(?-u:\b)".to_string(),
        ]
        .join("|"),
    )
});

/// Bare exception classes that are timeouts or transport failures.
const NON_ACTIONABLE_EXCEPTION_CLASSES: [&str; 2] = ["TimeoutError", "ConnectError"];

/// Whether one occurrence is outside what a harness edit could prevent.
/// Reads the raw excerpt as well as the normalized message: some providers
/// put the cause only in a body the normalization erases.
fn classifies_non_actionable(fingerprint: &FailureFingerprint, excerpt: &str) -> bool {
    if let Some(class) = &fingerprint.exception_class {
        let bare = class.rsplit('.').next().unwrap_or(class);
        if NON_ACTIONABLE_EXCEPTION_CLASSES.contains(&bare) {
            return true;
        }
    }
    let text = format!("{}\n{excerpt}", fingerprint.message).to_lowercase();
    if NON_ACTIONABLE_ANY_KIND.is_match(&text) {
        return true;
    }
    fingerprint.kind == FailureKind::ProviderError && NON_ACTIONABLE_PROVIDER.is_match(&text)
}

fn is_recurring(record: &FailureRecord, threshold: u64) -> bool {
    record.count >= threshold && record.is_actionable()
}

fn sort_records(records: &mut [FailureRecord]) {
    records.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.last_seen_turn.cmp(&a.last_seen_turn))
            .then_with(|| a.fingerprint.id.cmp(&b.fingerprint.id))
    });
}

/// Fold observations into a ledger. The cursor advances past every
/// observation and to `scanned_through_entry_index` when given.
#[must_use]
pub fn update_failure_ledger(
    ledger: &FailureLedger,
    observations: &[FailureObservation],
    threshold: Option<u64>,
    scanned_through_entry_index: Option<u64>,
) -> LedgerUpdate {
    let threshold = threshold.unwrap_or(DEFAULT_RECURRENCE_THRESHOLD).max(1);
    let mut failures: IndexMap<String, FailureRecord> = ledger
        .failures
        .iter()
        .map(|(id, record)| (id.clone(), record.normalized()))
        .collect();
    let mut recurring_before: IndexMap<String, bool> = IndexMap::new();
    let mut last_scanned = ledger.last_scanned_entry_index;
    for observation in observations {
        let id = &observation.fingerprint.id;
        if !recurring_before.contains_key(id) {
            let before = failures
                .get(id)
                .is_some_and(|record| is_recurring(record, threshold));
            recurring_before.insert(id.clone(), before);
        }
        let non_actionable = !observation.is_actionable();
        if let Some(existing) = failures.get_mut(id) {
            existing.count += 1;
            existing.last_seen_turn = existing.last_seen_turn.max(observation.turn);
            existing.last_seen_at.clone_from(&observation.at);
            existing.excerpt.clone_from(&observation.excerpt);
            if non_actionable {
                existing.non_actionable_count =
                    Some(existing.non_actionable_count.unwrap_or(0) + 1);
            }
            if let Some(replay) = &observation.replay_case {
                existing.replay_cases = merge_replay_case(&existing.replay_cases, replay.clone());
            }
        } else {
            let replay_cases = observation
                .replay_case
                .iter()
                .filter(|replay| replay_probe_of(&replay.source).is_some())
                .cloned()
                .collect();
            failures.insert(
                id.clone(),
                FailureRecord {
                    fingerprint: observation.fingerprint.clone(),
                    count: 1,
                    first_seen_turn: observation.turn,
                    last_seen_turn: observation.turn,
                    first_seen_at: observation.at.clone(),
                    last_seen_at: observation.at.clone(),
                    excerpt: observation.excerpt.clone(),
                    addressed_by_proposal_ids: Vec::new(),
                    replay_cases,
                    non_actionable_count: non_actionable.then_some(1),
                },
            );
        }
        last_scanned = last_scanned.max(observation.entry_index + 1);
    }
    if let Some(through) = scanned_through_entry_index {
        last_scanned = last_scanned.max(through);
    }
    let mut newly_recurring: Vec<FailureRecord> = recurring_before
        .iter()
        .filter(|(id, before)| {
            !**before
                && failures
                    .get(*id)
                    .is_some_and(|r| is_recurring(r, threshold))
        })
        .filter_map(|(id, _)| failures.get(id).cloned())
        .collect();
    sort_records(&mut newly_recurring);
    LedgerUpdate {
        ledger: FailureLedger {
            schema: 1,
            failures,
            last_scanned_entry_index: last_scanned,
        },
        newly_recurring,
    }
}

/// Fold observations into a ledger that spans sessions: counts add exactly
/// as in [`update_failure_ledger`], but the scan cursor stays the ledger's
/// own. Callers hold the harness state lock around the read-modify-write.
#[must_use]
pub fn merge_failure_observations(
    ledger: &FailureLedger,
    observations: &[FailureObservation],
    threshold: Option<u64>,
) -> LedgerUpdate {
    let mut update = update_failure_ledger(ledger, observations, threshold, None);
    update.ledger.last_scanned_entry_index = ledger.last_scanned_entry_index;
    update
}

/// The durable, monotone observation ordinal: the ledger's occurrence total.
#[must_use]
pub fn observation_ordinal(ledger: Option<&FailureLedger>) -> u64 {
    ledger.map_or(0, |ledger| {
        ledger.failures.values().map(|record| record.count).sum()
    })
}

/// Actionable failures at or over the threshold, most frequent first: the
/// refine triggers and the gate's failure opponents.
#[must_use]
pub fn recurring_failures(ledger: &FailureLedger, threshold: Option<u64>) -> Vec<FailureRecord> {
    let threshold = threshold.unwrap_or(DEFAULT_RECURRENCE_THRESHOLD);
    let mut records: Vec<FailureRecord> = ledger
        .failures
        .values()
        .filter(|record| is_recurring(record, threshold))
        .cloned()
        .collect();
    sort_records(&mut records);
    records
}

/// One line (plus excerpt, plus the newest verified replay case) per record.
#[must_use]
pub fn format_failure_ledger_for_prompt(records: &[FailureRecord], limit: usize) -> String {
    if records.is_empty() {
        return "None.".to_string();
    }
    let mut lines: Vec<String> = records
        .iter()
        .take(limit)
        .map(|record| {
            let fp = &record.fingerprint;
            let mut parts = vec![format!(
                "- {} [{}]",
                failure_opponent_id(&fp.id),
                fp.kind.as_str()
            )];
            if let Some(source) = fp.source.as_deref().filter(|source| !source.is_empty()) {
                parts.push(format!("source={source}"));
            }
            if let Some(class) = fp
                .exception_class
                .as_deref()
                .filter(|class| !class.is_empty())
            {
                parts.push(format!("class={class}"));
            }
            parts.push(format!("count={}", record.count));
            parts.push(format!(
                "turns={}..{}",
                record.first_seen_turn, record.last_seen_turn
            ));
            let replay = record.verified_replay_cases().last().copied();
            if replay.is_some() {
                parts.push("replay=verified".to_string());
            }
            let collapsed = collapse_js_whitespace(&record.excerpt);
            let excerpt = js_trim(&collapsed);
            let excerpt = if js_len(excerpt) > MAX_PROMPT_EXCERPT_LENGTH {
                format!("{}...", js_prefix(excerpt, MAX_PROMPT_EXCERPT_LENGTH))
            } else {
                excerpt.to_string()
            };
            let mut block = format!("{}\n  {excerpt}", parts.join(" "));
            if let Some(replay) = replay {
                block.push_str("\n  replay case (re-run to check the fix): ");
                block.push_str(&replay.source.replace('\n', "; "));
            }
            block
        })
        .collect();
    if records.len() > limit {
        lines.push(format!("- ... {} more", records.len() - limit));
    }
    lines.join("\n")
}

/// The planner instructions of a refine triggered by recurrence.
#[must_use]
pub fn format_recurrence_refine_instructions(records: &[FailureRecord]) -> String {
    [
        "Automatic refine triggered by recurrence: the failure fingerprints below recurred (count >= threshold).",
        "Your edits must target the cause: fix the skill, prompt note, or memory that lets the failure repeat. The evaluator decides which fingerprints your edits address; a proposal that addresses none of them is rejected.",
        "A fingerprint marked replay=verified is re-checked by re-running its replay case when a skill you write imports the module that case probes, and every claimed fix is checked by whether the failure recurs afterwards.",
        "Do not propose unrelated or speculative edits. Do not promote anything global unless explicitly requested.",
        "Recurring failures:",
        &format_failure_ledger_for_prompt(records, DEFAULT_PROMPT_LIMIT),
    ]
    .join("\n")
}

/// The planner instructions of a refine triggered by a provisional regression.
#[must_use]
pub fn format_regression_refine_instructions(
    regressions: &[ProvisionalRegression],
    records: &[FailureRecord],
) -> String {
    let mut lines = vec![
        "Automatic refine triggered by regression: a provisional refinement claimed to address failure fingerprints, but they recurred inside its observation window. The judge said pass; the outcome says fail.".to_string(),
        "Propose a repair: correct or replace the committed edits so the fingerprints below stop recurring. The evaluator decides which fingerprints the repair addresses, so the edits must target the cause. This repair goes through the normal gate; never bypass it.".to_string(),
        "Regressed provisional refinements:".to_string(),
    ];
    lines.extend(regressions.iter().map(|regression| {
        format!(
            "- proposal {} (observation window {}..{}) claimed to address: {}",
            regression.champion_id,
            regression.committed_turn,
            regression.until_turn,
            regression
                .fingerprints
                .iter()
                .map(|id| failure_opponent_id(id))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }));
    lines.push("Recurred failures:".to_string());
    lines.push(format_failure_ledger_for_prompt(
        records,
        DEFAULT_PROMPT_LIMIT,
    ));
    lines.join("\n")
}

/// Provisional champions in a stored RAVO state (`harness_state.json`'s
/// `ravo` value) whose claimed fingerprints recurred at `ordinal` inside
/// their window. Only windows stamped with `clock` (`"ordinal"` or
/// `"local-ordinal"`) are examined; the lineage is read defensively.
#[must_use]
pub fn find_provisional_regressions(
    ravo: Option<&Value>,
    recurred_fingerprint_ids: &[String],
    ordinal: u64,
    clock: &str,
) -> Vec<ProvisionalRegression> {
    let Some(lineage) = ravo
        .and_then(|ravo| ravo.get("lineage"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    if recurred_fingerprint_ids.is_empty() {
        return Vec::new();
    }
    let recurred: HashSet<&str> = recurred_fingerprint_ids
        .iter()
        .map(String::as_str)
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let position = ordinal as f64;
    let mut regressions = Vec::new();
    for champion in lineage {
        let Some(champion_id) = champion.get("proposalId").and_then(Value::as_str) else {
            continue;
        };
        let Some(window) = champion.get("provisional").and_then(Value::as_object) else {
            continue;
        };
        if window.get("clock").and_then(Value::as_str) != Some(clock) {
            continue;
        }
        let (Some(Value::Number(committed)), Some(Value::Number(until))) =
            (window.get("committedTurn"), window.get("untilTurn"))
        else {
            continue;
        };
        let (Some(start), Some(end)) = (committed.as_f64(), until.as_f64()) else {
            continue;
        };
        if position < start || position > end {
            continue;
        }
        let mut fingerprints: Vec<String> = champion
            .get("claimedFingerprints")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .filter(|id| recurred.contains(id))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        fingerprints.sort();
        fingerprints.dedup();
        if fingerprints.is_empty() {
            continue;
        }
        regressions.push(ProvisionalRegression {
            champion_id: champion_id.to_string(),
            fingerprints,
            committed_turn: committed.clone(),
            until_turn: until.clone(),
        });
    }
    regressions
}

/// Record an observed recurrence on the regressed champions of a stored
/// RAVO state, touching neither lineage order nor scores.
#[must_use]
pub fn record_provisional_regressions(
    ravo: &Value,
    regressions: &[ProvisionalRegression],
    turn: u64,
) -> Value {
    if regressions.is_empty() {
        return ravo.clone();
    }
    let Some(lineage) = ravo.get("lineage").and_then(Value::as_array) else {
        return ravo.clone();
    };
    let by_champion: IndexMap<&str, &Vec<String>> = regressions
        .iter()
        .map(|regression| (regression.champion_id.as_str(), &regression.fingerprints))
        .collect();
    let lineage: Vec<Value> = lineage
        .iter()
        .map(|champion| {
            let fingerprints = champion
                .get("proposalId")
                .and_then(Value::as_str)
                .and_then(|id| by_champion.get(id));
            let window = champion.get("provisional").and_then(Value::as_object);
            let (Some(fingerprints), Some(window)) = (fingerprints, window) else {
                return champion.clone();
            };
            if !matches!(
                (window.get("committedTurn"), window.get("untilTurn")),
                (Some(Value::Number(_)), Some(Value::Number(_)))
            ) {
                return champion.clone();
            }
            let mut provisional = window.clone();
            provisional.insert(
                "observedRecurrence".to_string(),
                serde_json::json!({ "turn": turn, "fingerprints": fingerprints }),
            );
            let mut updated = champion.as_object().cloned().unwrap_or_default();
            updated.insert("provisional".to_string(), Value::Object(provisional));
            Value::Object(updated)
        })
        .collect();
    let mut updated = ravo.as_object().cloned().unwrap_or_default();
    updated.insert("lineage".to_string(), Value::Array(lineage));
    Value::Object(updated)
}

/// Mark the matching unverified cases verified. A verification whose record
/// or case is gone is ignored; an already verified case keeps its stamp.
#[must_use]
pub fn apply_replay_verifications(
    ledger: &FailureLedger,
    verifications: &[ReplayVerification],
) -> FailureLedger {
    let mut updated = ledger.clone();
    for verification in verifications {
        let Some(record) = updated.failures.get_mut(&verification.fingerprint_id) else {
            continue;
        };
        let Some(index) = record.replay_cases.iter().position(|replay| {
            replay.source == verification.source && replay.verified_at.is_none()
        }) else {
            continue;
        };
        record.replay_cases[index].verified_at = Some(verification.verified_at.clone());
        record
            .replay_cases
            .retain(|replay| replay_probe_of(&replay.source).is_some());
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_failure;

    fn occurrence(
        kind: FailureKind,
        source: &str,
        excerpt: &str,
        entry_index: u64,
    ) -> FailureObservation {
        FailureObservation {
            fingerprint: fingerprint_failure(kind, Some(source), None, excerpt),
            excerpt: excerpt.to_string(),
            entry_index,
            turn: entry_index,
            at: format!("t{entry_index}"),
            replay_case: None,
        }
    }

    fn at(observation: &FailureObservation, entry_index: u64) -> FailureObservation {
        FailureObservation {
            entry_index,
            ..observation.clone()
        }
    }

    fn reload(ledger: &FailureLedger) -> FailureLedger {
        normalize_failure_ledger(&ledger.to_value())
    }

    fn ids(records: &[FailureRecord]) -> Vec<String> {
        records
            .iter()
            .map(|record| record.fingerprint.id.clone())
            .collect()
    }

    fn update(ledger: &FailureLedger, observations: &[FailureObservation]) -> LedgerUpdate {
        update_failure_ledger(ledger, observations, None, None)
    }

    const PREFIX: &str = "bash: build failed: error: cannot find symbol in module layout config error: cannot find symbol in module layout config error: cannot find symbol in module layout config error: cannot find symbol in module layout config error: cannot find symbol in module layout config ";

    #[test]
    fn non_actionable_failures_count_but_never_recur() {
        let denied = occurrence(
            FailureKind::ToolError,
            "web_fetch",
            "URL fetch was not approved",
            0,
        );
        let flaky = occurrence(FailureKind::ToolError, "flaky", "cannot connect", 1);
        let first = update(&FailureLedger::default(), &[denied.clone(), flaky.clone()]);
        let second = update(&first.ledger, &[at(&denied, 2), at(&flaky, 3)]);
        assert_eq!(second.ledger.failures[&denied.fingerprint.id].count, 2);
        assert_eq!(observation_ordinal(Some(&second.ledger)), 4);
        assert_eq!(
            ids(&second.newly_recurring),
            vec![flaky.fingerprint.id.clone()]
        );
        assert_eq!(
            ids(&recurring_failures(&second.ledger, None)),
            vec![flaky.fingerprint.id]
        );
        assert_eq!(
            merge_failure_observations(&first.ledger, &[at(&denied, 2)], None).newly_recurring,
            Vec::new()
        );
    }

    #[test]
    fn one_timeout_among_nine_fixable_occurrences_keeps_a_fingerprint_actionable() {
        let fixable = occurrence(
            FailureKind::ToolError,
            "bash",
            &format!("{PREFIX} while compiling target; see log"),
            0,
        );
        let timed_out = occurrence(
            FailureKind::ToolError,
            "bash",
            &format!("{PREFIX} while waiting the request timed out"),
            0,
        );
        assert_eq!(timed_out.fingerprint.id, fixable.fingerprint.id);
        assert!(!timed_out.is_actionable() && fixable.is_actionable());
        let id = fixable.fingerprint.id.clone();
        let mut ledger = update(&FailureLedger::default(), &[timed_out]).ledger;
        assert_eq!(ledger.failures[&id].non_actionable_count, Some(1));
        assert_eq!(recurring_failures(&ledger, Some(1)), Vec::new());
        for index in 1..10 {
            ledger = update(&ledger, &[at(&fixable, index)]).ledger;
        }
        assert_eq!(
            (
                ledger.failures[&id].count,
                ledger.failures[&id].non_actionable_count
            ),
            (10, Some(1))
        );
        assert_eq!(ids(&recurring_failures(&ledger, None)), vec![id]);
        assert_eq!(reload(&ledger), ledger);
    }

    #[test]
    fn a_strict_majority_of_non_actionable_occurrences_mutes_a_fingerprint_across_saves_and_merges()
    {
        let rate_limited = occurrence(
            FailureKind::ProviderError,
            "openwebui",
            "OpenWebUI request failed: 429 {\"detail\":\"rate limit exceeded\"}",
            0,
        );
        let not_found = occurrence(
            FailureKind::ProviderError,
            "openwebui",
            "OpenWebUI request failed: 400 {\"detail\":\"model not found\"}",
            1,
        );
        assert_eq!(not_found.fingerprint.id, rate_limited.fingerprint.id);
        let id = rate_limited.fingerprint.id.clone();
        let six: Vec<_> = (0..6).map(|index| at(&rate_limited, index)).collect();
        let ledger = reload(&update(&FailureLedger::default(), &six).ledger);
        let four: Vec<_> = (6..10).map(|index| at(&not_found, index)).collect();
        let merged = merge_failure_observations(&ledger, &four, None);
        let record = &merged.ledger.failures[&id];
        assert_eq!(
            (
                record.count,
                record.non_actionable_count,
                record.excerpt.as_str()
            ),
            (10, Some(6), not_found.excerpt.as_str())
        );
        assert_eq!(merged.newly_recurring, Vec::new());
        assert_eq!(
            recurring_failures(&reload(&merged.ledger), None),
            Vec::new()
        );
        // Five of ten is not a majority.
        let alternating: Vec<_> = (0..10)
            .map(|index| {
                at(
                    if index % 2 == 0 {
                        &rate_limited
                    } else {
                        &not_found
                    },
                    index,
                )
            })
            .collect();
        let even = update(&FailureLedger::default(), &alternating);
        assert_eq!(even.ledger.failures[&id].non_actionable_count, Some(5));
        assert_eq!(
            ids(&recurring_failures(&even.ledger, None)),
            vec![id.clone()]
        );
        // Only ever actionable: no tally at all.
        let actionable = update(
            &FailureLedger::default(),
            &[not_found.clone(), at(&not_found, 2)],
        );
        assert_eq!(actionable.ledger.failures[&id].non_actionable_count, None);
        assert_eq!(ids(&actionable.newly_recurring), vec![id]);
        assert_eq!(reload(&actionable.ledger), actionable.ledger);
    }

    #[test]
    fn a_record_written_before_the_tally_reads_its_legacy_flag_or_its_excerpt() {
        let rate_limited = occurrence(
            FailureKind::ProviderError,
            "openwebui",
            "OpenWebUI request failed: 429 {\"detail\":\"rate limit exceeded\"}",
            0,
        );
        let not_found_excerpt = "OpenWebUI request failed: 400 {\"detail\":\"model not found\"}";
        let id = rate_limited.fingerprint.id.clone();
        let legacy = |count: u64, excerpt: &str, extra: Value| {
            let mut record = serde_json::json!({
                "fingerprint": rate_limited.fingerprint, "count": count, "firstSeenTurn": 1, "lastSeenTurn": 2,
                "firstSeenAt": "t1", "lastSeenAt": "t2", "excerpt": excerpt, "addressedByProposalIds": [],
            });
            for (key, value) in extra.as_object().cloned().unwrap_or_default() {
                record[key] = value;
            }
            normalize_failure_ledger(
                &serde_json::json!({ "schema": 1, "lastScannedEntryIndex": 0, "failures": { id.clone(): record } }),
            )
        };
        assert_eq!(
            legacy(
                5,
                not_found_excerpt,
                serde_json::json!({ "nonActionable": true })
            )
            .failures[&id]
                .non_actionable_count,
            Some(5)
        );
        assert_eq!(
            legacy(3, &rate_limited.excerpt, Value::Null).failures[&id].non_actionable_count,
            Some(1)
        );
        assert_eq!(
            legacy(3, not_found_excerpt, Value::Null).failures[&id].non_actionable_count,
            None
        );
        assert_eq!(
            legacy(
                3,
                not_found_excerpt,
                serde_json::json!({ "nonActionableCount": 9 })
            )
            .failures[&id]
                .non_actionable_count,
            Some(3)
        );
        // Held without a tally: the stored excerpt is read before an observation overwrites it.
        let mut in_memory = legacy(1, &rate_limited.excerpt, Value::Null);
        in_memory.failures[&id].non_actionable_count = None;
        let twice = update(&in_memory, &[at(&rate_limited, 1)]).ledger;
        assert_eq!(
            (
                twice.failures[&id].count,
                twice.failures[&id].non_actionable_count
            ),
            (2, Some(2))
        );
        let not_found = FailureObservation {
            excerpt: not_found_excerpt.to_string(),
            ..at(&rate_limited, 1)
        };
        let then_fixable = update(&in_memory, &[not_found]).ledger;
        assert_eq!(then_fixable.failures[&id].non_actionable_count, Some(1));
        assert_eq!(
            reload(&then_fixable).failures[&id].non_actionable_count,
            Some(1)
        );
    }

    #[test]
    fn a_fingerprint_that_alone_classifies_non_actionable_counts_every_occurrence() {
        for (observed, next) in [
            (
                occurrence(FailureKind::ToolError, "web_fetch", "fetch HTTP 403", 0),
                occurrence(FailureKind::ToolError, "web_fetch", "fetch HTTP 999", 1),
            ),
            (
                occurrence(
                    FailureKind::ProviderError,
                    "anthropic",
                    "429 Too Many Requests",
                    0,
                ),
                occurrence(
                    FailureKind::ProviderError,
                    "anthropic",
                    "429 Too Many Requests",
                    1,
                ),
            ),
            (
                occurrence(
                    FailureKind::ToolError,
                    "web_fetch",
                    "URL fetch was not approved",
                    0,
                ),
                occurrence(
                    FailureKind::ToolError,
                    "web_fetch",
                    "URL fetch was not approved",
                    1,
                ),
            ),
        ] {
            let id = observed.fingerprint.id.clone();
            let stored = |count: u64, tally: Option<u64>| {
                let mut ledger =
                    update(&FailureLedger::default(), std::slice::from_ref(&observed)).ledger;
                let record = &mut ledger.failures[&id];
                record.count = count;
                record.non_actionable_count = tally;
                reload(&ledger)
            };
            let legacy = stored(5, None);
            assert_eq!(legacy.failures[&id].non_actionable_count, Some(5));
            assert_eq!(recurring_failures(&legacy, None), Vec::new());
            let merged = merge_failure_observations(&legacy, &[next], None);
            assert_eq!(merged.newly_recurring, Vec::new());
            assert_eq!(
                reload(&merged.ledger).failures[&id].non_actionable_count,
                Some(6)
            );
            assert_eq!(
                stored(6, Some(2)).failures[&id].non_actionable_count,
                Some(6)
            );
        }
    }

    #[test]
    fn a_record_newly_recurs_when_it_turns_actionable_after_crossing_the_threshold() {
        let fixable = occurrence(
            FailureKind::ToolError,
            "bash",
            &format!("{PREFIX} while compiling target; see log"),
            0,
        );
        let timed_out = occurrence(
            FailureKind::ToolError,
            "bash",
            &format!("{PREFIX} while waiting the request timed out"),
            0,
        );
        let id = fixable.fingerprint.id.clone();
        let mut ledger = FailureLedger::default();
        let mut fired = Vec::new();
        let sequence: Vec<&FailureObservation> = [&timed_out, &timed_out]
            .into_iter()
            .chain(std::iter::repeat_n(&fixable, 20))
            .collect();
        for (index, observed) in sequence.into_iter().enumerate() {
            let updated = update(&reload(&ledger), &[at(observed, index as u64)]);
            if updated
                .newly_recurring
                .iter()
                .any(|record| record.fingerprint.id == id)
            {
                fired.push(index);
            }
            ledger = updated.ledger;
        }
        assert_eq!(fired, vec![3]);
        assert_eq!(
            (
                ledger.failures[&id].count,
                ledger.failures[&id].non_actionable_count
            ),
            (22, Some(2))
        );

        let rate_limited = occurrence(
            FailureKind::ProviderError,
            "openwebui",
            "OpenWebUI request failed: 429 {\"detail\":\"rate limit exceeded\"}",
            0,
        );
        let not_found = occurrence(
            FailureKind::ProviderError,
            "openwebui",
            "OpenWebUI request failed: 400 {\"detail\":\"model not found\"}",
            1,
        );
        let mut global = FailureLedger::default();
        let mut merged_at = Vec::new();
        let sequence: Vec<&FailureObservation> = [&rate_limited, &rate_limited]
            .into_iter()
            .chain(std::iter::repeat_n(&not_found, 10))
            .chain([&rate_limited])
            .collect();
        for (index, observed) in sequence.into_iter().enumerate() {
            let merged =
                merge_failure_observations(&reload(&global), &[at(observed, index as u64)], None);
            if !merged.newly_recurring.is_empty() {
                merged_at.push(index);
            }
            global = merged.ledger;
        }
        assert_eq!(merged_at, vec![3]);
        let muted_by: Vec<_> = (20..30).map(|index| at(&rate_limited, index)).collect();
        let muted = update(&global, &muted_by);
        assert_eq!(
            (
                muted.newly_recurring.len(),
                recurring_failures(&muted.ledger, None).len()
            ),
            (0, 0)
        );
        let rearm_by: Vec<_> = (40..43).map(|index| at(&not_found, index)).collect();
        let rearmed = update(&muted.ledger, &rearm_by);
        assert_eq!(
            ids(&rearmed.newly_recurring),
            vec![rate_limited.fingerprint.id.clone()]
        );
    }
}
