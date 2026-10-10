//! Continual harness state: entries, refinement events, persistence,
//! merge, history, and prompt rendering.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Refinement entry kinds (the continual harness component set).
pub const REFINEMENT_KINDS: [&str; 5] = ["prompt", "memory", "skill", "subagent", "factory"];

/// Directory name under the agent dir (or session artifact dir).
pub const HARNESS_STATE_DIR_NAME: &str = "harness";
pub const REFINEMENT_HISTORY_FILE_NAME: &str = "refinement_history.jsonl";

/// Default overview limits (TS `DEFAULT_OVERVIEW_*` constants).
pub const DEFAULT_OVERVIEW_ENTRY_LIMIT: usize = 3;
pub const DEFAULT_OVERVIEW_REFINEMENT_LIMIT: usize = 10;
pub const DEFAULT_OVERVIEW_CONTENT_LIMIT: usize = 140;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefinementKind {
    Prompt,
    Memory,
    Skill,
    Subagent,
    Factory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefinementAction {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessScope {
    Local,
    Global,
}

/// One editable continual harness entry. The TS schema keeps
/// `created_at`/`updated_at` snake-cased (the rest of the fields are
/// single words).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEntry {
    pub id: String,
    pub kind: RefinementKind,
    pub title: String,
    pub content: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<HarnessScope>,
    pub reference: serde_json::Map<String, serde_json::Value>,
    #[serde(rename = "arguments")]
    pub arguments: serde_json::Map<String, serde_json::Value>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub source: String,
    #[serde(rename = "created_at")]
    pub created_at: String,
    #[serde(rename = "updated_at")]
    pub updated_at: String,
    pub version: u64,
    /// Keys this crate does not model (another producer's per-entry state,
    /// such as the fork's `trust`), carried through load, refine and save
    /// untouched, after the modelled keys as a TS save writes them.
    #[serde(flatten)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

impl HarnessEntry {
    /// The entry without its unmodelled keys: what an edit of the entry
    /// is compared on, since other producers' bookkeeping on it moves
    /// independently of its content.
    #[must_use]
    pub fn modelled(&self) -> Self {
        Self {
            extensions: serde_json::Map::new(),
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRefinementEvent {
    pub id: String,
    pub trigger: String,
    pub changes: Vec<String>,
    pub evidence: String,
    pub outcome: String,
    /// The TS event schema keeps the snake-cased `created_at`.
    #[serde(rename = "created_at")]
    pub created_at: String,
    /// Why the host ran the refine (`manual`, `recurrence`, `turn_interval`,
    /// ...), when it recorded one; a save keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessState {
    pub schema: u64,
    /// Entries keyed by kind, then id. Ordered (`BTreeMap`): the state
    /// serializes to the on-disk harness JSON, and unordered iteration
    /// would write random key order (and churn the file between runs).
    pub entries: BTreeMap<RefinementKind, BTreeMap<String, HarnessEntry>>,
    pub refinements: Vec<HarnessRefinementEvent>,
    /// Top-level keys this crate does not model (other producers' state, such
    /// as the fork's `ravo` / `failures` / `trustWindows`), carried through a
    /// load/save round trip untouched so a refine never erases them.
    #[serde(flatten)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

#[must_use]
pub fn empty_harness_state() -> HarnessState {
    HarnessState {
        schema: 1,
        entries: [
            (RefinementKind::Prompt, BTreeMap::new()),
            (RefinementKind::Memory, BTreeMap::new()),
            (RefinementKind::Skill, BTreeMap::new()),
            (RefinementKind::Subagent, BTreeMap::new()),
            (RefinementKind::Factory, BTreeMap::new()),
        ]
        .into_iter()
        .collect(),
        refinements: Vec::new(),
        extensions: serde_json::Map::new(),
    }
}

fn kind_from_name(name: &str) -> RefinementKind {
    match name {
        "prompt" => RefinementKind::Prompt,
        "memory" => RefinementKind::Memory,
        "skill" => RefinementKind::Skill,
        "factory" => RefinementKind::Factory,
        _ => RefinementKind::Subagent,
    }
}

#[must_use]
pub fn get_global_harness_state_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(HARNESS_STATE_DIR_NAME)
}

#[must_use]
pub fn get_local_harness_state_dir(session_artifact_dir: Option<&Path>) -> Option<PathBuf> {
    session_artifact_dir.map(|dir| dir.join(HARNESS_STATE_DIR_NAME))
}

#[must_use]
pub fn get_harness_state_path(harness_state_dir: &Path) -> PathBuf {
    harness_state_dir.join("harness_state.json")
}

/// The settings file the factory opt-in gate reads: the agent dir's
/// settings.json, the same document the kernel-side gate resolves through
/// `PRIME_AGENT_CODING_AGENT_DIR` (the host exports the session's agent
/// dir to the kernel, so both sides read one setting).
pub const FACTORY_SETTINGS_FILE_NAME: &str = "settings.json";

/// The one refusal every gated factory surface raises while the opt-in is
/// off. Byte-identical to the kernel's `FACTORY_DISABLED_MESSAGE`
/// (`prime-agent-runtime/src/rlm/factory.py`), so one exact message pins
/// both sides of the gate.
pub const FACTORY_DISABLED_MESSAGE: &str = "the factory is disabled; run /factory on to enable it";

/// The `factory.enabled` opt-in setting (default off), read leniently from
/// the agent dir's settings.json exactly like the kernel-side
/// `rlm.factory.factory_enabled()`: a missing file or key, a wrong-typed
/// value, or a corrupt document all read as the fail-closed disabled
/// default, so an unreadable setting refuses the factory instead of
/// silently enabling it.
#[must_use]
pub fn factory_enabled(agent_dir: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(agent_dir.join(FACTORY_SETTINGS_FILE_NAME)) else {
        return false;
    };
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    document
        .get("factory")
        .and_then(|factory| factory.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Load harness state; a corrupt or unreadable file degrades to empty rather
/// than throwing (prompt builds run on every turn). Entries are read
/// leniently ([`store::document`]).
#[must_use]
pub fn load_harness_state(harness_state_dir: &Path, scope: HarnessScope) -> HarnessState {
    store::document::read_harness_state_file(&get_harness_state_path(harness_state_dir), scope)
        .state
}

/// Merge global + local states: local ids conflict-prefixed with their scope.
///
/// # Panics
///
/// The `get_mut(kind).unwrap()` cannot panic: the empty state pre-populates every kind map.
#[must_use]
pub fn merge_harness_states(
    global_state: &HarnessState,
    local_state: Option<&HarnessState>,
) -> HarnessState {
    let mut merged = empty_harness_state();
    merged.schema = global_state
        .schema
        .max(local_state.map_or(1, |state| state.schema));
    for kind in REFINEMENT_KINDS {
        let kind_key = kind_from_name(kind);
        let global_entries = &global_state.entries[&kind_key];
        for (id, entry) in global_entries {
            let mut scoped = entry.clone();
            scoped.scope = Some(HarnessScope::Global);
            merged
                .entries
                .get_mut(&kind_key)
                .unwrap()
                .insert(id.clone(), scoped);
        }
        if let Some(local_entries) = local_state.map(|state| &state.entries[&kind_key]) {
            for (id, entry) in local_entries {
                let mut scoped = entry.clone();
                scoped.scope = Some(HarnessScope::Local);
                let merged_id = if merged.entries[&kind_key].contains_key(id) {
                    format!("local:{id}")
                } else {
                    id.clone()
                };
                merged
                    .entries
                    .get_mut(&kind_key)
                    .unwrap()
                    .insert(merged_id, scoped);
            }
        }
    }
    merged.refinements.clone_from(&global_state.refinements);
    if let Some(local_state) = local_state {
        merged.refinements.extend(local_state.refinements.clone());
    }
    merged
}

/// Atomically save harness state ([`store::document`]: the destination's
/// mode is kept, a new file is owner-only, a symlinked file is written
/// through, and a file that does not parse is copied aside first).
///
/// # Errors
///
/// Error when the directory cannot be created, serialization fails, or the atomic write fails.
pub fn save_harness_state(
    harness_state_dir: &Path,
    state: &HarnessState,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(harness_state_dir)?;
    store::document::write_harness_state_file(&get_harness_state_path(harness_state_dir), state)
}

/// Take the harness state file's lock ([`lock::lock_harness_state_file`]
/// with [`lock::HARNESS_STATE_LOCK`]), creating the directory. Every writer
/// of the file takes it around its re-read and save: the kernel's harness
/// store, refine, the RAVO commit and the ledger flush.
///
/// # Errors
///
/// Error when the directory cannot be created or one holder kept the lock
/// past the wait.
pub fn lock_harness_state(
    harness_state_dir: &Path,
) -> anyhow::Result<crate::platform::HeartbeatLock> {
    Ok(lock::lock_harness_state_file(
        &get_harness_state_path(harness_state_dir),
        lock::HARNESS_STATE_LOCK,
    )?)
}

/// Locked read-modify-write of the harness state file: both writer sides
/// (the kernel and refine) take `{file}.lock` around the reload and the save.
///
/// # Errors
///
/// Error when the directory cannot be created, the state lock cannot be acquired, or the atomic write fails.
pub fn update_harness_state<R>(
    harness_state_dir: &Path,
    scope: HarnessScope,
    update: impl FnOnce(&mut HarnessState) -> R,
) -> anyhow::Result<(R, PathBuf)> {
    let lock = lock_harness_state(harness_state_dir)?;
    let mut state = load_harness_state(harness_state_dir, scope);
    let result = update(&mut state);
    lock.ensure_owned()?;
    let written = save_harness_state(harness_state_dir, &state)?;
    Ok((result, written))
}

#[must_use]
pub fn get_refinement_history_path(harness_state_dir: &Path) -> PathBuf {
    harness_state_dir.join(REFINEMENT_HISTORY_FILE_NAME)
}

/// One refinement outcome (applied-edit record), persisted for rollback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinementResult {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub applied_edits: Vec<AppliedRefinementEdit>,
    pub harness_state_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<HarnessScope>,
    /// Keys this crate does not model (a refinement gate's report, written
    /// by an installed feature), carried through untouched.
    #[serde(flatten)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// One applied (or failed) edit with before/after snapshots: the TS
/// `AppliedRefinementEdit extends RefinementEdit` shape — the planned
/// edit's own fields ride along with the snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedRefinementEdit {
    pub action: RefinementAction,
    pub kind: RefinementKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<HarnessEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<HarnessEntry>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AppliedRefinementEdit {
    /// Start one applied-edit row from a planned edit (the TS wire shape
    /// carries the plan's own fields); the applying branch fills the
    /// action/kind/id resolution and the outcome fields.
    fn planned(
        edit: &planner::RefinementEdit,
        action: RefinementAction,
        kind: RefinementKind,
        id: String,
    ) -> Self {
        Self {
            action,
            kind,
            id,
            title: edit.title.clone(),
            content: edit.content.clone(),
            path: edit.path.clone(),
            reference: edit.reference.clone(),
            arguments: edit.arguments.clone(),
            metadata: edit.metadata.clone(),
            before: None,
            after: None,
            applied: false,
            error: None,
            reason: edit.reason.clone(),
        }
    }
}

#[must_use]
pub fn infer_refinement_result_scope(result: &RefinementResult) -> Option<HarnessScope> {
    if let Some(scope) = result.scope {
        return Some(scope);
    }
    let mut scopes: Vec<HarnessScope> = Vec::new();
    for edit in &result.applied_edits {
        let scope = edit
            .after
            .as_ref()
            .or(edit.before.as_ref())
            .and_then(|entry| entry.scope);
        if let Some(scope) = scope {
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
    }
    (scopes.len() == 1).then(|| scopes[0])
}

/// Append a refinement to the global history log (JSONL).
///
/// # Errors
///
/// Error when the directory cannot be created, serialization fails, or the
/// history append fails.
pub fn append_global_refinement(
    harness_state_dir: &Path,
    result: &RefinementResult,
) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(harness_state_dir)?;
    let history_path = get_refinement_history_path(harness_state_dir);
    let mut line = serde_json::to_string(result)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)?;
    file.write_all(line.as_bytes())?;
    Ok(history_path)
}

/// Load the global refinement history; malformed lines are skipped.
#[must_use]
pub fn load_global_refinement_history(harness_state_dir: &Path) -> Vec<RefinementResult> {
    let history_path = get_refinement_history_path(harness_state_dir);
    let Ok(content) = std::fs::read_to_string(&history_path) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(mut result) = serde_json::from_str::<RefinementResult>(trimmed) {
            if result.scope.is_none() {
                result.scope = Some(HarnessScope::Global);
            }
            results.push(result);
        }
    }
    results
}

/// Merge global and session history by id; session entries win, inheriting an
/// existing scope when they carry none.
#[must_use]
pub fn merge_refinement_history(
    global: &[RefinementResult],
    session: &[RefinementResult],
) -> Vec<RefinementResult> {
    let mut by_id: std::collections::BTreeMap<String, RefinementResult> =
        std::collections::BTreeMap::default();
    for result in global {
        by_id.insert(result.id.clone(), result.clone());
    }
    for result in session {
        let entry = match by_id.get(&result.id) {
            Some(existing) if result.scope.is_none() && existing.scope.is_some() => {
                let mut merged = result.clone();
                merged.scope = existing.scope;
                merged
            }
            _ => result.clone(),
        };
        by_id.insert(result.id.clone(), entry);
    }
    by_id.into_values().collect()
}

pub(crate) fn compact_text(text: &str, max_length: usize) -> String {
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_length {
        return normalized;
    }
    let keep = max_length.saturating_sub(3);
    let truncated: String = normalized.chars().take(keep).collect();
    format!("{truncated}...")
}

/// Digest-notation notice body for a refinement.
#[must_use]
pub fn format_refinement_notice_body(result: &RefinementResult) -> String {
    let mut lines = vec![compact_text(
        &result.summary,
        DEFAULT_OVERVIEW_CONTENT_LIMIT,
    )];
    for edit in &result.applied_edits {
        if !edit.applied {
            continue;
        }
        let entry = edit.after.as_ref().or(edit.before.as_ref());
        let scope = entry
            .and_then(|entry| entry.scope)
            .or(result.scope)
            .unwrap_or(HarnessScope::Local);
        let title = entry.map_or(edit.id.as_str(), |entry| entry.title.as_str());
        let content = entry
            .map(|entry| entry.content.as_str())
            .unwrap_or_default();
        lines.push(format!(
            "- {} {} [{}] {}: {}",
            action_name(edit.action),
            kind_name(edit.kind),
            scope_prefix(scope, &edit.id),
            title,
            compact_text(content, DEFAULT_OVERVIEW_CONTENT_LIMIT)
        ));
    }
    lines.join("\n")
}

fn action_name(action: RefinementAction) -> &'static str {
    match action {
        RefinementAction::Create => "create",
        RefinementAction::Update => "update",
        RefinementAction::Delete => "delete",
    }
}

fn kind_name(kind: RefinementKind) -> &'static str {
    match kind {
        RefinementKind::Prompt => "prompt",
        RefinementKind::Memory => "memory",
        RefinementKind::Skill => "skill",
        RefinementKind::Subagent => "subagent",
        RefinementKind::Factory => "factory",
    }
}

fn scope_prefix(scope: HarnessScope, id: &str) -> String {
    format!(
        "{}:{id}",
        match scope {
            HarnessScope::Local => "local",
            HarnessScope::Global => "global",
        }
    )
}

pub mod entries;
pub mod executor;
pub mod gate;
pub mod lock;
pub mod package_harness;
pub mod planner;
pub mod prompt_hook;
pub mod ranking;
pub mod relocate;
pub mod store;

// Export the compact-text helper for the digest formatter.
pub(crate) use compact_text as compact_harness_text;

#[cfg(test)]
mod tests {
    use super::*;

    /// A refine's load/save round trip keeps every top-level key it does not
    /// model (the fork runtime's `ravo` / `failures` / `trustWindows` state):
    /// losing them silently erased other producers' data.
    #[test]
    fn a_save_round_trip_keeps_unmodeled_top_level_state() {
        let dir = tempfile::tempdir().unwrap();
        let extra = serde_json::json!({
            "schema": 1,
            "entries": {"prompt": {}, "memory": {}, "skill": {}, "subagent": {}, "factory": {}},
            "refinements": [],
            "ravo": {"gates": [{"id": "g1", "status": "pass"}]},
            "failures": [{"signature": "sig", "count": 3}],
            "trustWindows": {"global": 0.5}
        });
        std::fs::write(
            get_harness_state_path(dir.path()),
            serde_json::to_string(&extra).unwrap(),
        )
        .unwrap();
        let state = load_harness_state(dir.path(), HarnessScope::Global);
        save_harness_state(dir.path(), &state).unwrap();
        let saved: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(get_harness_state_path(dir.path())).unwrap(),
        )
        .unwrap();
        for key in ["ravo", "failures", "trustWindows"] {
            assert_eq!(saved[key], extra[key], "{key} survives the round trip");
        }
    }

    /// An entry key this crate does not model (the fork's per-entry
    /// `trust`) survives a load/save round trip in place: dropping it
    /// silently reset another producer's measurements on every refine save.
    #[test]
    fn a_save_round_trip_keeps_unmodeled_entry_keys() {
        let dir = tempfile::tempdir().unwrap();
        let document = serde_json::json!({
            "schema": 1,
            "entries": {
                "prompt": {}, "memory": {},
                "skill": {"s1": {
                    "id": "s1", "kind": "skill", "title": "S", "content": "c", "path": "general",
                    "scope": "global", "reference": {}, "arguments": {}, "metadata": {},
                    "source": "refine", "created_at": "t0", "updated_at": "t1", "version": 2,
                    "trust": {"score": 35, "updated_at": "t2", "events": []}
                }},
                "subagent": {}, "factory": {}
            },
            "refinements": []
        });
        std::fs::write(
            get_harness_state_path(dir.path()),
            serde_json::to_string_pretty(&document).unwrap(),
        )
        .unwrap();
        let state = load_harness_state(dir.path(), HarnessScope::Global);
        save_harness_state(dir.path(), &state).unwrap();
        let saved = std::fs::read_to_string(get_harness_state_path(dir.path())).unwrap();
        assert_eq!(
            saved,
            format!("{}\n", serde_json::to_string_pretty(&document).unwrap())
        );
    }

    fn entry(id: &str, kind: RefinementKind, scope: HarnessScope, content: &str) -> HarnessEntry {
        HarnessEntry {
            id: id.to_string(),
            kind,
            title: format!("Entry {id}"),
            content: content.to_string(),
            path: format!("/h/{id}"),
            scope: Some(scope),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
            updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            version: 1,
            extensions: serde_json::Map::new(),
        }
    }

    #[test]
    fn factory_opt_in_reads_leniently_and_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        // No settings file: the opt-in default is disabled.
        assert!(!factory_enabled(tmp.path()));
        // The enabled shape the settings surface writes.
        std::fs::write(
            tmp.path().join(FACTORY_SETTINGS_FILE_NAME),
            r#"{"factory": {"enabled": true}, "compaction": {"enabled": true}}"#,
        )
        .unwrap();
        assert!(factory_enabled(tmp.path()));
        // An explicit false stays disabled.
        std::fs::write(
            tmp.path().join(FACTORY_SETTINGS_FILE_NAME),
            r#"{"factory": {"enabled": false}}"#,
        )
        .unwrap();
        assert!(!factory_enabled(tmp.path()));
        // A wrong-typed value reads as unset, which means disabled.
        std::fs::write(
            tmp.path().join(FACTORY_SETTINGS_FILE_NAME),
            r#"{"factory": {"enabled": "yes"}, "factory.enabled": true}"#,
        )
        .unwrap();
        assert!(!factory_enabled(tmp.path()));
        // A missing key is unset.
        std::fs::write(
            tmp.path().join(FACTORY_SETTINGS_FILE_NAME),
            r#"{"compaction": {}}"#,
        )
        .unwrap();
        assert!(!factory_enabled(tmp.path()));
        // A corrupt document reads as the disabled default, never a crash.
        std::fs::write(tmp.path().join(FACTORY_SETTINGS_FILE_NAME), "{ not json").unwrap();
        assert!(!factory_enabled(tmp.path()));
        // The refusal is the one exact message, byte-identical to the
        // kernel-side FACTORY_DISABLED_MESSAGE (rlm/factory.py).
        assert_eq!(
            FACTORY_DISABLED_MESSAGE,
            "the factory is disabled; run /factory on to enable it"
        );
    }

    #[test]
    fn state_round_trips_and_degrades_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = get_global_harness_state_dir(tmp.path());

        let state = load_harness_state(&dir, HarnessScope::Global);
        assert!(state.refinements.is_empty());

        let mut state = empty_harness_state();
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m1".to_string(),
                entry("m1", RefinementKind::Memory, HarnessScope::Global, "a fact"),
            );
        save_harness_state(&dir, &state).unwrap();
        let loaded = load_harness_state(&dir, HarnessScope::Global);
        assert_eq!(
            loaded.entries[&RefinementKind::Memory]["m1"].content,
            "a fact"
        );

        std::fs::write(get_harness_state_path(&dir), "not json").unwrap();
        assert!(
            load_harness_state(&dir, HarnessScope::Global).entries[&RefinementKind::Memory]
                .is_empty()
        );
    }

    /// The save lands exactly the pretty document; the store's writer syncs
    /// the file and its directory on every save (no opt-in branch to skip).
    #[test]
    fn harness_save_lands_the_exact_document() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = get_global_harness_state_dir(tmp.path());
        let mut state = empty_harness_state();
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m1".to_string(),
                entry("m1", RefinementKind::Memory, HarnessScope::Global, "a fact"),
            );
        let expected = format!("{}\n", serde_json::to_string_pretty(&state).unwrap());
        let written = save_harness_state(&dir, &state).unwrap();
        assert_eq!(written, get_harness_state_path(&dir));
        assert_eq!(
            std::fs::read_to_string(get_harness_state_path(&dir)).unwrap(),
            expected
        );
    }

    #[test]
    fn concurrent_update_harness_state_writes_all_land() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = get_global_harness_state_dir(tmp.path());
        std::thread::scope(|scope| {
            for worker in 0..8u32 {
                let dir = &dir;
                scope.spawn(move || {
                    for i in 0..10u32 {
                        let id = format!("m-{worker}-{i}");
                        update_harness_state(dir, HarnessScope::Global, |state| {
                            let memories = state.entries.get_mut(&RefinementKind::Memory).unwrap();
                            memories.insert(
                                id.clone(),
                                entry(&id, RefinementKind::Memory, HarnessScope::Global, "a fact"),
                            );
                            memories.len()
                        })
                        .unwrap();
                    }
                });
            }
        });
        let loaded = load_harness_state(&dir, HarnessScope::Global);
        assert_eq!(loaded.entries[&RefinementKind::Memory].len(), 80);
    }

    #[test]
    fn merge_prefixed_local_conflicts() {
        let mut global = empty_harness_state();
        global
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m1".to_string(),
                entry(
                    "m1",
                    RefinementKind::Memory,
                    HarnessScope::Global,
                    "global fact",
                ),
            );
        let mut local = empty_harness_state();
        local
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m1".to_string(),
                entry(
                    "m1",
                    RefinementKind::Memory,
                    HarnessScope::Local,
                    "local fact",
                ),
            );
        local
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m2".to_string(),
                entry(
                    "m2",
                    RefinementKind::Memory,
                    HarnessScope::Local,
                    "local only",
                ),
            );
        let merged = merge_harness_states(&global, Some(&local));
        let memories = &merged.entries[&RefinementKind::Memory];
        assert_eq!(memories.len(), 3);
        assert_eq!(memories["m1"].scope, Some(HarnessScope::Global));
        assert_eq!(memories["local:m1"].scope, Some(HarnessScope::Local));
        assert_eq!(memories["m2"].content, "local only");
    }

    #[test]
    fn history_append_load_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = get_global_harness_state_dir(tmp.path());
        let result = RefinementResult {
            id: "r1".to_string(),
            summary: "add a memory".to_string(),
            rationale: "reused twice".to_string(),
            expected_outcome: "faster routing".to_string(),
            applied_edits: vec![],
            harness_state_path: get_harness_state_path(&dir).display().to_string(),
            rollback_of: None,
            scope: None,
            extensions: serde_json::Map::new(),
        };
        append_global_refinement(&dir, &result).unwrap();
        let loaded = load_global_refinement_history(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].scope, Some(HarnessScope::Global));

        let mut session_result = result;
        session_result.summary = "session version".to_string();
        let merged = merge_refinement_history(&loaded, &[session_result]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].summary, "session version");
        assert_eq!(merged[0].scope, Some(HarnessScope::Global));
    }

    #[test]
    fn notice_body_digest_notation() {
        let result = RefinementResult {
            id: "r1".to_string(),
            summary: "created a memory about the flaky test".to_string(),
            rationale: String::new(),
            expected_outcome: String::new(),
            applied_edits: vec![AppliedRefinementEdit {
                action: RefinementAction::Create,
                kind: RefinementKind::Memory,
                id: "m1".to_string(),
                before: None,
                after: Some(entry(
                    "m1",
                    RefinementKind::Memory,
                    HarnessScope::Global,
                    "dup tests are flaky",
                )),
                applied: true,
                error: None,
                reason: None,
                title: None,
                content: None,
                path: None,
                reference: None,
                arguments: None,
                metadata: None,
            }],
            harness_state_path: String::new(),
            rollback_of: None,
            scope: None,
            extensions: serde_json::Map::new(),
        };
        let body = format_refinement_notice_body(&result);
        assert!(body.starts_with("created a memory about the flaky test"));
        assert!(body.contains("- create memory [global:m1] Entry m1: dup tests are flaky"));

        assert_eq!(
            infer_refinement_result_scope(&result),
            Some(HarnessScope::Global)
        );
    }
}
