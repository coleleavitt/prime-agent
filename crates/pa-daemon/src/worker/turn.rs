//! One agent turn: the runner that admits queued input, drives the
//! engine, and settles the result.
use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;

use super::{
    AgentMessageDigest,
    DaemonOutbound,
    EventPump,
    Lane,
    Map,
    Notify,
    OutboundFrame,
    PromptRequest,
    QueueCheckpoint,
    QueuedItem,
    Result,
    SessionActionSnapshot,
    SessionCore,
    SessionEngine,
    TurnSettle,
    Value,
    WorkerRecoveryJournal,
    checkpoint_queue_recovery,
    compact_action_label,
    create_daemon_event_meta,
    emit_refinement_event_for_session,
    emit_refinement_row,
    gather_delivery_batch,
    json,
    oneshot,
    session_snapshot,
};

mod emit;

pub(super) struct TurnRunner {
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(super) input_pauses: crate::session_input_pause::InputPauseTable,
    /// The prompt-admission registry: an admitted prompt clears when its
    /// turn settles.
    pub(super) prompt_admissions: crate::prompt_admission::WorkerAdmissions,
    pub(super) work_notify: Arc<Notify>,
    pub(super) idle_notify: Arc<Notify>,
    pub(crate) events: Arc<EventPump>,
    pub(super) engine: std::sync::Arc<dyn SessionEngine>,
    pub(super) recovery: Arc<Mutex<Option<WorkerRecoveryJournal>>>,
    pub(super) active_session_id: String,
    pub(super) roster_pushes: crate::roster_activity::RosterPushQueue,
    /// The idle passivation's live-bash gate.
    pub(super) user_bash: std::sync::Arc<crate::user_bash::UserBash>,
    /// The digest lane's counters (swarm PRs C/D): the runner counts model
    /// turns and agent-message ingestion turns for the lane controller.
    pub(super) agent_digest: Arc<AgentMessageDigest>,
    pub(super) passivation: PassivationContext,
    /// The shared pane-reporter slot (the Worker's `herdr` field): the
    /// runner reads it at every boundary so a create-time rebind is always
    /// current.
    pub(super) herdr: std::sync::Arc<std::sync::Mutex<crate::herdr::HerdrReporter>>,
    /// The idle compaction (experimental, off by default).
    pub(super) idle_compaction: super::idle_compaction::IdleCompaction,
}

/// The idle-passivation context on the turn runner: the worker token
/// for the graceful-stop request.
pub(super) struct PassivationContext {
    pub(super) agent_dir: std::path::PathBuf,
    pub(super) link: std::sync::Arc<crate::supervisor_link::SupervisorLink>,
    pub(super) worker_token: String,
    /// The session's scheduled tasks: pending ones keep the worker resident
    /// (the worker is what fires them).
    pub(super) scheduled_tasks: Option<std::sync::Arc<crate::session_tasks::SessionTaskScheduler>>,
}

