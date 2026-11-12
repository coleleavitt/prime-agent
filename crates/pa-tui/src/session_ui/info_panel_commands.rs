//! The read-only info-panel commands that fetch their content: `/session`, `/context`,
//! `/system-prompt`, and `/logs`.
use super::{
    AgentView,
    DaemonCommand,
    Duration,
    InfoContent,
    Map,
    SessionUi,
    UI_REQUEST_TIMEOUT_MS,
    Value,
    info_commands,
    terminal_columns,
};

impl SessionUi {
    /// One bounded daemon read for an info panel; a failure lands as the error row.
    async fn info_panel_read(
        &mut self,
        command: DaemonCommand,
        view: &mut AgentView,
    ) -> Option<Value> {
        match self
            .bounded_request(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), command)
            .await
        {
            Ok(data) => Some(data),
            Err(error) => {
                self.error_row(&format!("{error:#}"), view);
                None
            }
        }
    }

    /// `/session`: the session's stats.
    pub(super) async fn show_session_info(&mut self, view: &mut AgentView) {
        let command = DaemonCommand::GetSessionStats {
            id: None,
            active_session_id: self.active_session_id.clone(),
            rest: Map::default(),
        };
        let Some(stats) = self.info_panel_read(command, view).await else {
            return;
        };
        let name = self.session_name.clone();
        self.open_info_panel(
            view,
            None,
            InfoContent::Rows(info_commands::session_info_rows(&stats, name.as_deref())),
        );
        self.track_menu_opened("session", "command");
    }

    /// `/context` (and its `/usage` alias): the agent tree with token/cost columns and context
    /// utilization. The optional `all` argument is a deliberate TS delta (TS takes none): past
    /// the row budget the default view collapses to the highest-usage agents; `all` renders the
    /// whole tree.
    pub(super) async fn show_context_tree(&mut self, text: &str, args: &str, view: &mut AgentView) {
        let scope = match args {
            "" => info_commands::ContextTreeScope::Collapsed,
            "all" => info_commands::ContextTreeScope::EveryAgent,
            _ => {
                view.editor.set_text(text);
                self.error_row("Usage: /context [all]", view);
                return;
            }
        };
        let command = DaemonCommand::GetContextTree {
            id: None,
            active_session_id: self.active_session_id.clone(),
            rest: Map::default(),
        };
        let Some(tree) = self.info_panel_read(command, view).await else {
            return;
        };
        // TS render width: clamp(columns - 2, 60, 120).
        let width = terminal_columns().saturating_sub(2).clamp(60, 120);
        let factory_runs = crate::factory_view::parse_factory_runs(&self.factory_graph);
        self.open_info_panel(
            view,
            None,
            InfoContent::Rows(info_commands::context_tree_rows(
                &tree,
                width,
                scope,
                &factory_runs,
            )),
        );
        self.track_menu_opened("context", "command");
    }

    /// `/system-prompt`: the exact assembled prompt is unbounded, so it renders in the scrollable
    /// info panel instead of flooding the transcript.
    pub(super) async fn show_system_prompt(&mut self, view: &mut AgentView) {
        let command = DaemonCommand::GetSystemPrompt {
            id: None,
            active_session_id: self.active_session_id.clone(),
            rest: Map::default(),
        };
        let Some(data) = self.info_panel_read(command, view).await else {
            return;
        };
        let prompt = data
            .get("systemPrompt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut rows = info_commands::system_prompt_header_rows(prompt);
        rows.push(Vec::new());
        rows.extend(info_commands::system_prompt_body_rows(prompt));
        // Daemons that send the session context separately
        // (the prompt no longer carries it).
        if let Some(context) = data
            .get("sessionContext")
            .and_then(Value::as_str)
            .filter(|context| !context.is_empty())
        {
            rows.push(Vec::new());
            rows.extend(info_commands::session_context_header_rows(context));
            rows.push(Vec::new());
            rows.extend(info_commands::system_prompt_body_rows(context));
        }
        self.open_info_panel(view, None, InfoContent::Rows(rows));
        self.track_menu_opened("system-prompt", "command");
    }

    /// `/logs`: a client-side read of the logs directory (the daemon writes it, this client lists
    /// it).
    pub(super) fn show_logs(&mut self, view: &mut AgentView) {
        let Some(agent_dir) = pa_types::platform::agent_dir() else {
            self.error_row(
                "home directory not found: set HOME (or USERPROFILE on Windows)",
                view,
            );
            return;
        };
        self.open_info_panel(
            view,
            None,
            InfoContent::Rows(info_commands::logs_rows(&agent_dir.join("logs"))),
        );
        self.track_menu_opened("logs", "command");
    }
}
