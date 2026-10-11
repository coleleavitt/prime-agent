//! RAVO (see `README.md`): the gate that admits or refuses harness
//! refinements, the lineage of committed champions and the opponents they
//! are judged against, and the recording of provisional regressions.

mod authority;
mod auto_refine;
mod command;
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
pub use command::{
    ArcAgiTarget,
    RAVO_COMMAND,
    RAVO_USAGE,
    RavoCommand,
    parse_ravo_command,
    ravo_status_line,
};
pub use feature::{
    RAVO_ENV,
    RAVO_GATE_DECISION_EVENT,
    RAVO_SKILL,
    REFINEMENT_LOG_TARGET,
    RavoFeature,
    RavoOptions,
    RecurrenceFilter,
    ravo_enabled,
};
pub use gate::*;
pub use js::{canonical_json, locale_compare, sha256_hex};
pub use reducer::*;
pub use referee::*;
pub use run::{
    ModelFailure,
    ModelReply,
    NotStarted,
    RavoModel,
    RavoRunRequest,
    RavoRunService,
    RunServiceDeps,
    RunStores,
    StatusListener,
    parse_ravo_run_payload,
};
pub use run_host::{
    ModelFactory,
    RAVO_RUN_EVENT,
    RAVO_STATUS_FEATURE,
    SessionModel,
    ravo_run_allowed,
};
pub use runner::{DEFAULT_REPLAY_TIMEOUT, PythonReplayRunner};
pub use trust::*;
pub use trust_adjudication::*;
