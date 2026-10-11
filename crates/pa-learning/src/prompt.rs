//! The trajectory's two levers on the session (TS `agent-session.ts`
//! `_harnessTrajectoryBias` / `_trajectoryInternalizedReminders`):
//!
//! - Lever 1, the digest: entries joined to a stable-gap fingerprint render
//!   first, then new ones; internalized ones sink below the unlabelled, past
//!   the rendered slots; up to three confound-flagged stable-gap lines are
//!   surfaced in their own section.
//! - Lever 2, the reminders: a fingerprint labelled DROPPED (internalized)
//!   queues no recurrence refine, unless it is a security class (never
//!   muted) or recurs live in the session's own ledger.
//!
//! Both read only the sealed `trajectory.json` through its stat cache, and
//! both are absent while the kill switch is off or nothing has been sealed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use pa_core::features::SessionFeatureContext;
use pa_core::refinement::HarnessState;
use pa_core::refinement::prompt_hook::{
    HarnessPromptAdjustment,
    HarnessPromptHook,
    HarnessPromptSection,
};
use serde_json::Value;

use crate::store::read_trajectory_index;
use crate::trajectory::{
    PRIME_CORPUS,
    TrajectoryLabelKind,
    TrajectoryStoreFile,
    trajectory_index_enabled_from_env,
};

/// Heading of the digest's trajectory section.
pub const TRAJECTORY_SECTION_HEADING: &str =
    "engineer trajectory (confound-flagged; local signal, may reflect task-mix):";
/// At most this many stable-gap lines reach the digest.
pub const MAX_TRAJECTORY_LINES: usize = 3;

/// How the trajectory classes a harness entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryClass {
    StableGap,
    New,
    Internalized,
}

impl EntryClass {
    /// The lead sort key in the digest: stable gaps first, then new, then
    /// the unlabelled (0), then internalized.
    fn rank(self) -> i64 {
        match self {
            Self::StableGap => -2,
            Self::New => -1,
            Self::Internalized => 1,
        }
    }
}

/// Only explicit ledger fingerprints reach the session; `span:<hash>` keys
/// drive the CLI table only.
fn is_explicit_ledger_fingerprint(fingerprint: &str) -> bool {
    !fingerprint.starts_with("span:")
}

/// Fingerprints labelled DROPPED (internalized), security-class ones
/// excluded (TS `trajectoryInternalizedFingerprints`).
#[must_use]
pub fn trajectory_internalized_fingerprints(file: Option<&TrajectoryStoreFile>) -> HashSet<String> {
    file.into_iter()
        .flat_map(|file| &file.labels)
        .filter(|label| {
            label.corpus == PRIME_CORPUS
                && label.label == Some(TrajectoryLabelKind::Dropped)
                && !label.security_class
                && is_explicit_ledger_fingerprint(&label.fingerprint)
        })
        .map(|label| label.fingerprint.clone())
        .collect()
}

/// The entry `id` of a `<kind>:<id>` reference.
fn entry_ref_id(reference: &str) -> Option<&str> {
    let (kind, id) = reference.split_once(':')?;
    (!kind.is_empty() && !id.is_empty()).then_some(id)
}

/// Map labelled fingerprints to the harness entries that address them,
/// through the trust windows in the state (`proposalId` → claimed
/// fingerprints → touched entry refs, and the failure ledger's
/// `addressedByProposalIds`); keyed by merged entry id (TS
/// `trajectoryClassForEntries`).
#[must_use]
pub fn trajectory_class_for_entries(
    file: &TrajectoryStoreFile,
    state: &HarnessState,
) -> HashMap<String, EntryClass> {
    let mut class_of: HashMap<String, EntryClass> = HashMap::new();
    let Some(windows) = state
        .extensions
        .get("trustWindows")
        .and_then(Value::as_object)
    else {
        return class_of;
    };
    let mut fingerprint_class: HashMap<&str, EntryClass> = HashMap::new();
    for label in &file.labels {
        let Some(kind) = label.label else {
            continue;
        };
        if label.corpus != PRIME_CORPUS || !is_explicit_ledger_fingerprint(&label.fingerprint) {
            continue;
        }
        // A security class is never demoted to internalized for ordering.
        let class = match kind {
            TrajectoryLabelKind::Dropped if label.security_class => continue,
            TrajectoryLabelKind::Dropped => EntryClass::Internalized,
            TrajectoryLabelKind::New => EntryClass::New,
            TrajectoryLabelKind::Persists => EntryClass::StableGap,
        };
        fingerprint_class.insert(label.fingerprint.as_str(), class);
    }
    if fingerprint_class.is_empty() {
        return class_of;
    }
    let strings = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let mut apply = |window: &Value, fingerprint: &str| {
        let Some(class) = fingerprint_class.get(fingerprint).copied() else {
            return;
        };
        for reference in strings(window.get("touched")) {
            if let Some(id) = entry_ref_id(&reference) {
                class_of
                    .entry(id.to_string())
                    .and_modify(|existing| *existing = (*existing).min(class))
                    .or_insert(class);
            }
        }
    };
    for window in windows.values() {
        for fingerprint in strings(window.get("claimedFingerprints")) {
            apply(window, &fingerprint);
        }
    }
    let failures = state
        .extensions
        .get("failures")
        .and_then(|failures| failures.get("failures"))
        .and_then(Value::as_object);
    for (fingerprint, record) in failures.into_iter().flatten() {
        if !fingerprint_class.contains_key(fingerprint.as_str()) {
            continue;
        }
        for proposal_id in strings(record.get("addressedByProposalIds")) {
            if let Some(window) = windows.get(&proposal_id) {
                apply(window, fingerprint);
            }
        }
    }
    class_of
}

