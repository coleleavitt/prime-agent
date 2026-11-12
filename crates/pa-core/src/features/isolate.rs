//! Isolate a feature-hook future's panic so one bad hook can't take down
//! the enclosing run task.
//!
//! The run path in `pa-agent` / `pa-core` does not settle a task whose
//! inner future panicked: the task dies, the session's `rlm_child` run
//! never records a tool result, and the TUI keeps showing the cell as
//! running. A hook inside a `join_all` of hook futures can trip this with
//! a single `.unwrap()` or indexing bug. [`isolate_hook`] wraps each hook
//! future with `AssertUnwindSafe(fut).catch_unwind()`; a panic is logged
//! and the hook's output defaults, so the other hooks and the surrounding
//! task keep running.
//!
//! The precedent for the unwind pattern in this crate is
//! `kernel/manager/mod.rs:474`, which catches a user-callback panic.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt as _;

/// Run a feature hook's future and swallow a panic from the hook's path.
///
/// `hook` names the hook in the error log (e.g. `"after_tool_call"`); use
/// the same name at every site that invokes the same hook so logs can be
/// grouped. On panic the helper returns [`Default::default()`] for the
/// hook's output type; for every hook that returns `Option<T>` this is
/// `None`, which the callers already interpret as "nothing to add."
///
/// `AssertUnwindSafe` is honest here: a hook's future owns the arguments
/// it was called with and nothing shared mutably with the rest of the
/// session, so discarding its in-flight state on a panic is safe.
pub(crate) async fn isolate_hook<F>(hook: &'static str, fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Default + Send,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(output) => output,
        Err(_payload) => {
            tracing::error!(
                hook,
                "feature hook panicked; the session keeps running and this hook's output is dropped"
            );
            Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_hook_returns_the_default_and_does_not_propagate() {
        async fn boom() -> Option<String> {
            panic!("simulated hook panic");
        }
        let result = isolate_hook("test", boom()).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn a_well_behaved_hook_round_trips_its_output() {
        async fn ok() -> Option<String> {
            Some("noted".to_string())
        }
        let result = isolate_hook("test", ok()).await;
        assert_eq!(result, Some("noted".to_string()));
    }

    #[tokio::test]
    async fn a_panicking_hook_does_not_poison_sibling_hooks_in_join_all() {
        async fn boom() -> Option<String> {
            panic!("one hook panicked");
        }
        async fn ok(name: &str) -> Option<String> {
            Some(name.to_string())
        }
        let joined = futures::future::join_all([
            isolate_hook("test", ok("alpha")).boxed(),
            isolate_hook("test", boom()).boxed(),
            isolate_hook("test", ok("gamma")).boxed(),
        ])
        .await;
        assert_eq!(
            joined,
            vec![Some("alpha".to_string()), None, Some("gamma".to_string())]
        );
    }
}
