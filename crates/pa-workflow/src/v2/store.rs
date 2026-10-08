//! The durable per-root Workflow V2 store (`WORKFLOW-V2.md` §6, §7, §11;
//! TS `workflow-v2-store.ts`, slice 4): one owner-only SQLite database at
//! `<session-artifacts>/<root-session-id>/workflows/v2.sqlite`, the
//! controller's run aggregate.
//!
//! The on-disk format is the TS store's, byte for byte where TS writes
//! bytes: the same 15 `STRICT` tables plus the `store_migrations` ledger
//! (the identical DDL, so the recorded migration checksum verifies across
//! implementations), the same `application_id` / `user_version` tags, and
//! the same JSON encodings in every text column (a `serde_json` value
//! keeps its keys in insertion order, the JS object order `JSON.stringify`
//! writes). A TS-written store opens here and the reverse
//! (`tests/v2_store_golden.rs`).
//!
//! The store owns durability mechanism only, no controller policy: a caller
//! supplies a [`CommandEffect`] (the controller facts and outbox operations
//! a command emits); [`Store::apply_command`] checks capacity, idempotency,
//! and the revision/epoch fences, folds the facts through the pure reducer
//! (validating every projection), and persists normalized rows, gap-free
//! controller facts, the immutable command receipt, and stable outbox
//! operation ids in one `BEGIN IMMEDIATE` transaction with no host,
//! provider, or blob I/O inside it.
//!
//! Writer exclusion (§7 "a documented single-writer OS lock"):
//! [`Store::acquire_writer`] takes an exclusive advisory lock on
//! `v2.sqlite.lock` beside the database for the handle's lifetime (a second
//! writer, in this process or another, gets `store_writer_locked`), then
//! raises the durable `controller_epoch` strictly; every later write
//! transaction re-reads that epoch under `BEGIN IMMEDIATE` and fails closed
//! (`store_epoch_stale`) once a newer writer superseded this one. TS fenced
//! by the epoch increase alone; the lock file and the per-transaction
//! epoch check add exclusion without changing any byte the TS store reads.
//!
//! Dormant: no production path opens a store; the V2 capability stays
//! `CAPABILITY_UNAVAILABLE` (`super::host`). Windows probes unavailable, as
//! TS's did (owner-only single-writer semantics are unproven there).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::projection::{validate_projection_semantics, Projection, ProjectionKind};
use super::reducer::{
    check_terminalized_outcome, reduce_fact, revalidate_aggregate, AttemptRecord, DefinitionGraph,
    NodeState, ReducerError, RunAggregate,
};
use super::schema;
use super::wire::{self, Action, RequestError, WireError};

/// SQLite `application_id`: `0x57325354`, `W2ST` (Workflow V2 store).
pub const STORE_APPLICATION_ID: i64 = 0x5732_5354;
/// `PRAGMA user_version`: the migration count; a newer one fails closed.
pub const STORE_USER_VERSION: i64 = 1;
/// The database file under the store root.
pub const STORE_FILE_NAME: &str = "v2.sqlite";
/// The writer-lock file under the store root (Rust addition; TS never
/// reads it).
pub const WRITER_LOCK_FILE_NAME: &str = "v2.sqlite.lock";
const BUSY_TIMEOUT_MS: u64 = 5_000;
const MAX_SAFE: u64 = 9_007_199_254_740_991;

/// Per-root capacity quotas (§7 minimum profile).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Capacity {
    pub max_runs: u64,
    pub max_events: u64,
    pub max_commands: u64,
    pub max_operations: u64,
    pub max_host_inbox: u64,
    pub max_settlements: u64,
    pub max_tombstones: u64,
    /// Retained prompt/result UTF-8 bytes (256 MiB).
    pub max_text_bytes: u64,
    /// The database plus its WAL (512 MiB).
    pub max_db_bytes: u64,
    pub high_water_fraction: f64,
}

/// The §7 quotas.
pub const STORE_CAPACITY: Capacity = Capacity {
    max_runs: 1_024,
    max_events: 131_072,
    max_commands: 16_384,
    max_operations: 16_384,
    max_host_inbox: 262_144,
    max_settlements: 16_384,
    max_tombstones: 16_384,
    max_text_bytes: 268_435_456,
    max_db_bytes: 536_870_912,
    high_water_fraction: 0.9,
};

/// The free-space floor: `max(64 MiB, 10% of the filesystem)`.
const MIN_FREE_BYTES: u64 = 67_108_864;

/// The 15 minimum tables (§7) and the migration ledger, all `STRICT`. The
/// bytes are the TS store's `BASE_DDL` exactly: the migration checksum
/// covers them.
const BASE_DDL: &str = concat!(
    "CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE definitions (definition_digest TEXT PRIMARY KEY",
    ", canonical_bytes TEXT NOT NULL, utf8_bytes INTEGER NOT NULL",
    ", created_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE runs (run_id TEXT PRIMARY KEY",
    ", definition_digest TEXT NOT NULL REFERENCES definitions(definition_digest)",
    ", revision INTEGER NOT NULL, controller_epoch INTEGER NOT NULL",
    ", cancel_epoch INTEGER NOT NULL, phase TEXT NOT NULL, intent TEXT NOT NULL",
    ", outcome TEXT, conditions TEXT NOT NULL, host_cursor TEXT, terminal_outcome TEXT",
    ", terminal_digest TEXT, erased INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL",
    ", updated_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE nodes (run_id TEXT NOT NULL REFERENCES runs(run_id)",
    ", node_id TEXT NOT NULL, phase TEXT NOT NULL, intent TEXT NOT NULL, outcome TEXT",
    ", conditions TEXT NOT NULL, PRIMARY KEY (run_id, node_id)) STRICT;\n",
    "CREATE TABLE attempts (run_id TEXT NOT NULL REFERENCES runs(run_id)",
    ", attempt_id TEXT NOT NULL, node_id TEXT NOT NULL, operation_id TEXT",
    ", rlm_child_id TEXT, turn_id TEXT, settlement_digest TEXT, phase TEXT NOT NULL",
    ", intent TEXT NOT NULL, outcome TEXT, conditions TEXT NOT NULL, turn_json TEXT",
    ", PRIMARY KEY (run_id, attempt_id)) STRICT;\n",
    "CREATE TABLE child_turn_bindings (run_id TEXT NOT NULL, attempt_id TEXT NOT NULL",
    ", workflow_child_id TEXT NOT NULL, rlm_child_id TEXT NOT NULL, turn_id TEXT NOT NULL",
    ", request_id TEXT NOT NULL, canonical_digest TEXT NOT NULL, bound_at TEXT NOT NULL",
    ", PRIMARY KEY (run_id, attempt_id)",
    ", FOREIGN KEY (run_id, attempt_id) REFERENCES attempts(run_id, attempt_id)) STRICT;\n",
    "CREATE TABLE commands (request_digest TEXT PRIMARY KEY, request_id TEXT NOT NULL",
    ", run_id TEXT, command_id TEXT, action TEXT NOT NULL, canonical_bytes TEXT NOT NULL",
    ", receipt_json TEXT NOT NULL, created_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE operations (operation_id TEXT PRIMARY KEY",
    ", run_id TEXT NOT NULL REFERENCES runs(run_id), attempt_id TEXT, kind TEXT NOT NULL",
    ", request_id TEXT NOT NULL, canonical_request TEXT NOT NULL, host_request_id TEXT",
    ", phase TEXT NOT NULL, intent TEXT NOT NULL, outcome TEXT, conditions TEXT NOT NULL",
    ", claim_epoch INTEGER, claim_owner TEXT, claim_expires_at TEXT, receipt_json TEXT",
    ", created_at TEXT NOT NULL, updated_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE host_inbox (host_event_id TEXT PRIMARY KEY",
    ", run_id TEXT NOT NULL REFERENCES runs(run_id), host_cursor TEXT NOT NULL",
    ", type TEXT NOT NULL, fact_json TEXT NOT NULL, created_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE host_streams (run_id TEXT PRIMARY KEY REFERENCES runs(run_id)",
    ", host_cursor TEXT, updated_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE settlements (settlement_digest TEXT PRIMARY KEY",
    ", run_id TEXT NOT NULL REFERENCES runs(run_id), attempt_id TEXT NOT NULL",
    ", outcome TEXT NOT NULL, result_kind TEXT NOT NULL, result_utf8_bytes INTEGER",
    ", result_sha256 TEXT, usage_json TEXT NOT NULL, settlement_json TEXT NOT NULL",
    ", created_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE acceptance (run_id TEXT NOT NULL REFERENCES runs(run_id)",
    ", node_id TEXT NOT NULL, attempt_id TEXT, decision TEXT NOT NULL, evidence_digest TEXT",
    ", decided_at TEXT NOT NULL, PRIMARY KEY (run_id, node_id)) STRICT;\n",
    "CREATE TABLE budgets (run_id TEXT PRIMARY KEY REFERENCES runs(run_id)",
    ", max_concurrent_attempts INTEGER NOT NULL, max_total_tokens INTEGER NOT NULL",
    ", reserved_tokens INTEGER NOT NULL DEFAULT 0, settled_tokens INTEGER NOT NULL DEFAULT 0",
    ", updated_at TEXT NOT NULL) STRICT;\n",
    "CREATE TABLE tombstones (run_id TEXT NOT NULL REFERENCES runs(run_id)",
    ", rlm_child_id TEXT NOT NULL, request_id TEXT NOT NULL, tombstone_digest TEXT NOT NULL",
    ", created_at TEXT NOT NULL, PRIMARY KEY (run_id, rlm_child_id)) STRICT;\n",
    "CREATE TABLE events (run_id TEXT NOT NULL REFERENCES runs(run_id)",
    ", sequence INTEGER NOT NULL, event_id TEXT NOT NULL, revision INTEGER NOT NULL",
    ", type TEXT NOT NULL, controller_epoch INTEGER NOT NULL, cancel_epoch INTEGER NOT NULL",
    ", recorded_at TEXT NOT NULL, data_json TEXT NOT NULL, digest TEXT NOT NULL",
    ", compacted INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (run_id, sequence)",
    ", UNIQUE (event_id)) STRICT;\n",
    "CREATE UNIQUE INDEX commands_request_id ON commands (request_id);\n",
    "CREATE UNIQUE INDEX commands_run_command ON commands (run_id, command_id)",
    " WHERE command_id IS NOT NULL;\n",
    "CREATE INDEX operations_by_run_phase ON operations (run_id, phase);\n",
    "CREATE INDEX host_inbox_by_run ON host_inbox (run_id);\n",
    "CREATE INDEX settlements_by_run ON settlements (run_id);",
);

const MIGRATION_LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS store_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL) STRICT";

/// One append-only migration (new migrations append; history never
/// changes).
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: [Migration; 1] = [Migration {
    version: 1,
    name: "base",
    sql: BASE_DDL,
}];

fn migration_checksum(migration: &Migration) -> String {
    wire::sha256_digest(
        format!(
            "{}\n{}\n{}",
            migration.version, migration.name, migration.sql
        )
        .as_bytes(),
    )
}

/// The digest of the migration registry (the backup manifest binds it).
#[must_use]
pub fn migration_registry_digest() -> String {
    let checksums: Vec<String> = MIGRATIONS.iter().map(migration_checksum).collect();
    wire::sha256_digest(checksums.join("\n").as_bytes())
}

/// The 15 minimum tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    StoreMeta,
    Definitions,
    Runs,
    Nodes,
    Attempts,
    ChildTurnBindings,
    Commands,
    Operations,
    HostInbox,
    HostStreams,
    Settlements,
    Acceptance,
    Budgets,
    Tombstones,
    Events,
}

