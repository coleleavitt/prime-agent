//! Composition of the separately built feature crates (see
//! `docs/fork-feature-crates.md`). Each crate is wired here behind its own
//! Cargo feature; building with `--no-default-features` installs none, which
//! is the native product.

use std::sync::Arc;

use pa_core::features::SessionFeature;

/// The features this build enables, in installation order.
// One cfg-gated push per feature crate: the vector is empty (never mutated)
// when every feature is compiled out, so `vec![]` cannot express it.
#[allow(unused_mut, clippy::vec_init_then_push)]
#[must_use]
pub fn enabled_features() -> Vec<Arc<dyn SessionFeature>> {
    let mut features: Vec<Arc<dyn SessionFeature>> = Vec::new();
    #[cfg(feature = "workflow")]
    features.push(Arc::new(pa_workflow::WorkflowFeature));
    features
}

/// Install the enabled features into the session seam. Called once by the
/// binary before any session or worker starts.
pub fn install_enabled_features() {
    pa_core::features::install(enabled_features());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build installs exactly the features compiled into it; the
    /// native product (`--no-default-features`) installs none.
    #[test]
    #[allow(unused_mut, clippy::vec_init_then_push)] // as `enabled_features`
    fn the_build_enables_exactly_its_compiled_features() {
        let mut expected: Vec<&str> = Vec::new();
        #[cfg(feature = "workflow")]
        expected.push("workflow");
        let names: Vec<&str> = enabled_features()
            .iter()
            .map(|feature| feature.name())
            .collect();
        assert_eq!(names, expected);
    }
}
