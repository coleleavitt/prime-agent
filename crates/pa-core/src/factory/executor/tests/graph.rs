//! The fused graph snapshot, the bounded watch, the activity lane, and the
//! lane's frame cap (ports of `FactoryGraphWatchTest`,
//! `FactoryFrameCapTest`, and the opt-in gate's lane test).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use super::fake::{done, failed, node_status, running, Case, SleepMode};
use crate::factory::executor::snapshot::cap_factory_frame;
use crate::factory::executor::{GRAPH_EVENTS_TAIL, GRAPH_RUNS_WINDOW, LAST_FIRED_WINDOW};
use crate::factory::lane::{activity, FactoryLaneContext, StoredSpec, FACTORY_DISABLED_MESSAGE};
use crate::factory::pyvalue::PyValue;
use crate::factory::spec::RUN_MAX_CHILDREN_DEFAULT;

fn valid_machine() -> Value {
    json!({
        "run": { "budget_ms": 600_000, "failure_policy": "continue", "max_parallel": 4, "max_transitions": 40 },
        "states": [
            { "id": "collect", "entry": true, "subagent": "researcher", "outputs": [{ "name": "findings", "type": "text" }] },
            { "id": "reviewing", "subagent": { "prompt": "Review the draft." },
              "inputs": [{ "name": "draft", "type": "text", "from": "collect.findings" }],
              "outputs": [{ "name": "verdict", "type": "json" }], "max_entries": 4, "retries": 1 },
            { "id": "fixing", "subagent": { "prompt": "Fix the findings." }, "max_entries": 3 }
        ],
        "transitions": [
            { "from": "collect", "to": "reviewing" },
            { "from": "reviewing", "to": "fixing", "when": { "output": "verdict", "path": "approved", "op": "eq", "value": false } },
            { "from": "reviewing", "to": "reviewing", "when": { "output": "verdict", "op": "exists" } },
            { "from": "fixing", "to": "reviewing" }
        ]
    })
}

fn valid_dag() -> Value {
    json!({
        "run": { "budget_ms": 600_000, "failure_policy": "continue", "max_parallel": 4 },
        "nodes": [
            { "id": "collect", "subagent": "researcher", "outputs": [{ "name": "findings", "type": "text" }] },
            { "id": "fan-out", "subagent": { "prompt": "Expand each item.", "name": "expander", "model": "m1", "thinking": "low" },
              "depends_on": ["collect"], "inputs": [{ "name": "items", "type": "text", "from": "collect.findings" }] },
            { "id": "review", "subagent": { "prompt": "Review the fan-out." }, "depends_on": ["collect", "fan-out"],
              "inputs": [{ "name": "draft", "type": "text", "from": "collect.findings" }],
              "retries": 2, "budget_ms": 100_000, "failure_policy": "fail_fast" }
        ]
    })
}

/// The graph battery's setUp: a researcher subagent and the review-loop
/// machine stored as `sw`, plus the lane over the case's stores.
fn graph_case() -> (Arc<Case>, TestLane) {
    let case = Arc::new(Case::new());
    case.create_subagent(
        "researcher",
        "Researcher",
        "Collect the findings.",
        json!({}),
    );
    case.store_machine(valid_machine(), "sw");
    let lane = TestLane {
        case: Arc::clone(&case),
        enabled: Arc::new(AtomicBool::new(true)),
    };
    (case, lane)
}

struct TestLane {
    case: Arc<Case>,
    enabled: Arc<AtomicBool>,
}

impl FactoryLaneContext for TestLane {
    fn factory_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn stored_spec(&self, spec_id: &str) -> Option<StoredSpec> {
        let spec = self.case.specs.lock().unwrap().get(spec_id).cloned()?;
        Some(StoredSpec {
            id: spec_id.to_string(),
            subagents: self.case.subagent_table(&spec),
            spec: PyValue::from_json(&spec),
        })
    }
}

impl TestLane {
    async fn call(&self, request: Value) -> Result<Value, String> {
        activity(&self.case.executor, self, &request)
            .await
            .map_err(|refusal| refusal.0)
    }

