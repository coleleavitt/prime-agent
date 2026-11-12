//! The reconnect machinery: the update-restart resume window, the unexpected-loss hiccup loop, and
//! the announced shutdown's bounded recovery.

use super::{AgentView, DaemonClient, DaemonClientEvent, Duration, RecoveryKind, SessionUi, mpsc};

/// The in-flight reconnect attempt's connect leg: a spawned task's bounded
/// `DaemonClient::connect_with_retry` result (fresh client plus its event receiver).
pub(super) type ReconnectConnect = tokio::sync::oneshot::Receiver<
    anyhow::Result<(DaemonClient, mpsc::UnboundedReceiver<DaemonClientEvent>)>,
>;

/// Spec §10.2: the client reconnect window after an update restart (10 minutes).
const RECONNECT_WINDOW: Duration = Duration::from_mins(10);

/// The reconnect backoff cap.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// TS #2458 `DAEMON_RECONNECT_TIMEOUT_MS`: the announced non-update closing's recovery
/// window — an explicit stop stays stopped, so the pane waits bounded instead of retrying
/// through the §10.2 resume window.
pub(super) const DAEMON_SHUTDOWN_RECONNECT_WINDOW: Duration = Duration::from_secs(60);

/// TS #2458 `SHUTDOWN_RECONNECT_RETRY_MS`: the shutdown recovery's poll
/// cadence (fixed, unlike the doubling hiccup backoff).
pub(super) const SHUTDOWN_RECONNECT_RETRY: Duration = Duration::from_millis(100);

/// TS `DAEMON_RECONNECT_TIMEOUT_MS`: the bounded session-plane reconnect
/// window after the direct worker link dies.
const SESSION_RECONNECT_WINDOW: Duration = Duration::from_mins(1);
/// TS reconnect backoff cap (`min(2000, 100 * 2 ** min(attempt, 5))`).
const SESSION_RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);
/// One re-attach attempt's budget: the attach carries its own request timeouts; this bounds a
/// wedged attempt so the loop reschedules instead of blocking the UI.
pub(super) const SESSION_RECONNECT_ATTEMPT_TIMEOUT_S: u64 = 10;

/// The interactive loop's session re-attach driver (TS `DaemonAgentConnection.reconnect`): the
/// worker process behind the direct link died, so the attach retries through the supervisor — which
/// respawns the worker and hands out a fresh peer ticket — with the TS backoff inside the TS
/// window.
pub(super) struct SessionReconnect {
    pub(super) active_session_id: String,
    pub(super) deadline: tokio::time::Instant,
    pub(super) next_attempt: tokio::time::Instant,
    delay: Duration,
    pub(super) last_error: String,
}

impl SessionReconnect {
    pub(super) fn start(active_session_id: &str) -> Self {
        SessionReconnect {
            active_session_id: active_session_id.to_string(),
            deadline: tokio::time::Instant::now() + SESSION_RECONNECT_WINDOW,
            next_attempt: tokio::time::Instant::now(),
            delay: Duration::from_millis(100),
            last_error: String::new(),
        }
    }

    /// The next attempt with doubling backoff (capped).
    pub(super) fn next_attempt(mut self) -> Self {
        self.delay = (self.delay * 2).min(SESSION_RECONNECT_BACKOFF_MAX);
        self.next_attempt = tokio::time::Instant::now() + self.delay;
        self
    }
}

/// The interactive loop's full reconnect driver (spec §10.2): attempts with backoff inside the
/// window; the user can leave with Ctrl+C at any point. Three closings arm it — an update
/// restart's resume contract, an UNEXPECTED connection loss (a daemon hiccup at load, 2026-09-24:
/// the one-shot path exited the operator's TUI with "the daemon connection closed"), and an
/// ANNOUNCED non-update closing (TS #2458: the operator's own shutdown used to kill every
/// attached window) — the pane keeps its transcript and retries instead of dying.
pub(super) struct ReconnectLoop {
    pub(super) deadline: tokio::time::Instant,
    pub(super) next_attempt: tokio::time::Instant,
    pub(super) delay: Duration,
    /// The closing that armed this driver: the reattach banner and the expiry row follow it.
    pub(super) kind: RecoveryKind,
}

impl ReconnectLoop {
    pub(super) fn start(_update: &crate::daemon_client::DaemonClosingUpdate) -> Self {
        let delay = Duration::from_secs(1);
        ReconnectLoop {
            deadline: tokio::time::Instant::now() + RECONNECT_WINDOW,
            next_attempt: tokio::time::Instant::now() + delay,
            delay,
            kind: RecoveryKind::Update,
        }
    }

