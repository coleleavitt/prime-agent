//! Shared Workflow V2 store/reducer test support: the TS slice-4 fixture
//! builders (`workflow-v2-slice4-fixtures.ts`), the TS-shaped JSON views of
//! the Rust results, and the typed table dump the goldens compare.

#![allow(dead_code)] // each test binary uses its own subset

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use pa_workflow::v2::reducer::RunAggregate;
use pa_workflow::v2::store::{
    AcceptanceDecision,
    Acknowledgement,
    ChildTurnBinding,
    ClaimedOperation,
    Clock,
    CommandEffect,
    Decision,
    EventRow,
    FilesystemProbe,
    FilesystemSpace,
    OperationKind,
    OperationOutcome,
    OutboxEnqueue,
    RunBudget,
    Store,
    StoreError,
    StoreOptions,
    TombstoneBinding,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const RECORDED_AT: &str = "2026-09-15T00:00:00.000Z";
pub const SCOPE: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

pub fn evidence() -> String {
    format!("sha256:{}", "a".repeat(64))
}

pub fn sha256(text: &str) -> String {
    sha256_bytes(text.as_bytes())
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!(
        "sha256:{}",
        digest.iter().fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
    )
}

/// `validDefinition()` with one node `n1`, or the named nodes.
pub fn valid_definition(nodes: &[(&str, &[&str])], max_total_tokens: u64) -> Value {
    let specs: Vec<(&str, &[&str])> = if nodes.is_empty() {
        vec![("n1", &[])]
    } else {
        nodes.to_vec()
    };
    let last = specs.last().map_or("n1", |(id, _)| *id);
    json!({
        "protocol": "prime.workflow.definition/v2",
        "nodes": specs.iter().map(|(id, deps)| json!({
            "nodeId": id,
            "kind": "agent",
            "prompt": format!("prompt for {id}"),
            "dependsOn": deps.iter().map(|d| json!({ "nodeId": d, "require": "accepted" })).collect::<Vec<_>>(),
            "model": "model.test",
            "maxTurns": 1,
            "tools": "none",
            "maxTokens": 100,
        })).collect::<Vec<_>>(),
        "outputs": [last],
        "budget": { "maxConcurrentAttempts": 4, "maxTotalTokens": max_total_tokens, "semantics": "soft_admission" },
    })
}

pub fn definition() -> Value {
    valid_definition(&[], 10_000)
}

static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `ctrlEvent(runId, sequence, type, data)`.
pub fn ctrl_event(run_id: &str, sequence: u64, kind: &str, data: Value) -> Value {
    let counter = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let mut event = json!({
        "protocol": "prime.workflow.event/v2",
        "eventId": format!("ev-{run_id}-{sequence}-{counter}"),
        "runId": run_id,
        "sequence": sequence,
        "revision": sequence,
        "type": kind,
        "recordedAt": RECORDED_AT,
        "controllerEpoch": 1,
        "cancelEpoch": 0,
        "data": null,
        "digest": evidence(),
    });
    event["data"] = data;
    event
}

/// `hostEvent(hostEventId, hostCursor, type, data)`.
pub fn host_event(host_event_id: &str, host_cursor: &str, kind: &str, data: Value) -> Value {
    let mut event = json!({
        "protocol": "prime.workflow.retained-event/v2",
        "hostEventId": host_event_id,
        "hostCursor": host_cursor,
        "type": kind,
        "recordedAt": RECORDED_AT,
        "data": null,
        "digest": evidence(),
    });
    event["data"] = data;
    event
}

/// The `{requestId, rlmChildId, turnId, evidenceDigest}` turn data.
pub fn turn_data(request_id: &str, child: &str, turn: &str) -> Value {
    json!({ "requestId": request_id, "rlmChildId": child, "turnId": turn, "evidenceDigest": evidence() })
}

/// `turnSettlement(...)`: a completed text settlement ("hello world").
pub fn turn_settlement(
    attempt_id: &str,
    child: &str,
    turn: &str,
    usage_final: bool,
    quiescent: bool,
) -> Value {
    let text = "hello world";
    json!({
        "authorityScope": evidence(),
        "parentId": "parent.session",
        "requestId": format!("req-{attempt_id}"),
        "nodeId": "n1",
        "attemptId": attempt_id,
        "rlmChildId": child,
        "turnId": turn,
        "admittedAt": RECORDED_AT,
        "startedAt": RECORDED_AT,
        "settledAt": RECORDED_AT,
        "cancelActuated": false,
        "descendantsQuiescent": quiescent,
        "hostCursor": "hc-1",
        "settlementDigest": evidence(),
        "outcome": "completed",
        "result": { "kind": "text", "text": text, "utf8Bytes": text.len(), "sha256": sha256(text) },
        "usage": {
            "inputTokens": 10, "outputTokens": 20, "cacheReadTokens": 0, "cacheWriteTokens": 0,
            "totalTokens": 30, "costMicrousd": 0,
            "finality": if usage_final { "final" } else { "known_prefix" },
        },
        "error": null,
        "workflowChildId": format!("wfc-{attempt_id}"),
        "requestDigest": evidence(),
    })
}

pub fn create_request(run_id: &str) -> Value {
    json!({
        "protocol": "prime.workflow.request/v2",
        "requestId": format!("req-create-{run_id}"),
        "action": "create",
        "definition": definition(),
    })
}

pub fn create_effect(run_id: &str) -> CommandEffect {
    CommandEffect {
        definition: Some(definition()),
        events: vec![ctrl_event(
            run_id,
            1,
            "RunAdmitted",
            json!({ "evidenceDigest": evidence() }),
        )],
        ..CommandEffect::default()
    }
}

/// A post-create command request at `revision` (epochs 1 / 0).
pub fn post_request(action: &str, run_id: &str, command_id: &str, revision: u64) -> Value {
    let mut request = json!({
        "protocol": "prime.workflow.request/v2",
        "requestId": format!("req-{action}-{command_id}"),
        "action": action,
        "runId": run_id,
        "commandId": command_id,
        "expectedRevision": revision,
        "expectedControllerEpoch": 1,
        "expectedCancelEpoch": 0,
    });
    if action == "cancel" {
        request["reason"] = json!("stop");
    }
    request
}

pub fn events(events: Vec<Value>) -> CommandEffect {
    CommandEffect {
        events,
        ..CommandEffect::default()
    }
}

/// A stepped clock (`new Date(nowMs).toISOString()`), advanced by hand.
#[derive(Clone)]
pub struct TestClock(Arc<AtomicI64>);

impl TestClock {
    pub fn new() -> TestClock {
        // Date.UTC(2026, 8, 15, 0, 0, 0)
        TestClock(Arc::new(AtomicI64::new(1_789_430_400_000)))
    }

    pub fn advance(&self, millis: i64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }

    pub fn clock(&self) -> Clock {
        let now = Arc::clone(&self.0);
        Arc::new(move || pa_core::session::manager::format_iso(now.load(Ordering::SeqCst)))
    }
}

/// Ample free space: the host's own disk fullness never decides a test
/// (the free-space floor has its own test).
pub fn ample_space() -> FilesystemProbe {
    Arc::new(|_: &Path| {
        Some(FilesystemSpace {
            free: 1 << 40,
            total: 1 << 41,
        })
    })
}

pub fn options(root: &Path, clock: &TestClock) -> StoreOptions {
    StoreOptions {
        root: root.to_path_buf(),
        root_scope_digest: SCOPE.to_string(),
        clock: Some(clock.clock()),
        filesystem: Some(ample_space()),
    }
}

/// A private temp dir and its `workflows` store root.
pub fn store_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workflows");
    (dir, root)
}

