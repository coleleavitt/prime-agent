//! The slash-command ladder (`handle_slash`), the builtin client-command dispatch tail, the
//! command-catalog refresh/fold, and the shared connection-state read.
use super::{
    AgentView,
    AuthSelectorKind,
    CommandCatalogUpdate,
    DaemonCommand,
    DockFold,
    Duration,
    ForkLaunch,
    InfoContent,
    Map,
    RebuildKind,
    Result,
    SessionUi,
    SlashCommandExecution,
    SlashCommandRegistry,
    StatusKind,
    SubmitBehavior,
    UI_REQUEST_TIMEOUT_MS,
    Value,
    create_session,
    effort_picker,
    info_commands,
};

impl SessionUi {
    /// Slash-command dispatch: builtin client commands without a UI report unavailability, session
    /// commands forward to the session, unknown commands get the suggestion error or pass through
    /// as a prompt; `behavior` passes through, so a slash-prefixed follow-up keeps its lane.
    pub(super) async fn handle_slash(
        &mut self,
        text: &str,
        behavior: SubmitBehavior,
        view: &mut AgentView,
    ) -> Result<()> {
        let registry = SlashCommandRegistry::builtin();
        let (name, args) = pa_types::slash_commands::parse_slash_command(text)
            .unwrap_or_else(|| (String::new(), String::new()));

        // A bare `/skill:<name>` never sends: the daemon's admission seam would hand the model the
        // expanded protocol as its only user message with no task attached.
        if name.starts_with("skill:") && args.trim().is_empty() {
            // The draft restores into the argument position: the next keystrokes
            // become the request.
            view.editor.set_text(&format!("{text} "));
            self.note(
                "add your request after the skill, e.g. /skill:prime-agent-release make a release of PR #2731",
                view,
            );
            return Ok(());
        }

        // Client-local commands this build implements (not TS builtins).
        match name.as_str() {
            "help" => {
                self.note(
                    "/help           this list\n/list           live sessions\n/search <text>  find in the transcript (F3 older, Shift+F3 newer, Esc clears)\n/switch <n|id>  switch to a session from /list\n/switch         switch this session's model, keeping the saved default\n/new            start a new session\n/exit           detach and exit",
                    view,
                );
                return Ok(());
            }
            "list" => {
                self.refresh_list(view).await?;
                return Ok(());
            }
            "search" => {
                self.start_search(&args, view);
                return Ok(());
            }
            // `/switch <n|id>` moves to another live session; bare `/switch` falls through to the
            // registry, which opens the session-only model picker (upstream #840).
            "switch" if !args.is_empty() => {
                self.track_client_adoption(crate::interactive::ClientAdoption::SessionSwitch);
                self.switch_to(&args, view).await?;
                return Ok(());
            }
            _ => {}
        }

        let Some(resolved) = registry.parse(text) else {
            // Oversized names are prompts; close typos get the suggestion error,
            // everything else passes through to the model.
            if name.chars().count() > 64 {
                return self.send_prompt(text, behavior, view);
            }
            let candidates = registry.suggestion_candidates();
            return match pa_types::slash_commands::find_slash_command_suggestion(&name, &candidates)
            {
                Some(suggestion) => {
                    self.note(
                        &format!("Unknown command: /{name}. Did you mean /{suggestion}?"),
                        view,
                    );
                    Ok(())
                }
                None => self.send_prompt(text, behavior, view),
            };
        };

        // Bare `/harness` (or `/harness list`) opens the selector over the
        // list the session command returns (#1118).
        if resolved.name == "harness" && matches!(resolved.args.trim(), "" | "list") {
            self.track_command_used(resolved.name);
            return self.open_harness_selector(behavior, view);
        }
        // `/effort ultracode [on|off]` switches the session's standing
        // ultracode mode (the `/ultracode` session command), not a level.
        if resolved.name == "effort"
            && let Some(forwarded) = effort_picker::ultracode_session_command(&resolved.args)
        {
            self.track_command_used(resolved.name);
            return self.send_prompt(&forwarded, behavior, view);
        }
        let command = registry
            .get(resolved.name)
            .expect("resolved name is builtin");
        // `agent command used` (TS `captureAgentCommandUsed`): one report
        // per submitted builtin, client and session commands alike, by its
        // canonical name, before the command runs.
        self.track_command_used(resolved.name);
        match command.execution {
            SlashCommandExecution::Session => self.send_prompt(text, behavior, view),
            SlashCommandExecution::Client => {
                self.dispatch_client_command(&resolved, text, view).await
            }
        }
    }

