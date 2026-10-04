//! The resolution index (TS `distill/resolution-index.ts`): joins a failure
//! fingerprint observed in a Python cell to the cell that subsequently
//! stopped it happening, so that when the same fingerprint recurs the
//! `ipython` result carries the fix instead of leaving the model to
//! rediscover it.
//!
//! It reuses the failure ledger's fingerprinting unchanged, so an id here is
//! the same id there. Live records are mirrored to a per-repo store (see
//! [`FileResolutionStore`]) consulted when this session has not seen the
//! failure; every store failure degrades to the session-only index.

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::fingerprint::{
    failure_opponent_id, fingerprint_failure, parse_python_traceback, tool_error_text,
    FailureFingerprint, FailureKind,
};
use crate::js::{js_len, js_prefix, js_trim};

/// Fingerprint source, matching the tool name the failure ledger records.
pub const DEFAULT_RESOLUTION_SOURCE: &str = "ipython";
/// How many cells later a clean cell may still count as the fix.
pub const DEFAULT_RESOLUTION_WINDOW: u64 = 6;
/// Cap on retained resolutions; the least recently recorded is evicted first.
pub const DEFAULT_MAX_RESOLUTIONS: usize = 64;
/// Cell source is clipped to this many UTF-16 units before it is retained.
pub const DEFAULT_MAX_CELL_CHARS: usize = 1200;

/// One executed cell: its source, the text the model sees back (before any
/// annotation), and whether it errored or was aborted.
#[derive(Debug, Clone, Copy)]
pub struct ResolutionCell<'a> {
    pub code: &'a str,
    pub output: &'a str,
    pub is_error: bool,
}

/// A recorded fix. Field order is the TS object's; keys this crate does
/// not model ride along in `extra` when another writer's record is rewritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionRecord {
    pub fingerprint_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_class: Option<String>,
    /// Source of the cell that ran clean after the failure.
    pub fix: String,
    /// Source of the cell that produced the failure.
    pub failed: String,
    pub failed_at_cell: Number,
    pub fixed_at_cell: Number,
    /// Epoch ms the resolution was recorded; orders eviction in the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<Number>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ResolutionRecord {
    /// Read a stored record the way TS `isResolutionRecord` accepts one.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let raw = value.as_object()?;
        let number = |key: &str| match raw.get(key) {
            Some(Value::Number(number)) => Some(number.clone()),
            _ => None,
        };
        let string = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_string);
        let known = [
            "fingerprintId",
            "exceptionClass",
            "fix",
            "failed",
            "failedAtCell",
            "fixedAtCell",
            "recordedAt",
        ];
        Some(Self {
            fingerprint_id: string("fingerprintId")?,
            exception_class: string("exceptionClass"),
            fix: string("fix")?,
            failed: string("failed")?,
            failed_at_cell: number("failedAtCell")?,
            fixed_at_cell: number("fixedAtCell")?,
            recorded_at: number("recordedAt"),
            extra: raw
                .iter()
                .filter(|(key, _)| !known.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        })
    }

    fn recorded_at_ms(&self) -> f64 {
        self.recorded_at
            .as_ref()
            .and_then(Number::as_f64)
            .unwrap_or(0.0)
    }
}

/// Whether a hint came from this session's own history or from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionOrigin {
    Session,
    Store,
}

impl ResolutionOrigin {
    /// `session` or `store`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Store => "store",
        }
    }
}

/// The block appended to a tool result whose failure was fixed before.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolutionHint {
    pub fingerprint_id: String,
    pub record: ResolutionRecord,
    pub origin: ResolutionOrigin,
    pub text: String,
}

/// The durable half of the index: the resolutions one repo accumulated.
pub trait ResolutionStore: Send + Sync {
    /// Records held for this repo, oldest first; empty when unreadable.
    fn load(&self) -> Vec<ResolutionRecord>;
    /// Merge one record, replacing any earlier record of its fingerprint.
    fn save(&self, record: &ResolutionRecord);
    /// Drop a record whose fix has stopped working.
    fn forget(&self, fingerprint_id: &str);
}

/// Tunables of a [`ResolutionIndex`].
pub struct ResolutionIndexOptions {
    pub source: String,
    pub window: u64,
    pub max_records: usize,
    pub max_cell_chars: usize,
    pub store: Option<Box<dyn ResolutionStore>>,
}

