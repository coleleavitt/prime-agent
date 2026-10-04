//! The decision interface both phases share, and the fixed interpreter of an
//! exploration policy (TS `observation.ts` + `interpreter.ts`).
//!
//! Online exploration and frozen replay present the same [`ObservationView`],
//! so the identical policy JSON drives both. [`interpret_policy`] is the ONLY
//! code that acts on a policy: it ranks the legal cells by the named selection
//! rule and cuts a legal batch of at most `min(batchSize, W)` cells that never
//! holds a node together with its parent. It uses no randomness.

use std::cmp::Ordering;

use crate::policy::{ExplorationPolicy, SelectionRule, StopRule};
use crate::task::ProposeParams;

/// Base perturbation size a policy's `branchWidth` scales.
pub const BASE_STEP: f64 = 0.1;

/// A round improves the best only when it beats it by more than this; shared by
/// the online drivers and replay so `patience` fires on the same round in both.
pub const IMPROVE_EPS: f64 = 1e-12;

/// A candidate starting point for one new attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub node_id: String,
    pub is_root: bool,
    pub parent_id: Option<String>,
    pub score: f64,
    pub valid: bool,
}

/// The view a policy decides from; online and replay implement it alike.
pub trait ObservationView {
    /// Max parallelism W: the largest batch the driver accepts.
    fn max_parallelism(&self) -> usize;
    /// Eligible cells this round: the root plus every open-branch leaf.
    fn legal_actions(&self) -> Vec<Cell>;
    /// Best valid score over the revealed prefix (0 when nothing is valid).
    fn best_score(&self) -> f64;
}

/// Why a batch is illegal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LegalBatchError(pub String);

/// The drivers' defensive check on every batch: distinct cells, all legal,
/// within W, and never a node together with its parent.
///
/// # Errors
///
/// [`LegalBatchError`] naming the first violation.
pub fn assert_legal_batch(
    view: &dyn ObservationView,
    cells: &[Cell],
) -> Result<(), LegalBatchError> {
    let mut chosen: Vec<&str> = Vec::with_capacity(cells.len());
    for cell in cells {
        if chosen.contains(&cell.node_id.as_str()) {
            return Err(LegalBatchError(format!(
                "batch contains {} twice",
                cell.node_id
            )));
        }
        chosen.push(&cell.node_id);
    }
    if cells.len() > view.max_parallelism() {
        return Err(LegalBatchError(format!(
            "batch of {} exceeds max parallelism {}",
            cells.len(),
            view.max_parallelism()
        )));
    }
    let legal = view.legal_actions();
    for cell in cells {
        if !legal.iter().any(|known| known.node_id == cell.node_id) {
            return Err(LegalBatchError(format!(
                "cell {} is not a legal action",
                cell.node_id
            )));
        }
    }
    for cell in cells {
        if let Some(parent) = &cell.parent_id {
            if chosen.contains(&parent.as_str()) {
                return Err(LegalBatchError(format!(
                    "batch contains {} and its parent {parent}",
                    cell.node_id
                )));
            }
        }
    }
    Ok(())
}

/// Project a policy's knobs onto one generation attempt.
#[must_use]
pub fn project_propose_params(policy: &ExplorationPolicy) -> ProposeParams {
    ProposeParams {
        step_scale: f64::from(policy.branch_width) * BASE_STEP,
        refine_depth: policy.refine_depth,
        branch_width: policy.branch_width,
    }
}

fn effective_score(cell: &Cell) -> f64 {
    if cell.valid {
        cell.score
    } else {
        f64::NEG_INFINITY
    }
}

/// JS `a - b || tie`: a NaN or zero difference falls through to the tie-break.
fn by_difference(difference: f64, tie: Ordering) -> Ordering {
    if difference > 0.0 {
        Ordering::Greater
    } else if difference < 0.0 {
        Ordering::Less
    } else {
        tie
    }
}

