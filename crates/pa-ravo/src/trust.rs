//! Trust and dormancy on continual-harness entries (TS
//! `refinement/harness-trust.ts` and the trust half of `refinement.ts`).
//!
//! Every entry carries a trust score that moves on measured outcomes only:
//! a provisional window that closes with nothing recurring credits the
//! entries its commit wrote `+5`; an upheld referee verdict (a recorded
//! replay case re-executed in a subprocess, the recorded exception
//! recurring) debits the skill entry it ran for `-15`. Below
//! [`DORMANT_TRUST_THRESHOLD`] an entry is dormant: left out of the rendered
//! harness, never deleted, still readable and editable.
//!
//! `trustWindows` records at commit time what a refinement wrote: the
//! `kind:id` of every entry it touched, the fingerprints the gate accepted
//! it as addressing, and the imports each skill it wrote names. A claimed
//! failure that recurs inside the window without an upheld verdict closes
//! it `contested` (no credit, no debit); a replay is evidence only about
//! the skill entry it ran for, and only while that skill still imports what
//! the commit wrote.
//!
//! Formats are the TS objects', key order included: a window keeps the
//! order its keys were added in (JS object spread), so a window that
//! gained evidence and settled in one flush writes its keys as the TS flush
//! did.

use std::collections::HashMap;

use indexmap::IndexMap;
use pa_core::refinement::planner::RefinementEdit;
use pa_core::refinement::{HarnessState, RefinementAction, RefinementKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::js::{js_round, locale_compare, sorted_unique};
use crate::reducer::MAX_SAFE_INTEGER;
use crate::referee::skill_imports_of;

/// The trust an entry without a record has.
pub const DEFAULT_ENTRY_TRUST: i64 = 50;
pub const MIN_ENTRY_TRUST: i64 = 0;
pub const MAX_ENTRY_TRUST: i64 = 100;
/// Credit for a provisional window that closed with nothing recurring.
pub const CLEAN_WINDOW_CREDIT: i64 = 5;
/// Debit for an upheld referee verdict attributable to the entry.
pub const MEASURED_FAULT_DEBIT: i64 = 15;
/// Strictly below this an entry is dormant: readable, but not rendered.
pub const DORMANT_TRUST_THRESHOLD: i64 = 30;
/// Replays recorded per (window, entry, fingerprint) before it is never re-run.
pub const MAX_TRUST_ADJUDICATION_RUNS: usize = 3;

const MAX_TRUST_EVENTS: usize = 20;
const MAX_SETTLED_TRUST_WINDOWS: usize = 100;
const MAX_TRUST_ADJUDICATIONS_PER_WINDOW: usize = 32;
const SKILL_REF_PREFIX: &str = "skill:";

/// The per-entry key of trust bookkeeping.
pub const TRUST_KEY: &str = "trust";
/// The top-level key of the trust windows.
pub const TRUST_WINDOWS_KEY: &str = "trustWindows";

/// Why a score moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustReason {
    CleanWindow,
    MeasuredFault,
}

impl TrustReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CleanWindow => "clean_window",
            Self::MeasuredFault => "measured_fault",
        }
    }
}

/// Where a window stands. `Unmeasured`: it claimed nothing, settled on load
/// without credit. `Contested`: a claimed failure recurred in it without
/// an upheld verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustOutcome {
    Open,
    Clean,
    Contested,
    Faulted,
    Unmeasured,
}

impl TrustOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Clean => "clean",
            Self::Contested => "contested",
            Self::Faulted => "faulted",
            Self::Unmeasured => "unmeasured",
        }
    }

    fn parse(value: Option<&Value>) -> Option<Self> {
        match value?.as_str()? {
            "open" => Some(Self::Open),
            "clean" => Some(Self::Clean),
            "contested" => Some(Self::Contested),
            "faulted" => Some(Self::Faulted),
            "unmeasured" => Some(Self::Unmeasured),
            _ => None,
        }
    }
}

/// A replay verdict recorded on a window: the referee statuses that are
/// replay results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustAdjudicationStatus {
    Upheld,
    Cleared,
    Unverifiable,
}

impl TrustAdjudicationStatus {
    fn rank(self) -> u8 {
        match self {
            Self::Unverifiable => 0,
            Self::Cleared => 1,
            Self::Upheld => 2,
        }
    }

    fn parse(value: Option<&Value>) -> Option<Self> {
        match value?.as_str()? {
            "upheld" => Some(Self::Upheld),
            "cleared" => Some(Self::Cleared),
            "unverifiable" => Some(Self::Unverifiable),
            _ => None,
        }
    }
}

/// One score movement. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEvent {
    pub reason: TrustReason,
    pub delta: i64,
    /// Score after the delta was clamped and applied.
    pub score: i64,
    pub at: String,
    /// The refinement that committed the entry this event is attributed to.
    #[serde(rename = "proposalId")]
    pub proposal_id: String,
    /// The fingerprint whose upheld verdict produced a measured fault.
    #[serde(
        rename = "fingerprintId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub fingerprint_id: Option<String>,
}

/// An entry's trust record (`HarnessEntry.trust`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryTrust {
    pub score: i64,
    pub updated_at: String,
    pub events: Vec<TrustEvent>,
}

/// The replays recorded for one touched skill entry and one claimed
/// fingerprint of a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustAdjudication {
    /// A touched `skill:<id>`.
    pub entry: String,
    pub fingerprint_id: String,
    /// Strongest verdict seen: upheld > cleared > unverifiable.
    pub status: TrustAdjudicationStatus,
    /// Earliest in-window recurrence ordinal that prompted a run.
    pub ordinal: u64,
    /// When each run finished, sorted, at most [`MAX_TRUST_ADJUDICATION_RUNS`].
    pub runs: Vec<String>,
}

