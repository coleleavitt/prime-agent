//! Failure policies, retries, budgets, stop races, rate-limit backoff,
//! scale, and the ledger/notice contract (ports of `FactoryExecutorTest`).

use std::sync::Arc;

use serde_json::{json, Value};

use super::fake::{
    done, events_of, failed, instance_statuses, node_status, running, strings, Case, Event,
    SleepMode,
};
use crate::factory::executor::{BACKOFF_MAX_ATTEMPTS, EVENT_WINDOW, POLL_TIMEOUT_MS};

#[tokio::test]
async fn fail_fast_cancels_running_children() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    case.host.outcome("b", running());
    case.host.outcome("c", running());
    case.store_factory(
        json!({ "run": { "failure_policy": "continue", "max_parallel": 8 }, "nodes": [
            { "id": "a", "subagent": "worker", "failure_policy": "fail_fast" },
            { "id": "b", "subagent": "worker" },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(node_status(&status, "a")["status"], "error");
    assert_eq!(node_status(&status, "b")["status"], "cancelled");
    assert_eq!(node_status(&status, "c")["status"], "cancelled");
    assert_eq!(case.host.deleted_targets().len(), 2);
    assert_eq!(case.host.notice_kinds(), strings(&["failed"]));
}

#[tokio::test]
async fn continue_policy_finishes_remaining_nodes() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(node_status(&status, "a")["status"], "error");
    assert_eq!(node_status(&status, "b")["status"], "done");
    assert_eq!(node_status(&status, "c")["status"], "done");
    assert_eq!(case.host.deleted_targets(), Vec::<String>::new());
    assert_eq!(status["state"], "failed");
    assert_eq!(case.host.notice_kinds(), strings(&["failed"]));
}

fn store_escalating_chain(case: &Case) {
    case.store_factory(
        json!({ "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
        "sw",
    );
}

#[tokio::test]
async fn escalate_pauses_notifies_and_resume_continues() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    store_escalating_chain(&case);
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    assert_eq!(node_status(&paused, "a")["status"], "error");
    assert_eq!(node_status(&paused, "b")["status"], "pending");
    assert_eq!(case.host.spawn_calls("b"), Vec::<Value>::new());
    assert!(case.host.notice_kinds().contains(&"paused".to_string()));
    let resumed = case.resume(&result).await.expect("paused run resumes");
    assert_eq!(resumed["state"], "running");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "failed");
    assert_eq!(node_status(&finished, "b")["status"], "done");
    assert_eq!(case.host.notice_kinds(), strings(&["paused", "failed"]));
    let refusal = case
        .resume(&result)
        .await
        .expect_err("finished run refuses");
    assert!(refusal.0.contains("not paused"), "{refusal}");
}

#[tokio::test]
async fn resume_defers_rate_limited_admissions() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    case.host
        .with(|state| state.rate_limit_first.insert("b".into(), 2));
    store_escalating_chain(&case);
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    let mark = case.sleeps().len();
    let resumed = case.resume(&result).await.expect("resume");
    assert_eq!(resumed["started"], json!([]));
    assert_eq!(case.sleeps()[mark..].to_vec(), Vec::<f64>::new());
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "failed");
    assert_eq!(node_status(&finished, "b")["status"], "done");
    assert_eq!(case.host.spawn_calls("b").len(), 3);
    assert_eq!(case.sleeps()[mark..].to_vec(), vec![1.0]);
}