/// The stable-gap residue as raw, confound-tagged lines, most recurring
/// first (TS `formatTrajectoryLines`); the digest sanitizes each.
#[must_use]
pub fn format_trajectory_lines(file: &TrajectoryStoreFile, max: usize) -> Vec<String> {
    let mut stable: Vec<_> = file
        .labels
        .iter()
        .filter(|label| {
            label.corpus == PRIME_CORPUS
                && label.label == Some(TrajectoryLabelKind::Persists)
                && is_explicit_ledger_fingerprint(&label.fingerprint)
        })
        .collect();
    stable.sort_by(|left, right| {
        right
            .windows_recurring
            .cmp(&left.windows_recurring)
            .then_with(|| pa_ravo::locale_compare(&left.fingerprint, &right.fingerprint))
    });
    stable
        .into_iter()
        .take(max)
        .map(|label| {
            let name = if label.name.is_empty() {
                &label.fingerprint
            } else {
                &label.name
            };
            format!(
                "stable-gap: {name} recurs in {} of {} windows [confounds: {}]",
                label.windows_recurring,
                file.windows_observed,
                label.confounds.join(", ")
            )
        })
        .collect()
}

/// The digest adjustment for `state` from a sealed trajectory.
#[must_use]
pub fn trajectory_prompt_adjustment(
    file: &TrajectoryStoreFile,
    state: &HarnessState,
) -> HarnessPromptAdjustment {
    let entry_rank: BTreeMap<String, i64> = trajectory_class_for_entries(file, state)
        .into_iter()
        .map(|(id, class)| (id, class.rank()))
        .collect();
    HarnessPromptAdjustment {
        entry_rank,
        withheld: Vec::new(),
        sections: vec![HarnessPromptSection {
            heading: TRAJECTORY_SECTION_HEADING.to_string(),
            lines: format_trajectory_lines(file, MAX_TRAJECTORY_LINES),
        }],
    }
}

/// Lever 1: the trajectory of one agent dir, applied to every digest
/// render.
#[derive(Debug, Clone)]
pub struct TrajectoryPromptHook {
    agent_dir: PathBuf,
}

impl TrajectoryPromptHook {
    #[must_use]
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            agent_dir: agent_dir.to_path_buf(),
        }
    }
}

impl HarnessPromptHook for TrajectoryPromptHook {
    fn adjust(&self, state: &HarnessState) -> HarnessPromptAdjustment {
        if !trajectory_index_enabled_from_env() {
            return HarnessPromptAdjustment::default();
        }
        read_trajectory_index(&self.agent_dir)
            .map(|file| trajectory_prompt_adjustment(&file, state))
            .unwrap_or_default()
    }
}

/// Lever 2: the internalized fingerprints of a session's agent dir, minus
/// those recurring live in its own ledger.
#[derive(Debug, Clone, Copy, Default)]
pub struct InternalizedReminders;

impl pa_ravo::RecurrenceFilter for InternalizedReminders {
    fn muted(&self, context: &SessionFeatureContext, live_recurring: &[String]) -> HashSet<String> {
        if !trajectory_index_enabled_from_env() {
            return HashSet::new();
        }
        let mut muted = trajectory_internalized_fingerprints(
            read_trajectory_index(&context.agent_dir).as_ref(),
        );
        for id in live_recurring {
            muted.remove(id);
        }
        muted
    }
}
