//! Server-side fingerprint matcher (design doc §5 / `project_trust_loop_fix`
//! option 3): when the refiner model omits per-edit
//! `targetsFingerprints`, walk the recurring failure ledger excerpts and
//! deterministically attach fingerprints whose cause the edit's content
//! plainly covers.
//!
//! Three disjoint signals, any one of which attaches a fingerprint:
//!
//! 1. **Fingerprint id in edit text.** The 8+ hex-char id already uniquely
//!    names the failure in the prompt; an edit that cites `failure:abc123..`
//!    (or the bare id alone, as the system prompt instructs) addresses it.
//! 2. **Exception class or source substring match.** When the failure's
//!    `exceptionClass` (e.g. `ModuleNotFoundError`, `KeyError`) or `source`
//!    appears verbatim in the edit, the edit covers that fingerprint. The
//!    classes are distinctive enough to avoid false positives.
//! 3. **Distinctive-token Jaccard overlap.** Tokens of length ≥ 4 that are
//!    not stopwords, intersected across excerpt + edit; attach when the
//!    intersection has ≥ [`DISTINCTIVE_TOKEN_MATCH_MIN`] tokens.
//!
//! The matcher adds to [`super::planner::RefinementEdit::targets_fingerprints`]
//! — it never removes the model's own citations. Coverage only grows.

use std::collections::{BTreeSet, HashSet};

use serde_json::Value;

use super::HarnessState;
use super::planner::{RefinementEdit, RefinementProposal};

/// At least this many distinctive tokens must appear in BOTH the edit text
/// and the failure excerpt before the Jaccard path attaches a fingerprint.
/// Three is conservative: a one-sentence memory about the right topic will
/// hit this, but a tangential mention of one word will not.
pub const DISTINCTIVE_TOKEN_MATCH_MIN: usize = 3;

/// Tokens shorter than this are too common to carry signal.
const MIN_TOKEN_LEN: usize = 4;

/// The failure excerpt is clipped to this many characters in the prompt
/// (`refinement/executor.rs::unaddressed_recurring_for_prompt`'s
/// `EXCERPT_MAX`). Matching never looks past this bound either, so prompt
/// view and matcher view are the same words.
const EXCERPT_MATCH_WINDOW: usize = 240;

/// Tokens the matcher ignores in the Jaccard pass — common English stop
/// words plus procedure words the refinement prompt itself leans on. Kept
/// short so a rare domain word stays distinctive.
const STOPWORDS: &[&str] = &[
    "about",
    "after",
    "again",
    "also",
    "and",
    "any",
    "are",
    "because",
    "been",
    "before",
    "being",
    "between",
    "both",
    "but",
    "cannot",
    "could",
    "does",
    "doing",
    "done",
    "each",
    "every",
    "failure",
    "failures",
    "fingerprint",
    "fingerprints",
    "first",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "here",
    "into",
    "just",
    "only",
    "once",
    "other",
    "ours",
    "over",
    "same",
    "should",
    "some",
    "such",
    "than",
    "that",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "under",
    "until",
    "upon",
    "very",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "will",
    "with",
    "within",
    "would",
    "your",
    "yours",
    "the",
    "its",
    "not",
];

/// Attach server-side fingerprint matches to every edit in `proposal`
/// whose `targets_fingerprints` would otherwise be empty, driven from
/// `state.extensions["failures"]`. Edits that already cite fingerprints
/// are left untouched (the model's deliberate targeting wins).
///
/// The match is deterministic and auditable: a `tracing::info!` at
/// `pa_core::refinement::fingerprint_matcher` lists every attachment so a
/// trust-loop review can tell model-cited from matcher-cited fingerprints.
///
/// Returns the number of fingerprints attached across all edits.
pub fn attach_server_side_fingerprints(
    proposal: &mut RefinementProposal,
    state: &HarnessState,
) -> usize {
    let failures = match extract_failures(state) {
        Some(records) if !records.is_empty() => records,
        _ => return 0,
    };
    let mut attached = 0usize;
    for edit in &mut proposal.edits {
        attached += attach_matches_for_edit(edit, &failures);
    }
    attached
}

/// Distinctive (non-stopword, ≥ [`MIN_TOKEN_LEN`]) lowercase tokens from
/// `text`, preserving at most one occurrence of each.
fn distinctive_tokens(text: &str) -> HashSet<String> {
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    text.split(|char: char| !char.is_ascii_alphanumeric() && char != '_')
        .filter(|token| token.len() >= MIN_TOKEN_LEN)
        .map(str::to_ascii_lowercase)
        .filter(|token| !stop.contains(token.as_str()))
        .collect()
}

/// The text the edit presents to the ledger: title + content joined with a
/// newline, so a title-only or content-only hit still counts.
fn edit_text(edit: &RefinementEdit) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(title) = edit.title.as_deref() {
        parts.push(title);
    }
    if let Some(content) = edit.content.as_deref() {
        parts.push(content);
    }
    parts.join("\n")
}

