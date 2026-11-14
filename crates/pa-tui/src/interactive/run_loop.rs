//! The interactive run loop's per-iteration jobs over its state: the queued input batch with the
//! headless barriers, the input dispatch, the headless exit gate, the frame-wake inventory, and
//! the spinner and frame gate. The surface loop in `run.rs` owns the receivers its select
//! borrows and calls these in order.

use super::{
    AgentView,
    DaemonClient,
    DaemonClientEvent,
    Duration,
    HeadlessSettle,
    Instant,
    Renderer,
    Result,
    SessionUi,
    TerminalHandoff,
    UiInput,
    VecDeque,
    mpsc,
    spawn_session_reader,
};
use crate::suspend::SuspendTerminal;

/// Headless-only bound on the exit gate after [`UiInput::HeadlessDone`]: a settle member that never
/// drains would otherwise park the run forever; green settles take milliseconds.
const HEADLESS_SETTLE_TIMEOUT_MS: u64 = 60_000;

pub(super) const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);
/// The spinner's wall-clock cadence, not the render rate.
const SPINNER_INTERVAL_MS: u128 = 80;
/// The input-idle window the quiet tick waits out before materializing parked work.
const QUIET_TICK_INTERVAL: Duration = Duration::from_millis(50);

/// The animating loader's next phase boundary, the wake the select needs
/// while a quiet turn waits out its stream: TS `Loader`'s `setInterval`
/// keeps painting the 80ms cadence through quiet turns, and this
/// boundary is that interval's timer. The wake always precedes the
/// phase change it observes — an off-by-one here parks the loop for a
/// whole boundary instead of firing at the phase edge.
pub(super) fn next_spinner_deadline(started: Instant, now: Instant) -> Instant {
    // The remainder form keeps the arithmetic bounded by one phase: a
    // wide phase counter would truncate through `as usize` on 32-bit
    // targets after ~10.9 years of continuous animation and arm an
    // already-expired deadline, hot-spinning the select's wake.
    let into_phase = now.duration_since(started).as_millis() % SPINNER_INTERVAL_MS;
    now + Duration::from_millis((SPINNER_INTERVAL_MS - into_phase) as u64)
}

/// Sleep until `at`; an unarmed deadline never fires.
pub(super) async fn sleep_until_armed(at: Option<impl Into<tokio::time::Instant>>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// The `SubmitAndSettle` barrier's daemon round trip: submit the message
/// as a `prompt_and_wait`, then read the session's event sequence through
/// `get_rlm_children` — the one read-only command that reports it. Both
/// requests are bounded by `timeout_ms`.
async fn settled_sequence(
    client: &DaemonClient,
    active_session_id: String,
    message: String,
    timeout_ms: u64,
) -> anyhow::Result<u64> {
    let input = pa_types::daemon::PromptInput {
        content: None,
        images: None,
        streaming_behavior: Some(pa_types::daemon::StreamingBehavior::Steer),
        queue_if_busy: Some(true),
        expand_prompt_templates: None,
        source: None,
        agent_message_id: None,
        custom_message: None,
        queue_key: None,
        prefix_messages: None,
        admission_id: None,
        rlm_notice_nonce: None,
    };
    let waited = client
        .request_with_timeout(
            pa_types::daemon::DaemonCommand::PromptAndWait {
                id: None,
                active_session_id: active_session_id.clone(),
                message,
                input,
                rest: serde_json::Map::default(),
            },
            timeout_ms,
        )
        .await?;
    anyhow::ensure!(waited.success, "prompt_and_wait failed: {:?}", waited.error);
    let children = client
        .request_with_timeout(
            pa_types::daemon::DaemonCommand::GetRlmChildren {
                id: None,
                active_session_id,
                rest: serde_json::Map::default(),
            },
            timeout_ms,
        )
        .await?;
    children
        .data
        .as_ref()
        .and_then(|data| data.get("eventSequence"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("get_rlm_children carried no eventSequence"))
}

/// Which condition a headless render barrier waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeedleWait {
    /// `WaitRender`: a frame rendered after arming contains the needle.
    Shown,
    /// `WaitGone`: the newest frame no longer contains it.
    Gone,
}

/// How a drained input batch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchEnd {
    /// The iteration continues to its select.
    Drained,
    /// A handoff exit: the teardown runs now (its next surface owns the pane).
    LeaveRun,
}

/// How one applied input step leaves the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepEnd {
    /// The next queued input applies in this batch.
    More,
    /// The batch ends here.
    BatchDone,
    /// A handoff exit leaves the run.
    LeaveRun,
}

