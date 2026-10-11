//! The assisted authority (TS `ravo/authority.ts`): one complete edit set
//! is decided by the reducer alone, against the five assisted criteria,
//! the recurring failures (`failure:<fp>`) and the referee's adjudicated
//! claims (`referee:<fp>`), and the certificate is bound to digests of the
//! proposal and of the baseline it was judged against.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::js::{canonical_json, sha256_hex};
use crate::reducer::{
    GateStatus,
    RavoChampion,
    RavoConfig,
    RavoCriterion,
    RavoCriterionObservation,
    RavoEvaluation,
    RavoGateCertificate,
    RavoObservation,
    RavoOpponentPool,
    RavoProposal,
    RavoRejection,
    RavoState,
    RavoWindowClock,
    WindowSpan,
    empty_ravo_state,
    ravo_extend_opponents,
    ravo_mark_provisional,
    ravo_step,
    ravo_w,
};
use crate::referee::{
    RefereeVerdict,
    RefereeVerdictStatus,
    failure_opponent_passed,
    is_referee_opponent_id,
    referee_detail,
    referee_opponent_fingerprint,
    referee_opponent_id,
    referee_opponent_passed,
    referee_verdict_is_evidence,
};

/// The criteria every `/refine` is judged on.
pub const ASSISTED_RAVO_CRITERIA: [&str; 5] =
    ["evidence", "scope", "minimality", "contracts", "novelty"];

/// Observations a provisional champion stays provisional for.
pub const DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS: u64 = 20;

/// What a commit that claims no failure fingerprint becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnclaimedCommitPolicy {
    /// The step stands: lineage, champion and pressure advance.
    Measured,
    /// The edits are authorized, the state is left as it was.
    Unmeasured,
    /// The commit is refused as `unclaimed`.
    Reject,
}

/// Whether a criterion id is a failure opponent (`failure:<fp>`).
#[must_use]
pub fn is_failure_opponent_id(criterion_id: &str) -> bool {
    criterion_id.len() > pa_ledger::FAILURE_OPPONENT_PREFIX.len()
        && criterion_id.starts_with(pa_ledger::FAILURE_OPPONENT_PREFIX)
}

/// The fingerprint a failure opponent names.
#[must_use]
pub fn failure_opponent_fingerprint(criterion_id: &str) -> Option<&str> {
    is_failure_opponent_id(criterion_id)
        .then(|| &criterion_id[pa_ledger::FAILURE_OPPONENT_PREFIX.len()..])
}

/// The certificate with its binding. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistedRavoCertificate {
    #[serde(flatten)]
    pub certificate: RavoGateCertificate,
    pub proposal_digest: String,
    pub baseline_digest: String,
}

/// The judge's observation of a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistedRavoObservation {
    pub status: GateStatus,
    pub score: Option<u64>,
    pub detail: Option<String>,
    /// `None`: every assisted criterion failed (the TS default).
    pub failed_criteria: Option<Vec<String>>,
    pub addressed_fingerprints: Vec<String>,
}

/// One authorization. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistedRavoAuthorization {
    pub authorized: bool,
    pub certificate: AssistedRavoCertificate,
    pub proposal_digest: String,
    pub baseline_digest: String,
    pub next_state: RavoState,
}

/// The inputs of [`authorize_assisted_ravo`].
#[derive(Debug, Clone)]
pub struct AuthorityInput<'a> {
    pub proposal_id: &'a str,
    pub artifact: &'a Value,
    /// The state slice the certificate binds (see `baseline_view`).
    pub baseline: &'a Value,
    pub fast_score: u64,
    pub observation: AssistedRavoObservation,
    pub state: Option<&'a RavoState>,
    pub config: RavoConfig,
    /// `failure:<fp>` ids of the currently recurring failures.
    pub failure_opponents: &'a [String],
    pub referee_verdicts: &'a [RefereeVerdict],
    /// The commit's position on `turn_clock`; enables the window.
    pub turn: Option<u64>,
    pub turn_clock: Option<RavoWindowClock>,
    pub observation_window_turns: u64,
    pub unclaimed_commit: UnclaimedCommitPolicy,
}

/// The empty assisted state: the five criteria at weight 1.
#[must_use]
pub fn empty_assisted_ravo_state() -> RavoState {
    empty_ravo_state(RavoOpponentPool {
        criteria: ASSISTED_RAVO_CRITERIA
            .iter()
            .map(|id| RavoCriterion::seeded(id))
            .collect(),
    })
}