    /// Create a fresh session and rebind the view to it (the `/new` flow,
    /// shared with the `app.session.new` action).
    pub(super) async fn start_new_session(&mut self, view: &mut AgentView) -> Result<()> {
        let id = create_session(&self.client, &self.create_options(), None).await?;
        self.attach_session(&id, DockFold::Fresh).await?;
        // The tray's context usage rode the attach snapshot: the
        // rebuild copies it into the chrome without a stats round-trip.
        self.rebuild_view(view, &RebuildKind::Rebind);
        // A new session starts with no draft and no prompt history (the submitted
        // `/new` drains the draft already).
        view.editor.clear_history();
        view.editor.set_text("");
        self.note(&format!("started session {id}"), view);
        self.track_feature_outcome("new", "completed", None);
        Ok(())
    }

    /// A builtin client command: only the implemented subset runs locally;
    /// the rest report unavailability.
    async fn dispatch_client_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        text: &str,
        view: &mut AgentView,
    ) -> Result<()> {
        if !resolved.args.is_empty()
            && let Some(refusal) = ArgumentRefusal::for_command(resolved.name)
        {
            self.refuse_argument(refusal, resolved, text, view);
            return Ok(());
        }
        match resolved.name {
            // `/clear` stays the no-argument alias of `/new`.
            "new" if resolved.original_name == "clear" && !resolved.args.is_empty() => {
                self.note("Usage: /clear", view);
            }
            "new" => self.handle_new_command(resolved, text, view).await?,
            // `/quit` (and its `/exit` alias) detaches and exits (the session keeps running in the
            // daemon).
            "quit" => {
                self.exit_requested = true;
            }
            "resume" => {
                if resolved.args.is_empty() {
                    self.open_agents_view = true;
                    self.agents_view_resume_picker = true;
                } else {
                    let (selection, lookup) = self.resolve_resume_selector(&resolved.args);
                    self.pending_selection = Some(selection);
                    self.pending_selection_lookup = lookup;
                }
                self.exit_requested = true;
            }
            "model" | "switch" => self.handle_model_command(resolved, view).await?,
            "effort" => self.handle_effort_command(&resolved.args, view).await,
            "rewind" => self.open_rewind_or_tree(view).await?,
            "tree" => {
                self.track_feature_outcome("tree", "initiated", None);
                self.open_tree_selector(view, None).await?;
            }
            "fork" | "clone"
                if self.coordinator_mode
                    && ForkLaunch::from_args(&resolved.args) == Some(ForkLaunch::NewSession) =>
            {
                self.note(super::session_modes::COORDINATOR_FORK_REFUSAL, view);
            }
            "fork" => match ForkLaunch::from_args(&resolved.args) {
                Some(launch) => {
                    self.track_feature_outcome("fork", "initiated", None);
                    if launch == ForkLaunch::ReplaceInPlace {
                        self.track_client_adoption(crate::interactive::ClientAdoption::ForkReplace);
                    }
                    self.fork_launch = launch;
                    self.open_fork_selector(view).await?;
                }
                None => self.note("Usage: /fork [--replace]", view),
            },
            "clone" => match ForkLaunch::from_args(&resolved.args) {
                Some(launch) => {
                    self.track_feature_outcome("clone", "initiated", None);
                    if launch == ForkLaunch::ReplaceInPlace {
                        self.track_client_adoption(
                            crate::interactive::ClientAdoption::CloneReplace,
                        );
                    }
                    self.handle_clone_command(launch, view).await?;
                }
                None => self.note("Usage: /clone [--replace]", view),
            },
            "copy" => self.handle_copy_command(view).await?,
            // `/login` opens the providers selector (the full configuration menu
            // stays unported).
            "login" => {
                self.open_provider_auth(AuthSelectorKind::Login, view)
                    .await?;
                self.track_feature_outcome("login", "initiated", None);
            }
            "logout" => {
                self.open_provider_auth(AuthSelectorKind::Logout, view)
                    .await?;
                self.track_feature_outcome("logout", "initiated", None);
            }
            "import" => {
                let command_text = if resolved.args.is_empty() {
                    "/import".to_string()
                } else {
                    format!("/import {}", resolved.args)
                };
                self.open_import_confirm(&command_text, view);
            }
            "traces" => {
                self.handle_traces_command(resolved, view).await?;
            }
            "nightly" => self.handle_nightly_command(&resolved.args, view),
            "factory" => self.handle_factory_command(&resolved.args, view).await,
            "telemetry" => self.handle_telemetry_command(&resolved.args, view),
            "update" => self.confirm_update(view),
            // The auth flows run in the client process; other management subcommands
            // surface through the `mcp` CLI command instead of the TUI.
            "mcp" => self.handle_mcp_command(resolved, view).await?,
            // `/plugins [search]` opens the `/mcp` view (this client folds the
            // catalog into it); an argument prefills its search field.
            "plugins" => {
                self.open_mcp_view("/plugins", view, "").await?;
                let search = resolved.args.trim();
                if !search.is_empty()
                    && let Some(mcp) = view.mcp_view.as_mut()
                {
                    mcp.paste(search);
                }
            }
            // An explicit `.jsonl` path exports the current branch; anything else
            // exports HTML.
            "export" => {
                self.handle_export_command(resolved, view).await?;
            }
            // The session exports to a temp file and uploads as a secret gist.
            "share" => self.handle_share_command(view).await?,
            // `/hotkeys`: the full keyboard-shortcut reference renders from the EFFECTIVE bindings
            // so user `keybindings.json` overrides show their keys (client-side rows only). The
            // operator's 2026-09-26 directive: the guide renders as the read-only info panel
            // instead of the transcript flood.
            "hotkeys" => {
                self.open_info_panel(
                    view,
                    Some("Hotkeys".to_string()),
                    InfoContent::Markdown(crate::hotkeys::hotkeys_guide(view.editor.keybindings())),
                );
                self.track_menu_opened("hotkeys", "command");
            }
            "session" => self.show_session_info(view).await,
            "context" => self.show_context_tree(text, &resolved.args, view).await,
            "system-prompt" => self.show_system_prompt(view).await,
            "logs" => self.show_logs(view),
            // The shipped CHANGELOG.md entries, newest first; the TS accent `What's
            // New` title is the panel's title.
            "changelog" => {
                self.open_info_panel(
                    view,
                    Some("What's New".to_string()),
                    InfoContent::Markdown(info_commands::changelog_markdown(
                        &Self::changelog_path(),
                    )),
                );
                self.track_menu_opened("changelog", "command");
            }
            "settings" => self.open_settings_menu(view).await,
            // Start a side question without touching the transcript; the pane stays
            // open for follow-ups until esc returns to the main thread.
            "btw" => {
                if resolved.args.is_empty() {
                    self.note_as("Usage: /btw <question>", StatusKind::Warning, view);
                    return Ok(());
                }
                self.start_side_question(&resolved.args, view).await?;
            }
            "name" => {
                let name = resolved.args.trim();
                if name.is_empty() {
                    match &self.session_name {
                        Some(current) => self.note(&format!("Session name: {current}"), view),
                        None => self.note_as("Usage: /name <name>", StatusKind::Warning, view),
                    }
                    return Ok(());
                }
                self.set_session_name(name, view).await;
            }
            "cwd" => {
                self.handle_cwd_command(view, &resolved.args).await;
            }
            "fast" => self.handle_fast_command(view).await,
            "tier" => {
                self.handle_tier_command(view, &resolved.args).await;
            }
            "rlm-max-depth" => {
                self.handle_rlm_max_depth_command(view, &resolved.args)
                    .await;
            }
            "speed" => self.handle_speed_command(&resolved.args, view),
            "reload" => {
                if self.turn_active || view.working.is_some() || self.work_in_flight() {
                    self.note_as(
                        "Wait for the current response to finish before reloading.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                if view.compaction.is_some() {
                    self.note_as(
                        "Wait for compaction to finish before reloading.",
                        StatusKind::Warning,
                        view,
                    );
                    return Ok(());
                }
                self.handle_reload_command(view)?;
            }
            "vim" => self.toggle_vim_mode(view),
            "heartbeats" => self.open_heartbeats_view(view),
            other => {
                self.note(
                    &format!("/{other} is not available in this client yet"),
                    view,
                );
            }
        }
        Ok(())
    }

    /// The usage reply of a client command that takes no argument but got one.
    fn refuse_argument(
        &mut self,
        refusal: ArgumentRefusal,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        text: &str,
        view: &mut AgentView,
    ) {
        let usage = format!("Usage: /{}", resolved.name);
        match refusal {
            ArgumentRefusal::Note => self.note(&usage, view),
            ArgumentRefusal::Error => self.error_row(&usage, view),
            ArgumentRefusal::ErrorRestoringCommand => {
                view.editor
                    .set_text(&format!("/{} {}", resolved.original_name, resolved.args));
                self.error_row(&usage, view);
            }
            ArgumentRefusal::ErrorRestoringText => {
                view.editor.set_text(text);
                self.error_row(&usage, view);
            }
        }
    }

    /// `/new [--name <name>] [prompt]` (and `/clear`): start a fresh session, optionally named,
    /// optionally submitting a first prompt.
    async fn handle_new_command(
        &mut self,
        resolved: &pa_types::slash_commands::ResolvedSlashCommand,
        text: &str,
        view: &mut AgentView,
    ) -> Result<()> {
        // The raw suffix after the typed command name (TS parses
        // `text.slice(4)`): the leading separator and the verbatim
        // prompt survive, where the trimmed `resolved.args` would
        // lose them.
        let options = match crate::new_session_command::parse_new_session_command(
            &text[1 + resolved.original_name.len()..],
        ) {
            Ok(options) => options,
            Err(message) => {
                // The draft the submit drained restores next to the
                // error row (TS `editor.setText(text)` + `showError`).
                view.editor.set_text(text);
                self.error_row(&message, view);
                return Ok(());
            }
        };
        self.start_new_session(view).await?;
        if let Some(name) = options.name
            && !self.set_session_name(&name, view).await
        {
            // TS parity (`handleClearCommand`'s catch): the
            // rename failed after the session exists — the
            // prompt restores into the editor and never submits.
            if let Some(prompt) = options.prompt.as_deref() {
                view.editor.set_text(prompt);
            }
            return Ok(());
        }
        if let Some(prompt) = options.prompt {
            view.editor.add_to_history(&prompt);
            self.record_prompt_history(&prompt);
            self.send_prompt(&prompt, SubmitBehavior::Steer, view)?;
        }
        Ok(())
    }

    /// Rename the active session through the daemon's `SetSessionName` seam —
    /// the one path the `/name` and `/new --name` arms share. On success the
    /// local name, the chrome's chat title, and the `Session name set:` row
    /// update; on failure the error row reports and the caller learns the
    /// rename did not land.
    async fn set_session_name(&mut self, name: &str, view: &mut AgentView) -> bool {
        match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::SetSessionName {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    name: name.to_string(),
                    worker_token: None,
                    rest: Map::default(),
                },
            )
            .await
        {
            Ok(_) => {
                self.session_name = Some(name.to_string());
                self.sync_chat_name(view);
                self.plain_row(&format!("Session name set: {name}"), view);
                true
            }
            Err(error) => {
                self.error_row(&format!("{error:#}"), view);
                false
            }
        }
    }

    /// Report a menu surface opening (`tui menu opened`), fire-and-forget;
    /// `menu` names the surface, `source` how it opened.
    pub(super) fn track_menu_opened(&self, menu: &'static str, source: &'static str) {
        if let Some(telemetry) = self.telemetry.clone() {
            tokio::spawn(async move {
                telemetry.menu_opened(menu, source).await;
            });
        }
    }

    /// Fetch the session's slash-command catalog in the background: the response carries the
    /// `skill:` commands; a fetch races a rebind silently — the epoch drops the stale response.
    pub(crate) fn spawn_command_catalog_refresh(&mut self) {
        self.command_refresh_epoch += 1;
        let epoch = self.command_refresh_epoch;
        // The clear rides the same FIFO channel ahead of the fetch's response, so the old rows drop
        // immediately — a rebind never offers stale cross-session commands.
        let _ = self.command_updates.send(CommandCatalogUpdate {
            epoch,
            skill_commands: Vec::new(),
        });
        let updates = self.command_updates.clone();
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        tokio::spawn(async move {
            let request = DaemonCommand::GetCommands {
                id: None,
                active_session_id,
                rest: Map::default(),
            };
            let fetched = tokio::time::timeout(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                client.request_ok(request),
            )
            .await;
            let skill_commands = match fetched {
                Ok(Ok(data)) => crate::autocomplete::skill_command_entries(&data),
                // The catalog clears on failure.
                Ok(Err(_)) | Err(_) => Vec::new(),
            };
            let _ = updates.send(CommandCatalogUpdate {
                epoch,
                skill_commands,
            });
        });
    }

    /// Fold a landed command-catalog refresh into the session: the `skill:` commands replace the
    /// provider's list, gated by the `enableSkillCommands` setting; a stale epoch never applies.
    pub(crate) fn apply_command_catalog(
        &mut self,
        update: CommandCatalogUpdate,
        view: &mut AgentView,
    ) {
        if update.epoch < self.command_refresh_epoch {
            return;
        }
        // The cache keeps the raw fetch (the toggle re-applies it under the
        // setting's new value); the default applies when no settings seam exists.
        self.skill_commands_cache = update.skill_commands;
        let skills = if self
            .client_settings
            .as_ref()
            .is_none_or(|settings| settings.enable_skill_commands())
        {
            self.skill_commands_cache.clone()
        } else {
            Vec::new()
        };
        view.editor.set_autocomplete_skill_commands(skills);
        self.dirty = true;
    }

    /// The session's connection state (`get_connection_state` carries the connection fields;
    /// `get_state` serves the roster summary instead). `None` surfaces the failure as a note.
    pub(super) async fn connection_state(&mut self, view: &mut AgentView) -> Option<Value> {
        match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetConnectionState {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Map::default(),
                },
            )
            .await
        {
            Ok(data) => {
                // The queue delivery mode rides the state and is cached here (every state
                // read is the single refresh seam).
                if let Some(mode) = data.get("steeringMode").and_then(Value::as_str) {
                    self.steering_mode = mode.to_string();
                }
                Some(data)
            }
            Err(error) => {
                self.note(&format!("{error:#}"), view);
                None
            }
        }
    }
}

/// How a builtin client command that takes no argument answers one: the `Usage: /<name>` row,
/// and what the editor gets back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgumentRefusal {
    /// A muted note; the editor stays as the submit left it.
    Note,
    /// The error row; the editor stays as the submit left it.
    Error,
    /// The error row, with the command and its arguments back in the editor.
    ErrorRestoringCommand,
    /// The error row, with the submitted text back in the editor verbatim.
    ErrorRestoringText,
}

impl ArgumentRefusal {
    /// The refusal of a client command that takes no argument; `None` for the commands that take
    /// one.
    fn for_command(name: &str) -> Option<Self> {
        match name {
            "rewind" | "tree" => Some(Self::Note),
            "fast" => Some(Self::Error),
            "copy" | "login" | "logout" | "share" | "hotkeys" | "settings" | "reload"
            | "heartbeats" => Some(Self::ErrorRestoringCommand),
            "update" | "session" | "system-prompt" | "logs" | "changelog" => {
                Some(Self::ErrorRestoringText)
            }
            _ => None,
        }
    }
}