impl TurnRunner {
    pub(super) async fn run(self) {
        loop {
            let engine = self.engine.clone();
            let item: Option<(Lane, Vec<QueuedItem>)> = {
                let mut core = self.core.lock_or_recover();
                if core.shutdown_requested {
                    drop(core);
                    // The shutdown handler waits on the idle notify before
                    // disposing the kernel; the runner's last chance to fire it.
                    self.idle_notify.notify_waiters();
                    return;
                }
                // A cleared suspension alone must not admit: a manual
                // compaction is a busy state the resume sites do NOT clear.
                if self.input_pauses.paused() || core.queued_input_suspended || core.compacting {
                    core.busy = false;
                    None
                } else if core.steering.front().is_some() {
                    // A fold-refused front item is delivered now: its run
                    // re-admits it the usual way.
                    core.steer_fold.front_refused = false;
                    let items = gather_delivery_batch(&mut core, Lane::Steering);
                    core.running_admission_ids = items
                        .iter()
                        .filter_map(|item| item.admission_id.clone())
                        .collect();
                    core.busy = true;
                    core.abort_requested = false;
                    core.send_now_cut = false;
                    core.retry_abort_requested = false;
                    core.running_tool_calls.clear();
                    Some((Lane::Steering, items))
                } else if core.follow_up.front().is_some() {
                    let items = gather_delivery_batch(&mut core, Lane::FollowUp);
                    core.running_admission_ids = items
                        .iter()
                        .filter_map(|item| item.admission_id.clone())
                        .collect();
                    core.busy = true;
                    core.abort_requested = false;
                    core.send_now_cut = false;
                    core.retry_abort_requested = false;
                    core.running_tool_calls.clear();
                    Some((Lane::FollowUp, items))
                } else {
                    core.busy = false;
                    None
                }
            };
            if let Some((lane, items)) = item {
                // No pickup checkpoint: admission recorded its busy evidence; the
                // settle's `turn_end` verdict parks the session later. The item leaves
                // the queue projection BEFORE its turn starts (a stale row would
                // reject a browse edit).
                let visible_index = items.iter().position(|item| item.queue_visible);
                let anchor = visible_index.map(|index| &items[index]);
                {
                    let mut core = self.core.lock_or_recover();
                    if let Some(anchor) = anchor {
                        core.active_action = Some(crate::types::SessionActionActive {
                            kind: "turn".to_string(),
                            phase: "preparing".to_string(),
                            label: Some(compact_action_label(
                                anchor.preview.as_deref().unwrap_or(&anchor.message),
                            )),
                        });
                    }
                    let snapshot = Self::snapshot_from(&core);
                    drop(core);
                    let _ = self.emit_action_update(&snapshot);
                }
                self.push_roster_delta();
                self.run_turn(engine, lane, items).await;
            } else {
                self.idle_notify.notify_waiters();
                // Every park re-stamps the activity end: the idle-eviction
                // window measures from the TRUE last activity.
                {
                    let mut core = self.core.lock_or_recover();
                    core.last_activity_ms = crate::util::now_ms();
                }
                // A parked parent-owned child releases its kernel with
                // a snapshot flush; the next kernel use revives it.
                self.maybe_release_settled_child_kernel().await;
                // The whole-worker idle passivation (TS's
                // `idleEvictionMinutes` tier, worker-driven): the same park
                // state the kernel release proved, plus the idle clock. The
                // window arms for any idle unattached session under a live
                // threshold; the select's notified arm is the wake path — a
                // queued delivery wins the race and the next park re-arms.
                // The idle compaction (experimental) arms beside it: a
                // parked session compacts once before the prompt cache
                // lapses.
                let passivation = self.idle_passivation_window();
                let compaction = self.idle_compaction_window();
                tokio::select! {
                    () = self.work_notify.notified() => {}
                    () = super::idle_compaction::sleep_for(passivation) => {
                        self.maybe_request_idle_passivation().await;
                    }
                    () = super::idle_compaction::sleep_for(compaction) => {
                        self.maybe_run_idle_compaction().await;
                    }
                }
            }
        }
    }

    /// Release a parked parent-owned child's kernel; the engine owns the
    /// remaining gates. Failure leaves the kernel resident.
    async fn maybe_release_settled_child_kernel(&self) {
        let release = {
            let core = self.core.lock_or_recover();
            core.rlm_depth > 0
                && core.attached_client_ids.is_empty()
                && !core.compacting
                && !core.shutdown_requested
        };
        if release {
            self.engine.release_settled_child_kernel().await;
        }
    }