/// Drop a window clock this build does not know, so one unknown field does
/// not discard the whole state.
fn with_known_window_clocks(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(lineage) = value.get_mut("lineage").and_then(Value::as_array_mut) {
        for champion in lineage {
            let Some(window) = champion
                .get_mut("provisional")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let known = match window.get("clock") {
                None => true,
                Some(Value::String(clock)) => clock == "ordinal" || clock == "local-ordinal",
                Some(_) => false,
            };
            if !known {
                window.shift_remove("clock");
            }
        }
    }
    value
}

/// `Number.isSafeInteger(value) && value > 0`.
fn positive_safe(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    #[allow(clippy::cast_precision_loss)]
    let max = crate::reducer::MAX_SAFE_INTEGER as f64;
    (number.fract() == 0.0 && number > 0.0 && number <= max).then(|| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = number as u64;
        whole
    })
}

/// `Number.isSafeInteger(value) && value >= 0`.
fn natural_safe(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) if number.as_f64() == Some(0.0) => Some(0),
        other => positive_safe(other),
    }
}

/// The stored `ravo` value as a valid state: as stored when it is one, a
/// legacy (`{lineage: [{id, score, missedCriteria}], evaluator}`) state
/// migrated, and otherwise empty.
#[must_use]
pub fn normalize_assisted_ravo_state(value: Option<&Value>) -> RavoState {
    let Some(Value::Object(record)) = value else {
        return empty_assisted_ravo_state();
    };
    if record.get("lineage").is_some_and(Value::is_array)
        && record
            .get("evaluatedProposalIds")
            .is_some_and(Value::is_array)
        && record
            .get("opponents")
            .is_some_and(|value| !is_falsy(value))
    {
        if let Ok(state) = serde_json::from_value::<RavoState>(with_known_window_clocks(
            &Value::Object(record.clone()),
        )) {
            if ravo_w(&state) {
                return state;
            }
        }
    }
    let legacy_criteria = record
        .get("evaluator")
        .and_then(|evaluator| evaluator.get("criteria"))
        .and_then(Value::as_array);
    let (Some(lineage), Some(legacy_criteria)) = (
        record.get("lineage").and_then(Value::as_array),
        legacy_criteria,
    ) else {
        return empty_assisted_ravo_state();
    };
    let mut legacy_weights: Map<String, Value> = Map::new();
    for item in legacy_criteria {
        let (Some(id), Some(weight)) = (
            item.get("id").and_then(Value::as_str),
            positive_safe(item.get("weight")),
        ) else {
            continue;
        };
        legacy_weights.insert(id.to_string(), Value::from(weight));
    }
    let mut migrated: Vec<RavoChampion> = Vec::new();
    let mut parent_id: Option<String> = None;
    for item in lineage {
        let (Some(id), Some(score)) = (
            item.get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty()),
            natural_safe(item.get("score")),
        ) else {
            continue;
        };
        let missed = item
            .get("missedCriteria")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        migrated.push(RavoChampion {
            proposal_id: id.to_string(),
            parent_id: parent_id.clone(),
            score,
            artifact: Value::Null,
            missed_criterion_ids: missed,
            claimed_fingerprints: None,
            provisional: None,
            extra: Map::new(),
        });
        parent_id = Some(id.to_string());
    }
    RavoState {
        evaluated_proposal_ids: migrated
            .iter()
            .map(|champion| champion.proposal_id.clone())
            .collect(),
        lineage: migrated,
        champion_id: parent_id,
        opponents: RavoOpponentPool {
            criteria: ASSISTED_RAVO_CRITERIA
                .iter()
                .map(|id| {
                    let weight = legacy_weights.get(*id).and_then(Value::as_u64).unwrap_or(1);
                    RavoCriterion {
                        id: (*id).to_string(),
                        seed_weight: 1,
                        current_weight: weight.max(1),
                    }
                })
                .collect(),
        },
        extra: Map::new(),
    }
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(_) | Value::Object(_) => false,
    }
}

/// `sha256(canonicalJson(value))` (the TS `ravoArtifactDigest`; a
/// `serde_json::Value` is already what `JSON.parse(JSON.stringify(...))`
/// yields).
#[must_use]
pub fn ravo_artifact_digest(value: &Value) -> String {
    sha256_hex(&canonical_json(value))
}

