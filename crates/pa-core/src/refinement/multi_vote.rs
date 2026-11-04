//! Multi-vote auto-refine review (Claude Code's extract-memories gate).
//!
//! The native [`review_auto_refine`](super::executor::review_auto_refine)
//! runs one reviewer call. A single reviewer gets things wrong — a typo in
//! the trajectory that swayed one call, a cherry-picked excerpt, a bad day
//! for the fast model — and one wrong-vote lets a wrong refinement through,
//! which the measurement loop then has to roll back.
//!
//! This module runs three reviewers in parallel and lets the majority win.
//! Claude Code's `MAX_VERIFY_CLAIMS` / `VOTES_PER_CLAIM` / `REFUTATIONS_REQUIRED`
//! triple is the model: the vote fails as soon as two reviewers refuse, so a
//! lone approve never beats two refuse. The dissenting rationale rides in
//! `rationale` alongside the majority view so the history row explains the
//! disagreement.
//!
//! Wiring: a `make_reviewer` factory produces a fresh
//! [`RefinerFn`](super::executor::RefinerFn) per vote (the native type is
//! `FnOnce`, so each vote needs its own). The whole feature is gated by
//! [`crate::settings::experimental::ExperimentalSettings::multi_vote_refine`];
//! callers who leave the flag off run the single-pass path exactly as
//! before.

use std::sync::Arc;

use futures::future::join_all;
use pa_types::session::AgentMessage;

use super::executor::{
    AutoRefinePolicy,
    AutoRefineReview,
    AutoRefineReviewContext,
    RefinerFn,
    review_auto_refine,
};
use super::{HarnessState, RefinementResult};

/// Native defaults; matches Claude Code's extract-memories trio.
pub const DEFAULT_VOTES: usize = 3;
pub const DEFAULT_MAX_REFUTATIONS: usize = 2;

/// The knobs the caller picks per review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiVoteConfig {
    /// How many reviewers vote. Clamped to at least 1.
    pub votes: usize,
    /// The vote fails the moment this many reviewers refuse (Claude Code's
    /// `REFUTATIONS_REQUIRED`). Clamped to the range `1..=votes`.
    pub max_refutations: usize,
}

impl Default for MultiVoteConfig {
    fn default() -> Self {
        Self {
            votes: DEFAULT_VOTES,
            max_refutations: DEFAULT_MAX_REFUTATIONS,
        }
    }
}

impl MultiVoteConfig {
    #[must_use]
    pub fn clamp(self) -> Self {
        let votes = self.votes.max(1);
        let max_refutations = self.max_refutations.clamp(1, votes);
        Self {
            votes,
            max_refutations,
        }
    }
}

/// Run `config.votes` reviewers in parallel and combine their verdicts.
///
/// Approvals strictly fewer than `votes - max_refutations + 1` lose: with
/// `votes=3, max_refutations=2` the vote carries on 2+ approvals and fails
/// on 2+ refusals, mirroring Claude Code's majority rule.
///
/// Each vote's reviewer comes from `make_reviewer`: the factory is called
/// `votes` times, returning a fresh [`RefinerFn`] each time (the underlying
/// `FnOnce` consumes itself).
///
/// # Errors
///
/// Propagates the first per-vote error. A reviewer whose prose had no JSON
/// is a `should_refine=false` vote with its own rationale; it is not an
/// error (same behaviour as the single-pass path).
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors `review_auto_refine` signature and extends it with the vote factory + config; a struct wrapper would hide the parameters"
)]
pub async fn review_auto_refine_voting<F>(
    messages: &[AgentMessage],
    state: &HarnessState,
    history: &[RefinementResult],
    model: &pa_types::ai::Model,
    context: &AutoRefineReviewContext,
    policy: &dyn AutoRefinePolicy,
    make_reviewer: F,
    config: MultiVoteConfig,
) -> anyhow::Result<AutoRefineReview>
where
    F: Fn() -> RefinerFn + Send + Sync,
{
    let config = config.clamp();
    // Single-vote degenerate: identical to the native path.
    if config.votes == 1 {
        return review_auto_refine(
            messages,
            state,
            history,
            model,
            context,
            policy,
            make_reviewer(),
        )
        .await;
    }
    let state = Arc::new(state.clone());
    let history: Arc<[RefinementResult]> = Arc::from(history.to_vec());
    let messages: Arc<[AgentMessage]> = Arc::from(messages.to_vec());
    let votes = (0..config.votes).map(|_| {
        let reviewer = make_reviewer();
        let messages = Arc::clone(&messages);
        let state = Arc::clone(&state);
        let history = Arc::clone(&history);
        let model = model.clone();
        let context = AutoRefineReviewContext {
            reason: context.reason.clone(),
            turns_since_last_review: context.turns_since_last_review,
        };
        async move {
            review_auto_refine(
                &messages, &state, &history, &model, &context, policy, reviewer,
            )
            .await
        }
    });
    let results = join_all(votes).await;
    let mut reviews = Vec::with_capacity(config.votes);
    for result in results {
        reviews.push(result?);
    }
    Ok(combine_votes(reviews, config))
}

