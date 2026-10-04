//! RAVO (see `README.md`): the gate that admits or refuses harness
//! refinements, the lineage of committed champions and the opponents they
//! are judged against, and the recording of provisional regressions.

mod authority;
mod auto_refine;
mod feature;
mod gate;
mod js;
mod outcome;
mod reducer;
mod referee;
mod run;
mod run_host;
mod runner;
mod trigger;
mod trust;
mod trust_adjudication;
mod trust_runtime;
mod verification;

pub use authority::*;
pub use auto_refine::*;
pub use feature::{
    ravo_enabled, RavoFeature, RavoOptions, RAVO_ENV, RAVO_GATE_DECISION_EVENT,
    REFINEMENT_LOG_TARGET,
};
pub use gate::*;
pub use js::{canonical_json, locale_compare, sha256_hex};
pub use reducer::*;
pub use referee::*;
pub use run::{
    parse_ravo_run_payload, ModelFailure, ModelReply, NotStarted, RavoModel, RavoRunRequest,
    RavoRunService, RunServiceDeps, RunStores, StatusListener,
};
pub use run_host::{ModelFactory, SessionModel, RAVO_RUN_EVENT};
pub use runner::{PythonReplayRunner, DEFAULT_REPLAY_TIMEOUT};
pub use trust::*;
pub use trust_adjudication::*;
