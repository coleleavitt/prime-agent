//! Stage 3: dreaming policy improvement (TS `improve.ts`).
//!
//! From the current policy, [`propose_policies`] draws M candidates by seeded
//! mutation of the replay-live fields, each from its own `rng.fork("cand:<i>")`.
//! [`select_best_policy`] scores `{current} ∪ candidates` on the MEASURED pool
//! (the trees the current policy replays in full support) and returns the
//! argmax among the candidates whose replay quality is no lower than the
//! current policy's on EVERY measured tree; the current policy wins ties. A
//! candidate's spend is evidence-backed (charged at the horizon its replays on
//! the OTHER measured trees establish, the whole budget when alone) while the
//! incumbent is charged its raw spend. Every candidate gets a
//! [`CandidateVerdict`], and every step runs a [`run_lever_scan`] over a fixed
//! grid so the record says whether the pool had ANY lever.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::objective::{
    DEFAULT_OBJECTIVE,
    ObjectiveBudget,
    ObjectiveEvidence,
    ObjectiveScale,
    ObjectiveTerms,
    ReplayObjectiveConfig,
    compute_objective_terms,
    objective_budget_of,
    pool_score_scale,
};
use crate::policy::{
    ExplorationPolicy,
    POLICY_BOUNDS,
    PolicyField,
    REPLAY_DEAD_FIELDS,
    SELECTION_RULES,
    STOP_RULES,
    bound_of,
    clamp_policy,
    differs_only_in_replay_dead_fields,
    policy_fields_differing,
    policy_id,
};
use crate::replay::{ReplayConfig, ReplayResult, simulate_policy};
use crate::rng::SeededRng;
use crate::store::RecordedTree;

/// Ties in V (and the quality guard) within this resolve in favour of current.
pub const SELECT_EPS: f64 = 1e-9;
/// Gaussian perturbation size as a fraction of a numeric field's range.
const MUTATION_SCALE: f64 = 0.15;
/// Probability a mutation flips a named rule instead of perturbing numbers.
const RULE_FLIP_PROBABILITY: f64 = 0.34;
/// Sub-fork retries before a mutation that keeps landing on current is forced to differ.
const MUTATION_RETRIES: u32 = 8;
/// The `beta` values the lever-scan grid takes.
pub const LEVER_SCAN_BETAS: &[u32] = &[1, 2, 3, 4, 6, 8, 12];

/// Who generated a candidate policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateOrigin {
    Local,
    Llm,
}

impl CandidateOrigin {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Llm => "llm",
        }
    }
}

/// Who produced a dreaming step's candidate set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DreamerKind {
    Llm,
    Local,
    Mixed,
}

impl DreamerKind {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Local => "local",
            Self::Mixed => "mixed",
        }
    }
}

/// Why a candidate was or was not chosen (TS `CANDIDATE_REASONS`, in order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateReason {
    Winner,
    Tie,
    Worse,
    QualityRejected,
    Unmeasurable,
    Revoked,
    Identical,
    Duplicate,
}

impl CandidateReason {
    /// The wire literal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Winner => "winner",
            Self::Tie => "tie",
            Self::Worse => "worse",
            Self::QualityRejected => "quality-rejected",
            Self::Unmeasurable => "unmeasurable",
            Self::Revoked => "revoked",
            Self::Identical => "identical",
            Self::Duplicate => "duplicate",
        }
    }
}

/// One scored candidate of a dreaming step: pool means over the measured trees plus the verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateVerdict {
    pub index: usize,
    pub policy_id: String,
    pub policy: ExplorationPolicy,
    pub origin: CandidateOrigin,
    /// Fields differing from the current policy, in schema order (wire keys).
    pub changed: Vec<String>,
    pub duplicate_of: Option<usize>,
    pub value: f64,
    pub quality: f64,
    pub anytime: f64,
    pub cost: f64,
    pub rounds_saved: f64,
    #[serde(rename = "N")]
    pub n: f64,
    pub rounds: f64,
    pub out_of_support_cells: f64,
    pub in_support_mean: f64,
    pub in_support_min: f64,
    pub charged_probes: f64,
    pub charged_rounds: f64,
    pub evidence_trees: usize,
    pub eligible: bool,
    pub reason: CandidateReason,
}

