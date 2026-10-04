//! The witness: every digest in a prior mark recomputed against the live
//! filesystem. A path is reported unchanged only when both sides were
//! hashed and agree, when it is absent on both sides, or when a path
//! present at the mark and newly tagged is still the blob at an unmoved
//! HEAD; anything that could not be compared is unverifiable and is never
//! counted as unchanged. An absent skip-worktree entry compares as a
//! deletion, so a path a sparse checkout removes or restores is changed.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::claims::{ClaimStatus, ClaimVerdict};
use crate::mark::{
    capture_workspace, committed_changes_between, digest_recall_path, is_fully_verifiable,
    is_recall_excluded, paths_matching_commit, sort_utf16, workspace_digest, AbsentPresence,
    CaptureFailure, WorkspaceSnapshot, WorkspaceState, RECALL_ABSENT_DIGEST,
    RECALL_MAX_HASHED_BYTES, RECALL_UNVERIFIABLE,
};
use crate::store::RecallMarkFile;

pub const RECALL_COMMITS_UNLISTED: &str = "commits between the mark and HEAD could not be listed";
pub const RECALL_MARK_PREDATES_PRESENCE: &str =
    "the mark predates recording which skip-worktree paths are absent";
pub const RECALL_PRESENCE_CHANGED: &str =
    "sparse checkout changed: skip-worktree paths appeared or disappeared";

/// What changed between a mark and the live workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallWitnessReport {
    pub repo_root: String,
    pub mark_written_at: String,
    pub mark_head: Option<String>,
    pub head: Option<String>,
    pub head_moved: bool,
    pub tracked_tree_changed: bool,
    /// Sorted. Only the paths that could be compared when
    /// `changed_unknown_reason` is set.
    pub changed: Vec<String>,
    /// Set when the full set of changed paths cannot be established.
    pub changed_unknown_reason: Option<String>,
    /// Sorted.
    pub unverifiable: Vec<String>,
    /// Skip-worktree and assume-unchanged paths too many to hash or list
    /// now; counted, never listed.
    pub unhashed_tagged: usize,
    /// Every path that could not be compared, listed or not.
    pub uncompared_count: usize,
    /// Set when reported paths without a mark digest were taken off a
    /// partial mark's unrecorded count: the unrecorded remainder in
    /// `uncompared_count` is then a lower bound, up to this many.
    pub unrecorded_up_to: Option<usize>,
    /// `None` when the set of unchanged paths cannot be established.
    pub unchanged_count: Option<usize>,
    pub unchanged_unknown_reason: Option<String>,
    pub claims: Vec<ClaimVerdict>,
}

/// The facts [`compare_workspace`] compares.
pub struct CompareWorkspaceInputs<'a> {
    pub mark: &'a RecallMarkFile,
    pub snapshot: &'a WorkspaceSnapshot,
    /// Paths changed between the mark's HEAD and the current HEAD; `None`
    /// with a moved HEAD means git could not list them.
    pub committed_changes: Option<HashSet<String>>,
    /// Current digests for paths dirty or absent at the mark that git status
    /// no longer reports and that are not absent now.
    pub clean_now_digests: HashMap<String, String>,
    /// Tagged paths with no digest in the mark whose working content is the
    /// blob at an unmoved HEAD.
    pub tagged_matching_head: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathClass {
    Changed,
    Unchanged,
    Unverifiable,
}

fn classify_path(
    mark_digest: Option<&str>,
    current_digest: Option<&str>,
    clean_now_digest: Option<&str>,
    mark_may_omit_path: bool,
    matches_head: bool,
) -> PathClass {
    if mark_digest == Some(RECALL_UNVERIFIABLE) || current_digest == Some(RECALL_UNVERIFIABLE) {
        return PathClass::Unverifiable;
    }
    match (mark_digest, current_digest) {
        (Some(mark), Some(current)) => {
            if mark == current {
                PathClass::Unchanged
            } else {
                PathClass::Changed
            }
        }
        (Some(mark), None) => match clean_now_digest {
            None | Some(RECALL_UNVERIFIABLE) => PathClass::Unverifiable,
            Some(clean) if clean == mark => PathClass::Unchanged,
            Some(_) => PathClass::Changed,
        },
        // Past the mark's recorded window, or possibly absent at a mark that
        // did not record absence: nothing to compare with.
        (None, _) if mark_may_omit_path => PathClass::Unverifiable,
        // Clean at the mark and tagged since, with the content HEAD still
        // holds: only the index bits moved.
        (None, _) if matches_head => PathClass::Unchanged,
        // Clean at the mark and dirty or absent now, or committed between the
        // two HEADs: the content moved.
        (None, _) => PathClass::Changed,
    }
}

