//! Session-level /refine: message builders, history merge, and the
//! plan -> re-read -> apply -> persist flow.

use std::path::{Path, PathBuf};

use pa_types::ai::{UserContent, UserMessage};
use pa_types::session::{AgentMessage, CustomMessage, FileEntry};
use serde_json::json;

use super::AgentSession;
use super::model_usage::ModelUsage;
use crate::refinement::executor::{
    AutoRefineReview,
    AutoRefineReviewContext,
    RefineOptions as CoreRefineOptions,
    RefinementPlan,
    apply_refinement_plan,
    plan_refinement,
    review_auto_refine,
};
use crate::refinement::gate::{GateAdmission, RefinementGateRequest, RefinementGating};
use crate::refinement::{
    HarnessScope,
    RefinementResult,
    append_global_refinement,
    append_local_refinement,
    format_refinement_notice_body,
    load_global_refinement_history,
    load_harness_state,
    merge_harness_states,
    save_harness_state,
};
use crate::session::manager::SessionManager;

pub const REFINEMENT_AUDIT_CUSTOM_TYPE: &str = "prime-agent.refinement";
pub const REFINEMENT_OUTCOME_CUSTOM_TYPE: &str = "refinement_outcome";
/// Model-facing notice custom type (display=false).
pub const REFINEMENT_NOTICE_CUSTOM_TYPE: &str = "refinement_notice";
/// Review-declined-this-turn custom type: the per-turn memory reviewer
/// chose `should_refine=false`, so no proposal fired. The row carries the
/// rationale tail so the decliner's judgment is auditable (the
/// "decline-visibility gap" in `project_trust_loop_fix.md` — without it,
/// ~90% of turns leave no trace and the loop looks dead).
pub const REFINEMENT_DECLINE_CUSTOM_TYPE: &str = "refinement_decline";

pub const AUTO_REFINE_COMPACT_REASON: &str = "compact";
/// The per-turn memory-extraction trigger reason, carried on every proposal
/// the per-turn cadence fires. Distinct from `AUTO_REFINE_COMPACT_REASON` so
/// the harness history can tell which path admitted an entry, and so RAVO's
/// referee can read the trigger when framing its reasoning. Matches Claude
/// Code's `[extractMemories]` log prefix in intent.
pub const AUTO_REFINE_PER_TURN_REASON: &str = "per_turn";

/// Read the failure ledger off `state.extensions.failures` as raw JSON
/// (`pa-ledger` depends on `pa-core`, not the other way around, so the
/// probe plumbing cannot call the ledger crate's parser directly; it
/// reads the on-disk shape by hand). Each failure record has
/// `lastSeenAt` and `count` at the top level, which is everything the
/// probe needs.
fn run_recurrence_probe(
    state: &crate::refinement::HarnessState,
) -> Vec<crate::refinement::recurrence::RecurrenceHit> {
    let Some(failures) = state
        .extensions
        .get("failures")
        .and_then(serde_json::Value::as_object)
        .and_then(|map| map.get("failures"))
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let owned: std::collections::HashMap<String, crate::refinement::recurrence::FingerprintState> =
        failures
            .iter()
            .filter_map(|(id, raw)| {
                let record = raw.as_object()?;
                let last_seen_at = record
                    .get("lastSeenAt")
                    .and_then(serde_json::Value::as_str)?;
                let count = record
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                Some((
                    id.clone(),
                    crate::refinement::recurrence::FingerprintState {
                        last_seen_at: last_seen_at.to_string(),
                        count,
                    },
                ))
            })
            .collect();
    if owned.is_empty() {
        return Vec::new();
    }
    let observations: crate::refinement::recurrence::FingerprintObservations<'_> = owned
        .iter()
        .map(|(id, state)| (id.as_str(), state.clone()))
        .collect();
    crate::refinement::recurrence::probe_recurrence(state, &observations)
}

/// Record the probe result on the harness state's `pending_reverts`
/// extension when the recurrence-revert flag is on. The plans are
/// deterministic (grouped by the originating refinement result, sorted),
/// so persisting them here lets the next refinement boundary pick them up
/// and apply them if the `revert_apply` flag is on. Pure read-only when
/// no plans result.
fn record_recurrence_revert_plans(
    merged_state: &crate::refinement::HarnessState,
    hits: &[crate::refinement::recurrence::RecurrenceHit],
    history: &[crate::refinement::RefinementResult],
    reason: &str,
) -> Vec<crate::refinement::recurrence::RecurrenceRevertPlan> {
    let plans = crate::refinement::recurrence::plan_recurrence_reverts(hits, history);
    for plan in &plans {
        tracing::info!(
            target: "pa_core::refinement::recurrence",
            reason,
            refinement = %plan.refinement_result_id,
            hits = plan.motivating_hits.len(),
            rationale = %plan.proposal.rationale,
            "recurrence revert plan",
        );
    }
    // The `merged_state` is a read-only merge used for review context, so
    // `pending_reverts` only reaches durable state when the caller (apply
    // path) writes it. Return the plans so the caller can use them.
    let _ = merged_state;
    plans
}

/// Key on `HarnessState.extensions` that carries recurrence-revert plans
/// the probe prepared and the next apply boundary should execute. Shape:
/// `{"plans": [{"refinement_result_id": "...", "proposal": {...}}, ...]}`.
pub const PENDING_REVERTS_KEY: &str = "pending_reverts";

