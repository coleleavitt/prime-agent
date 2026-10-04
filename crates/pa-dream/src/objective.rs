//! The replay objective V, scale-invariant (TS `objective.ts`).
//!
//! ```text
//! V = (1 - beta3) q + beta3 anytime - beta1 S_eff / (W k1) + beta2 (1 - rounds_eff / k1)
//! ```
//!
//! `q` is the best valid score normalized to the pool's score range;
//! `anytime` the mean normalized best-so-far over the budget (held flat at `q`
//! for the unspent tail); `S = N + oos` the charged selections. With an
//! `evidence` horizon the spend terms charge `max(S, H_probes)` and
//! `max(rounds, H_rounds)`: a candidate's stop-early credit needs another tree
//! to vouch for it (`improve.rs`). See `docs/dream-rsi.md` for the derivation
//! and the recorded regressions behind every term. The arithmetic is plain
//! IEEE-754 in the TS operation order, so the same replay gives the same V.

use serde::{Deserialize, Serialize};

use crate::replay::ReplayResult;
use crate::store::RecordedTree;

/// The objective's weights.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplayObjectiveConfig {
    /// Cost of charging the whole per-rollout probe budget, in quality points.
    pub beta1: f64,
    /// Bonus for finishing in zero rounds (scaled by `1 - rounds / k1`).
    pub beta2: f64,
    /// Weight of the anytime term against final quality, in `[0, 1]`.
    pub beta3: f64,
}

/// `beta2 > beta1` so a saved round outweighs the at most W probes it costs.
pub const DEFAULT_OBJECTIVE: ReplayObjectiveConfig = ReplayObjectiveConfig {
    beta1: 0.05,
    beta2: 0.1,
    beta3: 0.25,
};

/// The observed valid-score range of a pool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectiveScale {
    pub score_min: f64,
    pub score_max: f64,
}

/// The per-rollout budget the cost terms are measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectiveBudget {
    /// Max parallelism W (`tree.header.w`).
    pub workers: u32,
    /// Max online rounds k1.
    pub k1: u32,
}

/// The spend horizon a candidate's cost terms are charged against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectiveEvidence {
    pub probes: f64,
    pub rounds: f64,
}

/// The terms of one replay.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectiveTerms {
    pub quality: f64,
    pub anytime: f64,
    pub cost: f64,
    pub rounds_saved: f64,
    pub charged_probes: f64,
    pub charged_rounds: f64,
    pub value: f64,
}

/// Min and max over the valid, finite node scores of every tree (roots included);
/// `{0, 0}` when there are none.
#[must_use]
pub fn pool_score_scale<'a>(pool: impl IntoIterator<Item = &'a RecordedTree>) -> ObjectiveScale {
    let mut score_min = f64::INFINITY;
    let mut score_max = f64::NEG_INFINITY;
    for tree in pool {
        for node in &tree.nodes {
            if !node.valid || !node.score.is_finite() {
                continue;
            }
            if node.score < score_min {
                score_min = node.score;
            }
            if node.score > score_max {
                score_max = node.score;
            }
        }
    }
    if score_min > score_max {
        return ObjectiveScale {
            score_min: 0.0,
            score_max: 0.0,
        };
    }
    ObjectiveScale {
        score_min,
        score_max,
    }
}

/// `clamp((best - min) / (max - min), 0, 1)`; a degenerate scale scores 1 at or
/// above its single value and 0 below.
#[must_use]
pub fn normalized_quality(best_score: f64, scale: ObjectiveScale) -> f64 {
    let span = scale.score_max - scale.score_min;
    if span.is_nan() || span <= 0.0 {
        return if best_score >= scale.score_max {
            1.0
        } else {
            0.0
        };
    }
    let raw = (best_score - scale.score_min) / span;
    raw.clamp(0.0, 1.0)
}

/// The probe budget `W * k1` and round cap `k1` a tree is scored against.
#[must_use]
pub fn objective_budget_of(budget: ObjectiveBudget) -> ObjectiveEvidence {
    let workers = budget.workers.max(1);
    let k1 = budget.k1.max(1);
    ObjectiveEvidence {
        probes: f64::from(workers) * f64::from(k1),
        rounds: f64::from(k1),
    }
}

/// The terms of one replay; without `evidence` the spend is the replay's own.
#[must_use]
pub fn compute_objective_terms(
    replay: &ReplayResult,
    cfg: &ReplayObjectiveConfig,
    scale: ObjectiveScale,
    budget: ObjectiveBudget,
    evidence: Option<ObjectiveEvidence>,
) -> ObjectiveTerms {
    let ObjectiveEvidence {
        probes: budget_probes,
        rounds: k1,
    } = objective_budget_of(budget);
    let quality = normalized_quality(replay.best_score, scale);
    let charged_count = replay.n + replay.out_of_support_cells;
    let charged = f64::from(charged_count);
    let mut anytime_sum = 0.0;
    for probe in 0..charged_count as usize {
        let score = replay
            .best_so_far
            .get(probe)
            .copied()
            .unwrap_or(replay.best_score);
        anytime_sum += normalized_quality(score, scale);
    }
    anytime_sum += (budget_probes - charged).max(0.0) * quality;
    let anytime = anytime_sum / budget_probes.max(charged);
    let charged_probes = evidence.map_or(charged, |evidence| charged.max(evidence.probes));
    let rounds = f64::from(replay.rounds);
    let charged_rounds = evidence.map_or(rounds, |evidence| rounds.max(evidence.rounds));
    let cost = charged_probes / budget_probes;
    let rounds_saved = 1.0 - charged_rounds / k1;
    let value = (1.0 - cfg.beta3) * quality + cfg.beta3 * anytime - cfg.beta1 * cost
        + cfg.beta2 * rounds_saved;
    ObjectiveTerms {
        quality,
        anytime,
        cost,
        rounds_saved,
        charged_probes,
        charged_rounds,
        value,
    }
}

/// V of one replay, charging its own spend.
#[must_use]
pub fn compute_objective(
    replay: &ReplayResult,
    cfg: &ReplayObjectiveConfig,
    scale: ObjectiveScale,
    budget: ObjectiveBudget,
) -> f64 {
    compute_objective_terms(replay, cfg, scale, budget, None).value
}