fn by_score_desc(a: &Cell, b: &Cell) -> Ordering {
    by_difference(
        effective_score(b) - effective_score(a),
        a.node_id.cmp(&b.node_id),
    )
}

/// Rank the eligible cells by the policy's selection rule (deterministic).
/// Node ids within one tree differ only in their `-n<seq>` digits, where code
/// order equals the TS `localeCompare` order.
#[must_use]
pub fn rank_eligible(policy: &ExplorationPolicy, view: &dyn ObservationView) -> Vec<Cell> {
    let mut actions = view.legal_actions();
    match policy.selection_rule {
        SelectionRule::BestFirst => {
            actions.sort_by(by_score_desc);
            actions
        }
        SelectionRule::ExploreRoot => {
            let (mut roots, mut rest): (Vec<Cell>, Vec<Cell>) =
                actions.into_iter().partition(|cell| cell.is_root);
            rest.sort_by(by_score_desc);
            roots.append(&mut rest);
            roots
        }
        SelectionRule::RoundRobin => {
            actions.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            actions
        }
        SelectionRule::Weighted => {
            let best = view.best_score();
            let weight = |cell: &Cell| -> f64 {
                let score = effective_score(cell);
                let promising =
                    cell.valid && best > 0.0 && score >= policy.promising_threshold * best;
                score
                    + if promising {
                        policy.exploration_bias
                    } else {
                        0.0
                    }
            };
            actions.sort_by(|a, b| by_difference(weight(b) - weight(a), a.node_id.cmp(&b.node_id)));
            actions
        }
    }
}

/// A legal batch from the view: greedy over the ranked cells, skipping a
/// duplicate, the parent of a chosen cell, and a cell whose parent is chosen.
#[must_use]
pub fn interpret_policy(policy: &ExplorationPolicy, view: &dyn ObservationView) -> Vec<Cell> {
    let limit = usize::try_from(policy.batch_size)
        .unwrap_or(usize::MAX)
        .min(view.max_parallelism());
    if limit == 0 {
        return Vec::new();
    }
    let mut chosen: Vec<Cell> = Vec::new();
    for cell in rank_eligible(policy, view) {
        if chosen.len() >= limit {
            break;
        }
        if chosen.iter().any(|picked| picked.node_id == cell.node_id) {
            continue;
        }
        if let Some(parent) = &cell.parent_id {
            if chosen.iter().any(|picked| &picked.node_id == parent) {
                continue;
            }
        }
        if chosen
            .iter()
            .any(|picked| picked.parent_id.as_deref() == Some(cell.node_id.as_str()))
        {
            continue;
        }
        chosen.push(cell);
    }
    chosen
}

/// The evolving state a stop rule reads each round.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StopState {
    /// Rounds completed so far.
    pub round: u32,
    /// Best valid score seen.
    pub best_score: f64,
    /// Consecutive rounds without an improvement to `best_score`.
    pub rounds_since_improvement: u32,
}

/// Whether exploration stops after the current round.
#[must_use]
pub fn apply_stop_rule(policy: &ExplorationPolicy, state: &StopState) -> bool {
    match policy.stop_rule {
        StopRule::Patience => state.rounds_since_improvement >= policy.beta,
        StopRule::Threshold => state.best_score >= policy.target_score,
        StopRule::FixedRounds => state.round >= policy.beta,
        StopRule::Never => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::DEFAULT_POLICY;

    struct Empty;

    impl ObservationView for Empty {
        fn max_parallelism(&self) -> usize {
            4
        }
        fn legal_actions(&self) -> Vec<Cell> {
            Vec::new()
        }
        fn best_score(&self) -> f64 {
            0.0
        }
    }

    #[test]
    fn an_empty_view_yields_an_empty_batch() {
        assert_eq!(
            interpret_policy(&DEFAULT_POLICY, &Empty),
            Vec::<Cell>::new()
        );
    }
}
