//! RAVO (see `README.md`): the gate that admits or refuses harness
//! refinements, the lineage of committed champions and the opponents they
//! are judged against, and the recording of provisional regressions.

mod authority;
mod feature;
mod gate;
mod js;
mod reducer;
mod referee;
mod runner;

pub use authority::*;
pub use feature::{
    ravo_enabled, RavoFeature, RavoOptions, RAVO_ENV, RAVO_GATE_DECISION_EVENT,
    REFINEMENT_LOG_TARGET,
};
pub use gate::*;
pub use js::{canonical_json, locale_compare, sha256_hex};
pub use reducer::*;
pub use referee::*;
pub use runner::{PythonReplayRunner, DEFAULT_REPLAY_TIMEOUT};
