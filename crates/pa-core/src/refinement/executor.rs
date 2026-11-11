//! The refinement executor: plan a refinement (rollback or LLM pass), re-read the harness store,
//! apply the proposal, and record the result.

use pa_types::ai::{AssistantMessage, StopReason};
use pa_types::session::AgentMessage;

use super::planner::{
    AUTO_REFINE_REVIEW_MAX_OUTPUT_TOKENS,
    AUTO_REFINE_REVIEW_SYSTEM_PROMPT,
    ApplyOptions,
    REFINEMENT_MAX_OUTPUT_TOKENS,
    REFINEMENT_SYSTEM_PROMPT,
    RefinementProposal,
    apply_refinement_proposal,
    parse_proposal,
    refinement_request,
    rollback_proposal,
};
use super::{
    HarnessScope,
    HarnessState,
    REFINEMENT_KINDS,
    RefinementKind,
    RefinementResult,
    already_targeted_fingerprints,
    infer_refinement_result_scope,
    merge_refinement_history,
};

#[derive(Debug, Default, Clone)]
pub struct RefineOptions {
    pub global: bool,
    pub instructions: Option<String>,
    pub rollback_id: Option<String>,
    /// The session's read-only package overlay (upstream #2298): the apply
    /// refuses update/delete edits against its entries.
    pub package_state: Option<std::sync::Arc<HarnessState>>,
}

/// A planned refinement awaiting application.
pub struct RefinementPlan {
    pub proposal: RefinementProposal,
    pub id: String,
    pub rollback_of: Option<String>,
    pub rollback_scope: Option<HarnessScope>,
}

/// Mint a refinement id in the canonical `refine_<timestamp>` format.
pub fn generate_refinement_id() -> String {
    let iso = crate::session::manager::format_iso_now();
    let digits: String = iso.chars().filter(char::is_ascii_digit).collect();
    format!("refine_{}", &digits[..digits.len().min(17)])
}

/// Characters of an entry's content (and a skill's serialized reference and
/// arguments) the refine prompt's overview shows.
pub(crate) const OVERVIEW_SNIPPET_CHARS: usize = 240;

/// Entries per kind the refine prompt's overview lists.
const OVERVIEW_ENTRIES_PER_KIND: usize = 40;

/// The overview's view of one text: its first [`OVERVIEW_SNIPPET_CHARS`]
/// characters (cut on a char boundary) and how many characters it hides.
fn snippet(text: &str) -> (String, usize) {
    let total = text.chars().count();
    if total <= OVERVIEW_SNIPPET_CHARS {
        (text.to_string(), 0)
    } else {
        (
            text.chars().take(OVERVIEW_SNIPPET_CHARS).collect(),
            total - OVERVIEW_SNIPPET_CHARS,
        )
    }
}

/// A snippet as the overview renders it: a truncated one says how much is
/// not shown, so the refiner can tell a fragment from a whole entry.
fn render_snippet(text: &str) -> String {
    match snippet(text) {
        (shown, 0) => shown,
        (shown, hidden) => format!("{shown}... (+{hidden} chars not shown)"),
    }
}

