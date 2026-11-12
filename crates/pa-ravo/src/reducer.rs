//! The pure RAVO reducer (TS `ravo/reducer.ts`): a lineage of committed
//! champions, a weighted opponent pool, and the one step that consumes a
//! proposal's evaluation and decides whether it commits.
//!
//! All scores and weights are non-negative safe integers, so every gate
//! comparison is exact and the whole state is JSON-safe. Serialized field
//! order follows the TS object literals, so a state written here is the
//! bytes the TS product writes.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::js::{locale_compare, sorted_unique};

/// `Number.MAX_SAFE_INTEGER`.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// A gate observation's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateStatus {
    Pass,
    Fail,
    Abstain,
    Error,
}

/// One opponent: a judged criterion with a seed and a current weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoCriterion {
    pub id: String,
    pub seed_weight: u64,
    pub current_weight: u64,
}

impl RavoCriterion {
    /// A new opponent at seed weight 1.
    #[must_use]
    pub fn seeded(id: &str) -> Self {
        Self {
            id: id.to_string(),
            seed_weight: 1,
            current_weight: 1,
        }
    }
}

/// The opponent pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RavoOpponentPool {
    pub criteria: Vec<RavoCriterion>,
}

/// The clock a provisional window is measured on: `ordinal` (the global
/// ledger's observation total) or `local-ordinal` (one session ledger's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RavoWindowClock {
    #[serde(rename = "ordinal")]
    Ordinal,
    #[serde(rename = "local-ordinal")]
    LocalOrdinal,
}

impl RavoWindowClock {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ordinal => "ordinal",
            Self::LocalOrdinal => "local-ordinal",
        }
    }
}

/// A claimed fingerprint that recurred inside a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedRecurrence {
    pub turn: u64,
    pub fingerprints: Vec<String>,
}

/// The observation window of a provisional champion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoProvisionalWindow {
    pub committed_turn: u64,
    pub until_turn: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<RavoWindowClock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_recurrence: Option<ObservedRecurrence>,
}

/// One committed candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoChampion {
    pub proposal_id: String,
    pub parent_id: Option<String>,
    pub score: u64,
    pub artifact: Value,
    pub missed_criterion_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_fingerprints: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisional: Option<RavoProvisionalWindow>,
    /// Keys a later build wrote, carried through.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The reducer state: the harness file's `ravo` key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoState {
    pub lineage: Vec<RavoChampion>,
    pub champion_id: Option<String>,
    pub opponents: RavoOpponentPool,
    pub evaluated_proposal_ids: Vec<String>,
    /// Keys a later build wrote, carried through.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One proposal: an id and the artifact it would commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RavoProposal {
    pub id: String,
    pub artifact: Value,
}

/// A screen or deep observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RavoObservation {
    pub status: GateStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One opponent's observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoCriterionObservation {
    pub criterion_id: String,
    pub status: GateStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A completed evaluation of one proposal, consumed once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoEvaluation {
    pub proposal_id: String,
    pub screen: RavoObservation,
    pub deep: RavoObservation,
    pub criteria: Vec<RavoCriterionObservation>,
}

/// The gate thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoConfig {
    pub screen_threshold: u64,
    /// The current opponent weight a commit may miss.
    pub epsilon: u64,
    /// Slack under the best recorded score the deep gate tolerates.
    #[serde(default)]
    pub deep_tolerance: u64,
    /// Fork divergence: when true, a challenger that addresses no
    /// fingerprints faces bar 0 (uncontested territory) instead of the
    /// all-time max. Default `false` preserves TS golden parity;
    /// `RAVO_DEFAULT_CONFIG` sets it `true` so fact memories and
    /// session-state notes aren't gated by an unrelated champion.
    #[serde(default)]
    pub scope_unclaimed_bar_to_zero: bool,
    /// Fork divergence: when true, a proposal whose screen passes but
    /// whose deep judge errored (`GateStatus::Error`, e.g. the model
    /// returned an empty response or a parse failure) is admitted as
    /// unmeasured — the edit lands, state is left unchanged, no champion
    /// is crowned. Default `false` preserves TS parity; without this a
    /// transient judge-side model failure silently killed otherwise valid
    /// proposals with a `RavoRejection::Deep`.
    #[serde(default)]
    pub admit_screen_pass_on_judge_error: bool,
}

