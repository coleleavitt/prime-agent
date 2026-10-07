//! The kernel `bash()` command checker and job runner.
//!
//! The model calls `rlm.bash(...)` in its Python kernel; the kernel is a thin
//! client of this crate, reached through the host (kernel host requests) or,
//! outside a host, through the `prime-agent --prime-agent-bash-host` sidecar.
//! The crate owns the refusal guards (destructive git, recursive chmod/chown,
//! force-push, secret echo, pipe-to-shell, sudo) and their pipeline.

mod context;
mod guards;
mod pipeline;
mod script;
mod shell;
mod verdict;

pub use context::{is_truthy_env_value, GuardContext};
pub use pipeline::{check, Allowances};
pub use script::Script;
pub use shell::{child_env, resolve_shell, ShellError};
pub use verdict::{GuardKind, Refusal};
