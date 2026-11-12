//! The durable run record: written per committed transition, reloaded by a
//! restarted host, and reconciled so `resume` continues an interrupted run
//! (guarded transitions and re-entry included). New with the host port:
//! the kernel executor had no persistence.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::fake::{Case, done, exhaust_restart_adoptions, node_status, running, strings};
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
async fn a_restart_past_the_adoption_cap_pauses_the_run_as_interrupted_and_resume_finishes_the_loop()
 {
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
    exhaust_restart_adoptions(&dir, &run_id);

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

#[tokio::test]
async fn a_host_restart_adopts_the_running_child_and_the_run_carries_on() {
    // The fork's children outlive the host: a restart re-drives the running
    // reviewer through the idempotent spawn, which adopts it - no second
    // child, no delete, no pause.
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
    drop(case);

    let restarted = Case::over(host, clock, Some(dir));
    restarted
        .wait_until(|| !restarted.all_events_of(&result, "adopted").is_empty())
        .await;
    let adopted: Vec<(Value, Value)> = restarted
        .all_events_of(&result, "adopting")
        .into_iter()
        .chain(restarted.all_events_of(&result, "adopted"))
        .map(|event| (event["kind"].clone(), event["child"].clone()))
        .collect();
    assert_eq!(
        adopted,
        [
            (json!("adopting"), json!("child-4")),
            (json!("adopted"), json!("child-4"))
        ]
    );
    restarted.host.child_outcome("child-4", done(APPROVE));
    let finished = restarted.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(restarted.host.spawn_calls("reviewing").len(), 2);
    assert!(restarted.host.deleted_targets().is_empty());
    assert_eq!(
        restarted.all_events_of(&result, "run_interrupted"),
        Vec::<Value>::new()
    );
    let run = restarted.executor.snapshot_run(&run_id).unwrap();
    assert_eq!(run.spawn_count, 4, "the adoption is no new admission");
}

#[tokio::test]
async fn a_child_lost_with_the_host_restart_pauses_the_run_as_interrupted() {
    // The host knows the re-driven child but it did not survive the restart
    // (its worker died with the host): the instance re-queues and the run
    // pauses as interrupted; resume deletes the lost child and admits the
    // instance afresh.
    let (_temp, dir, case) = store_case();
    case.host.child_outcome("child-1", running());
    case.host.child_outcome("child-2", done("ok"));
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    case.executor.flush().await;
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);
    host.with(|state| state.lost_children.insert("child-1".to_string()));

    let restarted = Case::over(host, clock, Some(dir));
    let paused = restarted.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    let lost: Vec<(Value, Value)> = ["adopting", "interrupted", "run_interrupted"]
        .into_iter()
        .flat_map(|event_kind| restarted.all_events_of(&result, event_kind))
        .map(|event| (event["kind"].clone(), event["child"].clone()))
        .collect();
    assert_eq!(
        lost,
        [
            (json!("adopting"), json!("child-1")),
            (json!("interrupted"), json!("child-1")),
            (json!("run_interrupted"), Value::Null)
        ]
    );

    restarted.resume(&result).await.expect("resume");
    let finished = restarted.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(restarted.host.deleted_targets(), ["child-1"]);
    assert_eq!(restarted.host.spawn_calls("a").len(), 2);
}

/// Start the stored spec until the host is parked at `parked` (the start's
/// admission never answers: the host dies there), and name the run.
async fn start_until(case: &Case, parked: &super::fake::Event) -> Value {
    let start = case.start();
    tokio::pin!(start);
    tokio::select! {
        _ = &mut start => panic!("the admission must park at the gate"),
        () = parked.wait() => {}
    }
    json!({ "run_id": case.executor.run_ids()[0] })
}

#[tokio::test]
async fn a_crash_between_the_admission_and_its_record_spawns_no_second_child() {
    // Write-before-effect: the picked child id is on disk before the host
    // admits the child, so a host that dies after the admission but before
    // the executor records it re-drives the same id and adopts the child.
    let (_temp, dir, case) = store_case();
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let _held = case.host.gate("rlm.run.admitted", 1);
    let admitted = case.host.gate_entered("rlm.run.admitted", 1);
    let result = start_until(&case, &admitted).await;
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);

    let restarted = Case::over(host, clock, Some(dir));
    let finished = restarted.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(restarted.host.spawn_calls("a").len(), 1);
    assert_eq!(restarted.all_events_of(&result, "adopted").len(), 1);
}

#[tokio::test]
async fn an_unknown_admission_pauses_the_run_and_resume_admits_it_afresh() {
    // The host keeps no record of the recorded child but its admission may
    // have run: the run pauses for the user instead of re-running it, and a
    // resume (the user's decision) admits the instance under a new id.
    let (_temp, dir, case) = store_case();
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let _held = case.host.gate("rlm.run", 1);
    let entered = case.host.gate_entered("rlm.run", 1);
    let result = start_until(&case, &entered).await;
    let run_id = Case::run_id(&result);
    let picked =
        record(&dir, &run_id)["run"]["states"][0]["entries"][0]["instances"][0]["pending_child"]
            .as_str()
            .expect("the picked id is recorded before the spawn")
            .to_string();
    let (host, clock) = (Arc::clone(&case.host), Arc::clone(&case.clock));
    drop(case);
    host.with(|state| state.unknown_admissions.insert(picked.clone()));

    let restarted = Case::over(host, clock, Some(dir));
    let paused = restarted.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    let unknown = restarted.all_events_of(&result, "admission_unknown");
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0]["child"], json!(picked));
    assert!(restarted.host.spawn_calls("a").is_empty());

    restarted.resume(&result).await.expect("resume");
    let finished = restarted.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(restarted.host.spawn_calls("a").len(), 1);
}