/// The lever scan of a dreaming step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeverScanRecord {
    /// Distinct grid policies scored (current included).
    pub policies: usize,
    /// Grid policies that passed the quality guard.
    pub eligible: usize,
    pub best_value: f64,
    pub best_policy_id: String,
    /// How far the best eligible grid policy beats current (0 when none does).
    pub gap: f64,
    /// `simulate_policy` calls the scan made.
    pub simulations: usize,
}

/// The scoring knobs of a dreaming step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DreamingScoreConfig {
    /// Max online rounds; with a tree's `w` it fixes the probe budget.
    pub k1: u32,
    /// Max replay rounds per simulation.
    pub k2: u32,
    pub objective: ReplayObjectiveConfig,
    /// How far (pool-range units) a candidate's quality may fall below current's on any tree.
    pub quality_eps: f64,
}

/// A policy's mean replay terms over a pool plus its support coverage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolScore {
    pub value: f64,
    pub quality: f64,
    pub anytime: f64,
    pub cost: f64,
    pub rounds_saved: f64,
    #[serde(rename = "N")]
    pub n: f64,
    pub rounds: f64,
    pub out_of_support_cells: f64,
    pub in_support_mean: f64,
    pub in_support_min: f64,
    pub charged_probes: f64,
    pub charged_rounds: f64,
}

impl PoolScore {
    /// The score of an empty pool.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            value: 0.0,
            quality: 0.0,
            anytime: 0.0,
            cost: 0.0,
            rounds_saved: 0.0,
            n: 0.0,
            rounds: 0.0,
            out_of_support_cells: 0.0,
            in_support_mean: 1.0,
            in_support_min: 1.0,
            charged_probes: 0.0,
            charged_rounds: 0.0,
        }
    }
}

/// How a policy's spend is charged on a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendCharge {
    /// A candidate: each tree at least at the horizon its other replays establish.
    Evidence,
    /// The incumbent: exactly what it recorded.
    Raw,
}

fn mutate_once(policy: &ExplorationPolicy, rng: &mut SeededRng) -> ExplorationPolicy {
    let mut next = policy.to_value();
    if rng.next() < RULE_FLIP_PROBABILITY {
        if rng.next_int(2) == 0 {
            next["selectionRule"] =
                json!(SELECTION_RULES[rng.next_int(SELECTION_RULES.len())].as_str());
        } else {
            next["stopRule"] = json!(STOP_RULES[rng.next_int(STOP_RULES.len())].as_str());
        }
    } else {
        let mutable: Vec<PolicyField> = POLICY_BOUNDS
            .iter()
            .map(|(field, _)| *field)
            .filter(|field| !REPLAY_DEAD_FIELDS.contains(field))
            .collect();
        let count = 1 + rng.next_int(2);
        for _ in 0..count {
            let field = mutable[rng.next_int(mutable.len())];
            let Some(bound) = bound_of(field) else {
                continue;
            };
            let span = bound.max - bound.min;
            let current = policy.numeric(field).unwrap_or(0.0);
            next[field.as_str()] =
                crate::json::number(current + rng.next_gaussian() * span * MUTATION_SCALE);
        }
    }
    clamp_policy(&next)
}

/// Perturb one or two replay-live numeric fields, or flip the selection or
/// stop rule; never returns the current policy's id.
#[must_use]
pub fn mutate_policy(policy: &ExplorationPolicy, rng: &mut SeededRng) -> ExplorationPolicy {
    let current = policy_id(policy);
    for attempt in 0..=MUTATION_RETRIES {
        let candidate = if attempt == 0 {
            mutate_once(policy, rng)
        } else {
            mutate_once(policy, &mut rng.fork(&format!("retry:{attempt}")))
        };
        if policy_id(&candidate) != current {
            return candidate;
        }
    }
    let index = SELECTION_RULES
        .iter()
        .position(|rule| *rule == policy.selection_rule)
        .unwrap_or(0);
    ExplorationPolicy {
        selection_rule: SELECTION_RULES[(index + 1) % SELECTION_RULES.len()],
        ..*policy
    }
}

/// M candidates, each from `rng.fork("cand:<i>")` (order-independent).
#[must_use]
pub fn propose_policies(
    current: &ExplorationPolicy,
    m: usize,
    rng: &SeededRng,
) -> Vec<ExplorationPolicy> {
    (0..m)
        .map(|index| mutate_policy(current, &mut rng.fork(&format!("cand:{index}"))))
        .collect()
}

