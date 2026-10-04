//! The RAVO gate on `/refine` (TS `refinement/ravo.ts`
//! `ravoEvaluateProposal`, and the gate half of `agent-session.ts`
//! `_planRefineInSpan` / `_applyRefineInSpan`): a fast structural screen,
//! one deep-judge model call, the referee on the claims the judge credits,
//! and the assisted authority's decision, bound to the proposal and the
//! baseline it was judged against.

use pa_core::refinement::executor::RefinerFn;
use pa_core::refinement::planner::{count_valid_refinement_edits, RefinementProposal};
use pa_core::refinement::{HarnessScope, HarnessState};
use pa_ledger::{failure_opponent_id, format_failure_ledger_for_prompt, FailureRecord};
use pa_types::ai::{AssistantContentBlock, AssistantMessage, StopReason};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::authority::{
    authorize_assisted_ravo, empty_assisted_ravo_state, failure_opponent_fingerprint,
    is_failure_opponent_id, normalize_assisted_ravo_state, AssistedRavoAuthorization,
    AssistedRavoObservation, AuthorityInput, UnclaimedCommitPolicy,
    DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
};
use crate::js::{js_round, js_tail};
use crate::reducer::{
    ravo_best_score, ravo_extend_opponents, GateStatus, RavoConfig, RavoRejection, RavoState,
    RavoWindowClock,
};
use crate::referee::{
    adjudicate_failure_claims, is_referee_opponent_id, referee_opponent_id,
    referee_verdict_is_evidence, skill_imports_of, RefereeVerdict, RefereeVerdictStatus,
    ReplayRunner,
};
use crate::trust::TRUST_KEY;

/// The harness state key RAVO owns.
pub const RAVO_KEY: &str = "ravo";

/// The gate thresholds every `/refine` meets (TS `RAVO_DEFAULT_CONFIG`).
pub const RAVO_DEFAULT_CONFIG: RavoConfig = RavoConfig {
    screen_threshold: 50,
    epsilon: 1,
    deep_tolerance: 10,
};

/// The rationale of an approval the harness moved out from under.
pub const RAVO_BASELINE_CHANGED_RATIONALE: &str =
    "RAVO authorization no longer matches the complete proposal and current harness baseline; retry /refine";

/// The seed criteria and what the judge is told each one means.
pub const RAVO_SEED_CRITERIA: [(&str, &str); 5] = [
    (
        "evidence",
        "Every edit is backed by concrete trajectory evidence.",
    ),
    (
        "scope",
        "Edits match the requested scope (local vs global) policy.",
    ),
    (
        "minimality",
        "Edits touch the smallest relevant components; no sprawling rewrites.",
    ),
    (
        "contracts",
        "Skill edits carry a valid python reference and arguments contract.",
    ),
    (
        "novelty",
        "Edits do not duplicate or overlap existing harness entries.",
    ),
];

/// The judge's system prompt (byte-identical to the TS product's).
pub const RAVO_JUDGE_SYSTEM_PROMPT: &str = r#"You are the RAVO deep evaluator for Prime Agent's /refine subsystem.

Score a proposed continual-harness refinement against the trajectory evidence.
Judge the QUALITY OF THE RESULTING HARNESS STATE, not prose style.

When <recurring_failures> is present, each listed failure is an opponent
criterion (id "failure:<fingerprint>"). The proposal ADDRESSES a fingerprint
only if its edits would plausibly prevent that exact failure from recurring
(a memory, prompt note, skill fix, or subagent change that targets its cause).
List the fingerprint ids the proposal genuinely addresses in
"addressedFingerprints"; a fingerprint not listed there counts as a missed
opponent. Never list a fingerprint the proposal merely mentions. A fingerprint
marked replay=verified is re-executed after you answer when a skill the proposal
writes imports the module its replay case probes, so listing one whose failure
has not actually stopped costs the proposal the gate. Do not list a fingerprint
whose failure is outside the harness's control (a provider outage, a user
denial, a flaky network).