/// One failure the matcher reads from `state.extensions["failures"]`. Field
/// names mirror `pa_ledger`'s `FailureRecord` + `FailureFingerprint`
/// (parsed from JSON rather than imported because `pa-ledger` depends on
/// `pa-core`, so the reverse import is forbidden).
#[derive(Debug, Clone)]
struct FailureSummary {
    id: String,
    exception_class: Option<String>,
    source: Option<String>,
    excerpt: String,
}

fn extract_failures(state: &HarnessState) -> Option<Vec<FailureSummary>> {
    let records = state
        .extensions
        .get("failures")?
        .get("failures")?
        .as_object()?;
    let mut out = Vec::with_capacity(records.len());
    for record in records.values() {
        let Some(fingerprint) = record.get("fingerprint") else {
            continue;
        };
        let Some(id) = fingerprint.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let exception_class = fingerprint
            .get("exceptionClass")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let source = fingerprint
            .get("source")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let excerpt_raw = record.get("excerpt").and_then(Value::as_str).unwrap_or("");
        let normalised: String = excerpt_raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let excerpt: String = normalised.chars().take(EXCERPT_MATCH_WINDOW).collect();
        out.push(FailureSummary {
            id: id.to_string(),
            exception_class,
            source,
            excerpt,
        });
    }
    Some(out)
}

/// Attach server-matched fingerprints to one edit, returning how many were
/// added. Preserves the order of already-cited fingerprints and appends
/// new ones in a stable (sorted) order.
fn attach_matches_for_edit(edit: &mut RefinementEdit, failures: &[FailureSummary]) -> usize {
    let text = edit_text(edit);
    if text.trim().is_empty() {
        return 0;
    }
    let lower_text = text.to_ascii_lowercase();
    let edit_tokens = distinctive_tokens(&lower_text);
    // Build a set of fingerprints already on this edit, so the matcher never
    // claims a duplicate attachment.
    let existing: BTreeSet<String> = edit
        .targets_fingerprints
        .as_ref()
        .map(|list| list.iter().cloned().collect())
        .unwrap_or_default();
    let mut matched: BTreeSet<String> = BTreeSet::new();
    for failure in failures {
        if existing.contains(&failure.id) || matched.contains(&failure.id) {
            continue;
        }
        if matches_failure(&lower_text, &edit_tokens, failure) {
            matched.insert(failure.id.clone());
        }
    }
    if matched.is_empty() {
        return 0;
    }
    let edit_id = edit.id.as_deref().unwrap_or("<new>");
    let edit_kind = edit
        .kind
        .map(|value| match value {
            super::RefinementKind::Prompt => "prompt",
            super::RefinementKind::Memory => "memory",
            super::RefinementKind::Skill => "skill",
            super::RefinementKind::Subagent => "subagent",
            super::RefinementKind::Factory => "factory",
        })
        .unwrap_or("unknown");
    let fingerprints_joined = matched.iter().cloned().collect::<Vec<_>>().join(",");
    let attached_count = matched.len();
    tracing::info!(
        target: "pa_core::refinement::fingerprint_matcher",
        edit_id = %edit_id,
        edit_kind = %edit_kind,
        attached = attached_count,
        fingerprints = %fingerprints_joined,
        "server-side matcher attached fingerprints to edit",
    );
    let mut list = edit.targets_fingerprints.clone().unwrap_or_default();
    let count = matched.len();
    for fp in matched {
        list.push(fp);
    }
    edit.targets_fingerprints = Some(list);
    count
}

fn matches_failure(
    lower_text: &str,
    edit_tokens: &HashSet<String>,
    failure: &FailureSummary,
) -> bool {
    // Signal 1: fingerprint id appears literally (either bare or with
    // the `failure:` prefix) in the edit text. The id is a hash slice —
    // a verbatim match is nearly impossible to false-positive.
    if lower_text.contains(&failure.id.to_ascii_lowercase()) {
        return true;
    }
    // Signal 2: exception class or source substring. Both are distinctive
    // ("ModuleNotFoundError", "ImportError", "<cell-123>") — a verbatim
    // substring match is a strong link. Skip generic one-word matches
    // like bare "Error".
    if let Some(class) = failure.exception_class.as_deref()
        && is_distinctive_token(class)
        && lower_text.contains(&class.to_ascii_lowercase())
    {
        return true;
    }
    if let Some(source) = failure.source.as_deref()
        && is_distinctive_token(source)
        && lower_text.contains(&source.to_ascii_lowercase())
    {
        return true;
    }
    // Signal 3: ≥ DISTINCTIVE_TOKEN_MATCH_MIN distinctive tokens shared
    // between edit and excerpt.
    let excerpt_tokens = distinctive_tokens(&failure.excerpt.to_ascii_lowercase());
    if excerpt_tokens.is_empty() {
        return false;
    }
    let overlap = edit_tokens.intersection(&excerpt_tokens).count();
    overlap >= DISTINCTIVE_TOKEN_MATCH_MIN
}