    /// The idle-eviction window for an unowned session (TS's
    /// `idleEvictionMinutes` consumer, worker-side; TS `canEvictWorker`
    /// reaches roots and children alike): `Some(remaining)` when the
    /// park state holds (unattached, not compacting, not shutting down,
    /// no live background bash, no queued input in the lanes — TS
    /// `isSessionActive`'s pending-prompt-admissions arm) and the
    /// setting is a live threshold; `None` otherwise (attached
    /// sessions, `"off"`, and any state the engine gates would reject
    /// stay parked without a timer). The client-owned refusal is
    /// supervisor-side (the descriptor's `ownerClientId`). The engine gate
    /// (`SessionEngine::can_passivate_worker`) is re-checked at the fire
    /// inside [`Self::maybe_request_idle_passivation`] — the
    /// fresh-snapshot fence — so this window only decides whether to
    /// arm.
    pub(super) fn idle_passivation_window(&self) -> Option<std::time::Duration> {
        let (attached, compacting, shutdown, queued, last_activity, cwd) = {
            let core = self.core.lock_or_recover();
            (
                core.attached_client_ids.is_empty(),
                core.compacting,
                core.shutdown_requested,
                // Queued work lives only on this resident worker, so
                // the passivation must never discard it.
                !core.steering.is_empty()
                    || !core.follow_up.is_empty()
                    || !core.pending_next_turn.is_empty()
                    // A paused pump or an input pause keeps the session resident:
                    // a revival starts un-suspended and would accept a post-abort prompt.
                    || core.queued_input_suspended
                    || self.input_pauses.paused(),
                core.last_activity_ms,
                core.cwd.clone(),
            )
        };
        if !attached || compacting || shutdown || queued {
            return None;
        }
        // A live background bash keeps the worker resident (a snapshot
        // cannot resurrect a live process).
        if self.user_bash.is_running() {
            return None;
        }
        // So does a scheduled task: the worker is what fires it.
        if self
            .passivation
            .scheduled_tasks
            .as_ref()
            .is_some_and(|tasks| tasks.has_pending())
        {
            return None;
        }
        let settings =
            pa_core::settings::SettingsManager::create(&cwd, &self.passivation.agent_dir);
        let minutes = match settings.get_idle_eviction() {
            pa_core::settings::IdleEviction::Off => return None,
            pa_core::settings::IdleEviction::Minutes(minutes) => minutes,
        };
        let now = crate::util::now_ms();
        let idle_ms = now.saturating_sub(last_activity);
        let threshold_ms = minutes.saturating_mul(60_000);
        Some(std::time::Duration::from_millis(
            threshold_ms.saturating_sub(idle_ms),
        ))
    }

    /// Re-check the gates on a fresh snapshot, then ask the supervisor
    /// for the graceful stop. A failed request leaves the worker resident.
    pub(super) async fn maybe_request_idle_passivation(&self) {
        let (attached, compacting, shutdown, queued, last_activity, cwd) = {
            let core = self.core.lock_or_recover();
            (
                core.attached_client_ids.is_empty(),
                core.compacting,
                core.shutdown_requested,
                !core.steering.is_empty()
                    || !core.follow_up.is_empty()
                    || !core.pending_next_turn.is_empty()
                    || core.queued_input_suspended
                    || self.input_pauses.paused(),
                core.last_activity_ms,
                core.cwd.clone(),
            )
        };
        // Any state change since the window armed cancels the
        // passivation.
        if !attached || compacting || shutdown || queued {
            return;
        }
        if self.user_bash.is_running() {
            return;
        }
        if self
            .passivation
            .scheduled_tasks
            .as_ref()
            .is_some_and(|tasks| tasks.has_pending())
        {
            return;
        }
        let settings =
            pa_core::settings::SettingsManager::create(&cwd, &self.passivation.agent_dir);
        let minutes = match settings.get_idle_eviction() {
            pa_core::settings::IdleEviction::Off => return,
            pa_core::settings::IdleEviction::Minutes(minutes) => minutes,
        };
        // The idle threshold still holds on the fresh clock.
        if crate::util::now_ms().saturating_sub(last_activity) < minutes.saturating_mul(60_000) {
            return;
        }
        // The engine gate's one definition lives with the engine
        // (`SessionEngine::can_passivate_worker`); a failing worker stays resident.
        if !self.engine.can_passivate_worker().await {
            return;
        }
        // The post-await revalidation (the fresh bots' race findings):
        // the engine gate's await opened a window - a bash admitted, a
        // prompt parked in a lane, a replay prefix restored, a manual
        // compaction started, a CLIENT ATTACHED, or the worker's own
        // graceful SHUTDOWN starting during it must all cancel the stop
        // (the shutdown would cancel the compaction and disconnect the
        // new client; a stop ask under a running shutdown races the
        // worker's own exit). The shutdown arm is stricter than TS:
        // daemon-mode.ts `passivateSession` checks `shuttingDown` only
        // BEFORE its fresh-snapshot await, not after it. The bash that
        // started in the window keeps the worker resident exactly like
        // the pre-gate check.
        {
            let core = self.core.lock_or_recover();
            if core.compacting
                || core.shutdown_requested
                || core.queued_input_suspended
                || !core.attached_client_ids.is_empty()
                || !core.steering.is_empty()
                || !core.follow_up.is_empty()
                || !core.pending_next_turn.is_empty()
            {
                return;
            }
        }
        if self.input_pauses.paused() {
            return;
        }
        if self.user_bash.is_running() {
            return;
        }
        let command = serde_json::json!({
            "type": "worker_idle_passivation",
            "workerToken": self.passivation.worker_token,
            "idleMinutes": minutes,
        });
        // The timeout only bounds the ask, not the stop: the supervisor's
        // stop path runs the routed shutdown back into this worker.
        let _ = self
            .passivation
            .link
            .request(command, std::time::Duration::from_secs(30))
            .await;
    }

