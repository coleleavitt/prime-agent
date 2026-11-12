//! One turn's engine-event sink: the abort gate, the run-boundary bookkeeping (progress
//! note, pane reporter, digest counters), persistence, the action-phase flips, and the wire
//! frames, all under the core lock the frames sequence under.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;
use serde_json::{Map, Value, json};

use super::super::session_core::SEND_NOW_CUT_MESSAGE;
use super::super::{
    ABORTED_TURN_SETTLE_ERROR,
    AgentMessageDigest,
    AssistantSnapshot,
    EngineEvent,
    EventPump,
    SessionCore,
    TurnSettle,
};
use crate::engine::SessionEngine;
use crate::protocol::{DaemonOutbound, create_daemon_event_meta};
use crate::types::SessionActionSnapshot;

/// What the emit path tells the turn's settle; written on the blocking emit thread, read
/// after the join.
#[derive(Default)]
pub(super) struct TurnSignals {
    /// Whether the engine surfaced any `agent_end` this run (the
    /// trailing synthesized frame is only a fallback).
    pub(super) engine_agent_end: AtomicBool,
    /// The pane reporter's settle hold: a run that FAILED without the
    /// engine's `agent_end` (a provider error before any terminal
    /// assistant row) still parks its error here, so the settle's
    /// fallback report can block the pane with the message instead
    /// of a false idle.
    pub(super) herdr_settle_error: Mutex<Option<String>>,
    /// Whether the PANE REPORTER already received this run's end —
    /// set only where `run_ended` is actually called (the engine
    /// `agent_end` flag above is set on SIGHT, before the abort gate
    /// can drop the event, so a swallowed `agent_end` must not make
    /// the settle's pane fallback skip and strand the pane working).
    pub(super) herdr_run_end: AtomicBool,
    /// Whether the abort gate ever observed the delivery's cancel flag
    /// DURING this turn (the per-event read): the fallback
    /// `agent_end` keys its silence on THIS association — an abort
    /// landing after the turn's last emitted event (a late abort
    /// racing the settle) never armed the gate and must not suppress
    /// the completed run's fallback (the macroscope finding: the
    /// post-join flag read raced `handle_abort`).
    pub(super) abort_gate_armed: AtomicBool,
    /// The picked batch's accepted rows (primary, then one per batched item) that passed
    /// the abort gate and persisted; it must survive an engine panic after persistence.
    pub(super) accepted_prefix: AtomicUsize,
    /// The settled outcome: it reaches the waiting prompt only at full settle (resolving
    /// at `Done` queued a follow-up behind the indefinite suspension).
    pub(super) outcome: Mutex<Option<TurnSettle>>,
}

/// The turn's shared handles.
pub(super) struct EmitContext {
    pub(super) core: Arc<Mutex<SessionCore>>,
    pub(super) engine: Arc<dyn SessionEngine>,
    pub(super) events: Arc<EventPump>,
    /// `message_update` frames park in a single slot; other frames go
    /// out direct, flushing the parked update first (wire order matches
    /// event-sequence order).
    pub(super) coalescer: Arc<crate::streaming::TurnStreamCoalescer>,
    pub(super) herdr: Arc<Mutex<crate::herdr::HerdrReporter>>,
    pub(super) agent_digest: Arc<AgentMessageDigest>,
    pub(super) agent_dir: std::path::PathBuf,
    pub(super) signals: Arc<TurnSignals>,
    /// The digest lane's turn counters (swarm PR D): this turn counts as
    /// an ingestion turn when its primary item was an agent-message
    /// delivery (the TS ingestion tag: the run's primary input was an
    /// agent message).
    pub(super) ingestion_turn: bool,
    pub(super) batch_len: usize,
}

/// The emit path's own per-turn state.
#[derive(Default)]
struct EmitState {
    /// The engine's own terminal `turn_end` frame keeps the
    /// trailing `Done` fallback silent.
    engine_turn_ended: bool,
    accepted_rows: usize,
    active_running: bool,
    /// Restored next-turn rows are PREFIX rows: the "Starting" row
    /// drops at the accepted row.
    emitting_prefix_rows: bool,
    /// The outcome row names the last `auto_retry_start` error on
    /// success too: one row replaces TS's per-attempt error rows
    /// (operator ruling 2026-09-23).
    last_retry_error: Option<String>,
}

