//! The LLM proposer, the LLM dreamer and the semantic-guidance writer (TS
//! `llm.ts`): child agents that generate candidates, revise policies and
//! write advisory insights. Every child call spends tokens; nothing else here
//! does.
//!
//! Soundness rests on the existing trust boundaries, never on the child's
//! cooperation: a dreamed policy is DATA parsed by the strict
//! [`parse_exploration_policy`] and then scored and selected under the
//! no-regression rule (`improve::select_best_policy`) with an online probation;
//! a proposed artifact is validated by `task.deserialize` before it enters the
//! tree and re-scored by `task.evaluate`; guidance is advisory TEXT built from
//! recorded artifacts and scalar scores only (never hidden tests). No child
//! output is ever executed here.

use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::field::Empty;

use crate::child::{
    ChildOutcome,
    ChildRejection,
    ChildRuntimeScope,
    JsonContainer,
    PROPOSER_RETRIES,
    RunAgent,
    RunAgentStatus,
    ValidationError,
    retryable,
    run_structured_child,
};
use crate::collate::locale_compare;
use crate::dreams::DreamProbationRecord;
use crate::improve::{
    CandidateInput,
    CandidateOrigin,
    CandidateReason,
    CandidateVerdict,
    DreamerKind,
    DreamingScoreConfig,
    SpendCharge,
    dreamer_kind_of,
    propose_policies,
    terms_on_pool,
};
use crate::json::{self, js_number, to_fixed};
use crate::objective::{
    DEFAULT_OBJECTIVE,
    ObjectiveScale,
    ReplayObjectiveConfig,
    pool_score_scale,
};
use crate::policy::{
    ExplorationPolicy,
    POLICY_BOUNDS,
    RECOVERY_POLICIES,
    REPLAY_DEAD_FIELDS,
    SELECTION_RULES,
    STOP_RULES,
    parse_exploration_policy,
    policy_id,
};
use crate::proposer::{ProposalRejectReason, ProposalTally, ProposeOutcome, Proposer};
use crate::rejections::{RejectionInput, RejectionLog, RejectionRole};
use crate::replay::{ReplayConfig, simulate_policy};
use crate::rng::SeededRng;
use crate::store::{DreamStoreError, RecordedTree};
use crate::task::{Artifact, DynTask, ProposeParams};

const JSON_OBJECT_ONLY: &str =
    "Return exactly one JSON object and nothing else: no prose, no code fences.";
const JSON_ARRAY_ONLY: &str =
    "Return exactly one JSON array and nothing else: no prose, no code fences.";

/// The LAST line of every proposer prompt.
pub const PROPOSER_JSON_ONLY: &str =
    "Return ONLY the JSON object, no prose, no code fences, no text before or after it.";
const PROPOSER_OUTPUT_CONTRACT: &str = "Output contract:\n- Your entire reply is ONE JSON object: the improved candidate, in the exact shape the contract above describes (without a contract, the same keys as the current candidate; a derived field such as a score or peak may be omitted).\n- Do not think out loud, explain, or add any text before or after the object. Do not wrap it in markdown code fences.\n- Keep the reply as short as the object itself: a reply that runs past its output limit is discarded and replaced by a local mutation.";

/// The first characters of every child prompt, by role, so a runner holding
/// only the request can tell the roles apart.
pub const PROPOSER_PROMPT_HEADER: &str = "# Dream-RSI proposer";
pub const DREAMER_PROMPT_HEADER: &str = "# Dream-RSI policy dreamer";
pub const GUIDANCE_PROMPT_HEADER: &str = "# Dream-RSI guidance writer";

/// Top recorded candidates per tree in a guidance digest, and the per-artifact JSON cap.
pub const DEFAULT_GUIDANCE_TOP_K: usize = 3;
pub const DEFAULT_GUIDANCE_MAX_ARTIFACT_CHARS: usize = 2000;

const GUIDANCE_PROMPT_PREFIX: &str = "Directional insights from prior trajectories (advisory; the hidden tests, not these notes, decide the score):";

/// The child role a prompt belongs to, read from its first line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamChildRole {
    Proposer,
    Dreamer,
    Guidance,
}

impl DreamChildRole {
    /// The role of `prompt`, by its header; `None` for a foreign prompt.
    #[must_use]
    pub fn of_prompt(prompt: &str) -> Option<Self> {
        if prompt.starts_with(PROPOSER_PROMPT_HEADER) {
            Some(Self::Proposer)
        } else if prompt.starts_with(DREAMER_PROMPT_HEADER) {
            Some(Self::Dreamer)
        } else if prompt.starts_with(GUIDANCE_PROMPT_HEADER) {
            Some(Self::Guidance)
        } else {
            None
        }
    }
}

/// The error a cancelled run surfaces as (TS `DreamAbortError`).
#[must_use]
pub fn dream_abort(message: &str) -> DreamStoreError {
    DreamStoreError::Aborted(message.to_string())
}

fn rejection_input(
    role: Option<RejectionRole>,
    iteration: u32,
    round: u32,
    attempt: u32,
    rejection: &ChildRejection,
    fell_back: bool,
) -> RejectionInput {
    RejectionInput {
        role,
        iteration,
        round,
        attempt,
        reason: rejection.reason,
        status: rejection.child_status,
        fell_back,
        tokens: rejection.tokens,
        output_tokens: rejection.output_tokens,
        stop_reason: rejection.stop_reason.clone(),
        error: rejection.error.clone(),
        excerpt: rejection.excerpt.clone(),
    }
}

