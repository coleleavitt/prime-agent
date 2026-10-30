//! Python states: a real isolated subprocess per call (ports of
//! `FactoryPythonStateTest`), run against `python3` in the case's work
//! directory with real-time sleeps.

use serde_json::{Value, json};

use super::fake::{Case, SleepMode, done, node_status, running};

const TOOLS: &str = r#"
import os, subprocess, sys, time

def plan(inputs):
    print("planning")  # prints go to stderr, never into the outputs
    return {"items": ["a", "b"], "mode": "go"}

async def echo(inputs):
    return {"echo": inputs}

def boom(inputs):
    raise ValueError("boom")

def wrong_keys(inputs):
    return {"other": 1}

def slow(inputs):
    with open("slow.pid", "w") as handle:
        handle.write(str(os.getpid()))
    time.sleep(60)
    return {}

def leave_one_running(inputs):
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    with open("left.pid", "w") as handle:
        handle.write(str(child.pid))
    return {}
"#;

fn python_case() -> Case {
    let case = Case::new();
    std::fs::write(case.python.dir.path().join("factory_py_tools.py"), TOOLS)
        .expect("write the tools module");
    case.set_sleep(SleepMode::Real);
    case
}

fn py(callable: &str) -> Value {
    json!({ "import": "factory_py_tools", "callable": callable })
}

/// Wait for the run's control loop to exit, then read `status()`.
async fn finish(case: &Case, result: &Value) -> Value {
    let run_id = Case::run_id(result);
    case.executor.join(&run_id).await;
    case.status(&run_id)
}