fn same_absence(mark: &AbsentPresence, now: &AbsentPresence) -> bool {
    match (mark, now) {
        (AbsentPresence::Checked(mark), AbsentPresence::Checked(now)) => {
            mark.count == now.count && mark.digest == now.digest
        }
        (AbsentPresence::Unchecked, AbsentPresence::Unchecked) => true,
        _ => false,
    }
}

fn format_claim_paths(paths: &[String]) -> String {
    let shown = paths[..paths.len().min(3)].join(", ");
    if paths.len() > 3 {
        format!("{shown} +{} more", paths.len() - 3)
    } else {
        shown
    }
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn cannot_verify(count: usize) -> String {
    format!(
        "cannot verify: {count} unverifiable path{}",
        plural_s(count)
    )
}

fn unverifiable_in_snapshot(state: &WorkspaceState) -> usize {
    state.dirty_overflow
        + state
            .dirty
            .iter()
            .filter(|(_, digest)| digest == RECALL_UNVERIFIABLE)
            .count()
}

/// Compare a mark with a live snapshot.
#[must_use]
#[allow(clippy::too_many_lines)] // one pass over the TS witness, kept together for review against it
pub fn compare_workspace(inputs: &CompareWorkspaceInputs<'_>) -> RecallWitnessReport {
    let CompareWorkspaceInputs {
        mark,
        snapshot,
        committed_changes,
        clean_now_digests,
        tagged_matching_head,
    } = inputs;
    let excluded = snapshot.excluded_path.as_deref();
    let mark_dirty: HashMap<&str, &str> = mark
        .state
        .dirty
        .iter()
        .filter(|(path, _)| !is_recall_excluded(path, excluded))
        .map(|(path, digest)| (path.as_str(), digest.as_str()))
        .collect();
    let committed: Option<HashSet<&str>> = committed_changes.as_ref().map(|changes| {
        changes
            .iter()
            .map(String::as_str)
            .filter(|path| !is_recall_excluded(path, excluded))
            .collect()
    });
    let absent_at_mark: HashSet<&str> = mark
        .state
        .absent
        .listed_paths()
        .iter()
        .map(String::as_str)
        .filter(|path| !is_recall_excluded(path, excluded))
        .collect();
    let absent_now: HashSet<&str> = snapshot
        .state
        .absent
        .listed_paths()
        .iter()
        .map(String::as_str)
        .collect();
    let snapshot_dirty: HashMap<&str, &str> = snapshot
        .state
        .dirty
        .iter()
        .map(|(path, digest)| (path.as_str(), digest.as_str()))
        .collect();
    let absent_digest = RECALL_ABSENT_DIGEST.as_str();
    let mark_digest_of = |path: &str| -> Option<&str> {
        mark_dirty
            .get(path)
            .copied()
            .or_else(|| absent_at_mark.contains(path).then_some(absent_digest))
    };
    let head_moved = mark.state.head != snapshot.state.head;
    let tracked_tree_changed = mark.state.tracked_tree_digest != snapshot.state.tracked_tree_digest;
    let commits_unlisted = head_moved && committed.is_none();
    let mark_partial = mark.state.dirty_overflow > 0;
    let presence_unknown = mark.state.absent == AbsentPresence::NotRecorded;
    let presence_changed =
        !presence_unknown && !same_absence(&mark.state.absent, &snapshot.state.absent);
    let overflow_now: HashSet<&str> = snapshot.overflow_paths.iter().map(String::as_str).collect();

    let mut candidates: Vec<&str> = Vec::new();
    let mut candidate_set: HashSet<&str> = HashSet::new();
    let sources = mark
        .state
        .dirty
        .iter()
        .map(|(path, _)| path.as_str())
        .filter(|path| mark_dirty.contains_key(path))
        .chain(
            mark.state
                .absent
                .listed_paths()
                .iter()
                .map(String::as_str)
                .filter(|path| absent_at_mark.contains(path)),
        )
        .chain(snapshot.state.dirty.iter().map(|(path, _)| path.as_str()))
        .chain(
            snapshot
                .state
                .absent
                .listed_paths()
                .iter()
                .map(String::as_str),
        )
        .chain(committed.iter().flatten().copied());
    for path in sources {
        if candidate_set.insert(path) {
            candidates.push(path);
        }
    }

    let mut changed: Vec<String> = Vec::new();
    let mut unverifiable: HashSet<&str> = overflow_now.clone();
    for &path in &candidates {
        if overflow_now.contains(path) {
            continue;
        }
        let is_committed = committed.as_ref().is_some_and(|set| set.contains(path));
        // A mark that did not record absence cannot say whether a tagged or
        // absent path was on disk then.
        let presence_at_mark_unknown =
            presence_unknown && (snapshot.tagged_paths.contains(path) || absent_now.contains(path));
        let current = snapshot_dirty
            .get(path)
            .copied()
            .or_else(|| absent_now.contains(path).then_some(absent_digest));
        match classify_path(
            mark_digest_of(path),
            current,
            clean_now_digests.get(path).map(String::as_str),
            !is_committed && (mark_partial || presence_at_mark_unknown),
            tagged_matching_head.contains(path),
        ) {
            PathClass::Changed => changed.push(path.to_string()),
            PathClass::Unverifiable => {
                unverifiable.insert(path);
            }
            PathClass::Unchanged => {}
        }
    }
    let unhashed_tagged = snapshot
        .unhashed_tagged_paths
        .iter()
        .filter(|path| !candidate_set.contains(path.as_str()))
        .count();
    // The mark's unrecorded paths are unnamed; count only as many as the
    // paths already reported without a mark digest leave over.
    let unverifiable_without_mark_digest = unverifiable
        .iter()
        .filter(|path| mark_digest_of(path).is_none())
        .count();
    let changed_without_mark_digest = changed
        .iter()
        .filter(|path| mark_digest_of(path).is_none())
        .count();
    let mark_unrecorded = if mark_partial {
        mark.state.dirty_overflow.saturating_sub(
            unhashed_tagged + unverifiable_without_mark_digest + changed_without_mark_digest,
        )
    } else {
        0
    };
    let unrecorded_up_to = (mark_partial && mark_unrecorded < mark.state.dirty_overflow)
        .then_some(mark.state.dirty_overflow);
    let uncompared_count = unverifiable.len() + unhashed_tagged + mark_unrecorded;

    let (unchanged_count, unchanged_unknown_reason) = if commits_unlisted {
        (
            None,
            Some("HEAD moved and the commits in between could not be listed".to_string()),
        )
    } else if mark_partial {
        (
            None,
            Some(format!(
                "the mark left {} path{} unrecorded",
                mark.state.dirty_overflow,
                plural_s(mark.state.dirty_overflow)
            )),
        )
    } else if presence_unknown {
        (None, Some(RECALL_MARK_PREDATES_PRESENCE.to_string()))
    } else {
        let universe: HashSet<&str> = snapshot
            .tracked_paths
            .iter()
            .map(String::as_str)
            .chain(candidates.iter().copied())
            .chain(overflow_now.iter().copied())
            .collect();
        (
            Some(
                universe
                    .len()
                    .saturating_sub(changed.len() + unverifiable.len() + unhashed_tagged),
            ),
            None,
        )
    };

    sort_utf16(&mut changed);
    let mut sorted_unverifiable: Vec<String> = unverifiable
        .iter()
        .map(|path| (*path).to_string())
        .collect();
    sort_utf16(&mut sorted_unverifiable);
    let current_digest = workspace_digest(&snapshot.state);
    let current_verifiable = is_fully_verifiable(&snapshot.state);
    let mark_digest = workspace_digest(&mark.state);
    let retagged_digest = (!tagged_matching_head.is_empty()).then(|| {
        let mut state = snapshot.state.clone();
        state
            .dirty
            .retain(|(path, _)| !tagged_matching_head.contains(path));
        workspace_digest(&state)
    });
    let claims = mark
        .claims
        .iter()
        .map(|claim| {
            // The claim's own digest covers the whole workspace; what the mark
            // failed to record cannot expire an exact match.
            let reason = if claim.digest_at_claim == current_digest {
                if current_verifiable {
                    None
                } else {
                    Some(cannot_verify(unverifiable_in_snapshot(&snapshot.state)))
                }
            } else if claim.digest_at_claim != mark_digest {
                Some("workspace changed after the claim, before the mark".to_string())
            } else if head_moved {
                Some(if commits_unlisted {
                    format!("HEAD moved; {RECALL_COMMITS_UNLISTED}")
                } else {
                    "HEAD moved".to_string()
                })
            } else if tracked_tree_changed {
                Some("tracked tree changed".to_string())
            } else if presence_changed {
                Some(RECALL_PRESENCE_CHANGED.to_string())
            } else if !changed.is_empty() {
                Some(format!("changed paths: {}", format_claim_paths(&changed)))
            } else if retagged_digest.as_deref() == Some(claim.digest_at_claim.as_str()) {
                Some("skip-worktree or assume-unchanged bits changed".to_string())
            } else if presence_unknown {
                Some(format!("cannot verify: {RECALL_MARK_PREDATES_PRESENCE}"))
            } else if uncompared_count > 0 {
                Some(cannot_verify(uncompared_count))
            } else {
                Some("workspace digest changed".to_string())
            };
            ClaimVerdict {
                claim: claim.clone(),
                status: reason.map_or(ClaimStatus::Current, |reason| ClaimStatus::Expired {
                    reason,
                }),
            }
        })
        .collect();

    RecallWitnessReport {
        repo_root: snapshot.repo_root.clone(),
        mark_written_at: mark.written_at.clone(),
        mark_head: mark.state.head.clone(),
        head: snapshot.state.head.clone(),
        head_moved,
        tracked_tree_changed,
        changed,
        changed_unknown_reason: if commits_unlisted {
            Some(RECALL_COMMITS_UNLISTED.to_string())
        } else if presence_unknown {
            Some(RECALL_MARK_PREDATES_PRESENCE.to_string())
        } else {
            None
        },
        unverifiable: sorted_unverifiable,
        unhashed_tagged,
        uncompared_count,
        unrecorded_up_to,
        unchanged_count,
        unchanged_unknown_reason,
        claims,
    }
}

/// Recompute `mark` against the live workspace. Dropping the future kills
/// outstanding git children and stops hashing.
///
/// # Errors
///
/// The [`CaptureFailure`] when git cannot describe the workspace now.
pub async fn witness_workspace(
    repo_root: &str,
    mark: &RecallMarkFile,
    agent_dir: &Path,
) -> Result<RecallWitnessReport, CaptureFailure> {
    let snapshot = capture_workspace(repo_root, Some(agent_dir)).await?;
    let root = Path::new(repo_root);
    let committed_changes = match (&mark.state.head, &snapshot.state.head) {
        (Some(from), Some(to)) if from != to => {
            committed_changes_between(root, from, to, snapshot.excluded_path.as_deref()).await
        }
        _ => None,
    };
    let excluded = snapshot.excluded_path.as_deref();
    let absent_now: HashSet<&str> = snapshot
        .state
        .absent
        .listed_paths()
        .iter()
        .map(String::as_str)
        .collect();
    let mut compared_at_mark: Vec<String> = Vec::new();
    for path in mark
        .state
        .dirty
        .iter()
        .filter(|(_, digest)| digest != RECALL_UNVERIFIABLE)
        .map(|(path, _)| path)
        .chain(mark.state.absent.listed_paths())
    {
        if compared_at_mark.contains(path)
            || snapshot.state.dirty_digest(path).is_some()
            || absent_now.contains(path.as_str())
            || snapshot.overflow_paths.contains(path)
            || is_recall_excluded(path, excluded)
        {
            continue;
        }
        compared_at_mark.push(path.clone());
    }
    let clean_now_digests: HashMap<String, String> = {
        let root = root.to_path_buf();
        let mut budget = RECALL_MAX_HASHED_BYTES;
        let mut digests = HashMap::new();
        for path in compared_at_mark {
            let root = root.clone();
            let Ok((path, digest, remaining)) = tokio::task::spawn_blocking(move || {
                let digest = digest_recall_path(&root, &path, true, &mut budget);
                (path, digest, budget)
            })
            .await
            else {
                return Err(CaptureFailure::GitUnavailable);
            };
            budget = remaining;
            digests.insert(path, digest);
        }
        digests
    };
    // A tagged path the mark neither has a digest for nor lists as absent was
    // on disk and clean then, so with HEAD unmoved it is unchanged iff it is
    // still HEAD's blob.
    let mut tagged_matching_head = HashSet::new();
    if let Some(head) = &snapshot.state.head {
        if mark.state.head.as_ref() == Some(head) && mark.state.dirty_overflow == 0 {
            let retagged: Vec<String> = snapshot
                .state
                .dirty
                .iter()
                .filter(|(path, digest)| {
                    snapshot.tagged_paths.contains(path)
                        && digest != RECALL_UNVERIFIABLE
                        && *digest != *RECALL_ABSENT_DIGEST
                        && mark.state.dirty_digest(path).is_none()
                })
                .map(|(path, _)| path.clone())
                .collect();
            if !retagged.is_empty() {
                tagged_matching_head = paths_matching_commit(root, head, retagged).await;
            }
        }
    }
    Ok(compare_workspace(&CompareWorkspaceInputs {
        mark,
        snapshot: &snapshot,
        committed_changes,
        clean_now_digests,
        tagged_matching_head,
    }))
}
