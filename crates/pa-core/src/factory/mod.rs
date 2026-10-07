//! The factory: state-machine workflows of spawned child agents.
//!
//! A continual-harness `factory` entry declares a machine of subagent
//! states and guarded transitions; this module owns the whole factory
//! stack host-side, always compiled (the bundled runtime calls it in every
//! build): the write-time validator and dag compiler ([`spec`]), the spawn
//! labels ([`labels`]), and the spec-operation surface the kernel's thin
//! `rlm.factory` client calls ([`spec_ops`]).

pub mod executor;
pub mod host;
pub mod labels;
pub mod lane;
pub mod pyvalue;
pub mod spec;
pub mod spec_ops;