    /// `rlm.factory.graph(ref)` in the kernel: a live run, else the
    /// stored spec's static graph (the client resolves the entry).
    fn kernel_graph(&self, reference: &str) -> Result<Value, String> {
        if let Some(snapshot) = self.case.executor.graph(Some(reference), false) {
            return Ok(snapshot);
        }
        let stored = self
            .stored_spec(reference)
            .ok_or_else(|| format!("unknown factory run or spec '{reference}'"))?;
        self.case
            .executor
            .spec_graph(reference, &stored.id, &stored.spec)
            .map_err(|refusal| refusal.0)
    }
}

#[tokio::test]
async fn graph_fuses_structure_and_live_state() {
    let (case, _lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.clock.advance(12.0);
    let graph = case.executor.graph(Some(&run_id), false).unwrap();
    assert_eq!(graph["run_id"], run_id.as_str());
    assert_eq!(graph["spec_id"], "sw");
    assert_eq!(graph["state"], "running");
    assert_eq!(graph["elapsed_ms"], 12_000);
    assert_eq!(
        graph["budget"],
        json!({ "limit_ms": 600_000, "consumed_ms": 12_000 })
    );
    let machine = &graph["machine"];
    assert_eq!(machine["order"], json!(["collect", "reviewing", "fixing"]));
    let collect = machine["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["id"] == "collect")
        .unwrap();
    assert_eq!(collect["entry"], true);
    assert_eq!(collect["lifecycle"], "task");
    assert_eq!(machine["run"]["max_parallel"], 4);
    assert_eq!(machine["run"]["failure_policy"], "continue");
    assert_eq!(machine["run"]["budget_ms"], 600_000);
    assert_eq!(machine["run"]["max_transitions"], 40);
    assert_eq!(
        machine["run"]["max_children"],
        RUN_MAX_CHILDREN_DEFAULT as u64
    );
    let guarded = machine["transitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["to"] == "fixing")
        .unwrap();
    assert_eq!(guarded["from"], "reviewing");
    assert_eq!(guarded["when"]["output"], "verdict");
    let ids: Vec<Value> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].clone())
        .collect();
    assert_eq!(ids, [json!("collect"), json!("reviewing"), json!("fixing")]);
    assert!(graph["active_nodes"]
        .as_array()
        .unwrap()
        .contains(&json!("collect")));
    assert_eq!(
        graph["usage"]["spawns"],
        result["started"].as_array().unwrap().len()
    );
    assert!(!graph["events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn graph_machine_carries_the_declared_run_limits() {
    let (case, _lane) = graph_case();
    case.store_machine(
        json!({
            "run": { "max_children": 2 },
            "states": [{ "id": "a", "entry": true, "subagent": "worker" }, { "id": "b", "subagent": "worker" }],
            "transitions": [{ "from": "a", "to": "b" }]
        }),
        "limits",
    );
    let result = case.start_spec("limits").await;
    let graph = case
        .executor
        .graph(Some(&Case::run_id(&result)), false)
        .unwrap();
    assert_eq!(graph["machine"]["run"]["max_children"], 2);
    assert_eq!(graph["machine"]["run"]["max_parallel"], 8);
}

#[tokio::test]
async fn graph_nodes_carry_per_stage_agent_counts() {
    let (case, lane) = graph_case();
    case.host.outcome("collect", running());
    let result = case.start().await;
    let graph = case
        .executor
        .graph(Some(&Case::run_id(&result)), false)
        .unwrap();
    assert_eq!(node_status(&graph, "collect")["running"], 1);
    assert_eq!(node_status(&graph, "collect")["queued"], 0);
    assert_eq!(node_status(&graph, "reviewing")["running"], 0);
    assert_eq!(node_status(&graph, "reviewing")["queued"], 0);
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    assert_eq!(node_status(&listed["runs"][0], "collect")["running"], 1);
    assert_eq!(node_status(&listed["runs"][0], "collect")["queued"], 0);
    case.store_machine(
        json!({
            "run": { "max_parallel": 1 },
            "states": [{ "id": "a", "entry": true, "subagent": "worker" }, { "id": "b", "entry": true, "subagent": "worker" }]
        }),
        "sat",
    );
    case.host.outcome("a", running());
    case.host.outcome("b", running());
    let sat = case.start_spec("sat").await;
    let sat_id = Case::run_id(&sat);
    let graph = case.executor.graph(Some(&sat_id), false).unwrap();
    assert_eq!(node_status(&graph, "a")["running"], 1);
    assert_eq!(node_status(&graph, "a")["queued"], 0);
    assert_eq!(node_status(&graph, "b")["running"], 0);
    assert_eq!(node_status(&graph, "b")["queued"], 1);
    case.executor.stop(&sat_id).await.unwrap();
    let graph = case.executor.graph(Some(&sat_id), false).unwrap();
    for node in graph["nodes"].as_array().unwrap() {
        assert_eq!(
            (node["running"].clone(), node["queued"].clone()),
            (json!(0), json!(0))
        );
    }
}

#[tokio::test]
async fn graph_transition_from_lists_are_snapshot_owned() {
    let (case, _lane) = graph_case();
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker" },
                { "id": "b", "entry": true, "subagent": "worker" },
                { "id": "c", "subagent": "worker" }
            ],
            "transitions": [{ "from": ["a", "b"], "to": "c" }]
        }),
        "join",
    );
    let result = case.start_spec("join").await;
    let run_id = Case::run_id(&result);
    let mut graph = case.executor.graph(Some(&run_id), false).unwrap();
    let join = &mut graph["machine"]["transitions"][0];
    assert_eq!(join["from"], json!(["a", "b"]));
    join["from"].as_array_mut().unwrap().push(json!("ghost"));
    let run = case.executor.snapshot_run(&run_id).unwrap();
    assert_eq!(run.machine["transitions"][0]["from"], json!(["a", "b"]));
    let again = case.executor.graph(Some(&run_id), false).unwrap();
    assert_eq!(
        again["machine"]["transitions"][0]["from"],
        json!(["a", "b"])
    );
}

