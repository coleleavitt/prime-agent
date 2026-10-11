//! The failure ledger and the resolution index (see `README.md`): runtime
//! failures observed in sessions are fingerprinted, counted in a ledger kept
//! in the harness state (per session and per machine), and joined to the
//! `ipython` cell that later fixed them, so a recurring failure carries its
//! fix back to the model.
//!
//! The crate plugs into sessions only through
//! [`pa_core::features::SessionFeature`]; `pa-cli` installs
//! [`FailureLedgerFeature`] behind its `ledger` Cargo feature. RAVO builds
//! on the pure ledger API and on [`LedgerHandle`] / [`LedgerObserver`].

mod extract;
mod feature;
mod fingerprint;
mod harness;
mod js;
mod ledger;
mod replay;
mod resolution;
mod resolution_store;

pub use extract::{IPYTHON_TOOL_NAME, extract_failures, observe_message, tool_result_text};
pub use feature::{
    FailureLedgerFeature,
    LedgerBoundary,
    LedgerFlush,
    LedgerHandle,
    LedgerObserver,
    LedgerOptions,
    LedgerScope,
    RESOLUTION_HINT_EVENT,
};
pub use fingerprint::{
    FAILURE_FINGERPRINT_ATTR,
    FAILURE_OPPONENT_PREFIX,
    FailureFingerprint,
    FailureKind,
    ParsedTraceback,
    failure_opponent_id,
    fingerprint_failure,
    fingerprint_tool_result_text,
    normalize_failure_message,
    parse_python_traceback,
};
pub use harness::{
    FAILURES_KEY,
    GLOBAL_FAILURE_LEDGER_ENV,
    HARNESS_STATE_DIR_NAME,
    HARNESS_STATE_FILE_NAME,
    HarnessDocument,
    HarnessStateError,
    acquire_harness_state_lock,
    global_failure_ledger_enabled,
    global_failure_ledger_enabled_from_env,
    global_harness_state_dir,
    harness_state_path,
    local_harness_state_dir,
    with_harness_state_lock,
};
pub use js::{iso_from_millis, now_iso, now_millis};
pub use ledger::{
    DEFAULT_PROMPT_LIMIT,
    DEFAULT_RECURRENCE_THRESHOLD,
    FailureLedger,
    FailureObservation,
    FailureRecord,
    LedgerUpdate,
    ProvisionalRegression,
    ReplayVerification,
    apply_replay_verifications,
    find_provisional_regressions,
    format_failure_ledger_for_prompt,
    format_recurrence_refine_instructions,
    format_regression_refine_instructions,
    merge_failure_observations,
    normalize_failure_ledger,
    observation_ordinal,
    record_provisional_regressions,
    recurring_failures,
    update_failure_ledger,
};
pub use replay::{
    MAX_REPLAY_CASES,
    MAX_REPLAY_SOURCE_CHARS,
    REPLAY_MODULE_DENYLIST,
    ReplayCase,
    ReplayProbe,
    derive_replay_case,
    is_replayable_module_path,
    merge_replay_case,
    normalize_replay_case,
    normalize_replay_cases,
    replay_probe_of,
    replay_probe_source,
    verified_replay_cases,
};
pub use resolution::{
    DEFAULT_MAX_CELL_CHARS,
    DEFAULT_MAX_RESOLUTIONS,
    DEFAULT_RESOLUTION_SOURCE,
    DEFAULT_RESOLUTION_WINDOW,
    ResolutionCell,
    ResolutionHint,
    ResolutionIndex,
    ResolutionIndexOptions,
    ResolutionOrigin,
    ResolutionRecord,
    ResolutionStore,
    format_resolution_hint,
};
pub use resolution_store::{
    FileResolutionStore,
    RESOLUTION_DIR_NAME,
    find_repo_dir,
    open_resolution_store,
    resolution_dir,
    resolution_store_path,
};