/// The optional keys of a window, in the order they were added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowKey {
    SettledTurn,
    FaultedFingerprints,
    FaultedEntries,
    SkillImports,
    Recurrences,
    Adjudications,
}

/// The attribution record for one committed refinement, keyed by proposal
/// id in `trustWindows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustWindow {
    pub proposal_id: String,
    /// `kind:id` of every entry the commit created or updated.
    pub touched: Vec<String>,
    /// Fingerprints the gate accepted the commit as addressing.
    pub claimed_fingerprints: Vec<String>,
    pub committed_turn: u64,
    pub until_turn: u64,
    pub outcome: TrustOutcome,
    pub settled_turn: Option<u64>,
    /// Claimed fingerprints an upheld verdict refuted.
    pub faulted_fingerprints: Option<Vec<String>>,
    /// Skill entries a fault on this window was charged to; absent on a
    /// faulted window, every touched entry counts as charged.
    pub faulted_entries: Option<Vec<String>>,
    /// Touched skill ref -> the imports the commit wrote for it.
    pub skill_imports: Option<IndexMap<String, Vec<String>>>,
    /// Claimed fingerprint -> earliest in-window ordinal it recurred at.
    pub recurrences: Option<IndexMap<String, u64>>,
    pub adjudications: Option<Vec<TrustAdjudication>>,
    /// The optional keys present, in insertion order.
    order: Vec<WindowKey>,
}

/// The windows of one harness state, in key order.
pub type TrustWindows = IndexMap<String, TrustWindow>;

/// Evidence a session observed about a window, recorded at the next flush
/// or apply of its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustWindowEvidence {
    Recurrence {
        proposal_id: String,
        fingerprint_id: String,
        ordinal: u64,
    },
    Adjudication {
        proposal_id: String,
        entry: String,
        fingerprint_id: String,
        status: TrustAdjudicationStatus,
        ordinal: u64,
        at: String,
    },
}

impl TrustWindowEvidence {
    fn proposal_id(&self) -> &str {
        match self {
            Self::Recurrence { proposal_id, .. } | Self::Adjudication { proposal_id, .. } => {
                proposal_id
            }
        }
    }

    fn fingerprint_id(&self) -> &str {
        match self {
            Self::Recurrence { fingerprint_id, .. } | Self::Adjudication { fingerprint_id, .. } => {
                fingerprint_id
            }
        }
    }

    fn ordinal(&self) -> u64 {
        match self {
            Self::Recurrence { ordinal, .. } | Self::Adjudication { ordinal, .. } => *ordinal,
        }
    }

    /// Whether this is a replay verdict (else a recurrence).
    #[must_use]
    pub fn is_adjudication(&self) -> bool {
        matches!(self, Self::Adjudication { .. })
    }
}

/// One score a settlement moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustAdjustment {
    pub kind: String,
    pub id: String,
    pub reason: TrustReason,
    pub delta: i64,
    pub before: i64,
    pub after: i64,
    /// The record stored back on the entry.
    pub trust: EntryTrust,
    pub proposal_id: String,
    pub fingerprint_id: Option<String>,
}

/// One window a settlement closed (or faulted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSettlement {
    pub proposal_id: String,
    pub from: TrustOutcome,
    pub outcome: TrustOutcome,
    pub turn: u64,
    /// Faulted: the upheld fingerprints. Contested: the recurred ones.
    /// Clean: every claimed one.
    pub fingerprints: Vec<String>,
}

/// What a settlement did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustSettlement {
    pub adjustments: Vec<TrustAdjustment>,
    pub settled: Vec<WindowSettlement>,
}

impl TrustSettlement {
    /// How many windows it closed with `outcome`.
    #[must_use]
    pub fn count(&self, outcome: TrustOutcome) -> u64 {
        self.settled
            .iter()
            .filter(|window| window.outcome == outcome)
            .count() as u64
    }
}

/// `kind:id`.
#[must_use]
pub fn harness_entry_ref(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// `(kind, id)` of a `kind:id` ref; `None` when either side is empty.
#[must_use]
pub fn parse_harness_entry_ref(entry_ref: &str) -> Option<(&str, &str)> {
    let (kind, id) = entry_ref.split_once(':')?;
    (!kind.is_empty() && !id.is_empty()).then_some((kind, id))
}

/// `clampTrust` for an integer score.
#[must_use]
pub fn clamp_trust(score: i64) -> i64 {
    score.clamp(MIN_ENTRY_TRUST, MAX_ENTRY_TRUST)
}

/// `Number.MAX_SAFE_INTEGER` as a double (exact).
#[allow(clippy::cast_precision_loss)]
const MAX_SAFE_F64: f64 = MAX_SAFE_INTEGER as f64;

/// `Math.round(value)` of a finite double, saturating into `i64`.
#[allow(clippy::cast_possible_truncation)]
fn round_to_i64(value: f64) -> i64 {
    js_round(value) as i64
}

/// `clampTrust` of a stored number.
fn clamp_trust_number(score: f64) -> i64 {
    if score.is_finite() {
        clamp_trust(round_to_i64(score))
    } else {
        DEFAULT_ENTRY_TRUST
    }
}

/// A fresh record at the default score.
#[must_use]
pub fn empty_entry_trust(at: &str) -> EntryTrust {
    EntryTrust {
        score: DEFAULT_ENTRY_TRUST,
        updated_at: at.to_string(),
        events: Vec::new(),
    }
}

/// An entry with no record is fully trusted, not untrusted.
#[must_use]
pub fn entry_trust_score(trust: Option<&EntryTrust>) -> i64 {
    trust.map_or(DEFAULT_ENTRY_TRUST, |trust| clamp_trust(trust.score))
}

/// Whether a record makes its entry dormant.
#[must_use]
pub fn is_dormant_trust(trust: Option<&EntryTrust>) -> bool {
    entry_trust_score(trust) < DORMANT_TRUST_THRESHOLD
}

/// Rocq `Ravo.v` Section 15: a measured fault leaves an entry strictly less
/// trusted than a clean window does, at every reachable score.
#[must_use]
pub fn fault_is_strictly_worse_than_clean_window(score: i64) -> bool {
    clamp_trust(score - MEASURED_FAULT_DEBIT) < clamp_trust(score + CLEAN_WINDOW_CREDIT)
}

fn finite(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|number| number.is_finite())
}

