//! Machine-form semantics: guarded transitions, fan-out, re-entry, joins,
//! `max_transitions`, stalls, optional inputs, residents, and the resume
//! generation (ports of `FactoryExecutorTest`).

use serde_json::{json, Value};

use super::fake::{done, entry_statuses, events_of, failed, node_status, running, strings, Case};

fn review_loop_machine() -> Value {
    json!({
        "run": { "failure_policy": "continue", "max_parallel": 4 },
        "states": [
            { "id": "draft", "entry": true, "subagent": "worker", "outputs": [{ "name": "draft", "type": "text" }] },
            { "id": "reviewing", "subagent": "worker",
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

#[tokio::test]
async fn machine_three_round_review_loop() {
    let case = Case::new();
    case.host.child_outcome("child-1", done("DRAFT-1"));
    case.host.child_outcome(
        "child-2",
        done("```json\n{\"verdict\": {\"approved\": false, \"findings\": [\"AUDIT-A1\"]}}\n```"),
    );
    case.host.child_outcome("child-3", done("fixed round 1"));
    case.host.child_outcome(
        "child-4",
        done("```json\n{\"verdict\": {\"approved\": false, \"findings\": [\"AUDIT-B1\"]}}\n```"),
    );
    case.host.child_outcome("child-5", done("fixed round 2"));
    case.host.child_outcome(
        "child-6",
        done("```json\n{\"verdict\": {\"approved\": true, \"findings\": []}}\n```"),
    );
    case.store_machine(review_loop_machine(), "sw");
    let result = case.start().await;
    assert_eq!(result["started"], json!(["draft"]));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let reviewing = node_status(&status, "reviewing");
    let fixing = node_status(&status, "fixing");
    assert_eq!(reviewing["status"], "done");
    assert_eq!(reviewing["entries_used"], 3);
    assert_eq!(reviewing["entries"].as_array().unwrap().len(), 3);
    assert_eq!(reviewing["instances"].as_array().unwrap().len(), 3);
    assert_eq!(fixing["status"], "done");
    assert_eq!(fixing["entries_used"], 2);
    assert_eq!(case.host.spawn_calls("reviewing").len(), 3);
    assert_eq!(case.host.spawn_calls("fixing").len(), 2);
    assert_eq!(status["usage"]["transitions_fired"], 5);
    assert_eq!(case.all_events_of(&result, "transition_fired").len(), 5);
    assert_eq!(
        case.all_events_of(&result, "transition_blocked"),
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn machine_guard_switch_fires_only_the_matching_branch() {
    let case = Case::new();
    case.host.outcome(
        "pick",
        done("```json\n{\"pick\": {\"choice\": \"left\"}}\n```"),
    );
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "pick", "entry": true, "subagent": "worker", "outputs": [{ "name": "pick", "type": "json" }] },
                { "id": "left", "subagent": "worker" },
                { "id": "right", "subagent": "worker" }
            ],
            "transitions": [
                { "from": "pick", "to": "left", "when": { "output": "pick", "path": "choice", "op": "eq", "value": "left" } },
                { "from": "pick", "to": "right", "when": { "output": "pick", "path": "choice", "op": "eq", "value": "right" } }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let left = node_status(&status, "left");
    let right = node_status(&status, "right");
    assert_eq!(left["status"], "done");
    assert_eq!(left["entries_used"], 1);
    assert_eq!(right["entries_used"], 0);
    assert_eq!(right["status"], "pending");
    assert_eq!(case.host.spawn_calls("right"), Vec::<Value>::new());
    assert_eq!(status["usage"]["transitions_fired"], 1);
    let fired = case.all_events_of(&result, "transition_fired");
    assert_eq!(fired.len(), 1);
    let guard = json!({ "output": "pick", "path": "choice", "op": "eq", "value": "left" });
    assert_eq!(fired[0]["when"], guard);
    let graph = case
        .executor
        .graph(Some(&Case::run_id(&result)), false)
        .unwrap();
    assert_eq!(graph["last_fired"].as_array().unwrap().len(), 1);
    assert_eq!(graph["last_fired"][0]["when"], guard);
}

#[tokio::test]
async fn machine_fan_out_from_one_settle_fires_all() {
    let case = Case::new();
    case.host.outcome("fan", done("go"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "fan", "entry": true, "subagent": "worker", "outputs": [{ "name": "go", "type": "text" }] },
                { "id": "left", "subagent": "worker" },
                { "id": "right", "subagent": "worker" }
            ],
            "transitions": [{ "from": "fan", "to": "left" }, { "from": "fan", "to": "right" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "left")["status"], "done");
    assert_eq!(node_status(&status, "right")["status"], "done");
    assert_eq!(status["usage"]["transitions_fired"], 2);
    let targets: Vec<Value> = case
        .all_events_of(&result, "transition_fired")
        .iter()
        .map(|event| event["to"].clone())
        .collect();
    assert_eq!(targets, [json!("left"), json!("right")]);
}

#[tokio::test]
async fn machine_max_entries_blocked_transition_quiesces_done() {
    let case = Case::new();
    case.store_machine(
        json!({
            "states": [
                { "id": "once", "entry": true, "subagent": "worker", "max_entries": 1 },
                { "id": "sink", "subagent": "worker", "max_entries": 1 }
            ],
            "transitions": [
                { "from": "once", "to": "sink" },
                { "from": "once", "to": "sink" },
                { "from": "once", "to": "once" }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let once = node_status(&status, "once");
    assert_eq!(once["entries_used"], 1);
    assert_eq!(once["max_entries"], 1);
    assert_eq!(node_status(&status, "sink")["entries_used"], 1);
    assert_eq!(status["usage"]["transitions_fired"], 1);
    let blocked = case.all_events_of(&result, "transition_blocked");
    assert_eq!(blocked.len(), 2);
    assert!(blocked
        .iter()
        .all(|event| event["to"] == "once" || event["to"] == "sink"));
    assert_eq!(
        blocked[0]["detail"],
        "state 'sink' is at max_entries 1; transition 'once' -> 'sink' blocked"
    );
}

#[tokio::test]
async fn machine_self_loop_reenters_until_guard_fails() {
    let case = Case::new();
    for (child, value) in [("child-1", 1), ("child-2", 2), ("child-3", 3)] {
        case.host.child_outcome(
            child,
            done(&format!(
                "```json\n{{\"count\": {{\"value\": {value}}}}}\n```"
            )),
        );
    }
    case.store_machine(
        json!({
            "states": [{ "id": "tick", "entry": true, "subagent": "worker", "max_entries": 4,
                         "outputs": [{ "name": "count", "type": "json" }] }],
            "transitions": [{ "from": "tick", "to": "tick",
                              "when": { "output": "count", "path": "value", "op": "lt", "value": 3 } }]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "tick")["entries_used"], 3);
    assert_eq!(case.host.spawn_calls("tick").len(), 3);
    assert_eq!(status["usage"]["transitions_fired"], 2);
}

#[tokio::test]
async fn machine_max_transitions_pauses_once_then_resumes() {
    let case = Case::new();
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
    let kinds = case.host.notice_kinds();
    assert!(kinds.contains(&"max_transitions_exceeded".to_string()));
    assert!(!kinds.contains(&"budget_exceeded".to_string()));
    assert_eq!(paused["usage"]["transitions_fired"], 1);
    assert_eq!(node_status(&paused, "b")["status"], "done");
    assert_eq!(node_status(&paused, "c")["entries_used"], 0);
    let resumed = case.resume(&result).await.expect("resume");
    assert_eq!(resumed["state"], "running");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(finished["usage"]["transitions_fired"], 2);
    assert_eq!(node_status(&finished, "c")["status"], "done");
}

#[tokio::test]
async fn machine_max_transitions_pause_mid_settle_does_not_refire_on_resume() {
    let case = Case::new();
    case.store_machine(
        json!({
            "run": { "max_transitions": 1, "failure_policy": "continue" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker" },
                { "id": "b", "subagent": "worker" },
                { "id": "c", "subagent": "worker" }
            ],
            "transitions": [{ "from": "a", "to": "b" }, { "from": "a", "to": "c" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    assert!(case
        .host
        .notice_kinds()
        .contains(&"max_transitions_exceeded".to_string()));
    assert_eq!(node_status(&paused, "b")["entries_used"], 1);
    assert_eq!(node_status(&paused, "c")["entries_used"], 0);
    assert_eq!(paused["usage"]["transitions_fired"], 1);
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(node_status(&finished, "b")["entries_used"], 1);
    assert_eq!(node_status(&finished, "c")["entries_used"], 1);
    assert_eq!(case.host.spawn_calls("b").len(), 1);
    assert_eq!(finished["usage"]["transitions_fired"], 2);
}

#[tokio::test]
async fn machine_join_paused_at_max_transitions_fires_after_resume() {
    let case = Case::new();
    case.store_machine(
        json!({
            "run": { "max_transitions": 1, "failure_policy": "continue" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker" },
                { "id": "b", "entry": true, "subagent": "worker" },
                { "id": "c", "subagent": "worker", "max_entries": 2 }
            ],
            "transitions": [{ "from": "a", "to": "c" }, { "from": ["a", "b"], "to": "c" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let paused = case.settle(&result).await;
    assert_eq!(paused["state"], "paused");
    assert!(case
        .host
        .notice_kinds()
        .contains(&"max_transitions_exceeded".to_string()));
    assert_eq!(node_status(&paused, "c")["entries_used"], 1);
    assert_eq!(paused["usage"]["transitions_fired"], 1);
    case.resume(&result).await.expect("resume");
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "done");
    assert_eq!(node_status(&finished, "c")["entries_used"], 2);
    let joins: Vec<Value> = case
        .all_events_of(&result, "transition_fired")
        .into_iter()
        .filter(|event| event["from"] == json!(["a", "b"]))
        .collect();
    assert_eq!(joins.len(), 1);
    assert_eq!(case.host.spawn_calls("c").len(), 2);
    assert_eq!(finished["usage"]["transitions_fired"], 2);
}

#[tokio::test]
async fn machine_guards_compare_json_strictly() {
    let case = Case::new();
    case.host.outcome(
        "pick",
        done("```json\n{\"pick\": {\"approved\": true}}\n```"),
    );
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "pick", "entry": true, "subagent": "worker", "outputs": [{ "name": "pick", "type": "json" }] },
                { "id": "boolish", "subagent": "worker" },
                { "id": "numish", "subagent": "worker" },
                { "id": "strish", "subagent": "worker" }
            ],
            "transitions": [
                { "from": "pick", "to": "boolish", "when": { "output": "pick", "path": "approved", "op": "eq", "value": true } },
                { "from": "pick", "to": "numish", "when": { "output": "pick", "path": "approved", "op": "eq", "value": 1 } },
                { "from": "pick", "to": "strish", "when": { "output": "pick", "path": "approved", "op": "eq", "value": "true" } }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "boolish")["status"], "done");
    assert_eq!(node_status(&status, "numish")["entries_used"], 0);
    assert_eq!(node_status(&status, "strish")["entries_used"], 0);

    case.host
        .outcome("num", done("```json\n{\"num\": {\"value\": 1.0}}\n```"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "num", "entry": true, "subagent": "worker", "outputs": [{ "name": "num", "type": "json" }] },
                { "id": "floats", "subagent": "worker" }
            ],
            "transitions": [{ "from": "num", "to": "floats", "when": { "output": "num", "path": "value", "op": "eq", "value": 1 } }]
        }),
        "numeric",
    );
    let result = case.start_spec("numeric").await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "floats")["status"], "done");

    case.host
        .outcome("flag", done("```json\n{\"flag\": {\"on\": true}}\n```"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "flag", "entry": true, "subagent": "worker", "outputs": [{ "name": "flag", "type": "json" }] },
                { "id": "notone", "subagent": "worker" }
            ],
            "transitions": [{ "from": "flag", "to": "notone", "when": { "output": "flag", "path": "on", "op": "ne", "value": 1 } }]
        }),
        "necheck",
    );
    let result = case.start_spec("necheck").await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "notone")["status"], "done");
}

#[tokio::test]
async fn machine_stall_fails_with_the_pending_entry_reason() {
    let case = Case::new();
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "a", "entry": true, "subagent": "worker", "outputs": [{ "name": "o", "type": "text" }] },
                { "id": "b", "subagent": "worker", "inputs": [{ "name": "i", "type": "text", "from": "c.o" }] },
                { "id": "c", "subagent": "worker", "outputs": [{ "name": "o", "type": "text" }] }
            ],
            "transitions": [{ "from": "a", "to": "b" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(node_status(&status, "b")["status"], "pending");
    let stalls = events_of(&status, "executor_error");
    assert_eq!(stalls.len(), 1);
    assert_eq!(
        stalls[0]["error"],
        "control loop stalled: pending entry of state 'b' is waiting for an input source that never settled"
    );
    assert!(case.host.notice_kinds().contains(&"failed".to_string()));
}

/// The review/fix loop whose reviewer reads the fixer's report optionally.
fn optional_fix_machine() -> Value {
    json!({
        "run": { "failure_policy": "continue", "max_parallel": 4 },
        "states": [
            { "id": "seed", "entry": true, "subagent": "worker", "outputs": [{ "name": "go", "type": "text" }] },
            { "id": "rev", "subagent": "worker",
              "inputs": [
                  { "name": "go", "type": "text", "from": "seed.go" },
                  { "name": "fix", "type": "json", "from": "fixer.fix", "optional": true }
              ],
              "outputs": [{ "name": "verdict", "type": "json" }], "max_entries": 4 },
            { "id": "fixer", "subagent": "worker",
              "inputs": [{ "name": "verdict", "type": "json", "from": "rev.verdict" }],
              "outputs": [{ "name": "fix", "type": "json" }], "max_entries": 2 }
        ],
        "transitions": [
            { "from": "seed", "to": "rev" },
            { "from": "rev", "to": "fixer", "when": { "output": "verdict", "path": "approved", "op": "eq", "value": false } },
            { "from": "fixer", "to": "rev" }
        ]
    })
}

#[tokio::test]
async fn machine_optional_input_binds_null_then_the_real_settle() {
    let case = Case::new();
    case.host.child_outcome("child-1", done("GO"));
    case.host.child_outcome(
        "child-2",
        done("```json\n{\"verdict\": {\"approved\": false, \"findings\": [\"AUDIT-A1\"]}}\n```"),
    );
    case.host.child_outcome(
        "child-3",
        done("```json\n{\"fix\": {\"fixed\": [\"AUDIT-A1\"]}}\n```"),
    );
    case.host.child_outcome(
        "child-4",
        done("```json\n{\"verdict\": {\"approved\": true, \"findings\": []}}\n```"),
    );
    case.store_machine(optional_fix_machine(), "sw");
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let prompts = case.host.spawn_prompts("rev");
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[0].contains("null") && !prompts[0].contains("AUDIT-A1"),
        "{}",
        prompts[0]
    );
    assert!(prompts[1].contains("AUDIT-A1"), "{}", prompts[1]);
    assert_eq!(node_status(&status, "rev")["entries_used"], 2);
    assert_eq!(node_status(&status, "fixer")["entries_used"], 1);
    assert_eq!(status["usage"]["transitions_fired"], 3);
}

fn optional_foreach_machine() -> Value {
    json!({
        "run": { "failure_policy": "continue", "max_parallel": 4 },
        "states": [
            { "id": "seed", "entry": true, "subagent": "worker" },
            { "id": "fan", "subagent": { "prompt": "Process item {items}." },
              "inputs": [{ "name": "items", "type": "json", "from": "src.items", "optional": true }],
              "foreach": { "over": "items", "max": 4 }, "max_entries": 2 },
            { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }], "max_entries": 1 }
        ],
        "transitions": [
            { "from": "seed", "to": "fan" },
            { "from": "fan", "to": "src" },
            { "from": "src", "to": "fan" }
        ]
    })
}

fn node_ready_details(case: &Case, result: &Value, node: &str, entry: Option<u64>) -> Vec<Value> {
    case.all_events_of(result, "node_ready")
        .into_iter()
        .filter(|event| event["node"] == node && entry.is_none_or(|entry| event["entry"] == entry))
        .map(|event| event["detail"].clone())
        .collect()
}

#[tokio::test]
async fn machine_optional_foreach_over_expands_empty_then_the_real_settle() {
    let case = Case::new();
    case.host.outcome(
        "src",
        done("```json\n{\"items\": [\"a\", \"b\", \"c\"]}\n```"),
    );
    case.store_machine(optional_foreach_machine(), "sw");
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let fan = node_status(&status, "fan");
    assert_eq!(fan["entries_used"], 2);
    assert_eq!(entry_statuses(fan), strings(&["done", "done"]));
    assert_eq!(
        case.all_events_of(&result, "node_error"),
        Vec::<Value>::new()
    );
    assert_eq!(
        node_ready_details(&case, &result, "fan", Some(0)),
        [json!("foreach expanded to zero items; nothing to run")]
    );
    assert_eq!(
        case.host.spawn_prompts("fan"),
        strings(&["Process item a.", "Process item b.", "Process item c."])
    );
    assert_eq!(node_status(&status, "src")["entries_used"], 1);
    assert_eq!(status["usage"]["transitions_fired"], 3);
}

#[tokio::test]
async fn machine_optional_input_over_an_errored_source_binds_the_null_sentinel() {
    let case = Case::new();
    case.host.child_outcome("child-1", done("GO"));
    case.host.child_outcome(
        "child-2",
        done("```json\n{\"verdict\": {\"approved\": false, \"findings\": [\"AUDIT-A1\"]}}\n```"),
    );
    case.host.child_outcome("child-3", failed("fixer exploded"));
    case.host.child_outcome(
        "child-4",
        done("```json\n{\"verdict\": {\"approved\": true, \"findings\": []}}\n```"),
    );
    case.store_machine(optional_fix_machine(), "sw");
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    let rev = node_status(&status, "rev");
    assert_eq!(rev["entries_used"], 2);
    assert_eq!(entry_statuses(rev), strings(&["done", "done"]));
    assert!(rev.get("error").is_none());
    let fixer = node_status(&status, "fixer");
    assert_eq!(fixer["entries_used"], 1);
    assert_eq!(fixer["error"], "fixer exploded");
    let prompts = case.host.spawn_prompts("rev");
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains("- fix: null"));
    assert!(prompts[1].contains("- fix: null"));
    assert_eq!(case.host.spawn_calls("fixer").len(), 1);
}

#[tokio::test]
async fn machine_optional_foreach_over_an_errored_source_expands_empty() {
    let case = Case::new();
    case.host.outcome("src", failed("src exploded"));
    case.store_machine(optional_foreach_machine(), "sw");
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    let fan = node_status(&status, "fan");
    assert_eq!(fan["entries_used"], 2);
    assert_eq!(entry_statuses(fan), strings(&["done", "done"]));
    assert!(fan.get("error").is_none());
    assert_eq!(case.host.spawn_calls("fan"), Vec::<Value>::new());
    let fan_errors: Vec<Value> = case
        .all_events_of(&result, "node_error")
        .into_iter()
        .filter(|event| event["node"] == "fan")
        .collect();
    assert_eq!(fan_errors, Vec::<Value>::new());
    assert_eq!(
        node_ready_details(&case, &result, "fan", None),
        [
            json!("foreach expanded to zero items; nothing to run"),
            json!("foreach expanded to zero items; nothing to run")
        ]
    );
}

#[tokio::test]
async fn machine_required_input_over_an_errored_source_fails_the_dependent_entry() {
    let case = Case::new();
    case.host.outcome("src", failed("src exploded"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 4 },
            "states": [
                { "id": "seed", "entry": true, "subagent": "worker" },
                { "id": "src", "subagent": "worker", "outputs": [{ "name": "items", "type": "json" }], "max_entries": 1 },
                { "id": "dep", "subagent": "worker", "inputs": [{ "name": "items", "type": "json", "from": "src.items" }], "max_entries": 1 }
            ],
            "transitions": [{ "from": "seed", "to": "src" }, { "from": "src", "to": "dep" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    let dep = node_status(&status, "dep");
    assert_eq!(dep["entries_used"], 1);
    assert_eq!(entry_statuses(dep), strings(&["error"]));
    assert_eq!(
        dep["entries"][0]["error"],
        "input 'items' from state 'src' is unavailable (latest settle status 'error')"
    );
    assert_eq!(case.host.spawn_calls("dep"), Vec::<Value>::new());
}

/// `src` settles done with `answer`; `opt` reads its port optionally, `req`
/// requires it.
async fn valueless_source(answer: &str, port: &str, port_type: &str) -> (Case, Value) {
    let case = Case::new();
    case.host.outcome("src", done(answer));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 4 },
            "states": [
                { "id": "seed", "entry": true, "subagent": "worker" },
                { "id": "src", "subagent": "worker", "outputs": [{ "name": port, "type": port_type }], "max_entries": 1 },
                { "id": "opt", "subagent": { "prompt": "Proceed." },
                  "inputs": [{ "name": port, "type": port_type, "from": format!("src.{port}"), "optional": true }], "max_entries": 1 },
                { "id": "req", "subagent": { "prompt": "Proceed." },
                  "inputs": [{ "name": port, "type": port_type, "from": format!("src.{port}") }], "max_entries": 1 }
            ],
            "transitions": [
                { "from": "seed", "to": "src" },
                { "from": "src", "to": "opt" },
                { "from": "src", "to": "req" }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    (case, status)
}

#[tokio::test]
async fn machine_optional_input_over_a_settled_source_with_no_captured_value_binds_the_null_sentinel(
) {
    let (case, status) = valueless_source("", "go", "text").await;
    assert_eq!(status["state"], "failed");
    let opt = node_status(&status, "opt");
    assert_eq!(opt["entries_used"], 1);
    assert_eq!(entry_statuses(opt), strings(&["done"]));
    assert!(case.host.spawn_prompts("opt")[0].contains("- go: None"));
    let req = node_status(&status, "req");
    assert_eq!(req["entries_used"], 1);
    assert_eq!(entry_statuses(req), strings(&["error"]));
    assert_eq!(
        req["entries"][0]["error"],
        "input 'go' from state 'src' has no captured output 'go'"
    );
    assert_eq!(case.host.spawn_calls("req"), Vec::<Value>::new());
}

#[tokio::test]
async fn machine_optional_input_over_a_failed_json_capture_binds_the_null_sentinel() {
    let (case, status) = valueless_source("no json here", "data", "json").await;
    assert_eq!(status["state"], "failed");
    let opt = node_status(&status, "opt");
    assert_eq!(opt["entries_used"], 1);
    assert_eq!(entry_statuses(opt), strings(&["done"]));
    assert!(case.host.spawn_prompts("opt")[0].contains("- data: null"));
    let req = node_status(&status, "req");
    assert_eq!(entry_statuses(req), strings(&["error"]));
    assert_eq!(
        req["entries"][0]["error"],
        "input 'data': no JSON object containing output 'data' in the upstream answer"
    );
    assert_eq!(case.host.spawn_calls("req"), Vec::<Value>::new());
}

#[tokio::test]
async fn resident_node_spawns_stays_alive_and_stops() {
    let case = Case::new();
    case.host.outcome("watcher", running());
    case.store_factory(
        json!({ "nodes": [
            { "id": "t", "subagent": "worker" },
            { "id": "watcher", "subagent": "worker", "lifecycle": "resident" }
        ] }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["t", "watcher"]));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "t")["status"], "done");
    let resident = node_status(&status, "watcher");
    assert_eq!(resident["status"], "running");
    assert_eq!(resident["lifecycle"], "resident");
    assert!(case.host.notice_kinds().contains(&"finished".to_string()));
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["watcher"]));
    assert_eq!(case.host.deleted_targets(), strings(&["child-2"]));
    assert_eq!(case.status(&Case::run_id(&result))["state"], "stopped");
}

#[tokio::test]
async fn resident_queued_instance_is_admitted_before_completion() {
    let case = Case::new();
    case.host.outcome("go", running());
    case.host.outcome("watcher", running());
    case.store_machine(
        json!({
            "run": { "max_parallel": 1 },
            "states": [
                { "id": "go", "entry": true, "subagent": "worker" },
                { "id": "watcher", "entry": true, "subagent": "worker", "lifecycle": "resident" }
            ],
            "transitions": []
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["go"]));
    case.wait_until(|| case.host.collects() >= 2).await;
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    let watcher = &run.states[1];
    assert_eq!(watcher.entries.last().unwrap().status.as_str(), "running");
    assert_eq!(
        watcher
            .entries
            .last()
            .unwrap()
            .instances
            .last()
            .unwrap()
            .status
            .as_str(),
        "pending"
    );
    case.host.outcome("go", done("go-done"));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    assert_eq!(node_status(&status, "watcher")["status"], "running");
    assert_eq!(status["usage"]["spawns"], 2);
    assert_eq!(case.host.spawn_calls("watcher").len(), 1);
    assert!(case.host.notice_kinds().contains(&"finished".to_string()));
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["watcher"]));
    assert_eq!(case.host.deleted_targets(), strings(&["child-2"]));
}

#[tokio::test]
async fn resident_saturated_cap_fails_the_run_instead_of_wedging() {
    let case = Case::new();
    case.host.outcome("r1", running());
    case.host.outcome("r2", running());
    case.store_machine(
        json!({
            "run": { "max_parallel": 1 },
            "states": [
                { "id": "r1", "entry": true, "subagent": "worker", "lifecycle": "resident" },
                { "id": "r2", "entry": true, "subagent": "worker", "lifecycle": "resident" }
            ],
            "transitions": []
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["r1"]));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    let errors = events_of(&status, "executor_error");
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["error"],
        "control loop stalled: resident instances hold every max_parallel 1 slot; queued instances can never be admitted"
    );
    assert!(case.host.notice_kinds().contains(&"failed".to_string()));
    assert_eq!(case.host.spawn_calls("r2"), Vec::<Value>::new());
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["r1", "r2"]));
    assert_eq!(case.host.deleted_targets(), strings(&["child-1"]));
}

