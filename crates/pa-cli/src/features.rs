//! Composition of the separately built feature crates (see
//! `docs/fork-feature-crates.md`). Each crate is wired here behind its own
//! Cargo feature; building with `--no-default-features` installs none, which
//! is the native product.

use std::sync::Arc;

use pa_core::features::SessionFeature;

/// The features this build enables, in installation order.
#[must_use]
// One cfg-gated push per feature crate (attributes on `vec!` elements are
// not stable); with every feature compiled out nothing is pushed.
#[allow(unused_mut, clippy::vec_init_then_push)]
pub fn enabled_features() -> Vec<Arc<dyn SessionFeature>> {
    let mut features: Vec<Arc<dyn SessionFeature>> = Vec::new();
    #[cfg(feature = "toolforge")]
    features.push(Arc::new(pa_toolforge::ToolforgeFeature::new()));
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

    /// The build installs exactly the feature crates its Cargo features
    /// enable; the native product (`--no-default-features`) installs none.
    #[test]
    #[allow(unused_mut, clippy::vec_init_then_push)] // as in `enabled_features`
    fn the_build_installs_exactly_its_enabled_features() {
        let names: Vec<&str> = enabled_features()
            .iter()
            .map(|feature| feature.name())
            .collect();
        let mut expected: Vec<&str> = Vec::new();
        #[cfg(feature = "toolforge")]
        expected.push("toolforge");
        assert_eq!(names, expected);
    }
}
