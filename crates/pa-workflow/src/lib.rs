//! The fork's Workflow host (`docs/fork-feature-crates.md`): the kernel
//! host request the runtime's `rlm.workflow` module sends. V1 (`v1`) is
//! the single `workflow.run_agent` operation.
//!
//! The crate plugs into sessions only through
//! [`pa_core::features::SessionFeature`]; `pa-cli` installs
//! [`WorkflowFeature`] behind its `workflow` Cargo feature. Without it the
//! runtime's `workflow.run_agent` fails with the standard unregistered
//! host-request error, which the runtime reports as `CapabilityUnavailable`.

pub mod v1;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::HostRequestHandlers;

/// The Workflow feature: registers the Workflow host request in every
/// session.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowFeature;

impl SessionFeature for WorkflowFeature {
    fn name(&self) -> &'static str {
        "workflow"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        v1::register_host_handlers(context, handlers);
    }
}
