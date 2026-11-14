//! The model-turn runner: the streaming run over the built session agent,
//! the retry/failover policy application, and the quota-park mid-run arm.
use std::cell::RefCell;
use std::sync::Arc;

use pa_types::sync::{MutexExt, RwLockExt};

use super::{
    AgentSessionEngine,
    EngineEvent,
    ProviderTarget,
    StopReason,
    TurnAdmission,
    TurnOnce,
    TurnPrompt,
    TurnResult,
    aborted_message,
    drop_trailing_assistant,
    json_round_trip,
    map_thinking_level,
    retry_event_to_engine_event,
};

/// The turn's single `emit`, handed through a `RefCell` slot to whichever retry-driver
/// callback is currently running.
type EmitCell<'e> = RefCell<&'e mut dyn FnMut(EngineEvent) -> bool>;

/// The serving state a failover switch captured (TS `_backupModel`).
#[derive(Clone)]
struct FailoverPrimary {
    model: pa_types::ai::Model,
    thinking_level: pa_agent::types::ThinkingLevel,
    session_restored: bool,
}

/// One turn's failover episode. Failover switch/restore re-bind the live agent's model,
/// append the model-change row, and re-bind the turn-boundary model facts (`model.info`
/// follows the session model). The first successful backup response restores session
/// state before tools, like TS; the captured run still serves backup traffic until it
/// settles.
#[derive(Clone, Default)]
struct FailoverEpisode {
    /// The failover-captured primary target state (TS `_backupModel`): session state
    /// restores at a successful `message_end`; request auth stays on the backup until the
    /// active run settles.
    primary: Arc<std::sync::Mutex<Option<FailoverPrimary>>>,
    /// The restore's model changes; the later `run_once` subscriber forwards them after the
    /// assistant row and before tool execution, matching TS.
    pending_model_changes: Arc<std::sync::Mutex<Vec<pa_types::session::ModelChangeEntry>>>,
    core: Option<Arc<pa_core::session_engine::engine::SessionEngine>>,
    persistence: Option<Arc<tokio::sync::Mutex<pa_core::session::manager::SessionManager>>>,
}

impl FailoverEpisode {
    /// Capture the primary state once (TS `_backupModel`), restored when the successful
    /// backup response arrives; a later switch re-arms the restore.
    fn capture_primary(
        &self,
        model: &pa_types::ai::Model,
        thinking_level: pa_agent::types::ThinkingLevel,
    ) {
        let mut primary = self.primary.lock_or_recover();
        match primary.as_mut() {
            None => {
                *primary = Some(FailoverPrimary {
                    model: model.clone(),
                    thinking_level,
                    session_restored: false,
                });
            }
            Some(primary) => primary.session_restored = false,
        }
    }

    fn mark_session_restored(&self) {
        if let Some(primary) = self.primary.lock_or_recover().as_mut() {
            primary.session_restored = true;
        }
    }

    /// Restore session state at the first successful backup `message_end`, awaited before
    /// tool execution. The agent loop has already captured its serving model, so restoring
    /// session state here does not redirect the remaining backup provider requests.
    async fn subscribe_session_restore(
        &self,
        agent: &Arc<pa_agent::agent::Agent>,
    ) -> pa_agent::agent::Subscription {
        let episode = self.clone();
        let agent_weak = Arc::downgrade(agent);
        agent
            .subscribe(move |event, _signal| {
                let episode = episode.clone();
                let agent_weak = agent_weak.clone();
                Box::pin(async move { episode.restore_session_on(event, &agent_weak).await })
            })
            .await
    }

    async fn restore_session_on(
        &self,
        event: pa_agent::types::AgentEvent,
        agent: &std::sync::Weak<pa_agent::agent::Agent>,
    ) -> anyhow::Result<()> {
        let pa_agent::types::AgentEvent::MessageEnd {
            message:
                pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(assistant)),
        } = event
        else {
            return Ok(());
        };
        if matches!(
            assistant.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            return Ok(());
        }
        let primary = self.primary.lock_or_recover().clone();
        let Some(primary) = primary.filter(|primary| !primary.session_restored) else {
            return Ok(());
        };
        let Some(agent) = agent.upgrade() else {
            return Ok(());
        };
        let model = json_round_trip(&primary.model)
            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
        agent
            .set_model_and_thinking_level(model, primary.thinking_level)
            .await;
        if let Some(core) = &self.core {
            core.update_model_facts(&primary.model);
        }
        if let Some(persistence) = &self.persistence {
            persistence
                .lock()
                .await
                .append_model_change(&primary.model.provider, &primary.model.id)?;
        }
        self.pending_model_changes
            .lock_or_recover()
            .push(pa_types::session::ModelChangeEntry {
                provider: primary.model.provider.clone(),
                model_id: primary.model.id.clone(),
            });
        self.mark_session_restored();
        Ok(())
    }
}

