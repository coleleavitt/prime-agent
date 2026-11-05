//! The turn loop's side-model UX calls on the turn runner: the generated
//! session title and the next-prompt suggestion fire from the running turn,
//! so they live on [`TurnRunner`] (the same `core`/`events`/`roster_pushes`
//! the daemon command handlers on [`crate::side_ux`]'s `Worker` use).

use std::sync::Arc;

use pa_types::sync::MutexExt;
use serde_json::{Value, json};

use super::turn::TurnRunner;
use crate::engine::{SessionEngine, SideUxOutcome, SideUxRequest};
use super::summary::{broadcast_custom_row_locked, persist_custom_row};
use crate::side_ux::{
    PROMPT_SUGGESTION_EVENT,
    SESSION_RECAP_EVENT,
    TOOL_USE_LABEL_EVENT,
    blocked_on,
    title_attempts,
    title_candidate,
};

impl TurnRunner {
    /// Ask for the session title when `message` is the session's first
    /// titleable prompt and the session has no name or title yet.
    pub(crate) fn maybe_generate_title(
        &self,
        engine: &Arc<dyn SessionEngine>,
        message: &str,
        custom_message: Option<&Value>,
        agent_message: Option<&str>,
    ) {
        let Some(text) = title_candidate(message, custom_message, agent_message) else {
            return;
        };
        let session_id = {
            let core = self.core.lock_or_recover();
            let Some(store) = core.store.as_ref() else {
                return;
            };
            if store.path.as_os_str().is_empty()
                || store.session_name().is_some()
                || store.ai_title().is_some()
            {
                return;
            }
            store.session_id().to_string()
        };
        if !title_attempts()
            .lock_or_recover()
            .insert(session_id.clone())
        {
            return;
        }
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let roster = self.roster_pushes.clone();
        tokio::spawn(async move {
            let request = SideUxRequest::SessionTitle {
                first_message: text,
            };
            let outcome = tokio::task::spawn_blocking(move || engine.run_side_ux(request))
                .await
                .unwrap_or_else(|error| SideUxOutcome::Failed(error.to_string()));
            let title = match outcome {
                SideUxOutcome::Text(title) => title,
                SideUxOutcome::Failed(error) => {
                    eprintln!("pa-daemon: session title generation failed: {error}");
                    title_attempts().lock_or_recover().remove(&session_id);
                    return;
                }
                SideUxOutcome::Nothing
                | SideUxOutcome::Disabled
                | SideUxOutcome::Unsupported
                | SideUxOutcome::RefinementApplied { .. } => {
                    return;
                }
            };
            let recorded = {
                let mut core = core.lock_or_recover();
                match core.store.as_mut() {
                    // The user's own name (or a title that landed meanwhile)
                    // wins; a replaced session never takes this one's title.
                    Some(store)
                        if store.session_id() == session_id
                            && store.session_name().is_none()
                            && store.ai_title().is_none() =>
                    {
                        store
                            .persist_entry(
                                "custom",
                                json!({
                                    "customType": crate::session_store::AI_TITLE_CUSTOM_TYPE,
                                    "data": { "title": title },
                                }),
                            )
                            .is_ok()
                    }
                    _ => false,
                }
            };
            if recorded {
                roster.push();
            }
        });
    }

    /// Claude Code's away summary: at the end of every settled turn, when
    /// the session is idle and clients are attached, ask the engine for a
    /// short recap and push it to the attached clients so the TUI has it
    /// ready to show when the user comes back to the pane. CC's `aws()`
    /// fires here with the same cadence; the engine side gates it on
    /// `awaySummaryEnabled` / `PRIME_AGENT_ENABLE_AWAY_SUMMARY`. A `/recap`
    /// still runs on demand through the ordinary `Recap` request and is
    /// unaffected.
    pub(crate) fn maybe_generate_recap(&self, engine: &Arc<dyn SessionEngine>) {
        let session_id = {
            let core = self.core.lock_or_recover();
            if core.attached_client_ids.is_empty()
                || blocked_on(&core.feature_status).is_some()
                || core.has_ongoing_work()
            {
                return;
            }
            core.active_session_id.clone()
        };
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let events = Arc::clone(&self.events);
        tokio::spawn(async move {
            let outcome =
                tokio::task::spawn_blocking(move || engine.run_side_ux(SideUxRequest::AutoRecap))
                    .await
                    .unwrap_or(SideUxOutcome::Nothing);
            let SideUxOutcome::Text(text) = outcome else {
                return;
            };
            {
                let core = core.lock_or_recover();
                if core.active_session_id != session_id || core.has_ongoing_work() {
                    return;
                }
            }
            crate::worker::emit_worker_event_with(
                &core,
                &events,
                json!({ "type": SESSION_RECAP_EVENT, "text": text }),
            );
        });
    }

