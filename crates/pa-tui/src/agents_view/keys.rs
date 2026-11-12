//! The list's key dispatch: the overlays and composers that own every key first, then the list
//! actions through the effective keybindings in TS dispatch order (a user override moves both
//! the handler and the hint — the same contract as the session view, #184), then the search
//! text.
use super::delete::PendingDelete;
use super::{
    AgentsViewMode,
    Composer,
    OpenedRow,
    PathBuf,
    RowKind,
    ScopeRoot,
    SelectionEdge,
    SelectionKey,
    SessionSelection,
};

/// One list action, bound to a keybinding id by [`LIST_KEYS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKey {
    ExitPress,
    DismissNotice,
    Preview,
    BranchFilter,
    ToggleScope,
    WorktreeFilter,
    Rename,
    Delete,
    Reply,
    NewSession,
    Program,
    Expand,
    Open,
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Back,
    InputClear,
    Exit,
    DeleteChar,
    DeleteToLineStart,
    DeleteWord,
}

impl ListKey {
    /// The actions that apply only with an empty search; with a query the key is search input.
    fn needs_empty_query(self) -> bool {
        match self {
            Self::DismissNotice
            | Self::ToggleScope
            | Self::WorktreeFilter
            | Self::Rename
            | Self::Delete
            | Self::Reply
            | Self::Program
            | Self::Expand
            | Self::Back
            | Self::Exit => true,
            Self::ExitPress
            | Self::Preview
            | Self::BranchFilter
            | Self::NewSession
            | Self::Open
            | Self::Up
            | Self::Down
            | Self::PageUp
            | Self::PageDown
            | Self::Top
            | Self::Bottom
            | Self::InputClear
            | Self::DeleteChar
            | Self::DeleteToLineStart
            | Self::DeleteWord => false,
        }
    }
}

/// The list actions in TS dispatch order; a declining action falls through to the next binding.
const LIST_KEYS: &[(&str, ListKey)] = &[
    // `app.clear` (default ctrl+c): the first press arms the exit hint, a second press while
    // armed exits.
    ("app.clear", ListKey::ExitPress),
    ("tui.select.cancel", ListKey::DismissNotice),
    // `app.agents.preview` (default ctrl+v): the selected session's conversation in place of
    // the list (Claude Code's resume-picker preview).
    ("app.agents.preview", ListKey::Preview),
    // `app.agents.branchFilter` (default ctrl+b): the saved sessions of this repository's
    // current branch only (Claude Code's resume-picker branch filter).
    ("app.agents.branchFilter", ListKey::BranchFilter),
    // `app.agents.toggleScope` (default ctrl+a, Claude Code's key, and ctrl+f, upstream
    // #826's): flip the saved catalog between every project and the current project.
    ("app.agents.toggleScope", ListKey::ToggleScope),
    // `app.agents.worktreeFilter` (default ctrl+w, several worktrees only; otherwise the
    // editor's word delete): the current project's worktree or all of them.
    ("app.agents.worktreeFilter", ListKey::WorktreeFilter),
    // `app.agents.rename` (default ctrl+r, before the delete arm): enter the rename composer.
    ("app.agents.rename", ListKey::Rename),
    // `app.agents.delete` (default ctrl+x): the first press arms the confirm (the hint reads
    // "stop" for live work, "delete" otherwise), the second press on the same row executes,
    // any other key clears the arm.
    ("app.agents.delete", ListKey::Delete),
    // `app.agents.reply` (default space): arm the reply over the selected row; the same target
    // disarms, and a space with a query is search text.
    ("app.agents.reply", ListKey::Reply),
    ("app.agents.new", ListKey::NewSession),
    // `app.agents.program` (default ctrl+o): show or hide the selected row's target spawn
    // program.
    ("app.agents.program", ListKey::Program),
    // `app.agents.expand` (default alt+right): toggle the selected parent's list when it has
    // children.
    ("app.agents.expand", ListKey::Expand),
    // `app.agents.open` (right) and the editor submit (enter) both open the selection; the
    // summary row toggles its list instead.
    ("app.agents.open", ListKey::Open),
    ("tui.select.confirm", ListKey::Open),
    ("tui.select.up", ListKey::Up),
    ("tui.select.down", ListKey::Down),
    ("tui.select.pageUp", ListKey::PageUp),
    ("tui.select.pageDown", ListKey::PageDown),
    // The list-edge jump keys (operator directive, no TS counterpart): one-row up/down is too
    // slow on a large forest, so home/end and their ctrl/super variants select the first/last
    // row (the search editor keeps `ctrl+a`/`ctrl+e` for its own line ends).
    ("tui.select.top", ListKey::Top),
    ("tui.select.bottom", ListKey::Bottom),
    // The scoped view's parent key (default left): it hands the terminal back to the scope
    // root's session and pops the scope; the global view has no hierarchy parent and consumes
    // the key without opening a chat.
    ("app.agents.back", ListKey::Back),
    // `app.input.clear` (default escape): clear the search; scoped, reopen the last-opened
    // session (the scope root in this flow) without touching the scope frame; otherwise exit.
    ("app.input.clear", ListKey::InputClear),
    // `app.exit` (default ctrl+d): leave the view without opening a session.
    ("app.exit", ListKey::Exit),
    // Editor text keys: backspace deletes the last character, ctrl+u clears the line, and
    // TS `Editor.handleInput`'s `deleteWordBackward` (ctrl+w / alt+backspace).
    ("tui.editor.deleteCharBackward", ListKey::DeleteChar),
    ("tui.editor.deleteToLineStart", ListKey::DeleteToLineStart),
    ("tui.editor.deleteWordBackward", ListKey::DeleteWord),
];