/// What every attempt of one model turn runs with.
struct TurnAttempt<'a> {
    agent: &'a Arc<pa_agent::agent::Agent>,
    prompt: &'a TurnPrompt,
    model: &'a pa_types::ai::Model,
    boundary_passed: &'a Arc<std::sync::atomic::AtomicBool>,
    pending_model_changes: &'a Arc<std::sync::Mutex<Vec<pa_types::session::ModelChangeEntry>>>,
    aborted: &'a dyn Fn() -> bool,
    /// Custom-row turns (harness notices, agent messages) owe no answer: silence
    /// acknowledges them.
    owes_answer: bool,
    overflow_window: u64,
}

impl TurnAttempt<'_> {
    /// A final reply that cannot end the turn (cut off at the output limit, or no visible
    /// text where an answer is owed) becomes a retryable failure for the retry driver.
    fn settle(
        &self,
        assistant: pa_agent::types::AssistantMessage,
    ) -> pa_agent::types::AssistantMessage {
        use pa_core::session_engine::provider_retry::{
            incomplete_reply_failure,
            incomplete_reply_reason,
        };
        match incomplete_reply_reason(&assistant, self.owes_answer, self.overflow_window) {
            Some(reason) => incomplete_reply_failure(assistant, reason),
            None => assistant,
        }
    }
}

/// What a turn that returned with the threshold yield armed does next.
enum ThresholdYield {
    /// The reply settles the attempt.
    Settle,
    /// The turn was aborted.
    Aborted,
    /// The boundary compacted: the tool run continues.
    Continue,
}

