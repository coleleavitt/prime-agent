//! One refinement run off the session's turn path: planning holds no session
//! lock, and the commit applies at a full-run idle boundary
//! ([`pa_agent::agent::Agent::reserve_idle`]). A background run is cancelled
//! with its session (disposal, branch navigation); a commit already writing
//! durable state finishes its audit and context append.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pa_types::session::{AgentMessage, CustomMessage};

use super::{
    CommittedRefinement,
    REFINEMENT_AUDIT_CUSTOM_TYPE,
    RefineOptions,
    RefinePlanningInputs,
    RefinementSessionDirs,
    RefinementSource,
    RefinementTranscript,
    apply_staged_recurrence_reverts,
    context_rows_by_ids,
    create_refinement_notice_message,
    create_refinement_outcome_message,
    refine_planning_inputs,
    strip_display_prefixes,
};
use crate::refinement::executor::{
    RefineOptions as CoreRefineOptions,
    RefinementPlan,
    RefinerFn,
    apply_refinement_plan,
    plan_refinement,
};
use crate::refinement::gate::{
    GateAdmission,
    RefineGuard,
    RefinementGateRequest,
    RefinementGateVerdict,
    RefinementGating,
};
use crate::refinement::lock::{HARNESS_STATE_LOCK, HarnessLockWait, try_lock_harness_state_file};
use crate::refinement::{
    HarnessScope,
    HarnessState,
    RefinementResult,
    append_global_refinement,
    append_local_refinement,
    get_harness_state_path,
    load_harness_state,
    save_harness_state,
};
use crate::session::manager::SessionManager;
use crate::session_engine::model_usage::ModelUsage;

/// Run the full refinement flow: plan (LLM or rollback), re-read the target
/// store, apply, persist state + history, and append the audit, outcome, and
/// notice entries to the session and the agent's live context.
/// `refine_call` performs the model request.
///
/// `agent_dir` is the session's settings root: its settings.json holds
/// the `factory.enabled` opt-in (default off), re-read after the (long)
/// model request, never snapshotted before it, so the gate decides on the
/// CURRENT setting. While it is off, factory create/update edits refuse
/// with the one disabled message — the same gate the kernel-side factory
/// writers raise (`rlm.factory.require_factory_enabled`), so a refinement
/// cannot author factories the user has not opted into. `None` (a session
/// without a wired agent dir) keeps the fail-closed disabled default.
///
/// # Errors
///
/// Returns an error when a local refinement is requested on an unpersisted
/// session, when the refinement plan (LLM or rollback) fails, when applying
/// or persisting the refined harness state fails, or when appending the
/// global history, audit, outcome, or notice records fails (after the
/// applied state and the live context already took the result).
// One refine funnel: the transcript, the store dirs, the model seam, the
// options, the source, and the resolved gates ride the same call
// (same style as the daemon's too_many_arguments seams).
#[allow(clippy::too_many_arguments)]
pub async fn execute_refinement(
    session: &tokio::sync::Mutex<SessionManager>,
    agent: &pa_agent::agent::Agent,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: RefinerFn,
    agent_dir: Option<&Path>,
) -> anyhow::Result<RefinementResult> {
    Ok(execute_refinement_gated(
        session,
        agent,
        transcript,
        global_harness_dir,
        model,
        options,
        source,
        refine_call,
        agent_dir,
        None,
    )
    .await?
    .result)
}

