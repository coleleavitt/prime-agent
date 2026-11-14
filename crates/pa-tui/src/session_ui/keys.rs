//! The terminal input grammar: key dispatch, mouse reports, paste,
//! selection/auto-scroll, and the input-state seams.
use super::{
    AgentView,
    ChoicePickerAction,
    ChoicePurpose,
    Duration,
    Instant,
    KeyEvent,
    Result,
    SessionUi,
    key_event_to_id,
};

/// How long the Ctrl+C exit hint arms the second-press exit (Claude Code's
/// double-press window: short enough that two presses meant to stop a turn
/// and then clear the editor do not read as an exit).
const CTRL_C_EXIT_HINT_MS: u64 = 800;
/// How long a drag must hold the window edge before auto-scroll starts.
const SELECTION_AUTO_SCROLL_DELAY: Duration = Duration::from_millis(150);

const ESCAPE_REPEAT_WINDOW_MS: std::time::Duration = std::time::Duration::from_millis(500);

/// One armed auto-scroll: the drag's last position and when the scroll window opened.
#[derive(Debug, Clone)]
pub(super) struct SelectionAutoScroll {
    direction: isize,
    row: usize,
    col: usize,
    started: Instant,
}

impl SessionUi {
    /// The OSC 52 sequences the headless run captured (TS writes them to
    /// stdout; headless verification reads them here).
    pub(crate) fn take_osc_emissions(&mut self) -> Vec<String> {
        match std::mem::replace(&mut self.osc_sink, crate::clipboard::OscSink::Stdout) {
            crate::clipboard::OscSink::Buffer(buffer) => {
                vec![String::from_utf8_lossy(&buffer).into_owned()]
            }
            crate::clipboard::OscSink::Stdout => Vec::new(),
        }
    }