/// Append each new plan to `state.extensions[PENDING_REVERTS_KEY]`
/// (initialising when absent), deduplicating by
/// `refinement_result_id`: a probe re-firing on the same recurrence does
/// not double-queue the same revert. Reverts stay queued until an apply
/// boundary consumes them in [`drain_pending_reverts`].
pub fn stage_pending_reverts(
    state: &mut crate::refinement::HarnessState,
    plans: &[crate::refinement::recurrence::RecurrenceRevertPlan],
) {
    if plans.is_empty() {
        return;
    }
    let existing = state
        .extensions
        .get(PENDING_REVERTS_KEY)
        .and_then(|value| value.get("plans"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen: std::collections::BTreeSet<String> = existing
        .iter()
        .filter_map(|entry| {
            entry
                .get("refinement_result_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let mut merged = existing;
    for plan in plans {
        if seen.insert(plan.refinement_result_id.clone()) {
            let entry = json!({
                "refinement_result_id": plan.refinement_result_id,
                "rationale": plan.proposal.rationale,
                "summary": plan.proposal.summary,
                "motivating_fingerprints": plan
                    .motivating_hits
                    .iter()
                    .map(|hit| hit.fingerprint_id.clone())
                    .collect::<Vec<_>>(),
            });
            merged.push(entry);
        }
    }
    state
        .extensions
        .insert(PENDING_REVERTS_KEY.to_string(), json!({ "plans": merged }));
}

/// Consume `state.extensions[PENDING_REVERTS_KEY]`, returning the ids of
/// originating refinement results to revert. Clears the extension so a
/// crash mid-apply doesn't re-run the same reverts.
pub fn drain_pending_reverts(state: &mut crate::refinement::HarnessState) -> Vec<String> {
    let Some(value) = state.extensions.remove(PENDING_REVERTS_KEY) else {
        return Vec::new();
    };
    let Some(plans) = value.get("plans").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    plans
        .iter()
        .filter_map(|entry| {
            entry
                .get("refinement_result_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Apply every plan the probe staged on `state` via
/// [`crate::refinement::planner::rollback_proposal`]. The caller holds the
/// store lock and will persist `state` once this returns. Draining happens
/// up-front so a partial failure still removes processed ids; the applied
/// reverts are logged under `pa_core::refinement::recurrence`.
fn apply_staged_recurrence_reverts(
    state: &mut crate::refinement::HarnessState,
    global_harness_dir: &Path,
    factory_enabled: bool,
    package_state: Option<std::sync::Arc<crate::refinement::HarnessState>>,
) {
    let ids = drain_pending_reverts(state);
    if ids.is_empty() {
        return;
    }
    // Build the history the planner expects: local + global merged exactly
    // as the review path does.
    let history = load_global_refinement_history(global_harness_dir);
    let options = CoreRefineOptions {
        global: false,
        instructions: None,
        rollback_id: None,
        package_state,
    };
    for id in &ids {
        let Some(target) = history.iter().find(|item| &item.id == id).cloned() else {
            tracing::warn!(
                target: "pa_core::refinement::recurrence",
                refinement = %id,
                "pending recurrence revert refers to a missing refinement result; dropped",
            );
            continue;
        };
        let proposal = crate::refinement::planner::rollback_proposal(&target);
        let scope = crate::refinement::infer_refinement_result_scope(&target)
            .unwrap_or(HarnessScope::Local);
        let plan = RefinementPlan {
            proposal,
            id: format!("revert_{id}"),
            rollback_of: Some(id.clone()),
            rollback_scope: Some(scope),
        };
        let result = apply_refinement_plan(state, plan, &options, None, factory_enabled);
        let applied = result
            .applied_edits
            .iter()
            .filter(|edit| edit.applied)
            .count();
        tracing::info!(
            target: "pa_core::refinement::recurrence",
            refinement = %id,
            applied,
            "applied staged recurrence revert",
        );
    }
}

/// The resolved auto-refine gates: the settings block with the product
/// defaults and clamps applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoRefineGates {
    pub enabled: bool,
    pub turn_interval: u64,
    pub compact: bool,
    pub cooldown_ms: u64,
}

impl Default for AutoRefineGates {
    fn default() -> Self {
        Self {
            enabled: true,
            turn_interval: 25,
            compact: true,
            cooldown_ms: 20 * 60 * 1000,
        }
    }
}

impl AutoRefineGates {
    /// The interval clamps to at least 1.
    #[must_use]
    pub fn from_settings(raw: Option<&crate::settings::AutoRefineSettings>) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        let defaults = Self::default();
        Self {
            enabled: raw.enabled.unwrap_or(defaults.enabled),
            turn_interval: raw.turn_interval.unwrap_or(defaults.turn_interval).max(1),
            compact: raw.compact.unwrap_or(defaults.compact),
            cooldown_ms: raw.cooldown_ms.unwrap_or(defaults.cooldown_ms),
        }
    }
}

/// The instructions a natively approved review carries into the run.
#[must_use]
pub fn auto_refine_instructions(reason: &str, review: &AutoRefineReview) -> String {
    crate::refinement::executor::native_auto_refine_instructions(reason, review)
}

/// Who triggered a refinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementSource {
    Auto,
    User,
    SelfRefine,
}

impl RefinementSource {
    /// The TS source label (`auto`, `user`, `self`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RefinementSource::Auto => "auto",
            RefinementSource::User => "user",
            RefinementSource::SelfRefine => "self",
        }
    }
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[must_use]
pub fn create_refinement_outcome_message(result: &RefinementResult) -> CustomMessage {
    let mut details = json!({
        "refinementId": result.id,
        "summary": result.summary,
        "scope": result.scope.unwrap_or(HarnessScope::Local),
        "edits": result.applied_edits,
    });
    if let (Some(rollback), Some(map)) = (result.rollback_of.clone(), details.as_object_mut()) {
        map.insert("rollbackOf".to_string(), json!(rollback));
    }
    CustomMessage {
        custom_type: REFINEMENT_OUTCOME_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(format!("Refinement complete: {}", result.summary)),
        display: true,
        details: Some(details),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// How much of the reviewer's rationale to show in a decline row's
/// display content. The full rationale is kept in `details.rationale`;
/// the display tail stays short so a per-turn cadence does not swamp
/// the chat scrollback.
pub const REFINEMENT_DECLINE_DISPLAY_CLIP: usize = 240;

/// Review-declined row. Called from the per-turn memory extractor's
/// `Declined` path (`pa-daemon::agent_engine::side_ux.rs`) so the
/// reviewer's judgment is visible in the transcript and auditable via
/// `grep refinement_decline` on the session JSONL.
///
/// `rationale` is the reviewer's own `rationale` field; `reason` is the
/// trigger reason ([`AUTO_REFINE_COMPACT_REASON`] or
/// [`AUTO_REFINE_PER_TURN_REASON`]).
#[must_use]
pub fn create_refinement_decline_message(rationale: &str, reason: &str) -> CustomMessage {
    let trimmed = rationale.trim();
    let tail = clip_decline_display(trimmed);
    let display_body = if tail.is_empty() {
        format!("Refinement declined ({reason})")
    } else {
        format!("Refinement declined ({reason}): {tail}")
    };
    CustomMessage {
        custom_type: REFINEMENT_DECLINE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(display_body),
        display: true,
        details: Some(json!({
            "reason": reason,
            "rationale": trimmed,
        })),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// Grapheme-ish clip of the rationale for the decline row's display
/// line. Keeps the first line only (reviewers sometimes return multi-line
/// rationales) and caps the length at
/// [`REFINEMENT_DECLINE_DISPLAY_CLIP`] chars, trailing `…` on overflow.
fn clip_decline_display(rationale: &str) -> String {
    let first_line = rationale.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= REFINEMENT_DECLINE_DISPLAY_CLIP {
        return first_line.to_string();
    }
    let truncated: String = first_line
        .chars()
        .take(REFINEMENT_DECLINE_DISPLAY_CLIP)
        .collect();
    format!("{truncated}…")
}

/// Model-facing notice (display=false).
#[must_use]
pub fn create_refinement_notice_message(
    result: &RefinementResult,
    source: RefinementSource,
) -> CustomMessage {
    let mut details = json!({
        "refinementId": result.id,
        "summary": result.summary,
        "scope": result.scope.unwrap_or(HarnessScope::Local),
        "edits": result.applied_edits,
        "source": source.as_str(),
    });
    if let (Some(rollback), Some(map)) = (result.rollback_of.clone(), details.as_object_mut()) {
        map.insert("rollbackOf".to_string(), json!(rollback));
    }
    CustomMessage {
        custom_type: REFINEMENT_NOTICE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(format!(
            "[{}-refinement]\n\n{}",
            source.as_str(),
            format_refinement_notice_body(result)
        )),
        display: false,
        details: Some(details),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// This run's live-context rows (the outcome, plus the notice only when any
/// edit applied), selected BY ID so interleaved runs never select each other's rows.
pub(crate) fn context_rows_by_ids(entries: &[FileEntry], ids: &[String]) -> Vec<AgentMessage> {
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.id()?;
            ids.iter()
                .position(|wanted| wanted == id)
                .map(|_| match entry {
                    FileEntry::CustomMessage { payload, .. } => {
                        AgentMessage::Custom(crate::session::create_custom_message(payload, entry))
                    }
                    _ => unreachable!("refinement context rows are custom-message rows"),
                })
        })
        .collect()
}

/// Refinement history recorded in this session's JSONL entries.
#[must_use]
pub fn session_refinement_history(entries: &[FileEntry]) -> Vec<RefinementResult> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::Custom { payload, .. }
                if payload.custom_type == REFINEMENT_AUDIT_CUSTOM_TYPE =>
            {
                payload
                    .data
                    .as_ref()
                    .and_then(|data| serde_json::from_value::<RefinementResult>(data.clone()).ok())
            }
            _ => None,
        })
        .collect()
}

/// Merged cross-session + in-session refinement history.
#[must_use]
pub fn load_refinement_history(
    session: &SessionManager,
    global_harness_dir: &Path,
) -> Vec<RefinementResult> {
    let global = load_global_refinement_history(global_harness_dir);
    let session_entries = session.refinement_history();
    crate::refinement::merge_refinement_history(&global, &session_entries)
}

/// The session's local harness state directory (under the session dir).
///
/// # Panics
///
/// The `expect` cannot fire: the mapping is total for `Some` session dirs.
#[must_use]
pub fn local_harness_state_dir(session: &SessionManager) -> PathBuf {
    let session_dir = session.get_session_dir().to_path_buf();
    crate::refinement::get_local_harness_state_dir(Some(&session_dir))
        .expect("session dir always yields a local harness dir")
}

/// Strip display-only `local:`/`global:`/`package:` prefixes from edit ids.
fn strip_display_prefixes(plan: RefinementPlan) -> RefinementPlan {
    let mut plan = plan;
    for edit in &mut plan.proposal.edits {
        if let Some(id) = &edit.id {
            if let Some(stripped) = id
                .strip_prefix("local:")
                .or_else(|| id.strip_prefix("global:"))
                .or_else(|| id.strip_prefix("package:"))
            {
                edit.id = Some(stripped.to_string());
            }
        }
    }
    plan
}

/// The transcript feeding the refinement planner: the conversation messages
/// plus the in-session refinement history (no owned copy of every entry).
pub struct RefinementTranscript<'a> {
    pub messages: &'a [AgentMessage],
    pub refinement_history: &'a [crate::refinement::RefinementResult],
}

/// Run the full refinement flow: plan (LLM or rollback), re-read the target
/// store, apply, persist state + history, and append the audit, outcome, and
/// notice entries to the session. `refine_call` performs the model request.
///
/// # Errors
///
/// Returns an error when a local refinement is requested on an unpersisted
/// session, when the refinement plan (LLM or rollback) fails, when applying
/// or persisting the refined harness state fails, or when appending the
/// audit, outcome, or notice entries fails.
// One refine funnel: the transcript, the store dirs, the model seam, the
// options, the source, and the resolved gates ride the same call
// (same style as the daemon's too_many_arguments seams).
#[allow(clippy::too_many_arguments)]
pub async fn execute_refinement(
    session: &mut SessionManager,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: crate::refinement::executor::RefinerFn,
    agent_dir: Option<&Path>,
) -> anyhow::Result<RefinementResult> {
    Ok(execute_refinement_with_rows(
        session,
        transcript,
        global_harness_dir,
        model,
        options,
        source,
        refine_call,
        agent_dir,
    )
    .await?
    .0)
}

/// [`execute_refinement`] plus the ids of the live-context rows this
/// run appended, so interleaved runs never select each other's rows.
///
/// # Errors
///
/// Returns the same errors as [`execute_refinement`].
///
/// `agent_dir` is the session's settings root: its settings.json holds
/// the `factory.enabled` opt-in (default off), re-read immediately before
/// the plan applies — after the (long) model request, never snapshotted
/// before it — so the gate decides on the CURRENT setting. While it is
/// off, factory create/update edits refuse with the one disabled message —
/// the same gate the kernel-side factory writers raise
/// (`rlm.factory.require_factory_enabled`), so a refinement cannot
/// author factories the user has not opted into. `None` (a session
/// without a wired agent dir) keeps the fail-closed disabled default.
#[allow(clippy::too_many_arguments)]
pub async fn execute_refinement_with_rows(
    session: &mut SessionManager,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: crate::refinement::executor::RefinerFn,
    agent_dir: Option<&Path>,
) -> anyhow::Result<(RefinementResult, Vec<String>)> {
    execute_refinement_gated(
        session,
        transcript,
        global_harness_dir,
        model,
        options,
        source,
        refine_call,
        agent_dir,
        None,
    )
    .await
}

/// [`execute_refinement_with_rows`] judged by an installed feature's gate
/// (`gating`, see [`crate::refinement::gate`]): a planned, non-empty,
/// non-rollback proposal is evaluated before it applies; a refused one
/// applies nothing and is recorded as its rejected result (audit and
/// outcome rows, and the global history for a global refine), and the
/// verdict records what happened in the state the funnel saves. `None`
/// runs the native, ungated flow.
///
/// # Errors
///
/// Returns the same errors as [`execute_refinement`], and the gate's
/// evaluation error.
#[allow(clippy::too_many_arguments)]
pub async fn execute_refinement_gated(
    session: &mut SessionManager,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: crate::refinement::executor::RefinerFn,
    agent_dir: Option<&Path>,
    gating: Option<RefinementGating>,
) -> anyhow::Result<(RefinementResult, Vec<String>)> {
    // The master switch (upstream #3578): a disabled session plans nothing.
    let settings = match agent_dir {
        Some(dir) => {
            let cwd = session.get_cwd().to_path_buf();
            let dir = dir.to_path_buf();
            Some(
                tokio::task::spawn_blocking(move || {
                    let settings = crate::settings::SettingsManager::create(&cwd, &dir);
                    anyhow::ensure!(
                        super::refinement_gate::enabled(Some(&settings)),
                        "Harness refinements are disabled in /settings"
                    );
                    Ok::<_, anyhow::Error>(settings)
                })
                .await??,
            )
        }
        None => None,
    };
    // Held until this refine's harness write landed or it failed.
    let _refine_guard = gating
        .as_ref()
        .and_then(|gating| gating.gate.begin_refine());
    let RefinementTranscript {
        messages,
        refinement_history,
    } = transcript;
    let local_harness_dir = local_harness_state_dir(session);
    // A pinned plan applies to the scope it was previewed for.
    let pinned = options.pinned_plan.clone();
    let global_scope = pinned
        .as_ref()
        .map_or(options.global, |pinned| pinned.global);
    let core_options = CoreRefineOptions {
        global: global_scope,
        instructions: options.instructions.clone(),
        rollback_id: options.rollback_id.clone(),
        package_state: options.package_state.clone(),
    };
    let requested_scope = if global_scope {
        HarnessScope::Global
    } else {
        HarnessScope::Local
    };
    // The approved plan applies exactly as previewed: no re-plan, and the
    // preview's baseline rejects a store changed since.
    let (plan, planning_state, baseline_state) = if let Some(pinned) = pinned {
        let inputs = refine_planning_inputs(
            &RefinementSessionDirs::of(session),
            refinement_history,
            global_harness_dir,
            &RefineOptions {
                global: global_scope,
                ..RefineOptions::default()
            },
        )?;
        (
            RefinementPlan {
                proposal: pinned.proposal,
                id: pinned.plan_id,
                rollback_of: None,
                rollback_scope: None,
            },
            inputs.planning_state,
            pinned.baseline_state,
        )
    } else {
        let inputs = refine_planning_inputs(
            &RefinementSessionDirs::of(session),
            refinement_history,
            global_harness_dir,
            options,
        )?;
        let plan = plan_refinement(
            messages,
            &inputs.planning_state,
            &inputs.history,
            model,
            &core_options,
            refine_call,
        )
        .await?;
        (
            strip_display_prefixes(plan),
            inputs.planning_state,
            inputs.baseline_state,
        )
    };

    let target_scope = plan.rollback_scope.unwrap_or(requested_scope);
    // Rollbacks are safety actions and an empty proposal is no candidate:
    // neither meets the gate.
    let verdict = match gating {
        Some(gating) if plan.rollback_of.is_none() && !plan.proposal.edits.is_empty() => {
            gating
                .gate
                .evaluate(RefinementGateRequest {
                    proposal_id: plan.id.clone(),
                    proposal: plan.proposal.clone(),
                    scope: target_scope,
                    baseline_state: baseline_state.clone(),
                    planning_state: planning_state.clone(),
                    messages: messages.to_vec(),
                    model: model.clone(),
                    source,
                    trigger: options.trigger.clone(),
                    model_call: gating.model_call,
                })
                .await?
        }
        _ => None,
    };
    let target_dir = match target_scope {
        HarnessScope::Global => global_harness_dir.to_path_buf(),
        HarnessScope::Local => local_harness_dir.clone(),
    };
    // The factory opt-in resolves HERE — after the planning request and
    // before the store lock, so the settings read never lengthens the
    // locked section — so a setting that changed during the
    // request (`/factory off` mid-plan) decides, not a snapshot captured
    // before it. The read rides `spawn_blocking` so the settings I/O
    // never blocks the async runtime worker (the refine arm holds the
    // session lock across this whole call). A session without a wired
    // agent dir keeps the fail-closed disabled default, and a panicked
    // read task reads as disabled — the same fail-closed leniency as
    // `factory_enabled` itself.
    let factory_enabled = match agent_dir {
        Some(agent_dir) => {
            let agent_dir = agent_dir.to_path_buf();
            tokio::task::spawn_blocking(move || crate::refinement::factory_enabled(&agent_dir))
                .await
                .unwrap_or(false)
        }
        None => false,
    };
    // Gap 2 apply half: resolve the revert-apply flag BEFORE moving
    // `settings` into the store-lock task. When on, the probe staged
    // plans on this scope's harness state (see [`stage_pending_reverts`])
    // and the apply draws them down below under the same store lock.
    let revert_apply_on = settings.as_ref().is_some_and(|settings| {
        let experimental = settings.reopen().experimental();
        experimental.auto_refine_recurrence_probe
            && experimental.auto_refine_recurrence_revert
            && experimental.auto_refine_recurrence_revert_apply
    });
    // Every refine holds the store lock from the re-read until the save
    // landed (upstream #3380's `update_harness_state`): the kernel's
    // harness writes and the ledger flush take the same `{file}.lock`.
    let store_lock = {
        let dir = target_dir.clone();
        tokio::task::spawn_blocking(move || {
            let lock = crate::refinement::lock_harness_state(&dir)?;
            // Re-read under the store lock: a switch turned off while the
            // model planned discards the result before anything is saved.
            anyhow::ensure!(
                super::refinement_gate::enabled(settings.as_ref()),
                "Harness refinements were disabled during planning; the result was discarded"
            );
            Ok(lock)
        })
        .await
        .map_err(|error| anyhow::anyhow!("refinement harness lock task failed: {error}"))??
    };
    let mut state = load_harness_state(&target_dir, target_scope);
    if revert_apply_on && target_scope == HarnessScope::Local {
        apply_staged_recurrence_reverts(
            &mut state,
            global_harness_dir,
            factory_enabled,
            core_options.package_state.clone(),
        );
    }
    if let Some(verdict) = &verdict {
        if let GateAdmission::Reject(rejected) = verdict.admit(&plan.proposal, &state) {
            let mut rejected = *rejected;
            if verdict.record_rejection(&mut state) {
                store_lock.ensure_owned()?;
                rejected.harness_state_path = save_harness_state(&target_dir, &state)?
                    .to_string_lossy()
                    .to_string();
            }
            // The history and session rows below are not the store's: release
            // the lock before writing them.
            drop(store_lock);
            return record_rejected_refinement(session, rejected, global_harness_dir, target_scope);
        }
        verdict.prepare_application(&mut state);
    }
    let mut result = apply_refinement_plan(
        &mut state,
        plan,
        &core_options,
        Some(baseline_state),
        factory_enabled,
    );
    if let Some(verdict) = &verdict {
        verdict.record_application(&mut state, &mut result);
    }
    store_lock.ensure_owned()?;
    result.harness_state_path = save_harness_state(&target_dir, &state)?
        .to_string_lossy()
        .to_string();
    drop(store_lock);
    if target_scope == HarnessScope::Global {
        append_global_refinement(global_harness_dir, &result)?;
    } else {
        // Mirror local-scope refinements into the shared harness dir so
        // the retrieval tick + feedback tap can observe them (they only
        // read the shared dir). The authoritative audit still lands in
        // the session jsonl as a `REFINEMENT_AUDIT_CUSTOM_TYPE` row
        // below; this is a tap mirror, not a replacement.
        if let Err(err) = append_local_refinement(global_harness_dir, &result) {
            tracing::warn!(
                target: "pa_core::refinement",
                %err,
                "local-refinement mirror append failed (tap will miss this record)"
            );
        }
    }
    // The audit append is attempted first; a failed write is caught (the row
    // stays live-indexed) and the audit error surfaces only after the outcome
    // records — the user's view and the durable stores do not diverge.
    let (_, audit_write) = session.append_custom_entry_retained(
        REFINEMENT_AUDIT_CUSTOM_TYPE,
        Some(serde_json::to_value(&result)?),
    );
    // Outcome for the TUI; notice for the model (only when edits applied).
    let outcome = create_refinement_outcome_message(&result);
    let (outcome_id, outcome_write) = session.append_custom_message_retained(
        &outcome.custom_type,
        outcome.content.clone(),
        outcome.display,
        outcome.details.clone(),
    );
    let mut context_row_ids = vec![outcome_id];
    if let Some(error) = audit_write {
        anyhow::bail!("refinement audit row not persisted: {error}");
    }
    if let Some(error) = outcome_write {
        anyhow::bail!("refinement outcome row not persisted: {error}");
    }
    if result.applied_edits.iter().any(|edit| edit.applied) {
        let notice = create_refinement_notice_message(&result, source);
        let notice_id = session.append_custom_message(
            &notice.custom_type,
            notice.content.clone(),
            notice.display,
            notice.details.clone(),
        )?;
        context_row_ids.push(notice_id);
    }
    Ok((result, context_row_ids))
}

/// Record a refinement the gate refused: nothing applied, so the audit and
/// outcome rows land (and the global history for a global refine), and no
/// model-facing notice.
fn record_rejected_refinement(
    session: &mut SessionManager,
    rejected: RefinementResult,
    global_harness_dir: &Path,
    target_scope: HarnessScope,
) -> anyhow::Result<(RefinementResult, Vec<String>)> {
    if target_scope == HarnessScope::Global {
        append_global_refinement(global_harness_dir, &rejected)?;
    } else {
        // Mirror local-scope rejections too: the retrieval feedback tap
        // reads from the shared harness dir only. Dropping the mirror
        // means a per-session rejection with failure fingerprints
        // (e.g. `failure:<fp>` in `ravo.missedCriteria`) is invisible
        // to the learning loop — which is exactly the gap that
        // produced zero signals for this user's corpus.
        if let Err(err) = append_local_refinement(global_harness_dir, &rejected) {
            tracing::warn!(
                target: "pa_core::refinement",
                %err,
                "local-rejection mirror append failed"
            );
        }
    }
    let (_, audit_write) = session.append_custom_entry_retained(
        REFINEMENT_AUDIT_CUSTOM_TYPE,
        Some(serde_json::to_value(&rejected)?),
    );
    let outcome = create_refinement_outcome_message(&rejected);
    let (outcome_id, outcome_write) = session.append_custom_message_retained(
        &outcome.custom_type,
        outcome.content.clone(),
        outcome.display,
        outcome.details.clone(),
    );
    if let Some(error) = audit_write {
        anyhow::bail!("refinement audit row not persisted: {error}");
    }
    if let Some(error) = outcome_write {
        anyhow::bail!("refinement outcome row not persisted: {error}");
    }
    Ok((rejected, vec![outcome_id]))
}

/// `/refine` request options (session layer).
#[derive(Debug, Default, Clone)]
pub struct RefineOptions {
    pub global: bool,
    pub instructions: Option<String>,
    pub rollback_id: Option<String>,
    /// Why a feature requested this refine, for the session's gate.
    pub trigger: Option<super::turn_boundary::RefineTrigger>,
    /// A plan approved through `refine.preview` (upstream #899): applied
    /// as previewed instead of re-planning; its scope wins over `global`.
    pub pinned_plan: Option<PreviewedRefinement>,
    /// The session's read-only package overlay (upstream #2298): planning
    /// sees it below the editable entries, the apply refuses to mutate it.
    pub package_state: Option<std::sync::Arc<crate::refinement::HarnessState>>,
}

/// A planned refinement held for approval (`refine.preview`, upstream
/// #899): the proposal exactly as previewed plus the target store's state
/// at planning time, so an apply onto a store that changed since the
/// preview is rejected instead of clobbering it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewedRefinement {
    pub plan_id: String,
    pub global: bool,
    pub instructions: Option<String>,
    pub proposal: crate::refinement::planner::RefinementProposal,
    pub baseline_state: crate::refinement::HarnessState,
}

impl PreviewedRefinement {
    /// The kernel-facing payload (`refine.preview`'s reply): snake-cased,
    /// one row per proposed edit.
    #[must_use]
    pub fn to_payload(&self) -> serde_json::Value {
        let edits: Vec<serde_json::Value> = self
            .proposal
            .edits
            .iter()
            .map(|edit| {
                json!({
                    "action": edit.action,
                    "kind": edit.kind,
                    "id": edit.id,
                    "title": edit.title,
                    "content": edit.content,
                    "reason": edit.reason,
                })
            })
            .collect();
        json!({
            "plan_id": self.plan_id,
            "summary": self.proposal.summary,
            "rationale": self.proposal.rationale,
            "expected_outcome": self.proposal.expected_outcome,
            "scope": if self.global { "global" } else { "local" },
            "edits": edits,
        })
    }
}

/// Plan a refinement without applying it (`refine.preview`): the same
/// planning pass, planning state, and history a `/refine` run uses; nothing
/// is written. The returned plan applies later through
/// [`RefineOptions::pinned_plan`].
///
/// # Errors
///
/// Returns the planning errors of [`execute_refinement`] (a local preview
/// without a session directory, a failed or unparseable planning call).
pub async fn preview_refinement(
    session: RefinementSessionDirs,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    global: bool,
    instructions: Option<String>,
    refine_call: crate::refinement::executor::RefinerFn,
) -> anyhow::Result<PreviewedRefinement> {
    let options = RefineOptions {
        global,
        instructions: instructions.clone(),
        package_state: session.package_state.clone(),
        ..RefineOptions::default()
    };
    let inputs = refine_planning_inputs(
        &session,
        transcript.refinement_history,
        global_harness_dir,
        &options,
    )?;
    let core_options = CoreRefineOptions {
        global,
        instructions: instructions.clone(),
        rollback_id: None,
        package_state: options.package_state.clone(),
    };
    let plan = strip_display_prefixes(
        plan_refinement(
            transcript.messages,
            &inputs.planning_state,
            &inputs.history,
            model,
            &core_options,
            refine_call,
        )
        .await?,
    );
    Ok(PreviewedRefinement {
        plan_id: plan.id,
        global,
        instructions,
        proposal: plan.proposal,
        baseline_state: inputs.baseline_state,
    })
}

/// The session directory facts a refinement plans with, read once so the
/// (slow) planning pass never holds the session lock.
#[derive(Debug, Clone)]
pub struct RefinementSessionDirs {
    pub local_harness_dir: PathBuf,
    pub has_session_dir: bool,
    /// The session's read-only package overlay (upstream #2298) a preview
    /// plans against.
    pub package_state: Option<std::sync::Arc<crate::refinement::HarnessState>>,
}

impl RefinementSessionDirs {
    #[must_use]
    pub fn of(session: &SessionManager) -> Self {
        Self {
            local_harness_dir: local_harness_state_dir(session),
            has_session_dir: session.has_session_dir(),
            package_state: None,
        }
    }
}

/// The planning inputs one refinement reads before its (slow) planning
/// pass: the planning state, the merged history, and the target store's
/// baseline.
struct RefinePlanningInputs {
    planning_state: crate::refinement::HarnessState,
    history: Vec<RefinementResult>,
    baseline_state: crate::refinement::HarnessState,
}

fn refine_planning_inputs(
    session: &RefinementSessionDirs,
    refinement_history: &[RefinementResult],
    global_harness_dir: &Path,
    options: &RefineOptions,
) -> anyhow::Result<RefinePlanningInputs> {
    let local_harness_dir = session.local_harness_dir.clone();
    let requested_scope = if options.global {
        HarnessScope::Global
    } else {
        HarnessScope::Local
    };
    // A local refinement needs the session's own directory: its harness state
    // and artifact paths live there. The daemon's engine session is deliberately
    // non-persisted but carries the session's directory, so local refinement runs.
    if options.rollback_id.is_none()
        && requested_scope == HarnessScope::Local
        && !session.has_session_dir
    {
        anyhow::bail!(
            "Local harness refinement requires a session directory; use global refinement instead."
        );
    }
    // Planning state: global, or merged global+local for local refinements.
    let global_state = load_harness_state(global_harness_dir, HarnessScope::Global);
    let mut planning_state = if requested_scope == HarnessScope::Global {
        global_state
    } else {
        let local_state = load_harness_state(&local_harness_dir, HarnessScope::Local);
        merge_harness_states(&global_state, Some(&local_state))
    };
    // Read-only package overlays plan as context below the editable entries.
    if let Some(package) = &options.package_state {
        crate::refinement::package_harness::overlay_package_harness(&mut planning_state, package);
    }
    let global = load_global_refinement_history(global_harness_dir);
    let history = crate::refinement::merge_refinement_history(&global, refinement_history);
    // Baseline captured before the (slow) LLM pass, so concurrent kernel
    // writes are rejected instead of clobbered.
    let baseline_scope = options
        .rollback_id
        .as_ref()
        .and_then(|id| history.iter().find(|item| &item.id == id))
        .and_then(crate::refinement::infer_refinement_result_scope)
        .unwrap_or(requested_scope);
    let baseline_dir = match baseline_scope {
        HarnessScope::Global => global_harness_dir.to_path_buf(),
        HarnessScope::Local => local_harness_dir,
    };
    let baseline_state = load_harness_state(&baseline_dir, baseline_scope);
    Ok(RefinePlanningInputs {
        planning_state,
        history,
        baseline_state,
    })
}

/// The compact-trigger round's resolution: decline, deferred behind an
/// active agent turn, or ran.
pub(crate) enum AutoRefineRound {
    /// No refinement ran; the caller stamps the review cooldown.
    Declined,
    /// Approved mid-stream: the review is retained and the next serviced
    /// boundary runs it.
    Deferred(AutoRefineReview),
    /// The refinement ran.
    Ran(RefinementResult),
}

/// The per-turn memory-extraction trigger's resolution.
#[derive(Debug, Clone)]
pub enum PerTurnReviewOutcome {
    /// The trigger did not even run the review; see [`PerTurnSkipReason`].
    Skipped(PerTurnSkipReason),
    /// The review ran and said no; the marker has advanced so unchanged
    /// prose will not fire the trigger again. The carried rationale is
    /// the reviewer model's own `rationale` field, so the daemon's
    /// `[extractMemories] review declined` log line names *why* the turn
    /// was skipped instead of just that it was.
    Declined(String),
    /// The review approved: the caller runs the refinement with this
    /// proposal (RAVO still gates it inside the run).
    Approved(AutoRefineReview),
}

/// Why a per-turn memory-extraction trigger was skipped. The daemon turns
/// these into `[extractMemories] skipping — ...` log lines (Claude Code's
/// shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerTurnSkipReason {
    /// `experimental.perTurnMemory` is off (the default).
    Disabled,
    /// The master harness-refinements switch is off; both triggers skip.
    RefinementsDisabled,
    /// No user message since the last extraction advanced the marker.
    NoUserProse,
}

impl PerTurnSkipReason {
    /// Claude Code's `[extractMemories] skipping — …` tail for an
    /// operator-friendly log line.
    #[must_use]
    pub fn log_tail(self) -> &'static str {
        match self {
            Self::Disabled => "experimental.perTurnMemory is off",
            Self::RefinementsDisabled => "harness refinements are disabled",
            Self::NoUserProse => "no user prose since last extraction",
        }
    }
}

/// The per-turn memory-extraction trigger's full round (review + run).
/// The `Ran` variant is boxed so a frequent skip (the default on a cold
/// cadence) does not drag the whole `RefinementResult` payload around.
#[derive(Debug)]
pub enum PerTurnRunOutcome {
    /// The trigger did not even run the review (see [`PerTurnSkipReason`]).
    Skipped(PerTurnSkipReason),
    /// The review ran and said no; the carried rationale is the reviewer
    /// model's own `rationale` field (see
    /// [`PerTurnReviewOutcome::Declined`]).
    Declined(String),
    /// The refinement ran; the result carries the written entries.
    Ran(Box<RefinementResult>),
}

impl AgentSession {
    /// The session's automatic-refine policy (the native one unless a
    /// feature installed its own).
    fn auto_refine_policy(&self) -> &dyn crate::refinement::executor::AutoRefinePolicy {
        self.auto_refine_policy
            .as_deref()
            .unwrap_or(&crate::refinement::executor::NativeAutoRefinePolicy)
    }

    /// The refiner model call on the session model, with the merged request
    /// headers (the Prime team header) the session's own requests send.
    pub(super) async fn refiner_call(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
    ) -> anyhow::Result<crate::refinement::executor::RefinerFn> {
        let (api_key, headers) = match self.auxiliary_model.clone() {
            Some(context) => {
                let model = model.clone();
                let routed = tokio::task::spawn_blocking(move || {
                    super::auxiliary_model::session_fallback_with_headers(&context, &model, api_key)
                })
                .await?;
                (routed.api_key, routed.headers)
            }
            None => (api_key, None),
        };
        Ok(default_refiner_call(
            api_key,
            headers,
            self.session_id().await,
        ))
    }

    /// The automatic-refine review over `review_context`: one reviewer, or
    /// with the multi-vote switch (`experimental.multiVoteRefine` /
    /// `PRIME_AGENT_MULTI_VOTE_REFINE`) on, three parallel reviewers whose
    /// majority verdict wins (Claude Code's extract-memories verification
    /// loop). Every reviewer carries the session's merged request headers
    /// (Prime team billing).
    async fn run_auto_refine_review(
        &self,
        messages: &[AgentMessage],
        merged_state: &crate::refinement::HarnessState,
        history: &[RefinementResult],
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        review_context: &AutoRefineReviewContext,
    ) -> anyhow::Result<AutoRefineReview> {
        // Resolve the merged request headers once, then build a fresh
        // refiner call per reviewer synchronously: the `Fn` bound on
        // `make_reviewer` forbids an async per-call build.
        let (resolved_key, resolved_headers) = match self.auxiliary_model.clone() {
            Some(context) => {
                let model = model.clone();
                let routed = tokio::task::spawn_blocking(move || {
                    super::auxiliary_model::session_fallback_with_headers(&context, &model, api_key)
                })
                .await?;
                (routed.api_key, routed.headers)
            }
            None => (api_key, None),
        };
        let session_id = self.session_id().await;
        let votes = self
            .multi_vote_refine_enabled()
            .then(crate::refinement::multi_vote::MultiVoteConfig::default);
        self.billed_auto_refine_review(
            AutoRefineReviewInputs {
                messages,
                merged_state,
                history,
                model,
                review_context,
            },
            || {
                default_refiner_call(
                    resolved_key.clone(),
                    resolved_headers.clone(),
                    session_id.clone(),
                )
            },
            votes,
        )
        .await
    }

    /// Run the review on reviewers from `make_reviewer`: one, or `votes`
    /// in parallel. Each reviewer's billed call lands as its own
    /// `refine_review` `model_usage` row, a failed review's calls included.
    pub(crate) async fn billed_auto_refine_review<F>(
        &self,
        inputs: AutoRefineReviewInputs<'_>,
        make_reviewer: F,
        votes: Option<crate::refinement::multi_vote::MultiVoteConfig>,
    ) -> anyhow::Result<AutoRefineReview>
    where
        F: Fn() -> crate::refinement::executor::RefinerFn + Send + Sync,
    {
        let bills = std::sync::Mutex::new(Vec::new());
        let billed_reviewer = || {
            let (call, billed) = billed_refiner_call(make_reviewer(), "refine_review");
            bills.lock().expect("reviewer bills lock").push(billed);
            call
        };
        let AutoRefineReviewInputs {
            messages,
            merged_state,
            history,
            model,
            review_context,
        } = inputs;
        let review = match votes {
            Some(config) => {
                crate::refinement::multi_vote::review_auto_refine_voting(
                    messages,
                    merged_state,
                    history,
                    model,
                    review_context,
                    self.auto_refine_policy(),
                    billed_reviewer,
                    config,
                )
                .await
            }
            None => {
                review_auto_refine(
                    messages,
                    merged_state,
                    history,
                    model,
                    review_context,
                    self.auto_refine_policy(),
                    billed_reviewer(),
                )
                .await
            }
        };
        let bills = bills.into_inner().expect("reviewer bills lock");
        for billed in bills {
            let billed = billed.lock().expect("billed refiner call lock").take();
            self.record_model_usage(billed).await;
        }
        review
    }

    /// The compact-trigger review: an LLM call over the conversation, the
    /// merged harness state, and the refinement history. `Ok(None)` is the
    /// decline; `Ok(Some(review))` a fresh approval.
    ///
    /// # Errors
    ///
    /// Returns an error when the conversation history cannot be read or
    /// when the review request fails; a decline is `Ok(None)`.
    ///
    /// # Panics
    ///
    /// Panics if the billed-call slot's mutex is poisoned.
    pub async fn review_compact_auto_refine(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: &Path,
        turns_since_last_review: u32,
        branch_version: u64,
    ) -> anyhow::Result<Option<AutoRefineReview>> {
        if !self.harness_refinements_enabled() {
            return Ok(None);
        }
        // The review reads the same planning inputs the run plans
        // against, extracted under this lock from the retained rows.
        let (parts, merged_state, history) = {
            let session = self.session_handle().lock().await;
            let local_state =
                load_harness_state(&local_harness_state_dir(&session), HarnessScope::Local);
            let global_state = load_harness_state(global_harness_dir, HarnessScope::Global);
            (
                session.refine_transcript_parts(),
                merge_harness_states(&global_state, Some(&local_state)),
                load_global_refinement_history(global_harness_dir),
            )
        };
        // Refinement deliberately reviews historical messages; await the
        // transcript parts after releasing the session mutex.
        let crate::session::manager::RefineTranscriptParts {
            messages,
            refinement_history: session_history,
        } = parts.await?;
        let history = crate::refinement::merge_refinement_history(&history, &session_history);
        // Post-fix recurrence probe (opt-in): scan the failure ledger for
        // any fingerprint an accepted entry claimed to prevent, log each
        // hit at INFO. The probe stays pure read-only: the review that
        // follows is unchanged, and no auto-revert fires from the hits.
        if self.auto_refine_recurrence_probe_enabled() {
            let hits = run_recurrence_probe(&merged_state);
            for hit in &hits {
                tracing::info!(
                    target: "pa_core::refinement::recurrence",
                    entry_id = %hit.entry_id,
                    entry_kind = ?hit.entry_kind,
                    fingerprint_id = %hit.fingerprint_id,
                    last_seen_at = %hit.last_seen_at,
                    total_count = hit.total_count,
                    "recurrence after refinement",
                );
            }
            if self.auto_refine_recurrence_revert_enabled() && !hits.is_empty() {
                let plans = record_recurrence_revert_plans(
                    &merged_state,
                    &hits,
                    &history,
                    AUTO_REFINE_COMPACT_REASON,
                );
                if self.auto_refine_recurrence_revert_apply_enabled() {
                    tracing::info!(
                        target: "pa_core::refinement::recurrence",
                        reason = AUTO_REFINE_COMPACT_REASON,
                        plans = plans.len(),
                        "recurrence revert apply flag is on; plans surface at next refinement boundary",
                    );
                    // Durable record: write the plans to the local harness
                    // state's `pending_reverts` extension so the next
                    // execute_refinement call picks them up under the store
                    // lock. Keeps the probe pure read, with the apply
                    // happening at the already-locked apply boundary.
                    let mut local_state = load_harness_state(
                        &local_harness_state_dir(&*self.session_handle().lock().await),
                        HarnessScope::Local,
                    );
                    stage_pending_reverts(&mut local_state, &plans);
                    if let Err(err) = save_harness_state(
                        &local_harness_state_dir(&*self.session_handle().lock().await),
                        &local_state,
                    ) {
                        tracing::warn!(
                            target: "pa_core::refinement::recurrence",
                            %err,
                            "could not stage pending_reverts to local harness state",
                        );
                    }
                }
            }
        }
        let review_context = AutoRefineReviewContext {
            reason: AUTO_REFINE_COMPACT_REASON.to_string(),
            turns_since_last_review,
        };
        let review = self
            .run_auto_refine_review(
                &messages,
                &merged_state,
                &history,
                model,
                api_key,
                &review_context,
            )
            .await?;
        if !review.should_refine {
            return Ok(None);
        }
        // A branch move (or a replacement teardown's discard) bumped the version
        // while the review's model call was in flight — the approval belongs to
        // the abandoned conversation, so the refinement run never starts.
        if !self.compact_auto_refine_branch_version_unchanged(branch_version) {
            return Ok(None);
        }
        Ok(Some(review))
    }

    /// The per-turn memory-extraction review (Claude Code's
    /// `[extractMemories]` cadence): fires at every settled turn in
    /// addition to compaction when `experimental.perTurnMemory` is on.
    ///
    /// Three skip guards mirror Claude Code's:
    /// 1. `experimental.perTurnMemory` is off (default): decline silently.
    /// 2. The harness refinements master switch is off.
    /// 3. "No new user prose since the last extraction": the session's
    ///    [`AgentSession::per_turn_memory_marker`] matches the current
    ///    user-message count, so nothing fresh is worth reviewing.
    ///
    /// On an active review (`Approved` or `Declined`) the marker advances
    /// to the current user-message count before returning, so the next
    /// per-turn tick after a skipped intermediate turn cannot double-spend
    /// the model call on unchanged prose.
    ///
    /// # Errors
    ///
    /// Returns the underlying transcript-read or review-call error. A
    /// skip is a successful [`PerTurnReviewOutcome::Skipped`], not an
    /// error.
    ///
    /// # Panics
    ///
    /// Panics if the in-memory `per_turn_memory_marker` mutex is
    /// poisoned by a panicking refinement reader; the session is
    /// unusable at that point and a clean restart is the recovery.
    pub async fn review_per_turn_auto_refine(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: &Path,
        user_message_count: u32,
    ) -> anyhow::Result<PerTurnReviewOutcome> {
        if !self.per_turn_memory_enabled() {
            return Ok(PerTurnReviewOutcome::Skipped(PerTurnSkipReason::Disabled));
        }
        if !self.harness_refinements_enabled() {
            return Ok(PerTurnReviewOutcome::Skipped(
                PerTurnSkipReason::RefinementsDisabled,
            ));
        }
        // "No new user prose since last extraction" (Claude Code's
        // `[extractMemories] skipping — no user prose since last
        // extraction`). The marker lives on the session (in-memory, not
        // persisted); a fresh session starts at 0 and the first prompt
        // bumps it above the marker.
        {
            let marker = self
                .per_turn_memory_marker
                .lock()
                .expect("per_turn_memory_marker lock");
            if user_message_count <= *marker {
                return Ok(PerTurnReviewOutcome::Skipped(
                    PerTurnSkipReason::NoUserProse,
                ));
            }
        }
        // The review reads the same planning inputs the compact-trigger
        // review uses; mirrors `review_compact_auto_refine` with a
        // different reason tag so RAVO and the history can tell the
        // paths apart.
        let (parts, merged_state, history) = {
            let session = self.session_handle().lock().await;
            let local_state =
                load_harness_state(&local_harness_state_dir(&session), HarnessScope::Local);
            let global_state = load_harness_state(global_harness_dir, HarnessScope::Global);
            (
                session.refine_transcript_parts(),
                merge_harness_states(&global_state, Some(&local_state)),
                load_global_refinement_history(global_harness_dir),
            )
        };
        let crate::session::manager::RefineTranscriptParts {
            messages,
            refinement_history: session_history,
        } = parts.await?;
        let history = crate::refinement::merge_refinement_history(&history, &session_history);
        if self.auto_refine_recurrence_probe_enabled() {
            let hits = run_recurrence_probe(&merged_state);
            for hit in &hits {
                tracing::info!(
                    target: "pa_core::refinement::recurrence",
                    entry_id = %hit.entry_id,
                    entry_kind = ?hit.entry_kind,
                    fingerprint_id = %hit.fingerprint_id,
                    last_seen_at = %hit.last_seen_at,
                    total_count = hit.total_count,
                    "recurrence after refinement",
                );
            }
            if self.auto_refine_recurrence_revert_enabled() && !hits.is_empty() {
                let plans = record_recurrence_revert_plans(
                    &merged_state,
                    &hits,
                    &history,
                    AUTO_REFINE_PER_TURN_REASON,
                );
                if self.auto_refine_recurrence_revert_apply_enabled() {
                    let mut local_state = load_harness_state(
                        &local_harness_state_dir(&*self.session_handle().lock().await),
                        HarnessScope::Local,
                    );
                    stage_pending_reverts(&mut local_state, &plans);
                    if let Err(err) = save_harness_state(
                        &local_harness_state_dir(&*self.session_handle().lock().await),
                        &local_state,
                    ) {
                        tracing::warn!(
                            target: "pa_core::refinement::recurrence",
                            %err,
                            "could not stage pending_reverts to local harness state",
                        );
                    }
                }
            }
        }
        let review_context = AutoRefineReviewContext {
            reason: AUTO_REFINE_PER_TURN_REASON.to_string(),
            turns_since_last_review: 0,
        };
        let review = self
            .run_auto_refine_review(
                &messages,
                &merged_state,
                &history,
                model,
                api_key,
                &review_context,
            )
            .await?;
        // Advance the marker whether the review approved or declined, so a
        // declined call doesn't fire again against identical prose.
        *self
            .per_turn_memory_marker
            .lock()
            .expect("per_turn_memory_marker lock") = user_message_count;
        Ok(if review.should_refine {
            PerTurnReviewOutcome::Approved(review)
        } else {
            PerTurnReviewOutcome::Declined(review.rationale)
        })
    }

    /// Set the per-turn memory marker to a specific user-message count. A
    /// resumed session calls this with the resumed transcript's user count
    /// so the per-turn trigger cannot re-extract history that preceded the
    /// resume.
    ///
    /// # Panics
    ///
    /// Panics if the in-memory `per_turn_memory_marker` mutex is
    /// poisoned by a panicking refinement reader; the session is
    /// unusable at that point and a clean restart is the recovery.
    pub fn set_per_turn_memory_marker(&self, user_message_count: u32) {
        *self
            .per_turn_memory_marker
            .lock()
            .expect("per_turn_memory_marker lock") = user_message_count;
    }

    /// The serialized arm's compact-trigger round: the review, then only on
    /// approval the refinement run. The serialized boundary is quiescent, so
    /// this arm carries no active-agent gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the review or the refinement run fails; a decline
    /// is `Ok(None)`.
    pub async fn auto_refine_after_compaction(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: std::path::PathBuf,
        turns_since_last_review: u32,
        branch_version: u64,
    ) -> anyhow::Result<Option<RefinementResult>> {
        let Some(review) = self
            .review_compact_auto_refine(
                model,
                api_key.clone(),
                &global_harness_dir,
                turns_since_last_review,
                branch_version,
            )
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(
            self.run_approved_refine(&review, model, api_key, global_harness_dir)
                .await?,
        ))
    }

    /// The per-turn equivalent of [`Self::auto_refine_after_compaction`]:
    /// run the review, and on approval execute the refinement. The
    /// outcome carries the skip reason (if any), decline, or the written
    /// refinement; the daemon turns it into the memory-saved chrome row.
    ///
    /// # Errors
    ///
    /// Returns the underlying review-call or refinement-run error. A skip
    /// or decline is NOT an error.
    pub async fn auto_refine_per_turn(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: std::path::PathBuf,
        user_message_count: u32,
    ) -> anyhow::Result<PerTurnRunOutcome> {
        let review = match self
            .review_per_turn_auto_refine(
                model,
                api_key.clone(),
                &global_harness_dir,
                user_message_count,
            )
            .await?
        {
            PerTurnReviewOutcome::Skipped(reason) => return Ok(PerTurnRunOutcome::Skipped(reason)),
            PerTurnReviewOutcome::Declined(rationale) => {
                return Ok(PerTurnRunOutcome::Declined(rationale));
            }
            PerTurnReviewOutcome::Approved(review) => review,
        };
        let run = self
            .auto_refine_policy()
            .approved_refine(AUTO_REFINE_PER_TURN_REASON, &review);
        let options = RefineOptions {
            global: run.global,
            instructions: Some(run.instructions),
            rollback_id: None,
            trigger: None,
            pinned_plan: None,
            package_state: None,
        };
        let result = self
            .refine(
                &options,
                RefinementSource::Auto,
                model,
                api_key,
                global_harness_dir,
            )
            .await?;
        Ok(PerTurnRunOutcome::Ran(Box::new(result)))
    }

    /// The approved review's refinement run; the result consumes the
    /// review.
    pub(crate) async fn run_approved_refine(
        &self,
        review: &AutoRefineReview,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: std::path::PathBuf,
    ) -> anyhow::Result<RefinementResult> {
        let run = self
            .auto_refine_policy()
            .approved_refine(AUTO_REFINE_COMPACT_REASON, review);
        let options = RefineOptions {
            global: run.global,
            instructions: Some(run.instructions),
            rollback_id: None,
            trigger: None,
            pinned_plan: None,
            package_state: None,
        };
        self.refine(
            &options,
            RefinementSource::Auto,
            model,
            api_key,
            global_harness_dir,
        )
        .await
    }
}

/// The refiner seam plus the slot its reply's billed call lands in. The
/// caller records the call once the refinement released the session lock:
/// the session-file usage store appends through that same lock.
pub(crate) fn billed_refiner_call(
    call: crate::refinement::executor::RefinerFn,
    source: &'static str,
) -> (
    crate::refinement::executor::RefinerFn,
    std::sync::Arc<std::sync::Mutex<Option<ModelUsage>>>,
) {
    let billed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = std::sync::Arc::clone(&billed);
    let seam: crate::refinement::executor::RefinerFn = Box::new(move |model, system, prompt| {
        let reply = call(model, system, prompt);
        Box::pin(async move {
            let reply = reply.await?;
            *slot.lock().expect("billed refiner call lock") = ModelUsage::of_reply(source, &reply);
            Ok(reply)
        })
    });
    (seam, billed)
}

/// What one automatic-refine review reads.
pub(crate) struct AutoRefineReviewInputs<'a> {
    pub(crate) messages: &'a [AgentMessage],
    pub(crate) merged_state: &'a crate::refinement::HarnessState,
    pub(crate) history: &'a [RefinementResult],
    pub(crate) model: &'a pa_types::ai::Model,
    pub(crate) review_context: &'a AutoRefineReviewContext,
}

