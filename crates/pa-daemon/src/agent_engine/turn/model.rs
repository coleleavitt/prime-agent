//! The model-turn runner: the streaming run over the built session agent,
//! the retry/failover policy application, and the quota-park mid-run arm.
use pa_types::sync::RwLockExt;

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
        #[derive(Clone)]
        struct FailoverPrimary {
            model: pa_types::ai::Model,
            thinking_level: pa_agent::types::ThinkingLevel,
            api_key: Option<String>,
            headers: Option<std::collections::BTreeMap<String, String>>,
        }
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return TurnResult::Error {
                    error: error.to_string(),
                    assistant: None,
                };
            }
        };
        // The preflight validates the model SERVING the run: a model whose
        // provider has no configured credential fails the run before the
        // provider request, with the login-guidance message — a routed
        // image-model episode authenticates its own image model.
        let preflight_model = self
            .armed_image_route()
            .map_or_else(|| model.clone(), |route| route.target.model);
        // A broken prime CLI directory context (a malformed
        // `.prime/context.json`, or one naming a missing saved context)
        // fails a Prime Inference run instead of billing the stored team,
        // as the prime CLI refuses to run under it (no TS equivalent).
        if self.config.faux_script.is_none()
            && preflight_model.provider == pa_core::auth::PRIME_INFERENCE_PROVIDER_ID
        {
            if let Err(error) = self.session_auth().prime_directory_selection() {
                return TurnResult::Error {
                    error: format!(
                        "Invalid Prime team selection: {error}\n\nFix it, or run `prime config unpin` in this directory."
                    ),
                    assistant: None,
                };
            }
        }
        if self.config.faux_script.is_none() && self.current_selection().api_key.is_none() {
            let mut registry = self.session_model_registry();
            registry.load_private_authorization_from_cache();
            let uses_oauth = registry
                .auth
                .get_all()
                .credential(&preflight_model.provider)
                .is_some_and(|credential| {
                    matches!(credential, pa_core::auth::AuthCredential::Oauth { .. })
                });
            if !registry.has_configured_auth(&preflight_model) {
                let message = if uses_oauth {
                    format!(
                        "Authentication failed for \"{}\". Credentials may have expired or network is unavailable.\n\nRun /login to update credentials.",
                        preflight_model.provider
                    )
                } else {
                    let docs = pa_core::packages::docs_path();
                    format!(
                        "No API key found for {}.\n\nUse /login to log into a provider via OAuth or API key. See:\n  {}\n  {}",
                        preflight_model.provider,
                        docs.join("providers.md").display(),
                        docs.join("models.md").display()
                    )
                };
                return TurnResult::Error {
                    error: message,
                    assistant: None,
                };
            }
            // An OAuth login resolves its key per turn: an expired token
            // refreshes here (the serving target's key was resolved at
            // build time and would go stale), and a failed refresh is an
            // authentication failure, not a keyless provider call.
            if uses_oauth {
                let resolved = registry
                    .get_api_key_and_headers(&preflight_model, preflight_model.headers.as_ref());
                if resolved.oauth_refresh_failed {
                    return TurnResult::Error {
                        error: resolved.error.unwrap_or_else(|| {
                            pa_core::auth::oauth_refresh_failed_message(&preflight_model.provider)
                        }),
                        assistant: None,
                    };
                }
                if resolved.ok {
                    if let Some(target) =
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
                }
            }
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
        // The serving target's Prime Inference key and team header were
        // resolved when the session was built (or its model last switched).
        // Re-resolve them for every turn so a `.prime/context.json` change
        // mid-session (`prime switch --local`) bills the team the preflight
        // above just validated, not the one the session started with. The
        // target keeps its model: only the request auth moves.
        let serving = self
            .provider_target
            .read_or_recover()
            .as_ref()
            .map(|target| target.model.clone())
            .filter(|serving| serving.provider == pa_core::auth::PRIME_INFERENCE_PROVIDER_ID);
        if let Some(serving) = serving {
            let (api_key, headers) = self.resolve_request_key_and_headers(&serving);
            if let Some(target) =
                self.provider_target
                    .write_or_recover()
                    .as_mut()
                    .filter(|target| {
                        target.model.provider == serving.provider && target.model.id == serving.id
                    })
            {
                target.api_key = api_key;
                target.headers = headers;
            }
        }
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
        // model: the candidate chain derives from the serving model.
        let candidates = match self.armed_image_route() {
            Some(route) => self.failover_candidates(&route.target.model),
            None => self.failover_candidates(&model),
        };
        // The cross-model fallback chain (`fallbackModels`) follows the
        // session model's providers; a routed image episode stays on its
        // image model (a fallback could not see the images).
        let fallback_models = match self.armed_image_route() {
            Some(_) => Vec::new(),
            None => self.fallback_models(&model),
        };
        // The pa-core retry driver owns the attempt loop; this engine
        // owns one turn. The single `emit` reference is handed through
        // a RefCell slot to whichever closure is currently running.
        let emit_cell = std::cell::RefCell::new(emit);
        // The overflow compact-and-retry re-issues the loop without a new
        // user message, so its turn starts as a continuation (TS
        // `agent.continue()`); ordinary turns start fresh.
        let first_attempt = std::cell::Cell::new(matches!(admission, TurnAdmission::FreshPrompt));
        // Failover switch/restore re-bind the live agent's model and
        // append the model-change row. The primary (model + thinking
        // level) is captured at the first switch, restored on settle.
        let persistence = {
            let guard = self.session.blocking_lock();
            guard
                .as_ref()
                .map(|engine| engine.session.shared_persistence())
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
        // The failover-captured primary target state (TS `_backupModel`):
        // model, thinking level, and request auth, restored on settle.
        let primary_state: std::cell::RefCell<Option<FailoverPrimary>> =
            std::cell::RefCell::new(None);
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
        let overflow_window = match self.armed_image_route() {
            Some(route) => route.target.model.context_window,
            None => model.context_window,
        };
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
                    let first = first_attempt.get();
                    first_attempt.set(false);
                    let agent = agent.clone();
                    let prompt = prompt.clone();
                    let model = model.clone();
                    async move {
                        // A retry re-issues the failed turn: the failed
                        // assistant message leaves the loop context first
                        // (TS `messages.slice(0, -1)`), then `continue`.
                        if !first {
                            drop_trailing_assistant(&agent).await;
                        }
                        match self
                            .run_turn_once(
                                &agent,
                                &prompt,
                                first,
                                boundary_passed,
                                aborted,
                                &mut **emit,
                            )
                            .await
                        {
                            Ok(TurnOnce::Message { assistant }) => {
                                // The settled messages already reached the
                                // transcript; this arm only carries the
                                // final message to the retry classifier.
                                Ok(*assistant)
                            }
                            Ok(TurnOnce::None) => Err(anyhow::anyhow!("No response produced.")),
                            Ok(TurnOnce::Aborted) => Ok(aborted_message(&model)),
                            Err(error) => Err(error),
                        }
                    }
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
                |delay| {
                    async move {
                        // Abort-aware wait: the cancel flag stops the retry sleep early (TS
                        // `_retryAbortController`).
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
                },
                |next: &pa_types::ai::Model| {
                    let agent = agent.clone();
                    let persistence = persistence.clone();
                    {
                        let mut primary = primary_state.borrow_mut();
                        // Capture the primary state once (TS `_backupModel`), restored when the
                        // turn settles.
                        if primary.is_none() {
                            let (api_key, headers) = self.resolve_request_key_and_headers(&model);
                            *primary = Some(FailoverPrimary {
                                model: model.clone(),
                                thinking_level: map_thinking_level(self.effective_thinking()),
                                api_key,
                                headers,
                            });
                        }
                    }
                    let next = next.clone();
                    async move {
                        let agent_model = json_round_trip(&next)
                            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                        // Clamp the level to the switched-to model (TS
                        // `clampThinkingLevel`); the primary restores with
                        // the primary.
                        let clamped =
                            pa_ai::models::clamp_thinking_level(&next, self.effective_thinking());
                        // The stream's provider target follows the switch (the same slot
                        // `set_model` swaps).
                        {
                            // A routed image episode fails over WITHIN the
                            // routed model (the candidates derive from the
                            // route's model), so the stream moves to the
                            // candidate's provider too: re-pinning the
                            // route's own target resent every "backup" retry
                            // to the provider that just failed (#3312). The
                            // routed tier re-clamps for the candidate row, as
                            // the route's resolution clamped it at arm time.
                            let (api_key, headers) = self.resolve_request_key_and_headers(&next);
                            let session_tier = *self.service_tier.read_or_recover();
                            let routed = self.armed_image_route().is_some();
                            let service_tier = if routed {
                                pa_types::ai::clamp_service_tier(Some(&next), session_tier)
                            } else {
                                session_tier
                            };
                            let mut target = self.provider_target.write_or_recover();
                            *target = Some(ProviderTarget {
                                service_tier,
                                api_key,
                                model: next.clone(),
                                headers,
                            });
                        }
                        agent.set_model(agent_model).await;
                        agent.set_thinking_level(map_thinking_level(clamped)).await;
                        if let Some(persistence) = persistence {
                            let mut session = persistence.lock().await;
                            session.append_model_change(&next.provider, &next.id)?;
                        }
                        Ok(())
                    }
                },
                || {
                    let agent = agent.clone();
                    let persistence = persistence.clone();
                    let primary = primary_state.borrow().clone();
                    async move {
                        let Some(FailoverPrimary {
                            model: primary_model,
                            thinking_level,
                            api_key: primary_api_key,
                            headers: primary_headers,
                        }) = primary
                        else {
                            return Ok(None);
                        };
                        let agent_model = json_round_trip(&primary_model)
                            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                        {
                            let mut target = self.provider_target.write_or_recover();
                            if let Some(route) = self.armed_image_route() {
                                *target = Some(route.target);
                            } else {
                                *target = Some(ProviderTarget {
                                    service_tier: *self.service_tier.read_or_recover(),
                                    api_key: primary_api_key,
                                    model: primary_model.clone(),
                                    headers: primary_headers,
                                });
                            }
                        }
                        agent.set_model(agent_model).await;
                        agent.set_thinking_level(thinking_level).await;
                        if let Some(persistence) = persistence {
                            let mut session = persistence.lock().await;
                            session
                                .append_model_change(&primary_model.provider, &primary_model.id)?;
                        }
                        Ok(Some(format!(
                            "{}/{}",
                            primary_model.provider, primary_model.id
                        )))
                    }
                },
                park,
            ),
        );
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
}
