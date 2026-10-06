//! Harness entry enable/disable (upstream #1118): a disabled entry stays
//! stored (and rollback-able) but is hidden from the system prompt, so a
//! retired memory or subagent spec stops steering the agent without being
//! deleted. The flag rides the entry's `enabled` key (absent = enabled),
//! the same key the kernel's `rlm.harness.set_enabled` writes.

use std::path::Path;
use std::time::Duration;

use super::{
    get_harness_state_path, load_harness_state, save_harness_state, HarnessEntry, HarnessScope,
    RefinementKind, REFINEMENT_KINDS,
};

/// The entry key the flag lives under.
pub const HARNESS_ENTRY_ENABLED_KEY: &str = "enabled";

impl HarnessEntry {
    /// Entries are enabled unless explicitly disabled, so state written
    /// before the flag existed keeps working.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.extensions.get(HARNESS_ENTRY_ENABLED_KEY) != Some(&serde_json::Value::Bool(false))
    }

    /// Set the flag (recorded explicitly either way).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.extensions.insert(
            HARNESS_ENTRY_ENABLED_KEY.to_string(),
            serde_json::Value::Bool(enabled),
        );
    }
}

/// One entry as `/harness` lists it (no content).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEntrySummary {
    pub scope: HarnessScope,
    pub kind: RefinementKind,
    pub id: String,
    pub title: String,
    pub path: String,
    pub enabled: bool,
    pub version: u64,
}

impl HarnessEntrySummary {
    /// The fully qualified `<scope>:<kind>:<id>` key `/harness` accepts.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            scope_name(self.scope),
            kind_name(self.kind),
            self.id
        )
    }
}