"verdict" is your own decision on the deep gate: "pass" if this candidate is at
least as good a harness state as the current champion, "fail" if it is worse,
"abstain" if you cannot tell from the evidence given. It is not a summary of
"score"; a non-pass verdict rejects the candidate regardless of the number.

Return JSON only:
{
  "verdict": "pass" | "fail" | "abstain",
  "score": 0-100,
  "failedCriteria": ["criterion ids that the proposal fails"],
  "addressedFingerprints": ["recurring failure fingerprint ids the proposal addresses"],
  "rationale": "one or two sentences"
}"#;

/// The judge's output budget.
pub const RAVO_JUDGE_MAX_OUTPUT_TOKENS: u64 = 2_048;

/// The conversation the judge reads, in UTF-16 units from its end.
pub const RAVO_JUDGE_CONVERSATION_UNITS: usize = 40_000;

/// Why a refine ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefineReason {
    Manual,
    RefineRun,
    Recurrence,
    Regression,
    TurnInterval,
    Compact,
    Rollback,
    RavoRun,
}

/// What a refine is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineKind {
    /// Does what it was asked.
    Directed,
    /// Periodic housekeeping.
    Checkpoint,
    /// Exists to stop recorded failures, and must claim one.
    Failure,
}

impl RefineReason {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::RefineRun => "refine_run",
            Self::Recurrence => "recurrence",
            Self::Regression => "regression",
            Self::TurnInterval => "turn_interval",
            Self::Compact => "compact",
            Self::Rollback => "rollback",
            Self::RavoRun => "ravo_run",
        }
    }

    /// The kind of refine a reason makes.
    #[must_use]
    pub fn kind(self) -> RefineKind {
        match self {
            Self::TurnInterval | Self::Compact => RefineKind::Checkpoint,
            Self::Recurrence | Self::Regression => RefineKind::Failure,
            Self::Manual | Self::RefineRun | Self::Rollback | Self::RavoRun => RefineKind::Directed,
        }
    }
}

/// The gate's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RavoDecision {
    Commit,
    RejectScreen,
    RejectDeep,
    RejectCriteria,
    RejectUnclaimed,
}

impl RavoDecision {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::RejectScreen => "reject_screen",
            Self::RejectDeep => "reject_deep",
            Self::RejectCriteria => "reject_criteria",
            Self::RejectUnclaimed => "reject_unclaimed",
        }
    }
}

/// Referee verdicts by status. Field order is the TS object's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefereeCounts {
    pub cleared: u64,
    pub upheld: u64,
    pub unverifiable: u64,
    pub no_evidence: u64,
    pub not_applicable: u64,
}

impl RefereeCounts {
    fn count(&mut self, status: RefereeVerdictStatus) {
        let slot = match status {
            RefereeVerdictStatus::Cleared => &mut self.cleared,
            RefereeVerdictStatus::Upheld => &mut self.upheld,
            RefereeVerdictStatus::Unverifiable => &mut self.unverifiable,
            RefereeVerdictStatus::NoEvidence => &mut self.no_evidence,
            RefereeVerdictStatus::NotApplicable => &mut self.not_applicable,
        };
        *slot += 1;
    }
}

/// The gate report attached to a refinement (`RefinementResult.ravo`).
/// Field order is the TS object's; `rationale` is untrusted judge text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RavoGateReport {
    pub fast_score: u64,
    pub best_score: u64,
    pub epsilon: u64,
    pub screen_threshold: u64,
    pub deep_tolerance: u64,
    pub failure_opponents: Vec<String>,
    pub decision: RavoDecision,
    pub deep_score: u64,
    pub missed_criteria: Vec<String>,
    pub missed_weight: u64,
    pub addressed_fingerprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referee_verdicts: Option<Vec<RefereeVerdict>>,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<AssistedRavoAuthorization>,
    pub measurable: bool,
    pub referee_counts: RefereeCounts,
}