/// What the LLM proposer runs with.
pub struct LlmProposerOptions<'a> {
    pub scope: &'a ChildRuntimeScope,
    /// Cancels the child; a cancelled child aborts the rollout.
    pub cancel: &'a CancellationToken,
    /// Per-attempt token budget handed to the child call.
    pub token_budget: u64,
    /// Task-specific contract appended to the prompt (public; never hidden tests).
    pub prompt_context: Option<String>,
    /// Semantic guidance inserted after the header when non-empty.
    pub guidance: Option<String>,
    /// Where every rejected child result is written.
    pub rejections: Option<&'a RejectionLog<'a>>,
    /// The loop iteration, stamped on each rejection.
    pub iteration: u32,
}

/// A [`Proposer`] that generates each attempt through a child agent (TS
/// `createLlmProposer`). The output is searched for its JSON object, validated
/// by `task.deserialize`, and enters the tree as an `origin: llm` node; a
/// retryable rejection gets [`PROPOSER_RETRIES`] more calls; a final rejection
/// falls back to the task's local `propose` (`origin: local`, the child's
/// tokens kept); a cancel aborts the rollout. Every child result is tallied.
pub struct LlmProposer<'a> {
    runner: &'a dyn RunAgent,
    task: &'a dyn DynTask,
    options: LlmProposerOptions<'a>,
    /// Per-rollout provenance: every child result examined.
    pub tally: ProposalTally,
}

impl<'a> LlmProposer<'a> {
    #[must_use]
    pub fn new(
        runner: &'a dyn RunAgent,
        task: &'a dyn DynTask,
        options: LlmProposerOptions<'a>,
    ) -> Self {
        Self {
            runner,
            task,
            options,
            tally: ProposalTally::default(),
        }
    }

    fn reject(
        &mut self,
        round: u32,
        attempt: u32,
        rejection: &ChildRejection,
        fell_back: bool,
    ) -> Result<(), DreamStoreError> {
        self.tally.reject(rejection.reason, fell_back);
        if let Some(log) = self.options.rejections {
            log.append(&rejection_input(
                None,
                self.options.iteration,
                round,
                attempt,
                rejection,
                fell_back,
            ))?;
        }
        Ok(())
    }
}

impl Proposer for LlmProposer<'_> {
    fn propose(
        &mut self,
        parent: Option<&Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> Result<ProposeOutcome, DreamStoreError> {
        let span = tracing::info_span!(
            "dream.llm_propose",
            dream.round = round,
            dream.tokens = Empty,
            dream.llm_output_tokens = Empty,
            dream.llm_attempts = Empty,
            dream.llm_fallback = Empty,
            dream.origin = Empty,
            dream.llm_reject_reason = Empty,
            dream.llm_status = Empty,
            dream.llm_reject_excerpt = Empty,
        );
        let _entered = span.enter();
        let parent_json = parent.map(|artifact| self.task.serialize(artifact));
        let prompt = build_propose_prompt(
            self.task.id(),
            self.options.prompt_context.as_deref(),
            self.options.guidance.as_deref(),
            parent_json.as_ref(),
            params,
            round,
        );
        let request = self.options.scope.request(prompt);
        let run_options = self
            .options
            .scope
            .options(self.options.cancel, self.options.token_budget);
        let mut tokens = 0;
        let mut output_tokens = 0;
        let mut attempts = 0;
        let task = self.task;
        let last = loop {
            attempts += 1;
            let outcome = run_structured_child(
                self.runner,
                &request,
                &run_options,
                JsonContainer::Object,
                |value| {
                    task.deserialize(value)
                        .map_err(|error| ValidationError::Shape(error.0))
                },
            );
            tokens += outcome.tokens();
            output_tokens += outcome.output_tokens();
            let rejection = match outcome {
                ChildOutcome::Accepted { value, .. } => break Ok(value),
                ChildOutcome::Rejected(rejection) => rejection,
            };
            let retry = retryable(rejection.reason)
                && attempts <= PROPOSER_RETRIES
                && !self.options.cancel.is_cancelled();
            if !retry {
                break Err(rejection);
            }
            self.reject(round, attempts, &rejection, false)?;
        };
        span.record("dream.tokens", tokens);
        span.record("dream.llm_output_tokens", output_tokens);
        span.record("dream.llm_attempts", attempts);
        let rejection = match last {
            Ok(artifact) => {
                self.tally.accept();
                span.record("dream.llm_fallback", false);
                span.record("dream.origin", "llm");
                return Ok(ProposeOutcome {
                    artifact,
                    tokens,
                    origin: Some(CandidateOrigin::Llm),
                });
            }
            Err(rejection) => rejection,
        };
        span.record("dream.llm_reject_reason", rejection.reason.as_str());
        span.record("dream.llm_status", rejection.child_status.as_str());
        span.record("dream.llm_reject_excerpt", rejection.excerpt.as_str());
        if rejection.reason == ProposalRejectReason::Aborted || self.options.cancel.is_cancelled() {
            self.reject(round, attempts, &rejection, false)?;
            span.record("dream.llm_fallback", false);
            return Err(dream_abort("dream proposer child aborted"));
        }
        // Rejected for good: the deterministic local proposer stands in,
        // keeping the tokens the child already spent on the node.
        self.reject(round, attempts, &rejection, true)?;
        let fallback = self.task.propose(parent, params, rng, round);
        span.record("dream.llm_fallback", true);
        span.record("dream.origin", "local");
        Ok(ProposeOutcome {
            artifact: fallback,
            tokens,
            origin: Some(CandidateOrigin::Local),
        })
    }
}

