//! The kernel half of plan mode: the host-only `plan_guard` frame that arms
//! the runtime's write guard (`rlm.plan_guard`, see
//! `prime-agent-runtime/src/rlm/repl.md` -> Plan guard).
//!
//! The session owns one [`PlanModeSwitch`]; every kernel it boots reads the
//! switch right after the `ready` handshake (before restore, bootstrap, or any
//! cell), and a toggle re-sends the frame to a running kernel. The frame's
//! token is minted per kernel process by the host and never enters a cell, so
//! kernel code cannot switch the guard off.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How long the host waits for the runtime to answer a `plan_guard` frame.
/// The runtime answers it on its reader thread (never behind a cell); a
/// runtime that predates the frame answers nothing the host can route.
pub(crate) const PLAN_GUARD_SETTLE_TIMEOUT_MS: u64 = 15_000;

/// One session's plan-mode flag, shared by the session engine (tool refusal,
/// per-turn notice) and the kernel lifecycle (the runtime guard). Cloning
/// shares the flag.
#[derive(Debug, Clone, Default)]
pub struct PlanModeSwitch {
    enabled: Arc<AtomicBool>,
}

impl PlanModeSwitch {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Set the flag and return the previous value.
    #[must_use]
    pub fn replace(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::SeqCst)
    }
}

/// The plan guard a kernel manager applies: the session's switch, the
/// host-owned directories the guarded kernel must still write (the session
/// artifact dir holding the namespace snapshot), and the directories that
/// stay read-only even inside a default writable root (the workspace: a
/// checkout under a temp dir is still the user's code).
#[derive(Debug, Clone, Default)]
pub struct KernelPlanGuard {
    pub mode: PlanModeSwitch,
    pub writable_roots: Vec<PathBuf>,
    pub protected_roots: Vec<PathBuf>,
}

/// A fresh per-kernel token: 32 random bytes, hex-encoded.
///
/// # Errors
///
/// Returns an error when the OS random source fails.
pub(crate) fn mint_plan_guard_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("plan guard token: OS randomness failed: {error}"))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_clones_share_one_flag() {
        let switch = PlanModeSwitch::new(false);
        let shared = switch.clone();
        assert!(!shared.replace(true));
        assert!(switch.is_enabled());
    }

    #[test]
    fn tokens_are_distinct_hex() {
        let first = mint_plan_guard_token().unwrap();
        let second = mint_plan_guard_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