/// Why a proposal was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCause {
    Gate,
    Screen,
    JudgeUnavailable,
    BaselineChanged,
    StaleEvidence,
}

impl RejectionCause {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Screen => "screen",
            Self::JudgeUnavailable => "judge_unavailable",
            Self::BaselineChanged => "baseline_changed",
            Self::StaleEvidence => "stale_evidence",
        }
    }
}

/// Classify a rejection, first match wins (TS `refinementRejectionCause`).
#[must_use]
pub fn refinement_rejection_cause(report: &RavoGateReport, approval_lost: bool) -> RejectionCause {
    if report.judge_error.is_some() {
        return RejectionCause::JudgeUnavailable;
    }
    if approval_lost || report.rationale == RAVO_BASELINE_CHANGED_RATIONALE {
        return RejectionCause::BaselineChanged;
    }
    if report.decision == RavoDecision::RejectScreen {
        return RejectionCause::Screen;
    }
    RejectionCause::Gate
}

/// The fast screen: the share of well-formed edits in [0, 100]; an empty
/// proposal screens at 0.
#[must_use]
pub fn ravo_fast_screen(proposal: &RefinementProposal, valid_edits: usize) -> u64 {
    if proposal.edits.is_empty() {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)]
    let share = 100.0 * valid_edits as f64 / proposal.edits.len() as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let score = js_round(share) as u64;
    score
}

/// The proposal as the JSON artifact the certificate binds and the lineage
/// records (TS `RefinementProposal` key order; absent fields omitted).
#[must_use]
pub fn proposal_artifact(proposal: &RefinementProposal) -> Value {
    let edits: Vec<Value> = proposal
        .edits
        .iter()
        .map(|edit| {
            let mut map = Map::new();
            let mut put = |key: &str, value: Option<Value>| {
                if let Some(value) = value {
                    map.insert(key.to_string(), value);
                }
            };
            put(
                "action",
                edit.action
                    .and_then(|action| serde_json::to_value(action).ok()),
            );
            put(
                "kind",
                edit.kind.and_then(|kind| serde_json::to_value(kind).ok()),
            );
            put("id", edit.id.clone().map(Value::from));
            put("title", edit.title.clone().map(Value::from));
            put("content", edit.content.clone().map(Value::from));
            put("path", edit.path.clone().map(Value::from));
            put("reference", edit.reference.clone().map(Value::Object));
            put("arguments", edit.arguments.clone().map(Value::Object));
            put("metadata", edit.metadata.clone().map(Value::Object));
            put("reason", edit.reason.clone().map(Value::from));
            Value::Object(map)
        })
        .collect();
    json!({
        "summary": proposal.summary,
        "rationale": proposal.rationale,
        "expectedOutcome": proposal.expected_outcome,
        "edits": edits,
    })
}

/// The stored RAVO state of a harness state, normalized; `None` when the
/// key is absent.
#[must_use]
pub fn stored_ravo_state(state: &HarnessState) -> Option<RavoState> {
    state
        .extensions
        .get(RAVO_KEY)
        .map(|value| normalize_assisted_ravo_state(Some(value)))
}

/// `ravo` without the recurrences recorded on its champions (turn-boundary
/// bookkeeping, written at any time).
#[must_use]
pub fn without_observed_recurrences(ravo: &RavoState) -> RavoState {
    let mut stripped = ravo.clone();
    for champion in &mut stripped.lineage {
        if let Some(window) = champion.provisional.as_mut() {
            window.observed_recurrence = None;
        }
    }
    stripped
}