/// The proposer prompt (TS `buildProposePrompt`): header, optional guidance,
/// the instruction, the candidate, the hints, the task contract, the output
/// contract, and the JSON-only instruction LAST.
#[must_use]
pub fn build_propose_prompt(
    task_id: &str,
    prompt_context: Option<&str>,
    guidance: Option<&str>,
    parent_json: Option<&Value>,
    params: &ProposeParams,
    round: u32,
) -> String {
    let mut parts = vec![format!("{PROPOSER_PROMPT_HEADER} ({task_id})")];
    if let Some(guidance) = guidance.filter(|guidance| !guidance.is_empty()) {
        parts.push(format!("{GUIDANCE_PROMPT_PREFIX}\n{guidance}"));
    }
    parts.push("Improve the candidate below into a better one for this scored task. Everything you need is in this message; do not search, browse, or call tools.".to_string());
    parts.push(format!(
        "Current candidate (JSON), or null to start fresh:\n{}",
        parent_json.map_or_else(|| "null".to_string(), json::stringify)
    ));
    parts.push(format!(
        "Generation hints: stepScale={}, refineDepth={}, branchWidth={}; round {round}.",
        js_number(params.step_scale),
        params.refine_depth,
        params.branch_width
    ));
    if let Some(context) = prompt_context.filter(|context| !context.is_empty()) {
        parts.push(context.to_string());
    }
    parts.push(PROPOSER_OUTPUT_CONTRACT.to_string());
    parts.push(PROPOSER_JSON_ONLY.to_string());
    parts.join("\n\n")
}

/// One recorded tree's replay of the CURRENT policy, as the dreamer prompt shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamerPoolTreeDigest {
    pub tree_id: String,
    #[serde(rename = "N")]
    pub n: u32,
    pub rounds: u32,
    pub out_of_support_cells: u32,
    pub best_score: f64,
    pub value: f64,
    pub quality: f64,
    pub anytime: f64,
    pub cost: f64,
    pub rounds_saved: f64,
}

/// One earlier candidate's verdict, as the dreamer prompt shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct DreamHistoryEntry {
    pub iteration: u32,
    pub policy_id: String,
    pub origin: CandidateOrigin,
    pub changed: Vec<String>,
    pub value: f64,
    pub quality: f64,
    pub reason: CandidateReason,
}

/// The dreamer's budget statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamerBudget {
    pub workers: u32,
    pub k1: u32,
    pub k2: u32,
}

/// Everything the dreamer child is told (TS `DreamChildInput`).
#[derive(Debug, Clone, PartialEq)]
pub struct DreamChildInput {
    pub current: ExplorationPolicy,
    pub m: usize,
    pub iteration: u32,
    pub objective: ReplayObjectiveConfig,
    pub budget: DreamerBudget,
    pub scale: ObjectiveScale,
    pub pool: Vec<DreamerPoolTreeDigest>,
    pub history: Vec<DreamHistoryEntry>,
}

/// The dreamer's context beyond the current policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct DreamerContext<'a> {
    pub iteration: u32,
    /// The frozen pool the candidates will be scored on.
    pub pool: &'a [RecordedTree],
    /// The scoring the prompt states; `DEFAULT_OBJECTIVE` when `None`.
    pub objective: Option<ReplayObjectiveConfig>,
    /// Max parallelism W; the pool's max `header.w` (else 1) when `None`.
    pub workers: Option<u32>,
    pub k1: Option<u32>,
    pub k2: Option<u32>,
    /// Verdicts of earlier dreaming steps.
    pub history: &'a [DreamHistoryEntry],
}

/// Digest the frozen pool for the dreamer (TS `buildDreamerInput`): the
/// CURRENT policy's replay on every tree (tree-id order) under the selection's
/// own simulator and objective, its spend charged raw. Scalars only.
#[must_use]
pub fn build_dreamer_input(
    current: &ExplorationPolicy,
    m: usize,
    context: &DreamerContext<'_>,
) -> DreamChildInput {
    let mut pool: Vec<&RecordedTree> = context.pool.iter().collect();
    pool.sort_by(|a, b| locale_compare(&a.header.tree_id, &b.header.tree_id));
    let objective = context.objective.unwrap_or(DEFAULT_OBJECTIVE);
    let workers = context
        .workers
        .unwrap_or_else(|| pool.iter().map(|tree| tree.header.w).fold(1, u32::max))
        .max(1);
    let k1 = context.k1.unwrap_or(1).max(1);
    let k2 = context.k2.unwrap_or(k1).max(1);
    let scale = pool_score_scale(pool.iter().copied());
    let replays: Vec<_> = pool
        .iter()
        .map(|tree| simulate_policy(tree, current, ReplayConfig { k2 }))
        .collect();
    let cfg = DreamingScoreConfig {
        k1,
        k2,
        objective,
        quality_eps: 0.0,
    };
    let terms = terms_on_pool(&replays, &pool, &cfg, scale, SpendCharge::Raw);
    let digest = pool
        .iter()
        .zip(replays.iter().zip(&terms))
        .map(|(tree, (replay, terms))| DreamerPoolTreeDigest {
            tree_id: tree.header.tree_id.clone(),
            n: replay.n,
            rounds: replay.rounds,
            out_of_support_cells: replay.out_of_support_cells,
            best_score: replay.best_score,
            value: terms.value,
            quality: terms.quality,
            anytime: terms.anytime,
            cost: terms.cost,
            rounds_saved: terms.rounds_saved,
        })
        .collect();
    DreamChildInput {
        current: *current,
        m,
        iteration: context.iteration,
        objective,
        budget: DreamerBudget { workers, k1, k2 },
        scale,
        pool: digest,
        history: context.history.to_vec(),
    }
}

