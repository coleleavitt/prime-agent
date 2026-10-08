//! The Workflow V2 store's behaviour (`WORKFLOW-V2.md` §6, §7, §11): the TS
//! slice-4 suite (`workflow-v2-store.test.ts`) ported, plus writer
//! exclusion, the per-transaction epoch fence, the free-space floor,
//! out-of-process SIGKILL crash windows, and restart recovery.

mod v2_support;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use pa_workflow::v2::reducer::ReducerCode;
use pa_workflow::v2::store::{
    AcceptanceDecision, Acknowledgement, ClaimedOperation, CommandEffect, Decision,
    FilesystemSpace, OperationKind, OperationOutcome, OutboxEnqueue, Store, StoreCode, StoreError,
    StoreOptions, Table, STORE_APPLICATION_ID, STORE_CAPACITY, STORE_FILE_NAME, STORE_USER_VERSION,
};
use serde_json::{json, Value};
use v2_support::{
    create_effect, create_request, ctrl_event, drive_to_admission_bound, events, evidence,
    host_event, open_writer, options, post_request, store_root, turn_data, turn_settlement,
    TestClock,
};

fn code(error: &StoreError) -> Option<StoreCode> {
    error.code()
}

fn raw(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join(STORE_FILE_NAME)).unwrap()
}

fn counts(store: &Store, tables: &[Table]) -> Vec<u64> {
    tables
        .iter()
        .map(|table| store.table_count(*table).unwrap())
        .collect()
}

// ---- probe + open discipline ------------------------------------------------

#[cfg(unix)]
#[test]
fn the_probe_finds_wal_full_and_immediate_writes() {
    use pa_workflow::v2::store::{probe_capability, CapabilityProbe};
    assert_eq!(
        probe_capability(),
        CapabilityProbe {
            available: true,
            reason: None
        }
    );
}

#[cfg(unix)]
#[test]
fn open_creates_an_owner_only_tagged_versioned_store() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let store = open_writer(&root, &clock, 1);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        (
            mode(&root),
            mode(&root.join(STORE_FILE_NAME)),
            mode(&root.join(format!("{STORE_FILE_NAME}-wal"))),
            mode(&root.join("v2.sqlite.lock")),
        ),
        (0o700, 0o600, 0o600, 0o600)
    );
    drop(store);
    let conn = raw(&root);
    let scalar = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
    assert_eq!(
        (
            scalar("PRAGMA application_id"),
            scalar("PRAGMA user_version"),
            // 15 domain tables + store_migrations
            scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"),
        ),
        (STORE_APPLICATION_ID, STORE_USER_VERSION, 16)
    );
}

#[cfg(unix)]
#[test]
fn open_refuses_a_group_readable_root_and_a_symlinked_database() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, root) = store_root();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
    let clock = TestClock::new();
    assert_eq!(
        code(&Store::open(options(&root, &clock)).unwrap_err()),
        Some(StoreCode::ModeTooOpen)
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let elsewhere = dir.path().join("elsewhere.sqlite");
    std::fs::write(&elsewhere, b"").unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join(STORE_FILE_NAME)).unwrap();
    assert_eq!(
        code(&Store::open(options(&root, &clock)).unwrap_err()),
        Some(StoreCode::FileSymlink)
    );
}

#[test]
fn the_writer_epoch_only_rises() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = Store::open(options(&root, &clock)).unwrap();
    assert_eq!(store.acquire_writer(5).unwrap(), 5);
    assert_eq!(
        code(&store.acquire_writer(5).unwrap_err()),
        Some(StoreCode::EpochStale)
    );
    assert_eq!(
        code(&store.acquire_writer(4).unwrap_err()),
        Some(StoreCode::EpochStale)
    );
    assert_eq!(
        code(&store.acquire_writer(0).unwrap_err()),
        Some(StoreCode::EpochRange)
    );
    // A refused raise keeps the epoch already held: the handle still writes.
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
}

#[test]
fn a_reader_handle_cannot_write() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = Store::open(options(&root, &clock)).unwrap();
    assert_eq!(
        code(
            &store
                .apply_command(&create_request("run-1"), &create_effect("run-1"))
                .unwrap_err()
        ),
        Some(StoreCode::NotAcquired)
    );
}

// ---- writer exclusion and the epoch fence ----------------------------------

#[test]
fn a_second_writer_handle_is_refused_until_the_first_drops() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let first = open_writer(&root, &clock, 1);
    let mut second = Store::open(options(&root, &clock)).unwrap();
    assert_eq!(
        code(&second.acquire_writer(2).unwrap_err()),
        Some(StoreCode::WriterLocked)
    );
    // The refused handle may still read.
    assert_eq!(second.table_count(Table::Runs).unwrap(), 0);
    drop(first);
    assert_eq!(second.acquire_writer(2).unwrap(), 2);
}

