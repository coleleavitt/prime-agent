//! The interactive run's entry points and open routes, and the terminal/headless surface loop:
//! the session open behind the opening phase, the post-open fold, the select over the session's
//! events and background channels, and the teardown.

use anyhow::Context;

use super::opening::{OpenedSession, OpeningEnd, OpeningPhase, open_failure};
use super::reconnect::Recovery;
use super::run_loop::{BatchEnd, RunLoop, fold_session_events, sleep_until_armed};
use super::{
    ACTIVITY_POLL_INTERVAL,
    AgentView,
    DaemonClient,
    Duration,
    ExitGuard,
    InteractiveOptions,
    InteractiveOutcome,
    PaneDrive,
    Renderer,
    Result,
    SelectionOrigin,
    SessionPanel,
    SessionUi,
    TELEMETRY_EXIT_TIMEOUT_MS,
    UiInput,
    UiMode,
    apply_startup_chrome,
    check_tmux_keyboard_setup,
    mpsc,
    run_onboarding_phase,
};

/// The surface loop's UI-input arm: the next input, parking for good once the
/// channel closed (`closed` remembers it). A closed `recv()` resolves `None`
/// on every poll, so an unparked arm hot-spins the select; the headless
/// driver drops its sender as soon as it queued the plan, while a `WaitIdle`
/// barrier still holds `HeadlessDone` in the pending queue. Once every
/// iteration also paints (a spinner phase per frame, when a frame outlasts
/// the 80ms phase on a big transcript or a loaded box), the spinning loop
/// stops yielding to the session reader on the same runtime: the stream
/// stops applying and the barrier expires.
///
/// The close itself resolves once (`None`): the loop must take one more
/// iteration to see `closed` and wake for the steps still queued in
/// `pending` (the opening phase queues typed-ahead input there). Parking
/// on the very poll that discovered the close would hold that select
/// forever with runnable steps queued.
pub(super) async fn next_ui_input(
    ui_rx: &mut mpsc::UnboundedReceiver<UiInput>,
    closed: &mut bool,
) -> Option<UiInput> {
    if !*closed {
        let input = ui_rx.recv().await;
        *closed = input.is_none();
        return input;
    }
    std::future::pending().await
}

/// Run the interactive UI until the user exits (terminal) or the plan
/// completes (headless).
///
/// Every error return funnels through the one exit restore: an early `?`
/// between the surface mount and the deliberate tail teardown (a draw
/// failure, a key-handler transport error, a suspend/resume failure)
/// must not hand the shell a terminal still in TUI state — raw mode,
/// the alternate screen, the enhancement modes armed. The restore is
/// idempotent, so a return after the tail already ran (the startup
/// refusal path finishes the surface itself) only re-emits the two
/// unconditional tail bytes.
/// The open route for one interactive run (TS `runAgentsViewLoop`'s
/// open versus the CLI's own open): the agents-view open waits through a
/// daemon update restart (TS #2391) instead of failing the open hard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionOpenRoute {
    /// The CLI's open (`prime-agent`, `--resume`, `--attach`): a single attempt.
    Cli,
    /// The agents view's open: waits through the update-restart window and retries; `panel`
    /// mounts on open and its close returns to the agents view.
    AgentsView { panel: Option<SessionPanel> },
    /// The return from a session the run requested: the requesting session reopens with its
    /// origin panel mounted; waits through the update restart like the agents view's open.
    Origin(SelectionOrigin),
}

/// Run one interactive session (the CLI open route).
///
/// # Errors
///
/// Returns `Err` when the interactive surface fails; the restore runs first whenever this run owned
/// or adopted the terminal.
pub async fn run_interactive(
    options: InteractiveOptions,
    ui: UiMode,
) -> Result<InteractiveOutcome> {
    run_interactive_route(options, ui, SessionOpenRoute::Cli).await
}

/// Run one interactive session opened from the agents view (see
/// [`SessionOpenRoute::AgentsView`]).
///
/// # Errors
///
/// Returns `Err` when the interactive surface fails or the open waits out the update-restart
/// window; the restore runs first whenever this run owned or adopted the terminal.
pub async fn run_interactive_agents_view_open(
    options: InteractiveOptions,
    ui: UiMode,
    panel: Option<SessionPanel>,
) -> Result<InteractiveOutcome> {
    run_interactive_route(options, ui, SessionOpenRoute::AgentsView { panel }).await
}

/// Reopen the session that requested the session just left (see [`SessionOpenRoute::Origin`]).
///
/// # Errors
///
/// Returns `Err` when the interactive surface fails or the open waits out the update-restart
/// window; the restore runs first whenever this run owned or adopted the terminal.
pub async fn run_interactive_origin_open(
    options: InteractiveOptions,
    ui: UiMode,
    origin: SelectionOrigin,
) -> Result<InteractiveOutcome> {
    run_interactive_route(options, ui, SessionOpenRoute::Origin(origin)).await
}