/// Why a step did not commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RavoRejection {
    AlreadyEvaluated,
    InvalidInput,
    Screen,
    Deep,
    Opponents,
    /// Issued by the assisted authority, never by [`ravo_step`].
    Unclaimed,
}

/// One opponent's line in a certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoCriterionCertificate {
    pub criterion_id: String,
    pub status: GateStatus,
    pub seed_weight: u64,
    pub current_weight: u64,
    pub counted_as_missed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The deterministic witness of one gate decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoGateCertificate {
    pub proposal_id: String,
    pub previous_champion_id: Option<String>,
    pub previous_best_score: u64,
    pub screen_threshold: u64,
    pub epsilon: u64,
    pub deep_tolerance: u64,
    pub screen: RavoObservation,
    pub deep: RavoObservation,
    pub criteria: Vec<RavoCriterionCertificate>,
    pub missed_criterion_ids: Vec<String>,
    pub missed_seed_weight: u64,
    pub missed_current_weight: u64,
    pub committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<RavoRejection>,
}

/// A step's next state and certificate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RavoStepResult {
    pub state: RavoState,
    pub certificate: RavoGateCertificate,
}

fn is_safe(value: u64) -> bool {
    value <= MAX_SAFE_INTEGER
}

fn checked_add(left: u64, right: u64) -> Option<u64> {
    left.checked_add(right).filter(|sum| is_safe(*sum))
}

/// The deep bar a challenger must clear, under the TS semantic. A
/// challenger that addresses one or more fingerprints is scoped to prior
/// champions whose `claimed_fingerprints` overlap it; a challenger that
/// addresses no fingerprints falls back to the all-time max. The fork
/// preserves this for TS golden parity and uses [`ravo_best_score_scoped`]
/// at the gate call site, where an unclaimed challenger is uncontested
/// territory instead of competing against every unrelated champion.
#[must_use]
pub fn ravo_best_score(lineage: &[RavoChampion], challenger_fingerprints: &[String]) -> u64 {
    if challenger_fingerprints.is_empty() {
        return lineage
            .iter()
            .map(|champion| champion.score)
            .max()
            .unwrap_or(0);
    }
    lineage
        .iter()
        .filter(|champion| {
            champion
                .claimed_fingerprints
                .as_deref()
                .is_some_and(|claimed| {
                    claimed
                        .iter()
                        .any(|fingerprint| challenger_fingerprints.contains(fingerprint))
                })
        })
        .map(|champion| champion.score)
        .max()
        .unwrap_or(0)
}

/// The fork's deep bar: like [`ravo_best_score`] but a challenger that
/// addresses no fingerprints is uncontested territory. Fact memories,
/// session-state notes and working-context memories face bar 0 instead of
/// the all-time max; the screen threshold and the deep absolute score are
/// still what decide admission. Symmetric with how `TrustWindow` refuses
/// to open on an empty `claimed_fingerprints` set.
///
/// The fork diverges from the TS semantic here deliberately: TS goldens
/// only exercise the empty branch because the TS run never hoisted
/// `addressedFingerprints` onto proposals. In the fork, a single
/// high-scoring champion on an unrelated topic was ratcheting the floor
/// across every unclaimed proposal on the machine.
#[must_use]
pub fn ravo_best_score_scoped(lineage: &[RavoChampion], challenger_fingerprints: &[String]) -> u64 {
    if challenger_fingerprints.is_empty() {
        return 0;
    }
    ravo_best_score(lineage, challenger_fingerprints)
}

