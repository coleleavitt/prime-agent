//! Browsing, reordering, and editing the parked steering/follow-up messages. The browse walks the
//! user-typed items only (operator decision 2026-10-16): internal prompts are a separate channel,
//! shown only as the strip's condensed row.
use super::{
    AgentView,
    DaemonCommand,
    Duration,
    Map,
    QueueBrowseDirection,
    QueueLane,
    Result,
    SessionUi,
    UI_REQUEST_TIMEOUT_MS,
    Value,
    anyhow,
};

/// The daemon capability that lets send-now move a running cell to the
/// background (`detach_running_cell`).
const CELL_DETACH: &str = "cell_detach";
/// The send-now detach's bound: the daemon waits up to 1.5 s for the kernel.
const SEND_NOW_DETACH_TIMEOUT_MS: u64 = 3_000;

/// The lease key of the queue-edit hold: one per client and session, so a
/// repeated acquire answers the same pause.
const QUEUE_EDIT_LEASE: &str = "tui-queue-edit";

impl SessionUi {
    pub(crate) fn sync_queue_selection(&mut self, view: &mut AgentView) {
        view.queue_selected = self.queue_selection.selected().cloned();
    }

    /// Report a queue-edit adoption event (`tui queue edited`), fire-and-forget.
    fn emit_queue_edit(&self, action: &'static str) {
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.queue_edited(action).await;
            });
        }
    }

    /// Move the selection one parked message older/newer and show it in the editor. Entering the
    /// browse stashes the editor draft; reaching the draft again restores it.
    pub(crate) fn browse_queue_selection(
        &mut self,
        direction: QueueBrowseDirection,
        view: &mut AgentView,
    ) {
        let entering = !self.queue_selection.is_browsing();
        let text = self
            .queue_selection
            .browse(&view.queued, &view.editor.get_text(), direction);
        if let Some(text) = text {
            view.editor.set_text(&text);
        }
        // Entering the browse (first selection of a parked message) is the
        // queue-edit adoption signal; per-arrow moves are not.
        if entering && self.queue_selection.is_browsing() {
            self.emit_queue_edit("select");
        }
        self.sync_queue_selection(view);
    }

    /// Hold the queue while a parked message is browsed (Claude Code takes
    /// a message out of its queue to edit it; the browse edits in place, so
    /// it pauses the session's queued input instead): acquire the pause when
    /// a browse opens, release it when the browse ends. Fails open — a
    /// refused or timed-out request leaves the browse working, unheld. A
    /// detach releases every pause this client holds.
    pub(crate) async fn reconcile_queue_hold(&mut self) {
        let browsing = self.queue_selection.is_browsing();
        match (&self.queue_hold, browsing) {
            (None, true) => {
                let acquired = self
                    .bounded_request(
                        Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                        DaemonCommand::AcquireSessionInputPause {
                            id: None,
                            active_session_id: self.active_session_id.clone(),
                            lease_key: QUEUE_EDIT_LEASE.to_string(),
                            rest: Map::default(),
                        },
                    )
                    .await;
                if let Some(pause_id) = acquired.ok().as_ref().and_then(|data| {
                    data.get("pauseId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }) {
                    self.queue_hold = Some((self.active_session_id.clone(), pause_id));
                }
            }
            (Some(_), false) => {
                let Some((session_id, pause_id)) = self.queue_hold.take() else {
                    return;
                };
                let _ = self
                    .bounded_request(
                        Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                        DaemonCommand::ReleaseSessionInputPause {
                            id: None,
                            active_session_id: session_id,
                            pause_id,
                            rest: Map::default(),
                        },
                    )
                    .await;
            }
            (None, false) | (Some(_), true) => {}
        }
    }

    /// Send one `mutate_queued_message` and return its status string (every outcome answers
    /// `success` with `{ status }`; only a malformed request fails the command).
    async fn queue_mutation(
        &self,
        lane: QueueLane,
        index: usize,
        expected_text: &str,
        mutation: Value,
    ) -> Result<Option<String>> {
        let data = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::MutateQueuedMessage {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    lane: Value::String(lane.wire_name().to_string()),
                    index: index as u64,
                    expected_text: expected_text.to_string(),
                    mutation,
                    rest: Map::default(),
                },
            )
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        Ok(data
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Swap the selected message with the adjacent user message in its lane (a silent no-op at the
    /// lane's first/last user message); the move is mirrored locally — the `session_action_update`
    /// event may land after the response.
    pub(crate) async fn move_queue_selection(
        &mut self,
        direction: i64,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(selected) = self.queue_selection.selected().cloned() else {
            return Ok(());
        };
        // Internal items keep their positions: the swap partner is the adjacent user item, so
        // the daemon's move direction spans any internal items between the two.
        let Some(target) = self.queue_selection.reorder_target(direction) else {
            return Ok(());
        };
        let status = self
            .queue_mutation(
                selected.lane,
                selected.index,
                &selected.text,
                serde_json::json!({ "type": "move", "direction": target as i64 - selected.index as i64 }),
            )
            .await;
        match status {
            Ok(Some(status)) if status == "applied" => {
                self.emit_queue_edit("reorder");
                crate::queued::mirror_lane_move(
                    &mut view.queued,
                    selected.lane,
                    selected.index,
                    target,
                );
                self.queue_selection.refresh_at(
                    &view.queued,
                    selected.lane,
                    target,
                    &selected.text,
                );
                self.sync_queue_selection(view);
                self.dirty = true;
            }
            Ok(Some(status)) => self.note(&queue_mutation_status_note(&status, false), view),
            // A malformed request (never sent by this build) surfaces the
            // daemon error like every other command.
            Ok(None) => {}
            Err(error) => self.note(&format!("{error:#}"), view),
        }
        Ok(())
    }

    /// Apply the edited editor text to the selected parked message: empty text deletes it;
    /// otherwise the edit replaces it and moves it to `target_lane` (Enter steers).
    pub(crate) async fn apply_queue_selection(
        &mut self,
        text: &str,
        target_lane: QueueLane,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(selected) = self.queue_selection.selected().cloned() else {
            return Ok(());
        };
        let trimmed = text.trim();
        // `images` stays absent on a replace: the server keeps the item's
        // attachments (some markers cannot be resolved by this client).
        let mutation = if trimmed.is_empty() {
            serde_json::json!({ "type": "delete" })
        } else {
            serde_json::json!({ "type": "replace", "text": trimmed, "lane": target_lane.wire_name() })
        };
        let status = self
            .queue_mutation(selected.lane, selected.index, &selected.text, mutation)
            .await;
        match status {
            Ok(Some(status)) if status == "applied" => {
                self.emit_queue_edit(if trimmed.is_empty() { "delete" } else { "edit" });
                if !trimmed.is_empty() {
                    view.editor.add_to_history(trimmed);
                    self.record_prompt_history(trimmed);
                }
                let draft = self.queue_selection.reset();
                view.editor.set_text(&draft);
            }
            Ok(Some(status)) => {
                // Enter submissions clear the editor before the mutation;
                // a failed edit returns to the editor, never swallowed.
                view.editor.set_text(text);
                self.note(&queue_mutation_status_note(&status, true), view);
            }
            Ok(None) => {}
            Err(error) => {
                view.editor.set_text(text);
                self.note(&format!("{error:#}"), view);
            }
        }
        self.sync_queue_selection(view);
        self.dirty = true;
        Ok(())
    }
}

impl SessionUi {
    /// Up on an empty editor (Claude Code's "press up to edit queued
    /// messages"): every human-typed parked message leaves the queue and lands
    /// in the editor, oldest first, one per line, so it can be reworked or
    /// dropped before it is sent. Each leaves through a `delete` checked
    /// against its text: one delivered meanwhile stays out of the editor.
    /// `false` when nothing human-typed is parked.
    pub(crate) async fn pull_queue_into_editor(&mut self, view: &mut AgentView) -> Result<bool> {
        // `flatten` already yields only the human-typed items (internal
        // prompts carry a queued-item origin), so every item pulls in.
        let items: Vec<_> = crate::queued::flatten(&view.queued).into_iter().collect();
        if items.is_empty() {
            return Ok(false);
        }
        // Newest first, so each delete leaves the earlier indices in place.
        let mut pulled = Vec::new();
        let mut missed = 0;
        for item in items.iter().rev() {
            match self
                .queue_mutation(
                    item.lane,
                    item.index,
                    &item.text,
                    serde_json::json!({ "type": "delete" }),
                )
                .await
            {
                Ok(Some(status)) if status == "applied" => pulled.push(item.text.clone()),
                Ok(_) | Err(_) => missed += 1,
            }
        }
        pulled.reverse();
        if !pulled.is_empty() {
            self.emit_queue_edit("pull");
            view.editor.set_text(&pulled.join("\n"));
        }
        if missed > 0 {
            self.note(
                &format!(
                    "{missed} queued message{} had already been sent",
                    if missed == 1 { "" } else { "s" }
                ),
                view,
            );
        }
        self.dirty = true;
        Ok(true)
    }

    /// The send-now key: the editor's text, when any, joins the steering lane,
    /// and a running turn is interrupted so the queue goes out at once (the
    /// abort delivers the parked steering messages as the next turn). A new
    /// message is sent first and the interrupt waits for the daemon to admit
    /// it (see [`Self::apply_prompt_outcome`]), so it rides the abort.
    pub(crate) async fn send_now(&mut self, view: &mut AgentView) -> Result<()> {
        if self.queue_selection.is_browsing() {
            // A browsed message is applied first, then sent with the rest.
            let text = view.editor.get_text();
            self.apply_queue_selection(&text, QueueLane::Steering, view)
                .await?;
        } else {
            let text = view.editor.get_text();
            if !text.trim().is_empty() {
                view.editor.set_text("");
                view.editor.add_to_history(&text);
                self.record_prompt_history(&text);
                let in_flight = self.prompt_in_flight;
                self.submit_prompt(&text, super::SubmitBehavior::Steer, view)
                    .await?;
                if self.turn_active && self.prompt_in_flight > in_flight {
                    self.send_now_after = Some(self.last_prompt_turn_end);
                    self.dirty = true;
                    return Ok(());
                }
            }
        }
        if self.turn_active {
            self.send_now_deliver(view).await;
        }
        self.dirty = true;
        Ok(())
    }

    /// Send-now's delivery once the queue holds the message (Claude Code's
    /// send-now keeps work it can move): the running kernel cell moves to the
    /// background, so the run goes on and takes the message at its next
    /// boundary while the cell keeps running. Without a movable cell (the
    /// model is mid-reply, the cell blocks its loop, an older daemon) the
    /// running work is interrupted instead.
    pub(crate) async fn send_now_deliver(&mut self, view: &mut AgentView) {
        let can_detach = view.compaction.is_none()
            && self
                .client
                .hello()
                .get("serverCapabilities")
                .and_then(Value::as_array)
                .is_some_and(|caps| caps.iter().any(|cap| cap.as_str() == Some(CELL_DETACH)));
        if can_detach {
            let detached = self
                .bounded_request(
                    Duration::from_millis(SEND_NOW_DETACH_TIMEOUT_MS),
                    DaemonCommand::DetachRunningCell {
                        id: None,
                        active_session_id: self.active_session_id.clone(),
                        rest: Map::default(),
                    },
                )
                .await
                .ok()
                .and_then(|data| data.get("detached").and_then(Value::as_bool))
                .unwrap_or(false);
            if detached {
                self.note(
                    "Moved the running cell to the background; your message joins the turn",
                    view,
                );
                return;
            }
        }
        self.interrupt_running_work(view);
    }

    /// The clear-queue key: drop every parked message (the running turn keeps
    /// going).
    pub(crate) async fn clear_queue(&mut self, view: &mut AgentView) -> Result<()> {
        if view.queued.is_empty() {
            self.note("The queue is empty", view);
            return Ok(());
        }
        if self.queue_selection.is_browsing() {
            let draft = self.queue_selection.reset();
            view.editor.set_text(&draft);
            self.sync_queue_selection(view);
        }
        let result = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::ClearQueue {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Map::default(),
                },
            )
            .await;
        match result {
            Ok(_) => {
                self.emit_queue_edit("clear");
                self.note("Queue cleared", view);
            }
            Err(error) => self.note(&format!("{error:#}"), view),
        }
        self.dirty = true;
        Ok(())
    }
}

/// The mutation status vocabulary (`applied`, `rejected`, `invalid`,
/// `unsupported`) maps to the status rows; `is_edit` picks the edit phrasing.
fn queue_mutation_status_note(status: &str, is_edit: bool) -> String {
    match status {
        "invalid" => {
            "Edited command is not a valid session command; edit kept in the editor".to_string()
        }
        "unsupported" => "Queue editing requires a newer daemon".to_string(),
        _ if is_edit => "Queue changed; edit kept in the editor".to_string(),
        _ => "Queue changed; reorder not applied".to_string(),
    }
}