/// The state the key preamble cleared that the list actions still read.
struct KeyPress {
    /// The exit hint was armed before this key.
    was_exit_armed: bool,
    /// The stop-or-delete confirm armed before this key (taken by the delete action).
    was_delete_armed: Option<PendingDelete>,
}

impl AgentsViewMode {
    /// Handle one key id: the bash view, the notice panel, the preview and an armed composer
    /// own the key first; then the list actions, then the search text.
    pub(super) fn handle_key(&mut self, key: &str) {
        // A sticky line clears on any keypress (the transient lines ride their own
        // expiry).
        self.clear_sticky_status();
        if self.shell_view.is_some() {
            self.handle_shell_view_key(key);
            return;
        }
        let was_exit_armed = self.exit_armed;
        // The notice panel: any key closes it (the refusal's ways out stay copy-pasteable while
        // it is up), except the exit key, which falls through so the double-press exit convention
        // keeps working with the panel open.
        if self.notice.is_some() && !self.keybindings.matches(key, "app.clear") {
            self.notice = None;
            return;
        }
        self.notice = None;
        // Any other key clears the exit hint and the stop-or-delete confirm.
        self.exit_armed = false;
        // The open preview owns every key but the exit key.
        if self.handle_preview_key(key) {
            return;
        }
        let was_delete_armed = self.pending_delete.take();
        // The armed composer owns every key before the app-level handlers; the draft
        // comes out owned, and an unarmed Search parks nothing.
        match std::mem::replace(&mut self.composer, Composer::Search) {
            Composer::Rename(rename) => {
                self.handle_rename_key(rename, key);
                return;
            }
            Composer::Reply(reply) => {
                self.handle_reply_key(reply, was_delete_armed, key);
                return;
            }
            Composer::Search => {}
        }
        let mut press = KeyPress {
            was_exit_armed,
            was_delete_armed,
        };
        let has_query = !self.query.is_empty();
        for &(binding, action) in LIST_KEYS {
            if (has_query && action.needs_empty_query()) || !self.keybindings.matches(key, binding)
            {
                continue;
            }
            if self.run_list_key(action, key, &mut press) {
                return;
            }
        }
        // The printable decode the editor uses (`decode_printable`):
        // the space arrives as the `space` key id (TS parseKey maps the
        // raw space there), and the shift+letter ids decode to their
        // characters.
        if let Some(text) = crate::editor::decode_printable(key) {
            let was_empty = self.query.is_empty();
            self.query.push_str(&text);
            self.query_changed(was_empty);
        }
    }

