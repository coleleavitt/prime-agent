//! Session creation and reuse on the worker: the create command's
//! construction of the live session.
use pa_types::sync::{MutexExt, RwLockExt};
use serde::Deserialize as _;
use serde_json::Value;

use super::{
    Arc,
    EngineModelSelection,
    Result,
    RlmSessionIdentity,
    SessionEngine,
    SessionFile,
    VecDeque,
    Worker,
    json,
    paths,
    response_failure,
    response_success,
    restore_queue_snapshot,
    session_file_name,
};
use crate::agent_engine::CreateSessionResources;
use crate::protocol::DaemonResponse;

impl Worker {
    /// Start one of a create's fire-and-forget tasks, keeping its abort handle. Finished handles
    /// are pruned here, so the list stays bounded by the running work.
    fn spawn_create_background(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let handle = tokio::spawn(work).abort_handle();
        let mut running = self.create_background.lock_or_recover();
        running.retain(|handle| !handle.is_finished());
        running.push(handle);
    }

    /// Retire this worker in-process (a test's simulated restart): abort the create's
    /// fire-and-forget tasks, the context-tree refreshes and the turn runner, and close the
    /// recovery journal. A real retire ends the worker's process and every writer with it; the
    /// worker here stays referenced by its own tasks, so dropping it stops none of them, and
    /// a turn it ran after the test removed its dir would recreate `recovery.jsonl` there.
    #[cfg(all(test, unix))] // its callers, the cloud-inbox restart tests, are unix-only
    pub(crate) fn retire(&self) {
        for handle in self.create_background.lock_or_recover().drain(..) {
            handle.abort();
        }
        self.context_tree.abort_refreshes();
        self.turn_runner.abort();
        drop(self.recovery.lock_or_recover().take());
    }