fn simulate_on_pool(
    policy: &ExplorationPolicy,
    sorted: &[&RecordedTree],
    cfg: &DreamingScoreConfig,
) -> Vec<ReplayResult> {
    sorted
        .iter()
        .map(|tree| simulate_policy(tree, policy, ReplayConfig { k2: cfg.k2 }))
        .collect()
}

fn evidence_for(
    replays: &[ReplayResult],
    index: usize,
    tree: &RecordedTree,
    cfg: &DreamingScoreConfig,
) -> ObjectiveEvidence {
    if replays.len() <= 1 {
        return objective_budget_of(ObjectiveBudget {
            workers: tree.header.w,
            k1: cfg.k1,
        });
    }
    let mut probes = 0;
    let mut rounds = 0;
    for (other_index, other) in replays.iter().enumerate() {
        if other_index == index {
            continue;
        }
        probes = probes.max(other.probes_to_best);
        rounds = rounds.max(other.rounds_to_best);
    }
    ObjectiveEvidence {
        probes: f64::from(probes),
        rounds: f64::from(rounds),
    }
}

/// The objective terms of one replay per tree, each tree's spend charged per `charge`.
#[must_use]
pub fn terms_on_pool(
    replays: &[ReplayResult],
    sorted: &[&RecordedTree],
    cfg: &DreamingScoreConfig,
    scale: ObjectiveScale,
    charge: SpendCharge,
) -> Vec<ObjectiveTerms> {
    sorted
        .iter()
        .enumerate()
        .map(|(index, tree)| {
            compute_objective_terms(
                &replays[index],
                &cfg.objective,
                scale,
                ObjectiveBudget {
                    workers: tree.header.w,
                    k1: cfg.k1,
                },
                match charge {
                    SpendCharge::Evidence => Some(evidence_for(replays, index, tree, cfg)),
                    SpendCharge::Raw => None,
                },
            )
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)] // pool sizes are tiny
fn aggregate_replays(replays: &[ReplayResult], terms: &[ObjectiveTerms]) -> PoolScore {
    if replays.is_empty() {
        return PoolScore::empty();
    }
    let mut sum = PoolScore::empty();
    sum.in_support_mean = 0.0;
    for (replay, term) in replays.iter().zip(terms) {
        sum.value += term.value;
        sum.quality += term.quality;
        sum.anytime += term.anytime;
        sum.cost += term.cost;
        sum.rounds_saved += term.rounds_saved;
        sum.charged_probes += term.charged_probes;
        sum.charged_rounds += term.charged_rounds;
        sum.n += f64::from(replay.n);
        sum.rounds += f64::from(replay.rounds);
        sum.out_of_support_cells += f64::from(replay.out_of_support_cells);
        sum.in_support_mean += replay.in_support;
        if replay.in_support < sum.in_support_min {
            sum.in_support_min = replay.in_support;
        }
    }
    let count = replays.len() as f64;
    PoolScore {
        value: sum.value / count,
        quality: sum.quality / count,
        anytime: sum.anytime / count,
        cost: sum.cost / count,
        rounds_saved: sum.rounds_saved / count,
        n: sum.n / count,
        rounds: sum.rounds / count,
        out_of_support_cells: sum.out_of_support_cells / count,
        in_support_mean: sum.in_support_mean / count,
        in_support_min: sum.in_support_min,
        charged_probes: sum.charged_probes / count,
        charged_rounds: sum.charged_rounds / count,
    }
}

struct PoolTerms {
    score: PoolScore,
    terms: Vec<ObjectiveTerms>,
}

fn pool_terms_of(
    replays: &[ReplayResult],
    sorted: &[&RecordedTree],
    cfg: &DreamingScoreConfig,
    scale: ObjectiveScale,
    charge: SpendCharge,
) -> PoolTerms {
    let terms = terms_on_pool(replays, sorted, cfg, scale, charge);
    PoolTerms {
        score: aggregate_replays(replays, &terms),
        terms,
    }
}

/// Measured trees minus one, floor 0: the trees that can vouch for a stop-early credit.
#[must_use]
pub fn evidence_trees_of(measured_trees: usize) -> usize {
    measured_trees.saturating_sub(1)
}

fn sorted_pool(pool: &[RecordedTree]) -> Vec<&RecordedTree> {
    let mut sorted: Vec<&RecordedTree> = pool.iter().collect();
    sorted.sort_by(|a, b| crate::collate::locale_compare(&a.header.tree_id, &b.header.tree_id));
    sorted
}

/// A policy's mean replay objective over the pool AS GIVEN, on the pool's own scale.
#[must_use]
pub fn score_policy_on_pool(
    policy: &ExplorationPolicy,
    pool: &[RecordedTree],
    cfg: &DreamingScoreConfig,
    charge: SpendCharge,
) -> PoolScore {
    let sorted = sorted_pool(pool);
    let replays = simulate_on_pool(policy, &sorted, cfg);
    pool_terms_of(&replays, &sorted, cfg, pool_score_scale(pool), charge).score
}

/// The trees the current policy replays in full support, with its replays.
pub struct MeasuredPool<'a> {
    /// Trees handed in, sorted by tree id.
    pub sorted: Vec<&'a RecordedTree>,
    /// The subset whose incumbent replay charged no out-of-support cell.
    pub measured: Vec<&'a RecordedTree>,
    /// The incumbent's replay on each measured tree.
    pub replays: Vec<ReplayResult>,
    /// The incumbent's mean in-support share over ALL trees (1 for an empty pool).
    pub current_in_support: f64,
}