fn safe_natural(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && (0.0..=MAX_SAFE_F64).contains(&number)).then(|| {
        // A whole number in [0, 2^53): exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = number as u64;
        whole
    })
}

fn safe_turn(value: Option<&Value>) -> u64 {
    safe_natural(value).unwrap_or(0)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn own_record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object()
}

fn normalize_trust_event(value: &Value) -> Option<TrustEvent> {
    let raw = value.as_object()?;
    let reason = match raw.get("reason")?.as_str()? {
        "clean_window" => TrustReason::CleanWindow,
        "measured_fault" => TrustReason::MeasuredFault,
        _ => return None,
    };
    let delta = finite(raw.get("delta"))?;
    let score = finite(raw.get("score"))?;
    let text = |key: &str| {
        raw.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(TrustEvent {
        reason,
        delta: round_to_i64(delta),
        score: clamp_trust_number(score),
        at: text("at"),
        proposal_id: text("proposalId"),
        fingerprint_id: raw
            .get("fingerprintId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
    })
}

/// A stored trust record as TS loads it; `None` (the record is dropped)
/// when it is not an object with a finite score.
#[must_use]
pub fn normalize_entry_trust(value: Option<&Value>) -> Option<EntryTrust> {
    let raw = value?.as_object()?;
    let score = finite(raw.get("score"))?;
    let mut events: Vec<TrustEvent> = raw
        .get("events")
        .and_then(Value::as_array)
        .map(|events| events.iter().filter_map(normalize_trust_event).collect())
        .unwrap_or_default();
    let excess = events.len().saturating_sub(MAX_TRUST_EVENTS);
    events.drain(..excess);
    Some(EntryTrust {
        score: clamp_trust_number(score),
        updated_at: raw
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        events,
    })
}

fn in_window(window: &TrustWindow, ordinal: u64) -> bool {
    ordinal <= MAX_SAFE_INTEGER && ordinal >= window.committed_turn && ordinal <= window.until_turn
}

fn is_adjudicable_entry(touched: &[String], entry: &str) -> bool {
    entry.starts_with(SKILL_REF_PREFIX) && touched.iter().any(|item| item == entry)
}

fn is_charged_entry(window: &TrustWindow, entry: &str) -> bool {
    window.outcome == TrustOutcome::Faulted
        && window
            .faulted_entries
            .as_ref()
            .is_none_or(|entries| entries.iter().any(|item| item == entry))
}

/// The same set of modules.
pub(crate) fn same_modules(left: &[String], right: &[String]) -> bool {
    let mut left: Vec<&String> = left.iter().collect();
    let mut right: Vec<&String> = right.iter().collect();
    left.sort();
    left.dedup();
    right.sort();
    right.dedup();
    left == right
}

fn normalize_skill_imports(
    touched: &[String],
    value: Option<&Value>,
) -> Option<IndexMap<String, Vec<String>>> {
    let raw = own_record(value)?;
    let mut imports = IndexMap::new();
    for (entry_ref, list) in raw {
        if !entry_ref.starts_with(SKILL_REF_PREFIX) || !touched.contains(entry_ref) {
            continue;
        }
        let modules = sorted_unique(string_list(Some(list)));
        if !modules.is_empty() {
            imports.insert(entry_ref.clone(), modules);
        }
    }
    (!imports.is_empty()).then_some(imports)
}

/// Fold runs into an adjudication: the higher status, the earlier ordinal,
/// the earliest runs of the union.
fn merge_adjudication(
    existing: Option<&TrustAdjudication>,
    incoming: &TrustAdjudication,
) -> TrustAdjudication {
    let base = existing.unwrap_or(incoming);
    let mut runs: Vec<String> = existing.map(|item| item.runs.clone()).unwrap_or_default();
    runs.extend(incoming.runs.iter().cloned());
    let mut runs = sorted_unique(runs);
    runs.truncate(MAX_TRUST_ADJUDICATION_RUNS);
    TrustAdjudication {
        entry: base.entry.clone(),
        fingerprint_id: base.fingerprint_id.clone(),
        status: if incoming.status.rank() > base.status.rank() {
            incoming.status
        } else {
            base.status
        },
        ordinal: base.ordinal.min(incoming.ordinal),
        runs,
    }
}

/// Merge a run into a window's list keyed by (entry, fingerprint); a new
/// pair past the cap is dropped.
fn with_adjudication(
    adjudications: &[TrustAdjudication],
    incoming: &TrustAdjudication,
) -> Vec<TrustAdjudication> {
    let index = adjudications.iter().position(|item| {
        item.entry == incoming.entry && item.fingerprint_id == incoming.fingerprint_id
    });
    let mut next = adjudications.to_vec();
    match index {
        None => {
            if adjudications.len() >= MAX_TRUST_ADJUDICATIONS_PER_WINDOW {
                return next;
            }
            next.push(merge_adjudication(None, incoming));
            next.sort_by(|left, right| {
                locale_compare(&left.entry, &right.entry)
                    .then_with(|| locale_compare(&left.fingerprint_id, &right.fingerprint_id))
            });
        }
        Some(index) => next[index] = merge_adjudication(Some(&adjudications[index]), incoming),
    }
    next
}

impl TrustWindow {
    /// Record that an optional key is now present (appended, as a JS
    /// spread adds a new key at the end).
    fn mark(&mut self, key: WindowKey) {
        if !self.order.contains(&key) {
            self.order.push(key);
        }
    }

    fn set_settled_turn(&mut self, turn: u64) {
        self.settled_turn = Some(turn);
        self.mark(WindowKey::SettledTurn);
    }

    fn set_faulted(&mut self, fingerprints: Vec<String>, entries: Vec<String>) {
        self.faulted_fingerprints = Some(fingerprints);
        self.mark(WindowKey::FaultedFingerprints);
        self.faulted_entries = Some(entries);
        self.mark(WindowKey::FaultedEntries);
    }

    /// The window as the TS object, keys in insertion order.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("proposalId".into(), Value::from(self.proposal_id.clone()));
        map.insert("touched".into(), Value::from(self.touched.clone()));
        map.insert(
            "claimedFingerprints".into(),
            Value::from(self.claimed_fingerprints.clone()),
        );
        map.insert("committedTurn".into(), Value::from(self.committed_turn));
        map.insert("untilTurn".into(), Value::from(self.until_turn));
        map.insert("outcome".into(), Value::from(self.outcome.as_str()));
        for key in &self.order {
            let (name, value) = match key {
                WindowKey::SettledTurn => ("settledTurn", self.settled_turn.map(Value::from)),
                WindowKey::FaultedFingerprints => (
                    "faultedFingerprints",
                    self.faulted_fingerprints.clone().map(Value::from),
                ),
                WindowKey::FaultedEntries => (
                    "faultedEntries",
                    self.faulted_entries.clone().map(Value::from),
                ),
                WindowKey::SkillImports => (
                    "skillImports",
                    self.skill_imports
                        .as_ref()
                        .and_then(|imports| serde_json::to_value(imports).ok()),
                ),
                WindowKey::Recurrences => (
                    "recurrences",
                    self.recurrences
                        .as_ref()
                        .and_then(|recurrences| serde_json::to_value(recurrences).ok()),
                ),
                WindowKey::Adjudications => (
                    "adjudications",
                    self.adjudications
                        .as_ref()
                        .and_then(|items| serde_json::to_value(items).ok()),
                ),
            };
            if let Some(value) = value {
                map.insert(name.into(), value);
            }
        }
        Value::Object(map)
    }
}

/// A stored window as TS loads it. A window that claims no fingerprint can
/// never be refuted: one still open loads `unmeasured` and never earns
/// anything. Malformed attribution or evidence is dropped piecewise.
fn normalize_trust_window(key: &str, value: &Value) -> Option<TrustWindow> {
    let raw = value.as_object()?;
    let proposal_id = raw
        .get("proposalId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(key)
        .to_string();
    if proposal_id.is_empty() {
        return None;
    }
    let committed_turn = safe_turn(raw.get("committedTurn"));
    let until_turn = committed_turn.max(safe_turn(raw.get("untilTurn")));
    let claimed_fingerprints = string_list(raw.get("claimedFingerprints"));
    let touched = string_list(raw.get("touched"));
    let recorded = TrustOutcome::parse(raw.get("outcome")).unwrap_or(TrustOutcome::Open);
    let outcome = if recorded == TrustOutcome::Open && claimed_fingerprints.is_empty() {
        TrustOutcome::Unmeasured
    } else {
        recorded
    };
    let mut window = TrustWindow {
        proposal_id,
        touched,
        claimed_fingerprints,
        committed_turn,
        until_turn,
        outcome,
        settled_turn: None,
        faulted_fingerprints: None,
        faulted_entries: None,
        skill_imports: None,
        recurrences: None,
        adjudications: None,
        order: Vec::new(),
    };
    let faulted = string_list(raw.get("faultedFingerprints"));
    let skill_imports = normalize_skill_imports(&window.touched, raw.get("skillImports"));
    let mut recurrences = IndexMap::new();
    for (fingerprint_id, ordinal) in own_record(raw.get("recurrences")).into_iter().flatten() {
        let Some(ordinal) = safe_natural(Some(ordinal)) else {
            continue;
        };
        if window.claimed_fingerprints.contains(fingerprint_id) && in_window(&window, ordinal) {
            recurrences.insert(fingerprint_id.clone(), ordinal);
        }
    }
    let adjudications = normalize_adjudications(&window, raw.get("adjudications"));
    let faulted_entries = if outcome == TrustOutcome::Faulted {
        sorted_unique(
            string_list(raw.get("faultedEntries"))
                .into_iter()
                .filter(|entry| is_adjudicable_entry(&window.touched, entry)),
        )
    } else {
        Vec::new()
    };
    if let Some(settled) = raw
        .get("settledTurn")
        .and_then(Value::as_f64)
        .filter(|turn| turn.fract() == 0.0 && turn.abs() <= MAX_SAFE_F64)
    {
        window.set_settled_turn(u64::try_from(round_to_i64(settled)).unwrap_or(0));
    }
    if !faulted.is_empty() {
        window.faulted_fingerprints = Some(faulted);
        window.mark(WindowKey::FaultedFingerprints);
    }
    if !faulted_entries.is_empty() {
        window.faulted_entries = Some(faulted_entries);
        window.mark(WindowKey::FaultedEntries);
    }
    if let Some(imports) = skill_imports {
        window.skill_imports = Some(imports);
        window.mark(WindowKey::SkillImports);
    }
    if !recurrences.is_empty() {
        window.recurrences = Some(recurrences);
        window.mark(WindowKey::Recurrences);
    }
    if !adjudications.is_empty() {
        window.adjudications = Some(adjudications);
        window.mark(WindowKey::Adjudications);
    }
    Some(window)
}

fn normalize_adjudications(window: &TrustWindow, value: Option<&Value>) -> Vec<TrustAdjudication> {
    let mut adjudications = Vec::new();
    for item in value.and_then(Value::as_array).into_iter().flatten() {
        let Some(raw) = item.as_object() else {
            continue;
        };
        let Some(entry) = raw.get("entry").and_then(Value::as_str) else {
            continue;
        };
        if !is_adjudicable_entry(&window.touched, entry) {
            continue;
        }
        let Some(fingerprint_id) = raw.get("fingerprintId").and_then(Value::as_str) else {
            continue;
        };
        if !window
            .claimed_fingerprints
            .iter()
            .any(|claimed| claimed == fingerprint_id)
        {
            continue;
        }
        let Some(status) = TrustAdjudicationStatus::parse(raw.get("status")) else {
            continue;
        };
        // TS: a number, in the window (a safe natural).
        let Some(ordinal) = safe_natural(raw.get("ordinal")) else {
            continue;
        };
        if !in_window(window, ordinal) {
            continue;
        }
        let Some(runs) = raw.get("runs").and_then(Value::as_array) else {
            continue;
        };
        if runs.is_empty() {
            continue;
        }
        let runs: Option<Vec<String>> = runs
            .iter()
            .map(|at| at.as_str().filter(|at| !at.is_empty()).map(str::to_string))
            .collect();
        let Some(runs) = runs else {
            continue;
        };
        adjudications = with_adjudication(
            &adjudications,
            &TrustAdjudication {
                entry: entry.to_string(),
                fingerprint_id: fingerprint_id.to_string(),
                status,
                ordinal,
                runs,
            },
        );
    }
    adjudications
}

/// The stored `trustWindows` as TS loads them; `None` when the value is
/// not an object.
#[must_use]
pub fn normalize_trust_windows(value: Option<&Value>) -> Option<TrustWindows> {
    let raw = value?.as_object()?;
    Some(
        raw.iter()
            .filter_map(|(key, raw)| {
                normalize_trust_window(key, raw).map(|window| (key.clone(), window))
            })
            .collect(),
    )
}

/// The windows as the TS `trustWindows` object.
#[must_use]
pub fn trust_windows_value(windows: &TrustWindows) -> Value {
    Value::Object(
        windows
            .iter()
            .map(|(key, window)| (key.clone(), window.to_value()))
            .collect(),
    )
}

/// Whether any window is still open.
#[must_use]
pub fn has_open_trust_windows(windows: Option<&TrustWindows>) -> bool {
    windows.is_some_and(|windows| {
        windows
            .values()
            .any(|window| window.outcome == TrustOutcome::Open)
    })
}

/// Drop the oldest settled windows past the cap; open windows are never
/// pruned.
fn prune_trust_windows(windows: TrustWindows) -> TrustWindows {
    let mut settled: Vec<(&String, u64)> = windows
        .iter()
        .filter(|(_, window)| window.outcome != TrustOutcome::Open)
        .map(|(key, window)| (key, window.settled_turn.unwrap_or(0)))
        .collect();
    if settled.len() <= MAX_SETTLED_TRUST_WINDOWS {
        return windows;
    }
    settled.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| locale_compare(left.0, right.0))
    });
    let excess = settled.len() - MAX_SETTLED_TRUST_WINDOWS;
    let drop: Vec<String> = settled[..excess]
        .iter()
        .map(|(key, _)| (*key).clone())
        .collect();
    windows
        .into_iter()
        .filter(|(key, _)| !drop.contains(key))
        .collect()
}

