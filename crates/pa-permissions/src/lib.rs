//! Claude Code's permission system for Prime Agent: `permissions.allow`,
//! `ask` and `deny` rule lists in Claude Code's syntax (`Bash(git:*)`,
//! `Read(./secrets/**)`, `Edit(src/**)`, `WebFetch(domain:example.com)`,
//! `mcp__server__tool`), permission modes (`default`, `acceptEdits`,
//! `plan`, `bypassPermissions`, `dontAsk`), `additionalDirectories`, the
//! `/permissions` command, a mode indicator and cycle for the TUI's plan
//! key, and "always allow" answers that persist an allow rule.
//!
//! The rules govern what reaches the host: the kernel's `bash()` (as
//! `Bash`, compound commands checked part by part), the bundled `edit`
//! skill's `file.edit` request (as `Edit`), `web.*` requests (as `WebFetch`
//! and `WebSearch`), MCP calls (as `mcp__<server>__<tool>`), and the
//! model's tool calls by name. Python the kernel runs directly is not
//! gated. Without settings nothing changes: an action no rule covers runs
//! unless the user settings choose `"fallback": "ask"`. See `README.md`.

pub mod actions;
pub mod bash;
pub mod command;
pub mod decide;
pub mod mode;
pub mod paths;
pub mod rule;
pub mod settings;
pub mod shell;
pub mod state;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use pa_core::features::{
    CONFIRMATION_TIMEOUT,
    ConfirmationAnswer,
    FeatureCommandOutcome,
    FeatureFuture,
    FeatureVerdict,
    HostRequestObservation,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    confirm_offering,
    publish_feature_status,
};
use pa_core::kernel::shared::HostRequestHandlers;

use crate::decide::{Action, Decision};
use crate::rule::RuleBehavior;
use crate::settings::SourcePaths;
use crate::state::SessionState;

/// The feature's name (`SessionFeature::name`).
pub const FEATURE_NAME: &str = "permissions";

/// How a session's sources and workspace trust are found.
pub type SourceResolver = Arc<dyn Fn(&SessionFeatureContext) -> (SourcePaths, bool) + Send + Sync>;

/// The sources of a session on this machine, and its workspace trust.
#[must_use]
pub fn resolve_sources(context: &SessionFeatureContext) -> (SourcePaths, bool) {
    let paths = SourcePaths::resolve(&context.cwd, &context.agent_dir);
    let trusted = pa_core::settings::SettingsManager::create(&context.cwd, &context.agent_dir)
        .project_scope_trusted();
    (paths, trusted)
}

/// The permissions feature: one instance serves every session.
pub struct PermissionsFeature {
    resolver: SourceResolver,
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionState>>>>,
    /// Each session's decision counts (the `permissions_session` event).
    tallies: Arc<Mutex<HashMap<String, Tally>>>,
}

/// The adoption event a governed session sends when it ends.
pub const PERMISSIONS_SESSION_EVENT: &str = "permissions_session";

/// One session's decision counts.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    allowed: u64,
    asked: u64,
    denied: u64,
    remembered: u64,
}

fn count(tallies: &Mutex<HashMap<String, Tally>>, session_id: &str, update: fn(&mut Tally)) {
    update(
        tallies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session_id.to_string())
            .or_default(),
    );
}

impl Default for PermissionsFeature {
    fn default() -> Self {
        Self::with_resolver(Arc::new(resolve_sources))
    }
}

impl PermissionsFeature {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A feature whose sessions read the sources `resolver` names (tests
    /// point it at a temporary tree).
    #[must_use]
    pub fn with_resolver(resolver: SourceResolver) -> Self {
        PermissionsFeature {
            resolver,
            sessions: Mutex::default(),
            tallies: Arc::default(),
        }
    }

    /// The session's state, loaded on first use.
    fn session(&self, context: &SessionFeatureContext) -> Arc<Mutex<SessionState>> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            sessions
                .entry(context.session_id.clone())
                .or_insert_with(|| {
                    let (paths, trusted) = (self.resolver)(context);
                    let state = SessionState::new(paths, trusted);
                    for problem in &state.settings.problems {
                        tracing::warn!(target: "pa_permissions", "{problem}");
                    }
                    Arc::new(Mutex::new(state))
                }),
        )
    }

    /// Publish the session's mode status.
    fn publish(context: &SessionFeatureContext, state: &SessionState) {
        publish_feature_status(&context.session_id, state.status());
    }

    /// Decide `action` for the session and settle it: a refusal blocks; an
    /// ask that can be remembered is put to the user here (so an "always
    /// allow" answer can add its rule); any other ask goes to the
    /// session's confirmation with the other features' asks.
    fn verdict(
        &self,
        context: &Arc<SessionFeatureContext>,
        action: Action,
    ) -> FeatureFuture<FeatureVerdict> {
        let session = self.session(context);
        let decision = {
            let mut state = session.lock().unwrap_or_else(PoisonError::into_inner);
            // A settings change that moves the mode or the cycle republishes
            // the status.
            if state.refresh() {
                Self::publish(context, &state);
            }
            state.policy().decide(&action)
        };
        let session_id = context.session_id.as_str();
        match decision {
            Decision::Allow => {
                count(&self.tallies, session_id, |tally| tally.allowed += 1);
                Box::pin(async { FeatureVerdict::Allow })
            }
            Decision::Deny { reason } => {
                count(&self.tallies, session_id, |tally| tally.denied += 1);
                Box::pin(async move { FeatureVerdict::Block(reason) })
            }
            Decision::Ask {
                reason,
                remember: None,
            } => {
                count(&self.tallies, session_id, |tally| tally.asked += 1);
                Box::pin(async move { FeatureVerdict::Ask(reason) })
            }
            Decision::Ask {
                reason,
                remember: Some(rule),
            } => {
                count(&self.tallies, session_id, |tally| tally.asked += 1);
                let tallies = Arc::clone(&self.tallies);
                let context = Arc::clone(context);
                Box::pin(async move {
                    let described = actions::describe(&action, &context.cwd);
                    let answer = confirm_offering(
                        &context.session_id,
                        &described,
                        &reason,
                        Some(&rule),
                        CONFIRMATION_TIMEOUT,
                    )
                    .await;
                    match answer {
                        ConfirmationAnswer::Allowed => FeatureVerdict::Allow,
                        ConfirmationAnswer::AllowedAlways => {
                            count(&tallies, &context.session_id, |tally| tally.remembered += 1);
                            let mut state = session.lock().unwrap_or_else(PoisonError::into_inner);
                            let saved = state.add_rule(RuleBehavior::Allow, &rule);
                            tracing::info!(target: "pa_permissions", ?saved, rule, "always allow");
                            FeatureVerdict::Allow
                        }
                        ConfirmationAnswer::Denied => {
                            FeatureVerdict::Block(format!("{reason} (the user refused it)"))
                        }
                        ConfirmationAnswer::Unavailable => FeatureVerdict::Block(format!(
                            "{reason} (it needs the user's confirmation, and no client can ask \
                             for it)"
                        )),
                        ConfirmationAnswer::TimedOut(after) => FeatureVerdict::Block(format!(
                            "{reason} (the user did not answer within {}s)",
                            after.as_secs()
                        )),
                    }
                })
            }
        }
    }
}

