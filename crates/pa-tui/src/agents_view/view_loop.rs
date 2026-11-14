//! The view run's loop state: the queued inputs, the background fetches and their re-arm
//! deadlines, the in-flight action dispatches, and the headless render barrier. The run loop in
//! [`super::run_agents_view_surface`] owns the receivers its select borrows; everything the arms
//! fold into lives here.
use std::time::Duration;

use pa_types::daemon::DaemonCommand;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::session_activity::ActivityRead;
use super::{
    AgentsViewLink,
    AgentsViewMode,
    AgentsViewOutcome,
    AgentsViewRun,
    DaemonClient,
    DaemonClientEvent,
    PULSE_INTERVAL_MS,
    PathBuf,
    Renderer,
    Result,
    SAVED_CATALOG_PAGE_FILES,
    SAVED_CATALOG_RECONCILE_INTERVAL_MS,
    SavedCatalog,
    StatusTone,
    UiInput,
    advance_running_pulse,
    external,
    heartbeats,
    persistent_shells,
    session_activity,
    spawn_delete_dispatch,
    spawn_headline_fetch,
    spawn_kill_dispatch,
    spawn_rename_dispatch,
    spawn_reply_dispatch,
    spawn_saved_catalog_fetch,
};

/// The loop's timer deadlines, copied out before a select so its arms never borrow the state.
#[derive(Debug, Clone, Copy)]
pub(super) struct Deadlines {
    /// The running rows' next animation pulse.
    pub(super) pulse: Instant,
    /// The streamed saved-catalog batch window's close.
    pub(super) saved_flush: Option<Instant>,
    /// The persistent shells list's next poll.
    pub(super) shells_fetch: Option<Instant>,
    /// The activity counts' next fetch cycle.
    pub(super) activity_fetch: Option<Instant>,
    /// The incident-notice log's next poll.
    pub(super) incident_poll: Instant,
    /// The armed headless render barrier's timeout.
    pub(super) wait_render: Option<Instant>,
}

