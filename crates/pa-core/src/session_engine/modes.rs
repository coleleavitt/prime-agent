//! The session's Claude Code modes ([`super::coordinator`] and
//! [`super::brief_mode`]) wired into one engine: resolved once at session
//! build, then consulted by the host-request gate, the system prompt, the
//! `rlm.*` bridge, `/brief`, and the turn boundary.

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::types::ShouldStopAfterTurnContext;

use super::brief_mode::BriefState;
use super::coordinator::{CoordinatorConfig, NestedMemoryPropagation};
use crate::kernel::shared::{HostRequestGate, HostRequestHandlers};
use crate::settings::experimental::ExperimentalSettings;

/// One session's modes.
#[derive(Clone)]
pub struct SessionModes {
    /// `Some` for a root session in coordinator mode.
    pub coordinator: Option<CoordinatorConfig>,
    /// The session's brief mode (always off below the root).
    pub brief: Arc<BriefState>,
    /// Whether brief mode was on when the session's prompt was built.
    brief_in_prompt: bool,
    /// Where workers' nested instruction files wait for this session.
    nested_memory: Option<Arc<NestedMemoryPropagation>>,
}

impl SessionModes {
    /// Resolve the modes: coordinator mode for a root session that does not
    /// run as an agent definition; brief mode for a root session, from the
    /// branch's newest `/brief` change, else the `briefMode` setting.
    #[must_use]
    pub fn resolve(
        experimental: &ExperimentalSettings,
        rlm_depth: u32,
        runs_agent_definition: bool,
        restored_brief: Option<bool>,
    ) -> Self {
        let coordinator =
            CoordinatorConfig::resolve(experimental, rlm_depth, runs_agent_definition);
        let nested_memory = coordinator
            .as_ref()
            .filter(|config| config.propagate_nested_memory)
            .map(|_| Arc::new(NestedMemoryPropagation::default()));
        let brief_on = rlm_depth == 0 && restored_brief.unwrap_or(experimental.brief_mode);
        Self {
            coordinator,
            brief: Arc::new(BriefState::new(brief_on)),
            brief_in_prompt: brief_on,
            nested_memory,
        }
    }

    /// Install the coordinator's spawn rules on the session's `rlm.*` bridge.
    pub fn wire_rlm(&self, bridge: &super::rlm_host::RlmHostBridge) {
        let Some(config) = &self.coordinator else {
            return;
        };
        bridge.ignore_spawn_model.store(
            config.force_worker_inherit_model,
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Some(nested_memory) = &self.nested_memory {
            let _ = bridge.nested_memory.set(Arc::clone(nested_memory));
        }
    }

    /// The coordinator's gate in front of `inner` (unchanged outside
    /// coordinator mode).
    #[must_use]
    pub fn gate(&self, inner: Option<HostRequestGate>) -> Option<HostRequestGate> {
        match &self.coordinator {
            Some(config) => Some(super::coordinator::coordinator_gate(config.clone(), inner)),
            None => inner,
        }
    }

    /// The coordinator section for the system prompt.
    #[must_use]
    pub fn coordinator_section(
        &self,
        scratchpad: Option<String>,
        has_agent_types: bool,
    ) -> Option<String> {
        self.coordinator.as_ref().map(|config| {
            super::coordinator::coordinator_section(
                config,
                &super::coordinator::CoordinatorPromptFacts {
                    scratchpad,
                    has_agent_types,
                },
            )
        })
    }

    /// Whether the system prompt carries the brief section.
    #[must_use]
    pub fn brief_in_prompt(&self) -> bool {
        self.brief_in_prompt
    }

    /// Register `rlm.send_user_message`'s host request.
    pub fn register_host_handlers(
        &self,
        handlers: &mut HostRequestHandlers,
        cwd: PathBuf,
        rows: &Arc<super::presented_artifact::PresentedArtifacts>,
        session: &Arc<tokio::sync::Mutex<crate::session::manager::SessionManager>>,
    ) {
        super::brief_mode::register_host_handlers(
            handlers,
            super::brief_mode::UserMessageContext {
                cwd,
                state: Arc::clone(&self.brief),
                rows: Arc::clone(rows),
                session: Arc::clone(session),
            },
        );
    }

    /// Publish the session's mode statuses (held until the embedding
    /// registers its sink).
    pub fn publish_statuses(&self, session_id: &str) {
        if self.coordinator.is_some() {
            crate::features::publish_feature_status(
                session_id,
                super::coordinator::coordinator_status(),
            );
        }
        if self.brief.is_on() {
            crate::features::publish_feature_status(
                session_id,
                super::brief_mode::brief_status(true),
            );
        }
    }

    /// What the turn boundary needs (`None` when neither mode can act).
    #[must_use]
    pub fn boundary(&self, host: Arc<dyn super::rlm_host::RlmSubagentHost>) -> ModesBoundary {
        ModesBoundary {
            brief: Arc::clone(&self.brief),
            nested_memory: self
                .nested_memory
                .as_ref()
                .map(|propagation| (Arc::clone(propagation), host)),
        }
    }
}

/// The modes' part of the turn boundary.
#[derive(Clone)]
pub struct ModesBoundary {
    brief: Arc<BriefState>,
    nested_memory: Option<(
        Arc<NestedMemoryPropagation>,
        Arc<dyn super::rlm_host::RlmSubagentHost>,
    )>,
}

impl ModesBoundary {
    /// After a tool batch, add the nested instruction files of the workers
    /// this run heard from (or collected) to the batch's touched paths; at a
    /// natural stop they wait, so the propagation never costs a model call.
    pub async fn propagate_nested(
        &self,
        turn: &ShouldStopAfterTurnContext,
        touched: &mut Vec<PathBuf>,
    ) {
        let Some((propagation, host)) = &self.nested_memory else {
            return;
        };
        super::coordinator::note_turn_workers(propagation, host, &turn.new_messages).await;
        if turn.tool_results.is_empty() {
            return;
        }
        for path in propagation.drain() {
            if !touched.contains(&path) {
                touched.push(path);
            }
        }
    }

    /// The brief-mode enforcement row's text for this turn, if any.
    #[must_use]
    pub fn brief_enforcement(&self, turn: &ShouldStopAfterTurnContext) -> Option<String> {
        super::brief_mode::enforcement_text(&self.brief, turn)
    }
}

#[cfg(test)]
#[path = "modes_engine_tests.rs"]
mod engine_tests;
