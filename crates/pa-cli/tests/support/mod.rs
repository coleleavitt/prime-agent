//! Helpers shared by the pa-cli integration test binaries.

use std::path::PathBuf;

/// The TS reference binary for differential tests, when one is supplied.
///
/// Opt-in only: `PA_TS_BINARY` must name an existing file. `prime-agent` on
/// PATH is never used, since that can be any build (a fork, or this Rust
/// binary itself) and would turn every differential test into a false
/// failure. CI sets nothing, so these tests skip there.
pub fn ts_binary() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("PA_TS_BINARY")?);
    path.is_file().then_some(path)
}