/// Picks between [`ravo_best_score`] (TS parity) and
/// [`ravo_best_score_scoped`] (fork) based on
/// [`RavoConfig::scope_unclaimed_bar_to_zero`]. `ravo_step` uses this so the
/// TS goldens' three-field configs still deserialize and still match their
/// recorded `previousBestScore`, while `RAVO_DEFAULT_CONFIG` (the fork's
/// production config) picks the scoped semantic.
fn best_score_for(
    lineage: &[RavoChampion],
    challenger_fingerprints: &[String],
    config: &RavoConfig,
) -> u64 {
    if config.scope_unclaimed_bar_to_zero {
        ravo_best_score_scoped(lineage, challenger_fingerprints)
    } else {
        ravo_best_score(lineage, challenger_fingerprints)
    }
}

/// The `addressedFingerprints` the gate hoists onto a proposal artifact
/// (empty for an artifact that claims none). The reducer uses this to scope
/// the deep bar; see `ravo_best_score`.
#[must_use]
pub(crate) fn addressed_fingerprints_of_artifact(artifact: &Value) -> Vec<String> {
    artifact
        .get("addressedFingerprints")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// An empty state over `opponents`.
#[must_use]
pub fn empty_ravo_state(opponents: RavoOpponentPool) -> RavoState {
    RavoState {
        lineage: Vec::new(),
        champion_id: None,
        opponents,
        evaluated_proposal_ids: Vec::new(),
        extra: Map::new(),
    }
}

/// Distinct non-empty ids whose current weights are positive safe integers
/// dominating their seed weights.
#[must_use]
pub fn valid_ravo_weights(pool: &RavoOpponentPool) -> bool {
    let mut ids = HashSet::new();
    pool.criteria.iter().all(|criterion| {
        !criterion.id.is_empty()
            && ids.insert(criterion.id.as_str())
            && is_safe(criterion.seed_weight)
            && criterion.seed_weight > 0
            && is_safe(criterion.current_weight)
            && criterion.current_weight >= criterion.seed_weight
    })
}

fn valid_provisional_window(window: Option<&RavoProvisionalWindow>) -> bool {
    let Some(window) = window else {
        return true;
    };
    if !is_safe(window.committed_turn) || !is_safe(window.until_turn) {
        return false;
    }
    if window.until_turn < window.committed_turn {
        return false;
    }
    let Some(observed) = &window.observed_recurrence else {
        return true;
    };
    is_safe(observed.turn)
        && observed.turn >= window.committed_turn
        && observed.turn <= window.until_turn
        && observed.fingerprints.iter().all(|id| !id.is_empty())
}

/// `ravoW`: every serializable reducer invariant, the champion chain
/// included.
#[must_use]
pub fn ravo_w(state: &RavoState) -> bool {
    if !valid_ravo_weights(&state.opponents) {
        return false;
    }
    let evaluated: HashSet<&str> = state
        .evaluated_proposal_ids
        .iter()
        .map(String::as_str)
        .collect();
    if evaluated.len() != state.evaluated_proposal_ids.len() {
        return false;
    }
    let mut parent: Option<&str> = None;
    let mut committed = HashSet::new();
    for champion in &state.lineage {
        if champion.proposal_id.is_empty()
            || committed.contains(champion.proposal_id.as_str())
            || champion.parent_id.as_deref() != parent
        {
            return false;
        }
        if !is_safe(champion.score) || !valid_provisional_window(champion.provisional.as_ref()) {
            return false;
        }
        if champion
            .claimed_fingerprints
            .as_ref()
            .is_some_and(|ids| ids.iter().any(String::is_empty))
        {
            return false;
        }
        committed.insert(champion.proposal_id.as_str());
        parent = Some(champion.proposal_id.as_str());
    }
    if state.champion_id.as_deref() != parent {
        return false;
    }
    state
        .lineage
        .iter()
        .all(|champion| evaluated.contains(champion.proposal_id.as_str()))
}

/// The exact weighted opponent gate (`F_gatekeeper`).
#[must_use]
pub fn ravo_gatekeeper(missed_weight: u64, epsilon: u64) -> bool {
    is_safe(missed_weight) && is_safe(epsilon) && missed_weight <= epsilon
}

/// Weakness pressure: double each named criterion's current weight.
/// Unknown ids are a no-op; an overflow leaves the pool unchanged.
#[must_use]
pub fn ravo_pressure(pool: &RavoOpponentPool, weak_criterion_ids: &[String]) -> RavoOpponentPool {
    let weak: HashSet<&str> = weak_criterion_ids.iter().map(String::as_str).collect();
    let mut doubled = Vec::with_capacity(pool.criteria.len());
    let mut changed = false;
    for criterion in &pool.criteria {
        if !weak.contains(criterion.id.as_str()) {
            doubled.push(criterion.clone());
            continue;
        }
        if criterion.current_weight > MAX_SAFE_INTEGER / 2 {
            return pool.clone();
        }
        changed = true;
        doubled.push(RavoCriterion {
            current_weight: criterion.current_weight * 2,
            ..criterion.clone()
        });
    }
    if !changed {
        return pool.clone();
    }
    RavoOpponentPool { criteria: doubled }
}

/// Extend the pool with new criteria at seed weight 1; existing ids keep
/// their weights, so extension only tightens the gate.
#[must_use]
pub fn ravo_extend_opponents(
    pool: &RavoOpponentPool,
    criterion_ids: &[String],
) -> RavoOpponentPool {
    let mut existing: HashSet<&str> = pool.criteria.iter().map(|c| c.id.as_str()).collect();
    let mut added = Vec::new();
    for id in criterion_ids {
        if id.is_empty() || existing.contains(id.as_str()) {
            continue;
        }
        existing.insert(id.as_str());
        added.push(RavoCriterion::seeded(id));
    }
    if added.is_empty() {
        return pool.clone();
    }
    let mut criteria = pool.criteria.clone();
    criteria.extend(added);
    RavoOpponentPool { criteria }
}

/// A provisional window to open on a champion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSpan {
    pub committed_turn: u64,
    pub until_turn: u64,
    pub clock: Option<RavoWindowClock>,
}

