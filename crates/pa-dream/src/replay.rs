//! Stage 2: the frozen replay simulator (TS `replay.ts`).
//!
//! A recorded tree is a deterministic, zero-execution simulator. A policy
//! re-walks it with the SAME interpreter that drove the rollout, and each
//! selected cell reveals — never generates — its recorded child: the root its
//! earliest-seq unrevealed child, a non-root leaf its single child. A legally
//! selected cell with nothing left to reveal is OUT OF SUPPORT: it reveals
//! nothing but is CHARGED as a probe. Replay uses no rng and no clock.

use serde::Serialize;

use crate::interpreter::{
    Cell,
    IMPROVE_EPS,
    LegalBatchError,
    ObservationView,
    StopState,
    apply_stop_rule,
    assert_legal_batch,
    interpret_policy,
};
use crate::objective::{
    ObjectiveBudget,
    ReplayObjectiveConfig,
    compute_objective,
    pool_score_scale,
};
use crate::policy::{ExplorationPolicy, policy_id};
use crate::store::RecordedTree;

/// One replay's accounting.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
    pub policy_id: String,
    pub tree_id: String,
    /// Ids revealed, in reveal order (root first).
    pub revealed_ids: Vec<String>,
    /// Revealed non-root nodes.
    #[serde(rename = "N")]
    pub n: u32,
    /// Decision rounds taken.
    pub rounds: u32,
    /// Best valid score over the revealed prefix (0 when nothing is valid).
    pub best_score: f64,
    /// Selections that revealed nothing because the recorded branch was exhausted.
    pub out_of_support_cells: u32,
    /// `N + outOfSupportCells`.
    pub selected_cells: u32,
    /// `N / selectedCells`; 1 when nothing was selected.
    pub in_support: f64,
    /// Running best after each charged selection (0 while nothing is valid).
    pub best_so_far: Vec<f64>,
    /// 1-based charged selection that first reached `bestScore`; 0 when the root is best.
    pub probes_to_best: u32,
    /// 1-based decision round of that selection; 0 when the root is best.
    pub rounds_to_best: u32,
}

/// Replay bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayConfig {
    /// Max replay rounds.
    pub k2: u32,
}

/// The incremental replay state over one recorded tree.
pub struct ReplaySimulator<'a> {
    recorded: &'a RecordedTree,
    revealed: Vec<bool>,
    order: Vec<usize>,
    round: u32,
}

impl<'a> ReplaySimulator<'a> {
    /// A simulator with only the root revealed.
    #[must_use]
    pub fn new(recorded: &'a RecordedTree) -> Self {
        let mut revealed = vec![false; recorded.nodes.len()];
        revealed[recorded.root_index] = true;
        Self {
            recorded,
            revealed,
            order: vec![recorded.root_index],
            round: 0,
        }
    }

    /// Ids revealed so far, in reveal order.
    #[must_use]
    pub fn revealed_ids(&self) -> Vec<String> {
        self.order
            .iter()
            .map(|index| self.recorded.nodes[*index].id.clone())
            .collect()
    }

    /// The decision view over the revealed prefix.
    #[must_use]
    pub fn view(&self) -> ReplayObservation<'_> {
        ReplayObservation {
            recorded: self.recorded,
            revealed: &self.revealed,
        }
    }

    /// Whether every recorded node is revealed.
    #[must_use]
    pub fn all_revealed(&self) -> bool {
        self.order.len() >= self.recorded.nodes.len()
    }

    /// Reveal the recorded child of a legal cell; `None` when out of support.
    pub fn reveal_for(&mut self, cell: &Cell) -> Option<usize> {
        let index = self.recorded.index_of(&cell.node_id)?;
        let child = self.recorded.children[index]
            .iter()
            .copied()
            .find(|child| !self.revealed[*child])?;
        self.revealed[child] = true;
        self.order.push(child);
        Some(child)
    }

    /// Advance the decision round.
    pub fn advance_round(&mut self) {
        self.round += 1;
    }

    /// The current decision round.
    #[must_use]
    pub fn round(&self) -> u32 {
        self.round
    }
}

/// The replay view: {root} ∪ {revealed non-root nodes with no revealed child}.
pub struct ReplayObservation<'a> {
    recorded: &'a RecordedTree,
    revealed: &'a [bool],
}

fn cell_of(recorded: &RecordedTree, index: usize) -> Cell {
    let node = &recorded.nodes[index];
    Cell {
        node_id: node.id.clone(),
        is_root: node.parent_id.is_none(),
        parent_id: node.parent_id.clone(),
        score: node.score,
        valid: node.valid,
    }
}

fn best_over_revealed(recorded: &RecordedTree, revealed: &[bool]) -> f64 {
    let mut best: Option<f64> = None;
    for (index, node) in recorded.nodes.iter().enumerate() {
        if revealed[index] && node.valid && best.is_none_or(|current| node.score > current) {
            best = Some(node.score);
        }
    }
    best.unwrap_or(0.0)
}

impl ObservationView for ReplayObservation<'_> {
    fn max_parallelism(&self) -> usize {
        usize::try_from(self.recorded.header.w.max(1)).unwrap_or(usize::MAX)
    }

    fn legal_actions(&self) -> Vec<Cell> {
        let mut out = Vec::new();
        for (index, node) in self.recorded.nodes.iter().enumerate() {
            if !self.revealed[index] {
                continue;
            }
            let open = node.parent_id.is_none()
                || !self.recorded.children[index]
                    .iter()
                    .any(|child| self.revealed[*child]);
            if open {
                out.push(cell_of(self.recorded, index));
            }
        }
        out
    }

    fn best_score(&self) -> f64 {
        best_over_revealed(self.recorded, self.revealed)
    }
}