/// [`execute_refinement`] judged by an installed feature's gate (`gating`,
/// see [`crate::refinement::gate`]): a planned, non-empty, non-rollback
/// proposal is evaluated before it applies; a refused one applies nothing
/// and is recorded as its rejected result (audit and outcome rows, and the
/// global history for a global refine), and the verdict records what
/// happened in the state the funnel saves. `None` runs the native, ungated
/// flow. Returns the result and the live-context rows the commit appended.
///
/// # Errors
///
/// Returns the same errors as [`execute_refinement`], and the gate's
/// evaluation error.
#[allow(clippy::too_many_arguments)]
pub async fn execute_refinement_gated(
    session: &tokio::sync::Mutex<SessionManager>,
    agent: &pa_agent::agent::Agent,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: RefinerFn,
    agent_dir: Option<&Path>,
    gating: Option<RefinementGating>,
) -> anyhow::Result<CommittedRefinement> {
    // Boxed: the planning and commit futures are large, and every caller
    // awaits this one.
    let ready = Box::pin(prepare_refinement(
        session,
        agent_dir,
        transcript,
        global_harness_dir,
        model,
        options,
        source,
        refine_call,
        gating,
    ))
    .await?;
    Box::pin(commit_refinement(
        session,
        agent,
        ready,
        source,
        agent_dir,
        /*compaction_flight*/ None,
        &pa_agent::abort::AbortSignal::never(),
    ))
    .await
}

/// Counts the refinement runs currently planning or committing — the
/// counter `refine.status` answers `in_flight` from. The guard spans a
/// run's whole body, so every source reports its flight at the one
/// shared start.
struct RefinementFlightGuard(Arc<AtomicUsize>);

impl RefinementFlightGuard {
    fn enter(in_flight: Arc<AtomicUsize>) -> Self {
        in_flight.fetch_add(1, Ordering::AcqRel);
        Self(in_flight)
    }
}

impl Drop for RefinementFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a refinement run needs from its session, owned so the run can live
/// in a background task: the stores, the agent whose idle boundary the
/// commit reserves, the session's cancellation, and the model-call seams.
#[derive(Clone)]
pub(crate) struct RefinementRuntime {
    pub(crate) session: Arc<tokio::sync::Mutex<SessionManager>>,
    pub(crate) agent: Arc<pa_agent::agent::Agent>,
    pub(crate) compaction_flight: Arc<tokio::sync::Mutex<()>>,
    pub(crate) refinements_in_flight: Arc<AtomicUsize>,
    pub(crate) cancel: pa_agent::abort::AbortSignal,
    pub(crate) agent_dir: Option<PathBuf>,
    pub(crate) auxiliary_model:
        Option<crate::session_engine::auxiliary_model::AuxiliaryModelContext>,
    pub(crate) usage_store: Arc<dyn crate::session_engine::rlm_usage::RlmChildUsageStore>,
    pub(crate) refinement_gate: Option<Arc<dyn crate::refinement::gate::RefinementGate>>,
    pub(crate) auto_refine_policy: Option<Arc<dyn crate::refinement::executor::AutoRefinePolicy>>,
    /// The session's read-only package overlay (upstream #2298): every
    /// refine plans with it unless its options carry their own.
    pub(crate) package_state: Option<Arc<HarnessState>>,
    /// The compact-trigger state whose branch version fences a review.
    pub(crate) compact_auto_refine:
        Arc<std::sync::Mutex<crate::session_engine::auto_refine_trigger::CompactAutoRefineState>>,
    pub(crate) review_switches: super::ReviewSwitches,
}

impl RefinementRuntime {
    /// The refiner model call on the session model, with the merged request
    /// headers (the Prime team header) the session's own requests send.
    pub(crate) async fn refiner_call(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
    ) -> anyhow::Result<RefinerFn> {
        let (api_key, headers) = self.request_auth(model, api_key).await?;
        let session_id = self.session.lock().await.get_session_id().to_string();
        Ok(super::default_refiner_call(api_key, headers, session_id))
    }

    /// The key and merged request headers a refiner call sends: the
    /// auxiliary-model routing's, else the session's own key.
    pub(crate) async fn request_auth(
        &self,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
    ) -> anyhow::Result<(
        Option<String>,
        Option<std::collections::BTreeMap<String, String>>,
    )> {
        Ok(match self.auxiliary_model.clone() {
            Some(context) => {
                let model = model.clone();
                let routed = tokio::task::spawn_blocking(move || {
                    crate::session_engine::auxiliary_model::session_fallback_with_headers(
                        &context, &model, api_key,
                    )
                })
                .await?;
                (routed.api_key, routed.headers)
            }
            None => (api_key, None),
        })
    }

