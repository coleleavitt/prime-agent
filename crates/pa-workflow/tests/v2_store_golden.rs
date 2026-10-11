//! Format parity with the TS fork's slice-4 store. `fixtures/v2-store/`
//! was produced by running the TS store itself under node
//! (`fixtures/v2-store/generate.mjs`, sources from commit 5e2b7f2fe): the
//! 58-step scenario, each step's TS result, a typed dump of every table of
//! the database TS wrote, and that database file.
//!
//! - The Rust store replays the same steps: every result and every stored
//!   byte (SQL `quote()` of each column, the schema text, the pragmas)
//!   equal the TS ones.
//! - The TS-written database opens in Rust (migration checksum, scope,
//!   integrity), hydrates to the TS aggregates, and takes further commands.
//! - The reverse direction (TS opens a Rust-written database) runs the TS
//!   store again (`ts_opens_a_rust_written_store`, ignored without node).

mod v2_support;

use std::path::Path;

use pa_workflow::v2::store::{STORE_FILE_NAME, Store};
use serde_json::{Value, json};
use v2_support::{
    SCOPE,
    TestClock,
    acknowledgement_from_json,
    aggregate_json,
    claimed_json,
    effect_from_json,
    error_json,
    events_json,
    fixture,
    read_json,
};

/// Run one scenario step against the Rust store, answering the TS-shaped
/// result.
fn run_step(step: &Value, root: &Path, clock: &TestClock, store: &mut Option<Store>) -> Value {
    let field = |key: &str| step[key].as_str().unwrap_or_default();
    let number = |key: &str| step[key].as_u64().unwrap_or_default();
    let settle = |result: Result<Value, pa_workflow::v2::store::StoreError>| match result {
        Ok(value) => value,
        Err(error) => error_json(&error),
    };
    match field("op") {
        "open" | "reopen" => {
            drop(store.take());
            let mut opened = Store::open(v2_support::options(root, clock)).unwrap();
            let epoch = settle(
                opened
                    .acquire_writer(number("epoch"))
                    .map(|epoch| json!(epoch)),
            );
            *store = Some(opened);
            epoch
        }
        "close" => {
            drop(store.take());
            Value::Null
        }
        "advance" => {
            clock.advance(step["ms"].as_i64().unwrap());
            Value::Null
        }
        op => {
            let store = store.as_mut().unwrap();
            settle(match op {
                "command" => store
                    .apply_command(&step["request"], &effect_from_json(&step["effect"]))
                    .map(|receipt| serde_json::to_value(receipt).unwrap()),
                "ingest" => store
                    .ingest_host_event(field("runId"), &step["event"])
                    .map(|ingestion| json!({ "applied": ingestion.applied, "hostCursor": ingestion.host_cursor })),
                "claim" => store
                    .claim_outbox(field("owner"), number("leaseMs"), number("limit"))
                    .map(|claimed| claimed_json(&claimed)),
                "ack" => store
                    .acknowledge_outbox(&acknowledgement_from_json(step))
                    .map(|()| Value::Null),
                "erase" => store
                    .erase_run_text(field("runId"), field("policyBasis"))
                    .map(|freed| json!({ "erased": freed > 0, "freedBytes": freed })),
                "compact" => store
                    .compact_events(field("runId"), number("throughSequence"), field("snapshotDigest"))
                    .map(|compacted| json!({ "compacted": compacted })),
                "listEvents" => store
                    .list_events(field("runId"), number("afterSequence"), number("limit"))
                    .map(|rows| events_json(&rows)),
                "reconcile" => store
                    .reconcile_terminal_mismatch(field("runId"))
                    .map(|quarantined| json!({ "quarantined": quarantined })),
                "aggregate" => store
                    .load_aggregate(field("runId"))
                    .map(|aggregate| aggregate.as_ref().map_or(Value::Null, aggregate_json)),
                "hostCursor" => store.host_cursor(field("runId")).map(|cursor| json!(cursor)),
                other => panic!("unknown scenario step {other}"),
            })
        }
    }
}

