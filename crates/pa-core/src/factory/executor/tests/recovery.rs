//! Machine-recovery fixes for the M1-M6 triage classes (ports of upstream
//! #3462's `FactoryExecutorTest` additions): run-scoped child names, the
//! full-answer binding lane and its capture retry, optional inputs over a
//! live upstream, exit captures that need verification, and the paused
//! run's error payload.

use serde_json::{json, Value};

use super::fake::{done, exited, failed, node_status, running, Case, HOST_ANSWER_PREVIEW_CHARS};
use crate::factory::executor::binding::{ANSWER_BINDING_CAP, ANSWER_CAPTURE_CAP};

fn label_of(result: &Value, configured: &str) -> String {
    format!("{}-{configured}", &result["run_id"].as_str().unwrap()[..6])
}

#[tokio::test]
async fn two_runs_of_one_machine_do_not_collide_on_child_names() {
    // M1: a prior run's settled children stay registered under the parent
    // session, so a verbatim configured name failed the next run's spawn
    // admission ("Agent name ... is unavailable"); the fake rejects
    // duplicate sibling names exactly like the supervisor.
    let case = Case::new();
    case.host.outcome("impl", done("IMPL"));
    case.host.outcome("check", done("CHECK"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "escalate", "max_parallel": 4 },
            "states": [
                { "id": "impl", "entry": true, "subagent": { "prompt": "Build.", "name": "impl-build" },
                  "outputs": [{ "name": "built", "type": "text" }] },
                { "id": "check", "subagent": "worker",
                  "inputs": [{ "name": "built", "type": "text", "from": "impl.built" }] }
            ],
            "transitions": [{ "from": "impl", "to": "check" }]
        }),
        "sw",
    );
    let first = case.start().await;
    assert_eq!(case.settle(&first).await["state"], "done");
    let second = case.start().await;
    assert_eq!(case.settle(&second).await["state"], "done");
    let spawned = |result: &Value| -> Vec<Value> {
        case.all_events_of(result, "spawned")
            .into_iter()
            .filter(|event| event["node"] == "impl")
            .map(|event| event["name"].clone())
            .collect()
    };
    assert_eq!(
        (spawned(&first), spawned(&second)),
        (
            vec![json!(label_of(&first, "impl-build"))],
            vec![json!(label_of(&second, "impl-build"))]
        )
    );
    assert_ne!(
        label_of(&first, "impl-build"),
        label_of(&second, "impl-build")
    );
    assert_eq!(
        case.all_events_of(&second, "node_error"),
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn json_output_longer_than_the_preview_cap_binds_from_the_full_answer() {
    // M2: the roster preview (~160 characters) truncated a longer fenced
    // JSON mid-object; the collect envelope's full answer is the binding
    // lane, the preview stays a preview.
    let case = Case::new();
    let worktree = "/Users/x/Research/pa-worktrees/rp-3354-the-very-long-worktree-name";
    let payload = json!({ "worktree": worktree, "notes": "y".repeat(150) });
    let long_json = format!("Intro prose.\n\n```json\n{payload}\n```\n\nOutro.");
    assert!(long_json.len() > HOST_ANSWER_PREVIEW_CHARS);
    case.host.outcome("setup", done(&long_json));
    case.host.outcome("impl", done("DONE"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "escalate", "max_parallel": 4 },
            "states": [
                { "id": "setup", "entry": true, "subagent": "worker",
                  "outputs": [{ "name": "worktree", "type": "json" }] },
                { "id": "impl", "subagent": "worker",
                  "inputs": [{ "name": "worktree", "type": "json", "from": "setup.worktree" }] }
            ],
            "transitions": [{ "from": "setup", "to": "impl" }]
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(case.settle(&result).await["state"], "done");
    let impl_prompt = case.host.spawn_prompts("impl")[0].clone();
    assert!(
        impl_prompt.contains(&format!("- worktree: {}", json!(worktree))),
        "{impl_prompt}"
    );
    assert_eq!(
        case.all_events_of(&result, "output_capture_failed"),
        Vec::<Value>::new()
    );
    // The ledger previews compactly; the captured text is the raw answer.
    let captured = case.all_events_of(&result, "answer_captured")[0]["answer"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(captured.chars().count() <= ANSWER_CAPTURE_CAP);
    assert!(captured.contains("```json\n"), "{captured}");
    assert!(!captured.contains("..."), "{captured}");
}

#[tokio::test]
async fn a_json_null_output_is_a_captured_value_not_a_failure() {
    let case = Case::new();
    case.host
        .outcome("src", done("```json\n{\"data\": null}\n```"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 4 },
            "states": [
                { "id": "src", "entry": true, "subagent": "worker",
                  "outputs": [{ "name": "data", "type": "json" }] },
                { "id": "dep", "subagent": { "prompt": "Proceed." },
                  "inputs": [{ "name": "data", "type": "json", "from": "src.data", "optional": true }] }
            ],
            "transitions": [{ "from": "src", "to": "dep" }]
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(case.settle(&result).await["state"], "done");
    assert_eq!(
        case.all_events_of(&result, "output_capture_failed"),
        Vec::<Value>::new()
    );
    assert!(case.host.spawn_prompts("dep")[0].contains("- data: null"));
}

#[tokio::test]
async fn capture_failure_names_the_binding_cap_and_records_the_event() {
    let case = Case::new();
    let huge = format!("{{\"blob\": \"{}\"}}", "z".repeat(ANSWER_BINDING_CAP + 50));
    case.host
        .outcome("src", done(&format!("```json\n{huge}\n```")));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 4 },
            "states": [
                { "id": "src", "entry": true, "subagent": "worker",
                  "outputs": [{ "name": "blob", "type": "json" }] },
                { "id": "dep", "subagent": { "prompt": "Use {blob}." },
                  "inputs": [{ "name": "blob", "type": "json", "from": "src.blob", "optional": true }] }
            ],
            "transitions": [{ "from": "src", "to": "dep" }]
        }),
        "sw",
    );
    let result = case.start().await;
    case.settle(&result).await;
    let failures = case.all_events_of(&result, "output_capture_failed");
    assert_eq!(failures.len(), 1, "{failures:?}");
    let error = failures[0]["error"].as_str().unwrap();
    assert!(error.contains(&ANSWER_BINDING_CAP.to_string()), "{error}");
    assert!(
        error.contains("keep the fenced JSON block compact"),
        "{error}"
    );
}

