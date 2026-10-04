//! Workflow V2: the durable workflow protocol family
//! (`prime.workflow.*/v2`). The closed wire — the strict codec, the
//! definition and projection semantic validators, canonical digests, and
//! the retained-host capability profile — is shared by every V2 boundary;
//! [`host`] answers the runtime's `workflow.v2.request`.
//!
//! What exists is the store-free half of the contract: `validate` is
//! served in full, and every action that needs the durable controller
//! (`create`, `start`, `cancel`, `retry`, `status`, `events`) answers the
//! closed `CAPABILITY_UNAVAILABLE` error, as the TS host's disabled
//! capability did.

pub mod capability;
pub mod host;
pub mod json;
pub mod projection;
pub mod schema;
pub mod wire;

use std::sync::Arc;

use pa_core::features::SessionFeatureContext;
use pa_core::kernel::shared::{host_handler, HostRequestHandlers};

pub use host::REQUEST_TYPE;

/// Register the V2 host request for one session.
pub fn register_host_handlers(context: &SessionFeatureContext, handlers: &mut HostRequestHandlers) {
    let config = Arc::new(host::HostConfig {
        telemetry: context.telemetry.clone(),
    });
    handlers.register(
        REQUEST_TYPE,
        host_handler(move |payload| host::handle_request(Arc::clone(&config), payload.data)),
    );
}