impl Default for ResolutionIndexOptions {
    fn default() -> Self {
        Self {
            source: DEFAULT_RESOLUTION_SOURCE.to_string(),
            window: DEFAULT_RESOLUTION_WINDOW,
            max_records: DEFAULT_MAX_RESOLUTIONS,
            max_cell_chars: DEFAULT_MAX_CELL_CHARS,
            store: None,
        }
    }
}

struct PendingFailure {
    fingerprint: FailureFingerprint,
    code: String,
    tokens: HashSet<String>,
    cell: u64,
}

/// One session's resolution index.
pub struct ResolutionIndex {
    pending: IndexMap<String, PendingFailure>,
    resolutions: IndexMap<String, ResolutionRecord>,
    source: String,
    window: u64,
    max_records: usize,
    max_cell_chars: usize,
    store: Option<Box<dyn ResolutionStore>>,
    cells: u64,
    now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[A-Za-z_][A-Za-z0-9_]*")
        .unwrap_or_else(|error| panic!("identifier pattern: {error}"))
});

/// Python keywords plus the builtins common enough that sharing one says
/// nothing about two cells being about the same thing.
const COMMON_TOKENS: &str = "abs all and any as assert async await bool break bytes callable class cls continue def del dict dir elif else
 enumerate except false filter finally float for format from getattr global hasattr id if import in input int is
 isinstance iter lambda len list map max min next none nonlocal not object open or pass print raise range repr
 return round self set setattr sorted str sum super true try tuple type vars while with yield zip";

static COMMON: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| COMMON_TOKENS.split_whitespace().collect());

fn significant_tokens(code: &str) -> HashSet<String> {
    IDENTIFIER
        .find_iter(code)
        .map(|found| found.as_str())
        .filter(|token| token.len() >= 2 && !COMMON.contains(token.to_lowercase().as_str()))
        .map(str::to_string)
        .collect()
}

/// `text.trim()` clipped to `max` UTF-16 units with a truncation marker.
#[must_use]
pub(crate) fn clip_cell(text: &str, max: usize) -> String {
    let trimmed = js_trim(text);
    if js_len(trimmed) > max {
        format!("{}\n# ... truncated", js_prefix(trimmed, max))
    } else {
        trimmed.to_string()
    }
}

impl Default for ResolutionIndex {
    fn default() -> Self {
        Self::new(ResolutionIndexOptions::default())
    }
}

impl ResolutionIndex {
    /// A fresh index.
    #[must_use]
    pub fn new(options: ResolutionIndexOptions) -> Self {
        Self {
            pending: IndexMap::new(),
            resolutions: IndexMap::new(),
            source: options.source,
            window: options.window.max(1),
            max_records: options.max_records.max(1),
            max_cell_chars: options.max_cell_chars.max(1),
            store: options.store,
            cells: 0,
            now_ms: Box::new(crate::js::now_millis),
        }
    }

    /// Replace the clock that stamps `recordedAt` (tests, goldens).
    #[must_use]
    pub fn with_clock(mut self, now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now_ms = Box::new(now_ms);
        self
    }

    /// The resolution recorded for a fingerprint, by this session or an earlier one.
    #[must_use]
    pub fn lookup(&self, fingerprint_id: &str) -> Option<ResolutionRecord> {
        self.resolutions
            .get(fingerprint_id)
            .cloned()
            .or_else(|| self.stored(fingerprint_id))
    }

    /// Every resolution this session recorded, least recently recorded first.
    #[must_use]
    pub fn records(&self) -> Vec<ResolutionRecord> {
        self.resolutions.values().cloned().collect()
    }

    /// Every resolution the store holds for this repo, oldest first.
    #[must_use]
    pub fn durable_records(&self) -> Vec<ResolutionRecord> {
        self.store
            .as_ref()
            .map(|store| store.load())
            .unwrap_or_default()
    }

    /// Fingerprints seen failing that no later cell has resolved yet.
    #[must_use]
    pub fn unresolved(&self) -> Vec<String> {
        self.pending.keys().cloned().collect()
    }