pub(super) struct TurnEmitter {
    context: EmitContext,
    state: EmitState,
}

impl TurnEmitter {
    pub(super) fn new(context: EmitContext) -> Self {
        Self {
            context,
            state: EmitState::default(),
        }
    }

    /// The restored next-turn rows, emitted as the turn's prefix rows.
    pub(super) fn emit_prefix_rows(&mut self, rows: Vec<Value>) {
        self.state.emitting_prefix_rows = !rows.is_empty();
        for row in rows {
            if !self.emit(EngineEvent::CustomMessage(row)) {
                break;
            }
        }
        self.state.emitting_prefix_rows = false;
    }

    /// One engine event. A cancelled turn stops consuming its own events
    /// (`false`), except the frames TS still broadcasts for an interrupted
    /// turn: aborted row/tool settle frames and the trailing `Done` (a
    /// dropped Done hung `prompt_and_wait` forever).
    pub(super) fn emit(&mut self, mut event: EngineEvent) -> bool {
        let context = &self.context;
        let state = &mut self.state;
        if matches!(event, EngineEvent::TurnEnd { .. }) {
            state.engine_turn_ended = true;
        }
        if matches!(event, EngineEvent::AgentEnd { .. }) {
            context
                .signals
                .engine_agent_end
                .store(true, Ordering::SeqCst);
        }
        let mut core = context.core.lock_or_recover();
        let more_queued = context.note_run_end(&core, &event);
        if !context.passes_abort_gate(&core, &event) {
            return false;
        }
        context.count_model_step(&event);
        context.report_pane_boundary(&event, more_queued);
        context.repin_compaction(&core, &mut event);
        // Send-now's cut row says why it stopped, durably and on the
        // wire alike.
        if core.send_now_cut {
            if let EngineEvent::AssistantMessage(message) = &mut event {
                if message.get("stopReason").and_then(Value::as_str) == Some("aborted") {
                    message["errorMessage"] = json!(SEND_NOW_CUT_MESSAGE);
                }
            }
        }
        if !persist_event(&mut core, &event) {
            return false;
        }
        let action_frame = state.advance_action_phase(&mut core, &event, context);
        let done_result = done_settle(&event);
        let Some(mut frames) = state.event_frames(context, &mut core, event) else {
            return false;
        };
        // The phase flip's queue-update frame rides the same batch,
        // after the row frames it follows; an unchanged projection
        // stays silent.
        if let Some(snapshot) = action_frame {
            if core.last_action_snapshot.as_ref() != Some(&snapshot) {
                core.last_action_snapshot = Some(snapshot.clone());
                frames.push(json!({
                    "type": "session_action_update",
                    "actions": snapshot,
                }));
            }
        }
        context.send_frames(&mut core, frames);
        drop(core);
        // The waiting response must observe the frames'
        // sequences and resolves only after the idle flip.
        if let Some(result) = done_result {
            *context.signals.outcome.lock_or_recover() = Some(result);
        }
        true
    }
}

impl EmitContext {
    /// The run's TRUE end — an `agent_end` with no queued work
    /// taking the next turn — keys both the note clear here and
    /// the pane's idle debounce. An internal boundary that
    /// hands the next turn to queued work (the async bash
    /// completion that cut this run) keeps the note for the work
    /// continuing in the next delivery. Cleared before the frame:
    /// its roster flush must not publish the stale note. The turn boundary also releases
    /// RLM child prompt tasks waiting on it, also when the abort gate swallows the frame.
    /// Returns whether queued work takes the next turn after an `agent_end`.
    fn note_run_end(&self, core: &SessionCore, event: &EngineEvent) -> bool {
        let agent_end = matches!(event, EngineEvent::AgentEnd { .. });
        let more_queued = agent_end && (!core.steering.is_empty() || !core.follow_up.is_empty());
        if agent_end && !more_queued {
            self.engine.clear_progress_note();
        }
        if matches!(event, EngineEvent::Done(_) | EngineEvent::DoneAborted) {
            self.engine.on_turn_done();
        }
        more_queued
    }