/// Open and acquire at `epoch`.
pub fn open_writer(root: &Path, clock: &TestClock, epoch: u64) -> Store {
    let mut store = Store::open(options(root, clock)).unwrap();
    store.acquire_writer(epoch).unwrap();
    store
}

/// Drive `run_id` through create..AttemptAdmissionBound (TS
/// `driveToAdmissionBound`): attempt `a1`, outbox `op1` (`hostreq-1`),
/// child `child1`, turn `turn1`.
pub fn drive_to_admission_bound(store: &mut Store, run_id: &str) {
    let e = evidence();
    store
        .apply_command(&create_request(run_id), &create_effect(run_id))
        .unwrap();
    for (sequence, command, kind, data) in [
        (2, "c-start", "RunStarted", json!({ "evidenceDigest": e })),
        (
            3,
            "c-ready",
            "NodeBecameReady",
            json!({ "nodeId": "n1", "evidenceDigest": e }),
        ),
        (
            4,
            "c-prep",
            "AttemptPrepared",
            json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
        ),
    ] {
        store
            .apply_command(
                &post_request("start", run_id, command, sequence - 1),
                &events(vec![ctrl_event(run_id, sequence, kind, data)]),
            )
            .unwrap();
    }
    store
        .apply_command(
            &post_request("start", run_id, "c-disp", 4),
            &CommandEffect {
                events: vec![ctrl_event(
                    run_id,
                    5,
                    "AttemptDispatchCommitted",
                    json!({ "nodeId": "n1", "attemptId": "a1", "evidenceDigest": e }),
                )],
                operations: vec![OutboxEnqueue {
                    operation_id: "op1".to_string(),
                    attempt_id: Some("a1".to_string()),
                    kind: OperationKind::Deliver,
                    request_id: "hostreq-1".to_string(),
                    canonical_request: json!({ "protocol": "prime.workflow.retained-request/v2", "operation": "child.admit" }),
                }],
                ..CommandEffect::default()
            },
        )
        .unwrap();
    store
        .apply_command(
            &post_request("start", run_id, "c-bind", 5),
            &CommandEffect {
                events: vec![ctrl_event(
                    run_id,
                    6,
                    "AttemptAdmissionBound",
                    json!({
                        "nodeId": "n1", "attemptId": "a1", "operationId": "op1",
                        "rlmChildId": "child1", "turnId": "turn1", "evidenceDigest": e,
                    }),
                )],
                bindings: vec![ChildTurnBinding {
                    attempt_id: "a1".to_string(),
                    workflow_child_id: "wfc-a1".to_string(),
                    rlm_child_id: "child1".to_string(),
                    turn_id: "turn1".to_string(),
                    request_id: "hostreq-1".to_string(),
                    canonical_digest: e,
                }],
                ..CommandEffect::default()
            },
        )
        .unwrap();
}

