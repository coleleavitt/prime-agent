//! The continual-learning gate (see `README.md`): the learning index rolls
//! the structured log up into sealed days keyed by failure fingerprint,
//! `prime-agent learning` compares the fingerprints refinements claimed to
//! address against the rest, and the Engineer Trajectory Index labels each
//! fingerprint's course over ISO-week windows and feeds its stable residue
//! back into the harness digest and the recurrence reminders.
//!
//! The crate plugs into sessions only through
//! [`pa_core::features::SessionFeature`] (the digest hook) and into RAVO
//! through [`pa_ravo::RecurrenceFilter`]; `pa-cli` installs
//! [`LearningFeature`] and wires `learning` behind its `learning` feature.

mod chart;
pub mod command;
mod feature;
mod fs;
mod index;
mod js;
mod json;
mod prompt;
mod report;
mod store;
mod trajectory;

use std::path::PathBuf;

pub use chart::{ChartOptions, ChartSeries, render_ascii_chart};
pub use feature::LearningFeature;
pub use index::{
    FingerprintDayStats,
    LEARNING_INDEX_SCHEMA,
    LearningDay,
    REFINEMENT_COMMITTED_MSG,
    RefinementCommit,
    RollUp,
    SealResult,
    SpanKey,
    TURN_SPAN_NAME,
    normalize_day,
    read_learning_index,
    roll_up_learning_days,
    seal_learning_days,
    span_fingerprint_key,
    write_learning_day,
};
pub use prompt::{
    EntryClass,
    InternalizedReminders,
    MAX_TRAJECTORY_LINES,
    TRAJECTORY_SECTION_HEADING,
    TrajectoryPromptHook,
    format_trajectory_lines,
    trajectory_class_for_entries,
    trajectory_internalized_fingerprints,
    trajectory_prompt_adjustment,
};
pub use report::{
    CohortStats,
    DEFAULT_MIN_COHORT_N,
    FingerprintTrend,
    LearningDayPoint,
    LearningReport,
    MannWhitney,
    RATE_DENOMINATOR,
    Window,
    build_learning_report,
    mann_whitney_one_sided,
    normal_cdf,
};
pub use store::{
    agent_log_path,
    learning_dir,
    learning_index_dir,
    read_backfill_days,
    read_trajectory_index,
    trajectory_backfill_dir,
    trajectory_index_path,
    write_trajectory_index,
};
pub use trajectory::{
    CorpusDay,
    DEFAULT_MAX_TRAJECTORY_WINDOWS,
    DEFAULT_MIN_TRAJECTORY_WINDOWS,
    DEFAULT_TRAJECTORY_INTERNALIZED_GAP,
    PRIME_CORPUS,
    SealTrajectoryOptions,
    TRAJECTORY_INDEX_ENV,
    TRAJECTORY_STORE_VERSION,
    TrajectoryFingerprintWindow,
    TrajectoryLabel,
    TrajectoryLabelKind,
    TrajectoryRateWindow,
    TrajectoryStoreFile,
    TrajectoryWindow,
    iso_week,
    iso_week_of_millis,
    matches_security_class,
    seal_trajectory_windows,
    trajectory_index_enabled,
    trajectory_index_enabled_from_env,
};

/// Why the index could not be sealed.
#[derive(Debug, thiserror::Error)]
pub enum LearningError {
    /// A log generation could not be read or decompressed.
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A day file or its directory could not be written.
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LearningError {
    /// The underlying I/O error's text (the command names the log itself).
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Read { path, source } | Self::Write { path, source } => {
                format!("{source} ({})", path.display())
            }
        }
    }
}
