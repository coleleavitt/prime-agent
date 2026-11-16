//! One session's persisted context state,
//! `<session artifacts>/context/state.json`: every decision a request
//! replays (tags, drops, compartments, the frozen history text), so a
//! resumed session renders the same bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const SCHEMA: u32 = 1;

fn schema() -> u32 {
    SCHEMA
}

/// One folded span of history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Compartment {
    /// The first and last message key of the chunk this compartment opens
    /// or closes; only the first compartment of a chunk carries a start key
    /// and only its last an end key (the cut is the union).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) start_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) end_key: Option<String>,
    /// The tag range the historian named.
    pub(crate) start_tag: u32,
    pub(crate) end_tag: u32,
    pub(crate) title: String,
    /// 1..100: how slowly it decays.
    pub(crate) importance: i32,
    /// P1 (every anchor) .. P4 (one fragment).
    pub(crate) tiers: [String; 4],
    /// When the historian published it (ms since the epoch; 0 unknown).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) created_ms: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if signature"
)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The decay-rendered history of the first `covers` applied compartments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Frozen {
    pub(crate) text: String,
    pub(crate) covers: usize,
}

/// The historian's backoff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistorianState {
    #[serde(default)]
    pub(crate) failures: u32,
    #[serde(default)]
    pub(crate) retry_after_ms: u64,
    #[serde(default)]
    pub(crate) runs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

/// The nudge's memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NudgeState {
    /// The live-tail size when the last nudge fired.
    #[serde(default)]
    pub(crate) fired_at_tail: usize,
    /// The band of the last nudge (0 none, 1 gentle, 2 firm, 3 urgent).
    #[serde(default)]
    pub(crate) band: u8,
}

/// The session's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextState {
    #[serde(default = "schema")]
    pub(crate) schema: u32,
    /// The last tag handed out.
    #[serde(default)]
    pub(crate) last_tag: u32,
    /// Message key -> tag.
    #[serde(default)]
    pub(crate) tags: BTreeMap<String, u32>,
    /// Tags rendered as their placeholder.
    #[serde(default)]
    pub(crate) dropped: BTreeSet<u32>,
    /// Tags `ctx.reduce` queued for the next permitted pass.
    #[serde(default)]
    pub(crate) queued: BTreeSet<u32>,
    /// Every published compartment, oldest first.
    #[serde(default)]
    pub(crate) compartments: Vec<Compartment>,
    /// How many of `compartments` requests render (the rest wait for a
    /// permitted pass).
    #[serde(default)]
    pub(crate) applied: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) frozen: Option<Frozen>,
    /// `ctx.flush` asked the next request to apply queued work.
    #[serde(default)]
    pub(crate) flush_requested: bool,
    #[serde(default)]
    pub(crate) historian: HistorianState,
    #[serde(default)]
    pub(crate) nudge: NudgeState,
}

impl Default for ContextState {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            last_tag: 0,
            tags: BTreeMap::new(),
            dropped: BTreeSet::new(),
            queued: BTreeSet::new(),
            compartments: Vec::new(),
            applied: 0,
            frozen: None,
            flush_requested: false,
            historian: HistorianState::default(),
            nudge: NudgeState::default(),
        }
    }
}

impl ContextState {
    /// The tag of `key`, handing out the next one on first sight.
    pub(crate) fn tag_for(&mut self, key: &str) -> u32 {
        if let Some(tag) = self.tags.get(key) {
            return *tag;
        }
        self.last_tag += 1;
        self.tags.insert(key.to_string(), self.last_tag);
        self.last_tag
    }

    /// The key a tag was handed to.
    pub(crate) fn key_of(&self, tag: u32) -> Option<&str> {
        self.tags
            .iter()
            .find(|(_, candidate)| **candidate == tag)
            .map(|(key, _)| key.as_str())
    }

    /// Forget every compartment (the history they covered is gone, e.g.
    /// after a native compaction); tags and drops stay valid.
    pub(crate) fn clear_compartments(&mut self) {
        self.compartments.clear();
        self.applied = 0;
        self.frozen = None;
    }
}

/// Where a session's state lives.
pub(crate) fn path(session_artifact_dir: &Path) -> PathBuf {
    session_artifact_dir.join("context").join("state.json")
}

/// The state at `path`; a missing or unreadable file is a fresh state.
pub(crate) fn load(path: &Path) -> ContextState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Write `state` atomically.
pub(crate) fn save(path: &Path, state: &ContextState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_tags_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path());
        let mut state = load(&file);
        assert_eq!(state, ContextState::default());
        assert_eq!(state.tag_for("t:a"), 1);
        assert_eq!(state.tag_for("u:2"), 2);
        assert_eq!(state.tag_for("t:a"), 1);
        assert_eq!(state.key_of(2), Some("u:2"));
        state.dropped.insert(1);
        state.compartments.push(Compartment {
            start_key: Some("u:2".to_string()),
            end_key: Some("t:a".to_string()),
            start_tag: 1,
            end_tag: 2,
            title: "t".to_string(),
            importance: 50,
            created_ms: 0,
            tiers: ["a".into(), "b".into(), "c".into(), "d".into()],
        });
        save(&file, &state).unwrap();
        assert_eq!(load(&file), state);
        state.clear_compartments();
        assert!(state.compartments.is_empty() && state.frozen.is_none());
        assert!(state.dropped.contains(&1));
    }
}
