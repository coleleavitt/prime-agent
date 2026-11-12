//! The chat surface's key ladder, for a key no overlay owns: the transcript viewport keys, the
//! focused activity dock, vim mode, the app actions in registration order, and the editor
//! fallback. Every match goes through the effective bindings, so a `keybindings.json` override
//! moves both handler and hint.
use pa_types::daemon::CycleDirection;

use super::search::SearchStep;
use super::{
    AgentView,
    ChatEntry,
    DaemonCommand,
    DockFocusSource,
    Map,
    ModelSwitchScope,
    QueueBrowseDirection,
    QueueLane,
    Result,
    SessionUi,
    StatusKind,
    SubmitBehavior,
};

/// One chat-surface key action, bound to a keybinding id by the ladder tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatKey {
    PageUp,
    PageDown,
    ViewportTop,
    ViewportFollow,
    PasteImage,
    InputClear,
    Exit,
    Interrupt,
    Clear,
    Suspend,
    ModelSelect,
    CycleModel(CycleDirection),
    PlanToggle,
    ToolsExpand,
    SubagentsFocus,
    ExternalEditor,
    PromptStash,
    SessionNew,
    SessionResume,
    AgentsBack,
    SessionTree,
    SessionFork,
    Search(SearchStep),
    SendNow,
    ClearQueue,
    RecallDraft,
    BrowseQueue(QueueBrowseDirection),
    MoveQueued(i64),
    FollowUp,
    PickerTab,
    DockBelowPrompt,
}

impl ChatKey {
    /// The keys that act only on a blank editor; with text they stay editor keys.
    fn needs_blank_editor(self) -> bool {
        matches!(
            self,
            Self::AgentsBack | Self::SessionTree | Self::SessionFork
        )
    }
}

/// The transcript viewport keys, ahead of the dock focus and the editor.
const VIEWPORT_KEYS: &[(&str, ChatKey)] = &[
    ("tui.viewport.pageUp", ChatKey::PageUp),
    ("tui.viewport.pageDown", ChatKey::PageDown),
    ("tui.viewport.top", ChatKey::ViewportTop),
    ("tui.viewport.follow", ChatKey::ViewportFollow),
];

/// The app actions in registration order, then the editor-context keys the editor would
/// otherwise take. A key whose action declines (a guard not met) falls through to the next
/// binding.
const APP_KEYS: &[(&str, ChatKey)] = &[
    // The editor's own ctrl+v is otherwise unbound, so the exact match is safe before any
    // editor motion.
    ("app.clipboard.pasteImage", ChatKey::PasteImage),
    ("app.input.clear", ChatKey::InputClear),
    ("app.exit", ChatKey::Exit),
    // `app.interrupt` routes through the `app.clear` handlers; only the second-press exit is
    // ctrl+c's alone.
    ("app.interrupt", ChatKey::Interrupt),
    ("app.clear", ChatKey::Clear),
    ("app.suspend", ChatKey::Suspend),
    ("app.model.select", ChatKey::ModelSelect),
    (
        "app.model.cycleForward",
        ChatKey::CycleModel(CycleDirection::Forward),
    ),
    (
        "app.model.cycleBackward",
        ChatKey::CycleModel(CycleDirection::Backward),
    ),
    ("app.plan.toggle", ChatKey::PlanToggle),
    ("app.tools.expand", ChatKey::ToolsExpand),
    ("app.subagents.focus", ChatKey::SubagentsFocus),
    ("app.editor.external", ChatKey::ExternalEditor),
    ("app.prompt.stash", ChatKey::PromptStash),
    ("app.session.new", ChatKey::SessionNew),
    ("app.session.resume", ChatKey::SessionResume),
    ("app.agents.back", ChatKey::AgentsBack),
    ("app.session.tree", ChatKey::SessionTree),
    ("app.session.fork", ChatKey::SessionFork),
    ("app.search.older", ChatKey::Search(SearchStep::Older)),
    ("app.search.newer", ChatKey::Search(SearchStep::Newer)),
    ("app.message.sendNow", ChatKey::SendNow),
    ("app.message.clearQueue", ChatKey::ClearQueue),
    ("tui.editor.cursorUp", ChatKey::RecallDraft),
    (
        "app.message.navigateOlder",
        ChatKey::BrowseQueue(QueueBrowseDirection::Older),
    ),
    (
        "app.message.navigateNewer",
        ChatKey::BrowseQueue(QueueBrowseDirection::Newer),
    ),
    ("app.message.moveEarlier", ChatKey::MoveQueued(-1)),
    ("app.message.moveLater", ChatKey::MoveQueued(1)),
    ("app.message.followUp", ChatKey::FollowUp),
    ("tui.input.tab", ChatKey::PickerTab),
    ("tui.editor.cursorDown", ChatKey::DockBelowPrompt),
];