/// What one commit opens a window over.
#[derive(Debug, Clone, Default)]
pub struct TrustClaim {
    pub proposal_id: String,
    pub touched: Vec<String>,
    pub claimed_fingerprints: Vec<String>,
    pub committed_turn: u64,
    pub until_turn: u64,
    /// What each touched skill imports as the commit wrote it.
    pub skill_imports: IndexMap<String, Vec<String>>,
}

/// Record the attribution of one committed refinement. Nothing is scored:
/// a commit is neither trusted nor distrusted until its window settles.
#[must_use]
pub fn open_trust_window(windows: Option<&TrustWindows>, claim: &TrustClaim) -> TrustWindows {
    let touched = sorted_unique(claim.touched.iter().cloned());
    let skill_imports = normalize_skill_imports(
        &touched,
        serde_json::to_value(&claim.skill_imports).ok().as_ref(),
    );
    let mut opened = TrustWindow {
        proposal_id: claim.proposal_id.clone(),
        touched,
        claimed_fingerprints: sorted_unique(claim.claimed_fingerprints.iter().cloned()),
        committed_turn: claim.committed_turn,
        until_turn: claim.committed_turn.max(claim.until_turn),
        outcome: TrustOutcome::Open,
        settled_turn: None,
        faulted_fingerprints: None,
        faulted_entries: None,
        skill_imports: None,
        recurrences: None,
        adjudications: None,
        order: Vec::new(),
    };
    if let Some(imports) = skill_imports {
        opened.skill_imports = Some(imports);
        opened.mark(WindowKey::SkillImports);
    }
    let mut next = windows.cloned().unwrap_or_default();
    next.insert(claim.proposal_id.clone(), opened);
    prune_trust_windows(next)
}

