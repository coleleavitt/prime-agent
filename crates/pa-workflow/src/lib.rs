//! The fork's Workflow host (`docs/fork-feature-crates.md`): the kernel
//! host requests the runtime's `rlm.workflow` modules send. V1 (`v1`) is
//! the single `workflow.run_agent` operation; V2 (`v2`) is the closed
//! `workflow.v2.request` envelope (`validate` served, the durable actions
//! answered `CAPABILITY_UNAVAILABLE`).
//!
//! The crate plugs into sessions only through
//! [`pa_core::features::SessionFeature`]; `pa-cli` installs
//! [`WorkflowFeature`] behind its `workflow` Cargo feature. Without it the
//! runtime's `workflow.run_agent` and `workflow.v2.request` fail with the
//! standard unregistered host-request error, which the runtime reports as
//! `CapabilityUnavailable`.

pub mod v1;
pub mod v2;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::HostRequestHandlers;

/// The Workflow feature: registers the Workflow host requests in every
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
        v2::register_host_handlers(context, handlers);
    }
}