    /// One bound list action; `false` when its guard declines the key.
    fn run_list_key(&mut self, action: ListKey, key: &str, press: &mut KeyPress) -> bool {
        match action {
            // One handled Ctrl+C press: the force-quit guard disarms once the whole observed pair
            // was handled; an exit re-arms from the run loop's break.
            ListKey::ExitPress => {
                if key == "ctrl+c" {
                    self.exit_guard.note_ctrl_c_handled();
                }
                if press.was_exit_armed {
                    self.running = false;
                } else {
                    self.exit_armed = true;
                }
            }
            // Esc dismisses the incident notice while it is the only thing to cancel (an empty
            // search prompt). An armed delete confirmation is the more dangerous state: Esc
            // cancels it (the take() above) and keeps the notice; without a visible notice, Esc
            // keeps its back/exit meaning.
            ListKey::DismissNotice => {
                return press.was_delete_armed.is_none() && self.dismiss_incident_notice();
            }
            ListKey::Preview => self.open_preview(),
            ListKey::BranchFilter => self.toggle_branch_filter(),
            ListKey::ToggleScope => self.toggle_saved_scope(),
            ListKey::WorktreeFilter => return self.toggle_worktree_filter(),
            ListKey::Rename => self.enter_rename_mode(),
            // Stopping or deleting a tailnet peer happens on its machine (TS #2516); the view
            // stays read-only context for it.
            ListKey::Delete => {
                if !self.guard_remote_row("stop or delete") {
                    self.confirm_delete_for_selected(press.was_delete_armed.take());
                }
            }
            ListKey::Reply => self.toggle_reply(),
            ListKey::NewSession => self.open_new_session(),
            ListKey::Program => self.cycle_program_for_selected(),
            ListKey::Expand => {
                let selected = self.rows.get(self.selected).cloned();
                if let Some(row) = selected {
                    if row.kind == RowKind::SubagentSummary || row.descendant_count > 0 {
                        self.toggle_subagent_list(&row);
                    }
                }
            }
            ListKey::Open => self.open_selected(),
            ListKey::Up => self.move_selection(-1),
            ListKey::Down => {
                self.saved_page_requested = true;
                self.move_selection(1);
            }
            ListKey::PageUp => self.move_selection(-(self.page_step() as isize)),
            ListKey::PageDown => {
                self.saved_page_requested = true;
                self.move_selection(self.page_step() as isize);
            }
            ListKey::Top => self.move_selection_to(SelectionEdge::First),
            ListKey::Bottom => {
                self.saved_page_requested = true;
                self.move_selection_to(SelectionEdge::Last);
            }
            ListKey::Back => {
                if self.scope_active {
                    self.open_scope_root(true);
                }
            }
            ListKey::InputClear => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.query_changed(false);
                } else if self.scope_active {
                    self.open_scope_root(false);
                } else {
                    self.running = false;
                }
            }
            ListKey::Exit => self.running = false,
            // A no-op edit on an empty query changes nothing: the re-arm's expensive retry must
            // not fire behind it.
            ListKey::DeleteChar => {
                if self.query.pop().is_some() {
                    self.query_changed(false);
                }
            }
            ListKey::DeleteToLineStart => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.query_changed(false);
                }
            }
            ListKey::DeleteWord => {
                if truncate_trailing_word(&mut self.query) {
                    self.query_changed(false);
                }
            }
        }
        true
    }

    /// TS `app.agents.new` (default ctrl+n): start a session; a plain "n" is search text like
    /// any other character. Scoped (the operator's 2026-09-28 directive, a deliberate TS
    /// divergence - TS always starts a root session): the session starts under the scope root,
    /// one level below it and in its directory, so it lists in this view and the agents-back
    /// return lands here. A root with no session file (a `--no-session` root) has nothing to
    /// bind and starts a root session.
    fn open_new_session(&mut self) {
        let (selection, rlm_depth, cwd) = match self.scope_root.clone() {
            Some(ScopeRoot {
                child_depth,
                session_file: Some(file),
                cwd,
            }) => (
                SessionSelection::NewChild {
                    parent_session_file: PathBuf::from(file),
                    rlm_depth: child_depth,
                },
                Some(child_depth),
                cwd,
            ),
            Some(ScopeRoot {
                session_file: None, ..
            })
            | None => (SessionSelection::New, None, None),
        };
        self.opened = Some(OpenedRow {
            selection,
            expanded_ancestors: Vec::new(),
            selected_row_identity: String::new(),
            selected_key: SelectionKey::default(),
            rlm_depth,
            has_children: false,
            status_message: None,
            cwd,
            panel: None,
        });
        self.running = false;
    }
}

/// Delete the query's trailing word run plus the whitespace before it
/// (TS `Editor.deleteWordBackwards` with the caret at the text's end —
/// this view's query is append-only, so the caret always sits there):
/// the search input's punctuation-aware walk, so a dotted query
/// ("error.rs") loses its trailing word run and keeps "error." — a
/// whitespace-only scan would take the whole dotted word. Returns
/// whether anything was deleted: a no-op edit re-arms nothing.
fn truncate_trailing_word(query: &mut String) -> bool {
    let chars: Vec<char> = query.chars().collect();
    let start = crate::search_input::word_walk_start(query);
    if start == chars.len() {
        return false;
    }
    *query = chars[..start].iter().collect();
    true
}