impl AgentSessionEngine {
    /// Drive one admitted prompt through the retry-driver model loop and
    /// emit the turn outcome (provider-failure retries + final-row
    /// surfacing). The trailing `Done` is owned by the caller.
    pub(super) fn run_model_turn(
        &self,
        admission: TurnAdmission,
        prompt: &TurnPrompt,
        boundary_passed: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> TurnResult {
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return TurnResult::Error {
                    error: error.to_string(),
                    assistant: None,
                };
            }
        };
        // The preflight validates the model SERVING the run: a routed image-model episode
        // authenticates its own image model.
        let preflight_model = self
            .armed_image_route()
            .map_or_else(|| model.clone(), |route| route.target.model);
        if let Err(error) = self.preflight_auth(&preflight_model) {
            return TurnResult::Error {
                error,
                assistant: None,
            };
        }
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => {
                return TurnResult::Error {
                    error: format!("{error:#}"),
                    assistant: None,
                };
            }
        };
        // No per-turn request-auth re-resolve: the session stream resolves
        // request auth on every provider call (the live-auth seam), so a
        // `.prime/context.json` change mid-session (`prime switch --local`)
        // bills the team the preflight above just validated.
        // The delivery's cancel flag is consulted at the admission, before
        // the agent run registers: an abort landing in the [pickup,
        // registration] window is otherwise lost. Delivery-scoped by
        // construction.
        if aborted() {
            return TurnResult::Aborted;
        }
        // A routed image-model episode applies its override BEFORE the
        // first provider call: the serving target swaps to the image
        // model (the agent state itself never swaps — per-run, like TS).
        self.apply_armed_image_route(&agent);
        let policy = self.retry_policy();
        let failover_policy = self.failover_policy();
        // A routed episode serves (and fails over within) the ROUTED
        // model: the candidate chain derives from the serving model. The
        // cross-model fallback chain (`fallbackModels`) follows the
        // session model's providers; a routed image episode stays on its
        // image model (a fallback could not see the images).
        let routed_model = self.armed_image_route().map(|route| route.target.model);
        let serving_model = routed_model.as_ref().unwrap_or(&model);
        let candidates = self.failover_candidates(serving_model);
        let fallback_models = if routed_model.is_some() {
            Vec::new()
        } else {
            self.fallback_models(&model)
        };
        let overflow_window = serving_model.context_window;
        // The pa-core retry driver owns the attempt loop; this engine
        // owns one turn.
        let emit_cell: EmitCell<'_> = RefCell::new(emit);
        // The overflow compact-and-retry re-issues the loop without a new
        // user message, so its turn starts as a continuation (TS
        // `agent.continue()`); ordinary turns start fresh.
        let first_attempt = std::cell::Cell::new(matches!(admission, TurnAdmission::FreshPrompt));
        let core = self.session.blocking_lock().as_ref().cloned();
        let episode = FailoverEpisode {
            persistence: core.as_ref().map(|core| core.session.shared_persistence()),
            core,
            ..FailoverEpisode::default()
        };
        // Retry/failover adoption telemetry (TS `auto_retry_start` counting):
        // retries increment `retry_count`, provider switches `failover_count`.
        // The semantic-edge recorder rides the same snapshot (TS
        // `prepareTurnRetry` / `clearTurnRetry` ride the retry events: the
        // parked id makes the retried call reuse the failed attempt's
        // idempotency key).
        let (telemetry, semantic_edges) = {
            let guard = self.session.blocking_lock();
            (
                guard.as_deref().and_then(|engine| engine.telemetry.clone()),
                guard
                    .as_deref()
                    .and_then(|engine| engine.session.semantic_edges()),
            )
        };
        // The quota-park seam (TS #2375): the retry chain consults the
        // engine at its give-up; a quota failure whose reset exceeds
        // the wait cap parks the session. The weak self keeps the
        // callback `'static`.
        let quota_parked_flag = std::sync::Arc::clone(&self.quota_parked_this_run);
        let engine_weak = self
            .self_weak
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let park: Option<pa_core::session_engine::provider_park::ParkDecisionCallback> =
            Some(&mut move |message, abort| {
                let engine_weak = engine_weak.clone();
                let abort = abort.to_string();
                let quota_parked_flag = std::sync::Arc::clone(&quota_parked_flag);
                Box::pin(async move {
                    let engine = engine_weak.as_ref().and_then(std::sync::Weak::upgrade)?;
                    let outcome = engine.park_for_quota_reset(&message, &abort).await;
                    if outcome.is_some() {
                        quota_parked_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    outcome
                })
            });
        let attempt = TurnAttempt {
            agent: &agent,
            prompt,
            model: &model,
            boundary_passed,
            pending_model_changes: &episode.pending_model_changes,
            aborted,
            owes_answer: matches!(prompt, TurnPrompt::User { .. }),
            overflow_window,
        };
        let restore_subscription = self
            .runtime
            .block_on(episode.subscribe_session_restore(&agent));
        let restore_primary = || self.restore_failover_primary(&episode, &agent, &emit_cell);
        let result = self.runtime.block_on(
            pa_core::session_engine::provider_failover::run_turn_with_model_fallback(
                &policy,
                &failover_policy,
                &candidates,
                &fallback_models,
                overflow_window,
                None,
                || {
                    let mut emit = emit_cell.borrow_mut();
                    let first = first_attempt.replace(false);
                    let attempt = &attempt;
                    async move { self.run_attempt(attempt, first, &mut **emit).await }
                },
                |event| {
                    let mut emit = emit_cell.borrow_mut();
                    let telemetry = telemetry.clone();
                    let semantic_edges = semantic_edges.clone();
                    async move {
                        // TS parks the id at `auto_retry_start` and clears
                        // it at the settle (`_resolveRetry`), so a
                        // body-identical retry reuses the failed
                        // attempt's id; the parked id lives until the
                        // next turn mints over it otherwise.
                        if let Some(recorder) = &semantic_edges {
                            match &event {
                                pa_core::session_engine::auto_retry::AutoRetryEvent::Start {
                                    ..
                                } => recorder.prepare_turn_retry(),
                                pa_core::session_engine::auto_retry::AutoRetryEvent::End {
                                    ..
                                } => {
                                    recorder.clear_turn_retry();
                                }
                            }
                        }
                        if let Some(telemetry) = &telemetry {
                            // One retry event in, one telemetry seam out.
                            telemetry.note_auto_retry_event(&event);
                        }
                        let engine_event = retry_event_to_engine_event(event);
                        if !emit(engine_event) {
                            anyhow::bail!("emit cancelled");
                        }
                        Ok(())
                    }
                },
                |delay| abort_aware_delay(delay, aborted),
                |next: &pa_types::ai::Model| {
                    self.switch_failover_model(&episode, &agent, &model, next.clone(), &emit_cell)
                },
                &restore_primary,
                park,
            ),
        );
        self.runtime.block_on(restore_subscription.unsubscribe());
        // A failing awaited listener (including a failed model-change
        // append) exits the retry driver before its normal restore callback.
        // Always restore the transport as well, while preserving both errors.
        let result = match result {
            Err(error) => match self.runtime.block_on(restore_primary()) {
                Ok(_) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{error:#}; primary restoration also failed: {cleanup:#}"
                )),
            },
            Ok(message) => Ok(message),
        };
        turn_result(result)
    }

    /// The credential preflight over the serving model: a model whose provider has no
    /// configured credential fails the run before the provider request, with the login
    /// guidance message. `Err` is the turn's error.
    fn preflight_auth(&self, preflight_model: &pa_types::ai::Model) -> Result<(), String> {
        if self.config.faux_script.is_some() {
            return Ok(());
        }
        // A broken prime CLI directory context (a malformed
        // `.prime/context.json`, or one naming a missing saved context)
        // fails a Prime Inference run instead of billing the stored team,
        // as the prime CLI refuses to run under it (no TS equivalent).
        if preflight_model.provider == pa_core::auth::PRIME_INFERENCE_PROVIDER_ID
            && let Err(error) = self.session_auth().prime_directory_selection() {
                return Err(format!(
                    "Invalid Prime team selection: {error}\n\nFix it, or run `prime config unpin` in this directory."
                ));
            }
        if self.current_selection().api_key.is_some() {
            return Ok(());
        }
        let mut registry = self.session_model_registry();
        registry.load_private_authorization_from_cache();
        let uses_oauth = registry
            .auth
            .get_all()
            .credential(&preflight_model.provider)
            .is_some_and(|credential| {
                matches!(credential, pa_core::auth::AuthCredential::Oauth { .. })
            });
        if !registry.has_configured_auth(preflight_model) {
            return Err(missing_credential_message(
                &preflight_model.provider,
                uses_oauth,
            ));
        }
        if !uses_oauth {
            return Ok(());
        }
        // An OAuth login resolves its key per turn: an expired token
        // refreshes here (the serving target's key was resolved at
        // build time and would go stale), and a failed refresh is an
        // authentication failure, not a keyless provider call.
        let resolved =
            registry.get_api_key_and_headers(preflight_model, preflight_model.headers.as_ref());
        if resolved.oauth_refresh_failed {
            return Err(resolved.error.unwrap_or_else(|| {
                pa_core::auth::oauth_refresh_failed_message(&preflight_model.provider)
            }));
        }
        if resolved.ok
            && let Some(target) =
                self.provider_target
                    .write_or_recover()
                    .as_mut()
                    .filter(|target| {
                        target.model.provider == preflight_model.provider
                            && target.model.id == preflight_model.id
                    })
            {
                target.api_key = resolved.api_key;
                target.headers = resolved.headers;
            }
        Ok(())
    }

    /// One attempt of the retry driver: the turn, then the threshold yields it compacts
    /// across, until a reply settles the attempt.
    async fn run_attempt(
        &self,
        attempt: &TurnAttempt<'_>,
        first: bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<pa_agent::types::AssistantMessage> {
        // A retry re-issues the failed turn: the failed
        // assistant message leaves the loop context first
        // (TS `messages.slice(0, -1)`), then `continue`.
        if !first {
            drop_trailing_assistant(attempt.agent).await;
        }
        let mut admit_prompt = first;
        loop {
            let assistant = match self
                .run_turn_once(
                    attempt.agent,
                    attempt.prompt,
                    admit_prompt,
                    attempt.boundary_passed,
                    attempt.pending_model_changes,
                    attempt.aborted,
                    emit,
                )
                .await?
            {
                TurnOnce::Message { assistant } => *assistant,
                TurnOnce::None => return Err(anyhow::anyhow!("No response produced.")),
                TurnOnce::Aborted => return Ok(aborted_message(attempt.model)),
            };
            match self.threshold_yield(attempt.aborted, emit).await {
                ThresholdYield::Settle => return Ok(attempt.settle(assistant)),
                ThresholdYield::Aborted => return Ok(aborted_message(attempt.model)),
                // Continue the tool run without dropping its successful assistant
                // or inserting another copy of the admitted user prompt.
                ThresholdYield::Continue => admit_prompt = false,
            }
        }
    }

    /// A turn that returned with the threshold yield armed: compact at the boundary and
    /// continue the run, unless a requested boundary, a queued steer or an abort ends it.
    async fn threshold_yield(
        &self,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> ThresholdYield {
        if !self
            .threshold_yielded
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return ThresholdYield::Settle;
        }
        let steer_queued = || {
            self.config
                .queued_steering_probe
                .as_ref()
                .is_some_and(|probe| probe())
        };
        // Requested boundaries stay owned by run_turns; consuming one here
        // would lose its explicit stop and goal-continuation semantics.
        let core = self.session.lock().await.clone();
        let pending = match core {
            Some(core) => {
                core.turn_boundary.compaction_scheduled().await
                    || core.turn_boundary.refine_pending().await
            }
            None => false,
        };
        if aborted() {
            return ThresholdYield::Aborted;
        }
        if pending || steer_queued() {
            return ThresholdYield::Settle;
        }
        if aborted() {
            return ThresholdYield::Aborted;
        }
        // Keep the retry/failover episode alive across this artificial
        // yield. The engine owns a multi-thread runtime; the synchronous
        // compaction boundary may re-enter it without holding async locks.
        let outcome = tokio::task::block_in_place(|| self.run_auto_compaction(emit));
        self.threshold_checks_enabled.store(
            outcome.continues_threshold_checks(),
            std::sync::atomic::Ordering::SeqCst,
        );
        if outcome == crate::auto_compaction::AutoCompactionRun::Cancelled || aborted() {
            return ThresholdYield::Aborted;
        }
        if steer_queued() {
            return ThresholdYield::Settle;
        }
        ThresholdYield::Continue
    }

    /// The failover switch to `next`: capture the primary (synchronously, at the switch),
    /// then move the stream's provider target, the agent and the session to the backup.
    fn switch_failover_model<'a, 'e>(
        &'a self,
        episode: &'a FailoverEpisode,
        agent: &'a Arc<pa_agent::agent::Agent>,
        model: &pa_types::ai::Model,
        next: pa_types::ai::Model,
        emit_cell: &'a EmitCell<'e>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + use<'a, 'e> {
        episode.capture_primary(model, map_thinking_level(self.effective_thinking()));
        async move {
            let agent_model =
                json_round_trip(&next).ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
            // Clamp the level to the switched-to model (TS
            // `clampThinkingLevel`); the primary restores with
            // the primary.
            let clamped = pa_ai::models::clamp_thinking_level(&next, self.effective_thinking());
            // The stream's provider target follows the switch (the same slot
            // `set_model` swaps).
            {
                // The switch's install serializes with the
                // other slot writers (the restore path in
                // the sibling callback takes the same lock):
                // the turn runs on the blocking pool, so the
                // write never touches an async executor
                // worker.
                let _reload_serialized = self.reload_lock.lock_or_recover();
                // A routed image episode fails over WITHIN the
                // routed model (the candidates derive from the
                // route's model), so the stream moves to the
                // candidate's provider too: re-pinning the
                // route's own target resent every "backup" retry
                // to the provider that just failed (#3312). The
                // routed tier re-clamps for the candidate row, as
                // the route's resolution clamped it at arm time.
                let session_tier = *self.service_tier.read_or_recover();
                let routed = self.armed_image_route().is_some();
                let service_tier = if routed {
                    pa_types::ai::clamp_service_tier(Some(&next), session_tier)
                } else {
                    session_tier
                };
                let mut target = self.provider_target.write_or_recover();
                // The daemon's stream re-resolves request auth
                // per provider call: the slot's auth half is
                // write-only here.
                *target = Some(ProviderTarget {
                    service_tier,
                    api_key: None,
                    model: next.clone(),
                    headers: None,
                });
            }
            agent.set_model(agent_model).await;
            agent.set_thinking_level(map_thinking_level(clamped)).await;
            if let Some(core) = &episode.core {
                core.update_model_facts(&next);
            }
            if let Some(persistence) = &episode.persistence {
                let mut session = persistence.lock().await;
                session.append_model_change(&next.provider, &next.id)?;
            }
            if !(emit_cell.borrow_mut())(EngineEvent::ModelChange(
                pa_types::session::ModelChangeEntry {
                    provider: next.provider.clone(),
                    model_id: next.id.clone(),
                },
            )) {
                anyhow::bail!("Model switch persistence was cancelled");
            }
            Ok(())
        }
    }

    /// Restore the failover-captured primary (read synchronously, at the call): the
    /// provider target, the agent's model and thinking level, and — unless the
    /// message-end restore already did — the session's model change. Returns the restored
    /// `provider/id`, `None` when no failover happened.
    fn restore_failover_primary<'a, 'e>(
        &'a self,
        episode: &'a FailoverEpisode,
        agent: &'a Arc<pa_agent::agent::Agent>,
        emit_cell: &'a EmitCell<'e>,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<String>>> + use<'a, 'e> {
        let primary = episode.primary.lock_or_recover().clone();
        async move {
            let Some(FailoverPrimary {
                model: primary_model,
                thinking_level,
                session_restored,
            }) = primary
            else {
                return Ok(None);
            };
            let agent_model = json_round_trip(&primary_model)
                .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
            {
                // The restore serializes with the other
                // slot writers (`set_model`, the failover
                // switch, the image-route apply/clear): the
                // lock makes the last writer leave the newest
                // target standing. The whole turn (this
                // closure included) runs on the blocking
                // pool — the worker parks the turn there
                // (`worker/turn.rs`) — so the slot write
                // below never touches an async executor
                // worker; only a concurrently reloading
                // session waits on this span.
                let _reload_serialized = self.reload_lock.lock_or_recover();
                // The armed-route install goes through the
                // fenced helper (no provider-slot lock held
                // across the route read).
                if !self.install_armed_route_target() {
                    // The restored target carries the model and
                    // the tier: the daemon's stream re-resolves
                    // request auth per provider call, so the
                    // slot's auth half is write-only here.
                    let target = self.restored_primary_target(&primary_model);
                    *self.provider_target.write_or_recover() = Some(target);
                }
            }
            agent.set_model(agent_model).await;
            agent.set_thinking_level(thinking_level).await;
            if let Some(core) = &episode.core {
                core.update_model_facts(&primary_model);
            }
            if let Some(persistence) = episode.persistence.as_ref().filter(|_| !session_restored) {
                persistence
                    .lock()
                    .await
                    .append_model_change(&primary_model.provider, &primary_model.id)?;
            }
            if !session_restored
                && !(emit_cell.borrow_mut())(EngineEvent::ModelChange(
                    pa_types::session::ModelChangeEntry {
                        provider: primary_model.provider.clone(),
                        model_id: primary_model.id.clone(),
                    },
                ))
            {
                anyhow::bail!("Model restoration persistence was cancelled");
            }
            episode.mark_session_restored();
            Ok(Some(format!(
                "{}/{}",
                primary_model.provider, primary_model.id
            )))
        }
    }
}

/// The no-credential failure: an OAuth login whose credentials did not resolve, or a
/// provider with no key at all.
fn missing_credential_message(provider: &str, uses_oauth: bool) -> String {
    if uses_oauth {
        return format!(
            "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable.\n\nRun /login to update credentials."
        );
    }
    let docs = pa_core::packages::docs_path();
    format!(
        "No API key found for {provider}.\n\nUse /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
        docs.join("providers.md").display(),
        docs.join("models.md").display()
    )
}

/// The retry wait, abort-aware: the cancel flag stops the retry sleep early (TS
/// `_retryAbortController`). Returns whether the wait ran out.
async fn abort_aware_delay(delay: std::time::Duration, aborted: &dyn Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if aborted() {
            return false;
        }
        if tokio::time::Instant::now() >= deadline {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The turn outcome of the retry driver's final message.
fn turn_result(result: anyhow::Result<pa_agent::types::AssistantMessage>) -> TurnResult {
    match result {
        Ok(message) => match message.stop_reason {
            // The failure already reached the transcript as the final
            // assistant message; the turn error still travels to
            // headless callers through the turn result.
            StopReason::Error => TurnResult::Error {
                error: message
                    .error_message
                    .clone()
                    .filter(|error| !error.is_empty())
                    .unwrap_or_else(|| "Assistant response failed".to_string()),
                assistant: Some(Box::new(message)),
            },
            StopReason::Aborted => TurnResult::Aborted,
            _ => TurnResult::Message(Box::new(message)),
        },
        Err(error) => TurnResult::Error {
            error: error.to_string(),
            assistant: None,
        },
    }
}