    /// Record one executed cell and return the hint to append to its result
    /// when it reproduced a fingerprint resolved before.
    pub fn observe(&mut self, cell: ResolutionCell<'_>) -> Option<ResolutionHint> {
        let index = self.cells;
        self.cells += 1;
        let fingerprint = self.fingerprint_of(cell);
        let hint = fingerprint
            .as_ref()
            .and_then(|fingerprint| self.hint_for(fingerprint, cell.code));
        self.settle_pending(cell, index, fingerprint.as_ref().map(|fp| fp.id.as_str()));
        if let Some(fingerprint) = fingerprint {
            let id = fingerprint.id.clone();
            self.pending.insert(
                id,
                PendingFailure {
                    code: clip_cell(cell.code, self.max_cell_chars),
                    tokens: significant_tokens(cell.code),
                    cell: index,
                    fingerprint,
                },
            );
        }
        hint
    }

    fn hint_for(&mut self, fingerprint: &FailureFingerprint, code: &str) -> Option<ResolutionHint> {
        let session = self.resolutions.get(&fingerprint.id).cloned();
        let origin = if session.is_some() {
            ResolutionOrigin::Session
        } else {
            ResolutionOrigin::Store
        };
        let record = session.or_else(|| self.stored(&fingerprint.id))?;
        // The recorded fix is the cell that just failed, so it never was one;
        // a stale durable fix goes too, or every later session repeats it.
        if record.fix == clip_cell(code, self.max_cell_chars) {
            self.resolutions.shift_remove(&fingerprint.id);
            if let Some(store) = &self.store {
                store.forget(&fingerprint.id);
            }
            return None;
        }
        Some(ResolutionHint {
            fingerprint_id: fingerprint.id.clone(),
            text: format_resolution_hint(&record, origin),
            record,
            origin,
        })
    }

    fn stored(&self, fingerprint_id: &str) -> Option<ResolutionRecord> {
        self.store
            .as_ref()?
            .load()
            .into_iter()
            .find(|record| record.fingerprint_id == fingerprint_id)
    }

    /// A pending failure is resolved by the next clean cell inside the
    /// window that does not reproduce it and shares an identifier with the
    /// cell that failed.
    fn settle_pending(&mut self, cell: ResolutionCell<'_>, index: u64, reproduced: Option<&str>) {
        let tokens = (!cell.is_error).then(|| significant_tokens(cell.code));
        let ids: Vec<String> = self.pending.keys().cloned().collect();
        for id in ids {
            if Some(id.as_str()) == reproduced {
                continue;
            }
            let Some(failure) = self.pending.get(&id) else {
                continue;
            };
            if index - failure.cell > self.window {
                self.pending.shift_remove(&id);
                continue;
            }
            let Some(tokens) = &tokens else {
                continue;
            };
            if failure.tokens.is_disjoint(tokens) {
                continue;
            }
            let Some(failure) = self.pending.shift_remove(&id) else {
                continue;
            };
            let fix = clip_cell(cell.code, self.max_cell_chars);
            // Re-running the same cell clean resolves the failure but teaches nothing.
            if fix == failure.code {
                continue;
            }
            self.record(ResolutionRecord {
                fingerprint_id: id,
                exception_class: failure.fingerprint.exception_class,
                fix,
                failed: failure.code,
                failed_at_cell: Number::from(failure.cell),
                fixed_at_cell: Number::from(index),
                recorded_at: None,
                extra: Map::new(),
            });
        }
    }

    fn record(&mut self, mut record: ResolutionRecord) {
        if record.recorded_at.is_none() {
            record.recorded_at = Some(Number::from((self.now_ms)()));
        }
        self.resolutions.shift_remove(&record.fingerprint_id);
        self.resolutions
            .insert(record.fingerprint_id.clone(), record.clone());
        while self.resolutions.len() > self.max_records {
            self.resolutions.shift_remove_index(0);
        }
        if let Some(store) = &self.store {
            store.save(&record);
        }
    }

    /// Same shape as the failure ledger's tool-result observation, so the
    /// ids match.
    fn fingerprint_of(&self, cell: ResolutionCell<'_>) -> Option<FailureFingerprint> {
        if let Some(traceback) = parse_python_traceback(cell.output) {
            return Some(fingerprint_failure(
                FailureKind::PythonException,
                Some(traceback.skill_name.as_deref().unwrap_or(&self.source)),
                Some(&traceback.exception_class),
                &traceback.message,
            ));
        }
        cell.is_error.then(|| {
            fingerprint_failure(
                FailureKind::ToolError,
                Some(&self.source),
                None,
                tool_error_text(cell.output),
            )
        })
    }
}