/// Mark a committed champion provisional: the fingerprints it claimed and
/// its observation window. Unknown ids and invalid windows are no-ops.
#[must_use]
pub fn ravo_mark_provisional(
    state: &RavoState,
    champion_id: &str,
    claimed_fingerprints: &[String],
    window: Option<WindowSpan>,
) -> RavoState {
    let Some(index) = state
        .lineage
        .iter()
        .position(|champion| champion.proposal_id == champion_id)
    else {
        return state.clone();
    };
    let claimed = sorted_unique(
        claimed_fingerprints
            .iter()
            .filter(|id| !id.is_empty())
            .cloned(),
    );
    let provisional = window.map(|window| RavoProvisionalWindow {
        committed_turn: window.committed_turn,
        until_turn: window.until_turn,
        clock: window.clock,
        observed_recurrence: None,
    });
    if !valid_provisional_window(provisional.as_ref()) {
        return state.clone();
    }
    let mut next = state.clone();
    let champion = &mut next.lineage[index];
    champion.claimed_fingerprints = Some(claimed);
    if provisional.is_some() {
        champion.provisional = provisional;
    }
    next
}

/// Observe recurred fingerprints against a champion: a claimed one inside
/// its window is a measured fault, recorded on the champion. Lineage order,
/// scores and weights never change.
#[must_use]
pub fn ravo_observe_champion(
    state: &RavoState,
    champion_id: &str,
    recurred_fingerprints: &[String],
    turn: u64,
) -> (RavoState, bool) {
    let Some(index) = state
        .lineage
        .iter()
        .position(|champion| champion.proposal_id == champion_id)
    else {
        return (state.clone(), false);
    };
    let champion = &state.lineage[index];
    let Some(window) = &champion.provisional else {
        return (state.clone(), false);
    };
    if !is_safe(turn) || turn < window.committed_turn || turn > window.until_turn {
        return (state.clone(), false);
    }
    let claimed: HashSet<&str> = champion
        .claimed_fingerprints
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let fingerprints = sorted_unique(
        recurred_fingerprints
            .iter()
            .filter(|id| claimed.contains(id.as_str()))
            .cloned(),
    );
    if fingerprints.is_empty() {
        return (state.clone(), false);
    }
    let mut next = state.clone();
    if let Some(provisional) = next.lineage[index].provisional.as_mut() {
        provisional.observed_recurrence = Some(ObservedRecurrence { turn, fingerprints });
    }
    (next, true)
}