async fn run_interactive_route(
    options: InteractiveOptions,
    ui: UiMode,
    route: SessionOpenRoute,
) -> Result<InteractiveOutcome> {
    // The headless harness never owned the terminal, so its error returns must not run a restore.
    let owns_terminal = matches!(ui, UiMode::Terminal);
    // A terminal-mode error that fired BEFORE this surface mounted must not tear down whatever the
    // CALLER had up.
    let surface_mounted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mounted = std::sync::Arc::clone(&surface_mounted);
    match run_interactive_surface(options, ui, route, mounted).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            // The adopting surface owns the release even when it fails before mounting; a
            // fresh-pane pre-mount failure has nothing to release.
            if owns_terminal
                && (surface_mounted.load(std::sync::atomic::Ordering::SeqCst)
                    || crate::altscreen::active())
            {
                crate::exit_restore::restore_terminal();
            }
            Err(error)
        }
    }
}

/// The senders the open session reports its background work on; the run loop holds one receiver
/// each ([`SessionReceivers`]). The run keeps one copy alive for its whole life, so a receiver
/// never sees its channel close while the session runs.
#[derive(Clone)]
struct SessionSenders {
    notes: mpsc::UnboundedSender<String>,
    compaction_abort: mpsc::UnboundedSender<crate::session_ui::CompactionAbortNote>,
    prompt: mpsc::UnboundedSender<crate::session_ui::PromptSubmitNote>,
    share: mpsc::UnboundedSender<crate::session_ui::ShareNote>,
    update: mpsc::UnboundedSender<crate::session_ui::UpdateNote>,
    reload: mpsc::UnboundedSender<crate::session_ui::ReloadNote>,
    traces_upload: mpsc::UnboundedSender<crate::session_ui::TracesUploadNote>,
    catalog: mpsc::UnboundedSender<crate::session_ui::ModelCatalogUpdate>,
    heartbeats: mpsc::UnboundedSender<crate::session_ui::HeartbeatsUpdate>,
    auth_panel: mpsc::UnboundedSender<crate::auth_panel::AuthPanelRequest>,
    bash: mpsc::UnboundedSender<crate::session_ui::BashActivityUpdate>,
    /// Background factory refreshes (the factory page's watch+graph cadence and the dock
    /// count's poll): the loop folds them into the session — the open page's panels and the
    /// dock's count.
    factory: mpsc::UnboundedSender<crate::session_ui::FactoryUpdate>,
    /// Background slash-command-catalog refreshes (`get_commands`): the loop folds the session's
    /// skill commands into the autocomplete provider.
    commands: mpsc::UnboundedSender<crate::session_ui::CommandCatalogUpdate>,
    page: mpsc::UnboundedSender<crate::session_ui::TranscriptPageNote>,
}

/// The run loop's ends of the [`SessionSenders`] channels.
struct SessionReceivers {
    notes: mpsc::UnboundedReceiver<String>,
    compaction_abort: mpsc::UnboundedReceiver<crate::session_ui::CompactionAbortNote>,
    prompt: mpsc::UnboundedReceiver<crate::session_ui::PromptSubmitNote>,
    share: mpsc::UnboundedReceiver<crate::session_ui::ShareNote>,
    update: mpsc::UnboundedReceiver<crate::session_ui::UpdateNote>,
    reload: mpsc::UnboundedReceiver<crate::session_ui::ReloadNote>,
    traces_upload: mpsc::UnboundedReceiver<crate::session_ui::TracesUploadNote>,
    catalog: mpsc::UnboundedReceiver<crate::session_ui::ModelCatalogUpdate>,
    heartbeats: mpsc::UnboundedReceiver<crate::session_ui::HeartbeatsUpdate>,
    auth_panel: mpsc::UnboundedReceiver<crate::auth_panel::AuthPanelRequest>,
    bash: mpsc::UnboundedReceiver<crate::session_ui::BashActivityUpdate>,
    factory: mpsc::UnboundedReceiver<crate::session_ui::FactoryUpdate>,
    commands: mpsc::UnboundedReceiver<crate::session_ui::CommandCatalogUpdate>,
    page: mpsc::UnboundedReceiver<crate::session_ui::TranscriptPageNote>,
}

fn session_channels() -> (SessionSenders, SessionReceivers) {
    let (notes, notes_rx) = mpsc::unbounded_channel();
    let (compaction_abort, compaction_abort_rx) = mpsc::unbounded_channel();
    let (prompt, prompt_rx) = mpsc::unbounded_channel();
    let (share, share_rx) = mpsc::unbounded_channel();
    let (update, update_rx) = mpsc::unbounded_channel();
    let (reload, reload_rx) = mpsc::unbounded_channel();
    let (traces_upload, traces_upload_rx) = mpsc::unbounded_channel();
    let (catalog, catalog_rx) = mpsc::unbounded_channel();
    let (heartbeats, heartbeats_rx) = mpsc::unbounded_channel();
    let (auth_panel, auth_panel_rx) = mpsc::unbounded_channel();
    let (bash, bash_rx) = mpsc::unbounded_channel();
    let (factory, factory_rx) = mpsc::unbounded_channel();
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (page, page_rx) = mpsc::unbounded_channel();
    (
        SessionSenders {
            notes,
            compaction_abort,
            prompt,
            share,
            update,
            reload,
            traces_upload,
            catalog,
            heartbeats,
            auth_panel,
            bash,
            factory,
            commands,
            page,
        },
        SessionReceivers {
            notes: notes_rx,
            compaction_abort: compaction_abort_rx,
            prompt: prompt_rx,
            share: share_rx,
            update: update_rx,
            reload: reload_rx,
            traces_upload: traces_upload_rx,
            catalog: catalog_rx,
            heartbeats: heartbeats_rx,
            auth_panel: auth_panel_rx,
            bash: bash_rx,
            factory: factory_rx,
            commands: commands_rx,
            page: page_rx,
        },
    )
}