#[tokio::test]
async fn repeated_pause_records_every_milestone_in_the_ledger() {
    let case = Case::new();
    case.host.outcome("a", failed("a boom"));
    case.host.outcome("b", failed("b boom"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "escalate" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker" },
                { "id": "b", "subagent": "worker" }
            ],
            "transitions": [{ "from": "a", "to": "b" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let first = case.settle(&result).await;
    assert_eq!(first["state"], "paused");
    assert_eq!(events_of(&first, "milestone").len(), 1);
    case.resume(&result).await.expect("resume");
    let second = case.settle(&result).await;
    assert_eq!(second["state"], "paused");
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    assert_eq!(run.pause_reason.as_deref(), Some("b boom"));
    let milestones = events_of(&second, "milestone");
    let kinds: Vec<Value> = milestones
        .iter()
        .map(|event| event["milestone"].clone())
        .collect();
    assert_eq!(kinds, [json!("paused"), json!("paused")]);
    assert!(milestones[0]["detail"].as_str().unwrap().contains("a boom"));
    assert!(milestones[1]["detail"].as_str().unwrap().contains("b boom"));
    assert_eq!(case.host.notice_kinds(), strings(&["paused"]));
    assert_eq!(milestones[0]["stage"], "shown");
    assert_eq!(milestones[1]["stage"], "delivered");
}

#[tokio::test]
async fn retries_respawn_until_attempts_exhausted() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [{ "id": "a", "subagent": "worker", "retries": 2 }] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(case.host.spawn_calls("a").len(), 3);
    assert_eq!(node_status(&status, "a")["attempts"], 3);
    assert_eq!(node_status(&status, "a")["status"], "error");
    assert_eq!(status["state"], "failed");
}

#[tokio::test]
async fn node_budget_marks_attempt_failed() {
    let case = Case::new();
    case.clock.set_advance_per_collect(2.0);
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker", "budget_ms": 1000 },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    let a = node_status(&status, "a");
    assert_eq!(a["status"], "error");
    assert!(a["error"].as_str().unwrap().contains("budget"));
    assert_eq!(case.host.spawn_calls("a").len(), 1);
    assert_eq!(node_status(&status, "b")["status"], "done");
    assert_eq!(status["state"], "failed");
}

#[tokio::test]
async fn run_budget_pauses_and_notifies_then_resume_completes() {
    let case = Case::new();
    case.clock.set_advance_per_collect(2.0);
    case.store_factory(
        json!({ "run": { "failure_policy": "continue", "budget_ms": 1500 }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    assert!(case
        .host
        .notice_kinds()
        .contains(&"budget_exceeded".to_string()));
    assert_eq!(case.host.spawn_calls("b"), Vec::<Value>::new());
    let milestone = events_of(&paused, "milestone")
        .into_iter()
        .find(|event| event["milestone"] == "budget_exceeded")
        .expect("budget milestone");
    let detail = milestone["detail"].as_str().unwrap();
    assert!(
        detail.contains("no new spawns") && detail.contains("2000ms"),
        "{detail}"
    );
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    assert_eq!(run.pause_reason.as_deref(), Some("run budget exceeded"));
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(node_status(&finished, "b")["status"], "done");
    assert_eq!(
        case.host.notice_kinds(),
        strings(&["budget_exceeded", "finished"])
    );
}

#[tokio::test]
async fn admission_phase_stops_at_the_run_budget_boundary() {
    let case = Case::new();
    case.host.with(|state| state.advance_per_run = 2.0);
    case.store_factory(
        json!({ "run": { "failure_policy": "continue", "budget_ms": 3000, "max_parallel": 8 }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker" },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a", "b"]));
    assert_eq!(case.host.calls_of("rlm.run").len(), 2);
    let status = case.status(&Case::run_id(&result));
    assert_eq!(status["state"], "paused");
    assert!(case
        .host
        .notice_kinds()
        .contains(&"budget_exceeded".to_string()));
    let c = node_status(&status, "c");
    assert_eq!(c["status"], "running");
    assert_eq!(c["instances"][0]["status"], "pending");
    assert_eq!(case.host.spawn_calls("c"), Vec::<Value>::new());
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(node_status(&finished, "c")["status"], "done");
    assert_eq!(
        case.host.notice_kinds(),
        strings(&["budget_exceeded", "finished"])
    );
}

#[tokio::test]
async fn run_max_children_pauses_at_admission_then_resume_completes() {
    let case = Case::new();
    case.store_factory(
        json!({ "run": { "failure_policy": "continue", "max_children": 2 }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker" },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a", "b"]));
    assert_eq!(case.host.calls_of("rlm.run").len(), 2);
    let status = case.status(&Case::run_id(&result));
    assert_eq!(status["state"], "paused");
    let kinds = case.host.notice_kinds();
    assert!(kinds.contains(&"max_children_exceeded".to_string()));
    assert!(!kinds.contains(&"budget_exceeded".to_string()));
    let milestone = events_of(&status, "milestone")
        .into_iter()
        .find(|event| event["milestone"] == "max_children_exceeded")
        .expect("children milestone");
    let detail = milestone["detail"].as_str().unwrap();
    assert!(
        detail.contains("run max_children 2 exceeded after 2 children"),
        "{detail}"
    );
    assert!(detail.contains("no new spawns"));
    assert!(detail.contains("resume with await rlm.factory.resume"));
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    assert_eq!(run.pause_reason.as_deref(), Some("max_children exceeded"));
    assert_eq!(status["usage"]["spawns"], 2);
    assert_eq!(status["usage"]["max_children"], 2);
    let c = node_status(&status, "c");
    assert_eq!(c["status"], "running");
    assert_eq!(c["instances"][0]["status"], "pending");
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(node_status(&finished, "c")["status"], "done");
    assert_eq!(finished["usage"]["spawns"], 3);
    assert_eq!(
        case.host.notice_kinds(),
        strings(&["max_children_exceeded", "finished"])
    );
}

#[tokio::test]
async fn foreach_children_count_against_the_run_max_children_budget() {
    let case = Case::new();
    case.host.outcome(
        "src",
        done("```json\n{\"items\": [\"w\", \"x\", \"y\", \"z\"]}\n```"),
    );
    case.store_factory(
        json!({
            "run": { "failure_policy": "continue", "max_children": 2, "max_parallel": 8 },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": { "prompt": "Expand item {items}." }, "depends_on": ["src"],
                  "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 256 } }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    assert!(case
        .host
        .notice_kinds()
        .contains(&"max_children_exceeded".to_string()));
    assert_eq!(paused["usage"]["spawns"], 2);
    let fan = node_status(&paused, "fan");
    assert_eq!(
        instance_statuses(fan),
        strings(&["running", "pending", "pending", "pending"])
    );
    assert_eq!(case.host.spawn_calls("fan").len(), 1);
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(node_status(&finished, "fan")["status"], "done");
    assert_eq!(finished["usage"]["spawns"], 5);
    let mut prompts = case.host.spawn_prompts("fan");
    prompts.sort();
    assert_eq!(
        prompts,
        strings(&[
            "Expand item w.",
            "Expand item x.",
            "Expand item y.",
            "Expand item z."
        ])
    );
    assert_eq!(
        case.host.notice_kinds(),
        strings(&["max_children_exceeded", "finished"])
    );
}

#[tokio::test]
async fn stop_cancels_children_and_pending_nodes() {
    let case = Case::new();
    case.host.outcome("a", running());
    store_escalating_chain(&case);
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a"]));
    case.wait_until(|| case.host.collects() >= 1).await;
    let stopped = case.stop(&result).await;
    assert_eq!(
        stopped,
        json!({ "run_id": result["run_id"], "state": "stopped", "cancelled": ["a", "b"] })
    );
    assert_eq!(case.host.deleted_targets(), strings(&["child-1"]));
    let status = case.status(&Case::run_id(&result));
    assert_eq!(status["state"], "stopped");
    assert_eq!(node_status(&status, "a")["status"], "cancelled");
    assert_eq!(node_status(&status, "b")["status"], "cancelled");
    let again = case.stop(&result).await;
    assert_eq!(
        again,
        json!({ "run_id": result["run_id"], "state": "stopped", "cancelled": [] })
    );
    assert_eq!(case.all_events_of(&result, "run_stopped").len(), 1);
}

#[tokio::test]
async fn concurrent_stop_runs_one_cancellation_pass() {
    let case = Arc::new(Case::new());
    case.host.outcome("a", running());
    case.host.outcome("b", running());
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }, { "id": "b", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    case.wait_until(|| case.host.collects() >= 1).await;
    let delete_gate = case.host.gate("rlm.delete_subagent", 1);
    let stop_one = {
        let executor = Arc::clone(&case.executor);
        let run_id = Case::run_id(&result);
        tokio::spawn(async move { executor.stop(&run_id).await })
    };
    case.wait_until(|| case.run_state(&result) == "stopping")
        .await;
    let stop_two = case.stop(&result).await;
    assert_eq!(
        stop_two,
        json!({ "run_id": result["run_id"], "state": "stopping", "cancelled": [] })
    );
    delete_gate.set();
    let stopped = stop_one.await.expect("join").expect("known run");
    assert_eq!(stopped["state"], "stopped");
    let mut cancelled: Vec<String> = serde_json::from_value(stopped["cancelled"].clone()).unwrap();
    cancelled.sort();
    assert_eq!(cancelled, strings(&["a", "b"]));
    assert_eq!(case.status(&Case::run_id(&result))["state"], "stopped");
    let mut deleted = case.host.deleted_targets();
    deleted.sort();
    assert_eq!(deleted, strings(&["child-1", "child-2"]));
    assert_eq!(case.all_events_of(&result, "run_stopped").len(), 1);
}

#[tokio::test]
async fn stop_window_admits_no_new_spawns_and_never_finalizes() {
    let case = Arc::new(Case::new());
    case.store_factory(
        json!({ "run": { "max_parallel": 2 }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker" },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let collect_gate = case.host.gate("rlm.collect", 1);
    let delete_gate = case.host.gate("rlm.delete_subagent", 1);
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a", "b"]));
    let run_id = Case::run_id(&result);
    tokio::task::yield_now().await; // the loop reaches collect #1 and suspends
    let stop_task = {
        let executor = Arc::clone(&case.executor);
        let run_id = run_id.clone();
        tokio::spawn(async move { executor.stop(&run_id).await })
    };
    case.wait_until(|| case.run_state(&result) == "stopping")
        .await;
    collect_gate.set();
    case.executor.join(&run_id).await; // the loop exits on "stopping" without admitting c
    delete_gate.set();
    let stopped = stop_task.await.expect("join").expect("known run");
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(case.status(&run_id)["state"], "stopped");
    assert_eq!(case.host.notice_kinds(), Vec::<String>::new());
    assert_eq!(case.host.calls_of("rlm.run").len(), 2);
}

#[tokio::test]
async fn stop_during_in_flight_admission_deletes_the_child() {
    let case = Case::new();
    case.host
        .with(|state| state.rate_limit_first.insert("a".into(), 1));
    let spawn_gate = case.host.gate("rlm.run", 2);
    let entered = case.host.gate_entered("rlm.run", 2);
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!([]));
    entered.wait().await; // the loop is suspended mid-spawn
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["a"]));
    spawn_gate.set();
    let run_id = Case::run_id(&result);
    case.executor.join(&run_id).await;
    let status = case.status(&run_id);
    let instance = &node_status(&status, "a")["instances"][0];
    assert_eq!(status["state"], "stopped");
    assert_eq!(instance["status"], "cancelled");
    assert_eq!(instance["child"], "child-1");
    assert_eq!(case.host.deleted_targets(), strings(&["child-1"]));
    assert_eq!(status["usage"]["spawns"], 0);
    assert_eq!(case.host.spawn_calls("a").len(), 2);
    let kinds: Vec<String> = status["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_string())
        .collect();
    let at = kinds.iter().position(|kind| kind == "run_stopped").unwrap();
    assert_eq!(kinds[at..].to_vec(), strings(&["run_stopped", "cancelled"]));
}

#[tokio::test]
async fn stop_during_admission_backoff_cancels_the_instance() {
    let case = Case::new();
    let (entered, release) = (Event::default(), Event::default());
    case.set_sleep(SleepMode::Gated {
        entered: entered.clone(),
        release: release.clone(),
    });
    case.host
        .with(|state| state.rate_limit_first.insert("a".into(), 2));
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!([]));
    entered.wait().await; // the loop waits out the backoff slice
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["a"]));
    release.set();
    let run_id = Case::run_id(&result);
    case.executor.join(&run_id).await;
    let status = case.status(&run_id);
    assert_eq!(status["state"], "stopped");
    assert_eq!(
        node_status(&status, "a")["instances"][0]["status"],
        "cancelled"
    );
    assert_eq!(case.host.spawn_calls("a").len(), 2);
    assert_eq!(case.host.deleted_targets(), Vec::<String>::new());
    assert_eq!(status["usage"]["spawns"], 0);
}

#[tokio::test]
async fn stop_cancels_an_in_flight_earlier_entry_of_a_reentered_state() {
    let case = Case::new();
    case.host.child_outcome("child-3", running());
    case.host.child_outcome("child-4", done("x-two"));
    case.store_machine(
        json!({
            "run": { "max_parallel": 4 },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker" },
                { "id": "b", "entry": true, "subagent": "worker" },
                { "id": "x", "subagent": "worker", "max_entries": 2 }
            ],
            "transitions": [{ "from": "a", "to": "x" }, { "from": "b", "to": "x" }]
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a", "b"]));
    let run_id = Case::run_id(&result);
    case.wait_until(|| {
        let run = case.executor.snapshot_run(&run_id).unwrap();
        run.states[2]
            .entries
            .iter()
            .any(|entry| entry.status.as_str() == "done")
    })
    .await;
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["x"]));
    let status = case.status(&run_id);
    assert_eq!(status["state"], "stopped");
    let node = node_status(&status, "x");
    let entries: Vec<(Value, Value)> = node["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (entry["index"].clone(), entry["status"].clone()))
        .collect();
    assert_eq!(
        entries,
        [(json!(0), json!("cancelled")), (json!(1), json!("done"))]
    );
    let instances: Vec<(Value, Value)> = node["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|instance| (instance["index"].clone(), instance["status"].clone()))
        .collect();
    assert_eq!(
        instances,
        [(json!(0), json!("cancelled")), (json!(1), json!("done"))]
    );
    assert_eq!(case.host.deleted_targets(), strings(&["child-3"]));
}

#[tokio::test]
async fn concurrent_stop_during_fail_fast_deletes_each_child_once() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    let delete_gate = case.host.gate("rlm.delete_subagent", 1);
    let delete_entered = case.host.gate_entered("rlm.delete_subagent", 1);
    case.store_factory(
        json!({ "run": { "failure_policy": "fail_fast" }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker" },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    delete_entered.wait().await;
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["state"], "stopped");
    delete_gate.set();
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "stopped");
    case.executor.join(&Case::run_id(&result)).await;
    let mut deleted = case.host.deleted_targets();
    deleted.sort();
    assert_eq!(deleted, strings(&["child-2", "child-3"]));
    assert_eq!(case.all_events_of(&result, "cancel_failed").len(), 0);
}

#[tokio::test]
async fn failed_delete_records_cancel_failed_and_cancelled() {
    let case = Case::new();
    case.host.with(|state| state.delete_fails = true);
    case.host.outcome("a", running());
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    case.wait_until(|| case.host.collects() >= 1).await;
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["a"]));
    let status = case.status(&Case::run_id(&result));
    assert_eq!(status["state"], "stopped");
    assert_eq!(
        node_status(&status, "a")["instances"][0]["status"],
        "cancelled"
    );
    let kinds: Vec<(Value, Value)> = status["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            (
                event["kind"].clone(),
                event.get("detail").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    let failed_at = kinds
        .iter()
        .position(|row| *row == (json!("cancel_failed"), Value::Null));
    let cancelled_at = kinds.iter().position(|row| {
        *row == (
            json!("cancelled"),
            json!("slot released despite the failed delete"),
        )
    });
    assert!(failed_at.is_some() && cancelled_at.is_some(), "{kinds:?}");
    assert!(failed_at < cancelled_at);
}

#[tokio::test]
async fn rate_limit_at_admission_defers_to_backoff() {
    let case = Case::new();
    case.host
        .with(|state| state.rate_limit_first.insert("a".into(), 2));
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!([]));
    assert_eq!(case.sleeps(), Vec::<f64>::new());
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(case.host.spawn_calls("a").len(), 3);
    assert_eq!(node_status(&status, "a")["attempts"], 3);
    assert_eq!(case.sleeps(), vec![1.0]);
}

#[tokio::test]
async fn rate_limit_backoff_exhaustion_fails_node() {
    let case = Case::new();
    case.host
        .with(|state| state.rate_limit_forever.insert("b".into()));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(BACKOFF_MAX_ATTEMPTS, 5);
    assert_eq!(case.host.spawn_calls("b").len(), 5);
    let b = node_status(&status, "b");
    assert_eq!(b["status"], "error");
    assert!(b["error"]
        .as_str()
        .unwrap()
        .contains("spawn admission failed"));
    assert_eq!(status["state"], "failed");
    let sleeps = case.sleeps();
    assert!(!sleeps.is_empty());
    assert!(sleeps
        .iter()
        .all(|delay| *delay <= POLL_TIMEOUT_MS as f64 / 1000.0));
    assert!((sleeps.iter().sum::<f64>() - (1.0 + 2.0 + 4.0 + 8.0)).abs() < 1e-9);
}

#[tokio::test]
async fn backoff_keeps_collecting_running_children() {
    let case = Case::new();
    case.host.outcome("a", running());
    case.host
        .with(|state| state.rate_limit_first.insert("b".into(), 3));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a"]));
    case.wait_until(|| !case.all_events_of(&result, "spawn_backoff").is_empty())
        .await;
    let collects_at_backoff = case.host.collects();
    case.host.outcome("a", done("late-but-collected"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "a")["status"], "done");
    assert_eq!(node_status(&status, "b")["status"], "done");
    assert!(case.host.collects() > collects_at_backoff);
    let settled: Vec<Value> = case
        .all_events_of(&result, "settled")
        .into_iter()
        .filter(|event| event["node"] == "a" && event["status"] == "done")
        .collect();
    assert_eq!(settled.len(), 1);
}

#[tokio::test]
async fn scale_chain_100_completes() {
    let case = Case::new();
    let mut nodes = vec![json!({ "id": "n0", "subagent": { "prompt": "step" } })];
    for index in 1..100 {
        nodes.push(json!({ "id": format!("n{index}"), "subagent": { "prompt": "step" }, "depends_on": [format!("n{}", index - 1)] }));
    }
    case.store_factory(
        json!({ "run": { "max_parallel": 8 }, "nodes": nodes }),
        "sw",
    );
    let started = std::time::Instant::now();
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(status["nodes"].as_array().unwrap().len(), 100);
    assert!(status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node["status"] == "done"));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn scale_fan_1000_completes() {
    let case = Case::new();
    let mut nodes = vec![json!({ "id": "n0", "subagent": { "prompt": "step" } })];
    for index in 1..1000 {
        nodes.push(json!({ "id": format!("n{index}"), "subagent": { "prompt": "step" }, "depends_on": ["n0"] }));
    }
    case.store_factory(
        json!({ "run": { "max_parallel": 64 }, "nodes": nodes }),
        "sw",
    );
    let started = std::time::Instant::now();
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(status["nodes"].as_array().unwrap().len(), 1000);
    assert!(status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node["status"] == "done"));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn status_marks_events_delivered_and_unknown_run_raises() {
    let case = Case::new();
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    case.wait_until(|| case.run_state(&result) != "running")
        .await;
    let stage = |kind: &str| -> Value {
        case.run_events(&result)
            .into_iter()
            .rev()
            .find(|event| event["kind"] == kind)
            .map(|event| event["stage"].clone())
            .unwrap()
    };
    assert_eq!(stage("answer_captured"), "arrived");
    assert_eq!(stage("milestone"), "shown");
    let status = case.status(&Case::run_id(&result));
    assert_eq!(stage("milestone"), "shown");
    assert_eq!(stage("answer_captured"), "delivered");
    assert_eq!(
        status["events"].as_array().unwrap().last().unwrap()["stage"],
        "shown"
    );
    assert_eq!(EVENT_WINDOW, 200);
    assert!(status["events"].as_array().unwrap().len() <= EVENT_WINDOW);
    assert_eq!(case.host.notice_kinds(), strings(&["finished"]));
    assert!(case
        .executor
        .status("no-such-run")
        .unwrap_err()
        .0
        .contains("unknown factory run"));
    assert!(case
        .executor
        .stop("no-such-run")
        .await
        .unwrap_err()
        .0
        .contains("unknown factory run"));
    assert!(case
        .executor
        .resume("no-such-run")
        .await
        .unwrap_err()
        .0
        .contains("unknown factory run"));
}

#[tokio::test]
async fn milestone_notices_carry_validated_payloads() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    store_escalating_chain(&case);
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "failed");
    assert_eq!(case.host.notice_kinds(), strings(&["paused", "failed"]));
    let notices = case.host.with(|state| state.notices.clone());
    let (paused_notice, failed_notice) = (&notices[0], &notices[1]);
    assert_eq!(paused_notice["run_id"], result["run_id"]);
    let detail = paused_notice["detail"].as_str().unwrap();
    assert!(detail.contains("state a failed") && detail.contains("resume"));
    assert_eq!(paused_notice["node"], "a");
    assert_eq!(failed_notice["kind"], "failed");
    assert_eq!(failed_notice["run_id"], result["run_id"]);
    assert!(failed_notice.get("node").is_none());
    let keys = |notice: &Value| {
        let mut keys: Vec<String> = notice.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(paused_notice),
        strings(&["detail", "kind", "node", "run_id"])
    );
    assert_eq!(keys(failed_notice), strings(&["detail", "kind", "run_id"]));
    assert_eq!(case.host.calls_of("factory.progress").len(), 2);
}

#[tokio::test]
async fn dead_bridge_keeps_the_milestone_in_the_ledger() {
    let case = Case::new();
    case.host.with(|state| state.dead_notices = true);
    case.host.outcome("a", failed("boom"));
    store_escalating_chain(&case);
    let result = case.start().await;
    case.wait_until(|| case.run_state(&result) == "paused")
        .await;
    case.wait_until(|| !case.all_events_of(&result, "milestone").is_empty())
        .await;
    let raw = case.all_events_of(&result, "milestone");
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0]["stage"], "recorded");
    assert!(raw[0]["detail"]
        .as_str()
        .unwrap()
        .contains("state a failed"));
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    let milestones = events_of(&paused, "milestone");
    assert_eq!(milestones.len(), 1);
    assert_eq!(milestones[0]["milestone"], "paused");
    assert_eq!(milestones[0]["stage"], "delivered");
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "failed");
    assert_eq!(node_status(&finished, "b")["status"], "done");
}