fn replay(root: &Path) -> Vec<Value> {
    let scenario = read_json("scenario.json");
    assert_eq!(scenario["scope"], SCOPE);
    let clock = TestClock::new();
    let mut store = None;
    scenario["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| run_step(step, root, &clock, &mut store))
        .collect()
}

#[test]
fn the_rust_store_answers_every_step_as_the_ts_store_did() {
    let (_dir, root) = v2_support::store_root();
    let results = replay(&root);
    let scenario = read_json("scenario.json");
    let expected = scenario["results"].as_array().unwrap();
    assert_eq!(results.len(), expected.len());
    for (index, (rust, ts)) in results.iter().zip(expected).enumerate() {
        v2_support::assert_same(
            rust,
            ts,
            &format!("step {index} {}", scenario["steps"][index]["op"]),
        );
    }
}

#[test]
fn the_rust_store_writes_the_ts_bytes() {
    let (_dir, root) = v2_support::store_root();
    replay(&root);
    assert!(!root.join(format!("{STORE_FILE_NAME}-wal")).exists());
    v2_support::assert_same(
        &v2_support::dump(&root.join(STORE_FILE_NAME)),
        &read_json("dump.json"),
        "dump",
    );
}

#[test]
fn the_committed_ts_database_is_the_dumped_one() {
    // Dump a copy: opening the WAL-mode fixture in place would leave its
    // shared-memory files in the source tree.
    let (_dir, root) = ts_store_root();
    v2_support::assert_same(
        &v2_support::dump(&root.join(STORE_FILE_NAME)),
        &read_json("dump.json"),
        "committed fixture",
    );
    assert!(!fixture("ts-v2.sqlite-shm").exists() && !fixture("ts-v2.sqlite-wal").exists());
}

/// Copy the TS-written database into a fresh owner-only store root.
fn ts_store_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let (dir, root) = v2_support::store_root();
    pa_core::platform::perms::create_dir_all_private(&root).unwrap();
    let db = root.join(STORE_FILE_NAME);
    std::fs::copy(fixture("ts-v2.sqlite"), &db).unwrap();
    pa_core::platform::perms::restrict_file(&db).unwrap();
    (dir, root)
}

#[test]
fn a_ts_written_store_opens_hydrates_and_continues_in_rust() {
    let (_dir, root) = ts_store_root();
    let clock = TestClock::new();
    let mut store = Store::open(v2_support::options(&root, &clock)).unwrap();
    // TS left the writer at epoch 2: a restart fences strictly above it.
    assert_eq!(
        store
            .acquire_writer(2)
            .unwrap_err()
            .code()
            .map(pa_workflow::v2::store::StoreCode::as_str),
        Some("store_epoch_stale")
    );
    store.acquire_writer(3).unwrap();

    let scenario = read_json("scenario.json");
    let steps = scenario["steps"].as_array().unwrap();
    for (index, step) in steps.iter().enumerate() {
        if step["op"] == "aggregate" {
            let run_id = step["runId"].as_str().unwrap();
            let aggregate = store.load_aggregate(run_id).unwrap().unwrap();
            assert_eq!(
                aggregate_json(&aggregate),
                scenario["results"][index],
                "{run_id}"
            );
        }
    }

    // run-2's settled cancelled attempt is rejected under the Rust writer:
    // the fact continues TS's gap-free sequence.
    let request = json!({
        "protocol": "prime.workflow.request/v2",
        "requestId": "req-rust-reject",
        "action": "cancel",
        "runId": "run-2",
        "commandId": "rust-reject",
        "expectedRevision": 10,
        "expectedControllerEpoch": 1,
        "expectedCancelEpoch": 0,
        "reason": "rust continues",
    });
    let effect = effect_from_json(&json!({
        "events": [v2_support::ctrl_event("run-2", 11, "AttemptRejected", json!({
            "nodeId": "n1", "attemptId": "a2", "evidenceDigest": v2_support::evidence(),
        }))],
    }));
    let receipt = store.apply_command(&request, &effect).unwrap();
    assert_eq!(receipt.applied_sequences, vec![11]);
    let aggregate = store.load_aggregate("run-2").unwrap().unwrap();
    assert_eq!(
        (
            aggregate.attempts[0].projection.phase.as_str(),
            aggregate.attempts[0].projection.outcome.as_deref(),
            aggregate.nodes[0].projection.phase.as_str(),
        ),
        ("terminal", Some("failed"), "exhausted")
    );
    // The TS compaction snapshot still fences the compacted prefix.
    assert_eq!(
        store
            .list_events("run-1", 0, 500)
            .unwrap_err()
            .code()
            .map(pa_workflow::v2::store::StoreCode::as_str),
        Some("SNAPSHOT_REQUIRED")
    );
}

#[test]
#[ignore = "runs the TS store under node; set PA_WORKFLOW_TS_SOURCES to the extracted sources (see generate.mjs)"]
fn ts_opens_a_rust_written_store() {
    let sources = std::env::var("PA_WORKFLOW_TS_SOURCES").expect("PA_WORKFLOW_TS_SOURCES");
    let (_dir, root) = v2_support::store_root();
    let results = replay(&root);
    let output = std::process::Command::new("node")
        .arg(fixture("generate.mjs"))
        .arg(&sources)
        .arg("--verify")
        .arg(&root)
        .arg("3")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ts: Value = serde_json::from_slice(&output.stdout).unwrap();
    let scenario = read_json("scenario.json");
    for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        if step["op"] == "aggregate" {
            assert_eq!(ts[step["runId"].as_str().unwrap()], results[index]);
        }
    }
}
