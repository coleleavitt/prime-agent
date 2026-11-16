//! The brain's own state, as two extension keys of the global
//! `harness_state.json` (every harness writer round-trips unknown top-level
//! keys untouched): `memoryControls` (pins, excludes, recorded
//! supersessions) and `memorySynthesis` (one synthesized summary per scope,
//! claude.ai web's `{memory, controls, updated_at}` contract with a
//! `synthesized` marker). Writes take the file's lock through
//! [`update_harness_state`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use pa_core::refinement::{
    HarnessScope,
    HarnessState,
    get_global_harness_state_dir,
    get_harness_state_path,
    load_harness_state,
    update_harness_state,
};
use serde::{Deserialize, Serialize};

/// The extension key of [`MemoryControls`].
pub(crate) const CONTROLS_KEY: &str = "memoryControls";
/// The extension key of [`MemorySynthesis`].
pub(crate) const SYNTHESIS_KEY: &str = "memorySynthesis";
/// The synthesis scope of the cross-project summary.
pub(crate) const GLOBAL_SCOPE: &str = "global";

/// User and model controls over which memories surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryControls {
    /// Keys always listed first in the digest and boosted in search.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) pinned: BTreeSet<String>,
    /// Keys never listed in the digest nor returned by search.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) excluded: BTreeSet<String>,
    /// Recorded supersessions: old key -> replacing key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) superseded: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
}

/// One scope's synthesized summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScopeSummary {
    /// One-line bullets, at most [`super::synth::MAX_LINES`].
    #[serde(default)]
    pub(crate) lines: Vec<String>,
    /// Always `true`: the text came from a synthesis, not a user edit.
    #[serde(default)]
    pub(crate) synthesized: bool,
    #[serde(default)]
    pub(crate) model: String,
    /// When the summary was last synthesized (empty before the first).
    #[serde(default)]
    pub(crate) updated_at: String,
    /// How many memories the last synthesis read.
    #[serde(default)]
    pub(crate) source_count: usize,
    /// Consecutive failed automatic syntheses (reset by a success).
    #[serde(default)]
    pub(crate) failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_attempt_at: Option<String>,
}

/// Every scope's summary, by scope key (`global`, `git:<root commit>`,
/// `slug:<project slug>`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemorySynthesis {
    #[serde(default)]
    pub(crate) scopes: BTreeMap<String, ScopeSummary>,
}

/// Both keys of one state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BrainState {
    pub(crate) controls: MemoryControls,
    pub(crate) synthesis: MemorySynthesis,
}

fn extension<T: for<'de> Deserialize<'de> + Default>(state: &HarnessState, key: &str) -> T {
    state
        .extensions
        .get(key)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

fn put_extension<T: Serialize + Default + PartialEq>(
    state: &mut HarnessState,
    key: &str,
    value: &T,
) {
    if *value == T::default() {
        state.extensions.remove(key);
    } else if let Ok(json) = serde_json::to_value(value) {
        state.extensions.insert(key.to_string(), json);
    }
}

/// The brain state of `agent_dir`'s global harness file.
pub(crate) fn read(agent_dir: &Path) -> BrainState {
    let state = load_harness_state(
        &get_global_harness_state_dir(agent_dir),
        HarnessScope::Global,
    );
    BrainState {
        controls: extension(&state, CONTROLS_KEY),
        synthesis: extension(&state, SYNTHESIS_KEY),
    }
}

/// Locked read-modify-write of the brain state; other keys of the file are
/// re-read under the lock and kept.
pub(crate) fn update<R>(
    agent_dir: &Path,
    change: impl FnOnce(&mut BrainState) -> R,
) -> anyhow::Result<R> {
    let (result, _) = update_harness_state(
        &get_global_harness_state_dir(agent_dir),
        HarnessScope::Global,
        |state| {
            let mut brain = BrainState {
                controls: extension(state, CONTROLS_KEY),
                synthesis: extension(state, SYNTHESIS_KEY),
            };
            let result = change(&mut brain);
            put_extension(state, CONTROLS_KEY, &brain.controls);
            put_extension(state, SYNTHESIS_KEY, &brain.synthesis);
            result
        },
    )?;
    Ok(result)
}

/// One cached read: the file, its modification time and length, the state.
type CachedRead = (PathBuf, Option<SystemTime>, u64, BrainState);

/// The brain state behind an mtime check, for the digest's render path.
#[derive(Debug, Default)]
pub(crate) struct CachedState {
    cached: Mutex<Option<CachedRead>>,
}

impl CachedState {
    /// The state of `agent_dir`, re-read only when the file changed.
    pub(crate) fn get(&self, agent_dir: &Path) -> BrainState {
        let path = get_harness_state_path(&get_global_harness_state_dir(agent_dir));
        let metadata = std::fs::metadata(&path).ok();
        let modified = metadata.as_ref().and_then(|meta| meta.modified().ok());
        let len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_path, cached_modified, cached_len, state)) = cached.as_ref()
            && *cached_path == path
            && *cached_modified == modified
            && *cached_len == len
        {
            return state.clone();
        }
        let state = read(agent_dir);
        *cached = Some((path, modified, len, state.clone()));
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_round_trip_beside_the_harness_keys() {
        let agent = tempfile::tempdir().unwrap();
        let dir = agent.path().join("harness");
        crate::brain::corpus::tests::write_harness(
            &dir,
            &[("g1", "t", "c", serde_json::json!({}))],
        );
        update(agent.path(), |brain| {
            brain.controls.pinned.insert("global:g1".to_string());
            brain.synthesis.scopes.insert(
                GLOBAL_SCOPE.to_string(),
                ScopeSummary {
                    lines: vec!["one".to_string()],
                    synthesized: true,
                    ..ScopeSummary::default()
                },
            );
        })
        .unwrap();
        let state = read(agent.path());
        assert!(state.controls.pinned.contains("global:g1"));
        assert_eq!(state.synthesis.scopes[GLOBAL_SCOPE].lines, ["one"]);
        let raw = load_harness_state(&dir, HarnessScope::Global);
        assert!(
            raw.entries[&pa_core::refinement::RefinementKind::Memory].contains_key("g1"),
            "the entries survive the brain write"
        );

        update(agent.path(), |brain| *brain = BrainState::default()).unwrap();
        let raw = load_harness_state(&dir, HarnessScope::Global);
        assert!(!raw.extensions.contains_key(CONTROLS_KEY));
        assert!(!raw.extensions.contains_key(SYNTHESIS_KEY));
    }

    #[test]
    fn the_cache_rereads_after_a_write() {
        let agent = tempfile::tempdir().unwrap();
        crate::brain::corpus::tests::write_harness(&agent.path().join("harness"), &[]);
        let cache = CachedState::default();
        assert_eq!(cache.get(agent.path()), BrainState::default());
        update(agent.path(), |brain| {
            brain.controls.excluded.insert("mc:1".to_string());
        })
        .unwrap();
        assert!(cache.get(agent.path()).controls.excluded.contains("mc:1"));
    }
}