#[tokio::test]
async fn graph_is_status_data_plus_the_static_graph() {
    let (case, _lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    // graph is a pure read: before any status() read the ledger stays
    // "recorded" through repeated graph calls; status() marks it.
    let before = case.executor.graph(Some(&run_id), false).unwrap();
    assert_eq!(before["events"][0]["stage"], "recorded");
    let again = case.executor.graph(Some(&run_id), false).unwrap();
    assert_eq!(again["events"][0]["stage"], "recorded");
    let status = case.settle(&result).await;
    assert_eq!(status["events"][0]["stage"], "delivered");
    let graph = case.executor.graph(Some(&run_id), false).unwrap();
    assert_eq!(graph["nodes"], status["nodes"]);
    assert_eq!(graph["usage"], status["usage"]);
    assert_eq!(graph["state"], status["state"]);
}

#[tokio::test]
async fn graph_lists_every_live_run_and_marks_active_nodes() {
    let (case, _lane) = graph_case();
    let first = case.start().await;
    case.settle(&first).await;
    let second = case.start().await;
    let listing = case.executor.graph(None, false).unwrap();
    let ids: Vec<Value> = listing["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["run_id"].clone())
        .collect();
    assert_eq!(ids, [first["run_id"].clone(), second["run_id"].clone()]);
    assert_eq!(listing["runs"][0]["active_nodes"], json!([]));
    assert!(listing["runs"][1]["active_nodes"]
        .as_array()
        .unwrap()
        .contains(&json!("collect")));
}

#[tokio::test]
async fn graph_keeps_a_failed_foreach_entrys_stage_active_while_siblings_run() {
    let (case, _lane) = graph_case();
    case.host.outcome(
        "src",
        done("{\"items\": [\"a\", \"b\", \"c\", \"d\", \"e\"]}"),
    );
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 2 },
            "states": [
                { "id": "src", "entry": true, "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": "worker", "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 5 } }
            ],
            "transitions": [{ "from": "src", "to": "fan" }]
        }),
        "fan",
    );
    let result = case.start_spec("fan").await;
    let run_id = Case::run_id(&result);
    case.host.child_outcome("child-2", failed("boom"));
    case.host.child_outcome("child-3", running());
    case.wait_until(|| {
        let run = case.executor.snapshot_run(&run_id).unwrap();
        run.states[1]
            .entries
            .iter()
            .any(|entry| entry.status.as_str() == "error")
    })
    .await;
    let graph = case.executor.graph(Some(&run_id), false).unwrap();
    let fan = node_status(&graph, "fan");
    assert_eq!(
        fan["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["status"].clone())
            .collect::<Vec<_>>(),
        [json!("error")]
    );
    assert_eq!(fan["running"], 1);
    assert_eq!(fan["queued"], 0);
    assert_eq!(graph["state"], "running");
    assert_eq!(graph["usage"]["running"], 1);
    assert!(graph["active_nodes"]
        .as_array()
        .unwrap()
        .contains(&json!("fan")));
}