    /// Push a roster delta after a busy flip, so subscribed clients see
    /// live status; the queue's consumer coalesces any racing flush.
    pub(crate) fn push_roster_delta(&self) {
        self.roster_pushes.push();
    }

    /// One delivery: the first item anchors the turn and the rest ride
    /// as co-delivered user rows of the same run.
    pub(super) async fn run_turn(
        &self,
        engine: std::sync::Arc<dyn SessionEngine>,
        lane: Lane,
        mut items: Vec<QueuedItem>,
    ) {
        let Some((first, batched)) = items.split_first() else {
            return;
        };
        self.emit_turn_event(json!({ "type": "agent_start" }));
        self.emit_turn_event(json!({ "type": "turn_start" }));
        // Claude Code's `showTurnDuration`: capture the wall-clock start at
        // turn-start emission; the settle below computes elapsed and
        // injects the `turn_duration` custom row.
        let turn_started_at = std::time::Instant::now();
        // Claude Code's generated session title: the session's first real
        // prompt names it on a side call, beside the turn.
        self.maybe_generate_title(
            &engine,
            &first.message,
            first.custom_message.as_ref(),
            first.agent_message.as_deref(),
        );
        // The pane reporter's run boundary (TS `agent_start`): working.
        self.herdr.lock_or_recover().run_started();
        engine.on_turn_start();

        let prompt_index = {
            let core = self.core.lock_or_recover();
            core.store
                .as_ref()
                .map_or(0, crate::session_store::SessionFile::message_count)
                / 2
        };
        let request = PromptRequest {
            batch: batched
                .iter()
                .map(|item| crate::engine::PromptBatchRow {
                    text: item.message.clone(),
                    images: item.images.clone(),
                })
                .collect(),
            message: first.message.clone(),
            images: first.images.clone(),
            source: "user".to_string(),
            agent_message_id: None,
            custom_message: first.custom_message.clone(),
        };
        let coalescer = {
            let core = self.core.lock_or_recover();
            Arc::new(crate::streaming::TurnStreamCoalescer::new(
                core.active_session_id.clone(),
                core.generation.clone(),
            ))
        };
        let flusher = {
            let coalescer = Arc::clone(&coalescer);
            let events = self.events.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(crate::streaming::UPDATE_FLUSH_INTERVAL).await;
                    if !coalescer.flush_pending(&events) {
                        break;
                    }
                }
            })
        };
        // Own engine clone, fenced on the session identity it serviced:
        // a branch move or replacement swaps the store mid-review.
        let review_session_id = {
            let core = self.core.lock_or_recover();
            core.store
                .as_ref()
                .map(|store| store.session_id().to_string())
                .unwrap_or_default()
        };
        let settled_admissions: Vec<String> = items
            .iter()
            .filter_map(|item| item.admission_id.clone())
            .collect();
        let signals = Arc::new(emit::TurnSignals::default());
        let emitter = emit::TurnEmitter::new(emit::EmitContext {
            core: Arc::clone(&self.core),
            engine: Arc::clone(&engine),
            events: self.events.clone(),
            coalescer: Arc::clone(&coalescer),
            herdr: Arc::clone(&self.herdr),
            agent_digest: Arc::clone(&self.agent_digest),
            agent_dir: crate::paths::agent_dir().unwrap_or_default(),
            signals: Arc::clone(&signals),
            ingestion_turn: first.agent_message.is_some(),
            batch_len: items.len(),
        });
        // `agent.prompt`: the delivered turn, a child of the submitting
        // client's span when the prompt carried its `traceparent`; the
        // engine's turn, request and tool spans nest under it.
        let prompt_span = tracing::info_span!(
            target: "pa_daemon::turn",
            "agent.prompt",
            session.id = review_session_id.as_str(),
            prompt.batch = items.len(),
            traceparent = first.traceparent.as_deref(),
        );
        // The done senders leave the items behind (done = None): on
        // shutdown, the batch's unaccepted tail returns to its lane below.
        let items_done: Vec<oneshot::Sender<TurnSettle>> = items
            .iter_mut()
            .filter_map(|item| item.done.take())
            .collect();
        let turn = tokio::task::spawn_blocking({
            let core = Arc::clone(&self.core);
            let engine = Arc::clone(&engine);
            move || {
                let _prompt_span = prompt_span.enter();
                run_prompt(&core, engine.as_ref(), emitter, prompt_index, request);
            }
        });
        let _ = turn.await;
        // The emit path is joined: a stale parked partial must not
        // surface after the settle events.
        coalescer.close();
        flusher.abort();
        let accepted_rows = signals
            .accepted_prefix
            .load(std::sync::atomic::Ordering::SeqCst);
        let more_queued = self.settle_lanes(lane, items, accepted_rows, engine.as_ref());
        self.push_roster_delta();
        // The fallback `agent_end` for runs that ended without a model
        // turn (session commands, pre-model failures): the engine's own
        // per-run frames (one per agent run, retried and continued runs
        // included — the TS `agent_end` `messages` payload) are the real
        // frames, and a run whose `agent_end` the abort gate swallowed
        // stays silent exactly like TS (the compact path's detached run).
        // An ABORTED settle keeps the same silence: the admission
        // consult's pre-run abort ends the turn with NO engine
        // `agent_end` at all (no run registered — the
        // compact-interrupt probe's suppressed-run wire shape, which the
        // fallback would otherwise break with a synthesized frame). The
        // association is the abort GATE's own observation during the
        // turn (the per-event flag read), never a post-join re-read of
        // the flag: an abort landing after the turn's last emitted event
        // cancels nothing of this run and must not suppress its fallback
        // (the flag stays armed until the next pickup — a settle-time
        // re-read would race `handle_abort` and silence a completed
        // session-command or pre-model-failure run).
        if !signals
            .engine_agent_end
            .load(std::sync::atomic::Ordering::SeqCst)
            && !signals
                .abort_gate_armed
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.emit_turn_event(json!({ "type": "agent_end" }));
        }
        // The settle's boundary state: the run's own `agent_end`
        // already reported (inside the emit path) — the pane
        // flag, not the engine's sight flag, so an `agent_end` the
        // abort gate swallowed still settles here too; a run that
        // ended without one reports here — the TS fallback arm.
        // This includes the aborted settle: the run's `agent_start`
        // already flipped the pane working, so suppressing the end
        // would strand the pane working forever (the wire emit's
        // abort-gate suppression is about the TUI's frames, not the
        // pane). A failed run (Done(Err) with no `agent_end`) parks
        // its error in the settle cell and blocks like the TS
        // error-hold arm instead of reporting a false idle.
        // `core.busy` flipped to false above; queued lanes still
        // holding items keep the settle debounced so the next pickup
        // cancels the idle flip.
        let error_hold = signals.herdr_settle_error.lock_or_recover().take();
        if !signals
            .herdr_run_end
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.herdr
                .lock_or_recover()
                .run_ended(error_hold, more_queued);
        }
        self.publish_settle().await;
        // Steers folded into this run settle with it.
        let (folded_done, folded_admissions) = {
            let mut core = self.core.lock_or_recover();
            (
                std::mem::take(&mut core.steer_fold.done),
                std::mem::take(&mut core.steer_fold.admissions),
            )
        };
        for admission_id in settled_admissions.into_iter().chain(folded_admissions) {
            self.prompt_admissions.clear(&admission_id);
        }
        // The turn is fully unwound: the waiting prompt now resolves, so
        // a client's next request always observes the idle session.
        let settled_outcome = signals.outcome.lock_or_recover().take();
        if !more_queued && matches!(settled_outcome, Some(TurnSettle::Completed)) {
            self.after_completed_turn(&engine, turn_started_at);
        }
        if let Some(result) = settled_outcome {
            for done in items_done.into_iter().chain(folded_done) {
                let _ = done.send(result.clone());
            }
        }
        self.spawn_compact_auto_refine_review(engine, review_session_id);
    }

    /// The settle's lanes: a shutdown never loses admitted input (the batch items whose
    /// transcript row never persisted exist nowhere else, so they return to the front of
    /// their lane, in order), and the session flips idle. Returns whether queued work
    /// takes the next turn.
    fn settle_lanes(
        &self,
        lane: Lane,
        items: Vec<QueuedItem>,
        accepted_rows: usize,
        engine: &dyn SessionEngine,
    ) -> bool {
        let mut core = self.core.lock_or_recover();
        if core.shutdown_requested && !core.turn_cancelled {
            let lane_items = match lane {
                Lane::Steering => &mut core.steering,
                Lane::FollowUp => &mut core.follow_up,
            };
            for item in items.into_iter().skip(accepted_rows).rev() {
                lane_items.push_front(item);
            }
        }
        let more_queued = !core.steering.is_empty() || !core.follow_up.is_empty();
        // Publish the note clear and idle transition atomically to
        // roster readers, which capture core then the engine note.
        // An abort-gated run may emit no later event to repair a
        // roster push that observes idle with its old note.
        // AgentEnd may have retained the note for work withdrawn before
        // this settle. Recheck the lanes even when that frame was observed.
        if !more_queued {
            engine.clear_progress_note();
        }
        core.busy = false;
        core.active_action = None;
        core.running_admission_ids.clear();
        more_queued
    }

    /// The idle settle's durable and visible tail: the settle checkpoint (the idle flip
    /// precedes it, so an unclean kill from here on must NOT read as interrupted work;
    /// undelivered lanes stay busy — admitted work a revive must redeliver), the session
    /// file's sync, the queue projection, and the idle wake.
    async fn publish_settle(&self) {
        let snapshot = {
            let core = self.core.lock_or_recover();
            Self::snapshot_from(&core)
        };
        checkpoint_queue_recovery(
            &self.recovery,
            &self.core,
            QueueCheckpoint::Settle {
                operation: "turn_end",
            },
            None,
        );
        let session_path = {
            let core = self.core.lock().unwrap();
            core.store.as_ref().map(|store| store.path.clone())
        };
        if let Some(path) = session_path {
            let _ =
                tokio::task::spawn_blocking(move || std::fs::File::open(&path)?.sync_all()).await;
        }
        let _ = self.emit_action_update(&snapshot);
        self.idle_notify.notify_waiters();
    }

    /// The side work a completed run with nothing queued behind it starts: Claude Code's
    /// prompt suggestion (predicting the user's next prompt), recap, tool-use label,
    /// per-turn memory extraction and turn-duration row.
    fn after_completed_turn(
        &self,
        engine: &std::sync::Arc<dyn SessionEngine>,
        turn_started_at: std::time::Instant,
    ) {
        self.maybe_suggest_prompt(engine);
        self.maybe_generate_recap(engine);
        self.maybe_generate_tool_use_label(engine);
        // Claude Code's `[extractMemories]`: the per-turn memory
        // extractor fires at every settled turn. The engine-side
        // gate (`experimental.perTurnMemory`) + RAVO keep it cheap
        // by default; the marker on the session skips the next
        // tick when no new user prose landed.
        let user_message_count = {
            let core = self.core.lock_or_recover();
            u32::try_from(
                core.store
                    .as_ref()
                    .map_or(0, crate::session_store::SessionFile::message_count)
                    / 2,
            )
            .unwrap_or(u32::MAX)
        };
        self.maybe_extract_memories(engine, user_message_count);
        // Claude Code's `showTurnDuration` row: a dim in-stream
        // `${verb} for ${d} \u{00B7} done ${clock}` line below the
        // assistant turn. Gated by `experimental.showTurnDuration`
        // (on by default); emits a durable `turn_duration` custom row
        // so replays carry it too.
        self.maybe_emit_turn_duration(turn_started_at);
    }

    /// The compact-trigger review services off the settle as a
    /// background round: the queued next prompt's admission never
    /// waits on it.
    fn spawn_compact_auto_refine_review(
        &self,
        engine: std::sync::Arc<dyn SessionEngine>,
        review_session_id: String,
    ) {
        let core = Arc::clone(&self.core);
        let events = self.events.clone();
        tokio::spawn(async move {
            // The round takes the engine's session mutex on the blocking
            // pool, never on this async task: a `blocking_lock` from the
            // runtime thread deadlocks.
            let refined = tokio::task::spawn_blocking(move || {
                pa_core::session_engine::compaction_trace::trace(
                    "autorefine.review_started",
                    &serde_json::Value::Null,
                );
                let outcome = engine.consume_compact_auto_refine();
                pa_core::session_engine::compaction_trace::trace(
                    "autorefine.review_done",
                    &serde_json::json!({ "ran": outcome.is_ok() }),
                );
                outcome
            })
            .await
            .unwrap_or_else(|error| Err(anyhow::anyhow!("auto-refinement task failed: {error}")));
            match refined {
                Ok(Some(result)) => {
                    // The outcome row and the model-facing notice (when edits
                    // applied) persist like the `/refine` command's rows, fenced on
                    // the serviced session.
                    let outcome_row =
                        pa_core::session_engine::refine::create_refinement_outcome_message(&result);
                    if let Ok(value) =
                        serde_json::to_value(pa_types::session::AgentMessage::Custom(outcome_row))
                    {
                        emit_refinement_row(&core, &events, &review_session_id, &value);
                    }
                    if result.applied_edits.iter().any(|edit| edit.applied) {
                        let notice =
                            pa_core::session_engine::refine::create_refinement_notice_message(
                                &result,
                                pa_core::session_engine::refine::RefinementSource::Auto,
                            );
                        if let Ok(value) =
                            serde_json::to_value(pa_types::session::AgentMessage::Custom(notice))
                        {
                            emit_refinement_row(&core, &events, &review_session_id, &value);
                        }
                    }
                    emit_refinement_event_for_session(
                        &core,
                        &events,
                        &review_session_id,
                        crate::worker::refine_complete_event(&result),
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("pa-daemon: auto-refinement after compaction failed: {error:#}");
                    emit_refinement_event_for_session(
                        &core,
                        &events,
                        &review_session_id,
                        json!({ "type": "refine_failed", "error": format!("{error:#}") }),
                    );
                }
            }
        });
    }

    /// The post-turn queue projection: an unchanged snapshot stays
    /// silent.
    fn emit_action_update(&self, snapshot: &SessionActionSnapshot) -> Result<()> {
        let mut core = self.core.lock_or_recover();
        if core.last_action_snapshot.as_ref() == Some(snapshot) {
            return Ok(());
        }
        core.last_action_snapshot = Some(snapshot.clone());
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = create_daemon_event_meta(
            &core.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = DaemonOutbound::SessionEvent {
            active_session_id: core.active_session_id.clone(),
            event: json!({ "type": "session_action_update", "actions": snapshot }),
            meta: Some(meta),
            rest: Map::default(),
        };
        let payload = serde_json::to_vec(&outbound)?;
        drop(core);
        self.events.send(OutboundFrame::session_event(payload));
        Ok(())
    }

    fn snapshot_from(core: &SessionCore) -> SessionActionSnapshot {
        session_snapshot(core)
    }

    fn emit_turn_event(&self, event: Value) {
        let mut core = self.core.lock_or_recover();
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = create_daemon_event_meta(
            &core.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = DaemonOutbound::SessionEvent {
            active_session_id: self.active_session_id.clone(),
            event,
            meta: Some(meta),
            rest: Map::default(),
        };
        let payload = serde_json::to_vec(&outbound).unwrap_or_default();
        drop(core);
        self.events.send(OutboundFrame::session_event(payload));
    }
}

/// The turn's engine run on the blocking pool: the restored next-turn prefix rows, the
/// prompt-context boundary, then the prompt itself through the turn's emitter.
fn run_prompt(
    core: &Arc<Mutex<SessionCore>>,
    engine: &dyn SessionEngine,
    mut emitter: emit::TurnEmitter,
    prompt_index: usize,
    request: PromptRequest,
) {
    // `abort_retry` stops an in-flight retry without aborting the turn itself.
    let aborted_probe =
        || core.lock_or_recover().abort_requested || core.lock_or_recover().retry_abort_requested;
    let parked = std::mem::take(&mut core.lock_or_recover().pending_next_turn);
    emitter.emit_prefix_rows(parked);
    // Sample the worker-owned leaf in memory before accepted rows
    // persist. The engine stays lazy and the accepted event stays first.
    let (file_backed, prompt_context_leaf) = {
        let core = core.lock().unwrap();
        core.store.as_ref().map_or((false, None), |store| {
            (
                !store.path.as_os_str().is_empty(),
                store.leaf_id().map(str::to_string),
            )
        })
    };
    // noSession's in-memory prefix IDs do not identify any durable
    // context the engine can restore. Only file-backed stores supply
    // this boundary; their missing-ID validation remains strict.
    if file_backed {
        engine.set_prompt_context_boundary(prompt_context_leaf);
    }
    engine.run_prompt(prompt_index, request, &aborted_probe, &mut |event| {
        emitter.emit(event)
    });
}