fn is_assisted_criterion(criterion_id: &str) -> bool {
    ASSISTED_RAVO_CRITERIA.contains(&criterion_id)
}

/// Authorize one complete assisted edit set. Recurring failures join the
/// pool as opponents before the step; one passes iff the judge credited it,
/// did not also fail it, and the referee does not charge it. Failure
/// opponents no longer recurring, and criteria `/refine` never observes,
/// are dormant passes. Each adjudicated claim also joins as
/// `referee:<fp>`. A commit that claims fingerprints opens a provisional
/// window at `turn`; one that claims nothing follows `unclaimed_commit`.
// TS `authorizeAssistedRavo` step by step: build the pool, step, then apply
// the unclaimed-commit policy.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn authorize_assisted_ravo(input: &AuthorityInput<'_>) -> AssistedRavoAuthorization {
    let proposal_digest = ravo_artifact_digest(input.artifact);
    let baseline_digest = ravo_artifact_digest(input.baseline);
    let addressed = &input.observation.addressed_fingerprints;
    let failure_opponents: Vec<String> = input
        .failure_opponents
        .iter()
        .filter(|id| is_failure_opponent_id(id))
        .cloned()
        .collect();
    let adjudicated: Vec<String> = failure_opponents
        .iter()
        .map(|id| failure_opponent_fingerprint(id).unwrap_or_default())
        .filter(|fingerprint| referee_verdict_is_evidence(verdict_for(input, fingerprint)))
        .map(referee_opponent_id)
        .collect();
    let base_state = input
        .state
        .cloned()
        .unwrap_or_else(empty_assisted_ravo_state);
    let mut extension = failure_opponents.clone();
    extension.extend(adjudicated);
    let state = RavoState {
        opponents: ravo_extend_opponents(&base_state.opponents, &extension),
        ..base_state.clone()
    };
    let detail = input.observation.detail.clone();
    let criteria = criterion_observations(input, &state, &failure_opponents);
    let stepped = ravo_step(
        &state,
        &RavoProposal {
            id: input.proposal_id.to_string(),
            artifact: input.artifact.clone(),
        },
        &RavoEvaluation {
            proposal_id: input.proposal_id.to_string(),
            screen: RavoObservation {
                status: if input.fast_score >= input.config.screen_threshold {
                    GateStatus::Pass
                } else {
                    GateStatus::Fail
                },
                score: Some(input.fast_score),
                detail: None,
            },
            deep: RavoObservation {
                status: input.observation.status,
                score: input.observation.score,
                detail,
            },
            criteria,
        },
        &input.config,
    );
    let mut next_state = stepped.state;
    let mut certificate = stepped.certificate;
    if certificate.committed {
        let claimed: Vec<String> = failure_opponents
            .iter()
            .map(|id| failure_opponent_fingerprint(id).unwrap_or_default())
            .filter(|fingerprint| addressed.iter().any(|id| id == fingerprint))
            .map(str::to_string)
            .collect();
        let policy = if claimed.is_empty() {
            input.unclaimed_commit
        } else {
            UnclaimedCommitPolicy::Measured
        };
        match policy {
            UnclaimedCommitPolicy::Reject => {
                certificate.committed = false;
                certificate.rejection = Some(RavoRejection::Unclaimed);
                let mut evaluated = state;
                evaluated
                    .evaluated_proposal_ids
                    .push(input.proposal_id.to_string());
                next_state = evaluated;
            }
            UnclaimedCommitPolicy::Unmeasured => next_state = base_state,
            UnclaimedCommitPolicy::Measured => {
                let window = input.turn.and_then(|turn| {
                    turn.checked_add(input.observation_window_turns)
                        .map(|until_turn| WindowSpan {
                            committed_turn: turn,
                            until_turn,
                            clock: input.turn_clock,
                        })
                });
                next_state =
                    ravo_mark_provisional(&next_state, input.proposal_id, &claimed, window);
            }
        }
    }
    AssistedRavoAuthorization {
        authorized: certificate.committed,
        certificate: AssistedRavoCertificate {
            certificate,
            proposal_digest: proposal_digest.clone(),
            baseline_digest: baseline_digest.clone(),
        },
        proposal_digest,
        baseline_digest,
        next_state,
    }
}