/// What a `skill:<id>` entry imports now; `None` once it is gone.
pub type SkillImportsLookup<'a> = &'a dyn Fn(&str) -> Option<Vec<String>>;

/// Record recurrences and replay verdicts on the windows they speak to.
/// Pure, idempotent and order-insensitive. Evidence is ignored for an
/// unknown proposal, an `unmeasured` window, a fingerprint the window did
/// not claim, an ordinal outside it; a verdict also for an entry that is
/// not a skill the window touched, on a `faulted` window for an entry the
/// fault was already charged to, and (with `current_skill_imports`) for a
/// skill rewritten since its commit to import something else.
#[must_use]
pub fn record_trust_window_evidence(
    windows: Option<&TrustWindows>,
    evidence: &[TrustWindowEvidence],
    current_skill_imports: Option<SkillImportsLookup<'_>>,
) -> Option<TrustWindows> {
    let mut next = windows?.clone();
    for item in evidence {
        let Some(window) = next.get(item.proposal_id()) else {
            continue;
        };
        if window.outcome == TrustOutcome::Unmeasured {
            continue;
        }
        let charged = match item {
            TrustWindowEvidence::Recurrence { .. } => true,
            TrustWindowEvidence::Adjudication { entry, .. } => is_charged_entry(window, entry),
        };
        if window.outcome == TrustOutcome::Faulted && charged {
            continue;
        }
        let fingerprint_id = item.fingerprint_id();
        let ordinal = item.ordinal();
        if !window
            .claimed_fingerprints
            .iter()
            .any(|claimed| claimed == fingerprint_id)
            || !in_window(window, ordinal)
        {
            continue;
        }
        let mut updated = window.clone();
        if let TrustWindowEvidence::Adjudication {
            entry, status, at, ..
        } = item
        {
            if !is_adjudicable_entry(&window.touched, entry) || at.is_empty() {
                continue;
            }
            if let Some(imports) = current_skill_imports.and_then(|current| current(entry)) {
                let recorded = window
                    .skill_imports
                    .as_ref()
                    .and_then(|imports| imports.get(entry))
                    .cloned()
                    .unwrap_or_default();
                if !same_modules(&imports, &recorded) {
                    continue;
                }
            }
            let current = window.adjudications.clone().unwrap_or_default();
            let adjudications = with_adjudication(
                &current,
                &TrustAdjudication {
                    entry: entry.clone(),
                    fingerprint_id: fingerprint_id.to_string(),
                    status: *status,
                    ordinal,
                    runs: vec![at.clone()],
                },
            );
            if adjudications != current {
                updated.adjudications = Some(adjudications);
                updated.mark(WindowKey::Adjudications);
            }
        }
        let earliest = window
            .recurrences
            .as_ref()
            .and_then(|recurrences| recurrences.get(fingerprint_id).copied());
        if earliest.is_none_or(|earliest| ordinal < earliest) {
            updated
                .recurrences
                .get_or_insert_with(IndexMap::new)
                .insert(fingerprint_id.to_string(), ordinal);
            updated.mark(WindowKey::Recurrences);
        }
        next.insert(item.proposal_id().to_string(), updated);
    }
    Some(next)
}