    /// The abort gate: once the delivery is cancelled, only the abort's settle frames pass
    /// (durable model cleanup is independent of visual abort-row suppression and emits no
    /// client-facing frame). The sighting arms the fallback's silence only when
    /// load-bearing: a sighting on a run that completed on its own cancels nothing.
    fn passes_abort_gate(&self, core: &SessionCore, event: &EngineEvent) -> bool {
        if !core.abort_requested || matches!(event, EngineEvent::ModelChange(_)) {
            return true;
        }
        let aborted_row = is_aborted_row(event);
        if core.suppress_aborted_row || !(is_abort_settle(event) || aborted_row) {
            self.signals.abort_gate_armed.store(true, Ordering::SeqCst);
            return false;
        }
        if aborted_row || matches!(event, EngineEvent::DoneAborted) {
            self.signals.abort_gate_armed.store(true, Ordering::SeqCst);
        }
        true
    }

    /// The session's messaging step counters (upstream #2352,
    /// read by the digest lane's controller): one model step per
    /// assistant row the persist path accepts (an `error` stop
    /// is not a completed step, TS `stopReason !== "error"`)
    /// with its usage tokens; the ingestion flag rides the turn.
    /// Counted only AFTER the abort/suppression gate accepts
    /// the event — a suppressed row is not a step — and the
    /// stats' leaf mutex keeps the count safe under the core
    /// lock the counter readers hold.
    fn count_model_step(&self, event: &EngineEvent) {
        let EngineEvent::AssistantMessage(message) = event else {
            return;
        };
        if message.get("stopReason").and_then(Value::as_str) == Some("error") {
            return;
        }
        let tokens = message
            .get("usage")
            .and_then(|usage| usage.get("totalTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.agent_digest
            .note_model_step(tokens, self.ingestion_turn);
    }

    /// The pane reporter's engine boundaries (the TS
    /// `agent_start` / `agent_end` hooks and the auto-retry
    /// hold): a run start or a retry keeps the pane working,
    /// and the run's end settles it — an error end holds
    /// working through the retry grace first, a queued-work
    /// end debounces the idle. A run whose `agent_end` the
    /// abort gate swallowed never reaches here, so the
    /// pane keeps its last state exactly like the TS detached
    /// run.
    fn report_pane_boundary(&self, event: &EngineEvent, more_queued: bool) {
        match event {
            EngineEvent::AgentStart => {
                self.herdr.lock_or_recover().run_started();
                // A later run in the same turn (the retry, the
                // continuation) re-opens its own end: the settle
                // fallback keys on the flag, so a run start must
                // clear it or an end-swallowed abort of the
                // LATER run would skip the settle and strand the
                // pane working.
                self.signals.herdr_run_end.store(false, Ordering::SeqCst);
            }
            EngineEvent::AgentEnd { messages } => {
                self.herdr
                    .lock_or_recover()
                    .run_ended(crate::herdr::error_hold_message(messages), more_queued);
                self.signals.herdr_run_end.store(true, Ordering::SeqCst);
            }
            EngineEvent::AutoRetryStart { .. } => {
                self.herdr.lock_or_recover().retry_started();
                // The retry re-runs the turn body: its end (when
                // the abort gate lets it through) re-sets the
                // flag; clearing here lets a swallowed retry end
                // still settle at the turn's close.
                self.signals.herdr_run_end.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    /// The engine cuts its in-memory entries; its
    /// `firstKeptEntryId` never matches this store's file ids,
    /// so a verbatim copy retains nothing on the durable read.
    /// Re-pin the boundary to the durable cut (TS: one store,
    /// ids match by construction) before persist + broadcast.
    fn repin_compaction(&self, core: &SessionCore, event: &mut EngineEvent) {
        let EngineEvent::Compaction {
            entry,
            event: payload,
        } = event
        else {
            return;
        };
        if entry.is_null() {
            return;
        }
        let repin_started = std::time::Instant::now();
        let durable_entries = core
            .store
            .as_ref()
            .and_then(|store| store.branch().len().checked_sub(1))
            .unwrap_or(0);
        if let Some(id) = core.store.as_ref().and_then(|store| {
            store.durable_first_kept_entry_id(keep_recent_tokens(&core.cwd, &self.agent_dir))
        }) {
            entry["firstKeptEntryId"] = json!(id);
            if let Some(result) = payload.get_mut("result").and_then(Value::as_object_mut) {
                result.insert("firstKeptEntryId".to_string(), json!(id));
            }
        }
        pa_core::session_engine::compaction_trace::trace(
            "emit.compaction_repin",
            &serde_json::json!({
                "durableEntries": durable_entries,
                "micros": repin_started.elapsed().as_micros(),
            }),
        );
    }

    /// Park a streaming update in the coalescer; `false` once the turn joined.
    fn park_update(
        &self,
        core: &mut SessionCore,
        message: AssistantSnapshot,
        stream_event: Option<&Value>,
        stream_kind: Option<&str>,
    ) -> bool {
        // Streaming updates park in the coalescer; `park_update` only
        // returns false after the turn joined, which cannot race this closure.
        if std::env::var_os(EVENT_LOG_ENV).is_some() {
            if let Some(value) = message.clone().into_wire() {
                let mut event = json!({
                    "type": "message_update",
                    "message": value,
                });
                if let Some(stream_event) = stream_event {
                    event["assistantMessageEvent"] = stream_event.clone();
                }
                append_event_log(&[event]);
            }
        }
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let delta = stream_event
            .and_then(|event| event.get("delta"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.coalescer
            .park_update(message, stream_kind.unwrap_or_default(), delta, sequence)
    }

    /// Sequence the frames and send them direct while their sequence still owns the store
    /// lock, so another row cannot overtake them; the parked update flushes first, pure
    /// updates stay parked.
    fn send_frames(&self, core: &mut SessionCore, frames: Vec<Value>) {
        append_event_log(&frames);
        let mut direct_payloads: Vec<Vec<u8>> = Vec::new();
        for event_json in frames {
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
                event: event_json,
                meta: Some(meta),
                rest: Map::default(),
            };
            let payload = serde_json::to_vec(&outbound).unwrap_or_default();
            direct_payloads.push(payload);
        }
        if !direct_payloads.is_empty() {
            self.coalescer.send_direct(&direct_payloads, &self.events);
        }
    }
}

impl EmitState {
    /// The `committing`/`running` transitions ride the events that mark the moments: the
    /// first accepted batch row commits, the first assistant output runs.
    fn advance_action_phase(
        &mut self,
        core: &mut SessionCore,
        event: &EngineEvent,
        context: &EmitContext,
    ) -> Option<SessionActionSnapshot> {
        if !self.emitting_prefix_rows
            && self.accepted_rows < context.batch_len
            && matches!(
                event,
                EngineEvent::UserMessage(_) | EngineEvent::CustomMessage(_)
            )
        {
            self.accepted_rows += 1;
            context
                .signals
                .accepted_prefix
                .store(self.accepted_rows, Ordering::SeqCst);
            if self.accepted_rows != 1 {
                return None;
            }
            if let Some(active) = core.active_action.as_mut() {
                active.phase = "committing".to_string();
            }
            return Some(super::super::session_snapshot(core));
        }
        if self.active_running
            || !matches!(
                event,
                EngineEvent::AssistantUpdate { .. } | EngineEvent::AssistantMessage(_)
            )
        {
            return None;
        }
        self.active_running = true;
        if let Some(active) = core.active_action.as_mut() {
            active.phase = "running".to_string();
        }
        Some(super::super::session_snapshot(core))
    }

    /// The wire frames of one event: one event may map to several (a custom row is a
    /// `message_start` + `message_end` pair), a parked streaming update to none. `None`
    /// once the turn joined (the parked update was refused).
    fn event_frames(
        &mut self,
        context: &EmitContext,
        core: &mut SessionCore,
        event: EngineEvent,
    ) -> Option<Vec<Value>> {
        let frames = match event {
            EngineEvent::UserMessage(message)
            | EngineEvent::ToolResultMessage(message)
            | EngineEvent::CustomMessage(message) => vec![
                json!({ "type": "message_start", "message": message }),
                json!({ "type": "message_end", "message": message }),
            ],
            EngineEvent::AssistantUpdate {
                message,
                stream_event,
            } => return assistant_update_frames(context, core, message, stream_event),
            EngineEvent::AssistantMessage(message) => {
                vec![json!({ "type": "message_end", "message": message })]
            }
            EngineEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => vec![json!({
                "type": "tool_execution_start",
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "args": args,
            })],
            EngineEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
            } => vec![json!({
                "type": "tool_execution_update",
                "toolCallId": tool_call_id,
                "partialResult": partial_result,
            })],
            EngineEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
            } => vec![json!({
                "type": "tool_execution_end",
                "toolCallId": tool_call_id,
                "result": result,
                "isError": is_error,
            })],
            EngineEvent::CompactionStart { event } | EngineEvent::Compaction { event, .. } => {
                vec![event]
            }
            EngineEvent::GoalUpdate { goal } => vec![json!({
                "type": "goal_update",
                "goal": goal,
            })],
            EngineEvent::RefineComplete { result } => vec![json!({
                "type": "refine_complete",
                "result": result,
            })],
            EngineEvent::RefineFailed { error } => vec![json!({
                "type": "refine_failed",
                "error": error,
            })],
            // The loop's run-boundary frames (TS `agent_start`/
            // `agent_end`): the run's whole message set rides
            // `agent_end` (one frame per agent run — retried and
            // continued runs included); the rows themselves
            // already went out through their own events, so no
            // persist here.
            EngineEvent::AgentStart => vec![json!({ "type": "agent_start" })],
            EngineEvent::AgentEnd { messages } => vec![json!({
                "type": "agent_end",
                "messages": messages,
            })],
            // Turn-boundary frames: the terminal message and tool
            // results ride `turn_end`; no persist here.
            EngineEvent::TurnStart => vec![json!({ "type": "turn_start" })],
            EngineEvent::TurnEnd {
                message,
                tool_results,
            } => vec![json!({
                "type": "turn_end",
                "message": message,
                "toolResults": tool_results,
            })],
            // The fallback terminal frame, silent once the
            // engine's own frame covered the run.
            EngineEvent::Done(Ok(())) if !self.engine_turn_ended => {
                vec![json!({ "type": "turn_end" })]
            }
            EngineEvent::Done(Err(error)) if !self.engine_turn_ended => {
                context
                    .signals
                    .herdr_settle_error
                    .lock_or_recover()
                    .replace(error.clone());
                vec![json!({ "type": "turn_end", "error": error })]
            }
            EngineEvent::DoneAborted if !self.engine_turn_ended => vec![json!({
                "type": "turn_end",
                "error": ABORTED_TURN_SETTLE_ERROR,
            })],
            EngineEvent::ModelChange(_)
            | EngineEvent::Done(Ok(()) | Err(_))
            | EngineEvent::DoneAborted => Vec::new(),
            EngineEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
                reason,
            } => {
                self.last_retry_error = Some(error_message.clone());
                let mut event = json!({
                    "type": "auto_retry_start",
                    "attempt": attempt,
                    "maxAttempts": max_attempts,
                    "delayMs": delay_ms,
                    "errorMessage": error_message,
                });
                match reason {
                    pa_core::session_engine::auto_retry::RetryStartReason::Quick => {}
                    pa_core::session_engine::auto_retry::RetryStartReason::Backup {
                        backup_model,
                    } => {
                        event["reason"] = json!("backup");
                        event["backupModel"] = json!(backup_model);
                    }
                }
                vec![event]
            }
            EngineEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
                restored_model,
            } => self.retry_end_frames(core, success, attempt, final_error, restored_model),
        };
        Some(frames)
    }

    /// `auto_retry_end` and the episode's ONE durable outcome row (operator ruling
    /// 2026-09-23), instead of one error row per failed attempt.
    fn retry_end_frames(
        &mut self,
        core: &mut SessionCore,
        success: bool,
        attempt: u32,
        final_error: Option<String>,
        restored_model: Option<String>,
    ) -> Vec<Value> {
        let mut event = json!({
            "type": "auto_retry_end",
            "success": success,
            "attempt": attempt,
        });
        if let Some(final_error) = &final_error {
            event["finalError"] = json!(final_error);
        }
        if let Some(restored_model) = restored_model {
            event["restoredModel"] = json!(restored_model);
        }
        let error = final_error
            .or_else(|| self.last_retry_error.take())
            .unwrap_or_else(|| "Unknown error".to_string());
        self.last_retry_error = None;
        let outcome = pa_core::session_engine::messages::create_provider_retry_outcome_message(
            success, attempt, &error,
        );
        let outcome = crate::session_commands::custom_message_value(&outcome);
        if outcome.is_object() {
            if let Some(store) = core.store.as_mut() {
                persist_custom_message(store, &outcome);
            }
        }
        vec![
            event,
            json!({ "type": "message_start", "message": outcome }),
            json!({ "type": "message_end", "message": outcome }),
        ]
    }
}

/// A streaming assistant update's frames: a message start or a block-end event goes out
/// direct (a block end settles the parked delta run, so it travels direct, flushing the
/// parked update first, in order); other updates park in the coalescer and frame nothing.
/// `None` once the turn joined.
fn assistant_update_frames(
    context: &EmitContext,
    core: &mut SessionCore,
    message: AssistantSnapshot,
    stream_event: Option<Value>,
) -> Option<Vec<Value>> {
    let stream_kind = stream_event
        .as_ref()
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str);
    let starts_message = stream_kind == Some("start");
    let settles_run = matches!(
        stream_kind,
        Some("text_end" | "thinking_end" | "toolcall_end")
    );
    if !starts_message && !settles_run {
        return context
            .park_update(core, message, stream_event.as_ref(), stream_kind)
            .then(Vec::new);
    }
    // Unreachable for streamed partials; a failed conversion frames nothing.
    let Some(value) = message.into_wire() else {
        return Some(Vec::new());
    };
    let mut event = json!({
        "type": if starts_message { "message_start" } else { "message_update" },
        "message": value,
    });
    if let Some(stream_event) = stream_event {
        event["assistantMessageEvent"] = stream_event;
    }
    Some(vec![event])
}

/// The durable row of one event: a `message` entry per row (TS's appendMessage path), a
/// custom message, a compaction record (a skipped compaction carries a null entry: publish
/// the event, never persist it), the goal state, and the in-flight tool-call set (updated
/// under the same core lock the frames sequence under: the roster feed never reads a
/// half-applied transition; the session's tool counter bumps with the same set, so the
/// summary a roster push composes from this event already carries it). `false` when the
/// model change cannot persist.
fn persist_event(core: &mut SessionCore, event: &EngineEvent) -> bool {
    match event {
        EngineEvent::ModelChange(change) => {
            if let Some(Err(error)) = core.store.as_mut().map(|store| {
                store.persist_entry(
                    "model_change",
                    json!({
                        "provider": change.provider, "modelId": change.model_id,
                    }),
                )
            }) {
                eprintln!("pa-daemon: model-change persistence failed: {error:#}");
                return false;
            }
        }
        EngineEvent::UserMessage(message)
        | EngineEvent::AssistantMessage(message)
        | EngineEvent::ToolResultMessage(message) => {
            if let Some(store) = core.store.as_mut() {
                let _ = store.persist_entry("message", json!({ "message": message }));
            }
        }
        EngineEvent::ToolExecutionStart { tool_call_id, .. } => {
            core.running_tool_calls
                .insert(tool_call_id.clone(), crate::util::now_ms());
            core.tool_use_count += 1;
        }
        EngineEvent::ToolExecutionEnd { tool_call_id, .. } => {
            core.running_tool_calls.remove(tool_call_id);
        }
        EngineEvent::CustomMessage(message) => {
            if let Some(store) = core.store.as_mut() {
                persist_custom_message(store, message);
            }
        }
        EngineEvent::Compaction { entry, .. } => {
            if let Some(store) = core.store.as_mut().filter(|_| !entry.is_null()) {
                let persist_started = std::time::Instant::now();
                let _ = store.persist_entry("compaction", entry.clone());
                pa_core::session_engine::compaction_trace::trace(
                    "emit.compaction_persist",
                    &serde_json::json!({
                        "micros": persist_started.elapsed().as_micros(),
                    }),
                );
            }
        }
        // The durable mirror of a goal-state change: each row is the new state.
        EngineEvent::GoalUpdate { goal } => {
            if let Some(store) = core.store.as_mut() {
                let _ = store.persist_entry(
                    "custom",
                    json!({
                        "customType": pa_core::goals::GOAL_STATE_CUSTOM_TYPE,
                        "data": goal,
                    }),
                );
            }
        }
        _ => {}
    }
    true
}

/// A custom message row's durable `custom_message` entry.
fn persist_custom_message(store: &mut crate::session_store::SessionFile, message: &Value) {
    let _ = store.persist_entry(
        "custom_message",
        json!({
            "customType": message.get("customType").cloned().unwrap_or(Value::Null),
            "content": message.get("content").cloned().unwrap_or(Value::Null),
            "display": message.get("display").cloned().unwrap_or(Value::Bool(true)),
            "details": message.get("details").cloned().unwrap_or(Value::Null),
        }),
    );
}

/// The turn outcome a `Done` settles with: the aborted settle carries its classification
/// structurally, not through the error text.
fn done_settle(event: &EngineEvent) -> Option<TurnSettle> {
    match event {
        EngineEvent::Done(result) => {
            pa_core::session_engine::compaction_trace::trace(
                "turn.done_emitted",
                &serde_json::json!({
                    "ok": matches!(result, Ok(())),
                }),
            );
            Some(match result {
                Ok(()) => TurnSettle::Completed,
                Err(error) => TurnSettle::Failed(error.clone()),
            })
        }
        EngineEvent::DoneAborted => Some(TurnSettle::Aborted),
        _ => None,
    }
}

/// Whether the event carries an aborted assistant row.
fn is_aborted_row(event: &EngineEvent) -> bool {
    let aborted =
        |message: &Value| message.get("stopReason").and_then(Value::as_str) == Some("aborted");
    match event {
        EngineEvent::AssistantMessage(message)
        | EngineEvent::AssistantUpdate {
            message: AssistantSnapshot::Wire(message),
            ..
        }
        | EngineEvent::TurnEnd { message, .. } => aborted(message),
        EngineEvent::AssistantUpdate {
            message: AssistantSnapshot::Loop(message),
            ..
        } => matches!(
            &**message,
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) if assistant.stop_reason == pa_agent::types::StopReason::Aborted
        ),
        EngineEvent::AgentEnd { messages } => messages.iter().any(aborted),
        _ => false,
    }
}