    pub(super) async fn handle_create(&self, payload: &Value) -> DaemonResponse {
        // One create in flight at a time: a concurrent create joins this open and
        // answers with the created summary instead of racing a second init.
        let _create_gate = self.create_gate.lock().await;
        let existing_summary = {
            let (core, inputs) = self.summary_inputs();
            core.created.then(|| self.summary_locked(&core, inputs))
        };
        if let Some(summary) = existing_summary {
            // Idempotent re-create after a supervisor restart or respawn.
            // A respawned worker's re-create re-binds the pane reporter
            // (the session may carry a fresh client env on the payload)
            // and re-reports: a supervisor restart must not leave the
            // pane holding a stale pre-restart state.
            self.rebind_herdr_reporter(payload);
            return response_success(
                None,
                "create",
                Some(serde_json::to_value(&summary).unwrap_or(Value::Null)),
            );
        }
        let request = match CreateRequest::parse(payload) {
            Ok(request) => request,
            Err(error) => return response_failure(None, "create", &error, None),
        };
        // A revived session restores the model its file pins before the startup
        // chain; an explicit model flag wins. The flags are authoritative for
        // this worker's runtime config and survive every session replacement.
        let flag = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        self.engine.configure_create_model(EngineModelSelection {
            provider: flag("provider"),
            model: flag("model"),
            api_key: flag("apiKey"),
            thinking: request.thinking,
        });
        // Folded before the background build spawn at the create's tail,
        // so the first build sees them.
        match create_resources(payload) {
            Ok(resources) => self.adopt_create_resources(resources).await,
            Err(error) => return response_failure(None, "create", &error, None),
        }
        let mut placement = match CreatePlacement::parse(payload, &self.config.agent_dir) {
            Ok(placement) => placement,
            Err(error) => return response_failure(None, "create", &error, None),
        };
        let (settings, trace_consent) = pa_core::agent_traces::ContinuousTraceUpload::load_settings(
            std::path::Path::new(&placement.cwd),
            &self.config.agent_dir,
        );
        let opened = {
            let traces = |path: &std::path::Path| {
                pa_core::agent_traces::ContinuousTraceUpload::install(
                    std::path::Path::new(&placement.cwd),
                    &self.config.agent_dir,
                    Some(path),
                    trace_consent.clone(),
                )
            };
            match self.open_create_store(&request, &placement, &traces).await {
                Ok(opened) => opened,
                Err(response) => return response,
            }
        };
        if let Some(recorded) = opened.branch_cwd {
            placement.cwd = recorded;
        }
        let mut store = opened.store;
        // The worker owns the session file; the engine reads it for the
        // system prompt's conversation-log path and the local harness dir.
        if !store.path.as_os_str().is_empty() {
            self.engine.set_session_file(store.path.clone());
        }
        let scoped_entries = self
            .register_startup_scope(payload, &settings, opened.continuing)
            .await;
        let name_persisted_by_fresh_arm = match self.write_fresh_prefix(
            &mut store,
            opened.fresh_prefix,
            request.name,
            &placement.cwd,
        ) {
            Ok(persisted) => persisted,
            Err(error) => return response_failure(None, "create", &error, None),
        };
        if !name_persisted_by_fresh_arm
            && let Some(name) = request.name.filter(|n| !n.trim().is_empty())
                && let Err(error) =
                    store.persist_entry("session_info", json!({ "name": name.trim() }))
                {
                    return response_failure(None, "create", &error.to_string(), None);
                }
        // Restore the persisted queue snapshot (crash/respawn recovery) from
        // the worker recovery journal.
        let (steering, follow_up, restored_suspended) = {
            let guard = self.recovery.lock_or_recover();
            match guard.as_ref() {
                Some(journal) => restore_queue_snapshot(journal, &self.config.active_session_id),
                None => (VecDeque::new(), VecDeque::new(), false),
            }
        };
        // The session's settings-seeded switches: a restarted session re-seeds
        // its auto-compaction flag from the persisted `compaction.enabled`.
        let steering_mode = queue_mode_wire(settings.get_steering_mode());
        let follow_up_mode = queue_mode_wire(settings.get_follow_up_mode());
        // The ACTIVE tier clamps to the model's support; the stored preference
        // keeps the requested tier, so an ineligible model shows the degraded tier.
        let configured_tier = opened
            .restored_tier
            .unwrap_or(Some(settings.get_default_service_tier()));
        let clamped_tier =
            crate::setting_switches::effective_service_tier(configured_tier, self.engine.as_ref());
        self.engine.configure_service_tier(clamped_tier);
        // The abort supervision's terminal record: the rebuilt transcript discloses
        // the abort with the same `compaction_outcome` row the auto-abort arms persist
        // (a manual run persists nothing); the dedup matches the row's fields alone.
        let interrupted_compaction_requested = payload.get("interruptedCompaction").is_some();
        let interrupted_compaction = crate::compaction::interrupted_compaction_disclosure(payload);
        // The disclosure row's landing state: the supervisor consumes the terminal
        // record only once the disclosure is durable (a requested record with no
        // disclosure row is vacuously durable; no requested record adds no key).
        let mut interrupted_compaction_persisted = interrupted_compaction_requested;
        let CreatePlacement {
            cwd,
            cwd_override,
            rlm_depth,
            rlm_max_depth,
            runtime,
            factory_child,
            thinking,
            child_script,
            ..
        } = placement;
        // The core lock stays inside this block: everything after it may await,
        // and a std MutexGuard must never ride an await point.
        let (summary, rlm_depth) = {
            let (mut core, inputs) = self.summary_inputs();
            core.cwd_override = cwd_override;
            if !cwd_override && Some(cwd.as_str()) != payload.get("cwd").and_then(Value::as_str) {
                self.engine.set_cwd(std::path::PathBuf::from(&cwd));
            }
            core.cwd = cwd;
            core.steering = steering;
            core.follow_up = follow_up;
            // The restored lanes carry their suspension: a user-aborted queue
            // revives still parked, never auto-running its prompts.
            core.queued_input_suspended = restored_suspended;
            // A resumed session's tool counter re-seeds from its file: a
            // revived worker keeps the count its roster rows carried.
            core.tool_use_count = store.tool_call_count();
            if let Some(disclosure) = &interrupted_compaction
                && !disclose_interrupted_compaction(&mut store, disclosure) {
                    interrupted_compaction_persisted = false;
                }
            core.store = Some(store);
            core.created = true;
            core.abort_requested = false;
            core.turn_cancelled = false;
            core.turn_completed = false;
            // A fresh session starts live: the previous close's
            // `session_closed` marker clears with the new session.
            if let Some(agent_engine) = &self.agent_engine {
                agent_engine.clear_session_closed();
            }
            core.auto_compaction_enabled = settings.get_compaction_enabled();
            core.service_tier = configured_tier;
            core.active_service_tier = clamped_tier;
            core.steering_mode = steering_mode.to_string();
            core.follow_up_mode = follow_up_mode.to_string();
            // The restored rows carry their forced-batch flag: the batch
            // gate rides the lane, so a restored forced batch keeps its
            // one-turn semantics.
            core.forced_all_steering = core.steering.iter().any(|item| item.forced_batch);
            core.scoped_models = scoped_entries;
            core.retry_abort_requested = false;
            // The session's depth falls back to the opened file's header: a resumed
            // subagent file carries its persisted depth, so the roster never re-nests
            // it under its original parent.
            let rlm_depth = rlm_depth
                .or_else(|| core.store.as_ref().and_then(SessionFile::rlm_depth))
                .unwrap_or(0);
            core.rlm_depth = rlm_depth;
            core.runtime_kind = if runtime.rlm_child_id.is_some() {
                "subagent".to_string()
            } else {
                "top-level".to_string()
            };
            core.rlm_child_id = runtime.rlm_child_id;
            core.parent_active_session_id = runtime.parent_active_session_id;
            core.parent_session_id = runtime.parent_session_id;
            core.child_script.clone_from(&child_script);
            (self.summary_locked(&core, inputs), rlm_depth)
        };
        // A worker reload over a crashed predecessor's session file: a
        // digested message whose row reached the durable inbox but whose
        // notice never queued (the crash landed between the durable append
        // and the notice's enqueue + checkpoint) would sit unread with no
        // later trigger to wake the session — the reload reconciles the
        // durable inbox and re-arms the one-per-batch notice (a no-op on a
        // clean or fully-read inbox).
        self.agent_digest.ensure_digest_notice();
        // The engine's agent-level queues drain per the same modes the
        // worker lane delivers by.
        self.engine
            .set_queue_modes(Some(steering_mode), Some(follow_up_mode));
        // Seed the engine's RLM identity: recursion depth and bound, the session's
        // persistence ids, the children's default thinking level.
        if let Err(error) = self.engine.configure_rlm_identity(RlmSessionIdentity {
            rlm_depth,
            rlm_max_depth,
            cwd: Some(summary.cwd.clone()),
            session_id: Some(summary.session_id.clone()),
            session_file: summary.session_file.clone(),
            thinking,
            child_script,
            semantic_spawn: runtime.semantic_spawn,
            factory_child,
        }) {
            return response_failure(None, "create", &error.to_string(), None);
        }
        if opened.continuing {
            self.reseed_rlm_children().await;
        }
        // The built-in Herdr connector binds here, per session: the pane
        // identity comes from the create payload's client env (the client
        // that owns the pane sent it), never from this process's ambient
        // environment — so sessions created in other panes report their
        // own panes regardless of where the supervisor booted (the TS
        // boot-context bug class, not reproduced). A live RLM child never
        // reports: it shares the parent's pane, and a child's turn or quit
        // must not flip or release it. The bind sits PAST the create's
        // last fallible step, so a create that fails never publishes an
        // idle claim for a session the supervisor then tears down (the
        // failed create would otherwise leave the pane ghost-claimed —
        // the force-kill path releases nothing).
        self.rebind_herdr_reporter(payload);
        // The engine renders this summary into the sender identity block
        // of worker-to-worker agent messages.
        if let Ok(summary_value) = serde_json::to_value(&summary) {
            self.engine.set_session_summary(summary_value);
        }
        // Deliberate divergence from TS's eager create build: the response stays
        // model-independent; a build failure surfaces on the first demand seam.
        if let Some(agent_engine) = &self.agent_engine {
            let engine = std::sync::Arc::clone(agent_engine);
            self.spawn_create_background(async move {
                let Ok(model) = engine.resolve_model() else {
                    return;
                };
                let _ = engine.ensure_core_session_async(&model).await;
            });
        }
        // Bind the schedule catalog onto the session (artifact partition,
        // job rebind, scheduler start) — TS `rebindCronJobsToState`.
        if let Err(error) = self.bind_scheduled_jobs().await {
            return response_failure(None, "create", &error.to_string(), None);
        }
        // Recovery journal writes must not happen while holding the core
        // lock: record_recovery locks the core to read the store.
        let _ = self.record_recovery(true, "create");
        let session_id = summary.session_id.clone();
        if let Some(registration) = &self.registration {
            registration.notify_session_created(session_id);
        }
        // Fire the same background `refreshAvailableModels` as TS session boot: the
        // effect is the on-disk cache file; failures fall back to the cached or
        // bundled catalog without touching the session.
        let agent_dir = self.config.agent_dir.clone();
        self.spawn_create_background(async move {
            let auth = pa_core::auth::AuthStorage::create(&agent_dir);
            let mut registry =
                pa_core::models::ModelRegistry::create(auth, agent_dir.join("models.json"));
            let _ = registry.refresh_available_models().await;
        });
        self.work_notify.notify_one();
        // Warm the context-tree cache at session open, so an early `/context`
        // answers from it instead of walking the artifact tree inline.
        self.poke_context_tree_refresh();
        self.wire_child_notifiers();
        let mut data = serde_json::to_value(&summary).unwrap_or(Value::Null);
        if interrupted_compaction_requested {
            data["interruptedCompactionPersisted"] =
                serde_json::json!(interrupted_compaction_persisted);
        }
        // A resumed create just rebuilt the store from the session
        // file: return the load's freed heap to the OS.
        pa_types::memory_release::trim_freed_heap();
        response_success(None, "create", Some(data))
    }