fn apply_trust_delta(
    trust: &EntryTrust,
    reason: TrustReason,
    delta: i64,
    at: &str,
    proposal_id: &str,
    fingerprint_id: Option<&str>,
) -> EntryTrust {
    let score = clamp_trust(entry_trust_score(Some(trust)) + delta);
    let mut events = trust.events.clone();
    events.push(TrustEvent {
        reason,
        delta,
        score,
        at: at.to_string(),
        proposal_id: proposal_id.to_string(),
        fingerprint_id: fingerprint_id.map(str::to_string),
    });
    let excess = events.len().saturating_sub(MAX_TRUST_EVENTS);
    events.drain(..excess);
    EntryTrust {
        score,
        updated_at: at.to_string(),
        events,
    }
}

/// Charges scores within one settlement: two windows settling in one call
/// can touch the same entry, and each sees the score the previous one left.
struct Charger<'a> {
    lookup: &'a dyn Fn(&str, &str) -> Option<Option<EntryTrust>>,
    at: &'a str,
    pending: HashMap<String, EntryTrust>,
    settlement: TrustSettlement,
}

impl Charger<'_> {
    fn charge(
        &mut self,
        window: &TrustWindow,
        refs: &[String],
        reason: TrustReason,
        delta: i64,
        fingerprint_id: Option<&str>,
    ) {
        for entry_ref in refs {
            let Some((kind, id)) = parse_harness_entry_ref(entry_ref) else {
                continue;
            };
            let Some(stored) = (self.lookup)(kind, id) else {
                continue;
            };
            let current = self
                .pending
                .get(entry_ref)
                .cloned()
                .or(stored)
                .unwrap_or_else(|| empty_entry_trust(self.at));
            let before = entry_trust_score(Some(&current));
            let trust = apply_trust_delta(
                &current,
                reason,
                delta,
                self.at,
                &window.proposal_id,
                fingerprint_id,
            );
            self.pending.insert(entry_ref.clone(), trust.clone());
            self.settlement.adjustments.push(TrustAdjustment {
                kind: kind.to_string(),
                id: id.to_string(),
                reason,
                delta,
                before,
                after: trust.score,
                trust,
                proposal_id: window.proposal_id.clone(),
                fingerprint_id: fingerprint_id.map(str::to_string),
            });
        }
    }

    /// Charge each entry of `upheld` once, attributed to its lowest upheld
    /// fingerprint; answers (entries, fingerprints).
    fn charge_faults(
        &mut self,
        window: &TrustWindow,
        upheld: &[&TrustAdjudication],
    ) -> (Vec<String>, Vec<String>) {
        let entries = sorted_unique(upheld.iter().map(|item| item.entry.clone()));
        for entry in &entries {
            let fingerprint = sorted_unique(
                upheld
                    .iter()
                    .filter(|item| &item.entry == entry)
                    .map(|item| item.fingerprint_id.clone()),
            )
            .into_iter()
            .next();
            self.charge(
                window,
                std::slice::from_ref(entry),
                TrustReason::MeasuredFault,
                -MEASURED_FAULT_DEBIT,
                fingerprint.as_deref(),
            );
        }
        let fingerprints = sorted_unique(upheld.iter().map(|item| item.fingerprint_id.clone()));
        (entries, fingerprints)
    }
}