/// The surface loop's state across iterations.
pub(super) struct RunLoop {
    /// The inputs waiting for dispatch; the opening phase's typed-ahead rides in first.
    pub(super) pending: VecDeque<UiInput>,
    pub(super) running: bool,
    /// The headless plan completed (the pane's onboarding drive marks it too).
    pub(super) headless_done: bool,
    /// The UI input channel closed (see [`super::run::next_ui_input`]). The opening phase
    /// reads the same channel: a close it already saw carries over, or the loop's first select
    /// would discover it, park the arm, and never wake for the typed-ahead steps still queued.
    pub(super) ui_closed: bool,
    /// The settle bound's deadline (see [`HEADLESS_SETTLE_TIMEOUT_MS`]).
    headless_settle_deadline: Option<Instant>,
    headless_settle_pending: bool,
    pub(super) last_bash_refresh: Instant,
    pub(super) last_factory_refresh: Instant,
    /// The enhanced-key modes settle once (kitty answer or fallback) and report one adoption
    /// event; headless runs hold pipes and never probe.
    enhanced_keys_pending: bool,
    /// The hyperlink capability is env-based, settles at run start; terminal runs report it
    /// once.
    hyperlinks_pending: bool,
    /// State changes coalesce into at most one frame per `MIN_RENDER_INTERVAL`;
    /// `render_deadline` is armed while a dirty frame waits out the interval.
    last_render_at: Option<Instant>,
    pub(super) render_deadline: Option<Instant>,
    pub(super) quiet_tick_deadline: Option<Instant>,
    anim_started: Option<Instant>,
    /// The spinner phase painted by the last frame (`usize::MAX` before the first): a quiet turn
    /// only dirties when the 80ms phase advances.
    last_pulse_phase: usize,
    /// Whether the Ctrl+C exit hint painted a frame that the expiry must clear.
    hint_painted: bool,
    wait_idle_deadline: Option<Instant>,
    /// The headless `SubmitAndSettle` barrier's target and deadline.
    settle_target: Option<(u64, Instant)>,
    /// The headless render barrier's armed state: its deadline, and the
    /// frames captured at arming (the `WaitRender` condition scans only
    /// frames rendered after the barrier became the queue's head, so a
    /// needle that already scrolled out of an older frame still satisfies
    /// it; `WaitGone` checks only the newest frame).
    wait_render_deadline: Option<Instant>,
    wait_render_baseline: usize,
    /// The frame count a holding render barrier last checked: a frame painted after it wakes the
    /// loop for the re-check (see the frame-wake inventory).
    wait_render_checked: usize,
}

impl RunLoop {
    pub(super) fn new(
        terminal: bool,
        pending: VecDeque<UiInput>,
        ui_closed: bool,
        headless_done: bool,
    ) -> Self {
        Self {
            pending,
            running: true,
            headless_done,
            ui_closed,
            headless_settle_deadline: None,
            headless_settle_pending: false,
            last_bash_refresh: Instant::now(),
            last_factory_refresh: Instant::now(),
            enhanced_keys_pending: terminal,
            hyperlinks_pending: terminal,
            last_render_at: None,
            render_deadline: None,
            quiet_tick_deadline: None,
            anim_started: None,
            last_pulse_phase: usize::MAX,
            hint_painted: false,
            wait_idle_deadline: None,
            settle_target: None,
            wait_render_deadline: None,
            wait_render_baseline: 0,
            wait_render_checked: 0,
        }
    }

    /// Arm the frame deadline at `at` unless an earlier one is already armed.
    fn wake_at(&mut self, at: Instant) {
        if self.render_deadline.is_none_or(|armed| at < armed) {
            self.render_deadline = Some(at);
        }
    }

    /// The run's one-shot capability reports: the enhanced-key modes once they settle, and the
    /// OSC 8 hyperlink gate. The flush shutdown can wait out a slow endpoint, so the hyperlink
    /// event is spawned instead of awaited.
    pub(super) async fn report_capabilities(&mut self, session: &SessionUi) {
        if self.enhanced_keys_pending
            && let Some((kitty, modify_other_keys)) = crate::enhanced_keys::settle_state()
        {
            if let Some(telemetry) = &session.telemetry {
                telemetry.enhanced_keys(kitty, modify_other_keys).await;
            }
            self.enhanced_keys_pending = false;
        }
        if self.hyperlinks_pending {
            if let Some(telemetry) = session.telemetry.clone() {
                let enabled = crate::hyperlinks::hyperlinks_enabled();
                tokio::spawn(async move {
                    telemetry.hyperlinks_active(enabled).await;
                });
            }
            self.hyperlinks_pending = false;
        }
    }

    /// One queued UI input from the select: a keystroke restarts the quiet window, so the rest
    /// of a typed burst (a command plus its Enter) applies before a parked request materializes.
    pub(super) fn queue_input(&mut self, input: UiInput) {
        if matches!(input, UiInput::Key(_) | UiInput::Paste(_)) {
            self.quiet_tick_deadline = None;
        }
        self.pending.push_back(input);
    }

