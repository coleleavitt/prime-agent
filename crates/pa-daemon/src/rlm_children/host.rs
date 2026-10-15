//! The host adapter concern: the `RlmSubagentHost` wire surface
//! (`spawn`, `create_session`, `list_subagents`, `interrupt_subagent`, `delete_subagent`,
//! `collect`, `rename`) over the supervisor's child-sessions registry,
//! with the spawn-admission helpers only this surface uses.
use pa_types::sync::MutexExt;

use super::{
    Arc,
    ChildCloseReason,
    ChildRecord,
    Context,
    DaemonCommand,
    Duration,
    INTERRUPT_RUN_MARKER,
    Instant,
    KILL_TIMEOUT_MS,
    Mutex,
    Path,
    PathBuf,
    RENAME_TIMEOUT_MS,
    Result,
    RlmChildResult,
    RlmCreateSessionHandle,
    RlmCreateSessionRequest,
    RlmDeleteSubagentResult,
    RlmHostFuture,
    RlmInterruptSubagentResult,
    RlmSpawnHandle,
    RlmSpawnRequest,
    RlmSubagentEntry,
    RlmSubagentHost,
    SpawnNameReservationGuard,
    SupervisorChildSessions,
    SupervisorChildSessionsInner,
    Value,
    WATCH_SETTLE_GRACE_MS,
    assert_thinking_supported,
    bail,
    create_default_rlm_subagent_session_name,
    json,
    now_ms,
    resolve_child_model,
    rlm_child_label,
    rlm_depth_limit_error,
    spawn_name_unavailable_error,
};

/// Resolve the child model with the daemon `allowedModels` allowlist enforced, refusing loudly with
/// the typed error and the `model refused` event; the settings read runs on the blocking pool.
async fn resolve_child_model_allowlisted(
    this: &SupervisorChildSessionsInner,
    reference: Option<&str>,
    surface: &'static str,
    target: &str,
) -> Result<String> {
    let identity = this.identity.lock_or_recover().clone();
    let cwd = identity.cwd.clone().unwrap_or_else(|| "/".to_string());
    let load_cwd = cwd.clone();
    let agent_dir = this.agent_dir.clone();
    let allowlist = tokio::task::spawn_blocking(move || {
        crate::model_allowlist::load(Path::new(&load_cwd), &agent_dir)
    })
    .await
    .context("the allowlist load task join failed")?;
    match resolve_child_model(
        &this.agent_dir,
        reference,
        identity.model.as_deref(),
        target,
        &allowlist,
    ) {
        Ok(model) => Ok(model),
        Err(error) => {
            if let Some(refusal) = error.downcast_ref::<pa_core::models::ModelAllowlistRefusal>() {
                this.model_refusal_telemetry.note_refused(
                    surface,
                    &refusal.selector,
                    Path::new(&cwd),
                );
            }
            Err(error)
        }
    }
}

/// Whether the collect's stability grace may re-clear the settle verdict it
/// watched land: only a verdict no terminal claim has taken. The settle
/// funnel (the watcher's notice claim plus its `settled` latch), a cancel,
/// a delete, and a parent close each claim the record and own their
/// verdict — a busy-again child after any of them is a follow-up turn
/// (delayed messaging) that keeps the settled result, and un-setting it
/// would leave a `running` record no watcher owns, quiescence still
/// settled, and a parent already told the run completed (TS never un-settles
/// a run after its task's `finally`). Only the unclaimed verdict — the
/// admission-to-run hand-off misread the grace exists to un-settle — is
/// re-cleared for re-reading.
pub(super) fn collect_grace_may_reclear(record: &ChildRecord) -> bool {
    !record.settled && !record.notice_delivered
}