/// A substring signal is distinctive enough to attach a fingerprint on when
/// it is at least [`MIN_TOKEN_LEN`] characters AND not a bare generic word
/// like "Error".
fn is_distinctive_token(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < MIN_TOKEN_LEN {
        return false;
    }
    // Reject the one-word "Error"/"Failure" classes — they match anything.
    let generic = matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "error" | "failure" | "exception" | "warning",
    );
    !generic
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::refinement::planner::{RefinementEdit, RefinementProposal};
    use crate::refinement::{HarnessState, RefinementAction, RefinementKind, empty_harness_state};

    fn state_with_failures(records: Vec<Value>) -> HarnessState {
        let mut state = empty_harness_state();
        let mut failures = Map::new();
        for record in records {
            let id = record
                .pointer("/fingerprint/id")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            failures.insert(id, record);
        }
        state
            .extensions
            .insert("failures".to_string(), json!({ "failures": failures }));
        state
    }

    fn failure_record(id: &str, class: &str, source: &str, excerpt: &str) -> Value {
        json!({
            "fingerprint": {
                "id": id,
                "kind": "exception",
                "exceptionClass": class,
                "source": source,
            },
            "count": 3,
            "excerpt": excerpt,
        })
    }

    fn edit(title: &str, content: &str) -> RefinementEdit {
        RefinementEdit {
            action: Some(RefinementAction::Create),
            kind: Some(RefinementKind::Memory),
            title: Some(title.to_string()),
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    fn proposal(edits: Vec<RefinementEdit>) -> RefinementProposal {
        RefinementProposal {
            summary: "s".to_string(),
            rationale: "r".to_string(),
            expected_outcome: "e".to_string(),
            edits,
        }
    }

    #[test]
    fn exception_class_substring_match_attaches_fingerprint() {
        let state = state_with_failures(vec![failure_record(
            "abc12345",
            "ModuleNotFoundError",
            "",
            "No module named 'thing'",
        )]);
        let mut p = proposal(vec![edit(
            "Imports note",
            "After a ModuleNotFoundError, run `pip install -e .`",
        )]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 1);
        assert_eq!(
            p.edits[0].targets_fingerprints.as_deref(),
            Some(&vec!["abc12345".to_string()][..])
        );
    }

    #[test]
    fn fingerprint_id_mentioned_in_edit_text_attaches() {
        let state = state_with_failures(vec![failure_record(
            "deadbeef1234",
            "AssertionError",
            "<cell-7>",
            "expected 1 match",
        )]);
        let mut p = proposal(vec![edit(
            "Sub_once anchor",
            "Fix for failure:deadbeef1234 — replace with unique prefix.",
        )]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 1);
        assert!(
            p.edits[0]
                .targets_fingerprints
                .as_deref()
                .unwrap()
                .contains(&"deadbeef1234".to_string())
        );
    }

    #[test]
    fn distinctive_token_overlap_attaches() {
        let state = state_with_failures(vec![failure_record(
            "cafef00d",
            "FileNotFoundError",
            "attach_image",
            "FileNotFoundError: screenshot.png missing from scratchpad directory",
        )]);
        let mut p = proposal(vec![edit(
            "Screenshot handling",
            "Save screenshot to the scratchpad directory before attaching.",
        )]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 1, "scratchpad + directory + screenshot → 3 overlaps");
    }

    #[test]
    fn unrelated_edit_attaches_nothing() {
        let state = state_with_failures(vec![failure_record(
            "abc",
            "ModuleNotFoundError",
            "",
            "camoufox page NameError",
        )]);
        let mut p = proposal(vec![edit(
            "unwrap-rs writing style",
            "Prefer spec-sheet form in release blog posts.",
        )]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 0);
        assert!(p.edits[0].targets_fingerprints.is_none());
    }

    #[test]
    fn existing_model_citation_is_not_duplicated() {
        let state = state_with_failures(vec![failure_record(
            "abc12345",
            "ModuleNotFoundError",
            "",
            "No module named 'thing'",
        )]);
        let mut p = proposal(vec![RefinementEdit {
            targets_fingerprints: Some(vec!["abc12345".to_string()]),
            ..edit("x", "ModuleNotFoundError again")
        }]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 0);
        assert_eq!(
            p.edits[0].targets_fingerprints.as_deref(),
            Some(&vec!["abc12345".to_string()][..])
        );
    }

    #[test]
    fn generic_exception_class_is_rejected() {
        // A bare "Error" or "Exception" is not distinctive — would false-
        // positive against half the edits in the harness.
        let state = state_with_failures(vec![failure_record(
            "abc",
            "Error",
            "",
            "something bad happened",
        )]);
        let mut p = proposal(vec![edit("random", "Here is an Error message")]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 0);
    }

    #[test]
    fn no_failures_in_state_is_a_noop() {
        let state = empty_harness_state();
        let mut p = proposal(vec![edit("x", "y")]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 0);
    }

    #[test]
    fn empty_edit_text_attaches_nothing_even_with_failures() {
        let state = state_with_failures(vec![failure_record(
            "abc",
            "ModuleNotFoundError",
            "",
            "No module",
        )]);
        let mut p = proposal(vec![RefinementEdit {
            action: Some(RefinementAction::Delete),
            kind: Some(RefinementKind::Memory),
            id: Some("x".to_string()),
            ..Default::default()
        }]);
        let added = attach_server_side_fingerprints(&mut p, &state);
        assert_eq!(added, 0);
    }
}