#[tokio::test]
async fn graph_of_a_stored_spec_returns_the_static_structure() {
    let (case, lane) = graph_case();
    let graph = lane.kernel_graph("sw").unwrap();
    assert_eq!(graph["run_id"], Value::Null);
    assert_eq!(graph["spec_id"], "sw");
    assert_eq!(graph["state"], Value::Null);
    assert_eq!(
        graph["machine"]["order"],
        json!(["collect", "reviewing", "fixing"])
    );
    assert_eq!(graph["nodes"], json!([]));
    assert_eq!(graph["active_nodes"], json!([]));
    assert_eq!(
        graph["budget"],
        json!({ "limit_ms": 600_000, "consumed_ms": 0 })
    );
    case.store_factory(valid_dag(), "dag");
    let dag_graph = lane.kernel_graph("dag").unwrap();
    assert_eq!(
        dag_graph["machine"]["order"],
        json!(["collect", "fan-out", "review"])
    );
    assert_eq!(
        lane.kernel_graph("missing"),
        Err("unknown factory run or spec 'missing'".to_string())
    );
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "broken",
    );
    case.corrupt_stored_spec("broken", json!({ "nodes": [] }));
    let error = lane.kernel_graph("broken").unwrap_err();
    assert!(error.contains("does not validate"), "{error}");
}