fn combine_votes(reviews: Vec<AutoRefineReview>, config: MultiVoteConfig) -> AutoRefineReview {
    let mut approvals: Vec<AutoRefineReview> = Vec::new();
    let mut refusals: Vec<AutoRefineReview> = Vec::new();
    for review in reviews {
        if review.should_refine {
            approvals.push(review);
        } else {
            refusals.push(review);
        }
    }
    let approved = !approvals.is_empty() && refusals.len() < config.max_refutations;
    if approved {
        // Carry the first approval's instructions and reply (the single-pass
        // caller only reads one), with the refusers' rationales appended so
        // the history row explains the split.
        let mut leader = approvals.into_iter().next().unwrap_or_else(default_refuse);
        leader.rationale = compose_rationale(&leader.rationale, &refusals, "refused");
        leader.should_refine = true;
        leader
    } else {
        // The majority refused: pick a refuser's rationale and append the
        // dissenters' approvals so the operator can see what was argued for.
        let mut leader = refusals.into_iter().next().unwrap_or_else(default_refuse);
        leader.rationale = compose_rationale(&leader.rationale, &approvals, "approved");
        leader.should_refine = false;
        leader.instructions = None;
        leader
    }
}

fn compose_rationale(
    leader_rationale: &str,
    dissenters: &[AutoRefineReview],
    dissent_label: &str,
) -> String {
    if dissenters.is_empty() {
        return leader_rationale.to_string();
    }
    let dissent = dissenters
        .iter()
        .map(|review| {
            let text = if review.rationale.is_empty() {
                "(no rationale)"
            } else {
                review.rationale.as_str()
            };
            format!("reviewer {dissent_label}: {text}")
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!("{leader_rationale} [dissent: {dissent}]")
}

fn default_refuse() -> AutoRefineReview {
    AutoRefineReview {
        should_refine: false,
        rationale: "Every reviewer declined without a structured decision.".to_string(),
        instructions: None,
        score: None,
        bar: None,
        reply: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(should_refine: bool, rationale: &str) -> AutoRefineReview {
        AutoRefineReview {
            should_refine,
            rationale: rationale.to_string(),
            instructions: should_refine.then(|| "refine".to_string()),
            score: None,
            bar: None,
            reply: serde_json::Map::new(),
        }
    }

    fn config() -> MultiVoteConfig {
        MultiVoteConfig::default()
    }

    #[test]
    fn two_approve_one_refuse_carries() {
        let combined = combine_votes(
            vec![
                review(true, "lesson is durable"),
                review(true, "evidence is cited"),
                review(false, "one-off noise"),
            ],
            config(),
        );
        assert!(combined.should_refine);
        assert!(combined.rationale.contains("lesson is durable"));
        assert!(combined.rationale.contains("dissent"));
        assert!(combined.rationale.contains("one-off noise"));
        assert_eq!(combined.instructions.as_deref(), Some("refine"));
    }

    #[test]
    fn two_refuse_one_approve_fails_with_dissent_recorded() {
        let combined = combine_votes(
            vec![
                review(true, "could help"),
                review(false, "not reproducible"),
                review(false, "already recorded"),
            ],
            config(),
        );
        assert!(!combined.should_refine);
        assert!(combined.instructions.is_none());
        assert!(combined.rationale.contains("not reproducible"));
        assert!(combined.rationale.contains("could help"));
    }

    #[test]
    fn unanimous_approvals_carry_without_dissent_annotation() {
        let combined = combine_votes(
            vec![review(true, "a"), review(true, "b"), review(true, "c")],
            config(),
        );
        assert!(combined.should_refine);
        assert!(!combined.rationale.contains("dissent"));
    }

    #[test]
    fn unanimous_refusals_fail_without_dissent_annotation() {
        let combined = combine_votes(
            vec![review(false, "a"), review(false, "b"), review(false, "c")],
            config(),
        );
        assert!(!combined.should_refine);
        assert!(!combined.rationale.contains("dissent"));
    }

    #[test]
    fn clamp_guards_against_zero_votes_and_oversize_refutations() {
        let clamped = MultiVoteConfig {
            votes: 0,
            max_refutations: 10,
        }
        .clamp();
        assert_eq!(clamped.votes, 1);
        assert_eq!(clamped.max_refutations, 1);
    }

    #[test]
    fn higher_refutation_threshold_approves_on_one_pass() {
        let config = MultiVoteConfig {
            votes: 3,
            max_refutations: 3,
        }
        .clamp();
        // With refutations_required=3, only a unanimous refusal fails; a
        // single approval carries.
        let combined = combine_votes(
            vec![review(true, "a"), review(false, "b"), review(false, "c")],
            config,
        );
        assert!(combined.should_refine);
    }

    #[test]
    fn empty_votes_defaults_to_refuse() {
        let combined = combine_votes(Vec::new(), config());
        assert!(!combined.should_refine);
    }
}
