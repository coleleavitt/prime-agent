//! The factory's kernel host requests.
//!
//! `factory.spec` serves the kernel's validator client (`rlm.factory`'s
//! `validate_factory_spec` and friends, and every `rlm.harness` factory
//! write): one spec operation per request, answered synchronously from the
//! host's single validator implementation.

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Register the factory host requests every session serves.
pub fn register_factory_host_handlers(handlers: &mut HostRequestHandlers) {
    handlers.register(
        "factory.spec",
        host_handler(|payload| async move { super::spec_ops::run_spec_op(&payload.data) }),
    );
}
