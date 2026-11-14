//! The opening phase: the input surface serves keystrokes while the session open runs (AGENTS.md
//! "Performance and the critical path": the input box mounts and accepts keystrokes without
//! waiting on daemon/kernel/config readiness that is not user-visible work). Editor-level keys
//! echo into the mounted startup chrome at the frame scheduler's cadence; everything else
//! (submits, commands, overlay keys) queues in `pending` — the same deque the run loop drains —
//! so typed-ahead is delivered, in order, the moment the open lands: never lost, never errored.
//! The open's own result (success or the daemon-refusal handoffs in [`open_failure`]) is
//! unchanged; only the WAIT moved off the input path.
//!
//! ORDERING (the contract the headless plans pin): the editor always holds a PREFIX of the
//! user's input stream. The first input that must queue (a submit, an overlay key, a mouse
//! report) parks the echo path for the rest of the phase — every later input queues behind it,
//! so a typed `H` can never land in the editor ahead of a queued `ctrl+home` the user pressed
//! first.

use super::run::next_ui_input;
use super::run_loop::{MIN_RENDER_INTERVAL, sleep_until_armed};
use super::{
    AgentView,
    DaemonClientEvent,
    ExitGuard,
    Instant,
    InteractiveOptions,
    InteractiveOutcome,
    Renderer,
    Result,
    SessionSelection,
    SessionUi,
    UiInput,
    VecDeque,
    mpsc,
};

/// The session open's result: the session's event receiver and UI, and whether the open waited
/// through a daemon update restart.
pub(super) type OpenedSession = (
    (mpsc::UnboundedReceiver<DaemonClientEvent>, SessionUi),
    bool,
);

/// How the opening phase ended.
pub(super) enum OpeningEnd {
    /// The open task finished (successfully or with the failure the handoffs classify).
    Opened(Box<Result<OpenedSession>>),
    /// `app.exit` on the empty editor: the run hands the terminal back without waiting out the
    /// open.
    Exited,
}

/// The opening phase's state: the queued typed-ahead and the echo frames' coalescing.
pub(super) struct OpeningPhase {
    /// The inputs that must wait for the open, in arrival order; the run loop drains them first.
    pub(super) pending: VecDeque<UiInput>,
    /// Whether editor-level keys still echo now. Base parity for the stash restore (finding
    /// #3): a session-reopen launch (Resume/Attach — the shape that can carry a stashed draft)
    /// never accepts typed-ahead into the editor during the open. The fold's
    /// `restore_prompt_stash_on_open` gates on the editor being EMPTY, so echoing into it
    /// pre-restore would silently consume the restore (the draft stays stashed while the
    /// typed-ahead sits in its place). A reopen's typed-ahead queues instead and dispatches
    /// through the editor AFTER the fold's restore — base ordering exactly. A NEW session owns
    /// no stash, so its launches keep the echo path (headless new-session plans included: no
    /// stash exists to protect).
    echoing: bool,
    /// The UI input channel closed during the phase (it carries over to the run loop).
    pub(super) input_closed: bool,
    last_render_at: Option<Instant>,
    render_deadline: Option<Instant>,
    dirty: bool,
    /// The startup chrome paints before the session loads only for a NEW chat (TS `ui.start()`
    /// renders the banner once before the session loads): the startup chrome carries the zero
    /// dock a fresh session mounts (`apply_startup_chrome`), so the placeholder frame never
    /// reflows when the attach lands. A direct open into an existing session holds the
    /// previous surface instead (TS attaches BEFORE the chat mounts — main.ts and the agents
    /// view construct the chat over an already-attached connection whose
    /// `getInitialSnapshot` is cached, so the first visible frame is the content): the queued
    /// clear rides the first draw's single flush, which carries the complete frame — no splash
    /// flash, no panel appearing late over a half-open view. The same predicate names the
    /// surface this phase may repaint: a NEW chat's chrome already mounted (its echo frames
    /// paint at the frame scheduler's cadence), while a held surface and headless capture runs
    /// never paint during the open — the first draw stays the open's content frame.
    pub(super) paints: bool,
    /// A first-run onboarding task owns the pane BEFORE the chat surface: its phase consumes
    /// `ui_rx` itself once the open lands (it needs the session, so it cannot run during the
    /// opening phase). The opening loop must therefore not touch the channel on an onboarding
    /// launch — the login/trace dialogs' keystrokes stay queued in the channel for that phase
    /// to consume in order, exactly the base ordering for this flow (the echo service here is
    /// the chat chrome's surface, the pane a New chat's open actually mounts).
    serves_input: bool,
}