    /// The unexpected-loss variant: same window, same backoff, its own expiry row.
    pub(super) fn start_lost() -> Self {
        let delay = Duration::from_secs(1);
        ReconnectLoop {
            deadline: tokio::time::Instant::now() + RECONNECT_WINDOW,
            next_attempt: tokio::time::Instant::now() + delay,
            delay,
            kind: RecoveryKind::Lost,
        }
    }

    /// TS #2458 `reconnectAfterShutdown`: the announced non-update closing. The window is the TS
    /// reconnect timeout (not the §10.2 resume window — an explicit stop stays stopped), the
    /// cadence is the TS fixed poll, and the expiry is the saved-transcript close.
    pub(super) fn start_shutdown() -> Self {
        ReconnectLoop {
            deadline: tokio::time::Instant::now() + DAEMON_SHUTDOWN_RECONNECT_WINDOW,
            next_attempt: tokio::time::Instant::now() + SHUTDOWN_RECONNECT_RETRY,
            delay: SHUTDOWN_RECONNECT_RETRY,
            kind: RecoveryKind::Shutdown,
        }
    }

    /// The next attempt: doubling backoff (capped) for the resume and
    /// hiccup windows; the shutdown recovery keeps the TS fixed poll.
    pub(super) fn next_attempt(mut self) -> Self {
        if !matches!(self.kind, RecoveryKind::Shutdown) {
            self.delay = (self.delay * 2).min(RECONNECT_BACKOFF_MAX);
        }
        self.next_attempt = tokio::time::Instant::now() + self.delay;
        self
    }
}

/// What the run loop does after a recovery arm: the arms that ended without work skip the
/// iteration's frame pass, and an expired or failed recovery ends the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecoveryStep {
    /// The iteration continues to its frame pass.
    Proceed,
    /// Nothing to paint: the loop starts its next iteration.
    NextIteration,
    /// The recovery expired: the run ends without another frame pass.
    Stop,
    /// The reattach failed for good: the run ends after this iteration's frame pass.
    StopAfterFrame,
}

/// The interactive loop's connection recovery: the full reconnect driver and its in-flight
/// connect leg, the session re-attach driver, and the loss latches that decide which owns a
/// lost connection.
#[derive(Default)]
pub(super) struct Recovery {
    /// Spec §10.2: the reconnect loop after a `daemon_closing` update frame, an unexpected loss,
    /// or an announced shutdown. Retry with backoff up to the window; each attempt reads the
    /// successor's hello (§10.3) and reattaches by durable session id (§10.4). UI input keeps
    /// flowing while reconnecting.
    reconnect: Option<ReconnectLoop>,
    /// The in-flight reconnect attempt's connect leg (spawned off the loop, so the connect+hello
    /// wait never blocks UI input or the render); the reattach leg runs inline on the loop.
    pub(super) connect: Option<ReconnectConnect>,
    /// The reader-death watch is one-shot: once the loss is handled (or suppressed), the arm
    /// parks so the closed watch cannot hot-spin the select loop.
    pub(super) reader_loss_handled: bool,
    /// The supervisor connection died while a live direct link kept serving the session: the
    /// loss is retained until the direct link itself dies; then the full reconnect driver owns
    /// the recovery.
    supervisor_lost: bool,
    /// Set once the event channel has returned None (see the events arm).
    events_closed: bool,
    /// The session re-attach driver: armed when the direct worker link dies (a killed or crashed
    /// worker); it re-attaches through the supervisor so the respawned worker serves the session
    /// again.
    session_reconnect: Option<SessionReconnect>,
}

impl Recovery {
    /// A closed event channel's `recv()` resolves None instantly and forever; while the reconnect
    /// driver owns the run that always-ready arm would hot-spin the loop and starve the tokio
    /// timers, so the events arm parks instead: the tick drives the retries.
    pub(super) fn parks_events(&self) -> bool {
        self.events_closed && self.reconnect.is_some()
    }

    /// The reconnect tick's deadline: parked while an attempt is in flight (the armed
    /// `next_attempt` is in the past, so an unparked tick would busy-spin the loop for the
    /// attempt's duration) or when no driver runs.
    pub(super) fn tick_deadline(&self) -> Option<tokio::time::Instant> {
        if self.connect.is_some() {
            return None;
        }
        self.reconnect.as_ref().map(|state| state.next_attempt)
    }

    /// The session re-attach driver's next attempt.
    pub(super) fn session_tick_deadline(&self) -> Option<tokio::time::Instant> {
        self.session_reconnect
            .as_ref()
            .map(|state| state.next_attempt)
    }