/// The entry's content as the overview shows it: whitespace runs collapsed.
fn overview_content(entry: &super::HarnessEntry) -> String {
    entry
        .content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The serialized skill reference/arguments the overview shows, if any.
fn overview_skill_maps(entry: &super::HarnessEntry) -> [Option<String>; 2] {
    let serialize = |map: &serde_json::Map<String, serde_json::Value>| {
        (entry.kind == RefinementKind::Skill && !map.is_empty())
            .then(|| serde_json::to_string(map).unwrap_or_default())
    };
    [serialize(&entry.reference), serialize(&entry.arguments)]
}

/// Characters of `entry` the refine overview does not show (content plus a
/// skill's reference and arguments); `0` when the refiner saw it whole.
#[must_use]
pub(crate) fn overview_hidden_chars(entry: &super::HarnessEntry) -> usize {
    let content_hidden = snippet(&overview_content(entry)).1;
    overview_skill_maps(entry)
        .iter()
        .flatten()
        .map(|text| snippet(text).1)
        .sum::<usize>()
        + content_hidden
}

/// Harness overview section for the refine prompt (per-kind, 40-entry cap,
/// 240-char content/ref/args snippets, each truncation marked with the hidden
/// length).
#[must_use]
pub fn overview_for_prompt(state: &HarnessState) -> String {
    let mut lines: Vec<String> = Vec::new();
    for kind in REFINEMENT_KINDS {
        let entries: Vec<&super::HarnessEntry> = state
            .entries
            .get(&kind_value(kind))
            .map(|records| records.values().collect())
            .unwrap_or_default();
        lines.push(format!("{kind}: {}", entries.len()));
        for entry in entries.iter().take(OVERVIEW_ENTRIES_PER_KIND) {
            let content = render_snippet(&overview_content(entry));
            let [reference, arguments] = overview_skill_maps(entry);
            let arguments_text = arguments
                .map(|text| format!(" args={}", render_snippet(&text)))
                .unwrap_or_default();
            let reference_text = reference
                .map(|text| format!(" ref={}", render_snippet(&text)))
                .unwrap_or_default();
            let label = super::package_harness::harness_entry_label(entry);
            // A disabled entry is marked so the refiner neither recreates
            // it under a new id nor mistakes it for active guidance.
            let disabled_text = if entry.is_enabled() {
                ""
            } else {
                " [disabled]"
            };
            lines.push(format!(
                "- [{label}]{disabled_text} {} ({}, v{}){}{}{}: {content}",
                super::compact_text(&entry.title, OVERVIEW_SNIPPET_CHARS),
                super::compact_text(&entry.path, OVERVIEW_SNIPPET_CHARS),
                super::package_harness::harness_version_text(entry.version),
                reference_text,
                arguments_text,
                super::package_harness::package_provenance_text(entry, OVERVIEW_SNIPPET_CHARS),
            ));
        }
        let overflow = entries.len().saturating_sub(OVERVIEW_ENTRIES_PER_KIND);
        if overflow > 0 {
            lines.push(format!("- +{overflow} more {kind} entries"));
        }
    }
    lines.join("\n")
}

#[must_use]
pub fn history_for_prompt(history: &[RefinementResult]) -> String {
    if history.is_empty() {
        return "No prior refinement history.".to_string();
    }
    history
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|item| {
            let edits = item
                .applied_edits
                .iter()
                .map(|edit| {
                    format!(
                        "{} {} {}:{}",
                        if edit.applied { "applied" } else { "failed" },
                        action_name(edit.action),
                        kind_name(edit.kind),
                        edit.id
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let rollback = item
                .rollback_of
                .as_ref()
                .map(|id| format!(" rollbackOf={id}"))
                .unwrap_or_default();
            format!(
                "[{}]{} {}\n{edits}\nExpected outcome: {}",
                item.id, rollback, item.summary, item.expected_outcome
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The `<recurring_failures>` block body for the refiner reviewer: recurring
/// fingerprints in the merged ledger that no existing entry already declares
/// in its `targets_fingerprints`. Returns `None` when there is nothing to
/// show, so the caller can omit the block entirely.
///
/// Parses `state.extensions["failures"]` as JSON directly rather than
/// importing `pa-ledger`: `pa-ledger` depends on `pa-core`, so the crate
/// graph forbids the reverse import (the same pattern `recurrence.rs` uses).
/// The on-disk schema is stable and shared with `pa-ledger`'s
/// `format_failure_ledger_for_prompt`; if either side evolves, both must
/// change together.
fn unaddressed_recurring_for_prompt(state: &HarnessState) -> Option<String> {
    const RECURRENCE_THRESHOLD: u64 = 2;
    const PROMPT_LIMIT: usize = 12;
    const EXCERPT_MAX: usize = 240;

    let failures = state.extensions.get("failures")?;
    let records = failures.get("failures")?.as_object()?;
    let already = already_targeted_fingerprints(state);

    let mut rows: Vec<(u64, String)> = Vec::new();
    for record in records.values() {
        let fp = record.get("fingerprint")?;
        let id = fp.get("id").and_then(|v| v.as_str())?;
        if already.contains(id) {
            continue;
        }
        let count = record.get("count").and_then(serde_json::Value::as_u64)?;
        if count < RECURRENCE_THRESHOLD {
            continue;
        }
        // A fingerprint whose occurrences were all non-actionable (e.g. user
        // denial, provider outage) is tracked but should not turn into a gate
        // opponent. The ledger writes `nonActionableCount` with the full
        // count when every occurrence was non-actionable, so filter on that.
        if let Some(non_actionable) = record
            .get("nonActionableCount")
            .and_then(serde_json::Value::as_u64)
        {
            if non_actionable >= count {
                continue;
            }
        }
        let kind = fp.get("kind").and_then(|v| v.as_str()).unwrap_or("unknown");
        let source = fp.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let class = fp
            .get("exceptionClass")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let first_turn = record
            .get("firstSeenTurn")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let last_turn = record
            .get("lastSeenTurn")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let excerpt_raw = record.get("excerpt").and_then(|v| v.as_str()).unwrap_or("");
        let excerpt_normalised: String =
            excerpt_raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let excerpt = if excerpt_normalised.chars().count() > EXCERPT_MAX {
            let prefix: String = excerpt_normalised.chars().take(EXCERPT_MAX).collect();
            format!("{prefix}...")
        } else {
            excerpt_normalised
        };
        let mut parts = vec![format!("- failure:{id} [{kind}]")];
        if !source.is_empty() {
            parts.push(format!("source={source}"));
        }
        if !class.is_empty() {
            parts.push(format!("class={class}"));
        }
        parts.push(format!("count={count}"));
        parts.push(format!("turns={first_turn}..{last_turn}"));
        rows.push((count, format!("{}\n  {excerpt}", parts.join(" "))));
    }

    if rows.is_empty() {
        return None;
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    let total = rows.len();
    let mut lines: Vec<String> = rows
        .into_iter()
        .take(PROMPT_LIMIT)
        .map(|(_, s)| s)
        .collect();
    if total > PROMPT_LIMIT {
        lines.push(format!("- ... {} more", total - PROMPT_LIMIT));
    }
    Some(lines.join("\n"))
}

/// Model-call seam (test seam over pa-ai completion): takes the request
/// model (output budget pre-clamped), the call's system prompt, and the
/// user prompt; returns the reply text.
pub type RefinerFn = Box<
    dyn FnOnce(
            pa_types::ai::Model,
            &'static str,
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AssistantMessage>> + Send>,
        > + Send,
>;

#[must_use]
pub fn merge_refinement_result_history(
    global: &[RefinementResult],
    session: &[RefinementResult],
) -> Vec<RefinementResult> {
    merge_refinement_history(global, session)
}

fn action_name(action: super::RefinementAction) -> &'static str {
    match action {
        super::RefinementAction::Create => "create",
        super::RefinementAction::Update => "update",
        super::RefinementAction::Delete => "delete",
    }
}

fn kind_name(kind: RefinementKind) -> &'static str {
    match kind {
        RefinementKind::Prompt => "prompt",
        RefinementKind::Memory => "memory",
        RefinementKind::Skill => "skill",
        RefinementKind::Subagent => "subagent",
        RefinementKind::Factory => "factory",
    }
}

fn kind_value(name: &str) -> RefinementKind {
    match name {
        "prompt" => RefinementKind::Prompt,
        "memory" => RefinementKind::Memory,
        "skill" => RefinementKind::Skill,
        "factory" => RefinementKind::Factory,
        _ => RefinementKind::Subagent,
    }
}

/// Serialize the conversation for the refine prompt, tail-capped.
fn conversation_text(messages: &[AgentMessage], cap: usize) -> String {
    let serialized = crate::session_engine::compaction_utils::serialize_conversation(messages);
    let chars: Vec<char> = serialized.chars().collect();
    if chars.len() <= cap {
        serialized
    } else {
        chars[chars.len() - cap..].iter().collect()
    }
}

/// Produce a refinement proposal (rollback, or the LLM pass) without mutating
/// any harness state. Callers re-read the harness file before applying because
/// the LLM call can take many seconds.
///
/// # Errors
///
/// Error on an unknown rollback id, a failed request build or LLM call, or an unparseable reply.
#[tracing::instrument(
    level = "info",
    name = "refine.plan",
    target = "pa_core::refine",
    skip_all,
    fields(
        refine.rollback = options.rollback_id.is_some(),
        refine.global = options.global,
        llm.model = model.id.as_str(),
    ),
    err
)]
pub async fn plan_refinement(
    messages: &[AgentMessage],
    state: &HarnessState,
    history: &[RefinementResult],
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    refine_call: RefinerFn,
) -> anyhow::Result<RefinementPlan> {
    let id = generate_refinement_id();
    if let Some(rollback_id) = &options.rollback_id {
        let Some(target) = history.iter().find(|item| &item.id == rollback_id) else {
            anyhow::bail!("Refinement {rollback_id} not found");
        };
        let fallback_scope = if options.global {
            HarnessScope::Global
        } else {
            HarnessScope::Local
        };
        return Ok(RefinementPlan {
            proposal: rollback_proposal(target),
            id,
            rollback_of: Some(target.id.clone()),
            rollback_scope: Some(infer_refinement_result_scope(target).unwrap_or(fallback_scope)),
        });
    }

    let conversation_text = conversation_text(messages, 80_000);
    let scope_instruction = if options.global {
        "Requested refinement scope: global. Only propose stable cross-session continual harness edits, durable user preferences, reusable skills/subagents, or explicitly project-qualified facts that should affect future Prime Agent sessions. Do not persist session-only progress, temporary blockers, or current-run coordination globally. Package entries in the overview are read-only: create a global same-kind, same-id override instead of updating or deleting a package entry."
    } else {
        "Requested refinement scope: local. Prefer local continual harness edits for current task progress, temporary blockers, current-run coordination, and project facts that are not clearly reusable across Prime Agent sessions. Global entries in the overview are read-only context, and package entries are also read-only: do not propose update or delete edits for them; create a local same-kind, same-id entry instead if an override is needed."
    };
    let build_prompt = |conversation: &str| -> String {
        let mut sections = vec![
            format!(
                "<current_harness_state>\n{}\n</current_harness_state>",
                overview_for_prompt(state)
            ),
            format!(
                "<refinement_history>\n{}\n</refinement_history>",
                history_for_prompt(history)
            ),
            format!("<conversation>\n{conversation}\n</conversation>"),
            format!("<scope_policy>\n{scope_instruction}\n</scope_policy>"),
        ];
        if let Some(instructions) = &options.instructions {
            sections.push(format!(
                "<user_refine_instructions>\n{instructions}\n</user_refine_instructions>"
            ));
        }
        sections.push(
            "Return only JSON edits. If no useful edit is justified, return an empty edits array with a rationale."
                .to_string(),
        );
        sections.join("\n\n")
    };
    let (request_max_tokens, user_prompt) = refinement_request(
        model,
        super::planner::REFINEMENT_SYSTEM_PROMPT,
        &conversation_text,
        &build_prompt,
        REFINEMENT_MAX_OUTPUT_TOKENS,
    )?;
    let mut request_model = model.clone();
    request_model.max_tokens = request_max_tokens.min(REFINEMENT_MAX_OUTPUT_TOKENS);

    let max_tokens = request_model.max_tokens;
    let reply = refine_call(request_model, REFINEMENT_SYSTEM_PROMPT, user_prompt).await?;
    let text = reply_text(&reply, max_tokens, "Refinement failed")?;
    Ok(RefinementPlan {
        proposal: parse_proposal(&text).map_err(anyhow::Error::msg)?,
        id,
        rollback_of: None,
        rollback_scope: None,
    })
}

/// The text of a side call's reply. A reply cut off at its output limit or
/// failed by the provider is an error that says so, never parsed as JSON.
fn reply_text(reply: &AssistantMessage, max_tokens: u64, failure: &str) -> anyhow::Result<String> {
    match reply.stop_reason {
        StopReason::Length => anyhow::bail!(
            "{failure}: the model hit its output limit ({max_tokens} tokens, {}/{})",
            reply.provider,
            reply.model
        ),
        StopReason::Error => anyhow::bail!(
            "{failure}: {}",
            reply.error_message.as_deref().unwrap_or("Unknown error")
        ),
        StopReason::Stop | StopReason::ToolUse | StopReason::Aborted => {}
    }
    Ok(reply
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Apply a plan to the (re-read) harness state. `factory_enabled` is the
/// `factory.enabled` opt-in (default off), resolved by the caller
/// immediately before this call — after the planning request — so the
/// synchronous apply always decides on the current setting, never a
/// pre-request snapshot: while it is off, factory create/update edits
/// refuse with the one disabled message, the same gate the kernel-side
/// factory writers raise.
#[tracing::instrument(
    level = "info",
    name = "refine.apply",
    target = "pa_core::refine",
    skip_all,
    fields(refine.global = options.global)
)]
pub fn apply_refinement_plan(
    state: &mut HarnessState,
    mut plan: RefinementPlan,
    options: &RefineOptions,
    baseline_state: Option<HarnessState>,
    factory_enabled: bool,
) -> RefinementResult {
    let scope = plan.rollback_scope.unwrap_or(if options.global {
        HarnessScope::Global
    } else {
        HarnessScope::Local
    });
    // Server-side fingerprint matcher (`project_trust_loop_fix` option 3):
    // when the refiner omitted per-edit `targetsFingerprints`, deterministically
    // attach fingerprints whose cause the edit's content covers. A rollback
    // proposal restores recorded snapshots, not refiner output, so it is
    // exempt — never add fingerprints a snapshot never carried.
    if plan.rollback_of.is_none() {
        super::fingerprint_matcher::attach_server_side_fingerprints(&mut plan.proposal, state);
    }
    apply_refinement_proposal(
        state,
        &plan.proposal,
        ApplyOptions {
            id: plan.id,
            rollback_of: plan.rollback_of,
            scope: Some(scope),
            baseline_state,
            factory_enabled,
            package_state: options.package_state.clone(),
        },
    )
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct AutoRefineReview {
    pub should_refine: bool,
    pub rationale: String,
    pub instructions: Option<String>,
    /// The reviewer's 0-100 usefulness score for the trajectory; `None`
    /// when the reviewer did not return the field (older prompt builds).
    pub score: Option<u32>,
    /// The passing bar the reviewer held the score to; `None` when absent.
    pub bar: Option<u32>,
    /// The reviewer's whole JSON reply, for a policy that reads more of it
    /// (see [`AutoRefinePolicy`]).
    pub reply: serde_json::Map<String, serde_json::Value>,
}

impl AutoRefineReview {
    /// Human-readable shape for the operator log: `"<verdict> score=N
    /// bar=M — <rationale>"`. Score/bar are omitted when the reviewer did
    /// not supply them (back-compat path).
    #[must_use]
    pub fn summary(&self) -> String {
        let verdict = if self.should_refine {
            "approved"
        } else {
            "declined"
        };
        let mut parts = vec![verdict.to_string()];
        if let Some(score) = self.score {
            parts.push(format!("score={score}"));
        }
        if let Some(bar) = self.bar {
            parts.push(format!("bar={bar}"));
        }
        let head = parts.join(" ");
        if self.rationale.is_empty() {
            head
        } else {
            format!("{head} \u{2014} {}", self.rationale)
        }
    }
}

/// The closing guidance of the native review prompt.
pub const AUTO_REFINE_REVIEW_GUIDANCE: &str = "Return shouldRefine=true when the trajectory contains evidence useful to this session's future turns. Prefer local harness edits for current task progress, temporary blockers, and current-run coordination. Ask for global refinement only for durable cross-session lessons or explicitly project-qualified lessons likely to be reused in future sessions.";

/// The refine an approved automatic review runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRefineRun {
    /// Refine the global store (else the session's local one).
    pub global: bool,
    pub instructions: String,
}

/// How a session's automatic refine reviews ask and what an approval runs
/// (installed through
/// [`crate::features::SessionFeature::auto_refine_policy`]). Every method
/// defaults to the native behaviour, which [`NativeAutoRefinePolicy`] is.
pub trait AutoRefinePolicy: Send + Sync {
    /// The review's system prompt.
    fn review_system_prompt(&self) -> &'static str {
        AUTO_REFINE_REVIEW_SYSTEM_PROMPT
    }

    /// The closing guidance paragraph of the review's prompt.
    fn review_guidance(&self) -> &'static str {
        AUTO_REFINE_REVIEW_GUIDANCE
    }

    /// The refine an approved `review` (triggered by `reason`) runs. The
    /// native policy honors the reviewer's own [`AutoRefineReview::reply`]
    /// `"scope"` field: the review prompt ([`AUTO_REFINE_REVIEW_GUIDANCE`])
    /// asks the model to pick `"local"` (default) or `"global"` (durable
    /// cross-session lesson), and this method routes accordingly.
    ///
    /// Before 2026-11-10 the native policy hardcoded `global: false` and
    /// dropped the reply's scope on the floor, so even a strong global-scope
    /// signal from the reviewer landed as a local refinement and the global
    /// store stopped growing after most sessions. The test stub at
    /// `StubPolicy::approved_refine` already matched the right shape; the
    /// native policy just skipped it. This closure restores the signal.
    fn approved_refine(&self, reason: &str, review: &AutoRefineReview) -> AutoRefineRun {
        let scope = review.reply.get("scope").and_then(serde_json::Value::as_str);
        AutoRefineRun {
            // Only the explicit `"global"` promotes; unknown or `"local"`
            // stays session-scoped. Mirrors the native guidance's defaults.
            global: matches!(scope, Some("global")),
            instructions: native_auto_refine_instructions(reason, review),
        }
    }
}

/// The native review and run.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeAutoRefinePolicy;

impl AutoRefinePolicy for NativeAutoRefinePolicy {}

/// The instructions a natively approved review carries into its local run.
#[must_use]
pub fn native_auto_refine_instructions(reason: &str, review: &AutoRefineReview) -> String {
    let detail = review
        .instructions
        .as_deref()
        .map(|instructions| {
            format!(
                "

Reviewer instructions: {instructions}"
            )
        })
        .unwrap_or_default();
    format!(
        "Automatic refine review triggered by {reason}. Only create/update/delete local harness entries if there is clear evidence that should help this session continue. Prefer an empty edits array over speculative or one-off memories. Do not promote anything global unless explicitly requested. Reviewer rationale: {}{detail}",
        review.rationale
    )
}

pub struct AutoRefineReviewContext {
    pub reason: String,
    pub turns_since_last_review: u32,
}

fn parse_auto_refine_review(text: &str) -> anyhow::Result<AutoRefineReview> {
    let value = match super::planner::extract_json_object(text) {
        Ok(value) => value,
        // A reviewer reply with no JSON object at all (prose only) is not a
        // failure: the reviewer proposed no structured decision, so the safe
        // outcome is "do not refine". Only a truncated or malformed object is
        // a real error worth surfacing to the caller's log.
        Err(error) if error == super::planner::NO_JSON_OBJECT_ERROR => {
            return Ok(AutoRefineReview {
                should_refine: false,
                rationale: "Reviewer replied without a structured decision; skipping refinement."
                    .to_string(),
                instructions: None,
                score: None,
                bar: None,
                reply: serde_json::Map::new(),
            });
        }
        Err(error) => return Err(anyhow::Error::msg(error)),
    };
    let record = value.as_object().cloned().unwrap_or_default();
    Ok(AutoRefineReview {
        should_refine: record.get("shouldRefine") == Some(&serde_json::Value::Bool(true)),
        rationale: record
            .get("rationale")
            .and_then(|value| value.as_str())
            .unwrap_or("No rationale provided.")
            .to_string(),
        instructions: record
            .get("instructions")
            .and_then(|value| value.as_str())
            .map(std::string::ToString::to_string),
        score: parse_u32_in_range(record.get("score")),
        bar: parse_u32_in_range(record.get("bar")),
        reply: record,
    })
}

/// Parse a JSON value as a `u32` in `0..=100`; `None` when absent, the
/// wrong type, out of range, or a float with a fractional part.
fn parse_u32_in_range(value: Option<&serde_json::Value>) -> Option<u32> {
    let value = value?;
    let integer = match value {
        serde_json::Value::Number(number) => number.as_u64().or_else(|| {
            number.as_f64().and_then(|float| {
                if float.is_finite() && float.fract() == 0.0 && float >= 0.0 {
                    Some(float as u64)
                } else {
                    None
                }
            })
        })?,
        serde_json::Value::String(text) => text.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    if integer > 100 {
        return None;
    }
    Some(integer as u32)
}

/// The automatic /refine review gate.
///
/// # Errors
///
/// Error on a failed request build or LLM call, or an unparseable reply.
pub async fn review_auto_refine(
    messages: &[AgentMessage],
    state: &HarnessState,
    history: &[RefinementResult],
    model: &pa_types::ai::Model,
    context: &AutoRefineReviewContext,
    policy: &dyn AutoRefinePolicy,
    review_call: RefinerFn,
) -> anyhow::Result<AutoRefineReview> {
    let system_prompt = policy.review_system_prompt();
    let conversation_text = conversation_text(messages, 40_000);
    // Recurring failures that no existing entry already claims to target
    // (`HarnessEntry.targets_fingerprints`). The refiner reviewer sees the
    // unaddressed backlog, so it can propose a memory that cites a
    // fingerprint id in `targetsFingerprints` and close the loop that
    // `pa-ravo` is penalising proposals against.
    let unaddressed_failures = unaddressed_recurring_for_prompt(state);
    let build_prompt = |conversation: &str| -> String {
        let mut parts = vec![
            format!(
                "<trigger>\n{}; {} assistant turns since last auto-refine review\n</trigger>",
                context.reason, context.turns_since_last_review
            ),
            format!(
                "<current_harness_state>\n{}\n</current_harness_state>",
                overview_for_prompt(state)
            ),
        ];
        if let Some(block) = unaddressed_failures.as_deref() {
            parts.push(format!(
                "<recurring_failures>\n{block}\n</recurring_failures>"
            ));
        }
        parts.push(format!(
            "<refinement_history>\n{}\n</refinement_history>",
            history_for_prompt(history)
        ));
        parts.push(format!("<conversation>\n{conversation}\n</conversation>"));
        parts.push(policy.review_guidance().to_string());
        parts.join("\n\n")
    };
    let (request_max_tokens, user_prompt) = refinement_request(
        model,
        system_prompt,
        &conversation_text,
        &build_prompt,
        AUTO_REFINE_REVIEW_MAX_OUTPUT_TOKENS,
    )?;
    let mut request_model = model.clone();
    request_model.max_tokens = request_max_tokens.min(AUTO_REFINE_REVIEW_MAX_OUTPUT_TOKENS);
    let max_tokens = request_model.max_tokens;
    let reply = review_call(request_model, system_prompt, user_prompt).await?;
    parse_auto_refine_review(&reply_text(
        &reply,
        max_tokens,
        "Auto-refine review failed",
    )?)
}

#[cfg(test)]
mod tests {
    use pa_types::ai::{AssistantContentBlock, TextContent};

    use super::*;

    fn text_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
                rest: serde_json::Map::default(),
            })],
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_types::ai::Usage::default(),
            stop_reason: pa_types::ai::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: serde_json::Map::default(),
            discarded_usage: None,
        }
    }

    fn seam(text: &str) -> RefinerFn {
        let text = text.to_string();
        Box::new(move |_model, _system, _prompt| {
            let text = text;
            Box::pin(async move { Ok(text_message(&text)) })
        })
    }

    #[tokio::test]
    async fn review_and_plan_carry_their_own_system_prompts() {
        let model = test_model();
        // The review seam records its system prompt in a shared cell.
        let review_systems: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let review_recorder = std::sync::Arc::clone(&review_systems);
        let review_call: RefinerFn = Box::new(move |_model, system, _prompt| {
            review_recorder.lock().unwrap().push(system);
            Box::pin(async move {
                Ok(text_message(
                    r#"{"shouldRefine": false, "rationale": "no"}"#,
                ))
            })
        });
        let review = review_auto_refine(
            &[],
            &super::super::empty_harness_state(),
            &[],
            &model,
            &AutoRefineReviewContext {
                reason: "compact".to_string(),
                turns_since_last_review: 0,
            },
            &NativeAutoRefinePolicy,
            review_call,
        )
        .await
        .unwrap();
        assert!(!review.should_refine);
        assert_eq!(
            *review_systems.lock().unwrap(),
            vec![AUTO_REFINE_REVIEW_SYSTEM_PROMPT]
        );
        let plan_systems: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let plan_recorder = std::sync::Arc::clone(&plan_systems);
        let reply = r#"{"summary":"s","edits":[]}"#.to_string();
        let plan_call: RefinerFn = Box::new(move |_model, system, _prompt| {
            plan_recorder.lock().unwrap().push(system);
            let reply = reply;
            Box::pin(async move { Ok(text_message(&reply)) })
        });
        let state = super::super::empty_harness_state();
        plan_refinement(
            &[],
            &state,
            &[],
            &model,
            &RefineOptions::default(),
            plan_call,
        )
        .await
        .unwrap();
        assert_eq!(
            *plan_systems.lock().unwrap(),
            vec![REFINEMENT_SYSTEM_PROMPT]
        );
    }

    /// A cut-off or failed reply reports its stop instead of a JSON parse
    /// error on its partial text.
    #[tokio::test]
    async fn a_cut_off_or_failed_reply_reports_why() {
        let stopped = |stop_reason: StopReason| -> RefinerFn {
            Box::new(move |_model, _system, _prompt| {
                Box::pin(async move {
                    Ok(AssistantMessage {
                        stop_reason,
                        error_message: Some("max_tokens is too large".to_string()),
                        ..text_message(r#"{"summary": "cut"#)
                    })
                })
            })
        };
        let (state, model) = (super::super::empty_harness_state(), test_model());
        let options = RefineOptions::default();
        let plan = plan_refinement(
            &[],
            &state,
            &[],
            &model,
            &options,
            stopped(StopReason::Length),
        );
        let ctx = AutoRefineReviewContext {
            reason: "checkpoint".to_string(),
            turns_since_last_review: 1,
        };
        let review = review_auto_refine(
            &[],
            &state,
            &[],
            &model,
            &ctx,
            &NativeAutoRefinePolicy,
            stopped(StopReason::Error),
        );
        assert_eq!(
            [
                plan.await.err().unwrap().to_string(),
                review.await.err().unwrap().to_string()
            ],
            [
                "Refinement failed: the model hit its output limit (8000 tokens, test/m)",
                "Auto-refine review failed: max_tokens is too large",
            ]
        );
    }

    #[test]
    fn refinement_ids_are_canonical() {
        let id = generate_refinement_id();
        assert!(id.starts_with("refine_"));
        assert!(
            id["refine_".len()..]
                .chars()
                .all(|char| char.is_ascii_digit())
        );
    }

    #[test]
    fn unaddressed_recurring_block_hides_already_targeted_fingerprints() {
        // Build a harness state with two recurring fingerprints and one
        // memory entry that targets the first.
        let mut state = super::super::empty_harness_state();
        let mut covered = entry(RefinementKind::Memory, "covers-ipython", "...");
        covered.targets_fingerprints = vec!["fp-ipython".to_string()];
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(covered.id.clone(), covered);
        let ledger = serde_json::json!({
            "schema": 1,
            "failures": {
                "a": {
                    "fingerprint": {
                        "id": "fp-ipython",
                        "kind": "python_exception",
                        "exceptionClass": "IndexError",
                        "source": "ipython"
                    },
                    "count": 129,
                    "firstSeenTurn": 1,
                    "lastSeenTurn": 1385,
                    "excerpt": "IndexError: list index out of range"
                },
                "b": {
                    "fingerprint": {
                        "id": "fp-other",
                        "kind": "tool_error",
                        "source": "bash"
                    },
                    "count": 5,
                    "firstSeenTurn": 10,
                    "lastSeenTurn": 20,
                    "excerpt": "bash: command not found"
                },
                "c": {
                    "fingerprint": {
                        "id": "fp-noisy",
                        "kind": "provider_error",
                        "source": "anthropic"
                    },
                    "count": 50,
                    "nonActionableCount": 50,
                    "firstSeenTurn": 1,
                    "lastSeenTurn": 300,
                    "excerpt": "fetch failed"
                }
            }
        });
        state.extensions.insert("failures".to_string(), ledger);

        let block = unaddressed_recurring_for_prompt(&state)
            .expect("one unaddressed, actionable fingerprint remains");
        assert!(
            !block.contains("fp-ipython"),
            "a targeted fingerprint must not reappear as an opponent: {block}"
        );
        assert!(
            !block.contains("fp-noisy"),
            "a fully non-actionable fingerprint must not list as an opponent: {block}"
        );
        assert!(
            block.contains("fp-other"),
            "the one open, actionable fingerprint must be listed: {block}"
        );
        assert!(block.contains("count=5"));

        // Covering the remaining fingerprint collapses the block entirely.
        let mut covered_all = entry(RefinementKind::Memory, "covers-bash", "...");
        covered_all.targets_fingerprints = vec!["fp-other".to_string()];
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(covered_all.id.clone(), covered_all);
        assert!(
            unaddressed_recurring_for_prompt(&state).is_none(),
            "every recurring fingerprint covered -> the block is omitted"
        );
    }

    #[test]
    fn a_prose_only_review_reply_is_a_no_op_not_a_failure() {
        // The auto-refine reviewer sometimes answers in prose with no JSON
        // object. That is "no structured decision", so the review must resolve
        // to "do not refine" rather than erroring (which logged an
        // auto-refinement failure for a benign case).
        let review = parse_auto_refine_review("I don't think any refinement is warranted here.")
            .expect("a prose reply is not an error");
        assert!(!review.should_refine);
        assert!(review.instructions.is_none());

        // A genuinely truncated JSON object is still an error worth surfacing.
        let truncated = parse_auto_refine_review("{\"shouldRefine\": true, \"rationale\": \"x\"");
        assert!(truncated.is_err(), "a truncated object stays an error");

        // A well-formed decision still parses.
        let ok = parse_auto_refine_review("{\"shouldRefine\": true, \"rationale\": \"go\"}")
            .expect("valid JSON parses");
        assert!(ok.should_refine);
        assert_eq!(ok.rationale, "go");
    }

    fn entry(kind: RefinementKind, id: &str, content: &str) -> super::super::HarnessEntry {
        super::super::HarnessEntry {
            id: id.to_string(),
            kind,
            title: "Title".to_string(),
            content: content.to_string(),
            path: "general".to_string(),
            scope: Some(HarnessScope::Local),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            targets_fingerprints: Vec::new(),
            lane: crate::refinement::RefinementLane::Lesson,
            extensions: serde_json::Map::new(),
        }
    }

    fn state_with(entries: Vec<super::super::HarnessEntry>) -> HarnessState {
        let mut state = super::super::empty_harness_state();
        for entry in entries {
            state
                .entries
                .get_mut(&entry.kind)
                .unwrap()
                .insert(entry.id.clone(), entry);
        }
        state
    }

    #[test]
    fn the_overview_marks_a_truncated_entry_with_the_hidden_length() {
        // #1317/#1290: the refiner saw 240 chars of a long entry with no
        // marker, so it could not tell a fragment from a whole entry.
        let long = "x".repeat(300);
        let overview = overview_for_prompt(&state_with(vec![
            entry(RefinementKind::Memory, "long", &long),
            entry(RefinementKind::Memory, "short", "whole"),
        ]));
        assert!(
            overview.contains(&format!(
                "- [local:long] Title (general, v1): {}... (+60 chars not shown)",
                "x".repeat(240)
            )),
            "{overview}"
        );
        assert!(
            overview.contains("- [local:short] Title (general, v1): whole\n")
                || overview.ends_with("- [local:short] Title (general, v1): whole"),
            "{overview}"
        );
    }

    #[test]
    fn skill_args_and_ref_snippets_cut_on_char_boundaries() {
        // #1290: the args/ref snippets were byte-sliced at 240, which panics
        // inside a multi-byte character.
        let mut skill = entry(RefinementKind::Skill, "s1", "Run it.");
        skill.arguments.insert(
            "path".to_string(),
            serde_json::json!({ "description": "é".repeat(300) }),
        );
        skill.reference.insert(
            "call_pattern".to_string(),
            serde_json::json!("ü".repeat(300)),
        );
        let overview = overview_for_prompt(&state_with(vec![skill]));
        let args = serde_json::to_string(
            &serde_json::json!({ "path": { "description": "é".repeat(300) } }),
        )
        .unwrap();
        let shown: String = args.chars().take(240).collect();
        let hidden = args.chars().count() - 240;
        assert!(
            overview.contains(&format!(" args={shown}... (+{hidden} chars not shown)")),
            "{overview}"
        );
    }

    #[test]
    fn an_update_to_an_entry_the_refiner_saw_truncated_is_refused() {
        // #1317: the refiner was asked for full replacement content of
        // entries it saw 240 chars of, and its updates destroyed the rest.
        let long = format!("{} tail the refiner never saw", "keep ".repeat(60));
        let mut state = state_with(vec![
            entry(RefinementKind::Memory, "long", &long),
            entry(RefinementKind::Memory, "short", "old"),
        ]);
        let update = |id: &str| super::super::planner::RefinementEdit {
            action: Some(super::super::RefinementAction::Update),
            kind: Some(RefinementKind::Memory),
            id: Some(id.to_string()),
            title: Some("Title".to_string()),
            content: Some("APPEND - prior clauses stand".to_string()),
            ..Default::default()
        };
        let result = apply_refinement_proposal(
            &mut state,
            &RefinementProposal {
                summary: "edit".to_string(),
                edits: vec![update("long"), update("short")],
                ..Default::default()
            },
            ApplyOptions {
                id: "r1".to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert_eq!(
            result
                .applied_edits
                .iter()
                .map(|edit| (edit.id.as_str(), edit.applied, edit.error.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "long",
                    false,
                    Some("entry was truncated in the refiner's view (+86 chars not shown); an update would replace content it never saw".to_string())
                ),
                ("short", true, None),
            ]
        );
        assert_eq!(state.entries[&RefinementKind::Memory]["long"].content, long);
        assert_eq!(
            state.entries[&RefinementKind::Memory]["short"].content,
            "APPEND - prior clauses stand"
        );
    }

    #[test]
    fn overview_and_history_sections() {
        let mut state = super::super::empty_harness_state();
        state
            .entries
            .get_mut(&RefinementKind::Memory)
            .unwrap()
            .insert(
                "m1".to_string(),
                super::super::HarnessEntry {
                    id: "m1".to_string(),
                    kind: RefinementKind::Memory,
                    title: "Fact".to_string(),
                    content: "builds are   green".to_string(),
                    path: "/m/m1".to_string(),
                    scope: Some(HarnessScope::Local),
                    reference: serde_json::Map::default(),
                    arguments: serde_json::Map::default(),
                    metadata: serde_json::Map::default(),
                    source: "test".to_string(),
                    created_at: String::new(),
                    updated_at: String::new(),
                    version: 0,
                    targets_fingerprints: Vec::new(),
                    lane: crate::refinement::RefinementLane::Lesson,
                    extensions: serde_json::Map::new(),
                },
            );
        let overview = overview_for_prompt(&state);
        assert!(overview.contains("memory: 1"));
        assert!(overview.contains("- [local:m1] Fact (/m/m1, v0): builds are green"));
        assert_eq!(history_for_prompt(&[]), "No prior refinement history.");
    }

    #[tokio::test]
    async fn plan_parses_proposal_and_rolls_back() {
        let state = super::super::empty_harness_state();
        let reply = r#"{"summary":"note it","rationale":"seen twice","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m2","title":"Note","content":"value"}]}"#;
        let plan = plan_refinement(
            &[],
            &state,
            &[],
            &test_model(),
            &RefineOptions::default(),
            seam(reply),
        )
        .await
        .unwrap();
        assert_eq!(plan.proposal.summary, "note it");
        assert_eq!(plan.proposal.edits.len(), 1);
        let mut history_state = state.clone();
        let result = apply_refinement_proposal(
            &mut history_state,
            &RefinementProposal {
                summary: "add".to_string(),
                ..Default::default()
            },
            ApplyOptions {
                id: "refine_target".to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        let rollback = plan_refinement(
            &[],
            &state,
            &[result],
            &test_model(),
            &RefineOptions {
                rollback_id: Some("refine_target".to_string()),
                ..Default::default()
            },
            seam("{}"),
        )
        .await
        .unwrap();
        assert_eq!(rollback.rollback_of.as_deref(), Some("refine_target"));
        assert_eq!(rollback.proposal.edits.len(), 0); // the seeded result applied no edits, so nothing inverts
        let missing = plan_refinement(
            &[],
            &state,
            &[],
            &test_model(),
            &RefineOptions {
                rollback_id: Some("nope".to_string()),
                ..Default::default()
            },
            seam("{}"),
        )
        .await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn auto_refine_review_parses_verdict() {
        let state = super::super::empty_harness_state();
        let review = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &AutoRefineReviewContext {
                reason: "checkpoint".to_string(),
                turns_since_last_review: 4,
            },
            &NativeAutoRefinePolicy,
            seam(r#"{"shouldRefine":true,"rationale":"pattern seen","instructions":"note the tactic"}"#),
        )
        .await
        .unwrap();
        assert!(review.should_refine);
        assert_eq!(review.rationale, "pattern seen");
        assert_eq!(review.instructions.as_deref(), Some("note the tactic"));
        let rejected = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &AutoRefineReviewContext {
                reason: "checkpoint".to_string(),
                turns_since_last_review: 1,
            },
            &NativeAutoRefinePolicy,
            seam(r#"{"shouldRefine":false}"#),
        )
        .await
        .unwrap();
        assert!(!rejected.should_refine);
        assert_eq!(rejected.rationale, "No rationale provided.");
        assert_eq!(rejected.instructions, None);
    }

    #[tokio::test]
    async fn auto_refine_review_parses_score_and_bar_and_formats_the_summary() {
        let state = super::super::empty_harness_state();
        let approved = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &AutoRefineReviewContext {
                reason: "checkpoint".to_string(),
                turns_since_last_review: 4,
            },
            &NativeAutoRefinePolicy,
            seam(
                r#"{"shouldRefine":true,"score":82,"bar":70,"rationale":"user confirmed the tactic twice"}"#,
            ),
        )
        .await
        .unwrap();
        assert_eq!(approved.score, Some(82));
        assert_eq!(approved.bar, Some(70));
        assert_eq!(
            approved.summary(),
            "approved score=82 bar=70 \u{2014} user confirmed the tactic twice",
        );
        // An absent score/bar (older reviewer prompt or reply) stays absent;
        // summary degrades to the verdict + rationale only.
        let bare = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &AutoRefineReviewContext {
                reason: "checkpoint".to_string(),
                turns_since_last_review: 1,
            },
            &NativeAutoRefinePolicy,
            seam(r#"{"shouldRefine":false,"rationale":"one-off noise"}"#),
        )
        .await
        .unwrap();
        assert_eq!(bare.score, None);
        assert_eq!(bare.bar, None);
        assert_eq!(bare.summary(), "declined \u{2014} one-off noise");
        // Out-of-range values read as unset; string-encoded integers are
        // accepted (lenient path for reviewers that quote the number).
        let clamped = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &AutoRefineReviewContext {
                reason: "checkpoint".to_string(),
                turns_since_last_review: 1,
            },
            &NativeAutoRefinePolicy,
            seam(r#"{"shouldRefine":false,"score":150,"bar":"60","rationale":"x"}"#),
        )
        .await
        .unwrap();
        assert_eq!(clamped.score, None);
        assert_eq!(clamped.bar, Some(60));
    }

    /// Asks with its own prompt texts and runs every approval globally.
    struct StubPolicy;

    impl AutoRefinePolicy for StubPolicy {
        fn review_system_prompt(&self) -> &'static str {
            "stub review system"
        }

        fn review_guidance(&self) -> &'static str {
            "stub guidance"
        }

        fn approved_refine(&self, reason: &str, review: &AutoRefineReview) -> AutoRefineRun {
            AutoRefineRun {
                global: review.reply.get("scope") != Some(&serde_json::json!("local")),
                instructions: format!("{reason}: {}", review.rationale),
            }
        }
    }

    /// A policy's review asks with its own system prompt and closing
    /// guidance and sees the whole reply; the native policy asks and runs
    /// exactly as before.
    #[tokio::test]
    async fn an_auto_refine_policy_shapes_the_review_and_its_run() {
        let asked: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, String)>>> =
            std::sync::Arc::default();
        let call = |reply: &'static str| -> RefinerFn {
            let asked = std::sync::Arc::clone(&asked);
            Box::new(move |_model, system, prompt| {
                asked.lock().unwrap().push((system, prompt));
                Box::pin(async move { Ok(text_message(reply)) })
            })
        };
        let context = AutoRefineReviewContext {
            reason: "compact".to_string(),
            turns_since_last_review: 2,
        };
        let state = super::super::empty_harness_state();
        let review = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &context,
            &StubPolicy,
            call(r#"{"shouldRefine":true,"rationale":"durable","scope":"local"}"#),
        )
        .await
        .unwrap();
        assert_eq!(review.reply.get("scope"), Some(&serde_json::json!("local")));
        assert_eq!(
            StubPolicy.approved_refine("compact", &review),
            AutoRefineRun {
                global: false,
                instructions: "compact: durable".to_string(),
            }
        );
        let native = review_auto_refine(
            &[],
            &state,
            &[],
            &test_model(),
            &context,
            &NativeAutoRefinePolicy,
            call(r#"{"shouldRefine":true,"rationale":"durable"}"#),
        )
        .await
        .unwrap();
        assert_eq!(
            NativeAutoRefinePolicy.approved_refine("compact", &native),
            AutoRefineRun {
                global: false,
                instructions: native_auto_refine_instructions("compact", &native),
            }
        );
        let asked = asked.lock().unwrap();
        assert_eq!(asked[0].0, "stub review system");
        assert!(asked[0].1.ends_with("\n\nstub guidance"));
        assert_eq!(asked[1].0, AUTO_REFINE_REVIEW_SYSTEM_PROMPT);
        assert!(
            asked[1]
                .1
                .ends_with(&format!("\n\n{AUTO_REFINE_REVIEW_GUIDANCE}"))
        );
    }

    fn test_model() -> pa_types::ai::Model {
        pa_types::ai::Model {
            id: "test".to_string(),
            name: "test".to_string(),
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            base_url: "https://example.invalid".to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pa_types::ai::ModelCost {
                input: 0.0.into(),
                output: 0.0.into(),
                cache_read: 0.0.into(),
                cache_write: 0.0.into(),
            },
            context_window: 100_000,
            max_tokens: 8_000,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }
}