/// The judged criteria a failure opponent missed under `failedCriteria`.
fn failed_criteria(input: &AuthorityInput<'_>) -> Vec<String> {
    input
        .observation
        .failed_criteria
        .clone()
        .unwrap_or_else(|| {
            ASSISTED_RAVO_CRITERIA
                .iter()
                .map(|id| (*id).to_string())
                .collect()
        })
}

/// The referee's (last) verdict on `fingerprint`.
fn verdict_for<'a>(input: &AuthorityInput<'a>, fingerprint: &str) -> Option<&'a RefereeVerdict> {
    input
        .referee_verdicts
        .iter()
        .rev()
        .find(|verdict| verdict.fingerprint_id == fingerprint)
}

/// Every opponent's observation: the five assisted criteria as judged, the
/// failure opponents (dormant ones pass), the referee opponents, and the
/// criteria `/refine` never observes (dormant passes).
fn criterion_observations(
    input: &AuthorityInput<'_>,
    state: &RavoState,
    failure_opponents: &[String],
) -> Vec<RavoCriterionObservation> {
    let detail = input.observation.detail.clone();
    let addressed = &input.observation.addressed_fingerprints;
    let failed = failed_criteria(input);
    let verdict_of = |fingerprint: &str| verdict_for(input, fingerprint);
    let judged = |criterion_id: &str, passed: bool| RavoCriterionObservation {
        criterion_id: criterion_id.to_string(),
        status: if input.observation.status != GateStatus::Pass {
            input.observation.status
        } else if passed {
            GateStatus::Pass
        } else {
            GateStatus::Fail
        },
        detail: detail.clone(),
    };
    let mut criteria: Vec<RavoCriterionObservation> = ASSISTED_RAVO_CRITERIA
        .iter()
        .map(|id| judged(id, !failed.iter().any(|failed| failed == id)))
        .collect();
    for criterion in &state.opponents.criteria {
        let Some(fingerprint) = failure_opponent_fingerprint(&criterion.id) else {
            continue;
        };
        let dormant = !failure_opponents.contains(&criterion.id);
        if dormant {
            criteria.push(RavoCriterionObservation {
                detail: Some("dormant: fingerprint is not currently recurring".to_string()),
                ..judged(&criterion.id, true)
            });
            continue;
        }
        let claimed = addressed.iter().any(|id| id == fingerprint);
        let verdict = verdict_of(fingerprint);
        let passed = failure_opponent_passed(claimed, verdict)
            && !failed.iter().any(|failed| failed == &criterion.id);
        match verdict {
            Some(verdict) if verdict.status != RefereeVerdictStatus::NotApplicable => {
                criteria.push(RavoCriterionObservation {
                    detail: Some(verdict.detail.clone()),
                    ..judged(&criterion.id, passed)
                });
            }
            _ => criteria.push(judged(&criterion.id, passed)),
        }
    }
    for criterion in &state.opponents.criteria {
        let Some(fingerprint) = referee_opponent_fingerprint(&criterion.id) else {
            continue;
        };
        let claimed = addressed.iter().any(|id| id == fingerprint);
        let verdict = verdict_of(fingerprint);
        criteria.push(RavoCriterionObservation {
            detail: Some(referee_detail(verdict, claimed)),
            ..judged(&criterion.id, referee_opponent_passed(claimed, verdict))
        });
    }
    for criterion in &state.opponents.criteria {
        if is_assisted_criterion(&criterion.id)
            || is_failure_opponent_id(&criterion.id)
            || is_referee_opponent_id(&criterion.id)
        {
            continue;
        }
        criteria.push(RavoCriterionObservation {
            detail: Some("dormant: not a /refine criterion".to_string()),
            ..judged(&criterion.id, true)
        });
    }
    criteria
}

/// Whether an authorization is still bound to `artifact` and `baseline`.
#[must_use]
pub fn assisted_ravo_binding_matches(
    authorization: &AssistedRavoAuthorization,
    artifact: &Value,
    baseline: &Value,
) -> bool {
    authorization.proposal_digest == ravo_artifact_digest(artifact)
        && authorization.baseline_digest == ravo_artifact_digest(baseline)
}

/// Whether an authorization authorized and is still bound.
#[must_use]
pub fn assisted_ravo_certificate_matches(
    authorization: &AssistedRavoAuthorization,
    artifact: &Value,
    baseline: &Value,
) -> bool {
    authorization.authorized && assisted_ravo_binding_matches(authorization, artifact, baseline)
}
