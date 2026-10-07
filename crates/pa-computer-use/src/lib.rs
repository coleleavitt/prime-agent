//! The host side of the bundled `computer-use` skill.
//!
//! The kernel's `computer_use` package is a thin client: every observation,
//! input, capture, policy and permission decision runs here, behind the
//! `computer_use.*` kernel host requests. See `README.md` for the scope, the
//! wire contract and the platform matrix.

// A target without a backend (Windows) still compiles and tests the
// platform-independent session machinery, but never constructs it: every
// request answers the no-backend replies.
#![cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]

#[cfg(unix)]
mod capture;
pub mod element;
pub mod error;
mod host;
pub mod keymap;
pub mod permissions;
mod platform;
pub mod policy;
mod process;
pub mod pyfmt;
pub mod render;
pub mod secure;
mod session;
mod spec;
pub mod telemetry;

pub use host::{ComputerUse, HostConfig, REQUEST_TYPES};

#[cfg(test)]
mod testing;