#[tokio::test]
async fn compact_snapshot_sheds_answers_and_carries_the_short_tail() {
    let (case, _lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.settle(&result).await;
    assert!(case.run_events(&result).len() >= 3);
    let full = case.executor.graph(Some(&run_id), false).unwrap();
    let compact = case.executor.graph(Some(&run_id), true).unwrap();
    let kinds = |graph: &Value| -> Vec<String> {
        graph["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(compact["events"].as_array().unwrap().len() <= GRAPH_EVENTS_TAIL);
    assert!(!kinds(&compact).contains(&"answer_captured".to_string()));
    assert!(kinds(&full).contains(&"answer_captured".to_string()));
    assert!(!compact["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node.get("answer_preview").is_some()));
    assert!(full["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node.get("answer_preview").is_some()));
}

#[tokio::test]
async fn last_fired_marks_the_recently_fired_edges() {
    let (case, _lane) = graph_case();
    let result = case.start().await;
    case.settle(&result).await;
    let graph = case
        .executor
        .graph(Some(&Case::run_id(&result)), false)
        .unwrap();
    let fired: Vec<(Value, Value)> = graph["last_fired"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| (edge["from"].clone(), edge["to"].clone()))
        .collect();
    assert!(fired.contains(&(json!("collect"), json!("reviewing"))));
    assert!(graph["last_fired"].as_array().unwrap().len() <= LAST_FIRED_WINDOW);
}

#[tokio::test]
async fn watch_returns_the_snapshot_when_nothing_changes() {
    let (case, _lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.settle(&result).await;
    let mut watched = case
        .executor
        .watch(&run_id, Some(0.0), false)
        .await
        .unwrap();
    assert_eq!(watched["changed"], false);
    assert_eq!(watched["run_id"], run_id.as_str());
    let graph = case.executor.graph(Some(&run_id), false).unwrap();
    watched.as_object_mut().unwrap().remove("changed");
    assert_eq!(watched, graph);
}

/// A run whose single child never settles.
async fn start_held_run(case: &Case) -> Value {
    case.create_subagent("sleeper", "Sleeper", "Never settles.", json!({}));
    case.store_machine(
        json!({ "states": [{ "id": "heldstate", "entry": true, "subagent": "sleeper" }] }),
        "held",
    );
    case.host.outcome("heldstate", running());
    case.start_spec("held").await
}

#[tokio::test]
async fn watch_blocks_until_the_run_changes() {
    let (case, _lane) = graph_case();
    case.set_sleep(SleepMode::Turn);
    let result = start_held_run(&case).await;
    let run_id = Case::run_id(&result);
    let stopper = {
        let executor = Arc::clone(&case.executor);
        let run_id = run_id.clone();
        tokio::spawn(async move {
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            executor.stop(&run_id).await.unwrap();
        })
    };
    let watched = case
        .executor
        .watch(&run_id, Some(30.0), false)
        .await
        .unwrap();
    stopper.await.unwrap();
    assert_eq!(watched["changed"], true);
    assert_eq!(watched["state"], "stopped");
}

#[tokio::test]
async fn watch_times_out_without_a_change() {
    let (case, _lane) = graph_case();
    let result = start_held_run(&case).await;
    let run_id = Case::run_id(&result);
    let watched = case
        .executor
        .watch(&run_id, Some(0.05), false)
        .await
        .unwrap();
    assert_eq!(watched["changed"], false);
    assert_eq!(watched["run_id"], run_id.as_str());
    assert_eq!(watched["state"], "running");
}

#[tokio::test]
async fn watch_rejects_unknown_runs_and_bad_timeouts() {
    let (case, _lane) = graph_case();
    assert_eq!(
        case.executor
            .watch("nope", Some(1.0), false)
            .await
            .unwrap_err()
            .0,
        "unknown factory run 'nope'"
    );
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    // -1, a non-number ("soon": the client sends None), and NaN.
    for timeout in [Some(-1.0), None, Some(f64::NAN)] {
        assert_eq!(
            case.executor
                .watch(&run_id, timeout, false)
                .await
                .unwrap_err()
                .0,
            "timeout must be a non-negative number of seconds"
        );
    }
}

#[tokio::test]
async fn activity_routes_every_action() {
    let (case, lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    let ids: Vec<Value> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["runId"].clone())
        .collect();
    assert_eq!(ids, [json!(run_id)]);
    let one = lane
        .call(json!({ "action": "graph", "runId": run_id }))
        .await
        .unwrap();
    assert_eq!(one["runId"], run_id.as_str());
    let spec_graph = lane
        .call(json!({ "action": "graph", "specId": "sw" }))
        .await
        .unwrap();
    assert_eq!(spec_graph["specId"], "sw");
    let status = lane
        .call(json!({ "action": "status", "runId": run_id }))
        .await
        .unwrap();
    assert_eq!(status["runId"], run_id.as_str());
    case.settle(&result).await;
    let second = lane
        .call(json!({ "action": "run", "specId": "sw" }))
        .await
        .unwrap();
    let second_id = second["runId"].as_str().unwrap().to_string();
    let stopped = lane
        .call(json!({ "action": "stop", "runId": second_id }))
        .await
        .unwrap();
    assert_eq!(stopped["state"], "stopped");
    let refusal = lane
        .call(json!({ "action": "resume", "runId": second_id }))
        .await
        .unwrap_err();
    assert!(refusal.contains("not paused"), "{refusal}");
    let watched = lane
        .call(json!({ "action": "watch", "runId": second_id, "timeoutMs": 5 }))
        .await
        .unwrap();
    assert!(watched.get("changed").is_some());
    assert_eq!(watched["runId"], second_id.as_str());
}

#[tokio::test]
async fn activity_reply_carries_the_wire_keys() {
    let (case, lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.clock.advance(3.0);
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    let row = &listed["runs"][0];
    assert_eq!(row["runId"], run_id.as_str());
    assert_eq!(row["specId"], "sw");
    assert_eq!(row["state"], "running");
    assert_eq!(row["elapsedMs"], 3_000);
    assert_eq!(row["budget"]["limitMs"], 600_000);
    for key in ["toolUses", "maxParallel", "transitionsFired"] {
        assert!(row["usage"].get(key).is_some(), "{key}");
    }
    let node = &row["nodes"][0];
    assert!(node.get("entriesUsed").is_some() && node.get("maxEntries").is_some());
    assert_eq!(row["machine"]["run"]["maxParallel"], 4);
    assert!(row.get("run_id").is_none() && row.get("elapsed_ms").is_none());
    assert!(row["usage"].get("tool_uses").is_none());
    let watched = lane
        .call(json!({ "action": "watch", "runId": run_id, "timeoutMs": 0 }))
        .await
        .unwrap();
    assert!(watched.get("changed").is_some());
    assert_eq!(watched["runId"], run_id.as_str());
    assert!(watched.get("run_id").is_none());
    let status = lane
        .call(json!({ "action": "status", "runId": run_id }))
        .await
        .unwrap();
    assert_eq!(status["runId"], run_id.as_str());
    assert!(status.get("run_id").is_none());
    let graph = case.executor.graph(Some(&run_id), false).unwrap();
    assert_eq!(graph["run_id"], run_id.as_str());
    assert_eq!(graph["elapsed_ms"], 3_000);
    assert!(graph["usage"].get("tool_uses").is_some());
    assert!(graph.get("runId").is_none());
}

#[tokio::test]
async fn run_activity_caps_the_error_reply() {
    let (case, lane) = graph_case();
    let transitions: Vec<Value> = (0..6_000)
        .map(|i| json!({ "from": "a", "to": format!("missing{i}") }))
        .collect();
    case.corrupt_stored_spec(
        "big-spec",
        json!({
            "run": { "failure_policy": "continue" },
            "states": [{ "id": "a", "entry": true, "subagent": "worker" }],
            "transitions": transitions
        }),
    );
    let reply = crate::factory::lane::capped_reply(
        activity(
            &case.executor,
            &lane,
            &json!({ "action": "run", "specId": "big-spec" }),
        )
        .await,
    );
    assert_eq!(
        reply,
        Err("factory activity reply exceeds the wire cap".to_string())
    );
}

#[tokio::test]
async fn activity_reply_carries_guards_verbatim() {
    let (case, lane) = graph_case();
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker", "outputs": [{ "name": "verdict", "type": "json" }] },
                { "id": "b", "subagent": "worker" }
            ],
            "transitions": [{ "from": "a", "to": "b", "when": { "output": "verdict", "op": "contains", "value": [{ "snake_key": 1 }] } }]
        }),
        "guarded",
    );
    let result = case.start_spec("guarded").await;
    let reply = lane
        .call(json!({ "action": "graph", "runId": result["run_id"] }))
        .await
        .unwrap();
    assert_eq!(
        reply["machine"]["transitions"][0]["when"],
        json!({ "output": "verdict", "op": "contains", "value": [{ "snake_key": 1 }] })
    );
}

fn store_solo(case: &Case) {
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [{ "id": "a", "entry": true, "subagent": "worker" }],
            "transitions": []
        }),
        "solo",
    );
}

#[tokio::test]
async fn unscoped_graph_bounds_the_terminal_history() {
    let (case, lane) = graph_case();
    store_solo(&case);
    let mut started = Vec::new();
    for _ in 0..25 {
        started.push(case.start_spec("solo").await);
    }
    for result in &started {
        case.settle(result).await;
    }
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    let ids: Vec<String> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["runId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), GRAPH_RUNS_WINDOW);
    let newest: Vec<String> = started[started.len() - GRAPH_RUNS_WINDOW..]
        .iter()
        .map(Case::run_id)
        .collect();
    assert_eq!(ids, newest);
    case.host.outcome("a", running());
    let live = case.start_spec("solo").await;
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    let ids: Vec<String> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["runId"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&Case::run_id(&live)));
    assert_eq!(ids.len(), GRAPH_RUNS_WINDOW + 1);
}

#[tokio::test]
async fn a_done_run_with_children_in_flight_reports_live() {
    let (case, lane) = graph_case();
    case.store_machine(
        json!({ "states": [{ "id": "watcher", "entry": true, "subagent": "worker", "lifecycle": "resident" }], "transitions": [] }),
        "resident",
    );
    case.host.outcome("watcher", running());
    let resident = case.start_spec("resident").await;
    let resident_id = Case::run_id(&resident);
    let status = case.settle(&resident).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "watcher")["running"], 1);
    store_solo(&case);
    for _ in 0..GRAPH_RUNS_WINDOW {
        let result = case.start_spec("solo").await;
        case.settle(&result).await;
    }
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    let ids: Vec<String> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["runId"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&resident_id));
    assert_eq!(ids.len(), GRAPH_RUNS_WINDOW + 1);
    let row = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["runId"] == resident_id.as_str())
        .unwrap();
    assert_eq!(row["state"], "done");
    assert_eq!(node_status(row, "watcher")["running"], 1);
    case.executor.stop(&resident_id).await.unwrap();
    let listed = lane.call(json!({ "action": "graph" })).await.unwrap();
    let ids: Vec<String> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["runId"].as_str().unwrap().to_string())
        .collect();
    assert!(!ids.contains(&resident_id));
    assert_eq!(ids.len(), GRAPH_RUNS_WINDOW);
}