/// Split a pool into the measured trees and the rest (one simulation per tree).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn measure_pool<'a>(
    current: &ExplorationPolicy,
    pool: &'a [RecordedTree],
    cfg: &DreamingScoreConfig,
) -> MeasuredPool<'a> {
    let sorted = sorted_pool(pool);
    let all = simulate_on_pool(current, &sorted, cfg);
    let mut measured = Vec::new();
    let mut replays = Vec::new();
    let mut in_support_sum = 0.0;
    for (tree, replay) in sorted.iter().zip(all) {
        in_support_sum += replay.in_support;
        if replay.out_of_support_cells == 0 {
            measured.push(*tree);
            replays.push(replay);
        }
    }
    let current_in_support = if sorted.is_empty() {
        1.0
    } else {
        in_support_sum / sorted.len() as f64
    };
    MeasuredPool {
        sorted,
        measured,
        replays,
        current_in_support,
    }
}

/// A candidate with its provenance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateInput {
    pub policy: ExplorationPolicy,
    pub origin: CandidateOrigin,
}

impl From<ExplorationPolicy> for CandidateInput {
    fn from(policy: ExplorationPolicy) -> Self {
        Self {
            policy,
            origin: CandidateOrigin::Local,
        }
    }
}

/// `llm` when every candidate came from a child agent, `local` when none did, else `mixed`.
#[must_use]
pub fn dreamer_kind_of(candidates: &[CandidateInput]) -> DreamerKind {
    let llm = candidates
        .iter()
        .filter(|candidate| candidate.origin == CandidateOrigin::Llm)
        .count();
    if llm == 0 {
        DreamerKind::Local
    } else if llm == candidates.len() {
        DreamerKind::Llm
    } else {
        DreamerKind::Mixed
    }
}

/// The outcome of [`select_best_policy`].
#[derive(Debug, Clone, PartialEq)]
pub struct PolicySelection {
    pub chosen_policy: ExplorationPolicy,
    pub chosen_score: f64,
    pub current_score: f64,
    pub chosen_quality: f64,
    pub current_quality: f64,
    /// The current policy's full score on the measured pool (spend charged raw).
    pub current: PoolScore,
    /// The current policy's lowest replay best over the measured trees: the probation floor.
    pub current_min_best: f64,
    pub improved: bool,
    /// Current plus every distinct, non-identical candidate.
    pub scored_count: usize,
    /// Verdicts with reason `quality-rejected`.
    pub quality_rejected: usize,
    pub candidate_policy_ids: Vec<String>,
    /// One verdict per candidate, in input order.
    pub candidates: Vec<CandidateVerdict>,
    pub dreamer: DreamerKind,
    pub pool_size: usize,
    pub measured_trees: usize,
    pub evidence_trees: usize,
    pub current_in_support: f64,
    /// `simulate_policy` calls made.
    pub simulations: usize,
}

#[allow(clippy::struct_excessive_bools)] // the decision table's flags, as the TS entry
struct ScoredEntry {
    input: CandidateInput,
    index: usize,
    id: String,
    changed: Vec<String>,
    duplicate_of: Option<usize>,
    identical: bool,
    replay_dead: bool,
    revoked: bool,
    score: PoolScore,
    eligible: bool,
    quality_rejected: bool,
}

