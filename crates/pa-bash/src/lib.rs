//! The kernel `bash()` command checker and job runner.
//!
//! The model calls `rlm.bash(...)` in its Python kernel; the kernel is a thin
//! client of this crate, reached through the host (kernel host requests) or,
//! outside a host, through the `prime-agent --prime-agent-bash-host` sidecar.
//! The crate owns the refusal guards (destructive git, recursive chmod/chown,
//! force-push, secret echo, pipe-to-shell, sudo), their pipeline, and the
//! runner that executes checked commands: process containment, the
//! completion fence, the bounded output buffer, progress events, and the
//! orphan-process journal.

mod context;
mod guards;
mod pipeline;
mod platform;
mod probe;
mod runner;
mod script;
mod service;
mod shell;
mod sidecar;
mod syntax;
mod verdict;

pub use context::{is_truthy_env_value, GuardContext};
pub use pipeline::{check, Allowances};
pub use runner::{ActivityError, JobTable, SpawnError, SpawnRequest};
pub use script::Script;
pub use service::{handle, REQUEST_TYPES};
pub use shell::{child_env, resolve_shell, ShellError};
pub use sidecar::serve_stdio;
pub use verdict::{GuardKind, Refusal};