#[tokio::test]
async fn optional_input_waits_for_a_live_upstream_before_spawning() {
    // M3: an optional input over another state that is still running must
    // not bind its null sentinel yet.
    let case = Case::new();
    case.host.outcome("impl", running());
    case.host.outcome("validate", done("VALIDATED"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 4 },
            "states": [
                { "id": "seed", "entry": true, "subagent": "worker" },
                { "id": "impl", "subagent": "worker", "outputs": [{ "name": "result", "type": "text" }] },
                { "id": "validate", "subagent": { "prompt": "Validate the build at {result}." },
                  "inputs": [{ "name": "result", "type": "text", "from": "impl.result", "optional": true }] }
            ],
            "transitions": [{ "from": "seed", "to": "impl" }, { "from": "seed", "to": "validate" }]
        }),
        "sw",
    );
    let result = case.start().await;
    case.wait_until(|| case.host.spawn_calls("impl").len() == 1)
        .await;
    case.wait_until(|| case.host.collects() >= 3).await;
    assert!(case.host.spawn_calls("validate").is_empty());
    case.host.outcome("impl", done("WTREE-A1"));
    assert_eq!(case.settle(&result).await["state"], "done");
    let validate = case.host.spawn_prompts("validate");
    assert_eq!(validate.len(), 1);
    assert!(validate[0].contains("WTREE-A1"), "{}", validate[0]);
    assert!(!validate[0].contains("None"), "{}", validate[0]);
}