fn passes_quality_guard(
    terms: &[ObjectiveTerms],
    baseline: &[ObjectiveTerms],
    quality_eps: f64,
) -> bool {
    let slack = quality_eps.max(0.0) + SELECT_EPS;
    terms
        .iter()
        .zip(baseline)
        .all(|(term, base)| term.quality >= base.quality - slack)
}

/// Score `{current} ∪ candidates` on the measured pool and return the argmax
/// over the eligible entries; see the module docs and `docs/dream-rsi.md`.
#[must_use]
#[allow(clippy::too_many_lines)] // one decision table, kept in one place as in the TS
pub fn select_best_policy<S: std::hash::BuildHasher>(
    current: &ExplorationPolicy,
    candidates: &[CandidateInput],
    pool: &[RecordedTree],
    cfg: &DreamingScoreConfig,
    revoked: &HashSet<String, S>,
) -> PolicySelection {
    let measured_pool = measure_pool(current, pool, cfg);
    let measured = &measured_pool.measured;
    let measurable = !measured.is_empty();
    let scale = pool_score_scale(measured.iter().copied());
    let current_id = policy_id(current);
    let current_terms = pool_terms_of(
        &measured_pool.replays,
        measured,
        cfg,
        scale,
        SpendCharge::Raw,
    );
    let current_score = current_terms.score;
    let current_min_best = if measurable {
        measured_pool
            .replays
            .iter()
            .fold(f64::INFINITY, |min, replay| {
                if replay.best_score < min {
                    replay.best_score
                } else {
                    min
                }
            })
    } else {
        0.0
    };
    let mut simulations = measured_pool.sorted.len();

    let mut entries: Vec<ScoredEntry> = Vec::with_capacity(candidates.len());
    let mut passed: HashMap<usize, bool> = HashMap::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (index, input) in candidates.iter().enumerate() {
        let id = policy_id(&input.policy);
        let changed: Vec<String> = policy_fields_differing(&input.policy, current)
            .into_iter()
            .map(|field| field.as_str().to_string())
            .collect();
        let identical = id == current_id;
        let duplicate_of = if identical {
            None
        } else {
            seen.get(&id).copied()
        };
        if !identical && duplicate_of.is_none() {
            seen.insert(id.clone(), index);
        }
        let replay_dead = !identical && differs_only_in_replay_dead_fields(&input.policy, current);
        let is_revoked = !identical && revoked.contains(&id);
        let (score, passed_guard) = if identical || replay_dead {
            (current_score, measurable)
        } else if let Some(earlier) = duplicate_of {
            (
                entries[earlier].score,
                passed.get(&earlier).copied().unwrap_or(false),
            )
        } else {
            let replays = simulate_on_pool(&input.policy, measured, cfg);
            let scored = pool_terms_of(&replays, measured, cfg, scale, SpendCharge::Evidence);
            simulations += measured.len();
            let guard = measurable
                && passes_quality_guard(&scored.terms, &current_terms.terms, cfg.quality_eps);
            (scored.score, guard)
        };
        passed.insert(index, passed_guard);
        let simulated = !identical && duplicate_of.is_none() && !replay_dead;
        entries.push(ScoredEntry {
            input: *input,
            index,
            id,
            changed,
            duplicate_of,
            identical,
            replay_dead,
            revoked: is_revoked,
            score,
            eligible: simulated && passed_guard && !is_revoked,
            quality_rejected: simulated && measurable && !passed_guard,
        });
    }

    let mut max_score = current_score.value;
    for entry in entries.iter().filter(|entry| entry.eligible) {
        if entry.score.value > max_score {
            max_score = entry.score.value;
        }
    }
    let current_wins = current_score.value >= max_score - SELECT_EPS;
    let winner = if current_wins {
        None
    } else {
        entries
            .iter()
            .filter(|entry| entry.eligible && entry.score.value >= max_score - SELECT_EPS)
            .min_by(|a, b| crate::collate::locale_compare(&a.id, &b.id))
    };
    let chosen = winner.filter(|entry| entry.score.value > current_score.value + SELECT_EPS);
    let improved = chosen.is_some();
    let chosen_score = chosen.map_or(current_score, |entry| entry.score);
    let chosen_index = chosen.map(|entry| entry.index);
    let evidence_trees = evidence_trees_of(measured.len());

    let verdicts: Vec<CandidateVerdict> = entries
        .iter()
        .map(|entry| {
            let off_support = entry.score.in_support_min < 1.0;
            let reason = if entry.identical {
                CandidateReason::Identical
            } else if entry.duplicate_of.is_some() {
                CandidateReason::Duplicate
            } else if entry.revoked {
                CandidateReason::Revoked
            } else if chosen_index == Some(entry.index) {
                CandidateReason::Winner
            } else if entry.replay_dead || off_support || !measurable {
                CandidateReason::Unmeasurable
            } else if entry.quality_rejected {
                CandidateReason::QualityRejected
            } else if entry.score.value >= chosen_score.value - SELECT_EPS {
                CandidateReason::Tie
            } else {
                CandidateReason::Worse
            };
            CandidateVerdict {
                index: entry.index,
                policy_id: entry.id.clone(),
                policy: entry.input.policy,
                origin: entry.input.origin,
                changed: entry.changed.clone(),
                duplicate_of: entry.duplicate_of,
                value: entry.score.value,
                quality: entry.score.quality,
                anytime: entry.score.anytime,
                cost: entry.score.cost,
                rounds_saved: entry.score.rounds_saved,
                n: entry.score.n,
                rounds: entry.score.rounds,
                out_of_support_cells: entry.score.out_of_support_cells,
                in_support_mean: entry.score.in_support_mean,
                in_support_min: entry.score.in_support_min,
                charged_probes: entry.score.charged_probes,
                charged_rounds: entry.score.charged_rounds,
                evidence_trees,
                eligible: entry.eligible,
                reason,
            }
        })
        .collect();

    PolicySelection {
        chosen_policy: chosen.map_or(*current, |entry| entry.input.policy),
        chosen_score: chosen_score.value,
        current_score: current_score.value,
        chosen_quality: chosen_score.quality,
        current_quality: current_score.quality,
        current: current_score,
        current_min_best,
        improved,
        scored_count: 1 + entries
            .iter()
            .filter(|entry| !entry.identical && entry.duplicate_of.is_none())
            .count(),
        quality_rejected: verdicts
            .iter()
            .filter(|verdict| verdict.reason == CandidateReason::QualityRejected)
            .count(),
        candidate_policy_ids: entries.iter().map(|entry| entry.id.clone()).collect(),
        candidates: verdicts,
        dreamer: dreamer_kind_of(candidates),
        pool_size: measured_pool.sorted.len(),
        measured_trees: measured.len(),
        evidence_trees,
        current_in_support: measured_pool.current_in_support,
        simulations,
    }
}