/// The slice of a harness state a certificate binds: the schema, the
/// entries and the RAVO state without recorded recurrences, never the
/// failure ledger or the refinement log (TS `refinementBaselineView`).
#[must_use]
pub fn refinement_baseline_view(state: &HarnessState) -> Value {
    let mut view = Map::new();
    view.insert("schema".to_string(), Value::from(state.schema));
    // Trust is settled at turn boundaries and by other processes, like the
    // failure ledger: binding it would reject an in-flight refine for a
    // reason that has nothing to do with the proposal.
    let mut entries = serde_json::to_value(&state.entries).unwrap_or(Value::Null);
    if let Some(kinds) = entries.as_object_mut() {
        for records in kinds.values_mut().filter_map(Value::as_object_mut) {
            for entry in records.values_mut().filter_map(Value::as_object_mut) {
                entry.shift_remove(TRUST_KEY);
            }
        }
    }
    view.insert("entries".to_string(), entries);
    if let Some(ravo) = stored_ravo_state(state) {
        view.insert(
            RAVO_KEY.to_string(),
            serde_json::to_value(without_observed_recurrences(&ravo)).unwrap_or(Value::Null),
        );
    }
    Value::Object(view)
}

/// `next` with every recurrence `current` records on the same champion
/// (inside that champion's window in `next`).
#[must_use]
pub fn carry_observed_recurrences(next: &RavoState, current: Option<&RavoState>) -> RavoState {
    let mut carried = next.clone();
    let Some(current) = current else {
        return carried;
    };
    for champion in &mut carried.lineage {
        let recurrence = current
            .lineage
            .iter()
            .find(|stored| stored.proposal_id == champion.proposal_id)
            .and_then(|stored| stored.provisional.as_ref())
            .and_then(|window| window.observed_recurrence.clone());
        let (Some(recurrence), Some(window)) = (recurrence, champion.provisional.as_mut()) else {
            continue;
        };
        if recurrence.turn >= window.committed_turn && recurrence.turn <= window.until_turn {
            window.observed_recurrence = Some(recurrence);
        }
    }
    carried
}

/// Store `ravo` in a harness state where a TS save puts the key (before
/// `failures` and `trustWindows` when it is new).
pub fn set_stored_ravo_state(state: &mut HarnessState, ravo: &RavoState) {
    let value = serde_json::to_value(ravo).unwrap_or(Value::Null);
    if let Some(slot) = state.extensions.get_mut(RAVO_KEY) {
        *slot = value;
        return;
    }
    let before = state
        .extensions
        .keys()
        .position(|key| key == "failures" || key == "trustWindows");
    match before {
        Some(index) => {
            state
                .extensions
                .shift_insert(index, RAVO_KEY.to_string(), value);
        }
        None => {
            state.extensions.insert(RAVO_KEY.to_string(), value);
        }
    }
}

/// The judge's parsed reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeReply {
    pub verdict: GateStatus,
    pub score: u64,
    pub failed_criteria: Vec<String>,
    pub addressed_fingerprints: Vec<String>,
    pub rationale: String,
}

/// The judge's own deep-gate verdict: only an explicit pass token passes,
/// absence is not consent.
#[must_use]
pub fn parse_judge_verdict(value: Option<&Value>) -> GateStatus {
    let text = match value {
        Some(Value::String(text)) => text.trim().to_lowercase(),
        _ => String::new(),
    };
    match text.as_str() {
        "pass" | "accept" | "passed" | "true" => GateStatus::Pass,
        "fail" | "reject" | "false" => GateStatus::Fail,
        _ => GateStatus::Abstain,
    }
}

/// `Number(value)` for a JSON value; `None` for `NaN`.
fn js_to_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Null => Some(0.0),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        Value::String(text) => string_to_number(text),
        Value::Array(items) => match items.as_slice() {
            [] => Some(0.0),
            [single] => match single {
                Value::String(text) => string_to_number(text),
                Value::Number(number) => number.as_f64(),
                Value::Null => Some(0.0),
                _ => None,
            },
            _ => None,
        },
        Value::Object(_) => None,
    }
}