#[tokio::test]
async fn resident_in_flight_stall_fails_the_run_instead_of_wedging() {
    let case = Case::new();
    case.host.outcome("watcher", running());
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "watcher", "entry": true, "subagent": "worker", "lifecycle": "resident" },
                { "id": "a", "entry": true, "subagent": "worker", "outputs": [{ "name": "o", "type": "text" }] },
                { "id": "b", "subagent": "worker", "inputs": [{ "name": "i", "type": "text", "from": "c.o" }] },
                { "id": "c", "subagent": "worker", "outputs": [{ "name": "o", "type": "text" }] }
            ],
            "transitions": [{ "from": "a", "to": "b" }]
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(result["started"], json!(["watcher", "a"]));
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(node_status(&status, "b")["status"], "pending");
    let stalls = events_of(&status, "executor_error");
    assert_eq!(stalls.len(), 1);
    let error = stalls[0]["error"].as_str().unwrap();
    assert!(
        error.contains("pending entry of state 'b'") && error.contains("never settled"),
        "{error}"
    );
    assert!(case.host.notice_kinds().contains(&"failed".to_string()));
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["cancelled"], json!(["watcher", "b", "c"]));
    assert_eq!(case.host.deleted_targets(), strings(&["child-1"]));
}

#[tokio::test]
async fn resume_bumps_the_loop_generation_no_double_admission() {
    let case = Case::new();
    case.host.outcome("a", failed("boom"));
    case.host
        .with(|state| state.rate_limit_first.insert("b".into(), 1));
    let progress_gate = case.host.gate("factory.progress", 1);
    let progress_entered = case.host.gate_entered("factory.progress", 1);
    let spawn_gate = case.host.gate("rlm.run", 3);
    case.store_factory(
        json!({ "nodes": [
            { "id": "a", "subagent": "worker" },
            { "id": "b", "subagent": "worker", "depends_on": ["a"] }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    progress_entered.wait().await;
    assert_eq!(case.run_state(&result), "paused");
    let old_task = case
        .executor
        .take_loop_task(&run_id)
        .expect("the pause-path loop");
    assert!(!old_task.is_finished());
    let resumed = case.resume(&result).await.expect("resume");
    assert_eq!(resumed["state"], "running");
    assert_eq!(resumed["started"], json!([]));
    progress_gate.set();
    old_task.await.expect("the old loop exits");
    spawn_gate.set();
    let finished = case.settle(&result).await;
    assert_eq!(finished["state"], "failed");
    assert_eq!(node_status(&finished, "b")["status"], "done");
    assert_eq!(case.host.spawn_calls("b").len(), 2);
    assert_eq!(
        case.all_events_of(&result, "executor_error"),
        Vec::<Value>::new()
    );
    let b_entries = case
        .all_events_of(&result, "state_entry")
        .into_iter()
        .filter(|event| event["node"] == "b")
        .count();
    assert_eq!(b_entries, 1);
}

#[tokio::test]
async fn long_state_ids_spawn_distinct_sibling_names() {
    let case = Case::new();
    case.store_machine(
        json!({
            "run": { "max_parallel": 8 },
            "states": [
                { "id": "collect-findings-pass-one", "entry": true, "subagent": "worker" },
                { "id": "collect-findings-pass-two", "entry": true, "subagent": "worker" }
            ],
            "transitions": []
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "done");
    let names = case.host.spawn_names();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    assert!(names
        .iter()
        .all(|name| name.starts_with("sw-collect-findings-") && name.len() <= 64));
    assert_eq!(
        node_status(&status, "collect-findings-pass-one")["status"],
        "done"
    );
    assert_eq!(
        node_status(&status, "collect-findings-pass-two")["status"],
        "done"
    );
}
