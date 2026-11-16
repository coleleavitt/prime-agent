//! The brain's harness prompt hook: pinned memories render first, excluded
//! and superseded ones are withheld (announced by count, so the model knows
//! `brain.search` reaches them), and the synthesized summaries of this
//! project and of all projects render as sections. With no controls, no
//! supersession and no summary the adjustment is empty and the digest is
//! byte-identical to the native one.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use pa_core::refinement::prompt_hook::{
    HarnessPromptAdjustment,
    HarnessPromptHook,
    HarnessPromptSection,
    WithheldHarnessEntries,
};
use pa_core::refinement::{HarnessScope, HarnessState, RefinementKind};

use super::controls::{BrainState, CachedState, GLOBAL_SCOPE};
use super::project::Project;
use super::supersede::{self, Item};

/// The lead sort key of a pinned entry (lower renders first).
const PINNED_RANK: i64 = -1_000;

/// One session's hook.
pub(crate) struct BrainPromptHook {
    pub(crate) agent_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) project: Project,
    pub(crate) state: Arc<CachedState>,
}

impl HarnessPromptHook for BrainPromptHook {
    fn adjust(&self, state: &HarnessState) -> HarnessPromptAdjustment {
        adjustment(
            state,
            &self.state.get(&self.agent_dir),
            &self.session_id,
            &self.project,
        )
    }
}

fn adjustment(
    state: &HarnessState,
    brain: &BrainState,
    session_id: &str,
    project: &Project,
) -> HarnessPromptAdjustment {
    let mut out = HarnessPromptAdjustment::default();
    let memories = &state.entries[&RefinementKind::Memory];
    let keyed: Vec<(&String, String, Option<String>)> = memories
        .iter()
        .map(|(merged_id, entry)| {
            let key_of = |id: &str| match entry.scope {
                Some(HarnessScope::Local) => format!("session:{session_id}:{id}"),
                _ => format!("global:{id}"),
            };
            let declared = entry
                .metadata
                .get("supersededBy")
                .and_then(serde_json::Value::as_str)
                .map(|by| {
                    if by.contains(':') {
                        by.to_string()
                    } else {
                        key_of(by)
                    }
                });
            (merged_id, key_of(&entry.id), declared)
        })
        .collect();

    let mut excluded = BTreeSet::new();
    for (merged_id, key, _) in &keyed {
        if brain.controls.pinned.contains(key) {
            out.entry_rank.insert((*merged_id).clone(), PINNED_RANK);
        }
        if brain.controls.excluded.contains(key) {
            excluded.insert((RefinementKind::Memory, (*merged_id).clone()));
        }
    }

    let items: Vec<Item<'_>> = keyed
        .iter()
        .map(|(merged_id, key, declared)| {
            let entry = &memories[*merged_id];
            Item {
                key,
                name: &entry.id,
                content: &entry.content,
                updated_at: &entry.updated_at,
                declared: declared.as_deref(),
            }
        })
        .collect();
    let superseded = supersede::superseded(&items, &brain.controls.superseded);
    let withheld_superseded: BTreeSet<(RefinementKind, String)> = keyed
        .iter()
        .filter(|(merged_id, key, _)| {
            superseded.contains_key(key)
                && !brain.controls.pinned.contains(key)
                && !excluded.contains(&(RefinementKind::Memory, (*merged_id).clone()))
        })
        .map(|(merged_id, _, _)| (RefinementKind::Memory, (*merged_id).clone()))
        .collect();

    if !excluded.is_empty() {
        out.withheld.push(WithheldHarnessEntries {
            label: "excluded".to_string(),
            note: "excluded with brain.exclude; brain.include restores them".to_string(),
            entries: excluded,
        });
    }
    if !withheld_superseded.is_empty() {
        out.withheld.push(WithheldHarnessEntries {
            label: "superseded".to_string(),
            note: "replaced by newer memories; brain.search(include_superseded=True) finds them"
                .to_string(),
            entries: withheld_superseded,
        });
    }

    let project_key = project.summary_key();
    for (scope, heading) in [
        (
            project_key.as_str(),
            "Synthesized memory, this project (brain.search and brain.get for detail):",
        ),
        (
            GLOBAL_SCOPE,
            "Synthesized memory, all projects (brain.search for detail):",
        ),
    ] {
        if let Some(summary) = brain
            .synthesis
            .scopes
            .get(scope)
            .filter(|summary| !summary.lines.is_empty())
        {
            out.sections.push(HarnessPromptSection {
                heading: heading.to_string(),
                lines: summary.lines.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pa_core::refinement::{HarnessEntry, empty_harness_state, merge_harness_states};

    use super::*;
    use crate::brain::controls::ScopeSummary;

    fn entry(id: &str, content: &str) -> HarnessEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": "memory", "title": id, "content": content,
            "path": format!("memories/{id}.md"), "reference": {}, "arguments": {},
            "metadata": {}, "source": "test", "created_at": "2026-11-01T00:00:00.000Z",
            "updated_at": "2026-11-01T00:00:00.000Z", "version": 1
        }))
        .unwrap()
    }

    fn merged() -> HarnessState {
        let mut global = empty_harness_state();
        let mut local = empty_harness_state();
        let memory = global.entries.get_mut(&RefinementKind::Memory).unwrap();
        for (id, content) in [
            ("a", "alpha"),
            ("b_part_1", "first"),
            ("b_part_2", "second"),
        ] {
            memory.insert(id.to_string(), entry(id, content));
        }
        local
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("a".to_string(), entry("a", "local alpha"));
        merge_harness_states(&global, Some(&local))
    }

    fn project() -> Project {
        Project {
            identity: Some("git:abc".to_string()),
            slug: "-repo".to_string(),
        }
    }

    #[test]
    fn no_brain_state_and_no_series_changes_nothing() {
        let mut global = empty_harness_state();
        global
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert("a".to_string(), entry("a", "alpha"));
        let state = merge_harness_states(&global, None);
        assert!(adjustment(&state, &BrainState::default(), "s1", &project()).is_empty());
    }

    #[test]
    fn pins_exclusions_supersession_and_summaries_render() {
        let mut brain = BrainState::default();
        brain.controls.pinned.insert("session:s1:a".to_string());
        brain.controls.excluded.insert("global:a".to_string());
        brain.synthesis.scopes.insert(
            "git:abc".to_string(),
            ScopeSummary {
                lines: vec!["builds use mold".to_string()],
                synthesized: true,
                ..ScopeSummary::default()
            },
        );
        brain
            .synthesis
            .scopes
            .insert(GLOBAL_SCOPE.to_string(), ScopeSummary::default());
        let out = adjustment(&merged(), &brain, "s1", &project());
        assert_eq!(
            out.entry_rank,
            BTreeMap::from([("local:a".to_string(), PINNED_RANK)])
        );
        assert_eq!(out.withheld.len(), 2);
        assert_eq!(out.withheld[0].label, "excluded");
        assert!(
            out.withheld[0]
                .entries
                .contains(&(RefinementKind::Memory, "a".to_string()))
        );
        assert_eq!(out.withheld[1].label, "superseded");
        assert_eq!(
            out.withheld[1].entries,
            BTreeSet::from([(RefinementKind::Memory, "b_part_1".to_string())])
        );
        assert_eq!(out.sections.len(), 1, "an empty summary renders nothing");
        assert_eq!(out.sections[0].lines, ["builds use mold"]);
    }
}