/// The fixed lever-scan grid around `current`: current first, then every
/// selection rule x stop rule x batchSize 1..min(W, 8) x [`LEVER_SCAN_BETAS`],
/// deduplicated by policy id. Touches no rng.
#[must_use]
pub fn lever_scan_grid(current: &ExplorationPolicy, workers: u32) -> Vec<ExplorationPolicy> {
    let max_batch = bound_of(PolicyField::BatchSize)
        .map_or(8.0, |bound| bound.max)
        .min(f64::from(workers.max(1)));
    let mut out = Vec::new();
    let mut ids = HashSet::new();
    let mut push = |policy: ExplorationPolicy| {
        if ids.insert(policy_id(&policy)) {
            out.push(policy);
        }
    };
    push(*current);
    for selection_rule in SELECTION_RULES {
        for stop_rule in STOP_RULES {
            let mut batch_size = 1;
            while f64::from(batch_size) <= max_batch {
                for beta in LEVER_SCAN_BETAS {
                    let mut raw = current.to_value();
                    raw["selectionRule"] = Value::from(selection_rule.as_str());
                    raw["stopRule"] = Value::from(stop_rule.as_str());
                    raw["batchSize"] = Value::from(batch_size);
                    raw["beta"] = Value::from(*beta);
                    push(clamp_policy(&raw));
                }
                batch_size += 1;
            }
        }
    }
    out
}

fn pool_workers(pool: &[RecordedTree]) -> u32 {
    pool.iter().map(|tree| tree.header.w).fold(1, u32::max)
}

