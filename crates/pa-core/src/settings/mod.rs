//! Settings subsystem: typed settings documents, global/project storage,
//! legacy migrations, and the manager.

pub mod claude_code;
pub mod experimental;
pub(crate) mod interactive_settings;
pub mod launch;
pub(crate) mod load;
pub(crate) mod manager;
pub(crate) mod merge;
pub mod side_ux;
pub(crate) mod storage;
pub(crate) mod types;

#[cfg(test)]
mod claude_code_tests;
#[cfg(test)]
mod persistence_tests;

pub use manager::{
    DEFAULT_IDLE_EVICTION_MINUTES,
    DEFAULT_SESSION_ARCHIVE_MAX_AGE_DAYS,
    DEFAULT_SESSION_ARCHIVE_MAX_SESSIONS,
    IdleEviction,
    SessionArchivePolicy,
    SettingsError,
    SettingsManager,
};
pub use storage::{
    CONFIG_DIR_NAME,
    FileSettingsStorage,
    InMemorySettingsStorage,
    SettingsScope,
    SettingsStorage,
};
pub use types::{
    AutoRefineSettings,
    AutonomousSettings,
    CompactionSettings,
    QueueModeSetting,
    SandboxSettings,
    Settings,
    ThinkingLevelSetting,
    TransportSetting,
    UpdateChannel,
};