// ---------------------------------------------------------------------------
// TS-shaped JSON views.
// ---------------------------------------------------------------------------

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn opt_text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_string)
}

/// A TS `CommandEffect` object.
pub fn effect_from_json(value: &Value) -> CommandEffect {
    let list = |key: &str| value[key].as_array().cloned().unwrap_or_default();
    CommandEffect {
        events: list("events"),
        operations: list("operations")
            .iter()
            .map(|op| OutboxEnqueue {
                operation_id: text(op, "operationId"),
                attempt_id: opt_text(op, "attemptId"),
                kind: match op["kind"].as_str() {
                    Some("cancel") => OperationKind::Cancel,
                    Some("delete") => OperationKind::Delete,
                    _ => OperationKind::Deliver,
                },
                request_id: text(op, "requestId"),
                canonical_request: op["canonicalRequest"].clone(),
            })
            .collect(),
        definition: value.get("definition").cloned(),
        budget: value.get("budget").map(|budget| RunBudget {
            max_concurrent_attempts: budget["maxConcurrentAttempts"].as_u64().unwrap(),
            max_total_tokens: budget["maxTotalTokens"].as_u64().unwrap(),
        }),
        bindings: list("bindings")
            .iter()
            .map(|b| ChildTurnBinding {
                attempt_id: text(b, "attemptId"),
                workflow_child_id: text(b, "workflowChildId"),
                rlm_child_id: text(b, "rlmChildId"),
                turn_id: text(b, "turnId"),
                request_id: text(b, "requestId"),
                canonical_digest: text(b, "canonicalDigest"),
            })
            .collect(),
        acceptance: list("acceptance")
            .iter()
            .map(|a| AcceptanceDecision {
                node_id: text(a, "nodeId"),
                attempt_id: opt_text(a, "attemptId"),
                decision: match a["decision"].as_str() {
                    Some("accepted") => Decision::Accepted,
                    Some("rejected") => Decision::Rejected,
                    _ => Decision::NotEvaluated,
                },
                evidence_digest: opt_text(a, "evidenceDigest"),
            })
            .collect(),
        tombstones: list("tombstones")
            .iter()
            .map(|t| TombstoneBinding {
                rlm_child_id: text(t, "rlmChildId"),
                request_id: text(t, "requestId"),
                tombstone_digest: text(t, "tombstoneDigest"),
            })
            .collect(),
    }
}

pub fn acknowledgement_from_json(value: &Value) -> Acknowledgement {
    Acknowledgement {
        operation_id: text(value, "operationId"),
        owner: text(value, "owner"),
        host_request_id: text(value, "hostRequestId"),
        outcome: match value["outcome"].as_str() {
            Some("failed") => OperationOutcome::Failed,
            Some("ambiguous") => OperationOutcome::Ambiguous,
            _ => OperationOutcome::Succeeded,
        },
        receipt: value["receipt"].clone(),
    }
}

/// `{error: code}` as the TS step runner records a throw.
pub fn error_json(error: &StoreError) -> Value {
    let code = match error {
        StoreError::Refused { code, .. } => code.as_str().to_string(),
        StoreError::Reducer(error) => error.code.as_str().to_string(),
        other => other.to_string(),
    };
    json!({ "error": code })
}

pub fn claimed_json(claimed: &[ClaimedOperation]) -> Value {
    Value::Array(
        claimed
            .iter()
            .map(|op| {
                json!({
                    "operationId": op.operation_id,
                    "runId": op.run_id,
                    "attemptId": op.attempt_id,
                    "kind": op.kind,
                    "requestId": op.request_id,
                    "canonicalRequest": op.canonical_request,
                    "hostRequestId": op.host_request_id,
                    "claimEpoch": op.claim_epoch,
                    "claimExpiresAt": op.claim_expires_at,
                })
            })
            .collect(),
    )
}