/// Score the lever-scan grid under the selection rule and summarize it.
#[must_use]
pub fn run_lever_scan(
    current: &ExplorationPolicy,
    pool: &[RecordedTree],
    cfg: &DreamingScoreConfig,
) -> LeverScanRecord {
    let grid: Vec<CandidateInput> = lever_scan_grid(current, pool_workers(pool))
        .into_iter()
        .map(CandidateInput::from)
        .collect();
    let selection = select_best_policy(current, &grid, pool, cfg, &HashSet::new());
    LeverScanRecord {
        policies: grid.len(),
        eligible: selection
            .candidates
            .iter()
            .filter(|verdict| verdict.eligible)
            .count(),
        best_value: selection.chosen_score,
        best_policy_id: policy_id(&selection.chosen_policy),
        gap: if selection.improved {
            selection.chosen_score - selection.current_score
        } else {
            0.0
        },
        simulations: selection.simulations,
    }
}

/// A dreaming step's candidate source (the seam the in-session LLM dreamer
/// implements). It must return already-parsed, in-bounds policies; they are
/// scored and selected by exactly the rule local candidates are.
pub trait CandidateSource {
    /// Up to `m` revised policies around `current`, drawing only from `rng`.
    fn propose(
        &mut self,
        current: &ExplorationPolicy,
        m: usize,
        rng: &SeededRng,
    ) -> Vec<CandidateInput>;
}

/// The zero-token local dreamer: [`propose_policies`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalDreamer;

impl CandidateSource for LocalDreamer {
    fn propose(
        &mut self,
        current: &ExplorationPolicy,
        m: usize,
        rng: &SeededRng,
    ) -> Vec<CandidateInput> {
        propose_policies(current, m, rng)
            .into_iter()
            .map(CandidateInput::from)
            .collect()
    }
}

/// One dreaming step's inputs.
pub struct DreamingOptions<'a, 'c> {
    pub current: ExplorationPolicy,
    pub pool: &'a [RecordedTree],
    /// Revised policies M per step.
    pub dreams: usize,
    pub k1: u32,
    pub k2: u32,
    pub rng: SeededRng,
    pub objective: ReplayObjectiveConfig,
    pub quality_eps: f64,
    /// Loop iteration, stamped on the spans.
    pub iteration: u32,
    /// The candidate source; the local dreamer when `None`.
    pub candidates: Option<&'a mut (dyn CandidateSource + 'c)>,
    /// Run the lever scan (rng-free; on by default).
    pub lever_scan: bool,
    /// Policy ids this run adopted and reverted after probation.
    pub revoked: &'a HashSet<String>,
}

/// One dreaming step's outcome.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamResult {
    pub chosen_policy: ExplorationPolicy,
    pub chosen_policy_id: String,
    pub chosen_score: f64,
    pub current_score: f64,
    pub chosen_quality: f64,
    pub current_quality: f64,
    pub current: PoolScore,
    pub current_min_best: f64,
    pub improved: bool,
    pub scored_count: usize,
    pub quality_rejected: usize,
    pub candidate_policy_ids: Vec<String>,
    pub candidates: Vec<CandidateVerdict>,
    pub dreamer: DreamerKind,
    pub lever_scan: Option<LeverScanRecord>,
    pub pool_size: usize,
    pub measured_trees: usize,
    pub evidence_trees: usize,
    pub simulations: usize,
    pub tokens: u64,
}