impl Table {
    /// Every minimum table.
    pub const ALL: [Table; 15] = [
        Table::StoreMeta,
        Table::Definitions,
        Table::Runs,
        Table::Nodes,
        Table::Attempts,
        Table::ChildTurnBindings,
        Table::Commands,
        Table::Operations,
        Table::HostInbox,
        Table::HostStreams,
        Table::Settlements,
        Table::Acceptance,
        Table::Budgets,
        Table::Tombstones,
        Table::Events,
    ];

    /// The SQL name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Table::StoreMeta => "store_meta",
            Table::Definitions => "definitions",
            Table::Runs => "runs",
            Table::Nodes => "nodes",
            Table::Attempts => "attempts",
            Table::ChildTurnBindings => "child_turn_bindings",
            Table::Commands => "commands",
            Table::Operations => "operations",
            Table::HostInbox => "host_inbox",
            Table::HostStreams => "host_streams",
            Table::Settlements => "settlements",
            Table::Acceptance => "acceptance",
            Table::Budgets => "budgets",
            Table::Tombstones => "tombstones",
            Table::Events => "events",
        }
    }
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// The store's closed fail-closed codes (`as_str` spells the TS code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreCode {
    PathEscape,
    SymlinkComponent,
    WrongOwner,
    ModeTooOpen,
    RootSymlink,
    RootNotDir,
    FileSymlink,
    NotRegularFile,
    InvalidId,
    InvalidDigest,
    WalUnavailable,
    SynchronousUnavailable,
    ForeignKeysUnavailable,
    IntegrityFailed,
    Foreign,
    UserVersionNewer,
    MigrationMissing,
    MigrationUnknown,
    MigrationChecksum,
    MigrationName,
    TornSchema,
    ScopeMismatch,
    EpochRange,
    EpochStale,
    WriterLocked,
    NotAcquired,
    MetaMissing,
    CapacityExceeded,
    ConditionsCorrupt,
    JsonCorrupt,
    DefinitionMissing,
    TerminalOutcomeMismatch,
    NodeMissing,
    EventGap,
    NotValidate,
    ValidateNotMutation,
    NotMutation,
    IdempotencyConflict,
    NotFound,
    CommandFenceStale,
    CreateNoDefinition,
    CreateNoEvents,
    EventRunMismatch,
    StaleClaim,
    OperationHostIdConflict,
    SnapshotRequired,
    ErasureRefused,
    CompactionRefused,
    TimeInvalid,
}

impl StoreCode {
    /// The TS code string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StoreCode::PathEscape => "store_path_escape",
            StoreCode::SymlinkComponent => "store_symlink_component",
            StoreCode::WrongOwner => "store_wrong_owner",
            StoreCode::ModeTooOpen => "store_mode_too_open",
            StoreCode::RootSymlink => "store_root_symlink",
            StoreCode::RootNotDir => "store_root_not_dir",
            StoreCode::FileSymlink => "store_file_symlink",
            StoreCode::NotRegularFile => "store_not_regular_file",
            StoreCode::InvalidId => "store_invalid_id",
            StoreCode::InvalidDigest => "store_invalid_digest",
            StoreCode::WalUnavailable => "store_wal_unavailable",
            StoreCode::SynchronousUnavailable => "store_synchronous_unavailable",
            StoreCode::ForeignKeysUnavailable => "store_foreign_keys_unavailable",
            StoreCode::IntegrityFailed => "store_integrity_failed",
            StoreCode::Foreign => "store_foreign",
            StoreCode::UserVersionNewer => "store_user_version_newer",
            StoreCode::MigrationMissing => "store_migration_missing",
            StoreCode::MigrationUnknown => "store_migration_unknown",
            StoreCode::MigrationChecksum => "store_migration_checksum",
            StoreCode::MigrationName => "store_migration_name",
            StoreCode::TornSchema => "store_torn_schema",
            StoreCode::ScopeMismatch => "store_scope_mismatch",
            StoreCode::EpochRange => "store_epoch_range",
            StoreCode::EpochStale => "store_epoch_stale",
            StoreCode::WriterLocked => "store_writer_locked",
            StoreCode::NotAcquired => "store_not_acquired",
            StoreCode::MetaMissing => "store_meta_missing",
            StoreCode::CapacityExceeded => "CAPACITY_EXCEEDED",
            StoreCode::ConditionsCorrupt => "store_conditions_corrupt",
            StoreCode::JsonCorrupt => "store_json_corrupt",
            StoreCode::DefinitionMissing => "store_definition_missing",
            StoreCode::TerminalOutcomeMismatch => "store_terminal_outcome_mismatch",
            StoreCode::NodeMissing => "store_node_missing",
            StoreCode::EventGap => "store_event_gap",
            StoreCode::NotValidate => "store_not_validate",
            StoreCode::ValidateNotMutation => "store_validate_not_mutation",
            StoreCode::NotMutation => "store_not_mutation",
            StoreCode::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            StoreCode::NotFound => "NOT_FOUND",
            StoreCode::CommandFenceStale => "COMMAND_FENCE_STALE",
            StoreCode::CreateNoDefinition => "store_create_no_definition",
            StoreCode::CreateNoEvents => "store_create_no_events",
            StoreCode::EventRunMismatch => "store_event_run_mismatch",
            StoreCode::StaleClaim => "STALE_CLAIM",
            StoreCode::OperationHostIdConflict => "OPERATION_HOST_ID_CONFLICT",
            StoreCode::SnapshotRequired => "SNAPSHOT_REQUIRED",
            StoreCode::ErasureRefused => "ERASURE_REFUSED",
            StoreCode::CompactionRefused => "COMPACTION_REFUSED",
            StoreCode::TimeInvalid => "store_time_invalid",
        }
    }
}

/// Every store failure. Nothing is ever partially applied: a failing write
/// rolls its transaction back.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A hardening, integrity, fence, idempotency, or capacity refusal.
    #[error("{}: {message}", .code.as_str())]
    Refused { code: StoreCode, message: String },
    /// The pure reducer refused a fact or produced an invalid projection.
    #[error(transparent)]
    Reducer(#[from] ReducerError),
    /// A value outside the closed wire.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// A public request outside the closed request family.
    #[error(transparent)]
    Request(#[from] RequestError),
    #[error("store_sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("store_io: {0}")]
    Io(#[from] io::Error),
}

impl StoreError {
    /// The refusal code, when this is a store refusal.
    #[must_use]
    pub fn code(&self) -> Option<StoreCode> {
        match self {
            StoreError::Refused { code, .. } => Some(*code),
            _ => None,
        }
    }
}

fn refuse<T>(code: StoreCode, message: impl Into<String>) -> Result<T, StoreError> {
    Err(StoreError::Refused {
        code,
        message: message.into(),
    })
}

type Result<T, E = StoreError> = std::result::Result<T, E>;

// ---------------------------------------------------------------------------
// Shapes.
// ---------------------------------------------------------------------------

/// The store's clock: host-authored RFC 3339 UTC with milliseconds
/// (`Date.prototype.toISOString`). Tests inject a fixed or stepped clock.
pub type Clock = Arc<dyn Fn() -> String + Send + Sync>;

/// Free and total bytes of the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemSpace {
    pub free: u64,
    pub total: u64,
}

/// The free-space probe the capacity guard consults (`None`: unknown, which
/// fabricates no headroom but is not itself a refusal).
pub type FilesystemProbe = Arc<dyn Fn(&Path) -> Option<FilesystemSpace> + Send + Sync>;

/// How to open a store.
#[derive(Clone)]
pub struct StoreOptions {
    /// The owner-only store root, `<session-artifacts>/<root>/workflows/`
    /// (host-derived, never caller input).
    pub root: PathBuf,
    /// `sha256:` of the closed root-session authority scope, bound into
    /// `store_meta` on first open and checked on every later one.
    pub root_scope_digest: String,
    /// The controller/receipt clock (`None`: the system clock).
    pub clock: Option<Clock>,
    /// The filesystem probe (`None`: `statvfs`).
    pub filesystem: Option<FilesystemProbe>,
}

/// The kind of one outbox operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Deliver,
    Cancel,
    Delete,
}

impl OperationKind {
    fn as_str(self) -> &'static str {
        match self {
            OperationKind::Deliver => "deliver",
            OperationKind::Cancel => "cancel",
            OperationKind::Delete => "delete",
        }
    }

    /// The operation projection intent (`cancel`, else `deliver`).
    fn intent(self) -> &'static str {
        if self == OperationKind::Cancel {
            "cancel"
        } else {
            "deliver"
        }
    }
}

/// One stable outbox operation a command enqueues.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEnqueue {
    pub operation_id: String,
    pub attempt_id: Option<String>,
    pub kind: OperationKind,
    pub request_id: String,
    /// The exact host request, redelivered verbatim (canonical bytes) on
    /// claim expiry.
    pub canonical_request: Value,
}

/// One immutable `(child, turn)` binding (evidence; never native
/// topology).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildTurnBinding {
    pub attempt_id: String,
    pub workflow_child_id: String,
    pub rlm_child_id: String,
    pub turn_id: String,
    pub request_id: String,
    pub canonical_digest: String,
}

/// An acceptance decision (`not_evaluated`, `accepted`, `rejected`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    NotEvaluated,
    Accepted,
    Rejected,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::NotEvaluated => "not_evaluated",
            Decision::Accepted => "accepted",
            Decision::Rejected => "rejected",
        }
    }
}

/// One node acceptance row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceDecision {
    pub node_id: String,
    pub attempt_id: Option<String>,
    pub decision: Decision,
    pub evidence_digest: Option<String>,
}

/// One native-topology tombstone binding (evidence only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TombstoneBinding {
    pub rlm_child_id: String,
    pub request_id: String,
    pub tombstone_digest: String,
}

/// A run's closed soft budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunBudget {
    pub max_concurrent_attempts: u64,
    pub max_total_tokens: u64,
}

/// What one command commits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandEffect {
    /// Schema-valid controller facts, gap-free from the run's last sequence.
    pub events: Vec<Value>,
    pub operations: Vec<OutboxEnqueue>,
    /// `create`: the definition to persist (bound by digest).
    pub definition: Option<Value>,
    /// `create`: the run's budget (default: the definition's).
    pub budget: Option<RunBudget>,
    pub bindings: Vec<ChildTurnBinding>,
    pub acceptance: Vec<AcceptanceDecision>,
    pub tombstones: Vec<TombstoneBinding>,
}

/// The immutable receipt of one committed command (`receipt_json`, in the
/// TS key order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandReceipt {
    pub action: String,
    pub request_id: String,
    pub run_id: Option<String>,
    pub command_id: Option<String>,
    pub request_digest: String,
    pub revision: u64,
    pub controller_epoch: u64,
    pub cancel_epoch: u64,
    pub applied_sequences: Vec<u64>,
    pub recorded_at: String,
}

/// One host-fact ingestion's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingestion {
    /// False for a duplicate `hostEventId` (nothing changed).
    pub applied: bool,
    pub host_cursor: Option<String>,
}

/// One claimed outbox operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedOperation {
    pub operation_id: String,
    pub run_id: String,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub request_id: String,
    pub canonical_request: String,
    pub host_request_id: Option<String>,
    pub claim_epoch: u64,
    pub claim_expires_at: String,
}

/// An operation's terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationOutcome {
    Succeeded,
    Failed,
    Ambiguous,
}