/// Re-walk `recorded` with `policy`. Deterministic and zero-cost.
///
/// # Panics
///
/// Never in practice: the interpreter only emits legal batches, and the
/// defensive legality check panics only if that invariant breaks.
#[must_use]
pub fn simulate_policy(
    recorded: &RecordedTree,
    policy: &ExplorationPolicy,
    cfg: ReplayConfig,
) -> ReplayResult {
    try_simulate_policy(recorded, policy, cfg)
        .unwrap_or_else(|error| panic!("illegal replay batch: {error}"))
}

/// [`simulate_policy`] with the legality check surfaced.
///
/// # Errors
///
/// [`LegalBatchError`] when the interpreter emitted an illegal batch.
pub fn try_simulate_policy(
    recorded: &RecordedTree,
    policy: &ExplorationPolicy,
    cfg: ReplayConfig,
) -> Result<ReplayResult, LegalBatchError> {
    let mut sim = ReplaySimulator::new(recorded);
    let k2 = cfg.k2.max(1);
    let root = &recorded.nodes[recorded.root_index];
    let mut running_best = 0.0;
    let mut seen_valid = false;
    if root.valid {
        running_best = root.score;
        seen_valid = true;
    }
    // Lags the true running max by up to IMPROVE_EPS, exactly as online.
    let mut best_score = if seen_valid { running_best } else { 0.0 };
    let mut rounds_since_improvement = 0;
    let mut rounds = 0;
    let mut out_of_support = 0;
    let mut best_so_far = Vec::new();
    let mut selection_round = Vec::new();
    while rounds < k2 {
        if sim.all_revealed() {
            break;
        }
        let batch = {
            let view = sim.view();
            let batch = interpret_policy(policy, &view);
            if batch.is_empty() {
                break;
            }
            assert_legal_batch(&view, &batch)?;
            batch
        };
        for cell in &batch {
            match sim.reveal_for(cell) {
                None => out_of_support += 1,
                Some(index) => {
                    let node = &recorded.nodes[index];
                    if node.valid && (!seen_valid || node.score > running_best) {
                        running_best = node.score;
                        seen_valid = true;
                    }
                }
            }
            best_so_far.push(if seen_valid { running_best } else { 0.0 });
            selection_round.push(rounds + 1);
        }
        rounds += 1;
        sim.advance_round();
        let new_best = best_over_revealed(recorded, &sim.revealed);
        if new_best > best_score + IMPROVE_EPS {
            best_score = new_best;
            rounds_since_improvement = 0;
        } else {
            rounds_since_improvement += 1;
        }
        let state = StopState {
            round: rounds,
            best_score,
            rounds_since_improvement,
        };
        if apply_stop_rule(policy, &state) {
            break;
        }
    }
    let revealed_ids = sim.revealed_ids();
    let final_best = best_over_revealed(recorded, &sim.revealed);
    let n = u32::try_from(revealed_ids.len() - 1).unwrap_or(u32::MAX);
    let selected_cells = n + out_of_support;
    let mut probes_to_best = 0;
    if seen_valid && !(root.valid && root.score >= final_best) {
        probes_to_best = best_so_far
            .iter()
            .position(|score| *score >= final_best)
            .map_or(0, |index| u32::try_from(index + 1).unwrap_or(u32::MAX));
    }
    let rounds_to_best = if probes_to_best == 0 {
        0
    } else {
        selection_round[probes_to_best as usize - 1]
    };
    Ok(ReplayResult {
        policy_id: policy_id(policy),
        tree_id: recorded.header.tree_id.clone(),
        revealed_ids,
        n,
        rounds,
        best_score: final_best,
        out_of_support_cells: out_of_support,
        selected_cells,
        in_support: if selected_cells == 0 {
            1.0
        } else {
            f64::from(n) / f64::from(selected_cells)
        },
        best_so_far,
        probes_to_best,
        rounds_to_best,
    })
}

/// The standalone `dream replay` entry: one simulation inside a `dream.replay`
/// span carrying the result and its V (the tree is its own pool).
#[must_use]
pub fn simulate_policy_with_span(
    recorded: &RecordedTree,
    policy: &ExplorationPolicy,
    k1: u32,
    k2: u32,
    objective: &ReplayObjectiveConfig,
) -> ReplayResult {
    let span = tracing::info_span!(
        "dream.replay",
        dream.policy_id = %policy_id(policy),
        dream.tree_id = %recorded.header.tree_id,
        dream.revealed_n = tracing::field::Empty,
        dream.rounds = tracing::field::Empty,
        dream.v = tracing::field::Empty,
        dream.out_of_support = tracing::field::Empty,
        dream.in_support = tracing::field::Empty,
        dream.probes_to_best = tracing::field::Empty,
        dream.simulations = 1,
    );
    let _entered = span.enter();
    let result = simulate_policy(recorded, policy, ReplayConfig { k2 });
    let v = compute_objective(
        &result,
        objective,
        pool_score_scale([recorded]),
        ObjectiveBudget {
            workers: recorded.header.w,
            k1,
        },
    );
    span.record("dream.revealed_n", result.n);
    span.record("dream.rounds", result.rounds);
    span.record("dream.v", v);
    span.record("dream.out_of_support", result.out_of_support_cells);
    span.record("dream.in_support", result.in_support);
    span.record("dream.probes_to_best", result.probes_to_best);
    result
}