    /// The armed double-Escape action, taken once inside the window.
    pub(super) fn take_escape_repeat_action(&mut self) -> Option<&'static str> {
        let action = self.escape_repeat_action;
        if let Some(until) = self.escape_repeat_until
            && Instant::now() < until
        {
            self.escape_repeat_action = None;
            self.escape_repeat_until = None;
            return action;
        }
        self.escape_repeat_action = None;
        self.escape_repeat_until = None;
        None
    }

    /// Arm the double-Escape action for the repeat window.
    pub(super) fn arm_escape_repeat(&mut self, action: &'static str) {
        self.escape_repeat_action = Some(action);
        self.escape_repeat_until = Some(Instant::now() + ESCAPE_REPEAT_WINDOW_MS);
    }

    /// The Ctrl+C exit hint is armed: a second press inside the window terminates the client.
    pub(super) fn ctrl_c_hint_visible(&self) -> bool {
        self.ctrl_c_hint_until
            .is_some_and(|until| Instant::now() < until)
    }

    pub(super) fn show_ctrl_c_hint(&mut self, action: &'static str) {
        self.ctrl_c_hint_until = Some(Instant::now() + Duration::from_millis(CTRL_C_EXIT_HINT_MS));
        self.ctrl_c_hint_action = action;
    }

    /// The exit-hint key's action (`app.clear` for Ctrl+C, `app.exit` for
    /// Ctrl+D): the tray names the key that armed it.
    pub(crate) fn ctrl_c_hint_action(&self) -> &'static str {
        self.ctrl_c_hint_action
    }

    /// Whether a Ctrl+C now has work to interrupt: a turn, a provider-failover
    /// retry wait, a compaction, a user bash command, or a side question. Such
    /// a press only interrupts; it never arms the exit. A retry wait counts as
    /// interruptible work even though `turn_active` is false between the failed
    /// attempt's `TurnEnded` and the retry's next `TurnStarted`.
    pub(super) fn has_interruptible_work(&self, view: &AgentView) -> bool {
        self.turn_active
            || view.retry.is_some()
            || view.compaction.is_some()
            || self.user_bash_running
            || self.active_side_question_id.is_some()
    }

    /// Up on an empty editor restores the draft the last Ctrl+C cleared.
    /// Text AND the collapsed-paste map come back together (CC's
    /// `holdCleared` tuple carries `pastedContents` for exactly this
    /// reason: a draft like `look at this [paste #3 example.pdf]` is
    /// dead text without its paste map).
    pub(super) fn restore_cleared_draft(&mut self, view: &mut AgentView) -> bool {
        match self.cleared_draft.take() {
            Some(snap) if view.editor.get_text().is_empty() => {
                view.editor.set_text(&snap.text);
                view.editor.restore_paste_snapshot(snap.paste_snapshot);
                self.dirty = true;
                true
            }
            Some(snap) => {
                self.cleared_draft = Some(snap);
                false
            }
            None => false,
        }
    }

    /// Disarm the hint: escape, editing text, or shutdown.
    pub(super) fn clear_ctrl_c_hint(&mut self) {
        self.ctrl_c_hint_until = None;
    }

    /// The armed hint's expiry: the render loop arms its deadline there so the expired hint
    /// repaints away, not when the next unrelated event arrives.
    pub(crate) fn ctrl_c_hint_expiry(&self) -> Option<std::time::Instant> {
        self.ctrl_c_hint_until
            .filter(|until| std::time::Instant::now() < *until)
    }

    /// While mouse reporting is active the terminal gates native link handling, so clicks the TUI
    /// consumes open their OSC 8 targets themselves; reports are consumed even while a picker,
    /// selector, or loader owns the frame: the wheel never scrolls behind one, but its rows select.
    pub(crate) fn handle_mouse(&mut self, event: crate::mouse::MouseEvent, view: &mut AgentView) {
        if !crate::mouse_tracking::active() {
            return;
        }
        let overlay_focused = view.model_picker.is_some()
            || view.choice_picker.is_some()
            || matches!(
                view.harness_selector,
                Some(crate::view::HarnessSelectorState::Open(_))
            )
            || view.heartbeats_picker.is_some()
            || view.goal_panel.is_some()
            || view.bash_view.is_some()
            || view.info_panel.is_some()
            || view.tree_selector.is_some()
            || view.fork_selector.is_some()
            || view.rewind_selector.is_some()
            || view.share_loader.is_some()
            || view.mcp_view.is_some()
            || view.factory_view.is_some()
            || view.onboarding.is_some();
        let left = event.button == crate::mouse::BUTTON_LEFT;
        let release_was_drag = left && !event.press && self.left_mouse_dragged;
        let row = event.y.saturating_sub(1) as usize;
        let col = event.x.saturating_sub(1) as usize;
        if left && event.press {
            self.left_mouse_dragged = event.motion;
            if !event.motion {
                self.pressed_hyperlink = view.hyperlink_at(row, col);
                self.escape_tree_shortcut_spent = false;
            }
        }
        if let Some(delta) = crate::mouse::wheel_scroll_delta(&event) {
            if !overlay_focused {
                let delta = self.wheel_acceleration.step(delta.signum(), Instant::now());
                self.window.scroll(delta, view);
                self.dirty = true;
            }
            return;
        }
        // The hover report (operator directive 2026-09-26: `?1003` any-event
        // tracking delivers it): re-render only when the hovered row changed.
        if event.button == crate::mouse::BUTTON_NONE && event.motion {
            if view.note_hover(row, col) {
                self.dirty = true;
            }
            return;
        }
        let left_press = event.press && left;
        let mut open_pressed_link = false;
        if overlay_focused {
            // The frame surface first (its rows are the selectable spans), then the window.
            self.stop_selection_auto_scroll();
            if left_press && !event.motion {
                self.record_pressed_click(view, &event);
                if !view.begin_frame_selection(row, col) {
                    view.begin_selection(row, col);
                }
                self.snap_multi_click(view, &event, row, col);
                self.dirty = true;
            } else if left_press && event.motion {
                self.left_mouse_dragged = true;
                self.click_counter.reset();
                view.extend_active_selection(row, col);
                self.dirty = true;
            } else if !event.press && view.has_selection() {
                let text = view.end_active_selection();
                if let Some(text) = text {
                    self.copy_selection(&text, view);
                }
                self.dirty = true;
            } else if !event.press {
                view.clear_selection();
                open_pressed_link = left && !event.motion && !release_was_drag;
            }
        } else if left_press && !event.motion {
            self.stop_selection_auto_scroll();
            self.record_pressed_click(view, &event);
            if !view.begin_selection(row, col) {
                view.begin_frame_selection(row, col);
            }
            self.snap_multi_click(view, &event, row, col);
            self.dirty = true;
        } else if left_press && event.motion {
            self.left_mouse_dragged = true;
            self.click_counter.reset();
            view.extend_active_selection(row, col);
            self.update_selection_auto_scroll(view, row, col);
            self.dirty = true;
        } else if !event.press && view.has_selection() {
            self.stop_selection_auto_scroll();
            let text = view.end_active_selection();
            if let Some(text) = text {
                self.copy_selection(&text, view);
            }
            self.dirty = true;
        } else if !event.press {
            self.stop_selection_auto_scroll();
            view.clear_selection();
            open_pressed_link = left && !event.motion && !release_was_drag;
        }
        // A plain left release opens the pressed link first, then the link under the release; with
        // no link there, it fires the click target recorded at the press.
        if open_pressed_link {
            let url = self
                .pressed_hyperlink
                .take()
                .or_else(|| view.hyperlink_at(row, col));
            if let Some(url) = url {
                self.open_hyperlink(&url);
            } else if !event.shift && !event.alt && !event.ctrl {
                // The dispatch is gated on the release's modifiers: modified clicks stay
                // selection-only.
                self.dispatch_plain_click(view, row);
            }
        }
        // Clear the press state on every left release so a later release can
        // never open a stale press; the click target rides the same cleanup.
        if left && !event.press {
            self.left_mouse_dragged = false;
            self.pressed_hyperlink = None;
            self.pressed_click = None;
        }
    }

    /// Count a plain left press into the multi-click run and snap the selection it began: a
    /// double click selects the word under the pointer, a triple click the row (upstream #1089).
    /// The release then copies it like a finished drag. Modified presses break the run.
    fn snap_multi_click(
        &mut self,
        view: &mut AgentView,
        event: &crate::mouse::MouseEvent,
        row: usize,
        col: usize,
    ) {
        if event.shift || event.alt || event.ctrl {
            self.click_counter.reset();
            return;
        }
        let unit = match self.click_counter.register(Instant::now(), row, col) {
            2 => crate::selection::SelectUnit::Word,
            3 => crate::selection::SelectUnit::Line,
            _ => return,
        };
        view.select_unit_at(row, col, unit);
    }

    /// Open one clicked link; a headless run has no terminal, so it records the
    /// URL for its verifier instead.
    fn open_hyperlink(&mut self, url: &str) {
        let Some(href) = crate::hyperlinks::openable_href(url) else {
            return;
        };
        self.opened_urls.push(href.clone());
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            crate::browser::open_in_browser(&href);
        }
    }

    /// Copy a finished selection through the shared platform/tmux/OSC 52
    /// chain. A headless run records the text for its verifier instead.
    /// Only a local platform-tool write confirms delivery; a terminal
    /// request is reported as unconfirmed.
    pub(super) fn copy_selection(&mut self, text: &str, view: &mut AgentView) {
        let lines = text.lines().count().max(1);
        self.copies.push(text.to_string());
        self.track_selection(lines);
        if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            self.toast("Copied selection to clipboard", view);
            return;
        }
        match crate::clipboard::copy_to_clipboard(text, &mut self.osc_sink) {
            Ok(crate::clipboard::CopyOutcome::Confirmed) => {
                self.toast("Copied selection to clipboard", view);
            }
            Ok(crate::clipboard::CopyOutcome::Requested) => {
                self.toast(crate::clipboard::CLIPBOARD_REQUESTED, view);
            }
            Err(error) => self.error_row(&error, view),
        }
    }

    fn update_selection_auto_scroll(&mut self, view: &AgentView, row: usize, col: usize) {
        match view.selection_auto_scroll_direction(row) {
            Some(direction) => match &mut self.selection_auto_scroll {
                Some(armed) if armed.direction == direction => {
                    armed.row = row;
                    armed.col = col;
                }
                _ => {
                    self.selection_auto_scroll = Some(SelectionAutoScroll {
                        direction,
                        row,
                        col,
                        started: Instant::now(),
                    });
                }
            },
            None => self.selection_auto_scroll = None,
        }
    }

    pub(crate) fn stop_selection_auto_scroll(&mut self) {
        self.selection_auto_scroll = None;
    }

    /// Whether the auto-scroll driver is armed: the run loop's quiet tick keys on
    /// it (an idle surface parks the tick).
    pub(crate) fn selection_auto_scroll_armed(&self) -> bool {
        self.selection_auto_scroll.is_some()
    }

    /// One idle tick of the auto-scroll (the run loop's 50 ms arm stands in
    /// for TS's timer): after the hold window each tick scrolls one line set.
    pub(crate) fn selection_auto_scroll_tick(&mut self, view: &mut AgentView) {
        let Some(armed) = self.selection_auto_scroll.clone() else {
            return;
        };
        if Instant::now().duration_since(armed.started) < SELECTION_AUTO_SCROLL_DELAY {
            return;
        }
        if view.selection_auto_scroll_direction(armed.row) != Some(armed.direction)
            || !self
                .window
                .scroll_selection(armed.direction, armed.col, view)
        {
            self.selection_auto_scroll = None;
            return;
        }
        self.dirty = true;
    }

    /// A bracketed paste goes to the focused input.
    pub(crate) fn handle_paste(&mut self, text: &str, view: &mut AgentView) {
        // The overlays own the whole frame while open (like their key
        // dispatch): the paste lands in the overlay's own input or is
        // consumed by the input-less ones, never in the hidden editor
        // prompt behind.
        if view.route_paste(text) {
            self.dirty = true;
            return;
        }
        let _ = view.editor.handle_paste(text);
    }

    /// One key press while the `/effort` picker is open: Esc/Ctrl+C close it
    /// without applying; Enter applies the picked row.
    async fn handle_choice_picker_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        // The picker consumes Ctrl+C (close, not exit); report it so the force-quit
        // guard can disarm.
        if id == "ctrl+c" {
            self.exit_guard.note_ctrl_c_handled();
        }
        let action = view
            .choice_picker
            .as_mut()
            .map(|picker| picker.handle_key(&id, view.editor.keybindings()));
        match action {
            Some(ChoicePickerAction::None) | None => {}
            Some(ChoicePickerAction::Cancel) => {
                view.choice_picker = None;
                self.dirty = true;
            }
            Some(ChoicePickerAction::Apply { purpose, key }) => {
                view.choice_picker = None;
                self.dirty = true;
                match purpose {
                    ChoicePurpose::Effort => self.apply_thinking_level(&key, view).await,
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<()> {
        let handled = self.handle_key_inner(key, view, running).await;
        self.reconcile_queue_hold().await;
        handled
    }

    /// One key press: a mounted picker, panel or selector owns the frame and takes the key;
    /// otherwise it runs the chat surface's ladder.
    async fn handle_key_inner(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<()> {
        // A key press breaks a multi-click run (standard editor behavior): the next press on the
        // same spot is a fresh single click, not a double click.
        self.click_counter.reset();
        // Any non-Escape key re-arms the double-Esc tree shortcut (one shot per input chain);
        // Escape itself never resets, so repeated Escape converges to the inert empty state.
        if key_event_to_id(&key).is_some_and(|id| id != "escape") {
            self.escape_tree_shortcut_spent = false;
        }
        // The `/model` picker owns the frame while open: its keys go before the
        // editor, viewport keys, and Ctrl+C.
        if view.model_picker.is_some() {
            return self.handle_model_picker_key(key, view).await;
        }
        // The `/effort` picker owns the frame the same way.
        if view.choice_picker.is_some() {
            return self.handle_choice_picker_key(key, view).await;
        }
        if matches!(
            view.harness_selector,
            Some(crate::view::HarnessSelectorState::Open(_))
        ) {
            return self.handle_harness_selector_key(key, view);
        }
        if view.mcp_view.is_some() {
            return self.handle_mcp_view_key(key, view);
        }
        // The factory page owns the frame the same way.
        if view.factory_view.is_some() {
            return self.handle_factory_view_key(key, view).await;
        }
        // The `/heartbeats` view owns the frame the same way.
        if view.heartbeats_picker.is_some() {
            return self.handle_heartbeats_picker_key(key, view).await;
        }
        if view.bash_view.is_some() {
            return self.handle_bash_view_key(key, view);
        }
        if view.goal_panel.is_some() {
            return self.handle_goal_panel_key(key, view);
        }
        if view.info_panel.is_some() {
            return self.handle_info_panel_key(key, view);
        }
        if view.tree_selector.is_some() {
            return self.handle_tree_selector_key(key, view).await;
        }
        if view.fork_selector.is_some() {
            return self.handle_fork_selector_key(key, view).await;
        }
        if view.rewind_selector.is_some() {
            return self.handle_rewind_selector_key(key, view).await;
        }
        if view.confirm.is_some() {
            return self.handle_confirm_key(key, view).await;
        }
        if view.ask_question.is_some() {
            return self.handle_ask_question_key(key, view);
        }
        if view.provider_auth.is_some() {
            return self.handle_provider_auth_key(key, view).await;
        }
        if view.auth_panel.is_some() {
            return self.handle_auth_panel_key(key, view);
        }
        if view.settings_menu.is_some() {
            return self.handle_settings_menu_key(key, view).await;
        }
        // The `/share` loader owns the frame while an upload runs.
        if view.share_loader.is_some() {
            return self.handle_share_loader_key(key, view);
        }
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        self.handle_chat_key(&id, view, running).await
    }
}