/// A history entry per verdict of one finished dreaming step.
#[must_use]
pub fn history_of(iteration: u32, verdicts: &[CandidateVerdict]) -> Vec<DreamHistoryEntry> {
    verdicts
        .iter()
        .map(|verdict| DreamHistoryEntry {
            iteration,
            policy_id: verdict.policy_id.clone(),
            origin: verdict.origin,
            changed: verdict.changed.clone(),
            value: verdict.value,
            quality: verdict.quality,
            reason: verdict.reason,
        })
        .collect()
}

/// The entry a reverted adoption adds after its winner: the same policy, reason `revoked`.
#[must_use]
pub fn revoked_history_entry(
    iteration: u32,
    winner: &CandidateVerdict,
    probation: &DreamProbationRecord,
) -> DreamHistoryEntry {
    DreamHistoryEntry {
        iteration,
        policy_id: probation.policy_id.clone(),
        origin: winner.origin,
        changed: winner.changed.clone(),
        value: winner.value,
        quality: winner.quality,
        reason: CandidateReason::Revoked,
    }
}

/// The strict per-entry parse of a dreamer array.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCandidates {
    pub kept: Vec<ExplorationPolicy>,
    pub dropped: Vec<DroppedCandidate>,
    /// Entries in the array (0 for a non-array).
    pub returned: usize,
}

/// One dreamer entry the strict parser refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedCandidate {
    pub index: usize,
    pub reason: String,
}

/// Map a returned JSON array through the strict policy parser per entry (TS
/// `parseCandidateArray`). Never fails: a non-array is zero entries.
#[must_use]
pub fn parse_candidate_array(value: &Value) -> ParsedCandidates {
    let Value::Array(entries) = value else {
        return ParsedCandidates {
            kept: Vec::new(),
            dropped: Vec::new(),
            returned: 0,
        };
    };
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match parse_exploration_policy(entry) {
            Ok(policy) => kept.push(policy),
            Err(error) => dropped.push(DroppedCandidate {
                index,
                reason: error.0,
            }),
        }
    }
    ParsedCandidates {
        kept,
        dropped,
        returned: entries.len(),
    }
}

/// What the LLM dreamer runs with.
pub struct LlmDreamerOptions<'a> {
    pub scope: &'a ChildRuntimeScope,
    pub cancel: &'a CancellationToken,
    pub token_budget: u64,
    /// The labelled fork for the local mutator (the fallback or the top-up).
    pub local_fallback_rng: SeededRng,
    pub context: DreamerContext<'a>,
    /// Where every rejected child result is written, as `role: dreamer` lines.
    pub rejections: Option<&'a RejectionLog<'a>>,
}

/// The outcome of one LLM dreaming call.
#[derive(Debug, Clone, PartialEq)]
pub struct DreamedCandidates {
    /// The child's kept entries (identical/duplicate ones included), then any local top-up.
    pub candidates: Vec<CandidateInput>,
    pub tokens: u64,
    pub dreamer: DreamerKind,
    pub returned: usize,
    pub dropped: Vec<DroppedCandidate>,
    /// Distinct policies, different from current, the child contributed (at most `m`).
    pub kept: usize,
    /// Child entries beyond the `m` distinct ones.
    pub truncated: usize,
    /// Local candidates appended to fill the budget.
    pub local: usize,
    /// True when the child produced nothing usable and the whole set is local.
    pub llm_fallback: bool,
}