    /// An update close frame arms the reconnect driver: the UI stays mounted and reconnects.
    fn start_update(
        &mut self,
        update: &crate::daemon_client::DaemonClosingUpdate,
        session: &mut SessionUi,
        view: &mut AgentView,
    ) {
        session.note(
            &format!(
                "the daemon is restarting for an update (about {}s) — reconnecting…",
                update.est_seconds.max(1)
            ),
            view,
        );
        self.reconnect = Some(ReconnectLoop::start(update));
        session.dirty = true;
    }

    /// An unexpected connection loss (no update in flight) is a daemon hiccup, not a session
    /// end: the pane keeps its transcript and retries with the update restart's window and
    /// backoff; the window expires into the exit note.
    fn start_lost(&mut self, session: &mut SessionUi, view: &mut AgentView) {
        session.note_as(
            "the daemon connection closed — reconnecting…",
            crate::chat::StatusKind::Warning,
            view,
        );
        self.reconnect = Some(ReconnectLoop::start_lost());
        session.dirty = true;
    }

    /// TS #2458 `reconnectAfterShutdown`'s arming: an announced non-update closing keeps the pane
    /// mounted (the recovery never relaunches the daemon — an explicit stop stays stopped). No-op
    /// when the notice is absent or a driver already owns the recovery.
    fn arm_shutdown_recovery(&mut self, session: &mut SessionUi, view: &mut AgentView) -> bool {
        if self.reconnect.is_some() || session.daemon_closing_notice.as_deref() != Some("shutdown")
        {
            return false;
        }
        session.note_as(
            "the Prime Agent daemon shut down; waiting for it to come back…",
            crate::chat::StatusKind::Warning,
            view,
        );
        // TS #2458's yield rule: the shutdown recovery owns the run — a session-plane retry armed
        // by an earlier direct-link loss would race it through a dying supervisor, and its expiry
        // would block submits after a later reconnect lands.
        self.session_reconnect = None;
        self.reconnect = Some(ReconnectLoop::start_shutdown());
        session.dirty = true;
        true
    }

    /// The loss signals a folded event batch raised. An update close frame arms the reconnect
    /// driver immediately: the doomed connection's reader task is gone, but the client retains an
    /// event sender, so the channel itself never closes - the frame, not the EOF, is the trigger
    /// (spec §10.2). An update closing outranks a shutdown recovery in flight (TS #2458): the
    /// §10.2 resume contract replaces it. An announced non-update closing arms the bounded
    /// shutdown recovery instead, and a dead direct worker link arms the session re-attach
    /// driver: the warning row rides the chat while the driver retries.
    pub(super) fn after_events(&mut self, session: &mut SessionUi, view: &mut AgentView) {
        if let Some(update) = session.reconnect.take() {
            self.start_update(&update, session, view);
        }
        self.arm_shutdown_recovery(session, view);
        if self.session_reconnect.is_some() {
            return;
        }
        let Some(lost) = session.transport_lost.take() else {
            return;
        };
        if self.supervisor_lost && self.reconnect.is_none() {
            // A supervisor loss retained while the direct link lived: the supervisor client is
            // dead, so the session-plane retry loop could never restore it — the full reconnect
            // driver replaces the client and reattaches.
            self.start_lost(session, view);
            self.supervisor_lost = false;
        } else if self.reconnect.is_none() {
            session.note_as(
                "Daemon connection lost; reconnecting…",
                crate::chat::StatusKind::Warning,
                view,
            );
            self.session_reconnect = Some(SessionReconnect::start(&lost));
        }
        // With a full reconnect driver running, the dead direct link joins it instead of racing a
        // session-plane retry through a supervisor it cannot reach.
        session.dirty = true;
    }

    /// The event channel closed.
    pub(super) fn on_events_closed(&mut self, session: &mut SessionUi, view: &mut AgentView) {
        self.events_closed = true;
        if let Some(update) = session.reconnect.take() {
            // §10: an update restart closed the daemon; the UI stays mounted and reconnects.
            self.start_update(&update, session, view);
            return;
        }
        // TS #2458: the announced non-update closing owns the recovery, not the hiccup loop;
        // already reconnecting, the dead channel's None frames are expected.
        if self.arm_shutdown_recovery(session, view) || self.reconnect.is_some() {
            return;
        }
        self.start_lost(session, view);
    }

