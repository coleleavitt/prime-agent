//! The durable run record: written per committed transition, reloaded by a
//! restarted host, and reconciled so `resume` continues an interrupted run
//! (guarded transitions and re-entry included). New with the host port:
//! the kernel executor had no persistence.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::fake::{Case, done, node_status, running, strings};
use crate::factory::executor::store::INTERRUPTED_PAUSE_REASON;

fn record(dir: &Path, run_id: &str) -> Value {
    let bytes = std::fs::read(dir.join(format!("{run_id}.json"))).expect("record on disk");
    serde_json::from_slice(&bytes).expect("record json")
}

fn store_case() -> (tempfile::TempDir, PathBuf, Case) {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let dir = temp.path().join("factory-runs");
    let case = Case::with_store(Some(dir.clone()));
    (temp, dir, case)
}

/// The review loop with a guarded switch and bounded re-entry.
fn review_loop() -> Value {
    json!({
        "run": { "failure_policy": "continue", "max_parallel": 4 },
        "states": [
            { "id": "draft", "entry": true, "subagent": "worker", "outputs": [{ "name": "draft", "type": "text" }] },
            { "id": "reviewing", "subagent": { "prompt": "Review: {draft}" },
              "inputs": [{ "name": "draft", "type": "text", "from": "draft.draft" }],
              "outputs": [{ "name": "verdict", "type": "json" }], "max_entries": 4 },
            { "id": "fixing", "subagent": "worker", "max_entries": 3 }
        ],
        "transitions": [
            { "from": "draft", "to": "reviewing" },
            { "from": "reviewing", "to": "fixing",
              "when": { "output": "verdict", "path": "approved", "op": "eq", "value": false } },
            { "from": "fixing", "to": "reviewing" }
        ]
    })
}

const REJECT: &str = "```json\n{\"verdict\": {\"approved\": false}}\n```";
const APPROVE: &str = "```json\n{\"verdict\": {\"approved\": true}}\n```";

#[tokio::test]
async fn every_committed_transition_reaches_the_record() {
    let (_temp, dir, case) = store_case();
    case.host.child_outcome("child-1", done("DRAFT"));
    case.host.child_outcome("child-2", done(REJECT));
    case.host.child_outcome("child-3", done("fixed"));
    case.host.child_outcome("child-4", running());
    case.store_machine(review_loop(), "sw");
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    // Round 2's reviewer is in flight: draft -> reviewing -> fixing ->
    // reviewing fired, three entries settled.
    case.wait_until(|| case.host.spawn_calls("reviewing").len() == 2)
        .await;
    case.executor.flush().await;
    let on_disk = record(&dir, &run_id);
    let live = case.executor.snapshot_run(&run_id).unwrap();
    assert_eq!(on_disk["version"], 1);
    assert_eq!(on_disk["run"], serde_json::to_value(&live).unwrap());
    assert_eq!(on_disk["run"]["transitions_fired"], 3);
    assert_eq!(on_disk["run"]["state"], "running");
    // The run finishes; the record follows it to the terminal state.
    case.host.child_outcome("child-4", done(APPROVE));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    case.executor.flush().await;
    let on_disk = record(&dir, &run_id);
    assert_eq!(on_disk["run"]["state"], "done");
    assert_eq!(
        on_disk["run"]["events"],
        serde_json::to_value(case.run_events(&result)).unwrap()
    );
}

#[tokio::test]
async fn a_host_restart_pauses_the_run_as_interrupted_and_resume_finishes_the_loop() {
    let (_temp, dir, case) = store_case();
    case.host.child_outcome("child-1", done("DRAFT"));
    case.host.child_outcome("child-2", done(REJECT));
    case.host.child_outcome("child-3", done("fixed"));
    case.host.child_outcome("child-4", running());
    case.store_machine(review_loop(), "sw");
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.wait_until(|| case.host.spawn_calls("reviewing").len() == 2)
        .await;
    case.executor.flush().await;
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    // The host process dies: its session (and the executor) goes away.
    drop(case);

    let restarted = Case::over(host, clock, Some(dir.clone()));
    assert_eq!(restarted.executor.run_ids(), std::slice::from_ref(&run_id));
    let status = restarted.status(&run_id);
    assert_eq!(status["state"], "paused");
    let run = restarted.executor.snapshot_run(&run_id).unwrap();
    assert_eq!(run.pause_reason.as_deref(), Some(INTERRUPTED_PAUSE_REASON));
    let reviewing = node_status(&status, "reviewing");
    let statuses: Vec<String> = reviewing["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|instance| instance["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        statuses,
        strings(&["done", "pending"]),
        "the lost reviewer re-queues"
    );
    let interrupted = restarted.all_events_of(&result, "interrupted");
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0]["node"], "reviewing");
    assert_eq!(interrupted[0]["child"], "child-4");
    assert_eq!(restarted.all_events_of(&result, "run_interrupted").len(), 1);
    // The record carries the reconciliation without waiting for a mutation.
    restarted.executor.flush().await;
    assert_eq!(record(&dir, &run_id)["run"]["state"], "paused");

    // Resume: the lost child is retired, the reviewer re-admits under a
    // fresh attempt label, and the guarded loop runs to completion.
    restarted.host.child_outcome("child-5", done(REJECT));
    restarted.host.child_outcome("child-6", done("fixed again"));
    restarted.host.child_outcome("child-7", done(APPROVE));
    let resumed = restarted
        .resume(&result)
        .await
        .expect("interrupted run resumes");
    assert_eq!(resumed["state"], "running");
    let finished = restarted.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert!(
        restarted
            .host
            .deleted_targets()
            .contains(&"child-4".to_string())
    );
    let reviewing = node_status(&finished, "reviewing");
    assert_eq!(reviewing["entries_used"], 3);
    assert_eq!(node_status(&finished, "fixing")["entries_used"], 2);
    assert_eq!(finished["usage"]["transitions_fired"], 5);
    let names = restarted.host.spawn_names();
    let retry_label = names
        .iter()
        .find(|name| name.starts_with("sw-reviewing-") && name.ends_with("-i1-a2"))
        .cloned();
    assert!(
        retry_label.is_some(),
        "the re-admitted reviewer carries the attempt suffix: {names:?}"
    );
    // Re-entry re-bound the draft into every reviewer prompt.
    assert!(
        restarted
            .host
            .spawn_prompts("reviewing")
            .iter()
            .all(|prompt| prompt == "Review: DRAFT")
    );
}

