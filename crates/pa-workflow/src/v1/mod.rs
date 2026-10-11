//! Workflow V1: the ephemeral, single-operation host. The kernel's
//! `rlm.workflow.run_agent` asks the host for exactly one tool-less
//! provider turn and gets back a closed, digest-bound result.

mod host;
mod preflight;
mod runner;
pub mod wire;

use std::sync::Arc;

pub use host::RUN_AGENT_REQUEST_TYPE;
use pa_core::features::SessionFeatureContext;
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};

/// Register the V1 host request for one session.
pub fn register_host_handlers(context: &SessionFeatureContext, handlers: &mut HostRequestHandlers) {
    let config = Arc::new(host::HostConfig {
        agent_dir: context.agent_dir.clone(),
        cwd: context.cwd.clone(),
        session_model: context.model.clone(),
        telemetry: context.telemetry.clone(),
    });
    handlers.register(
        RUN_AGENT_REQUEST_TYPE,
        host_handler(move |payload| host::handle_run_agent(Arc::clone(&config), payload.data)),
    );
}