    /// Drain the whole queued input batch in this one iteration (TS dispatches every event of a
    /// stdin chunk): a burst applies as one batch instead of one render pass per event. The
    /// headless barriers hold at the queue head until their condition (or deadline) releases
    /// them.
    pub(super) async fn drain_inputs(
        &mut self,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<BatchEnd> {
        let mut inputs_pending = !self.pending.is_empty();
        // Whether this iteration applied a headless plan step (its frame paints pre-select).
        let mut step_applied = false;
        while inputs_pending {
            inputs_pending = match self.pending.front() {
                Some(UiInput::WaitIdle { timeout_ms }) => {
                    let timeout_ms = *timeout_ms;
                    self.wait_idle_barrier(timeout_ms, session, view)
                }
                Some(UiInput::SubmitAndSettle { text, timeout_ms }) => {
                    let (text, timeout_ms) = (text.clone(), *timeout_ms);
                    self.settle_barrier(text, timeout_ms, session, view).await
                }
                Some(UiInput::WaitRender { needle, timeout_ms }) => {
                    let (needle, timeout_ms) = (needle.clone(), *timeout_ms);
                    self.render_barrier(
                        &needle,
                        timeout_ms,
                        NeedleWait::Shown,
                        session,
                        view,
                        renderer,
                    )
                }
                Some(UiInput::WaitGone { needle, timeout_ms }) => {
                    let (needle, timeout_ms) = (needle.clone(), *timeout_ms);
                    self.render_barrier(
                        &needle,
                        timeout_ms,
                        NeedleWait::Gone,
                        session,
                        view,
                        renderer,
                    )
                }
                // Timing markers observe the previous frame without dirtying the view or
                // adding a select/render iteration to the measured input sequence. The render
                // count rides along: the frame capture dedupes on plain text, so restyle-only
                // renders need a separate witness.
                Some(UiInput::Timestamp(sender)) => {
                    sender.send((Instant::now(), renderer.headless_renders()))?;
                    self.pending.pop_front();
                    true
                }
                Some(_) => {
                    let input = self
                        .pending
                        .pop_front()
                        .expect("the queue head was checked");
                    step_applied = true;
                    match self.apply_step(input, session, view, renderer).await? {
                        StepEnd::More => true,
                        StepEnd::BatchDone => false,
                        StepEnd::LeaveRun => return Ok(BatchEnd::LeaveRun),
                    }
                }
                // Without this arm the drain loop would spin on the empty queue — the select
                // below would never run again, starving every render and input after the first
                // batch.
                None => false,
            };
        }
        // A headless step paints its frame before the select can fold anything else in: a
        // transient state the step mounted (the `/share` loader while `gh` runs) would otherwise
        // be replaced unpainted when its outcome arm wins the select's race against the frame.
        if step_applied && session.dirty && !renderer.is_terminal() {
            renderer.render_headless(session, view);
            self.last_pulse_phase = view.pulse_frame;
            self.render_deadline = None;
        }
        Ok(BatchEnd::Drained)
    }

    /// The `WaitIdle` barrier: a parked follow-up/steering message keeps the barrier waiting
    /// until the session delivers it, and so does a picked-up prompt whose turn is still
    /// preparing (the "Starting" row: between the previous turn's end and this turn's start,
    /// the turn is inactive and the lanes are empty); a submit whose round trip is still armed
    /// holds the barrier too (the async submit resolves off the render path). Returns whether
    /// the batch continues.
    fn wait_idle_barrier(
        &mut self,
        timeout_ms: u64,
        session: &mut SessionUi,
        view: &mut AgentView,
    ) -> bool {
        let busy = session.turn_active
            || !view.queued.is_empty()
            || view.queued.starting.is_some()
            || session.prompt_submits_in_flight() > 0;
        if !busy {
            self.wait_idle_deadline = None;
            self.pending.pop_front();
            return true;
        }
        match self.wait_idle_deadline {
            None => {
                self.wait_idle_deadline = Some(Instant::now() + Duration::from_millis(timeout_ms));
            }
            Some(deadline) if Instant::now() > deadline => {
                self.wait_idle_deadline = None;
                self.pending.pop_front();
                session.note("timed out waiting for the turn to finish", view);
            }
            Some(_) => {}
        }
        false
    }

    /// The `SubmitAndSettle` barrier. The supervisor races its response and event queues in one
    /// select, so the response arriving proves nothing: hold until this run has processed the
    /// settled sequence. Returns whether the batch continues.
    async fn settle_barrier(
        &mut self,
        text: String,
        timeout_ms: u64,
        session: &mut SessionUi,
        view: &mut AgentView,
    ) -> bool {
        let Some((settled, deadline)) = self.settle_target else {
            match settled_sequence(
                &session.client,
                session.active_session_id.clone(),
                text,
                timeout_ms,
            )
            .await
            {
                Ok(settled) => {
                    self.settle_target =
                        Some((settled, Instant::now() + Duration::from_millis(timeout_ms)));
                }
                Err(error) => {
                    session.note(&format!("settle barrier failed: {error:#}"), view);
                    self.pending.pop_front();
                }
            }
            return true;
        };
        if session.last_event_sequence >= settled {
            self.settle_target = None;
            self.pending.pop_front();
            return true;
        }
        if Instant::now() > deadline {
            session.note("timed out waiting for the turn to settle", view);
            self.settle_target = None;
            self.pending.pop_front();
            return true;
        }
        false
    }

    /// The render barrier: `WaitRender` holds until a frame rendered after arming contains the
    /// needle; `WaitGone` holds until the newest frame cleared it (the baseline is recorded at
    /// arming). The timeout note never embeds the needle: quoting it would make a timed-out wait
    /// satisfy the very condition that failed. Returns whether the batch continues.
    fn render_barrier(
        &mut self,
        needle: &str,
        timeout_ms: u64,
        wait: NeedleWait,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &Renderer,
    ) -> bool {
        let shown = wait == NeedleWait::Shown;
        let frames = renderer.headless_frames();
        let Some(deadline) = self.wait_render_deadline else {
            let current_state_ok = frames.is_some_and(|frames| {
                frames
                    .last()
                    .is_some_and(|frame| frame.contains(needle) == shown)
            });
            if current_state_ok {
                self.pending.pop_front();
                return true;
            }
            self.wait_render_baseline = frames.map_or(0, <[String]>::len);
            self.wait_render_checked = self.wait_render_baseline;
            self.wait_render_deadline = Some(Instant::now() + Duration::from_millis(timeout_ms));
            return false;
        };
        let satisfied = frames.is_some_and(|frames| match wait {
            NeedleWait::Shown => frames
                .get(self.wait_render_baseline..)
                .unwrap_or_default()
                .iter()
                .any(|frame| frame.contains(needle)),
            NeedleWait::Gone => !frames.last().is_some_and(|frame| frame.contains(needle)),
        });
        if satisfied {
            self.wait_render_deadline = None;
            self.pending.pop_front();
            return true;
        }
        if Instant::now() > deadline {
            self.wait_render_deadline = None;
            self.pending.pop_front();
            session.note(
                match wait {
                    NeedleWait::Shown => "timed out waiting for the headless render condition",
                    NeedleWait::Gone => "timed out waiting for the headless render to clear",
                },
                view,
            );
            return true;
        }
        self.wait_render_checked = frames.map_or(0, <[String]>::len);
        false
    }

    /// Apply one input, paint it, and decide whether the batch goes on.
    async fn apply_step(
        &mut self,
        input: UiInput,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<StepEnd> {
        let chunk_end = matches!(input, UiInput::InputChunkEnd);
        if !chunk_end {
            session.dirty = true;
        }
        self.apply_input(input, session, view, renderer).await?;
        // Paint the handled input in this iteration: the select below can otherwise wait out its
        // 50ms tick before the next draw, felt as keystroke-to-render lag. A handoff paints
        // nothing: the next surface owns the pane.
        if let Some(terminal) = renderer
            .is_terminal_mut()
            .filter(|_| !chunk_end || session.dirty)
        {
            if !session.open_agents_view && session.pending_selection.is_none() {
                // The inline paint must reflect tray state the handled key just armed (the
                // Ctrl+C exit hint): the refresh below the select only reaches the frame
                // gate, and the inline paint clears `dirty` — an idle terminal would
                // otherwise never show the armed hint.
                view.chrome.tray_override = session.tray_override(view);
                crate::app::draw(terminal, view)?;
                // The 16ms gate measures its interval from this frame.
                self.last_render_at = Some(Instant::now());
                self.last_pulse_phase = view.pulse_frame;
                self.render_deadline = None;
            }
            session.dirty = false;
        }
        // An exit key must not wait out the select tick before the bounded shutdown path
        // runs.
        if !self.running {
            return Ok(StepEnd::BatchDone);
        }
        // `exit_requested` is the leave-now signal (agents-back, `/resume`, `/exit`): the
        // teardown below must run this iteration, not after the select's 50ms idle tick
        // parks the loop — that park reads as switch latency. A HANDOFF takes the leave now
        // (its next surface owns the pane). A non-handoff exit keeps the tail pass: its
        // frame gate paints the final chat frame the exit's main-screen flush shows;
        // headless keeps the tail pass so captured frames stay identical.
        if renderer.is_terminal() && session.exit_requested {
            session.exit_reason = "session_request";
            if session.open_agents_view || session.pending_selection.is_some() {
                return Ok(StepEnd::LeaveRun);
            }
            return Ok(StepEnd::BatchDone);
        }
        // Headless input keeps the one-step-per-iteration order the plans were written
        // against: every step renders before the next applies (the captured frame sequence
        // IS the verifier evidence). The terminal path keeps the full batch drain.
        if renderer.is_terminal() {
            Ok(StepEnd::More)
        } else {
            Ok(StepEnd::BatchDone)
        }
    }

    /// Dispatch one input to the session.
    async fn apply_input(
        &mut self,
        input: UiInput,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<()> {
        match input {
            UiInput::Key(key) => self.apply_key(key, session, view, renderer).await?,
            UiInput::Paste(text) => {
                session.stop_selection_auto_scroll();
                // The inline auth panel owns the frame: the paste lands in its field, never
                // in the editor.
                if view.auth_panel.is_some() {
                    session.paste_to_auth_panel(&text, view);
                } else {
                    session.handle_paste(&text, view);
                }
            }
            // A mouse report reaches the transcript scroll dispatch's wheel branch;
            // non-wheel reports are consumed inside.
            UiInput::Mouse(event) => {
                session.handle_mouse(event, view);
            }
            // The headless plan's pause step or the end of a terminal write: the queued
            // keystroke batch ahead of this barrier is fully handled, so the parked
            // suggestions materialize now (into this input's inline paint on a terminal).
            UiInput::SettleIdle | UiInput::InputChunkEnd => {
                session.materialize_editor_autocomplete(view);
            }
            UiInput::Submit(text) => {
                session.stop_selection_auto_scroll();
                let dispatched = session
                    .submit_prompt(&text, crate::session_ui::SubmitBehavior::Steer, view)
                    .await;
                if let Err(error) = dispatched {
                    // A rejected submission surfaces the `⚠ Error` row and keeps the client
                    // mounted with the draft restored.
                    session.error_row(&format!("{error:#}"), view);
                    view.editor.set_text(&text);
                    session.dirty = true;
                }
                if session.pending_traces_login() {
                    session.run_traces_login(view);
                }
            }
            UiInput::Timestamp(_) => unreachable!("timing marker handled above"),
            UiInput::HeadlessDone => self.headless_done = true,
            UiInput::WaitRender { .. } | UiInput::WaitGone { .. } => {
                unreachable!("render barrier handled above")
            }
            UiInput::ScrollTop => {
                session.stop_selection_auto_scroll();
                view.scroll_to_top();
                session.window.request_head();
            }
            UiInput::Resize => {
                session.stop_selection_auto_scroll();
                if let Ok((_width, height)) = crossterm::terminal::size() {
                    view.set_terminal_rows(height);
                }
                // A font-size change resizes the cells, not just the grid (and a
                // tmux layout change can move the pane).
                crate::terminal_image::terminal_resized();
            }
            UiInput::WaitIdle { .. } | UiInput::SubmitAndSettle { .. } => {
                unreachable!("barrier handled above")
            }
        }
        Ok(())
    }

    /// One key: the session's dispatch, then the terminal handoffs it requested (the traces
    /// login, a suspend, the external editor, the MCP auth panel).
    async fn apply_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<()> {
        session.stop_selection_auto_scroll();
        match session.handle_key(key, view, &mut self.running).await {
            Ok(()) => {}
            // A daemon refusal or a connection failure on this key's request
            // surfaces an error row and keeps the loop running with the editor
            // state preserved — a failed request never exits the client.
            Err(error)
                if crate::daemon_client::is_daemon_rejection(&error)
                    || crate::daemon_client::is_daemon_unreachable(&error) =>
            {
                session.error_row(&format!("{error:#}"), view);
            }
            // Everything else (protocol corruption) stays fatal.
            Err(error) => return Err(error),
        }
        if session.pending_traces_login() {
            session.run_traces_login(view);
        }
        if session.take_suspend_request() && renderer.is_terminal_mut().is_some() {
            suspend_to_shell(session, view, renderer);
        }
        if let Some(command) = session.take_external_editor_request() {
            edit_in_external_editor(&command, session, view, renderer).await;
        }
        // The `/mcp` view resolved to an auth request: mount the inline auth panel
        // and spawn the client auth command — no flow touches the terminal.
        if session.pending_mcp_auth() {
            session.run_mcp_auth(view);
        }
        Ok(())
    }

    /// The headless exit gate: the plan completed, and the run ends once every settle member
    /// drains (an in-flight `/share` upload or inline auth flow holds the run open like an
    /// active turn; a live terminal never ends the run on its own). The members are snapshotted
    /// so the bound can name what stuck. Returns whether the run ends.
    pub(super) fn headless_settled(
        &mut self,
        session: &SessionUi,
        view: &AgentView,
    ) -> Result<bool> {
        if !self.headless_done {
            return Ok(false);
        }
        let settle = HeadlessSettle::snapshot(
            session,
            view,
            self.pending.len(),
            self.wait_idle_deadline.is_some(),
        );
        if settle.settled() {
            return Ok(true);
        }
        // The settle bound: the gate's wait is the harness's only unbounded one, so a member
        // that never drains fails the run with its name instead of wedging the test binary
        // forever (the CI wedge family).
        self.headless_settle_pending = true;
        let deadline = self
            .headless_settle_deadline
            .get_or_insert(Instant::now() + Duration::from_millis(HEADLESS_SETTLE_TIMEOUT_MS));
        if Instant::now() >= *deadline {
            anyhow::bail!(
                "the headless run's settle did not complete within {}ms of the plan's completion: {}",
                HEADLESS_SETTLE_TIMEOUT_MS,
                settle.blockers().join("; ")
            );
        }
        Ok(false)
    }

    /// The frame-wake inventory: every pending-work state whose observation needs a loop
    /// iteration arms the frame deadline here, pre-select, so a dirty frame (the open's first
    /// paint; a keystroke's toast or hint) never parks behind a select with no other wake. The
    /// headless barriers' deadlines and the hint/toast expiry arm HERE, not after the frame
    /// gate; the quiet tick arms while parked work waits for typing to pause.
    pub(super) fn arm_wakes(&mut self, session: &SessionUi, view: &AgentView, renderer: &Renderer) {
        // An exit key must not wait out the select: the loop condition consumes
        // `running`/`exit_requested` at the NEXT wake, and the input batch's bare `break` only
        // leaves the batch. The frame arm is the one that can wake now, so a pending exit takes
        // it immediately — the tail pass paints its final frame within the same iteration.
        if !self.running || session.exit_requested {
            self.wake_at(Instant::now());
        }
        if (session.dirty || session.wants_transcript_page(view)) && self.render_deadline.is_none()
        {
            self.render_deadline = Some(Instant::now());
        }
        // Headless plan steps apply one per iteration, and once the driver's channel closed its
        // arm parks (see [`super::run::next_ui_input`]), so the queue wakes its own next
        // iteration: a runnable head step wakes the loop now, and a holding render barrier as
        // soon as a frame painted after its last check. The state barriers (`WaitIdle`,
        // `SubmitAndSettle`) wake on the session events that release them, plus their own
        // deadlines below. An open channel keeps its own pacing: the driver's next send is the
        // wake.
        if self.ui_closed && !renderer.is_terminal() {
            let wake_now = match self.pending.front() {
                None | Some(UiInput::WaitIdle { .. } | UiInput::SubmitAndSettle { .. }) => false,
                Some(UiInput::WaitRender { .. } | UiInput::WaitGone { .. }) => {
                    renderer.headless_frames().map_or(0, <[String]>::len)
                        != self.wait_render_checked
                }
                Some(_) => true,
            };
            if wake_now {
                self.render_deadline = Some(Instant::now());
            }
        }
        let barrier_deadlines = [
            self.wait_idle_deadline,
            self.wait_render_deadline,
            self.settle_target.map(|(_, deadline)| deadline),
        ];
        for deadline in barrier_deadlines
            .into_iter()
            .flatten()
            .chain(session.ctrl_c_hint_expiry())
            .chain(view.toasts.next_expiry())
        {
            self.wake_at(deadline);
        }
        // The animating loader's next phase boundary: every paint clears
        // the deadline it satisfied, and the boundary is the only wake a
        // quiet turn has between stream events (a running tool, a
        // provider gap, silent thinking) — without this arm the select
        // parked until the 2s bash-activity poll, freezing the spinner
        // and skipping whole seconds of the elapsed counter.
        if let Some(started) = self.anim_started {
            self.wake_at(next_spinner_deadline(started, Instant::now()));
        }
        // A settle waiting out a member is pending work like the
        // autocomplete park: the gate runs at the loop top, so its
        // re-check (and the settle bound's expiry) needs this arm's
        // wake — a fully quiet select would otherwise park the
        // settle forever, and the bound itself fires only on an
        // iteration. Terminal runs never arm it (`headless_done`
        // exists only on the headless harness). The window anchors at the first iteration that
        // sees pending work: another arm's wake must not restart it, or a streaming reply
        // starves the parked request.
        let quiet_work = view.editor.has_pending_autocomplete()
            || session.selection_auto_scroll_armed()
            || (self.headless_done && self.headless_settle_pending);
        self.quiet_tick_deadline = if quiet_work {
            self.quiet_tick_deadline
                .or_else(|| Some(Instant::now() + QUIET_TICK_INTERVAL))
        } else {
            None
        };
    }

    /// After the select: the spinner phase, the Ctrl+C hint and toast expiries, the tray
    /// override, and the frame gate.
    pub(super) fn animate_and_paint(
        &mut self,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<()> {
        // Spinner animation: the wall clock drives the phase, not the render rate — the frame gate
        // below caps renders, and only a phase change dirties the frame.
        let animating = session.turn_active
            || view.retry.is_some()
            || view.compaction.is_some()
            || view.share_loader.is_some();
        if animating {
            let now = Instant::now();
            let started = *self.anim_started.get_or_insert(now);
            let phase = (now.duration_since(started).as_millis() / SPINNER_INTERVAL_MS) as usize;
            view.pulse_frame = phase;
            if phase != self.last_pulse_phase {
                session.dirty = true;
            }
            // Arm the next phase boundary: without a deadline the select
            // would only wake on the 50ms tick, adding up to a full tick
            // of spinner latency to every phase change.
            self.wake_at(next_spinner_deadline(started, now));
        } else {
            self.anim_started = None;
            self.last_pulse_phase = usize::MAX;
        }

        // The Ctrl+C exit hint expires on a timer: once the window passed, the hint row repaints
        // away — the expiry's wake arm lives in the pre-select inventory, and the same
        // iteration's frame gate repaints it away.
        if session.ctrl_c_hint_expiry().is_some() {
            self.hint_painted = true;
        } else if self.hint_painted {
            session.dirty = true;
            self.hint_painted = false;
        }

        // The action toasts auto-dismiss on their TTL: once one goes, the overlay repaints away.
        if view.toasts.prune_expired(Instant::now()) {
            session.dirty = true;
        }

        // The tray override row follows the session's hint state on every frame. Refreshed here —
        // after the select, right before the paint — because a loop-top refresh goes stale across
        // the select's sleep: the expiry wake would repaint the hint with the pre-sleep value.
        view.chrome.tray_override = session.tray_override(view);

        self.frame_gate(session, view, renderer)?;
        if session.exit_requested {
            session.exit_reason = "session_request";
            self.running = false;
        }
        Ok(())
    }

    /// The frame gate: at most one render per `MIN_RENDER_INTERVAL` — every state change inside
    /// the window coalesces into the next frame (a stream burst renders at most one frame per
    /// tick), and a dirty state waits for the deadline arm instead of burning a render now.
    fn frame_gate(
        &mut self,
        session: &mut SessionUi,
        view: &mut AgentView,
        renderer: &mut Renderer,
    ) -> Result<()> {
        if !session.dirty {
            // A fired deadline with nothing dirty to paint must not re-fire on every iteration
            // (the arm would return immediately): drop it. A future arm (the spinner's next phase
            // boundary) stays — the select needs that wakeup.
            if self
                .render_deadline
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                self.render_deadline = None;
            }
            return Ok(());
        }
        let Some(terminal) = renderer.is_terminal_mut() else {
            // Headless capture keeps the per-change frame sequence
            // the verifiers assert on: no wall-clock interval applies.
            // The paint bookkeeping matches the terminal's: a paint
            // satisfied the armed deadline, and the pre-select
            // inventory re-arms every wake still pending, so the
            // harness runs the terminal's wake path.
            renderer.render_headless(session, view);
            session.dirty = false;
            self.last_pulse_phase = view.pulse_frame;
            self.render_deadline = None;
            return Ok(());
        };
        if let Some(last) = self
            .last_render_at
            .filter(|at| at.elapsed() < MIN_RENDER_INTERVAL)
        {
            self.render_deadline = Some(last + MIN_RENDER_INTERVAL);
            return Ok(());
        }
        crate::app::draw(terminal, view)?;
        session.dirty = false;
        self.last_render_at = Some(Instant::now());
        self.last_pulse_phase = view.pulse_frame;
        self.render_deadline = None;
        // The attach fold arms this once: the first frame that renders the rebuilt
        // transcript materializes its visible window, so return that freed heap right
        // after the frame paints instead of keeping the resume's peak resident.
        if session.take_trim_after_frame() {
            pa_types::memory_release::trim_freed_heap();
        }
        Ok(())
    }

    /// What the loop does after a recovery arm; returns whether the iteration skips its frame
    /// pass.
    pub(super) fn after_recovery(&mut self, step: super::reconnect::RecoveryStep) -> bool {
        use super::reconnect::RecoveryStep;
        match step {
            RecoveryStep::Proceed => false,
            RecoveryStep::NextIteration => true,
            RecoveryStep::Stop => {
                self.running = false;
                true
            }
            RecoveryStep::StopAfterFrame => {
                self.running = false;
                false
            }
        }
    }
}

/// Fold a batch of session events: the first event plus every queued frame (a stream burst
/// applies as one transcript pass instead of one full re-layout per frame), the tray's context
/// refresh on a settled turn or compaction, and the rebind or resync the batch requested.
pub(super) async fn fold_session_events(
    first: DaemonClientEvent,
    events: &mut mpsc::UnboundedReceiver<DaemonClientEvent>,
    was_active: bool,
    session: &mut SessionUi,
    view: &mut AgentView,
) {
    session.apply_client_event(first, view);
    while let Ok(event) = events.try_recv() {
        session.apply_client_event(event, view);
    }
    // A settled turn or compaction refreshes the tray's context usage.
    let compaction_settled = std::mem::take(&mut session.compaction_settled);
    if compaction_settled || (was_active && !session.turn_active) {
        session.refresh_stats().await;
        session.rebuild_tray(view);
    }
    // A `session_binding` supersede notice: the session lives under a new active
    // id, so re-attach to it — the transcript rebuilds from the snapshot (silent,
    // no banner). A failed re-attach changes nothing (the old one detaches only
    // after a new attach succeeds); the next supersede notice or submit-path retry
    // re-attaches once a worker can serve the session.
    if let Some(current) = session.pending_rebind.take() {
        // The rebind's fresh attach is also the resync.
        session.pending_resync = false;
        match session
            .attach_session(&current, crate::session_ui::DockFold::FirstFrame)
            .await
        {
            Ok(()) => session.rebuild_view(view, &crate::session_ui::RebuildKind::Rebind),
            Err(error) => session.note(&format!("session rebind failed: {error:#}"), view),
        }
    }
    // Lost session events (the supervisor's queue for this connection
    // overflowed): re-attach the same session and rebuild from its snapshot,
    // so a dropped `tool_execution_end`/`agent_end` cannot leave the view
    // waiting on a turn that already ended. A rebind above already did it.
    if std::mem::take(&mut session.pending_resync) {
        let current = session.active_session_id.clone();
        match session
            .attach_session(&current, crate::session_ui::DockFold::Held)
            .await
        {
            Ok(()) => session.rebuild_view(view, &crate::session_ui::RebuildKind::Resync),
            Err(error) => session.note(&format!("session resync failed: {error:#}"), view),
        }
    }
}

/// Hand the terminal to the shell and stop the process group; execution continues here once the
/// user foregrounds the process, where the cycle re-applies raw mode, the alt screen, and SGR
/// mouse tracking. Headless runs keep no terminal renderer, so the request is observed and
/// dropped.
fn suspend_to_shell(session: &mut SessionUi, view: &mut AgentView, renderer: &mut Renderer) {
    match crate::suspend::suspend_cycle(
        &mut crate::suspend::ProcessSignals,
        &mut TerminalHandoff { renderer, view },
    ) {
        Ok(()) => session.track_suspend_used("resumed"),
        Err(error) => {
            session.track_suspend_used("failed");
            session.error_row(&format!("{error:#}"), view);
            // Try to take the terminal back so the run stays usable; if that also fails, the
            // draw below surfaces the broken frame.
            let _ = renderer.resume();
        }
    }
}

/// Hand the terminal to the configured editor, then resume. The input reader would steal the
/// editor's keys, so it stops (flag + join) and respawns after the resume. Headless runs own no
/// reader and skip the handoff.
async fn edit_in_external_editor(
    command: &str,
    session: &mut SessionUi,
    view: &mut AgentView,
    renderer: &mut Renderer,
) {
    let (ui_tx, exit_guard) = match &*renderer {
        Renderer::Terminal {
            ui_tx, exit_guard, ..
        } => (ui_tx.clone(), exit_guard.clone()),
        Renderer::Headless { .. } => return,
    };
    crate::input::stop_reader();
    // The suspend path's SIGINT shield: the handoff restores cooked mode (ISIG), and a
    // cooked-mode editor wrapper (`code --wait`, `subl -w`) turns Ctrl+C into SIGINT for the
    // shared foreground group — the default disposition would kill the TUI mid-edit. The no-op
    // handler (never SIG_IGN) keeps the child's own Ctrl+C (handled signals reset across exec).
    let mut signals = crate::suspend::ProcessSignals;
    let shielded = if crate::suspend::supported() {
        use crate::suspend::SuspendSignals;
        signals.ignore_sigint()
    } else {
        Ok(())
    };
    let stopped = shielded.and_then(|()| TerminalHandoff { renderer, view }.stop());
    let outcome = match stopped {
        Ok(()) => crate::external_editor::edit(command, &view.editor.get_expanded_text()).await,
        Err(error) => Err(error),
    };
    // SIGINT is back to default before the surface takes over.
    if crate::suspend::supported() {
        use crate::suspend::SuspendSignals;
        let _ = signals.restore_sigint();
    }
    // The surface returns even when the editor run failed.
    let resumed = TerminalHandoff { renderer, view }.resume();
    if let Err(error) = resumed {
        session.error_row(&format!("{error:#}"), view);
    }
    spawn_session_reader(ui_tx, exit_guard);
    session.apply_external_editor_outcome(outcome, view);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary the select arms is always the current phase's
    /// 80ms edge — the wake must precede the phase change it observes
    /// (an off-by-one parks the loop for a whole boundary). Samples sit
    /// at least a millisecond inside their phase because `Instant`
    /// round-trips can lose sub-millisecond ticks on the platform
    /// clock, and the phase is floored from whole milliseconds.
    #[test]
    fn spinner_deadline_is_the_current_phase_edge() {
        let started = Instant::now();
        let at = |ms: u64| started + Duration::from_millis(ms);
        assert_eq!(next_spinner_deadline(started, started), at(80));
        assert_eq!(next_spinner_deadline(started, at(1)), at(80));
        assert_eq!(next_spinner_deadline(started, at(79)), at(80));
        assert_eq!(next_spinner_deadline(started, at(81)), at(160));
        assert_eq!(next_spinner_deadline(started, at(161)), at(240));
        assert_eq!(next_spinner_deadline(started, at(239)), at(240));
    }
}
