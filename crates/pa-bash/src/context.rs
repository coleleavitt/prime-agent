//! The kernel state a check reads: its working directory, its environment at
//! call time, and the guard bypasses frozen at kernel start.
//!
//! The checker never reads the host process's own environment or working
//! directory: the kernel sends its own (`os.getcwd()`, `os.environ`) with every
//! request, so a model write to `os.environ` or a `os.chdir` reaches the checks
//! and the spawned command exactly as it reached the Python implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::verdict::GuardKind;

/// Where and with what environment a command is checked (and run).
#[derive(Debug, Clone)]
pub struct GuardContext {
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    launch_bypass: BTreeSet<GuardKind>,
    traceparent: Option<String>,
}

impl GuardContext {
    /// A context for `cwd` and the kernel environment `env`, with no guard
    /// bypassed at kernel start.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, env: BTreeMap<String, String>) -> Self {
        Self {
            cwd: cwd.into(),
            env,
            launch_bypass: BTreeSet::new(),
            traceparent: None,
        }
    }

    /// Mark `guard` as bypassed for the whole kernel (its bypass variable
    /// was set when the kernel started).
    #[must_use]
    pub fn with_launch_bypass(mut self, guard: GuardKind) -> Self {
        self.launch_bypass.insert(guard);
        self
    }

    /// The W3C traceparent the command's span carries: child processes
    /// (the command itself and the guards' probes) inherit it.
    #[must_use]
    pub fn with_traceparent(mut self, traceparent: Option<String>) -> Self {
        self.traceparent = traceparent;
        self
    }

    /// The kernel's working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// One variable of the kernel environment.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    /// The whole kernel environment.
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Whether `guard` was bypassed at kernel start.
    #[must_use]
    pub fn launch_bypassed(&self, guard: GuardKind) -> bool {
        self.launch_bypass.contains(&guard)
    }

    /// Whether `guard`'s bypass variable is set now although it was not at
    /// kernel start (a mid-session write the guard ignores).
    #[must_use]
    pub fn late_bypass(&self, guard: GuardKind) -> bool {
        !self.launch_bypassed(guard) && is_truthy_env_value(self.var(guard.bypass_env()))
    }

    #[must_use]
    pub fn traceparent(&self) -> Option<&str> {
        self.traceparent.as_deref()
    }
}

/// A bypass variable counts when it is set to anything but empty or `0`.
#[must_use]
pub fn is_truthy_env_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}