/// `${n + 1}` for a stored JSON number.
fn plus_one(number: &Number) -> String {
    if let Some(int) = number.as_u64() {
        return (u128::from(int) + 1).to_string();
    }
    if let Some(int) = number.as_i64() {
        return (i128::from(int) + 1).to_string();
    }
    let next = number.as_f64().unwrap_or(0.0) + 1.0;
    if next.fract() == 0.0 && next.abs() < 1e21 {
        format!("{next:.0}")
    } else {
        next.to_string()
    }
}

/// The `<ipython_resolution_hint>` block (TS `formatResolutionHint`).
#[must_use]
pub fn format_resolution_hint(record: &ResolutionRecord, origin: ResolutionOrigin) -> String {
    let opponent = failure_opponent_id(&record.fingerprint_id);
    let label = match record
        .exception_class
        .as_deref()
        .filter(|class| !class.is_empty())
    {
        Some(class) => format!("{opponent}, {class}"),
        None => opponent,
    };
    let place = match origin {
        ResolutionOrigin::Store => "an earlier session",
        ResolutionOrigin::Session => "this session",
    };
    [
        "<ipython_resolution_hint>".to_string(),
        "You hit this before; this fixed it:".to_string(),
        "```python".to_string(),
        record.fix.clone(),
        "```".to_string(),
        format!(
            "({label}; first hit at cell {}, fixed at cell {} of {place})",
            plus_one(&record.failed_at_cell),
            plus_one(&record.fixed_at_cell)
        ),
        "</ipython_resolution_hint>".to_string(),
    ]
    .join("\n")
}