fn invalid_certificate(
    state: RavoState,
    proposal: &RavoProposal,
    evaluation: &RavoEvaluation,
    config: &RavoConfig,
    previous: &RavoState,
    challenger_fingerprints: &[String],
    rejection: RavoRejection,
) -> RavoStepResult {
    RavoStepResult {
        certificate: RavoGateCertificate {
            proposal_id: proposal.id.clone(),
            previous_champion_id: previous.champion_id.clone(),
            previous_best_score: best_score_for(&previous.lineage, challenger_fingerprints, config),
            screen_threshold: config.screen_threshold,
            epsilon: config.epsilon,
            deep_tolerance: config.deep_tolerance,
            screen: evaluation.screen.clone(),
            deep: evaluation.deep.clone(),
            criteria: Vec::new(),
            missed_criterion_ids: Vec::new(),
            missed_seed_weight: 0,
            missed_current_weight: 0,
            committed: false,
            rejection: Some(rejection),
        },
        state,
    }
}

/// Consume one proposal's evaluation exactly once. A screen failure never
/// reaches the deep gate; missing, abstaining and errored opponents are
/// conservative misses; a deep abstention or error rejects.
// The gate sequence of TS `ravoStep`, in order (screen, deep, opponents,
// commit): splitting it would hide the order the formalization pins.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn ravo_step(
    state: &RavoState,
    proposal: &RavoProposal,
    evaluation: &RavoEvaluation,
    config: &RavoConfig,
) -> RavoStepResult {
    let challenger_fingerprints = addressed_fingerprints_of_artifact(&proposal.artifact);
    if state.evaluated_proposal_ids.contains(&proposal.id) {
        return invalid_certificate(
            state.clone(),
            proposal,
            evaluation,
            config,
            state,
            &challenger_fingerprints,
            RavoRejection::AlreadyEvaluated,
        );
    }
    if proposal.id.is_empty()
        || evaluation.proposal_id != proposal.id
        || !ravo_w(state)
        || !is_safe(config.screen_threshold)
        || !is_safe(config.epsilon)
        || !is_safe(config.deep_tolerance)
    {
        return invalid_certificate(
            state.clone(),
            proposal,
            evaluation,
            config,
            state,
            &challenger_fingerprints,
            RavoRejection::InvalidInput,
        );
    }
    let mut evaluated_state = state.clone();
    evaluated_state
        .evaluated_proposal_ids
        .push(proposal.id.clone());
    let screen_pass = evaluation.screen.status == GateStatus::Pass
        && evaluation
            .screen
            .score
            .is_some_and(|score| is_safe(score) && score >= config.screen_threshold);
    if !screen_pass {
        return invalid_certificate(
            evaluated_state,
            proposal,
            evaluation,
            config,
            state,
            &challenger_fingerprints,
            RavoRejection::Screen,
        );
    }
    let previous_best_score = best_score_for(&state.lineage, &challenger_fingerprints, config);
    let slacked = evaluation
        .deep
        .score
        .filter(|score| is_safe(*score))
        .and_then(|score| checked_add(score, config.deep_tolerance));
    let deep_pass = evaluation.deep.status == GateStatus::Pass
        && slacked.is_some_and(|score| score >= previous_best_score);
    if !deep_pass {
        return invalid_certificate(
            evaluated_state,
            proposal,
            evaluation,
            config,
            state,
            &challenger_fingerprints,
            RavoRejection::Deep,
        );
    }
    let mut observations: Vec<&RavoCriterionObservation> = Vec::new();
    let mut duplicate = false;
    for observation in &evaluation.criteria {
        if let Some(slot) = observations
            .iter_mut()
            .find(|seen| seen.criterion_id == observation.criterion_id)
        {
            duplicate = true;
            *slot = observation;
        } else {
            observations.push(observation);
        }
    }
    let unknown = evaluation.criteria.iter().any(|observation| {
        !state
            .opponents
            .criteria
            .iter()
            .any(|criterion| criterion.id == observation.criterion_id)
    });
    if duplicate || unknown {
        return invalid_certificate(
            evaluated_state,
            proposal,
            evaluation,
            config,
            state,
            &challenger_fingerprints,
            RavoRejection::InvalidInput,
        );
    }
    let mut missed_seed_weight = 0u64;
    let mut missed_current_weight = 0u64;
    let mut overflow = false;
    let mut sorted = state.opponents.criteria.clone();
    sorted.sort_by(|left, right| locale_compare(&left.id, &right.id));
    let criteria: Vec<RavoCriterionCertificate> = sorted
        .iter()
        .map(|criterion| {
            let observation = observations
                .iter()
                .find(|observation| observation.criterion_id == criterion.id);
            let status = observation.map_or(GateStatus::Abstain, |observation| observation.status);
            let counted_as_missed = status != GateStatus::Pass;
            if counted_as_missed {
                match (
                    checked_add(missed_seed_weight, criterion.seed_weight),
                    checked_add(missed_current_weight, criterion.current_weight),
                ) {
                    (Some(seed), Some(current)) => {
                        missed_seed_weight = seed;
                        missed_current_weight = current;
                    }
                    _ => overflow = true,
                }
            }
            RavoCriterionCertificate {
                criterion_id: criterion.id.clone(),
                status,
                seed_weight: criterion.seed_weight,
                current_weight: criterion.current_weight,
                counted_as_missed,
                detail: observation.and_then(|observation| observation.detail.clone()),
            }
        })
        .collect();
    if overflow {
        return invalid_certificate(
            evaluated_state,
            proposal,
            evaluation,
            config,
            state,
            &challenger_fingerprints,
            RavoRejection::InvalidInput,
        );
    }
    let missed_criterion_ids = sorted_unique(
        criteria
            .iter()
            .filter(|item| item.counted_as_missed)
            .map(|item| item.criterion_id.clone()),
    );
    let committed = ravo_gatekeeper(missed_current_weight, config.epsilon);
    let certificate = RavoGateCertificate {
        proposal_id: proposal.id.clone(),
        previous_champion_id: state.champion_id.clone(),
        previous_best_score,
        screen_threshold: config.screen_threshold,
        epsilon: config.epsilon,
        deep_tolerance: config.deep_tolerance,
        screen: evaluation.screen.clone(),
        deep: evaluation.deep.clone(),
        criteria,
        missed_criterion_ids: missed_criterion_ids.clone(),
        missed_seed_weight,
        missed_current_weight,
        committed,
        rejection: (!committed).then_some(RavoRejection::Opponents),
    };
    if !committed {
        return RavoStepResult {
            state: evaluated_state,
            certificate,
        };
    }
    let champion = RavoChampion {
        proposal_id: proposal.id.clone(),
        parent_id: state.champion_id.clone(),
        score: evaluation.deep.score.unwrap_or(0),
        artifact: proposal.artifact.clone(),
        missed_criterion_ids: missed_criterion_ids.clone(),
        claimed_fingerprints: None,
        provisional: None,
        extra: Map::new(),
    };
    let mut lineage = state.lineage.clone();
    lineage.push(champion);
    RavoStepResult {
        state: RavoState {
            lineage,
            champion_id: Some(proposal.id.clone()),
            opponents: ravo_pressure(&state.opponents, &missed_criterion_ids),
            evaluated_proposal_ids: evaluated_state.evaluated_proposal_ids,
            // A commit builds a fresh state object in TS: keys it does not
            // know are not carried into it.
            extra: Map::new(),
        },
        certificate,
    }
}
