//! Superseded harness memories teach flyrag once: one detached `Misled` per
//! memory id the first time the brain sees it superseded, recorded in
//! `<agent dir>/harness/brain_demoted.json` so later ticks do not repeat it.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::PoisonError;

use super::controls;
use super::search::shared_brain;
use crate::Outcome;
use crate::handle::RerankerHandle;

const DEMOTED_FILE: &str = "brain_demoted.json";

/// Demote the newly superseded memories; returns how many.
pub(crate) fn demote_superseded(agent_dir: &Path, handle: &RerankerHandle) -> usize {
    let state = controls::read(agent_dir);
    let ids: BTreeSet<String> = {
        let brain = shared_brain(agent_dir);
        let mut brain = brain.lock().unwrap_or_else(PoisonError::into_inner);
        brain.refresh(&state.controls.superseded);
        brain
            .superseded()
            .keys()
            .filter_map(|key| brain.get(key)?.memory_id.clone())
            .collect()
    };
    let path = agent_dir.join("harness").join(DEMOTED_FILE);
    let mut done: BTreeSet<String> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let fresh: Vec<String> = ids.difference(&done).cloned().collect();
    for id in &fresh {
        handle.record_feedback_detached(id, Outcome::Misled);
    }
    if !fresh.is_empty() {
        done.extend(fresh.iter().cloned());
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_vec_pretty(&done)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(error) = written {
            tracing::warn!(target: "pa_daemon::retrieval", %error, "brain: demoted set not saved");
        }
    }
    fresh.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::corpus::tests::write_harness;

    #[test]
    fn superseded_memories_are_demoted_once() {
        let agent = tempfile::tempdir().unwrap();
        write_harness(
            &agent.path().join("harness"),
            &[
                ("h_part_1", "one", "first handoff", serde_json::json!({})),
                ("h_part_2", "two", "second handoff", serde_json::json!({})),
            ],
        );
        let handle = RerankerHandle::new(crate::FlyRagReranker::new(7).unwrap());
        handle.record_memory_upsert("h_part_1", "first handoff");
        handle.record_memory_upsert("h_part_2", "second handoff");
        assert_eq!(demote_superseded(agent.path(), &handle), 1);
        assert!(handle.learned_prior("h_part_1") < 0.0);
        assert_eq!(demote_superseded(agent.path(), &handle), 0, "not repeated");
    }
}