#[tokio::test]
async fn activity_validates_its_request_shape() {
    let (_case, lane) = graph_case();
    for bad in [
        json!({ "action": "bogus" }),
        json!({ "action": "status" }),
        json!({ "action": "watch", "runId": 5 }),
        json!({ "action": "watch" }),
        json!({ "action": "graph", "specId": 5 }),
        json!({ "action": "watch", "runId": "x", "timeoutMs": -1 }),
        json!({ "action": "watch", "runId": "x", "timeoutMs": "soon" }),
        json!({ "action": "watch", "runId": "x", "timeoutMs": 1_000_000_000 }),
        json!({ "action": "run" }),
    ] {
        assert!(lane.call(bad.clone()).await.is_err(), "{bad}");
    }
}

// Port of `test_the_activity_lane_refuses_while_disabled`.
#[tokio::test]
async fn the_activity_lane_refuses_while_disabled() {
    let (case, lane) = graph_case();
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    lane.enabled.store(false, Ordering::SeqCst);
    for action in ["graph", "watch", "status", "run", "stop", "resume"] {
        let mut request = json!({ "action": action });
        if action == "run" {
            request["specId"] = json!("sw");
        } else {
            request["runId"] = json!(run_id);
        }
        assert_eq!(
            lane.call(request).await,
            Err(FACTORY_DISABLED_MESSAGE.to_string()),
            "{action}"
        );
    }
}