    /// Claude Code's `tool_use_summary_generation`: at the end of a
    /// settled turn that ran at least one tool, write a tiny
    /// commit-subject-style label for the batch and push it as a
    /// [`TOOL_USE_LABEL_EVENT`] so the TUI can show it below the feature
    /// panel. Gated by the side-ux `tool_use_labels` switch (default
    /// off). No-op when the engine returns [`SideUxOutcome::Nothing`]
    /// (e.g. no tools ran this turn).
    pub(crate) fn maybe_generate_tool_use_label(&self, engine: &Arc<dyn SessionEngine>) {
        let session_id = {
            let core = self.core.lock_or_recover();
            if core.attached_client_ids.is_empty() {
                return;
            }
            core.active_session_id.clone()
        };
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let events = Arc::clone(&self.events);
        tokio::spawn(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                engine.run_side_ux(SideUxRequest::ToolUseLabel)
            })
            .await
            .unwrap_or(SideUxOutcome::Nothing);
            let SideUxOutcome::Text(text) = outcome else {
                return;
            };
            {
                let core = core.lock_or_recover();
                if core.active_session_id != session_id {
                    return;
                }
            }
            crate::worker::emit_worker_event_with(
                &core,
                &events,
                json!({ "type": TOOL_USE_LABEL_EVENT, "text": text }),
            );
        });
    }

    /// Claude Code's `[extractMemories]` per-turn cadence: at the end of
    /// every settled turn, run the refinement pipeline's review + apply
    /// over the full transcript so a lesson worth saving lands now
    /// instead of waiting for compaction. Gated by
    /// `experimental.perTurnMemory` / `PRIME_AGENT_POST_TURN_MEMORY`
    /// (default off); the engine-side `auto_refine_per_turn` enforces
    /// the three skip guards (feature off, master refinements off, no
    /// new user prose), and RAVO gates each proposal. An applied run
    /// returns the serialized `RefinementResult` here, and we turn it
    /// into the same `refinement_outcome` custom row the compaction
    /// auto-refine boundary emits (plus a `refinement_notice` when
    /// edits actually applied, and the `refine_complete` worker event
    /// for the TUI's completion state) so the saved entries land in the
    /// chat stream — not on a chrome footer row — matching Claude
    /// Code's `appendSystemMessage` placement for extracted memories.
    pub(crate) fn maybe_extract_memories(
        &self,
        engine: &Arc<dyn SessionEngine>,
        user_message_count: u32,
    ) {
        let session_id = {
            let core = self.core.lock_or_recover();
            if core.attached_client_ids.is_empty() {
                return;
            }
            core.active_session_id.clone()
        };
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let events = Arc::clone(&self.events);
        tokio::spawn(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                engine.run_side_ux(SideUxRequest::MemoryExtract {
                    user_message_count,
                })
            })
            .await
            .unwrap_or(SideUxOutcome::Nothing);
            let SideUxOutcome::RefinementApplied { result_json } = outcome else {
                return;
            };
            let typed: pa_core::refinement::RefinementResult =
                match serde_json::from_str(&result_json) {
                    Ok(result) => result,
                    Err(error) => {
                        eprintln!(
                            "pa-daemon: per-turn memory extractor returned an invalid \
                             RefinementResult: {error}"
                        );
                        return;
                    }
                };
            let outcome =
                pa_core::session_engine::refine::create_refinement_outcome_message(&typed);
            let Ok(outcome_value) =
                serde_json::to_value(pa_types::session::AgentMessage::Custom(outcome))
            else {
                return;
            };
            let notice_value = typed
                .applied_edits
                .iter()
                .any(|edit| edit.applied)
                .then(|| {
                    pa_core::session_engine::refine::create_refinement_notice_message(
                        &typed,
                        pa_core::session_engine::refine::RefinementSource::SelfRefine,
                    )
                })
                .and_then(|notice| {
                    serde_json::to_value(pa_types::session::AgentMessage::Custom(notice)).ok()
                });
            {
                let mut core_guard = core.lock_or_recover();
                if core_guard.active_session_id != session_id {
                    return;
                }
                persist_custom_row(&mut core_guard, &outcome_value);
                broadcast_custom_row_locked(&mut core_guard, &events, &outcome_value);
                if let Some(notice) = notice_value.as_ref() {
                    persist_custom_row(&mut core_guard, notice);
                    broadcast_custom_row_locked(&mut core_guard, &events, notice);
                }
            }
            crate::user_bash::emit_session_event_frame(
                &core,
                &events,
                crate::worker::refine_complete_event(&typed),
            );
        });
    }

    /// Predict the next prompt after a run's true end; the suggestion
    /// reaches the clients only while the session is still idle.
    pub(crate) fn maybe_suggest_prompt(&self, engine: &Arc<dyn SessionEngine>) {
        let session_id = {
            let core = self.core.lock_or_recover();
            if core.attached_client_ids.is_empty()
                || blocked_on(&core.feature_status).is_some()
                || core.has_ongoing_work()
            {
                return;
            }
            core.active_session_id.clone()
        };
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let events = Arc::clone(&self.events);
        tokio::spawn(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                engine.run_side_ux(SideUxRequest::PromptSuggestion)
            })
            .await
            .unwrap_or(SideUxOutcome::Nothing);
            let SideUxOutcome::Text(text) = outcome else {
                return;
            };
            {
                let core = core.lock_or_recover();
                if core.active_session_id != session_id || core.has_ongoing_work() {
                    return;
                }
            }
            crate::worker::emit_worker_event_with(
                &core,
                &events,
                json!({ "type": PROMPT_SUGGESTION_EVENT, "text": text }),
            );
        });
    }
}