/// The LLM dreamer (TS `proposePoliciesWithAgent`): ask a child for `m`
/// revised policies, keep the entries the strict parser accepts, take the
/// first `m` DISTINCT policies that differ from the current one (identical and
/// duplicate entries ride along for the selection to label), and top up any
/// shortfall from the local mutator on the labelled fork.
///
/// # Errors
///
/// [`DreamStoreError::Aborted`] on a cancelled child; a store error from the
/// rejection log.
#[allow(clippy::too_many_lines)] // one call, its span table, and the take loop
pub fn propose_policies_with_agent(
    runner: &dyn RunAgent,
    current: &ExplorationPolicy,
    m: usize,
    options: &LlmDreamerOptions<'_>,
) -> Result<DreamedCandidates, DreamStoreError> {
    let iteration = options.context.iteration;
    let input = build_dreamer_input(current, m, &options.context);
    let prompt = build_dream_prompt(&input);
    let span = tracing::info_span!(
        "dream.llm_dream",
        dream.candidates_requested = m,
        dream.iteration = iteration,
        dream.tokens = Empty,
        dream.llm_attempts = Empty,
        dream.candidates_returned = Empty,
        dream.candidates_dropped = Empty,
        dream.llm_status = Empty,
        dream.llm_reject_reason = Empty,
        dream.llm_reject_excerpt = Empty,
        dream.candidates_kept = Empty,
        dream.candidates_truncated = Empty,
        dream.candidates_local = Empty,
        dream.candidates = Empty,
        dream.dreamer = Empty,
        dream.llm_fallback = Empty,
    );
    let _entered = span.enter();
    let reject = |attempt: u32, rejection: &ChildRejection, fell_back: bool| {
        options.rejections.map_or(Ok(()), |log| {
            log.append(&rejection_input(
                Some(RejectionRole::Dreamer),
                iteration,
                0,
                attempt,
                rejection,
                fell_back,
            ))
        })
    };
    let request = options.scope.request(prompt);
    let run_options = options.scope.options(options.cancel, options.token_budget);
    let mut tokens = 0;
    let mut attempts = 0;
    let mut parsed: Option<ParsedCandidates> = None;
    let mut last: Option<Result<ParsedCandidates, ChildRejection>> = None;
    if m > 0 {
        loop {
            attempts += 1;
            parsed = None;
            let outcome = run_structured_child(
                runner,
                &request,
                &run_options,
                JsonContainer::Array,
                |value| {
                    let candidates = parse_candidate_array(value);
                    parsed = Some(candidates.clone());
                    if candidates.kept.is_empty() {
                        return Err(ValidationError::Shape(if candidates.returned == 0 {
                            "the array holds no policy object".to_string()
                        } else {
                            format!(
                                "every entry was dropped: {}",
                                candidates
                                    .dropped
                                    .iter()
                                    .map(|entry| format!("[{}] {}", entry.index, entry.reason))
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            )
                        }));
                    }
                    Ok(candidates)
                },
            );
            tokens += outcome.tokens();
            match outcome {
                ChildOutcome::Accepted { value, .. } => {
                    last = Some(Ok(value));
                    break;
                }
                ChildOutcome::Rejected(rejection) => {
                    let retry = retryable(rejection.reason)
                        && attempts <= PROPOSER_RETRIES
                        && !options.cancel.is_cancelled();
                    if !retry {
                        last = Some(Err(rejection));
                        break;
                    }
                    reject(attempts, &rejection, false)?;
                }
            }
        }
    }
    span.record("dream.tokens", tokens);
    span.record("dream.llm_attempts", attempts);
    span.record(
        "dream.candidates_returned",
        parsed.as_ref().map_or(0, |parsed| parsed.returned),
    );
    span.record(
        "dream.candidates_dropped",
        parsed.as_ref().map_or(0, |parsed| parsed.dropped.len()),
    );
    match &last {
        Some(Err(rejection)) => {
            span.record("dream.llm_status", rejection.child_status.as_str());
            span.record("dream.llm_reject_reason", rejection.reason.as_str());
            span.record("dream.llm_reject_excerpt", rejection.excerpt.as_str());
            if rejection.reason == ProposalRejectReason::Aborted || options.cancel.is_cancelled() {
                reject(attempts, rejection, false)?;
                span.record("dream.llm_fallback", false);
                return Err(dream_abort("dream dreamer child aborted"));
            }
            reject(attempts, rejection, true)?;
        }
        Some(Ok(_)) => {
            span.record("dream.llm_status", RunAgentStatus::Completed.as_str());
        }
        None => {}
    }
    let from_child = match &last {
        Some(Ok(parsed)) => parsed.kept.as_slice(),
        _ => &[],
    };
    let current_id = policy_id(current);
    let mut seen = std::collections::HashSet::new();
    let mut taken = Vec::new();
    let mut truncated = 0;
    for policy in from_child {
        if seen.len() >= m {
            truncated += 1;
            continue;
        }
        let id = policy_id(policy);
        if id != current_id {
            seen.insert(id);
        }
        taken.push(CandidateInput {
            policy: *policy,
            origin: CandidateOrigin::Llm,
        });
    }
    let kept = seen.len();
    let local: Vec<CandidateInput> =
        propose_policies(current, m - kept, &options.local_fallback_rng)
            .into_iter()
            .map(CandidateInput::from)
            .collect();
    let local_count = local.len();
    let mut candidates = taken;
    candidates.extend(local);
    let llm_fallback = kept == 0 && m > 0;
    let dreamer = dreamer_kind_of(&candidates);
    span.record("dream.candidates_kept", kept);
    span.record("dream.candidates_truncated", truncated);
    span.record("dream.candidates_local", local_count);
    span.record("dream.candidates", candidates.len());
    span.record("dream.dreamer", dreamer.as_str());
    span.record("dream.llm_fallback", llm_fallback);
    let (returned, dropped) =
        parsed.map_or((0, Vec::new()), |parsed| (parsed.returned, parsed.dropped));
    Ok(DreamedCandidates {
        candidates,
        tokens,
        dreamer,
        returned,
        dropped,
        kept,
        truncated,
        local: local_count,
        llm_fallback,
    })
}