/// ECMAScript `StringToNumber` for decimal literals and `Infinity`.
fn string_to_number(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    let unsigned = trimmed.trim_start_matches(['+', '-']);
    if unsigned == "Infinity" {
        return Some(if trimmed.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let decimal = unsigned
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | 'e' | 'E' | '+' | '-'));
    if !decimal || trimmed.len() - unsigned.len() > 1 {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The JSON candidate of a judge reply: a fenced block when there is one,
/// then the outermost braces.
fn judge_candidate(text: &str) -> &str {
    let trimmed = text.trim();
    let fenced = trimmed.find("```").and_then(|open| {
        let after = &trimmed[open + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let body_start = after.len() - after.trim_start().len();
        let body = &after[body_start..];
        body.find("```").map(|close| body[..close].trim())
    });
    let candidate = fenced.unwrap_or(trimmed);
    match (candidate.find('{'), candidate.rfind('}')) {
        (Some(start), Some(end)) if end > start => &candidate[start..=end],
        _ => candidate,
    }
}

/// Parse the judge's JSON reply.
///
/// # Errors
///
/// The JSON parse error when the candidate is not JSON.
pub fn extract_judge_json(text: &str) -> Result<JudgeReply, serde_json::Error> {
    let parsed: Value = serde_json::from_str(judge_candidate(text))?;
    let empty = Map::new();
    let record = parsed.as_object().unwrap_or(&empty);
    let score = js_to_number(record.get("score"))
        .filter(|score| score.is_finite())
        .map_or(0, |score| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let clamped = js_round(score).clamp(0.0, 100.0) as u64;
            clamped
        });
    let verdict_value = match record.get("verdict") {
        None | Some(Value::Null) => record.get("status"),
        some => some,
    };
    Ok(JudgeReply {
        verdict: parse_judge_verdict(verdict_value),
        score,
        failed_criteria: string_list(record.get("failedCriteria")),
        addressed_fingerprints: string_list(record.get("addressedFingerprints")),
        rationale: record
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn reply_text(reply: &AssistantMessage) -> String {
    reply
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything one gate evaluation reads.
pub struct GateEvaluation<'a> {
    pub proposal: &'a RefinementProposal,
    pub proposal_id: &'a str,
    pub state: &'a RavoState,
    pub config: RavoConfig,
    pub conversation_text: String,
    pub harness_overview: String,
    pub baseline: Value,
    pub recurring_failures: &'a [FailureRecord],
    pub turn: Option<u64>,
    pub turn_clock: Option<RavoWindowClock>,
    pub refine_kind: RefineKind,
    pub model: pa_types::ai::Model,
    pub runner: &'a dyn ReplayRunner,
    /// Extra `sys.path` roots the referee replays with.
    pub sys_path: &'a [String],
}

fn judge_prompt(evaluation: &GateEvaluation<'_>, failure_opponents: &[String]) -> String {
    let judged_pool = ravo_extend_opponents(&evaluation.state.opponents, failure_opponents);
    let failure_description = |id: &str| {
        evaluation
            .recurring_failures
            .iter()
            .find(|record| failure_opponent_id(&record.fingerprint.id) == id)
            .map(|record| {
                format!(
                    "Recurring {} ({}x): {}",
                    record.fingerprint.kind.as_str(),
                    record.count,
                    record.fingerprint.message
                )
            })
    };
    let criteria_text = judged_pool
        .criteria
        .iter()
        .filter(|criterion| !is_referee_opponent_id(&criterion.id))
        .filter(|criterion| {
            !is_failure_opponent_id(&criterion.id) || failure_description(&criterion.id).is_some()
        })
        .map(|criterion| {
            let description = RAVO_SEED_CRITERIA
                .iter()
                .find(|(id, _)| *id == criterion.id)
                .map(|(_, description)| (*description).to_string())
                .or_else(|| failure_description(&criterion.id))
                .unwrap_or_else(|| criterion.id.clone());
            format!(
                "- {} (weight {}): {description}",
                criterion.id, criterion.current_weight
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut sections = vec![format!("<criteria>\n{criteria_text}\n</criteria>")];
    if !evaluation.recurring_failures.is_empty() {
        sections.push(format!(
            "<recurring_failures>\n{}\n</recurring_failures>",
            format_failure_ledger_for_prompt(
                evaluation.recurring_failures,
                pa_ledger::DEFAULT_PROMPT_LIMIT
            )
        ));
        let candidates = evaluation
            .recurring_failures
            .iter()
            .map(|record| record.fingerprint.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!(
            "The proposal must address these recurring failures. Return the fingerprint ids it addresses in \"addressedFingerprints\" (candidates: {candidates})."
        ));
    }
    sections.push(format!(
        "<current_harness_state>\n{}\n</current_harness_state>",
        evaluation.harness_overview
    ));
    sections.push(format!(
        "<proposal>\n{}\n</proposal>",
        serde_json::to_string_pretty(&proposal_artifact(evaluation.proposal)).unwrap_or_default()
    ));
    sections.push(format!(
        "<conversation>\n{}\n</conversation>",
        evaluation.conversation_text
    ));
    sections.push(
        "Score the proposal and list any failed criterion ids. Return JSON only.".to_string(),
    );
    sections.join("\n\n")
}

async fn call_judge(
    model: &pa_types::ai::Model,
    model_call: RefinerFn,
    prompt: String,
) -> anyhow::Result<JudgeReply> {
    let mut model = model.clone();
    model.max_tokens = model.max_tokens.min(RAVO_JUDGE_MAX_OUTPUT_TOKENS);
    let reply = model_call(model, RAVO_JUDGE_SYSTEM_PROMPT, prompt).await?;
    if reply.stop_reason == StopReason::Error {
        anyhow::bail!(
            "{}",
            reply
                .error_message
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| "judge call failed".to_string())
        );
    }
    Ok(extract_judge_json(&reply_text(&reply))?)
}

/// The criteria a judged proposal missed and their current weight: the
/// certificate's missed set, or, when the step never reached the opponents,
/// the judge's failed criteria plus the unaddressed failure opponents.
fn missed_criteria(
    state: &RavoState,
    failure_opponents: &[String],
    addressed: &[String],
    judge: Option<&JudgeReply>,
    authorization: &AssistedRavoAuthorization,
    referee_verdicts: &[RefereeVerdict],
) -> (Vec<String>, u64) {
    let unaddressed: Vec<String> = failure_opponents
        .iter()
        .filter(|id| {
            failure_opponent_fingerprint(id)
                .is_some_and(|fingerprint| !addressed.iter().any(|claimed| claimed == fingerprint))
        })
        .cloned()
        .collect();
    let missed: Vec<String> = if authorization.certificate.certificate.criteria.is_empty() {
        let mut missed: Vec<String> = Vec::new();
        let judged_failed = judge
            .map(|reply| reply.failed_criteria.clone())
            .unwrap_or_default();
        for id in judged_failed.into_iter().chain(unaddressed) {
            if !missed.contains(&id) {
                missed.push(id);
            }
        }
        missed
    } else {
        authorization
            .certificate
            .certificate
            .missed_criterion_ids
            .clone()
    };
    let mut pool_ids = failure_opponents.to_vec();
    pool_ids.extend(
        referee_verdicts
            .iter()
            .filter(|verdict| referee_verdict_is_evidence(Some(verdict)))
            .map(|verdict| referee_opponent_id(&verdict.fingerprint_id)),
    );
    let pool = ravo_extend_opponents(&state.opponents, &pool_ids);
    let missed_weight = pool
        .criteria
        .iter()
        .filter(|criterion| missed.contains(&criterion.id))
        .map(|criterion| criterion.current_weight)
        .sum();
    (missed, missed_weight)
}

/// Evaluate one proposal (TS `ravoEvaluateProposal`), consulting the judge
/// through `model_call`. Judge errors fail closed: an unevaluated proposal
/// is never authorized.
// TS `ravoEvaluateProposal` in its order: screen, judge, referee,
// authority, decision.
#[allow(clippy::too_many_lines)]
pub async fn ravo_evaluate_proposal(
    evaluation: GateEvaluation<'_>,
    model_call: RefinerFn,
) -> RavoGateReport {
    let config = evaluation.config;
    let mut failure_opponents: Vec<String> = Vec::new();
    for record in evaluation.recurring_failures {
        let id = failure_opponent_id(&record.fingerprint.id);
        if !failure_opponents.contains(&id) {
            failure_opponents.push(id);
        }
    }
    let fast_score = ravo_fast_screen(
        evaluation.proposal,
        count_valid_refinement_edits(evaluation.proposal),
    );
    let best_score = ravo_best_score(&evaluation.state.lineage);
    let artifact = proposal_artifact(evaluation.proposal);
    let unclaimed_commit = if evaluation.refine_kind == RefineKind::Failure {
        UnclaimedCommitPolicy::Reject
    } else {
        UnclaimedCommitPolicy::Unmeasured
    };
    let authority = |observation: AssistedRavoObservation, verdicts: &[RefereeVerdict]| {
        authorize_assisted_ravo(&AuthorityInput {
            proposal_id: evaluation.proposal_id,
            artifact: &artifact,
            baseline: &evaluation.baseline,
            fast_score,
            observation,
            state: Some(evaluation.state),
            config,
            failure_opponents: &failure_opponents,
            referee_verdicts: verdicts,
            turn: evaluation.turn,
            turn_clock: evaluation.turn_clock,
            observation_window_turns: DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
            unclaimed_commit,
        })
    };
    if fast_score < config.screen_threshold {
        let rationale = format!(
            "structural screen scored {fast_score} below threshold {}",
            config.screen_threshold
        );
        let authorization = authority(
            AssistedRavoObservation {
                status: GateStatus::Abstain,
                score: None,
                detail: Some(rationale.clone()),
                failed_criteria: None,
                addressed_fingerprints: Vec::new(),
            },
            &[],
        );
        return RavoGateReport {
            fast_score,
            best_score,
            epsilon: config.epsilon,
            screen_threshold: config.screen_threshold,
            deep_tolerance: config.deep_tolerance,
            failure_opponents,
            decision: RavoDecision::RejectScreen,
            deep_score: 0,
            missed_criteria: Vec::new(),
            missed_weight: 0,
            addressed_fingerprints: Vec::new(),
            referee_verdicts: None,
            rationale,
            judge_error: None,
            authorization: Some(authorization),
            measurable: false,
            referee_counts: RefereeCounts::default(),
        };
    }
    let prompt = judge_prompt(&evaluation, &failure_opponents);
    let judged = call_judge(&evaluation.model, model_call, prompt).await;
    let (judge, judge_error) = match judged {
        Ok(reply) => (Some(reply), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let known: Vec<&str> = evaluation
        .recurring_failures
        .iter()
        .map(|record| record.fingerprint.id.as_str())
        .collect();
    let addressed: Vec<String> = judge
        .as_ref()
        .map(|reply| {
            reply
                .addressed_fingerprints
                .iter()
                .filter(|id| known.contains(&id.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let rationale = match (&judge, &judge_error) {
        (Some(reply), _) => reply.rationale.clone(),
        (None, Some(error)) => format!(
            "deep judge unavailable ({error}); no harness edits were authorized; retry /refine when evaluation is available"
        ),
        (None, None) => String::new(),
    };
    let referee_verdicts = if judge_error.is_some() {
        Vec::new()
    } else {
        adjudicate_failure_claims(
            evaluation.recurring_failures,
            &addressed,
            &skill_imports_of(&evaluation.proposal.edits),
            evaluation.sys_path,
            evaluation.runner,
        )
        .await
    };
    let observation = match &judge {
        Some(reply) => AssistedRavoObservation {
            status: reply.verdict,
            score: Some(reply.score),
            detail: Some(rationale.clone()),
            failed_criteria: Some(reply.failed_criteria.clone()),
            addressed_fingerprints: addressed.clone(),
        },
        None => AssistedRavoObservation {
            status: GateStatus::Error,
            score: None,
            detail: Some(rationale.clone()),
            failed_criteria: None,
            addressed_fingerprints: Vec::new(),
        },
    };
    let authorization = authority(observation, &referee_verdicts);
    let unclaimed = evaluation.refine_kind == RefineKind::Failure
        && judge_error.is_none()
        && addressed.is_empty();
    let decision = if unclaimed {
        RavoDecision::RejectUnclaimed
    } else if authorization.authorized {
        RavoDecision::Commit
    } else {
        match authorization.certificate.certificate.rejection {
            Some(RavoRejection::Screen) => RavoDecision::RejectScreen,
            Some(RavoRejection::Opponents) => RavoDecision::RejectCriteria,
            _ => RavoDecision::RejectDeep,
        }
    };
    let (missed, missed_weight) = missed_criteria(
        evaluation.state,
        &failure_opponents,
        &addressed,
        judge.as_ref(),
        &authorization,
        &referee_verdicts,
    );
    let mut referee_counts = RefereeCounts::default();
    for verdict in &referee_verdicts {
        referee_counts.count(verdict.status);
    }
    RavoGateReport {
        fast_score,
        best_score,
        epsilon: config.epsilon,
        screen_threshold: config.screen_threshold,
        deep_tolerance: config.deep_tolerance,
        failure_opponents,
        decision,
        deep_score: judge.as_ref().map_or(best_score, |reply| reply.score),
        missed_criteria: missed,
        missed_weight,
        measurable: decision == RavoDecision::Commit && !addressed.is_empty(),
        addressed_fingerprints: addressed,
        referee_verdicts: Some(referee_verdicts),
        rationale,
        judge_error,
        authorization: Some(authorization),
        referee_counts,
    }
}

/// The state a gate starts from when the store holds none.
#[must_use]
pub fn gate_start_state(stored: Option<&RavoState>) -> RavoState {
    stored.cloned().unwrap_or_else(empty_assisted_ravo_state)
}

/// The scope label a log line or event carries.
#[must_use]
pub fn scope_name(scope: HarnessScope) -> &'static str {
    match scope {
        HarnessScope::Local => "local",
        HarnessScope::Global => "global",
    }
}

/// The text the judge reads: the serialized conversation's tail.
#[must_use]
pub fn judge_conversation_text(messages: &[pa_types::session::AgentMessage]) -> String {
    let serialized = pa_core::session_engine::compaction_utils::serialize_conversation(messages);
    js_tail(&serialized, RAVO_JUDGE_CONVERSATION_UNITS).to_string()
}

#[cfg(test)]
mod tests {
    use pa_core::refinement::{empty_harness_state, HarnessEntry, RefinementKind};
    use serde_json::json;

    use super::*;

    fn with_memory(entry: Value) -> HarnessState {
        let mut state = empty_harness_state();
        let entry: HarnessEntry = serde_json::from_value(entry).unwrap();
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(entry.id.clone(), entry);
        state
    }

    /// TS `refinementBaselineView` binds the entries without `trust`
    /// (settled at turn boundaries and by other processes, like the
    /// failure ledger) and with every other key.
    #[test]
    fn the_baseline_view_binds_entries_without_their_trust() {
        let entry = json!({
            "id": "m1", "kind": "memory", "title": "M", "content": "c", "path": "general",
            "scope": "local", "reference": {}, "arguments": {}, "metadata": {},
            "source": "refine", "created_at": "t0", "updated_at": "t1", "version": 1,
            "other": 1
        });
        let mut trusted = entry.clone();
        trusted["trust"] = json!({"score": 20, "updated_at": "t2", "events": []});
        assert_eq!(
            refinement_baseline_view(&with_memory(trusted)),
            refinement_baseline_view(&with_memory(entry.clone()))
        );
        assert_eq!(
            refinement_baseline_view(&with_memory(entry))["entries"]["memory"]["m1"]["other"],
            json!(1)
        );
    }
}