    /// The supervisor reader died (the events already delivered were folded first, so an update's
    /// close frame racing the death owns the recovery).
    pub(super) fn on_reader_death(&mut self, session: &mut SessionUi, view: &mut AgentView) {
        if session.reconnect.is_some() {
            session.dirty = true;
        } else if self.arm_shutdown_recovery(session, view) {
            // The announced non-update closing owns the recovery: the supervisor
            // socket's death joins its driver.
        } else if session.client.direct_session_id().is_some() {
            // A supervisor socket loss while a live direct link still serves the session is not a
            // pane-level loss: retain the loss instead of recovering, and hand it to the full
            // reconnect driver when the direct link later dies.
            self.supervisor_lost = true;
        } else if self.session_reconnect.is_some() {
            // The direct link already died and the session-plane driver is retrying through the
            // NOW-DEAD supervisor: stop it (it would ride a dead client) and hand the recovery to
            // the full driver.
            self.session_reconnect = None;
            if self.reconnect.is_none() {
                self.start_lost(session, view);
            }
            session.dirty = true;
        } else if self.reconnect.is_none() {
            self.start_lost(session, view);
        }
    }

    /// The reconnect tick: an expired window ends the run, otherwise one connect attempt runs
    /// off the loop.
    pub(super) fn on_tick(
        &mut self,
        session: &mut SessionUi,
        view: &mut AgentView,
        socket_path: &std::path::Path,
    ) -> RecoveryStep {
        if self.connect.is_some() {
            return RecoveryStep::NextIteration;
        }
        let Some(state) = self.reconnect.as_ref() else {
            return RecoveryStep::NextIteration;
        };
        let kind = state.kind;
        if tokio::time::Instant::now() > state.deadline {
            self.expire(kind, session, view);
            return RecoveryStep::Stop;
        }
        let socket_path = socket_path.to_path_buf();
        let (attempt_tx, attempt_rx) = tokio::sync::oneshot::channel();
        self.connect = Some(attempt_rx);
        // The shutdown recovery's discovery waits no longer than the bound — one bounded
        // connect+hello per poll (the resume/hiccup windows keep the retrying helper).
        let shutdown = matches!(kind, RecoveryKind::Shutdown);
        tokio::spawn(async move {
            // No outer timeout: dropping the future mid-attempt would cancel an
            // in-flight handshake without its reader abort running (a leaked reader and
            // socket). The leg self-bounds — every attempt carries its own budgets.
            let attempt = if shutdown {
                DaemonClient::connect(&socket_path).await
            } else {
                DaemonClient::connect_with_retry(&socket_path).await
            };
            let _ = attempt_tx.send(attempt);
        });
        RecoveryStep::Proceed
    }

    /// The reconnect window ran out: the closing's expiry row, and the run ends.
    fn expire(&mut self, kind: RecoveryKind, session: &mut SessionUi, view: &mut AgentView) {
        let (note, reason) = match kind {
            // The daemon never came back within the reconnect timeout — the saved-transcript
            // close (the session file survives on disk).
            RecoveryKind::Shutdown => (
                "The Prime Agent daemon shut down while this window was attached. The session transcript remains saved; restart Prime Agent and reopen it from Agents View.",
                "daemon_closed",
            ),
            RecoveryKind::Lost => (
                "could not reconnect to the daemon within 10 minutes — run `prime-agent attach` to resume.",
                "daemon_reconnect_failed",
            ),
            RecoveryKind::Update => (
                "could not reconnect to the daemon within 10 minutes — the update finished but this window is detached. Run `prime-agent attach` to resume.",
                "update_reconnect_failed",
            ),
        };
        session.note(note, view);
        session.exit_reason = reason;
        self.reconnect = None;
        session.dirty = true;
    }