    /// `call` billed under `source` at reply time (see [`recorded_refiner_call`]).
    pub(crate) fn recorded(&self, call: RefinerFn, source: &'static str) -> RefinerFn {
        recorded_refiner_call(
            call,
            source,
            Arc::clone(&self.session),
            Arc::clone(&self.usage_store),
        )
    }

    /// One refinement on the session model: the planning call, and the gate
    /// call an installed refinement gate may make, both billed.
    ///
    /// # Errors
    ///
    /// Returns the run's failure (see [`run_refinement`]).
    pub(crate) async fn run(
        self,
        options: RefineOptions,
        source: RefinementSource,
        model: pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: PathBuf,
    ) -> anyhow::Result<CommittedRefinement> {
        let refiner = self.refiner_call(&model, api_key.clone()).await?;
        let gate_call = match self.refinement_gate {
            Some(_) => Some(self.refiner_call(&model, api_key).await?),
            None => None,
        };
        self.run_with_calls(
            options,
            source,
            model,
            refiner,
            gate_call,
            global_harness_dir,
        )
        .await
    }

    /// [`Self::run`] with injected model calls: `refine_call` plans, and
    /// `gate_call` is the one call the session's refinement gate (when a
    /// feature installed one) may make; without it the gate is not
    /// consulted.
    ///
    /// # Errors
    ///
    /// Returns the run's failure (see [`run_refinement`]).
    pub(crate) async fn run_with_calls(
        self,
        mut options: RefineOptions,
        source: RefinementSource,
        model: pa_types::ai::Model,
        refine_call: RefinerFn,
        gate_call: Option<RefinerFn>,
        global_harness_dir: PathBuf,
    ) -> anyhow::Result<CommittedRefinement> {
        if options.package_state.is_none() {
            options.package_state.clone_from(&self.package_state);
        }
        let refine_call = self.recorded(refine_call, "refine");
        // The gate's judge call bills like the planning call.
        let gating = match (&self.refinement_gate, gate_call) {
            (Some(gate), Some(model_call)) => Some(RefinementGating {
                gate: Arc::clone(gate),
                model_call: self.recorded(model_call, "refine_gate"),
            }),
            _ => None,
        };
        run_refinement(
            &self,
            options,
            source,
            model,
            refine_call,
            gating,
            global_harness_dir,
        )
        .await
    }
}

/// One refinement run: planning (cancellable, no session lock held), then
/// the commit at a full-run idle boundary.
pub(crate) async fn run_refinement(
    runtime: &RefinementRuntime,
    options: RefineOptions,
    source: RefinementSource,
    model: pa_types::ai::Model,
    refine_call: RefinerFn,
    gating: Option<RefinementGating>,
    global_harness_dir: PathBuf,
) -> anyhow::Result<CommittedRefinement> {
    let _flight = RefinementFlightGuard::enter(Arc::clone(&runtime.refinements_in_flight));
    let ready = pa_agent::abort::race_with_abort(
        async {
            // The transcript's consumed artifacts are extracted under this first
            // lock straight from the retained rows: no second clone of the rows.
            let parts = runtime.session.lock().await.refine_transcript_parts();
            let crate::session::manager::RefineTranscriptParts {
                messages,
                refinement_history,
            } = parts.await?;
            prepare_refinement(
                &runtime.session,
                runtime.agent_dir.as_deref(),
                RefinementTranscript {
                    messages: &messages,
                    refinement_history: &refinement_history,
                },
                &global_harness_dir,
                &model,
                &options,
                source,
                refine_call,
                gating,
            )
            .await
        },
        &runtime.cancel,
    )
    .await??;
    commit_refinement(
        &runtime.session,
        &runtime.agent,
        ready,
        source,
        runtime.agent_dir.as_deref(),
        Some(&runtime.compaction_flight),
        &runtime.cancel,
    )
    .await
}