/// The default model seam over pa-ai completion.
#[must_use]
pub fn default_refiner_call(
    api_key: Option<String>,
    headers: Option<std::collections::BTreeMap<String, String>>,
    session_id: String,
) -> crate::refinement::executor::RefinerFn {
    Box::new(move |model, system_prompt, prompt| {
        Box::pin(async move {
            let context = pa_types::ai::Context {
                system_prompt: Some(system_prompt.to_string()),
                messages: vec![pa_types::ai::Message::User(UserMessage {
                    content: UserContent::Text(prompt),
                    timestamp: 0,
                    rest: serde_json::Map::default(),
                })],
                tools: None,
            };
            let mut stream_options =
                pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
                    api_key,
                    headers: headers.map(|headers| headers.into_iter().collect()),
                    session_id: Some(session_id),
                    // One-off prompts ride SSE: they keep the conversation id (cache key,
                    // opencode routing) but never take or replace the session's Codex
                    // WebSocket and its continuation.
                    transport: Some(pa_ai::types::Transport::Sse),
                    ..Default::default()
                });
            stream_options.reasoning = pa_types::ai::minimum_reasoning(&model);
            Ok(pa_ai::complete_simple(&model, &context, Some(stream_options)).await?)
        })
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pa_types::ai::{AssistantContentBlock, AssistantMessage, Model, StopReason, TextContent};
    use tempfile::TempDir;

    use super::*;
    use crate::refinement::executor::RefinerFn;

    #[test]
    fn run_recurrence_probe_reads_the_failures_extension_and_reports_hits() {
        use crate::refinement::{HarnessEntry, HarnessScope, HarnessState, RefinementKind};
        let entry = HarnessEntry {
            id: "e1".to_string(),
            kind: RefinementKind::Memory,
            title: "N".to_string(),
            content: "C".to_string(),
            path: "general".to_string(),
            scope: Some(HarnessScope::Local),
            reference: serde_json::Map::new(),
            arguments: serde_json::Map::new(),
            metadata: serde_json::Map::new(),
            source: "refine".to_string(),
            created_at: "2026-11-04T00:00:00Z".to_string(),
            updated_at: "2026-11-04T00:00:00Z".to_string(),
            version: 1,
            targets_fingerprints: vec!["fp-a".to_string()],
            lane: crate::refinement::RefinementLane::Lesson,
            extensions: serde_json::Map::new(),
        };
        let mut entries: BTreeMap<RefinementKind, BTreeMap<String, HarnessEntry>> = BTreeMap::new();
        entries
            .entry(RefinementKind::Memory)
            .or_default()
            .insert(entry.id.clone(), entry);
        let mut extensions = serde_json::Map::new();
        extensions.insert(
            "failures".to_string(),
            serde_json::json!({
                "schema": 1,
                "failures": {
                    "fp-a": {
                        "lastSeenAt": "2026-11-05T00:00:00Z",
                        "count": 3,
                    },
                    "fp-unused": {
                        "lastSeenAt": "2026-11-05T00:00:00Z",
                        "count": 1,
                    },
                },
                "lastScannedEntryIndex": 0,
            }),
        );
        let state = HarnessState {
            schema: 1,
            entries,
            refinements: Vec::new(),
            extensions,
        };
        let hits = run_recurrence_probe(&state);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry_id, "e1");
        assert_eq!(hits[0].fingerprint_id, "fp-a");
        assert_eq!(hits[0].total_count, 3);
    }

    /// Gap 2 regression: `stage_pending_reverts` appends plans
    /// deduplicating by `refinement_result_id`, and `drain_pending_reverts`
    /// consumes them and clears the extension so a crash mid-apply does
    /// not re-apply the same reverts.
    #[test]
    fn stage_then_drain_pending_reverts_dedupes_and_clears() {
        use crate::refinement::HarnessState;
        use crate::refinement::recurrence::{RecurrenceHit, RecurrenceRevertPlan};
        let mut state = HarnessState {
            schema: 1,
            entries: BTreeMap::new(),
            refinements: Vec::new(),
            extensions: serde_json::Map::new(),
        };
        let hit = RecurrenceHit {
            entry_id: "e1".into(),
            entry_kind: crate::refinement::RefinementKind::Memory,
            entry_updated_at: "2026-11-04T00:00:00Z".into(),
            fingerprint_id: "fp-a".into(),
            last_seen_at: "2026-11-05T00:00:00Z".into(),
            total_count: 3,
        };
        let plan = RecurrenceRevertPlan {
            refinement_result_id: "r1".into(),
            proposal: crate::refinement::planner::RefinementProposal {
                summary: "s".into(),
                rationale: "rat".into(),
                expected_outcome: "e".into(),
                edits: Vec::new(),
            },
            motivating_hits: vec![hit],
        };
        stage_pending_reverts(&mut state, &[plan.clone()]);
        // Restaging the same plan twice must not duplicate.
        stage_pending_reverts(&mut state, &[plan]);
        let ids = drain_pending_reverts(&mut state);
        assert_eq!(ids, vec!["r1".to_string()]);
        assert!(
            state.extensions.get(PENDING_REVERTS_KEY).is_none(),
            "drain clears the extension",
        );
        // A second drain is an empty no-op (fail-closed).
        assert!(drain_pending_reverts(&mut state).is_empty());
    }

    #[test]
    fn run_recurrence_probe_returns_empty_when_the_failures_key_is_absent() {
        use crate::refinement::HarnessState;
        let state = HarnessState {
            schema: 1,
            entries: BTreeMap::new(),
            refinements: Vec::new(),
            extensions: serde_json::Map::new(),
        };
        assert!(run_recurrence_probe(&state).is_empty());
    }

    fn text_assistant(text: &str) -> AssistantMessage {
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
            stop_reason: StopReason::Stop,
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
            Box::pin(async move { Ok(text_assistant(&text)) })
        })
    }

    /// A refiner seam that rewrites the factory settings file mid-request
    /// (the `/factory` flip during the model request), then returns the
    /// plan authored under the pre-flip value.
    fn flip_seam(settings: &Path, flipped: &'static str, reply: &'static str) -> RefinerFn {
        let settings = settings.to_path_buf();
        Box::new(move |_model, _system, _prompt| {
            std::fs::write(&settings, flipped).unwrap();
            Box::pin(async move { Ok(text_assistant(reply)) })
        })
    }

    fn test_model() -> Model {
        Model {
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

    fn persisted_session(dir: &TempDir) -> SessionManager {
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut session = SessionManager::in_memory(dir.path());
        session.materialize_session_file(Some(session_dir));
        session
    }

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
            rest: serde_json::Map::default(),
        })
    }

    #[tokio::test]
    async fn disabling_refinements_blocks_planning_and_discards_in_flight_results() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let settings = agent_dir.join("settings.json");
        std::fs::write(&settings, r#"{"harnessRefinements":false}"#).unwrap();
        let entries_before = session.retained_entries().to_vec();
        let error = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            Box::new(|_, _, _| panic!("disabled refinement must not call the planner")),
            Some(&agent_dir),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("disabled"));
        std::fs::write(&settings, r#"{"harnessRefinements":true}"#).unwrap();
        let error = execute_refinement(
            &mut session, RefinementTranscript { messages: &[user_message("remember this")], refinement_history: &[] },
            &global_dir, &test_model(), &RefineOptions::default(), RefinementSource::SelfRefine,
            flip_seam(&settings, r#"{"harnessRefinements":false}"#,
                r#"{"summary":"memory","edits":[{"action":"create","kind":"memory","id":"note","title":"Note","content":"Remember this"}]}"#),
            Some(&agent_dir),
        ).await.unwrap_err();
        assert!(error.to_string().contains("discarded"));
        assert_eq!(session.retained_entries(), entries_before.as_slice());
        assert!(
            !local_harness_state_dir(&session)
                .join("harness_state.json")
                .exists()
        );
        assert!(!global_dir.exists());
    }

    #[test]
    fn refinement_messages_match_wire_shape() {
        let result = RefinementResult {
            id: "refine_1".to_string(),
            summary: "add memory".to_string(),
            rationale: "seen twice".to_string(),
            expected_outcome: "recall".to_string(),
            applied_edits: vec![],
            harness_state_path: String::new(),
            rollback_of: None,
            scope: Some(HarnessScope::Local),
            extensions: serde_json::Map::new(),
        };
        let outcome = create_refinement_outcome_message(&result);
        assert_eq!(outcome.custom_type, "refinement_outcome");
        assert_eq!(
            outcome.content,
            UserContent::Text("Refinement complete: add memory".to_string())
        );
        assert!(outcome.display);
        let notice = create_refinement_notice_message(&result, RefinementSource::User);
        assert_eq!(notice.custom_type, "refinement_notice");
        assert!(!notice.display);
        assert_eq!(
            notice.content,
            UserContent::Text("[user-refinement]\n\nadd memory".to_string())
        );
    }

    #[test]
    fn refinement_decline_row_carries_reason_and_rationale_tail() {
        // Short rationale: whole thing fits in the display line.
        let message = create_refinement_decline_message(
            "nothing durable this turn, user was iterating on a layout",
            AUTO_REFINE_PER_TURN_REASON,
        );
        assert_eq!(message.custom_type, "refinement_decline");
        assert!(message.display, "declines must be visible in the TUI");
        assert_eq!(
            message.content,
            UserContent::Text(
                "Refinement declined (per_turn): nothing durable this turn, user was iterating \
                 on a layout"
                    .to_string(),
            )
        );
        let details = message.details.expect("details");
        assert_eq!(
            details.get("reason").and_then(|value| value.as_str()),
            Some(AUTO_REFINE_PER_TURN_REASON),
        );
        assert_eq!(
            details.get("rationale").and_then(|value| value.as_str()),
            Some("nothing durable this turn, user was iterating on a layout"),
        );

        // Multi-line rationale: only the first line shows in the display tail;
        // the full rationale rides in `details.rationale`.
        let multi = create_refinement_decline_message(
            "first-line summary\nsecond-line detail about why not to extract",
            AUTO_REFINE_COMPACT_REASON,
        );
        assert_eq!(
            multi.content,
            UserContent::Text("Refinement declined (compact): first-line summary".to_string()),
        );
        assert_eq!(
            multi
                .details
                .unwrap()
                .get("rationale")
                .and_then(|value| value.as_str()),
            Some("first-line summary\nsecond-line detail about why not to extract"),
        );

        // Overflow rationale: clipped on display, full text preserved in details.
        let long: String = "a".repeat(REFINEMENT_DECLINE_DISPLAY_CLIP + 50);
        let clipped = create_refinement_decline_message(&long, AUTO_REFINE_PER_TURN_REASON);
        let UserContent::Text(ref body) = clipped.content else {
            panic!("expected text content");
        };
        assert!(body.ends_with('\u{2026}'), "ellipsis on overflow");
        assert_eq!(
            clipped
                .details
                .unwrap()
                .get("rationale")
                .and_then(|value| value.as_str())
                .map(str::len),
            Some(long.len()),
            "full rationale preserved for grep audit",
        );

        // Empty rationale: row still emits, just with the bare reason tag.
        let empty = create_refinement_decline_message("", AUTO_REFINE_PER_TURN_REASON);
        assert_eq!(
            empty.content,
            UserContent::Text("Refinement declined (per_turn)".to_string()),
        );
    }

    #[test]
    fn auto_refine_gates_resolve_the_ts_defaults_and_clamps() {
        assert_eq!(
            AutoRefineGates::from_settings(None),
            AutoRefineGates {
                enabled: true,
                turn_interval: 25,
                compact: true,
                cooldown_ms: 20 * 60 * 1000,
            }
        );
        let raw = crate::settings::AutoRefineSettings {
            enabled: Some(false),
            turn_interval: Some(0),
            compact: Some(false),
            cooldown_ms: Some(5),
        };
        assert_eq!(
            AutoRefineGates::from_settings(Some(&raw)),
            AutoRefineGates {
                enabled: false,
                turn_interval: 1,
                compact: false,
                cooldown_ms: 5,
            }
        );
    }

    #[test]
    fn auto_refine_instructions_compose_the_ts_text() {
        let review = AutoRefineReview {
            should_refine: true,
            rationale: "reusable tactic".to_string(),
            instructions: Some("record it".to_string()),
            score: None,
            bar: None,
            reply: serde_json::Map::new(),
        };
        assert_eq!(
            auto_refine_instructions("compact", &review),
            "Automatic refine review triggered by compact. Only create/update/delete local harness entries if there is clear evidence that should help this session continue. Prefer an empty edits array over speculative or one-off memories. Do not promote anything global unless explicitly requested. Reviewer rationale: reusable tactic

Reviewer instructions: record it"
        );
        let bare = AutoRefineReview {
            should_refine: true,
            rationale: "reusable tactic".to_string(),
            instructions: None,
            score: None,
            bar: None,
            reply: serde_json::Map::new(),
        };
        assert_eq!(
            auto_refine_instructions("compact", &bare),
            "Automatic refine review triggered by compact. Only create/update/delete local harness entries if there is clear evidence that should help this session continue. Prefer an empty edits array over speculative or one-off memories. Do not promote anything global unless explicitly requested. Reviewer rationale: reusable tactic"
        );
    }

    #[tokio::test]
    async fn factory_edits_in_a_refinement_respect_the_opt_in_gate() {
        // The host /refine flow funnels factory create edits through the
        // same opt-in gate as the kernel writers: while `factory.enabled`
        // is off, the edit refuses with the one exact disabled message
        // and nothing persists; enabled, the same proposal applies.
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"sweep","edits":[{"action":"create","kind":"factory","id":"sweep","title":"Factory","content":"Sweep review.","arguments":{"machine":{"states":[{"id":"collect","entry":true,"subagent":"worker"}],"transitions":[]}}}]}"#;
        let disabled = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(reply),
            None,
        )
        .await
        .unwrap();
        assert!(!disabled.applied_edits[0].applied);
        assert_eq!(
            disabled.applied_edits[0].error.as_deref(),
            Some(crate::refinement::FACTORY_DISABLED_MESSAGE)
        );
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Factory].is_empty());
        // The user opts in: the same proposal applies, read live from the
        // agent dir's settings.json.
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join(crate::refinement::FACTORY_SETTINGS_FILE_NAME),
            r#"{"factory": {"enabled": true}}"#,
        )
        .unwrap();
        let enabled = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(reply),
            Some(&agent_dir),
        )
        .await
        .unwrap();
        assert!(enabled.applied_edits[0].applied);
        assert!(enabled.applied_edits[0].error.is_none());
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Factory].contains_key("sweep"));
    }

    #[tokio::test]
    async fn factory_gate_decides_on_the_apply_time_setting_not_a_planning_snapshot() {
        // The opt-in re-reads immediately before the apply — after the
        // planning request — so a `/factory` flip during the model request
        // decides, not the value the plan was authored under: a plan
        // authored while enabled that lands after the setting turned off
        // refuses with the one exact disabled message and nothing
        // persists, and a plan authored while disabled that lands after
        // the opt-in applies.
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let settings = agent_dir.join(crate::refinement::FACTORY_SETTINGS_FILE_NAME);
        let reply = r#"{"summary":"sweep","edits":[{"action":"create","kind":"factory","id":"sweep","title":"Factory","content":"Sweep review.","arguments":{"machine":{"states":[{"id":"collect","entry":true,"subagent":"worker"}],"transitions":[]}}}]}"#;
        // Authored while enabled; the request flips the setting off.
        std::fs::write(&settings, r#"{"factory": {"enabled": true}}"#).unwrap();
        let turned_off = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            flip_seam(&settings, r#"{"factory": {"enabled": false}}"#, reply),
            Some(&agent_dir),
        )
        .await
        .unwrap();
        assert!(!turned_off.applied_edits[0].applied);
        assert_eq!(
            turned_off.applied_edits[0].error.as_deref(),
            Some(crate::refinement::FACTORY_DISABLED_MESSAGE)
        );
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Factory].is_empty());
        // Authored while disabled; the request opts in.
        std::fs::write(&settings, r#"{"factory": {"enabled": false}}"#).unwrap();
        let turned_on = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            flip_seam(&settings, r#"{"factory": {"enabled": true}}"#, reply),
            Some(&agent_dir),
        )
        .await
        .unwrap();
        assert!(turned_on.applied_edits[0].applied);
        assert!(turned_on.applied_edits[0].error.is_none());
    }

    #[tokio::test]
    async fn execute_refinement_persists_state_and_entries() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        session
            .append_message(user_message("do a thing twice"))
            .unwrap();
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;
        let result = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(reply),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.applied_edits.len(), 1);
        assert!(result.applied_edits[0].applied);
        let state_path = Path::new(&result.harness_state_path);
        assert!(state_path.exists());
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Memory].contains_key("m1"));
        let entries = session.get_all_entries().to_vec();
        assert_eq!(session_refinement_history(&entries).len(), 1);
        let custom_messages: Vec<&FileEntry> = entries
            .iter()
            .filter(|entry| {
                matches!(entry, FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == REFINEMENT_OUTCOME_CUSTOM_TYPE
                        || payload.custom_type == REFINEMENT_NOTICE_CUSTOM_TYPE)
            })
            .collect();
        assert_eq!(custom_messages.len(), 2);
        assert_eq!(load_refinement_history(&session, &global_dir).len(), 1);
    }

    /// A refiner seam that must never run (a pinned plan never re-plans).
    fn no_replan() -> RefinerFn {
        Box::new(|_model, _system, _prompt| {
            Box::pin(async { anyhow::bail!("a pinned plan must not re-plan") })
        })
    }

    /// Upstream #899: `refine.preview` plans without writing anything, and the
    /// approved plan applies exactly as previewed (no second planning call).
    #[tokio::test]
    async fn a_previewed_plan_applies_exactly_without_replanning() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"local:m1","title":"Tactic","content":"Use tactic A","reason":"seen twice"}]}"#;
        let preview = preview_refinement(
            RefinementSessionDirs::of(&session),
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            false,
            Some("note the tactic".to_string()),
            seam(reply),
        )
        .await
        .unwrap();
        let mut payload = preview.to_payload();
        let plan_id = payload["plan_id"].as_str().unwrap().to_string();
        assert!(plan_id.starts_with("refine_"), "{plan_id}");
        payload["plan_id"] = json!("<id>");
        assert_eq!(
            payload,
            json!({
                "plan_id": "<id>",
                "summary": "note it",
                "rationale": "repeated",
                "expected_outcome": "recall",
                "scope": "local",
                "edits": [{
                    "action": "create", "kind": "memory", "id": "m1",
                    "title": "Tactic", "content": "Use tactic A", "reason": "seen twice"
                }]
            })
        );
        // Nothing was written by the preview.
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        assert!(
            load_harness_state(&harness_dir, HarnessScope::Local)
                .entries
                .values()
                .all(BTreeMap::is_empty)
        );

        let result = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions {
                pinned_plan: Some(preview),
                ..RefineOptions::default()
            },
            RefinementSource::SelfRefine,
            no_replan(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.id, plan_id);
        assert_eq!(result.applied_edits.len(), 1);
        assert!(result.applied_edits[0].applied);
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert_eq!(
            state.entries[&crate::refinement::RefinementKind::Memory]["m1"].content,
            "Use tactic A"
        );
    }

    /// A store edited between the preview and the apply is not clobbered:
    /// the preview's baseline rejects the stale edit.
    #[tokio::test]
    async fn a_previewed_plan_is_rejected_when_the_store_changed_since() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let first = r#"{"summary":"v1","edits":[{"action":"create","kind":"memory","id":"m1","title":"T","content":"one"}]}"#;
        execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("x")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(first),
            None,
        )
        .await
        .unwrap();
        let update = r#"{"summary":"v2","edits":[{"action":"update","kind":"memory","id":"m1","title":"T","content":"two"}]}"#;
        let preview = preview_refinement(
            RefinementSessionDirs::of(&session),
            RefinementTranscript {
                messages: &[user_message("x")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            false,
            None,
            seam(update),
        )
        .await
        .unwrap();
        // The store moves on after the preview.
        let concurrent = r#"{"summary":"v3","edits":[{"action":"update","kind":"memory","id":"m1","title":"T","content":"three"}]}"#;
        execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("x")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(concurrent),
            None,
        )
        .await
        .unwrap();
        let result = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("x")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions {
                pinned_plan: Some(preview),
                ..RefineOptions::default()
            },
            RefinementSource::SelfRefine,
            no_replan(),
            None,
        )
        .await
        .unwrap();
        assert!(
            !result.applied_edits[0].applied,
            "{:?}",
            result.applied_edits
        );
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert_eq!(
            state.entries[&crate::refinement::RefinementKind::Memory]["m1"].content,
            "three"
        );
    }

    /// The failed rows stay live-indexed so the in-process history sees
    /// the refinement.
    #[tokio::test]
    async fn audit_write_failure_reports_after_durable_edits() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        // Bootstrap the flush rule: rows only persist after the first
        // assistant entry (persist_entry defers them until then).
        session
            .append_message(AgentMessage::Assistant(text_assistant("seed")))
            .unwrap();
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"lesson","edits":[{"action":"create","kind":"memory","id":"m9","title":"Lesson","content":"durable"}]}"#;
        // Fail every session-file write: the path becomes a directory (the
        // harness stores live under a sibling dir and stay writable).
        let file = session.get_session_file().unwrap().to_path_buf();
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        let error = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("x")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(reply),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("audit row not persisted"),
            "the audit error surfaces after the durable writes: {error:#}"
        );
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Memory].contains_key("m9"));
        // The notice is ordered after the audit rethrow.
        let entries = session.get_all_entries().to_vec();
        assert_eq!(session_refinement_history(&entries).len(), 1);
        assert!(
            !entries.iter().any(
                |entry| matches!(entry, FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == REFINEMENT_NOTICE_CUSTOM_TYPE)
            ),
            "the notice is ordered after the audit rethrow in TS"
        );
    }

    fn oracle_fixture() -> String {
        let mut rows = vec![
            serde_json::json!({"type":"session","id":"s","version":3,"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}),
        ];
        let mut parent: Option<String> = None;
        for i in 0..5 {
            let id = format!("u{i}");
            rows.push(serde_json::json!({"type":"message","id":id.clone(),"parentId":parent,"message":{"role":"user","content":format!("hello {i}"),"timestamp":0}}));
            parent = Some(id);
        }
        rows.push(serde_json::json!({"type":"custom","id":"audit","parentId":parent,"customType":"prime-agent.refinement","data":{"id":"refine_0","summary":"seed","rationale":"seeded rationale","expectedOutcome":"seeded outcome","appliedEdits":[],"harnessStatePath":""}}));
        rows.into_iter().map(|row| row.to_string() + "\n").collect()
    }

    /// One captured refiner request.
    #[derive(Debug, Clone, PartialEq)]
    struct CapturedRequest {
        max_tokens: u64,
        system: &'static str,
        user: String,
    }

    /// Byte-identical across every extraction path; the leased-window leg
    /// deletes the file first, so a hidden fallback fails.
    #[tokio::test]
    async fn refine_request_is_identical_across_extraction_paths() {
        use std::io::Write as _;
        let body = oracle_fixture();
        let mut captured: Vec<CapturedRequest> = Vec::new();
        for leg in ["full-reader", "windowed-leased", "windowed-unleased"] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("session.jsonl");
            std::fs::write(&path, &body).unwrap();
            let mut session = if leg == "full-reader" {
                SessionManager::open(dir.path(), dir.path(), &path)
            } else {
                SessionManager::open_windowed(dir.path(), dir.path(), &path)
                    .await
                    .unwrap()
            };
            if leg == "windowed-leased" {
                session.set_append_ownership(
                    crate::session::window::AppendOwnership::SessionLeaseHeld,
                );
                // The served-path proof: removing the file cannot fail
                // the shared-window parts.
                std::fs::remove_file(&path).unwrap();
                let probe = session.refine_transcript_parts();
                probe.await.unwrap();
                std::fs::write(&path, &body).unwrap();
            }
            let global_harness_dir = dir.path().join("harness");
            let parts = session.refine_transcript_parts();
            let crate::session::manager::RefineTranscriptParts {
                messages,
                refinement_history,
            } = parts.await.unwrap();
            let captured_requests: std::sync::Arc<std::sync::Mutex<Vec<CapturedRequest>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = std::sync::Arc::clone(&captured_requests);
            let reply = r#"{"summary":"bench","edits":[]}"#.to_string();
            let result = execute_refinement(
                &mut session,
                RefinementTranscript {
                    messages: &messages,
                    refinement_history: &refinement_history,
                },
                &global_harness_dir,
                &test_model(),
                &RefineOptions::default(),
                RefinementSource::User,
                Box::new(move |model, system, prompt| {
                    sink.lock().unwrap().push(CapturedRequest {
                        max_tokens: model.max_tokens,
                        system,
                        user: prompt,
                    });
                    let reply = reply;
                    Box::pin(async move { Ok(text_assistant(&reply)) })
                }),
                None,
            )
            .await
            .unwrap_or_else(|error| panic!("'{leg}' leg failed: {error:#}"));
            assert!(result.applied_edits.is_empty());
            let got = captured_requests.lock().unwrap().clone();
            assert_eq!(got.len(), 1, "'{leg}' leg: exactly one refiner call");
            captured.push(got[0].clone());
        }
        for (a, b) in captured.iter().zip(captured.iter().skip(1)) {
            assert_eq!(
                a, b,
                "the refiner request diverged between extraction paths (the frozen surface)"
            );
        }
        assert!(
            captured[0].user.contains("[refine_0] seed"),
            "the fixture's audit history must appear in the prompt"
        );
        assert!(
            captured[0].user.contains("hello 4"),
            "the fixture's conversation must appear in the prompt"
        );

        // The read-leg proof: the unleased window must serve an
        // out-of-band audit row.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, &body).unwrap();
        let session = SessionManager::open_windowed(dir.path(), dir.path(), &path)
            .await
            .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(
            br#"{"type":"custom","id":"oob-audit","parentId":"audit","customType":"prime-agent.refinement","data":{"id":"refine_oob","summary":"oob","rationale":"r","expectedOutcome":"o","appliedEdits":[],"harnessStatePath":""}}"#
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        let parts = session.refine_transcript_parts();
        let crate::session::manager::RefineTranscriptParts {
            refinement_history, ..
        } = parts.await.unwrap();
        assert!(
            refinement_history
                .iter()
                .any(|item| item.id == "refine_oob"),
            "the unleased window must keep re-reading the file (out-of-band audit visible)"
        );
    }

    #[tokio::test]
    async fn global_refinement_appends_history() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let reply = r#"{"summary":"global lesson","edits":[{"action":"create","kind":"memory","id":"g1","title":"Lesson","content":"durable"}]}"#;
        let result = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("x")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions {
                global: true,
                ..Default::default()
            },
            RefinementSource::SelfRefine,
            seam(reply),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.scope, Some(HarnessScope::Global));
        let global_state = load_harness_state(&global_dir, HarnessScope::Global);
        assert!(
            global_state.entries[&crate::refinement::RefinementKind::Memory].contains_key("g1")
        );
        let history = load_global_refinement_history(&global_dir);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, result.id);
        let rolled = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions {
                global: true,
                rollback_id: Some(result.id.clone()),
                ..Default::default()
            },
            RefinementSource::User,
            seam("unused"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(rolled.rollback_of.as_deref(), Some(result.id.as_str()));
        let global_state = load_harness_state(&global_dir, HarnessScope::Global);
        assert!(
            !global_state.entries[&crate::refinement::RefinementKind::Memory].contains_key("g1")
        );
    }

    /// A stub gate: refuses or admits every proposal, records a `stub` key
    /// in the state it saves and a `stubReport` key on the result, and
    /// counts the refines it was held across.
    struct StubGate {
        admit: bool,
        held: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        evaluated: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct StubVerdict {
        admit: bool,
        judged: String,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct StubHold(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for StubHold {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::refinement::gate::RefinementGate for StubGate {
        fn begin_refine(&self) -> Option<crate::refinement::gate::RefineGuard> {
            self.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(Box::new(StubHold(std::sync::Arc::clone(&self.held))))
        }

        fn evaluate(
            &self,
            request: RefinementGateRequest,
        ) -> crate::features::FeatureFuture<
            anyhow::Result<Option<Box<dyn crate::refinement::gate::RefinementGateVerdict>>>,
        > {
            let admit = self.admit;
            let evaluated = std::sync::Arc::clone(&self.evaluated);
            let held = self.held.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let reply = (request.model_call)(
                    request.model.clone(),
                    "judge",
                    request.proposal.summary.clone(),
                )
                .await?;
                let judged = reply
                    .content
                    .iter()
                    .find_map(|block| match block {
                        pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                evaluated.lock().unwrap().push(format!(
                    "{} {} {:?} held={held}",
                    request.proposal_id.starts_with("refine_"),
                    request.source.as_str(),
                    request.scope
                ));
                let log = std::sync::Arc::clone(&evaluated);
                Ok(Some(Box::new(StubVerdict { admit, judged, log })
                    as Box<dyn crate::refinement::gate::RefinementGateVerdict>))
            })
        }
    }

    impl crate::refinement::gate::RefinementGateVerdict for StubVerdict {
        fn admit(
            &self,
            proposal: &crate::refinement::planner::RefinementProposal,
            _current: &crate::refinement::HarnessState,
        ) -> GateAdmission {
            if self.admit {
                return GateAdmission::Apply;
            }
            let mut extensions = serde_json::Map::new();
            extensions.insert("stubReport".to_string(), json!(self.judged));
            GateAdmission::Reject(Box::new(RefinementResult {
                id: "refused".to_string(),
                summary: format!("refused: {}", proposal.summary),
                rationale: proposal.rationale.clone(),
                expected_outcome: proposal.expected_outcome.clone(),
                applied_edits: vec![],
                harness_state_path: String::new(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                extensions,
            }))
        }

        fn record_rejection(&self, state: &mut crate::refinement::HarnessState) -> bool {
            state
                .extensions
                .insert("stub".to_string(), json!({ "rejected": 1 }));
            true
        }

        fn prepare_application(&self, state: &mut crate::refinement::HarnessState) {
            self.log.lock().unwrap().push("prepare".to_string());
            state
                .extensions
                .insert("stubPrepared".to_string(), json!(true));
        }

        fn record_application(
            &self,
            state: &mut crate::refinement::HarnessState,
            result: &mut RefinementResult,
        ) {
            self.log.lock().unwrap().push("apply".to_string());
            state
                .extensions
                .insert("stub".to_string(), json!({ "applied": 1 }));
            result
                .extensions
                .insert("stubReport".to_string(), json!(self.judged));
        }
    }

    fn stub_gating(
        admit: bool,
    ) -> (
        RefinementGating,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let held = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let evaluated = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let gating = RefinementGating {
            gate: std::sync::Arc::new(StubGate {
                admit,
                held: std::sync::Arc::clone(&held),
                evaluated: std::sync::Arc::clone(&evaluated),
            }),
            model_call: seam("judged ok"),
        };
        (gating, held, evaluated)
    }

    const MEMORY_REPLY: &str = r#"{"summary":"note it","rationale":"repeated","expectedOutcome":"recall","edits":[{"action":"create","kind":"memory","id":"m1","title":"Tactic","content":"Use tactic A"}]}"#;

    fn custom_types(session: &SessionManager) -> Vec<String> {
        session
            .get_all_entries()
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::CustomMessage { payload, .. } => Some(payload.custom_type.clone()),
                FileEntry::Custom { payload, .. } => Some(payload.custom_type.clone()),
                _ => None,
            })
            .collect()
    }

    /// A gate that refuses a proposal: nothing applies, the gate's own
    /// result is what the session records (audit and outcome rows, no
    /// model-facing notice), and the state the verdict updated is saved.
    #[tokio::test]
    async fn a_refusing_gate_applies_nothing_and_records_its_result() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        session.append_message(user_message("seed")).unwrap();
        let global_dir = dir.path().join("harness");
        let (gating, held, evaluated) = stub_gating(false);
        let (result, rows) = execute_refinement_gated(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(MEMORY_REPLY),
            None,
            Some(gating),
        )
        .await
        .unwrap();
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let mut extensions = serde_json::Map::new();
        extensions.insert("stubReport".to_string(), json!("judged ok"));
        assert_eq!(
            result,
            RefinementResult {
                id: "refused".to_string(),
                summary: "refused: note it".to_string(),
                rationale: "repeated".to_string(),
                expected_outcome: "recall".to_string(),
                applied_edits: vec![],
                harness_state_path: crate::refinement::get_harness_state_path(&harness_dir)
                    .to_string_lossy()
                    .to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                extensions,
            }
        );
        assert_eq!(rows.len(), 1);
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Memory].is_empty());
        assert_eq!(
            state.extensions.get("stub"),
            Some(&json!({ "rejected": 1 }))
        );
        assert_eq!(
            custom_types(&session),
            [REFINEMENT_AUDIT_CUSTOM_TYPE, REFINEMENT_OUTCOME_CUSTOM_TYPE]
        );
        assert_eq!(
            evaluated.lock().unwrap().clone(),
            ["true user Local held=1"]
        );
        assert_eq!(held.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// An admitting gate: the verdict prepares the re-read store, the edits
    /// apply, and the verdict's records land in the saved state and on the
    /// recorded result.
    #[tokio::test]
    async fn an_admitting_gate_applies_and_records_into_state_and_result() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        session.append_message(user_message("seed")).unwrap();
        let global_dir = dir.path().join("harness");
        let (gating, held, evaluated) = stub_gating(true);
        let (result, _) = execute_refinement_gated(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::Auto,
            seam(MEMORY_REPLY),
            None,
            Some(gating),
        )
        .await
        .unwrap();
        assert!(result.applied_edits[0].applied);
        assert_eq!(
            result.extensions.get("stubReport"),
            Some(&json!("judged ok"))
        );
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let state = load_harness_state(&harness_dir, HarnessScope::Local);
        assert!(state.entries[&crate::refinement::RefinementKind::Memory].contains_key("m1"));
        assert_eq!(state.extensions.get("stub"), Some(&json!({ "applied": 1 })));
        assert_eq!(state.extensions.get("stubPrepared"), Some(&json!(true)));
        assert_eq!(
            custom_types(&session),
            [
                REFINEMENT_AUDIT_CUSTOM_TYPE,
                REFINEMENT_OUTCOME_CUSTOM_TYPE,
                REFINEMENT_NOTICE_CUSTOM_TYPE
            ]
        );
        assert_eq!(
            evaluated.lock().unwrap().clone(),
            ["true auto Local held=1", "prepare", "apply"]
        );
        assert_eq!(held.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Every refine, gated or not, writes under the harness state file's
    /// lock (upstream #3380): while another live writer holds it, the
    /// refine waits for the holder and then re-reads, so it keeps what the
    /// holder saved instead of racing or overwriting it.
    #[tokio::test]
    async fn a_refine_writes_only_under_the_harness_store_lock() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        session.append_message(user_message("seed")).unwrap();
        let global_dir = dir.path().join("harness");
        let harness_dir =
            crate::refinement::get_local_harness_state_dir(Some(session.get_session_dir()))
                .unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = {
            let harness_dir = harness_dir.clone();
            std::thread::spawn(move || {
                let held = crate::refinement::lock_harness_state(&harness_dir).unwrap();
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
                let mut state = crate::refinement::empty_harness_state();
                state
                    .extensions
                    .insert("holderMark".to_string(), json!(true));
                crate::refinement::save_harness_state(&harness_dir, &state).unwrap();
                drop(held);
            })
        };
        held_rx.recv().unwrap();
        let result = execute_refinement(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("do a thing twice")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(MEMORY_REPLY),
            None,
        )
        .await
        .unwrap();
        holder.join().unwrap();
        assert!(result.applied_edits[0].applied);
        let saved =
            std::fs::read_to_string(crate::refinement::get_harness_state_path(&harness_dir))
                .unwrap();
        assert!(
            saved.contains("holderMark") && saved.contains("Use tactic A"),
            "the refine re-read after the holder's save and kept it: {saved}"
        );
        assert!(
            !crate::platform::LockDir::path_for(&crate::refinement::get_harness_state_path(
                &harness_dir
            ))
            .exists()
        );
    }

    /// An empty proposal is no candidate: the gate never evaluates it.
    #[tokio::test]
    async fn an_empty_proposal_never_meets_the_gate() {
        let dir = TempDir::new().unwrap();
        let mut session = persisted_session(&dir);
        let global_dir = dir.path().join("harness");
        let (gating, _, evaluated) = stub_gating(false);
        let (result, _) = execute_refinement_gated(
            &mut session,
            RefinementTranscript {
                messages: &[user_message("nothing to learn")],
                refinement_history: &[],
            },
            &global_dir,
            &test_model(),
            &RefineOptions::default(),
            RefinementSource::User,
            seam(r#"{"summary":"nothing","edits":[]}"#),
            None,
            Some(gating),
        )
        .await
        .unwrap();
        assert_eq!(result.summary, "nothing");
        // Not evaluated: the gate saw nothing (the store lock is the
        // refine's own `lock_harness_state`, not a gate seam).
        assert!(evaluated.lock().unwrap().is_empty());
    }
}