#[test]
fn a_superseded_writer_is_fenced_with_no_mutation() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    // A writer that ignores the lock file (the TS store) raises the epoch.
    raw(&root)
        .execute(
            "UPDATE store_meta SET value = '7' WHERE key = 'controller_epoch'",
            [],
        )
        .unwrap();
    let before = counts(&store, &[Table::Events, Table::Commands]);
    let error = store
        .apply_command(
            &post_request("start", "run-1", "c-start", 1),
            &events(vec![ctrl_event(
                "run-1",
                2,
                "RunStarted",
                json!({ "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::EpochStale));
    assert_eq!(counts(&store, &[Table::Events, Table::Commands]), before);
    assert_eq!(
        code(&store.claim_outbox("w", 1_000, 10).unwrap_err()),
        Some(StoreCode::EpochStale)
    );
}

// ---- command transaction + idempotency --------------------------------------

#[test]
fn create_persists_the_run_rows_and_one_gap_free_event() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    let receipt = store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    assert_eq!(
        (
            receipt.action.as_str(),
            receipt.applied_sequences.clone(),
            receipt.command_id.clone()
        ),
        ("create", vec![1], None)
    );
    assert_eq!(
        store.run_projection("run-1").unwrap().unwrap().phase,
        "created"
    );
    assert_eq!(
        counts(
            &store,
            &[
                Table::Definitions,
                Table::Nodes,
                Table::Budgets,
                Table::HostStreams,
                Table::Events,
                Table::Commands
            ]
        ),
        vec![1, 1, 1, 1, 1, 1]
    );
    assert_eq!(
        store.command_receipt(&receipt.request_digest).unwrap(),
        Some(receipt)
    );
}

#[test]
fn an_identical_replay_answers_the_stored_receipt() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    let first = store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    clock.advance(5_000);
    let second = store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    assert_eq!(second, first);
    assert_eq!(
        counts(&store, &[Table::Commands, Table::Runs, Table::Events]),
        vec![1, 1, 1]
    );
}

#[test]
fn a_reused_request_id_with_changed_bytes_conflicts() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    let mut request = create_request("run-1");
    request["requestId"] = json!("shared-id");
    store
        .apply_command(&request, &create_effect("run-1"))
        .unwrap();
    request["definition"] = v2_support::valid_definition(&[], 999);
    let error = store
        .apply_command(&request, &create_effect("run-2"))
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::IdempotencyConflict));
    assert_eq!(store.table_count(Table::Runs).unwrap(), 1);
}