/// A planned refinement waiting for its commit: no persistence, session
/// lock, or run reservation is held while it waits.
pub(crate) struct ReadyRefinement {
    plan: RefinementPlan,
    core_options: CoreRefineOptions,
    baseline_state: HarnessState,
    target_scope: HarnessScope,
    target_dir: PathBuf,
    global_harness_dir: PathBuf,
    settings: Option<crate::settings::SettingsManager>,
    /// The gate's verdict on the plan, consulted under the store lock.
    verdict: Option<Box<dyn RefinementGateVerdict>>,
    /// The staged recurrence reverts apply with this commit.
    revert_apply_on: bool,
    /// Held until this refine's harness write landed or it failed.
    _refine_guard: Option<RefineGuard>,
}

/// Plan one refinement: the master gate, the planning (or the pinned
/// plan), and the gate's evaluation. Session reads take the lock briefly;
/// the slow model calls hold nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_refinement(
    session: &tokio::sync::Mutex<SessionManager>,
    agent_dir: Option<&Path>,
    transcript: RefinementTranscript<'_>,
    global_harness_dir: &Path,
    model: &pa_types::ai::Model,
    options: &RefineOptions,
    source: RefinementSource,
    refine_call: RefinerFn,
    gating: Option<RefinementGating>,
) -> anyhow::Result<ReadyRefinement> {
    let (session_dirs, cwd) = {
        let session = session.lock().await;
        (
            RefinementSessionDirs::of(&session),
            session.get_cwd().to_path_buf(),
        )
    };
    // The master switch (upstream #3578): a disabled session plans nothing.
    // The one settings construction rides the ready refinement to the commit
    // recheck; `enabled` reopens the store live on each read.
    let settings = match agent_dir.map(Path::to_path_buf) {
        Some(dir) => Some(
            tokio::task::spawn_blocking(move || {
                let settings = crate::settings::SettingsManager::create(&cwd, &dir);
                anyhow::ensure!(
                    crate::session_engine::refinement_gate::enabled(Some(&settings)),
                    "Harness refinements are disabled in /settings"
                );
                Ok::<_, anyhow::Error>(settings)
            })
            .await??,
        ),
        None => None,
    };
    let refine_guard = gating
        .as_ref()
        .and_then(|gating| gating.gate.begin_refine());
    let RefinementTranscript {
        messages,
        refinement_history,
    } = transcript;
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
        let RefinePlanningInputs { planning_state, .. } = refine_planning_inputs(
            &session_dirs,
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
            planning_state,
            pinned.baseline_state,
        )
    } else {
        let inputs = refine_planning_inputs(
            &session_dirs,
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
                    planning_state,
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
        HarnessScope::Local => session_dirs.local_harness_dir,
    };
    // The probe staged revert plans on this scope's harness state (see
    // `stage_pending_reverts`); the commit draws them down under the same
    // store lock when the apply flag is on.
    let revert_apply_on = settings.as_ref().is_some_and(|settings| {
        let experimental = settings.reopen().experimental();
        experimental.auto_refine_recurrence_probe
            && experimental.auto_refine_recurrence_revert
            && experimental.auto_refine_recurrence_revert_apply
    });
    Ok(ReadyRefinement {
        plan,
        core_options,
        baseline_state,
        target_scope,
        target_dir,
        global_harness_dir: global_harness_dir.to_path_buf(),
        settings,
        verdict,
        revert_apply_on,
        _refine_guard: refine_guard,
    })
}

type RefinementCommitGuards<'a> = (
    pa_agent::agent::IdleReservation<'a>,
    Option<tokio::sync::MutexGuard<'a, ()>>,
    tokio::sync::MutexGuard<'a, SessionManager>,
    crate::platform::HeartbeatLock,
);