/// The startup open (TS `runAgentsViewLoop` -> `openAgentsViewSession`, TS #2391): an
/// agents-view open that lands while the daemon prepares an update restart waits through the
/// restart window (bounded, 500ms retry cadence, the attached-session reconnect budget) and
/// retries against the successor instead of failing the open; the CLI route keeps the single
/// attempt. The first attempt reuses the pre-mount connection; a retry reconnects fresh. Only
/// the open waits — once the session is up, the reconnect loop owns the mid-chat restart window.
///
/// The open runs as a BACKGROUND task: the opening phase serves keystrokes while it completes.
/// TS `init()` runs `ui.start()` (the input surface is live) and awaits
/// `rebindCurrentSession()` on an already-serving event loop; the port's old shape ran the whole
/// open pipeline before the run loop took its first input, so time-to-type waited on the create,
/// the attach, the dock folds, and every daemon readiness behind them (the operator's
/// input-latency report: first frame 40-52ms, keystroke echo ~250ms later warm, ~0.6-1.6s cold).
fn spawn_session_open(
    first_connection: (
        DaemonClient,
        mpsc::UnboundedReceiver<crate::daemon_client::DaemonClientEvent>,
    ),
    options: &InteractiveOptions,
    senders: &SessionSenders,
    route: &SessionOpenRoute,
) -> tokio::task::JoinHandle<Result<OpenedSession>> {
    let mut first_connection = Some(first_connection);
    let options = options.clone();
    let senders = senders.clone();
    let waits_through_update_restart = !matches!(route, SessionOpenRoute::Cli);
    tokio::spawn(async move {
        crate::update_restart_wait::wait_through_update_restart(
            waits_through_update_restart,
            crate::update_restart_wait::DAEMON_UPDATE_RESTART_OPEN_WAIT_MS,
            crate::update_restart_wait::DAEMON_UPDATE_RESTART_OPEN_RETRY_MS,
            || {
                // The attempt future owns everything it touches (an `async move` over clones
                // taken here): an `FnMut` closure's captures may not escape into the returned
                // future, so the synchronous body moves the pieces out instead.
                let first = first_connection.take();
                let options = options.clone();
                let senders = senders.clone();
                async move {
                    let (client, events) = match first {
                        Some(first) => first,
                        None => DaemonClient::connect(&options.socket_path)
                            .await
                            .with_context(|| "the interactive UI could not attach to the daemon")?,
                    };
                    let session = SessionUi::open(
                        client,
                        &options,
                        senders.notes,
                        senders.compaction_abort,
                        senders.prompt,
                        senders.share,
                        senders.reload,
                        senders.update,
                        senders.traces_upload,
                        senders.catalog,
                        senders.auth_panel,
                        crate::session_ui::ActivityUpdates {
                            heartbeats: senders.heartbeats,
                            bash: senders.bash,
                            factory: senders.factory,
                            commands: senders.commands,
                        },
                        senders.page,
                    )
                    .await?;
                    Ok((events, session))
                }
            },
        )
        .await
    })
}

/// The view and the terminal surface come up BEFORE the session attach: the pane paints the
/// startup chrome immediately, and the transcript renders when the snapshot lands.
fn startup_view(options: &InteractiveOptions) -> AgentView {
    let theme = crate::app::load_theme(&options.theme);
    let mut view = AgentView::new(theme);
    view.code_block_indent
        .clone_from(&options.code_block_indent);
    view.mermaid_mode = options
        .client_settings
        .as_ref()
        .map_or_else(crate::markdown::MermaidMode::default, |settings| {
            crate::markdown::MermaidMode::from_setting(&settings.mermaid_rendering_mode())
        });
    // The file-completion provider browses the SESSION cwd, not the process cwd: the completion
    // menu must browse the directory the user sees.
    let provider =
        crate::autocomplete::CombinedAutocompleteProvider::from_registry(options.cwd.clone());
    provider.start_file_index();
    view.editor.set_autocomplete_provider(Box::new(provider));
    view.editor.set_keybindings(options.keybindings.clone());
    view.show_images = options.show_images;
    if let Some(settings) = &options.client_settings {
        view.show_hardware_cursor = settings.show_hardware_cursor();
        view.editor
            .set_vim_enabled(settings.editor_mode() == crate::client_settings::EditorMode::Vim);
        // A chat opens at the persisted `chatDetail` level; an unset store reads as the collapsed
        // `overview` startup level (operator directive 2026-09-28).
        view.detail = crate::chat::Detail::from_wire_name(&settings.chat_detail());
    }
    apply_startup_chrome(&mut view, options);
    view
}