/// `Number.isInteger(x) ? String(x) : x.toFixed(6)`.
fn fmt6(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 {
        js_number(value)
    } else {
        to_fixed(value, 6)
    }
}

fn policy_schema_text() -> String {
    let names = |all: &[&str]| all.join(", ");
    let numeric: Vec<String> = POLICY_BOUNDS
        .iter()
        .map(|(field, bound)| {
            format!(
                "- {}: {} in [{}, {}]",
                field.as_str(),
                if bound.integer { "integer" } else { "number" },
                js_number(bound.min),
                js_number(bound.max)
            )
        })
        .collect();
    [
        "Named-rule fields (use exactly one listed value each):".to_string(),
        format!(
            "- selectionRule: one of {}",
            names(
                &SELECTION_RULES
                    .iter()
                    .map(|rule| rule.as_str())
                    .collect::<Vec<_>>()
            )
        ),
        format!(
            "- recoveryPolicy: one of {}",
            names(
                &RECOVERY_POLICIES
                    .iter()
                    .map(|rule| rule.as_str())
                    .collect::<Vec<_>>()
            )
        ),
        format!(
            "- stopRule: one of {}",
            names(
                &STOP_RULES
                    .iter()
                    .map(|rule| rule.as_str())
                    .collect::<Vec<_>>()
            )
        ),
        "Numeric fields:".to_string(),
        numeric.join("\n"),
    ]
    .join("\n")
}

fn objective_text(input: &DreamChildInput) -> String {
    let ReplayObjectiveConfig {
        beta1,
        beta2,
        beta3,
    } = input.objective;
    let DreamerBudget { workers, k1, k2 } = input.budget;
    let trees = input.pool.len();
    [
        format!(
            "How a candidate is judged. Each candidate is replayed on {trees} recorded discovery tree{} (W = {workers} cells per round, online round cap k1 = {k1}, replay round cap k2 = {k2}) and its mean V is compared with the current policy's mean V on the same trees. ",
            if trees == 1 { "" } else { "s" }
        ),
        format!(
            "V = (1 - beta3) * q + beta3 * anytime - beta1 * S / (W * k1) + beta2 * (1 - rounds / k1), with q the best revealed score normalized to the pool's score range [{}, {}], anytime the mean normalized best-so-far over the probe budget W * k1 (rewards reaching the best early), S the charged selections (revealed nodes plus out-of-support selections) and rounds the replay decision rounds; beta1 = {}, beta2 = {}, beta3 = {}. ",
            fmt6(input.scale.score_min),
            fmt6(input.scale.score_max),
            js_number(beta1),
            js_number(beta2),
            js_number(beta3)
        ),
        "Selection rule: a candidate whose q is below the current policy's on ANY of these trees is excluded; among the rest the highest mean V wins, and the current policy wins every tie, so only a STRICTLY higher mean V on these recorded trees is accepted. ".to_string(),
        "Evidence-backed spend: a candidate's stop-early credit (fewer charged probes, fewer rounds) is charged at the latest probe and round at which the same candidate was still improving on the OTHER recorded trees, so on a single tree there is none (a candidate is charged the full budget W * k1 and k1 rounds), the current policy is charged exactly what it spent, and no-worse quality must hold on every tree. ".to_string(),
        "Probation: an adopted policy's first online rollout must reach at least the current policy's lowest recorded best on these trees, or the adoption is reverted and the policy is revoked for the rest of the run; a replay win on recorded trees is not an online result. ".to_string(),
        "Replay mechanics: a candidate re-walks each recorded tree, selecting cells by its own rules and revealing the recorded child of each selected cell; nothing new is ever generated. A selected cell whose recorded children are all revealed is out of support: it reveals nothing but is charged as a probe.".to_string(),
    ]
    .concat()
}

fn policy_semantics_text(workers: u32) -> String {
    let dead: Vec<&str> = REPLAY_DEAD_FIELDS
        .iter()
        .map(|field| field.as_str())
        .collect();
    [
        "Field semantics (what replay reads):".to_string(),
        "- selectionRule ranks the eligible cells (the root plus every revealed leaf) each round: best-first by score descending; explore-root puts the root first, then the rest by score; round-robin by node id; weighted by score plus explorationBias for every cell scoring at least promisingThreshold times the current best.".to_string(),
        format!("- batchSize: cells probed per round, capped at W = {workers} at runtime, so a value above {workers} changes nothing. A batch never holds a node together with its parent."),
        "- stopRule: patience stops after beta consecutive rounds without improving the best score; fixed-rounds stops after beta rounds; threshold stops once the best score reaches targetScore; never runs to the round cap.".to_string(),
        "- beta is read only under patience and fixed-rounds; targetScore only under threshold; promisingThreshold and explorationBias only under weighted. Changing a field the current rules do not read changes nothing.".to_string(),
        format!("- {} are never read by replay (they shape only how new candidates are generated online): a policy that differs from the current one only in them replays identically and cannot win. Keep them at the current values.", dead.join(", ")),
    ]
    .join("\n")
}

