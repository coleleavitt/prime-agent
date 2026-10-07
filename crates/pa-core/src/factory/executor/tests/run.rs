//! Dry run, subagent resolution, input binding, fan-in, answer capture,
//! and foreach (ports of `FactoryExecutorTest`).

use serde_json::{json, Value};

use super::fake::{
    done, entry_statuses, failed, instance_statuses, node_status, running, strings, Case,
};
use crate::factory::executor::binding::ANSWER_CAPTURE_CAP;
use crate::factory::executor::FactoryRefusal;

#[tokio::test]
async fn run_rejects_invalid_dag_and_starts_nothing() {
    let case = Case::new();
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "empty",
    );
    case.corrupt_stored_spec("empty", json!({ "nodes": [] }));
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "cyclic",
    );
    case.corrupt_stored_spec(
        "cyclic",
        json!({ "nodes": [
            { "id": "a", "subagent": "worker", "depends_on": ["b"] },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
    );
    for spec_id in ["empty", "cyclic"] {
        let error = case.try_start(spec_id).await.expect_err("invalid spec");
        assert!(error.0.contains("factory"), "{error}");
    }
    assert_eq!(case.host.with(|state| state.calls.len()), 0);
}

#[tokio::test]
async fn run_lists_all_missing_subagent_references() {
    let case = Case::new();
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "ghost-a" },
            { "id": "b", "subagent": "ghost-b" }
        ] }),
        "sw",
    );
    let error = case.try_start("sw").await.expect_err("missing subagents");
    assert_eq!(
        error,
        FactoryRefusal(
            "state 'a' references unknown subagent 'ghost-a'; state 'b' references unknown subagent 'ghost-b'"
                .into()
        )
    );
    assert_eq!(case.host.with(|state| state.calls.len()), 0);
}