/// The status line (`statusLine`): its command runs off the paint path, on the loop's own timer
/// arm, and reports back over its channel.
fn startup_status_line(
    options: &InteractiveOptions,
    view: &mut AgentView,
) -> (
    crate::status_line::StatusLine,
    mpsc::UnboundedReceiver<crate::status_line::StatusLineRun>,
) {
    let status_line_settings = options
        .client_settings
        .as_ref()
        .and_then(|settings| settings.status_line());
    if let Some(settings) = &status_line_settings {
        view.chrome.status_line_padding = settings.padding;
        view.chrome.vim_indicator_hidden = settings.hide_vim_mode_indicator;
    }
    crate::status_line::StatusLine::new(
        status_line_settings,
        options.cwd.clone(),
        crate::status_line::RunFacts {
            added_dirs: options
                .client_settings
                .as_ref()
                .map(|settings| settings.launch_additional_directories())
                .unwrap_or_default(),
            output_style: options
                .client_settings
                .as_ref()
                .map_or_else(|| "default".to_string(), |settings| settings.output_style()),
            started: tokio::time::Instant::now(),
        },
    )
}

/// One `⚠` warning row on the opened session's chat.
fn push_startup_warning(notice: &str, session: &mut SessionUi, view: &mut AgentView) {
    view.push_entry(crate::chat::ChatEntry::Status {
        text: format!("\u{26a0} {notice}"),
        kind: crate::chat::StatusKind::Warning,
    });
    session.dirty = true;
}

/// The post-open fold: the session's first view, the prompt history, the layout handoff's adopt,
/// the startup warnings, the restored stash, and the panel the open route mounts.
async fn fold_opened_session(
    session: &mut SessionUi,
    view: &mut AgentView,
    route: SessionOpenRoute,
    waited_for_update_restart: bool,
    headless: bool,
) {
    if headless {
        session.osc_sink = crate::clipboard::OscSink::Buffer(Vec::new());
    }
    // The tray's context usage came in with the attach snapshot, so the
    // open path does not block on a stats fetch.
    session.spawn_model_catalog_refresh();
    session.rebuild_view(view, &crate::session_ui::RebuildKind::Rebind);
    // Seed the editor's up-arrow ring from the project's persisted prompt
    // history, so a freshly opened or resumed session recalls prompts from
    // every earlier session here. Interactive only; scripted/headless runs stay
    // deterministic. Best effort: a miss leaves recall empty.
    if !headless {
        session.seed_prompt_history(view).await;
    }
    // The cross-view layout handoff's adopt (view::handoff): a re-entry whose attach cursor exactly
    // matches the previous run's held handoff holds its visible-window packs for the first draw;
    // any chat mutation retires it. A cursor-less re-attach never adopts.
    if session.attach_cursor_present {
        view.adopt_layout_handoff(
            &session.session_id,
            &session.attach_event_generation,
            session.attach_event_sequence,
        );
    }
    if let Some(notice) = check_tmux_keyboard_setup().await {
        push_startup_warning(&notice, session, view);
    }
    // An open that waited through the update restart says so in the session's first status row.
    if waited_for_update_restart {
        push_startup_warning(
            crate::update_restart_wait::DAEMON_UPDATE_RESTART_WAIT_NOTICE,
            session,
            view,
        );
    }
    // The ban-risk warning on an Anthropic subscription credential at startup.
    if let Some(provider) = session.current_model_provider().await {
        session
            .maybe_warn_anthropic_subscription_auth_if_subscribed(Some(provider.as_str()), view)
            .await;
    }
    // The once-per-installation telemetry disclosure (TS
    // agent-session-services): rendered as an info row here — the alt
    // screen hides any pre-TUI stderr print, so the row is the only
    // shape the user actually sees.
    session.maybe_show_telemetry_notice(view);
    // TS `restorePromptStashOnOpen`: a draft stashed on the way out (a
    // previous chat view of this session left via the agents view or a
    // switch) returns to the editor when its chat reopens.
    session.restore_prompt_stash_on_open(view);
    match route {
        SessionOpenRoute::AgentsView { panel: Some(panel) } => {
            session.open_agents_view_panel(panel, true, view);
        }
        SessionOpenRoute::Origin(origin) => {
            session.open_agents_view_panel(origin.panel, origin.closes_to_agents_view, view);
        }
        SessionOpenRoute::AgentsView { panel: None } | SessionOpenRoute::Cli => {}
    }
}