/// The `permissions_session` properties, or `None` for a session the
/// feature never governed (no rules, the default mode, nothing asked or
/// refused).
fn session_properties(state: &SessionState, tally: Tally) -> Option<pa_telemetry::Properties> {
    let mut rules = [0u64; 3];
    for rule in state.settings.rules.iter().chain(&state.session_rules) {
        let slot = match rule.behavior {
            RuleBehavior::Allow => 0,
            RuleBehavior::Ask => 1,
            RuleBehavior::Deny => 2,
        };
        rules[slot] += 1;
    }
    let governed = rules.iter().any(|count| *count > 0)
        || state.mode() != mode::PermissionMode::Default
        || tally.asked + tally.denied + tally.remembered > 0;
    if !governed {
        return None;
    }
    let mut properties = pa_telemetry::Properties::new();
    properties.set("mode", state.mode().as_str().into());
    for (key, value) in [
        ("allow_rules", rules[0]),
        ("ask_rules", rules[1]),
        ("deny_rules", rules[2]),
        ("allowed_count", tally.allowed),
        ("asked_count", tally.asked),
        ("denied_count", tally.denied),
        ("remembered_count", tally.remembered),
    ] {
        properties.set(key, value.into());
    }
    Some(properties)
}

impl SessionFeature for PermissionsFeature {
    fn name(&self) -> &'static str {
        FEATURE_NAME
    }

    fn register_host_handlers(
        &self,
        _context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        // The gate reviews `file.edit` / `file.read` before this answers:
        // reaching the handler means the action is allowed.
        for request_type in [actions::FILE_EDIT_REQUEST, actions::FILE_READ_REQUEST] {
            handlers.register(
                request_type,
                Arc::new(|_payload| Box::pin(async { Ok(serde_json::json!({ "allowed": true })) })),
            );
        }
    }

    fn on_session_start(
        &self,
        context: &Arc<SessionFeatureContext>,
        _history: &[pa_agent::types::AgentMessage],
    ) {
        let session = self.session(context);
        let state = session.lock().unwrap_or_else(PoisonError::into_inner);
        if state.cycle().is_some() || state.mode() != mode::PermissionMode::Default {
            Self::publish(context, &state);
        }
    }

    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, _reason: &str) {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&context.session_id);
        let tally = self
            .tallies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&context.session_id)
            .unwrap_or_default();
        if let (Some(session), Some(telemetry)) = (session, &context.telemetry) {
            let state = session.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(properties) = session_properties(&state, tally) {
                telemetry.track(PERMISSIONS_SESSION_EVENT, &properties);
            }
        }
    }

    fn review_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let anchors = self
            .session(context)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paths
            .anchors();
        self.verdict(context, actions::from_tool_call(call, &anchors))
    }

    fn review_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let anchors = self
            .session(context)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paths
            .anchors();
        match actions::from_host_request(request, &anchors) {
            Some(action) => self.verdict(context, action),
            None => Box::pin(async { FeatureVerdict::Allow }),
        }
    }

    fn slash_commands(&self) -> Vec<pa_types::slash_commands::BuiltinSlashCommand> {
        vec![pa_types::slash_commands::BuiltinSlashCommand {
            name: command::COMMAND,
            description: "List permission rules and their sources, switch the permission mode, \
                          add or remove a rule",
            execution: pa_types::slash_commands::SlashCommandExecution::Session,
            argument_hint: Some(
                "[list | mode <mode> | allow|ask|deny <rule> | remove <rule> | check <action>]",
            ),
            aliases: &[],
            takes_argument: true,
        }]
    }

    fn execute_slash_command(
        &self,
        context: &Arc<SessionFeatureContext>,
        name: &str,
        args: &str,
    ) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
        if name != command::COMMAND {
            return None;
        }
        let session = self.session(context);
        let outcome = {
            let mut state = session.lock().unwrap_or_else(PoisonError::into_inner);
            let outcome = command::run(&mut state, args);
            if let Ok((_, true)) = &outcome {
                Self::publish(context, &state);
            }
            outcome
        };
        Some(Box::pin(async move {
            outcome.map(|(text, _)| FeatureCommandOutcome {
                text,
                completion: None,
            })
        }))
    }
}

#[cfg(test)]
mod tests;