/// One dreaming step: propose M candidates, select the no-worse policy, run the
/// lever scan. Opens `dream.dream` wrapping one coarse `dream.replay` and one
/// zero-duration `dream.candidate` per verdict.
#[must_use]
#[allow(clippy::too_many_lines)] // the span table is long; the logic is three calls
pub fn run_dreaming(options: DreamingOptions<'_, '_>) -> DreamResult {
    let cfg = DreamingScoreConfig {
        k1: options.k1,
        k2: options.k2,
        objective: options.objective,
        quality_eps: options.quality_eps,
    };
    let iteration = options.iteration;
    let candidates = match options.candidates {
        Some(source) => source.propose(&options.current, options.dreams, &options.rng),
        None => LocalDreamer.propose(&options.current, options.dreams, &options.rng),
    };
    let span = tracing::info_span!(
        "dream.dream",
        dream.candidates = candidates.len(),
        dream.pool_size = options.pool.len(),
        dream.iteration = iteration,
        dream.chosen_policy_id = tracing::field::Empty,
        dream.chosen_score = tracing::field::Empty,
        dream.current_score = tracing::field::Empty,
        dream.chosen_quality = tracing::field::Empty,
        dream.current_quality = tracing::field::Empty,
        dream.quality_rejected = tracing::field::Empty,
        dream.unmeasurable = tracing::field::Empty,
        dream.improved = tracing::field::Empty,
        dream.dreamer = tracing::field::Empty,
        dream.in_support_current = tracing::field::Empty,
        dream.measured_trees = tracing::field::Empty,
        dream.evidence_trees = tracing::field::Empty,
        dream.simulations = tracing::field::Empty,
        dream.lever_gap = tracing::field::Empty,
        dream.lever_policies = tracing::field::Empty,
        dream.lever_simulations = tracing::field::Empty,
    );
    let _entered = span.enter();
    let selection = {
        let replay_span = tracing::info_span!(
            "dream.replay",
            dream.policy_id = %policy_id(&options.current),
            dream.iteration = iteration,
            dream.simulations = tracing::field::Empty,
            dream.measured_trees = tracing::field::Empty,
        );
        let _replay = replay_span.enter();
        let selected = select_best_policy(
            &options.current,
            &candidates,
            options.pool,
            &cfg,
            options.revoked,
        );
        replay_span.record("dream.simulations", selected.simulations);
        replay_span.record("dream.measured_trees", selected.measured_trees);
        selected
    };
    let lever_scan = options
        .lever_scan
        .then(|| run_lever_scan(&options.current, options.pool, &cfg));
    for verdict in &selection.candidates {
        let _candidate = tracing::info_span!(
            "dream.candidate",
            dream.iteration = iteration,
            dream.candidate_index = verdict.index,
            dream.policy_id = %verdict.policy_id,
            dream.origin = verdict.origin.as_str(),
            dream.reason = verdict.reason.as_str(),
            dream.eligible = verdict.eligible,
            dream.value = verdict.value,
            dream.quality = verdict.quality,
            dream.anytime = verdict.anytime,
            dream.cost = verdict.cost,
            dream.rounds_saved = verdict.rounds_saved,
            dream.in_support_min = verdict.in_support_min,
            dream.charged_probes = verdict.charged_probes,
            dream.charged_rounds = verdict.charged_rounds,
            dream.changed = %verdict.changed.join(","),
        )
        .entered();
    }
    span.record(
        "dream.chosen_policy_id",
        policy_id(&selection.chosen_policy).as_str(),
    );
    span.record("dream.chosen_score", selection.chosen_score);
    span.record("dream.current_score", selection.current_score);
    span.record("dream.chosen_quality", selection.chosen_quality);
    span.record("dream.current_quality", selection.current_quality);
    span.record("dream.quality_rejected", selection.quality_rejected);
    span.record(
        "dream.unmeasurable",
        selection
            .candidates
            .iter()
            .filter(|verdict| verdict.reason == CandidateReason::Unmeasurable)
            .count(),
    );
    span.record("dream.improved", selection.improved);
    span.record("dream.dreamer", selection.dreamer.as_str());
    span.record("dream.in_support_current", selection.current_in_support);
    span.record("dream.measured_trees", selection.measured_trees);
    span.record("dream.evidence_trees", selection.evidence_trees);
    span.record("dream.simulations", selection.simulations);
    if let Some(scan) = &lever_scan {
        span.record("dream.lever_gap", scan.gap);
        span.record("dream.lever_policies", scan.policies);
        span.record("dream.lever_simulations", scan.simulations);
    }
    DreamResult {
        chosen_policy_id: policy_id(&selection.chosen_policy),
        chosen_policy: selection.chosen_policy,
        chosen_score: selection.chosen_score,
        current_score: selection.current_score,
        chosen_quality: selection.chosen_quality,
        current_quality: selection.current_quality,
        current: selection.current,
        current_min_best: selection.current_min_best,
        improved: selection.improved,
        scored_count: selection.scored_count,
        quality_rejected: selection.quality_rejected,
        candidate_policy_ids: selection.candidate_policy_ids,
        candidates: selection.candidates,
        dreamer: selection.dreamer,
        lever_scan,
        pool_size: options.pool.len(),
        measured_trees: selection.measured_trees,
        evidence_trees: selection.evidence_trees,
        simulations: selection.simulations,
        tokens: 0,
    }
}

/// The default scoring config for a step: `DEFAULT_OBJECTIVE`, no quality slack.
#[must_use]
pub fn score_config(k1: u32, k2: u32) -> DreamingScoreConfig {
    DreamingScoreConfig {
        k1,
        k2,
        objective: DEFAULT_OBJECTIVE,
        quality_eps: 0.0,
    }
}
