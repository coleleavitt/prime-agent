//! Agent-engine turn execution: the turn state machine — the model-turn
//! runner, the turn boundary, the turn loop, the once-runner with its
//! retry/failover and quota-park machinery, and the session-agent
//! constructor.
use super::{
    AbortController,
    AgentSessionEngine,
    AutoCompactionRun,
    BoundaryRun,
    DaemonAllowlist,
    EngineEvent,
    GoalBoundary,
    Model,
    OverflowArmRun,
    ProviderTarget,
    QUOTA_WAKE_MAX_RETRIES,
    QUOTA_WAKE_RETRY_DELAY_MS,
    QuotaParkState,
    StopReason,
    TurnAdmission,
    TurnOnce,
    TurnPrompt,
    TurnResult,
    Value,
    aborted_message,
    drop_trailing_assistant,
    json,
    json_round_trip,
    map_thinking_level,
    retry_event_to_engine_event,
};

mod boundary;
#[cfg(test)]
mod fallback_models_tests;
mod model;
mod quota;
mod run_loop;
mod run_once;