impl OperationOutcome {
    fn as_str(self) -> &'static str {
        match self {
            OperationOutcome::Succeeded => "succeeded",
            OperationOutcome::Failed => "failed",
            OperationOutcome::Ambiguous => "ambiguous",
        }
    }
}

/// An outbox acknowledgement by the claim's owner.
#[derive(Debug, Clone, PartialEq)]
pub struct Acknowledgement {
    pub operation_id: String,
    pub owner: String,
    pub host_request_id: String,
    pub outcome: OperationOutcome,
    pub receipt: Value,
}

/// One controller event row (`listEvents`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub sequence: u64,
    pub event_id: String,
    pub revision: u64,
    pub kind: String,
    pub controller_epoch: u64,
    pub cancel_epoch: u64,
    pub recorded_at: String,
    pub data_json: String,
    pub digest: String,
}

/// One outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    pub operation_id: String,
    pub run_id: String,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub request_id: String,
    pub canonical_request: String,
    pub host_request_id: Option<String>,
    pub phase: String,
    pub intent: String,
    pub outcome: Option<String>,
    pub conditions: String,
    pub claim_epoch: Option<u64>,
    pub claim_owner: Option<String>,
    pub claim_expires_at: Option<String>,
    pub receipt_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The store capability verdict (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityProbe {
    pub available: bool,
    /// Why it is unavailable (`None` when available).
    pub reason: Option<&'static str>,
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

fn assert_id<'a>(value: &'a str, field: &str) -> Result<&'a str> {
    if schema::is_id(value) {
        Ok(value)
    } else {
        refuse(StoreCode::InvalidId, format!("invalid {field}: {value}"))
    }
}

fn assert_digest<'a>(value: &'a str, field: &str) -> Result<&'a str> {
    if schema::is_digest(value) {
        Ok(value)
    } else {
        refuse(
            StoreCode::InvalidDigest,
            format!("invalid {field}: {value}"),
        )
    }
}

fn sql_int(value: u64) -> i64 {
    // Every stored integer is a wire integer (<= 2^53 - 1).
    i64::try_from(value.min(MAX_SAFE)).unwrap_or(i64::MAX)
}

fn from_sql_int(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn parse_json(text: &str, what: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| StoreError::Refused {
        code: StoreCode::JsonCorrupt,
        message: format!("{what} is not JSON: {error}"),
    })
}

fn text_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn utf8_len(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

/// `Date.parse` of the store's own `toISOString` timestamps
/// (`YYYY-MM-DDTHH:MM:SS.sssZ`) to Unix milliseconds.
fn parse_iso_millis(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }
    let field = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second, millis) = (
        field(11..13)?,
        field(14..16)?,
        field(17..19)?,
        field(20..23)?,
    );
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1_000 + millis)
}

fn system_clock() -> String {
    pa_core::session::manager::format_iso_now()
}

// ---------------------------------------------------------------------------
// Filesystem hardening (owner-only, no symlinks, realpath under the root).
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn assert_owned_private(
    path: &Path,
    metadata: &std::fs::Metadata,
    allowed_mode: u32,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !pa_core::platform::perms::is_owned_by_current_user(metadata) {
        return refuse(
            StoreCode::WrongOwner,
            format!("store path not owned by current user: {}", path.display()),
        );
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 || mode & 0o777 & !allowed_mode != 0 {
        return refuse(
            StoreCode::ModeTooOpen,
            format!(
                "store path mode {:o} exceeds {allowed_mode:o}: {}",
                mode & 0o777,
                path.display()
            ),
        );
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // the unix arm refuses; inherited ACLs govern here
fn assert_owned_private(
    _path: &Path,
    _metadata: &std::fs::Metadata,
    _allowed_mode: u32,
) -> Result<()> {
    Ok(())
}

fn assert_hardened_dir(root: &Path) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        return refuse(
            StoreCode::RootSymlink,
            format!("store root is a symlink: {}", root.display()),
        );
    }
    if !metadata.is_dir() {
        return refuse(
            StoreCode::RootNotDir,
            format!("store root is not a directory: {}", root.display()),
        );
    }
    assert_owned_private(root, &metadata, pa_core::platform::perms::PRIVATE_DIR_MODE)?;
    Ok(std::fs::canonicalize(root)?)
}

/// A file directly under the canonical root: absent, or a private regular
/// file whose realpath is exactly `root/name`.
fn assert_hardened_file(canonical_root: &Path, name: &str) -> Result<PathBuf> {
    let path = canonical_root.join(name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return refuse(
            StoreCode::FileSymlink,
            format!("store file is a symlink: {}", path.display()),
        );
    }
    if !metadata.is_file() {
        return refuse(
            StoreCode::NotRegularFile,
            format!("store file is not a regular file: {}", path.display()),
        );
    }
    assert_owned_private(
        &path,
        &metadata,
        pa_core::platform::perms::PRIVATE_FILE_MODE,
    )?;
    if std::fs::canonicalize(&path)? != path {
        return refuse(
            StoreCode::PathEscape,
            format!("store realpath escapes root: {}", path.display()),
        );
    }
    Ok(path)
}

/// Open (creating owner-only) a regular file the hardening already vetted.
fn open_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    pa_core::platform::perms::set_private_mode(&mut options);
    Ok(options.open(path)?)
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // directories cannot be opened for sync here
fn fsync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Free and total bytes of the filesystem holding `path`.
#[cfg(unix)]
fn statvfs_space(path: &Path) -> Option<FilesystemSpace> {
    let stats = nix::sys::statvfs::statvfs(path).ok()?;
    // `c_ulong` / `fsblkcnt_t` are narrower than u64 on some unix targets.
    #[allow(clippy::useless_conversion)]
    let fragment = u64::from(stats.fragment_size());
    #[allow(clippy::useless_conversion)]
    let free = u64::from(stats.blocks_available()).checked_mul(fragment)?;
    #[allow(clippy::useless_conversion)]
    let total = u64::from(stats.blocks()).checked_mul(fragment)?;
    Some(FilesystemSpace { free, total })
}

#[cfg(not(unix))]
fn statvfs_space(_path: &Path) -> Option<FilesystemSpace> {
    None
}

// ---------------------------------------------------------------------------
// Connection setup: pragmas, integrity, schema, scope.
// ---------------------------------------------------------------------------

fn pragma_text(conn: &Connection, sql: &str) -> Result<String> {
    let value: rusqlite::types::Value = conn.query_row(sql, [], |row| row.get(0))?;
    Ok(match value {
        rusqlite::types::Value::Text(text) => text,
        rusqlite::types::Value::Integer(number) => number.to_string(),
        other => format!("{other:?}"),
    })
}

fn pragma_int(conn: &Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    let journal = pragma_text(conn, "PRAGMA journal_mode = WAL")?;
    if !journal.eq_ignore_ascii_case("wal") {
        return refuse(
            StoreCode::WalUnavailable,
            format!("journal_mode is {journal}"),
        );
    }
    conn.execute_batch("PRAGMA synchronous = FULL")?;
    if pragma_int(conn, "PRAGMA synchronous")? != 2 {
        return refuse(StoreCode::SynchronousUnavailable, "synchronous is not FULL");
    }
    conn.execute_batch("PRAGMA foreign_keys = ON")?;
    if pragma_int(conn, "PRAGMA foreign_keys")? != 1 {
        return refuse(StoreCode::ForeignKeysUnavailable, "foreign_keys is not ON");
    }
    conn.execute_batch(&format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}"))?;
    conn.execute_batch("PRAGMA cell_size_check = ON")?;
    conn.execute_batch("PRAGMA wal_autocheckpoint = 512")?;
    Ok(())
}

fn assert_integrity(conn: &Connection) -> Result<()> {
    for check in ["quick_check", "integrity_check"] {
        let verdict = pragma_text(conn, &format!("PRAGMA {check}"))?;
        if verdict != "ok" {
            return refuse(
                StoreCode::IntegrityFailed,
                format!("{check} returned {verdict}"),
            );
        }
    }
    Ok(())
}

fn table_names(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

fn run_migrations(conn: &mut Connection, applied_through: i64) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if applied_through == 0 {
        tx.execute_batch(MIGRATION_LEDGER_DDL)?;
    }
    for migration in &MIGRATIONS {
        if migration.version <= applied_through {
            continue;
        }
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO store_migrations (version, name, checksum, applied_at) VALUES (?, ?, ?, ?)",
            params![
                migration.version,
                migration.name,
                migration_checksum(migration),
                system_clock()
            ],
        )?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {}", MIGRATIONS.len()))?;
    tx.commit()?;
    Ok(())
}

fn verify_applied_migrations(conn: &Connection, applied_through: i64) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT version, name, checksum FROM store_migrations ORDER BY version")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if i64::try_from(rows.len()).unwrap_or(i64::MAX) < applied_through {
        return refuse(
            StoreCode::MigrationMissing,
            "applied migration ledger is short",
        );
    }
    for (version, name, checksum) in rows {
        let Some(registry) = MIGRATIONS
            .iter()
            .find(|migration| migration.version == version)
        else {
            return refuse(
                StoreCode::MigrationUnknown,
                format!("applied migration {version} not in registry"),
            );
        };
        if checksum != migration_checksum(registry) {
            return refuse(
                StoreCode::MigrationChecksum,
                format!("migration {version} checksum mismatch"),
            );
        }
        if name != registry.name {
            return refuse(
                StoreCode::MigrationName,
                format!("migration {version} name mismatch"),
            );
        }
    }
    Ok(())
}

fn initialize_or_validate_schema(conn: &mut Connection) -> Result<()> {
    let application_id = pragma_int(conn, "PRAGMA application_id")?;
    let tables = table_names(conn)?;
    if application_id == 0 {
        if tables.iter().any(|name| !name.starts_with("sqlite_")) {
            return refuse(
                StoreCode::Foreign,
                "untagged store already has tables; refusing to adopt",
            );
        }
        run_migrations(conn, 0)?;
        conn.execute_batch(&format!("PRAGMA application_id = {STORE_APPLICATION_ID}"))?;
        return Ok(());
    }
    if application_id != STORE_APPLICATION_ID {
        return refuse(
            StoreCode::Foreign,
            format!("foreign application_id {application_id}"),
        );
    }
    let user_version = pragma_int(conn, "PRAGMA user_version")?;
    if user_version > STORE_USER_VERSION {
        return refuse(
            StoreCode::UserVersionNewer,
            format!("user_version {user_version} > {STORE_USER_VERSION}"),
        );
    }
    verify_applied_migrations(conn, user_version)?;
    if user_version < i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX) {
        run_migrations(conn, user_version)?;
    }
    for table in Table::ALL {
        if !tables.iter().any(|name| name == table.name()) {
            return refuse(
                StoreCode::TornSchema,
                format!("missing required table {}", table.name()),
            );
        }
    }
    Ok(())
}

fn bind_or_check_scope(conn: &Connection, root_scope_digest: &str) -> Result<()> {
    let bound: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'root_scope_digest'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match bound {
        None => {
            conn.execute(
                "INSERT INTO store_meta (key, value) VALUES ('root_scope_digest', ?)",
                [root_scope_digest],
            )?;
            conn.execute_batch(
                "INSERT OR IGNORE INTO store_meta (key, value) VALUES ('controller_epoch', '0');\n\
                 INSERT OR IGNORE INTO store_meta (key, value) VALUES ('text_bytes', '0');",
            )?;
            Ok(())
        }
        Some(bound) if bound == root_scope_digest => Ok(()),
        Some(_) => refuse(
            StoreCode::ScopeMismatch,
            "root scope digest does not match the store",
        ),
    }
}