pub fn events_json(rows: &[EventRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "sequence": row.sequence,
                    "event_id": row.event_id,
                    "revision": row.revision,
                    "type": row.kind,
                    "controller_epoch": row.controller_epoch,
                    "cancel_epoch": row.cancel_epoch,
                    "recorded_at": row.recorded_at,
                    "data_json": row.data_json,
                    "digest": row.digest,
                })
            })
            .collect(),
    )
}

/// The TS `RunAggregate` JSON (`JSON.parse(JSON.stringify(aggregate))`).
pub fn aggregate_json(aggregate: &RunAggregate) -> Value {
    let mut nodes = Map::new();
    for node in &aggregate.nodes {
        nodes.insert(
            node.node_id.clone(),
            serde_json::to_value(&node.projection).unwrap(),
        );
    }
    let mut attempts = Map::new();
    for attempt in &aggregate.attempts {
        attempts.insert(
            attempt.attempt_id.clone(),
            json!({
                "attemptId": attempt.attempt_id,
                "nodeId": attempt.node_id,
                "operationId": attempt.operation_id,
                "rlmChildId": attempt.rlm_child_id,
                "turnId": attempt.turn_id,
                "settlementDigest": attempt.settlement_digest,
                "projection": attempt.projection,
                "turn": attempt.turn,
            }),
        );
    }
    json!({
        "runId": aggregate.run_id,
        "revision": aggregate.revision,
        "controllerEpoch": aggregate.controller_epoch,
        "cancelEpoch": aggregate.cancel_epoch,
        "run": aggregate.run,
        "nodes": nodes,
        "nodeOrder": aggregate.nodes.iter().map(|node| node.node_id.clone()).collect::<Vec<_>>(),
        "attempts": attempts,
        "attemptOrder": aggregate.attempts.iter().map(|a| a.attempt_id.clone()).collect::<Vec<_>>(),
        "hostCursor": aggregate.host_cursor,
        "lastControllerSequence": aggregate.last_controller_sequence,
        "appliedHostEventIds": aggregate.applied_host_event_ids,
    })
}

// ---------------------------------------------------------------------------
// The typed dump (`generate.mjs` `dump`).
// ---------------------------------------------------------------------------

/// Every table row as SQL `quote()` literals in rowid order, the schema,
/// and the identity pragmas; `store_migrations.applied_at` masked.
pub fn dump(path: &Path) -> Value {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let scalar = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
    let mut statement = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    let schema: Vec<Value> = statement
        .query_map([], |row| {
            Ok(json!({
                "type": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "tbl_name": row.get::<_, String>(2)?,
                "sql": row.get::<_, Option<String>>(3)?,
            }))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut tables = Map::new();
    for name in names {
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({name})"))
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let select = columns
            .iter()
            .map(|column| format!("quote({column})"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = conn
            .prepare(&format!("SELECT {select} FROM {name} ORDER BY rowid"))
            .unwrap();
        let rows: Vec<Value> = statement
            .query_map([], |row| {
                let mut object = Map::new();
                for (index, column) in columns.iter().enumerate() {
                    object.insert(column.clone(), Value::String(row.get(index)?));
                }
                Ok(Value::Object(object))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.insert(name, Value::Array(rows));
    }
    if let Some(Value::Array(rows)) = tables.get_mut("store_migrations") {
        for row in rows {
            row["applied_at"] = json!("<masked>");
        }
    }
    json!({
        "applicationId": scalar("PRAGMA application_id"),
        "userVersion": scalar("PRAGMA user_version"),
        "schema": schema,
        "tables": tables,
    })
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v2-store")
        .join(name)
}

pub fn read_json(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture(name)).unwrap()).unwrap()
}

/// The first path where two values differ (`None` when equal), for
/// readable golden failures.
pub fn first_difference(left: &Value, right: &Value, path: &str) -> Option<String> {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()) {
                let (x, y) = (
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                );
                if let Some(found) = first_difference(x, y, &format!("{path}.{key}")) {
                    return Some(found);
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            for index in 0..a.len().max(b.len()) {
                let (x, y) = (
                    a.get(index).unwrap_or(&Value::Null),
                    b.get(index).unwrap_or(&Value::Null),
                );
                if let Some(found) = first_difference(x, y, &format!("{path}[{index}]")) {
                    return Some(found);
                }
            }
            None
        }
        _ if left == right => None,
        _ => Some(format!("{path}: rust {left} != ts {right}")),
    }
}

/// `assert_eq!` with the first differing path in the message.
pub fn assert_same(rust: &Value, ts: &Value, what: &str) {
    if let Some(difference) = first_difference(rust, ts, "$") {
        panic!("{what}: {difference}");
    }
}

/// The slice-3 settlement cases (`fixtures/v2-settlement/cases.json`).
pub fn read_settlement_cases() -> Vec<Value> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-settlement/cases.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