async fn run_interactive_surface(
    options: InteractiveOptions,
    ui: UiMode,
    route: SessionOpenRoute,
    surface_mounted: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<InteractiveOutcome> {
    // The TS theme emits raw ANSI color codes regardless of NO_COLOR; match that.
    crossterm::style::force_color_output(true);
    // The first open attempt uses this connection (a fresh-pane connect failure must stay a
    // pre-mount failure); a wait retry reconnects against the successor daemon.
    let first_connection = DaemonClient::connect_with_retry(&options.socket_path)
        .await
        .with_context(|| "the interactive UI could not attach to the daemon")?;
    let (senders, mut receivers) = session_channels();
    // The double-Ctrl+C force-quit guard: the terminal reader observes the pair while this loop is
    // wedged in a daemon request; a std-thread watchdog enforces the exit deadline without the
    // runtime.
    let exit_guard = ExitGuard::new();
    let mut view = startup_view(&options);
    let (mut status_line, mut status_line_rx) = startup_status_line(&options, &mut view);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiInput>();
    // Headless verification runs capture the OSC 52 clipboard channel instead of the plain pipes.
    let headless = matches!(ui, UiMode::Headless(_));
    // A panic between the mount below and the deliberate teardown must still hand the terminal
    // back whole: the unwind guard fires the one exit restore (a set_hook cannot — tokio catches
    // task panics).
    let _surface_restore = crate::exit_restore::SurfaceRestore::armed();
    let mut renderer = Renderer::setup(
        ui,
        ui_tx,
        exit_guard.clone(),
        options.fullscreen_mouse,
        &surface_mounted,
    )?;
    let mut opening = OpeningPhase::new(&options, headless);
    if opening.paints
        && let Some(terminal) = renderer.is_terminal_mut()
    {
        crate::app::draw(terminal, &mut view)?;
    }
    let mut open_task = spawn_session_open(first_connection, &options, &senders, &route);
    let opened = match opening
        .run(&mut open_task, &mut ui_rx, &mut view, &mut renderer)
        .await?
    {
        OpeningEnd::Opened(opened) => opened,
        OpeningEnd::Exited => {
            exit_guard.arm_for_exit();
            renderer.finish(&mut view, false);
            return Ok(InteractiveOutcome {
                frames: Vec::new(),
                ..Default::default()
            });
        }
    };
    let ((mut events, mut session), waited_for_update_restart) = match *opened {
        Ok(opened) => opened,
        Err(error) => return open_failure(error, &options, &exit_guard, renderer, &mut view),
    };
    session.exit_guard = exit_guard.clone();
    // The supervisor reader's death watch: the event channel stays open across a supervisor socket
    // loss, so this watch is the signal the reconnect driver arms on. Taken from the LIVE session
    // client after the open (a wait retry may have reconnected).
    let mut reader_dead = session.client.reader_dead();
    fold_opened_session(
        &mut session,
        &mut view,
        route,
        waited_for_update_restart,
        headless,
    )
    .await;
    // Declared above the onboarding phase: the pane's drive marks it.
    let mut headless_done = false;
    // First-run onboarding owns the pane before the session screen: a ready home sees the trace
    // question alone, a not-ready home runs the full sign-in flow, and the marker gate keeps it
    // one-shot.
    if let Some(task) = options.onboarding.clone() {
        let mut drive = PaneDrive {
            ui_rx: &mut ui_rx,
            renderer: &mut renderer,
            exit_guard: &exit_guard,
            keybindings: view.editor.keybindings().clone(),
            auth_panel_rx: &mut receivers.auth_panel,
            headless_done: &mut headless_done,
        };
        let exit_requested =
            run_onboarding_phase(&task, &mut session, &mut view, &mut drive).await?;
        if exit_requested {
            // The exit deadline is armed from the moment the run decides to leave: no cleanup below
            // may block past it.
            exit_guard.arm_for_exit();
            // Promote any Ctrl+C-cleared draft into persistent history
            // before the detach wires the client off, so a cleared
            // draft survives the exit (CC's `holdCleared` -> history
            // drain path).
            session.flush_cleared_draft_on_exit();
            session.detach_for_exit().await;
            // The user quit at the onboarding screen: still hand the terminal back.
            renderer.finish(&mut view, false);
            return Ok(InteractiveOutcome {
                active_session_id: session.active_session_id.clone(),
                session_id: session.session_id.clone(),
                transcript_entry_work: view.entry_work.get(),
                ..Default::default()
            });
        }
    }
    // `--plan` goes through the session command, so the change is durable
    // and the kernel guard arms before the first prompt's turn.
    if options.initial_plan_mode {
        session.track_client_adoption(crate::interactive::ClientAdoption::PlanFlag);
        session
            .submit_prompt(
                "/plan on",
                crate::session_ui::SubmitBehavior::Steer,
                &mut view,
            )
            .await?;
    }
    if let Some(initial) = &options.initial_message {
        session
            .submit_prompt(initial, crate::session_ui::SubmitBehavior::Steer, &mut view)
            .await?;
    }

    // The fold's first frame must land before the opening phase's
    // typed-ahead queue dispatches: base painted it between the fold and
    // the first input (the select round-trip owns one frame per
    // iteration), and a pre-loaded queue would otherwise starve it — the
    // queued inputs would paint only their own deltas over a state the
    // verifier never saw rendered.
    if !opening.pending.is_empty() && session.dirty {
        if let Some(terminal) = renderer.is_terminal_mut() {
            crate::app::draw(terminal, &mut view)?;
        } else {
            renderer.render_headless(&mut session, &mut view);
        }
        session.dirty = false;
    }

    // The opening phase's typed-ahead inputs, queued behind the open, dispatch first, in order.
    let mut run_loop = RunLoop::new(
        renderer.is_terminal(),
        std::mem::take(&mut opening.pending),
        opening.input_closed,
        headless_done,
    );
    let mut recovery = Recovery::default();

    'run: while run_loop.running {
        if !session.dirty {
            session.spawn_transcript_page(&mut view);
        }
        run_loop.report_capabilities(&session).await;
        if run_loop
            .drain_inputs(&mut session, &mut view, &mut renderer)
            .await?
            == BatchEnd::LeaveRun
        {
            break 'run;
        }
        if run_loop.headless_settled(&session, &view)? {
            break;
        }
        run_loop.arm_wakes(&session, &view, &renderer);
        if status_line.enabled() {
            status_line.observe_facts(
                &session.status_line_facts(&view),
                tokio::time::Instant::now(),
            );
        }
        let status_line_deadline = status_line.deadline();
        let was_active = session.turn_active;
        // The select's deadlines, snapshotted so the arm futures read copies instead of borrowing
        // the loop state. The 2s bash-activity and factory polls run on their own absolute
        // deadlines and only on daemons that advertise the kernel-bash registry or the factory
        // lane (without the capability every fire was a no-op, so the arm parks). The factory
        // poll runs whether or not the page is open (the dock's factory count stays live); an
        // open page adds its selected run's watch ahead of the graph inside the same serialized
        // cycle.
        let bash_poll = session
            .kernel_bash_supported()
            .then(|| run_loop.last_bash_refresh + ACTIVITY_POLL_INTERVAL);
        let factory_poll = session
            .factory_activity_supported()
            .then(|| run_loop.last_factory_refresh + ACTIVITY_POLL_INTERVAL);
        let quiet_tick = run_loop.quiet_tick_deadline;
        let frame_deadline = run_loop.render_deadline;
        let parks_events = recovery.parks_events();
        let reconnect_tick = recovery.tick_deadline();
        let session_reconnect_tick = recovery.session_tick_deadline();
        let reader_loss_handled = recovery.reader_loss_handled;
        let attempt_rx = &mut recovery.connect;
        tokio::select! {
            maybe_event = async {
                if parks_events {
                    std::future::pending::<()>().await;
                }
                events.recv().await
            } => {
                if let Some(event) = maybe_event {
                    fold_session_events(event, &mut events, was_active, &mut session, &mut view)
                        .await;
                    recovery.after_events(&mut session, &mut view);
                } else {
                    recovery.on_events_closed(&mut session, &mut view);
                }
            }
            reader_death = async {
                // One-shot: a ready arm would hot-spin the select.
                if reader_loss_handled {
                    std::future::pending::<()>().await;
                }
                reader_dead.changed().await
            } => {
                recovery.reader_loss_handled = true;
                if reader_death.is_ok() && *reader_dead.borrow_and_update() {
                    // An update restart's close frame can race this signal (the reader emits the
                    // frame, then dies): drain every frame the reader already delivered before
                    // deciding, so the update's own reconnect driver owns the recovery.
                    while let Ok(event) = events.try_recv() {
                        session.apply_client_event(event, &mut view);
                    }
                    recovery.on_reader_death(&mut session, &mut view);
                }
            }
            input = async {
                // The headless driver drops its sender after queueing the plan: a closed recv is
                // always ready and would starve turn events while a barrier or the final
                // submitted prompt is still settling, so the arm parks once it saw the close.
                if run_loop.headless_done {
                    std::future::pending::<Option<UiInput>>().await
                } else {
                    next_ui_input(&mut ui_rx, &mut run_loop.ui_closed).await
                }
            } => {
                // `None` is the close, reported once: this iteration ends so the next
                // one sees `ui_closed` and wakes for the queued steps.
                if let Some(input) = input {
                    run_loop.queue_input(input);
                }
            }
            Some(note) = receivers.notes.recv() => session.apply_background_note(&note, &mut view),
            () = async {
                match status_line_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                status_line.fire(tokio::time::Instant::now());
            }
            maybe_status = status_line_rx.recv() => {
                if let Some(rows) = maybe_status.and_then(|run| status_line.apply(run))
                    && view.chrome.status_line != rows {
                        view.chrome.status_line = rows;
                        session.dirty = true;
                    }
            }
            Some(note) = receivers.page.recv() => session.apply_transcript_page(note, &mut view),
            Some(outcome) = receivers.compaction_abort.recv() => {
                session.apply_compaction_abort_outcome(outcome, &mut view);
            }
            Some(outcome) = receivers.share.recv() => session.apply_share_outcome(outcome, &mut view),
            Some(outcome) = receivers.update.recv() => session.apply_update_note(outcome, &mut view),
            Some(outcome) = receivers.reload.recv() => {
                session.apply_reload_outcome(outcome, &mut view).await;
            }
            Some(note) = receivers.traces_upload.recv() => {
                session.apply_traces_upload_note(note, &mut view);
            }
            Some(update) = receivers.catalog.recv() => session.apply_model_catalog(update, &mut view),
            Some(request) = receivers.auth_panel.recv() => {
                session.apply_auth_panel_request(request, &mut view).await;
            }
            Some(update) = receivers.heartbeats.recv() => {
                session.apply_heartbeat_update(update, &mut view);
            }
            Some(update) = receivers.bash.recv() => session.apply_bash_activity(update, &mut view),
            Some(update) = receivers.factory.recv() => session.apply_factory_update(update, &mut view),
            Some(update) = receivers.commands.recv() => {
                session.apply_command_catalog(update, &mut view);
            }
            // Protocol corruption stays fatal exactly like the inline submit's ladder.
            Some(note) = receivers.prompt.recv() => session.apply_prompt_outcome(note, &mut view).await?,
            () = sleep_until_armed(reconnect_tick) => {
                let step = recovery.on_tick(&mut session, &mut view, &options.socket_path);
                if run_loop.after_recovery(step) {
                    continue;
                }
            }
            maybe_attempt = async {
                match attempt_rx.as_mut() {
                    Some(receiver) => receiver.await,
                    // Nothing in flight: park the arm — the type is inferred from the in-flight
                    // arm, and the tick is the only spawner (a plain `None` return would hot-spin).
                    None => std::future::pending().await,
                }
            } => {
                let step = recovery
                    .on_attempt(maybe_attempt, &mut session, &mut view, &mut events, &mut reader_dead)
                    .await;
                if run_loop.after_recovery(step) {
                    continue;
                }
            }
            () = sleep_until_armed(session_reconnect_tick) => {
                let step = recovery.on_session_tick(&mut session, &mut view).await;
                if run_loop.after_recovery(step) {
                    continue;
                }
            }
            // The quiet tick runs only while work is actually pending: a parked editor
            // autocomplete request (suggestions resolve asynchronously after the keystroke
            // batch, so the dropdown opens only once typing pauses) or an armed selection
            // auto-scroll. An idle surface parks this arm.
            () = sleep_until_armed(quiet_tick) => {
                run_loop.quiet_tick_deadline = None;
                session.materialize_editor_autocomplete(&mut view);
                session.selection_auto_scroll_tick(&mut view);
            }
            () = sleep_until_armed(bash_poll) => {
                run_loop.last_bash_refresh = std::time::Instant::now();
                session.spawn_bash_activity_refresh();
                session.refresh_expanded_dock(&mut view);
            }
            () = sleep_until_armed(factory_poll) => {
                run_loop.last_factory_refresh = std::time::Instant::now();
                session.spawn_factory_refresh();
            }
            () = crate::inline_image::payload_ready() => {
                // A preview's off-paint transcode settled: the next frame places it (or
                // re-lays the row out as its fallback).
                session.dirty = true;
            }
            // The coalesced frame's deadline arrived: the frame gate below paints the
            // accumulated state now.
            () = sleep_until_armed(frame_deadline) => {}
        }
        run_loop.animate_and_paint(&mut session, &mut view, &mut renderer)?;
    }

    finish_run(
        &mut session,
        &mut view,
        renderer,
        &exit_guard,
        &mut receivers.page,
        waited_for_update_restart,
    )
    .await
}