fn read_meta(conn: &Connection, key: &str) -> Result<String> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM store_meta WHERE key = ?", [key], |row| {
            row.get(0)
        })
        .optional()?;
    match value {
        Some(value) => Ok(value),
        None => refuse(
            StoreCode::MetaMissing,
            format!("store_meta key {key} missing"),
        ),
    }
}

fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = ?",
        params![key, value, value],
    )?;
    Ok(())
}

fn meta_number(conn: &Connection, key: &str) -> Result<u64> {
    // `Number(...)`: a corrupt counter reads as zero headroom, never a crash.
    Ok(read_meta(conn, key)?
        .trim()
        .parse::<u64>()
        .unwrap_or_default())
}

fn add_text_bytes(conn: &Connection, delta: i64) -> Result<()> {
    let current = i64::try_from(meta_number(conn, "text_bytes")?).unwrap_or(i64::MAX);
    write_meta(
        conn,
        "text_bytes",
        &current.saturating_add(delta).max(0).to_string(),
    )
}

fn count(conn: &Connection, table: Table) -> Result<u64> {
    // `table` is a fixed internal identifier, never caller input.
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {}", table.name()),
        [],
        |row| row.get(0),
    )?;
    Ok(from_sql_int(count))
}

// ---------------------------------------------------------------------------
// Hydration.
// ---------------------------------------------------------------------------

fn hydrate_projection(
    kind: ProjectionKind,
    phase: &str,
    intent: &str,
    outcome: Option<&str>,
    conditions_json: &str,
) -> Result<Projection> {
    let conditions = parse_json(conditions_json, "conditions")?;
    if !conditions.is_array() {
        return refuse(
            StoreCode::ConditionsCorrupt,
            "conditions column is not an array",
        );
    }
    let value =
        json!({ "phase": phase, "intent": intent, "outcome": outcome, "conditions": conditions });
    validate_value(kind, &value)
}

fn validate_value(kind: ProjectionKind, value: &Value) -> Result<Projection> {
    validate_projection_semantics(kind, value).map_err(|error| {
        StoreError::Reducer(ReducerError {
            code: super::reducer::ReducerCode::Projection,
            detail: format!("{kind:?} projection {error}"),
        })
    })
}

/// The latest persisted `RunTerminalized.data.outcome` (survives
/// compaction).
fn persisted_terminal_outcome(conn: &Connection, run_id: &str) -> Result<Option<String>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data_json FROM events WHERE run_id = ? AND type = 'RunTerminalized' ORDER BY sequence DESC LIMIT 1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(data) = data else { return Ok(None) };
    let data = parse_json(&data, "RunTerminalized data")?;
    Ok(match data.get("outcome") {
        None | Some(Value::Null) => None,
        Some(Value::String(outcome)) => Some(outcome.clone()),
        Some(other) => Some(other.to_string()),
    })
}

fn max_sequence(conn: &Connection, run_id: &str) -> Result<u64> {
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE run_id = ?",
        [run_id],
        |row| row.get(0),
    )?;
    Ok(from_sql_int(max))
}

/// One `attempts` row, column by column.
type AttemptColumns = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
);

/// An outbox row's claim columns: epoch, owner, host request id, intent.
type ClaimColumns = (Option<i64>, Option<String>, Option<String>, String);

struct RunRow {
    definition_digest: String,
    revision: i64,
    controller_epoch: i64,
    cancel_epoch: i64,
    phase: String,
    intent: String,
    outcome: Option<String>,
    conditions: String,
    host_cursor: Option<String>,
    terminal_outcome: Option<String>,
}

fn read_run_row(conn: &Connection, run_id: &str) -> Result<Option<RunRow>> {
    Ok(conn
        .query_row(
            "SELECT definition_digest, revision, controller_epoch, cancel_epoch, phase, intent, outcome, conditions, host_cursor, terminal_outcome FROM runs WHERE run_id = ?",
            [run_id],
            |row| {
                Ok(RunRow {
                    definition_digest: row.get(0)?,
                    revision: row.get(1)?,
                    controller_epoch: row.get(2)?,
                    cancel_epoch: row.get(3)?,
                    phase: row.get(4)?,
                    intent: row.get(5)?,
                    outcome: row.get(6)?,
                    conditions: row.get(7)?,
                    host_cursor: row.get(8)?,
                    terminal_outcome: row.get(9)?,
                })
            },
        )
        .optional()?)
}