/// Reserve the commit: the agent's idle boundary, the compaction flight
/// (every live-context replacer takes it), the session lock, and the target
/// store's lock. A contended lock releases everything taken before it, so
/// a turn, a compaction, or a kernel harness write is never held up behind
/// a waiting commit.
async fn reserve_refinement_commit<'a>(
    session: &'a tokio::sync::Mutex<SessionManager>,
    agent: &'a pa_agent::agent::Agent,
    target_dir: &Path,
    compaction_flight: Option<&'a tokio::sync::Mutex<()>>,
) -> anyhow::Result<RefinementCommitGuards<'a>> {
    let state_path = get_harness_state_path(target_dir);
    let mut store_wait = HarnessLockWait::new(&state_path, HARNESS_STATE_LOCK);
    loop {
        let admission = agent.reserve_idle().await;
        let flight = match compaction_flight {
            Some(flight) => {
                if let Ok(guard) = flight.try_lock() {
                    Some(guard)
                } else {
                    drop(admission);
                    drop(flight.lock().await);
                    continue;
                }
            }
            None => None,
        };
        let Ok(session_guard) = session.try_lock() else {
            drop(flight);
            drop(admission);
            drop(session.lock().await);
            continue;
        };
        let path = state_path.clone();
        let lock = tokio::task::spawn_blocking(move || {
            try_lock_harness_state_file(&path, HARNESS_STATE_LOCK)
        })
        .await?;
        match lock {
            Ok(lock) => return Ok((admission, flight, session_guard, lock)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                drop(session_guard);
                drop(flight);
                drop(admission);
                store_wait.contended()?;
                tokio::time::sleep(HARNESS_STATE_LOCK.retry).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// What the store write decided.
enum StoreOutcome {
    Applied(RefinementResult),
    /// The gate refused: nothing applied.
    Rejected(RefinementResult),
}

/// Commit a planned refinement at a full-run idle boundary: apply it to the
/// re-read store, record the history and the session rows, and append this
/// run's live-context rows to the agent.
pub(crate) async fn commit_refinement(
    session: &tokio::sync::Mutex<SessionManager>,
    agent: &pa_agent::agent::Agent,
    ready: ReadyRefinement,
    source: RefinementSource,
    agent_dir: Option<&Path>,
    compaction_flight: Option<&tokio::sync::Mutex<()>>,
    cancel: &pa_agent::abort::AbortSignal,
) -> anyhow::Result<CommittedRefinement> {
    let ReadyRefinement {
        plan,
        core_options,
        baseline_state,
        target_scope,
        target_dir,
        global_harness_dir,
        settings,
        verdict,
        revert_apply_on,
        _refine_guard,
    } = ready;
    // The factory opt-in resolves HERE — after the planning request and
    // before the commit's locks, so the settings read never lengthens the
    // locked section — so a setting that changed during the request
    // (`/factory off` mid-plan) decides, not a snapshot captured before
    // it. A session without a wired agent dir keeps the fail-closed
    // disabled default, and a panicked read task reads as disabled — the
    // same fail-closed leniency as `factory_enabled` itself.
    let factory_enabled = match agent_dir {
        Some(agent_dir) => {
            let agent_dir = agent_dir.to_path_buf();
            tokio::task::spawn_blocking(move || crate::refinement::factory_enabled(&agent_dir))
                .await
                .unwrap_or(false)
        }
        None => false,
    };
    let (_admission, _flight, mut session, store_lock) = pa_agent::abort::race_with_abort(
        reserve_refinement_commit(session, agent, &target_dir, compaction_flight),
        cancel,
    )
    .await??;
    pa_agent::abort::throw_if_aborted(cancel)?;
    // From this point the commit completes even if the session closes:
    // dropping planning is safe, dropping between the durable save and the
    // audit append is not.
    let store_global_dir = global_harness_dir.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        // The apply-time recheck: a toggle-off during the (slow) planning
        // request discards the plan here — after the lock, before any load,
        // apply, or save, so nothing persists and no rows append.
        anyhow::ensure!(
            crate::session_engine::refinement_gate::enabled(settings.as_ref()),
            "Harness refinements were disabled during planning; the result was discarded"
        );
        let mut state = load_harness_state(&target_dir, target_scope);
        if revert_apply_on && target_scope == HarnessScope::Local {
            apply_staged_recurrence_reverts(
                &mut state,
                &store_global_dir,
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
                return Ok(StoreOutcome::Rejected(rejected));
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
        Ok::<_, anyhow::Error>(StoreOutcome::Applied(result))
    })
    .await
    .map_err(|error| anyhow::anyhow!("refinement harness update task failed: {error}"))??;
    let (result, applied) = match outcome {
        StoreOutcome::Applied(result) => (result, true),
        StoreOutcome::Rejected(result) => (result, false),
    };
    let history_write = record_history(&global_harness_dir, target_scope, &result);
    // The audit append is attempted first; a failed write is caught (the row
    // stays live-indexed) and its error surfaces only after the outcome
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
    let mut notice_write = None;
    if applied && result.applied_edits.iter().any(|edit| edit.applied) {
        let notice = create_refinement_notice_message(&result, source);
        let (notice_id, error) = session.append_custom_message_retained(
            &notice.custom_type,
            notice.content.clone(),
            notice.display,
            notice.details.clone(),
        );
        notice_write = error;
        context_row_ids.push(notice_id);
    }
    let rows: Vec<CustomMessage> =
        context_rows_by_ids(session.retained_entries(), &context_row_ids);
    // The rows push onto the live context, never a full rebuild — a rebuild
    // would resurrect a retried turn's dropped trailing assistant.
    let loop_rows = rows
        .iter()
        .filter_map(|row| {
            crate::session_engine::session_message_to_loop(&AgentMessage::Custom(row.clone()))
        })
        .collect();
    agent.append_messages(loop_rows).await;
    let failures: Vec<String> = [
        ("global history", history_write),
        ("audit", audit_write.map(|error| error.to_string())),
        ("outcome", outcome_write.map(|error| error.to_string())),
        ("notice", notice_write.map(|error| error.to_string())),
    ]
    .into_iter()
    .filter_map(|(store, error)| error.map(|error| format!("{store}: {error}")))
    .collect();
    if !failures.is_empty() {
        anyhow::bail!(
            "refinement {} but records were not persisted: {}",
            if applied { "applied" } else { "was rejected" },
            failures.join("; ")
        );
    }
    Ok(CommittedRefinement { result, rows })
}

/// Record the result in the refinement history: the global history for a
/// global refine; a local one (applied or rejected) mirrors into the shared
/// harness dir so the retrieval tick and feedback tap observe it (they only
/// read the shared dir). The authoritative audit still lands in the session
/// jsonl; a failed mirror only logs. Returns the global history's write error.
fn record_history(
    global_harness_dir: &Path,
    target_scope: HarnessScope,
    result: &RefinementResult,
) -> Option<String> {
    match target_scope {
        HarnessScope::Global => append_global_refinement(global_harness_dir, result)
            .err()
            .map(|error| error.to_string()),
        HarnessScope::Local => {
            if let Err(err) = append_local_refinement(global_harness_dir, result) {
                tracing::warn!(
                    target: "pa_core::refinement",
                    %err,
                    "local-refinement mirror append failed (tap will miss this record)"
                );
            }
            None
        }
    }
}

/// The billed refiner seam: the reply's usage row records at reply time —
/// the planning call holds no session lock, and a resolved bill persists
/// even when its caller is aborted or dropped mid-commit.
pub(crate) fn recorded_refiner_call(
    call: RefinerFn,
    source: &'static str,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    usage_store: Arc<dyn crate::session_engine::rlm_usage::RlmChildUsageStore>,
) -> RefinerFn {
    Box::new(move |model, system_prompt, prompt| {
        let reply = call(model, system_prompt, prompt);
        let session = Arc::clone(&session);
        let usage_store = Arc::clone(&usage_store);
        Box::pin(async move {
            let reply = reply.await?;
            // The task owns the resolved bill before the first await. Dropping
            // its JoinHandle detaches the append instead of dropping the bill.
            if let Some(usage) = ModelUsage::of_reply(source, &reply) {
                let recording = tokio::spawn(async move {
                    let session_id = session.lock().await.get_session_id().to_string();
                    if let Err(error) = usage_store.append_model_usage(session_id, usage).await {
                        eprintln!("pa-core: model usage row not persisted: {error}");
                    }
                });
                if let Err(error) = recording.await {
                    eprintln!("pa-core: model usage recording task failed: {error}");
                }
            }
            Ok(reply)
        })
    })
}