/// The pid a tool wrote to `file` in the work directory.
fn written_pid(case: &Case, file: &str) -> Option<i32> {
    std::fs::read_to_string(case.python.dir.path().join(file))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// The pid a tool wrote to `file`, once written.
async fn pid_file(case: &Case, file: &str) -> i32 {
    for _ in 0..1000 {
        if let Some(pid) = written_pid(case, file) {
            return pid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{file} was never written");
}

/// Whether `pid` is still a running process (a zombie counts as gone).
fn running_process(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .next()?
                .to_string();
            Some(state != "Z" && state != "X")
        })
        .unwrap_or(false)
}

/// A killed process is reaped asynchronously: wait for it to leave.
async fn gone(pid: i32) -> bool {
    for _ in 0..500 {
        if !running_process(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn python_states_route_and_bind_like_agent_states() {
    let case = python_case();
    case.host
        .outcome("work", done("```json\n{\"count\": 2}\n```"));
    case.store_machine(
        json!({
            "states": [
                { "id": "plan", "entry": true, "python": py("plan"),
                  "outputs": [{ "name": "items", "type": "json" }, { "name": "mode", "type": "text" }] },
                { "id": "work", "subagent": "worker",
                  "inputs": [{ "name": "items", "type": "json", "from": "plan.items" }],
                  "outputs": [{ "name": "count", "type": "json" }] },
                { "id": "halt", "subagent": "worker" },
                { "id": "check", "python": py("echo"),
                  "inputs": [
                      { "name": "count", "type": "json", "from": "work.count" },
                      { "name": "mode", "type": "text", "from": "plan.mode" }
                  ],
                  "outputs": [{ "name": "echo", "type": "json" }] }
            ],
            "transitions": [
                { "from": "plan", "to": "work", "when": { "output": "mode", "op": "eq", "value": "go" } },
                { "from": "plan", "to": "halt", "when": { "output": "mode", "op": "eq", "value": "stop" } },
                { "from": "work", "to": "check" }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = finish(&case, &result).await;

    assert_eq!(status["state"], "done");
    // Only the agent state spawned a child.
    assert_eq!(case.host.calls_of("rlm.run"), case.host.spawn_calls("work"));
    let prompts = case.host.spawn_prompts("work");
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("[\"a\", \"b\"]"), "{prompts:?}");
    assert_eq!(node_status(&status, "halt")["status"], "pending");
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    let check = run
        .states
        .iter()
        .find(|state| state.state_id == "check")
        .unwrap();
    assert_eq!(
        check.entries[0].outputs.clone().map(Value::Object),
        Some(json!({ "echo": { "count": 2, "mode": "go" } }))
    );
    let plan = node_status(&status, "plan");
    assert_eq!(plan["instances"][0]["child"], Value::Null);
    assert_eq!(
        plan["answer_preview"],
        "{\"items\":[\"a\",\"b\"],\"mode\":\"go\"}"
    );
    let graph = case
        .executor
        .graph(Some(&Case::run_id(&result)), false)
        .unwrap();
    let graph_plan = graph["machine"]["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "plan")
        .unwrap();
    assert_eq!(graph_plan["python"], "factory_py_tools.plan");
    assert_eq!(graph_plan.get("subagent"), None);
}

#[tokio::test]
async fn failed_calls_retry_then_apply_the_failure_policy() {
    for (callable, expected) in [
        ("boom", "python state exited with code 1: "),
        (
            "wrong_keys",
            "python state returned keys ['other'] but declares outputs ['out']",
        ),
    ] {
        let case = python_case();
        case.store_machine(
            json!({
                "run": { "failure_policy": "continue" },
                "states": [{ "id": "call", "entry": true, "python": py(callable), "retries": 1,
                             "outputs": [{ "name": "out", "type": "json" }] }]
            }),
            "sw",
        );
        let status = finish(&case, &case.start().await).await;
        assert_eq!(status["state"], "failed", "{callable}");
        let call = node_status(&status, "call");
        assert_eq!(call["attempts"], 2, "{callable}");
        let error = call["error"].as_str().unwrap();
        assert!(error.contains(expected), "{callable}: {error}");
        if callable == "boom" {
            assert!(error.contains("ValueError: boom"), "{error}");
        }
    }
}

#[tokio::test]
async fn a_call_past_its_time_limit_is_killed_without_a_retry() {
    let case = python_case();
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [{ "id": "slow", "entry": true, "python": py("slow"), "retries": 2, "budget_ms": 300 }]
        }),
        "sw",
    );
    let status = finish(&case, &case.start().await).await;
    // A loaded machine can reach the limit before the call wrote its pid.
    if let Some(pid) = written_pid(&case, "slow.pid") {
        assert!(gone(pid).await, "the timed-out call was not killed");
    }
    let slow = node_status(&status, "slow");
    assert_eq!(slow["attempts"], 1);
    assert_eq!(slow["error"], "python state time limit 300ms exceeded");
}

#[tokio::test]
async fn stop_kills_a_running_call() {
    let case = python_case();
    case.store_machine(
        json!({ "states": [{ "id": "slow", "entry": true, "python": py("slow") }] }),
        "sw",
    );
    let result = case.start().await;
    let pid = pid_file(&case, "slow.pid").await;
    let stopped = case.stop(&result).await;
    assert_eq!(stopped["state"], "stopped");
    assert!(gone(pid).await, "stop left the call running");
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    let entry = &run.states[0].entries[0];
    assert_eq!(
        (entry.status.as_str(), entry.instances[0].status.as_str()),
        ("cancelled", "cancelled")
    );
}

#[tokio::test]
async fn a_call_takes_what_it_left_running_with_it() {
    let case = python_case();
    case.store_machine(
        json!({ "states": [{ "id": "leave", "entry": true, "python": py("leave_one_running") }] }),
        "sw",
    );
    let status = finish(&case, &case.start().await).await;
    assert_eq!(status["state"], "done");
    let pid = pid_file(&case, "left.pid").await;
    assert!(gone(pid).await, "the call's leftover process survived it");
}

#[tokio::test]
async fn a_python_deadline_is_not_delayed_by_a_running_child() {
    let case = python_case();
    case.host.outcome("work", running());
    // Each child poll spends its whole timeout (nothing settles).
    case.clock.set_advance_per_collect(0.3);
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue" },
            "states": [
                { "id": "work", "entry": true, "subagent": "worker" },
                { "id": "slow", "entry": true, "python": py("slow"), "budget_ms": 300 }
            ]
        }),
        "sw",
    );
    let result = case.start().await;
    // Bound well below the ordinary 2-second child poll.
    let first_poll = loop {
        if let Some(call) = case.host.calls_of("rlm.collect").first() {
            break call["timeout_ms"].as_u64().unwrap();
        }
        tokio::task::yield_now().await;
    };
    assert!(first_poll <= 300, "the child poll waited {first_poll}ms");
    case.wait_until(|| {
        node_status(&case.status(&Case::run_id(&result)), "slow")["status"] == "error"
    })
    .await;
    case.stop(&result).await;
}

#[tokio::test]
async fn executor_failure_stops_its_python_processes() {
    let case = python_case();
    case.host.outcome("work", running());
    case.store_machine(
        json!({ "states": [
            { "id": "work", "entry": true, "subagent": "worker" },
            { "id": "slow", "entry": true, "python": py("slow") }
        ] }),
        "sw",
    );
    let result = case.start().await;
    let pid = pid_file(&case, "slow.pid").await;
    case.host.with(|state| state.collect_fails = true);
    finish(&case, &result).await;
    let run = case.executor.snapshot_run(&Case::run_id(&result)).unwrap();
    let slow = &run.states[1].entries[0];
    assert_eq!(
        (
            run.state.as_str(),
            slow.status.as_str(),
            slow.instances[0].status.as_str(),
        ),
        ("failed", "cancelled", "cancelled")
    );
    assert!(gone(pid).await, "the failed loop left the call running");
}