/// Sleep until `at`; an unarmed deadline never fires.
pub(super) async fn sleep_until_armed(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The view run's loop state (see the module docs).
pub(super) struct ViewLoop {
    client: DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
    /// The saved catalog's scope (cwd + sessionDir), shared by every catalog fetch.
    cwd: PathBuf,
    session_dir: Option<PathBuf>,
    /// The in-flight saved-catalog fetch's request id: only its streamed rows apply.
    catalog_request: Option<String>,
    /// The newest heartbeat-catalog fetch: a superseded fetch's answer never applies.
    heartbeat_generation: u64,
    shells_fetch_at: Option<Instant>,
    activity_fetch_at: Option<Instant>,
    /// The saved-catalog stream's open batch window: the first buffered row arms it, the flush
    /// closes it.
    pub(super) saved_flush: Option<Instant>,
    /// The inputs queued for the loop, applied one per iteration.
    pub(super) pending: Vec<UiInput>,
    last_pulse: Instant,
    /// The incident-notice poll: re-read appended agent.jsonl bytes, re-rendered only on a
    /// changed collapsed line.
    incident_poll_at: Instant,
    /// In-flight delete/rename dispatches; teardown waits for them so an exit
    /// right after a confirmed action does not drop the request.
    action_dispatches: Vec<tokio::task::JoinHandle<()>>,
    /// The headless plan's render barrier: an armed hold's deadline, and the frames captured at
    /// arming (the condition scans only post-arming frames).
    wait_render_deadline: Option<Instant>,
    wait_render_baseline: usize,
}

impl ViewLoop {
    /// Arm the open-time fetches: the saved catalog (unless the carried one loaded), the
    /// heartbeat catalog, the persistent shells, the activity counts (unless a carried cycle is
    /// still in flight), and the external sessions. Every fetch runs while the view is already
    /// interactive, so a large catalog never delays the first frame.
    pub(super) fn start(
        client: DaemonClient,
        ui_tx: mpsc::UnboundedSender<UiInput>,
        mode: &mut AgentsViewMode,
        activity_fetch: &mut Option<oneshot::Receiver<Vec<ActivityRead>>>,
    ) -> Self {
        let cwd = mode.options.cwd.clone();
        let session_dir = mode.options.session_dir.clone();
        // A loaded catalog never re-fetches; a first run - or one whose previous fetch failed -
        // arms the fetch.
        let catalog_request = (!mode.saved_catalog_loaded).then(|| {
            spawn_saved_catalog_fetch(
                &client,
                ui_tx.clone(),
                cwd.clone(),
                session_dir.clone(),
                None,
                Some(SAVED_CATALOG_PAGE_FILES),
            )
        });
        // The heartbeat catalog feeds the rows' `◷ N` badges: the same
        // selector-less `heartbeats_list` the activity dock reads, fetched
        // open-time like TS `refreshHeartbeats` and re-read on every
        // `heartbeats_changed`.
        let heartbeat_generation: u64 = 1;
        heartbeats::spawn_heartbeat_catalog_fetch(&client, ui_tx.clone(), heartbeat_generation);
        // The persistent shells' list poll: one fetch in flight, the next armed when it lands.
        if client.supports_server_capability(persistent_shells::CAPABILITY) {
            persistent_shells::spawn_list_fetch(&client, ui_tx.clone());
        }
        // The activity lines' counts: one cycle in flight at a time, the next armed when it lands.
        if activity_fetch.is_none() {
            *activity_fetch = Some(session_activity::spawn_session_activity_fetch(
                &client,
                mode.live_session_ids(),
            ));
        }
        // TS `start()`'s open-time settle (`armSavedSearchFetch` followed by
        // `resolveMissingSelectionAnchor`): a carried catalog arms no fetch,
        // so no terminal load ever arrives to settle the entry anchor's wait -
        // resolve it now. The anchor's row either landed from the carry's
        // rebuild above or the catalog already settled without it; an armed
        // fetch keeps the wait (its load settles it).
        if catalog_request.is_none() {
            mode.settle_carried_anchor();
        }
        // Other products' sessions (Claude Code's), when the daemon serves them.
        if client.supports_server_capability(external::EXTERNAL_SESSIONS_CAPABILITY) {
            external::spawn_external_catalog_fetch(&client, ui_tx.clone());
        }
        Self {
            client,
            ui_tx,
            cwd,
            session_dir,
            catalog_request,
            heartbeat_generation,
            shells_fetch_at: None,
            activity_fetch_at: None,
            saved_flush: None,
            pending: Vec::new(),
            last_pulse: Instant::now(),
            incident_poll_at: Instant::now()
                + Duration::from_millis(crate::incident_notices::INCIDENT_NOTICE_POLL_INTERVAL_MS),
            action_dispatches: Vec::new(),
            wait_render_deadline: None,
            wait_render_baseline: 0,
        }
    }

    pub(super) fn deadlines(&self) -> Deadlines {
        Deadlines {
            pulse: self.last_pulse + Duration::from_millis(PULSE_INTERVAL_MS),
            saved_flush: self.saved_flush,
            shells_fetch: self.shells_fetch_at,
            activity_fetch: self.activity_fetch_at,
            incident_poll: self.incident_poll_at,
            wait_render: self.wait_render_deadline,
        }
    }

    /// `WaitRender` holds the queued plan batch until a post-arming frame contains the needle;
    /// the daemon-driven answers jump the hold (the promotion below), or the catalog's landing
    /// would wedge the wait on its own condition. Returns whether the barrier holds.
    pub(super) fn hold_render_barrier(
        &mut self,
        mode: &mut AgentsViewMode,
        renderer: &Renderer,
        redraw: &mut bool,
    ) -> bool {
        if self.wait_render_deadline.is_some()
            && let Some(index) = self.pending.iter().position(is_daemon_answer)
        {
            let input = self.pending.remove(index);
            self.pending.insert(0, input);
        }
        while let Some(UiInput::WaitRender { needle, timeout_ms }) = self.pending.first() {
            let frames = renderer.headless_frames();
            let Some(deadline) = self.wait_render_deadline else {
                if frames.is_some_and(|frames| {
                    frames
                        .last()
                        .is_some_and(|frame| frame.contains(needle.as_str()))
                }) {
                    self.pending.remove(0);
                    continue;
                }
                self.wait_render_baseline = frames.map_or(0, <[String]>::len);
                self.wait_render_deadline =
                    Some(Instant::now() + Duration::from_millis(*timeout_ms));
                return true;
            };
            let satisfied = frames.is_some_and(|frames| {
                frames
                    .get(self.wait_render_baseline..)
                    .unwrap_or_default()
                    .iter()
                    .any(|frame| frame.contains(needle.as_str()))
            });
            if satisfied {
                self.wait_render_deadline = None;
                self.pending.remove(0);
                continue;
            }
            if Instant::now() <= deadline {
                return true;
            }
            self.wait_render_deadline = None;
            self.pending.remove(0);
            // The deadline pops the hold and the plan proceeds; the status note reports the
            // wait, never the needle — quoting it would satisfy the failed condition.
            mode.set_status("timed out waiting for the headless render condition");
            *redraw = true;
        }
        false
    }

    /// The next queued input, oldest first.
    pub(super) fn next_input(&mut self) -> Option<UiInput> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    /// Apply one queued input to the view, then dispatch what it queued.
    pub(super) fn apply_input(&mut self, mode: &mut AgentsViewMode, input: UiInput) {
        match input {
            UiInput::Key(key) => {
                mode.handle_key(&key);
                // The parked suggestion request materializes once the key's edits
                // landed.
                mode.materialize_composer_autocomplete();
                self.dispatch_key_requests(mode);
            }
            // A plain click opens the row under it; drags and wheel turns are consumed inside.
            UiInput::Mouse(event) => {
                mode.handle_mouse(&event);
                self.dispatch_import(mode);
            }
            UiInput::ExternalLoaded { sessions } => {
                mode.apply_external_loaded(sessions);
            }
            UiInput::ImportResult { request, outcome } => {
                mode.import_result(&request, outcome);
            }
            UiInput::Paste(text) => {
                mode.handle_paste(&text);
            }
            UiInput::Notify(notify) => notify.notify_one(),
            // `Settled` is the plan's own settle no-op; `WaitRender` never reaches the batch
            // pop (the pre-pass at the loop head owns it).
            UiInput::Resize | UiInput::Settled | UiInput::WaitRender { .. } => {}
            UiInput::DeleteResult {
                message,
                tone,
                deleted_saved_path,
            } => {
                mode.delete_result(&message, tone, deleted_saved_path);
                if self.shells_fetch_at.is_some() {
                    self.shells_fetch_at = Some(Instant::now());
                }
            }
            UiInput::RenameResult { rename, outcome } => {
                mode.rename_result(rename, outcome);
                // A newer name the user asked for while this write ran gets its own
                // write now: one writer per session, so the newest name lands last.
                self.dispatch_rename(mode);
            }
            UiInput::HeadlineResult { key, result } => {
                mode.headline_result(&key, result);
            }
            // The saved-resume's mid-send status lands between the resume and the
            // prompt like any set_status, never queued behind the result it
            // precedes.
            UiInput::ReplyProgress(text) => {
                mode.set_status(&text);
            }
            UiInput::ReplyResult { key, outcome } => {
                mode.reply_result(&key, outcome);
            }
            UiInput::KillResult { key, outcome } => {
                mode.kill_result(&key, outcome);
            }
            // The saved-catalog scan landed: the Inactive section builds now.
            UiInput::SavedLoaded {
                sessions,
                more,
                continued,
            } => {
                self.settle_saved_fetch(mode);
                if continued {
                    mode.upsert_saved(sessions);
                } else {
                    mode.apply_saved_loaded(sessions);
                }
                // The catalog's success retires the failure status (never keep reporting an
                // unavailable catalog after it loaded).
                if mode
                    .status_text()
                    .is_some_and(|status| status.starts_with("Saved sessions unavailable"))
                {
                    mode.status = None;
                }
                mode.rebuild_rows();
                mode.finish_saved_page(more);
            }
            UiInput::SavedFailed { error } => {
                // The catalog settled on a terminal failure: the anchor's wait ends with it,
                // the failure keeps the last good rows.
                self.settle_saved_fetch(mode);
                mode.settle_anchor_wait_on_saved_failure();
                mode.saved_fetch_failed = true;
                mode.set_status_tone(
                    &format!("Saved sessions unavailable: {error}"),
                    StatusTone::Error,
                );
            }
            // Only the newest fetch applies (TS
            // `heartbeatCatalogGeneration`): responses can reorder.
            UiInput::HeartbeatsLoaded {
                generation,
                heartbeats,
            } => {
                if generation == self.heartbeat_generation {
                    mode.heartbeats = heartbeats;
                }
            }
            UiInput::PersistentShellsLoaded(data) => {
                if let Some(data) = data {
                    mode.apply_persistent_shells(data);
                }
                self.shells_fetch_at =
                    Some(Instant::now() + crate::interactive::ACTIVITY_POLL_INTERVAL);
            }
            UiInput::PersistentShellTail {
                id,
                generation,
                outcome,
            } => {
                if let Some(view) = mode
                    .shell_view
                    .as_mut()
                    .filter(|view| view.detail_id().as_deref() == Some(id.as_str()))
                {
                    match outcome {
                        Ok(tail) => view.set_output(&id, &tail, generation),
                        Err(error) => view.set_error(error, true, Some(generation)),
                    }
                }
            }
            // A landed kill re-reads the list now unless a read is already in flight.
            UiInput::PersistentShellKilled { id, outcome } => match outcome {
                Ok(()) => {
                    if self.shells_fetch_at.is_some() {
                        self.shells_fetch_at = Some(Instant::now());
                    }
                }
                Err(error) => {
                    if let Some(view) = mode
                        .shell_view
                        .as_mut()
                        .filter(|view| view.detail_id().as_deref() == Some(id.as_str()))
                    {
                        view.set_error(error, false, None);
                    }
                }
            },
            UiInput::SessionActivityLoaded(reads) => {
                mode.apply_session_activity(reads);
                self.activity_fetch_at =
                    Some(Instant::now() + crate::interactive::ACTIVITY_POLL_INTERVAL);
            }
            // The headless plan ended: the run stops here (the
            // interactive harness's `HeadlessDone` contract). A plan
            // that ends without an exit key still captures its frames
            // and returns instead of spinning forever.
            UiInput::Done => mode.running = false,
        }
        // The bash view's keys and wheel scrolls queue its action: dispatch
        // it after the input has been applied.
        if let Some(action) = mode.pending_shell_action.take() {
            self.action_dispatches
                .extend(persistent_shells::spawn_action(
                    &self.client,
                    self.ui_tx.clone(),
                    action,
                ));
        }
    }

    /// The wire calls a key queued, each dispatched off the key loop; their outcomes land as
    /// inputs. The 2s exit drain covers an Enter-then-exit the same way it covers a confirmed
    /// stop-or-delete.
    fn dispatch_key_requests(&mut self, mode: &mut AgentsViewMode) {
        // The executed stop-or-delete dispatch (the second ctrl+x).
        if let Some(action) = mode.take_delete_action() {
            self.action_dispatches.push(spawn_delete_dispatch(
                &self.client,
                self.ui_tx.clone(),
                action,
            ));
        }
        self.dispatch_rename(mode);
        // The reply composer's dispatches.
        if let Some(request) = mode.pending_reply.take() {
            self.action_dispatches.push(spawn_reply_dispatch(
                &self.client,
                self.ui_tx.clone(),
                request,
            ));
        }
        if let Some(request) = mode.pending_kill.take() {
            self.action_dispatches.push(spawn_kill_dispatch(
                &self.client,
                self.ui_tx.clone(),
                request,
            ));
        }
        if let Some((key, active_session_id)) = mode.pending_headline.take() {
            spawn_headline_fetch(&self.client, self.ui_tx.clone(), key, active_session_id);
        }
        self.dispatch_import(mode);
    }

    /// The confirmed rename, dispatched off the key loop; its outcome lands as a `RenameResult`.
    fn dispatch_rename(&mut self, mode: &mut AgentsViewMode) {
        if let Some(rename) = mode.pending_rename.take() {
            self.action_dispatches.push(spawn_rename_dispatch(
                &self.client,
                self.ui_tx.clone(),
                rename,
            ));
        }
    }

    /// An external session's import, dispatched off the input loop.
    fn dispatch_import(&mut self, mode: &mut AgentsViewMode) {
        if let Some(request) = mode.pending_import.take() {
            self.action_dispatches.push(external::spawn_import_dispatch(
                &self.client,
                self.ui_tx.clone(),
                request,
            ));
        }
    }

    /// The saved-catalog fetch settled: its final response is the authoritative array, so a late
    /// stream frame must not upsert its un-enriched row over it.
    fn settle_saved_fetch(&mut self, mode: &mut AgentsViewMode) {
        mode.drop_saved_stream();
        self.saved_flush = None;
        self.catalog_request = None;
    }

    /// One daemon event; returns whether the frame changed. A closed connection ends the run.
    pub(super) fn apply_daemon_event(
        &mut self,
        mode: &mut AgentsViewMode,
        event: Option<DaemonClientEvent>,
    ) -> bool {
        match event {
            Some(DaemonClientEvent::RosterUpdate {
                changed,
                removed,
                resync,
            }) => {
                mode.apply_roster_update(changed, removed, resync);
                true
            }
            // The scan streams its rows: one rebuild per batch window, so
            // the Inactive section appears progressively instead of after
            // the whole scan.
            Some(DaemonClientEvent::SessionListItem {
                session,
                request_id,
            }) => {
                if self.catalog_request.as_deref() == Some(request_id.as_str()) {
                    mode.buffer_saved_stream_item(session);
                    if self.saved_flush.is_none() {
                        self.saved_flush = Some(
                            Instant::now()
                                + Duration::from_millis(SAVED_CATALOG_RECONCILE_INTERVAL_MS),
                        );
                    }
                }
                false
            }
            // TS `heartbeats_changed` → `refreshHeartbeats`:
            // the daemon-global broadcast re-reads the
            // catalog; the landed answer redraws, not this.
            Some(DaemonClientEvent::HeartbeatsChanged) => {
                self.heartbeat_generation += 1;
                heartbeats::spawn_heartbeat_catalog_fetch(
                    &self.client,
                    self.ui_tx.clone(),
                    self.heartbeat_generation,
                );
                false
            }
            Some(_) => false,
            None => {
                mode.set_status_tone("the daemon connection closed", StatusTone::Error);
                mode.running = false;
                true
            }
        }
    }

    /// An activity-count cycle landed: its reads apply as an input; a failed cycle re-arms the
    /// next one after the poll interval.
    pub(super) fn activity_fetch_landed(
        &mut self,
        result: Result<Vec<ActivityRead>, oneshot::error::RecvError>,
    ) {
        if let Ok(reads) = result {
            self.pending.push(UiInput::SessionActivityLoaded(reads));
        } else {
            self.activity_fetch_at =
                Some(Instant::now() + crate::interactive::ACTIVITY_POLL_INTERVAL);
        }
    }

    /// The persistent shells list's poll fired: one fetch, the next armed when it lands.
    pub(super) fn refresh_persistent_shells(&mut self) {
        self.shells_fetch_at = None;
        persistent_shells::spawn_list_fetch(&self.client, self.ui_tx.clone());
    }

    /// The activity counts' poll fired: the next cycle over the live sessions.
    pub(super) fn refresh_session_activity(
        &mut self,
        mode: &AgentsViewMode,
    ) -> oneshot::Receiver<Vec<ActivityRead>> {
        self.activity_fetch_at = None;
        session_activity::spawn_session_activity_fetch(&self.client, mode.live_session_ids())
    }

    /// The bookkeeping after every wake: the status line's expiry, a due animation pulse and
    /// incident poll, and the saved-catalog re-arm and next page. Returns whether the frame
    /// changed.
    pub(super) fn after_wake(&mut self, mode: &mut AgentsViewMode) -> bool {
        // The status line's timer: an expired line clears here after every arm, so
        // a status that aged out mid-batch never survives the redraw that follows.
        let mut redraw = mode.expire_status(std::time::Instant::now());
        // Coalesce a due animation pulse with the input or roster frame, and a due incident poll
        // behind a busy input stream.
        redraw |= advance_running_pulse(mode, &mut self.last_pulse, Instant::now());
        if Instant::now() >= self.incident_poll_at {
            self.incident_poll_at = Instant::now()
                + Duration::from_millis(crate::incident_notices::INCIDENT_NOTICE_POLL_INTERVAL_MS);
            redraw |= mode.refresh_incident_notices();
        }
        // A failed saved-catalog fetch re-arms on the next query change or deferred shell open
        // (a key, a paste, or a click): one honest retry behind a terminal failure.
        if mode.take_saved_fetch_rearm() {
            self.catalog_request =
                Some(self.spawn_catalog_fetch(None, Some(SAVED_CATALOG_PAGE_FILES)));
            // A superseded fetch's stream stops applying: the new fetch owns the
            // catalog.
            mode.drop_saved_stream();
            self.saved_flush = None;
        }
        // The unfolded rest of the store: a scroll near the list's end fetches the next page, a
        // search fetches all of it (its rows stream in as they fold).
        if self.catalog_request.is_none()
            && let Some((cursor, limit)) = mode.next_saved_fetch()
        {
            mode.saved_page_requested = false;
            self.catalog_request = Some(self.spawn_catalog_fetch(Some(cursor), limit));
        }
        redraw
    }

    fn spawn_catalog_fetch(&self, cursor: Option<String>, limit: Option<u64>) -> String {
        spawn_saved_catalog_fetch(
            &self.client,
            self.ui_tx.clone(),
            self.cwd.clone(),
            self.session_dir.clone(),
            cursor,
            limit,
        )
    }

    /// The run's teardown: the surface hands back, the roster unsubscribes, the in-flight
    /// dispatches settle and their results apply, and the outcome (with the roster link on a
    /// handoff) returns.
    pub(super) async fn finish(
        mut self,
        mode: &mut AgentsViewMode,
        renderer: Renderer,
        events: mpsc::UnboundedReceiver<DaemonClientEvent>,
        mut ui_rx: mpsc::UnboundedReceiver<UiInput>,
        activity_fetch: Option<oneshot::Receiver<Vec<ActivityRead>>>,
        exit_guard: &crate::exit_guard::ExitGuard,
    ) -> Result<AgentsViewRun> {
        // The view decided to leave. The force-quit deadline arms below,
        // after the stop-or-delete drain settles: a confirmed request
        // completes before any deadline can cut it down. `renderer.finish`
        // consumes the renderer, so the terminal check is read first.
        let handing_off = mode.opened.is_some();
        let terminal_exit = matches!(renderer, Renderer::Terminal { .. }) && !handing_off;
        // A selection hands the pane to the chat it opened (TS `result.type !== "exit"`);
        // exiting releases the alternate screen.
        let frames = renderer.finish(mode.opened.is_some());
        // TS `AgentsViewRosterStore.dispose` fires the roster unsubscribe
        // fire-and-forget ("nobody needs the ack"; the supervisor also drops
        // the subscription with the socket), so no handoff ever waits on it.
        {
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .request(DaemonCommand::RosterUnsubscribe {
                        id: None,
                        rest: serde_json::Map::default(),
                    })
                    .await;
            });
        }
        let opened = mode.opened.take();
        // A handoff retires the watchdog before the drain wait: a double-press must not force-quit a
        // process that is merely switching views.
        if handing_off {
            exit_guard.cancel();
        }
        // An in-flight stop-or-delete dispatch settles before the connection closes: an exit right
        // after the second ctrl+x must not drop the request.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        for dispatch in self.action_dispatches {
            let _ = tokio::time::timeout_at(deadline, dispatch).await;
        }
        // Apply queued delete/rename results before the catalog snapshot, so the
        // next view run does not show a deleted path or the old saved name.
        for input in std::mem::take(&mut self.pending)
            .into_iter()
            .chain(std::iter::from_fn(|| ui_rx.try_recv().ok()))
        {
            match input {
                UiInput::DeleteResult {
                    message,
                    tone,
                    deleted_saved_path,
                } => {
                    mode.delete_result(&message, tone, deleted_saved_path);
                }
                UiInput::RenameResult { rename, outcome } => {
                    mode.rename_result(rename, outcome);
                }
                // The reply outcomes apply on the exit path too (an Enter-then-exit): the
                // statuses paint nothing on a run that ended, but the adoption actions
                // (`reply_sent`, `killed`) must still reach the outcome.
                UiInput::HeadlineResult { key, result } => {
                    mode.headline_result(&key, result);
                }
                UiInput::ReplyProgress(text) => {
                    mode.set_status(&text);
                }
                UiInput::ReplyResult { key, outcome } => {
                    mode.reply_result(&key, outcome);
                }
                UiInput::KillResult { key, outcome } => {
                    mode.kill_result(&key, outcome);
                }
                UiInput::SessionActivityLoaded(reads) => {
                    mode.apply_session_activity(reads);
                }
                _ => {}
            }
        }
        // The force-quit deadline arms only after the drain; a selection is a view switch, not an
        // exit, so the deadline covers only the leaves that end this process.
        if terminal_exit {
            exit_guard.arm_for_exit();
        }
        // A handoff returns the roster connection for the flow's next view run
        // (TS `persistentState.rosterClient`); a selection-less exit closes it.
        let link = if opened.is_some() {
            // The handoff link carries the catalog the run loaded (TS
            // `persistentState.savedSessions`/`savedCatalogLoaded`): the flow's
            // next view run paints the Inactive rows it already holds on its
            // first frame and skips the fetch when this one loaded them.
            Some(AgentsViewLink {
                client: self.client,
                events,
                saved: SavedCatalog {
                    sessions: mode.saved.clone(),
                    loaded: mode.saved_catalog_loaded,
                    more: mode.saved_more.clone(),
                },
                session_activity: std::mem::take(&mut mode.session_activity),
                persistent_shells: std::mem::take(&mut mode.persistent_shells),
                activity_fetch,
                saved_scope: mode.saved_scope,
                branch_only: mode.branch_only,
                all_worktrees: mode.all_worktrees,
            })
        } else {
            self.client.close();
            None
        };
        Ok(AgentsViewRun {
            link,
            outcome: AgentsViewOutcome {
                selection: opened.as_ref().map(|row| row.selection.clone()),
                frames,
                query: (!mode.query.is_empty()).then(|| mode.query.clone()),
                scope_popped: mode.scope_popped,
                scope_dropped: mode.scope_dropped,
                scope_back: mode.scope_back,
                expanded_ancestors: opened
                    .as_ref()
                    .map(|row| row.expanded_ancestors.clone())
                    .unwrap_or_default(),
                selected_row_identity: opened.as_ref().map(|row| row.selected_row_identity.clone()),
                selected_key: opened.as_ref().map(|row| row.selected_key.clone()),
                opened_rlm_depth: opened.as_ref().and_then(|row| row.rlm_depth),
                opened_has_children: opened.as_ref().is_some_and(|row| row.has_children),
                opened_cwd: opened
                    .as_ref()
                    .and_then(|row| row.cwd.clone())
                    .map(std::path::PathBuf::from),
                status_message: opened.as_ref().and_then(|row| row.status_message.clone()),
                panel: opened.as_ref().and_then(|row| row.panel.clone()),
                actions: std::mem::take(&mut mode.actions),
                incident_notice_state: std::mem::take(&mut mode.incident_notice_state),
            },
        })
    }
}

/// The daemon-driven answers that jump an armed render barrier: the
/// saved catalog's landing (or its terminal failure), the heartbeat
/// catalog's landing, and the stop-or-delete dispatch results are the
/// events the plan's needles wait on — they must never queue behind the
/// hold they satisfy.
fn is_daemon_answer(input: &UiInput) -> bool {
    matches!(
        input,
        UiInput::SavedLoaded { .. }
            | UiInput::SavedFailed { .. }
            | UiInput::DeleteResult { .. }
            | UiInput::RenameResult { .. }
            | UiInput::HeadlineResult { .. }
            | UiInput::ReplyProgress(_)
            | UiInput::ReplyResult { .. }
            | UiInput::KillResult { .. }
            | UiInput::HeartbeatsLoaded { .. }
            | UiInput::ExternalLoaded { .. }
            | UiInput::ImportResult { .. }
            | UiInput::PersistentShellsLoaded(_)
            | UiInput::PersistentShellTail { .. }
            | UiInput::PersistentShellKilled { .. }
            | UiInput::SessionActivityLoaded(_)
    )
}