#[tokio::test]
async fn capture_retry_rebinds_a_settle_that_raced_the_answer_capture() {
    // M2: a child settled with no captured answer yet; the capture retry
    // re-collects it once and the declared output binds.
    let case = Case::new();
    case.host.with(|state| {
        state.late_children.insert("child-2".to_string());
    });
    case.host
        .outcome("build", done("```json\n{\"built\": \"BIN-A1\"}\n```"));
    case.host.outcome("use", done("USED"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "escalate", "max_parallel": 4 },
            "states": [
                { "id": "seed", "entry": true, "subagent": "worker" },
                { "id": "build", "subagent": "worker", "outputs": [{ "name": "built", "type": "json" }] },
                { "id": "use", "subagent": { "prompt": "Use {built}." },
                  "inputs": [{ "name": "built", "type": "json", "from": "build.built" }] }
            ],
            "transitions": [{ "from": "seed", "to": "build" }, { "from": "build", "to": "use" }]
        }),
        "sw",
    );
    let result = case.start().await;
    assert_eq!(case.settle(&result).await["state"], "done");
    assert_eq!(
        case.host.spawn_prompts("use"),
        vec!["Use \"BIN-A1\".".to_string()]
    );
    assert_eq!(
        case.all_events_of(&result, "output_capture_failed"),
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn child_exit_captures_a_provisional_answer_and_marks_needs_verify() {
    // M4: an exited child's last assistant text is a PROVISIONAL answer: the
    // entry keeps its error verdict but reads needs_verify, and the exit is
    // never counted as settled work.
    let case = Case::new();
    case.host.child_outcome(
        "child-1",
        exited(
            "worker exited mid-flight",
            "The CI gate ran clean on lane A; the report is at /tmp/report.md.",
        ),
    );
    case.store_machine(
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 4 },
            "states": [{ "id": "work", "entry": true, "subagent": "worker" }],
            "transitions": []
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(status["needs_verify"], json!(["work"]));
    let work = node_status(&status, "work");
    assert_eq!(work["needs_verify"], true);
    let row = &work["entries"][0];
    assert_eq!(row["status"], "error");
    assert_eq!(row["needs_verify"], true);
    assert!(row["provisional_answer"]
        .as_str()
        .unwrap()
        .contains("report is at /tmp/report.md"));
    let marked: Vec<Value> = case
        .all_events_of(&result, "needs_verify")
        .into_iter()
        .map(|event| event["node"].clone())
        .collect();
    assert_eq!(marked, vec![json!("work")]);
    assert_eq!(
        case.all_events_of(&result, "answer_captured"),
        Vec::<Value>::new()
    );
    assert_eq!(status["usage"]["settled"], 1);
}

#[tokio::test]
async fn late_sibling_exit_after_terminal_entry_still_marks_needs_verify() {
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
                  "foreach": { "over": "items", "max": 2 } }
            ]
        }),
        "sw",
    );
    case.host.child_outcome("child-2", failed("boom-1"));
    case.host.child_outcome("child-3", running());
    let result = case.start().await;
    let run_id = Case::run_id(&result);
    case.wait_until(|| {
        case.executor.snapshot_run(&run_id).is_some_and(|run| {
            run.states
                .iter()
                .find(|state| state.state_id == "fan")
                .is_some_and(|state| {
                    state
                        .entries
                        .iter()
                        .any(|entry| entry.status == crate::factory::executor::model::Status::Error)
                })
        })
    })
    .await;
    case.host.child_outcome(
        "child-3",
        exited(
            "worker exited mid-flight",
            "Lane B finished; results staged at /tmp/lane-b.md.",
        ),
    );
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "failed");
    assert_eq!(status["needs_verify"], json!(["fan"]));
    let fan = node_status(&status, "fan");
    assert_eq!(fan["needs_verify"], true);
    let row = &fan["entries"][0];
    assert_eq!(row["status"], "error");
    assert_eq!(row["needs_verify"], true);
    assert!(row["provisional_answer"]
        .as_str()
        .unwrap()
        .contains("results staged at /tmp/lane-b.md"));
    assert_eq!(case.all_events_of(&result, "needs_verify").len(), 1);
    assert_eq!(case.host.spawn_calls("fan").len(), 2);
    assert_eq!(case.all_events_of(&result, "retry"), Vec::<Value>::new());
}

#[tokio::test]
async fn paused_run_status_carries_the_last_error_and_remedy() {
    // M6: a paused run's status names what failed and how to continue.
    let case = Case::new();
    case.host.outcome("src", done("no json here"));
    case.store_machine(
        json!({
            "run": { "failure_policy": "escalate", "max_parallel": 4 },
            "states": [
                { "id": "src", "entry": true, "subagent": "worker",
                  "outputs": [{ "name": "data", "type": "json" }] },
                { "id": "dep", "subagent": "worker",
                  "inputs": [{ "name": "data", "type": "json", "from": "src.data" }] }
            ],
            "transitions": [{ "from": "src", "to": "dep" }]
        }),
        "sw",
    );
    let result = case.start().await;
    let status = case.settle(&result).await;
    assert_eq!(status["state"], "paused");
    assert_eq!(
        status["pause_reason"],
        "input 'data': no JSON object containing output 'data' in the upstream answer"
    );
    let last_error = status["last_error"].as_str().unwrap();
    assert!(last_error.contains("state 'dep' failed"), "{last_error}");
    assert!(
        last_error.contains("no JSON object containing output 'data'"),
        "{last_error}"
    );
    assert!(status["remedy"]
        .as_str()
        .unwrap()
        .contains(&format!("rlm.factory.resume('{}')", Case::run_id(&result))));
}