#[test]
fn a_reused_command_id_with_changed_bytes_conflicts() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    store
        .apply_command(
            &post_request("start", "run-1", "cmd-1", 1),
            &events(vec![ctrl_event(
                "run-1",
                2,
                "RunStarted",
                json!({ "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap();
    let mut clash = post_request("start", "run-1", "cmd-1", 2);
    clash["requestId"] = json!("different-request");
    let error = store
        .apply_command(
            &clash,
            &events(vec![ctrl_event(
                "run-1",
                3,
                "RunDraining",
                json!({ "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::IdempotencyConflict));
}

#[test]
fn validate_writes_nothing() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let store = open_writer(&root, &clock, 1);
    let request = json!({
        "protocol": "prime.workflow.request/v2",
        "requestId": "v1",
        "action": "validate",
        "definition": v2_support::definition(),
    });
    assert_eq!(store.validate(&request).unwrap(), "v1");
    assert_eq!(
        counts(&store, &[Table::Commands, Table::Runs, Table::Events]),
        vec![0, 0, 0]
    );
    assert_eq!(
        code(&store.validate(&create_request("run-1")).unwrap_err()),
        Some(StoreCode::NotValidate)
    );
}

#[test]
fn a_stale_fence_mutates_nothing() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    let error = store
        .apply_command(
            &post_request("start", "run-1", "cmd-1", 99),
            &events(vec![ctrl_event(
                "run-1",
                2,
                "RunStarted",
                json!({ "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::CommandFenceStale));
    assert_eq!(store.table_count(Table::Events).unwrap(), 1);
    assert_eq!(
        store.run_projection("run-1").unwrap().unwrap().phase,
        "created"
    );
}

#[test]
fn a_gapped_event_sequence_is_refused_atomically() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    let error = store
        .apply_command(
            &post_request("start", "run-1", "cmd-1", 1),
            &events(vec![ctrl_event(
                "run-1",
                5,
                "RunStarted",
                json!({ "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::EventGap));
    assert_eq!(
        counts(&store, &[Table::Events, Table::Commands]),
        vec![1, 1]
    );
}

#[test]
fn a_command_on_an_unknown_run_is_not_found() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    let error = store
        .apply_command(
            &post_request("start", "ghost", "cmd-1", 1),
            &CommandEffect::default(),
        )
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::NotFound));
}

#[test]
fn an_illegal_transition_is_the_reducers_refusal_with_no_effect() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    // NodeBecameReady needs an active run.
    let error = store
        .apply_command(
            &post_request("start", "run-1", "cmd-1", 1),
            &events(vec![ctrl_event(
                "run-1",
                2,
                "NodeBecameReady",
                json!({ "nodeId": "n1", "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::Reducer(refusal) if refusal.code == ReducerCode::NodeReadyRunPhase),
        "{error}"
    );
    assert_eq!(
        counts(&store, &[Table::Events, Table::Commands]),
        vec![1, 1]
    );
}

#[test]
fn status_and_events_are_not_mutations() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    let status = json!({ "protocol": "prime.workflow.request/v2", "requestId": "s1", "action": "status", "runId": "run-1" });
    assert_eq!(
        code(
            &store
                .apply_command(&status, &CommandEffect::default())
                .unwrap_err()
        ),
        Some(StoreCode::NotMutation)
    );
}

// ---- outbox / inbox / cursors ------------------------------------------------

fn ack(operation_id: &str, owner: &str, outcome: OperationOutcome) -> Acknowledgement {
    Acknowledgement {
        operation_id: operation_id.to_string(),
        owner: owner.to_string(),
        host_request_id: "hostreq-1".to_string(),
        outcome,
        receipt: json!({ "ok": true }),
    }
}

#[test]
fn the_outbox_redelivers_the_same_identity_and_only_the_owner_acknowledges() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    drive_to_admission_bound(&mut store, "run-1");
    let first = store.claim_outbox("worker-1", 60_000, 10).unwrap();
    assert_eq!(
        first,
        vec![ClaimedOperation {
            operation_id: "op1".to_string(),
            run_id: "run-1".to_string(),
            attempt_id: Some("a1".to_string()),
            kind: "deliver".to_string(),
            request_id: "hostreq-1".to_string(),
            canonical_request:
                r#"{"operation":"child.admit","protocol":"prime.workflow.retained-request/v2"}"#
                    .to_string(),
            host_request_id: None,
            claim_epoch: 1,
            claim_expires_at: "2026-09-15T00:01:00.000Z".to_string(),
        }]
    );
    assert_eq!(store.claim_outbox("worker-1", 60_000, 10).unwrap(), vec![]);
    clock.advance(120_000);
    let again = store.claim_outbox("worker-1", 60_000, 10).unwrap();
    assert_eq!(
        (
            again[0].operation_id.as_str(),
            again[0].canonical_request.as_str()
        ),
        ("op1", first[0].canonical_request.as_str())
    );
    assert_eq!(
        code(
            &store
                .acknowledge_outbox(&ack("op1", "intruder", OperationOutcome::Succeeded))
                .unwrap_err()
        ),
        Some(StoreCode::StaleClaim)
    );
    store
        .acknowledge_outbox(&ack("op1", "worker-1", OperationOutcome::Succeeded))
        .unwrap();
    let row = store.operation("op1").unwrap().unwrap();
    assert_eq!(
        (
            row.phase.as_str(),
            row.outcome.as_deref(),
            row.host_request_id.as_deref()
        ),
        ("terminal", Some("succeeded"), Some("hostreq-1"))
    );
    assert_eq!(store.claim_outbox("worker-1", 0, 10).unwrap(), vec![]);
}

#[test]
fn a_host_fact_applies_once_and_advances_the_cursor_atomically() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    drive_to_admission_bound(&mut store, "run-1");
    let turn = turn_data("hostreq-1", "child1", "turn1");
    let started = store
        .ingest_host_event(
            "run-1",
            &host_event("he-start", "hc-1", "TurnStarted", turn.clone()),
        )
        .unwrap();
    assert_eq!(
        (started.applied, started.host_cursor.as_deref()),
        (true, Some("hc-1"))
    );
    let mut settled = turn;
    settled["settlement"] = turn_settlement("a1", "child1", "turn1", true, true);
    let first = store
        .ingest_host_event(
            "run-1",
            &host_event("he-settle", "hc-2", "TurnSettled", settled.clone()),
        )
        .unwrap();
    assert_eq!(
        (first.applied, first.host_cursor.as_deref()),
        (true, Some("hc-2"))
    );
    assert_eq!(
        counts(&store, &[Table::Settlements, Table::HostInbox]),
        vec![1, 2]
    );
    let again = store
        .ingest_host_event(
            "run-1",
            &host_event("he-settle", "hc-9", "TurnSettled", settled),
        )
        .unwrap();
    assert_eq!(
        (again.applied, again.host_cursor.as_deref()),
        (false, Some("hc-2"))
    );
    assert_eq!(store.table_count(Table::HostInbox).unwrap(), 2);
    assert_eq!(store.host_cursor("run-1").unwrap().as_deref(), Some("hc-2"));
}

#[test]
fn a_refused_host_fact_leaves_the_cursor_and_inbox_alone() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    drive_to_admission_bound(&mut store, "run-1");
    // A turn no attempt is bound to.
    let error = store
        .ingest_host_event(
            "run-1",
            &host_event(
                "he-x",
                "hc-x",
                "TurnStarted",
                turn_data("hostreq-1", "child9", "turn9"),
            ),
        )
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::Reducer(refusal) if refusal.code == ReducerCode::HostFactUnbound),
        "{error}"
    );
    assert_eq!(store.table_count(Table::HostInbox).unwrap(), 0);
    assert_eq!(store.host_cursor("run-1").unwrap(), None);
}

#[test]
fn the_host_cursor_and_the_event_cursor_never_convert() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    drive_to_admission_bound(&mut store, "run-1");
    let before = store
        .load_aggregate("run-1")
        .unwrap()
        .unwrap()
        .last_controller_sequence;
    store
        .ingest_host_event(
            "run-1",
            &host_event(
                "he-start",
                "hc-77",
                "TurnStarted",
                turn_data("hostreq-1", "child1", "turn1"),
            ),
        )
        .unwrap();
    assert_eq!(
        store
            .load_aggregate("run-1")
            .unwrap()
            .unwrap()
            .last_controller_sequence,
        before
    );
    assert_eq!(
        store.host_cursor("run-1").unwrap().as_deref(),
        Some("hc-77")
    );
    assert_eq!(store.list_events("run-1", 0, 500).unwrap().len(), 6);
    assert_eq!(store.list_events("run-1", 6, 500).unwrap().len(), 0);
}

// ---- restart -----------------------------------------------------------------

#[test]
fn a_restart_reopens_above_the_old_epoch_with_state_intact() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut first = open_writer(&root, &clock, 1);
    drive_to_admission_bound(&mut first, "run-1");
    drop(first);
    let mut second = Store::open(options(&root, &clock)).unwrap();
    assert_eq!(
        code(&second.acquire_writer(1).unwrap_err()),
        Some(StoreCode::EpochStale)
    );
    second.acquire_writer(2).unwrap();
    let aggregate = second.load_aggregate("run-1").unwrap().unwrap();
    assert_eq!(
        (
            aggregate.attempts[0].projection.phase.as_str(),
            aggregate.last_controller_sequence
        ),
        ("admission_bound", 6)
    );
}

#[test]
fn a_restart_redelivers_a_claimed_operation_under_the_new_epoch_only() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut old = open_writer(&root, &clock, 1);
    drive_to_admission_bound(&mut old, "run-1");
    let claimed = old.claim_outbox("worker-1", 60_000, 10).unwrap();
    drop(old); // the daemon dies holding the claim
    let mut new = open_writer(&root, &clock, 2);
    // The dead epoch's owner cannot resolve it.
    assert_eq!(
        code(
            &new.acknowledge_outbox(&ack("op1", "worker-1", OperationOutcome::Succeeded))
                .unwrap_err()
        ),
        Some(StoreCode::StaleClaim)
    );
    // Not before the lease expires; then the same request, epoch 2.
    assert_eq!(new.claim_outbox("worker-2", 60_000, 10).unwrap(), vec![]);
    clock.advance(60_001);
    let redelivered = new.claim_outbox("worker-2", 60_000, 10).unwrap();
    assert_eq!(
        (
            redelivered[0].operation_id.as_str(),
            redelivered[0].canonical_request.as_str(),
            redelivered[0].claim_epoch,
        ),
        ("op1", claimed[0].canonical_request.as_str(), 2)
    );
    new.acknowledge_outbox(&ack("op1", "worker-2", OperationOutcome::Ambiguous))
        .unwrap();
    assert_eq!(
        new.operation("op1").unwrap().unwrap().outcome.as_deref(),
        Some("ambiguous")
    );
}

#[test]
fn a_scope_mismatch_on_reopen_fails_closed() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    drop(open_writer(&root, &clock, 1));
    let other = StoreOptions {
        root_scope_digest: format!("sha256:{}", "d".repeat(64)),
        ..options(&root, &clock)
    };
    assert_eq!(
        code(&Store::open(other).unwrap_err()),
        Some(StoreCode::ScopeMismatch)
    );
}

#[test]
fn a_failure_after_the_event_append_leaves_zero_durable_effect() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    let before = counts(&store, &[Table::Events, Table::Operations, Table::Commands]);
    let error = store
        .apply_command(
            &post_request("start", "run-1", "c-start", 1),
            &CommandEffect {
                events: vec![ctrl_event(
                    "run-1",
                    2,
                    "RunStarted",
                    json!({ "evidenceDigest": evidence() }),
                )],
                operations: vec![OutboxEnqueue {
                    operation_id: "bad id with spaces".to_string(),
                    attempt_id: None,
                    kind: OperationKind::Deliver,
                    request_id: "r".to_string(),
                    canonical_request: json!({}),
                }],
                ..CommandEffect::default()
            },
        )
        .unwrap_err();
    assert_eq!(code(&error), Some(StoreCode::InvalidId));
    assert_eq!(
        counts(&store, &[Table::Events, Table::Operations, Table::Commands]),
        before
    );
    assert_eq!(
        store.run_projection("run-1").unwrap().unwrap().phase,
        "created"
    );
}

// ---- corruption / migration / capacity --------------------------------------

fn reopen_error(root: &Path) -> StoreError {
    Store::open(options(root, &TestClock::new())).unwrap_err()
}

#[test]
fn a_foreign_application_id_is_refused() {
    let (_dir, root) = store_root();
    drop(open_writer(&root, &TestClock::new(), 1));
    raw(&root)
        .execute_batch("PRAGMA application_id = 12345")
        .unwrap();
    assert_eq!(code(&reopen_error(&root)), Some(StoreCode::Foreign));
}

#[test]
fn a_newer_user_version_is_refused() {
    let (_dir, root) = store_root();
    drop(open_writer(&root, &TestClock::new(), 1));
    raw(&root)
        .execute_batch(&format!("PRAGMA user_version = {}", STORE_USER_VERSION + 5))
        .unwrap();
    assert_eq!(
        code(&reopen_error(&root)),
        Some(StoreCode::UserVersionNewer)
    );
}

#[test]
fn a_tampered_migration_checksum_is_refused() {
    let (_dir, root) = store_root();
    drop(open_writer(&root, &TestClock::new(), 1));
    raw(&root)
        .execute(
            "UPDATE store_migrations SET checksum = 'sha256:deadbeef' WHERE version = 1",
            [],
        )
        .unwrap();
    assert_eq!(
        code(&reopen_error(&root)),
        Some(StoreCode::MigrationChecksum)
    );
}

#[test]
fn an_untagged_database_with_tables_is_not_adopted() {
    let (_dir, root) = store_root();
    pa_core::platform::perms::create_dir_all_private(&root).unwrap();
    let db = root.join(STORE_FILE_NAME);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE other (x INTEGER)")
        .unwrap();
    pa_core::platform::perms::restrict_file(&db).unwrap();
    assert_eq!(code(&reopen_error(&root)), Some(StoreCode::Foreign));
}

#[test]
fn a_corrupt_database_file_is_refused() {
    let (_dir, root) = store_root();
    drop(open_writer(&root, &TestClock::new(), 1));
    std::fs::write(
        root.join(STORE_FILE_NAME),
        b"this is not a sqlite database file at all, totally corrupt",
    )
    .unwrap();
    assert!(matches!(reopen_error(&root), StoreError::Sqlite(_)));
}

#[test]
fn create_is_refused_at_the_text_bytes_high_water_mark() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    drop(store);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let high =
        (STORE_CAPACITY.max_text_bytes as f64 * STORE_CAPACITY.high_water_fraction) as u64 + 1;
    raw(&root)
        .execute(
            "UPDATE store_meta SET value = ? WHERE key = 'text_bytes'",
            [high.to_string()],
        )
        .unwrap();
    let mut store = open_writer(&root, &clock, 2);
    assert_eq!(
        code(
            &store
                .apply_command(&create_request("run-2"), &create_effect("run-2"))
                .unwrap_err()
        ),
        Some(StoreCode::CapacityExceeded)
    );
    assert_eq!(store.table_count(Table::Runs).unwrap(), 1);
}

#[test]
fn create_is_refused_below_the_free_space_floor_but_cancel_is_exempt() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    store
        .apply_command(&create_request("run-1"), &create_effect("run-1"))
        .unwrap();
    drop(store);
    // 9% free of 1 TiB: under the 10% floor (well above 64 MiB).
    let tight = StoreOptions {
        filesystem: Some(Arc::new(|_: &Path| {
            Some(FilesystemSpace {
                free: 99 << 30,
                total: 1 << 40,
            })
        })),
        ..options(&root, &clock)
    };
    let mut store = Store::open(tight).unwrap();
    store.acquire_writer(2).unwrap();
    assert_eq!(
        code(
            &store
                .apply_command(&create_request("run-2"), &create_effect("run-2"))
                .unwrap_err()
        ),
        Some(StoreCode::CapacityExceeded)
    );
    let cancel = post_request("cancel", "run-1", "c-cancel", 1);
    store
        .apply_command(
            &cancel,
            &events(vec![ctrl_event(
                "run-1",
                2,
                "RunCancellationRequested",
                json!({ "evidenceDigest": evidence() }),
            )]),
        )
        .unwrap();
    assert_eq!(
        store.run_projection("run-1").unwrap().unwrap().phase,
        "cancelling"
    );
}

// ---- terminal path, retention, backup ---------------------------------------

fn drive_to_terminal(store: &mut Store) {
    drive_to_admission_bound(store, "run-1");
    let turn = turn_data("hostreq-1", "child1", "turn1");
    store
        .ingest_host_event(
            "run-1",
            &host_event("he-start", "hc-1", "TurnStarted", turn.clone()),
        )
        .unwrap();
    let mut settled = turn;
    settled["settlement"] = turn_settlement("a1", "child1", "turn1", true, true);
    store
        .ingest_host_event(
            "run-1",
            &host_event("he-settle", "hc-2", "TurnSettled", settled),
        )
        .unwrap();
    let e = evidence();
    store
        .apply_command(
            &post_request("start", "run-1", "c-obs", 6),
            &events(vec![ctrl_event(
                "run-1",
                7,
                "AttemptSettlementObserved",
                json!({ "nodeId": "n1", "attemptId": "a1", "settlementDigest": e, "outcome": "completed" }),
            )]),
        )
        .unwrap();
    store
        .apply_command(
            &post_request("start", "run-1", "c-acc", 7),
            &CommandEffect {
                events: vec![ctrl_event(
                    "run-1",
                    8,
                    "AttemptAccepted",
                    json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
                )],
                acceptance: vec![AcceptanceDecision {
                    node_id: "n1".to_string(),
                    attempt_id: Some("a1".to_string()),
                    decision: Decision::Accepted,
                    evidence_digest: Some(e.clone()),
                }],
                ..CommandEffect::default()
            },
        )
        .unwrap();
    store
        .apply_command(
            &post_request("start", "run-1", "c-drain", 8),
            &events(vec![ctrl_event(
                "run-1",
                9,
                "RunDraining",
                json!({ "evidenceDigest": e }),
            )]),
        )
        .unwrap();
    store
        .apply_command(
            &post_request("start", "run-1", "c-term", 9),
            &events(vec![ctrl_event(
                "run-1",
                10,
                "RunTerminalized",
                json!({ "evidenceDigest": e, "outcome": "succeeded" }),
            )]),
        )
        .unwrap();
    store.claim_outbox("w", 60_000, 10).unwrap();
    store
        .acknowledge_outbox(&ack("op1", "w", OperationOutcome::Succeeded))
        .unwrap();
}

#[test]
fn a_run_reaches_a_succeeded_terminal_with_an_accepted_node() {
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_terminal(&mut store);
    let aggregate = store.load_aggregate("run-1").unwrap().unwrap();
    assert_eq!(
        (
            aggregate.run.outcome.as_deref(),
            aggregate.nodes[0].projection.outcome.as_deref(),
            aggregate.attempts[0].projection.outcome.as_deref(),
            aggregate.attempts[0]
                .turn
                .as_ref()
                .and_then(|turn| turn.outcome.as_deref()),
        ),
        (
            Some("succeeded"),
            Some("accepted"),
            Some("completed"),
            Some("completed")
        )
    );
    assert_eq!(store.table_count(Table::Acceptance).unwrap(), 1);
}

#[test]
fn erasure_is_refused_while_the_run_is_nonterminal() {
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_admission_bound(&mut store, "run-1");
    assert_eq!(
        code(&store.erase_run_text("run-1", "test-policy").unwrap_err()),
        Some(StoreCode::ErasureRefused)
    );
}

#[test]
fn erasure_keeps_the_digest_byte_count_and_settlement_evidence() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut store = open_writer(&root, &clock, 1);
    drive_to_terminal(&mut store);
    assert_eq!(store.erase_run_text("run-1", "retention-30d").unwrap(), 11);
    assert_eq!(store.table_count(Table::Settlements).unwrap(), 1);
    let settlement: String = raw(&root)
        .query_row("SELECT settlement_json FROM settlements", [], |row| {
            row.get(0)
        })
        .unwrap();
    let settlement: Value = serde_json::from_str(&settlement).unwrap();
    assert_eq!(
        settlement["result"],
        json!({
            "kind": "erased",
            "utf8Bytes": 11,
            "sha256": v2_support::sha256("hello world"),
            "erasedAt": "2026-09-15T00:00:00.000Z",
            "policyBasis": "retention-30d",
        })
    );
    // A second erasure frees nothing.
    assert_eq!(store.erase_run_text("run-1", "retention-30d").unwrap(), 0);
}

#[test]
fn compaction_waits_for_terminal_operations_then_requires_a_snapshot() {
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_admission_bound(&mut store, "run-1");
    assert_eq!(
        code(&store.compact_events("run-1", 3, &evidence()).unwrap_err()),
        Some(StoreCode::CompactionRefused)
    );
    drop(store);
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_terminal(&mut store);
    assert_eq!(store.compact_events("run-1", 5, &evidence()).unwrap(), 5);
    assert_eq!(
        code(&store.list_events("run-1", 0, 500).unwrap_err()),
        Some(StoreCode::SnapshotRequired)
    );
    assert_eq!(store.list_events("run-1", 5, 500).unwrap().len(), 5);
}

#[test]
fn compaction_never_blanks_the_terminal_fact() {
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_terminal(&mut store);
    store.compact_events("run-1", 10, &evidence()).unwrap();
    let rows = store.list_events("run-1", 9, 500).unwrap();
    let terminal = rows
        .iter()
        .find(|row| row.kind == "RunTerminalized")
        .unwrap();
    let data: Value = serde_json::from_str(&terminal.data_json).unwrap();
    assert_eq!(data["outcome"], "succeeded");
    // The run still hydrates (its terminal outcome is still provable).
    assert_eq!(
        store
            .run_projection("run-1")
            .unwrap()
            .unwrap()
            .outcome
            .as_deref(),
        Some("succeeded")
    );
}

#[test]
fn a_backup_is_a_manifest_bound_tagged_database() {
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_terminal(&mut store);
    let dest = root.join("backup").join("v2.bak");
    let manifest = store.backup_to(&dest).unwrap();
    assert_eq!(
        manifest["databaseSha256"],
        json!(v2_support::sha256_bytes(&std::fs::read(&dest).unwrap()))
    );
    assert_eq!(
        (
            manifest["protocol"].clone(),
            manifest["rootScopeDigest"].clone(),
            manifest["applicationId"].clone(),
            manifest["controllerEpoch"].clone(),
            manifest["hostCursors"].clone(),
        ),
        (
            json!("prime.workflow.store-backup/v2-slice4"),
            json!(v2_support::SCOPE),
            json!(STORE_APPLICATION_ID),
            json!(1),
            json!({ "run-1": "hc-2" }),
        )
    );
    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("backup").join("v2.bak.manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(written, manifest);
    let image = rusqlite::Connection::open(&dest).unwrap();
    let scalar = |sql: &str| {
        image
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    assert_eq!(
        (
            scalar("PRAGMA application_id"),
            scalar("SELECT COUNT(*) FROM runs")
        ),
        (STORE_APPLICATION_ID, 1)
    );
}

// ---- RunTerminalized equality on load + durable quarantine ------------------

fn tamper_terminal_outcome(root: &Path) {
    raw(root)
        .execute(
            "UPDATE runs SET outcome = 'failed', terminal_outcome = 'failed' WHERE run_id = 'run-1'",
            [],
        )
        .unwrap();
}

#[test]
fn a_terminal_outcome_disagreeing_with_its_fact_fails_closed_on_load() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut first = open_writer(&root, &clock, 1);
    drive_to_terminal(&mut first);
    drop(first);
    tamper_terminal_outcome(&root);
    let second = open_writer(&root, &clock, 2);
    assert_eq!(
        code(&second.load_aggregate("run-1").unwrap_err()),
        Some(StoreCode::TerminalOutcomeMismatch)
    );
    assert_eq!(
        code(&second.run_projection("run-1").unwrap_err()),
        Some(StoreCode::TerminalOutcomeMismatch)
    );
}

#[test]
fn reconciliation_quarantines_a_tampered_run_once() {
    let (_dir, root) = store_root();
    let clock = TestClock::new();
    let mut first = open_writer(&root, &clock, 1);
    drive_to_terminal(&mut first);
    drop(first);
    tamper_terminal_outcome(&root);
    let mut second = open_writer(&root, &clock, 2);
    assert!(second.reconcile_terminal_mismatch("run-1").unwrap());
    let run = second.run_projection("run-1").unwrap().unwrap();
    assert_eq!(
        (
            run.phase.as_str(),
            run.outcome.clone(),
            run.conditions.contains(&"integrity_failed".to_string())
        ),
        ("quarantined", None, true)
    );
    let last = second.list_events("run-1", 10, 500).unwrap();
    assert_eq!(
        (last.len(), last[0].kind.as_str(), last[0].event_id.as_str()),
        (1, "RunQuarantined", "reconcile-quarantine-run-1-11")
    );
    assert!(!second.reconcile_terminal_mismatch("run-1").unwrap());
}

#[test]
fn reconciliation_leaves_a_consistent_terminal_run() {
    let (_dir, root) = store_root();
    let mut store = open_writer(&root, &TestClock::new(), 1);
    drive_to_terminal(&mut store);
    assert!(!store.reconcile_terminal_mismatch("run-1").unwrap());
    assert_eq!(
        store
            .run_projection("run-1")
            .unwrap()
            .unwrap()
            .outcome
            .as_deref(),
        Some("succeeded")
    );
}

// ---- out-of-process crash windows and cross-process exclusion ---------------

const CHILD_ROOT: &str = "PA_WORKFLOW_V2_CHILD_ROOT";
const CHILD_MODE: &str = "PA_WORKFLOW_V2_CHILD_MODE";

/// Spawn this test binary as the child process running `child_entry`.
#[cfg(unix)]
fn spawn_child(root: &Path, mode: &str) -> std::process::Child {
    use std::process::{Command, Stdio};
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_entry",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ROOT, root)
        .env(CHILD_MODE, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// The child process: drive the store to admission-bound, then die by
/// SIGKILL inside an uncommitted ingest transaction (on the clock call the
/// mode names), or hold the writer until killed.
#[test]
#[ignore = "child-process entry point of the crash and exclusion tests"]
fn child_entry() {
    let (Ok(root), Ok(mode)) = (std::env::var(CHILD_ROOT), std::env::var(CHILD_MODE)) else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let armed = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicU32::new(0));
    let die_at = match mode.as_str() {
        // 1: the host_inbox row's time, 2: the projection upsert (after the
        // inbox INSERT ran), for TurnStarted.
        "kill-after-inbox-insert" => 2,
        // TurnSettled: 2 is the settlement row's time, 3 the upsert (after
        // the inbox and settlement INSERTs ran).
        "kill-after-settlement-insert" => 3,
        _ => u32::MAX,
    };
    let clock = {
        let (armed, calls) = (Arc::clone(&armed), Arc::clone(&calls));
        Arc::new(move || {
            if armed.load(Ordering::SeqCst) && calls.fetch_add(1, Ordering::SeqCst) + 1 == die_at {
                #[cfg(unix)]
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::this(),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            "2026-09-15T00:00:00.000Z".to_string()
        })
    };
    let mut store = Store::open(StoreOptions {
        clock: Some(clock),
        ..options(&root, &TestClock::new())
    })
    .unwrap();
    store.acquire_writer(1).unwrap();
    drive_to_admission_bound(&mut store, "run-1");
    println!("BOUND");
    let turn = turn_data("hostreq-1", "child1", "turn1");
    match mode.as_str() {
        "kill-after-inbox-insert" => {
            armed.store(true, Ordering::SeqCst);
            let _ = store.ingest_host_event(
                "run-1",
                &host_event("he-kill", "hc-kill", "TurnStarted", turn),
            );
        }
        "kill-after-settlement-insert" => {
            store
                .ingest_host_event(
                    "run-1",
                    &host_event("he-start", "hc-1", "TurnStarted", turn.clone()),
                )
                .unwrap();
            let mut settled = turn;
            settled["settlement"] = turn_settlement("a1", "child1", "turn1", true, true);
            armed.store(true, Ordering::SeqCst);
            let _ = store.ingest_host_event(
                "run-1",
                &host_event("he-kill", "hc-kill", "TurnSettled", settled),
            );
        }
        _ => {
            println!("HELD");
            // Hold the writer until the parent kills this process.
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
    }
    println!("UNREACHABLE");
}

/// Run a crash child to its death; answer its stdout.
#[cfg(unix)]
fn run_crash_child(root: &Path, mode: &str) -> String {
    use std::os::unix::process::ExitStatusExt;
    let output = spawn_child(root, mode).wait_with_output().unwrap();
    assert_eq!(
        output.status.signal(),
        Some(9),
        "the child must die by SIGKILL"
    );
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn a_sigkill_inside_an_uncommitted_ingest_leaves_zero_partial_effect() {
    let (_dir, root) = store_root();
    let stdout = run_crash_child(&root, "kill-after-inbox-insert");
    assert!(
        stdout.contains("BOUND") && !stdout.contains("UNREACHABLE"),
        "{stdout}"
    );
    // The dead writer's lock died with it; WAL recovery drops the open
    // transaction and keeps every committed command.
    let store = open_writer(&root, &TestClock::new(), 2);
    assert_eq!(store.table_count(Table::HostInbox).unwrap(), 0);
    assert_eq!(store.host_cursor("run-1").unwrap(), None);
    let aggregate = store.load_aggregate("run-1").unwrap().unwrap();
    assert_eq!(
        (
            aggregate.last_controller_sequence,
            aggregate.attempts[0].projection.phase.as_str()
        ),
        (6, "admission_bound")
    );
}

#[cfg(unix)]
#[test]
fn a_sigkill_after_the_settlement_insert_loses_the_whole_settlement() {
    let (_dir, root) = store_root();
    let stdout = run_crash_child(&root, "kill-after-settlement-insert");
    assert!(
        stdout.contains("BOUND") && !stdout.contains("UNREACHABLE"),
        "{stdout}"
    );
    let mut store = open_writer(&root, &TestClock::new(), 2);
    assert_eq!(
        counts(&store, &[Table::HostInbox, Table::Settlements]),
        vec![1, 0]
    );
    assert_eq!(store.host_cursor("run-1").unwrap().as_deref(), Some("hc-1"));
    let meta: String = raw(&root)
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'text_bytes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // Only the definition's bytes: the lost settlement's 11 never counted.
    let definition_bytes = pa_workflow::v2::wire::canonical_json(&v2_support::definition())
        .unwrap()
        .len();
    assert_eq!(meta, definition_bytes.to_string());
    // The host re-sends the page; it applies exactly once now.
    let mut settled = turn_data("hostreq-1", "child1", "turn1");
    settled["settlement"] = turn_settlement("a1", "child1", "turn1", true, true);
    let applied = store
        .ingest_host_event(
            "run-1",
            &host_event("he-kill", "hc-kill", "TurnSettled", settled),
        )
        .unwrap();
    assert!(applied.applied);
    assert_eq!(
        counts(&store, &[Table::HostInbox, Table::Settlements]),
        vec![2, 1]
    );
}

#[cfg(unix)]
#[test]
fn a_writer_in_another_process_excludes_this_one_until_it_dies() {
    use std::io::{BufRead, BufReader};
    let (_dir, root) = store_root();
    let mut child = spawn_child(&root, "hold");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("HELD") {
        line.clear();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "the child exited early"
        );
    }
    let mut store = Store::open(options(&root, &TestClock::new())).unwrap();
    assert_eq!(
        code(&store.acquire_writer(2).unwrap_err()),
        Some(StoreCode::WriterLocked)
    );
    child.kill().unwrap(); // by PID
    child.wait().unwrap();
    assert_eq!(store.acquire_writer(2).unwrap(), 2);
    assert_eq!(
        store
            .load_aggregate("run-1")
            .unwrap()
            .unwrap()
            .last_controller_sequence,
        6
    );
}