impl SessionUi {
    /// One key on the chat surface. Dispatch order: the transcript viewport keys, then the
    /// focused subagent summary line, then vim mode, then the app actions, then the editor.
    pub(super) async fn handle_chat_key(
        &mut self,
        id: &str,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<()> {
        if self.run_chat_keys(VIEWPORT_KEYS, id, view, running).await? {
            return Ok(());
        }
        if self.subagents_focused && self.handle_dock_focus_key(id, view) {
            return Ok(());
        }
        // Vim mode owns NORMAL mode's printable keys and INSERT mode's Escape before any app
        // binding; everything it declines (Enter, chords, a bare NORMAL-mode Escape) falls through.
        if view.editor.handle_vim_key(id) {
            self.clear_ctrl_c_hint();
            self.dirty = true;
            return Ok(());
        }
        if self.run_chat_keys(APP_KEYS, id, view, running).await? {
            return Ok(());
        }
        self.handle_editor_key(id, view).await
    }

    /// Run the actions whose bindings match `id`, in table order, until one handles the key.
    async fn run_chat_keys(
        &mut self,
        keys: &[(&str, ChatKey)],
        id: &str,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<bool> {
        for &(binding, key) in keys {
            if !view.editor.keybindings().matches(id, binding) {
                continue;
            }
            if key.needs_blank_editor() && !view.editor.get_text().trim().is_empty() {
                continue;
            }
            if self.run_chat_key(key, id, view, running).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// One bound action; `false` when its guard declines the key.
    async fn run_chat_key(
        &mut self,
        key: ChatKey,
        id: &str,
        view: &mut AgentView,
        running: &mut bool,
    ) -> Result<bool> {
        match key {
            ChatKey::PageUp => self.scroll_transcript_page(-1, "page_up", view),
            ChatKey::PageDown => self.scroll_transcript_page(1, "page_down", view),
            ChatKey::ViewportTop => {
                view.scroll_to_top();
                self.window.request_head();
                self.track_scroll("top", view.is_following());
                self.dirty = true;
            }
            ChatKey::ViewportFollow => {
                self.collapse_transcript_window(view);
                view.scroll_to_bottom();
                self.track_scroll("follow", view.is_following());
                self.dirty = true;
            }
            ChatKey::PasteImage => self.handle_clipboard_image_paste(view).await,
            ChatKey::InputClear => self.handle_escape_key(view).await?,
            // Ctrl+D on an empty editor exits at once (a detach: the session keeps
            // running in the daemon), the terminal's end-of-input habit.
            ChatKey::Exit => {
                if !view.editor.get_text().is_empty() {
                    return Ok(false);
                }
                self.exit_reason = "ctrl_d";
                *running = false;
            }
            ChatKey::Interrupt | ChatKey::Clear => self.handle_clear_press(key, id, view, running),
            // Suspend hands the terminal to the shell; the loop performs the cycle right after
            // dispatch, and the SIGCONT continuation re-applies raw mode and mouse tracking.
            ChatKey::Suspend => {
                if crate::suspend::supported() {
                    self.suspend_requested = true;
                } else {
                    self.note("Suspend to background is not supported on Windows", view);
                }
            }
            ChatKey::ModelSelect => {
                // A completion request parked by this same press must not materialize
                // a dropdown over the picker on the next idle tick.
                view.editor.cancel_autocomplete();
                self.open_model_picker(view, "", ModelSwitchScope::SavedDefault)
                    .await?;
                // The picker opens over the user's own text (a draft or browsed
                // message), so its apply must keep it.
                self.picker_restored_draft = true;
                self.track_menu_opened("model", "shortcut");
                self.dirty = true;
            }
            ChatKey::CycleModel(direction) => self.cycle_model(direction, view).await,
            // Plan mode flips through the session command (durable, and the
            // kernel guard follows); the draft in the editor stays untouched.
            // A published permission-mode cycle replaces the toggle with its
            // commands (Claude Code's shift+tab mode cycle).
            ChatKey::PlanToggle => {
                self.track_client_adoption(crate::interactive::ClientAdoption::PlanKey);
                let commands = if self.permission_cycle.is_empty() {
                    vec!["/plan".to_string()]
                } else {
                    self.permission_cycle.clone()
                };
                for command in commands {
                    self.submit_prompt(&command, SubmitBehavior::Steer, view)
                        .await?;
                }
            }
            ChatKey::ToolsExpand => self.cycle_detail(view),
            ChatKey::SubagentsFocus => {
                self.focus_subagents_summary(&DockFocusSource::Shortcut, view);
                self.dirty = true;
            }
            ChatKey::ExternalEditor => self.request_external_editor(view),
            // Stash with a draft, restore with an empty editor; the manual stash
            // returns only on this key, never on a chat open or switch landing.
            ChatKey::PromptStash => {
                // A queue browse parks the real draft; leave the browse first so the stash acts
                // on the user's own draft (the disarmed browse cannot turn the next Enter into a
                // delete).
                if self.queue_selection.has_draft() {
                    let draft = self.queue_selection.reset();
                    view.editor.set_text(&draft);
                    self.dirty = true;
                } else if self.queue_selection.is_browsing() {
                    self.queue_selection.reset();
                    self.dirty = true;
                }
                self.sync_queue_selection(view);
                self.handle_prompt_stash(view);
            }
            // The `/new` flow; fires with a draft too (no editor-text gate).
            ChatKey::SessionNew => {
                self.start_new_session(view).await?;
                self.dirty = true;
            }
            // Open the agents view; fires with a draft too — the draft is stashed
            // on the exit path and returns when the chat reopens.
            ChatKey::SessionResume => {
                if self.leave_for_agents_view(view) {
                    self.agents_view_resume_picker = true;
                }
            }
            // With an empty editor the bound key hands the terminal to the agents
            // view; with text it stays an editor cursor motion.
            ChatKey::AgentsBack => {
                self.leave_for_agents_view(view);
            }
            ChatKey::SessionTree => {
                self.open_tree_selector(view, None).await?;
                self.dirty = true;
            }
            ChatKey::SessionFork => {
                self.open_fork_selector(view).await?;
                self.dirty = true;
            }
            ChatKey::Search(step) => self.step_search(Some(step), view),
            ChatKey::SendNow => {
                view.editor.cancel_autocomplete();
                self.send_now(view).await?;
            }
            ChatKey::ClearQueue => self.clear_queue(view).await?,
            // Up on an empty editor: the draft the last Ctrl+C cleared comes back,
            // else the human-typed queued messages come out of the queue into the
            // editor (history browses only when neither exists).
            ChatKey::RecallDraft => {
                let recalled = view.editor.get_text().is_empty()
                    && !view.editor.is_showing_autocomplete()
                    && !view.editor.is_history_navigation_active()
                    && !self.queue_selection.is_browsing()
                    && (self.restore_cleared_draft(view)
                        || self.pull_queue_into_editor(view).await?);
                return Ok(recalled);
            }
            ChatKey::BrowseQueue(direction) => {
                self.browse_queue_selection(direction, view);
                self.dirty = true;
            }
            ChatKey::MoveQueued(delta) => self.move_queue_selection(delta, view).await?,
            ChatKey::FollowUp => self.handle_follow_up_key(view).await?,
            ChatKey::PickerTab => return self.open_picker_from_tab(view).await,
            // TS `CustomEditor.handleInput`'s move-below-prompt hook
            // (`onMoveBelowPrompt` -> `focusSubagentSummary`): Down at the end
            // of the prompt — no autocomplete open, no history browse, the
            // cursor at the last line's end — hands the focus to the activity
            // dock in every session shape, all-zero counts included; every
            // other Down falls through to the editor's cursor motion. Only the
            // tray override (the armed exit hint, the streaming follow-up
            // hint) keeps the editor's Down.
            ChatKey::DockBelowPrompt => {
                if view.editor.is_showing_autocomplete()
                    || view.editor.is_history_navigation_active()
                    || !view.editor.is_cursor_at_end()
                    || !self.focus_subagents_summary(&DockFocusSource::PromptDown, view)
                {
                    return Ok(false);
                }
                // The selection collapses with the handoff, or a later keystroke would
                // fall back through and replace the stale range.
                view.editor.clear_selection();
                self.dirty = true;
            }
        }
        Ok(true)
    }

    /// A page key moves half a viewport (Claude Code's step): the rows read last stay on screen
    /// for context. The viewport consumes the key before the editor, so a selection must
    /// collapse here or it survives the scroll as a stale replace range.
    fn scroll_transcript_page(
        &mut self,
        direction: isize,
        name: &'static str,
        view: &mut AgentView,
    ) {
        let half_page = (view.page_size() / 2).max(1) as isize;
        view.editor.clear_selection();
        self.window.scroll(direction * half_page, view);
        self.track_scroll(name, view.is_following());
        self.dirty = true;
    }

    /// The focused (expanded) dock owns these keys; every other key falls through after
    /// releasing the focus (`false`). Configured bindings match first, so a rebound arrow keeps
    /// its configured action; the literal Left/Right fallbacks apply only after none matched.
    fn handle_dock_focus_key(&mut self, id: &str, view: &mut AgentView) -> bool {
        let kb = view.editor.keybindings();
        let open = kb.matches(id, "tui.select.confirm") || kb.matches(id, "app.subagents.focus");
        let down = kb.matches(id, "tui.select.down");
        let up = kb.matches(id, "tui.select.up");
        let back = kb.matches(id, "tui.select.cancel") || kb.matches(id, "app.agents.back");
        let expand = kb.matches(id, "app.tools.expand");
        if open {
            self.open_dock_group_view(view);
            return true;
        }
        let dock = self.activity_dock_state();
        // Up from the top row leaves the dock; Down from the bottom row wraps to the top.
        if down || (up && dock.groups()[0] != self.activity_group) {
            let direction = if down {
                crate::chrome::ActivityDirection::Next
            } else {
                crate::chrome::ActivityDirection::Prev
            };
            self.activity_group = dock.step(self.activity_group, direction);
            self.update_subagent_summary(view);
            self.dirty = true;
            return true;
        }
        if expand {
            self.cycle_detail(view);
            return true;
        }
        if up || back || id == "left" {
            self.subagents_focused = false;
            self.update_subagent_summary(view);
            self.dirty = true;
            return true;
        }
        if id == "right" {
            self.open_dock_group_view(view);
            return true;
        }
        self.subagents_focused = false;
        self.update_subagent_summary(view);
        false
    }

    /// `app.input.clear` (default escape): the completion menu, the selection, the search
    /// highlight, the side-question pane and a queue browse each take the press first; then the
    /// double-Escape ladder.
    async fn handle_escape_key(&mut self, view: &mut AgentView) -> Result<()> {
        // The completion surface consumes Esc: the open dropdown closes and the abort ladder
        // never runs — closing a menu must not abort a running turn (deliberate divergence: the
        // TS custom-editor overlay propagates Esc to the interrupt after closing).
        if view.editor.is_showing_autocomplete() || view.editor.has_pending_autocomplete() {
            view.editor.cancel_autocomplete();
            self.clear_ctrl_c_hint();
            return Ok(());
        }
        // An active selection consumes the first Escape (standard editors' drop-the-selection
        // press): the interrupt/clear ladder runs on the next press.
        if view.editor.has_selection() {
            view.editor.clear_selection();
            self.clear_ctrl_c_hint();
            self.dirty = true;
            return Ok(());
        }
        self.clear_ctrl_c_hint();
        // A shown search match takes the first Escape: the highlight clears.
        if self.clear_search(view) {
            return Ok(());
        }
        // An open side-question pane owns the key: the running turn aborts and
        // the pane closes; the armed escape-repeat disarms first.
        if view.side_pane.is_some() {
            self.escape_repeat_action = None;
            self.escape_repeat_until = None;
            self.clear_side_question(true, view);
            return Ok(());
        }
        // Leaving browse mode restores the stashed draft instead of arming an
        // accidental empty-submit delete of the selected queued message.
        if self.queue_selection.has_draft() {
            let draft = self.queue_selection.reset();
            view.editor.set_text(&draft);
            self.sync_queue_selection(view);
            self.dirty = true;
            return Ok(());
        }
        // Double-Escape: the second press within the window opens `/rewind` (Claude Code's
        // message selector; the tree on a daemon without it) when the session is idle or the
        // editor empty, and clears the input otherwise. The repeat's tree action is one shot per
        // input chain (the operator's 2026-09-29 Esc-overflow ruling).
        if let Some(action) = self.take_escape_repeat_action() {
            if action == "tree" {
                self.escape_tree_shortcut_spent = true;
                self.open_rewind_or_tree(view).await?;
            } else {
                view.editor.set_text("");
            }
            self.dirty = true;
            return Ok(());
        }
        let action = if self.turn_active || view.editor.get_text().trim().is_empty() {
            "tree"
        } else {
            "clear"
        };
        if action == "tree" && self.escape_tree_shortcut_spent {
            // The gesture already fired: this press interrupts like every Escape
            // but arms no reopen.
            self.interrupt_running_work(view);
            return Ok(());
        }
        self.arm_escape_repeat(action);
        // The repeat arms, then the same abort ladder as the Ctrl+C interrupt
        // runs, minus the exit hint.
        self.interrupt_running_work(view);
        Ok(())
    }

    /// `app.clear` (default ctrl+c) and `app.interrupt`: an open completion menu closes, a
    /// selection copies (`app.clear` only), and otherwise the press clears the editor (idle) or
    /// interrupts the running work.
    fn handle_clear_press(
        &mut self,
        key: ChatKey,
        id: &str,
        view: &mut AgentView,
        running: &mut bool,
    ) {
        let interrupt = key == ChatKey::Interrupt;
        // One handled Ctrl+C press: the force-quit guard disarms once every
        // observed press of the pair was handled without an exit.
        if id == "ctrl+c" {
            self.exit_guard.note_ctrl_c_handled();
        }
        if view.editor.is_showing_autocomplete() {
            view.editor.cancel_autocomplete();
            self.clear_ctrl_c_hint();
            return;
        }
        // A selection makes the press a copy (Claude Code's rule): it neither
        // interrupts nor arms the exit. The transcript selection first, then
        // the editor's.
        if !interrupt && view.has_selection() {
            if let Some(text) = view.end_active_selection() {
                self.copy_selection(&text, view);
            }
            self.dirty = true;
            return;
        }
        if !interrupt && view.editor.has_selection() {
            if let Some(text) = view.editor.copy_selection() {
                self.copy_selection(&text, view);
            }
            self.dirty = true;
            return;
        }
        // A press with work to interrupt only interrupts (Claude Code's rule):
        // stopping a turn never arms the exit, so pressing twice to stop a
        // stubborn turn cannot quit mid-abort.
        if self.has_interruptible_work(view) {
            self.interrupt_or_clear_draft(key, view);
        } else {
            self.clear_editor_or_exit(key, view, running);
        }
    }

    /// The clear press with nothing to interrupt: a second press inside the hint window exits;
    /// otherwise the editor clears into the slot Up restores and the exit hint arms.
    fn clear_editor_or_exit(&mut self, key: ChatKey, view: &mut AgentView, running: &mut bool) {
        let interrupt = key == ChatKey::Interrupt;
        // A second press inside the hint window exits.
        if self.ctrl_c_hint_visible() && !interrupt {
            self.exit_reason = "ctrl_c_twice";
            *running = false;
            return;
        }
        // A queue browse ends first, its stashed draft restored: the
        // press acts on the user's own draft.
        if self.queue_selection.has_draft() || self.queue_selection.is_browsing() {
            let draft = self.queue_selection.reset();
            view.editor.set_text(&draft);
            self.sync_queue_selection(view);
        }
        let draft = view.editor.get_text();
        if !draft.is_empty() {
            self.hold_editor_draft(draft, view);
        }
        if !interrupt {
            self.show_ctrl_c_hint("app.clear");
        }
        self.dirty = true;
    }

    /// The clear press on running work. With a genuine draft in the box the press clears the box
    /// instead of interrupting, so wiping a half-typed message mid-turn never stops the turn. The
    /// draft lands in the slot Up restores, the hint stays disarmed, and the next press (empty
    /// box now) falls through to the interrupt. A queue browse, a stashed draft, or a pure
    /// `app.interrupt` binding keep interrupting.
    fn interrupt_or_clear_draft(&mut self, key: ChatKey, view: &mut AgentView) {
        let draft = view.editor.get_text();
        if busy_press_clears_draft(
            key == ChatKey::Interrupt,
            self.queue_selection.is_browsing(),
            self.queue_selection.has_draft(),
            &draft,
        ) {
            self.hold_editor_draft(draft, view);
            self.clear_ctrl_c_hint();
            self.dirty = true;
            return;
        }
        self.clear_ctrl_c_hint();
        // A running side question aborts first; the pane stays mounted and renders the
        // cancelled turn when the terminal event streams back.
        if let Some(side_question_id) = self.active_side_question_id.clone() {
            let client = self.client.clone();
            let active_session_id = self.active_session_id.clone();
            let notes = self.notes.clone();
            tokio::spawn(async move {
                if let Err(error) = client
                    .request_ok(DaemonCommand::AbortSideQuestion {
                        id: None,
                        active_session_id,
                        side_question_id,
                        rest: Map::default(),
                    })
                    .await
                {
                    let _ = notes.send(format!("the side question abort failed: {error:#}"));
                }
            });
        }
        self.interrupt_running_work(view);
        self.dirty = true;
    }

    /// Clear the editor into the slot Up restores. The PREVIOUS slot's text (if any) is promoted
    /// into persistent history, matching CC's `holdCleared(prev, next, { intoHistory: true })`: a
    /// user who clears several drafts in a row keeps the just-cleared one recallable via Up AND
    /// the earlier ones recallable via the project's history.
    fn hold_editor_draft(&mut self, draft: String, view: &mut AgentView) {
        let new_draft = super::prompt_history::ClearedDraft {
            text: draft,
            paste_snapshot: view.editor.get_paste_snapshot(),
        };
        self.hold_cleared_draft(view, new_draft);
        view.editor.set_text("");
    }

    /// Hand the terminal to the agents view (the bound key's exit); a session without one says
    /// why. Returns whether the handoff was requested.
    fn leave_for_agents_view(&mut self, view: &mut AgentView) -> bool {
        self.dirty = true;
        if self.return_to_agents_view {
            self.open_agents_view = true;
            self.exit_requested = true;
            return true;
        }
        self.note(
            "The agents view needs a daemon-hosted session; start normally (without --no-session) to browse sessions",
            view,
        );
        false
    }

    /// A configured editor hands off through the loop (the terminal belongs to the renderer).
    fn request_external_editor(&mut self, view: &mut AgentView) {
        match crate::external_editor::editor_command() {
            None => {
                view.push_entry(ChatEntry::Status {
                    text: "\u{26a0} No editor configured. Set $VISUAL or $EDITOR environment variable."
                        .to_string(),
                    kind: StatusKind::Warning,
                });
                self.last_status_index = None;
                if let Some(telemetry) = self.telemetry.clone() {
                    tokio::spawn(async move {
                        telemetry.external_editor_used("no_editor").await;
                    });
                }
            }
            Some(command) => {
                self.external_editor_request = Some(command);
            }
        }
        self.dirty = true;
    }

    /// The follow-up key: parks the message on the follow-up lane and delivers when the run goes
    /// idle; while a queued message is selected, the edit re-parks it there.
    async fn handle_follow_up_key(&mut self, view: &mut AgentView) -> Result<()> {
        if self.queue_selection.is_browsing() || !view.editor.get_text().trim().is_empty() {
            view.editor.submit();
            for event in view.editor.take_events() {
                if let crate::editor::EditorEvent::Submitted(text) = event {
                    self.submit_editor_text(&text, QueueLane::FollowUp, view)
                        .await?;
                }
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// Tab in a picker-command argument context opens the command's menu prefilled with the
    /// typed partial. An open completion dropdown keeps its own Tab; this is the no-menu path.
    /// `false` when the key stays the editor's.
    async fn open_picker_from_tab(&mut self, view: &mut AgentView) -> Result<bool> {
        if view.editor.is_showing_autocomplete() {
            return Ok(false);
        }
        let Some((command, partial)) = view.editor.picker_argument_context() else {
            return Ok(false);
        };
        // A completion request parked by this same press must not materialize
        // a dropdown over the menu on the next tick.
        view.editor.cancel_autocomplete();
        let scope = match command.as_str() {
            "switch" => ModelSwitchScope::SessionOnly,
            "model" => ModelSwitchScope::SavedDefault,
            "mcp" => {
                self.leave_queue_browse_for_picker(view);
                self.open_mcp_view("/mcp", view, partial.trim()).await?;
                // A failed roster load leaves no view mounted; the flag must not leak into
                // the NEXT picker.
                if view.mcp_view.is_none() {
                    self.picker_restored_draft = false;
                }
                self.track_menu_opened("mcp", "tab");
                self.dirty = true;
                return Ok(true);
            }
            _ => return Ok(false),
        };
        self.leave_queue_browse_for_picker(view);
        self.open_model_picker(view, partial.trim(), scope).await?;
        // Belt-and-braces (the picker always mounts here); keeps the
        // failed-open contract symmetric with the mcp arm.
        if view.model_picker.is_none() {
            self.picker_restored_draft = false;
        }
        self.track_menu_opened("model", "tab");
        self.dirty = true;
        Ok(true)
    }

    /// The menu takes the frame from a queue browse: the next Enter must submit a prompt, not
    /// route into `apply_queue_selection`; ending the browse restores the stashed draft, so a
    /// failed menu open loses nothing.
    fn leave_queue_browse_for_picker(&mut self, view: &mut AgentView) {
        if self.queue_selection.has_draft() {
            let draft = self.queue_selection.reset();
            view.editor.set_text(&draft);
            self.picker_restored_draft = true;
        } else {
            self.queue_selection.reset();
        }
        self.sync_queue_selection(view);
    }

    /// The editor fallback: the key edits the prompt, then its events apply — a submit sends
    /// (or re-parks a browsed queued message), a cut/copy reaches the clipboard.
    async fn handle_editor_key(&mut self, id: &str, view: &mut AgentView) -> Result<()> {
        view.editor.handle_input(id);
        if !view.editor.get_text().is_empty() {
            self.clear_ctrl_c_hint();
        }
        for event in view.editor.take_events() {
            match event {
                crate::editor::EditorEvent::Submitted(text) => {
                    self.submit_editor_text(&text, QueueLane::Steering, view)
                        .await?;
                }
                crate::editor::EditorEvent::ClipboardWrite(text) => {
                    // An editor cut/copy follows the same clipboard path as
                    // mouse selection. Headless tests retain the selection.
                    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                        match crate::clipboard::copy_to_clipboard(&text, &mut self.osc_sink) {
                            Ok(crate::clipboard::CopyOutcome::Confirmed) => {
                                self.toast("Copied selection to clipboard", view);
                            }
                            Ok(crate::clipboard::CopyOutcome::Requested) => {
                                self.toast(crate::clipboard::CLIPBOARD_REQUESTED, view);
                            }
                            Err(message) => self.error_row(&message, view),
                        }
                    } else {
                        self.toast("Copied selection to clipboard", view);
                    }
                }
                crate::editor::EditorEvent::Changed(_)
                | crate::editor::EditorEvent::AutocompleteToggled(_) => {}
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// One submitted editor text: a browsed queued message re-parks on `lane`; otherwise the text
    /// joins the prompt history and submits on `lane`'s delivery.
    async fn submit_editor_text(
        &mut self,
        text: &str,
        lane: QueueLane,
        view: &mut AgentView,
    ) -> Result<()> {
        if self.queue_selection.is_browsing() {
            return self.apply_queue_selection(text, lane, view).await;
        }
        view.editor.add_to_history(text);
        self.record_prompt_history(text);
        let behavior = match lane {
            QueueLane::Steering => SubmitBehavior::Steer,
            QueueLane::FollowUp => SubmitBehavior::FollowUp,
        };
        self.submit_prompt(text, behavior, view).await
    }
}

/// Whether a Ctrl+C that lands on running work should clear a genuine draft
/// in the input box instead of interrupting the work. The clear only applies
/// to the `app.clear` press (`interrupt` is false): a user-bound pure
/// `app.interrupt` key always interrupts. A queue browse or a stashed draft
/// (the box shows a browsed message, not the user's own draft) and an empty
/// box both fall through to the interrupt.
fn busy_press_clears_draft(
    interrupt: bool,
    browsing: bool,
    has_stashed_draft: bool,
    draft: &str,
) -> bool {
    !interrupt && !browsing && !has_stashed_draft && !draft.is_empty()
}

/// The opening phase's echo gate: whether the post-open key dispatch
/// ([`SessionUi::handle_key`]'s ladder) would consume this key BEFORE its
/// editor fallback, in the state a fresh session's opening can be in — no
/// dock focus, no mounted picker or panel, no turn. This is the fresh-state
/// projection of the ladder: the always-fire arms in registration order,
/// the empty-editor arms, and the two editor-context arms; keep it in
/// lockstep with the ladder above.
///
/// A key claimed here takes its normal route — queued behind the session
/// open and dispatched through the full keymap-aware ladder once the
/// session lands — instead of echoing into the editor as a stray motion
/// (the misroute: a user-bound `left`/`space`/single-char action must run
/// its action, and default `left` on the empty editor is `app.agents.back`,
/// not a cursor move).
pub(crate) fn opening_echo_key_claimed(
    kb: &crate::keybindings::KeybindingsManager,
    id: &str,
    editor: &crate::editor::Editor,
) -> bool {
    // The always-fire arms: the transcript viewport, the image paste, the
    // escape ladder, the interrupt family, suspend, the model picker and
    // cycles, the detail cycle, the dock focus, the external editor, the
    // stash, the session-level commands, the queue browse, and the
    // follow-up key.
    if kb.matches(id, "tui.viewport.pageUp")
        || kb.matches(id, "tui.viewport.pageDown")
        || kb.matches(id, "tui.viewport.top")
        || kb.matches(id, "tui.viewport.follow")
        || kb.matches(id, "app.clipboard.pasteImage")
        || kb.matches(id, "app.input.clear")
        || kb.matches(id, "app.interrupt")
        || kb.matches(id, "app.clear")
        || kb.matches(id, "app.suspend")
        || kb.matches(id, "app.model.select")
        || kb.matches(id, "app.model.cycleForward")
        || kb.matches(id, "app.model.cycleBackward")
        || kb.matches(id, "app.tools.expand")
        || kb.matches(id, "app.subagents.focus")
        || kb.matches(id, "app.editor.external")
        || kb.matches(id, "app.prompt.stash")
        || kb.matches(id, "app.session.new")
        || kb.matches(id, "app.session.resume")
        || kb.matches(id, "app.message.sendNow")
        || kb.matches(id, "app.search.older")
        || kb.matches(id, "app.search.newer")
        || kb.matches(id, "app.message.clearQueue")
        || kb.matches(id, "app.message.navigateOlder")
        || kb.matches(id, "app.message.navigateNewer")
        || kb.matches(id, "app.message.moveEarlier")
        || kb.matches(id, "app.message.moveLater")
        || kb.matches(id, "app.message.followUp")
    {
        return true;
    }
    // The empty-editor arms (`app.exit` is the opening loop's own immediate
    // exit and never reaches the queue).
    if editor.get_text().trim().is_empty()
        && (kb.matches(id, "app.agents.back")
            || kb.matches(id, "app.session.tree")
            || kb.matches(id, "app.session.fork"))
    {
        return true;
    }
    // The editor-context arms: Tab into a picker-command argument (the
    // `/model`/`/mcp` partial), and Down's move-below-prompt dock handoff.
    if kb.matches(id, "tui.input.tab")
        && !editor.is_showing_autocomplete()
        && editor.picker_argument_context().is_some()
    {
        return true;
    }
    kb.matches(id, "tui.editor.cursorDown")
        && !editor.is_showing_autocomplete()
        && !editor.is_history_navigation_active()
        && editor.is_cursor_at_end()
}

#[cfg(test)]
mod opening_echo_claim_tests {
    use std::collections::BTreeMap;

    use super::opening_echo_key_claimed;
    use crate::editor::Editor;
    use crate::keybindings::{KeybindingsConfig, KeybindingsManager};

    fn manager(bindings: &[(&str, &str)]) -> KeybindingsManager {
        let config: KeybindingsConfig = bindings
            .iter()
            .map(|(action, key)| (action.to_string(), vec![key.to_string()]))
            .collect::<BTreeMap<_, _>>();
        KeybindingsManager::with_user_bindings(config)
    }

    fn editor_with(text: &str) -> Editor {
        let mut editor = Editor::new();
        if !text.is_empty() {
            editor.set_text(text);
        }
        editor
    }

    /// Default `left` on the EMPTY editor is `app.agents.back` — the
    /// misroute the gate exists for: the key must queue (its action
    /// runs at the fold), never echo as an editor cursor move.
    #[test]
    fn default_left_on_the_empty_editor_is_claimed() {
        let kb = KeybindingsManager::new();
        let editor = editor_with("");
        assert!(
            opening_echo_key_claimed(&kb, "left", &editor),
            "left on the empty editor is app.agents.back, not an editor motion"
        );
    }

    /// Default `left` with text in the editor stays the editor's cursor
    /// motion (the agents-back guard fires only on the empty editor), so
    /// the gate leaves it to the editor fallback.
    #[test]
    fn default_left_with_text_is_not_claimed() {
        let kb = KeybindingsManager::new();
        let editor = editor_with("draft");
        assert!(
            !opening_echo_key_claimed(&kb, "left", &editor),
            "left with text is the editor's cursor motion"
        );
    }

    /// The plain typing keys the opening loop echoes (space, backspace,
    /// single chars) stay unclaimed under the default bindings.
    #[test]
    fn plain_editor_keys_stay_unclaimed_by_default() {
        let kb = KeybindingsManager::new();
        for (id, text) in [
            ("space", ""),
            ("backspace", ""),
            ("delete", ""),
            ("right", ""),
            ("a", ""),
            ("z", "draft"),
        ] {
            let editor = editor_with(text);
            assert!(
                !opening_echo_key_claimed(&kb, id, &editor),
                "{id} must stay the editor fallback under default bindings"
            );
        }
    }

    /// A user-bound single-char action is claimed (the reviewer's
    /// user-binding case): the bound action routes through the post-open
    /// dispatch, never into the editor as text.
    #[test]
    fn a_user_bound_single_char_action_is_claimed() {
        let kb = manager(&[("app.session.new", "a")]);
        let editor = editor_with("");
        assert!(
            opening_echo_key_claimed(&kb, "a", &editor),
            "a user-bound 'a' must run its action, not type an 'a'"
        );
    }

    /// A user-bound space action is claimed the same way; with the
    /// binding moved off space, space returns to the editor fallback.
    #[test]
    fn a_user_bound_space_action_is_claimed() {
        let kb = manager(&[("app.session.new", "space")]);
        let editor = editor_with("");
        assert!(
            opening_echo_key_claimed(&kb, "space", &editor),
            "a user-bound space must run its action"
        );
        let unbound = manager(&[("app.session.new", "f9")]);
        assert!(!opening_echo_key_claimed(&unbound, "space", &editor));
    }

    /// `app.agents.back` rebound off `left` returns `left` to the editor
    /// fallback (the gate reads the EFFECTIVE keymap, not the default).
    #[test]
    fn a_rebound_agents_back_frees_left_for_the_editor() {
        let kb = manager(&[("app.agents.back", "alt+left")]);
        let editor = editor_with("");
        assert!(
            !opening_echo_key_claimed(&kb, "left", &editor),
            "with agents.back rebound, left is the editor's cursor motion"
        );
    }

    /// The always-fire app arms are claimed regardless of the editor's
    /// text: the escape ladder, the interrupt family, suspend, the
    /// model picker, the stash, the session commands, and the queue
    /// browse keys.
    #[test]
    fn the_always_fire_arms_are_claimed() {
        let kb = KeybindingsManager::new();
        let editor = editor_with("draft");
        for id in [
            "escape",
            "ctrl+c",
            "ctrl+z",
            "ctrl+l",
            "alt+m",
            "ctrl+o",
            "alt+a",
            "ctrl+g",
            "ctrl+s",
            "alt+up",
            "alt+down",
            "alt+enter",
            "ctrl+enter",
            "ctrl+alt+x",
        ] {
            assert!(
                opening_echo_key_claimed(&kb, id, &editor),
                "{id} is an always-fire app arm and must be claimed"
            );
        }
    }

    /// The empty-editor session arms (`app.session.tree`, `app.session.fork`
    /// user-bound here — they carry no default key) are claimed only on
    /// the empty editor.
    #[test]
    fn the_empty_editor_session_arms_claim_only_when_empty() {
        let kb = manager(&[("app.session.tree", "ctrl+t")]);
        assert!(opening_echo_key_claimed(&kb, "ctrl+t", &editor_with("")));
        assert!(!opening_echo_key_claimed(
            &kb,
            "ctrl+t",
            &editor_with("draft")
        ));
    }
}

#[cfg(test)]
mod ctrl_c_busy_clear_tests {
    use super::busy_press_clears_draft;

    /// Ctrl+C on running work with a genuine draft clears the box instead of
    /// interrupting (the behavior the branch adds).
    #[test]
    fn a_draft_on_running_work_clears_instead_of_interrupting() {
        assert!(busy_press_clears_draft(
            false, /* interrupt */
            false, /* browsing */
            false, /* has_stashed_draft */
            "half-typed"
        ));
    }

    /// An empty box on running work interrupts (the box has nothing to clear),
    /// so the press reaches the interrupt ladder.
    #[test]
    fn an_empty_box_on_running_work_interrupts() {
        assert!(!busy_press_clears_draft(false, false, false, ""));
    }

    /// Browsing the queue, or holding a stashed draft, shows a browsed message
    /// rather than the user's own draft: the press interrupts, never clearing
    /// the browsed text or desyncing the queue selection.
    #[test]
    fn a_queue_browse_or_stashed_draft_interrupts() {
        assert!(!busy_press_clears_draft(
            false,
            true, /* browsing */
            false,
            "browsed message"
        ));
        assert!(!busy_press_clears_draft(
            false,
            false,
            true, /* has_stashed_draft */
            "browsed message"
        ));
    }

    /// A user-bound pure `app.interrupt` key always interrupts, even with a
    /// draft in the box: it is the escape hatch for the old behavior.
    #[test]
    fn a_pure_interrupt_binding_always_interrupts() {
        assert!(!busy_press_clears_draft(
            true, /* interrupt */
            false,
            false,
            "half-typed"
        ));
    }
}