/// Keep a store bounded whatever the writer's own limits were: the newest
/// [`DEFAULT_MAX_RESOLUTIONS`] records, cells clipped.
pub(crate) fn bound_records(records: Vec<ResolutionRecord>) -> Vec<ResolutionRecord> {
    let mut records = records;
    records.sort_by(|a, b| {
        a.recorded_at_ms()
            .partial_cmp(&b.recorded_at_ms())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let skip = records.len().saturating_sub(DEFAULT_MAX_RESOLUTIONS);
    records
        .into_iter()
        .skip(skip)
        .map(|record| ResolutionRecord {
            fix: clip_cell(&record.fix, DEFAULT_MAX_CELL_CHARS),
            failed: clip_cell(&record.failed, DEFAULT_MAX_CELL_CHARS),
            ..record
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract_failures;

    const FAILING_CELL: &str = "agents = client.list_agents()";
    const FIX_CELL: &str = "agents = client.agents()\nprint(f\"{len(agents)} agents\")";
    const RECURRENCE_CELL: &str = "for agent in client.list_agents():\n    print(agent.name)";

    fn attribute_error(line: u32, source: &str) -> String {
        [
            "Traceback (most recent call last):".to_string(),
            format!("  File \"<ipython-input-{line}>\", line 1, in <module>"),
            format!("    {source}"),
            "AttributeError: 'AgentClient' object has no attribute 'list_agents'".to_string(),
        ]
        .join("\n")
    }

    fn cell<'a>(code: &'a str, output: &'a str, is_error: bool) -> ResolutionCell<'a> {
        ResolutionCell {
            code,
            output,
            is_error,
        }
    }

    #[test]
    fn a_recurring_fingerprint_gets_the_cell_that_fixed_it() {
        let mut index = ResolutionIndex::default().with_clock(|| 5);
        let failure = attribute_error(1, FAILING_CELL);
        assert_eq!(index.observe(cell(FAILING_CELL, &failure, true)), None);
        assert_eq!(index.observe(cell(FIX_CELL, "3 agents", false)), None);
        let recurrence = attribute_error(3, "for agent in client.list_agents():");
        let hint = index
            .observe(cell(RECURRENCE_CELL, &recurrence, true))
            .unwrap();
        let id = hint.fingerprint_id.clone();
        assert_eq!(
            hint,
            ResolutionHint {
                fingerprint_id: id.clone(),
                record: ResolutionRecord {
                    fingerprint_id: id.clone(),
                    exception_class: Some("AttributeError".to_string()),
                    fix: FIX_CELL.to_string(),
                    failed: FAILING_CELL.to_string(),
                    failed_at_cell: Number::from(0),
                    fixed_at_cell: Number::from(1),
                    recorded_at: Some(Number::from(5)),
                    extra: Map::new(),
                },
                origin: ResolutionOrigin::Session,
                text: format!(
                    "<ipython_resolution_hint>\nYou hit this before; this fixed it:\n```python\n{FIX_CELL}\n```\n(failure:{id}, AttributeError; first hit at cell 1, fixed at cell 2 of this session)\n</ipython_resolution_hint>"
                ),
            }
        );
    }

    #[test]
    fn a_cell_fingerprints_exactly_like_the_ledger_observes_it() {
        let traceback = attribute_error(1, FAILING_CELL);
        let message: pa_agent::types::AgentMessage = serde_json::from_value(serde_json::json!({
            "role": "toolResult", "toolCallId": "call-0", "toolName": "ipython",
            "content": [{"type": "text", "text": traceback}], "isError": true, "timestamp": 0
        }))
        .unwrap();
        let observed = extract_failures(&[message], 0, 1, &|| String::new());
        let mut index = ResolutionIndex::default();
        index.observe(cell(FAILING_CELL, &traceback, true));
        assert_eq!(index.unresolved(), vec![observed[0].fingerprint.id.clone()]);
    }

    #[test]
    fn an_unrelated_clean_cell_is_not_the_fix() {
        let mut index = ResolutionIndex::default();
        index.observe(cell(FAILING_CELL, &attribute_error(1, FAILING_CELL), true));
        index.observe(cell("import pandas as pd", "", false));
        assert_eq!(index.records(), Vec::new());
        assert_eq!(index.unresolved().len(), 1);
    }

    #[test]
    fn a_cell_that_itself_failed_is_not_the_fix() {
        let mut index = ResolutionIndex::default();
        let failure = attribute_error(1, FAILING_CELL);
        let type_error = "Traceback (most recent call last):\n  File \"<ipython-input-2>\", line 1, in <module>\n    agents = client.agents()\nTypeError: agents() takes 0 positional arguments but 1 was given";
        index.observe(cell(FAILING_CELL, &failure, true));
        index.observe(cell(FIX_CELL, type_error, true));
        assert_eq!(index.records(), Vec::new());
        index.observe(cell(FIX_CELL, "3 agents", false));
        let failed: Vec<String> = index
            .records()
            .into_iter()
            .map(|record| record.failed)
            .collect();
        assert_eq!(failed, vec![FAILING_CELL.to_string()]);
        let hint = index
            .observe(cell(RECURRENCE_CELL, &failure, true))
            .unwrap();
        assert_eq!(hint.record.fix, FIX_CELL);
    }

    #[test]
    fn a_failure_no_cell_inside_the_window_resolved_is_forgotten() {
        let mut index = ResolutionIndex::new(ResolutionIndexOptions {
            window: 1,
            ..ResolutionIndexOptions::default()
        });
        index.observe(cell(FAILING_CELL, &attribute_error(1, FAILING_CELL), true));
        index.observe(cell("import pandas as pd", "", false));
        index.observe(cell(FIX_CELL, "3 agents", false));
        assert_eq!(index.records(), Vec::new());
        assert_eq!(index.unresolved(), Vec::<String>::new());
    }

    #[test]
    fn a_recorded_fix_is_dropped_once_that_very_cell_reproduces_the_fingerprint() {
        let mut index = ResolutionIndex::default();
        let failure = attribute_error(1, FAILING_CELL);
        index.observe(cell(FAILING_CELL, &failure, true));
        index.observe(cell(FIX_CELL, "3 agents", false));
        assert_eq!(index.records().len(), 1);
        assert_eq!(index.observe(cell(FIX_CELL, &failure, true)), None);
        assert_eq!(index.records(), Vec::new());
    }

    #[test]
    fn the_session_index_evicts_the_least_recently_recorded() {
        let mut index = ResolutionIndex::new(ResolutionIndexOptions {
            max_records: 1,
            ..ResolutionIndexOptions::default()
        });
        for name in ["zz", "zzz"] {
            let failure = format!(
                "Traceback (most recent call last):\n  File \"<ipython-input-1>\", line 1, in <module>\n    handle.{name}()\nAttributeError: module tool has no attribute {name}"
            );
            index.observe(cell(&format!("handle.{name}()"), &failure, true));
            index.observe(cell(&format!("handle.run_{name}()\nhandle"), "ok", false));
        }
        let fixes: Vec<String> = index
            .records()
            .into_iter()
            .map(|record| record.fix)
            .collect();
        assert_eq!(fixes, vec!["handle.run_zzz()\nhandle".to_string()]);
    }
}