fn pool_text(input: &DreamChildInput) -> String {
    if input.pool.is_empty() {
        return "Current policy on the pool: no recorded trees yet.".to_string();
    }
    let mut rows = Vec::with_capacity(input.pool.len() + 1);
    let mut total = 0.0;
    for tree in &input.pool {
        total += tree.value;
    }
    #[allow(clippy::cast_precision_loss)] // a pool never holds 2^52 trees
    let mean = total / input.pool.len() as f64;
    rows.push(format!(
        "Current policy on the pool (its replay per tree; mean V {} is the value to beat):",
        fmt6(mean)
    ));
    for tree in &input.pool {
        rows.push(format!(
            "- {}: N {}, rounds {}, out-of-support {}, best {}, V {} (q {}, anytime {}, cost {}, rounds saved {})",
            tree.tree_id,
            tree.n,
            tree.rounds,
            tree.out_of_support_cells,
            fmt6(tree.best_score),
            fmt6(tree.value),
            fmt6(tree.quality),
            fmt6(tree.anytime),
            fmt6(tree.cost),
            fmt6(tree.rounds_saved)
        ));
    }
    rows.join("\n")
}

fn history_text(input: &DreamChildInput) -> Option<String> {
    if input.history.is_empty() {
        return None;
    }
    let mut rows = vec![
        "Earlier candidates and their verdicts (do not repeat a losing or revoked one unchanged):"
            .to_string(),
    ];
    for entry in &input.history {
        rows.push(format!(
            "- iteration {}: {} ({}; changed {}) -> V {}, q {}: {}",
            entry.iteration,
            entry.policy_id,
            entry.origin.as_str(),
            if entry.changed.is_empty() {
                "nothing".to_string()
            } else {
                entry.changed.join(", ")
            },
            fmt6(entry.value),
            fmt6(entry.quality),
            entry.reason.as_str()
        ));
    }
    Some(rows.join("\n"))
}

/// The dreamer prompt (TS `buildDreamPrompt`).
#[must_use]
pub fn build_dream_prompt(input: &DreamChildInput) -> String {
    let mut parts = vec![
        DREAMER_PROMPT_HEADER.to_string(),
        format!(
            "Propose up to {} revised exploration policies that should score better than the current one on replay (dreaming step {}). A policy is DATA: a flat JSON object with exactly these fields.",
            input.m, input.iteration
        ),
        policy_schema_text(),
        objective_text(input),
        policy_semantics_text(input.budget.workers),
        format!("Current policy:\n{}", json::stringify(&input.current)),
        pool_text(input),
    ];
    parts.extend(history_text(input));
    parts.push(format!(
        "Return at most {} policy objects that are pairwise distinct and each differ from the current policy in at least one field replay reads; a duplicate or a copy of the current policy is discarded, as is any object with an unknown or missing field, a wrong type, or an out-of-range value. {JSON_ARRAY_ONLY}",
        input.m
    ));
    parts.join("\n\n")
}

/// One recorded candidate in a guidance digest.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceNodeDigest {
    pub score: f64,
    pub round: u32,
    pub artifact_json: String,
}

/// One recorded tree in a guidance digest. Recorded artifacts and scalar scores only.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceTreeDigest {
    pub tree_id: String,
    pub policy_id: String,
    pub best_score: f64,
    /// Revealed non-root nodes.
    pub attempts: usize,
    /// Online decision rounds the rollout took.
    pub rounds: u32,
    /// Distinct failure classes seen, sorted.
    pub fail_classes: Vec<String>,
    /// The top-k valid candidates by score (ties by seq), truncated to the cap.
    pub top_nodes: Vec<GuidanceNodeDigest>,
}

/// The guidance writer's input: a bounded, deterministic digest of the frozen pool.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuidanceInput {
    pub task_id: String,
    pub iteration: u32,
    pub pool_size: usize,
    pub trees: Vec<GuidanceTreeDigest>,
}

/// Insight text a guidance writer produced and the tokens it spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceInsights {
    pub text: String,
    pub tokens: u64,
}

/// An injected guidance writer (test-only, token-free).
pub type InjectedInsights = Box<dyn FnMut(&GuidanceInput) -> GuidanceInsights + Send>;

/// The semantic-guidance ablation: a guidance-writer child per iteration >= 1,
/// or an injected writer in its place.
pub enum SemanticGuidance {
    Child,
    Injected(InjectedInsights),
}

/// The first `max_chars` UTF-16 units of `json`, plus `...` when cut.
fn truncate_artifact(json: &str, max_chars: usize) -> String {
    let units: Vec<u16> = json.encode_utf16().collect();
    if units.len() <= max_chars {
        json.to_string()
    } else {
        format!("{}...", String::from_utf16_lossy(&units[..max_chars]))
    }
}