#[tokio::test]
async fn a_paused_run_stays_paused_across_a_restart_and_keeps_its_reason() {
    let (_temp, dir, case) = store_case();
    case.store_machine(
        json!({
            "run": { "max_transitions": 1, "failure_policy": "continue" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker" },
                { "id": "b", "subagent": "worker" },
                { "id": "c", "subagent": "worker" }
            ],
            "transitions": [{ "from": "a", "to": "b" }, { "from": "b", "to": "c" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    case.executor.flush().await;
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);
    let restarted = Case::over(host, clock, Some(dir));
    let run_id = Case::run_id(&result);
    let run = restarted.executor.snapshot_run(&run_id).unwrap();
    assert_eq!(
        run.pause_reason.as_deref(),
        Some("max_transitions exceeded")
    );
    assert_eq!(
        restarted.all_events_of(&result, "run_interrupted"),
        Vec::<Value>::new()
    );
    restarted.resume(&result).await.expect("resume");
    let finished = restarted.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(finished["usage"]["transitions_fired"], 2);
    assert_eq!(
        restarted.host.spawn_calls("b").len(),
        1,
        "the paused transition never re-fires"
    );
}

#[tokio::test]
async fn terminal_runs_reload_verbatim() {
    let (_temp, dir, case) = store_case();
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    case.settle(&result).await;
    case.executor.flush().await;
    let run_id = Case::run_id(&result);
    let before = case.executor.snapshot_run(&run_id).unwrap();
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);
    let restarted = Case::over(host, clock, Some(dir));
    let mut after = restarted.executor.snapshot_run(&run_id).unwrap();
    // Derived indices are rebuilt, never persisted.
    after.index();
    let mut expected = before;
    expected.index();
    assert_eq!(after, expected);
}

#[tokio::test]
async fn a_restart_mid_stop_finishes_stopping() {
    let (_temp, dir, case) = store_case();
    case.host.outcome("a", running());
    case.store_factory(json!({ "nodes": [{ "id": "a", "subagent": "worker" }, { "id": "b", "subagent": "worker", "depends_on": ["a"] }] }), "sw");
    let result = case.start().await;
    case.wait_until(|| case.host.collects() >= 1).await;
    let delete_gate = case.host.gate("rlm.delete_subagent", 1);
    let entered = case.host.gate_entered("rlm.delete_subagent", 1);
    let stop = {
        let executor = Arc::clone(&case.executor);
        let run_id = Case::run_id(&result);
        tokio::spawn(async move { executor.stop(&run_id).await })
    };
    entered.wait().await; // the host dies mid-stop
    case.executor.flush().await;
    stop.abort();
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);
    delete_gate.set();
    let restarted = Case::over(host, clock, Some(dir));
    let status = restarted.status(&Case::run_id(&result));
    assert_eq!(status["state"], "stopped");
    assert_eq!(node_status(&status, "a")["status"], "cancelled");
    assert_eq!(node_status(&status, "b")["status"], "cancelled");
    let stopped = restarted.all_events_of(&result, "run_stopped");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0]["detail"], "stopped; the host restarted mid-stop");
}

#[tokio::test]
async fn unreadable_records_are_skipped_not_fatal() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let dir = temp.path().join("factory-runs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("broken.json"), b"{ not json").unwrap();
    std::fs::write(dir.join("future.json"), br#"{"version": 99, "run": {}}"#).unwrap();
    let case = Case::with_store(Some(dir));
    assert_eq!(case.executor.run_ids(), Vec::<String>::new());
}