/// The `rlm.create_session` launch env (TS `createRlmRootSession`'s
/// minimal map: `collectDaemonLaunchEnv({ PATH })` plus the provider's
/// env API-key var plus `PRIME_TEAM_ID` for prime-inference).
fn create_session_launch_env(model: &str) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    let provider = model
        .split_once('/')
        .map_or(model, |(provider, _)| provider);
    if let Some(env_key) =
        pa_ai::env_api_keys::find_env_keys(provider).and_then(|keys| keys.first().cloned())
    {
        if let Ok(value) = std::env::var(&env_key) {
            env.insert(env_key, value);
        }
    }
    if provider == "prime-inference" {
        if let Ok(team) = std::env::var("PRIME_TEAM_ID") {
            env.insert("PRIME_TEAM_ID".to_string(), team);
        }
    }
    env
}

impl RlmSubagentHost for SupervisorChildSessions {
    fn spawn(&self, request: RlmSpawnRequest) -> RlmHostFuture<RlmSpawnHandle> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let identity = this.identity.lock_or_recover().clone();
            if identity.rlm_depth >= identity.rlm_max_depth {
                bail!(rlm_depth_limit_error(
                    identity.rlm_depth,
                    identity.rlm_max_depth
                ));
            }
            let child_id = format!("sub-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            let name = request.name.clone().unwrap_or_else(|| {
                create_default_rlm_subagent_session_name(&request.prompt, &child_id)
            });
            // A requested name is reserved before the first await and held
            // until the admission settles, so parallel same-name spawns
            // cannot both register; a default name never reserves.
            let _reservation = request
                .name
                .is_some()
                .then(|| {
                    if !this.reserve_spawn_name(&name) {
                        return Err(spawn_name_unavailable_error(&name, identity.rlm_depth + 1));
                    }
                    Ok(SpawnNameReservationGuard {
                        inner: Arc::clone(&this),
                        name: name.clone(),
                    })
                })
                .transpose()?;
            let admission = async {
                this.assert_name_available(&name, identity.rlm_depth + 1)
                    .await?;
                let model = resolve_child_model_allowlisted(
                    &this,
                    request.model.as_deref(),
                    "spawn",
                    "subagent",
                )
                .await?;
                assert_thinking_supported(&this.agent_dir, request.thinking.as_deref(), &model)?;
                let thinking = request.thinking.as_deref().or(identity.thinking.as_deref());
                let child_dir = this.child_session_dir(&child_id, &identity)?;
                let cwd = identity.cwd.clone().unwrap_or_else(|| "/".to_string());
                let mut runtime_metadata = json!({
                    "kind": "subagent",
                    "rlmChildId": child_id,
                    "parentActiveSessionId": this.parent_active_session_id,
                    "rlmDepth": identity.rlm_depth + 1,
                    "createdAt": now_ms(),
                });
                // The grant the spawn drew funds the child (upstream #1192).
                if let Some(grant) = request.token_budget {
                    runtime_metadata["rlmTokenAllowance"] = json!(grant);
                }
                let created = match this
                    .create_child(
                        &child_id,
                        Some(&name),
                        Some(&request.prompt),
                        identity.rlm_depth + 1,
                        &model,
                        thinking,
                        &cwd,
                        &child_dir,
                        request.spawned_by_request_id.as_deref(),
                        Some(runtime_metadata),
                        &identity,
                        // TS runs `rlm.spawn` children in-process
                        // inside the parent worker, so they share its
                        // env; the Rust child is a separate worker
                        // process, so the spawn forwards the parent
                        // worker's env (filtered) instead.
                        Some(pa_types::daemon::launch_env::collect_process_launch_env()),
                    )
                    .await
                {
                    Ok(created) => created,
                    Err(error) => {
                        // No child exists to own the dir made for it.
                        this.discard_ephemeral_child_dir(&child_id);
                        return Err(error);
                    }
                };
                let record = ChildRecord {
                    rlm_child_id: child_id.clone(),
                    session_name: created.session_name.clone().unwrap_or_else(|| name.clone()),
                    active_session_id: created.active_session_id.clone(),
                    session_id: created.session_id.clone(),
                    session_dir: created.session_dir.clone(),
                    model: model.clone(),
                    label: rlm_child_label(&request.prompt),
                    started_at_ms: now_ms(),
                    settled_status: None,
                    settled: false,
                    answer_preview: None,
                    answer_text: None,
                    answer_captured: false,
                    replied_since_task: false,
                    interrupted: false,
                    notice_delivered: false,
                    prompt_admitted: false,
                    error: None,
                    closed_by_parent: false,
                    retirement_finished: false,
                    retirement_notified: false,
                    retirement_roster_emitted: false,
                    usage_registration_forgotten: false,
                    session_file: created.session_file.clone(),
                    attributed_rows: Some(0),
                    pending_usage_report: None,
                    usage_watch_live: false,
                    usage_rearm: false,
                    emit_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    last_emitted_status: None,
                    rename_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                };
                let record = Arc::new(Mutex::new(record));
                this.children.lock().await.push(Arc::clone(&record));
                this.refresh_running().await;
                this.emit_child_update(&record).await;
                anyhow::Ok((record, created, model))
            }
            .await;
            let (record, created, model) = admission?;
            // The task prompt runs detached from the spawn admission: the
            // handle returns at registration and the child's first turn
            // starts after the parent's continuation request is in flight.
            let watcher_this = Arc::clone(&this);
            let watcher_record = Arc::clone(&record);
            let prompt = request.prompt.clone();
            let plan_mode = request.plan_mode;
            // TS `spawnMessage`: the kickoff rides prompt admission as
            // the parent's `agent_message` row, so the child renders a
            // parent message and the model reads the
            // "[task from parent]" label.
            let kickoff_content = format!("[task from parent]\n\n{prompt}");
            let kickoff_row_id = format!("spawn:{child_id}");
            let mut parent_endpoint = json!({
                "activeSessionId": this.parent_active_session_id,
            });
            if let Some(session_id) = &identity.session_id {
                parent_endpoint["sessionId"] = json!(session_id);
            }
            if let Some(name) = this.parent_session_name.lock_or_recover().clone() {
                parent_endpoint["sessionName"] = json!(name);
            }
            let kickoff_row =
                pa_core::session_engine::agent_messaging::create_agent_session_message_row(
                    &pa_core::session_engine::agent_messaging::AgentSessionMessageRowPayload {
                        id: &kickoff_row_id,
                        prompt: &kickoff_content,
                        message: &prompt,
                        from: &parent_endpoint,
                        from_relationship: Some(
                            pa_core::session_engine::agent_messaging::AgentFamilyRelationship::Parent,
                        ),
                        // TS `spawnMessage` carries no `target`.
                        target: None,
                        timestamp: now_ms(),
                    },
                );
            let child_active_session_id = created.active_session_id.clone();
            let child_session_file = created.session_file.clone();
            let child_log_id = child_id.clone();
            // Capture the current turn boundary before detaching: spawn
            // admission happens mid-turn, so the parent's continuation
            // request reaches the provider first (see `wait_turn_done`).
            let turn_generation = *this.turn_done.subscribe().borrow();
            tokio::spawn(async move {
                watcher_this.wait_turn_done(turn_generation).await;
                // The parent closed before the prompt admitted: the
                // child is closed with it, so the detached task prompt
                // never fires.
                if watcher_record.lock().await.closed_by_parent {
                    return;
                }
                watcher_record.lock().await.prompt_admitted = true;
                // A child spawned during plan mode starts in it: the session
                // command queues ahead of the task prompt (durable in the
                // child's file, and its kernel guard arms first). A child
                // that cannot enter plan mode never runs the task.
                if plan_mode {
                    if let Err(error) = watcher_this
                        .prompt_child(&child_active_session_id, "/plan on", &[], None)
                        .await
                    {
                        let _ = watcher_this
                            .kill_child(&child_active_session_id, ChildCloseReason::Killed)
                            .await;
                        watcher_this
                            .settle_failed(
                                &watcher_record,
                                format!("could not start the child in plan mode: {error:#}"),
                                super::lifecycle::FailedArm::Prompt,
                            )
                            .await;
                        return;
                    }
                }
                if let Err(error) = watcher_this
                    .prompt_child(
                        &child_active_session_id,
                        &kickoff_content,
                        &[],
                        Some(&kickoff_row),
                    )
                    .await
                {
                    // The route can fail ambiguously around a worker
                    // replacement. The durable session file arbitrates: a
                    // kickoff row in the file landed (re-sending would
                    // duplicate the first turn); one retry is safe.
                    let landed = session_file_carries_spawn_kickoff(
                        child_session_file.as_deref(),
                        &kickoff_row_id,
                    );
                    let retried = if landed {
                        Ok(())
                    } else {
                        watcher_this
                            .prompt_child(
                                &child_active_session_id,
                                &kickoff_content,
                                &[],
                                Some(&kickoff_row),
                            )
                            .await
                    };
                    if let Err(retry_error) = retried {
                        eprintln!(
                            "pa-daemon: RLM child task prompt failed for {child_log_id}: {error:#}; retry failed: {retry_error:#}"
                        );
                        let _ = watcher_this
                            .kill_child(&child_active_session_id, ChildCloseReason::Killed)
                            .await;
                        watcher_this
                            .settle_failed(
                                &watcher_record,
                                format!("{retry_error:#}"),
                                super::lifecycle::FailedArm::Prompt,
                            )
                            .await;
                        return;
                    }
                }
                watcher_this.watch_child_settle(&watcher_record).await;
            });
            Ok(RlmSpawnHandle {
                rlm_child_id: child_id,
                name,
                session_dir: created.session_dir,
                model,
            })
        })
    }

    fn create_session(
        &self,
        request: RlmCreateSessionRequest,
    ) -> RlmHostFuture<RlmCreateSessionHandle> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let identity = this.identity.lock_or_recover().clone();
            if identity.rlm_depth != 0 {
                bail!("rlm.create_session is available only from a depth-0 session");
            }
            let model = resolve_child_model_allowlisted(
                &this,
                request.model.as_deref(),
                "create_session",
                "top-level session",
            )
            .await?;
            assert_thinking_supported(&this.agent_dir, request.thinking.as_deref(), &model)?;
            // A depth-0 resident session is created exactly like a client
            // `create`: the shared sessions dir and the requested cwd.
            let cwd = match &request.cwd {
                Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
                Some(cwd) => Path::new(identity.cwd.as_deref().unwrap_or("/")).join(cwd),
                None => PathBuf::from(identity.cwd.clone().unwrap_or_else(|| "/".to_string())),
            };
            let sessions_dir = crate::paths::sessions_dir(&this.agent_dir)?;
            std::fs::create_dir_all(&sessions_dir)
                .with_context(|| format!("create sessions dir {}", sessions_dir.display()))?;
            let thinking = request.thinking.as_deref().or(identity.thinking.as_deref());
            let created = this
                .launch_child(
                    "root",
                    request.name.as_deref(),
                    &request.prompt,
                    0,
                    &model,
                    thinking,
                    &cwd.to_string_lossy(),
                    &sessions_dir,
                    None,
                    &identity,
                    // The TS minimal map for a depth-0 root (the
                    // env-key heuristic is stated in the PR).
                    Some(create_session_launch_env(&model)),
                )
                .await?;
            // The TS create-path summary validation: a resident depth-0
            // session must never report another depth.
            if created.summary_rlm_depth.is_some_and(|depth| depth != 0) {
                bail!("Daemon supervisor returned an invalid depth-0 session summary");
            }
            Ok(RlmCreateSessionHandle {
                active_session_id: created.active_session_id.clone(),
                session_id: created
                    .session_id
                    .clone()
                    .unwrap_or_else(|| created.active_session_id.clone()),
                name: created
                    .session_name
                    .clone()
                    .unwrap_or_else(|| created.active_session_id.clone()),
                session_file: created.session_file.unwrap_or_default(),
                model,
            })
        })
    }

    fn list_subagents(&self) -> RlmHostFuture<Vec<RlmSubagentEntry>> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let records = this.children.lock().await.clone();
            let mut entries = Vec::with_capacity(records.len());
            for record in &records {
                // The settle watcher owns worker refreshes. A roster read is a
                // snapshot and must not queue behind a long supervisor request.
                let record = record.lock().await;
                entries.push(SupervisorChildSessions::entry(&record));
            }
            Ok(entries)
        })
    }

    /// Abort only the child worker's in-flight run (the `abort` route with
    /// the `interruptRun` marker: no queue suspension, no queue withdraw, no
    /// compaction abort), keeping the child registered with its session,
    /// descendants, and follow-up admission. The record lock rides the
    /// round trip so the settle watcher cannot claim the no-reply notice
    /// between the abort and the interrupted mark.
    fn interrupt_subagent(&self, target: String) -> RlmHostFuture<RlmInterruptSubagentResult> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            // Shared deadline for all operations to avoid nested timeout mismatch.
            let deadline = Instant::now() + Duration::from_millis(KILL_TIMEOUT_MS);

            let mut matches = this.matching_records(&target).await;
            let record = match matches.len() {
                0 => return Ok(RlmInterruptSubagentResult::not_found()),
                1 => matches.remove(0),
                _ => bail!(
                    "RLM subagent selector \"{target}\" is ambiguous in the current parent session"
                ),
            };
            let mut record = record.lock().await;
            let entry = SupervisorChildSessions::entry(&record);
            if entry.is_terminal() {
                return Ok(RlmInterruptSubagentResult::resolved(
                    entry, /*aborted*/ false,
                ));
            }
            let command = DaemonCommand::Abort {
                id: None,
                active_session_id: record.active_session_id.clone(),
                rest: serde_json::Map::from_iter([(INTERRUPT_RUN_MARKER.to_string(), json!(true))]),
            };
            let aborted = match this.command_by(&command, deadline, 1_000).await {
                Ok(data) => data
                    .get("interrupted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                // A passive child (its worker left after settling) has no
                // run to abort.
                Err(error) if format!("{error:#}").starts_with("Unknown active session:") => false,
                Err(error) => {
                    return Err(error).with_context(|| format!("interrupt RLM child \"{target}\""));
                }
            };
            if aborted && record.settled_status.is_none() {
                record.interrupted = true;
            }
            Ok(RlmInterruptSubagentResult::resolved(entry, aborted))
        })
    }

    fn delete_subagent(&self, target: String) -> RlmHostFuture<RlmDeleteSubagentResult> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            if !this.replay_retired_usage(Some(&target)).await {
                bail!("RLM child usage remains owed after retirement; retry deletion");
            }
            // Shared deadline for all serial operations in this handler,
            // avoiding nested timeout budget mismatch.
            let deadline = Instant::now() + Duration::from_millis(KILL_TIMEOUT_MS);
            // Selector errors surface unwrapped (the TS message is the
            // product surface); only the kill below gets a delete context.
            let record = match this.resolve_record(&target, "subagent").await {
                Ok(record) => record,
                Err(miss) => {
                    // Retirement is idempotent (the M5 class): a selector
                    // whose delete receipt already returned answers from
                    // the tombstone instead of erroring, so a re-delete of
                    // a settled child (an operator retire, the factory's
                    // cancel pass) never reports the slot as still held.
                    let matches = this
                        .deleted_children
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .values()
                        .filter(|deleted| deleted.matches(&target))
                        .cloned()
                        .collect::<Vec<_>>();
                    match matches.len() {
                        0 => return Err(miss),
                        1 => {
                            let deleted = &matches[0];
                            return Ok(RlmDeleteSubagentResult {
                                subagent: RlmSubagentEntry {
                                    rlm_child_id: deleted.rlm_child_id.clone(),
                                    active_session_id: Some(deleted.active_session_id.clone()),
                                    session_id: deleted.session_id.clone(),
                                    session_name: deleted.session_name.clone(),
                                    session_dir: deleted.session_dir.clone(),
                                    status: deleted.status,
                                    activity: None,
                                    tool_use_count: None,
                                    duration_ms: Some(
                                        now_ms().saturating_sub(deleted.started_at_ms),
                                    ),
                                    answer_preview: deleted.answer_preview.clone(),
                                    replied_since_task: None,
                                    progress_note: None,
                                    label: None,
                                    last_activity_at: Some(deleted.started_at_ms),
                                    activity_stale_ms: None,
                                },
                                outcome: Some("deleted"),
                            });
                        }
                        _ => bail!(
                            "RLM subagent selector \"{target}\" is ambiguous in the current parent session"
                        ),
                    }
                }
            };
            let rename_lock = record.lock().await.rename_lock.clone();
            let _rename_guard = rename_lock.lock().await;
            let already_terminated = this.has_retired_usage(&record);
            let no_longer_matches = {
                let record = record.lock().await;
                (record.closed_by_parent && !already_terminated) || !record.matches(&target)
            };
            if no_longer_matches {
                bail!("No direct RLM subagent matches \"{target}\" in the current parent session");
            }
            let active_session_id = record.lock().await.active_session_id.clone();
            // Kill first: a failed kill keeps the child tracked; the
            // `rlmLedgerDelete` marker tells the supervisor this kill is a
            // delete, never a plain stop.
            let record_guard = record.lock().await;
            let command = DaemonCommand::Kill {
                id: None,
                active_session_id: active_session_id.clone(),
                rest: serde_json::Map::from_iter([
                    ("rlmLedgerDelete".to_string(), json!("user")),
                    ("rlmChildId".to_string(), json!(record_guard.rlm_child_id)),
                ]),
            };
            drop(record_guard);
            let was_running = record.lock().await.settled_status.is_none();
            // Capture before the kill: a deleted running child's durable
            // rows are its last observable spend on the parent side.
            if !already_terminated {
                this.emit_child_usage(&record).await;
                // Use remaining time from the shared deadline instead of
                // an independent timeout to avoid nested timeout mismatch.
                this.command_by(&command, deadline, 1_000)
                    .await
                    .with_context(|| format!("kill RLM child \"{target}\""))?;
                this.retain_retired_usage(&record, super::RetirementIntent::Delete { was_running });
                // Observe any rows that landed before the acknowledged kill.
                this.emit_child_usage(&record).await;
            }
            // The final observation landed: the registration drops.
            let usage_persisted = this.forget_child_usage(&record).await;
            this.finish_retirement(&record).await;
            let entry = {
                let record = record.lock().await;
                SupervisorChildSessions::entry(&record)
            };
            if usage_persisted {
                this.remove_retired_usage(&record);
            }
            if !usage_persisted {
                bail!("RLM child usage remains owed after retirement; retry deletion");
            }
            Ok(RlmDeleteSubagentResult {
                subagent: entry,
                outcome: Some("deleted"),
            })
        })
    }

    fn collect(&self, targets: Vec<String>, timeout_ms: u64) -> RlmHostFuture<Vec<RlmChildResult>> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            // Resolve targets outside the registry lock: `resolve_record`
            // takes it too, and the tokio mutex is not re-entrant.
            let mut records: Vec<Arc<Mutex<ChildRecord>>> = if targets.is_empty() {
                this.children.lock().await.clone()
            } else {
                Vec::with_capacity(targets.len())
            };
            // A target whose delete receipt already returned resolves
            // immediately to its settled cancelled envelope; unknown
            // selectors keep erroring (a respawned freed name wins first).
            let mut deleted_results: Vec<RlmChildResult> = Vec::new();
            for target in &targets {
                let record = match this.resolve_record(target, "child").await {
                    Ok(record) => record,
                    Err(miss) => {
                        let matches = this
                            .deleted_children
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .values()
                            .filter(|deleted| deleted.matches(target))
                            .cloned()
                            .collect::<Vec<_>>();
                        match matches.len() {
                            0 => return Err(miss),
                            1 => deleted_results
                                .push(SupervisorChildSessions::deleted_collect_result(&matches[0])),
                            _ => bail!(
                                "RLM child selector \"{target}\" is ambiguous in the current parent session"
                            ),
                        }
                        continue;
                    }
                };
                records.push(record);
            }
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut results = Vec::with_capacity(records.len());
            for record in &records {
                // Whether the record was already settled when this collect
                // began: only a settle that happens INSIDE this collect (it
                // was running on entry) gets the stability grace below —
                // records settled on a previous collect keep their result
                // (TS completed children stay readable until deleted).
                let settled_on_entry = record.lock().await.settled_status.is_some();
                this.refresh_record(record).await;
                // A settled child that is busy again runs a follow-up turn:
                // re-arm usage observation (the task-run watcher retired at
                // its settle).
                if record.lock().await.settled_status.is_some() {
                    let active_session_id = record.lock().await.active_session_id.clone();
                    if matches!(this.child_busy(&active_session_id).await, Ok(true)) {
                        SupervisorChildSessionsInner::arm_usage_watch(&this, record).await;
                    }
                }
                let still_running = record.lock().await.settled_status.is_none();
                if still_running {
                    // Wait inside the shared budget, then re-read the child:
                    // a timeout yields the current snapshot, never an error.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let active_session_id = record.lock().await.active_session_id.clone();
                    this.wait_for_child(&active_session_id, remaining).await;
                    this.refresh_record(record).await;
                }
                // Stability re-check, the watcher's own defense — but only
                // for a settle that happened inside THIS collect (records
                // that were settled on entry keep their result; TS
                // completed children stay readable until deleted). A prompt
                // admitted to an idle worker can read idle once between the
                // admission and the turn pop (the queue snapshot and the
                // busy flag change under different locks on the far side of
                // a socket). The watcher re-checks with a short grace
                // before settling; a collect that settles the same record
                // inside that window must not report it either — the
                // factory executor (and any `rlm.collect` reader) would
                // consume a `done` result with no captured answer and lose
                // the child's output forever (the record recovers, the
                // caller never re-reads). A child that went busy again
                // inside the grace keeps waiting inside the shared budget;
                // one that stays idle is really settled. A settle a
                // terminal claim already owns stays final (the funnel's
                // notice delivered and the watcher retired at its settle,
                // or a cancel, delete, or close claimed the row): the busy
                // child is a follow-up turn that keeps the settled result,
                // and only the unclaimed misread re-clears
                // (`collect_grace_may_reclear`).
                if !settled_on_entry && record.lock().await.settled_status.is_some() {
                    let active_session_id = record.lock().await.active_session_id.clone();
                    tokio::time::sleep(Duration::from_millis(WATCH_SETTLE_GRACE_MS)).await;
                    let busy_again = matches!(this.child_busy(&active_session_id).await, Ok(true));
                    // The gate and the re-clear commit at ONE hold of the
                    // record lock. Two acquisitions left a hand-off gap
                    // where a claim already queued on the lock (the settle
                    // funnel's notice claim, its `settled` latch, a
                    // cancel) ran in between: the gate read the verdict as
                    // unclaimed, the claim took it, and the clear still
                    // stripped it — the funnel latched a record reading
                    // `running`, quiescence saw the latch, the parent held
                    // the terminal notice, and no watcher was left to
                    // re-settle. A claim queued behind this hold lands
                    // after the clear and aborts at its own commit (the
                    // tail's notice claim keeps watching a re-cleared
                    // verdict), so the record is consistent at every
                    // interleave: claimed verdicts keep their status,
                    // unclaimed misreads re-clear.
                    let recleared = {
                        let mut record = record.lock().await;
                        let may = busy_again && collect_grace_may_reclear(&record);
                        if may {
                            record.settled_status = None;
                        }
                        may
                    };
                    if recleared {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if !remaining.is_zero() {
                            this.wait_for_child(&active_session_id, remaining).await;
                        }
                    }
                    // Refresh again after the grace whatever the busy
                    // re-check said: an answer that landed inside the grace
                    // must ride this collect's result (the refresh
                    // re-captures a `None` preview).
                    this.refresh_record(record).await;
                }
                let result = {
                    let record = record.lock().await;
                    SupervisorChildSessions::collect_result(&record)
                };
                results.push(result);
            }
            // Live entries first, the deleted generations' envelopes after.
            results.extend(deleted_results);
            Ok(results)
        })
    }

    fn rename(&self, name: String, session_id: Option<String>) -> RlmHostFuture<String> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            // Resolve the target: the caller's own session (an absent
            // session id, its live active id, or its durable session id),
            // or one direct child by rlm child id, active id, or durable
            // session id ONLY — a child NAME never selects a rename
            // target (TS `renameAgentFamilySession`).
            let identity = this.identity.lock_or_recover().clone();
            let self_target = match &session_id {
                None => true,
                Some(target) => {
                    target == &this.parent_active_session_id
                        || identity.session_id.as_deref() == Some(target.as_str())
                }
            };
            let (active_session_id, record) = if self_target {
                (this.parent_active_session_id.clone(), None)
            } else {
                let target = session_id.clone().unwrap_or_default();
                let mut by_ids = None;
                let mut by_name = false;
                {
                    let children = this.children.lock().await;
                    for candidate in children.iter() {
                        let record = candidate.lock().await;
                        if record.matches_id(&target) {
                            by_ids = Some(Arc::clone(candidate));
                            break;
                        }
                        if record.session_name == target {
                            by_name = true;
                        }
                    }
                }
                match by_ids {
                    Some(record) => {
                        let active_session_id = record.lock().await.active_session_id.clone();
                        (active_session_id, Some(record))
                    }
                    None if by_name => bail!(
                        "rlm.rename session_id \"{target}\" must be the full session id or a child handle, not a session name or id suffix"
                    ),
                    None => bail!(
                        "rlm.rename can only rename the current session or one of its direct children"
                    ),
                }
            };
            // Serialize the daemon command and parent-side record update
            // against another rename or delete of this child.
            let rename_lock = if let Some(record) = &record {
                Some(record.lock().await.rename_lock.clone())
            } else {
                None
            };
            let _rename_guard = if let Some(lock) = &rename_lock {
                Some(lock.lock().await)
            } else {
                None
            };
            if let Some(record) = &record {
                if record.lock().await.closed_by_parent {
                    bail!(
                        "rlm.rename can only rename the current session or one of its direct children"
                    );
                }
            }
            // The rename itself is daemon-owned: the supervisor's live
            // rename route reserves the name, asserts sibling uniqueness
            // across the family, and appends the child's RLM ledger
            // rename. `renamedBy: parent` marks the parent-directed
            // rename so the renamed session's transcript notice names it.
            // The child may be passivated — or a prior wake may have
            // re-keyed its roster row past the record's spawn-time id —
            // so the rename rides the same durable-selector wake retry
            // the child prompts use (the record keeps the spawn-time
            // routing id; only its durable session id still names a
            // passivated child).
            let renamed_by = record.as_ref().map(|_| {
                pa_core::session_engine::agent_messaging::AgentFamilyRelationship::Parent
                    .as_str()
                    .to_string()
            });
            let make_command = |selector: &str| DaemonCommand::Rename {
                id: None,
                active_session_id: selector.to_string(),
                name: name.clone(),
                renamed_by: renamed_by.clone(),
                rest: serde_json::Map::default(),
            };
            this.command_with_durable_wake(
                make_command,
                &active_session_id,
                RENAME_TIMEOUT_MS,
                &format!("rename session \"{active_session_id}\""),
            )
            .await?;
            // A parent-directed rename updates the parent-side record so
            // the roster row, collect/delete selectors, and the parent's
            // name-availability check all stop matching the old name.
            if let Some(record) = record {
                record.lock().await.session_name = name.clone();
            }
            Ok(name)
        })
    }
}

/// Whether the child's durable session file already carries the spawn
/// kickoff row (`agent_message` custom row with `details.id` =
/// `spawn:<child id>`). The session file is the record a worker
/// replacement replays from, so it arbitrates an ambiguous prompt-route
/// failure: a row in the file was durably processed by the dead worker (a
/// re-send would duplicate the first turn), a missing row provably never
/// landed.
fn session_file_carries_spawn_kickoff(session_file: Option<&str>, spawn_row_id: &str) -> bool {
    let Some(path) = session_file.filter(|path| !path.is_empty()) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("custom_message")
                && entry.get("customType").and_then(Value::as_str)
                    == Some(pa_core::session_engine::agent_messaging::AGENT_MESSAGE_CUSTOM_TYPE)
                && entry.pointer("/details/id").and_then(Value::as_str) == Some(spawn_row_id)
        })
}
