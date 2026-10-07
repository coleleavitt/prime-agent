//! PLACEHOLDER: replaced by the guard's port.

use crate::context::GuardContext;
use crate::script::Script;

/// The stderr warning printed (once) when the bypass variable appeared after kernel start.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = None;

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = false;

#[expect(
    clippy::unnecessary_wraps,
    reason = "placeholder until the guard port lands"
)]
pub(crate) fn check(_script: &Script<'_>, _context: &GuardContext) -> Result<(), String> {
    Ok(())
}