/// Settle every window the ordinal and its recorded evidence decide (TS
/// `settleTrustWindows`). `lookup` answers an entry's current trust:
/// `None` when the entry is gone, `Some(None)` when it has no record.
/// Returns the windows and what the settlement did; the caller stores
/// each adjustment's record back on its entry.
///
/// - An upheld verdict for one of a window's skill entries faults it,
///   whether it was open, clean or contested: each such entry is charged
///   `-15` once, nothing else the commit wrote is.
/// - A faulted window charges an upheld verdict for a skill it has not
///   charged yet, and keeps its outcome.
/// - An open window past `untilTurn` closes `contested` when a claimed
///   failure recurred in it, else `clean`, crediting every touched entry.
/// - A `cleared` or `unverifiable` verdict settles nothing.
#[must_use]
pub fn settle_trust_windows(
    windows: &TrustWindows,
    lookup: &dyn Fn(&str, &str) -> Option<Option<EntryTrust>>,
    turn: u64,
    at: &str,
) -> (TrustWindows, TrustSettlement) {
    let mut next = TrustWindows::new();
    let mut charger = Charger {
        lookup,
        at,
        pending: HashMap::new(),
        settlement: TrustSettlement::default(),
    };
    for (key, window) in windows {
        if window.outcome == TrustOutcome::Unmeasured {
            next.insert(key.clone(), window.clone());
            continue;
        }
        let upheld: Vec<&TrustAdjudication> = window
            .adjudications
            .iter()
            .flatten()
            .filter(|item| {
                item.status == TrustAdjudicationStatus::Upheld
                    && is_adjudicable_entry(&window.touched, &item.entry)
                    && !is_charged_entry(window, &item.entry)
                    && window.claimed_fingerprints.contains(&item.fingerprint_id)
                    && in_window(window, item.ordinal)
            })
            .collect();
        let mut updated = window.clone();
        if window.outcome == TrustOutcome::Faulted {
            if !upheld.is_empty() {
                let (entries, fingerprints) = charger.charge_faults(window, &upheld);
                let mut all_fingerprints = window.faulted_fingerprints.clone().unwrap_or_default();
                all_fingerprints.extend(fingerprints);
                let mut all_entries = window.faulted_entries.clone().unwrap_or_default();
                all_entries.extend(entries);
                updated.set_faulted(sorted_unique(all_fingerprints), sorted_unique(all_entries));
            }
        } else if !upheld.is_empty() {
            let (entries, fingerprints) = charger.charge_faults(window, &upheld);
            updated.outcome = TrustOutcome::Faulted;
            updated.set_settled_turn(turn);
            updated.set_faulted(fingerprints.clone(), entries);
            charger.settlement.settled.push(WindowSettlement {
                proposal_id: window.proposal_id.clone(),
                from: window.outcome,
                outcome: TrustOutcome::Faulted,
                turn,
                fingerprints,
            });
        } else if window.outcome == TrustOutcome::Open && turn > window.until_turn {
            let recurred = sorted_unique(
                window
                    .recurrences
                    .iter()
                    .flatten()
                    .map(|(id, _)| id.clone()),
            );
            let (outcome, fingerprints) = if recurred.is_empty() {
                charger.charge(
                    window,
                    &window.touched,
                    TrustReason::CleanWindow,
                    CLEAN_WINDOW_CREDIT,
                    None,
                );
                (TrustOutcome::Clean, window.claimed_fingerprints.clone())
            } else {
                (TrustOutcome::Contested, recurred)
            };
            updated.outcome = outcome;
            updated.set_settled_turn(turn);
            charger.settlement.settled.push(WindowSettlement {
                proposal_id: window.proposal_id.clone(),
                from: TrustOutcome::Open,
                outcome,
                turn,
                fingerprints,
            });
        }
        next.insert(key.clone(), updated);
    }
    (prune_trust_windows(next), charger.settlement)
}

/// What the trust bookkeeping reads and writes on a harness state's
/// entries: [`HarnessState`] on the refine path, the raw `entries` object of
/// a ledger flush's document on the flush path.
pub trait TrustEntries {
    /// An entry's trust: `None` when the entry is gone, `Some(None)` when
    /// it has no (well-formed) record.
    fn entry_trust(&self, kind: &str, id: &str) -> Option<Option<EntryTrust>>;

    /// A skill entry's `reference`, when the skill exists.
    fn skill_reference(&self, id: &str) -> Option<Map<String, Value>>;

    /// Store a record on an existing entry.
    fn set_entry_trust(&mut self, kind: &str, id: &str, trust: &EntryTrust);
}

fn refinement_kind(kind: &str) -> Option<RefinementKind> {
    match kind {
        "prompt" => Some(RefinementKind::Prompt),
        "memory" => Some(RefinementKind::Memory),
        "skill" => Some(RefinementKind::Skill),
        "subagent" => Some(RefinementKind::Subagent),
        "factory" => Some(RefinementKind::Factory),
        _ => None,
    }
}

impl TrustEntries for HarnessState {
    fn entry_trust(&self, kind: &str, id: &str) -> Option<Option<EntryTrust>> {
        let entry = self.entries.get(&refinement_kind(kind)?)?.get(id)?;
        Some(normalize_entry_trust(entry.extensions.get(TRUST_KEY)))
    }

    fn skill_reference(&self, id: &str) -> Option<Map<String, Value>> {
        self.entries
            .get(&RefinementKind::Skill)?
            .get(id)
            .map(|entry| entry.reference.clone())
    }

    fn set_entry_trust(&mut self, kind: &str, id: &str, trust: &EntryTrust) {
        let Some(kind) = refinement_kind(kind) else {
            return;
        };
        if let Some(entry) = self
            .entries
            .get_mut(&kind)
            .and_then(|records| records.get_mut(id))
        {
            entry.extensions.insert(
                TRUST_KEY.to_string(),
                serde_json::to_value(trust).unwrap_or(Value::Null),
            );
        }
    }
}

/// The raw `entries` object of a harness state document.
impl TrustEntries for Map<String, Value> {
    fn entry_trust(&self, kind: &str, id: &str) -> Option<Option<EntryTrust>> {
        let entry = self.get(kind)?.as_object()?.get(id)?.as_object()?;
        Some(normalize_entry_trust(entry.get(TRUST_KEY)))
    }