    /// The agent engine adopts a create's resources; a new create re-resolves the sandbox
    /// against its own override.
    async fn adopt_create_resources(&self, resources: CreateSessionResources) {
        let Some(agent_engine) = &self.agent_engine else {
            return;
        };
        *agent_engine.sandbox.write_or_recover() = crate::agent_engine::SandboxSlot::Unresolved;
        if let Some(autonomous) = &resources.autonomous {
            *agent_engine.autonomous.lock().await =
                pa_core::autonomous::create_autonomous_runtime_state(Some(autonomous), None);
        }
        *agent_engine.create_resources.write_or_recover() = resources;
    }

    /// Open the create's session store: reopen an existing file, create a file-backed fresh
    /// session at the requested path or in the sessions directory, or an in-memory one (no
    /// file, like the TS `noSession` create). `Err` is the create's failure response.
    #[expect(
        clippy::result_large_err,
        reason = "the error is the create's whole failure response, built where the failure is known"
    )]
    async fn open_create_store(
        &self,
        request: &CreateRequest<'_>,
        placement: &CreatePlacement,
        traces: &TraceInstall<'_>,
    ) -> std::result::Result<OpenedStore, DaemonResponse> {
        let fresh = || {
            SessionFile::create(
                &placement.cwd,
                placement.parent_session_path.as_deref(),
                placement.rlm_depth.unwrap_or(0),
            )
        };
        match (&request.session_path, request.no_session) {
            (Some(path), false) if path.exists() => {
                self.reopen_session(path, request, placement, traces).await
            }
            (Some(path), false) => {
                let mut created = fresh();
                created.set_path(path.clone());
                created.trace_upload = traces(&created.path);
                created.lease = Some(Arc::new(self.acquire_session_lease(path).await?));
                if let Err(error) = created.rewrite() {
                    return Err(response_failure(None, "create", &error.to_string(), None));
                }
                Ok(OpenedStore::fresh(
                    created,
                    FreshPrefixPlan::PathBacked { fold_name: false },
                ))
            }
            (None, true) => Ok(OpenedStore::fresh(fresh(), FreshPrefixPlan::InMemory)),
            (None, false) => {
                let mut created = fresh();
                let path = placement
                    .session_dir
                    .join(session_file_name(created.session_id()));
                created.set_path(path.clone());
                created.trace_upload = traces(&created.path);
                created.lease = Some(Arc::new(self.acquire_session_lease(&path).await?));
                // One durable write instead of three: the prefix, the `active` state,
                // and the session name land in a single rewrite; the create stays
                // pathless until it succeeds, so no reader reads the file mid-create.
                Ok(OpenedStore::fresh(
                    created,
                    FreshPrefixPlan::PathBacked { fold_name: true },
                ))
            }
            (Some(_), true) => Err(response_failure(
                None,
                "create",
                "Session cannot be both no-session and session-pathed",
                None,
            )),
        }
    }

    /// A fresh session file's runtime lease, taken off the async runtime; a refusal is the
    /// create's failure response.
    #[expect(
        clippy::result_large_err,
        reason = "the error is the create's whole failure response, built where the failure is known"
    )]
    async fn acquire_session_lease(
        &self,
        path: &std::path::Path,
    ) -> std::result::Result<crate::lease::SessionLease, DaemonResponse> {
        let path = path.to_path_buf();
        let agent_dir = self.config.agent_dir.clone();
        tokio::task::spawn_blocking(move || {
            crate::lease::acquire_runtime_session_lease(&path, &agent_dir)
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|lease| lease)
        .map_err(|error| crate::hold_refusal::create_failure_response(&error))
    }

    /// Reopen an existing session file under its runtime lease, restoring the model and
    /// thinking level it pins. A reopen is APPEND-ONLY: only the rows this open appends
    /// persist.
    #[expect(
        clippy::result_large_err,
        reason = "the error is the create's whole failure response, built where the failure is known"
    )]
    async fn reopen_session(
        &self,
        path: &std::path::Path,
        request: &CreateRequest<'_>,
        placement: &CreatePlacement,
        traces: &TraceInstall<'_>,
    ) -> std::result::Result<OpenedStore, DaemonResponse> {
        let loaded = {
            let path = path.to_path_buf();
            let agent_dir = self.config.agent_dir.clone();
            tokio::task::spawn_blocking(move || {
                let lease = crate::lease::acquire_runtime_session_lease(&path, &agent_dir)?;
                pa_core::session::manager::repair_jsonl_damage(&path);
                let mut store = SessionFile::open_windowed(&path)?;
                store.lease = Some(Arc::new(lease));
                Ok(store)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        };
        let mut opened = match loaded {
            Ok(opened) => opened,
            Err(error) => return Err(crate::hold_refusal::create_failure_response(&error)),
        };
        opened.trace_upload = traces(&opened.path);
        let branch_cwd = if placement.cwd_override {
            None
        } else {
            super::session_cwd::branch_cwd(&opened)
        };
        if opened.skipped_lines > 0 {
            // The rows stay on disk (the append-only
            // reopen): the skip count is the damage
            // report the torn-tail repair can act on — a
            // silent skip is how a torn session degraded
            // unnoticed (the operator's 2026-10-08
            // report).
            eprintln!(
                "pa-daemon: session {} skipped {} unparsable row(s) on open; they stay on disk",
                path.display(),
                opened.skipped_lines
            );
        }
        // The session-model restore records its decision only for a path
        // this worker opened — a failed open never leaks the binding into a
        // later create.
        self.engine.set_session_file(path.to_path_buf());
        // One fold serves all consumers: the model restore takes its saved
        // context off the store this create just opened.
        let restored = opened.restored_settings();
        let restored_tier = opened.has_service_tier().then_some(restored.service_tier);
        let has_thinking_level = opened.has_thinking_level();
        let saved =
            crate::agent_engine::saved_session_context_from_parts(&restored, has_thinking_level);
        if !request.flagged_model {
            self.engine
                .restore_session_model(path, Some(saved.clone()))
                .await;
        }
        // The saved thinking level restores here when the create carries no
        // explicit flag; the saved MODEL restores through the engine's
        // session-model restore.
        self.engine.configure_model(EngineModelSelection {
            provider: None,
            model: None,
            api_key: None,
            thinking: request.thinking.or(saved.thinking),
        });
        let append_start = opened.entries.len();
        append_creation_prefix(
            &mut opened,
            self.engine.as_ref(),
            &self.config.agent_dir,
            branch_cwd.as_deref().unwrap_or(&placement.cwd),
            false,
        );
        let _ = opened.append_session_state("active");
        // A reopen is APPEND-ONLY (the operator's 2026-10-08
        // report: a reopened session showed none of the old
        // messages): the full reader skips malformed rows in
        // memory, and the legacy full-file rewrite this
        // arm carried DELETED them from disk — a gap early
        // in the parent chain took the whole transcript
        // with it. Only the rows this open appended
        // persist; the file keeps every original byte for
        // the torn-tail repair to see.
        if let Err(error) = opened.persist_appended(append_start) {
            return Err(response_failure(None, "create", &error.to_string(), None));
        }
        // Prime the usage fold on the file's final identity,
        // off the runtime and before the core lock:
        // summaries under the lock fold only the appended
        // tail.
        let primed = path.to_path_buf();
        let _ =
            tokio::task::spawn_blocking(move || crate::session_store::read_session_info(&primed))
                .await;
        Ok(OpenedStore {
            store: opened,
            continuing: true,
            restored_tier,
            fresh_prefix: FreshPrefixPlan::None,
            branch_cwd,
        })
    }

    /// The `models` patterns (else settings `enabledModels`) resolve once into the scoped
    /// list and register as the engine's startup scope. This runs AFTER the store open
    /// (the saved-model restore's wait already covered the catalog fetch): resolving
    /// earlier stores an empty scope. Returns the wire shape `set_scoped_models` stores
    /// (the connection state surface and the cycler's input): `{ model, thinkingLevel? }`.
    async fn register_startup_scope(
        &self,
        payload: &Value,
        settings: &pa_core::settings::SettingsManager,
        is_continuing: bool,
    ) -> Vec<Value> {
        let model_patterns = match payload.get("models").and_then(Value::as_array) {
            Some(patterns) => patterns
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<String>>(),
            None => settings.get_enabled_models().unwrap_or_default(),
        };
        let scoped_models = if model_patterns.is_empty() {
            Vec::new()
        } else {
            let mut registry = crate::state_getters::worker_model_registry(&self.config.agent_dir);
            pa_core::models::resolve_model_scope_from_models_with_readiness(
                &mut registry,
                &model_patterns,
                settings
                    .get_default_provider()
                    .zip(settings.get_default_model()),
                is_continuing,
                pa_core::models::SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
            )
            .await
        };
        self.engine.configure_startup_scope(
            scoped_models.clone(),
            model_patterns.clone(),
            is_continuing,
        );
        scoped_models
            .iter()
            .map(|scoped| {
                let mut entry = json!({ "model": scoped.model });
                // The pattern's `:thinking` suffix; the wire name is the level the
                // cycler and the state readers parse back.
                if let Some(level) = scoped.thinking_level {
                    entry["thinkingLevel"] = json!(level);
                }
                entry
            })
            .collect()
    }

    /// The fresh arms' deferred creation prefix, written against the registered scope (the
    /// continuing arm wrote its own prefix, so the scope never changes a continuing file's
    /// pinned model and thinking). Returns whether the session name landed in the fresh
    /// file's single rewrite, so the shared name persist does not append a second
    /// `session_info` line.
    fn write_fresh_prefix(
        &self,
        store: &mut SessionFile,
        plan: FreshPrefixPlan,
        name: Option<&str>,
        cwd: &str,
    ) -> std::result::Result<bool, String> {
        let fold_name = match plan {
            FreshPrefixPlan::None => return Ok(false),
            FreshPrefixPlan::InMemory => {
                append_creation_prefix(
                    store,
                    self.engine.as_ref(),
                    &self.config.agent_dir,
                    cwd,
                    true,
                );
                return Ok(false);
            }
            FreshPrefixPlan::PathBacked { fold_name } => fold_name,
        };
        append_creation_prefix(
            store,
            self.engine.as_ref(),
            &self.config.agent_dir,
            cwd,
            true,
        );
        let _ = store.append_session_state("active");
        let folded = match name.filter(|n| fold_name && !n.trim().is_empty()) {
            Some(name) => {
                store.append_session_info(name);
                true
            }
            None => false,
        };
        store.rewrite().map_err(|error| error.to_string())?;
        Ok(folded)
    }

    /// The RLM children's notifiers: the delete boundary invalidates the context-tree
    /// cache's rows for the deleted child immediately (the background refresh would
    /// otherwise keep its last row), and child updates reach the clients as
    /// `rlm_child_update` frames.
    fn wire_child_notifiers(&self) {
        let Some(children) = self
            .agent_engine
            .as_ref()
            .and_then(|agent_engine| agent_engine.children.as_ref())
        else {
            return;
        };
        let cache = std::sync::Arc::clone(&self.context_tree);
        children.set_delete_notifier(std::sync::Arc::new(move |child_id| {
            cache.invalidate_child(child_id);
        }));
        let core = std::sync::Arc::clone(&self.core);
        let events = self.events.clone();
        children.set_child_update_sink(std::sync::Arc::new(move |mut child| {
            if let Some(parent_id) = core.lock_or_recover().rlm_child_id.clone() {
                child["parentId"] = json!(parent_id);
            }
            crate::user_bash::emit_session_event_frame(
                &core,
                &events,
                json!({ "type": "rlm_child_update", "child": child }),
            );
        }));
    }

    /// (Re)bind the pane reporter from the create payload (the TS
    /// `session_start` hook): resolve the client env the create carried,
    /// start the reporter for this session's Herdr pane when the session
    /// runs in one, and force-publish the current state with this
    /// session's reference. The previous reporter (a replaced session's,
    /// or a respawn's) is dropped here — its task goes silent without a
    /// release, exactly like the TS replacement arm, so it cannot race
    /// this session's reports on the pane.
    pub(super) fn rebind_herdr_reporter(&self, payload: &Value) {
        let client_env: std::collections::BTreeMap<String, String> = payload
            .get("env")
            .cloned()
            .and_then(|env| serde_json::from_value(env).ok())
            .map(|env| crate::herdr::filter_client_env(&env))
            .unwrap_or_default();
        // TS keys the child skip on the SPAWN OVERRIDE ONLY
        // (`sessionOptionsOverride?.rlmDepth`), never the persisted file
        // depth: a resumed subagent file opened as a top-level session
        // still reports for its own pane (the file's depth serves the
        // roster and usage attribution, not the reporter decision).
        let spawned_as_child = payload
            .get("rlmDepth")
            .and_then(Value::as_u64)
            .is_some_and(|depth| depth > 0);
        let (active, session_ref) = {
            let core = self.core.lock_or_recover();
            (core.busy, Worker::herdr_session_ref(&core))
        };
        let reporter = match crate::herdr::HerdrConfig::from_env(&client_env) {
            Some(config) if !spawned_as_child => {
                // The fresh epoch: bumping the shared counter makes the
                // replaced reporter's task drop its queued boundary
                // events instead of flushing them over this session's
                // pane state.
                let generation = self
                    .herdr_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                crate::herdr::HerdrReporter::start(
                    config,
                    session_ref.clone(),
                    generation,
                    std::sync::Arc::clone(&self.herdr_generation),
                )
            }
            None if !spawned_as_child && self.herdr.lock_or_recover().enabled() => {
                // An idempotent re-create that carries no pane identity
                // (e.g. a replay from a client outside a Herdr pane)
                // must not strip the binding an earlier create or an
                // attach installed — the session keeps reporting for its
                // pane (adopt-if-absent, never rebind to nothing).
                return;
            }
            // Not inside a Herdr pane (the no-op reporter), or a live
            // RLM child: subagents share the parent's pane, so their runs
            // must not flip it and their quits must not release it.
            _ => crate::herdr::HerdrReporter::default(),
        };
        reporter.session_started(active, session_ref);
        *self.herdr.lock_or_recover() = reporter;
    }
}

pub(super) fn active_session_id_of(payload: &[u8]) -> String {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("activeSessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

pub(super) fn worker_server_capabilities(agent_dir: &std::path::Path) -> Vec<String> {
    // The factory lane advertises only while its opt-in gate reads
    // enabled (`factory.enabled`, default off).
    crate::factory_activity::advertised_server_capabilities(agent_dir)
}

/// The create payload's session selection and model flags, validated before the create
/// changes anything.
struct CreateRequest<'a> {
    session_path: Option<std::path::PathBuf>,
    no_session: bool,
    name: Option<&'a str>,
    /// An explicit model flag wins over the model a revived session file pins.
    flagged_model: bool,
    /// The wire contract takes validated levels only: an invalid one fails the create
    /// loudly instead of being silently dropped.
    thinking: Option<pa_types::ai::ModelThinkingLevel>,
}

impl<'a> CreateRequest<'a> {
    fn parse(payload: &'a Value) -> std::result::Result<Self, String> {
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .map(paths::expand_tilde)
            .transpose()
            .map_err(|error| error.to_string())?;
        let thinking = match payload.get("thinking") {
            None => None,
            Some(Value::String(level)) => Some(
                pa_ai::models::thinking_level_from_str(level).ok_or_else(|| {
                    format!(
                        "Invalid thinking level \"{level}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                    )
                })?,
            ),
            Some(_) => return Err("Invalid thinking level: expected a string".to_string()),
        };
        Ok(Self {
            session_path,
            no_session: payload
                .get("noSession")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            name: payload.get("name").and_then(Value::as_str),
            flagged_model: payload.get("model").and_then(Value::as_str).is_some(),
            thinking,
        })
    }
}

/// The create's resource overrides, with the sandbox mode validated.
fn create_resources(payload: &Value) -> std::result::Result<CreateSessionResources, String> {
    let resources = CreateSessionResources::deserialize(payload)
        .map_err(|error| format!("Invalid create config: {error}"))?;
    if let Some(mode) = resources
        .sandbox
        .as_deref()
        .filter(|mode| pa_core::os_sandbox::SandboxMode::from_wire(mode).is_none())
    {
        return Err(format!(
            "Invalid sandbox mode \"{mode}\". Valid values: off, read-only, workspace-write"
        ));
    }
    Ok(resources)
}

/// Where and as what the created session runs: its directory, the sessions directory a
/// fresh file lands in, and the RLM recursion and runtime identity (children of an RLM
/// parent run at depth+1; the durable create replays these so a respawned child keeps
/// them).
struct CreatePlacement {
    cwd: String,
    /// A saved session opened with its own explicit cwd pins the run
    /// (upstream #2528); otherwise the branch's recorded `/cwd` wins.
    cwd_override: bool,
    session_dir: std::path::PathBuf,
    rlm_depth: Option<u32>,
    rlm_max_depth: Option<u32>,
    parent_session_path: Option<String>,
    runtime: SubagentRuntime,
    factory_child: bool,
    thinking: Option<String>,
    /// Verification seam (TS children inherit the parent's `sessionConfig`): a scripted
    /// parent passes its children's engine file down; product creates carry `None`.
    child_script: Option<String>,
}

impl CreatePlacement {
    fn parse(payload: &Value, agent_dir: &std::path::Path) -> std::result::Result<Self, String> {
        let session_dir = match payload.get("sessionDir").and_then(Value::as_str) {
            Some(dir) => paths::expand_tilde(dir),
            None => paths::sessions_dir(agent_dir),
        }
        .map_err(|error| error.to_string())?;
        let (rlm_depth, rlm_max_depth) = create_payload_rlm_depth(payload)?;
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        Ok(Self {
            cwd: payload
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("/")
                .to_string(),
            cwd_override: payload.get("cwdOverride").and_then(Value::as_bool) == Some(true),
            session_dir,
            rlm_depth,
            rlm_max_depth,
            parent_session_path: text("parentSessionPath"),
            runtime: SubagentRuntime::of_create(payload),
            factory_child: payload
                .get("runtimeMetadata")
                .is_some_and(|metadata| metadata.get("factory").is_some()),
            thinking: text("thinking"),
            child_script: text("childScript"),
        })
    }
}

/// The subagent runtime identity (TS `runtimeMetadata` on the create command): the child
/// id and the parent's live/persisted ids ride the session summaries so the roster can key
/// children `parentPath#childId` like TS `rosterAgentIdForSummary`. The semantic spawn
/// origin (TS `semanticParentSessionId` + `semanticSpawnedByRequestId`) exists only for
/// this arm: a resumed saved subagent file is a top-level runtime and spawns no edge.
#[derive(Default)]
struct SubagentRuntime {
    rlm_child_id: Option<String>,
    parent_active_session_id: Option<String>,
    parent_session_id: Option<String>,
    semantic_spawn: Option<crate::engine::SemanticSpawnOrigin>,
}

impl SubagentRuntime {
    fn of_create(payload: &Value) -> Self {
        let Some(metadata) = payload
            .get("runtimeMetadata")
            .filter(|metadata| metadata.get("kind").and_then(Value::as_str) == Some("subagent"))
        else {
            return Self::default();
        };
        let text = |key: &str| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let parent_session_id = text("parentSessionId");
        Self {
            rlm_child_id: text("rlmChildId"),
            parent_active_session_id: text("parentActiveSessionId"),
            parent_session_id: parent_session_id.clone(),
            semantic_spawn: Some(crate::engine::SemanticSpawnOrigin {
                parent_session_id,
                spawned_by_request_id: payload
                    .get("spawnedByRequestId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
        }
    }
}

/// The session store a create opened, and what its arm leaves to the later steps.
struct OpenedStore {
    store: SessionFile,
    /// The arm OPENED an existing session file: `is_continuing` reads this arm fact,
    /// never an existence check.
    continuing: bool,
    /// The service tier the reopened file pins.
    #[expect(
        clippy::option_option,
        reason = "`None`: the file pins no tier (the settings default applies); `Some(None)`: it pins none"
    )]
    restored_tier: Option<Option<pa_types::ai::ServiceTier>>,
    /// The creation prefix a fresh arm defers until after the startup scope registers, so
    /// a fresh `--models` session persists the scoped startup pick.
    fresh_prefix: FreshPrefixPlan,
    /// The directory the reopened branch recorded with `/cwd`.
    branch_cwd: Option<String>,
}

impl OpenedStore {
    fn fresh(store: SessionFile, fresh_prefix: FreshPrefixPlan) -> Self {
        Self {
            store,
            continuing: false,
            restored_tier: None,
            fresh_prefix,
            branch_cwd: None,
        }
    }
}

/// Installs a session file's continuous trace upload under the create's trace consent.
type TraceInstall<'a> = dyn Fn(&std::path::Path) -> Option<Arc<pa_core::agent_traces::ContinuousTraceUpload>>
    + Sync
    + 'a;

/// The wire name of a queue mode setting.
fn queue_mode_wire(mode: pa_core::settings::QueueModeSetting) -> &'static str {
    match mode {
        pa_core::settings::QueueModeSetting::All => "all",
        pa_core::settings::QueueModeSetting::OneAtATime => "one-at-a-time",
    }
}

/// Persist the interrupted compaction's disclosure row unless the transcript already
/// carries it; returns whether the row is durable.
fn disclose_interrupted_compaction(
    store: &mut SessionFile,
    disclosure: &crate::compaction::InterruptedCompactionDisclosure,
) -> bool {
    let already_disclosed = store
        .entries()
        .iter()
        .any(|entry| entry.type_ == "custom_message" && entry.fields == disclosure.row);
    already_disclosed
        || store
            .persist_entry_at(
                "custom_message",
                disclosure.row.clone(),
                &disclosure.declared_at,
            )
            .is_ok()
}

/// RLM depth fields of a create payload: `(depth, max_depth)`. Values must
/// fit a u32; anything else fails the create instead of silently truncating.
fn create_payload_rlm_depth(payload: &Value) -> Result<(Option<u32>, Option<u32>), String> {
    fn parse(payload: &Value, key: &str) -> Result<Option<u32>, String> {
        match payload.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .map(Some)
                .ok_or_else(|| format!("create {key} must be a non-negative integer")),
        }
    }
    let depth = parse(payload, "rlmDepth")?;
    let max_depth = parse(payload, "rlmMaxDepth")?;
    Ok((depth, max_depth))
}

/// Creation prefix for a daemon-hosted session file: fresh files record all three changes;
/// a reopened session records them only when no earlier entry did. Which fresh create
/// arm still owes its creation prefix (the prefix runs after the startup scope registers).
#[derive(Clone, Copy)]
enum FreshPrefixPlan {
    /// Nothing pending: the continuing arm wrote its own prefix before
    /// the scope.
    None,
    /// The in-memory `noSession` create: the prefix stays in memory, no
    /// rewrite.
    InMemory,
    /// A file-backed fresh session: prefix, `active` state, the optional
    /// single-write name fold, then one rewrite.
    PathBacked { fold_name: bool },
}

fn append_creation_prefix(
    store: &mut SessionFile,
    engine: &dyn SessionEngine,
    agent_dir: &std::path::Path,
    cwd: &str,
    fresh: bool,
) {
    let has_thinking_entry = store.has_thinking_level();
    let has_service_tier_entry = store.has_service_tier();
    let thinking_level = engine
        .effective_thinking_level()
        .unwrap_or_else(|| "off".to_string());
    if fresh {
        if let Some((provider, model_id)) = engine.creation_model() {
            store.append_model_change(&provider, &model_id);
        }
        store.append_thinking_level_change(&thinking_level);
    } else if !has_thinking_entry {
        store.append_thinking_level_change(&thinking_level);
    }
    if fresh || !has_service_tier_entry {
        let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
        let service_tier = settings.get_default_service_tier();
        store.append_entry(
            "service_tier_change",
            json!({ "serviceTier": service_tier }),
        );
    }
}

#[cfg(test)]
#[path = "create_collapse_tests.rs"]
mod create_collapse_tests;