#[tokio::test]
async fn resolves_subagent_by_id_and_title_with_model_settings() {
    let case = Case::new();
    case.create_subagent(
        "worker-md",
        "The Worker",
        "Template by title.",
        json!({ "model": "pi/test-model", "thinking": "low" }),
    );
    case.store_factory(
        json!({ "run": { "max_parallel": 2 }, "nodes": [
            { "id": "x", "subagent": "The Worker" },
            { "id": "y", "subagent": "worker-md" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    for node in ["x", "y"] {
        let spawn = case.host.spawn_calls(node);
        assert_eq!(spawn.len(), 1, "{node}");
        assert_eq!(spawn[0]["prompt"], "Template by title.");
        assert_eq!(spawn[0]["kwargs"]["model"], "pi/test-model");
        assert_eq!(spawn[0]["kwargs"]["thinking"], "low");
    }
}

#[tokio::test]
async fn inline_subagent_name_labels_the_spawned_child() {
    let case = Case::new();
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": { "prompt": "Do the review.", "name": "reviewer" } }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(case.host.spawn_names(), strings(&["reviewer"]));
    let names: Vec<Value> = case
        .all_events_of(&result, "spawned")
        .iter()
        .map(|event| event["name"].clone())
        .collect();
    assert_eq!(names, [json!("reviewer")]);
}

#[tokio::test]
async fn inline_subagent_name_disambiguates_reentry_and_foreach() {
    let case = Case::new();
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [{ "id": "loop", "entry": true, "subagent": { "prompt": "Work.", "name": "worker" }, "max_entries": 2 }],
            "transitions": [{ "from": "loop", "to": "loop" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(case.host.spawn_names(), strings(&["worker", "worker-i1"]));

    case.host.outcome(
        "src",
        done("Here.\n```json\n{\"items\": [\"one\", \"two\"]}\n```"),
    );
    case.host.with(|state| {
        state.calls.clear();
        state.children.clear();
        state.counter = 0;
    });
    case.store_factory(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 8 },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                {
                    "id": "fan",
                    "subagent": { "prompt": "Expand item {items}.", "name": "expander" },
                    "depends_on": ["src"],
                    "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                    "foreach": { "over": "items", "max": 4 }
                }
            ]
        }),
        "fan",
    );
    let result = case.start_spec("fan").await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let expanders: Vec<String> = case
        .host
        .spawn_names()
        .into_iter()
        .filter(|name| name.starts_with("expander"))
        .collect();
    assert_eq!(expanders, strings(&["expander", "expander-i1"]));
}

#[tokio::test]
async fn run_starts_ready_nodes_and_reports_counts() {
    let case = Case::new();
    case.store_factory(
        json!({ "run": { "max_parallel": 2 }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] },
            { "id": "c", "subagent": "worker", "depends_on": ["b"] },
            { "id": "d", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert!(result["run_id"].is_string());
    assert_eq!(result["spec_id"], "sw");
    assert_eq!(result["nodes"], 4);
    assert_eq!(result["max_parallel"], 2);
    assert_eq!(result["started"], json!(["a", "d"]));
    assert_eq!(result["pending"], json!(["b", "c"]));
    assert_eq!(case.host.calls_of("rlm.run").len(), 2);
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert!(status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node["status"] == "done"));
    assert_eq!(case.host.notice_kinds(), strings(&["finished"]));
    assert_eq!(status["usage"]["spawns"], 4);
    assert_eq!(status["usage"]["settled"], 4);
}

#[tokio::test]
async fn propagation_binds_answer_into_prompt() {
    let case = Case::new();
    case.host.outcome("a", done("ANSWER-A"));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker", "outputs": [{ "name": "out", "type": "text" }] },
            { "id": "b", "subagent": { "prompt": "Summarize: {draft}" }, "depends_on": ["a"],
              "inputs": [{ "name": "draft", "type": "text", "from": "a.out" }] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a"]));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(
        case.host.spawn_prompts("b"),
        strings(&["Summarize: ANSWER-A"])
    );
    assert_eq!(node_status(&status, "b")["answer_preview"], "answer-b");
}

/// Two-node json binding: `a` answers `answer`, `b` renders `Process {data}.`.
async fn json_binding(answer: &str) -> (Case, Value) {
    let case = Case::new();
    case.host.outcome("a", done(answer));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker", "outputs": [{ "name": "result", "type": "json" }] },
            { "id": "b", "subagent": { "prompt": "Process {data}." }, "depends_on": ["a"],
              "inputs": [{ "name": "data", "type": "json", "from": "a.result" }] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    (case, status)
}

#[tokio::test]
async fn json_input_prefers_fenced_block() {
    let (case, status) =
        json_binding("Verdict text.\n```json\n{\"result\": {\"x\": 1}}\n```").await;
    assert_eq!(status["state"], "done");
    assert_eq!(
        case.host.spawn_prompts("b"),
        strings(&["Process {\"x\": 1}."])
    );
}

#[tokio::test]
async fn json_input_falls_back_to_whole_text() {
    let (case, status) = json_binding("{\"result\": 7}").await;
    assert_eq!(status["state"], "done");
    assert_eq!(case.host.spawn_prompts("b"), strings(&["Process 7."]));
}

#[tokio::test]
async fn bad_json_input_fails_node_without_spawning() {
    let case = Case::new();
    case.host.outcome("a", done("not json at all"));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker", "outputs": [{ "name": "result", "type": "json" }] },
            { "id": "b", "subagent": { "prompt": "Process {data}." }, "depends_on": ["a"],
              "inputs": [{ "name": "data", "type": "json", "from": "a.result" }] },
            { "id": "c", "subagent": "worker" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(case.host.spawn_calls("b"), Vec::<Value>::new());
    let b = node_status(&status, "b");
    assert_eq!(b["status"], "error");
    assert!(b["error"]
        .as_str()
        .unwrap()
        .contains("no JSON object containing output"));
    assert_eq!(node_status(&status, "c")["status"], "done");
    assert_eq!(status["state"], "failed");
}

#[tokio::test]
async fn unplaced_inputs_are_appended() {
    let case = Case::new();
    case.host.outcome("a", done("ANSWER-A"));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker", "outputs": [{ "name": "out", "type": "text" }] },
            { "id": "b", "subagent": { "prompt": "No placeholders here." }, "depends_on": ["a"],
              "inputs": [{ "name": "draft", "type": "text", "from": "a.out" }] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(
        case.host.spawn_prompts("b"),
        strings(&["No placeholders here.\n\n## Inputs\n- draft: ANSWER-A\n"])
    );
}

#[tokio::test]
async fn two_parent_fan_in_binds_both_answers() {
    let case = Case::new();
    case.host.outcome("a", done("ANSWER-A"));
    case.host.outcome("b", done("ANSWER-B"));
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker", "outputs": [{ "name": "out", "type": "text" }] },
            { "id": "b", "subagent": "worker", "outputs": [{ "name": "out", "type": "text" }] },
            { "id": "c", "subagent": { "prompt": "Combine {left} and {right}." }, "depends_on": ["a", "b"],
              "inputs": [
                  { "name": "left", "type": "text", "from": "a.out" },
                  { "name": "right", "type": "text", "from": "b.out" }
              ] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["a", "b"]));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(
        case.host.spawn_prompts("c"),
        strings(&["Combine ANSWER-A and ANSWER-B."])
    );
    assert_eq!(node_status(&status, "c")["entries_used"], 1);
    assert_eq!(case.host.spawn_calls("c").len(), 1);
    assert_eq!(
        case.all_events_of(&result, "transition_blocked"),
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn fan_in_waits_for_every_parent_across_batches() {
    let case = Case::new();
    case.host.child_outcome("child-1", done("go"));
    case.host.child_outcome("child-2", done("ANSWER-B"));
    case.host.child_outcome("child-3", running());
    case.store_factory(
        json!({ "run": { "failure_policy": "continue" }, "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"], "outputs": [{ "name": "out", "type": "text" }] },
            { "id": "c", "subagent": "worker", "depends_on": ["a"] },
            { "id": "d", "subagent": { "prompt": "Combine {left}." }, "depends_on": ["b", "c"],
              "inputs": [{ "name": "left", "type": "text", "from": "b.out" }] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.wait_until(|| {
        let run = case.executor.snapshot_run(&run_id).unwrap();
        run.states[1].entries.iter().any(|entry| entry.is_settle)
            && case.host.calls_of("rlm.collect").len() >= 2
    })
    .await;
    assert_eq!(case.host.spawn_calls("d"), Vec::<Value>::new());
    let d_entries: Vec<Value> = case
        .all_events_of(&result, "state_entry")
        .into_iter()
        .filter(|event| event["node"] == "d")
        .collect();
    assert_eq!(d_entries, Vec::<Value>::new());
    case.host.child_outcome("child-3", done("ANSWER-C"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(
        case.host.spawn_prompts("d"),
        strings(&["Combine ANSWER-B."])
    );
    assert_eq!(
        case.all_events_of(&result, "transition_blocked"),
        Vec::<Value>::new()
    );
    assert_eq!(node_status(&status, "d")["entries_used"], 1);
}

#[tokio::test]
async fn answer_capture_cap_slices_previews() {
    let case = Case::new();
    let long_answer = "x".repeat(300);
    case.host.outcome("a", done(&long_answer));
    case.store_factory(
        json!({ "nodes": [{ "id": "a", "subagent": "worker" }] }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let captured = node_status(&status, "a")["answer_preview"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(captured, long_answer[..ANSWER_CAPTURE_CAP]);
}

/// `store_fan_factory`: src lists five items; fan expands up to five.
fn store_fan_factory(case: &Case, policy: &str) {
    case.host.outcome(
        "src",
        done("{\"items\": [\"a\", \"b\", \"c\", \"d\", \"e\"]}"),
    );
    case.store_factory(
        json!({
            "run": { "failure_policy": policy, "max_parallel": 8 },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": "worker", "depends_on": ["src"],
                  "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 5 } }
            ]
        }),
        "sw",
    );
}

#[tokio::test]
async fn foreach_expands_clamped_instances() {
    let case = Case::new();
    case.host.outcome(
        "src",
        done("Here.\n```json\n{\"items\": [1, 2, 3, 4, 5]}\n```"),
    );
    case.store_factory(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 8 },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": { "prompt": "Expand item {items}." }, "depends_on": ["src"],
                  "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 3 } }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let fan = node_status(&status, "fan");
    assert_eq!(fan["status"], "done");
    assert_eq!(fan["instances"].as_array().unwrap().len(), 3);
    let mut prompts = case.host.spawn_prompts("fan");
    prompts.sort();
    assert_eq!(
        prompts,
        strings(&["Expand item 1.", "Expand item 2.", "Expand item 3."])
    );
}

#[tokio::test]
async fn foreach_zero_items_marks_node_done() {
    let case = Case::new();
    case.host.outcome("src", done("{\"items\": []}"));
    case.store_factory(
        json!({
            "run": { "failure_policy": "continue" },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": "worker", "depends_on": ["src"],
                  "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 4 } }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "fan")["status"], "done");
    assert_eq!(case.host.spawn_calls("fan"), Vec::<Value>::new());
}

#[tokio::test]
async fn foreach_mixed_instances_escalate_pauses() {
    let case = Case::new();
    store_fan_factory(&case, "escalate");
    let result = case.start().await;
    case.host.child_outcome("child-2", failed("boom-1"));
    case.host.child_outcome("child-3", failed("boom-2"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "paused");
    assert_eq!(case.host.notice_kinds(), strings(&["paused"]));
    let fan = node_status(&status, "fan");
    assert_eq!(fan["status"], "error");
    assert_eq!(
        instance_statuses(fan),
        strings(&["error", "error", "done", "done", "done"])
    );
}

#[tokio::test]
async fn foreach_mixed_instances_fail_fast_cancels_siblings() {
    let case = Case::new();
    store_fan_factory(&case, "fail_fast");
    let result = case.start().await;
    case.host.child_outcome("child-2", failed("boom"));
    for child in ["child-3", "child-4", "child-5", "child-6"] {
        case.host.child_outcome(child, running());
    }
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(case.host.notice_kinds(), strings(&["failed"]));
    let fan = node_status(&status, "fan");
    assert_eq!(fan["status"], "error");
    assert_eq!(
        instance_statuses(fan),
        strings(&["error", "cancelled", "cancelled", "cancelled", "cancelled"])
    );
    assert_eq!(case.host.deleted_targets().len(), 4);
}

#[tokio::test]
async fn foreach_mixed_instances_continue_finishes_with_node_error() {
    let case = Case::new();
    store_fan_factory(&case, "continue");
    let result = case.start().await;
    case.host.child_outcome("child-2", failed("boom-1"));
    case.host.child_outcome("child-3", failed("boom-2"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(case.host.notice_kinds(), strings(&["failed"]));
    let fan = node_status(&status, "fan");
    assert_eq!(fan["status"], "error");
    assert_eq!(
        instance_statuses(fan),
        strings(&["error", "error", "done", "done", "done"])
    );
    assert_eq!(case.host.deleted_targets(), Vec::<String>::new());
}

#[tokio::test]
async fn foreach_failure_never_admits_queued_siblings_and_waits_for_running_ones() {
    let case = Case::new();
    case.host.outcome(
        "src",
        done("{\"items\": [\"a\", \"b\", \"c\", \"d\", \"e\"]}"),
    );
    case.store_factory(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 2 },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": "worker", "depends_on": ["src"],
                  "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 5 } }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
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
    assert_eq!(case.run_state(&result), "running");
    assert_eq!(case.host.spawn_calls("fan").len(), 2);
    case.host.child_outcome("child-3", done("ok"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    let fan = node_status(&status, "fan");
    assert_eq!(fan["status"], "error");
    assert_eq!(
        instance_statuses(fan),
        strings(&["error", "done", "cancelled", "cancelled", "cancelled"])
    );
    assert_eq!(case.host.spawn_calls("fan").len(), 2);
    assert_eq!(case.host.deleted_targets(), Vec::<String>::new());
    let cancelled: Vec<Value> = case
        .all_events_of(&result, "cancelled")
        .into_iter()
        .filter(|event| event["node"] == "fan")
        .collect();
    assert_eq!(cancelled.len(), 3);
    assert!(cancelled
        .iter()
        .all(|event| event["detail"] == "entry failed before admission"));
}

#[tokio::test]
async fn foreach_sibling_failure_after_terminal_entry_settles_without_retry() {
    let case = Case::new();
    case.host
        .outcome("src", done("{\"items\": [\"a\", \"b\"]}"));
    case.store_factory(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 8 },
            "nodes": [
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }] },
                { "id": "fan", "subagent": "worker", "depends_on": ["src"],
                  "inputs": [{ "name": "items", "type": "json", "from": "src.items" }],
                  "foreach": { "over": "items", "max": 2 }, "retries": 1 }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.host.child_outcome("child-2", failed("boom-1"));
    case.host.child_outcome("child-4", failed("boom-2"));
    case.host.child_outcome("child-3", running());
    case.wait_until(|| {
        let run = case.executor.snapshot_run(&run_id).unwrap();
        run.states[1]
            .entries
            .iter()
            .any(|entry| entry.status.as_str() == "error")
    })
    .await;
    assert_eq!(case.host.spawn_calls("fan").len(), 3);
    case.host.child_outcome("child-3", failed("boom-3"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    let fan = node_status(&status, "fan");
    assert_eq!(fan["status"], "error");
    assert_eq!(instance_statuses(fan), strings(&["error", "error"]));
    let attempts: Vec<Value> = fan["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|instance| instance["attempt"].clone())
        .collect();
    assert_eq!(attempts, [json!(2), json!(1)]);
    assert_eq!(case.host.spawn_calls("fan").len(), 3);
    let retries: Vec<Value> = case
        .all_events_of(&result, "retry")
        .into_iter()
        .filter(|event| event["node"] == "fan")
        .map(|event| event["instance"].clone())
        .collect();
    assert_eq!(retries, [json!(0)]);
    // The entry layer reads error throughout.
    assert_eq!(entry_statuses(fan), strings(&["error"]));
}