    fn skill_reference(&self, id: &str) -> Option<Map<String, Value>> {
        let entry = self.get("skill")?.as_object()?.get(id)?.as_object()?;
        Some(
            entry
                .get("reference")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        )
    }

    fn set_entry_trust(&mut self, kind: &str, id: &str, trust: &EntryTrust) {
        if let Some(entry) = self
            .get_mut(kind)
            .and_then(Value::as_object_mut)
            .and_then(|records| records.get_mut(id))
            .and_then(Value::as_object_mut)
        {
            entry.insert(
                TRUST_KEY.to_string(),
                serde_json::to_value(trust).unwrap_or(Value::Null),
            );
        }
    }
}

/// What a skill with `reference` imports (TS `skillImportsOf` over one
/// update edit).
#[must_use]
pub fn reference_imports(reference: &Map<String, Value>) -> Vec<String> {
    skill_imports_of(&[RefinementEdit {
        action: Some(RefinementAction::Update),
        kind: Some(RefinementKind::Skill),
        reference: Some(reference.clone()),
        ..RefinementEdit::default()
    }])
}

/// What the `skill:<id>` entry of `entries` imports now; `None` when it is
/// not a skill or is gone.
pub fn current_skill_imports(entries: &dyn TrustEntries, entry_ref: &str) -> Option<Vec<String>> {
    let (kind, id) = parse_harness_entry_ref(entry_ref)?;
    if kind != "skill" {
        return None;
    }
    entries
        .skill_reference(id)
        .map(|reference| reference_imports(&reference))
}

/// `windows` with `evidence` recorded, checked against the current imports
/// of `entries` (TS `recordHarnessTrustEvidence`).
#[must_use]
pub fn record_harness_trust_evidence(
    windows: Option<&TrustWindows>,
    entries: &dyn TrustEntries,
    evidence: &[TrustWindowEvidence],
) -> Option<TrustWindows> {
    let current = |entry_ref: &str| current_skill_imports(entries, entry_ref);
    record_trust_window_evidence(windows, evidence, Some(&current))
}

/// Settle `windows` and write the moved scores onto `entries` (TS
/// `settleHarnessTrust`).
pub fn settle_harness_trust(
    windows: &TrustWindows,
    entries: &mut dyn TrustEntries,
    turn: u64,
    at: &str,
) -> (TrustWindows, TrustSettlement) {
    let (windows, settlement) = {
        let lookup = |kind: &str, id: &str| entries.entry_trust(kind, id);
        settle_trust_windows(windows, &lookup, turn, at)
    };
    for adjustment in &settlement.adjustments {
        entries.set_entry_trust(&adjustment.kind, &adjustment.id, &adjustment.trust);
    }
    (windows, settlement)
}

/// The stored trust windows of a harness state, normalized.
#[must_use]
pub fn stored_trust_windows(state: &HarnessState) -> Option<TrustWindows> {
    normalize_trust_windows(state.extensions.get(TRUST_WINDOWS_KEY))
}

/// Where the trust log lines go (TS `HARNESS_TRUST_LOG_COMPONENT`).
pub const HARNESS_TRUST_LOG_TARGET: &str = "pa_ravo::harness_trust";

/// One record per window a settlement closed and per score it moved (TS
/// `logHarnessTrustSettlement`). Judge and tool text are never logged.
pub fn log_trust_settlement(settlement: &TrustSettlement, scope: &str) {
    for window in &settlement.settled {
        tracing::info!(
            target: HARNESS_TRUST_LOG_TARGET,
            proposal_id = window.proposal_id.as_str(),
            scope,
            from = window.from.as_str(),
            outcome = window.outcome.as_str(),
            ordinal = window.turn,
            fingerprints = window.fingerprints.join(","),
            "harness.trust.settled"
        );
    }
    for adjustment in &settlement.adjustments {
        tracing::info!(
            target: HARNESS_TRUST_LOG_TARGET,
            proposal_id = adjustment.proposal_id.as_str(),
            scope,
            entry = harness_entry_ref(&adjustment.kind, &adjustment.id),
            reason = adjustment.reason.as_str(),
            delta = adjustment.delta,
            before = adjustment.before,
            after = adjustment.after,
            dormant = adjustment.after < DORMANT_TRUST_THRESHOLD,
            fingerprint_id = adjustment.fingerprint_id.as_deref(),
            "harness.trust.adjusted"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rocq `Ravo.v` Section 15 over the whole clamp range.
    #[test]
    fn a_measured_fault_stays_strictly_below_a_clean_window_at_every_score() {
        for score in MIN_ENTRY_TRUST..=MAX_ENTRY_TRUST {
            assert!(fault_is_strictly_worse_than_clean_window(score), "{score}");
        }
    }

    /// 50 -> 35 -> 20 -> 5 -> 0 -> 0: clamped, dormant from 20 on, the
    /// event list bounded.
    #[test]
    fn scores_clamp_and_go_dormant_below_the_threshold() {
        let mut trust = empty_entry_trust("t0");
        let mut scores = Vec::new();
        for round in 0..25 {
            trust = apply_trust_delta(
                &trust,
                TrustReason::MeasuredFault,
                -MEASURED_FAULT_DEBIT,
                &format!("t{round}"),
                "p",
                Some("f"),
            );
            scores.push(trust.score);
        }
        assert_eq!(scores[..5], [35, 20, 5, 0, 0]);
        assert!(!is_dormant_trust(Some(&EntryTrust {
            score: 30,
            ..empty_entry_trust("t")
        })));
        assert!(is_dormant_trust(Some(&EntryTrust {
            score: 29,
            ..empty_entry_trust("t")
        })));
        assert!(!is_dormant_trust(None));
        assert_eq!(trust.events.len(), MAX_TRUST_EVENTS);
        assert_eq!(trust.events[0].at, "t5");
    }
}