/// The run's teardown after the loop decided to leave: the transcript window collapses, the
/// draft stashes on a handoff, the terminal is restored, the resume hint and the detach run
/// bounded, and the outcome returns.
async fn finish_run(
    session: &mut SessionUi,
    view: &mut AgentView,
    renderer: Renderer,
    exit_guard: &ExitGuard,
    page_rx: &mut mpsc::UnboundedReceiver<crate::session_ui::TranscriptPageNote>,
    waited_for_update_restart: bool,
) -> Result<InteractiveOutcome> {
    // The run decided to leave: arm the force-quit deadline so every cleanup step below is
    // best-effort — a wedged shutdown cannot hold the process open past it. A handoff (agents-back,
    // a `/resume` selection) is a view switch, not an exit: arming here turned a busy box's slow
    // switch into a mid-teardown process kill ("shutdown stalled; forced exit.", the live report),
    // so the deadline covers only the leaves that end this process.
    let handing_off = session.open_agents_view || session.pending_selection.is_some();
    if renderer.is_terminal() && !session.open_agents_view {
        // Leaving the run flushes only the history already loaded and contiguous with
        // the tail: an unfinished page is abandoned (dropping the task aborts its
        // request), a page that already arrived applies, and a resident run cut off
        // from the tail drops before the flush.
        session.window.retire_fetch();
        while let Ok(note) = page_rx.try_recv() {
            session.apply_transcript_page(note, view);
        }
        session.collapse_transcript_window(view);
    }
    if renderer.is_terminal() && !handing_off {
        exit_guard.arm_for_exit();
    }
    // A handoff stashes the live draft for the session being left; every exit releases the
    // run's binding (a held draft stays in the store for the next view).
    if session.open_agents_view || session.pending_selection.is_some() {
        session.stash_draft_for_agents_view(view);
        // The cross-view layout handoff (view::handoff): hold the last frame's visible-window packs
        // keyed by the LATEST event sequence this run has seen, so the unchanged-session re-entry's
        // first draw reuses them. A cursor-less attach never keys (the collapsed default identity
        // could alias).
        if session.attach_cursor_present {
            view.stash_layout_handoff(
                &session.session_id,
                &session.attach_event_generation,
                session.last_event_sequence,
            );
        }
    }
    session.release_prompt_stash_session();
    // Agents-back and `/resume` hand the pane to the agents view; the alternate screen stays in
    // place for it (TS `stop({ preserveAltScreen: true })`).
    let preserve_alt_screen = session.open_agents_view;
    // The terminal is restored (the alternate screen left, the transcript flushed to the main
    // screen) BEFORE the daemon round-trips below: right after a double Ctrl+C the worker is
    // still settling the interrupted turn, the round-trips ran out their bounds, and the
    // force-quit deadline fired first, leaving the alternate screen with no flush, so the view
    // jumped to the launch-time screen. With the terminal restored, the remaining steps are
    // bookkeeping: the deadline restarts and each bounded step proves progress.
    // Only a terminal run arms the watchdog (headless verification keeps deterministic
    // teardown): a headless exit forced mid-test would end the whole test process with
    // status 0, so the tests after it would never run and still read as passed.
    let owns_terminal = renderer.is_terminal();
    let frames = renderer.finish(view, preserve_alt_screen);
    if owns_terminal && !handing_off {
        exit_guard.arm_for_exit();
        crate::exit_guard::note_exit_progress();
    }
    // Fetch the session stats while the connection is alive, then print the resume hint after
    // teardown; the agents-view handoff never prints it, so the round-trip is dead work on that
    // path.
    let resume_hint = if session.open_agents_view {
        None
    } else {
        let hint = session.exit_resume_hint().await;
        // What the daemon keeps doing goes above the resume line (the client never owns it).
        let running = crate::session_ui::still_running_note(
            session.turn_active,
            crate::queued::flatten(&view.queued).len(),
        );
        match (running, hint) {
            (Some(running), Some(hint)) => Some(format!("{running}\n{hint}")),
            (running, hint) => running.or(hint),
        }
    };
    crate::exit_guard::note_exit_progress();
    // Detach explicitly so the session's attached-client count stays honest; bounded hard: a
    // wedged worker socket can never hold the exit path. The agents-view handoff fires the detach
    // in the background instead (bookkeeping must not delay the switch).
    // Promote any Ctrl+C-cleared draft into persistent history before the
    // detach/handoff, so a cleared draft survives the exit (CC's
    // `holdCleared` -> history drain path).
    session.flush_cleared_draft_on_exit();
    if session.open_agents_view {
        session.detach_for_handoff();
    } else {
        session.detach_for_exit().await;
    }
    crate::exit_guard::note_exit_progress();
    // `tui exit` (schema v1): how the run ended. Bounded the same way as the detach — telemetry
    // must never hold the exit path open. The handoff still emits the event but does not wait for
    // the flush.
    let exit_reason = session.exit_reason();
    let turn_active_at_exit = session.turn_active;
    if let Some(telemetry) = session.telemetry.clone() {
        let exit_event = async move {
            let () = telemetry
                .client_exit(exit_reason, turn_active_at_exit)
                .await;
        };
        if session.open_agents_view {
            tokio::spawn(exit_event);
        } else {
            let _ =
                tokio::time::timeout(Duration::from_millis(TELEMETRY_EXIT_TIMEOUT_MS), exit_event)
                    .await;
        }
    }
    let outcome = InteractiveOutcome {
        active_session_id: session.active_session_id.clone(),
        session_id: session.session_id.clone(),
        resume_hint,
        last_assistant_text: session.last_assistant_text.clone(),
        frames,
        // The headless OSC 52 capture (terminal runs wrote the sequences to stdout as they
        // happened).
        clipboard_emissions: session.take_osc_emissions(),
        return_to_agents_view: preserve_alt_screen,
        agents_view_scope: session.scoped_agents_view.take(),
        selection_request: session.pending_selection.take(),
        selection_needs_lookup: session.pending_selection_lookup,
        selection_rlm_depth: session.pending_selection_rlm_depth,
        selection_origin: session.pending_selection_origin.take(),
        copies: std::mem::take(&mut session.copies),
        opened_urls: std::mem::take(&mut session.opened_urls),
        // The view's status line keeps the wait notice when the chat hands back.
        agents_view_notice: if waited_for_update_restart && preserve_alt_screen {
            Some(crate::update_restart_wait::DAEMON_UPDATE_RESTART_WAIT_NOTICE.to_string())
        } else {
            None
        },
        agents_view_query: None,
        agents_view_resume_picker: session.open_agents_view && session.agents_view_resume_picker,
        handoff_seeds: view.handoff_seeds,
        transcript_entry_work: view.entry_work.get(),
        turn_active_at_exit,
    };
    // The agents-view handoff's background detach owns this connection now; every other exit
    // closes it here.
    if !preserve_alt_screen {
        session.client.close();
    }
    // A handoff lets the process keep running: retire the watchdog. Every other completion is a
    // process exit, where the deadline dies with the process — or fires when the exit wedged,
    // which is the point.
    if outcome.return_to_agents_view || outcome.selection_request.is_some() {
        exit_guard.cancel();
    }
    Ok(outcome)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A closed UI channel parks its select arm: the headless driver drops
    /// its sender right after queueing the plan, while a `WaitIdle` barrier
    /// still holds `HeadlessDone` in the pending queue. A `recv()` that
    /// resolves on every poll hot-spins the loop, and once every iteration
    /// paints a frame the loop stops yielding to the session reader task
    /// (the stream stops applying and the barrier expires).
    #[tokio::test(start_paused = true)]
    async fn a_closed_ui_channel_parks_its_select_arm() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiInput>();
        ui_tx.send(UiInput::HeadlessDone).expect("send");
        drop(ui_tx);
        let mut closed = false;
        let first = next_ui_input(&mut ui_rx, &mut closed).await;
        assert!(
            matches!(first, Some(UiInput::HeadlessDone)),
            "the queued input arrives first"
        );
        assert!(
            next_ui_input(&mut ui_rx, &mut closed).await.is_none(),
            "the close resolves once, so the loop iterates and sees it"
        );
        let parked = tokio::time::timeout(
            Duration::from_secs(60),
            next_ui_input(&mut ui_rx, &mut closed),
        )
        .await;
        assert!(parked.is_err(), "a closed channel must park, not resolve");
        assert!(closed, "the arm remembers the closed channel");
    }
}
