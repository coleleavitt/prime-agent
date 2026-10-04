//! Composition of the separately built feature crates (see
//! `docs/fork-feature-crates.md`). Each crate is wired here behind its own
//! Cargo feature; building with `--no-default-features` installs none, which
//! is the native product.

use std::sync::Arc;
use std::time::Duration;

use pa_core::features::SessionFeature;

/// The features this build enables, in installation order.
#[must_use]
// One cfg-gated push per feature crate (attributes on `vec!` elements are
// not stable); with every feature compiled out nothing is pushed.
#[allow(unused_mut, clippy::vec_init_then_push)]
pub fn enabled_features() -> Vec<Arc<dyn SessionFeature>> {
    vec![
        #[cfg(feature = "recall")]
        Arc::new(pa_recall::WorkspaceRecall::default()),
        #[cfg(feature = "toolforge")]
        Arc::new(pa_toolforge::ToolforgeFeature::new()),
        #[cfg(feature = "workflow")]
        Arc::new(pa_workflow::WorkflowFeature),
    ]
}

/// Install the enabled features into the session seam. Called once by the
/// binary before any session or worker starts.
pub fn install_enabled_features() {
    pa_core::features::install(enabled_features());
}

/// How long the process waits at exit for the features' background work.
const FEATURE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Give the installed features a bounded chance to finish background work
/// before the process exits; returns at once when none is installed.
pub fn flush_enabled_features() {
    pa_core::features::flush_installed(FEATURE_FLUSH_TIMEOUT);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each Cargo feature installs its crate, and nothing else is installed:
    /// `--no-default-features` installs none.
    #[test]
    fn the_build_installs_exactly_its_enabled_features() {
        let names: Vec<&str> = enabled_features()
            .iter()
            .map(|feature| feature.name())
            .collect();
        let expected: Vec<&str> = vec![
            #[cfg(feature = "recall")]
            "recall",
            #[cfg(feature = "toolforge")]
            "toolforge",
            #[cfg(feature = "workflow")]
            "workflow",
        ];
        assert_eq!(names, expected);
    }
}