/// Whether the event settles an aborted run's frames. `auto_retry_end` settles an abort
/// that lands between attempts (TS `abortRetry`'s "Retry cancelled"): no run is live then,
/// so no aborted row or `turn_end` follows, and a swallowed end leaves clients counting
/// down a retry that will never come.
fn is_abort_settle(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::ModelChange(_)
            | EngineEvent::AutoRetryEnd { .. }
            | EngineEvent::ToolExecutionEnd { .. }
            | EngineEvent::ToolResultMessage(_)
            | EngineEvent::TurnEnd { .. }
            | EngineEvent::AgentEnd { .. }
            | EngineEvent::Done(_)
            | EngineEvent::DoneAborted
    )
}

/// The verification seam's event log (`PA_DAEMON_EVENT_LOG=<path>`): the emitted session
/// events, appended for harness debugging.
const EVENT_LOG_ENV: &str = "PA_DAEMON_EVENT_LOG";

fn append_event_log(frames: &[Value]) {
    use std::io::Write;
    let Ok(path) = std::env::var(EVENT_LOG_ENV) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    for frame in frames {
        let _ = writeln!(file, "{frame}");
    }
}

/// The compaction cut budget the engine ran with: the durable boundary
/// re-cut in the turn callback must pin the same cut.
fn keep_recent_tokens(cwd: &str, agent_dir: &std::path::Path) -> u64 {
    pa_core::settings::SettingsManager::create(cwd, agent_dir)
        .settings()
        .compaction
        .clone()
        .unwrap_or_default()
        .keep_recent_tokens
        .unwrap_or(pa_core::session_engine::compaction::DEFAULT_KEEP_RECENT_TOKENS)
}