fn scope_name(scope: HarnessScope) -> &'static str {
    match scope {
        HarnessScope::Local => "local",
        HarnessScope::Global => "global",
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

fn parse_kind(text: &str) -> Option<RefinementKind> {
    Some(match text {
        "prompt" => RefinementKind::Prompt,
        "memory" => RefinementKind::Memory,
        "skill" => RefinementKind::Skill,
        "subagent" => RefinementKind::Subagent,
        "factory" => RefinementKind::Factory,
        _ => return None,
    })
}

fn parse_scope(text: &str) -> Option<HarnessScope> {
    match text {
        "local" => Some(HarnessScope::Local),
        "global" => Some(HarnessScope::Global),
        _ => None,
    }
}

/// Every entry of the local (when the session has one) and global stores,
/// local first, each sorted by kind then id.
#[must_use]
pub fn list_harness_entries(
    local_dir: Option<&Path>,
    global_dir: &Path,
) -> Vec<HarnessEntrySummary> {
    let mut summaries = Vec::new();
    let stores = local_dir
        .map(|dir| (HarnessScope::Local, dir))
        .into_iter()
        .chain(std::iter::once((HarnessScope::Global, global_dir)));
    for (scope, dir) in stores {
        let state = load_harness_state(dir, scope);
        for kind in REFINEMENT_KINDS.iter().filter_map(|name| parse_kind(name)) {
            for (id, entry) in state.entries.get(&kind).into_iter().flatten() {
                summaries.push(HarnessEntrySummary {
                    scope,
                    kind,
                    id: id.clone(),
                    title: entry.title.clone(),
                    path: entry.path.clone(),
                    enabled: entry.is_enabled(),
                    version: entry.version,
                });
            }
        }
    }
    summaries
}

/// Resolve a `/harness` entry reference: `<id>`, `<kind>:<id>`,
/// `<scope>:<id>`, or `<scope>:<kind>:<id>`. A bare reference that matches
/// several entries asks for the qualified key.
///
/// # Errors
///
/// No entry, or an ambiguous reference (naming the candidates).
pub fn resolve_harness_entry<'a>(
    reference: &str,
    entries: &'a [HarnessEntrySummary],
) -> Result<&'a HarnessEntrySummary, String> {
    let reference = reference.trim();
    let parts: Vec<&str> = reference.splitn(3, ':').collect();
    let (scope, kind, id) = match parts.as_slice() {
        [scope, kind, id] if parse_scope(scope).is_some() && parse_kind(kind).is_some() => {
            (parse_scope(scope), parse_kind(kind), *id)
        }
        [prefix, rest] if parse_scope(prefix).is_some() => (parse_scope(prefix), None, *rest),
        [prefix, rest] if parse_kind(prefix).is_some() => (None, parse_kind(prefix), *rest),
        _ => (None, None, reference),
    };
    let matches: Vec<&HarnessEntrySummary> = entries
        .iter()
        .filter(|entry| {
            entry.id == id
                && scope.is_none_or(|scope| entry.scope == scope)
                && kind.is_none_or(|kind| entry.kind == kind)
        })
        .collect();
    match matches.as_slice() {
        [] => Err(format!("No harness entry matches {reference}")),
        [entry] => Ok(entry),
        many => Err(format!(
            "{reference} matches several harness entries; use one of: {}",
            many.iter()
                .map(|entry| entry.key())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Attempts (5 ms apart) at the store's lock before a write gives up; a
/// lock older than 10 s is stale (the kernel writer's protocol).
const LOCK_ATTEMPTS: u32 = 400;
const LOCK_RETRY: Duration = Duration::from_millis(5);
const LOCK_STALE: Duration = Duration::from_secs(10);

/// Flip one entry's flag in the store at `dir` under the store's lock (the
/// same lock the kernel's harness writes take). Blocking.
///
/// # Errors
///
/// The lock stayed held, the entry is gone, or the write failed.
pub fn set_harness_entry_enabled(
    dir: &Path,
    scope: HarnessScope,
    kind: RefinementKind,
    id: &str,
    enabled: bool,
) -> anyhow::Result<HarnessEntrySummary> {
    std::fs::create_dir_all(dir)?;
    let state_path = get_harness_state_path(dir);
    let mut lock = None;
    for attempt in 0..LOCK_ATTEMPTS {
        match crate::platform::LockDir::acquire(&state_path, LOCK_STALE) {
            Ok(held) => {
                lock = Some(held);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt + 1 < LOCK_ATTEMPTS {
                    std::thread::sleep(LOCK_RETRY);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    let Some(_lock) = lock else {
        anyhow::bail!(
            "harness state is locked by another writer: {}",
            state_path.display()
        );
    };
    let mut state = load_harness_state(dir, scope);
    let entry = state
        .entries
        .get_mut(&kind)
        .and_then(|entries| entries.get_mut(id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No harness entry {}:{}:{id}",
                scope_name(scope),
                kind_name(kind)
            )
        })?;
    entry.set_enabled(enabled);
    entry.updated_at = crate::session::manager::format_iso_now();
    let summary = HarnessEntrySummary {
        scope,
        kind,
        id: id.to_string(),
        title: entry.title.clone(),
        path: entry.path.clone(),
        enabled,
        version: entry.version,
    };
    save_harness_state(dir, &state)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refinement::empty_harness_state;

    fn entry(kind: RefinementKind, id: &str, extensions: &serde_json::Value) -> HarnessEntry {
        HarnessEntry {
            id: id.to_string(),
            kind,
            title: format!("{id} title"),
            content: "body".to_string(),
            path: "general".to_string(),
            scope: None,
            reference: serde_json::Map::new(),
            arguments: serde_json::Map::new(),
            metadata: serde_json::Map::new(),
            source: "refine".to_string(),
            created_at: "t0".to_string(),
            updated_at: "t0".to_string(),
            version: 2,
            extensions: extensions.as_object().cloned().unwrap_or_default(),
        }
    }

    fn store(dir: &Path, entries: Vec<HarnessEntry>) {
        let mut state = empty_harness_state();
        for entry in entries {
            state
                .entries
                .entry(entry.kind)
                .or_default()
                .insert(entry.id.clone(), entry);
        }
        save_harness_state(dir, &state).unwrap();
    }

    #[test]
    fn the_flag_defaults_on_and_round_trips_through_the_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = tmp.path().join("local");
        let global = tmp.path().join("global");
        store(
            &local,
            vec![entry(
                RefinementKind::Subagent,
                "reviewer",
                &serde_json::json!({}),
            )],
        );
        store(
            &global,
            vec![
                entry(
                    RefinementKind::Memory,
                    "stale",
                    &serde_json::json!({ "enabled": false }),
                ),
                entry(
                    RefinementKind::Subagent,
                    "reviewer",
                    &serde_json::json!({ "trust": 1 }),
                ),
            ],
        );
        let entries = list_harness_entries(Some(&local), &global);
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.key(), entry.enabled))
                .collect::<Vec<_>>(),
            vec![
                ("local:subagent:reviewer".to_string(), true),
                ("global:memory:stale".to_string(), false),
                ("global:subagent:reviewer".to_string(), true),
            ]
        );
        // A bare id that names two entries asks for the qualified key.
        assert_eq!(
            resolve_harness_entry("reviewer", &entries).unwrap_err(),
            "reviewer matches several harness entries; use one of: local:subagent:reviewer, global:subagent:reviewer"
        );
        let target = resolve_harness_entry("global:reviewer", &entries).unwrap();
        assert_eq!(target.key(), "global:subagent:reviewer");
        assert_eq!(
            resolve_harness_entry("stale", &entries).unwrap().key(),
            "global:memory:stale"
        );
        assert_eq!(
            resolve_harness_entry("memory:nope", &entries).unwrap_err(),
            "No harness entry matches memory:nope"
        );
        let summary = set_harness_entry_enabled(
            &global,
            HarnessScope::Global,
            RefinementKind::Subagent,
            "reviewer",
            false,
        )
        .unwrap();
        assert!(!summary.enabled);
        let reloaded = load_harness_state(&global, HarnessScope::Global);
        let stored = &reloaded.entries[&RefinementKind::Subagent]["reviewer"];
        assert!(!stored.is_enabled());
        // Other producers' keys ride along untouched.
        assert_eq!(stored.extensions.get("trust"), Some(&serde_json::json!(1)));
        set_harness_entry_enabled(
            &global,
            HarnessScope::Global,
            RefinementKind::Memory,
            "stale",
            true,
        )
        .unwrap();
        assert!(
            load_harness_state(&global, HarnessScope::Global).entries[&RefinementKind::Memory]
                ["stale"]
                .is_enabled()
        );
        assert!(set_harness_entry_enabled(
            &global,
            HarnessScope::Global,
            RefinementKind::Memory,
            "gone",
            true
        )
        .is_err());
    }
}