/// A run's attempts in creation order, each projection validated.
fn load_attempts(conn: &Connection, run_id: &str) -> Result<Vec<AttemptRecord>> {
    let mut statement = conn.prepare(
        "SELECT attempt_id, node_id, operation_id, rlm_child_id, turn_id, settlement_digest, phase, intent, outcome, conditions, turn_json FROM attempts WHERE run_id = ? ORDER BY rowid",
    )?;
    let rows = statement
        .query_map([run_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<AttemptColumns>>>()?;
    let mut attempts = Vec::with_capacity(rows.len());
    for (
        attempt_id,
        node_id,
        operation_id,
        rlm_child_id,
        turn_id,
        settlement_digest,
        phase,
        intent,
        outcome,
        conditions,
        turn_json,
    ) in rows
    {
        let turn = match turn_json {
            Some(turn) => Some(validate_value(
                ProjectionKind::Turn,
                &parse_json(&turn, "turn_json")?,
            )?),
            None => None,
        };
        attempts.push(AttemptRecord {
            projection: hydrate_projection(
                ProjectionKind::Attempt,
                &phase,
                &intent,
                outcome.as_deref(),
                &conditions,
            )?,
            attempt_id,
            node_id,
            operation_id,
            rlm_child_id,
            turn_id,
            settlement_digest,
            turn,
        });
    }
    Ok(attempts)
}

/// Load and validate one run's aggregate (§11 boundary (2)): every
/// projection through the one validator, and the `RunTerminalized`
/// equality re-enforced for a terminal run.
fn load_run_context(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<(RunAggregate, DefinitionGraph)>> {
    let Some(run_row) = read_run_row(conn, run_id)? else {
        return Ok(None);
    };
    let definition: Option<String> = conn
        .query_row(
            "SELECT canonical_bytes FROM definitions WHERE definition_digest = ?",
            [&run_row.definition_digest],
            |row| row.get(0),
        )
        .optional()?;
    let Some(definition) = definition else {
        return refuse(
            StoreCode::DefinitionMissing,
            format!("run {run_id} references a missing definition"),
        );
    };
    let definition = wire::decode_definition(&parse_json(&definition, "definition")?)?;
    let graph = DefinitionGraph::of(&definition);

    let run = hydrate_projection(
        ProjectionKind::Run,
        &run_row.phase,
        &run_row.intent,
        run_row.outcome.as_deref(),
        &run_row.conditions,
    )?;
    if run.phase == "terminal" {
        let persisted = persisted_terminal_outcome(conn, run_id)?;
        if let Err(error) = check_terminalized_outcome(persisted.as_deref(), &run) {
            return refuse(
                StoreCode::TerminalOutcomeMismatch,
                format!("run {run_id} terminal outcome disagreement: {error}"),
            );
        }
        if run_row
            .terminal_outcome
            .as_ref()
            .is_some_and(|terminal| Some(terminal) != run.outcome.as_ref())
        {
            return refuse(
                StoreCode::TerminalOutcomeMismatch,
                format!("run {run_id} terminal_outcome column disagrees with normalized outcome"),
            );
        }
    }

    let mut nodes = Vec::with_capacity(graph.node_ids.len());
    for node_id in &graph.node_ids {
        let row: Option<(String, String, Option<String>, String)> = conn
            .query_row(
                "SELECT phase, intent, outcome, conditions FROM nodes WHERE run_id = ? AND node_id = ?",
                [run_id, node_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((phase, intent, outcome, conditions)) = row else {
            return refuse(
                StoreCode::NodeMissing,
                format!("run {run_id} node {node_id} missing"),
            );
        };
        nodes.push(NodeState {
            node_id: node_id.clone(),
            projection: hydrate_projection(
                ProjectionKind::Node,
                &phase,
                &intent,
                outcome.as_deref(),
                &conditions,
            )?,
        });
    }

    let attempts = load_attempts(conn, run_id)?;

    let aggregate = RunAggregate {
        run_id: run_id.to_string(),
        revision: from_sql_int(run_row.revision),
        controller_epoch: from_sql_int(run_row.controller_epoch),
        cancel_epoch: from_sql_int(run_row.cancel_epoch),
        run,
        nodes,
        attempts,
        host_cursor: run_row.host_cursor,
        last_controller_sequence: max_sequence(conn, run_id)?,
        applied_host_event_ids: Vec::new(),
    };
    revalidate_aggregate(&aggregate)?;
    Ok(Some((aggregate, graph)))
}

fn upsert_run_rows(conn: &Connection, aggregate: &RunAggregate, now: &str) -> Result<()> {
    let run = &aggregate.run;
    let terminal_outcome = if run.phase == "terminal" {
        run.outcome.as_deref()
    } else {
        None
    };
    conn.execute(
        "UPDATE runs SET revision = ?, controller_epoch = ?, cancel_epoch = ?, phase = ?, intent = ?, outcome = ?, conditions = ?, host_cursor = ?, terminal_outcome = ?, terminal_digest = ?, updated_at = ? WHERE run_id = ?",
        params![
            sql_int(aggregate.revision),
            sql_int(aggregate.controller_epoch),
            sql_int(aggregate.cancel_epoch),
            run.phase,
            run.intent,
            run.outcome,
            to_json(&run.conditions),
            aggregate.host_cursor,
            terminal_outcome,
            Option::<String>::None,
            now,
            aggregate.run_id,
        ],
    )?;
    for node in &aggregate.nodes {
        let projection = &node.projection;
        conn.execute(
            "UPDATE nodes SET phase = ?, intent = ?, outcome = ?, conditions = ? WHERE run_id = ? AND node_id = ?",
            params![
                projection.phase,
                projection.intent,
                projection.outcome,
                to_json(&projection.conditions),
                aggregate.run_id,
                node.node_id,
            ],
        )?;
    }
    for attempt in &aggregate.attempts {
        conn.execute(
            "INSERT INTO attempts (run_id, attempt_id, node_id, operation_id, rlm_child_id, turn_id, settlement_digest, phase, intent, outcome, conditions, turn_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(run_id, attempt_id) DO UPDATE SET operation_id = excluded.operation_id, rlm_child_id = excluded.rlm_child_id, turn_id = excluded.turn_id, settlement_digest = excluded.settlement_digest, phase = excluded.phase, intent = excluded.intent, outcome = excluded.outcome, conditions = excluded.conditions, turn_json = excluded.turn_json",
            params![
                aggregate.run_id,
                attempt.attempt_id,
                attempt.node_id,
                attempt.operation_id,
                attempt.rlm_child_id,
                attempt.turn_id,
                attempt.settlement_digest,
                attempt.projection.phase,
                attempt.projection.intent,
                attempt.projection.outcome,
                to_json(&attempt.projection.conditions),
                attempt.turn.as_ref().map(to_json),
            ],
        )?;
    }
    Ok(())
}

fn append_events(
    conn: &Connection,
    run_id: &str,
    events: &[Value],
    start: u64,
) -> Result<Vec<u64>> {
    let mut applied = Vec::with_capacity(events.len());
    let mut expected = start;
    for event in events {
        expected += 1;
        let sequence = event.get("sequence").and_then(Value::as_u64);
        if sequence != Some(expected) {
            return refuse(
                StoreCode::EventGap,
                format!(
                    "event sequence {} != expected {expected}",
                    event["sequence"]
                ),
            );
        }
        let number =
            |key: &str| sql_int(event.get(key).and_then(Value::as_u64).unwrap_or_default());
        conn.execute(
            "INSERT INTO events (run_id, sequence, event_id, revision, type, controller_epoch, cancel_epoch, recorded_at, data_json, digest) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                run_id,
                sql_int(expected),
                text_field(event, "eventId"),
                number("revision"),
                text_field(event, "type"),
                number("controllerEpoch"),
                number("cancelEpoch"),
                text_field(event, "recordedAt"),
                to_json(&event["data"]),
                text_field(event, "digest"),
            ],
        )?;
        applied.push(expected);
    }
    Ok(applied)
}

// ---------------------------------------------------------------------------
// The store.
// ---------------------------------------------------------------------------

/// The held writer fence: the epoch and the OS lock.
struct Writer {
    epoch: u64,
    lock: File,
}

/// One open store handle (the sole logical writer once
/// [`Store::acquire_writer`] succeeds).
pub struct Store {
    conn: Connection,
    root: PathBuf,
    db_path: PathBuf,
    root_scope_digest: String,
    clock: Option<Clock>,
    filesystem: Option<FilesystemProbe>,
    writer: Option<Writer>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("db_path", &self.db_path)
            .field(
                "writer_epoch",
                &self.writer.as_ref().map(|writer| writer.epoch),
            )
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating owner-only on first use) the store under
    /// `options.root`: hardened paths, WAL + `synchronous=FULL` +
    /// `foreign_keys`, integrity checks, the migration ledger verified, and
    /// the root scope bound or checked. Opening does not make this handle a
    /// writer.
    ///
    /// # Errors
    ///
    /// Every hardening, integrity, migration, or scope violation (the
    /// handle is never returned half-open).
    pub fn open(options: StoreOptions) -> Result<Store> {
        assert_digest(&options.root_scope_digest, "rootScopeDigest")?;
        pa_core::platform::perms::create_dir_all_private(&options.root)?;
        let canonical_root = assert_hardened_dir(&options.root)?;
        let db_path = assert_hardened_file(&canonical_root, STORE_FILE_NAME)?;
        // Create the file owner-only before SQLite does: its WAL and shared
        // memory files copy the database file's mode.
        drop(open_private(&db_path)?);

        let mut conn = Connection::open(&db_path)?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        apply_pragmas(&conn)?;
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", db_path.display()));
            match pa_core::platform::perms::restrict_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
        assert_integrity(&conn)?;
        initialize_or_validate_schema(&mut conn)?;
        bind_or_check_scope(&conn, &options.root_scope_digest)?;
        fsync_dir(&canonical_root)?;
        Ok(Store {
            conn,
            root: canonical_root,
            db_path,
            root_scope_digest: options.root_scope_digest,
            clock: options.clock,
            filesystem: options.filesystem,
            writer: None,
        })
    }

    /// The database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.db_path
    }

    fn now(&self) -> String {
        self.clock
            .as_ref()
            .map_or_else(system_clock, |clock| clock())
    }

    /// Become the sole writer at a strictly higher `controllerEpoch`: take
    /// the exclusive OS lock (held until this handle drops), then raise the
    /// durable epoch.
    ///
    /// # Errors
    ///
    /// `store_epoch_range`, `store_writer_locked` (another handle holds the
    /// lock), or `store_epoch_stale` (`epoch` is not above the current one).
    pub fn acquire_writer(&mut self, epoch: u64) -> Result<u64> {
        if !(1..=MAX_SAFE).contains(&epoch) {
            return refuse(
                StoreCode::EpochRange,
                format!("controllerEpoch out of range: {epoch}"),
            );
        }
        let previous = self.writer.take();
        let previous_epoch = previous.as_ref().map(|writer| writer.epoch);
        let lock = if let Some(writer) = previous {
            writer.lock
        } else {
            let path = assert_hardened_file(&self.root, WRITER_LOCK_FILE_NAME)?;
            let file = open_private(&path)?;
            match file.try_lock() {
                Ok(()) => file,
                Err(std::fs::TryLockError::WouldBlock) => {
                    return refuse(
                        StoreCode::WriterLocked,
                        "another writer holds the store lock",
                    )
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        };
        let raised = Self::raise_epoch(&mut self.conn, &self.root, epoch);
        // A refused raise keeps what this handle already held: its earlier
        // epoch and the lock, or neither.
        self.writer = match (&raised, previous_epoch) {
            (Ok(()), _) => Some(Writer { epoch, lock }),
            (Err(_), Some(previous)) => Some(Writer {
                epoch: previous,
                lock,
            }),
            (Err(_), None) => None,
        };
        raised.map(|()| epoch)
    }

    fn raise_epoch(conn: &mut Connection, root: &Path, epoch: u64) -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = meta_number(&tx, "controller_epoch")?;
        if epoch <= current {
            return refuse(
                StoreCode::EpochStale,
                format!("controllerEpoch {epoch} <= current {current}"),
            );
        }
        write_meta(&tx, "controller_epoch", &epoch.to_string())?;
        Self::commit(root, tx)
    }

    fn writer_epoch(&self) -> Result<u64> {
        match &self.writer {
            Some(writer) => Ok(writer.epoch),
            None => refuse(StoreCode::NotAcquired, "writer epoch not acquired"),
        }
    }

    /// Begin one writer transaction: `BEGIN IMMEDIATE`, then the epoch
    /// fence (a newer writer supersedes this handle).
    fn begin(&mut self) -> Result<(Transaction<'_>, u64)> {
        let epoch = self.writer_epoch()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = meta_number(&tx, "controller_epoch")?;
        if current != epoch {
            return refuse(
                StoreCode::EpochStale,
                format!("writer epoch {epoch} superseded by {current}"),
            );
        }
        Ok((tx, epoch))
    }

    fn commit(root: &Path, tx: Transaction<'_>) -> Result<()> {
        tx.commit()?;
        fsync_dir(root)
    }

    // ---- validate (pure) --------------------------------------------------

    /// §6 `validate`: strict structural and semantic validation only; it
    /// writes nothing. Answers the request id.
    ///
    /// # Errors
    ///
    /// The decode verdict, or `store_not_validate` for another action.
    pub fn validate(&self, request: &Value) -> Result<String> {
        let decoded = wire::decode_public_request(request)?;
        if decoded.action != Action::Validate {
            return refuse(
                StoreCode::NotValidate,
                "validate() requires a validate action",
            );
        }
        Ok(decoded.request_id)
    }

    // ---- command transaction ---------------------------------------------

    /// §6/§7 command transaction (see the module docs).
    ///
    /// # Errors
    ///
    /// The decode verdict; `CAPACITY_EXCEEDED` (before any effect);
    /// `IDEMPOTENCY_CONFLICT`; `NOT_FOUND`; `COMMAND_FENCE_STALE`;
    /// `store_event_gap`; a reducer refusal; any binding/outbox validation
    /// failure. Every refusal leaves zero durable effect.
    #[allow(clippy::too_many_lines)] // one transaction, step by step as §7 orders it
    pub fn apply_command(
        &mut self,
        request: &Value,
        effect: &CommandEffect,
    ) -> Result<CommandReceipt> {
        self.writer_epoch()?;
        let decoded = wire::decode_public_request(request)?;
        let action = decoded.action;
        match action {
            Action::Validate => {
                return refuse(
                    StoreCode::ValidateNotMutation,
                    "use validate() for the validate action",
                )
            }
            Action::Status | Action::Events => {
                return refuse(
                    StoreCode::NotMutation,
                    format!("{} is not a mutation", action.wire_name()),
                )
            }
            Action::Create | Action::Start | Action::Cancel | Action::Retry => {}
        }
        let canonical_bytes = wire::canonical_json(request)?;
        let request_digest = wire::sha256_digest(canonical_bytes.as_bytes());
        let now = self.now();
        self.capacity_guard(action)?;

        let root = self.root.clone();
        let (tx, _) = self.begin()?;
        // Identical canonical bytes: the stored receipt, nothing written.
        let existing: Option<String> = tx
            .query_row(
                "SELECT receipt_json FROM commands WHERE request_digest = ?",
                [&request_digest],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let receipt: CommandReceipt =
                serde_json::from_str(&existing).map_err(|error| StoreError::Refused {
                    code: StoreCode::JsonCorrupt,
                    message: format!("receipt_json is not a receipt: {error}"),
                })?;
            tx.commit()?;
            return Ok(receipt);
        }
        // `requestId` is the sole idempotency key: changed bytes conflict.
        let prior: Option<String> = tx
            .query_row(
                "SELECT request_digest FROM commands WHERE request_id = ?",
                [&decoded.request_id],
                |row| row.get(0),
            )
            .optional()?;
        if prior.is_some_and(|prior| prior != request_digest) {
            return refuse(
                StoreCode::IdempotencyConflict,
                format!(
                    "requestId {} reused with different bytes",
                    decoded.request_id
                ),
            );
        }

        let is_create = action == Action::Create;
        let mut prev: Option<RunAggregate> = None;
        let graph: DefinitionGraph;
        let run_id: String;
        let mut command_id: Option<String> = None;
        if is_create {
            let Some(definition) = &effect.definition else {
                return refuse(
                    StoreCode::CreateNoDefinition,
                    "create requires effect.definition",
                );
            };
            graph = DefinitionGraph::of(&wire::decode_definition(definition)?);
            let Some(first) = effect.events.first() else {
                return refuse(
                    StoreCode::CreateNoEvents,
                    "create requires a RunAdmitted event",
                );
            };
            run_id = text_field(first, "runId").to_string();
        } else {
            run_id = decoded.run_id.clone().unwrap_or_default();
            let command = text_field(request, "commandId").to_string();
            let prior: Option<String> = tx
                .query_row(
                    "SELECT request_digest FROM commands WHERE run_id = ? AND command_id = ?",
                    [&run_id, &command],
                    |row| row.get(0),
                )
                .optional()?;
            if prior.is_some_and(|prior| prior != request_digest) {
                return refuse(
                    StoreCode::IdempotencyConflict,
                    format!("command {command} reused with different bytes"),
                );
            }
            let Some((aggregate, run_graph)) = load_run_context(&tx, &run_id)? else {
                return refuse(StoreCode::NotFound, format!("run {run_id} not found"));
            };
            let fence = |key: &str| request.get(key).and_then(Value::as_u64);
            if fence("expectedRevision") != Some(aggregate.revision)
                || fence("expectedControllerEpoch") != Some(aggregate.controller_epoch)
                || fence("expectedCancelEpoch") != Some(aggregate.cancel_epoch)
            {
                return refuse(
                    StoreCode::CommandFenceStale,
                    format!("stale fence for run {run_id}"),
                );
            }
            command_id = Some(command);
            graph = run_graph;
            prev = Some(aggregate);
        }

        // Fold the facts through the pure reducer (every projection
        // validated).
        let mut state = prev.clone();
        for event in &effect.events {
            if text_field(event, "runId") != run_id {
                return refuse(StoreCode::EventRunMismatch, "event runId mismatch");
            }
            state = Some(reduce_fact(state.as_ref(), event, &graph)?);
        }
        let Some(state) = state else {
            return refuse(StoreCode::CreateNoEvents, "command produced no run state");
        };
        let prev_max = prev
            .as_ref()
            .map_or(0, |prev| prev.last_controller_sequence);

        if let (true, Some(definition)) = (is_create, &effect.definition) {
            let canonical_definition = wire::canonical_json(definition)?;
            let definition_digest = wire::sha256_digest(canonical_definition.as_bytes());
            let definition_bytes = utf8_len(&canonical_definition);
            tx.execute(
                "INSERT OR IGNORE INTO definitions (definition_digest, canonical_bytes, utf8_bytes, created_at) VALUES (?, ?, ?, ?)",
                params![definition_digest, to_json(definition), sql_int(definition_bytes), now],
            )?;
            add_text_bytes(&tx, sql_int(definition_bytes))?;
            tx.execute(
                "INSERT INTO runs (run_id, definition_digest, revision, controller_epoch, cancel_epoch, phase, intent, outcome, conditions, host_cursor, terminal_outcome, terminal_digest, erased, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
                params![
                    run_id,
                    definition_digest,
                    sql_int(state.revision),
                    sql_int(state.controller_epoch),
                    sql_int(state.cancel_epoch),
                    state.run.phase,
                    state.run.intent,
                    state.run.outcome,
                    to_json(&state.run.conditions),
                    state.host_cursor,
                    Option::<String>::None,
                    Option::<String>::None,
                    now,
                    now,
                ],
            )?;
            for node in &state.nodes {
                tx.execute(
                    "INSERT INTO nodes (run_id, node_id, phase, intent, outcome, conditions) VALUES (?, ?, ?, ?, ?, ?)",
                    params![
                        run_id,
                        node.node_id,
                        node.projection.phase,
                        node.projection.intent,
                        node.projection.outcome,
                        to_json(&node.projection.conditions),
                    ],
                )?;
            }
            tx.execute(
                "INSERT INTO host_streams (run_id, host_cursor, updated_at) VALUES (?, ?, ?)",
                params![run_id, state.host_cursor, now],
            )?;
            let budget = effect.budget.unwrap_or(RunBudget {
                max_concurrent_attempts: definition["budget"]["maxConcurrentAttempts"]
                    .as_u64()
                    .unwrap_or_default(),
                max_total_tokens: definition["budget"]["maxTotalTokens"]
                    .as_u64()
                    .unwrap_or_default(),
            });
            tx.execute(
                "INSERT INTO budgets (run_id, max_concurrent_attempts, max_total_tokens, reserved_tokens, settled_tokens, updated_at) VALUES (?, ?, ?, 0, 0, ?)",
                params![
                    run_id,
                    sql_int(budget.max_concurrent_attempts),
                    sql_int(budget.max_total_tokens),
                    now
                ],
            )?;
        }

        upsert_run_rows(&tx, &state, &now)?;
        let applied_sequences = append_events(&tx, &run_id, &effect.events, prev_max)?;

        for binding in &effect.bindings {
            tx.execute(
                "INSERT INTO child_turn_bindings (run_id, attempt_id, workflow_child_id, rlm_child_id, turn_id, request_id, canonical_digest, bound_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    run_id,
                    assert_id(&binding.attempt_id, "attemptId")?,
                    assert_id(&binding.workflow_child_id, "workflowChildId")?,
                    assert_id(&binding.rlm_child_id, "rlmChildId")?,
                    assert_id(&binding.turn_id, "turnId")?,
                    assert_id(&binding.request_id, "requestId")?,
                    assert_digest(&binding.canonical_digest, "canonicalDigest")?,
                    now,
                ],
            )?;
        }
        for decision in &effect.acceptance {
            tx.execute(
                "INSERT INTO acceptance (run_id, node_id, attempt_id, decision, evidence_digest, decided_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(run_id, node_id) DO UPDATE SET attempt_id = excluded.attempt_id, decision = excluded.decision, evidence_digest = excluded.evidence_digest, decided_at = excluded.decided_at",
                params![
                    run_id,
                    assert_id(&decision.node_id, "nodeId")?,
                    decision
                        .attempt_id
                        .as_deref()
                        .map(|id| assert_id(id, "attemptId"))
                        .transpose()?,
                    decision.decision.as_str(),
                    decision
                        .evidence_digest
                        .as_deref()
                        .map(|digest| assert_digest(digest, "evidenceDigest"))
                        .transpose()?,
                    now,
                ],
            )?;
        }
        for tombstone in &effect.tombstones {
            tx.execute(
                "INSERT INTO tombstones (run_id, rlm_child_id, request_id, tombstone_digest, created_at) VALUES (?, ?, ?, ?, ?)",
                params![
                    run_id,
                    assert_id(&tombstone.rlm_child_id, "rlmChildId")?,
                    assert_id(&tombstone.request_id, "requestId")?,
                    assert_digest(&tombstone.tombstone_digest, "tombstoneDigest")?,
                    now,
                ],
            )?;
        }

        // Stable outbox operations: pending, claim unheld.
        let pending = ["claim_unheld", "receipt_absent", "effect_unclassified"];
        for operation in &effect.operations {
            let intent = operation.kind.intent();
            validate_value(
                ProjectionKind::Operation,
                &json!({ "phase": "pending", "intent": intent, "outcome": null, "conditions": pending }),
            )?;
            tx.execute(
                "INSERT INTO operations (operation_id, run_id, attempt_id, kind, request_id, canonical_request, host_request_id, phase, intent, outcome, conditions, claim_epoch, claim_owner, claim_expires_at, receipt_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, 'pending', ?, NULL, ?, NULL, NULL, NULL, NULL, ?, ?)",
                params![
                    assert_id(&operation.operation_id, "operationId")?,
                    run_id,
                    operation.attempt_id,
                    operation.kind.as_str(),
                    assert_id(&operation.request_id, "requestId")?,
                    wire::canonical_json(&operation.canonical_request)?,
                    intent,
                    to_json(&pending),
                    now,
                    now,
                ],
            )?;
        }

        let receipt = CommandReceipt {
            action: action.wire_name().to_string(),
            request_id: decoded.request_id,
            run_id: Some(run_id.clone()),
            command_id: command_id.clone(),
            request_digest: request_digest.clone(),
            revision: state.revision,
            controller_epoch: state.controller_epoch,
            cancel_epoch: state.cancel_epoch,
            applied_sequences,
            recorded_at: now.clone(),
        };
        tx.execute(
            "INSERT INTO commands (request_digest, request_id, run_id, command_id, action, canonical_bytes, receipt_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                request_digest,
                receipt.request_id,
                (!is_create).then_some(run_id),
                command_id,
                receipt.action,
                canonical_bytes,
                to_json(&receipt),
                now,
            ],
        )?;
        Self::commit(&root, tx)?;
        Ok(receipt)
    }

    /// Refuse `create`/`start`/`retry` before any durable effect at the 90%
    /// high-water mark of any quota, the DB+WAL cap, or the free-space
    /// floor. Settlement, cancel, inbox, and reconciliation are exempt.
    fn capacity_guard(&self, action: Action) -> Result<()> {
        if !matches!(action, Action::Create | Action::Start | Action::Retry) {
            return Ok(());
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let high_water =
            |quota: u64| (quota as f64 * STORE_CAPACITY.high_water_fraction).floor() as u64;
        let checks = [
            (
                "runs",
                count(&self.conn, Table::Runs)?,
                STORE_CAPACITY.max_runs,
            ),
            (
                "events",
                count(&self.conn, Table::Events)?,
                STORE_CAPACITY.max_events,
            ),
            (
                "commands",
                count(&self.conn, Table::Commands)?,
                STORE_CAPACITY.max_commands,
            ),
            (
                "operations",
                count(&self.conn, Table::Operations)?,
                STORE_CAPACITY.max_operations,
            ),
            (
                "host_inbox",
                count(&self.conn, Table::HostInbox)?,
                STORE_CAPACITY.max_host_inbox,
            ),
            (
                "settlements",
                count(&self.conn, Table::Settlements)?,
                STORE_CAPACITY.max_settlements,
            ),
            (
                "tombstones",
                count(&self.conn, Table::Tombstones)?,
                STORE_CAPACITY.max_tombstones,
            ),
            (
                "text_bytes",
                meta_number(&self.conn, "text_bytes")?,
                STORE_CAPACITY.max_text_bytes,
            ),
        ];
        for (name, value, quota) in checks {
            if value >= high_water(quota) {
                return refuse(
                    StoreCode::CapacityExceeded,
                    format!("high-water for {name}: {value}/{quota}"),
                );
            }
        }
        let mut db_bytes = 0;
        for suffix in ["", "-wal"] {
            match std::fs::symlink_metadata(format!("{}{suffix}", self.db_path.display())) {
                Ok(metadata) => db_bytes += metadata.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if db_bytes >= high_water(STORE_CAPACITY.max_db_bytes) {
            return refuse(StoreCode::CapacityExceeded, "db+wal high-water");
        }
        // A missing statfs fabricates no headroom; the quotas above gate.
        // (TS computed this floor but its refusal sat inside the `try` whose
        // `catch` swallowed it, so it never refused; §7 requires it.)
        let space = match &self.filesystem {
            Some(probe) => probe(&self.db_path),
            None => statvfs_space(&self.db_path),
        };
        if let Some(FilesystemSpace { free, total }) = space {
            let floor = MIN_FREE_BYTES.max(total / 10);
            if free < floor {
                return refuse(
                    StoreCode::CapacityExceeded,
                    format!("free space {free} < floor {floor}"),
                );
            }
        }
        Ok(())
    }

    // ---- host inbox --------------------------------------------------------

    /// §7 host-page ingestion in one `BEGIN IMMEDIATE`: dedup by
    /// `hostEventId`, reduce the fact (every projection validated, §11
    /// boundary (3)), persist the inbox row and any settlement evidence,
    /// and advance `hostCursor` in the same transaction. A duplicate is a
    /// no-op that never regresses the cursor. The controller names the
    /// owning run (host facts address requests, children, and turns).
    ///
    /// # Errors
    ///
    /// `NOT_FOUND`, a reducer refusal, or a settlement-shape violation; the
    /// cursor and every row stay as they were.
    pub fn ingest_host_event(&mut self, run_id: &str, event: &Value) -> Result<Ingestion> {
        let root = self.root.clone();
        let clock = self.clock.clone();
        let now = || clock.as_ref().map_or_else(system_clock, |clock| clock());
        let (tx, _) = self.begin()?;
        let Some((aggregate, graph)) = load_run_context(&tx, run_id)? else {
            return refuse(StoreCode::NotFound, format!("run {run_id} not found"));
        };
        let host_event_id = text_field(event, "hostEventId");
        let duplicate: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM host_inbox WHERE host_event_id = ?",
                [host_event_id],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            tx.commit()?;
            return Ok(Ingestion {
                applied: false,
                host_cursor: aggregate.host_cursor,
            });
        }
        let state = reduce_fact(Some(&aggregate), event, &graph)?;
        tx.execute(
            "INSERT INTO host_inbox (host_event_id, run_id, host_cursor, type, fact_json, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                host_event_id,
                run_id,
                text_field(event, "hostCursor"),
                text_field(event, "type"),
                to_json(event),
                now(),
            ],
        )?;
        if text_field(event, "type") == "TurnSettled" {
            persist_settlement(&tx, run_id, &event["data"]["settlement"], &now())?;
        }
        upsert_run_rows(&tx, &state, &now())?;
        tx.execute(
            "UPDATE host_streams SET host_cursor = ?, updated_at = ? WHERE run_id = ?",
            params![state.host_cursor, now(), run_id],
        )?;
        Self::commit(&root, tx)?;
        Ok(Ingestion {
            applied: true,
            host_cursor: state.host_cursor,
        })
    }

    // ---- outbox ------------------------------------------------------------

    /// §7 outbox claim: pending, `retry_wait`, and lease-expired claimed
    /// operations, oldest first, for the current writer epoch with an owner
    /// and a lease deadline. Redelivery hands back the **same** canonical
    /// request and host request id; it never mints a new effect.
    ///
    /// # Errors
    ///
    /// Not the writer, an invalid owner id, or a store failure.
    pub fn claim_outbox(
        &mut self,
        owner: &str,
        lease_ms: u64,
        limit: u64,
    ) -> Result<Vec<ClaimedOperation>> {
        let owner = assert_id(owner, "ownerId")?.to_string();
        let now = self.now();
        let Some(now_ms) = parse_iso_millis(&now) else {
            return refuse(StoreCode::TimeInvalid, format!("clock answered {now}"));
        };
        let expires_at = pa_core::session::manager::format_iso(
            now_ms.saturating_add(i64::try_from(lease_ms).unwrap_or(i64::MAX)),
        );
        let root = self.root.clone();
        let (tx, epoch) = self.begin()?;
        let rows = {
            let mut statement = tx.prepare(
                "SELECT operation_id, run_id, attempt_id, kind, request_id, canonical_request, host_request_id, intent FROM operations WHERE phase = 'pending' OR phase = 'retry_wait' OR (phase = 'claimed' AND claim_expires_at < ?) ORDER BY rowid LIMIT ?",
            )?;
            let rows = statement
                .query_map(params![now, sql_int(limit)], |row| {
                    Ok((
                        ClaimedOperation {
                            operation_id: row.get(0)?,
                            run_id: row.get(1)?,
                            attempt_id: row.get(2)?,
                            kind: row.get(3)?,
                            request_id: row.get(4)?,
                            canonical_request: row.get(5)?,
                            host_request_id: row.get(6)?,
                            claim_epoch: epoch,
                            claim_expires_at: expires_at.clone(),
                        },
                        row.get::<_, String>(7)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let held = ["claim_held", "receipt_absent", "effect_unclassified"];
        let mut claimed = Vec::with_capacity(rows.len());
        for (operation, intent) in rows {
            validate_value(
                ProjectionKind::Operation,
                &json!({ "phase": "claimed", "intent": intent, "outcome": null, "conditions": held }),
            )?;
            tx.execute(
                "UPDATE operations SET phase = 'claimed', claim_epoch = ?, claim_owner = ?, claim_expires_at = ?, conditions = ?, updated_at = ? WHERE operation_id = ?",
                params![sql_int(epoch), owner, expires_at, to_json(&held), now, operation.operation_id],
            )?;
            claimed.push(operation);
        }
        Self::commit(&root, tx)?;
        Ok(claimed)
    }

    /// §7 outbox acknowledgement: only the current epoch's claim owner
    /// terminalizes an operation (classified effect, immutable receipt);
    /// the host request id binds once.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND`, `STALE_CLAIM`, `OPERATION_HOST_ID_CONFLICT`, or an
    /// invalid host request id.
    pub fn acknowledge_outbox(&mut self, acknowledgement: &Acknowledgement) -> Result<()> {
        let now = self.now();
        let root = self.root.clone();
        let (tx, epoch) = self.begin()?;
        let row: Option<ClaimColumns> = tx
            .query_row(
                "SELECT claim_epoch, claim_owner, host_request_id, intent FROM operations WHERE operation_id = ?",
                [&acknowledgement.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((claim_epoch, claim_owner, host_request_id, intent)) = row else {
            return refuse(
                StoreCode::NotFound,
                format!("operation {} not found", acknowledgement.operation_id),
            );
        };
        if claim_epoch.map(from_sql_int) != Some(epoch)
            || claim_owner.as_deref() != Some(acknowledgement.owner.as_str())
        {
            return refuse(
                StoreCode::StaleClaim,
                format!("operation {} claim is stale", acknowledgement.operation_id),
            );
        }
        if host_request_id.is_some_and(|bound| bound != acknowledgement.host_request_id) {
            return refuse(
                StoreCode::OperationHostIdConflict,
                "host request id rebinding is forbidden",
            );
        }
        let terminal = ["claim_held", "receipt_present", "effect_classified"];
        validate_value(
            ProjectionKind::Operation,
            &json!({
                "phase": "terminal",
                "intent": intent,
                "outcome": acknowledgement.outcome.as_str(),
                "conditions": terminal,
            }),
        )?;
        tx.execute(
            "UPDATE operations SET phase = 'terminal', outcome = ?, conditions = ?, host_request_id = ?, receipt_json = ?, updated_at = ? WHERE operation_id = ?",
            params![
                acknowledgement.outcome.as_str(),
                to_json(&terminal),
                assert_id(&acknowledgement.host_request_id, "hostRequestId")?,
                to_json(&acknowledgement.receipt),
                now,
                acknowledgement.operation_id,
            ],
        )?;
        Self::commit(&root, tx)
    }

    // ---- bounded reads (diagnostic; the public status/events API is slice 8)

    /// The run's validated projection.
    ///
    /// # Errors
    ///
    /// A hydration refusal (corrupt rows, terminal-outcome mismatch).
    pub fn run_projection(&self, run_id: &str) -> Result<Option<Projection>> {
        Ok(load_run_context(&self.conn, run_id)?.map(|(aggregate, _)| aggregate.run))
    }

    /// The run's validated aggregate.
    ///
    /// # Errors
    ///
    /// A hydration refusal.
    pub fn load_aggregate(&self, run_id: &str) -> Result<Option<RunAggregate>> {
        Ok(load_run_context(&self.conn, run_id)?.map(|(aggregate, _)| aggregate))
    }

    /// The stored receipt for a request digest.
    ///
    /// # Errors
    ///
    /// A store failure or a corrupt receipt.
    pub fn command_receipt(&self, request_digest: &str) -> Result<Option<CommandReceipt>> {
        let receipt: Option<String> = self
            .conn
            .query_row(
                "SELECT receipt_json FROM commands WHERE request_digest = ?",
                [request_digest],
                |row| row.get(0),
            )
            .optional()?;
        receipt
            .map(|receipt| {
                serde_json::from_str(&receipt).map_err(|error| StoreError::Refused {
                    code: StoreCode::JsonCorrupt,
                    message: format!("receipt_json is not a receipt: {error}"),
                })
            })
            .transpose()
    }

    /// Up to `limit` (1..=500) controller events after `after_sequence`.
    ///
    /// # Errors
    ///
    /// `SNAPSHOT_REQUIRED` when the cursor falls inside a compacted prefix.
    pub fn list_events(
        &self,
        run_id: &str,
        after_sequence: u64,
        limit: u64,
    ) -> Result<Vec<EventRow>> {
        let snapshot: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM store_meta WHERE key = ?",
                [format!("snapshot:{run_id}")],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(snapshot) = snapshot {
            let through = parse_json(&snapshot, "snapshot")?["throughSequence"]
                .as_u64()
                .unwrap_or_default();
            if after_sequence < through {
                return refuse(
                    StoreCode::SnapshotRequired,
                    format!("events before {through} are compacted"),
                );
            }
        }
        let mut statement = self.conn.prepare(
            "SELECT sequence, event_id, revision, type, controller_epoch, cancel_epoch, recorded_at, data_json, digest FROM events WHERE run_id = ? AND sequence > ? ORDER BY sequence LIMIT ?",
        )?;
        let rows = statement
            .query_map(
                params![
                    run_id,
                    sql_int(after_sequence),
                    sql_int(limit.clamp(1, 500))
                ],
                |row| {
                    Ok(EventRow {
                        sequence: from_sql_int(row.get(0)?),
                        event_id: row.get(1)?,
                        revision: from_sql_int(row.get(2)?),
                        kind: row.get(3)?,
                        controller_epoch: from_sql_int(row.get(4)?),
                        cancel_epoch: from_sql_int(row.get(5)?),
                        recorded_at: row.get(6)?,
                        data_json: row.get(7)?,
                        digest: row.get(8)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One outbox row.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub fn operation(&self, operation_id: &str) -> Result<Option<OperationRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT operation_id, run_id, attempt_id, kind, request_id, canonical_request, host_request_id, phase, intent, outcome, conditions, claim_epoch, claim_owner, claim_expires_at, receipt_json, created_at, updated_at FROM operations WHERE operation_id = ?",
                [operation_id],
                |row| {
                    Ok(OperationRow {
                        operation_id: row.get(0)?,
                        run_id: row.get(1)?,
                        attempt_id: row.get(2)?,
                        kind: row.get(3)?,
                        request_id: row.get(4)?,
                        canonical_request: row.get(5)?,
                        host_request_id: row.get(6)?,
                        phase: row.get(7)?,
                        intent: row.get(8)?,
                        outcome: row.get(9)?,
                        conditions: row.get(10)?,
                        claim_epoch: row.get::<_, Option<i64>>(11)?.map(from_sql_int),
                        claim_owner: row.get(12)?,
                        claim_expires_at: row.get(13)?,
                        receipt_json: row.get(14)?,
                        created_at: row.get(15)?,
                        updated_at: row.get(16)?,
                    })
                },
            )
            .optional()?)
    }

    /// The run's committed host cursor.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub fn host_cursor(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT host_cursor FROM host_streams WHERE run_id = ?",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Rows in one minimum table.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub fn table_count(&self, table: Table) -> Result<u64> {
        count(&self.conn, table)
    }

    // ---- recovery, retention, backup --------------------------------------

    /// §11/§9 recovery: durably quarantine a terminal run whose persisted
    /// `RunTerminalized` outcome (or `terminal_outcome` column) disagrees
    /// with the normalized outcome — `quarantined`, `integrity_failed`, the
    /// terminal outcome cleared, and a gap-free `RunQuarantined` fact.
    /// Answers whether it quarantined. (As in TS, the run row's `revision`
    /// stays; the fact carries `revision + 1`.)
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` or a store failure.
    pub fn reconcile_terminal_mismatch(&mut self, run_id: &str) -> Result<bool> {
        let now = self.now();
        let root = self.root.clone();
        let (tx, _) = self.begin()?;
        let Some(row) = read_run_row(&tx, run_id)? else {
            return refuse(StoreCode::NotFound, format!("run {run_id} not found"));
        };
        if row.phase != "terminal" {
            tx.commit()?;
            return Ok(false);
        }
        let persisted = persisted_terminal_outcome(&tx, run_id)?;
        let column_agrees = row
            .terminal_outcome
            .as_ref()
            .is_none_or(|terminal| Some(terminal) == row.outcome.as_ref());
        if persisted == row.outcome && column_agrees {
            tx.commit()?;
            return Ok(false);
        }
        let prior = parse_json(&row.conditions, "conditions")?;
        let mut conditions: Vec<Value> = prior
            .as_array()
            .into_iter()
            .flatten()
            .filter(|condition| *condition != "integrity_verified")
            .cloned()
            .collect();
        if !conditions
            .iter()
            .any(|condition| condition == "integrity_failed")
        {
            conditions.push(Value::String("integrity_failed".to_string()));
        }
        let quarantined = validate_value(
            ProjectionKind::Run,
            &json!({ "phase": "quarantined", "intent": row.intent, "outcome": null, "conditions": conditions }),
        )?;
        tx.execute(
            "UPDATE runs SET phase = 'quarantined', outcome = NULL, conditions = ?, terminal_outcome = NULL, updated_at = ? WHERE run_id = ?",
            params![to_json(&quarantined.conditions), now, run_id],
        )?;
        let next = max_sequence(&tx, run_id)? + 1;
        let data = json!({
            "evidenceDigest": wire::sha256_digest(format!("{run_id}:{next}:quarantine").as_bytes()),
        });
        let data_json = to_json(&data);
        tx.execute(
            "INSERT INTO events (run_id, sequence, event_id, revision, type, controller_epoch, cancel_epoch, recorded_at, data_json, digest) VALUES (?, ?, ?, ?, 'RunQuarantined', ?, ?, ?, ?, ?)",
            params![
                run_id,
                sql_int(next),
                format!("reconcile-quarantine-{run_id}-{next}"),
                row.revision + 1,
                row.controller_epoch,
                row.cancel_epoch,
                now,
                data_json,
                wire::sha256_digest(data_json.as_bytes()),
            ],
        )?;
        Self::commit(&root, tx)?;
        Ok(true)
    }

    /// §7 erasure of a terminal run's result text: each text result becomes
    /// an erasure marker keeping its UTF-8 byte count and digest; the text
    /// counter drops by the freed bytes. Answers the freed byte count.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND`, `ERASURE_REFUSED` (nonterminal or quarantined run), or a
    /// store failure.
    pub fn erase_run_text(&mut self, run_id: &str, policy_basis: &str) -> Result<u64> {
        let now = self.now();
        let root = self.root.clone();
        let (tx, _) = self.begin()?;
        let phase: Option<String> = tx
            .query_row("SELECT phase FROM runs WHERE run_id = ?", [run_id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(phase) = phase else {
            return refuse(StoreCode::NotFound, format!("run {run_id} not found"));
        };
        if phase != "terminal" {
            return refuse(
                StoreCode::ErasureRefused,
                format!("run {run_id} is not terminal ({phase})"),
            );
        }
        let rows = {
            let mut statement = tx.prepare(
                "SELECT settlement_digest, result_kind, result_utf8_bytes, settlement_json FROM settlements WHERE run_id = ?",
            )?;
            let rows = statement
                .query_map([run_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut freed: u64 = 0;
        for (digest, kind, bytes, settlement) in rows {
            if kind != "text" {
                continue;
            }
            let mut settlement = parse_json(&settlement, "settlement_json")?;
            let Some(sha256) = settlement["result"]
                .get("text")
                .filter(|text| text.is_string())
                .and(settlement["result"].get("sha256").cloned())
            else {
                continue;
            };
            let bytes = from_sql_int(bytes.unwrap_or_default());
            settlement["result"] = json!({
                "kind": "erased",
                "utf8Bytes": bytes,
                "sha256": sha256,
                "erasedAt": now,
                "policyBasis": policy_basis,
            });
            tx.execute(
                "UPDATE settlements SET settlement_json = ? WHERE settlement_digest = ?",
                params![to_json(&settlement), digest],
            )?;
            freed += bytes;
        }
        tx.execute(
            "UPDATE runs SET erased = 1, updated_at = ? WHERE run_id = ?",
            params![now, run_id],
        )?;
        if freed > 0 {
            add_text_bytes(&tx, -sql_int(freed))?;
        }
        Self::commit(&root, tx)?;
        Ok(freed)
    }

    /// §7 fenced compaction: once every operation of the run is terminal,
    /// blank nonterminal controller-event payloads through
    /// `through_sequence` (terminal and quarantine facts are immutable
    /// evidence) and record the snapshot; a later read of the compacted
    /// prefix answers `SNAPSHOT_REQUIRED`. Answers the compacted row count.
    ///
    /// # Errors
    ///
    /// An invalid digest, `NOT_FOUND`, `COMPACTION_REFUSED`, or a store
    /// failure.
    pub fn compact_events(
        &mut self,
        run_id: &str,
        through_sequence: u64,
        snapshot_digest: &str,
    ) -> Result<u64> {
        assert_digest(snapshot_digest, "snapshotDigest")?;
        let root = self.root.clone();
        let (tx, _) = self.begin()?;
        let host_cursor: Option<Option<String>> = tx
            .query_row(
                "SELECT host_cursor FROM runs WHERE run_id = ?",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(host_cursor) = host_cursor else {
            return refuse(StoreCode::NotFound, format!("run {run_id} not found"));
        };
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id = ? AND phase != 'terminal'",
            [run_id],
            |row| row.get(0),
        )?;
        if pending > 0 {
            return refuse(
                StoreCode::CompactionRefused,
                format!("run {run_id} has {pending} nonterminal operations"),
            );
        }
        let compacted = tx.execute(
            "UPDATE events SET compacted = 1, data_json = '{}' WHERE run_id = ? AND sequence <= ? AND type NOT IN ('RunTerminalized', 'RunQuarantined') AND compacted = 0",
            params![run_id, sql_int(through_sequence)],
        )?;
        let effective: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE run_id = ? AND compacted = 1",
            [run_id],
            |row| row.get(0),
        )?;
        let snapshot = json!({
            "throughSequence": effective,
            "requestedThrough": through_sequence,
            "snapshotDigest": snapshot_digest,
            "hostCursor": host_cursor,
        });
        write_meta(&tx, &format!("snapshot:{run_id}"), &to_json(&snapshot))?;
        Self::commit(&root, tx)?;
        Ok(u64::try_from(compacted).unwrap_or_default())
    }

    /// §7 backup boundary: a consistent owner-only image through SQLite's
    /// online-backup API, plus `<dest>.manifest.json` binding the image
    /// digest, the identity tags, the migration registry digest, the root
    /// scope, the writer epoch, every run's host cursor, and the time.
    /// Answers the manifest. (Restore is offline and reconciles before
    /// admission — slice 7.)
    ///
    /// # Errors
    ///
    /// Not the writer, or an I/O or SQLite failure.
    pub fn backup_to(&self, dest: &Path) -> Result<Value> {
        let epoch = self.writer_epoch()?;
        if let Some(parent) = dest.parent() {
            pa_core::platform::perms::create_dir_all_private(parent)?;
        }
        drop(open_private(dest)?);
        {
            let mut target = Connection::open(dest)?;
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut target)?;
            backup.run_to_completion(256, std::time::Duration::ZERO, None)?;
        }
        let image = std::fs::read(dest)?;
        let mut host_cursors = serde_json::Map::new();
        {
            let mut statement = self
                .conn
                .prepare("SELECT run_id, host_cursor FROM host_streams")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (run_id, cursor) in rows {
                host_cursors.insert(run_id, cursor.map_or(Value::Null, Value::String));
            }
        }
        let manifest = json!({
            "protocol": "prime.workflow.store-backup/v2-slice4",
            "databaseSha256": wire::sha256_digest(&image),
            "applicationId": STORE_APPLICATION_ID,
            "userVersion": STORE_USER_VERSION,
            "migrationRegistryDigest": migration_registry_digest(),
            "rootScopeDigest": self.root_scope_digest,
            "controllerEpoch": epoch,
            "hostCursors": host_cursors,
            "createdAt": self.now(),
        });
        let manifest_path = PathBuf::from(format!("{}.manifest.json", dest.display()));
        let mut file = open_private(&manifest_path)?;
        file.set_len(0)?;
        let pretty = serde_json::to_string_pretty(&manifest).unwrap_or_default();
        io::Write::write_all(&mut file, pretty.as_bytes())?;
        file.sync_all()?;
        Ok(manifest)
    }
}

fn persist_settlement(
    conn: &Connection,
    run_id: &str,
    settlement: &Value,
    now: &str,
) -> Result<()> {
    let digest = assert_digest(
        text_field(settlement, "settlementDigest"),
        "settlementDigest",
    )?;
    let result = &settlement["result"];
    let kind = text_field(result, "kind");
    let bytes = (kind != "none").then(|| sql_int(result["utf8Bytes"].as_u64().unwrap_or_default()));
    let sha256 = matches!(kind, "text" | "too_large").then(|| text_field(result, "sha256"));
    conn.execute(
        "INSERT OR IGNORE INTO settlements (settlement_digest, run_id, attempt_id, outcome, result_kind, result_utf8_bytes, result_sha256, usage_json, settlement_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            digest,
            run_id,
            assert_id(text_field(settlement, "attemptId"), "attemptId")?,
            text_field(settlement, "outcome"),
            kind,
            bytes,
            sha256,
            to_json(settlement.get("usage").unwrap_or(&Value::Null)),
            to_json(settlement),
            now,
        ],
    )?;
    if kind == "text" {
        add_text_bytes(conn, bytes.unwrap_or_default())?;
    }
    Ok(())
}

/// The §7 capability probe: a throwaway WAL + `synchronous=FULL` file
/// database and one `BEGIN IMMEDIATE` write. Any failure is unavailable;
/// it never throws and never falls back to another store. Windows is
/// unavailable unconditionally, as TS's was.
#[must_use]
pub fn probe_capability() -> CapabilityProbe {
    if cfg!(windows) {
        return CapabilityProbe {
            available: false,
            reason: Some("windows_unsupported"),
        };
    }
    let Ok(dir) = tempfile_dir() else {
        return CapabilityProbe {
            available: false,
            reason: Some("probe_failed"),
        };
    };
    let verdict = probe_in(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    verdict
}

fn tempfile_dir() -> io::Result<PathBuf> {
    let base = std::env::temp_dir().join(format!(
        "prime-agent-{}",
        pa_core::platform::perms::effective_uid()
            .map_or_else(|| "user".to_string(), |uid| uid.to_string())
    ));
    let dir = base.join(format!(
        "wf-v2-store-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    pa_core::platform::perms::create_dir_all_private(&dir)?;
    Ok(dir)
}

fn probe_in(dir: &Path) -> CapabilityProbe {
    let unavailable = |reason| CapabilityProbe {
        available: false,
        reason: Some(reason),
    };
    let attempt = || -> Result<Option<&'static str>> {
        let mut conn = Connection::open(dir.join(STORE_FILE_NAME))?;
        if !pragma_text(&conn, "PRAGMA journal_mode = WAL")?.eq_ignore_ascii_case("wal") {
            return Ok(Some("wal_unavailable"));
        }
        conn.execute_batch("PRAGMA synchronous = FULL")?;
        if pragma_int(&conn, "PRAGMA synchronous")? != 2 {
            return Ok(Some("synchronous_full_unavailable"));
        }
        conn.execute_batch(
            "CREATE TABLE probe (a INTEGER PRIMARY KEY, b INTEGER NOT NULL) STRICT",
        )?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("INSERT INTO probe (a, b) VALUES (1, 1)", [])? != 1 {
            return Ok(Some("immediate_write_unavailable"));
        }
        tx.commit()?;
        Ok(None)
    };
    match attempt() {
        Ok(None) => CapabilityProbe {
            available: true,
            reason: None,
        },
        Ok(Some(reason)) => unavailable(reason),
        Err(_) => unavailable("probe_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamps_round_trip_through_the_parser() {
        for text in [
            "2026-09-15T00:00:00.000Z",
            "1970-01-01T00:00:00.001Z",
            "2024-02-29T23:59:59.999Z",
        ] {
            let millis = parse_iso_millis(text).unwrap();
            assert_eq!(pa_core::session::manager::format_iso(millis), text);
        }
        assert_eq!(parse_iso_millis("2026-09-15T00:00:00Z"), None);
        assert_eq!(parse_iso_millis("2026-13-15T00:00:00.000Z"), None);
    }

    #[test]
    fn the_migration_checksum_is_the_ts_one() {
        // `sha256("1\nbase\n" + BASE_DDL)` as the TS store recorded it.
        assert_eq!(
            migration_checksum(&MIGRATIONS[0]),
            "sha256:ac64d22f8d9adba77b21b35af29f23b540984729d7f35869c1f5448b29f6d78d"
        );
    }
}