// FactoryFrameCapTest. `test_a_non_finite_frame_fails_loudly` has no port:
// a `serde_json::Value` cannot carry NaN or Infinity, so the belt it pins
// is ruled out by the type.

#[test]
fn an_oversized_error_frame_fails_loudly() {
    let huge_reason = format!("unknown factory run '{}'", "x".repeat(300_000));
    let mut frame = json!({ "event": "done", "id": "r", "status": "error", "reason": huge_reason });
    cap_factory_frame(&mut frame);
    assert!(frame["reason"].as_str().unwrap().len() < 10_000);
    assert!(frame["reason"].as_str().unwrap().contains("wire cap"));
    let mut small = json!({ "event": "done", "id": "r", "status": "error", "reason": "boom" });
    cap_factory_frame(&mut small);
    assert_eq!(small["reason"], "boom");
}

#[test]
fn an_oversized_reply_is_trimmed_then_failed() {
    let events: Vec<Value> = (0..50).map(|i| json!({ "kind": "settled", "seq": i, "stage": "recorded", "big": "y".repeat(12_000) })).collect();
    let mut frame =
        json!({ "event": "done", "id": "r", "status": "ok", "result": { "events": events } });
    cap_factory_frame(&mut frame);
    assert_eq!(frame["status"], "ok");
    assert!(frame["result"]["events"].as_array().unwrap().len() < 50);
    assert_eq!(
        frame["result"]["events"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["seq"],
        49
    );
    let mut frame = json!({ "event": "done", "id": "r", "status": "ok", "result": { "events": [{ "big": "y".repeat(300_000) }] } });
    cap_factory_frame(&mut frame);
    assert_eq!(frame["status"], "error");
    assert!(frame["reason"].as_str().unwrap().contains("wire cap"));
    let states: Vec<Value> = (0..2000)
        .map(|_| json!({ "id": "x".repeat(200) }))
        .collect();
    let mut frame = json!({ "event": "done", "id": "r", "status": "ok", "result": { "machine": { "states": states } } });
    cap_factory_frame(&mut frame);
    assert_eq!(frame["status"], "error");
    let runs: Vec<Value> = (0..40)
        .map(|i| json!({ "run_id": format!("r{i}"), "events": vec![json!({ "big": "y".repeat(4000) }); 20], "machine": {} }))
        .collect();
    let mut frame =
        json!({ "event": "done", "id": "r", "status": "ok", "result": { "runs": runs } });
    cap_factory_frame(&mut frame);
    assert_eq!(frame["status"], "ok");
    let rows = frame["result"]["runs"].as_array().unwrap();
    assert_eq!(rows.len(), 40);
    assert_eq!(rows.last().unwrap()["run_id"], "r39");
    // Tails floor oldest row first, before any row drops. (The Python
    // battery's rows shared ONE events list through a shallow dict copy,
    // so every row shrank together and all ended at one event; with
    // independent rows the oldest rows floor first and the newest keep
    // more of their tail — the same per-row trim rule.)
    let tails: Vec<usize> = rows
        .iter()
        .map(|row| row["events"].as_array().unwrap().len())
        .collect();
    assert!(tails.iter().all(|tail| *tail >= 1), "{tails:?}");
    assert_eq!(tails[0], 1);
    assert!(tails.windows(2).all(|pair| pair[0] <= pair[1]), "{tails:?}");
}

#[test]
fn the_cap_drops_terminal_rows_before_live_ones() {
    let live = json!({ "runId": "r-live", "state": "running", "usage": { "running": 1 }, "events": [{ "kind": "settled" }], "machine": {} });
    let states: Vec<Value> = (0..8).map(|_| json!({ "id": "s".repeat(2000) })).collect();
    let mut runs = vec![live];
    for i in 1..=30 {
        runs.push(json!({ "runId": format!("t{i}"), "state": "done", "usage": { "running": 0 }, "events": [{ "kind": "settled" }], "machine": { "states": states } }));
    }
    let mut frame =
        json!({ "event": "done", "id": "r", "status": "ok", "result": { "runs": runs } });
    cap_factory_frame(&mut frame);
    assert_eq!(frame["status"], "ok");
    let rows = frame["result"]["runs"].as_array().unwrap();
    assert_eq!(rows[0]["runId"], "r-live");
    assert_eq!(rows.last().unwrap()["runId"], "t30");
    assert!(rows.len() < 31);
}

#[test]
fn a_live_row_never_silently_drops() {
    let live_big = json!({ "runId": "r-big", "state": "running", "usage": { "running": 2 }, "events": [{ "big": "y".repeat(300_000) }], "machine": {} });
    let live_small = json!({ "runId": "r-small", "state": "paused", "usage": { "running": 1 }, "events": [{ "kind": "settled" }], "machine": {} });
    let mut frame = json!({ "event": "done", "id": "r", "status": "ok", "result": { "runs": [live_big, live_small] } });
    cap_factory_frame(&mut frame);
    assert_eq!(frame["status"], "error");
    assert!(frame["reason"].as_str().unwrap().contains("wire cap"));
    assert!(frame.get("result").is_none());
}