/// Digest a frozen pool for the guidance writer (TS `buildGuidanceInput`):
/// trees in tree-id order, top nodes by score descending then seq ascending,
/// bounded by `top_k` and `max_artifact_chars`.
#[must_use]
pub fn build_guidance_input(
    pool: &[RecordedTree],
    task_id: &str,
    iteration: u32,
    top_k: usize,
    max_artifact_chars: usize,
) -> GuidanceInput {
    let mut sorted: Vec<&RecordedTree> = pool.iter().collect();
    sorted.sort_by(|a, b| locale_compare(&a.header.tree_id, &b.header.tree_id));
    let trees: Vec<GuidanceTreeDigest> = sorted
        .into_iter()
        .map(|tree| {
            let non_root: Vec<_> = tree
                .nodes
                .iter()
                .filter(|node| node.parent_id.is_some())
                .collect();
            let mut valid: Vec<_> = tree
                .nodes
                .iter()
                .filter(|node| node.valid && node.score.is_finite())
                .collect();
            valid.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.seq.cmp(&b.seq))
            });
            let mut fail_classes: Vec<String> = non_root
                .iter()
                .filter_map(|node| node.fail_class.clone())
                .filter(|class| !class.is_empty())
                .collect();
            fail_classes.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            fail_classes.dedup();
            GuidanceTreeDigest {
                tree_id: tree.header.tree_id.clone(),
                policy_id: tree.header.policy_id.clone(),
                best_score: valid.first().map_or(0.0, |node| node.score),
                attempts: non_root.len(),
                rounds: non_root.iter().map(|node| node.round).max().unwrap_or(0),
                fail_classes,
                top_nodes: valid
                    .iter()
                    .take(top_k)
                    .map(|node| GuidanceNodeDigest {
                        score: node.score,
                        round: node.round,
                        artifact_json: truncate_artifact(
                            &tree
                                .load_blob(node)
                                .map(|blob| json::stringify(&blob))
                                .unwrap_or_default(),
                            max_artifact_chars,
                        ),
                    })
                    .collect(),
            }
        })
        .collect();
    GuidanceInput {
        task_id: task_id.to_string(),
        iteration,
        pool_size: trees.len(),
        trees,
    }
}

fn build_guidance_prompt(input: &GuidanceInput) -> String {
    [
        format!("{GUIDANCE_PROMPT_HEADER} ({})", input.task_id),
        format!(
            "Below is a digest of {} prior discovery trajectories for this scored task (iteration {}): per tree the exploration policy id, the best score, the evaluated attempts, the decision rounds, the failure classes seen, and the top-scoring recorded candidates. Write 3-8 short directional insights a proposer could use to improve its next candidates: which kinds of edits raised the score, what failed and why, and what remains unexplored. Everything you need is in this message; do not search, browse, or call tools.",
            input.pool_size, input.iteration
        ),
        format!("Trajectory digest (JSON):\n{}", json::stringify(input)),
        format!("Return {{\"insights\": \"<the insights as plain text>\"}}. {JSON_OBJECT_ONLY}"),
    ]
    .join("\n\n")
}

/// The guidance writer must return `{"insights": "<non-empty text>"}`.
fn parse_insights(value: &Value) -> Result<String, String> {
    let Value::Object(map) = value else {
        return Err("guidance must be a JSON object".to_string());
    };
    match map.get("insights") {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        _ => Err("guidance insights must be a non-empty string".to_string()),
    }
}

/// One iteration's semantic guidance inside `dream.llm_guidance` (TS
/// `resolveGuidance`): a child failure (after one retry of an `error`) falls
/// back to EMPTY guidance, an abort aborts the run, an injected writer stands
/// in for the child.
///
/// # Errors
///
/// [`DreamStoreError::Aborted`] when the writer was cancelled.
pub fn resolve_guidance(
    runner: &dyn RunAgent,
    option: &mut SemanticGuidance,
    input: &GuidanceInput,
    scope: &ChildRuntimeScope,
    cancel: &CancellationToken,
    token_budget: u64,
) -> Result<GuidanceInsights, DreamStoreError> {
    let span = tracing::info_span!(
        "dream.llm_guidance",
        dream.iteration = input.iteration,
        dream.pool_size = input.pool_size,
        dream.tokens = Empty,
        dream.llm_fallback = Empty,
    );
    let _entered = span.enter();
    if let SemanticGuidance::Injected(insights) = option {
        let produced = insights(input);
        span.record("dream.tokens", produced.tokens);
        span.record("dream.llm_fallback", false);
        return Ok(produced);
    }
    let request = scope.request(build_guidance_prompt(input));
    let run_options = scope.options(cancel, token_budget);
    let mut spent = 0;
    let mut attempt = 0;
    let status = loop {
        let result = runner.run(&request, &run_options);
        spent += result.total_tokens;
        // A completed child whose output does not validate is an `error`.
        let status = if result.status == RunAgentStatus::Completed {
            match crate::child::extract_json_value(&result.output, JsonContainer::Object)
                .and_then(|value| parse_insights(&value))
            {
                Ok(text) => {
                    span.record("dream.tokens", spent);
                    span.record("dream.llm_fallback", false);
                    return Ok(GuidanceInsights {
                        text,
                        tokens: spent,
                    });
                }
                Err(_) => RunAgentStatus::Error,
            }
        } else {
            result.status
        };
        if status != RunAgentStatus::Error || attempt >= 1 || cancel.is_cancelled() {
            break status;
        }
        attempt += 1;
    };
    if status == RunAgentStatus::Aborted || cancel.is_cancelled() {
        return Err(dream_abort("dream guidance child aborted"));
    }
    span.record("dream.tokens", spent);
    span.record("dream.llm_fallback", true);
    Ok(GuidanceInsights {
        text: String::new(),
        tokens: spent,
    })
}