impl OpeningPhase {
    pub(super) fn new(options: &InteractiveOptions, headless: bool) -> Self {
        let new_session = matches!(
            &options.session,
            SessionSelection::New | SessionSelection::NewChild { .. }
        );
        Self {
            pending: VecDeque::new(),
            echoing: new_session,
            input_closed: false,
            last_render_at: None,
            render_deadline: None,
            dirty: false,
            paints: !headless && new_session,
            serves_input: options.onboarding.is_none(),
        }
    }

    /// Serve input until the open task completes.
    pub(super) async fn run(
        &mut self,
        open_task: &mut tokio::task::JoinHandle<Result<OpenedSession>>,
        ui_rx: &mut mpsc::UnboundedReceiver<UiInput>,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<OpeningEnd> {
        loop {
            tokio::select! {
                // The open completing is the phase's exit; the failure handoffs run against the
                // same error exactly as before.
                open_result = &mut *open_task => {
                    return Ok(OpeningEnd::Opened(Box::new(open_result.unwrap_or_else(|join_error| {
                        Err(anyhow::anyhow!("the session open task failed: {join_error}"))
                    }))));
                }
                // A closed reader parks the arm instead of hot-spinning on the instantly-ready
                // closed channel while the open task still runs.
                maybe_input = next_ui_input(ui_rx, &mut self.input_closed), if self.serves_input => {
                    if let Some(input) = maybe_input
                        && self.on_input(input, view) {
                            return Ok(OpeningEnd::Exited);
                        }
                    self.paint_echo(view, renderer)?;
                }
                // The coalesced echo frame's deadline (armed only while a dirty frame waits out
                // the render interval).
                () = sleep_until_armed(self.render_deadline), if self.render_deadline.is_some() => {
                    self.render_deadline = None;
                    if let Some(terminal) = renderer.is_terminal_mut() {
                        crate::app::draw(terminal, view)?;
                    }
                    self.last_render_at = Some(Instant::now());
                    self.dirty = false;
                }
            }
        }
    }

    /// One input during the open; returns whether the run exits now.
    fn on_input(&mut self, input: UiInput, view: &mut AgentView) -> bool {
        match input {
            UiInput::Key(key) => {
                let Some(id) = crate::keys::key_event_to_id(&key) else {
                    return false;
                };
                // `app.exit` on the empty editor (default ctrl+d) is the one exit that must not
                // wait out the open: hand the terminal back now (the open task dies with the
                // runtime; the daemon-side create, if it already ran, leaves a resumable session
                // exactly like an exit right after the open). A queue already holding input keeps
                // the exit's ordering instead: the exit dispatches with the rest.
                if self.echoing
                    && view.editor.keybindings().matches(&id, "app.exit")
                    && view.editor.get_text().is_empty()
                {
                    return true;
                }
                // Pure editor keys echo now (the TS editor path) while the stream is still a
                // clean prefix — but only when the effective keymap leaves the key to the editor
                // fallback (finding #1): a key the post-open dispatch would claim for an
                // app/session action keeps its normal route, queued behind the open and
                // dispatched through the full keymap-aware ladder at the fold, instead of
                // becoming a stray editor motion (the misroute: default `left` on the empty
                // editor is `app.agents.back`, and a user-bound space or single char runs its
                // bound action). The first queue parks the echo path so later keys cannot jump
                // it.
                let editor_motion = matches!(
                    id.as_str(),
                    "backspace" | "delete" | "left" | "right" | "space"
                ) || id.chars().count() == 1;
                if self.echoing
                    && editor_motion
                    && !crate::session_ui::opening_echo_key_claimed(
                        view.editor.keybindings(),
                        &id,
                        &view.editor,
                    )
                {
                    view.editor.handle_input(&id);
                    self.dirty = true;
                } else {
                    self.queue(UiInput::Key(key));
                }
            }
            UiInput::Paste(text) => {
                if self.echoing {
                    let _ = view.editor.handle_paste(&text);
                    self.dirty = true;
                } else {
                    self.pending.push_back(UiInput::Paste(text));
                }
            }
            UiInput::Resize => {
                // Geometry is order-insensitive, so the resize always applies now: the same
                // update the run loop's resize arm performs, so the editor lays its window out
                // against the new row count and the echo frames paint at the new geometry
                // instead of the stale one.
                if let Ok((_width, height)) = crossterm::terminal::size() {
                    view.set_terminal_rows(height);
                }
                self.dirty = true;
            }
            // A write boundary carries no input: the echo continues, and queued
            // typed-ahead materializes on the run loop's tick after the open.
            UiInput::InputChunkEnd => {}
            other => self.queue(other),
        }
        false
    }

    /// Queue an input that must wait for the open; the echo path parks behind it.
    fn queue(&mut self, input: UiInput) {
        self.echoing = false;
        self.pending.push_back(input);
    }

    /// The echo frame coalesces on the run loop's own `MIN_RENDER_INTERVAL`: a typing burst paints
    /// as one frame, a lone key paints immediately.
    fn paint_echo(&mut self, view: &mut AgentView, renderer: &mut Renderer) -> Result<()> {
        if !self.paints || !self.dirty {
            return Ok(());
        }
        let now = Instant::now();
        if let Some(at) = self
            .last_render_at
            .filter(|at| now.duration_since(*at) < MIN_RENDER_INTERVAL)
        {
            self.render_deadline = Some(at + MIN_RENDER_INTERVAL);
            return Ok(());
        }
        if let Some(terminal) = renderer.is_terminal_mut() {
            crate::app::draw(terminal, view)?;
        }
        self.last_render_at = Some(now);
        self.dirty = false;
        self.render_deadline = None;
        Ok(())
    }
}

/// A failed open. A daemon refusal for the startup create/attach/resume hands the pane off to the
/// agents view instead of dying to the shell; only transport/protocol failures stay fatal (the
/// surface hands the terminal back before the CLI reports them).
pub(super) fn open_failure(
    error: anyhow::Error,
    options: &InteractiveOptions,
    exit_guard: &ExitGuard,
    renderer: Renderer,
    view: &mut AgentView,
) -> Result<InteractiveOutcome> {
    // The unknown-session check matches the daemon's RAW refusal message exactly - it must name
    // this attach's own selector - so a selector containing the phrase could not forge it.
    let unknown_session = match &options.session {
        SessionSelection::Attach(selector) => {
            let expected = format!("Unknown active session: {selector}");
            error
                .chain()
                .any(|cause| {
                    cause
                        .downcast_ref::<crate::daemon_client::RequestRejected>()
                        .is_some_and(|rejection| rejection.message == expected)
                })
                .then_some(selector)
        }
        SessionSelection::New | SessionSelection::NewChild { .. } | SessionSelection::Resume(_) => {
            None
        }
    };
    let handoff = if let Some(selector) = unknown_session {
        Some((
            format!(
                "No running or saved session matches \"{selector}\" — pick a session to continue."
            ),
            Some(selector.clone()),
        ))
    } else if crate::daemon_client::is_daemon_timeout(&error) {
        // A response/handshake timeout (the daemon alive but slow at load) is a hiccup, not a
        // protocol failure: the same session-picker fallback, never a fatal exit (operator
        // directive 2026-09-24).
        Some((format!("{error:#} — pick a session to continue."), None))
    } else if crate::daemon_client::is_daemon_rejection(&error)
        || crate::update_restart_wait::is_update_restart_deadline_error(&error)
    {
        // Any other daemon refusal gets the same session-picker fallback, and so does the open
        // wait's deadline failure: the guidance to retry must land on the view the user came
        // from.
        Some((format!("{error:#}"), None))
    } else {
        None
    };
    let Some((notice, query)) = handoff else {
        // The surface is already up: hand the terminal back before the CLI reports the failure.
        if renderer.is_terminal() {
            exit_guard.arm_for_exit();
        }
        renderer.finish(view, false);
        return Err(error);
    };
    // The handoff keeps the process alive: disarm the double-Ctrl+C watchdog.
    exit_guard.cancel();
    let frames = renderer.finish(view, true);
    Ok(InteractiveOutcome {
        return_to_agents_view: true,
        agents_view_notice: Some(notice),
        agents_view_query: query,
        frames,
        ..Default::default()
    })
}