    /// The reconnect attempt's connect leg landed: reattach over the fresh connection (its
    /// events and reader watch replace the dead ones), or schedule the next attempt.
    pub(super) async fn on_attempt(
        &mut self,
        attempt: Result<
            anyhow::Result<(DaemonClient, mpsc::UnboundedReceiver<DaemonClientEvent>)>,
            tokio::sync::oneshot::error::RecvError,
        >,
        session: &mut SessionUi,
        view: &mut AgentView,
        events: &mut mpsc::UnboundedReceiver<DaemonClientEvent>,
        reader_dead: &mut tokio::sync::watch::Receiver<bool>,
    ) -> RecoveryStep {
        self.connect = None;
        // The deadline check runs here too: the tick arm parks while an attempt is in
        // flight, so the window can never overrun its advertised bound by more than the
        // in-flight attempt's connect leg.
        let Some(state) = self.reconnect.as_ref() else {
            return RecoveryStep::NextIteration;
        };
        let kind = state.kind;
        if tokio::time::Instant::now() > state.deadline {
            self.expire(kind, session, view);
            return RecoveryStep::Stop;
        }
        match attempt {
            Ok(Ok((client, fresh_events))) => {
                // The reattach self-bounds (a timeout cannot cancel the failure-path client
                // close); the budget's expiry is a RETRY outcome, never a fatal one (§10.4: a
                // queued attach can legitimately wait out a slow restore).
                match session.reattach_after_recovery(client, view, kind).await {
                    Ok(crate::session_ui::ReattachOutcome::Attached) => {
                        *events = fresh_events;
                        *reader_dead = session.client.reader_dead();
                        // The fresh connection owes nothing to the old one's loss states: a
                        // retained supervisor-loss flag or a session-plane retry left over from
                        // before the reconnect must not fire on the new link, and the loss watch
                        // re-arms (the new client's supervisor reader can die later, and the
                        // one-shot latch must not park that loss).
                        *self = Self::default();
                        session.reconnect = None;
                        session.dirty = true;
                    }
                    Ok(crate::session_ui::ReattachOutcome::AttachBudgetExceeded) => {
                        // The queued attach outlived the attempt's budget (a slow restore):
                        // schedule another on both paths (§10.4 — never a fatal exit).
                        session.note("the daemon is still restoring — retrying…", view);
                        session.dirty = true;
                        self.schedule_next_attempt();
                    }
                    // An unexpected-loss reattach failure is a hiccup like any other (the worker
                    // still respawning), and a shutdown recovery retries until its own bound
                    // lands the saved-transcript close: keep retrying through the window instead
                    // of exiting — the pane never dies to it (the operator's kicked-out class).
                    Err(error) if matches!(kind, RecoveryKind::Lost | RecoveryKind::Shutdown) => {
                        session.note_as(
                            &format!("reattach failed: {error:#} — retrying…"),
                            crate::chat::StatusKind::Warning,
                            view,
                        );
                        session.dirty = true;
                        self.schedule_next_attempt();
                    }
                    Err(error) => {
                        session.note(
                            &format!("reattach after the update failed: {error:#} — run `prime-agent attach` to resume"),
                            view,
                        );
                        session.exit_reason = "update_reattach_failed";
                        session.dirty = true;
                        return RecoveryStep::StopAfterFrame;
                    }
                }
            }
            Ok(Err(_)) => self.schedule_next_attempt(),
            // The attempt leg was dropped: the next tick re-arms.
            Err(_) => {}
        }
        RecoveryStep::Proceed
    }

    fn schedule_next_attempt(&mut self) {
        if let Some(state) = self.reconnect.take() {
            self.reconnect = Some(state.next_attempt());
        }
    }

    /// The session re-attach driver's attempt: re-attach the same session through the
    /// supervisor; the window's expiry surfaces the last error and the UI stays mounted.
    pub(super) async fn on_session_tick(
        &mut self,
        session: &mut SessionUi,
        view: &mut AgentView,
    ) -> RecoveryStep {
        let Some(mut state) = self.session_reconnect.take() else {
            return RecoveryStep::NextIteration;
        };
        // The user switched sessions while the link was down: the new attach owns its own
        // connection, so this driver stops.
        if state.active_session_id != session.active_session_id {
            return RecoveryStep::NextIteration;
        }
        // The reconnect attempt's budget covers the attach alone: the surface is already
        // up and its dock holds, so the first-frame fold's bounded fetches cannot eat the
        // 10s attempt budget on a slow daemon.
        let anchor = session.reader_anchor(view);
        let attempt = tokio::time::timeout(
            Duration::from_secs(SESSION_RECONNECT_ATTEMPT_TIMEOUT_S),
            session.attach_session(&state.active_session_id, crate::session_ui::DockFold::Held),
        )
        .await;
        state.last_error = match attempt {
            Ok(Ok(())) => {
                // The resynced transcript replaces the chat, then the reconnected status
                // lands on the rebuilt chat.
                session.resync_view(anchor, view).await;
                session.note_as("Daemon reconnected", crate::chat::StatusKind::Info, view);
                session.reconnection_failed = None;
                session.spawn_heartbeat_refresh();
                session.dirty = true;
                return RecoveryStep::Proceed;
            }
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => "the session re-attach attempt timed out".to_string(),
        };
        if tokio::time::Instant::now() > state.deadline {
            // The window expired: the last error surfaces as the closed event's
            // error row, and the UI stays mounted.
            let failure = format!("Daemon reconnection failed: {}", state.last_error);
            session.error_row(&failure, view);
            session.reconnection_failed = Some(state.last_error);
            session.dirty = true;
        } else {
            self.session_reconnect = Some(state.next_attempt());
        }
        RecoveryStep::Proceed
    }
}
